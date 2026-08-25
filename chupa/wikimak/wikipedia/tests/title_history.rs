use std::collections::HashMap;

// Exercise the private state machine through a local copy of its public
// inputs by including the implementation: the module deliberately remains
// crate-private until sync integration is complete.
#[path = "../src/title_history.rs"]
mod title_history;
use title_history::{Event, EventKind, Interval, Reconstruction, TitleKey};

fn key(ns: i64, title: &str) -> TitleKey {
    TitleKey {
        ns,
        title: title.as_bytes().to_vec(),
    }
}

fn event(page: u32, kind: EventKind, at: u32, ordinal: u64, title: Option<TitleKey>) -> Event {
    Event {
        page_id: Some(page),
        kind,
        at,
        source_ordinal: ordinal,
        historical: title,
    }
}

#[test]
fn move_and_namespace_move_close_then_open() {
    let a = key(0, "A");
    let b = key(1, "Talk:A");
    let state = Reconstruction::from_events(vec![
        event(1, EventKind::Create, 10, 0, Some(a.clone())),
        event(1, EventKind::Move, 20, 1, Some(b.clone())),
    ]);
    assert_eq!(
        state.by_title[&a],
        vec![Interval {
            page_id: 1,
            start: 10,
            end: Some(20)
        }]
    );
    assert_eq!(
        state.by_title[&b],
        vec![Interval {
            page_id: 1,
            start: 20,
            end: None
        }]
    );
}

#[test]
fn delete_restore_preserves_the_gap() {
    let a = key(0, "A");
    let state = Reconstruction::from_events(vec![
        event(1, EventKind::Create, 10, 0, Some(a.clone())),
        event(1, EventKind::Delete, 20, 1, Some(a.clone())),
        event(1, EventKind::Restore, 30, 2, Some(a.clone())),
    ]);
    assert_eq!(
        state.by_title[&a],
        vec![
            Interval {
                page_id: 1,
                start: 10,
                end: Some(20)
            },
            Interval {
                page_id: 1,
                start: 30,
                end: None
            },
        ]
    );
}

#[test]
fn revision_observation_does_not_erase_explicit_delete_gap() {
    let a = key(0, "A");
    let state = Reconstruction::from_events(vec![
        event(1, EventKind::Create, 10, 0, Some(a.clone())),
        event(1, EventKind::Delete, 20, 1, Some(a.clone())),
        event(1, EventKind::RevisionInferred, 25, 2, Some(a.clone())),
        event(1, EventKind::Restore, 30, 3, Some(a.clone())),
    ]);
    assert_eq!(
        state.by_title[&a],
        vec![
            Interval {
                page_id: 1,
                start: 10,
                end: Some(20)
            },
            Interval {
                page_id: 1,
                start: 30,
                end: None
            },
        ]
    );
}

#[test]
fn recreate_transfers_reused_title_to_new_page_id() {
    let a = key(0, "A");
    let state = Reconstruction::from_events(vec![
        event(1, EventKind::Create, 10, 0, Some(a.clone())),
        event(1, EventKind::Delete, 20, 1, Some(a.clone())),
        event(2, EventKind::Create, 30, 2, Some(a.clone())),
    ]);
    assert_eq!(state.current_by_page, HashMap::from([(2, a.clone())]));
    assert_eq!(state.by_title[&a][1].page_id, 2);
}

#[test]
fn reverse_page_id_title_reuse_still_follows_chronology() {
    let a = key(0, "A");
    let state = Reconstruction::from_events(vec![
        event(9, EventKind::Create, 10, 0, Some(a.clone())),
        event(2, EventKind::Create, 20, 1, Some(a.clone())),
    ]);
    assert_eq!(state.current_by_page, HashMap::from([(2, a.clone())]));
    assert_eq!(
        state.by_title[&a],
        vec![
            Interval {
                page_id: 9,
                start: 10,
                end: Some(20),
            },
            Interval {
                page_id: 2,
                start: 20,
                end: None,
            },
        ]
    );
}

#[test]
fn shuffled_equal_time_uses_source_ordinal_and_duplicate_create_page_is_noop() {
    let a = key(0, "A");
    let b = key(0, "B");
    let state = Reconstruction::from_events(vec![
        event(1, EventKind::Move, 20, 3, Some(b.clone())),
        event(1, EventKind::CreatePage, 10, 2, Some(a.clone())),
        event(1, EventKind::Create, 10, 1, Some(a.clone())),
    ]);
    assert_eq!(
        state.by_title[&a],
        vec![Interval {
            page_id: 1,
            start: 10,
            end: Some(20)
        }]
    );
    assert_eq!(state.current_by_page[&1], b);
}

#[test]
fn revision_only_observation_infers_missing_open_but_current_fields_do_not_drive_state() {
    let old = key(0, "Old");
    let observed = key(0, "Observed");
    let state = Reconstruction::from_events(vec![
        event(1, EventKind::RevisionInferred, 5, 0, Some(old.clone())),
        event(1, EventKind::Move, 10, 1, Some(observed.clone())),
    ]);
    assert_eq!(state.by_title[&old][0].end, Some(10));
    assert_eq!(state.current_by_page[&1], observed);
}

#[test]
fn merge_and_null_page_id_do_not_mutate_title_state() {
    let a = key(0, "A");
    let state = Reconstruction::from_events(vec![
        Event {
            page_id: None,
            kind: EventKind::Create,
            at: 1,
            source_ordinal: 0,
            historical: Some(a.clone()),
        },
        event(1, EventKind::Merge, 2, 1, Some(a)),
    ]);
    assert!(state.by_title.is_empty());
}
