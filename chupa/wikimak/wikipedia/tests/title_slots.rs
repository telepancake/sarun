use rusqlite::Connection;
use tempfile::TempDir;
use wikimak_wikipedia::title_slots::{
    OlderTitleInterval, OlderTitleIntervalsMut, SqliteOlderTitleIntervals, TitleBinding,
    TitleSlotGenerations, TitleSlots, TITLE_SLOT_BYTES,
};

fn bound(page_id: u32, valid_since: u32) -> TitleBinding {
    TitleBinding::bound(page_id, valid_since).unwrap()
}

#[test]
fn flat_slots_are_exactly_eight_bytes_and_query_current_state() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("current-title-slots");
    let slots = TitleSlots::atomic_rebuild(
        &path,
        &[
            (0, TitleBinding::unbound(100)),
            (1, bound(42, 200)),
            (3, bound(77, 300)),
        ],
    )
    .unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().len(), 4 * TITLE_SLOT_BYTES);
    assert_eq!(slots.current(1), Some(bound(42, 200)));
    assert_eq!(slots.current(2), Some(TitleBinding::unbound(0)));
    assert_eq!(slots.current(3), Some(bound(77, 300)));
    assert_eq!(slots.current(4), None);
}

#[test]
fn tau_query_uses_current_slot_then_sparse_older_overflow() {
    let tmp = TempDir::new().unwrap();
    let slots =
        TitleSlots::atomic_rebuild(tmp.path().join("slots"), &[(7, bound(300, 3000))]).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    let mut older = SqliteOlderTitleIntervals::open(&mut conn).unwrap();
    older
        .replace(
            7,
            &[
                OlderTitleInterval {
                    start: 500,
                    end: 1000,
                    page_id: 0,
                },
                OlderTitleInterval {
                    start: 1000,
                    end: 2000,
                    page_id: 100,
                },
                OlderTitleInterval {
                    start: 2000,
                    end: 3000,
                    page_id: 200,
                },
            ],
        )
        .unwrap();

    assert_eq!(slots.page_at(7, 499, &older).unwrap(), None);
    assert_eq!(slots.page_at(7, 750, &older).unwrap(), None);
    assert_eq!(slots.page_at(7, 1000, &older).unwrap(), Some(100));
    assert_eq!(slots.page_at(7, 2999, &older).unwrap(), Some(200));
    assert_eq!(slots.page_at(7, 3000, &older).unwrap(), Some(300));
}

#[test]
fn prepared_rebuild_crash_leaves_old_file_selected() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("slots");
    let old = TitleSlots::atomic_rebuild(&path, &[(1, bound(10, 100))]).unwrap();
    drop(old);

    let prepared = TitleSlots::prepare_rebuild(&path, &[(1, bound(20, 200))]).unwrap();
    // Simulated crash: the durable .tmp exists but rename never happened.
    drop(prepared);
    let reopened = TitleSlots::open(&path).unwrap();
    assert_eq!(reopened.current(1), Some(bound(10, 100)));

    let replaced = TitleSlots::atomic_rebuild(&path, &[(1, bound(30, 300))]).unwrap();
    assert_eq!(replaced.current(1), Some(bound(30, 300)));
}

#[test]
fn title_id_reshard_remaps_flat_slots_and_overflow() {
    let tmp = TempDir::new().unwrap();
    let old_path = tmp.path().join("old-slots");
    let old = TitleSlots::atomic_rebuild(
        &old_path,
        &[(1, bound(11, 1100)), (2, TitleBinding::unbound(2200))],
    )
    .unwrap();
    let remap = [(1, 17), (2, 9)];
    let prepared =
        TitleSlots::prepare_remapped(tmp.path().join("new-slots"), &old, &remap).unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let mut older = SqliteOlderTitleIntervals::open(&mut conn).unwrap();
    older
        .replace(
            1,
            &[OlderTitleInterval {
                start: 100,
                end: 1100,
                page_id: 7,
            }],
        )
        .unwrap();
    older.remap_title_ids(&remap).unwrap();
    let new = prepared.commit().unwrap();

    assert_eq!(new.current(17), Some(bound(11, 1100)));
    assert_eq!(new.current(9), Some(TitleBinding::unbound(2200)));
    assert_eq!(new.page_at(17, 500, &older).unwrap(), Some(7));
    assert_eq!(new.page_at(1, 500, &older).unwrap(), None);
}

