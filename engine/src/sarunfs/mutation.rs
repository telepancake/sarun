//! Capture mutation notification boundary.
//!
//! Filesystem policy records typed mutations here; UI delivery and
//! `BoxState`'s process-provenance producer share the one bounded journal.
//! Protocol callbacks never own or drain this queue.

use std::sync::{Arc, Mutex};

use crate::capture::{BoxState, EventQ};
use crate::depot::BoxDepot;

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

    /// Begin one attributed mutation group. Writer identity is resolved while
    /// the calling process still exists and cannot be substituted later by a
    /// transport's release callback.
    pub(crate) fn writer<'a>(
        &self,
        box_state: &'a BoxState,
        actor: u32,
        host_actor: bool,
    ) -> CaptureWriter<'a> {
        CaptureWriter {
            box_state,
            writer: if host_actor {
                box_state.writer_for(actor)
            } else {
                box_state.guest_writer_for(actor)
            },
        }
    }

    pub(crate) fn observe_writer(&self, box_state: &BoxState, actor: u32, host_actor: bool) {
        if host_actor {
            let _ = box_state.writer_for(actor);
        } else {
            let _ = box_state.guest_writer_for(actor);
        }
    }

    pub(crate) fn set_xattr(&self, box_state: &BoxState, rel: &str, key: &str, value: &[u8]) {
        box_state.set_xattr(rel, key, value);
    }

    pub(crate) fn remove_xattr(&self, box_state: &BoxState, rel: &str, key: &str) -> bool {
        box_state.remove_xattr(rel, key)
    }
}

/// Depot mutation capability carrying the already-resolved writer identity.
/// Read-only layer inspection continues to use `BoxState`; changing persistent
/// layer state requires this value.
pub(crate) struct CaptureWriter<'a> {
    box_state: &'a BoxState,
    writer: i64,
}

impl CaptureWriter<'_> {
    pub(crate) fn ensure_file(&self, rel: &str, mode: u32) -> i64 {
        self.box_state.ensure_file_row(rel, mode, self.writer)
    }

    pub(crate) fn finalize_file(&self, rel: &str, size: i64, mtime_ns: i64) {
        self.box_state
            .finalize_file(rel, size, mtime_ns, self.writer);
    }

    pub(crate) fn set_dir(&self, rel: &str, mode: u32) {
        self.box_state.set_dir(rel, mode, self.writer);
    }

    pub(crate) fn set_symlink(&self, rel: &str, target: &std::path::Path) {
        self.box_state.set_symlink(rel, target, self.writer);
    }

    pub(crate) fn set_special(&self, rel: &str, mode: u32, rdev: u64) {
        self.box_state.set_special(rel, mode, rdev, self.writer);
    }

    pub(crate) fn whiteout(&self, rel: &str) {
        self.box_state.set_whiteout(rel, self.writer);
    }

    pub(crate) fn delete(&self, rel: &str) {
        self.box_state.drop_row(rel);
        self.whiteout(rel);
    }

    pub(crate) fn rename(&self, old: &str, new: &str) {
        self.box_state.rename_row(old, new);
    }

    pub(crate) fn reparent(&self, old: &str, new: &str) {
        self.box_state.reparent(old, new);
    }

    pub(crate) fn set_mode(&self, rel: &str, mode: u32) {
        self.box_state.set_mode(rel, mode);
    }

    pub(crate) fn set_owner(&self, rel: &str, uid: u32, gid: u32) {
        self.box_state.set_owner(rel, uid, gid);
    }

    pub(crate) fn set_mtime(&self, rel: &str, mtime_ns: i64) {
        self.box_state.set_mtime(rel, mtime_ns);
    }

    pub(crate) fn set_atime(&self, rel: &str, atime_ns: i64) {
        self.box_state.set_atime(rel, atime_ns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Entry;

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

    #[test]
    fn capture_writer_is_the_depot_mutation_boundary() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temp = std::env::temp_dir().join(format!(
            "sarun-capture-writer-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        // SAFETY: every state-home-dependent test using BoxState is serialized
        // by TEST_STATE_HOME_LOCK.
        unsafe { std::env::set_var("XDG_STATE_HOME", &temp) };
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let box_state = BoxState::create(9831).unwrap();
        let journal = MutationJournal::new();
        let capture = journal.writer(&box_state, std::process::id(), false);
        capture.set_dir("work", 0o755);
        capture.ensure_file("work/result", 0o100644);
        capture.finalize_file("work/result", 17, 23);
        capture.set_mode("work/result", 0o100600);
        capture.set_owner("work/result", 1000, 1001);
        journal.set_xattr(&box_state, "work/result", "user.test", b"value");
        capture.rename("work/result", "work/final");

        assert!(matches!(
            box_state.entry("work"),
            Some(Entry::Dir { mode: 0o040755, .. })
        ));
        assert!(matches!(
            box_state.entry("work/final"),
            Some(Entry::File { mode: 0o100600, .. })
        ));
        assert_eq!(box_state.owner_of("work/final"), Some((1000, 1001)));
        assert_eq!(
            box_state.get_xattr("work/final", "user.test"),
            Some(b"value".to_vec())
        );
        assert_eq!(box_state.owner_of("work/result"), None);
        assert_eq!(box_state.get_xattr("work/result", "user.test"), None);

        journal
            .writer(&box_state, std::process::id(), false)
            .delete("work/final");
        assert!(matches!(
            box_state.entry("work/final"),
            Some(Entry::Whiteout)
        ));
        assert_eq!(box_state.owner_of("work/final"), None);
        assert_eq!(box_state.get_xattr("work/final", "user.test"), None);

        drop(capture);
        drop(box_state);
        let _ = std::fs::remove_dir_all(temp);
    }
}
