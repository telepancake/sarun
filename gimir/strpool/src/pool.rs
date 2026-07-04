//! Public [`Pool`] facade. Holds one [`Shard`] per file under the pool dir.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

use crate::error::StrpoolError;
use crate::shard::Shard;
use crate::{DictProvider, PoolConfig, Result};

pub struct Pool {
    cfg: PoolConfig,
    shard_bits: u8,
    shards: Vec<Mutex<Shard>>,
}

impl Pool {
    pub fn open(
        dir: &Path,
        cfg: PoolConfig,
        dict_provider: Option<Arc<dyn DictProvider>>,
    ) -> Result<Self> {
        assert!(cfg.shard_count >= 1, "shard_count must be >= 1");
        let shard_bits: u8 = if cfg.shard_count <= 1 {
            0
        } else {
            cfg.shard_count.next_power_of_two().trailing_zeros() as u8
        };
        std::fs::create_dir_all(dir)?;
        let mut shards = Vec::with_capacity(cfg.shard_count as usize);
        for id in 0..cfg.shard_count {
            let path = shard_path(dir, id);
            let shard = Shard::open(id, path, dict_provider.clone())?;
            shards.push(Mutex::new(shard));
        }
        Ok(Self {
            cfg,
            shard_bits,
            shards,
        })
    }

    pub fn append(&self, shard_id: u32, s: &[u8]) -> Result<u64> {
        let shard = self.shard(shard_id)?;
        let mut g = shard.lock().expect("shard mutex poisoned");
        let local = g.append(s)?;
        Ok(self.global_id(local, shard_id))
    }

    pub fn append_many(&self, shard_id: u32, strings: &[&[u8]]) -> Result<Vec<u64>> {
        let shard = self.shard(shard_id)?;
        let mut g = shard.lock().expect("shard mutex poisoned");
        let locals = g.append_many(strings)?;
        Ok(locals
            .into_iter()
            .map(|l| self.global_id(l, shard_id))
            .collect())
    }

    pub fn flush(&self, shard_id: u32) -> Result<()> {
        let shard = self.shard(shard_id)?;
        let mut g = shard.lock().expect("shard mutex poisoned");
        g.flush()
    }

    pub fn maybe_seal(&self, shard_id: u32) -> Result<bool> {
        let shard = self.shard(shard_id)?;
        let mut g = shard.lock().expect("shard mutex poisoned");
        g.maybe_seal(self.cfg.seal_threshold_bytes)
    }

    /// Set the dict id to use on the next seal for every shard.
    pub fn set_dict(&self, dict_id: u32) -> Result<()> {
        for shard in &self.shards {
            let mut g = shard.lock().expect("shard mutex poisoned");
            g.set_next_dict(dict_id);
        }
        Ok(())
    }

    /// Iterate every string in `shard_id`, invoking `f(global_id, &bytes)`.
    /// Holds the shard's lock for the duration. Returning `Err` from `f`
    /// stops iteration.
    pub fn for_each_in_shard<F: FnMut(u64, &[u8]) -> Result<()>>(
        &self,
        shard_id: u32,
        mut f: F,
    ) -> Result<()> {
        let shard = self.shard(shard_id)?;
        let g = shard.lock().expect("shard mutex poisoned");
        let shard_bits = self.shard_bits;
        g.for_each(|local, bytes| {
            let id = ((local as u64) << shard_bits) | (shard_id as u64);
            f(id, bytes)
        })
    }

    /// Parallel substring scan across all shards. Order across shards is
    /// unspecified; within a shard, results are in insertion order.
    pub fn scan_substring<'a>(
        &'a self,
        needle: &'a [u8],
    ) -> impl ParallelIterator<Item = Result<(u64, Vec<u8>)>> + 'a {
        (0..self.cfg.shard_count)
            .into_par_iter()
            .flat_map_iter(move |sid| {
                let mut hits: Vec<Result<(u64, Vec<u8>)>> = Vec::new();
                let res = self.for_each_in_shard(sid, |id, bytes| {
                    if memchr::memmem::find(bytes, needle).is_some() {
                        hits.push(Ok((id, bytes.to_vec())));
                    }
                    Ok(())
                });
                if let Err(e) = res {
                    hits.push(Err(e));
                }
                hits.into_iter()
            })
    }

    fn shard(&self, shard_id: u32) -> Result<&Mutex<Shard>> {
        if shard_id >= self.cfg.shard_count {
            return Err(StrpoolError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("shard_id {} out of range", shard_id),
            )));
        }
        Ok(&self.shards[shard_id as usize])
    }

    fn global_id(&self, local: u32, shard_id: u32) -> u64 {
        ((local as u64) << self.shard_bits) | (shard_id as u64)
    }
}

fn shard_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("shard-{:04}", id))
}
