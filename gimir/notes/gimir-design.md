# gimir: data design

This document walks the six-step methodology from `CLAUDE.md` §2
over gimir: what the system holds, what flows through it, at what
scale, with what operations, what shape the data wants. Libraries,
byte layouts, and module boundaries live downstream of the shapes
named in step 5.

Citations point at the README, the historian's note
(`notes/gimir-intent-history.md`), and import-plan §1 /
browsing-plan §1.

---

## Step 1: Conceptual data inventory

Every distinct piece of information gimir holds, transmits, or
displays, grouped by domain.

### Configuration and identity

1. **repos.txt entry.** One line per upstream the user wants
   mirrored: kind hint (`git:`, `web:`, `ietf`, `fossil:`, `wiki:`,
   default git), URL or domain, optional `store=NAME`. O(10²) per
   user, user-bounded. Read on every command; user-edited. One
   entry → one Mirror; `store=` binds to one Store. README line 22.
2. **stores.txt entry.** One line per named Store binding
   (`NAME backing args`). O(1)–O(10). User-edited; a Mirror
   references one Store; absent stores.txt yields an implicit
   `default` fs store (historian §5 R5).
3. **cacheRoot directory.** `~/.cache/gimir/`, optionally a
   symlink. Exactly one per user; contains the depot, all mirrors,
   run logs, and all blob and KV state (README lines 24-25).
4. **Mirror.** The unit of mirroring: kind, host, path, Store
   binding, relative path under cacheRoot
   (`mirrors/<host>/<path>.git`, `wiki/<dbname>`, `web/<host>/...`).
   One per repos.txt entry, O(10²). Created on first pull;
   removed by `gimir gc`.
5. **Source kind.** Closed set: `git`, `web`, `ietf`, bridged
   (`hg`, `svn`, `fossil`), `wiki`. ~7 kinds. Determines fetch
   behavior and admin UI dispatch.
6. **Host identity.** DNS name from repos.txt (github.com,
   gitlab.com, codeberg.org, sr.ht, dumps.wikimedia.org,
   upload.wikimedia.org, datatracker.ietf.org, arbitrary). Key for
   per-host concurrency budgets and per-host policies. O(10)–
   O(100).

### Git mirror data

