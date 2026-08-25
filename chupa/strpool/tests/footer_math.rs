mod common;

use std::io::Write;

use strpool::_internal::{parse_footer, write_footer_bytes, FOOTER_SIZE};
use strpool::{Pool, PoolConfig};

fn cfg() -> PoolConfig {
    PoolConfig {
        shard_count: 1,
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
fn footer_size_is_eight() {
    assert_eq!(FOOTER_SIZE, 8);
}

#[test]
fn handcrafted_footer_parses() {
    // tail_len = 7, entry_count = 2.
    let footer_bytes = write_footer_bytes(strpool::_internal::Footer {
        tail_len: 7,
        entry_count: 2,
    });
    let parsed = parse_footer(&footer_bytes).expect("must parse");
    assert_eq!(parsed.tail_len, 7);
    assert_eq!(parsed.entry_count, 2);

    // Manually construct a shard file: 7 bytes of tail then footer.
    let dir = common::scratch_dir("handcraft");
    let path = common::shard_path(&dir, 0);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"abc\0de\0").unwrap(); // 7 bytes, 2 strings.
    f.write_all(&footer_bytes).unwrap();
    drop(f);

    let pool = Pool::open(&dir, cfg(), None).unwrap();
    let collected = collect_shard(&pool, 0);
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].1, b"abc");
    assert_eq!(collected[1].1, b"de");
}

#[test]
fn empty_file_treated_as_empty_shard() {
    let dir = common::scratch_dir("empty-file");
    let path = common::shard_path(&dir, 0);
    // Touch a zero-byte file.
    std::fs::File::create(&path).unwrap();
    let pool = Pool::open(&dir, cfg(), None).unwrap();
    let collected = collect_shard(&pool, 0);
    assert!(collected.is_empty());
    // And appending to it should work.
    pool.append(0, b"first").unwrap();
    pool.flush(0).unwrap();
    let collected = collect_shard(&pool, 0);
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].1, b"first");
}
