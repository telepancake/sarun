//! Persisted union-frame store — the missing wiring between Path B's
//! union/delta primitives (`layer`, `geostack`, `lanes`) and Path A's VBF
//! persistence (`wikimak_depot::Depot`, the same engine `store.rs` drives).
//!
//! `frame::Frame` holds the §7 lifecycle (a `refPrefix` full-state union plus a
//! geometric delta stack) entirely in RAM; its `seal()` discards history. This
//! module promotes that exact lifecycle onto a real `Depot` so the union bytes
//! land as VBF frames and a historical ref's tree reconstructs SHA-exact from
//! the STORED bytes — read back through the depot, never re-derived from git:
//!
//! * **refPrefix** (the full-state union, `layer::encode_union`) is the BASE
//!   chain's **f0** record — one standalone codec blob (DESIGN §8, terminology
//!   map: refPrefix = TREES f0).
//! * each **live delta layer** produced by `layer::delta_multi_lane_stacked`
//!   (reslot-by-oid, current state read streaming from base+stack — never a
//!   materialized union, §5.1) is **prepended** to the DELTAS chain as its own
//!   VBF frame, newest-first: the new delta is the new f0, the previous f0 is
//!   demoted verbatim into the f1 accumulator, and the accumulator seals to a
//!   cold frame past the threshold (`Depot::seal_f1`) — the identical
//!   prepend/seal discipline `store.rs::prepend_batch` uses, reused unchanged.
//! * a **read or a seal** materializes the union
//!   (`overlay_full(base, collapse(deltas))`, holes dissolve, §4) — the ONLY
//!   place a full union is built. Serving a lane's git tree
//!   (`layer::reconstruct_lane_tree_oid`) is such a read.
//!
//! The union codec bytes ARE `depot::codec` bytes, so the frame machinery
//! carries them unchanged (workmap §3): this is a thin persistence adapter over
//! the existing Depot, not a second engine. The lane→tree-oid map that feeds
//! `advance` is reflog-derived (`reflog.rs`) and lives in RAM for the ingest;
//! it is supplied by the caller, exactly as `frame::Frame` takes lane trees.
//!
//! DEFERRED (see IMPL-NOTES): a persisted *seal* that rewrites BASE to the
//! collapsed union and re-encodes the folded-in history as REVERSE deltas from
//! the new full-state (DESIGN §5.3, "f0 full, everything else reverse"). The
//! current forward-delta frame lifecycle is fully persisted and reconstructs
//! every live revision from stored bytes; cross-seal reverse reconstruction is
//! the one primitive `frame.rs` itself does not yet carry.

use std::path::Path;

use wikimak_depot::{Depot, DepotConfig, FrameDecoder, FrameEncoder};

use crate::geostack::GeoStack;
use crate::layer::{self, LaneTree};
use crate::{Error, Result};

/// refPrefix (full-state union) — one f0 record.
const BASE: u64 = 0;
/// Forward delta layers, newest-first (f0 = newest delta, f1/cold = older).
const DELTAS: u64 = 1;
const MAX_CHAIN_ID: u64 = 2;

/// f1 accumulator seal point (raw bytes), matching `store.rs`. Small; the
/// current write file's slack is the dead-byte ceiling.
const SEAL_THRESHOLD: u64 = 256 * 1024;
const FILE_SIZE_THRESHOLD: u64 = 4 << 20;
const EVICTION_DEAD_RATIO: f32 = 0.5;

fn chain_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Chain(e.to_string())
}

fn compress(src: &[u8], level: i32) -> Result<Vec<u8>> {
    wikimak_depot::compress_frame(src, None, level).map_err(Error::Chain)
}

fn decompress(frame: &[u8]) -> Result<Vec<u8>> {
    wikimak_depot::decompress_frame(frame, None).map_err(Error::Chain)
}

