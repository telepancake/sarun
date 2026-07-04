# Wikipedia mirroring: research findings + implementation plan

Status: PLAN (nothing implemented). Researched 2026-06-12 against live
dumps.wikimedia.org artifacts, mediawiki.org manuals, wikitech, and the
xmldatadumps-l list. Facts below marked [verified] were checked against
primary sources this session; the full citation trail is in the appendix.

Goal: full-history imports of MediaWiki instances (user picks which sites
to keep up to date in repos.txt), stored in gimir's chain/mux zstd
storage, kept current incrementally, with enough local structure (titles,
redirects, siteinfo) that the wiki can later be browsed fully offline.
Rendering has its own companion plan (`wikipedia-browsing-plan.md`),
written before implementation so the import schema is validated against
what the renderer needs; its §7 checklist produced two amendments folded
in here (normalized title keys in §2.4; file-metadata ingestion below)
and one accepted gap (historical category membership). This plan covers
the depot, instance table, and titles table.

---

## 1. The source landscape (what we verified)

### 1.1 Full-history dumps

- Legacy XML dumps (`dumps.wikimedia.org/<dbname>/<YYYYMMDD>/`) are
  **deprecated since 2026-01** and reduced to monthly cadence. The
  replacement is **MediaWiki Content File Exports**: two monthly datasets,
  `mediawiki_content_history` (all revisions) and
  `mediawiki_content_current`, at
  `dumps.wikimedia.org/other/mediawiki_content_history/<wiki>/<YYYY-MM-DD>/xml/bzip2/`,
  bzip2 only, `SHA256SUMS` per directory, files named
  `<wiki>-<date>-pNNNNpNNNN.xml.bz2` with oversized pages split by
  revision range (`...-pXrAAArBBB.xml.bz2`). [verified: downloaded a
  cswiki part — valid export-0.11 XML, same schema as legacy, generator
  "MediaWiki Content File Export 0.3.12"]
- Legacy full-history (`pages-meta-history`) still produced for now: bz2
  AND 7z for every wiki. enwiki 20260501: **1,886 GB bz2 / 292 GB 7z**
  across 961 standalone files; fiwiki ~17 GB bz2 / 4.4 GB 7z. Each part
  file is a complete XML document with its own `<siteinfo>` header.
  [verified via dumpstatus.json + file inspection]
- **Multistream exists only for pages-articles**, never for history
  dumps. History bz2 files are SINGLE-stream, multi-block (written by
  lbzip2). [verified by byte-scan: one stream header, ~550 blocks]
- XML schema export-0.11: `<page>` (title, ns, id, optional
  `<redirect title=.../>`), `<revision>` (id, parentid, timestamp,
  contributor username+id|ip, minor, comment, origin, model, format,
  `<text bytes sha1 xml:space>`, `<sha1>`). `sha1` = SHA-1 of the UTF-8
  wikitext in base-36, zero-padded to 31 chars [verified by recomputing].
  RevisionDelete'd parts appear as empty elements with
  `deleted="deleted"` (text/comment/contributor independently); bytes and
  the text sha1 attribute MAY still be present on a deleted text element.
  Page-deleted revisions are absent from dumps entirely (archive table).

### 1.2 Incremental sources

