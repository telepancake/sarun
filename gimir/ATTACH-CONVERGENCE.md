# Attach convergence — use the depot, not a loading dock

2026-07-05 audit finding: the mirror serve path violates DEPOT-DESIGN
§8. Attach today = decode the mirror store → build one flattened
`Layer` → `import_layer` COPY into a fresh sqlar box → slurp that box
whole into RAM at hydrate (`capture.rs load_mirror`). §8 says attach
is *pure bookkeeping, no copying*, needing only the readout half of
the trait, with blobs materialized lazily (§7 cache). The abstractions
exist and are unused:

- `gimir/depot` has the algebra + `LayerSink/LayerSource`, but NO
  readout trait; the de-facto readout (`engine/src/depot.rs BoxDepot`)
  is engine-local and sqlar-only.
- `gimir/depot-cache` (content-keyed blob pool + tree materialization
  + nlink eviction) is implemented and wired to NOTHING.
- wikimak `page_head*` and `VbfDepot::head_layer` are already O(1)
  random-access; `ietf_attach` ignores that and walks full history;
  `git_attach` walks every frame via `read_store`.
- `overlay.rs` reads `blob_path()` directly in 3 places — the blob
  seam leak DESIGN §7 already calls out.

## Target

```
attach = bookkeeping row {kind, store_path, ref, pinned_rev}
serve  = overlay readout → Readout trait impl over the mirror store
blobs  = materialized on demand through depot-cache (mmap/exec safe)
copy   = never (import_layer path deleted from attach verbs)
```

## Chips

1. **Readout trait in `gimir/depot`** (`variant.rs`): the §8 readout
   half — `entry(components) / children(components) / blob(components)`
   over opaque byte names; blob returns bytes-or-backing-file so loose
   file stores stay zero-copy. Engine's `BoxDepot` readout half becomes
   an impl of it (sqlar variant); a generic `View`-backed impl covers
   any resolved snapshot.
2. **Store adapters**: `Readout` impls for wikimak (page head — O(1)
   already), ietf (`head_layer`), gitdepot (tip via `read_head_record`,
   decoded once, cached; full-history frames later). Live beside the
   store crates; unit tests per adapter.
3. **Reference attachments in the engine**: `ro_attachments` grows
   external entries (kept alongside box-id entries); hydrate builds a
   Readout-backed attachment instead of opening a sqlar box; overlay
   entry/children/blob for attachments route through the trait —
   which also closes the `blob_path()` leak for attachments. Attach
   verbs stop calling `import_layer`; pinning stays (rev recorded at
   attach time, store append-only ⇒ a pinned rev never changes).
4. **Wire `depot-cache`**: attachment blob reads that need a real fd
   (mmap/exec/pread paths) materialize into the cache pool; repeated
   reads hit the pool. Eviction = space management, never data loss.
5. **Proof tests**: §8 byte-identical invariant (same box run with and
   without attachment → identical captured layer — the missing half of
   test_ro_attach); laziness (attach a store with N pages, assert no
   O(N) I/O or sqlar box creation); EROFS suite unchanged.
6. **Delete the copy path** from the attach verbs (attach_ro_layer’s
   import remains only for genuine imports, if anything still wants
   one).

Consistency note: no sub-box/apply-on-complete dance is needed —
attach reads an already-consistent pinned rev; store-side flock +
dirty-flag repair own write-time consistency (MIRRORS.md).
