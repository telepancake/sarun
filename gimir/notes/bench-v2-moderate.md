# gitdepot v2 sanity bench — moderate repo (2026-07-06)

Fixture: synthetic, 500 commits / 800 files / 23MB tree / tags+branch
(the workspace repo is a SHALLOW clone — v2 refuses shallow parents
loudly where v1 stored dangling shas; use full-history sources).
Baseline: fresh `repack -adf` pack = 12.0MB. Tool:
`cargo run --release -p gitdepot --example bench -- <store> [deep_sha]`.

| op | result |
|---|---|
| full mirror (500 commits) | 2m12s (~0.26s/commit) |
| no-op mirror | 0.11s |
| 1-commit update | 0.62s (bounded ✓) |
| resolve_ref | 29ms |
| tip decode + root ls | 70ms |
| tip full 800-file walk | 0.5ms |
| tree 400 deep | 32s ✗ |
| store vs pack | 47.5MB vs 12.0MB (3.96×) ✗ |

Diagnosis:
- Size: cold tier = 13.0MB ≈ pack (encoding at pack parity ✓); the
  rest is DEAD BYTES in f0 (32MB file, superseded head frames) + f1 —
  reclaim happens only on 32MiB file roll. Fix: smaller depot file
  threshold for gitdepot (e.g. 4MiB) shrinks the garbage window ~8×.
- Deep access: cached walk costs ~80ms/step × depth; dominated by the
  full-view RE-ENCODE per frame boundary (refPrefix) + full-view
  apply per step = O(depth × tree). Design fork, user to decide:
  (1) delta-anchored frames (prefix = previous delta record, already
      in hand): kills the re-encode; with in-place apply → O(depth ×
      delta). Slightly worse compression than view-anchoring.
  (2) checkpoint records (full layer every K): keeps view-anchoring
      compression, bounds walk to K steps, +1 full record per K.
  They compose: checkpoints + delta-anchoring between them is the
  likely end state (K trades size vs worst-case access).

## 2026-07-06 — in-place walk (apply_mut)

Fix landed: `depot::apply_mut` (in-place O(delta) apply, property-tested
equal to `apply`) + `Store::walk_tree_views` keeps ONE working view
mutated per record instead of a fresh full `apply` + Vec of every
intermediate view. Anchoring discipline and on-disk format unchanged
(the full-view re-encode remains once per cold frame; byte-fidelity
pinned by `tree_walk_matches_apply_reference_byte_exact`).

Fixture regenerated per the recipe above (seed 7, 500 commits / 800
files / 23.4MB tree; pack 12.8MiB; store 28M — the 4MiB depot file
threshold is already in). Same store, same deep sha (main~400):

| op | before | after |
|---|---|---|
| resolve_ref | 8.3ms | 9.8ms |
| tip decode + root ls | 67ms | 80ms |
| tip full 800-file walk | 0.46ms | 0.49ms |
| tree 400 deep | 16.3s | **0.45s** ✓ |

(16.3s not 32s pre-fix here: this store was written with the 4MiB file
threshold, so its cold tier differs from the original 2026-07-06 run;
same pathology, same O(depth × tree) shape.) Deep access is now ~1ms/step
+ one full-view re-encode per cold frame — well under the 2s target.

## 2026-07-06 — real repo (ripgrep, 2252 commits, 222 files / 3.1MB tree)

Branches-only (annotated tags BLOCK the mirror — must-fix, see below);
baseline pack `repack -adf` = 3.40MB.

| op | gitdepot v2 | git |
|---|---|---|
| full import | 84.6s (~38ms/commit; ls-tree+cat-file per commit) | clone 1.3s |
| store | 8.7MB = f1 7.6 + f0 0.6 + sqlite 0.5, cold 0, zero dead | 3.40MB (2.6×) |
| no-op tick | 0.10s | ~0.1s |
| 1-commit update | 0.36s | ~instant |
| tip decode+ls | 13ms | archive 19ms (parity) |
| 400-deep | 228ms | 31ms |
| 2000-deep | 203ms (FLAT — one f1 frame) | 32ms |

