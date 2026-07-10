//! Read-side wiring of the sharded title dictionary (browsing plan;
//! "wire the designed dictionary" work order).
//!
//! Import appends each `(ns, normalized_title)` ONCE to the strpool
//! (shard = `fnv1a(title) % shard_count`) and records the dense id in
//! `title_id_to_page` / `page_to_title_id`. This module is the READ
//! half:
//!
//!   * [`TitleCache`] — exact title → dense ids. One decompress-walk of
//!     the fnv-picked shard builds a byte-keyed map; a bounded-budget
//!     LRU keeps the hottest shard maps resident so a render's link set
//!     costs at most ONE walk per touched shard (repeat lookups are
//!     hash probes). This is the batching mechanism for render-time
//!     link resolution: the renderer discovers links *during* template
//!     expansion, so the "whole link set" is not knowable up front —
//!     the shard-granular cache gives the same I/O shape (one scan per
//!     touched shard per render) without a pre-pass.
//!   * [`scan_candidates`] — the pages-listing / substring-search scan:
//!     ALL shards walked in parallel (`std::thread::scope`), each
//!     thread keeping only the K smallest matching `(title, id)` pairs,
//!     merged into a globally byte-ordered candidate window. Bounded
//!     memory: never more than `threads * need` candidates resident.
//!
//! The pool stores title BYTES exactly as import normalized them
//! (`page.title.trim()`, namespace prefix kept); matching semantics
//! here must mirror the sqlite reads they replace — exact = byte
//! equality, substring filter = lossy-UTF-8 lowercase `contains`, the
//! same rule `Instance::pages` has always applied.

use std::collections::HashMap;

use strpool::Pool;

use crate::error::Result;

/// FNV-1a 64-bit over the normalized title bytes — MUST stay in
/// lockstep with import.rs's private `fnv1a` (the shard picker used at
/// append time); a divergence would send lookups to the wrong shard.
/// Pinned by the one-shard-per-exact-lookup acceptance test, which
/// fails loudly if the two hashes ever disagree.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The shard a normalized title lives in (import's placement rule).
pub(crate) fn shard_for(normalized: &[u8], shard_count: u32) -> u32 {
    if shard_count <= 1 {
        0
    } else {
        (fnv1a(normalized) % shard_count as u64) as u32
    }
}

/// One decompressed shard, indexed for exact lookup. `ids` is a Vec
/// because the pool may hold the same bytes more than once (two
/// namespaces sharing a prefixed title, or an append whose sqlite
/// transaction rolled back followed by a re-append).
struct CachedShard {
    /// `Pool::shard_entry_count` at build time; append-only pool, so a
    /// changed stamp means "rebuild", never "silently wrong".
    stamp: u32,
    map: HashMap<Vec<u8>, Vec<u64>>,
    /// Approximate resident bytes (keys + entries), for the budget.
    bytes: usize,
    /// LRU clock value of the last hit.
    last_used: u64,
}

/// Bounded LRU of decompressed-and-indexed title shards. Lives on
/// `InstanceInner` (under the instance mutex), so no interior locking.
pub(crate) struct TitleCache {
    budget_bytes: usize,
    used_bytes: usize,
    clock: u64,
    shards: HashMap<u32, CachedShard>,
}

/// Default cache budget: a few MB, per the render-batching design note.
/// Enough for every test-scale pool and for several real shards; a
/// render touching more shards than fit re-scans on the LRU boundary,
/// trading RAM for I/O exactly like any cache.
pub(crate) const TITLE_CACHE_BUDGET: usize = 8 << 20;

impl TitleCache {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        TitleCache { budget_bytes, used_bytes: 0, clock: 0, shards: HashMap::new() }
    }

    /// Dense ids whose pool bytes equal `normalized` — the exact-title
    /// lookup. Touches only the fnv-picked shard: a cache hit (fresh
    /// stamp) is a hash probe with NO pool I/O; a miss walks that one
    /// shard once and indexes it.
    pub(crate) fn lookup(
        &mut self,
        pool: &Pool,
        shard_count: u32,
        normalized: &[u8],
    ) -> Result<Vec<u64>> {
        let sid = shard_for(normalized, shard_count);
        self.clock += 1;
        let stamp = pool.shard_entry_count(sid)?;
        let fresh = self.shards.get(&sid).is_some_and(|c| c.stamp == stamp);
        if !fresh {
            self.rebuild(pool, sid, stamp)?;
        }
        let entry = self.shards.get_mut(&sid).expect("just ensured");
        entry.last_used = self.clock;
        Ok(entry.map.get(normalized).cloned().unwrap_or_default())
    }

    fn rebuild(&mut self, pool: &Pool, sid: u32, stamp: u32) -> Result<()> {
        if let Some(old) = self.shards.remove(&sid) {
            self.used_bytes -= old.bytes;
        }
        let mut map: HashMap<Vec<u8>, Vec<u64>> = HashMap::new();
        let mut bytes = 0usize;
        pool.for_each_in_shard(sid, |id, b| {
            bytes += b.len() + std::mem::size_of::<u64>() * 2;
            map.entry(b.to_vec()).or_default().push(id);
            Ok(())
        })?;
        // Evict least-recently-used shards until the newcomer fits. A
        // single shard larger than the whole budget still gets cached
        // (alone) — refusing it would re-walk the shard on every
        // lookup, the exact cost the cache exists to avoid.
        while self.used_bytes + bytes > self.budget_bytes && !self.shards.is_empty() {
            let coldest = *self
                .shards
                .iter()
                .min_by_key(|(_, c)| c.last_used)
                .map(|(k, _)| k)
                .expect("non-empty");
            let old = self.shards.remove(&coldest).expect("present");
            self.used_bytes -= old.bytes;
        }
        self.used_bytes += bytes;
        self.shards.insert(sid, CachedShard { stamp, map, bytes, last_used: self.clock });
        Ok(())
    }
}

