# Data mirrors — the program

The point of the whole depot substrate (DEPOT-DESIGN.md): local,
incrementally-updated mirrors of external corpora, stored in the layout
each corpus's shape wants, served through sarun. Three mirrors first:

| mirror | shape | store | state |
|---|---|---|---|
| **wikipedia** | ~99%-identical revision chains per page | `wikimak/*` (depot chains, un-sabotaged 2026-07, 12× measured) | pipeline built: discover → fetch → bz2 → parse → import; NO driver CLI yet |
| **IETF drafts** | revision chains per draft name (`draft-x-00..-NN`) — the tiered-VBF doc's other named workload | `depot-vbf` chains (canonical layers) + sqlite bookkeeping | not started |
| **git repos** | DAG of tree snapshots, newest-first | `gitdepot` (view-anchored chains; SHA-exact export) | import/export works; no incremental update, no fetch loop |

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

1. **wikipedia driver** (`wikimak` CLI): `import <dump(.bz2|.xml)> <root>`,
   `head/history/text <root> <title>` — makes the existing pipeline
   usable end to end on real dumps. Then `discover`/`fetch` wiring for
   dumps.wikimedia.org.
2. **IETF drafts** (`ietf-mirror` crate): datatracker/rsync listing →
   per-draft chains of canonical layers in depot-vbf; sqlite for series
   state; `update` idempotent + resumable.
3. **git mirror loop**: gitdepot incremental import (append new commits
   to the chain instead of full re-import; needs the caller-anchored
   frame mode), fetch-and-update verb, then RO-attach a ref via the
   cache.
4. **Serve/browse**: wikipedia browsing plan (docs/wikipedia-browsing-
   plan.md) + list-widget DAG navigation already landed; a pane per
   mirror later.

## Non-goals for now

Provider extradata (issues/PRs), CDP capture, full provider matrix —
SCOPING.md keeps the record; mirrors of bulk corpora come first.