7. **Git object.** Content-addressed by sha (sha1 or sha256
   per upstream's hash family). Per-mirror 10³–10⁶; depot-wide
   meaningfully smaller after dedup across forks and submodules.
   Append-only under hash; immutable until depot GC removes it
   as ref-unreachable. README lines 53-60; `3b0b03b`.
8. **Git ref.** Named pointer (refname, sha). 10²–10⁴ per mirror.
   Force-updateable on each fetch (reftable adopted opportunistically,
   `760d52c`). Refs are the liveness signal for depot GC.
9. **Working-directory clone.** From `gimir clone`: a normal git
   working directory whose `objects/info/alternates` points at the
   depot, so checkout works without copying objects. O(10) per
   user, outside cacheRoot, under user control.
10. **Submodule registration.** Per parent commit, (parent sha,
    path, upstream URL, child sha). O(10²) per mirror. Pulled
    recursively in full (README line 52); derived from gitlinks.

### Forge extradata (uniform schema across providers)

11. **Extradata item.** Wiki page, discussion, ticket/issue, pull
    request, comment thread, release — uniform internal schema
    (README lines 28-32). Stored as git blobs referenced by
    extradata refs (`120c97e`). Per mirror: zero to 10⁵+ for
    active forge projects. Re-fetched per cooldown ladder; keyed
    by (mirror, provider kind, item kind, upstream id).
12. **Forge provider configuration.** Tools used to fetch
    extradata: `gh`, `glab`, `tea`, `hut`, plus the fossil sidecar
    (~5). Configured outside gimir; invoked with host pinned
    (`65592c6`).

### Fossil-bridged-forge data

13. **Fossil sidecar clone.** One fossil repository per fossil-
    bridged mirror — source of tickets, wiki, forum, tech-notes
    (README line 31). O(10⁰)–O(10¹) per user; incremental on pull.
14. **Bridge state.** Per bridged forge (hg, svn, fossil),
    incremental fast-import marks/checkpoint. O(1) per bridged
    mirror; KB each (`8596a25`).

### IETF document corpus

15. **IETF document.** RFC, internet-draft, BCP, FYI; text +
    metadata. ~10⁴ RFCs + ~10⁴ drafts; drafts churn, RFCs
    immutable; pulled by conditional GET (`8c7fd5c`, `cbb2803`).
16. **Conditional-GET cache entry.** Per-URL (etag, last-modified,
    body-hash, fetched-at). ~10⁴ for the IETF index + O(10²)–
    O(10³) per webmirror. Witnesses "we have this version."

### Arbitrary-website data

17. **PageView.** Per-URL sequence of captures with body bytes
    and asset references (static HTTP walker or chromedp).
    O(10)–O(10⁴) URLs per `web:` mirror; O(10⁰)–O(10²) revisions
    per URL. Append on re-fetch; revision identity is
    timestamp + body-hash.
18. **Blob.** Content-addressed byte object: page bodies, assets,
    media. 10⁴–10⁷ per active `web:` or `wiki:` mirror; deduped
    within a mirror. GC'd when no PageView, WACZ, or media
    reference cites them (`blobs-impl-WORK`).
19. **WACZ archive.** Web-archive bundle of a `web:` mirror, on
    demand (`gimir export-wacz`); user-initiated.

### Wikipedia / MediaWiki instance data

20. **Instance.** MediaWiki site keyed by dbname (the stable key;
    domains change, dbnames don't — import-plan §2.1). Carries
    dbname, domain, API URL, project, language, flags, added_at,
    watermarks. O(1)–O(10) per user.
21. **Sitematrix entry.** Authoritative dbname↔domain map from
    meta.wikimedia.org (import-plan §1.3). ~900 entries; refreshed
    periodically.
22. **Siteinfo snapshot.** Full `action=query&meta=siteinfo` JSON:
    namespaces, aliases, magic words, interwiki map, extension
    tags, parser functions, magic variables, language variants
    (~10⁴ bytes). Sequence of snapshots per instance, deduped
    when unchanged. The historical sequence is itself useful —
    siteinfo at τ governs rendering at τ (browsing-plan §2).
23. **Page.** Keyed by (instance, page_id). Carries title history,
    namespace, redirect status, a chain of revisions. page_id
    semantics are non-trivial: it can disappear and reappear, a
    title can switch page_ids, merges/moves emit events rather
    than rewriting history (import-plan §1.4). ~10⁹ across major
    wikimedia instances; ~6×10⁷ enwiki. Event-sourced.
24. **Page revision.** Keyed by (instance, page_id, rev_id):
    rev_id, parent_id, timestamp, contributor (username+id | IP),
    minor flag, comment, origin, model, format, text, sha1
    (base-36), byte size, plus per-field "deleted" flags (text,
    comment, contributor independently). Text variable-size,
    cap 2 MB (import-plan §2.5). ~1.27×10⁹ for enwiki history.
    Immutable once stored; corrections (RevisionDelete flips,
    merges, deletes) recorded as events.
25. **Page text / comment / contributor / revmeta lanes.** Per-
    page columnar projections of revision data (import-plan §2.2).
    Per-column ordered sequence of records, newest-first.
    Cardinality follows revision count; heavily skewed (most pages
    <10 revisions; a few 10⁵+). Lane sets stable per instance.
26. **Page events lane.** Sparse per-page log of control
    operations — move, delete, restore, merge in/out, visibility
    flip, tombstone, source attribution. O(1)–O(10²) per page;
    append-only (import-plan §2.2, §1.4).
27. **Page-level pretrained dictionary.** One per (instance, lane),
    trained on a representative sample of newest revisions; zstd
    dictionary bytes (~10⁵ bytes per dict). O(10⁰)–O(10¹) per
    instance per lane. Trained once, refreshed on deliberate
    repack (import-plan §2.5 step 5).
28. **Titles index.** "What page_id did title T in namespace N
    point at, at τ?" plus the reverse (import-plan §2.4). Interval
    list `[(start_ts, end_ts|open, page_id)]` keyed by normalized
    `(ns, dbkey)`, newest first; reverse keyed by page_id.
    O(10⁷)–O(10⁸) keys per major wiki. Fully rebuilt from each
    monthly mediawiki_history TSV; incrementally advanced
    between snapshots.
29. **Category transition record.** Per (page_id, category)
    add/remove with timestamp, captured in the depot-import scan
    (import-plan §2.5 step 3b; browsing-plan §2). O(10⁸) for
    major wikis; append-only.
30. **File metadata record.** Per (instance, file): sha1 (base-36),
    size, dimensions, mime, upload timestamp; current version from
    `image.sql.gz`; historical versions cached from
    `prop=imageinfo` on demand (oldimage is not dumped). O(10⁵)–
    O(10⁸) per instance (browsing-plan §4, §7).
31. **File binary / thumb.** Lazily fetched bytes for (file,
    version, width). Stored in the per-mirror blob store keyed by
    that triple. Cardinality is browsing-governed (browsing-plan
    §4). Permanent after fetch.
32. **Wikipedia instance watermarks.** Per-instance: last
    full-dump date, last-incr date, last-history-snapshot date,
    current maxrevid. Small; one per instance; advanced per pull.
33. **Render cache entry.** Optional render-on-demand cache keyed
    by (instance, page_id, resolved rev_id, τ-day, renderer
    version) → rendered HTML (browsing-plan §5). Bounded LRU,
    flushed on renderer version bump.

### Storage primitive: chain + mux artifacts

34. **Chain record.** A single byte-string entry in an append-only
    newest-first chain. Opaque bytes; cardinality equals the
    number of artifact versions (README §"Chain").
35. **Chain frame.** A zstd frame holding one or more records,
    compressed either with a pretrained dictionary (frame 0 only)
    or in refPrefix mode (frames 1+). Frame 0 is the solitary
    head (import-plan §2.2); frame 1 is the accumulator;
    frames 2+ are immutable sealed.
36. **Chain pretrained dictionary.** Identified by dict_id riding
    in the zstd frame header; bytes resolved through a caller-
    supplied lookup. O(10⁰)–O(10²) per backing.
37. **Mux artifact.** Directory of named chains (lanes) plus a
    manifest plus per-lane sidecars recording each record's global
    revision index (README §"Mux"). One per Wikipedia page; one
    per `web:` URL; one per IETF document revision sequence.

### Runtime / observability

38. **Cooldown state.** Per (mirror, source kind):
    `next_eligible_at`, ladder rung, last attempt result.
    Generalized to every source (`14ce3e6`). O(10²)–O(10³);
    advanced per attempt; bypassed on explicit user action.
39. **Fetch-state row.** Per (mirror, source kind, item key):
    watermark, last attempt time, last error. The unit of per-item
    resumability. O(10⁴)–O(10⁶) (`ab28868`, `b6b4f1c`).
40. **Submodule state.** Per parent commit per submodule:
    resolved upstream URL, conversion outcome. O(10³)–O(10⁴)
    (historian §5 R6).
41. **In-flight pull record.** Per concurrent pull: mirror, start
    time, phase, cancellability handle. O(10⁰)–O(10¹)
    simultaneous (`57cb51a`).
42. **Per-host concurrency budget.** Per host: current in-flight
    count, configured max. `internal/hostlimit` shared across git
    fetches, provider invocations, conditional GETs, and
    upload.wikimedia.org pulls under the Robot policy
    (browsing-plan §4). O(10) hosts.
43. **Run log.** Per invocation: forensic capture of every
    subprocess, every HTTP request and response (headers full,
    bodies redacted), every error, under
    `~/.cache/gimir/runs/<ISO8601>-<pid>.log` (`16a00ef`).
    Append-only during a run.
44. **Activity / progress record.** Per running operation: phase
    counters, status string, last update time. Drives the admin
    UI's progress display (`c32c0cb`). Discarded when the
    operation completes.
45. **Mirror stats.** Per Mirror summary: object count, on-disk
    bytes, ref count, last-pulled, extradata counts, error counts.
    Recomputed on demand for the no-args display and admin index.
46. **Schema version.** Small per-store value pinning the state
    schema. Bumped on migration; consulted on open (historian §5
    R6).

### User-facing browse-and-search state

47. **Grep query and corpus selection.** Query string + glob over
    mirrors/extradata. Per-invocation, ephemeral.
48. **Search index inputs.** The corpus grep iterates over: git
    tree contents at mirror tips + extradata bodies + wikitext
    bodies + IETF documents + web page captures. 10⁷–10¹⁰ bytes
    typical; up to 10¹²–10¹³ at enwiki-history scale.
49. **Web UI URL state.** URL-encoded state for list expansion,
    asof timestamp, page navigation (historian §4 rule 3;
    browsing-plan §5). Per-request.
50. **Undo token.** Per recently-deleted entity: prior location +
    sha to restore, short window so a click can revert (historian
    §4 rule 2, `970b3c3`). Bounded recent set per session.

The list runs to 50 items grouped into ten clusters; the
clustering is what step 5 acts on.

---

## Step 2: Workflows and origins

For each command the README names, and each non-command workflow the
historian's note identifies, what gimir does end-to-end. Latency
labels: **interactive** = sub-second, **responsive** = a few seconds,
**batch** = minutes-to-hours, **background** = overnight.

### `gimir` (no arguments) — usage + mirror stats

Print usage, then a table of mirrors with size, age, last-pull
outcome. Responsive. Read repos.txt, resolve to Mirrors, recompute
mirror-stats (45) cheaply from on-disk size + latest fetch-state
+ cooldown rows, print. Read-only. Touches 1, 4, 38, 39, 45.

### `gimir clone <suffix>`

Produce a normal git working directory whose `objects/info/
alternates` points at the depot — no objects copied. Responsive.
Parse repos.txt, resolve the suffix against mirror URLs (point
lookup), locate the depot, `git clone --no-checkout` with
alternates, check out. Read-only against the depot. Touches 1,
4, 7, 8, 9.

### `gimir pull`

Bring every mirror current with bounded effort; per-host
etiquette respected; auth flows through the user's own tooling
(README lines 26-29). Batch. Sequence: read repos.txt and
stores.txt, resolve Mirrors with Store bindings; for each Mirror,
check cooldown (38) and skip unless eligible (or user override);
dispatch by source kind:
- **git**: fetch refs + objects into the depot; recurse
  submodules (10); refresh extradata (11) via the provider tool.
- **bridged forge**: incremental fast-import advancing bridge
  state (14); refresh fossil sidecar extras when applicable.
- **ietf**: conditional-GET index + changed documents.
- **web**: walk URLs (static or chromedp); bodies/assets to
  Blobs (18); revisions to PageView (17).
- **wiki**: see seed/incremental sub-workflows below.

Throughout: append run-log lines (43); update activity (44);
advance fetch-state (39) and cooldown (38) on completion.
Upstream feeds are origin; write-mostly across the cluster set.

### `gimir gc`

Drop mirrors absent from repos.txt; sweep depot objects
unreachable from any surviving ref. Batch. Read repos.txt; diff
against on-disk Mirror set; move removed Mirrors to the trash
area with their refs and state (recoverable via undo, 50); walk
depot objects, sweep unreachable, compact. Touches 1, 4, 7, 8,
10, 38, 39, 45, 50.

### `gimir grep`

Phrase search across every preserved byte (git tree contents,
extradata bodies, IETF documents, web captures, wiki revisions),
returning locations with enough context to navigate. Responsive
on small corpora, batch on large. The corpus (48) is the union of
each Mirror's bytes: git tip tree, latest extradata blob, IETF
bodies, latest PageView body, newest wiki revision in included
namespaces. Read-only. Touches 7, 11, 15, 17, 18, 24, 47, 48.

### `gimir serve`

Foreground process binding localhost-only (historian §2),
serving admin UI under `/_admin/` and browsing UI for every
mirror kind; plain HTML (historian §4 rule 3); full-page
POST/redirect mutations; URL-encoded expansion state; the wiki
date picker selects τ. Sub-workflows: mirror index, per-mirror
list/add/delete of primitives (refs, tickets, URLs, pages — the
admin CRUD surface), browse git ref content, browse extradata
item, browse web revision, browse wiki page at τ (see below),
search, activity panel streaming in-flight pulls with cancel
(57cb51a), cooldown inspector with click-to-bypass (342a61d),
one-click undo on recent deletes. Read-mostly; mutations write
fetch-state, undo tokens, trash. Touches every cluster.

### Provider extradata refresh (sub-workflow of pull)

For a Mirror's forge host, invoke `gh`/`glab`/`tea`/`hut` with
the host pinned; walk entity list (issues, PRs, discussions,
wiki, releases) DESC by upstream id, stopping when the depot
already holds an unchanged version (78de946). Write-mostly into
extradata refs.

### Webmirror conditional-GET refresh

For ietf and web kinds: read the conditional-GET row (16) for
the URL; GET with `If-None-Match` / `If-Modified-Since`; on 304
advance the watermark, on 200 write body to Blobs (18) and
append the PageView revision (17). Conditional-GET is the
witness for "we have this version already" (cbb2803, 8a3b2a2).

### WACZ export

`gimir export-wacz <mirror>` walks every PageView and referenced
Blob, packages into WACZ 1.1.1, writes to chosen path. Read-only
over the mirror.

### Wikipedia seed import (first `gimir pull` for a fresh `wiki:`)

End-to-end without spooling intermediate files to disk;
resumable per-part on crash; idempotent on rerun. Batch, hours
to days at larger scale. Import-plan §2.5. Sequence: resolve
dbname via sitematrix (21), open the Instance record (20);
discover newest content-file-export run, enumerate part files
with checksums; for each part-file group (ordered by page
coverage per the split-pages note, import-plan §2.5): stream
HTTP GET → block-parallel bz2 decode → streaming XML parser →
per-page revision batches, nothing spooled to disk; resumability
unit is the per-part watermark plus per-page max-rev-timestamp.
For each page batch: append columnar lanes (25, 26) via the
Store's columnar capability — the seed helper writes
`[solitary head][one sealed frame of everything else]`. The
same scan feeds the per-instance dict trainer (27) and extracts
category transitions (29); once the trainer converges, repack
the head of every text chain with the dict. Per-revision sha1
verified (24's sha1) against the dump; mismatches stored AS
DUMPED with a flag (import-plan §2.5 step 4). Watermarks (32)
advance per file. dumps.wikimedia.org is the byte origin;
write-mostly across 20-32; lanes are write-once-read-many.

### Wikipedia incremental update

Daily/weekly pull. Refresh siteinfo; ingest each unconsumed
adds-changes day; on a new monthly mediawiki_history TSV,
rebuild titles (28) from scratch (the discipline; import-plan
§2.4) and reconcile visibility/delete/restore against the depot,
appending events (26). Each successful prepend cycles the page
chain through accumulator and seal. Touches 23-32, 38, 39.

### Wikipedia browse-at-τ (`gimir serve` route for a wiki page)

Calendar control selects τ; page renders as at τ, with internal
links carrying asof. Responsive on cache hit; batch on a cold
template-heavy render. Sequence: normalize title; point-lookup
`(ns, dbkey)` → page_id at τ in titles (28); scan revmeta lane
(25) newest-first for `timestamp ≤ τ`; decode text lane for that
rev_id (frame 0 alone for current; chain walk for older); pass
to renderer, which resolves every transclusion through the same
titles + revision-at-τ lookups recursively, resolves
{{#invoke:}} into Module: pages similarly, resolves File:X via
file-metadata KV (30) and Blob store (31) with lazy media fetch;
cache result under the render-cache key (33). siteinfo at τ
(22) and file-metadata at τ (30) govern rendering. Read-mostly
plus the render-cache write. Touches 20, 22, 23-31, 33.

### Wikipedia media lazy-fetch (sub-workflow of browse)

First render needing File:X at τ: look up file-metadata (30) at
τ; if absent, fetch `prop=imageinfo` to populate; compute the
upload.wikimedia.org URL for (version, render-bucket width);
fetch under the codified Robot policy (≤2 conns, ≤25 Mbps,
standard bucket widths only — browsing-plan §4); store in Blobs
keyed by (file, version, width); serve forever after. Touches
6, 18, 30, 31, 42.

### Cooldown evaluation

Every fetch attempt for (mirror, source kind) consults the
cooldown row (38); on success/failure advances the ladder; user
override (admin click, `--verbose`) bypasses (342a61d, ae9bd4a).

### Run-log capture

Always on, structural. Every subprocess and every HTTP request
and response (via an installed `http.DefaultTransport` round-
tripper) flows into the per-run log with body redaction
(16a00ef).

### Admin CRUD

For each primitive (Source, Mirror, Ref, Log, Depot,
FetchStateRow): list, add (where meaningful), single-click
delete (970b3c3), single-click bulk delete (5f433c8), one-click
undo. Full-page POST/redirect.

### Initial setup

Install the binary; edit `~/.config/gimir/repos.txt`; optionally
make `~/.cache/gimir/` a symlink onto a larger filesystem; first
`gimir pull` materializes the cache layout and starts fetching.

### Adding a new source kind

A new kind hint appears in repos.txt; the source-kind dispatch
(wired via `mirror.SetSourceKindLookup` from `cli`) resolves it
to a provider whose `Kind()` matches; the engine calls `Fetch`
and the provider owns the pull.

---

## Step 3: Magnitude and distribution

For each item, order of magnitude (bytes and count), distribution
shape where nameable, working set vs long tail, growth profile.
Numbers from source documents are cited; gaps are named.

### The small set (KB to MB, O(10⁰)–O(10⁴) items)

Items 1, 2, 4, 5, 6, 14, 16, 20, 21, 32, 46, 41, 42, 49, 50:
each record bytes-to-KB; aggregate per user in the megabytes;
small uniform sets, bounded by user actions. Cooldown rows (38)
O(10²)–O(10³). Fetch-state rows (39) O(10⁴)–O(10⁶), aggregate
~10⁷–10⁸ bytes, bounded linear in mirrors and their depths.
Submodule state (40) 10³–10⁴ rows. Provider configurations
(12) O(1) — five tools.

### Git mirror data

Git objects (7): per-mirror 10³–10⁶; depot-wide post-dedup
expected 10⁷–10⁸ at typical scale (precise figure unknown
without measurement). Median ~kilobytes, heavy-tailed in object
size, long-tailed in object age (most accesses to recent
objects). Aggregate O(10¹⁰)–O(10¹¹) for ~100 mirrors. Growth
sub-linear after dedup. Refs (8) 10²–10⁴ per mirror.
Working-directory clones (9) O(10) per user, outside the depot.

### Forge extradata

Extradata items (11): zero for most mirrors, 10⁴–10⁵ for active
forge projects; bodies from bytes (one-line comments) to ~MB
(long threads). Aggregate O(10⁹)–O(10¹⁰) at typical deployment.
Zipfian over projects and within a project over thread length.

### Fossil-bridged data

Fossil sidecar clones (13): O(10⁰)–O(10¹), each up to GBs.

### IETF corpus

IETF documents (15): ~10⁴ RFCs + ~10⁴ drafts; median tens of KB,
long tail of hundreds of KB; aggregate ~10⁹–10¹⁰. Linear growth
(~10² new RFCs/year); access skewed toward recent and well-known.

### Arbitrary website data

PageView (17), Blobs (18): per-mirror cardinality varies by
orders of magnitude with the site shape. Aggregate is user-
determined; the shape is a long tail of small captures and a
short tail of blob-heavy ones.

### Wikipedia data (the dominant cluster)

Pages (23): ~10⁹ across the union of major instances; ~6×10⁷
enwiki; ~10³ tiny wikis like votewiki (1328 pages, historian §5).
Page revisions (24): enwiki ~1.27×10⁹ (import-plan §2.7);
votewiki 3678 over 1328 pages; per-revision bytes median few KB,
cap 2 MB, heavy tail; inter-revision redundancy ~99% across
adjacent revisions (import-plan §2.7); Zipfian over pages
(a few have very deep history; most are shallow). Text/comment/
contributor/revmeta lanes (25): cardinality follows item 24;
aggregate uncompressed enwiki ~55–60 TB (import-plan §1.5);
compressed in the chain shape with pretrained dict + refPrefix
~0.6–0.7 TB total — head set ~400–500 GB, tail comparatively
small. Events lane (26) sparse, O(1)–O(10²) per page.
Pretrained dictionaries (27) O(10⁰)–O(10¹) per instance per
lane, ~10⁵ bytes each. Titles index (28) 10⁷–10⁸ keys per major
wiki; values interval lists, median few, long-tail pages move
many times; aggregate 10⁹–10¹⁰. Category transitions (29)
O(10⁸) for major wikis, ~100 bytes each. File metadata (30)
~10⁸ for commonswiki (image.sql.gz ~18 GB); 10⁵–10⁷ for large
project wikis. File binaries/thumbs (31) browsing-governed;
thumbs ~10⁴ bytes median, originals tens of MB. Watermarks (32)
one row per instance. Render cache (33) bounded LRU.

### Storage primitive layer

Chain records (34), frames (35), dicts (36), mux artifacts (37):
cardinality follows the artifacts they store — one mux per
Wikipedia page, per `web:` URL, per IETF doc revision sequence.

### Observability

In-flight pulls (41) O(10⁰)–O(10¹) simultaneous. Run logs (43)
one per invocation, per-run 10⁴–10⁹ bytes; aggregate retention-
governed and currently a knob. Activity records (44) O(in-flight).
Mirror stats (45) one per mirror, few hundred bytes, recomputed.

### Total disk budget for a typical deployment

A typical single-user deployment: ~100 git mirrors with their
extradata, ~10 forge providers' data, one mid-sized Wikipedia
instance the size of cswiki (a few hundred thousand pages),
the IETF corpus.

- Git depot: ~10¹⁰–10¹¹ bytes post-dedup, driven by the
  particular projects.
- Extradata: ~10⁹–10¹⁰ bytes at the high end of active forge
  projects.
- IETF corpus: ~10⁹–10¹⁰ bytes.
- Web mirrors: user-determined, ~10⁹–10¹⁰ bytes typical.
- cswiki-scale Wikipedia instance: ~10¹⁰ bytes (head set
  dominates, import-plan §1.5); votewiki's 6.2 MB bz2 → 67 MB fs
  measurement confirms that at tiny scale fs fan-out overhead
  dominates (historian §5).
- State, cooldown, run logs, activity, undo: under 1 GB aggregate.

Order-of-magnitude total: ~10¹¹ bytes (~100 GB) for a typical
deployment with one mid-sized wiki. At enwiki scale alone, the
wiki backing is ~5×10¹¹ bytes (~500 GB head, small tail —
import-plan §1.5), an order of magnitude larger than all other
data combined.

Genuine unknowns: depot-wide git object cardinality after dedup
under 100+ mirrors with submodule overlap; render cache hit
rate; run-log aggregate under a retention policy not yet set.
These flow into step 5 as "shape known, sizing pending
measurement."

---

## Step 4: Derive operations

The operations, weighted by frequency observed across step 2's
workflows. Each row names: the operation, the items it touches,
the workflows in which it appears, a frequency weight on a
qualitative scale (★★★★★ = on every command, ★ = rare batch).

| Operation | Items | Workflows | Weight |
|---|---|---|---|
| Read repos.txt, parse, resolve to Mirrors | 1, 4 | every command | ★★★★★ |
| Point lookup Mirror by unambiguous URL suffix | 4 | clone | ★★★ |
| Read Mirror stats | 45 | no-args, serve index | ★★★★ |
| Read cooldown row for (mirror, source kind) | 38 | pull, every fetch attempt | ★★★★★ |
| Advance cooldown row | 38 | pull | ★★★★ |
| Read fetch-state for (mirror, source kind, item) | 39 | pull, extradata refresh | ★★★★★ |
| Advance fetch-state watermark | 39 | pull | ★★★★ |
| Append to run log | 43 | every command | ★★★★★ |
| Update activity record | 44 | pull, serve | ★★★★ |
| Read activity records (UI) | 44 | serve | ★★★ |
| Append git objects, content-addressed | 7 | pull (git, bridges, wiki via export) | ★★★★ |
| Point lookup git object by sha | 7 | clone, grep, serve git browse | ★★★★ |
| Update git ref (force-updateable) | 8 | pull, extradata write | ★★★★ |
| List git refs of a mirror | 8 | serve mirror page | ★★★ |
| Walk git tree at a ref | 7, 8 | clone, grep, serve | ★★★ |
| Append extradata blob to a ref | 7, 11 | pull (extradata refresh) | ★★★ |
| Read extradata item body | 11 | serve, grep | ★★★ |
| Resolve provider for host | 6, 12 | pull (extradata) | ★★★ |
| Conditional GET against cached etag | 16 | pull (ietf, web) | ★★★ |
| Append Blob, content-addressed | 18 | pull (web, wiki media), WACZ | ★★★ |
| Point lookup Blob by hash | 18 | serve web/wiki, grep, WACZ | ★★★★ |
| Append PageView revision for URL | 17 | pull (web) | ★★ |
| Walk PageView revisions for URL | 17 | serve web, WACZ | ★★ |
| Linear scan / index query over corpus | 7, 11, 15, 17, 24, 48 | grep, serve search | ★★★ |
| Resolve dbname via sitematrix | 21 | pull (wiki seed) | ★ |
| Read instance record | 20 | pull (wiki), serve wiki | ★★★ |
| Append siteinfo snapshot when changed | 22 | pull (wiki) | ★★ |
| Point lookup siteinfo at τ | 22 | serve wiki render | ★★★★ |
| Append page revisions, batched, newest-first | 24, 25, 26 | pull (wiki seed, incremental) | ★★★★ |
| Read newest revision of a page (head) | 25 (text + revmeta) | serve wiki render at τ=now | ★★★★★ |
| Read page revision at τ (revmeta scan ≤ τ, then text decode) | 25 | serve wiki render at τ | ★★★★ |
| Walk page revision history newest-to-oldest | 25 | serve wiki per-page history | ★★ |
| Append page event | 26 | pull (wiki incremental, monthly reconcile) | ★★ |
| Read pretrained dict by id | 27, 36 | every chain decode | ★★★★★ |
| Train pretrained dict from sample | 27 | pull (wiki seed) once per instance per lane | ★ |
| Repack head of every text chain (apply dict) | 25, 27 | post-train step in pull (wiki seed) | ★ |
| Point lookup `(ns, dbkey)` → page_id at τ | 28 | serve wiki render (every link resolution) | ★★★★★ |
| Reverse lookup page_id → (ns, dbkey) at τ | 28 | serve wiki render | ★★★ |
| Rebuild titles index from TSV snapshot | 28 | pull (wiki monthly) | ★ |
| Append category transitions | 29 | pull (wiki seed, incremental) | ★★ |
| Range scan (page, category, ≤ τ) → categories at τ | 29 | serve wiki render | ★★ |
| Point lookup file metadata at τ | 30 | serve wiki render | ★★★ |
| Append file metadata version | 30 | pull (wiki seed), lazy on imageinfo | ★★ |
| Point lookup blob by (file, version, width) | 31 | serve wiki render | ★★★★ |
| Fetch and append blob for (file, version, width) under Robot policy | 31, 42 | first-render demand | ★★ |
| Read render-cache entry by key | 33 | serve wiki render | ★★★★ |
| Append render-cache entry | 33 | serve wiki render | ★★★★ |
| Acquire/release per-host concurrency slot | 42 | every outbound fetch | ★★★★★ |
| Append run-log line | 43 | every external operation | ★★★★★ |
| Move entity to trash (single-click delete) | 50 | serve admin | ★★ |
| Restore from trash (undo) | 50 | serve admin | ★★ |
| Walk depot objects for GC, mark live by ref reachability | 7, 8 | gc | ★ |
| Compute and display mirror stats | 45 | no-args, serve | ★★★★ |

Cross-workflow observations:

- **Point lookup by content hash** appears across git objects,
  Blobs, file binaries, pretrained dicts. The shape is the
  same in each case: hash → bytes.
- **Head-read of the newest record** is the dominant access on
  the chain primitive — `gimir clone` for git heads, render of
  the current wiki revision, current PageView, latest fetched
  IETF document. The chain primitive's frame-0-is-solitary
  invariant exists precisely because this is the dominant op.
- **Point lookup of an interval-keyed record at τ** is the
  defining operation for titles, siteinfo, and file metadata
  in the wiki cluster.
- **Append, newest-first, with bounded prepend cost** is the
  defining write operation for chains.
- **Range scan over a small ordered key set** appears for refs,
  for the per-mirror history view, and for the per-page
  history view.
- **Conditional read (etag/last-modified)** is the witness
  shape for ietf and web kinds.
- **Mark-and-sweep liveness** appears at the depot (refs are
  the marks, git objects are swept) and at the chain index
  (index entries are the marks, shard frames are swept).

---

## Step 5: Match operations + magnitude to structure

Each cluster gets a named shape. The shape names a regime, not an
implementation; what library or file format renders it is
downstream.

### Cluster A — configuration and registry

Items 1, 2, 5, 12, 21, 46. Tiny set; user-edited or compiled-in;
read on every command, written rarely. **Shape: read-mostly
in-memory map sourced from on-disk text files of record.** The
text file IS the source of truth; the parse is its in-memory
projection. The right shape because volume is tiny, consumers
want point lookup by name, and the user authors the source of
truth in a text editor.

### Cluster B — content-addressed object pool (the git depot)

Items 7, 8, 10. 10⁷–10⁸ unique objects after dedup; 10¹⁰–10¹¹
bytes. Dominant op: point lookup by sha. Writes append, never
modify in place. Liveness signal: union of all mirrors' refs.

**Shape: content-addressed object pool, append-only at the
object level, with mark-and-sweep GC driven by ref reachability
across all mirrors.** Pre-exists as git's own object model;
gimir leans on it via shared `alternates`. Forks and submodule
overlap deduplicate by construction. The append-only invariant
makes crash recovery trivial; "have we already stored this?"
costs the same as a lookup, so reruns are free; N forks cost
the bytes of one plus their deltas.

### Cluster C — durable indexed state

Items 38, 39, 40, 20, 32, 46, 41, 42, 50, plus the persistent
parts of 45 and 16. Rows up to 10⁶, each a few KB at most.
Operations: point lookup by composite key, small range scans
(e.g. all fetch-state rows for a mirror), transactional updates
(advancing fetch-state and cooldown together).

**Shape: indexed transactional key-value store with durable
single-writer / multi-reader semantics, schema-evolved on open.**
The regime where a transactional KV dominates: many small
records, keyed lookups, occasional small range scans, multi-row
atomic updates, crash durability. The historian's note records
this cluster's migration from JSON sidecars into state.db
(`1e07f0e`) and then onto the Store interface's KV — the shape
asserting itself against earlier sidecar approximations.

### Cluster D — per-key chain of versioned binary payloads (chain + mux)

Items 25 (text/comment/contributor/revmeta lanes), 26 (events
lane), 17 (PageView), 22 (siteinfo snapshots), 30 (file metadata
versions), 11 to the extent that extradata refs evolve, and the
underlying record/frame structure 34-37. The carrier of the
largest data in gimir. Per-key record counts heavy-tailed;
per-record bytes from tens (file metadata) to ~2 MB (wikitext
cap). Inter-record redundancy is high — the source of the
cluster's compression win. Operations: head-read (newest record)
dominant; append newest-first is the write op; walk newest-to-
oldest is history; record-at-τ goes through the chain.

**Shape: per-key append-only newest-first chain of zstd-frame
records, with a solitary head frame, a re-encoded accumulator
frame, and immutable sealed tail frames; pretrained per-(instance,
lane) dictionary on the head; refPrefix on accumulator and sealed
frames; columnar projection (mux) so a head-read touches only
requested columns.** Solitary frame 0 makes head reads one small
decode (compression carried by the pretrained dict, not by
neighbors). Accumulator amortizes per-frame entropy-table cost.
Sealed frames are immutable and survive every prepend byte-
identical. Columnar lanes mean rendering current text never
decompresses comments or contributors. The import-plan §2.7
expected ledger (0.6–0.7 TB for enwiki vs 15–60 TB for per-row
gzip) is the workload defense.

### Cluster E — large-cardinality time-keyed interval index (titles)

Item 28. 10⁷–10⁸ keys per major wiki. Operations: point lookup
by `(ns, dbkey)` at τ — one of the most frequent ops in the
system; reverse point lookup by page_id at τ. **Derived data**,
rebuilt from each monthly mediawiki_history TSV in full and
advanced incrementally between snapshots.

**Shape: read-mostly key-value index with structured values
(newest-first interval list) carrying the temporal axis inside
the value, fully rebuildable from upstream truth.** Large enough
to demand efficient point lookup but small enough that periodic
full rebuild is the discipline for correctness (import-plan §2.4).
Rebuild from upstream truth is load-bearing because the upstream
snapshots themselves are full rebuilds; partial maintenance
accumulates known approximation (delete/restore ~99.9% accurate,
import-plan §1.3), and rebuild caps that approximation.

### Cluster F — append-only event log per page (events lane)

Item 26. Small per page (10⁰–10²), large aggregate (10⁸ on
enwiki). Append on incremental update; read newest-first when a
render checks visibility at τ. **Shape: per-page append-only
event log embedded in the chain primitive as its own lane.**
Same access pattern as the chain; semantically distinct records;
interleaves with revmeta reads at render time.

### Cluster G — content-addressed byte object store (Blobs)

Items 18, 27, 31. 10⁴–10⁸ blobs per active mirror; sizes median
small (thumb), tail large (original). Point lookup by hash;
append on first sight; mark-and-sweep liveness from PageView,
WACZ, file-metadata, and dict-id references.

**Shape: content-addressed byte object pool, append-only, with
mark-and-sweep liveness from the consumers.** Same shape as
cluster B but with different consumers. The unification is
load-bearing: "store any opaque bytes once" is a single primitive
across the system.

### Cluster H — conditional-GET witness state

Item 16. Per-URL (etag, last-modified, body-hash); 10⁴–10⁵ per
active webmirror; point lookup by URL, update per fetch. **Shape:
indexed key-value store, same regime as cluster C, same backing.**
Flagged for inventory completeness.

### Cluster I — bytes-on-disk corpus for search

Item 48. 10⁹–10¹³ bytes; heavy-tailed in document size; phrase
search returning located matches.

**Shape: the corpus IS the union of the underlying clusters'
content (B, D, G bodies); search reads through them.** Whether
an auxiliary inverted index sits beside the corpus is a sizing
question keyed to the per-deployment corpus size. When the index
exists, it is the same indexed-KV regime as cluster C, keyed by
token.

### Cluster J — observability stream (run logs, activity)

Items 43, 44. Per-run 10⁴–10⁹ bytes; aggregate retention-bounded.
Append-only during a run; admin UI reads whole files; in-flight
display reads selected rows.

**Shape: append-only log file per run; in-memory ring of activity
records for currently-running operations.** The run is the unit,
the log is its record, the file is the natural unit of retention.
Activity is ephemeral; the admin UI streams it at request time.

### Cluster K — render cache

Item 33. **Shape: bounded LRU keyed by content-deterministic key
(browsing-plan §5), flushed on renderer version bump; backed by
the Blob store for body bytes and the KV store for metadata.**
LRU policy is the only piece beyond clusters G and C.

### Cluster L — user-facing ephemeral state

Items 47, 49, plus 50's ephemeral side. Per-request data in HTTP
cycles or short-TTL rows. **Shape: in-memory per-request data plus
short-TTL rows in the indexed KV.** No new shape.

---

## Step 6: The shape IS the contract

What the chosen shapes commit gimir to, system-wide.

### From cluster A (configuration in text files)

Cheap: editing repos.txt is the only way to add or remove a
Mirror, and the file IS the answer to "what is gimir mirroring?"
Changes are visible on the next command, no restart. Configuration
is diffable and version-controllable. Round-tripping configuration
back through the system is inexpressible — gimir does not rewrite
repos.txt. Failure mode: a malformed line is a parse error on the
next command. Concurrency: the file is re-read per command.

### From cluster B (content-addressed object pool)

Cheap: dedup of forks and submodules; clone via shared
alternates (no object copying); reruns of fetch (have-we-already
== lookup). Inexpressible: editing an object in place (name IS
hash); moving the depot without moving the working directories'
alternates (README lines 59-60). Failure mode: object missing →
re-fetch from upstream; if upstream is gone, the object is gone
(gimir is a hedge against upstream disappearance, not a
guarantee). Durability: git's fsync plus the filesystem's; no
separate journal. Concurrency: racing writes write the same
bytes to the same name; reads are unconstrained; GC is a
single-writer step.

### From cluster C (durable indexed KV)

Cheap: point lookup by key; small range scans by key prefix;
multi-row atomic updates; schema migration on open. Inexpressible:
full-table scans over millions of rows at runtime; secondary
indexes on value fields not anticipated by the key design (a new
query pattern wants a key-derivation step at write time). Failure
mode: transactions commit or abort; the most recent committed
state survives crash. Durability: the SQLite WAL contract;
nothing layered on top. Concurrency: single-writer / multi-reader
per database; the Store binding distributes writers across
databases.

### From cluster D (per-key chain of versioned binary payloads)

Cheap: head-read in one zstd decode against the pretrained dict;
prepend with bounded re-encode cost (frame 0 + accumulator only);
walk newest-to-oldest with frame-by-frame refPrefix decode; GC of
an entire chain (tombstone, sealed frames are small to drop);
adding or removing a lane independently of siblings.
Inexpressible: random-access read of the K-th oldest record (the
chain is genuinely a chain, not an array); in-place edit of an
already-stored record (corrections land as events); cross-page
joins on the chain (cross-key operations go through the auxiliary
indexes). Failure mode: a frame fails to decode → that revision
plus its prefix-dependents are unreadable; the dictionary id must
resolve. Chain integrity is a function of frame-by-frame zstd
correctness — no CRCs or magic numbers layered on top of zstd's
own frame format. Durability: append → fsync → flip a pointer
atomically; no WAL of our own. If a crash happens between fsync
and pointer flip, the durably-written frame is unreachable on
restart. Concurrency: single-writer per chain; many chains in
parallel under the per-host budget; readers walk from the pointer
they hold and the writer appends past it. The pretrained
dictionary discipline carries an additional contract: the system
pre-trains once per (instance, lane), commits to it for the
foreseeable head set, and accepts that re-training is a head
repack.

### From cluster E (time-keyed interval index)

Cheap: point lookup of `(ns, dbkey)` at τ; reverse lookup of
page_id at τ; full rebuild from a fresh TSV. Modestly expensive:
"all titles a page_id has held" goes through the value's interval
list — a value-decode, not a scan. Inexpressible: cross-instance
title lookups (instances are separate indices). Failure mode:
malformed interval inside a value is a per-key inaccuracy;
periodic rebuild from upstream truth bounds it. Durability:
cluster C's. Concurrency: rebuild is a single-writer batch
producing a new index; the swap is atomic at the KV level.

### From cluster F (per-page event log)

Cheap: append on incremental update; newest-first walk at render
time. Inexpressible: cross-page event scans ("all RevisionDelete
events in 2023") — those reduce to the auxiliary indexes or
explicit batch jobs over the depot. Failure mode and durability:
cluster D's.

### From cluster G (content-addressed byte object store)

Same contract as B with PageView, WACZ, file-metadata, and dict-id
references playing the role git refs play in B. Cheap: dedup
across re-fetches and identical bytes (same thumb at the same
render bucket, etc.). Inexpressible: editing in place; listing in
any order other than the storage's enumeration order. Failure
mode: blob missing → re-fetch from upstream where the upstream is
authoritative (web, upload.wikimedia.org); blob loss is genuine
loss where the user IS the upstream (export bundles, trained
dicts). Durability: the user's filesystem.

### From cluster H (conditional-GET witness)

Subsumed into cluster C.

### From cluster I (search corpus)

Cheap: phrase search whose answer is a small number of matches;
scoped search ("this mirror only," "extradata only"). Expensive
at enwiki scale without an auxiliary index — at that scale a
shape decision is forced; the auxiliary index, when needed, is
cluster C's regime keyed by token. Failure mode: stale auxiliary
index → stale matches; the index is derived and rebuildable.

### From cluster J (run logs)

Cheap: append during a run; random access of a past run by
ISO8601 + pid (file name IS the key); deletion by file unlink.
Inexpressible: cross-run queries — the unit of retention is the
file; cross-run search reduces to grep over the files (the
intended ergonomic). Durability: the filesystem's fsync on the
run log file.

### From cluster K (render cache)

Inherits cluster G's (for body bytes) and cluster C's (for
metadata) contracts. Adds: cache invalidation is by renderer
version (a single bump invalidates all entries) and by τ-day
granularity (the cache accepts τ-day staleness rather than
chasing perfect invalidation across template-closure changes —
browsing-plan §5).

### From cluster L (ephemeral UI state)

Per-request data lives only for the request. URL-encoded state
is sharable as a URL. Undo tokens live in the indexed KV with
short TTL.

### The system-wide contract

The shape of gimir, summed across clusters, makes the
following promises by construction:

- **Authority lives in text files at known paths or in the
  upstream feeds.** repos.txt and stores.txt are the user's
  authoring surface; everything else is either upstream data
  preserved as faithfully as the source allows or derived from
  those two text files plus the upstream data. The system can
  always be reconstructed by re-running the workflows.
- **Bytes are stored once and only once across forks,
  submodules, identical blobs, and identical PageViews.**
  Content-addressed storage is the gimir-wide commitment.
- **Versioned payloads at scale are stored as per-key chains
  with a hot solitary head, a warm accumulator, and an
  immutable cold tail.** Head reads are fast by construction;
  history reads are bounded by the chain depth; storage is
  compact because the per-instance pretrained dictionary plus
  refPrefix compress the redundancy across revisions.
- **State that needs transactional consistency lives in a
  single indexed KV per Store.** Multi-row atomic updates use
  the underlying engine's transactional primitive; nothing is
  layered on top.
- **Durability is "append, fsync, flip a pointer," at every
  level the system writes its own data.** No write-ahead logs
  of our own, no journals of our own, no CRCs of our own.
- **The system is single-writer per artifact: per chain, per
  KV, per ref namespace, per Mirror.** Concurrency comes from
  fanning out across artifacts, not from contention on one.
- **Time is a first-class index, not a metadata afterthought,
  for the wiki cluster.** The titles index, the siteinfo
  sequence, the file-metadata history, the revmeta lane, the
  events lane, the category transitions — all of them carry
  τ in their key or in their value's structure, so render-at-τ
  is a sequence of point lookups rather than a global scan.
- **Mirror lifecycle is "create on first appearance in
  repos.txt, delete on disappearance, trash before sweep."**
  The undo discipline is structural: deletion is cheap to
  reverse for a bounded window, so the admin UI exposes
  one-click delete (historian §4 rule 2).
- **Every external operation leaves a forensic trace.** The
  run log is the witness; nothing escapes capture.

The system-wide contract in one sentence: gimir is a per-user
local replica whose authority is a text file and a set of
upstream feeds, whose bytes live in content-addressed pools
and per-key compressed chains, whose state lives in
transactional indexed KVs, and whose durability story at every
layer is "append, fsync, flip a pointer" with no journals of
our own — so that the system survives crashes by re-reading
what the platform committed, scales to a Wikipedia instance by
exploiting the inter-revision redundancy that per-row storage
cannot, and serves a render-at-τ query as a sequence of point
lookups against time-indexed structures.