Attribution: size gap = whole-blob-per-touched-file records vs git's
intra-blob xdelta (lever if needed: byte-delta encoding inside
records). Import cost = per-commit subprocess round-trips (lever:
fast-export streaming importer). Deep access flat-in-depth while
history fits one accumulator frame; checkpoints only matter once many
cold frames exist.

FINDINGS: (1) annotated tags abort mirror ("out of scope") — real
repos all have them; peel to commit + preserve tag object in meta for
SHA-exact export. (2) git archive is depth-independent (~30ms) —
content addressing; our 6-7× constant at depth is acceptable, the
asymptotics are not worse than one frame decode + k applies.

## 2026-07-06 — window/LDM fix (user finding: "solid archive must beat the pack")

compress() had level-default window (~2MB @ L3), no LDM — the
accumulator's cross-record redundancy sits MB apart in the raw stream
and was structurally invisible to the encoder. Window now sized to the
frame (cap wlog 27 = decode default limit) + LDM. ripgrep, same data:

| | f1 | store total | vs pack 3.40MB |
|---|---|---|---|
| L3, old window     | 7.63MB | 8.70MB | 2.6× |
| L3, window+LDM     | 1.52MB | 2.58MB | 0.76× |
| L19, window+LDM    | 1.26MB | 2.19MB | 0.64× |

Deep-2000 access on the wide-window store: 274ms (vs 203ms before —
LDM decode cost, negligible). Import time unchanged (~85s L3).

## 2026-07-06 — O(changes) streaming importer (ripgrep, tags kept)

Discovery reworked from O(tree × history) to O(changes): the old loop
spawned `cat-file commit` + `ls-tree -r` + a fresh `cat-file --batch`
per commit, re-piping EVERY blob of the WHOLE tree for EVERY commit.
Now: ONE `git log --format=%x01%H --raw -z --no-renames --no-abbrev
--diff-merges=first-parent --topo-order --reverse --branches --tags`
stream for the whole walk + ONE persistent `cat-file --batch`
(request-one/read-one) for raw commit objects and changed blobs; views
built frontier-style (clone first parent's view + apply_mut of the
first-parent delta, refcounted by remaining children from one
`rev-list --parents` pre-pass). Chain encoding untouched — old/new
stores verified OBJECT-IDENTICAL on ripgrep (commit/tag records, ref
rows, byte-identical TREES records; `tests/equivalence.rs` pins the
same against a git-built reference on a merge-heavy DAG).

Fixture: fresh `git clone --mirror` of ripgrep — 2259 commits,
annotated tags KEPT this time (260 tag objects imported; the earlier
run had to strip them). Same machine, L3, store 2.2MB either way.

| | old (per-commit) | new (streaming) |
|---|---|---|
| full import | 57.9s | **4.9s** (11.8×) |
| bytes read by driver (rchar, /proc/self/io) | 5.03GB | 124MB (40×) |
| syscalls r/w | 859k / 1724k | 30k / 50k |
| max frontier (live views) | — | 4 |
| incremental update (3 commits) | — | 1.4s |

Live smoke (sarun binary): `sarun gitdepot mirror <ripgrep clone>` =
12.9s clone+import end-to-end; second mirror after moving master +
re-adding a tag = 3 new commits in 2.2s.

Remaining per-commit O(tree) CPU: the frontier's deep View clone and
`encode(diff(None, view))` for each NEW tree (the chain's full-record
anchor). Accepted for v1 — restructuring View for sharing is a
separate change.

## 2026-07-06 — stub buffer + laddered bootstrap (ripgrep)

