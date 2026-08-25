I have enough context now. Here is the build spec.

---

# Branch-Lane Git Store Rewrite — Build Spec (design of record + implementation plan)

Reviewed against: `gimir/notes/branch-lane-model.md`, `gimir/depot/src/{lib.rs,codec.rs,variant.rs}`, `gimir/gitdepot/src/{lib.rs,store.rs,readout.rs}`, `gimir/wikimak/depot/src/{lib.rs,inner.rs}`. Confirmed holes #1–#4 as stated.

The single most important reframing this spec makes, up front, because it changes what the rewrite *is*: **the rewrite is a change to the store's encoding layout (lockstep lanes + variant deltas), not to the fetch/ingest object-plumbing.** Hole #4 proves the design's plan to delete `materialize_snapshots` / the stub contract is unsound. The fetch front-end (`dag_scope`, `LogStream`, `CatFile`, `delta_layer`, `seed_views`, `Frontier`, `materialize_snapshots`, the shallow stub, `collect_refs`) is **reused essentially as-is**; what gets rebuilt is `store.rs`'s TREES chain and the new lane/variant/retirement logic. Keep this in mind reading Part 2.

---

## Part 1 — Design amendments

### Hole #1 — empty git tree (`4b825dc…`) collapses to depot `None`

**Root cause.** `apply_node` / `apply_node_mut` (`depot/src/lib.rs:237-239`, `:259`) enforce the canonical-form rule *empty node ⇒ nonexistent ⇒ `None`*. That rule is **load-bearing for confluence and the 299 property tests and must not change.** The problem is purely at the git↔depot boundary: git's empty root tree is a real object, but the algebra can only produce `None`, which `ingest_stream` (`lib.rs:1284`) turns into `Err(Unsupported("… empty tree"))` aborting the whole import, and which `walk_tree_views` (`store.rs:928`) reports as "resolves to nothing".

**Key fact that makes the fix cheap:** git forbids empty *subtrees* (you cannot commit an empty directory), so the empty tree can only ever be the **root** of a commit tree. We only need to represent "empty root exists," never an empty interior node.

**Amendment (close it, no algebra change).** Introduce a gitdepot-level **root-empty sentinel**: a single reserved attribute on the *root* `View` (e.g. attrs key `b"\0empty-root"`, value empty). It lives only on the root node, so it can never collide with a child's `mode` attr that `view_tree_oid` reads (`lib.rs:1563-1567`).

- It makes the empty-root `View` non-empty in the algebra's eyes, so it survives `diff`/`apply_mut`/`compose` (attrs-bearing nodes already "exist") and round-trips through the TREES/lane walk instead of resolving to `None`.
- `view_tree_oid` (`lib.rs:1558`) already iterates only `view.children`, ignoring root attrs, so a sentinel root `View` still emits the empty tree body ⇒ oid `4b825dc642cb6eb9a060e54bf8d69288fbee4904`. **No change to `view_tree_oid`.**
- Injection point: in `ingest_stream`, replace the `view.ok_or_else(…"empty tree")` at `lib.rs:1284-1285` with: if `view is None && tree_oid == EMPTY_TREE_OID`, substitute the sentinel `View`; assert `view_tree_oid(sentinel) == tree_oid`. Keep the `"resolves to nothing"` guard in `walk_tree_views` as a genuine-corruption tripwire (it can no longer fire for a legal empty root, because the stored reverse delta now carries the sentinel).
- This also fixes the lockstep case the review flags: a lane that transiently empties at revision *i* stores a sentinel view at index *i*, so the empty-delta / temporal walk never hits `None`.

**Zero change to the `depot` crate.** The sentinel is a git-boundary convention. Confluence and the property tests are untouched.

### Hole #2 — future-dated committer poisons the retirement threshold

**Amendment.** The retirement decision must not depend on any attacker-controllable git date field. Anchor it to a **trusted local clock** — the fetch/receive wall time (`store.rs:194 now_secs()` already exists):

- Record, per live ref/lane, a **`last_advanced_at` receive-timestamp** in bookkeeping, stamped with `now_secs()` at ingest whenever that ref's tip changes.
- A ref is **retired** when it has not advanced for ~1 month of *local* wall-clock (a `RETIRE_SECS` const), never by comparison against a committer date. "The repo's newest commit" as a threshold anchor is **removed**.
- Wherever a commit date is still needed (display, tiebreak), **clamp to `min(committer_date, now_secs())`** and, if an aggregate is ever required, use a robust statistic (high percentile), never raw max. Document that **committer date is untrusted; receive time is authoritative** for liveness.
- Retirement writes the binding to the reflog (`ReflogRecord` already exists, `store.rs:417`) and is non-destructive (soundness obligation 4).

