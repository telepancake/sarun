# Data mirrors — the program

The point of the whole depot substrate (DEPOT-DESIGN.md): local,
incrementally-updated mirrors of external corpora, stored in the layout
each corpus's shape wants, served through sarun. Three mirrors first:

| mirror | shape | store | state |
|---|---|---|---|
| **wikipedia** | ~99%-identical revision chains per page, plus page actions | `wikimak/*` (depot chains, un-sabotaged 2026-07, 138× measured after session-end `collect`) | `wikimak` CLI: full-snapshot bootstrap, daily adds/changes maintenance, MediaWiki History page actions, local browse |
| **IETF drafts** | revision chains per draft name (`draft-x-00..-NN`) — the tiered-VBF doc's other named workload | multi-chain `depot-vbf::VbfDepot` (canonical layers) + sqlite bookkeeping | `ietf-mirror` crate + `ietfmak` CLI: update (idempotent, incremental, 404-watermarked) / list / head / text / history |
| **git repos** | DAG of tree snapshots, newest-first | `gitdepot` store (tiered four-chain wikimak-depot store — TREES/COMMITS/REFLOG/TAGS with stable indices; annotated tags stored as raw tag objects, nested chains included, tags at trees supported (deduped to a commit's tree or imported as a standalone TREES record; blob-target tags are the only refusal), refs resolve peeled; bounded prepend, proven by roundtrip.rs update_io_is_bounded_not_o_history; SHA-exact export, tag objects verbatim; no re-import path — a rewrite is new records + repointed refs) | import/export/`update` (incremental prepend, rewrites included) + `mirror` (bare-clone fetch loop) |

## Common architecture (per DEPOT-DESIGN)

- **Store**: each mirror's data in its shape-appropriate depot; bookkeeping
  (fetch cooldowns, watermarks, dump/series state) in its own sqlite —
  never in the depot (§3).
- **Fetch**: eventually inside sarun tap boxes (SCOPING.md's mesh: flows
  visible, per-host limits, tokens host-side). First iterations may fetch
  host-side; the box move is mechanical later.
- **Serve**: reads through the depot APIs; workspace access via RO
  attachments (§8), materialized through the depot-cache (§7) — a wiki
  snapshot or a git ref attaches to a box with no checkout.
- **Update**: incremental by design — chains prepend (newest-first; the new head is frame 0). Scheduled by the
  engine (`engine/src/mirrors.rs` + `sarun mirror` CLI + the Mirrors
  pane): jobs in `{state_home}/mirrors.db`, a minute tick starts due
  ones, states running/paused/pending/scheduled/completed/error/stopped,
  force-run and run-pending on demand. The drivers are compiled into the
  sarun binary (multi-call dispatch: `sarun gitdepot|wikimak|ietfmak …`
  or an argv[0] symlink); a run spawns the engine's own binary in driver
  mode, so the engine PROCESS still never dials out — fetch happens in
  the child. Interrupted runs surface as
  `stopped` and auto-resume — safe because the stores self-repair
  (dirty-flag chain repair in wikimak, watermark fences in ietf-mirror,
  per-root flocks in both).
- **Portable Wikipedia libraries**: the mirror root is self-identifying
  (`wiki_dbname` in `meta.db`); its mount path and directory name are not
  identity. In the Mirrors pane, `O` opens either one existing root or a
  directory containing roots. Sarun validates `meta.db`, `depot/`, and
  `titles/` read-only, then records the current absolute paths in this
  host's `mirrors.db`. Attached jobs start paused—browsing is immediate,
  while network upkeep requires an explicit resume.

## Phases

1. **wikipedia driver** (`wikimak` CLI): DONE — import + head/history/
   text and local HTTP browsing. A new mirror bootstraps once from a full
   content-history XML snapshot. Routine `fetch` consumes only daily
   adds/changes and MediaWiki History page-action TSVs: initially every
   action partition, then the previous snapshot's frontier partition plus
   every later partition from a newer History snapshot (the former frontier
   is partial and expands in the next snapshot).
   Full XML re-ingest is the separate explicit `refresh-full` command;
   all-partition action/visibility reconciliation is the separate explicit
   `reconcile-history` command. Neither is scheduled automatically.
   Advertised XML SHA-256/SHA-1/MD5 is calculated during direct streaming:
   a mismatch leaves complete pages recovered but refuses the part watermark,
   so a later copy deduplicates the valid prefix and continues. History TSVs
   have no adjacent advertised digest, so each bzip2 stream is imported in
   one SQLite transaction and rolls back on decoding or parse failure.
   History also records upstream revision visibility/suppression metadata;
   it never removes archived revision content. Because Wikimedia may revise
   arbitrary old partitions, the frontier update is recorded separately
   from the last explicitly fully reconciled History snapshot.
2. **IETF drafts** (`ietf-mirror` crate): DONE — `all_id.txt` index →
   per-draft chains of full-snapshot canonical layers in a multi-chain
   `VbfDepot`; sqlite for series state; `update` idempotent + resumable
   (revision watermarks; listed-but-404 revisions watermarked missing).
3. **git mirror loop**: gitdepot incremental import DONE (`update`:
   new tree/commit/reflog records batch-prepended to the tiered chains,
   former tree head demoted to a bridge delta in the accumulator, cold
   history untouched; NO fast-forward requirement — a rewrite or a ref
   deletion is reflog records + refs-table repoints, old commits stay
   resolvable forever). Fetch-and-update DONE (`mirror <url> <root>`:
   bare mirror clone under `<root>/repo.git`, store under
   `<root>/store`; no re-import path). RO-attach DONE and
   CONVERGED (ATTACH-CONVERGENCE.md, 2026-07-05): `git_attach` is pure
   bookkeeping — ref→sha from store metadata only, one pinned Ext row
   `{kind,store,ref,rev,prefix,name}` named `git:<label>/<ref>@<sha8>`;
   the overlay serves it through the depot Readout trait (getattr from
   entry metadata, blobs via depot-cache fds — mmap/exec work), no
   sqlar import, no copy. `test_git_attach_rs.py` proves read-through,
   EROFS, DAG visibility; `test_attach_convergence_rs.py` proves the
   §8 byte-identical invariant and laziness (200-file store: attach is
   O(bookkeeping), one read = one cache blob).
4. **Serve/browse**: all three attach verbs live — `git_attach`,
   `wiki_attach` (the pinned revision of a page), `ietf_attach` (the
   pinned revision of a draft) — one CLI surface:
   `sarun NAME attach git|wiki|ietf
   SRC REF [AT]`. Each appends one pinned read-only reference (named
   `git:main@sha8`, `wiki:enwiki/Title@r100`, `ietf:draft-x@01`),
   served read-at-rev, lazily through the readout trait (a SHARED
   flock taken only for the bounded pinned decode — imports/updates
   run freely alongside a hydrated attachment) and shown on the
   owning session row (`attachments` in the session dict). The
   mirror crates' read paths are feature-gated (`fetch` off in-engine):
   the engine never dials out; fetching stays in wikimak/ietfmak/
   gitdepot. `test_mirror_attach_rs.py` proves all three through the
   real CLI. Later: browse panes per mirror.

## Wikipedia correction backlog (2026-07-27 audit)

The first real lvwiki import and a 15-page, high-revision rendering
sample exposed the following correctness work. These are required
corrections, not optional tuning:

Completed items below are pinned by regression tests; unchecked items
remain design or migration work.

### Revision storage

- [x] A fresh page must finish as one standalone f0 plus sealed cold
  history (or f0 alone for a one-revision page), never with an f1.
  The transient importer RAM bound must not determine the persistent
  f1/cold layout. An exceptional huge page may split into several cold
  frames, but its final partial history must also be cold.
- [ ] Measure and specify the pretrained f0 dictionaries; the current
  Wikipedia frame encoder uses none. The design must first resolve
  per-chain versus per-instance scope, training reservoir/byte caps,
  dictionary size/count and update lifecycle, then define a versioned
  f0 envelope plus durable dictionary identity and crash ordering.
- [x] Wire the depot's streaming frame decoder into Wikipedia history
  walks so an exceptional cold frame is not materialized whole.
- [x] A fresh depot index must start small and grow geometrically. Do
  not preallocate the enwiki-sized 100,000,000-slot/800 MB index.
- [ ] Provide a safe shrink migration for existing oversized indexes.

### Title storage and lookup

- [x] Title shards grow on demand. When a shard crosses its measured size
  target, atomically rebuild with twice as many shards and remap every
  persisted dense title id. Four fixed shards are not an acceptable
  default.
- [x] Exact lookup must touch one small shard directly (or the existing
  keyed title index), not decompress and hash an enormous shard.
  Remove the 64 MiB whole-dictionary request cache once shard sizing
  is correct.

### MediaWiki History metadata

- [x] Treat each monthly MediaWiki History release as a reconstructed
  metadata snapshot. Replacement/reconciliation must be complete and
  atomic across all published parts; partition changes must not leave
  stale action or visibility rows.
- [x] Do not remove archived revision content when later metadata marks a
  revision suppressed. Suppression is additional archival metadata.
- [x] Preserve malformed-field evidence and report it explicitly rather
  than silently coercing records.
- [ ] Apply page move/deletion events to the browsing title timeline
  without erasing archaeological content.
- [ ] The current verbose SQLite representation is intentionally shelved
  for a separate redesign; do not entrench it while fixing the above.

### Rendering fidelity

Regression fixtures must cover the exact audited failures:

- [x] `mw.html` nil attributes/styles are omitted, never rendered as
  `id="nil"` or `style="nil"`.
- [x] `mw.ustring` implements the Unicode pattern behavior needed by
  Latvian climate templates (including `[-–−—]` replacement).
- [x] Template redirects transclude their target with caller arguments
  and retain redirect loop/depth protection.
- [x] Lua functions may return stringifiable `mw.html` objects while a
  plain table remains an error.
- [x] `indicator` is metadata-only; `graph` and `imagemap` receive a safe,
  unobtrusive fallback instead of leaking raw source.
- [x] Extension stripping applies inside references, especially `nowiki`.
- [x] Large valid template invocations such as the audited Wikidata-list
  SPARQL call must not leak as raw braces/query text.
- [x] Percent-encoded wiki and file targets are decoded/canonicalized once,
  never encoded into `%25...`.
- [x] Same-page fragment links remain `#fragment`.
- [x] External links stop their URL before an HTML/strip-marker label.
- [x] Repeated render misses are aggregated by cause and count.

## Non-goals for now

Provider extradata (issues/PRs), CDP capture, full provider matrix —
SCOPING.md keeps the record; mirrors of bulk corpora come first.

## Rejected (do not resurface)

- **Mirroring Wikimedia Enterprise HTML dumps** as a rendered-page
  source (2026-07-05): loses exact edits (no revision chain — the
  corpus IS the edit sequence) and provenance (a third-party render of
  an unknowable input set: which page/template/module revs produced it
  cannot be stated, so it is neither reproducible nor attributable).
  Rendering derives from the mirrored wikitext chains in-house; the
  expansion records its full pin set (page rev + every transcluded
  template/module rev at the chosen τ) and the result is depot-cache
  material keyed by that pin set — never authoritative data.
