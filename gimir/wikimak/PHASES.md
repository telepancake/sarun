# wikimak phases

Each phase is one agent-dispatchable unit of work. Definitions are tight on
purpose: the agent reads the SPEC.md files plus the phase definition and
nothing else.

## W3-Rust-1 — depot crate

**Goal**: ship `wikimak/depot` as a working crate against `SPEC.md`.

**What lands**:
- `wikimak/depot/src/` modules implementing the surface in
  `wikimak/depot/SPEC.md`. Module layout is the implementer's call.
- `wikimak/depot/tests/` with the acceptance suite (see TESTS below).
- No changes to `strpool/`, `wikimak/mediawiki/`, `wikimak/wikipedia/`, or
  any Go file.

**TESTS that must pass** (write them first, TDD-style):

The tests talk to the depot through the public API only: `open`,
`append(chain_id, new_f0, new_f1, seal_old_f1)`, `read_f0`, `read_f1`,
`cold_iter`, `flush`, `delete_all`. They are byte-payload tests: the
depot is opaque to zstd, so tests feed it arbitrary byte slices labeled
"f0", "f1", "cold". No real zstd encoding is required by these tests.

- `open_creates_layout`: open a fresh root; index file exists at size
  `max_chain_id * 8` bytes of zeros; `f0/`, `f1/`, and `cold/` dirs
  exist; `cold/cold` is an empty file; no `f0/file-*` or `f1/file-*`
  files yet.
- `first_append_no_f1`: `append(42, b"f0-bytes-A", None, false)`;
  `read_f0(42)` returns `b"f0-bytes-A"`; `read_f1(42)` returns
  `Ok(None)`; `cold_iter(42)` is empty.
- `first_append_with_some_f1_is_error`: `append(42, b"f0", Some(b"f1"),
  false)` on an empty chain returns an error.
- `second_append_writes_f1`: append A then append B with `new_f1 =
  Some(b"f1-records")`, no seal; `read_f0(42) == b"B-f0"`,
  `read_f1(42) == Some(b"f1-records")`, `cold_iter(42)` empty.
- `seal_moves_f1_bytes_to_cold_verbatim`: append A, B (gives chain an
  f1), then C with `seal_old_f1 = true` and `new_f1 = Some(b"new-f1")`;
  the previous f1's bytes appear in `cold_iter` byte-identical, as the
  first (newest) cold frame; `read_f1` returns the new f1.
- `multiple_seals_build_cold_chain_newest_first`: do four seals; assert
  `cold_iter` yields the four old-f1 byte payloads in newest-first
  order, each byte-identical to what was passed at seal time.
- `multiple_chains_independent`: drive 100 distinct chain_ids through a
  few appends each (mix of seals and non-seals); every chain's
  `read_f0`/`read_f1`/`cold_iter` returns only its own bytes.
- `flush_durability`: append + seal across several chains; `flush()`;
  drop the depot; reopen; assert `read_f0`/`read_f1`/`cold_iter` return
  the same bytes for every chain.
- `no_flush_may_lose`: append without `flush()`; drop the depot via a
  path that does not fsync (simulating crash); reopen. The depot must
  not panic and must not return corrupt bytes. It may or may not show
  the recent append — the test asserts no-panic and either-or, nothing
  stronger.
- `index_entry_is_8_bytes`: dump the raw index bytes after one append
  to chain 42; assert the file size is exactly `max_chain_id * 8`;
  assert bytes `[42*8 .. 42*8+8]` are nonzero and the rest are zero.
- `eviction_reclaims_dead_f0_space`: configure a small
  `file_size_threshold` (e.g. 64 KiB) and `eviction_dead_ratio = 0.5`.
  Append many revisions to one chain (each append deprecates the prior
  f0 in its f0 file). Drive enough turnover that an f0 file's dead
  ratio crosses 0.5. Either via an opportunistic flush trigger or via
  an implementer-exposed `maybe_evict` hook (test detects which is
  available), force eviction. Afterward: the victim f0 file is gone
  from disk, all chains are still readable byte-identical, the index
  points at the new locations.