**Interaction to pin (obligation 3):** retirement retires **refs (pins)**, not lanes. A base lane referenced by a still-live variant stays materializable via base-switching (see #base-switch), **independent of ref retirement** — retirement can never drop a referenced base into a hole.

### Hole #3 — "fetch all advertised except what the server hides" pulls unbounded foreign refs

**Amendment.** Reject the design doc's "fetch all refs the server advertises" clause. The boundary is **"bounded and ours," not "not the server hides it"** (the premise is factually false — GitHub/GitLab/Gerrit advertise `refs/pull/*`, `refs/notes/*`, `refs/merge-requests/*`, `refs/changes/*`). Retain the **explicit allowlist** the shipped code already has:

- `collect_refs` (`lib.rs:463-466`) and `init_stub_dir` (`lib.rs:1720-1721`) already restrict to `refs/heads/*` + `refs/tags/*` with the exact rationale in the comment. **Keep them.** The rewrite must not regress this to `+refs/*:refs/*`.
- `refs/notes/*` is **opt-in** via config, off by default. Everything else excluded.
- Correct the design doc's Fetch section to say allowlist, not "all advertised."

### Hole #4 — non-thin pack is NOT self-contained; the have-closure must be physically present (soundness-breaking)

**Amendment.** Reject the design's claim that `--no-thin` lets us delete `materialize_snapshots` / the stub contract / rehydration. `--no-thin` only forbids *external delta bases*; with `have=T_old` the server still omits every object reachable from `T_old`, so `T_new`'s unchanged tree closure is absent from the incremental pack. Two independent consumers need that closure physically present:

1. **git's post-fetch connectivity check** and `git diff-tree` against a boundary parent (`lib.rs:1665-1670`) need the boundary tips' trees/blobs to exist locally.
2. The **encode walk** needs the parent tree to diff against.

Resolution, and it is subtler than "keep materialize_snapshots," because it splits the two consumers:

- **Retain `materialize_snapshots` + the shallow stub** (`lib.rs:1849`, stub contract `lib.rs:1640-1681`). Before each fetch, reconstruct each live tip's full tree+blob closure **from the depot** into the stub (exactly `materialize_tip_snapshots`, `lib.rs:1943`). At-rest storage is still "depot + tip SHAs" (the doc's goal survives); only the **transient peak** includes materialized tip snapshots. **Correct the doc's peak-disk line** to `pack + materialized tip snapshots + depot`, not `pack + depot`.
- **The encode side does NOT read `T_new`'s full closure from git.** Do not adopt the doc's "encode reading objects from the local index in whatever order the frame wants." Keep the existing model: `delta_layer` builds a Layer from `git diff-tree` **changed paths** (present in the incremental pack) and `apply_mut`s it over the **parent view reconstructed in RAM from the depot** (`seed_views` / `Frontier`, `lib.rs:1273-1298`). The full closure never leaves the depot; only changed objects come from the pack. This is why the connectivity check (git-side) needs materialize but the encoder (depot-side) does not.

**Net:** `materialize_snapshots` and the stub contract are **retained**, overruling the deletion the original design (and the deletion hint in the task) implied. `blob:none` / `--filter=tree:0` / want-by-oid can still go (they are an orthogonal lazy-fetch optimization, not the have-closure), and dropping them does not reintroduce this hole.

### Conflict resolution across amendments

- **#4 vs. the "delete materialize_snapshots/stub-contract" hint:** #4 wins — retained.
- **#3 vs. #4:** independent and compose (allowlisted *wants* + correct *have-closure*).
- **#2 (retirement, live set) vs. lane death (topology):** the live-lane set = topological live lanes **minus** retired refs; base lifetime is governed **only** by base-switching (topology), never by retirement (see obligation-3 note above).
- **#1 vs. everything:** the sentinel is view-level and encoding-agnostic; it carries forward unchanged into the lane encoder and is orthogonal to #2/#3/#4.

---

## Part 2 — Implementation plan

Sequencing keeps the crate buildable and the suite green between every step. Steps 0–2 land on the **existing** TREES store (encoding-agnostic fixes). Steps 3–4 are pure functions. Steps 5–6 build the new lane store **alongside** the old one. Step 7 cuts over. Step 8 deletes dead code.

### What is deleted vs. reused (call-outs the task asked for)

**Deleted:**
- `walk_order` (`store.rs:1016`, the TREES record-size linearization model) — replaced by topological lane-revision ordering (Step 3).
- The single whole-tree TREES reverse-delta chain as the *primary* current-state store, and `walk_tree_views`/`tree_view`/`tree_views` on that layout (`store.rs:915-989`) — superseded by lockstep lane frames (kept only if a migration/export reader is still wanted, else removed).
- `blob:none` / `--filter=tree:0` / want-by-oid machinery (the filter-poison path around `natural_key`, `lib.rs:2254`) — the model drops partial/lazy fetch.
- `Staged` spill-log paths (`store.rs:1459`) **only if** the lane prepend model no longer needs cross-rung single-prepend buffering — see Open Question 2; otherwise adapted, not deleted.

