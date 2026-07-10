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
  titles/             # strpool::Pool::open(this); shard count tuned per wiki
    shard-NNNN
  meta.db             # rusqlite: title intervals, categories, part watermarks,
                      # siteinfo timeline, page id ↔ chain id map
```

## Page → chain mapping

The depot uses `u64 chain_id`. Wikipedia page ids are `i64`. Mapping:
`chain_id = page_id as u64`. `max_chain_id` is only the fresh index's
size hint: the depot's sparse index auto-grows for page ids beyond it,
so there is no user-visible capacity knob. A page id at or above the
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

Subsequent design pass may switch from "one frame per revision" to
"frame per revision batch" once we measure the per-frame overhead on real
data. The depot doesn't care.

A FRESH page (empty chain — the bulk-import common case) is built
FORWARD (depot SPEC §"Bulk forward construction"): the dump's
oldest-first revisions stream through in ingest-RAM-bound batches, each
full batch landing as ONE cold frame written ONCE (the batch's newest
record is excluded — it is the frame's refPrefix anchor and carries into
the next batch as its oldest record, reproducing the newest-first read
walk's anchor invariant in dump order), and the final tail lands as
f0/f1 at the commit (the depot index flip). History write amplification
is 1.0, measured in the forward_build tests. A page whose chain already
exists (update mode) takes the prepend path.

## sqlite schema (sketch)

```
title_intervals(page_id INTEGER, ns INTEGER, normalized_title BLOB,
                start_ts INTEGER, end_ts INTEGER /* NULL = open */,
                PRIMARY KEY(page_id, start_ts)) WITHOUT ROWID;
title_id_to_page(title_id INTEGER PRIMARY KEY, ns INTEGER, normalized_title BLOB);
page_to_title_id(page_id INTEGER, title_id INTEGER, PRIMARY KEY(page_id, title_id));
category_intervals(page_id INTEGER, category BLOB,
                   start_ts INTEGER, end_ts INTEGER) WITHOUT ROWID;
parts_seen(part_filename TEXT PRIMARY KEY, sha256 TEXT, completed_at INTEGER);
siteinfo_snapshots(captured_at INTEGER PRIMARY KEY, json BLOB);
```

`title_id` is the strpool id for a normalized title. The renderer uses it
everywhere instead of the title bytes.

## Crash-safety contract

- Inherits the depot's contract (write → fsync → flip → fsync).
- sqlite gives us its own transaction durability.
- strpool gives us its `flush()` contract.
- `Instance::flush` calls `depot.flush()`, sqlite WAL checkpoint or commit
  boundary, and `pool.flush(shard_id)` for all shards.
- Import is per-page atomic: one transaction per page that ties together
  the depot append, the strpool title append (if title is new), and the
  sqlite inserts. We use sqlite's `BEGIN IMMEDIATE` per page; on commit
  the page is visible end-to-end. On crash without commit, the page is
  not visible (depot frame bytes may be present but the index points at
  the old f0, sqlite row absent).

## Out of scope (for now)

- Rendering (lives in a future `wikimak-render` crate).
- Search (likewise).
- Incremental dump catch-up (W6 work).
- mediawiki_history TSV ingest (W5 work).