- **adds-changes dumps** (`dumps.wikimedia.org/other/incr/<wiki>/<date>/`):
  still produced daily as of 2026-06-11 [verified directory listing].
  Contents: `pages-meta-hist-incr.xml.bz2` (same XML schema), stubs,
  `maxrevid.txt` (largest rev id from 12h before the run; the range is
  (previous day's maxrevid, this maxrevid]), `status.txt`, md5sums.
  ~40 days retained. Explicitly experimental: "we don't guarantee the
  data is complete or correct". **Deletes/moves/suppressions are NOT
  captured** — only new revisions. Runs on the deprecated legacy infra;
  no announced successor — treat as best-effort and design for fallback.
- **EventStreams** (stream.wikimedia.org, SSE): metadata-only (no
  revision text), streams incl. revision-create, page-move, page-delete,
  page-undelete, revision-visibility-change; 7–31 days retention;
  resume via timestamp-based Last-Event-ID. Free.
- **Action API**: the only free revision-TEXT path outside dumps.
  `prop=revisions&rvprop=content` capped at 50 revs/request;
  Special:Export capped at 1000 revs (POST continuation). Etiquette:
  serial requests, `maxlag=5`, descriptive User-Agent.
- Wikimedia Enterprise HTML free mirror was **discontinued 2025-03**;
  free tier is monthly snapshots via API only; realtime is paid. Not a
  dependency for us.

### 1.3 Structure sources

- **mediawiki_history TSV** (`dumps.wikimedia.org/other/mediawiki_history/`):
  monthly, ~2–7 days after month end, **only the last 2 snapshots are
  retained**, each snapshot is a full rebuild since 2001 (never mix
  snapshots). TSV bz2, no header row, 76 columns (authoritative list:
  wikitech "MediaWiki history dumps"). Per-wiki files split monthly
  (enwiki/wikidata/commons), yearly (~40 mid wikis), or all-time.
  Relevant: `event_entity` page|revision|user, `event_type`
  create|move|delete|restore, `page_id`, `page_title_historical`,
  `page_namespace_historical`, `event_timestamp`, and on revision rows
  `revision_deleted_parts(_are_suppressed)`, `revision_text_sha1`,
  `revision_is_deleted_by_page_deletion`. Caveats: pre-Dec-2004 title
  history mostly unrecoverable; deleted pages without recoverable ids
  surface with null page_id; `page_is_redirect` is current-state only;
  complex delete/restore chains intentionally approximate (~99.9%
  metric accuracy).
- **SQL table dumps**: ~24 tables per run (page, redirect, categorylinks,
  page_props, linktarget, pagelinks, langlinks, sites, ...). NEVER
  published: text, revision, logging, user, archive. Format: gzipped
  mysqldump extended INSERTs. Note: pagelinks/templatelinks now join
  through `linktarget`. `pages-logging.xml.gz` carries log events
  (titles, not reliable page_ids before 2013).
- **siteinfo**: the XML dump header carries ONLY sitename, dbname, base,
  generator, case, and localized namespace names (with per-namespace
  case attributes). Namespace ALIASES + canonical names, magic words
  (incl. localized #REDIRECT synonyms), interwiki map, extension tags,
  parser function hooks, variables, protocols, languagevariants,
  specialpagealiases exist **only via
  `action=query&meta=siteinfo`** — exactly the set Parsoid's API
  SiteConfig consumes. Must be snapshotted alongside imports.
- **Instance enumeration**: `action=sitematrix` (meta.wikimedia.org) is
  the authoritative dbname↔domain map (~900 entries, with
  closed/private/fishbowl flags). `dumps.wikimedia.org/index.json` lists
  all wikis' latest run status; per-run `dumpstatus.json` gives exact
  file lists, sizes, sha1s.

### 1.4 ID semantics (drives the depot design)

page_id and rev_id are unique per instance, so depot keys are
(instance, page_id) and (instance, page_id, rev_id) — but **the mapping
is not append-only**:

1. rev_ids vanish between snapshots (page deletion → archive; oversight).
2. rev_ids move between page_ids (history merge; reversible via unmerge).
3. Revision content mutates in place: visible ↔ `deleted="deleted"` per
   field (RevisionDelete), with rev_id unchanged.
4. page_ids disappear and reappear (delete + undelete reclaims the old
   id only best-effort since MW 1.27); a title can switch page_id (move
   leaves a NEW page_id redirect behind; delete + recreate).
5. New rev_ids BELOW the watermark can appear (Special:Import assigns
   fresh local ids out of order; undeletion restores old rev_ids).
6. rev_parent_id chains are not fixed up after merges.

Consequence: the depot must be **event-sourced**. Imported revisions are
immutable records; corrections (visibility changes, tombstones, merges,
moves) are appended as control events, never in-place edits. "Max rev_id
seen" is a per-source-file watermark, not a per-page correctness claim.

### 1.5 Sizes / decompression (drives the pipeline design)

- User's prior experiments: full history in refPrefix zstd chains lands
  in the text-only Kiwix ZIM size class — an order of magnitude under
  the bz2 dumps. Reference points: enwiki ZIM nopic ~48 GB; enwiki
  history bz2 ~1.9 TB (~55–60 TB XML). The dominant cost is the HEAD
  set (newest revisions, standalone-compressed): ~400–500 GB at enwiki
  scale depending on dict quality, frame-0 batching, and compression
  level; the refPrefix tail of older deltas is comparatively small.
  Small wikis are MBs–GBs.
- bz2: single-threaded decode ≈ 30–60 MB/s output per core. lbzip2-style
  block-parallel decompression works on these single-stream files
  (bit-aligned block boundary scan); pbzip2-the-tool does NOT. In Go:
  vendor a block-parallel bz2 reader (`github.com/cosnicolaou/pbzip2`
  pattern — pure Go, scans block magic, decodes blocks concurrently) with
  `compress/bzip2` as the correctness fallback. Additional parallelism
  across part files (961 for enwiki) is free since each is standalone XML.
- 7z (legacy only): 6.5× smaller download but single LZMA2 block — no
  intra-file parallel decode, and the new exports are bz2-only. Optional
  seeding optimization, not the main path.

---

## 2. Design

### 2.1 Shape in gimir

A new non-code source, exactly like `web:`/`ietf`:

- repos.txt entry: `wiki:en.wikipedia.org` (or `wiki:<dbname>`); repourl
  learns the `wiki` hint; `MirrorRelPath()` → `wiki/<dbname>`. dbname is
  the stable instance key (domains can change; dbnames don't).
- `providers.wikiSource` with `Kind() == "wiki"`: engine skips git,
  Fetch owns the pull. Per-mirror `store=` binding (stores.txt) chooses
  the backing — **recommend sqlite backing for big wikis** (see 2.6).
- All state in `m.Store`: depot in the versioned store, instance/titles/
  watermarks in KV, siteinfo + dictionaries in blobs.

### 2.2 Page depot

Artifact key: `page/<page_id>` (instance comes free from the per-mirror
store). Columnar lanes per page — this is what mux was built for:

- `text` — wikitext bytes per revision (empty + flag when hidden).
  Compression: pretrained per-instance dict on chain frame 0, refPrefix
  for frames 1+. Frame 0 is SOLITARY — it holds the newest revision
  alone, which is what makes head reads one small decode (the
  random-access property). The dict is what makes solitary viable:
  without it, approaching ZIM-class compression would force ZIM-style
  multi-revision blocks and destroy head-read latency; the dict
  recovers most of that compression on a standalone record. Older
  revisions in frames 1+ are near-optimal under refPrefix. Consequence
  of solitary frame 0: EVERY prepend spills the old head — into the
  frame-1 ACCUMULATOR, a multi-revision refPrefix frame that is
  re-encoded on each update and splits off an immutable chunk once it
  reaches the seal threshold. One revision per frame is an
  anti-pattern: each tiny frame pays refPrefix context setup on
  decode, restarts its entropy tables (fresh Huffman/FSE tables can
  outweigh a small delta), and can only reference its single-neighbor
  prefix instead of the whole in-frame window. Multi-revision frames
  amortize all three (benchmark the per-revision-frame overhead if in
  doubt — it is large).
- `comment` — edit summaries. Pretrained per-instance dict.
- `contributor` — encoded user id + name | IP. Pretrained dict.
- `revmeta` — fixed encoding of rev_id, parent_id, timestamp, flags
  (minor, text/comment/user-hidden, suppressed), model/format (interned),
  sha1 (binary, 20 bytes from base-36), byte size, origin. Pretrained dict.
- `events` — sparse control lane: page move / delete / restore / merge
  in+out / visibility-change / tombstone records, each with source
  attribution (which dump/TSV snapshot asserted it). This is where the
  1.4 mutations land as appends.

Revision identity within a page: rev_id. Re-importing the same revision
is idempotent (skip if rev_id already present with equal sha1; if sha1
differs, append a correction event — that's a RevisionDelete flip).

### 2.3 Instances table

KV `instances/<dbname>` → JSON {dbname, domain, api URL, project, lang,
flags, added_at, watermarks: {last_full_dump, last_incr_date,
last_history_snapshot, maxrevid}}. Populated from sitematrix on add,
refreshed on pull. siteinfo: full
`action=query&meta=siteinfo&siprop=<everything>` JSON appended to
versioned artifact `siteinfo` (deduped when unchanged) — history of site
config is itself useful and cheap.

### 2.4 Titles table

The question it answers: "what page_id did title T in namespace N point
at, at time τ?" plus the reverse "what was page <id> called at τ?".

- KV `title/<ns>:<dbkey>` → encoded interval list
  [(start_ts, end_ts|open, page_id)], newest first. Keys are stored
  NORMALIZED at import time (namespace resolved to its canonical id,
  underscores→spaces, per-namespace case rule applied per the siteinfo
  snapshot) so render-time lookups — including red-link existence
  checks and {{#ifexist:}} — are exact point lookups (browsing plan
  §7).
- KV `pagetitle/<page_id>` → interval list of (ns, dbkey) — the reverse
  index, built from the same events.
- Built from mediawiki_history TSV: page-entity rows
  (create/move/delete/restore: open/close/reopen intervals keyed by
  page_id, title_historical) cross-validated against revision rows'
  `page_title_historical`, the XML dump's `<redirect>` elements and
  current titles, and `page.sql.gz` + `redirect.sql.gz` for the current
  snapshot state. Post-2013 moves can be spot-checked against
  `pages-logging` (log_page is reliable from 2013 on).
- Rebuild discipline: each monthly TSV snapshot is a full rebuild —
  the title index is **rebuilt from scratch on each ingest** (idempotent,
  derived data), then the TSV is discarded. Between snapshots, current
  pointers are advanced by incremental signals (new pages from
  adds-changes; later EventStreams moves).
- Known holes documented in the index itself: pre-Dec-2004 events are
  approximations; entries inferred from revision titles get a flag.

### 2.5 Import pipeline (storage-considerate)

Seed (per instance):
1. Discover newest complete run via Content File Exports listing
   (fallback: legacy dumpstatus.json). Enumerate part files + checksums.
2. Stream each part: HTTP GET → block-parallel bz2 decode → streaming
   XML parser → per-page revision batches. **Nothing is spooled to
   disk** — no .bz2 retained, no decompressed XML ever materialized.
   Crash recovery = per-file rerun; idempotence makes reruns cheap
   (pages already at-or-past the file's revisions are skipped by rev_id
   check before any chain write).
3. Per page: buffer revisions up to a byte budget (default 256 MB;
   wikitext caps at 2 MB/rev), reverse to newest-first, one batched
   columnar append per buffer. Oldest buffer lands first, newer buffers
   prepend on top — chain head re-encoding stays bounded.
   SPLIT PAGES (load-bearing, easy to miss): a page being complete
   within one part file is NOT an invariant. At least enwiki has pages
   whose revisions span multiple dump files — the content exports name
   them explicitly (`<wiki>-<date>-pXrAAArBBB.xml.bz2` = page X,
   revisions AAA..BBB), and other shapes may exist. Two defenses,
   both required:
   (a) scheduling — parse page/revision ranges out of part filenames;
   files covering the SAME page id form a group imported sequentially
   in ascending revision order (slice 1 seeds, later slices prepend on
   top — the same oldest-first/newer-on-top composition as chunked
   buffers); groups with disjoint page coverage parallelize freely.
   (b) depot-existence fallback — regardless of scheduling, when the
   importer encounters a page_id that already has revisions in the
   depot, it switches from Seed to a merge-Prepend with per-rev_id
   dedupe (skip revisions already present; newer revisions prepend).
   This also covers dump flavors that split pages without announcing
   it in the filename, and overlapping reruns.
   Consequence for resume/idempotence: the per-page watermark is
   max(rev timestamp) actually stored, checked before any chain write
   — already the design, but split pages make it mandatory rather
   than belt-and-braces.
   SEED: during initial import all of a page's revisions are in hand
   at once, so the seed helper takes the bunch of blobs and emits
   `[solitary head][ONE frame with everything else]` — the tail frame
   compressed in STREAMING fashion (ZSTD_compressStream2, records fed
   one by one, prefix = the head; the concatenated payload is never
   materialized). No accumulator at all, no framing policy, no chunk
   policy. The accumulator MATERIALIZES on the first incremental
   update: Prepend with spillNew=true spills the head into a fresh
   frame above the sealed one.
   Why this is sound — the prefix anchor is a RECORD VALUE, not a
   frame: the sealed frame is encoded against the seed-time head
   record, and after the first spill that exact record is the
   accumulator's oldest record, terminal in the preceding frame
   forever across accumulator re-encodings.
   Chain-layer prerequisites (W1/W3): the Prepend(spillNew) op + the
   streaming seed helper (see §2.7), and a windowLog CAP (~128 MB) —
   inter-revision matches are adjacent, so capping costs ~nothing and
   keeps decode memory sane.
3b. Category transitions: the same revision scan extracts direct
   [[Category:X]] declarations (localized ns aliases from siteinfo)
   and records (page_id, category, added/removed-at) transitions into
   a time-indexed membership KV — feeds category-at-date browsing
   (browsing plan §2); template-added categories are out of scope for
   this index by design.
4. Verify per revision: recompute sha1 (base-36) of text where present
   — but KNOWN DATA QUALITY ISSUE: some dump revisions carry text whose
   newlines were fudged by the dump pipeline, so the recomputed sha1
   does not match the dump's own sha1 (the API would return the
   correctly-hashed version; we deliberately do NOT hit the API for
   this — dumps are the source of truth for import). Policy: on
   mismatch, optionally brute-force cheap newline variants (trailing
   \n added/stripped, \r\n vs \n) and accept the first that matches;
   if none match, store the text AS DUMPED, set a sha1-mismatch flag
   in revmeta, and count it — a mismatch is logged data quality, NEVER
   an import failure. Count reconciliation against dumpstatus.json
   revision/page counts at file level; record per-file done markers +
   maxrevid watermark in KV.
5. Dict training: a two-pass-over-the-head shape. First, stream the
   import without a text dict so initial pages land — comment/
   contributor/revmeta lanes can land with provisional dicts trained on
   a small upfront sample (these lanes are small per record, the win
   from a perfect dict is modest, and they're not the bottleneck).
   Second, once N pages of newest-revision wikitext have been observed
   (target N ~ a few thousand pages of representative samples — enough
   for `ZDICT_trainFromBuffer` to converge), train the per-instance
   text dict via `chain.BuildDict`, persist the dict bytes in blobs
   keyed by dict_id, and **repack the head of every text chain**: a
   single `chain.Prepend([], dict, framing)` on each page's text lane
   re-encodes frame 0 with the dict (head-frame-only optimization;
   frames 1+ are unchanged byte-identical) and from that point every
   new revision joining the chain inherits the dict treatment. The
   comment/contributor/revmeta lanes can be re-dicted on the same
   schedule if their samples converge later. DictProvider is resolved
   from `blobs.Get(dict_id_blob)`; per-mirror dict_id → blob hash map
   lives in KV `dict/<lane>/<dict_id>`.
6. Concurrency: parts are independent; process K parts in parallel
   under the hostlimit budget; within a part, bz2 block decode is
   parallel while XML parse stays sequential.

Update (per pull, respecting the existing cooldown ladder):
1. siteinfo refresh (cheap).
2. adds-changes: for each unconsumed day ≤ today, ingest
   `pages-meta-hist-incr.xml.bz2` (same parser), advance maxrevid
   watermark from `maxrevid.txt`. Missing/failed days → fall back to
   API catch-up (`generator=allrevisions` window) bounded by etiquette
   rules, or wait for the next monthly full export and reconcile.
3. Monthly when a new mediawiki_history snapshot lands: rebuild titles
   index; reconcile revision visibility (`revision_deleted_parts`) and
   page delete/restore/merge state against the depot, emitting events
   for diffs. This is what catches everything adds-changes can't see.
4. Optional later: EventStreams subscription for near-realtime
   move/delete/visibility events (metadata-only, applied as events).

### 2.6 Store capability: columnar versioned artifacts

The current `store.VersionedStore.Append(key, metadata, body Hash)` is
two-lane and one-revision-at-a-time. The depot needs (a) named columns
and (b) batched appends (per-revision chain rewrites would be quadratic
I/O). Per hard rule 8 (no per-backing capabilities), this is a store
interface extension implemented across ALL THREE backings + storetest:

- `store.ColumnarStore` (reached via `Store.Columnar()`):
  `AppendRevisions(artifactKey string, revs []ColumnRevision) error`
  where `ColumnRevision = map[column string][]byte` (newest-first batch);
  `IterateRevisions(artifactKey, columns []string) iter` (newest-first,
  decode only requested columns); `Latest`, `Artifacts` analogous to
  VersionedStore. Column dict configuration via per-store options.
- fs backing: direct mapping onto mux (one mux per artifact, lane per
  column) + a batch `mux.AppendRevisions` (chain.Prepend already takes
  batches; mux only needs the plural entry point).
- mem backing: trivial slices.
- sqlite backing: per-column rows or chain-blobs — backing's choice;
  contract is behavioral.

This is the only storage-layer prerequisite; everything else composes
from existing primitives.

### 2.7 Depot architecture (final, supersedes earlier engine surveys)

The depot is custom. Three storage tiers, one shard format. Independent
shard sets per tier so reads don't fan out across temperatures, a
chain-id-keyed flat array as the only top-level index, and in-frame
pointers from each frame to the next so the index never grows with
history depth.

**The shard format** (one type, used by all three tiers):
- A shard is one append-only file. Writes always go to the current
  shard's tail. The previous location for a chain stops being
  reachable as soon as the index entry (or the preceding frame's
  next-pointer) is flipped to the new location. No tombstone bitmap,
  no sidecar — the index IS the liveness signal. On GC, walk the
  shard and check each frame against the index; frames not reachable
  from the index are dead.
- Each frame in a shard is preceded by an 8-byte `(u32 next_shard_id,
  u32 next_offset)` pointer at fixed offset BEFORE the zstd frame
  magic. The pointer is plain bytes, not part of the zstd frame —
  zstd frames are immutable and must never be mutated; the pointer
  needs to be flippable when GC relocates the next frame down the
  chain. So the on-disk layout per logical frame is
  `[ u32 next_shard_id | u32 next_offset ] [ zstd frame bytes ]`.
  Updating a chain pointer is a single 8-byte aligned pwrite — atomic
  on real filesystems — and zstd never sees those 8 bytes.
- GC is "when a shard's dead ratio crosses threshold T (e.g. 50%),
  walk the shard, copy live frames to the current tail of the same
  tier (which updates the chain-id entry-point array, or the
  preceding frame's pointer header, as the moved frame requires),
  unlink the dead shard." No allocator, no free-space tracking — the
  shard is monotone-append until repacked.
- Crash safety: write bytes → fsync → flip the index entry / chain
  pointer. If you crash before the flip is durable, you may lose the
  write. There is no recovery code; the next start re-reads whatever
  the filesystem actually committed. Same contract as the strpool
  crate.

**Three tiers, separate shard sets**:

| Tier | What lives here | Mutability | Why separate |
|---|---|---|---|
| f0 (hot) | One slot per chain: the newest revision standalone-compressed (with the chain's pretrained dict). Bounded fluctuating size — edits add bytes, reverts/deletes remove them. | Whole-value replaced on every page edit. | This is the ONLY tier the renderer reads for "show me the page now." Folding f1 in would page in records the reader never asked for. |
| f1 (warm) | One slot per chain: the accumulator (multi-record refPrefix frame, oldest-newer-on-top). Monotonically grows between seals; resets on seal. | Whole-value replaced on every page edit. Highest write *volume* of the three (grows toward the seal threshold). | Read only on history requests, not on current-revision browsing. Compaction cadence is fastest here, so isolating it keeps the f0 shards stable for cold reads. |
| Cold (sealed) | Append-only sealed frames, frame 2 and beyond. | Immutable until the chain is GC'd (page delete + tombstone aging). | Different storage hardware likely (slow disk OK); compaction is rare (tombstone-driven on bulk deletes). |

**One index, sized by chain count, not by frame count**:
- Flat array indexed by `chain_id`. Each entry is a fixed `(shard_id,
  offset)` pair pointing at the chain's f0 frame in the hot tier.
- f1 is found via the `(next_shard, next_offset)` header prepended to
  f0's bytes; cold frames via the same trick on f1, then chained.
- enwiki: 60M chains × 16 B = **~960 MB index**, mmap-able. All of
  wikimedia: 700M × 16 B ≈ **~11 GB**, still on-disk-fits-on-laptop.
- Fixed-size entries are load-bearing: they let the index be a flat
  array with arithmetic addressing (`base + chain_id × 16`), no
  hashmap or B-tree at the top level. Migration if we ever need to
  widen is "regenerate the array."

**Write costs per page-edit, characterised honestly**:
- f0: ~1.5 KB written (one head value, fluctuating, whole-value
  replace). Tombstones accumulate at the rate of edits-per-day; a
  ~1 GB shard reaches 50% dead in roughly its lifetime worth of
  edits — practical cadence: long.
- f1: starts ~1.5 KB right after a seal, grows toward the seal
  threshold (say 1 MB), averaging perhaps ~500 KB per write across
  the cycle. Volume is ~300× hot. Compaction cadence: short.
- Cold: only edits at seal time — one sealed-frame append per seal.
  No in-place changes. Compaction: rare, tombstone-driven on deletes.

**What we are NOT doing** (and why):
- Variable-size index entries. A trap — they kill the flat-array
  trick and force a hashmap/B-tree.
- Per-frame upper-level indexes. Frame-pointer headers make the index
  scale with chain count, not history depth.
- An embedded KV engine (sqlite/redb/fjall) for the three tiers.
  Their write-ahead logs, page caches, and compaction strategies pay
  for properties we don't use (in-place updates, range scans over
  arbitrary keys, MVCC). Auxiliary indexes (titles, categories,
  parts, siteinfo) ARE in sqlite or equivalent — that workload is
  read-mostly with range queries and B-trees fit it.
- Per-tier "do we have crash recovery" worry. The append-fsync-flip
  ordering is the only durability primitive; correctness follows.

For scale, the alternative of "just run the big engine": restoring the
enwiki full-history dump into stock MediaWiki+MariaDB stores one text
row per revision — uncompressed that is the raw ~55–60 TB; with
$wgCompressRevisions each revision gzips INDEPENDENTLY (generic row
storage cannot exploit that adjacent revisions are ~99% identical), so
~3–5× off → roughly 12–20 TB, plus a ~1.27B-row revision table and its
indexes, plus a database daemon. WMF production itself couldn't live
with per-row storage: their External Storage clusters concatenate
batches of adjacent revisions and diff them — a bespoke cross-revision
compressor bolted onto MySQL. The chain layer IS that compressor,
native. Expected ledger: ~0.6–0.7 TB (heads + packs + indices) versus
~15–60 TB, i.e. 25–100×, before counting operational weight.

**W3 bake-off, restated**: not "fs vs sqlite vs LSM," but
"measure each shard tier's compaction cadence in practice." Seed
cswiki on the three-tier design, replay a simulated month of daily
churn, log: shard write throughput by tier, dead-ratio over time per
tier, compaction frequency, total bytes hitting disk vs bytes
logically changed (write amp), cold-read latency. The Go fs and
sqlite backings stay in W1's columnar interface for small wikis; the
three-tier design is the big-wiki backing.

Knob coupling worth recording: solitary f0 + pretrained dict (§2.2)
is the chosen point on the compression-vs-random-access curve — the
dict recovers the compression that ZIM-style multi-revision blocks
would otherwise buy, without their head-read latency — and the f1
seal threshold is the second knob, trading f1 re-encode cost against
sealed-chunk granularity. The dict decision and the storage decision
reinforce each other.

---

## 3. Phases

- **W1 — store columnar capability.** `store.Columnar()` interface +
  mem/fs/sqlite implementations + storetest suite additions;
  `mux.AppendRevisions` batch entry point; **chain: restore the
  solitary-head/accumulator/seal discipline** (framing over the
  absorbed region + forced boundary, per §2.7 — prior art in the
  deleted internal/vbf/prepend.go, see git history). The tiered
  chain-native backing (§2.7: head store + accumulator + immutable
  packs) lands as its own step once the interface is proven on the
  existing three. Acceptance: suite green on all backings; batch of
  10k revisions lands in O(batch) I/O; steady-state single-record
  appends seal at the threshold and preserve sealed frames
  byte-identically.
- **W2 — dump plumbing.** `internal/mediawiki`: discovery client
  (Content File Exports listing + legacy dumpstatus.json + incr
  listing), checksum-verified streaming HTTP, block-parallel bz2 reader
  (vendored pure-Go, `compress/bzip2` fallback, correctness tested
  against both single- and multi-stream files), streaming export-0.11
  parser with `deleted="deleted"` handling + base-36 sha1 verify.
  Acceptance: parse a real small-wiki history dump (votewiki, 6.5 MB)
  byte-correct; parallel decode beats serial on a multi-MB file.
- **W3 — depot import (seed) + backing bake-off.** Page depot schema
  on Columnar, batched per-page import, idempotent reruns, per-file
  resume markers, dict training, progress via the existing activity
  infra. Acceptance: full votewiki seed end-to-end; cswiki/fiwiki
  (~17 GB bz2) seeded on the candidate backings (tiered packs,
  sqlite-only, LSM-only) and measured — throughput, on-disk size (expect ≈ ZIM-text scale), then
  a simulated month of daily churn comparing write amplification,
  file growth, and cold-read latency (§2.7 decision gate);
  re-running a completed file is a fast no-op.
- **W4 — instance wiring.** sitematrix client, instances KV, siteinfo
  snapshotting; `wiki:` hint in repourl; `providers.wikiSource`
  (Kind "wiki") + engine/CLI wiring; cooldown integration. Acceptance:
  `wiki:<domain>` in repos.txt → `gimir pull` seeds and is resumable;
  admin index lists the wiki mirror with a badge.
- **W5 — titles.** mediawiki_history TSV streaming parser (76-col,
  escaping rules), interval builder, title + reverse KV indexes,
  page-events lane population, page.sql/redirect.sql cross-check
  parser (extended-INSERT streaming). Acceptance: title-at-time queries
  correct on a wiki with documented moves; full rebuild idempotent.
- **W6 — incremental updates.** adds-changes daily ingest with maxrevid
  watermarking and gap handling; monthly TSV reconciliation (visibility
  flips, deletes/restores/merges as events); API catch-up fallback.
  Acceptance: two consecutive days of a real wiki's incr dumps applied;
  a simulated RevisionDelete flip lands as an event without rewriting
  history.
- **W7 — file metadata.** Seed a KV file-metadata table per instance
  from `image.sql.gz` (current version: sha1 base-36, size, dims,
  mime, upload timestamp); historical version metadata comes from
  `prop=imageinfo` on first need and is cached (the oldimage table is
  not in the public dumps). Required by the browsing plan's media
  pipeline (file-version-at-date selection via
  `archive/<ts>!<name>` URLs); binaries stay lazy (browsing plan §4).
- **W8+ (explicitly deferred).** Rendering — now planned separately in
  `wikipedia-browsing-plan.md` (phases B1-B7); admin per-page history
  browser; EventStreams realtime; 7z-seeded legacy import;
  categorylinks/page_props ingestion for current-category browsing;
  WACZ-style export.

Each phase follows the established two-agent tester→implementer flow
with real-corpus acceptance tests (votewiki/cswiki are small enough to
use real dumps in CI-adjacent smoke tests; synthesized fixtures cover
the nasty cases: deleted attrs, merges, out-of-order rev_ids).

---

## Appendix: primary sources

- Content File Exports: wikitech "MediaWiki Content File Exports";
  dumps.wikimedia.org/other/mediawiki_content_history/readme.html;
  xmldatadumps-l announcement thread (2026-01-31).
- Legacy dumps: meta "Data dumps/Dump format", "/Dump frequency",
  "/What's available for download"; enwiki 20260501 dumpstatus.json;
  operations-dumps xmldumps-backup + mediawiki-dumps-legacy chart config
  (pagesPerChunkHistory, lbzip2forhistory, maxrevbytes=35e9).
- XML schema: mediawiki.org/xml/export-0.11.xsd; Manual:Revision_table
  (base-36 sha1); Manual:RevisionDelete; Data dumps/Dump format
  (deleted="deleted").
- Incrementals: dumps.wikimedia.org/other/incr/ + wikitech
  "Dumps/Adds-changes dumps"; EventStreams: stream.wikimedia.org/?spec +
  wikitech "Event Platform/EventStreams HTTP Service"; API:Revisions,
  Manual:Parameters_to_Special:Export, API:Etiquette.
- mediawiki_history: dumps.wikimedia.org/other/mediawiki_history/
  readme.html; wikitech "Data Platform/Data Lake/Edits/MediaWiki history
  dumps" (76-field schema), "/Mediawiki page history" (state intervals,
  page_artificial_id — NOT exported), "Page and user history
  reconstruction algorithm"; Diff blog 2020-10-01 (99.9% accuracy claim).
- SQL/siteinfo: simplewiki latest listing (table inventory);
  Manual:Redirect_table, Manual:Page_table, Manual:Sites_table;
  API:Siteinfo; Parsoid src/Config/Api/SiteConfig.php (the siprop set a
  renderer needs); Extension:SiteMatrix.
- ID semantics: Manual:Page_table, Manual:Revision_table,
  Manual:Merging_histories, Manual:Importing_XML_dumps,
  Help:Moving_a_page, Help:RevisionDelete.
- bz2: lbzip2 man page (parallel decompression of any .bz2), pbzip2 man
  page (cannot), phabricator T239866 (old pbzip2 truncation bug),
  mediawiki.org/wiki/Dbzip2 (never deployed); empirical single-stream
  verification of history parts; github.com/cosnicolaou/pbzip2 +
  github.com/mxmlnkn/indexed_bzip2 (block-parallel readers).
- Sizes: enwiki 20260501 dumpstatus.json (1,886 GB bz2 / 292 GB 7z /
  961 files); download.kiwix.org/zim/wikipedia/ (en nopic ~48 GB);
  meta "Data dumps/Dumps sizes and growth".
