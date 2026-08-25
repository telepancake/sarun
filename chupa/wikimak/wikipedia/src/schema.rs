//! sqlite DDL for `meta.db`. SPEC §"sqlite schema (sketch)".

/// All DDL statements applied at `Instance::open` time. Each statement is
/// `CREATE TABLE IF NOT EXISTS` so reopen is idempotent.
pub const META_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS title_interval_overflow (
        title_id INTEGER NOT NULL,
        start_s INTEGER NOT NULL,
        end_s INTEGER NOT NULL,
        page_id INTEGER NOT NULL,
        PRIMARY KEY(title_id, start_s)
    ) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS idx_title_interval_overflow_lookup
        ON title_interval_overflow(title_id, start_s, end_s)",
    "CREATE TABLE IF NOT EXISTS title_slot_state (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        generation INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS title_slot_intent (
        title_id INTEGER PRIMARY KEY,
        page_id INTEGER NOT NULL,
        valid_since INTEGER NOT NULL
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS parts_seen (
        part_filename TEXT PRIMARY KEY,
        sha256 TEXT,
        completed_at INTEGER
    )",
    "CREATE TABLE IF NOT EXISTS sync_state (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS page_actions (
        source_key TEXT PRIMARY KEY,
        source_partition TEXT NOT NULL,
        event_log_id INTEGER,
        source_ordinal INTEGER NOT NULL,
        event_type TEXT NOT NULL,
        event_timestamp TEXT NOT NULL,
        event_comment TEXT NOT NULL,
        actor_id INTEGER,
        actor_name TEXT NOT NULL,
        page_id INTEGER,
        title_historical TEXT NOT NULL,
        title_current TEXT NOT NULL,
        namespace_historical INTEGER,
        namespace_current INTEGER,
        page_deleted INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_page_actions_page_time
        ON page_actions(page_id, event_timestamp DESC)",
    "CREATE TABLE IF NOT EXISTS revision_visibility (
        revision_id INTEGER PRIMARY KEY,
        page_id INTEGER,
        source_partition TEXT NOT NULL,
        deleted_parts TEXT NOT NULL,
        parts_are_suppressed INTEGER NOT NULL,
        deleted_by_page_deletion INTEGER NOT NULL,
        page_deletion_timestamp TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_revision_visibility_page
        ON revision_visibility(page_id, revision_id)",
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
    // Small instance properties (title shard count/generation).
    "CREATE TABLE IF NOT EXISTS instance_flags (
        key TEXT PRIMARY KEY,
        value INTEGER NOT NULL
    )",
];

/// Names of the tables created by [`META_DDL`], in order. Tests use this to
/// assert the schema exists.
pub const META_TABLES: &[&str] = &[
    "title_interval_overflow",
    "title_slot_state",
    "title_slot_intent",
    "parts_seen",
    "sync_state",
    "page_actions",
    "revision_visibility",
    "siteinfo_snapshots",
    "interwiki_map",
];