/// One pool hit from [`scan_candidates`]: the title bytes and the dense
/// id they carry.
pub(crate) type Candidate = (Vec<u8>, u64);

/// The result of one scan pass: candidates in ascending `(title, id)`
/// order, and — when any per-thread heap overflowed — the exclusive
/// upper bound the caller must window the NEXT pass from. Candidates
/// above the bound were dropped (some thread may hold smaller unseen
/// ones past its heap), so the returned list is exactly the globally
/// smallest matches in `(window_lo, bound]`.
pub(crate) struct ScanPass {
    pub candidates: Vec<Candidate>,
    pub next_window: Option<Candidate>,
}

/// Walk EVERY shard in parallel (`std::thread::scope`, shards chunked
/// over at most `MAX_SCAN_THREADS` threads), keeping per thread the
/// `need` smallest `(title, id)` pairs that satisfy `matches` and sort
/// strictly above `window_lo`. Memory is bounded by `threads * need`
/// candidates; each pass costs exactly one walk per shard (visible in
/// `Pool::scan_counts`).
pub(crate) fn scan_candidates(
    pool: &Pool,
    shard_count: u32,
    matches: &(dyn Fn(&[u8]) -> bool + Sync),
    need: usize,
    window_lo: Option<&Candidate>,
) -> Result<ScanPass> {
    use std::collections::BinaryHeap;

    const MAX_SCAN_THREADS: usize = 8;
    if need == 0 {
        return Ok(ScanPass { candidates: Vec::new(), next_window: None });
    }
    let threads = (shard_count as usize).clamp(1, MAX_SCAN_THREADS);

    // Per-thread: max-heap of the `need` smallest seen, plus an
    // overflow bound (everything discarded was >= the final heap max).
    type ThreadOut = Result<(BinaryHeap<Candidate>, bool)>;
    let mut outs: Vec<ThreadOut> = Vec::with_capacity(threads);
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(threads);
        for t in 0..threads {
            handles.push(s.spawn(move || -> ThreadOut {
                let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();
                let mut overflowed = false;
                let mut sid = t as u32;
                while sid < shard_count {
                    pool.for_each_in_shard(sid, |id, bytes| {
                        if !matches(bytes) {
                            return Ok(());
                        }
                        if let Some(lo) = window_lo {
                            if (bytes, id) <= (lo.0.as_slice(), lo.1) {
                                return Ok(());
                            }
                        }
                        if heap.len() == need {
                            let max = heap.peek().expect("non-empty");
                            if (bytes, id) >= (max.0.as_slice(), max.1) {
                                overflowed = true;
                                return Ok(());
                            }
                            heap.pop();
                            overflowed = true;
                        }
                        heap.push((bytes.to_vec(), id));
                        Ok(())
                    })?;
                    sid += threads as u32;
                }
                Ok((heap, overflowed))
            }));
        }
        for h in handles {
            outs.push(h.join().expect("title scan thread panicked"));
        }
    });

    // The safe window: candidates <= min over overflowed threads of
    // their kept maximum. Anything above that bound may be shadowed by
    // a discarded-but-smaller candidate in another thread.
    let mut bound: Option<Candidate> = None;
    let mut merged: Vec<Candidate> = Vec::new();
    for out in outs {
        let (heap, overflowed) = out?;
        let items = heap.into_sorted_vec();
        if overflowed {
            let thread_max = items.last().cloned().expect("overflow implies non-empty");
            bound = Some(match bound {
                None => thread_max,
                Some(b) => b.min(thread_max),
            });
        }
        merged.extend(items);
    }
    if let Some(b) = &bound {
        merged.retain(|c| c <= b);
    }
    merged.sort();
    Ok(ScanPass { candidates: merged, next_window: bound })
}