#[test]
fn truncated_main_file_is_rejected_but_truncated_tmp_is_ignored() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("slots");
    TitleSlots::atomic_rebuild(&path, &[(1, bound(8, 9))]).unwrap();
    std::fs::write(tmp.path().join("slots.tmp"), [1, 2, 3]).unwrap();
    assert!(TitleSlots::open(&path).is_ok());
    assert!(
        !tmp.path().join("slots.tmp").exists(),
        "a valid open cleans abandoned preparation"
    );

    std::fs::write(&path, [1, 2, 3]).unwrap();
    let error = TitleSlots::open(path)
        .err()
        .expect("truncated main must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn numeric_boundaries_are_rejected_before_storage() {
    assert!(TitleBinding::bound(0, 1).is_err());
    assert!(TitleBinding::try_bound(u32::MAX as u64 + 1, 1).is_err());
    assert!(TitleBinding::try_bound(1, -1).is_err());
    assert!(TitleBinding::try_bound(1, u32::MAX as i64 + 1).is_err());
    assert!(OlderTitleInterval::try_new(20, 10, 1).is_err());
    assert!(OlderTitleInterval::try_new(1, 2, u32::MAX as u64 + 1).is_err());
}

#[test]
fn history_snapshot_streams_current_slots_and_sparse_overflow() {
    let tmp = TempDir::new().unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    drop(SqliteOlderTitleIntervals::open(&mut conn).unwrap());

    let transaction = conn.transaction().unwrap();
    let mut builder = TitleSlotGenerations::prepare_snapshot(tmp.path(), 1, &transaction).unwrap();
    builder
        .push_title(3, 33, 3000, &[(1000, 2000, 11), (2000, 3000, 22)])
        .unwrap();
    builder.push_title(9, 0, 4000, &[(3000, 4000, 44)]).unwrap();
    let prepared = builder.finish().unwrap();

    // The complete file is durable before the metadata transaction selects
    // its generation.
    let slots = prepared.commit().unwrap();
    TitleSlotGenerations::select(&transaction, 1).unwrap();
    transaction.commit().unwrap();

    assert_eq!(TitleSlotGenerations::selected(&conn).unwrap(), 1);
    let reverse = TitleSlotGenerations::open_selected_page_titles(tmp.path(), &conn).unwrap();
    let older = SqliteOlderTitleIntervals::open(&mut conn).unwrap();
    assert_eq!(reverse.current_title_id(33), Some(3));
    assert_eq!(reverse.current_title_id(44), None);
    assert_eq!(slots.current(3), Some(bound(33, 3000)));
    assert_eq!(slots.page_at(3, 1500, &older).unwrap(), Some(11));
    assert_eq!(slots.page_at(3, 2500, &older).unwrap(), Some(22));
    assert_eq!(slots.page_at(3, 3000, &older).unwrap(), Some(33));
    assert_eq!(slots.page_at(9, 3500, &older).unwrap(), Some(44));
    assert_eq!(slots.page_at(9, 4000, &older).unwrap(), None);
}

