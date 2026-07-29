# Data mirrors — the program

The point of the whole depot substrate (DEPOT-DESIGN.md): local,
incrementally-updated mirrors of external corpora, stored in the layout
each corpus's shape wants, served through sarun. Three mirrors first:

| mirror | shape | store | state |
|---|---|---|---|
| **wikipedia** | revisions plus typed page/user/global actions | portable `.swdump` event stream with embedded dictionary + generated `.swtitle` lookup | `wikimak` CLI: full-snapshot bootstrap, daily adds/changes merge, MediaWiki History actions, direct local browse |
| **IETF drafts** | revision chains per draft name (`draft-x-00..-NN`) — the tiered-VBF doc's other named workload | multi-chain `depot-vbf::VbfDepot` (canonical layers) + sqlite bookkeeping | `ietf-mirror` crate + `ietfmak` CLI: update (idempotent, incremental, 404-watermarked) / list / head / text / history |
| **git repos** | DAG of tree snapshots, newest-first | `gitdepot` store (tiered four-chain wikimak-depot store — TREES/COMMITS/REFLOG/TAGS with stable indices; annotated tags stored as raw tag objects, nested chains included, tags at trees supported (deduped to a commit's tree or imported as a standalone TREES record; blob-target tags are the only refusal), refs resolve peeled; bounded prepend, proven by roundtrip.rs update_io_is_bounded_not_o_history; SHA-exact export, tag objects verbatim; no re-import path — a rewrite is new records + repointed refs) | import/export/`update` (incremental prepend, rewrites included) + `mirror` (bare-clone fetch loop) |

## Common architecture (per DEPOT-DESIGN)

- **Store**: each mirror uses a shape-appropriate format. Wikipedia keeps its
  source identity and update frontier in the portable event stream; IETF and
  git continue to use depots.
- **Fetch**: eventually inside sarun tap boxes (SCOPING.md's mesh: flows
  visible, per-host limits, tokens host-side). First iterations may fetch
  host-side; the box move is mechanical later.
- **Serve**: Wikipedia reads frames directly from the archive; other mirrors
  retain their depot APIs. A pinned wiki revision or git ref attaches to a box
  with no checkout.
- **Update**: incremental by design — chains prepend (newest-first; the new head is frame 0). Scheduled by the
  engine (`engine/src/mirrors.rs` + `sarun mirror` CLI + the Mirrors
  pane): jobs in `{state_home}/mirrors.db`, a minute tick starts due
  ones, states running/paused/pending/scheduled/completed/error/stopped,
  force-run and run-pending on demand. The drivers are compiled into the
  sarun binary (multi-call dispatch: `sarun gitdepot|wikimak|ietfmak …`
  or an argv[0] symlink); a run spawns the engine's own binary in driver
  mode, so the engine PROCESS still never dials out — fetch happens in
  the child. Interrupted runs surface as
  `stopped` and auto-resume.
- **Portable Wikipedia libraries**: each `.swdump` is self-identifying through
  its manifest; its mount path and filename are not identity. In the Mirrors
  pane, `O` opens one archive or a directory of archives. Sarun validates the
  archive and adjacent `.swtitle`, then records the current absolute path in
  this host's `mirrors.db`. Attached jobs start paused—browsing is immediate,
  while network upkeep requires an explicit resume.

## Phases

1. **wikipedia driver** (`wikimak` CLI): DONE — create/update, pinned revision
   attachments, document rendering, and local HTTP browsing all read the
   archive format. A new mirror bootstraps from full revision XML and every
   MediaWiki History partition. Routine `fetch` merges daily adds/changes with
   the newest completed and current partial History partitions, using three
   days of overlap. The installed result is repacked at zstd level 9 into
   128 KiB frames with an 800 KiB embedded dictionary, then `.swtitle` is
   regenerated. `refresh-full` is an explicit full re-download and is never
   scheduled automatically. Scratch-space reduction remains future work.
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
remain design or follow-up work.

### Revision storage

- [x] A fresh page must finish as one standalone f0 plus sealed cold
  history (or f0 alone for a one-revision page), never with an f1.
  Page-scoped collection/reversal is the accepted memory bound; all
  older revisions form exactly one cold frame.
- [x] Train one 64 KiB native-zstd f0 dictionary per Wikipedia instance
  after the initial full import, from a deterministic sample capped at
  2,048 heads/32 MiB. Persist and activate it before crash-resumable
  f0-only repacking; daily updates reuse it. Mixed plain/dictionary heads
  remain readable, and f1/cold frames remain dictionary-free refPrefix
  frames. No custom frame envelope is used.
- [x] Wire the depot's streaming frame decoder into Wikipedia history
  walks so an exceptional cold frame is not materialized whole.
- [x] A fresh depot index must start small and grow geometrically. Do
  not preallocate the enwiki-sized 100,000,000-slot/800 MB index.

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
- [x] Apply page move/deletion events to the browsing title timeline
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
