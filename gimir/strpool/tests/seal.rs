mod common;

use strpool::_internal::{parse_footer, FOOTER_SIZE};
use strpool::{Pool, PoolConfig};

fn small_seal_cfg() -> PoolConfig {
    PoolConfig {
        shard_count: 1,
        seal_threshold_bytes: 256,
    }
}

fn read_footer(path: &std::path::Path) -> strpool::_internal::Footer {
    let bytes = std::fs::read(path).unwrap();
    parse_footer(&bytes[bytes.len() - FOOTER_SIZE..]).expect("valid footer on disk")
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
fn seal_compacts_tail_and_iter_preserves_order() {
    let dir = common::scratch_dir("seal-basic");
    let pool = Pool::open(&dir, small_seal_cfg(), None).unwrap();
    // Append until the tail exceeds the 256-byte threshold.
    let mut ids = Vec::new();
    let mut originals = Vec::new();
    for i in 0..200 {
        let s = format!("entry-number-{}-with-pad", i);
        ids.push(pool.append(0, s.as_bytes()).unwrap());
        originals.push(s.into_bytes());
    }
    pool.flush(0).unwrap();
    let path = common::shard_path(&dir, 0);
    let pre_size = std::fs::metadata(&path).unwrap().len();
    let pre_footer = read_footer(&path);
    assert!(pre_footer.tail_len > 256);

    let sealed = pool.maybe_seal(0).unwrap();
    assert!(sealed, "seal should have run");

    let post_footer = read_footer(&path);
    assert_eq!(post_footer.tail_len, 0, "tail should be empty after seal");
    let post_size = std::fs::metadata(&path).unwrap().len();
    // Frame region grew (it went from 0 bytes to some compressed payload).
    let pre_frame_region = (pre_size - FOOTER_SIZE as u64) - pre_footer.tail_len as u64;
    let post_frame_region = post_size - FOOTER_SIZE as u64;
    assert!(post_frame_region > pre_frame_region);

    // Iter still yields all entries in order with correct ids.
    let collected = collect_shard(&pool, 0);
    assert_eq!(collected.len(), originals.len());
    for (i, (id, bytes)) in collected.iter().enumerate() {
        assert_eq!(*id, ids[i]);
        assert_eq!(bytes, &originals[i]);
    }
}

#[test]
fn below_threshold_seal_is_noop() {
    let dir = common::scratch_dir("seal-noop");
    let pool = Pool::open(&dir, small_seal_cfg(), None).unwrap();
    pool.append(0, b"tiny").unwrap();
    pool.flush(0).unwrap();
    let sealed = pool.maybe_seal(0).unwrap();
    assert!(!sealed);
}

#[test]
fn crash_mid_seal_cleans_tmp_and_preserves_shard() {
    let dir = common::scratch_dir("seal-crash");
    let pool = Pool::open(&dir, small_seal_cfg(), None).unwrap();
    pool.append(0, b"alpha").unwrap();
    pool.append(0, b"beta").unwrap();
    pool.flush(0).unwrap();
    drop(pool);

    // Simulate a crash mid-seal: write a partial `.tmp`.
    let path = common::shard_path(&dir, 0);
    let tmp = {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        std::path::PathBuf::from(s)
    };
    std::fs::write(&tmp, b"partial-garbage-from-aborted-seal").unwrap();

    let pre_shard_bytes = std::fs::read(&path).unwrap();

    let pool = Pool::open(&dir, small_seal_cfg(), None).unwrap();
    assert!(!tmp.exists(), "tmp must be deleted on open");
    let post_shard_bytes = std::fs::read(&path).unwrap();
    assert_eq!(pre_shard_bytes, post_shard_bytes, "shard must be unchanged");

    let collected = collect_shard(&pool, 0);
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].1, b"alpha");
    assert_eq!(collected[1].1, b"beta");
}