#[test]
fn published_generation_is_invisible_until_selector_transaction_commits() {
    let tmp = TempDir::new().unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    drop(SqliteOlderTitleIntervals::open(&mut conn).unwrap());

    let transaction = conn.transaction().unwrap();
    let mut first = TitleSlotGenerations::prepare_snapshot(tmp.path(), 1, &transaction).unwrap();
    first.push_title(5, 50, 500, &[(100, 500, 40)]).unwrap();
    first.finish().unwrap().commit().unwrap();
    TitleSlotGenerations::select(&transaction, 1).unwrap();
    transaction.commit().unwrap();

    {
        let transaction = conn.transaction().unwrap();
        let mut second =
            TitleSlotGenerations::prepare_snapshot(tmp.path(), 2, &transaction).unwrap();
        second.push_title(5, 60, 600, &[(200, 600, 45)]).unwrap();
        second.finish().unwrap().commit().unwrap();
        TitleSlotGenerations::select(&transaction, 2).unwrap();
        drop(transaction); // simulated crash before metadata commit
    }

    assert_eq!(TitleSlotGenerations::selected(&conn).unwrap(), 1);
    let selected = TitleSlotGenerations::open_selected(tmp.path(), &conn).unwrap();
    let reverse = TitleSlotGenerations::open_selected_page_titles(tmp.path(), &conn).unwrap();
    let older = SqliteOlderTitleIntervals::open(&mut conn).unwrap();
    assert_eq!(selected.current(5), Some(bound(50, 500)));
    assert_eq!(reverse.current_title_id(50), Some(5));
    assert_eq!(reverse.current_title_id(60), None);
    assert_eq!(selected.page_at(5, 150, &older).unwrap(), Some(40));
    assert_eq!(selected.page_at(5, 550, &older).unwrap(), Some(50));
}

#[test]
fn collection_keeps_only_selected_slot_generation() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join("titles-g1")).unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    drop(SqliteOlderTitleIntervals::open(&mut conn).unwrap());
    for generation in [1, 2] {
        let tx = conn.transaction().unwrap();
        let mut builder =
            TitleSlotGenerations::prepare_snapshot(tmp.path(), generation, &tx).unwrap();
        builder.push_title(0, generation as u64, 10, &[]).unwrap();
        builder.finish().unwrap().commit().unwrap();
        TitleSlotGenerations::select(&tx, generation).unwrap();
        tx.commit().unwrap();
    }

    TitleSlotGenerations::collect_unselected(tmp.path(), 2).unwrap();
    assert!(!tmp.path().join("title-slots.1").exists());
    assert!(!tmp.path().join("page-titles.1").exists());
    assert!(tmp.path().join("title-slots.2").exists());
    assert!(tmp.path().join("page-titles.2").exists());
    assert!(
        tmp.path().join("titles-g1").is_dir(),
        "slot collection must not infer or collect the independently selected pool generation"
    );
}

#[test]
fn batched_current_cycle_preserves_both_directions_and_replays_idempotently() {
    let tmp = TempDir::new().unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    drop(SqliteOlderTitleIntervals::open(&mut conn).unwrap());
    let transaction = conn.transaction().unwrap();
    let mut initial =
        TitleSlotGenerations::prepare_snapshot(tmp.path(), 1, &transaction).unwrap();
    initial.push_title(1, 10, 100, &[]).unwrap();
    initial.push_title(2, 20, 100, &[]).unwrap();
    initial.push_title(3, 30, 100, &[]).unwrap();
    initial.finish().unwrap().commit().unwrap();
    TitleSlotGenerations::select(&transaction, 1).unwrap();
    transaction.commit().unwrap();

    let changes = [
        (1, bound(20, 200)),
        (2, bound(30, 200)),
        (3, bound(10, 200)),
    ];
    for _ in 0..2 {
        let (forward, reverse) =
            TitleSlotGenerations::apply_current(tmp.path(), 1, &changes).unwrap();
        assert_eq!(forward.current(1), Some(bound(20, 200)));
        assert_eq!(forward.current(2), Some(bound(30, 200)));
        assert_eq!(forward.current(3), Some(bound(10, 200)));
        assert_eq!(reverse.current_title_id(10), Some(3));
        assert_eq!(reverse.current_title_id(20), Some(1));
        assert_eq!(reverse.current_title_id(30), Some(2));
    }
}
