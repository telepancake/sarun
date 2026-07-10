//! Per-shard walk counters (`Pool::scan_counts`) and the append-only
//! stamp (`Pool::shard_entry_count`) — the instrumentation callers use
//! to pin how many shards a read touches (wikimak's title-dictionary
//! acceptance tests diff these around exact vs substring lookups).

mod common;

use rayon::iter::ParallelIterator;
use strpool::{Pool, PoolConfig};

fn cfg(shards: u32) -> PoolConfig {
    PoolConfig {
        shard_count: shards,
        seal_threshold_bytes: 1 << 30,
    }
}

#[test]
fn for_each_counts_only_its_shard() {
    let dir = common::scratch_dir("scan_counters_one");
    let pool = Pool::open(&dir, cfg(4), None).unwrap();
    for sid in 0..4 {
        pool.append(sid, format!("entry-{sid}").as_bytes()).unwrap();
    }
    assert_eq!(pool.scan_counts(), vec![0, 0, 0, 0], "appends are not scans");

    pool.for_each_in_shard(2, |_, _| Ok(())).unwrap();
    assert_eq!(
        pool.scan_counts(),
        vec![0, 0, 1, 0],
        "one walk of shard 2 bumps exactly shard 2's counter"
    );

    pool.for_each_in_shard(2, |_, _| Ok(())).unwrap();
    pool.for_each_in_shard(0, |_, _| Ok(())).unwrap();
    assert_eq!(pool.scan_counts(), vec![1, 0, 2, 0]);
}

#[test]
fn substring_scan_counts_every_shard_once() {
    let dir = common::scratch_dir("scan_counters_all");
    let pool = Pool::open(&dir, cfg(4), None).unwrap();
    for sid in 0..4 {
        pool.append(sid, format!("needle-{sid}").as_bytes()).unwrap();
    }
    let hits: Vec<_> = pool
        .scan_substring(b"needle")
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(hits.len(), 4, "one hit per shard");
    assert_eq!(
        pool.scan_counts(),
        vec![1, 1, 1, 1],
        "a substring scan walks every shard exactly once"
    );
}

#[test]
fn entry_count_stamp_is_append_monotone_and_seal_stable() {
    let dir = common::scratch_dir("scan_counters_stamp");
    // Zero threshold so `maybe_seal` always seals a non-empty tail.
    let pool = Pool::open(
        &dir,
        PoolConfig {
            shard_count: 2,
            seal_threshold_bytes: 0,
        },
        None,
    )
    .unwrap();
    assert_eq!(pool.shard_entry_count(0).unwrap(), 0);
    pool.append(0, b"a").unwrap();
    pool.append(0, b"b").unwrap();
    pool.append(1, b"c").unwrap();
    assert_eq!(pool.shard_entry_count(0).unwrap(), 2, "stamp counts appends");
    assert_eq!(pool.shard_entry_count(1).unwrap(), 1);

    // Sealing compresses the tail but assigns no ids: the stamp (and
    // therefore any cache keyed on it) must not move.
    assert!(pool.maybe_seal(0).unwrap(), "non-empty tail must seal");
    assert_eq!(
        pool.shard_entry_count(0).unwrap(),
        2,
        "seal does not change the entry-count stamp"
    );
}
