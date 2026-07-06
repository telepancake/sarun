# Data mirrors — the program

The point of the whole depot substrate (DEPOT-DESIGN.md): local,
incrementally-updated mirrors of external corpora, stored in the layout
each corpus's shape wants, served through sarun. Three mirrors first:

| mirror | shape | store | state |
|---|---|---|---|
| **wikipedia** | ~99%-identical revision chains per page | `wikimak/*` (depot chains, un-sabotaged 2026-07, 12× measured) | `wikimak` CLI: import/head/text/history + discover/fetch sync with `parts_seen` watermarks |
| **IETF drafts** | revision chains per draft name (`draft-x-00..-NN`) — the tiered-VBF doc's other named workload | multi-chain `depot-vbf::VbfDepot` (canonical layers) + sqlite bookkeeping | `ietf-mirror` crate + `ietfmak` CLI: update (idempotent, incremental, 404-watermarked) / list / head / text / history |
| **git repos** | DAG of tree snapshots, newest-first | `gitdepot` store (tiered four-chain wikimak-depot store — TREES/COMMITS/REFLOG/TAGS with stable indices; annotated tags stored as raw tag objects, nested chains included, refs resolve peeled; bounded prepend, proven by roundtrip.rs update_io_is_bounded_not_o_history; SHA-exact export, tag objects verbatim; no re-import path — a rewrite is new records + repointed refs) | import/export/`update` (incremental prepend, rewrites included) + `mirror` (bare-clone fetch loop) |

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

## Phases

1. **wikipedia driver** (`wikimak` CLI): DONE — import + head/history/
   text, and `discover`/`fetch` (`sync`) against dumps.wikimedia.org
   with per-part watermarks and streamed checksum verification.
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
   `wiki_attach` (a page's head text), `ietf_attach` (a draft's full
   revision series) — one CLI surface: `sarun NAME attach git|wiki|ietf
   SRC REF [AT]`. Each appends one pinned read-only reference (named
   `git:main@sha8`, `wiki:enwiki/Title@r100`, `ietf:draft-x@01`),
   served lazily through the readout trait and shown on the owning
   session row (`attachments` in the session dict). The
   mirror crates' read paths are feature-gated (`fetch` off in-engine):
   the engine never dials out; fetching stays in wikimak/ietfmak/
   gitdepot. `test_mirror_attach_rs.py` proves all three through the
   real CLI. Later: browse panes per mirror; read-at-rev adapters (wiki/ietf pin identity today, git pins content).

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