/// A shard's persisted union frame: the BASE refPrefix + the DELTAS chain, with
/// an in-RAM geometric stack (rebuilt from the persisted deltas on open) so the
/// write side reads the current state as base + a bounded ~log(n) stack, never a
/// materialized union.
pub struct UnionStore {
    depot: Depot,
    level: i32,
    /// The refPrefix, cached from BASE f0 (source of truth is the depot).
    base: Vec<u8>,
    /// The live delta layers as a geometric stack (§7), rebuilt from the
    /// persisted DELTAS chain — kept shallow for the streaming current read.
    stack: GeoStack<Vec<u8>>,
    /// Current lane trees, for the reslot's free oid lookup on the next
    /// advance (§5.2). Empty after a bare `open` — the caller supplies them
    /// (reflog-derived) before advancing.
    lanes: Vec<LaneTree>,
}

/// delta ∘ delta merge for the stack: `compose_stream(lower, upper)` (holes
/// survive, §4). A fully-annihilating merge yields an empty-but-decodable layer.
fn compose(lower: Vec<u8>, upper: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    depot::stream::compose_stream(&lower, &upper, &mut out)
        .expect("compose_stream on canonical layers");
    if out.is_empty() {
        layer::empty_union()
    } else {
        out
    }
}

fn size(l: &Vec<u8>) -> u64 {
    l.len() as u64
}

impl UnionStore {
    fn open_depot(dir: &Path) -> Result<Depot> {
        Depot::open(DepotConfig {
            root: dir.to_path_buf(),
            max_chain_id: MAX_CHAIN_ID,
            file_size_threshold: FILE_SIZE_THRESHOLD,
            eviction_dead_ratio: EVICTION_DEAD_RATIO,
        })
        .map_err(chain_err)
    }

