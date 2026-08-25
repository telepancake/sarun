//! Read-side wiring of the sharded title dictionary (browsing plan;
//! "wire the designed dictionary" work order).
//!
//! Import appends each normalized, namespace-qualified title once to the
//! strpool (shard = `fnv1a(title) % shard_count`). Fixed-width forward
//! and reverse slot files bind dense title ids to current page ids.
//!
//!   * [`lookup_ids`] — exact title → dense ids. It walks only the
//!     fnv-picked shard; writer-side dynamic re-sharding keeps that
//!     direct lookup unit small without an expanded dictionary cache.
//!   * [`scan_candidates`] — the pages-listing / substring-search scan:
//!     ALL shards walked in parallel (`std::thread::scope`), each
//!     thread keeping only the K smallest matching `(title, id)` pairs,
//!     merged into a globally byte-ordered candidate window. Bounded
//!     memory: never more than `threads * need` candidates resident.
//!
//! The pool stores normalized title bytes with namespace prefixes kept.
//! Exact matching is byte equality; substring matching is lossy-UTF-8
//! lowercase `contains`.

use strpool::Pool;

use crate::error::Result;

pub(crate) fn normalize_title(title: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(title).replace('_', " ");
    text.split_whitespace().collect::<Vec<_>>().join(" ").into_bytes()
}

/// FNV-1a 64-bit over the normalized title bytes — MUST stay in
/// lockstep with every title append and lookup.
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

/// Exact lookup scans one deliberately small shard directly. Dynamic
/// re-sharding keeps that unit bounded, so retaining expanded whole-shard
/// hash maps (formerly a 64 MiB request cache) is counterproductive.
pub(crate) fn lookup_ids(
    pool: &Pool,
    shard_count: u32,
    normalized: &[u8],
) -> Result<Vec<u64>> {
    let sid = shard_for(normalized, shard_count);
    let mut ids = Vec::new();
    pool.for_each_in_shard(sid, |id, bytes| {
        if bytes == normalized {
            ids.push(id);
        }
        Ok(())
    })?;
    Ok(ids)
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
