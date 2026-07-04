# Wikipedia mirroring: browsing & rendering plan

Status: PLAN. Companion to `wikipedia-import-plan.md` — written BEFORE
import implementation on purpose, so the import schema is validated
against what rendering actually needs (§7 is that checklist).
Research integrated: media access (no media dumps; lazy fetch — §4)
and non-PHP wikitext/Scribunto prior art (§3a).

Goal: a local browser window showing accurate-enough rendered wikitext
with interwiki links, infoboxes, external links, and images/media all
working and all served from the local copy — plus a wayback-style date
picker: pick τ, and the page renders as it stood at τ, using page
revisions, template/module revisions, AND page titles as of τ.

---

## 1. The forcing decision

The date picker kills every shortcut. Pre-rendered HTML (Enterprise
dumps, ZIM, scraping) only exists for "now"; rendering at arbitrary τ
requires expanding wikitext locally with the template/module closure
resolved at τ. So gimir needs a **local wikitext renderer**. This is
the largest single work item in the whole Wikipedia effort — larger
than import — and it's a permanent approximation chase (XOWA spent
years at it). "Accurate enough" therefore gets an explicit metric and
harness (§6) instead of vibes.

The good news: the import plan already stores everything a renderer
needs, in time-sliced form — full wikitext of ALL namespaces
(Template:, Module:, MediaWiki:, File: description pages included),
titles-at-τ, siteinfo snapshots. Rendering is a pure function of the
depot: `render(instance, title, τ) → HTML`.

## 2. Time-sliced resolution (the asof contract)

One rule applied uniformly: **resolve everything at τ.**

- Page: title → page_id via titles-at-τ; content = revision with max
  timestamp ≤ τ (revmeta lane scan, newest-first).
- Redirects: a property of the revision text (#REDIRECT parsed from
  the τ-revision; localized synonyms from magicwords) — NOT from the
  current-only redirect table. Follow at τ, loop-capped.
