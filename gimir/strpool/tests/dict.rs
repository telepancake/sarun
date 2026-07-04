mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use strpool::{DictProvider, Pool, PoolConfig, StrpoolError};

struct MapProvider {
    inner: Mutex<HashMap<u32, Vec<u8>>>,
}

impl MapProvider {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
    fn insert(&self, id: u32, bytes: Vec<u8>) {
        self.inner.lock().unwrap().insert(id, bytes);
    }
}

impl DictProvider for MapProvider {
    fn dict(&self, id: u32) -> Result<Option<Vec<u8>>, StrpoolError> {
        Ok(self.inner.lock().unwrap().get(&id).cloned())
    }
}

fn small_cfg() -> PoolConfig {
    PoolConfig {
        shard_count: 1,
        seal_threshold_bytes: 128,
    }
}

fn sample_strings(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            format!(
                "common-prefix-shared-by-all-entries-payload-{}-item-{}",
                i % 7,
                i
            )
            .into_bytes()
        })
        .collect()
}

#[test]
fn trained_dict_seal_and_iter_roundtrip() {
    // Train a dict over a sample, store it under its embedded id, seal with
    // it, reopen with the provider, and verify iter returns the originals.
    let samples = sample_strings(500);
    let sample_refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
    let dict_bytes = zstd::dict::from_samples(&sample_refs, 1024).unwrap();
    let dict_id =
        zstd::zstd_safe::get_dict_id_from_dict(&dict_bytes).expect("trained dict must have id");
    let dict_id: u32 = dict_id.into();

    let provider = Arc::new(MapProvider::new());
    provider.insert(dict_id, dict_bytes.clone());
    let provider: Arc<dyn DictProvider> = provider;

    let dir = common::scratch_dir("dict-rt");
    let pool: Pool = Pool::open(&dir, small_cfg(), Some(Arc::clone(&provider))).unwrap();
    pool.set_dict(dict_id).unwrap();
    for s in &samples {
        pool.append(0, s).unwrap();
    }
    pool.flush(0).unwrap();
    assert!(pool.maybe_seal(0).unwrap(), "seal should have run");
    drop(pool);

    // Reopen with the provider; iter must return originals.
    let pool: Pool = Pool::open(&dir, small_cfg(), Some(provider)).unwrap();
    let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
    pool.for_each_in_shard(0, |id, bytes| {
        got.push((id, bytes.to_vec()));
        Ok(())
    })
    .unwrap();
    assert_eq!(got.len(), samples.len());
    for (i, (_, bytes)) in got.iter().enumerate() {
        assert_eq!(bytes, &samples[i]);
    }
}

#[test]
fn reopen_without_dict_surfaces_missing_dict() {
    // Same setup, then reopen with None (no dict resolution possible).
    let samples = sample_strings(500);
    let sample_refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
    let dict_bytes = zstd::dict::from_samples(&sample_refs, 1024).unwrap();
    let dict_id =
        zstd::zstd_safe::get_dict_id_from_dict(&dict_bytes).expect("trained dict must have id");
    let dict_id: u32 = dict_id.into();

    let provider = Arc::new(MapProvider::new());
    provider.insert(dict_id, dict_bytes);
    let provider: Arc<dyn DictProvider> = provider;

    let dir = common::scratch_dir("dict-missing");
    let pool: Pool = Pool::open(&dir, small_cfg(), Some(provider)).unwrap();
    pool.set_dict(dict_id).unwrap();
    for s in &samples {
        pool.append(0, s).unwrap();
    }
    pool.flush(0).unwrap();
    assert!(pool.maybe_seal(0).unwrap());
    drop(pool);

    let pool = Pool::open(&dir, small_cfg(), None).unwrap();
    let err = pool
        .for_each_in_shard(0, |_, _| Ok(()))
        .expect_err("iter must surface MissingDict for the sealed frame");
    match err {
        StrpoolError::MissingDict(id) => assert_eq!(id, dict_id),
        other => panic!("unexpected error: {other}"),
    }
}
