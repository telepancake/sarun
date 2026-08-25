# gimir: project intent and iteration history

## 1. What gimir is (in one paragraph)

gimir is a statically linked, single-user, locally-run mirror manager.
Its job is to keep a personal cache of upstream things — git repositories
first, but also the rich metadata around them (issues, PRs, wikis,
discussions, releases), fossil-bridged source forges, IETF document
corpora, arbitrary websites, and eventually whole MediaWiki instances —
on the user's own disk, fully preserved, dedup'd across forks and
submodules, browsable offline through a localhost web UI, and searchable
across all of it. The user's framing in `README.md` (lines 1-7, 50-60)
is "local mirror manager that uses new fancy git features and manages
the local mirror depot automatically, so it is fast and compact even
when getting big." AGENTS.md (line 6) restates it as "A locally-run,
single-user git mirror manager." The thesis is not "I want a forge" — it
is "I want a personal disk-resident replica of the upstream worlds I
care about, kept current with bounded effort, that survives the
upstream going away."

## 2. The user and the deployment

- **Who.** Single user, personal machine. AGENTS.md line 6 is explicit:
  "single-user." The admin web UI binds localhost-only with no auth
  (AGENTS.md line 112: "Auth gating for `/_admin/`: localhost-only;
  flag if you change the listen address"). There is no multi-user
  story, no permissions model, no sharing.
- **Hardware / OS.** Linux and macOS. README.md lines 4-7 pin static
  linking on Linux via musl as a first-class requirement; libSystem is
  dynamic on macOS either way. The README is emphatic about the
  toolchain budget: `CGO_ENABLED=1` plus a C compiler, but **"No
  `libzstd-dev`, no `sqlite-dev`, no `pkg-config` is needed on the
  host"** (line 19). zstd and SQLite are pulled in from vendored C
  source (`third_party/zstd/` git submodule, `third_party/sqlite/`
  amalgamation checked in — README lines 9-17). The `vendor-cgo-deps`
  branch (commit `e6e7927`, "build: vendor libzstd and sqlite via
  submodules; drop external deps") is where this discipline became
  policy.
- **Disk budget.** Open-ended but the user thinks in real magnitudes.
  AGENTS.md line 122 records the user's prior measurement that enwiki
  full-history "lands in the text-only Kiwix ZIM size class — an order
  of magnitude under the bz2 dumps" (~400-500 GB head set at enwiki
  scale). `docs/wikipedia-import-plan.md` §1.5 documents the same.
  Big-wiki backings have to be picked accordingly (sqlite/LSM, not
  per-file fs).
- **Environment.** A binary you run as a CLI (`gimir clone`, `gimir
  pull`, `gimir gc`, `gimir grep`, `gimir logs`) or as a foreground
  localhost webserver (`gimir serve`). Not a daemon, not a system
  service. Configuration is a single text file: `~/.config/gimir/
  repos.txt` (README line 22). State is one directory: `~/.cache/
  gimir/` (which the user can symlink onto another filesystem — README
  line 25 — for the disk-budget reason). Mirrors live under
  `~/.cache/gimir/mirrors/<host>/<path>.git`, all sharing a single
  content-addressed depot at `~/.cache/gimir/objects/` via Git's
  `objects/info/alternates` mechanism (AGENTS.md line 7).
- **Authentication.** Deliberately not solved by gimir. README lines
  26-27: "Authentication is supposed to be handled by the user and to
  just work. Gimir does not go out of it's way to hide auth prompts,
  but also does not specifically accomodate them." Provider extradata
  fetchers shell out to the user's already-configured `gh`, `glab`,
  `tea`, `hut` (AGENTS.md line 28; commit `65592c6`: "providers: pin
  host on every gh/glab/tea/hut invocation").

## 3. What gimir mirrors

- **Git repositories**, read-only (no pushing upstream — README line
  50). Submodules pulled recursively in full (README line 52).
  Working directories produced by `gimir clone` are normal git
  worktrees that share objects via alternates with the depot, so
  multiple clones across forks and shared submodules do not duplicate
  storage (README lines 53-60; AGENTS.md line 7).
- **Forge extradata** for github, gitlab, codeberg, sourcehut, gitea
  — wiki pages, discussions, tickets, pull requests, comment threads
  (README lines 28-30; AGENTS.md line 9). Stored in git refs in an
  internal uniform schema (commit `120c97e`: "Move extradata storage
  from filesystem JSON into git refs"). Mediated by per-host
  concurrency limits (`internal/hostlimit`) and a cooldown ladder per
  source.
- **Bridged forges** — hg, svn, fossil — converted to git via
  incremental fast-import (`work/bridges` branch → `8596a25` "Add
  hg/svn/fossil -> git bridges with incremental conversion";
  `internal/bridges`). Fossil clones become sidecars and also yield
  ticket/wiki/forum/tech-note extradata (README line 31).
- **IETF document corpus** — RFCs, drafts, BCPs, FYIs. Originally over
  rsync (commits `f792104`, `ed03456`); later replaced by HTTPS index
  + conditional GET when the user decided the rsync transport was the
  wrong tool (`8c7fd5c`: "ietf: replace rsync transport with HTTPS
  index + conditional GET"; followed by a generic `internal/webmirror`
  conditional-GET cache, commit `cbb2803`).
- **Arbitrary websites** via `web:<URL>` repos.txt entries. Two
  capture modes: static-HTTP walker and a real headless browser
  (chromedp, AGENTS.md line 53). Body+asset bytes go through a
  content-addressed `internal/blobs` store; per-URL revisions through
  `internal/webpage`'s PageView. WACZ 1.1.1 export (`gimir
  export-wacz`) — AGENTS.md line 116, all six phases shipped.
- **MediaWiki instances** — `wiki:<domain>` entries. PLANNED, large,
  fully designed in `docs/wikipedia-import-plan.md` (researched
  2026-06-12 against live `dumps.wikimedia.org`). Per-instance page
  depot, time-indexed titles, instances KV with sitematrix +
  siteinfo, columnar lanes per page (text / comment / contributor /
  revmeta / events), pretrained per-instance zstd dictionaries, and
  a companion **wikitext renderer** for date-sliced browsing
  (`docs/wikipedia-browsing-plan.md`). This sub-project is large
  enough — and complete enough as a design — to be treated as a
  project-within-the-project; the renderer is "the largest single
  work item in the whole Wikipedia effort — larger than import"
  (browsing-plan §1).
- **Excluded by policy.** Build artefacts and source archives are
  not mirrored (README line 33). GitHub wiki extradata is a separate
  clonable repo, deferred (AGENTS.md line 110).

## 4. The user's accumulated discipline

These are the recurring rules pulled from `AGENTS.md` "Hard rules" and
"Patterns to follow", from `CLAUDE.md`, and from commits where prior
iterations were rejected.

- **Vendor C source, no system libraries at runtime.** README lines
  4-7, 19; AGENTS.md line 22; commit `246ee8d` "build: vendor libzstd
  and sqlite via submodules; drop external deps". The Makefile is the
  cgo build seam (`4352a51`).
- **Static linking on Linux via musl.** README line 20 names the
  exact build command. macOS is unchanged.
- **Trust the platform.** CLAUDE.md §3 is the long form; in practice
  the project refuses CRCs, magic numbers, journals, fsck, and
  recovery code on top of guarantees the OS, the filesystem, or
  SQLite already provides. The `wikimak/depot/SPEC.md` explicit
  "Out of scope" list and notes/eval-B-proposed.md §3 record this as
  the project's strongest principle.
- **No porcelain recovery verbs.** AGENTS.md rule 1: no Reset, Heal,
  Prune, Refresh, Abort, Recover, Fix, Force. Admin UI exposes
  primitive entities (Source, Mirror, Ref, Log, Depot, FetchStateRow)
  with list/add/delete; composition handles recovery. "Reset
  extradata" is "go to the mirror page, find the ref, click delete."
- **No type-to-confirm gates.** AGENTS.md rule 2; commit `970b3c3`
  "admin: replace type-to-confirm gate with single-click delete +
  undo". `<details>` + "type DELETE" was named friction-as-theater.
  Single-click + Undo, because deletion is cheap to reverse (rename
  to `.trash/`, remember the SHA, etc.).
- **No JS frameworks.** AGENTS.md rule 3: `html/template` only. No
  React, htmx, Alpine, fetch+innerHTML. Full-page POST/redirect for
  mutations; URL state for things like list expansion.
- **No CLI flag proliferation.** AGENTS.md rule 4: admin operations
  belong in the web UI, not in `gimir pull --reset --force --heal`.
- **Cooldown ladder is bypassable on explicit user action.** AGENTS.md
  rule 5: a human click or `--verbose` is intent, not automation.
  Commits `342a61d` and `ae9bd4a` record this.
- **Forensic logging is always on.** AGENTS.md rule 6; commit
  `16a00ef` "runlog: per-run forensic log of subprocess + HTTP events".
  Every external operation gets captured to
  `~/.cache/gimir/runs/<ISO8601>-<pid>.log` with full headers and
  redacted bodies. A logging `http.RoundTripper` is installed at
  `http.DefaultTransport` so any new HTTP-using code is auto-logged.
- **No per-backing capability flags in the Store abstraction.**
  AGENTS.md rule 8 + commit `f05d466` "store: delete Capabilities —
  every backing implements the same contract". A previous
  `Capabilities()` method with `FastScan`/`AtomicMultiPut`/
  `FullTextIndex` flags was deleted as the named leak. "Either a
  feature is in the interface and every backing implements it, or it
  doesn't exist at all."
- **Strict downward import layering.** AGENTS.md lines 13-25: `mirror`
  does NOT import `providers`; engine kind dispatch goes through
  `mirror.SetSourceKindLookup` wired from `cli`. No upward edges.
- **Storage sequences are newest-first; `Prepend` is the name.**
  AGENTS.md line 122 (W1 round 2 / "NAMING RULE"); commits `117a0f3`,
  `e487875`, `d8bbcea`. The user explicitly struck down a "temporal
  vocabulary" exemption — every operation that places a new element
  at the front of the read order is `Prepend`, in chain, mux,
  `VersionedStore`, and `ColumnarStore`. Domain APIs that aren't
  sequence APIs use order-neutral verbs (`webpage.PageView.Record`).
- **Atomic file writes.** AGENTS.md "Patterns": tmp + `os.Rename`,
  always.
- **Sympathetic-test resistance.** AGENTS.md lines 60-74 list five
  named sympathetic-test failures in the project's own past
  (`TestStoreCheckpointsByItemCount`, `TestIETFIndexCooldownSkipsFetch`,
  `TestRenderRecursesIntoLargeBucket`,
  `TestAdminPullSelectedDispatches`, `TestVBFRefPrefixWireFormat`).
  The mitigation is the two-agent split: tester first, implementer
  later, with an independent verification pass between them.
- **AGENTS.md is the shared lore file.** AGENTS.md lines 1-3 frame it
  as "the storyteller's notebook — the project context every agent
  dispatched into this repo needs." CLAUDE.md (added later) is the
  principles document — simplicity, data layout, trust the platform,
  tests pin behavior, comments rot — distilled into rules.
- **The user's design is the contract.** CLAUDE.md closes on this:
  "The user's stated design is the contract. Simplicity means doing
  exactly what the design specifies — no more (no scaffolding) and
  no less (no skipping load-bearing parts)." This rule arose from the
  Rust depot/wikipedia failure recorded in `notes/eval-A-current.md`,
  where an implementer dropped zstd encoding entirely because the
  test suite did not pin it.

## 5. Iteration history (what's been tried, kept, or superseded)

### Foundation (very early on `main` and the `work/*` branches)

- `537eba9` "Add gimir foundation: mirror engine, CLI, and pluggable
  interfaces" — the first real shape: a mirror engine, a CLI, an
  interface for sources.
- `fcb40b0` per-host concurrency limiter shared by git fetches and
  providers (`internal/hostlimit`).
- `120c97e` move extradata storage from filesystem JSON into git refs.
  This was the first big representation shift: JSON sidecars were
  rejected in favor of git refs holding the extradata blobs.
- `work/grep`, `work/serve`, `work/bridges`, `work/providers` (merged
  `6284517`, `c13874b`, `8596a25`, `0812453`): the four pillars
  beyond bare git mirroring landed in parallel — search, browse,
  source-forge bridges, and forge extradata fetchers.
- `760d52c` "Use reftable inits and geometric depot repack
  opportunistically" — the user is reaching for newer git features
  (reftable, geometric repack) that match the "fancy git features"
  framing in the README.
- `3b0b03b` "mirror: publish mirror refs into depot so forks
  negotiate from shared history" — depot-as-shared-objects becomes
  the storage model.

### The cooldown + extradata work (mid history)

- `14ce3e6` "mirror: generalize fetch-cooldown to every source with
  success+failure ladders" — the cooldown ladder enters the design.
- `ab28868`, `b6b4f1c`, `6ec1f00`, `78de946`, `c32c0cb` — the
  extradata sync gets per-item checkpointing, per-kind failure
  isolation, totals/progress, and a tree-as-state DESC stop-when-
  known rewrite. Several of the sympathetic-test failures listed in
  AGENTS.md trace back here.
- `cooldown-admin-WORK`, `admin-ui-WORK`, `admin-ux-WORK`,
  `mirror-state-store-port-WORK` (and TESTS): the cooldown UI and
  state store. Resulted in commits `e42e5a5` (primitive CRUD surface
  for sources/mirrors/refs/logs/depot), `970b3c3` (single-click +
  undo replacing type-to-confirm), `5f433c8` (bulk delete also
  reversible), `342a61d` (admin click bypasses cooldown), `57cb51a`
  (in-flight pull visibility + cancel), `6f46526` (per-mirror
  cooldown inspector), `4cf4dd5` (admin pull wires AfterPull so
  extradata sync actually runs — the named user-reported bug).
  These commits are where AGENTS.md hard rules 1, 2, 5, 7 crystallized.
- `sqlite-state` branch / commit `1e07f0e` "mirror: replace per-mirror
  JSON sidecars with sqlite state.db" — the second representation
  shift: JSON sidecars rejected again, this time in favor of SQLite.

### IETF mirroring (a probe that became the webmirror primitive)

- `f792104` adds an IETF source over rsync, with multiple iterations
  (`4cfd3c3`, `ce18811`, `eaa73b6`). The user wrote a focused pure-Go
  rsync receiver (`3758c01`) and then replaced it with picosh
  (`ed03456`). The rsync mode was then **superseded** by HTTPS +
  conditional GET (`8c7fd5c`), which generalized into
  `internal/webmirror` (`cbb2803`) and a delta-fetch persisted cache
  for the datatracker index (`8a3b2a2`). Lesson recorded: the wrong
  transport had been picked first.

### VBF — the original versioned-binary-format attempt

- `vbf-cgo-zstd`, `vbfchain-impl-WORK`, `vbfmux-impl-WORK`, plus the
  README commits `2308370` and `d0c48a4`. The first VBF was a
  monolithic `vbf.Store`/`Open`/`Prepend`/`IterateAll` with fixed
  frame-0/1/2 sizes-metadata-data layout (AGENTS.md line 120).
- `5acc70b` "vbf: switch to DataDog/zstd for true refPrefix encoding"
  — the user found the standard library zstd wasn't doing real
  refPrefix and switched cgo bindings.
- `efe14bd` "vbf: size encoder windowLog to fit prefix+src" — caught
  a windowLog bug; the `TestVBFRefPrefixWireFormat` corpus that hid
  this bug is on the AGENTS.md sympathetic-test list.
- **Superseded** by the chain+mux split (see below). The pre-refactor
  monolithic VBF was DELETED in `fd6f2b0` once it had no remaining
  callers, because "its presence misled readers into thinking it was
  the live format" (AGENTS.md line 120).

### Chain + mux — the current versioned storage primitive

- `vbfchain-impl-WORK` / `vbfmux-impl-WORK`: the split into a
  low-level append-only newest-first zstd-frame chain (`internal/vbf/
  chain`) and a columnar per-lane Mux on top (`internal/vbf/mux`).
  Documented in README §"Versioned storage: chain + mux".
- `chain-pretrained-dict-WORK` / `-TESTS`: pretrained dictionaries
  via `ZDICT_trainFromBuffer`, dict_id riding in the standard zstd
  frame header (replacing the prior VDID skippable-frame sidecar).
  Commit `a7e2d90`. Raw content bytes are rejected as dicts.
- `w1-chain-seal-WORK` / `-TESTS` (round 1) and `w1-columnar-WORK` /
  `-TESTS` (round 2): re-shaped the chain API around the Wikipedia
  use case. `Prepend` got replaced by `Build` + `Append(spillNew)`
  (commit `04a1b4c`), and then renamed back to `Prepend` per the
  naming rule (`e487875`). The framing callback was deleted. The
  columnar capability was added to `store.Store` (`a274860`) across
  all three backings, with streaming compression for the sealed
  frame (`fef5770`).

### The Store abstraction (a sustained refactor — R1-R6)

The user spent six numbered rounds turning bytes-on-disk into an
interface contract.

- `store-iface-tests-WORK` + `store-mem-impl-WORK` (R1): the
  interface plus a `storetest.Suite` and an in-memory reference
  impl (`b63bdaa`, `911d6ba`).
- `store-fs-impl-WORK` (R2): the filesystem-backed Store (`534d3ce`).
- `webpage-store-impl-WORK` / `-tests-WORK` (R3): migrated the
  webpage layer onto the Store interface (`fecf4cd`).
- `store-sqlite-impl-WORK` / `-tests-WORK` (R4): single-file SQLite
  Store (`08d1380`); same suite passes — "abstraction validated
  across three structurally-different backings."
- `store-binding-tests-WORK` (R5): per-mirror store binding via
  `stores.txt` + `repos.txt store=NAME` + admin UI (`f223d94`,
  `f78ef20`). Backward-compat: absent `stores.txt` yields an
  implicit `default` fs store at cacheRoot.
- `mirror-state-store-port-WORK` / `-TESTS` (R6): the global
  `state.db` (cooldown, submodules, schema) ported to `store.KV()`
  with a one-shot migration importing both legacy `<cacheRoot>/
  state.db` (renamed `.migrated` after copy) and JSON sidecars
  (`e83c6c0`, `80ca4d1`). Also fixed a latent bug:
  `Engine.DeleteMirrorState` now takes a `Mirror` instead of a
  `mirror_key string`, because fetch_state and submodule_state use
  different basenames.
- The `Capabilities()` flag was deleted (`f05d466`) — see §4.

### Website mirroring (six phases, all shipped)

`blobs-impl-WORK` / `-tests-WORK` (Phase 1, `internal/blobs`);
`webpage-impl-WORK` / `-tests-WORK` (Phase 2, static HTTP);
`browsercap-impl-WORK` / `-tests-WORK` (Phase 3, headless via
chromedp); `webpage-phase4-conditional-get-WORK` (Phase 4 conditional
GET); `webpage-phase5-wacz-WORK` (Phase 5 WACZ export);
`webpage-phase6-wiring-WORK` / `-TESTS` (Phase 6 wiring: `web:<URL>`
repos.txt entries → `providers.webSource` opens a PageView over
`Mirror.Store`, engine skips `git init` for `Kind() == "web"`, admin
per-mirror handler dispatches on `web/` prefix to a HEAD-less
renderer with per-URL revision view). AGENTS.md line 116 records all
six as done.

### Wikipedia — the largest single thread

`docs/wikipedia-import-plan.md` (initial: `06f7537`, 2026-06-12) and
its many follow-up commits. The plan evolved as research and
implementation revealed constraints:

- `a714d7c` text lane gets a pretrained dict + sample-train-repack
  workflow.
- `f15c324` then `da77106` then `7b47439` then `8573616` then
  `9090023` — depot backing debated through several shapes: per-frame
  sqlite rows; KV-separated LSM; tiered chain-native (head store +
  immutable frame packs); restored accumulator tier. The user keeps
  rewriting the backing tier to match measured behavior.
- W1 (chain seal + columnar): COMPLETE, both rounds, see above.
- W2 (`w2-dumps-WORK` / `-TESTS`) — `internal/mediawiki`. Discover,
  Fetch, NewBz2Reader (pure-Go block-parallel — bit-scan for 48-bit
  block magic, synthetic single-block streams, in-order reassembly),
  NewPageStream (export-0.11 streaming parser including
  `deleted="deleted"` flags and SiteInfo), VerifyRevSHA1.
  Live-verified end-to-end on votewiki (1328 pages / 3678 revs, all
  sha1 exact). Commit `858b0ef` "mediawiki: fix content-history
  layout to live reality" — the *legacy* dumpstatus.json layout
  was wrong; the live one is `parts+SHA256SUMS+_SUCCESS` under
  `<date>/xml/bzip2/`. Commit `9d38cce` records the split-pages
  reality (enwiki splits pages by revision range across files —
  `pXrAAArBBB` names).
- W3 (`w3-importer-WORK` / `-TESTS`) — `internal/wikipedia`:
  per-page atomic Seed/Prepend, sha1 policy (as-dumped text with
  fudge counted), direct-[[Category:]] transition index. Live
  verified on votewiki (51s full, 35µs rerun no-op). Recorded
  weakness: fs on-disk = 67MB from 6.2MB bz2 — predicted fs fan-out
  overhead at tiny scale, not compression failure (AGENTS.md
  line 122).
- `rust-chain-SPIKE` (`6573ea8` "spike: chain primitive in Rust for
  language comparison") — the user wanted a side-by-side language
  comparison for the chain primitive.
- `strpool-crate` + `strpool-crate-rebuild` (`fa86e48`, `7acaecc`,
  `0382c12`) — an append-only sharded byte-string pool, single file
  per shard, with defensive scaffolding removed (snapshot, unsafe
  transmute, size cap, derivable knobs all stripped). Merged via
  `0c5919c`. This was where the depot's "trust the platform"
  discipline was first crystallized as a separate crate.
- `wikimak/` workspace (`5a0bf17`) — a Rust workspace with `depot`,
  `mediawiki`, `wikipedia` crates and per-crate SPEC.md files.
- `w3-rust-1-depot-WORK` / `-TESTS`, `w3-rust-2-mediawiki-WORK` /
  `-TESTS`, `w3-rust-3-wikipedia-WORK` / `-TESTS` — the Rust ports
  of W1/W2/W3. The depot is built around the three-tier shape
  (f0/f1/cold) documented in `wikimak/depot/SPEC.md` and §2.7 of the
  import plan.
- **The most recent named failure** is captured in
  `notes/eval-A-current.md`: the Rust `wikipedia` crate W3-Rust-3 was
  implemented WITHOUT calling zstd anywhere — it concatenates raw
  record bytes as the f1 accumulator and never seals. The evaluator
  notes that the implementer chose "the simplest scheme that passes
  the suite" and dropped zstd + dict training + sealing entirely.
  This is precisely the failure pattern that CLAUDE.md was then
  written to forbid. `eval-B-proposed.md` evaluates the §2.7 design
  itself and finds it sound. Whether the Rust port stays or gets
  reworked is unclear from the history; the eval notes are the
  latest commits on the working branch.

### Browsing/rendering plan

- `docs/wikipedia-browsing-plan.md` (`90d5a9c`, with subsequent
  amendments `81cdba3`, `195c9b3`, `4c8580e`) — the wayback-style
  date picker forces a local wikitext renderer
  (`render(instance, title, τ)`); renderer = parser core +
  transclusion engine + Scribunto via embedded Lua 5.1 + lazy media
  pipeline (no bulk media dumps exist). Accuracy is measured against
  MediaWiki's parserTests corpus + a live-diff harness. Phases B1-B7
  defined. B1 had a feasibility spike: `858fd19` "wikitext: B1
  feasibility spike — parser core renders real articles
  recognizably." `internal/wikitext` exists as a directory but is
  early-stage.

## 6. The constant goals (what has stayed the same)

From the very first README to the latest plan:

- **Local-only, single-user.** The localhost-only admin UI, the
  cache directory, the `~/.config/gimir/repos.txt` text file. There
  is no remote, no server, no API, no team mode.
- **Statically linked binary; vendor C source.** README, since the
  earliest README edits and the `vendor-cgo-deps` branch.
- **Read-only mirrors; full upstream preservation.** README line 50,
  unchanged. No push, no edit. Everything reasonably preservable
  gets preserved (the "as much of extra data as it can reasonably
  obtain" framing in README line 28).
- **Forks and submodules don't duplicate.** Shared depot via
  alternates, present from `537eba9` onward; restated in AGENTS.md
  line 7.
- **Append-only, newest-first storage.** Once "chain" entered the
  design, it has stayed append-only with the newest record at the
  front of the read order. The naming rule (`Prepend` everywhere)
  pins this.
- **Trust the platform.** No CRCs / magic / journals / fsck added on
  top of OS, filesystem, or SQLite guarantees. Reinforced in every
  storage iteration.
- **"Fast and compact even when getting big."** README line 57. The
  whole VBF → chain+mux → tiered depot arc is the user trying to
  honor this at enwiki scale.
- **No CLI flag accumulation; admin lives in the web UI.** AGENTS.md
  rule 4, unchanged.
- **No JS frameworks.** AGENTS.md rule 3, unchanged.
- **Forensic runlog is always on.** AGENTS.md rule 6, unchanged.

## 7. Known open work

From AGENTS.md "Things the user has asked for that haven't shipped"
(lines 114-122) and from `wikimak/PHASES.md`:

- **Shipped.** Website mirroring phases 1-6. The storage abstraction
  refactor R1-R6. The VBF chain+mux refactor and the
  chain-pretrained-dict layer. W1 chain seal + columnar (both
  rounds). W2 mediawiki dump plumbing (live-verified on votewiki).
  W3 Go importer over ColumnarStore (live-verified on votewiki).
- **In motion in Rust.** The `wikimak/` workspace contains a Rust
  port: W3-Rust-1 (depot), W3-Rust-2 (mediawiki), W3-Rust-3
  (wikipedia, evaluated against CLAUDE.md and found to drop zstd
  encoding — see `notes/eval-A-current.md`). Whether the Rust port
  continues, gets reworked, or is abandoned in favor of the
  already-shipped Go path is **unclear from the history**; the
  most recent commits on the working branch are evaluation notes,
  not a decision.
- **Planned, not yet implemented (Go side).** W4: the `wiki:`
  provider that wires `internal/wikipedia` into the engine + admin.
  W5+: bake-off on cswiki (fs vs sqlite, size/throughput/churn),
  per-instance dictionary training repack, incremental
  (adds-changes + EventStreams), file-metadata ingestion (W7).
- **Planned, not yet implemented (browsing).** B1 had a feasibility
  spike; B2-B7 are unstarted. The Scribunto engine choice (vendored
  PUC Lua 5.1 via cgo vs gopher-lua) is an explicit B3 spike.
- **Declined by the user** (AGENTS.md lines 105-112). Trash UI;
  JS-free inline expansion; live counter sparklines; GitHub wiki
  extradata; per-source cooldown-ladder editor; auth gating for
  admin.

## 8. What this note is NOT

This is not a design proposal, a code review, an evaluation of past
iterations, or a recommendation for what to build next. It is a
descriptive record of what gimir is and what its user has
consistently asked of it across the iterations on record.

Where two sources phrase it differently, both phrasings are noted:
README.md frames gimir as "local mirror manager that uses new fancy
git features and manages the local mirror depot automatically";
AGENTS.md frames it as "a locally-run, single-user git mirror
manager." The two are compatible; neither dominates.

Where the history is genuinely ambiguous — most notably, whether
the Rust port of W3 will be continued or reworked after the
zstd-omission failure recorded in `eval-A-current.md` — the note
says so and stops.
