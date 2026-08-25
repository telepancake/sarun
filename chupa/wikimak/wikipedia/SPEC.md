# wikimak-wikipedia — spec

Domain glue. Ties `depot` (storage), `mediawiki` (dump I/O), and `strpool`
(title bytes) into a per-instance Wikipedia mirror.

## API (sketch — pinned during W3-Rust)

```rust
pub struct Instance { /* one per dbname */ }

pub struct InstanceConfig {
    pub root: PathBuf,             // <gimir-cache>/wikimak/<dbname>/
    pub dbname: String,
    pub max_chain_id: u64,         // sized for the wiki (e.g. 100M for enwiki)
}

impl Instance {
    pub fn open(cfg: InstanceConfig) -> Result<Self>;

    /// Import one PageStream into the instance. The stream is consumed
    /// to EOF on success; partial consumption on error leaves the instance
    /// in a consistent state (per-page atomic; resume on retry).
    pub fn import(&self, stream: &mut PageStream<impl Read>) -> Result<ImportStats>;

    /// Read the current head text of a page by id. Used by the renderer.
    pub fn page_head(&self, page_id: u64) -> Result<Option<RevisionMeta>>;

    /// Iterate all revisions of a page, newest-first. Each yields
    /// metadata + a callback to fetch the text bytes lazily.
    pub fn page_history(&self, page_id: u64) -> Result<HistoryIter>;

    pub fn flush(&self) -> Result<()>;
}

pub struct ImportStats {
    pub pages: u64,
    pub revisions_new: u64,
    pub revisions_deduped: u64,
    pub sha1_ok: u64,
    pub sha1_fudged: u64,
    pub sha1_mismatch: u64,
}
```

## Layout under `root`

```
<root>/
  depot/              # wikimak_depot::Depot::open(this)
    index
    f0/  f1/  cold/
  titles/             # generation 0 (legacy-compatible)
  titles-gN/          # immutable re-sharded generations; meta.db atomically
                      # selects generation and count after dense-id remapping
    shard-NNNN
  meta.db             # rusqlite: title intervals, categories, part watermarks,
                      # siteinfo timeline, page id ↔ chain id map
```

## Page → chain mapping

The depot uses `u64 chain_id`. Wikipedia page ids are `i64`. Mapping:
`chain_id = page_id as u64`. The depot index starts at one slot and
auto-grows geometrically for observed page ids, so there is no
user-visible capacity knob. A page id at or above the
depot's 2^40 sanity ceiling (a corrupt id, not a big wiki) is rejected
LOUDLY at import time, before any write for that page. (No silent
remapping, no silent skipping.)

## Per-revision storage in the depot

One frame per revision. Frame payload is a small binary record:

```
[ u32 schema_version | u32 flags | u64 rev_id | u64 parent_id | u64 ts_unix_micros
| u64 contributor_user_id | u8 contributor_kind | varint contributor_len | contributor_bytes
| varint comment_len | comment_bytes | varint sha1_len | sha1_bytes
| varint text_len | text_bytes ]
```

Schema_version starts at 1; flags bits: TEXT_HIDDEN, COMMENT_HIDDEN,
CONTRIBUTOR_HIDDEN, SUPPRESSED, SHA1_MISMATCH. The depot sees this as
opaque bytes; the wikipedia layer is the only thing that decodes it.

After a successful first full-content import, a deterministic min-hash
sample of at most 32,768 current f0 records is read. Every selected revision
record is used in full; samples are never truncated. At least 128 records and
1 MiB are required; smaller mirrors remain plain. The sample trains one
800 KiB `revision` dictionary per instance, scaled down when the complete
sample corpus is less than eight times that size. Seed finalization is not
run by daily updates. `wikimak repack-f0` explicitly trains a successor from
the current heads and recompresses every live f0 frame.

Dictionary-compressed f0 frames carry the trained dictionary's native
zstd dictionary id; dictionary id zero means a provisional plain f0.
The immutable dictionary bytes live under
`dictionaries/<lane>-<dict_id>.zdict` and must be fsynced before a
referencing depot head is committed; `<lane>.current` is an atomic,
fsynced pointer selecting the dictionary for future heads. f1/cold
context is known from the depot tier and must carry dictionary id zero
because those frames use refPrefix. Missing ids and dictionary-bearing
history frames are hard errors. No extra per-frame envelope or generic
depot-format bump is needed: the depot deliberately treats payloads as
opaque.

The dictionary is persisted and activated before heads are repacked. Each
f0 replacement preserves its exact next pointer, so f1/cold bytes are never
rewritten. A crash may leave a valid mixture of plain and dictionary heads;
reopening decodes both, and rerunning seed finalization resumes the remaining
heads with the already-active dictionary rather than training another.

A FRESH page is collected page-at-a-time and sorted by immutable
revision id. The highest id lands alone in f0; every older record lands
newest-first in exactly one cold frame. Fresh construction never creates
f1. On update, identical ids deduplicate against the authoritative depot
chain. A strictly newer prefix takes the prepend path; interleaved/older
additions are streaming-merged by revision id and installed with the
depot's expected-pointer atomic replacement.

If an existing revision id arrives with different complete record
bytes, the stored archival record remains canonical. The incoming bytes
are retained as a tagged, self-delimiting correction occurrence in the
separate `corrections/` depot (one logical append-only chain per page).
No conflict blob or revision timestamp is duplicated in SQLite.

## sqlite schema (sketch)

```
title_interval_overflow(title_id INTEGER, start_s INTEGER,
                        end_s INTEGER, page_id INTEGER,
                        PRIMARY KEY(title_id, start_s)) WITHOUT ROWID;
title_slot_state(singleton INTEGER PRIMARY KEY, generation INTEGER);
title_slot_intent(title_id INTEGER PRIMARY KEY,
                  page_id INTEGER, valid_since INTEGER);
parts_seen(part_filename TEXT PRIMARY KEY, sha256 TEXT, completed_at INTEGER);
siteinfo_snapshots(captured_at INTEGER PRIMARY KEY, json BLOB);
```

`title_id` is the strpool id for a normalized, namespace-qualified title.
`title-slots.N` stores one eight-byte `{page_id, valid_since_s}` current
binding per title id; page id zero is unbound. `page-titles.N` is the
eight-byte reverse current mapping (`title_id + 1`, zero absent). Only
closed intervals older than the current binding live in SQLite.
Current-binding intents are durable row-log upserts. Writer reads overlay
them immediately; every 4,096 pending title ids and each clean/salvage
flush applies the whole set to both flat files with one paired fsync,
then clears the rows. Writer open replays rows idempotently; read-only
open rejects an unreplayed set.

## Crash-safety contract

- Inherits the depot's contract (write → fsync → flip → fsync).
- sqlite gives us its own transaction durability.
- strpool gives us its `flush()` contract.
- `Instance::flush` calls `depot.flush()`, sqlite WAL checkpoint or commit
  boundary, and `pool.flush(shard_id)` for all shards.
- Revision content is per-page atomic at the depot index flip. A crash
  before a fresh/replacement flip leaves the prior chain (or emptiness)
  visible; a retry re-merges from that authoritative chain. Title
  metadata changes first enter a durable, idempotent redo set. Bounded
  batches update and fsync both flat directions, then clear the redo rows.
  Writer open replays a surviving batch before exposing the instance.

## Out of scope (for now)

- Rendering (lives in a future `wikimak-render` crate).
- Search (likewise).
- Incremental dump catch-up (W6 work).
- mediawiki_history TSV ingest (W5 work).
