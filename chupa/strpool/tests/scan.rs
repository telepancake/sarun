mod common;

use std::collections::HashSet;

use rayon::iter::ParallelIterator;
use strpool::{Pool, PoolConfig};

fn cfg(shards: u32) -> PoolConfig {
    PoolConfig {
        shard_count: shards,
        seal_threshold_bytes: 1 << 30,
    }
}

fn build_pool(n: usize, shards: u32) -> (Pool, Vec<Vec<u8>>, std::path::PathBuf) {
    let dir = common::scratch_dir("scan");
    let pool = Pool::open(&dir, cfg(shards), None).unwrap();
    let strings: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            // Mix of lengths and contents. About 1/10 contain "needle".
            if i % 10 == 0 {
                format!("string-{i}-with-needle-inside-{}", i % 3).into_bytes()
            } else {
                format!("string-{i}-no-match-{}", i * 7).into_bytes()
            }
        })
        .collect();
    for (i, s) in strings.iter().enumerate() {
        let sid = (i as u32) % shards;
        pool.append(sid, s).unwrap();
    }
    for sid in 0..shards {
        pool.flush(sid).unwrap();
    }
    (pool, strings, dir)
}

#[test]
fn scan_finds_only_matches() {
    let (pool, strings, _dir) = build_pool(100_000, 4);
    let needle = b"needle";

    let expected: HashSet<Vec<u8>> = strings
        .iter()
        .filter(|s| memchr::memmem::find(s, needle).is_some())
        .cloned()
        .collect();

    let got: Vec<(u64, Vec<u8>)> = pool
        .scan_substring(needle)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let got_bytes: HashSet<Vec<u8>> = got.iter().map(|(_, b)| b.clone()).collect();
    assert_eq!(got_bytes, expected);
    // And every returned entry must actually contain the needle.
    for (_, b) in &got {
        assert!(memchr::memmem::find(b, needle).is_some());
    }
}

#[test]
fn scan_is_deterministic_across_thread_counts() {
    let (pool, _strings, _dir) = build_pool(10_000, 4);
    let needle = b"needle";
    let mut sets = Vec::new();
    for threads in [1usize, 4, 8] {
        let p = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let s: HashSet<(u64, Vec<u8>)> = p.install(|| {
            pool.scan_substring(needle)
                .collect::<Result<HashSet<_>, _>>()
                .unwrap()
        });
        sets.push(s);
    }
    assert_eq!(sets[0], sets[1]);
    assert_eq!(sets[1], sets[2]);
}