    /// Open (creating if absent) a union store at `dir`, rebuilding the in-RAM
    /// geometric stack from the persisted deltas. Lane trees are NOT restored
    /// (they are reflog-derived); a reopened store reconstructs any revision
    /// from stored bytes but must be handed lane trees before it can `advance`.
    pub fn open(dir: &Path, level: i32) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let depot = Self::open_depot(dir)?;
        let mut s = UnionStore { depot, level, base: Vec::new(), stack: GeoStack::new(), lanes: Vec::new() };
        s.base = s.read_base()?;
        for d in s.read_deltas_oldest_first()? {
            s.stack.push(d, size, compose);
        }
        Ok(s)
    }

    /// Seed the refPrefix from the initial lanes: BASE f0 = their union, the
    /// delta stack empty. Errors if the store is already seeded.
    pub fn seed(&mut self, lanes: Vec<LaneTree>) -> Result<()> {
        if !self.base.is_empty() && self.base != layer::empty_union() {
            return Err(Error::Chain("union store already seeded".into()));
        }
        if self.n_deltas() != 0 {
            return Err(Error::Chain("union store already has deltas".into()));
        }
        let base = layer::encode_union(&lanes);
        // Fresh BASE chain: seed as the sole f0 record (no f1 on a first prepend).
        self.depot
            .prepend(BASE, &compress(&base, self.level)?, None, false)
            .map_err(chain_err)?;
        self.base = base;
        self.lanes = lanes;
        Ok(())
    }

    /// Set the current lane trees after a bare `open`, so the next `advance`
    /// has the old lanes' oids to reslot against (§5.2). The caller derives
    /// these from the reflog (`reflog.rs`).
    pub fn set_lanes(&mut self, lanes: Vec<LaneTree>) {
        self.lanes = lanes;
    }

    /// Advance to a new set of lane trees (a written layer): generate the delta
    /// against the current state (base + live stack, read streaming — no union
    /// materialized, §5.1), PERSIST it as a VBF frame prepend on the DELTAS
    /// chain, and push it on the geometric stack.
    pub fn advance(&mut self, new_lanes: Vec<LaneTree>) -> Result<()> {
        let delta = layer::delta_multi_lane_stacked(
            &self.base,
            self.stack.layers(),
            &self.lanes,
            &new_lanes,
        );
        self.prepend_delta(&delta)?;
        self.stack.push(delta, size, compose);
        self.lanes = new_lanes;
        Ok(())
    }

    /// The number of persisted delta layers.
    pub fn n_deltas(&self) -> usize {
        let mut n = 0usize;
        // Cheap: the RAM stack does not equal the raw delta count (it is
        // compacted), so count the persisted records instead.
        self.walk_delta_records(&mut |_| {
            n += 1;
            Ok(false)
        })
        .expect("delta walk");
        n
    }

    /// Flush pending depot writes durable.
    pub fn flush(&self) -> Result<()> {
        self.depot.flush().map_err(chain_err)
    }

    // --------------------------------------------------------- read side

    /// The BASE refPrefix (from stored f0); an unseeded chain reads as the
    /// canonical empty union.
    fn read_base(&self) -> Result<Vec<u8>> {
        match self.depot.read_f0(BASE) {
            Ok(frame) => decompress(&frame),
            Err(wikimak_depot::Error::NoFrame) => Ok(layer::empty_union()),
            Err(e) => Err(chain_err(e)),
        }
    }

    /// Walk the persisted DELTAS records newest-first (the same f0 → f1 → cold
    /// walk `store.rs::walk_records` uses; f1/cold anchored on the preceding
    /// record). `visit` returns true to stop.
    fn walk_delta_records(
        &self,
        visit: &mut dyn FnMut(Vec<u8>) -> Result<bool>,
    ) -> Result<()> {
        let head = match self.depot.read_f0(DELTAS) {
            Ok(frame) => decompress(&frame)?,
            Err(wikimak_depot::Error::NoFrame) => return Ok(()),
            Err(e) => return Err(chain_err(e)),
        };
        if visit(head.clone())? {
            return Ok(());
        }
        let mut anchor = head;
        let one_frame = |frame: &[u8],
                         anchor: &mut Vec<u8>,
                         visit: &mut dyn FnMut(Vec<u8>) -> Result<bool>|
         -> Result<bool> {
            let mut stopped = false;
            let mut last: Option<Vec<u8>> = None;
            stream_frame_records(frame, anchor, &mut |rec| {
                stopped = visit(rec.clone())?;
                last = Some(rec);
                Ok(stopped)
            })?;
            if let Some(l) = last {
                *anchor = l;
            }
            Ok(stopped)
        };
        if let Some(f1) = self.depot.read_f1(DELTAS).map_err(chain_err)? {
            if one_frame(&f1, &mut anchor, visit)? {
                return Ok(());
            }
        }
        for cold in self.depot.cold_iter(DELTAS).map_err(chain_err)? {
            let frame = cold.map_err(chain_err)?;
            if one_frame(&frame, &mut anchor, visit)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// The persisted delta layers oldest-first — the apply order a reader
    /// overlays onto the base.
    fn read_deltas_oldest_first(&self) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        self.walk_delta_records(&mut |rec| {
            out.push(rec);
            Ok(false)
        })?;
        out.reverse();
        Ok(out)
    }

    /// Materialize the full union at the current tip, read entirely from stored
    /// bytes: `overlay_full(base, collapse(all deltas))`. A read/seal-only path
    /// (§4, §7) — never used to generate a delta.
    pub fn union(&self) -> Result<Vec<u8>> {
        self.union_at(usize::MAX)
    }

    /// As [`union`], but overlaying only the first `n_deltas` persisted deltas
    /// (oldest-first) — the full-state as of that revision, served from stored
    /// bytes. `n_deltas >= total` yields the current tip.
    pub fn union_at(&self, n_deltas: usize) -> Result<Vec<u8>> {
        let base = self.read_base()?;
        let deltas = self.read_deltas_oldest_first()?;
        let take = n_deltas.min(deltas.len());
        if take == 0 {
            return Ok(base);
        }
        let mut combined = deltas[0].clone();
        for d in &deltas[1..take] {
            let mut next = Vec::new();
            depot::stream::compose_stream(&combined, d, &mut next)?;
            combined = if next.is_empty() { layer::empty_union() } else { next };
        }
        let mut out = Vec::new();
        depot::stream::overlay_full(&base, &combined, &mut out)?;
        Ok(if out.is_empty() { layer::empty_union() } else { out })
    }

    /// Reconstruct lane `lane`'s git tree oid at the current tip, SHA-exact,
    /// FROM STORED BYTES (base + deltas read back through the depot). The
    /// deliverable: a historical ref's tree served from the persisted VBF
    /// frames, not re-derived from git.
    pub fn reconstruct_lane(&self, lane: u32) -> Result<String> {
        let union = self.union()?;
        layer::reconstruct_lane_tree_oid(&union, lane).map_err(Error::from)
    }

    /// Reconstruct lane `lane`'s tree oid as of the revision after applying the
    /// first `n_deltas` persisted deltas — a historical revision, from stored
    /// bytes.
    pub fn reconstruct_lane_at(&self, n_deltas: usize, lane: u32) -> Result<String> {
        let union = self.union_at(n_deltas)?;
        layer::reconstruct_lane_tree_oid(&union, lane).map_err(Error::from)
    }

    // -------------------------------------------------------- write side

    /// Prepend one delta layer as a VBF frame on the DELTAS chain: the delta is
    /// the new f0; the previous f0 is demoted verbatim into the f1 accumulator
    /// (anchored on the new record), which seals to cold past the threshold.
    /// This is `store.rs::prepend_batch` specialized to a single verbatim
    /// record.
    fn prepend_delta(&self, record: &[u8]) -> Result<()> {
        let level = self.level;
        let prev = match self.depot.read_f0(DELTAS) {
            Ok(frame) => Some(decompress(&frame)?),
            Err(wikimak_depot::Error::NoFrame) => None,
            Err(e) => return Err(chain_err(e)),
        };
        let Some(prev_record) = prev else {
            // Seed the DELTAS chain: the depot forbids f1 on a first prepend.
            self.depot
                .prepend(DELTAS, &compress(record, level)?, None, false)
                .map_err(chain_err)?;
            return Ok(());
        };
        let entries_len = prev_record.len() as u64 + 8;
        let old_f1 = self.depot.read_f1(DELTAS).map_err(chain_err)?;
        let old_raw_len = match &old_f1 {
            Some(z) => zstd::zstd_safe::get_frame_content_size(z)
                .map_err(|_| Error::Chain("zstd frame content size".into()))?
                .ok_or_else(|| Error::Chain("zstd frame without content size".into()))?,
            None => 0,
        };
        let seal_old = old_f1.is_some() && old_raw_len + entries_len > SEAL_THRESHOLD;
        let total_raw = entries_len + if seal_old { 0 } else { old_raw_len };
        let mut enc = FrameEncoder::new(total_raw, Some(record), level).map_err(Error::Chain)?;
        enc.write(&(prev_record.len() as u64).to_le_bytes()).map_err(Error::Chain)?;
        enc.write(&prev_record).map_err(Error::Chain)?;
        if !seal_old {
            if let Some(z) = &old_f1 {
                use std::io::Read as _;
                let mut dec = FrameDecoder::new(z, Some(&prev_record)).map_err(Error::Chain)?;
                let mut buf = vec![0u8; 128 << 10];
                loop {
                    let n = dec.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    enc.write(&buf[..n]).map_err(Error::Chain)?;
                }
            }
        }
        let new_f1 = enc.finish().map_err(Error::Chain)?;
        let new_f0 = compress(record, level)?;
        self.depot
            .prepend(DELTAS, &new_f0, Some(&new_f1), seal_old)
            .map_err(chain_err)?;
        if total_raw > SEAL_THRESHOLD {
            self.depot.seal_f1(DELTAS).map_err(chain_err)?;
        }
        Ok(())
    }
}

