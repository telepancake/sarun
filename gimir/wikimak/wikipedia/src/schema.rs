//! sqlite DDL for `meta.db`. SPEC §"sqlite schema (sketch)".

/// All DDL statements applied at `Instance::open` time. Each statement is
/// `CREATE TABLE IF NOT EXISTS` so reopen is idempotent.
pub const META_DDL: &[&str] = &[
    // `title_id` keys the interval to the strpool title dictionary
    // (title_id_to_page.title_id) so reads resolve a title by dense id
    // — never by scanning this table's `normalized_title` BLOB (the
    // third copy of every title; kept for the write-side derivation
    // and the legacy backfill only). Import does not write the column
    // yet: schema-side triggers derive it on INSERT/retitle, and
    // `ensure_title_dictionary_schema` (instance.rs) adds + backfills
    // it on legacy dbs — the same lazy-migration discipline as the
    // revisions_seen.ts column.
    "CREATE TABLE IF NOT EXISTS title_intervals (
        page_id INTEGER NOT NULL,
        ns INTEGER NOT NULL,
        normalized_title BLOB NOT NULL,
        start_ts INTEGER NOT NULL,
        end_ts INTEGER,
        title_id INTEGER,
        PRIMARY KEY(page_id, start_ts)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS title_id_to_page (
        title_id INTEGER PRIMARY KEY,
        ns INTEGER NOT NULL,
        normalized_title BLOB NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS page_to_title_id (
        page_id INTEGER NOT NULL,
        title_id INTEGER NOT NULL,
        PRIMARY KEY(page_id, title_id)
    )",
    "CREATE TABLE IF NOT EXISTS category_intervals (
        page_id INTEGER NOT NULL,
        category BLOB NOT NULL,
        start_ts INTEGER NOT NULL,
        end_ts INTEGER,
        PRIMARY KEY(page_id, category, start_ts)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS parts_seen (
        part_filename TEXT PRIMARY KEY,
        sha256 TEXT,
        completed_at INTEGER
    )",
    "CREATE TABLE IF NOT EXISTS siteinfo_snapshots (
        captured_at INTEGER PRIMARY KEY,
        json BLOB NOT NULL
    )",
    // Interwiki map, captured alongside a siteinfo snapshot (shared
    // `captured_at`), so the τ read API can pick the map contemporaneous
    // with the site config it renders against (browsing plan §2:
    // interwikimap-at-τ). `is_local` = the prefix resolves to a wiki WE
    // mirror (a local cross-instance link); false for every external wiki.
    // Export-0.11 dumps carry no interwiki data, so in practice this table
    // is empty and asof seeds a built-in map — but the wiring is here for
    // an API/sitematrix source (import plan §1.3) that does carry one.
    "CREATE TABLE IF NOT EXISTS interwiki_map (
        captured_at INTEGER NOT NULL,
        prefix TEXT NOT NULL,
        url TEXT NOT NULL,
        is_local INTEGER NOT NULL,
        PRIMARY KEY(captured_at, prefix)
    ) WITHOUT ROWID",
    // Extension beyond SPEC sketch: per-revision dedup. SPEC §"Crash-
    // safety contract" says dedup is sqlite-driven; we materialize a
    // (page_id, rev_id) table to make idempotent re-imports cheap.
    //
    // `ts` (unix micros) is the revision's timestamp, recorded at import
    // so reads never scan a chain to find "the newest revision ≤ τ":
    // chain order is import-prepend order, not timestamp order, so the
    // argmax that used to require decoding the WHOLE chain is now one
    // indexed sqlite lookup + an early-stopping frame walk to the named
    // record. Rows written before the column existed are NULL; reads
    // fall back to the full-chain scan once and backfill (see
    // `Instance::revision_query`). The column is added to legacy dbs by
    // `ensure_revision_ts_schema` at open.
    "CREATE TABLE IF NOT EXISTS revisions_seen (
        page_id INTEGER NOT NULL,
        rev_id INTEGER NOT NULL,
        ts INTEGER,
        PRIMARY KEY(page_id, rev_id)
    ) WITHOUT ROWID",
    // Crash bookkeeping: 'dirty' = 1 between the first import write and
    // the next successful flush. On open, dirty means the last session
    // may have committed revisions_seen rows whose depot frames were
    // never flushed (power loss) — imports then verify a page's rows
    // against the CHAIN before trusting them (lazy per-page repair).
    "CREATE TABLE IF NOT EXISTS instance_flags (
        key TEXT PRIMARY KEY,
        value INTEGER NOT NULL
    )",
    // Reverse-lookup index for title pool dedup: by (ns, normalized_title).
    "CREATE INDEX IF NOT EXISTS idx_title_id_to_page_title
        ON title_id_to_page (ns, normalized_title)",
];

/// Names of the tables created by [`META_DDL`], in order. Tests use this to
/// assert the schema exists.
pub const META_TABLES: &[&str] = &[
    "title_intervals",
    "title_id_to_page",
    "page_to_title_id",
    "category_intervals",
    "parts_seen",
    "siteinfo_snapshots",
    "interwiki_map",
];
