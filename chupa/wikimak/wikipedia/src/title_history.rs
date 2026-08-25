use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TitleKey {
    pub ns: i64,
    pub title: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
    Create,
    CreatePage,
    Move,
    Delete,
    Restore,
    Merge,
    RevisionInferred,
}

#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub page_id: Option<u32>,
    pub kind: EventKind,
    pub at: u32,
    pub source_ordinal: u64,
    pub historical: Option<TitleKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Interval {
    pub page_id: u32,
    pub start: u32,
    pub end: Option<u32>,
}

#[derive(Default)]
pub(crate) struct Reconstruction {
    pub by_title: HashMap<TitleKey, Vec<Interval>>,
    pub current_by_page: HashMap<u32, TitleKey>,
    current_page_by_title: HashMap<TitleKey, u32>,
}

impl Reconstruction {
    pub fn from_events(mut events: Vec<Event>) -> Self {
        // MWH partitions and rows are not a reliable cross-page time order.
        // A global chronological merge is required for title reuse: the
        // later page must take ownership regardless of either page id.
        // This is also chronological within every page.
        events.sort_by_key(|event| {
            (event.at, event.source_ordinal, event.page_id.unwrap_or(u32::MAX))
        });
        let mut out = Self::default();
        let mut explicitly_deleted = std::collections::HashSet::new();
        for event in events {
            let Some(page_id) = event.page_id else {
                continue;
            };
            match event.kind {
                EventKind::Merge => {}
                EventKind::Delete => {
                    out.close_page(page_id, event.at);
                    explicitly_deleted.insert(page_id);
                }
                EventKind::CreatePage if out.current_by_page.contains_key(&page_id) => {}
                EventKind::RevisionInferred if explicitly_deleted.contains(&page_id) => {}
                EventKind::Create | EventKind::CreatePage | EventKind::Move | EventKind::Restore => {
                    explicitly_deleted.remove(&page_id);
                    if let Some(title) = event.historical {
                        out.open(page_id, title, event.at);
                    }
                }
                EventKind::RevisionInferred => {
                    if let Some(title) = event.historical {
                        out.open(page_id, title, event.at);
                    }
                }
            }
        }
        out
    }

    fn open(&mut self, page_id: u32, title: TitleKey, at: u32) {
        if self.current_by_page.get(&page_id) == Some(&title) {
            return;
        }
        self.close_page(page_id, at);
        if let Some(previous_page) = self.current_page_by_title.get(&title).copied() {
            self.close_page(previous_page, at);
        }
        self.by_title
            .entry(title.clone())
            .or_default()
            .push(Interval {
                page_id,
                start: at,
                end: None,
            });
        self.current_by_page.insert(page_id, title);
        self.current_page_by_title
            .insert(self.current_by_page[&page_id].clone(), page_id);
    }

    fn close_page(&mut self, page_id: u32, at: u32) {
        let Some(title) = self.current_by_page.remove(&page_id) else {
            return;
        };
        self.current_page_by_title.remove(&title);
        let intervals = self
            .by_title
            .get_mut(&title)
            .expect("live title has interval");
        let current = intervals.last_mut().expect("live title has interval");
        if at <= current.start {
            intervals.pop();
        } else {
            current.end = Some(at);
        }
    }
}