The persistent full fetch buffer is gone. `<root>/repo.git` at rest is
a KB-scale SHALLOW STUB — tip commit objects + tag chains + refs +
`shallow` boundary, packed (contract: lib.rs "THE STUB CONTRACT",
stage-0 validated on file://, local-path and https/github):

* Before every fetch, every tip's FULL snapshot (trees+blobs) is
  materialized from the store via one fast-import. Load-bearing: the
  server excludes exactly the shallow tips' closures, thin-pack bases
  and the connectivity walk both resolve locally on any transport (no
  promisor/lazy-fetch). Tips-only-shallow WITHOUT snapshots fails the
  connectivity check ("missing blob … did not send all necessary
  objects"); a commit-graph-complete stub without shallow makes
  negotiation exact but strands thin bases (index-pack "unresolved
  deltas") — both measured and rejected in stage 0.
* Anything attaching BEHIND a tip is resent by the server (shallow
  grafts cut the haves closure at the tips). Correctness unaffected
  (known commits skip, views seed from the store); cost = refetched
  bytes. Endemic shape on tag forests: fetching an old tag with only
  newer tips as haves resends ~everything between — the bootstrap
  ladder neutralizes it with READY RUNGS (tags whose peeled commit is
  already imported fetch as tag objects only).
* After every run the stub is rebuilt fresh from the store (tip commit
  bytes REGENERATED — tree oid recomputed host-side from the view via
  sha1, extra-header commits use their preserved raw — and asserted
  against the recorded shas; the honest one-copy story). `mirror rm` /
  `--frugal` may drop even the stub; the next tick rebuilds it.

First contact defaults to the LADDERED bootstrap (`--whole` opts back
to clone+import): waves of tags in natural-version order (chronological
ordering rejected: dates are unknowable before fetching tag objects,
and a `--filter=tree:0` metadata wave poisons later rungs — a
present-but-filtered want makes fetch skip its closure), ready rungs
swept in bulk between waves, one converge fetch at the end. The ladder
is TRANSPORT ONLY: all rungs stage through ONE Ingest (records spill to
`<store>/staging/` past GITDEPOT_SPILL_BOUND, default 256MiB; the bound
selects the staging medium, never the prepend count) and land as ONE
prepend per touched chain — asserted: bootstrap depot_prepends == 5
(TREES seed + TREES + COMMITS + TAGS + REFLOG) for any rung count, and
a spilled 40-commit update == the in-RAM update object-identically at 4
prepends (tests/spill.rs). Crash mid-bootstrap: staging is scratch; an
empty store is wiped and the bootstrap restarts from zero.

Fixture: fresh `git clone --mirror` of ripgrep (2259 commits, 249 tags,
9.7MB), local path remote. L3.

| op | old (persistent clone) | new (stub) |
|---|---|---|
| first contact | 8.3s clone+import (`--whole`) | 18.4s laddered (10 rungs) |
| buffer at rest | 9.7MB forever | **404K** |
| store | 2.2MB | 2.2MB |
| no-op tick | ~0.1s | 1.1s (ls-remote short-circuit) |
| 1-commit update | 0.36s | 8.1s |
| peak rung buffer | — | 11.3MB (see below) |

Peak/update-cost attribution: BOTH are the union-of-tip-snapshots term.
ripgrep is tag-dense relative to its size — 249 tags whose snapshots
union to ~7.5MB of fast-import pack (no inter-blob deltas), which every
fetch must have present, so the bootstrap peak (~11.3MB) exceeds the
9.7MB clone and each update tick pays ~7s of materialization. On the
intended shape (history ≫ tag count × tree: long-lived repos with real
churn) the ratio inverts — the stub test fixture (30×64KB replaced
file, 3 tags) bounds every rung and every update peak under ⅓ of the
clone, asserted in tests/stub.rs. Lever if tag-dense repos matter:
carry snapshot packs across ticks in the at-rest stub (trades the
KB-at-rest property for O(1) tick cost), or delta-compress the
snapshot packs.

Bootstrap peak disk = one rung's pack + snapshot packs + staging log
(≈ the final raw accumulator — the product itself) + the stub.

Equivalence: laddered bootstrap == single-shot import on the
merge-heavy DAG — per-sha commit records, parent edges, per-commit
canonical view bytes, refs, tag objects (indices may permute: each
order is a valid topo order; both stores pass the git-reference index
contract) — tests/equivalence.rs. Stub lifecycle, delta-only update
fetch, missing-stub rebuild (sha-asserted), ladder peak bound —
tests/stub.rs. All existing tests green (19+4+3, 0 ignored; equivalence
grew to 3), +3 stub +1 spill.
