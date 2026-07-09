# gitdepot IMPL-NOTES (Phase 2 — Implement)

Worked against `DESIGN.md` (authoritative) and the current `WORKMAP.md`, whose
"first thing to implement" is: *give `layer.rs:delta_multi_lane_stacked` an
oid-addressed `Objects`-style source so it no longer needs full lane content in
RAM — the prerequisite for the git wiring and for deleting cluster C.* That is
what this pass lands, additively, keeping every conforming path (and its tests)
untouched.

## Build status

Green, zero warnings, with the prescribed command:

    cd /home/user/sarun/gimir && uv run --with cargo-zigbuild --with ziglang \
      cargo zigbuild --release -p gitdepot --tests \
      --target x86_64-unknown-linux-musl
    -> Finished `release` profile in 37.83s

All 54 gitdepot lib unit tests pass (including the new oid-path test); the
end-to-end integration binaries still build.

## What changed and why

Only `gitdepot/src/layer.rs` (the single authoritative §2 encoder per the map).
Before this pass its delta generator diverged from §6 on one point the map calls
out explicitly: it "takes whole `LaneTree`s with content in RAM" — every lane's
`LaneEntry` carries `content: Vec<u8>`, so the full file bytes of every lane are
resident while generating a delta. §6 requires the opposite: variants are
matched by `(mode, oid)` read for free from the lane trees, and blob content is
fetched by oid **only when a genuinely new variant node is emitted**.

Added the oid-addressed core (design §6), and re-expressed the existing
content-carrying functions as thin adapters over it — a behavior-preserving
refactor (proven byte-for-byte by the new test), not a rewrite of the logic:

- `LaneRef` / `LaneTreeRef` — a content-free lane view, `path -> (mode, oid)`.
- `trait Objects { fn blob(&mut self, oid: &Oid) -> Result<Vec<u8>, WErr>; }` —
  the oid-addressed blob source (mirrors `oidenc::Objects`); the one place a
  fresh variant's bytes are read.
- `MemObjects` (`oid -> content`, with `from_lanes`) — the in-memory source
  that backs the compatibility adapters and the tests.
- `union_groups_oid`, `encode_union_oid`, `current_variants_ref`,
  `delta_multi_lane_stacked_oid` — the oid-only implementations. `blob` is
  called only to emit a new variant; a pruned or bitmap-only update fetches
  nothing.
- `encode_union` and `delta_multi_lane_stacked` are now adapters: they strip
  content to `LaneTreeRef`, build a `MemObjects` off the lanes, and call the oid
  core (`.expect("in-memory objects never fail")`). Every existing caller
  (`frame.rs`, `unionstore.rs`, `shards.rs`, all their tests) is unchanged.
- New test `oid_addressed_matches_content_path_and_fetches_minimally`: the oid
  path produces byte-identical seed unions and deltas to the content path, a
  counting `Objects` proves `blob` is called only for the one fresh variant (the
  pruned and bitmap-only cases fetch nothing), and the result still
  reconstructs every new lane exactly.

Net effect: the authoritative encoder now conforms to §6's "no lane content in
RAM" at its API boundary. A caller that already has `(mode, oid)` trees and a
git object source (rather than pre-loaded content) can drive `encode_union_oid`
/ `delta_multi_lane_stacked_oid` directly and never materialize lane content —
which is exactly what the git-wiring and cluster-C-deletion steps below need.

## Not done in this pass, and why (staged, not unimplementable)

The map frames the full convergence as a sequence of commits, of which the above
is the first. The remaining divergences are genuinely large, destructive to the
conforming live store, and interdependent; doing them in one compiling pass
would mean rewriting `store.rs`/`lib.rs`/`unionstore.rs` and risking the live
integration suite, contra "build on conforming code, do not rewrite it." None is
blocked by the design — each is a scoped follow-on:

1. **Feed the oid API from `gitsrc.rs` (content-free fetch).** `gitsrc` still
   reads blob content eagerly into `LaneTree`. Next it should yield `(mode, oid)`
   trees plus an `Objects` backed by `git cat-file --batch`, so the persisted
   engine runs fully content-free. The seam now exists on the encoder side.
2. **§7 direction in `unionstore.rs`.** It stores **forward** deltas over an old
   base (reverse-at-seal is DEFERRED in its own doc). §7 wants newest-full /
   older-reverse. This is the one place the §2-correct engine (B) is still
   §7-wrong.
3. **Fold the union payload into the live `store.rs` TREES chain** (§1/§2) and
   **delete cluster C** (`variants.rs` + `oidenc.rs` + `lanestore.rs` +
   `reslot.rs` and their `tests/lanestore*.rs`), whose on-disk shape contradicts
   §2 (nested `\0v/\0m` wrapper, mode-as-attr, bitmap never omitted for
   all-ones). C's two design-right ideas — subtree-prune-by-oid and
   reverse-at-seal — should be ported onto `layer.rs`/`unionstore.rs` first (the
   oid source landed here is the enabler for the first of those). Deletion is
   deferred until B is wired live, so nothing regresses meanwhile.
4. **Persisted sharding (§9)** — `shards.rs` runs on in-RAM `frame::Frame`s
   only; no per-shard Depot yet.
5. **Lane inactivity retirement (§8)** — `lanes.rs` dies a lane only on
   merge/drop, not on the "no new commit for a long stretch → retire into the
   reflog" rule.
6. **mmap / `MADV_DONTNEED` single-pass updater (§4)** — both engines compose
   in-RAM `Vec<u8>`; the streaming mmap adapter is unimplemented anywhere.
