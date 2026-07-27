mod common;

use rayon::iter::ParallelIterator;
use strpool::{Pool, PoolConfig};

fn cfg(shards: u32) -> PoolConfig {
    PoolConfig {
        shard_count: shards,
        seal_threshold_bytes: 1 << 30,
    }
}

fn open_fd_count() -> usize {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot count descriptors through {directory}: {error}"))
        .count()
}

fn materialized_pool_holds_constant_descriptors(shard_count: u32) {
    let dir = common::scratch_dir(&format!("fd-bound-{shard_count}"));
    {
        let pool = Pool::open(&dir, cfg(shard_count), None).unwrap();
        for shard in 0..shard_count {
            pool.append(shard, format!("seed-{shard}").as_bytes()).unwrap();
            pool.flush(shard).unwrap();
        }
    }

    let before = open_fd_count();
    let pool = Pool::open(&dir, cfg(shard_count), None).unwrap();
    let after = open_fd_count();
    assert!(
        after <= before + 2,
        "opening {shard_count} materialized shards retained {} descriptors",
        after.saturating_sub(before)
    );

    let id = pool.append(shard_count - 1, b"post-open needle").unwrap();
    assert_eq!(pool.get(id).unwrap().as_deref(), Some(&b"post-open needle"[..]));
    let hits = pool
        .scan_substring(b"needle")
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(hits, vec![(id, b"post-open needle".to_vec())]);
    pool.flush(shard_count - 1).unwrap();
    assert!(
        open_fd_count() <= before + 2,
        "append/get/parallel scan left shard descriptors retained"
    );
}

#[test]
fn materialized_256_and_512_shard_pools_hold_o1_file_descriptors() {
    materialized_pool_holds_constant_descriptors(256);
    materialized_pool_holds_constant_descriptors(512);
}
