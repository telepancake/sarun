//! Capture mutation notification boundary.
//!
//! Filesystem policy records typed mutations here; UI delivery and
//! `BoxState`'s process-provenance producer share the one bounded journal.
//! Protocol callbacks never own or drain this queue.

use std::sync::{Arc, Mutex};

use crate::capture::{BoxState, EventQ};

const EVENT_CAPACITY: usize = 4096;

#[derive(Clone)]
pub(crate) struct MutationJournal {
    events: EventQ,
}

impl MutationJournal {
    pub(crate) fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        }
    }

    /// Connect process-provenance insertions from this box to the same ordered
    /// journal as filesystem mutations.
    pub(crate) fn attach_box(&self, box_state: &BoxState) {
        box_state.set_event_sink(self.events.clone());
    }

    pub(crate) fn record(&self, box_id: i64, rel: String, operation: &'static str) {
        let mut events = self.events.lock().unwrap();
        if events.len() >= EVENT_CAPACITY {
            events.drain(..EVENT_CAPACITY / 2);
        }
        events.push_back((box_id, rel, operation));
    }

    pub(crate) fn drain(&self) -> Vec<(i64, String, &'static str)> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_is_bounded_and_preserves_newest_mutation_order() {
        let journal = MutationJournal::new();
        for index in 0..=EVENT_CAPACITY {
            journal.record(7, index.to_string(), "write");
        }
        let events = journal.drain();
        assert_eq!(events.len(), EVENT_CAPACITY / 2 + 1);
        assert_eq!(events.first().unwrap().1, (EVENT_CAPACITY / 2).to_string());
        assert_eq!(events.last().unwrap().1, EVENT_CAPACITY.to_string());
        assert!(journal.drain().is_empty());
    }
}
