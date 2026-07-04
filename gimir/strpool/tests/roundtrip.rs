mod common;

use std::sync::Arc;

use strpool::{Pool, PoolConfig};

fn cfg(shards: u32) -> PoolConfig {
    PoolConfig {
        shard_count: shards,
        seal_threshold_bytes: 1 << 30,
    }
}

fn collect_shard(pool: &Pool, shard_id: u32) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    pool.for_each_in_shard(shard_id, |id, bytes| {
        out.push((id, bytes.to_vec()));
        Ok(())
    })
    .unwrap();
    out
}

#[test]
fn empty_pool_iter_empty() {
    let dir = common::scratch_dir("empty");
    let pool = Pool::open(&dir, cfg(1), None).unwrap();
    assert!(collect_shard(&pool, 0).is_empty());
}

#[test]
fn append_iter_roundtrip_various_sizes() {
    for &n in &[1usize, 2, 50, 500, 10_000] {
        let dir = common::scratch_dir(&format!("rt-{n}"));
        let strings: Vec<Vec<u8>> = (0..n)
            .map(|i| format!("hello-world-{}", i).into_bytes())
            .collect();
        let pool = Pool::open(&dir, cfg(1), None).unwrap();
        let mut ids = Vec::new();
        for s in &strings {
            ids.push(pool.append(0, s).unwrap());
        }
        pool.flush(0).unwrap();
        drop(pool);

        // Reopen.
        let pool = Pool::open(&dir, cfg(1), None).unwrap();
        let collected = collect_shard(&pool, 0);
        assert_eq!(collected.len(), n);
        for (i, (id, bytes)) in collected.iter().enumerate() {
            assert_eq!(*id, ids[i], "id mismatch at index {i} (n={n})");
            assert_eq!(bytes, &strings[i], "bytes mismatch at index {i} (n={n})");
        }
    }
}

#[test]
fn append_many_sequential_ids() {
    let dir = common::scratch_dir("many");
    // 2 shards → shard_bits=1 (derived).
    let pool = Pool::open(&dir, cfg(2), None).unwrap();
    let strings: Vec<Vec<u8>> = (0..256).map(|i| format!("item-{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = strings.iter().map(|v| v.as_slice()).collect();
    let ids = pool.append_many(0, &refs).unwrap();
    pool.flush(0).unwrap();
    // Ids should be sequential under shard_bits=1: local 0..N → 0, 2, 4, ...
    for (i, &id) in ids.iter().enumerate() {
        assert_eq!(id, (i as u64) << 1);
    }
    let collected = collect_shard(&pool, 0);
    assert_eq!(collected.len(), strings.len());
    for (i, (id, bytes)) in collected.iter().enumerate() {
        assert_eq!(*id, ids[i]);
        assert_eq!(bytes, &strings[i]);
    }
}

#[test]
fn lsb_shard_ids() {
    let dir = common::scratch_dir("lsb");
    // 4 shards → shard_bits=2 (derived).
    let pool: Arc<Pool> = Arc::new(Pool::open(&dir, cfg(4), None).unwrap());
    for sid in 0..4 {
        let id = pool
            .append(sid, format!("string-{sid}").as_bytes())
            .unwrap();
        // Low 2 bits == shard id.
        assert_eq!((id & 0b11) as u32, sid, "shard {sid} id={id}");
        // First entry in each shard has local id 0 → global id == shard id.
        assert_eq!(id, sid as u64);
    }
    for sid in 0..4 {
        pool.flush(sid).unwrap();
    }
}