- Templates/modules: every transclusion {{X}} / {{#invoke:M|f}}
  resolves Template:X / Module:M through titles-at-τ → page_id →
  revision-at-τ. Recursively, whole closure at τ, with MediaWiki's
  expansion-depth and loop-detection limits.
- Parser functions that query the wiki ({{#ifexist:}},
  {{PAGESINCATEGORY:}}, {{REVISIONTIMESTAMP}}...) answer from the
  titles table / revmeta at τ.
- System messages ({{int:...}}, interface text): MediaWiki: namespace
  pages at τ; fall back to shipped defaults for messages never edited
  locally.
- Site CSS/JS: MediaWiki:Common.css, skin CSS pages, TemplateStyles
  subpages — all wiki pages, all resolved at τ.
- siteinfo (namespaces, aliases, magicwords, interwikimap): snapshot
  with max fetch-time ≤ τ; for τ before our first snapshot, use the
  oldest we have (config drift across years is real but mostly
  additive; recorded as an approximation).
- Media: file binary + thumb as of τ — VERIFIED fully reconstructible:
  superseded versions live at
  `upload.wikimedia.org/.../archive/<x>/<xy>/<YYYYMMDDHHMMSS>!<Filename>`
  and `prop=imageinfo` (iistart/iiend, iiprop=url|archivename|sha1)
  enumerates versions with direct archive URLs; pick newest ≤ τ. The
  oldimage table is NOT dumped, so version metadata comes from the
  API on first need and is cached in KV.

Honest wayback caveats (displayed in the UI, not hidden): the depot
sees history as visible at import time — pages deleted before our
first import have no revisions at any τ; RevisionDelete'd text renders
as "revision hidden"; pre-Dec-2004 title history is approximate
(import plan §1.3). Category membership at τ: note the category PAGE
in the dump carries only the header text — the member list is built
from declarations on member pages. DIRECT declarations
([[Category:X]] in page wikitext) get a time-indexed membership index
built during import (the seed pipeline touches every revision anyway;
it records add/remove transitions per (page, category)). What that
index cannot see is TEMPLATE-ADDED membership (stub/tracking/infobox
categories emitted during expansion) — those are current-only via
categorylinks.sql and marked as such in category listings.

## 3. Renderer architecture (`internal/wikitext`)

Layered, each layer testable alone, house-style in-house:

1. **Parser core** — wikitext → document tree → HTML: headings,
   lists, tables, links ([[...]] with ns/interwiki/File: dispatch),
   external links, formatting, <nowiki>/<pre>, HTML-in-wikitext
   sanitization. Conformance corpus: MediaWiki's parserTests
   (tests/parser/ in core, thousands of input→HTML cases; GPL-2.0+,
   so FETCH at test time rather than vendoring into the repo).
   There is no formal wikitext grammar; the de-facto spec is
   parserTests + Parsoid's wikipeg PEG (tokenization reference only).
2. **Preprocessor/transclusion engine** — template expansion with
   MediaWiki's exact order: <includeonly>/<noinclude>/<onlyinclude>,
   parameter substitution ({{{1|default}}}), parser functions (core +
   ParserFunctions: #if, #ifeq, #switch, #expr, #time, #ifexist...),
   magic words/variables ({{PAGENAME}}, {{CURRENTYEAR}} → τ!),
   subst-irrelevance (dumps carry post-subst text), depth/loop limits.
   All page lookups go through the asof contract (§2).
   NO prior art in Go exists (verified survey: Go wikitext packages
   are dump readers/AST-only toys; zero transclusion, zero Scribunto)
   — this is first-of-its-kind. The authoritative expansion reference
   is MediaWiki core's Preprocessor_Hash.php + PPFrame (GPL —
   reference-only, no code reuse); note Parsoid itself does NOT
   reimplement the preprocessor (it calls MediaWiki's), so Parsoid is
   a tokenizer/DOM reference, not an expansion one. Correctness
   check: diff against `action=expandtemplates` on sampled corpora.
3. **Scribunto layer** — {{#invoke:}} via embedded Lua 5.1
   (Scribunto is Lua 5.1-only; sandbox removes io/os/coroutines, so
   coroutine fidelity is moot; default limits 50 MB / 7 s CPU; frame
   semantics: lazy frame.args, the two-frame frame:getParent()
   pattern, frame:preprocess / expandTemplate / callParserFunction).
   Engine choice is a B3 spike with two candidates:
   (a) **vendored PUC Lua 5.1 via cgo** — exact pattern/number
   semantics, ~3× faster, and vendored-C is the house style (zstd,
   sqlite); costs cgo callback chattiness for the mw.* boundary;
   (b) **gopher-lua (pure Go)** — easier sandboxing via contexts, but
   its pattern matching is Go-regexp-based (no back-references, no
   position captures) and mw.ustring is BUILT on Lua patterns — the
   single biggest fidelity risk; choosing (b) means writing a real
   Lua-pattern engine in Go (small, bounded — PUC's matcher is a few
   hundred lines).
   The mw.* host side is ours either way: mw.text, mw.title,
   mw.ustring, mw.html, mw.uri, mw.language (plural/formatnum — stub
   then grow), mw.site (from siteinfo), mw.message, mw.hash.
   References with usable licenses: **XOWA's Java Scribunto (dual
   GPLv3/Apache-2.0 — full mw.* port incl. mw.wikibase; abandoned but
   complete)** and **wikitextprocessor (Python, MIT, active — tells
   us which mw.* surface real modules actually exercise)**; plus
   Scribunto's own lualib Lua files (the host-callback boundary spec,
   GPL — reference-only). Module: pages come from the depot at τ.
4. **mw.wikibase** — many infoboxes pull from Wikidata at render
   time. Optional but schema-free: wikidatawiki is just another
   instance (entity JSON revisions in its dumps, entity-at-τ via the
   same depot). Without it: render placeholders, count the misses in
   the harness. With it: wire mw.wikibase to the local wikidatawiki
   depot.
5. **Extension tags** — <ref>/<references> (Cite) implemented early
   (ubiquitous); <gallery>, <templatestyles>, <math> (client-side
   KaTeX/MathJax over the raw TeX — Wikipedia itself is moving to
   native MathML and browsers now support MathML Core, so no
   Mathoid-style service is needed), <score>,
   <timeline>, <charinsert>... progressively; unknown tags render as
   visible labeled placeholders, never silently dropped, and are
   counted by the harness.

Failure discipline: a template/module that errors renders an inline
error box (like MediaWiki's script-error), never aborts the page.

## 4. Media pipeline

No bulk media dumps exist — VERIFIED: none since ~2013, your.org
mirror tarballs frozen at March 2013, Phabricator T298394 (produce
Commons media dumps) open and unassigned, ~108M files / ~400 TB
originals. Full Commons is petabyte-class anyway, so media is
**lazily materialized**, which is also the storage-considerate answer:

- On first render that needs File:X: resolve local-vs-Commons (check
  the wiki's own image table first, fall back to commonswiki — the
  MediaWiki repo-chain model), compute the upload.wikimedia.org URL
  (VERIFIED scheme: `/<project>/<x>/<xy>/<Filename>` with x/xy = first
  hex chars of md5(filename_with_underscores); thumbs at
  `/thumb/<x>/<xy>/<Filename>/<NNN>px-<Filename>`), fetch the
  pre-scaled thumb at a STANDARD bucket width (20/40/60/120/250/330/
  500/960 px render buckets; non-standard widths get snapped
  server-side anyway), store in the per-mirror blob store keyed by
  (file, version, width), serve locally forever after.
- Etiquette is codified, not vibes — wikitech Robot policy for
  upload.wikimedia.org: **≤2 concurrent connections, ≤25 Mbps**,
  prefer thumbs over originals, standard sizes only,
  Accept-Encoding: gzip, honor 429 Retry-After, pause 15 min after
  5xx, descriptive User-Agent with contact info mandatory. Wire these
  limits into the hostlimit budget as a per-host policy.
- Metadata: `image.sql.gz` (current version per file: sha1 base-36,
  size, dims, mime, upload timestamp; commonswiki's is ~18 GB) seeds
  the KV file-metadata table (import W7); HISTORICAL versions come
  from `prop=imageinfo` on first need (oldimage is not dumped) and
  are cached. The daily `mediatitles` dump (all ns-6 titles per wiki)
  plus per-wiki `imagelinks.sql.gz` can build a needed-file manifest
  for prefetch.
- A "prefetch media for mirrored pages" maintenance command for users
  who want fully-offline-capable mirrors (bounded by namespace/page
  selection and the Robot-policy budget); otherwise offline rendering
  shows placeholder boxes for never-fetched media.
- File: description pages (attribution/licensing) render from the
  commonswiki/local depot like any page when those instances are
  mirrored; template-heavy, so Commons Template:/Module: namespaces
  matter for attribution display. Structured Data on Commons JSON
  dumps + imageinfo extmetadata are the machine-readable supplement.

## 5. Serving (extends `gimir serve`)

- Routes: `/wiki/<instance>/<title>` (current), `?asof=<timestamp>`
  (the date picker), `/wiki/<instance>/media/...` (blob-served),
  special routes: Special:AllPages (titles table), Special:Search
  (later), per-page history view (revmeta lane — cheap, columnar).
- Date picker UI: calendar control pinned in a header bar; sticky per
  session; every internal link carries the asof through; "this page
  at this date" permalinks.
- Red/blue links: batched existence-at-τ checks against the titles
  table during render.
- Interwiki: interwikimap-at-τ; prefixes resolving to a locally
  mirrored instance become local links (cross-instance browsing);
  everything else renders as an external link, visually marked;
  offline mode renders them inert.
- External links: rendered normally, marked as external.
- Caching: render-on-demand is the model; cache is an optimization
  with an honest key: (instance, page_id, resolved rev_id, τ-day,
  renderer version). Template-closure changes under a fixed page
  rev_id make perfect invalidation expensive — accept τ-day staleness
  granularity, bound the cache (LRU in KV/blobs), flush on renderer
  version bump.

## 6. Accuracy: defined, not vibed

`internal/wikitext` ships an accuracy harness from day one:

- **parserTests conformance**: run MediaWiki's parserTests corpus
  (tests/parser/ in core + per-extension files incl. Scribunto's;
  GPL-2.0+ → fetched/cached at test time, never vendored), track
  pass-rate; gate phases on agreed subsets (core syntax first,
  extensions later).
- **Live-diff harness**: for a sampled corpus of real pages (per
  wiki: top-N viewed + random-N + known-hard infobox pages), fetch
  live `action=parse` HTML, render locally at τ=now, compare at
  structure level (headings, link sets, table shapes, image refs,
  reference counts — not byte equality), score per page, aggregate.
  This is the "accurate enough" metric; regressions fail CI-adjacent
  checks. Misrendered-construct counters (unknown tags, failed
  invokes, missing modules, wikibase misses) come along free.
- Date-picker correctness: synthesized fixture wiki with scripted
  moves/template-edits across time; assert τ-renders pick the right
  revisions and titles (this doubles as the import titles-table test).

## 7. What browsing demands of the import schema (validation checklist)

Checked against the import plan as written:

| Need | Status |
|---|---|
| Wikitext of ALL namespaces (Template/Module/MediaWiki/File) | ✓ full-history dumps carry all namespaces |
| Revision-at-τ per page without decoding text | ✓ revmeta lane is columnar; newest-first scan. If deep-history pages make τ-scans slow, add a per-page sparse (ts→rev) index later — backing-internal |
| Title→page_id at τ, including aliases/case rules | ✓ titles table; **amendment: title keys must be stored NORMALIZED** (namespace-resolved, underscores→spaces, per-namespace case rule from siteinfo) so render-time lookups are exact — normalization happens at import using the siteinfo snapshot |
| Existence-at-τ (red links, #ifexist) batched | ✓ titles table point lookups |
| siteinfo snapshots over time | ✓ planned (versioned artifact) |
| interwikimap at τ | ✓ part of siteinfo snapshot |
| System messages at τ | ✓ MediaWiki: ns is in the dumps |
| Site CSS/TemplateStyles at τ | ✓ wiki pages in the depot |
| File metadata + version history | **NEW import item**: seed KV from image.sql.gz (current versions); historical versions via prop=imageinfo on demand, cached — oldimage is NOT dumped; binaries lazy (§4) |
| Wikidata entities at τ (mw.wikibase) | ✓ structurally free — wikidatawiki as another instance; OPTIONAL |
| Category membership at τ | ✓ for direct declarations — **NEW import item**: time-indexed (page, category) transition index built during the seed/update revision scan (near-free, same pass). ✗ for template-added categories (visible only under expansion): current-only via categorylinks.sql, marked in the UI |
| Cross-instance links (interwiki to mirrored wikis) | ✓ instances table maps prefix→dbname |

Conclusion: the import schema survives contact with rendering, with
two amendments (normalized title keys; file-metadata ingestion) and
one accepted gap (historical categories). These are folded into the
import plan.

## 8. Phases

- **B1 — parser core.** Wikitext→HTML minus templates; parserTests
  subset green; serve route + per-page history view + date picker
  skeleton (resolves revisions/titles at τ, renders raw-ish).
  Already useful: a browsable time-machine wiki with ugly pages.
- **B2 — transclusion engine.** Template expansion + parser functions
  + magic words, all asof-τ; fixture-wiki date-correctness tests;
  live-diff harness stood up and scoring.
- **B3 — Scribunto.** Embedded Lua 5.1 sandbox + mw.* progressively;
  target: top-N enwiki infoboxes render. Harness scores drive the
  stdlib priority order.
- **B4 — media.** Lazy fetch + blob serve + thumbs + prefetch
  command; File version-at-τ where metadata allows.
- **B5 — polish to "browsable".** Cite/references, TemplateStyles,
  site CSS at τ, red links, interwiki (incl. cross-instance), search
  via titles table prefix match; render cache.
- **B6 — wikibase (optional).** wikidatawiki import + mw.wikibase
  wiring; harness re-score.
- **B7+ — long tail.** math, galleries, language variants,
  historical categories, Special: pages, parserTests long tail.

Ordering vs import phases: B1 needs W1-W4 (a seeded small wiki to
browse); B2+ overlap with W5-W6 freely. The fixture wiki from B2 is
shared with W5's titles tests.
