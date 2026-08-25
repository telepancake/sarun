mod common;

use std::io::Write;

use strpool::{Pool, PoolConfig};

fn cfg() -> PoolConfig {
    PoolConfig {
        shard_count: 1,
        seal_threshold_bytes: 1 << 30,
    }
}

fn collect(pool: &Pool) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    pool.for_each_in_shard(0, |_, bytes| {
        out.push(bytes.to_vec());
        Ok(())
    })
    .unwrap();
    out
}

#[test]
fn flushed_appends_survive_reopen() {
    // After a flush, the flushed state is recoverable.
    let dir = common::scratch_dir("crash-flushed");
    let pool = Pool::open(&dir, cfg(), None).unwrap();
    for i in 0..10 {
        pool.append(0, format!("flushed-{i}").as_bytes()).unwrap();
    }
    pool.flush(0).unwrap();
    drop(pool);

    let pool = Pool::open(&dir, cfg(), None).unwrap();
    let got = collect(&pool);
    assert_eq!(got.len(), 10);
    for (i, b) in got.iter().enumerate() {
        assert_eq!(b, format!("flushed-{i}").as_bytes());
    }
}

#[test]
fn restoring_flushed_snapshot_yields_flushed_state() {
    // A crash between two flushes loses appends since the last flush. We
    // model "crash" by capturing the file bytes EXACTLY at flush time and
    // restoring them later.
    let dir = common::scratch_dir("crash-at-flush");
    let pool = Pool::open(&dir, cfg(), None).unwrap();
    let flushed: Vec<Vec<u8>> = (0..5)
        .map(|i| format!("flushed-{i}").into_bytes())
        .collect();
    let mut flushed_ids = Vec::new();
    for s in &flushed {
        flushed_ids.push(pool.append(0, s).unwrap());
    }
    pool.flush(0).unwrap();
    let path = common::shard_path(&dir, 0);
    let flushed_snapshot = std::fs::read(&path).unwrap();

    // Append more without flushing.
    for i in 0..5 {
        pool.append(0, format!("post-{i}").as_bytes()).unwrap();
    }
    drop(pool);

    // "Crash": restore the file to its flushed-snapshot state.
    std::fs::write(&path, &flushed_snapshot).unwrap();

    let pool = Pool::open(&dir, cfg(), None).unwrap();
    let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
    pool.for_each_in_shard(0, |id, bytes| {
        got.push((id, bytes.to_vec()));
        Ok(())
    })
    .unwrap();
    assert_eq!(got.len(), 5);
    for (i, (id, bytes)) in got.iter().enumerate() {
        assert_eq!(*id, flushed_ids[i]);
        assert_eq!(bytes, &flushed[i]);
    }
}

#[test]
fn truncate_at_random_offsets_does_not_panic() {
    // Without flush, there are no guarantees beyond "shouldn't panic".
    let dir = common::scratch_dir("crash-truncate");
    let pool = Pool::open(&dir, cfg(), None).unwrap();
    let entries: Vec<Vec<u8>> = (0..10).map(|i| format!("entry-{i}").into_bytes()).collect();
    for (i, s) in entries.iter().enumerate() {
        pool.append(0, s).unwrap();
        if i == 4 {
            pool.flush(0).unwrap();
        }
    }
    let path = common::shard_path(&dir, 0);
    let post_size = std::fs::metadata(&path).unwrap().len();
    drop(pool);

    let mut offsets: Vec<u64> = vec![post_size, post_size - 1, 8, post_size / 2];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..32 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let off = 8 + (x % (post_size - 7));
        offsets.push(off);
    }

    for &off in &offsets {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(off)
            .unwrap();
        // Open may succeed or fail; iter may yield anything or error. The
        // contract is just "no panic".
        if let Ok(pool) = Pool::open(&dir, cfg(), None) {
            let _ = pool.for_each_in_shard(0, |_, _| Ok(()));
        }
    }
}

#[test]
fn file_smaller_than_footer_errors() {
    // The only "totally bogus file" case we still surface as an error:
    // a non-empty file too small to contain a footer.
    let dir = common::scratch_dir("crash-tiny");
    let path = common::shard_path(&dir, 0);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&[0x11, 0x22, 0x33, 0x44]).unwrap();
    drop(f);
    assert!(Pool::open(&dir, cfg(), None).is_err());
}
