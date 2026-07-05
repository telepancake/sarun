# Data mirrors — the program

The point of the whole depot substrate (DEPOT-DESIGN.md): local,
incrementally-updated mirrors of external corpora, stored in the layout
each corpus's shape wants, served through sarun. Three mirrors first:

| mirror | shape | store | state |
|---|---|---|---|
| **wikipedia** | ~99%-identical revision chains per page | `wikimak/*` (depot chains, un-sabotaged 2026-07, 12× measured) | `wikimak` CLI: import/head/text/history + discover/fetch sync with `parts_seen` watermarks |
| **IETF drafts** | revision chains per draft name (`draft-x-00..-NN`) — the tiered-VBF doc's other named workload | multi-chain `depot-vbf::VbfDepot` (canonical layers) + sqlite bookkeeping | `ietf-mirror` crate + `ietfmak` CLI: update (idempotent, incremental, 404-watermarked) / list / head / text / history |
| **git repos** | DAG of tree snapshots, newest-first | `gitdepot` (view-anchored chains; SHA-exact export) | import/export/`update` (incremental fast-forward append); no fetch loop yet |

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
- **Update**: incremental by design — chains append; git mirrors need
  incremental import (see gitdepot TODO).

## Phases

1. **wikipedia driver** (`wikimak` CLI): DONE — import + head/history/
   text, and `discover`/`fetch` (`sync`) against dumps.wikimedia.org
   with per-part watermarks and streamed checksum verification.
2. **IETF drafts** (`ietf-mirror` crate): DONE — `all_id.txt` index →
   per-draft chains of full-snapshot canonical layers in a multi-chain
   `VbfDepot`; sqlite for series state; `update` idempotent + resumable
   (revision watermarks; listed-but-404 revisions watermarked missing).
3. **git mirror loop**: gitdepot incremental import DONE (`update`:
   new frames prepended, former head's standalone frame replaced by a
   bridge delta, all older frames verbatim; fast-forward-only, refuses
   rewritten/topo-interleaved history → re-import). Remaining:
   fetch-and-update loop over remotes, then RO-attach a ref via the
   cache.
4. **Serve/browse**: wikipedia browsing plan (docs/wikipedia-browsing-
   plan.md) + list-widget DAG navigation already landed; a pane per
   mirror later.

## Non-goals for now

Provider extradata (issues/PRs), CDP capture, full provider matrix —
SCOPING.md keeps the record; mirrors of bulk corpora come first.
