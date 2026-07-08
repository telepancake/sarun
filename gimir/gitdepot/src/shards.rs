//! The sharding harness (ASSEMBLY.md §1): a mirror is split into
//! `2^shard_bits` shards, each an independent [`Frame`] (its own refPrefix,
//! delta stack, and lanes). A path is routed to a shard by the top `shard_bits`
//! of a stable hash of its **full git path** (NOT including the `\0<slot>`
//! variant tag), so every version of a path lands in the same shard and the
//! split is stable across re-shard. Shards share the git object source but keep
//! completely separate delta state, so an advance runs one thread per shard.
//!
//! Sharding is for storage/delta locality only — it never changes identity. A
//! lane's git tree spans all shards, so reconstruction gathers that lane's
//! entries from every shard and hashes them together (§6): the shard split is
//! invisible in the resulting tree oid.

use crate::frame::Frame;
use crate::layer::{self, LaneTree};

/// FNV-1a over the full git path. Cheap, stable, no external dep; the exact
/// function is an OPEN choice in §1 — this is a placeholder with the required
/// stability, swappable for xxhash/sha-prefix without touching the harness.
fn path_hash(path: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in path {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A sharded mirror: `2^shard_bits` independent frames.
pub struct Shards {
    shard_bits: u32,
    frames: Vec<Frame>,
}

/// Which shard a path belongs to: the top `shard_bits` of its hash.
fn shard_of(path: &[u8], shard_bits: u32) -> usize {
    if shard_bits == 0 {
        return 0;
    }
    (path_hash(path) >> (64 - shard_bits)) as usize
}

/// Split `lanes` into one per-shard lane-tree vector: shard `s`'s entry for
/// lane `j` holds exactly lane `j`'s paths that route to `s` (possibly empty).
fn split(lanes: &[LaneTree], shard_bits: u32) -> Vec<Vec<LaneTree>> {
    let n = 1usize << shard_bits;
    let mut out: Vec<Vec<LaneTree>> = (0..n).map(|_| vec![LaneTree::new(); lanes.len()]).collect();
    for (j, tree) in lanes.iter().enumerate() {
        for (path, e) in tree {
            let s = shard_of(path, shard_bits);
            out[s][j].insert(path.clone(), e.clone());
        }
    }
    out
}

impl Shards {
    /// Seed a sharded mirror from the initial lanes.
    pub fn seed(shard_bits: u32, lanes: Vec<LaneTree>) -> Self {
        let per_shard = split(&lanes, shard_bits);
        let frames = per_shard.into_iter().map(Frame::seed).collect();
        Shards { shard_bits, frames }
    }

    /// Number of shards.
    pub fn n_shards(&self) -> usize {
        self.frames.len()
    }

    /// Advance every shard to the new lanes, one thread per shard (shards keep
    /// separate delta state, so this is embarrassingly parallel).
    pub fn advance(&mut self, new_lanes: Vec<LaneTree>) {
        let per_shard = split(&new_lanes, self.shard_bits);
        std::thread::scope(|scope| {
            for (frame, sub) in self.frames.iter_mut().zip(per_shard) {
                scope.spawn(move || frame.advance(sub));
            }
        });
    }

    /// Seal every shard (one thread per shard).
    pub fn seal(&mut self) {
        std::thread::scope(|scope| {
            for frame in self.frames.iter_mut() {
                scope.spawn(move || frame.seal());
            }
        });
    }

    /// Reconstruct lane `lane`'s full git tree oid: gather that lane's entries
    /// from every shard, then hash the combined tree once. The shard split does
    /// not affect the oid.
    pub fn reconstruct_tree_oid(&self, lane: u32) -> Result<String, depot::walk::DecodeError> {
        let mut entries = Vec::new();
        for frame in &self.frames {
            entries.extend(layer::extract_lane_entries(&frame.union(), lane)?);
        }
        layer::tree_oid_of_entries(&entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::{LaneEntry, LaneTree, Mode};

    fn lane(entries: &[(&[u8], Mode, &[u8], &[u8])]) -> LaneTree {
        entries
            .iter()
            .map(|(p, m, oid, c)| {
                (p.to_vec(), LaneEntry { mode: *m, oid: oid.to_vec(), content: c.to_vec() })
            })
            .collect()
    }

    fn assert_all_lanes(s: &Shards, lanes: &[LaneTree]) {
        for (j, t) in lanes.iter().enumerate() {
            assert_eq!(
                s.reconstruct_tree_oid(j as u32).unwrap(),
                layer::lanetree_tree_oid(t).unwrap(),
                "lane {j}"
            );
        }
    }

    /// A sharded mirror reconstructs every lane exactly, and the tree oid is
    /// identical no matter how many shards the paths are split across.
    #[test]
    fn sharding_is_transparent_and_sha_exact() {
        let s0 = vec![
            lane(&[
                (b"README", Mode::File, b"r0", b"hi\n"),
                (b"src/main.rs", Mode::File, b"m0", b"fn main(){}\n"),
                (b"src/util.rs", Mode::File, b"u0", b"pub fn u(){}\n"),
                (b"docs/guide.md", Mode::File, b"d0", b"# guide\n"),
                (b"run.sh", Mode::Exec, b"x0", b"#!\n"),
            ]),
            lane(&[
                (b"README", Mode::File, b"r0", b"hi\n"),
                (b"src/main.rs", Mode::File, b"m1", b"fn main(){2}\n"),
                (b"docs/guide.md", Mode::File, b"d0", b"# guide\n"),
            ]),
        ];

        // Same history across a range of shard counts must give the same oids.
        let mut oids_by_bits = Vec::new();
        for bits in [0u32, 1, 2, 3] {
            let mut s = Shards::seed(bits, s0.clone());
            assert_eq!(s.n_shards(), 1usize << bits);
            assert_all_lanes(&s, &s0);

            let mut s1 = s0.clone();
            s1[1] = lane(&[
                (b"README", Mode::File, b"r1", b"hello\n"),
                (b"src/main.rs", Mode::File, b"m1", b"fn main(){2}\n"),
                (b"src/util.rs", Mode::File, b"u0", b"pub fn u(){}\n"),
            ]);
            s.advance(s1.clone());
            assert_all_lanes(&s, &s1);
            s.seal();
            assert_all_lanes(&s, &s1);

            oids_by_bits.push(s.reconstruct_tree_oid(0).unwrap());
        }
        assert!(oids_by_bits.iter().all(|o| *o == oids_by_bits[0]), "shard count changed the oid");
    }

    #[test]
    fn randomized_sharded_lifecycle() {
        fn next(rng: &mut u64) -> u64 {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            *rng
        }
        let paths: [&[u8]; 7] =
            [b"a", b"d/x", b"d/y", b"d/e/f", b"README", b"src/lib.rs", b"z"];
        let modes = [Mode::File, Mode::Exec, Mode::Symlink];
        let gen_tree = |rng: &mut u64| -> LaneTree {
            let mut t = LaneTree::new();
            for p in paths {
                if next(rng) % 4 != 0 {
                    let mode = modes[(next(rng) % 3) as usize];
                    let c = vec![b'v', (next(rng) % 9) as u8 + b'0'];
                    let oid = [&[mode as u8][..], &c[..]].concat();
                    t.insert(p.to_vec(), LaneEntry { mode, oid, content: c });
                }
            }
            t
        };
        let mut rng = 0xbadc_0ffeeu64;
        for _ in 0..60 {
            let bits = (next(&mut rng) % 4) as u32;
            let n_lanes = 1 + (next(&mut rng) % 3) as usize;
            let mut lanes: Vec<LaneTree> = (0..n_lanes).map(|_| gen_tree(&mut rng)).collect();
            let mut s = Shards::seed(bits, lanes.clone());
            assert_all_lanes(&s, &lanes);
            for _ in 0..1 + next(&mut rng) % 5 {
                let j = (next(&mut rng) % n_lanes as u64) as usize;
                lanes[j] = gen_tree(&mut rng);
                s.advance(lanes.clone());
                assert_all_lanes(&s, &lanes);
                if next(&mut rng) % 3 == 0 {
                    s.seal();
                    assert_all_lanes(&s, &lanes);
                }
            }
        }
    }
}