/// Stream the u64-length-prefixed records of a multi-record frame (f1 or cold)
/// newest-first, one record in RAM at a time — the same streaming
/// `store.rs::stream_frame_records` does. `visit` returns true to stop early.
fn stream_frame_records(
    frame: &[u8],
    prefix: &[u8],
    visit: &mut dyn FnMut(Vec<u8>) -> Result<bool>,
) -> Result<()> {
    use std::io::Read as _;
    let mut dec = FrameDecoder::new(frame, Some(prefix)).map_err(Error::Chain)?;
    let read_full = |dec: &mut FrameDecoder<'_>, buf: &mut [u8]| -> Result<usize> {
        let mut got = 0;
        while got < buf.len() {
            let n = dec.read(&mut buf[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok(got)
    };
    loop {
        let mut hdr = [0u8; 8];
        match read_full(&mut dec, &mut hdr)? {
            0 => return Ok(()),
            8 => {}
            _ => return Err(Error::Chain("truncated record".into())),
        }
        let len = u64::from_le_bytes(hdr) as usize;
        let mut rec = vec![0u8; len];
        if read_full(&mut dec, &mut rec)? != len {
            return Err(Error::Chain("truncated record".into()));
        }
        if visit(rec)? {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::{LaneEntry, LaneTree, Mode};
    use std::collections::BTreeMap;

    fn lane(entries: &[(&[u8], Mode, &[u8], &[u8])]) -> LaneTree {
        entries
            .iter()
            .map(|(p, m, oid, c)| {
                (p.to_vec(), LaneEntry { mode: *m, oid: oid.to_vec(), content: c.to_vec() })
            })
            .collect()
    }

    /// Seed + advance a multi-lane history, then REOPEN the depot and
    /// reconstruct every lane at every revision SHA-exact from the STORED bytes
    /// — the persistence round-trip the union engine was missing.
    #[test]
    fn persisted_union_reconstructs_sha_exact() {
        let tmp = match tempfile::tempdir() {
            Ok(d) => d,
            Err(_) => return,
        };
        let dir = tmp.path().join("depot");

        let s0 = vec![
            lane(&[
                (b"README", Mode::File, b"r0", b"hi\n"),
                (b"src/main.rs", Mode::File, b"m0", b"fn main(){}\n"),
                (b"src/run.sh", Mode::Exec, b"x0", b"#!\n"),
            ]),
            lane(&[
                (b"README", Mode::File, b"r0", b"hi\n"),
                (b"src/main.rs", Mode::File, b"m1", b"fn main(){2}\n"),
                (b"link", Mode::Symlink, b"l0", b"README"),
            ]),
        ];
        // Revision 1: lane 0 edits README + drops +x, adds a file.
        let mut s1 = s0.clone();
        s1[0] = lane(&[
            (b"README", Mode::File, b"r1", b"hello\n"),
            (b"src/main.rs", Mode::File, b"m0", b"fn main(){}\n"),
            (b"src/run.sh", Mode::File, b"x1", b"#!\n"),
            (b"CHANGELOG", Mode::File, b"c0", b"- x\n"),
        ]);
        // Revision 2: lane 1 collapses main.rs to lane 0's, adds a gitlink.
        let mut s2 = s1.clone();
        s2[1] = lane(&[
            (b"README", Mode::File, b"r0", b"hi\n"),
            (b"src/main.rs", Mode::File, b"m0", b"fn main(){}\n"),
            (b"dep", Mode::Gitlink, b"g0", b"0123456789abcdef0123456789abcdef01234567"),
        ]);

        {
            let mut us = UnionStore::open(&dir, 3).unwrap();
            us.seed(s0.clone()).unwrap();
            us.advance(s1.clone()).unwrap();
            us.advance(s2.clone()).unwrap();
            us.flush().unwrap();
        }

        // Reopen: reconstruction now comes only from stored VBF frames.
        let us = UnionStore::open(&dir, 3).unwrap();
        assert_eq!(us.n_deltas(), 2, "two persisted delta layers");

        // Current tip: every lane's tree oid matches building it directly.
        for (j, t) in s2.iter().enumerate() {
            assert_eq!(
                us.reconstruct_lane(j as u32).unwrap(),
                layer::lanetree_tree_oid(t).unwrap(),
                "tip lane {j}",
            );
        }
        // Historical revision after 1 delta (== s1): from stored bytes.
        for (j, t) in s1.iter().enumerate() {
            assert_eq!(
                us.reconstruct_lane_at(1, j as u32).unwrap(),
                layer::lanetree_tree_oid(t).unwrap(),
                "rev1 lane {j}",
            );
        }
        // The seed revision (0 deltas == s0).
        for (j, t) in s0.iter().enumerate() {
            assert_eq!(
                us.reconstruct_lane_at(0, j as u32).unwrap(),
                layer::lanetree_tree_oid(t).unwrap(),
                "rev0 lane {j}",
            );
        }
    }

    /// Force the DELTAS chain past the f1 seal threshold with many largish
    /// advances so records spill through f0 -> f1 (multi-record) -> cold, then
    /// REOPEN and reconstruct EVERY historical revision's lane tree SHA-exact
    /// from stored bytes. This exercises the newly-wired code the 2-advance test
    /// never reaches: the multi-record, anchor-chained `stream_frame_records`
    /// walk, `seal_f1`, and `cold_iter` — the actual persistence risk surface.
    #[test]
    fn persisted_seals_to_cold_and_reconstructs_every_revision() {
        let tmp = match tempfile::tempdir() {
            Ok(d) => d,
            Err(_) => return,
        };
        let dir = tmp.path().join("depot");

        // N revisions (rev0 = seed). Each advance adds a unique ~40 KiB file to
        // lane 0, so each delta is ~40 KiB raw; well past 16 revisions the
        // accumulator (256 KiB threshold) has sealed to cold several times.
        const N: usize = 20;
        let build = |k: usize| -> Vec<LaneTree> {
            // lane 0: README + one growing set of large data files f1..=fk.
            let mut lane0: LaneTree = BTreeMap::new();
            lane0.insert(
                b"README".to_vec(),
                LaneEntry { mode: Mode::File, oid: b"r0".to_vec(), content: b"hi\n".to_vec() },
            );
            for j in 1..=k {
                let mut content = vec![(j as u8).wrapping_mul(37); 40 * 1024];
                content.extend_from_slice(format!("rev{j}").as_bytes());
                lane0.insert(
                    format!("src/f{j:03}.dat").into_bytes(),
                    LaneEntry {
                        mode: Mode::File,
                        oid: format!("blob{j:04}").into_bytes(),
                        content,
                    },
                );
            }
            // lane 1: README whose blob oid flips every other revision, plus a
            // stable file — so a non-lane-0 lane also changes across history.
            let mut lane1: LaneTree = BTreeMap::new();
            lane1.insert(
                b"README".to_vec(),
                LaneEntry {
                    mode: Mode::File,
                    oid: format!("r{}", k / 2).into_bytes(),
                    content: b"hi\n".to_vec(),
                },
            );
            lane1.insert(
                b"a.txt".to_vec(),
                LaneEntry { mode: Mode::File, oid: b"a0".to_vec(), content: b"a\n".to_vec() },
            );
            vec![lane0, lane1]
        };

        let revisions: Vec<Vec<LaneTree>> = (0..N).map(build).collect();

        {
            let mut us = UnionStore::open(&dir, 3).unwrap();
            us.seed(revisions[0].clone()).unwrap();
            for rev in &revisions[1..] {
                us.advance(rev.clone()).unwrap();
            }
            us.flush().unwrap();
        }

        // Cold frames live at `<root>/cold/cold`; it is created empty on open
        // and only grows when `seal_f1` retires the accumulator to cold. A
        // non-empty cold file is hard proof the seal-and-cold-walk path was
        // actually exercised — else this test is no stronger than the 2-advance
        // one.
        let cold_len = std::fs::metadata(dir.join("cold").join("cold"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(
            cold_len > 0,
            "DELTAS chain must have sealed at least one cold frame \
             (cold file is {cold_len} bytes); the multi-tier walk is unexercised otherwise",
        );

        // Reopen: reconstruction now comes only from stored VBF frames.
        let us = UnionStore::open(&dir, 3).unwrap();
        assert_eq!(us.n_deltas(), N - 1, "one delta per advance");

        // Every historical revision k, every lane, SHA-exact from stored bytes.
        for (k, rev) in revisions.iter().enumerate() {
            for (j, t) in rev.iter().enumerate() {
                assert_eq!(
                    us.reconstruct_lane_at(k, j as u32).unwrap(),
                    layer::lanetree_tree_oid(t).unwrap(),
                    "rev{k} lane {j} from stored bytes",
                );
            }
        }
        // Tip via the un-parameterized path too.
        for (j, t) in revisions[N - 1].iter().enumerate() {
            assert_eq!(
                us.reconstruct_lane(j as u32).unwrap(),
                layer::lanetree_tree_oid(t).unwrap(),
                "tip lane {j}",
            );
        }
    }
}
