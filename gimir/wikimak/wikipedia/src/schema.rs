//! sqlite DDL for `meta.db`. SPEC §"sqlite schema (sketch)".

/// All DDL statements applied at `Instance::open` time. Each statement is
/// `CREATE TABLE IF NOT EXISTS` so reopen is idempotent.
pub const META_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS title_intervals (
        page_id INTEGER NOT NULL,
        ns INTEGER NOT NULL,
        normalized_title BLOB NOT NULL,
        start_ts INTEGER NOT NULL,
        end_ts INTEGER,
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
    // Extension beyond SPEC sketch: per-revision dedup. SPEC §"Crash-
    // safety contract" says dedup is sqlite-driven; we materialize a
    // (page_id, rev_id) table to make idempotent re-imports cheap.
    "CREATE TABLE IF NOT EXISTS revisions_seen (
        page_id INTEGER NOT NULL,
        rev_id INTEGER NOT NULL,
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
];
