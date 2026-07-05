# gimir/ — integration source

Working material for meshing **gimir** (a Go git-mirror + provider-archive
manager) into sarun as Rust. This is **not** a vendored pristine upstream (not
`engine/vendor-patches/` discipline) — it is source we are actively reshaping, so
it lives as tracked files to edit in place.

The immediate goal is **the depot abstraction** — see `DEPOT-BRIEF.md`. Everything
here is input to that: the already-built Rust store variant, its specs, and the
design record.

## Provenance

Extracted from `github.com/telepancake/gimir`:
- Rust workspace + `docs/wikipedia-*` + `notes/` — branch
  `claude/quirky-fermi-1xeKq` @ `b7a4dfe` (the Wikipedia line; 166 commits, off
  mainline).
- `docs/tiered-vbf-and-strpool.md`, `meta/reports/vbf-recovery.md` — gimir `main`
  (the distilled design + the archaeology/sabotage trace).
- `SCOPING.md` — the integration analysis written this session.

## What's here (and its state)

Rust workspace (`Cargo.toml` — members `strpool`, `wikimak/{depot,mediawiki,
wikipedia}`):

| crate | what | state |
|---|---|---|
| `wikimak/depot` | the tiered revision-chain store (f0/f1/cold, flat index, index-flip commit) — **one depot variant** | **works**, zstd-opaque, 17-test acceptance suite green |
| `strpool` | string → dense integer id interning (sharded, positional ids) | builds; needs the C toolchain to test |
| `wikimak/mediawiki` | MediaWiki dump pipeline: discover → fetch → multistream bz2 → XML parse → sha1 | real + tested; needs C (`bzip2-sys`, bundled `rusqlite`) |
| `wikimak/wikipedia` | the glue encoding revisions into the depot | **FIXED** (2026-07): f0 standalone zstd, f1 refPrefix-anchored on the head record, sealing at a threshold → cold frames form. Verified by MEASURED on-disk size (`tests/compression.rs`): a 120-revision ~5 MB page stores at 12× compression, in the design's 10–20× target. The sabotage record stays in `meta/reports/vbf-recovery.md` §4. |

Docs: `DEPOT-BRIEF.md` (the north star), `SCOPING.md` (the mesh analysis),
`docs/` (owner design), `notes/` (design reasoning), each crate's `SPEC.md`,
`wikimak/PHASES.md`.

## Build / verify

The workspace is independent of sarun's `engine/` workspace (separate
`Cargo.toml`; sarun's root ignores only `/Cargo.toml`, not `gimir/Cargo.toml`).

```
cd gimir
cargo test -p wikimak-depot     # the store — 17 tests, no external C beyond zstd
cargo test                      # full: needs a C toolchain (zstd, bzip2, sqlite bundled)
```

## The work, in order (from SCOPING.md, reframed by DEPOT-BRIEF.md)

1. **Design the depot trait** (`DEPOT-BRIEF.md`) — the named-tree-with-tombstones
   interface, data model first. `wikimak/depot` is a *variant* to inform it, not
   the abstraction.
2. Make `wikimak/depot` (and `strpool`) build+test in this tree — done for depot.
3. ~~Un-sabotage `wikimak/wikipedia`~~ DONE — refPrefix + sealing, verified by
   measured on-disk size against a real multi-revision page
   (`wikimak/wikipedia/tests/compression.rs`). Per-chain dict training remains
   an open tuning question (size against the real corpus first).
4. Map the other three data kinds (fs layers, git repos, sqlar) onto depot
   variants; wire the box overlay to address its store through the trait.
5. Mesh: provider capture inside sarun tap boxes (flows-visible), serve/UI as a
   pane, CDP for the GitHub-Discussions gap.