**Reused (unchanged unless noted):**
- **depot compose algebra** (`depot/src/lib.rs`: `apply`/`apply_mut`/`diff`/`compose`/`squash`, canonical `codec`) — two-axis is *nested* `diff`/`compose`, no new algebra.
- **`FrameEncoder`/`FrameDecoder`/`seal_f1`/`compose_f1`/`chunk_newest_first`** (`wikimak/depot/src/lib.rs:89,149,258,356`) — the VBF physical layer, verbatim.
- **oid assertion**: `git_obj_oid` + `view_tree_oid` (`gitdepot/src/lib.rs:1546,1558`) — leaf byte-exactness, verbatim.
- **fetch/ingest front-end**: `collect_refs` (allowlist, #3), `dag_scope`, `LogStream`, `CatFile`, `delta_layer`, `seed_views`, `Frontier`.
- **`materialize_snapshots` + shallow stub contract** (`lib.rs:1849`, `:1640`) — RETAINED per #4.

### Steps

**Step 0 — Ref allowlist regression pin (hole #3).** Files: `gitdepot/src/lib.rs` (`collect_refs:463`, `init_stub_dir:1716`); design doc Fetch section. Add an opt-in `notes` config flag; leave heads+tags default. *Test:* feed `collect_refs`/`ls-remote` parsing a synthetic advertisement containing `refs/pull/*`, `refs/notes/*`, `refs/merge-requests/*` → asserts only heads+tags returned (notes only when flag set). *Deps:* none. Green on current store.

**Step 1 — Empty-root sentinel (hole #1).** Files: `gitdepot/src/lib.rs` (`ingest_stream:1283-1285`: add `EMPTY_TREE_OID` const, substitute sentinel `View`, assert `view_tree_oid == tree_oid`); confirm `view_tree_oid:1558` needs no change; `store.rs:928` guard downgraded to corruption-only. *Test:* import a fixture with (a) an initial `git commit --allow-empty` (empty root tree) and (b) a mid-history commit deleting all files then repopulating; assert full import succeeds and the reconstructed tree oid at both revisions == `4b825dc…`. (Existing `--allow-empty` tests at `roundtrip.rs:724`/`equivalence.rs:110` only cover empty *commits*; this adds empty *tree* coverage.) *Deps:* Step 0 optional. Green on current store; carries forward to the lane encoder unchanged.

**Step 2 — Trusted-clock retirement (hole #2).** Files: `gitdepot/src/store.rs` (bookkeeping: per-ref `last_advanced_at` column stamped `now_secs()`; `RETIRE_SECS`; retirement decision fn; reflog binding via existing `ReflogRecord`), `lib.rs` update path. No dependence on committer date; clamp any date use to `min(date, now)`. *Test:* import refs where one commit carries `GIT_COMMITTER_DATE=2099` → assert the live set is not emptied and all recent refs stay live; simulate a ref not advancing past `RETIRE_SECS` (inject an old `last_advanced_at`) → assert it retires to reflog and resolves back through reflog (recoverable). *Deps:* none (operates on refs/live set, encoding-agnostic). Green on current store.

**Step 3 — Topological lane assignment (pure).** New file `gitdepot/src/lanes.rs`. From `rev-list --parents` (already surfaced by `dag_scope`), compute a per-commit lane id: lane born at a branch point, alive while it develops concurrently, dead at merge; a shared lockstep revision index. Pure function, no store I/O. *Test:* synthetic DAGs (linear; single branch+merge; two concurrent lanes; criss-cross merge) → assert lane ids, birth/death revisions, and that live-lane width at each revision ≈ `git log --graph` width. *Deps:* none. Green (new isolated module).

**Step 4 — Variant clustering (pure).** In `gitdepot/src/lanes.rs`. Given live lanes' tree blob-oid sets at a revision, compute Jaccard `|A∩B|/|A∪B|`, cluster into variant groups vs. independents, pick a deterministic base per cluster. Pure. *Test:* synthetic blob-oid sets → assert cluster membership, base pick, and determinism across runs. *Deps:* Step 3. Green.

**Step 5 — Lane store encoder / reconstructor (two-axis), alongside old store.** Files: new `gitdepot/src/store_lanes.rs` (or new chains in `store.rs`) built on `FrameEncoder`/`seal_f1`/`compose_f1`/`chunk_newest_first`. Encode per shared revision index: temporal reverse deltas `diff(view_i, view_{i-1})` within a lane with **empty deltas** where a lane didn't move; variant deltas `diff(base_view_i, variant_view_i)`. Reconstruct lane L at i: temporal-walk base B to i, then apply L's variant delta at i — one canonical composition order; assert the leaf oid via `view_tree_oid`/`git_obj_oid`. Existing TREES path untouched. *Test:* golden reconstruction over a lane×revision matrix — every reconstructed lane-at-i tree oid == fetched oid, including empty-delta revisions and empty-root (Step 1) revisions; a property test in the 299-seed style over random DAG+lane+variant matrices asserting `apply`/`compose` associativity and leaf-oid match (soundness obligation 2). *Deps:* Steps 3, 4, 1. Green (not yet wired).

**Step 6 — Base-switching / reframe.** In `store_lanes.rs`. On base-lane death (topology signal from Step 3) while variants are live: promote a surviving variant — reconstruct its full state, materialize it as the new anchor, re-express survivors as deltas going forward (bounded one-time reframe). Base-in-effect pinned per switch boundary; past immutable. *Test:* synthetic DAG where a base lane merges while variants live → assert pre-switch revisions reconstruct against the old base and post-switch against the new base, all leaf oids exact; assert adding the switch does not change any pre-existing frame's reconstruction (reconstruct all revisions before/after — identical). *Deps:* Step 5. Green.

**Step 7 — Cut ingest + readout over to the lane store.** Files: `gitdepot/src/lib.rs` (`ingest_stream`, `import`, `update`), `store.rs`/`store_lanes.rs`, `readout.rs` (`TipReadout` reads the head frame's live lanes). Reuse `delta_layer`, `seed_views`, `Frontier`, `materialize_snapshots` (**#4**), the stub (**#4**), `collect_refs` allowlist (**#3**), the empty-root sentinel (**#1**), trusted-clock retirement (**#2**). Keep `import`/`update` signatures. *Test:* full import+update roundtrip on a real branchy/merge fixture (extend `roundtrip.rs`/`equivalence.rs`) — every commit's tree oid exact across incremental updates; assert current-state read is O(live-lanes), not O(history), via frontier/pos instrumentation (the whole point of the model). *Deps:* Steps 5, 6, 2, 1, 0. Green after cutover.

**Step 8 — Delete dead code.** Remove `walk_order`, the old TREES reverse-delta chain readers no longer used, `blob:none`/`--filter=tree:0` machinery, and `Staged` paths if Open Question 2 resolves to "not needed." **Retain** `materialize_snapshots` + stub (#4), `FrameEncoder`/`seal_f1` (reused), `git_obj_oid`/`view_tree_oid` (reused), depot algebra (reused). *Test:* whole suite green; `make test` + `make test-oci`; `prototype/test_musl_rs.py` (static linkage) green; grep proves `walk_order` and the filter machinery are gone. *Deps:* Step 7.

---

## Part 3 — Open questions (need a human call)

1. **Lane physical layout.** One VBF chain *per lane* (walked in lockstep across N chains; O(1) single-lane current read, but N-chain coordination) **vs.** one chain whose per-revision record holds *all* lanes at index *i* (true single shared index per the doc's "one shared revision index addresses every lane directly," but reading one lane decodes the whole revision) **vs.** a hybrid (per-cluster chain). The doc's language leans single-index; the readout's O(1) current-state goal leans per-lane. Which?

2. **Lane-index stability across incremental fetches, and the fate of `Staged`.** Incremental fetch adds commits that can re-shape topology (a branch point appears earlier; a "dead" lane revives). Do we (a) keep an **append-only** lane/revision index and force a reframe when new history contradicts it, or (b) **recompute** lane assignment each fetch and reframe wholesale? This decides whether the cross-rung single-prepend buffering (`Staged`, `store.rs:1459`) is still needed or deleted (Step 8).

3. **Retirement window specifics.** Is `RETIRE_SECS` a fixed ~30-day no-advance window? At bootstrap (no prior `last_advanced_at`), do we stamp all refs with the initial fetch time (so nothing retires on first import)? Do **tags** (which never "advance") ever retire, or is retirement branches-only?

4. **Variant base-pick rule.** The base is "arbitrary" but must be **stable across incremental runs** to avoid needless reframes. Fixed rule (lowest lane id / oldest lane / largest blob set)? And the **Jaccard cutoff** for variant-vs-independent is unspecified in the doc — fixed constant, and what value, or tuned per repo?

5. **Sentinel key namespace (#1).** Confirm the reserved root-attr key (`b"\0empty-root"` proposed) is acceptable as a git-boundary-only convention, and that no non-git producer will ever write this store (the model is git-only, so an empty *interior* node cannot arise) — if that ever changes, the sentinel would need promotion into the depot algebra rather than the boundary.