- `eviction_reclaims_dead_f1_space`: same shape as above but for f1.
  Drive enough seals (which deprecate the old f1) that an f1 file
  crosses the dead ratio. Force eviction. Victim f1 file gone; chains
  still readable; the f0 frames' next_pointers now point at the new f1
  locations.
- `cold_file_never_evicted`: drive many appends and force several
  evictions in f0 and f1; the `cold/cold` file is still the same inode
  and still contains every cold frame ever written, byte-identical.
- `delete_all_unlinks_everything`: after a busy session, call
  `delete_all`. The root directory is empty of f0/f1/cold files and
  the index file (or all zeroed — implementer's choice, test allows
  either).
- `mid_eviction_crash_safe`: start an eviction, abort mid-walk
  (implementer must expose a test hook OR the test simulates by
  reopening before the eviction's final unlink and fsync). After
  reopen: every chain is still readable byte-identical; the depot can
  run eviction again to completion. No data loss; duplicate copies in
  the destination file are dead and get deprecated naturally.
- `frame_header_layout`: handcraft a chain's append by calling the
  public API, then read the f0 file's raw bytes; assert the first 8
  bytes are `chain_id` LE, the next 8 bytes are the `next_pointer` LE,
  the next 4 bytes are `zstd_len` LE, and the following `zstd_len`
  bytes match the payload passed to `append`. This pins the on-disk
  frame header at 20 bytes per SPEC §"Frame format".
- `cold_pointer_chain_walks_correctly`: after K seals, assert that the
  cold-frame pointers form a chain of length K: newest cold's
  next_pointer points at the next-older cold, …, the oldest cold's
  next_pointer is `(0, 0)`. The test inspects the cold file's raw
  bytes to verify this (does NOT require a public API for it).

**Non-negotiable from SPEC.md** (re-read before implementing):
- No tombstone bitmap, no sidecar, no journal, no magic, no CRC.
- Frame header is 20 bytes: `[u64 chain_id LE | u64 next_pointer LE |
  u32 zstd_len LE]`, followed by `zstd_len` opaque bytes.
- Index entry is 8 bytes: `[u32 file_id LE | u32 offset LE]`.
- One chain has exactly one f0 and zero-or-one f1, plus zero-or-more
  cold frames. Never more than one f0 or f1 per chain.
- Cold is ONE file per depot, append-only, never evicted, `unlink`'d
  whole on `delete_all`.
- Eviction migrates frames from a victim f0/f1 file to the current
  write target in the same tier. Only two pointer patches: the index
  (for f0 victims) or the f0 frame's next_pointer (for f1 victims).
- Crash recovery = trust the OS. The index flip is the atomic commit
  point of every append.
- The depot does NOT call zstd. It stores and returns opaque bytes.

**Dispatch shape**: one tester agent writes the tests; one implementer
agent writes the crate. Same two-agent pattern as Go phases. The tester
reads only `wikimak/depot/SPEC.md` and this phase definition. The
implementer reads SPEC + the tester's branch.

## W3-Rust-2 — mediawiki crate

**Goal**: ship `wikimak/mediawiki` as a working crate against
`wikimak/mediawiki/SPEC.md`. The Go package at `internal/mediawiki/`
is the behavior reference — every Rust function must match the Go
function it ports byte-for-byte on the same inputs.

**What lands**:
- `wikimak/mediawiki/src/` modules implementing the SPEC's API
  surface. Suggested split: `discover.rs`, `fetch.rs`, `bz2.rs`,
  `parser.rs`, `sha1.rs`, `types.rs`. One-file is OK if it stays
  readable.
- `wikimak/mediawiki/tests/` with the acceptance suite.
- `wikimak/mediawiki/tests/data/` — copy of the Go fixtures from
  `internal/mediawiki/testdata/` (verbatim files; no re-encoding).
- No changes to `strpool/`, `wikimak/depot/`, `wikimak/wikipedia/`,
  or any Go file.

### Fixtures

Copy these from `internal/mediawiki/testdata/` into
`wikimak/mediawiki/tests/data/`:
- `content_history_index.html` — apache directory listing
- `content_history_done.html` — `xml/bzip2/` directory listing
- `dumpstatus_done.json` — legacy `dumpstatus.json` (job done)
- `dumpstatus_in_progress.json` — legacy `dumpstatus.json` (job
  pending)
- `export_three_pages.xml`, `export_anon_and_user.xml`,
  `export_truncated.xml` — XML stream fixtures
- `small_payload.txt`, `small_payload.txt.bz2`
- `multiblock_payload.txt`, `multiblock_payload.txt.bz2`
- `multistream.bz2`, `multistream.txt`

### Tests — fixture-based (always run)

**discover** (use `wiremock` or roll a tiny `std::net::TcpListener`
server — implementer's call):
- `discover_content_history_happy_path`: serve the two index HTMLs
  and a `SHA256SUMS` listing 3 part files; `discover("votewiki")`
  returns a `Run { source: ContentHistory, date: 2025-01-15, parts:
  [3 items sorted by p<int>] }`.
- `discover_filters_incomplete_dates`: an older date has `_SUCCESS`
  but the newest date doesn't; discover picks the older one.
- `discover_falls_back_to_legacy_on_404`: content-history root
  responds 404; legacy `dumpstatus.json` is served and parsed.
- `discover_legacy_status_in_progress_skipped`: legacy
  `dumpstatus_in_progress.json` is served for the newest date and an
  older `done` exists; the older one is returned.
- `discover_part_filenames_sorted_by_page_range`: parts with names
  `*-p1p100`, `*-p2p50`, `*-p101p200` come back sorted by the first
  page-range integer, not lexicographic.

**fetch**:
- `fetch_streams_with_checksum`: serve `small_payload.txt.bz2` with
  its real SHA-256 in the `Part`. Read to EOF via `VerifyingReader`;
  bytes match; no error on drop.
- `fetch_sha256_mismatch_errors_on_eof`: same setup but corrupt the
  expected hex; reading to EOF returns an error.
- `fetch_partial_read_skips_check`: read N < total bytes, drop the
  reader; no panic, no error. (The SPEC explicitly says calling
  `into_inner()` or dropping mid-stream skips the check.)
- `fetch_uses_sha1_when_no_sha256`: a `Part` with `sha256: None,
  sha1: Some(...)` verifies against sha1 instead.

**bz2**:
- `bz2_single_block_roundtrip`: decode `small_payload.txt.bz2`;
  bytes equal `small_payload.txt`.
- `bz2_multi_block_single_stream`: decode
  `multiblock_payload.txt.bz2`; bytes equal `multiblock_payload.txt`.
  Run with `workers: 1` and `workers: 4`; both produce identical
  output.
- `bz2_multistream`: decode `multistream.bz2`; bytes equal
  `multistream.txt`. Run with `workers: 1` and `workers: 4`.
- `bz2_truncated_errors`: feed a truncated bz2 stream; reader
  surfaces an `Err`, doesn't panic.

**parser**:
- `parser_three_pages_round_trip`: feed `export_three_pages.xml`;
  iterator yields 3 `Page` records with the expected ids, titles,
  namespaces, revision counts. `site_info` returns the expected
  `SiteInfo` with namespaces map populated.
- `parser_contributor_variants`: feed `export_anon_and_user.xml`;
  assert one revision has `Contributor::Anonymous { ip }` and one
  has `Contributor::Named { username, user_id }`. Include a
  `<contributor deleted="deleted" />` case and assert
  `Contributor::Hidden` + `contributor_hidden: true`.
- `parser_hidden_text_and_comment`: a revision with `<text
  deleted="deleted" />` and `<comment deleted="deleted" />`; assert
  `text_hidden: true`, `comment_hidden: true`, text is `""`.
- `parser_suppressed_heuristic`: revision with `<text
  deleted="deleted" />` and no `bytes=`, no `sha1=` attributes →
  `suppressed: true`. With either attribute present →
  `suppressed: false`.
- `parser_truncated_returns_error`: feed `export_truncated.xml`;
  iterator yields some `Ok(Page)` records then one `Err`, then
  ends. Does not panic.

**sha1**:
- `verify_rev_sha1_matched_basic`: text whose base-36 SHA-1 matches
  the stored value → `(true, text, [])`.
- `verify_rev_sha1_no_match`: random text vs unrelated hash →
  `(false, text, vec_of_variants)` where the variants list shows
  the normalizations attempted (newline-fudge etc.).
- `verify_rev_sha1_newline_fudge`: text whose `\n` normalization
  matches → `(true, normalized, [variant_name])`.
- `verify_rev_sha1_leftpad_31`: stored sha1 is left-padded to 31
  chars when shorter; verifier accepts both padded and unpadded
  forms.

### Tests — live (gated behind `#[ignore]`; runnable via `cargo test
-- --ignored`)

One file: `wikimak/mediawiki/tests/livewiki.rs`. Hits
`https://dumps.wikimedia.org` directly.

- `live_votewiki_discover_fetch_bz2_pagestream`: the full pipeline.
  `discover("votewiki")` returns a Run; `fetch` the first (and
  usually only) part; pipe through `new_bz2_reader`; pipe through
  `new_page_stream`; assert ≥ 1 page yielded, all
  `Result<Page>::Ok`, and the stream completes without error. Also
  assert `site_info` is populated with `db_name = "votewiki"`.

### Non-negotiable from SPEC.md

- The Go crate's `wikitestdata_test.go` is the live-verification
  reference. Match its assertions where they're behavior-pinning
  (votewiki has very few pages, very small dumps — flake is low).
- `dumpsBaseURL` equivalent: a private constant
  `DUMPS_BASE_URL = "https://dumps.wikimedia.org"`. Tests override
  via a config struct or by constructing the URL externally — NOT
  by special-casing the base URL in production code.
- No retry/backoff. No local caching. No async runtime. Blocking
  reqwest only.
- Errors via `thiserror` enum. No `anyhow`.

### Dispatch shape

One tester writes the test suite + fixtures. One implementer writes
the crate. The tester reads only
`wikimak/mediawiki/SPEC.md`, this PHASES section, and
`internal/mediawiki/testdata/` (for fixtures to copy). The
implementer reads SPEC + the tester's branch + `internal/mediawiki/`
(the Go reference) for behavior parity.

## W3-Rust-3 — wikipedia crate

**Goal**: ship `wikimak/wikipedia` per `wikimak/wikipedia/SPEC.md`. This
is the domain glue tying `depot`, `mediawiki`, and `strpool` together
into a per-instance Wikipedia mirror. There is NO Go reference for this
crate — the SPEC is the only source of truth.

**What lands**:
- `wikimak/wikipedia/src/` modules implementing the SPEC's `Instance`
  surface. Suggested split: `lib.rs`, `instance.rs`, `import.rs`,
  `revision.rs` (the per-revision binary record codec), `schema.rs`
  (sqlite DDL + queries), `error.rs`. One-file is acceptable.
- `wikimak/wikipedia/tests/` with the acceptance suite.
- No changes to `strpool/`, `wikimak/depot/`, `wikimak/mediawiki/`, or
  any Go file.

### Fixtures

Reuse the mediawiki crate's test fixtures by including them via path
or by copying the few needed into `wikimak/wikipedia/tests/data/`. At
minimum copy `export_three_pages.xml` (3 pages, multiple revisions)
and `export_anon_and_user.xml` (contributor variants). No new
fixtures required.

### Per-revision record codec

SPEC §"Per-revision storage in the depot" pins the layout:
```
[ u32 schema_version | u32 flags | u64 rev_id | u64 parent_id
| u64 ts_unix_micros | u64 contributor_user_id | u8 contributor_kind
| varint contributor_len | contributor_bytes
| varint comment_len    | comment_bytes
| varint sha1_len       | sha1_bytes
| varint text_len       | text_bytes ]
```
- All multi-byte ints little-endian.
- `schema_version = 1`.
- `varint` is unsigned LEB128 (low 7 bits, MSB = continuation).
- Flags bits, lowest-to-highest:
  `0x01 TEXT_HIDDEN | 0x02 COMMENT_HIDDEN | 0x04 CONTRIBUTOR_HIDDEN |
   0x08 SUPPRESSED | 0x10 SHA1_MISMATCH`.
- `contributor_kind`: `0 Anonymous | 1 Named | 2 Hidden`. For
  Anonymous the `contributor_bytes` is the IP string; for Named it
  is the username and `contributor_user_id` is set; for Hidden both
  are zero/empty.
- `ts_unix_micros`: revision timestamp in microseconds since epoch
  (i64 fits in u64 for any plausible date).

Each frame in the depot is ONE revision record. (SPEC notes a possible
future shift to "revision batch per frame" — out of scope for this
phase.)

### Tests — fixture-based (always run)

**layout**:
- `open_creates_layout`: open fresh root; `depot/index`, `depot/f0/`,
  `depot/f1/`, `depot/cold/cold`, `titles/`, `meta.db` all exist.
  `meta.db` has the schema tables from SPEC §"sqlite schema (sketch)".
- `open_then_reopen_no_op`: open + drop + reopen; no errors, same
  layout, no extra files.

**import (fixture-driven via `PageStream`)**:
- `import_single_page_single_revision`: feed an XML byte-buffer with
  exactly one `<page>` containing one `<revision>` via
  `mediawiki::new_page_stream`. After `import`:
  - `ImportStats.pages == 1`, `revisions_new == 1`, `sha1_*` counters
    sum to 1.
  - `page_head(page_id)` returns `Some(RevisionMeta)` with the rev's
    fields matching the XML.
  - `page_history(page_id)` yields exactly one item whose text
    callback returns the original text bytes.
- `import_page_with_multiple_revisions`: a `<page>` with 5
  `<revision>` children. After import:
  - `page_head` returns the newest revision (highest rev id / latest
    timestamp).
  - `page_history` yields 5 items newest-first, each text byte-
    identical to the source XML, metadata fields match per-row.
- `import_multiple_pages_independent`: 3 pages in the stream, each
  with 2 revisions; reading any page's head/history returns only
  that page's data.
- `import_three_pages_fixture`: feed `export_three_pages.xml`
  verbatim through the pipeline; assert pages/revisions counts and
  spot-check one revision's text bytes.
- `contributor_variants_round_trip`: feed
  `export_anon_and_user.xml`; assert one page's revision has
  `Contributor::Anonymous { ip }` round-tripped through the depot
  frame back to `RevisionMeta`, another has `Contributor::Named {
  username, user_id }`.
- `hidden_and_suppressed_flags_round_trip`: hand-rolled XML with
  `<text deleted="deleted"/>`, `<comment deleted="deleted"/>`, etc;
  assert the corresponding flag bits in the round-tripped record.

**dedup**:
- `revision_dedup_on_reimport`: import the same stream twice. After
  the second import:
  - `revisions_deduped == revisions_new_first_pass`.
  - `revisions_new` for second pass == 0.
  - `page_head`/`page_history` results are identical between the
    two states.
  - The depot's f0 frame count for the affected chains is unchanged
    by the second import.

**error paths**:
- `page_id_overflow_errors_before_writes`: open Instance with
  `max_chain_id = 100`; feed a `<page id="500">`. Either `import`
  returns `Err` immediately OR the stream is consumed up to that
  page and the offending page is skipped — implementer's call,
  pin in test. EITHER WAY: no depot frame for page 500 exists,
  no meta.db row references page 500.
- `import_is_per_page_atomic_on_inner_error`: synthesize a stream
  where page 2's record fails mid-write (use a small
  `eviction_dead_ratio` and a `max_chain_id` chosen so page 2's
  chain_id exceeds it). Pages 1 and 3 (if any) are committed;
  page 2 has no depot frame, no meta.db row, no strpool title
  reservation.

**sha1 counters**:
- `sha1_counters_populated`: feed three revisions whose stored sha1
  matches text directly, fudged-newline-matches, and mismatches.
  After import: `sha1_ok == 1`, `sha1_fudged == 1`,
  `sha1_mismatch == 1`. The mismatch revision is stored with the
  `SHA1_MISMATCH` flag bit set.

**title pool**:
- `title_id_pool_stores_normalized_title`: import a single page.
  `meta.db.page_to_title_id(page_id)` returns a `title_id`. The
  strpool entry at that id decodes to the normalized title bytes.
- `title_intervals_track_renames`: import a page whose 3 revisions
  carry 2 distinct titles (the XML allows this: each revision can
  carry its own `<title>`, though export-0.11 puts title at page
  level; for this test the implementer may synthesize a stream
  with two `<page>` records sharing a page id but differing
  titles — like a real rename history — OR pin a simpler
  assertion that a single-title page yields exactly one
  `title_intervals` row with `end_ts IS NULL`). Pin which.

**durability**:
- `flush_then_reopen_round_trip`: import N pages, `instance.flush()`,
  drop the `Instance`, reopen with the same config. Every page's
  `page_head` and `page_history` matches the pre-drop state byte-
  identical.
- `unflushed_drop_may_lose_recent`: import without `flush`; drop;
  reopen. No panic, no corruption. SPEC's per-page atomicity
  contract means committed pages stay committed; uncommitted pages
  vanish cleanly. Assert: no torn pages (a page is either fully
  visible or fully absent).

### Tests — live (gated behind `#[ignore]`)

`wikimak/wikipedia/tests/livewiki.rs`:
- `live_votewiki_import_then_read_round_trip`: full pipeline
  `discover("votewiki") → fetch(part) → new_bz2_reader →
  new_page_stream → instance.import`. Then:
  - Drop the Instance.
  - Reopen at the same root.
  - For at least 3 known votewiki pages (look up by id from the
    fresh stream a second time), assert `page_head` byte-matches
    the dump's newest revision text and `page_history` count
    equals the dump's revision count per page.

This satisfies SPEC's "separate process reads ... and matches the
dump's bytes". The drop-and-reopen approximates a separate process
for I/O purposes; an actual `std::process::Command` spawn is OK but
not required.

### Non-negotiable from SPEC.md

- One frame per revision in the depot. The depot is opaque to the
  record codec.
- `chain_id = page_id as u64`. No silent remapping.
- Per-page atomicity: depot append + strpool append (if new title) +
  sqlite rows commit together. `BEGIN IMMEDIATE` per page.
- `Instance::flush` calls `depot.flush()`, sqlite checkpoint/commit,
  and `pool.flush(shard_id)` for all touched shards.
- No retry/backoff, no local cache of dump parts (the part is read
  by-stream, decoded by-stream, imported by-stream).
- No `anyhow`/`eyre`. Errors via `thiserror`.

### Dispatch shape

One tester writes the test suite + API skeleton. One implementer
writes the crate. The tester reads ONLY:
- `wikimak/wikipedia/SPEC.md`
- this PHASES section
- `wikimak/depot/SPEC.md` and `wikimak/mediawiki/SPEC.md` (for API
  signatures of the dependencies)
- `strpool/src/pool.rs` (for the `Pool` API surface)

The implementer reads the above plus the tester's branch.

## W3-Rust-4 — measurement

Run the W3 cswiki bake-off as planned in `docs/wikipedia-import-plan.md`
§2.7 — sustained write throughput, write amplification, shard counts and
sizes, GC cadence. Numbers go in `wikimak/MEASUREMENTS.md`.

After W3-Rust-4 we know whether the depot architecture works in practice
and we have a concrete starting point for the Go-vs-Rust comparison the
user asked about earlier.
