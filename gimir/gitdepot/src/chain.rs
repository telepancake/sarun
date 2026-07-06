//! The refPrefix chain store: frames newest-first, each older frame
//! zstd-compressed with the next-newer record as `ZSTD_CCtx_refPrefix` —
//! the tiered-VBF anchoring discipline applied to whole tree-layers.
//!
//! `<store>/meta.sqlite` — refs + reflog + commit metadata (bookkeeping),\n//!                         WAL.
//! `<store>/chain`       — frames newest-first:
//!                         `[u32 raw_len LE | u32 zstd_len LE | zstd bytes]*`
//!
//! A pre-sqlite store carries `<store>/meta.json` instead — readable as
//! legacy, converted to sqlite (and the json removed) on the first
//! write. sqlite is THE format; json is read-only compatibility.
//!
//! The chain file has no magic, no version, no checksum — same division
//! of labor as the VBF design (integrity is the storage/transport
//! layer's job).

use std::io::Write;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::{CommitMeta, Error, Meta, RefMeta, Result, SizeReport};

// ------------------------------------------------------------ meta.sqlite

/// `commits.pos` is NOT the frame index. A prepend must cost O(new), so
/// new rows take keys BELOW the current minimum (newest = smallest key)
/// and no existing row is ever renumbered. Frame index i (0 = newest) =
/// `pos - MIN(pos)`; newest-first order = `ORDER BY pos ASC`.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS kv(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS refs(
    name       TEXT PRIMARY KEY,
    sha        TEXT NOT NULL,
    -- Upstream-pruned refs are MARKED, never dropped: local history is
    -- never destroyed by a remote deletion. NULL = live.
    deleted_at INTEGER
);
-- Append-only observation log of every ref movement (creation,
-- fast-forward, rewrite, upstream prune). Rows are never updated or
-- deleted; old_sha NULL = ref created, new_sha NULL = ref deleted
-- upstream. `at` is the local observation time (unix secs).
CREATE TABLE IF NOT EXISTS reflog(
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    at      INTEGER NOT NULL,
    ref     TEXT NOT NULL,
    old_sha TEXT,
    new_sha TEXT,
    note    TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS commits(
    pos           INTEGER PRIMARY KEY,
    sha           TEXT NOT NULL UNIQUE,
    parents       TEXT NOT NULL,
    author        BLOB NOT NULL,
    committer     BLOB NOT NULL,
    message       BLOB NOT NULL,
    extra_headers TEXT NOT NULL DEFAULT '',
    raw           BLOB
);
";

fn sql_err(e: rusqlite::Error) -> Error {
    Error::Meta(e.to_string())
}

fn db_path(store: &Path) -> std::path::PathBuf {
    store.join("meta.sqlite")
}

fn json_path(store: &Path) -> std::path::PathBuf {
    store.join("meta.json")
}

/// True when `store` holds bookkeeping in either format — "is there a
/// store here?".
pub fn store_exists(store: &Path) -> bool {
    db_path(store).exists() || json_path(store).exists()
}

fn open_db(store: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path(store),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .map_err(sql_err)?;
    configure(&conn)?;
    Ok(conn)
}

fn create_db(store: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path(store)).map_err(sql_err)?;
    configure(&conn)?;
    conn.execute_batch(SCHEMA).map_err(sql_err)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    // sha-prefix resolution goes through LIKE; git shas are
    // case-sensitive, and case-sensitive LIKE can use the sha index.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;\n\
         PRAGMA case_sensitive_like=ON;",
    )
    .map_err(sql_err)
}

fn read_legacy(store: &Path) -> Result<Meta> {
    let json = std::fs::read_to_string(json_path(store))?;
    serde_json::from_str(&json).map_err(|e| Error::Meta(e.to_string()))
}

/// Open the store's bookkeeping for WRITING: sqlite if present, else
/// one-time conversion from legacy meta.json (json removed on success).
fn ensure_db(store: &Path) -> Result<Connection> {
    if db_path(store).exists() {
        return open_db(store);
    }
    let legacy = read_legacy(store)?;
    write_meta(store, &legacy)?;
    open_db(store)
}

fn dehex(s: &str) -> Result<Vec<u8>> {
    hex::decode(s).map_err(|e| Error::Meta(e.to_string()))
}

fn insert_commit(conn: &Connection, pos: i64, cm: &CommitMeta) -> Result<()> {
    conn.execute(
        "INSERT INTO commits(pos, sha, parents, author, committer, message,
                             extra_headers, raw)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            pos,
            cm.sha,
            cm.parents.join(" "),
            dehex(&cm.author_hex)?,
            dehex(&cm.committer_hex)?,
            dehex(&cm.message_hex)?,
            cm.extra_headers.join("\n"),
            if cm.raw_hex.is_empty() { None } else { Some(dehex(&cm.raw_hex)?) },
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn row_commit(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommitMeta> {
    let parents: String = row.get("parents")?;
    let extra: String = row.get("extra_headers")?;
    let author: Vec<u8> = row.get("author")?;
    let committer: Vec<u8> = row.get("committer")?;
    let message: Vec<u8> = row.get("message")?;
    let raw: Option<Vec<u8>> = row.get("raw")?;
    Ok(CommitMeta {
        sha: row.get("sha")?,
        parents: parents.split(' ').filter(|s| !s.is_empty()).map(str::to_string).collect(),
        author_hex: hex::encode(author),
        committer_hex: hex::encode(committer),
        message_hex: hex::encode(message),
        extra_headers: extra.split('\n').filter(|s| !s.is_empty()).map(str::to_string).collect(),
        raw_hex: raw.map(hex::encode).unwrap_or_default(),
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn log_ref(
    conn: &Connection,
    at: i64,
    name: &str,
    old_sha: Option<&str>,
    new_sha: Option<&str>,
    note: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO reflog(at, ref, old_sha, new_sha, note)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![at, name, old_sha, new_sha, note],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Reconcile the refs table with the OBSERVED upstream refs, writing a
/// reflog row per movement. Refs missing upstream are marked
/// (deleted_at), never dropped — the commits they pin stay in the
/// store. `note` annotates moved-ref rows (e.g. "" for fast-forward).
fn apply_refs(conn: &Connection, refs: &[RefMeta], note: &str) -> Result<()> {
    let at = now_secs();
    let mut existing: std::collections::HashMap<String, (String, bool)> = {
        let mut stmt = conn
            .prepare("SELECT name, sha, deleted_at IS NOT NULL FROM refs")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))
            .map_err(sql_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql_err)?;
        rows
    };
    for r in refs {
        match existing.remove(&r.name) {
            Some((old, false)) if old == r.sha => {}
            Some((old, false)) => {
                conn.execute(
                    "UPDATE refs SET sha = ?2 WHERE name = ?1",
                    rusqlite::params![r.name, r.sha],
                )
                .map_err(sql_err)?;
                log_ref(conn, at, &r.name, Some(&old), Some(&r.sha), note)?;
            }
            // New, or resurrected after an upstream prune: a creation.
            _ => {
                conn.execute(
                    "INSERT OR REPLACE INTO refs(name, sha, deleted_at)
                     VALUES (?1, ?2, NULL)",
                    rusqlite::params![r.name, r.sha],
                )
                .map_err(sql_err)?;
                log_ref(conn, at, &r.name, None, Some(&r.sha), note)?;
            }
        }
    }
    for (name, (old, deleted)) in existing {
        if deleted {
            continue;
        }
        conn.execute(
            "UPDATE refs SET deleted_at = ?2 WHERE name = ?1",
            rusqlite::params![name, at],
        )
        .map_err(sql_err)?;
        log_ref(conn, at, &name, Some(&old), None, "pruned upstream")?;
    }
    Ok(())
}

fn kv_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
        .optional()
        .map_err(sql_err)
}

fn kv_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO kv(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Full bookkeeping load — O(all commits). Only for code paths that
/// genuinely need every commit (export, log); the attach/list hot paths
/// use the point accessors below.
pub fn read_meta(store: &Path) -> Result<Meta> {
    if !db_path(store).exists() {
        return read_legacy(store);
    }
    let conn = open_db(store)?;
    let label = kv_get(&conn, "label")?.unwrap_or_default();
    let url = kv_get(&conn, "url")?.unwrap_or_default();
    let refs = refs_of(&conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT sha, parents, author, committer, message, extra_headers, raw
             FROM commits ORDER BY pos ASC",
        )
        .map_err(sql_err)?;
    let commits = stmt
        .query_map([], row_commit)
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err)?;
    Ok(Meta { label, url, refs, commits })
}

/// Rewrite the WHOLE bookkeeping (import, legacy conversion). Positions
/// restart at 0..n; any legacy meta.json is removed after the commit.
pub fn write_meta(store: &Path, meta: &Meta) -> Result<()> {
    let mut conn = create_db(store)?;
    let tx = conn.transaction().map_err(sql_err)?;
    tx.execute("DELETE FROM commits", []).map_err(sql_err)?;
    tx.execute("DELETE FROM kv", []).map_err(sql_err)?;
    kv_set(&tx, "schema", "1")?;
    kv_set(&tx, "label", &meta.label)?;
    kv_set(&tx, "url", &meta.url)?;
    apply_refs(&tx, &meta.refs, "")?;
    for (i, cm) in meta.commits.iter().enumerate() {
        insert_commit(&tx, i as i64, cm)?;
    }
    tx.commit().map_err(sql_err)?;
    let legacy = json_path(store);
    if legacy.exists() {
        std::fs::remove_file(legacy)?;
    }
    Ok(())
}

/// Replace-all refs — O(refs), commits untouched (the k==0 update path).
pub fn write_refs(store: &Path, refs: &[RefMeta]) -> Result<()> {
    let mut conn = ensure_db(store)?;
    let tx = conn.transaction().map_err(sql_err)?;
    apply_refs(&tx, refs, "")?;
    tx.commit().map_err(sql_err)
}

/// (label, url) — kv point reads, no commit materialization.
pub fn identity(store: &Path) -> Result<(String, String)> {
    if !db_path(store).exists() {
        let m = read_legacy(store)?;
        return Ok((m.label, m.url));
    }
    let conn = open_db(store)?;
    Ok((
        kv_get(&conn, "label")?.unwrap_or_default(),
        kv_get(&conn, "url")?.unwrap_or_default(),
    ))
}

pub fn set_identity(store: &Path, label: &str, url: &str) -> Result<()> {
    let mut conn = ensure_db(store)?;
    let tx = conn.transaction().map_err(sql_err)?;
    kv_set(&tx, "label", label)?;
    kv_set(&tx, "url", url)?;
    tx.commit().map_err(sql_err)
}

/// The store's human label ("WHICH git?").
pub fn label(store: &Path) -> Result<String> {
    Ok(identity(store)?.0)
}

fn refs_of(conn: &Connection) -> Result<Vec<RefMeta>> {
    let mut stmt = conn
        .prepare("SELECT name, sha FROM refs WHERE deleted_at IS NULL ORDER BY name")
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| Ok(RefMeta { name: r.get(0)?, sha: r.get(1)? }))
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err);
    rows
}

/// LIVE refs (upstream-pruned ones excluded), name-ordered (the\n/// `for-each-ref` order) — O(refs).
pub fn refs(store: &Path) -> Result<Vec<RefMeta>> {
    if !db_path(store).exists() {
        return Ok(read_legacy(store)?.refs);
    }
    refs_of(&open_db(store)?)
}

pub fn commit_count(store: &Path) -> Result<usize> {
    if !db_path(store).exists() {
        return Ok(read_legacy(store)?.commits.len());
    }
    let conn = open_db(store)?;
    conn.query_row("SELECT COUNT(*) FROM commits", [], |r| r.get::<_, i64>(0))
        .map(|n| n as usize)
        .map_err(sql_err)
}

/// Commit shas newest-first (frame order) — the update() suffix check.
pub fn commit_shas(store: &Path) -> Result<Vec<String>> {
    if !db_path(store).exists() {
        return Ok(read_legacy(store)?.commits.into_iter().map(|c| c.sha).collect());
    }
    let conn = open_db(store)?;
    let mut stmt = conn
        .prepare("SELECT sha FROM commits ORDER BY pos ASC")
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| r.get(0))
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err);
    rows
}

/// The commit at frame index `idx` (0 = newest) — one point lookup.
pub fn commit_at(store: &Path, idx: usize) -> Result<CommitMeta> {
    if !db_path(store).exists() {
        let m = read_legacy(store)?;
        return m
            .commits
            .into_iter()
            .nth(idx)
            .ok_or_else(|| Error::Meta(format!("no commit at index {idx}")));
    }
    let conn = open_db(store)?;
    conn.query_row(
        "SELECT sha, parents, author, committer, message, extra_headers, raw
         FROM commits WHERE pos = (SELECT MIN(pos) FROM commits) + ?1",
        [idx as i64],
        row_commit,
    )
    .optional()
    .map_err(sql_err)?
    .ok_or_else(|| Error::Meta(format!("no commit at index {idx}")))
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Resolve REF — a ref name (`main` matches `refs/heads/main`, tags
/// likewise, or the full refname) or a unique commit-sha prefix (ANY
/// commit in the chain is addressable, not just the tips) — to
/// `(sha, frame index)`. `Ok(None)` = nothing matched; an ambiguous sha
/// prefix or a ref pointing outside the chain is an error. Point
/// lookups only — no commit-list materialization.
pub fn resolve_ref(store: &Path, refname: &str) -> Result<Option<(String, usize)>> {
    if !db_path(store).exists() {
        return resolve_ref_legacy(&read_legacy(store)?, refname);
    }
    let conn = open_db(store)?;
    // Name order = the for-each-ref order the Vec-based resolver
    // scanned in (refs/heads before refs/tags).
    let hit: Option<String> = conn
        .query_row(
            "SELECT sha FROM refs
             WHERE deleted_at IS NULL
               AND (name = ?1 OR name = 'refs/heads/' || ?1
                    OR name = 'refs/tags/' || ?1)
             ORDER BY name LIMIT 1",
            [refname],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let min_pos = |conn: &Connection, sha: &str| -> Result<Option<i64>> {
        conn.query_row(
            "SELECT pos - (SELECT MIN(pos) FROM commits) FROM commits WHERE sha = ?1",
            [sha],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)
    };
    if let Some(sha) = hit {
        return match min_pos(&conn, &sha)? {
            Some(idx) => Ok(Some((sha, idx as usize))),
            None => Err(Error::Meta("ref target not in chain".into())),
        };
    }
    let mut stmt = conn
        .prepare(
            "SELECT sha, pos - (SELECT MIN(pos) FROM commits)
             FROM commits WHERE sha LIKE ?1 ESCAPE '\\' LIMIT 2",
        )
        .map_err(sql_err)?;
    let hits = stmt
        .query_map([format!("{}%", like_escape(refname))], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err)?;
    match hits.as_slice() {
        [(sha, idx)] => Ok(Some((sha.clone(), *idx as usize))),
        [] => Ok(None),
        _ => Err(Error::Meta(format!("commit prefix {refname} is ambiguous"))),
    }
}

fn resolve_ref_legacy(meta: &Meta, refname: &str) -> Result<Option<(String, usize)>> {
    if let Some(r) = meta.refs.iter().find(|r| {
        r.name == refname
            || r.name.strip_prefix("refs/heads/") == Some(refname)
            || r.name.strip_prefix("refs/tags/") == Some(refname)
    }) {
        return match meta.commits.iter().position(|c| c.sha == r.sha) {
            Some(idx) => Ok(Some((r.sha.clone(), idx))),
            None => Err(Error::Meta("ref target not in chain".into())),
        };
    }
    let hits: Vec<(String, usize)> = meta
        .commits
        .iter()
        .enumerate()
        .filter(|(_, c)| c.sha.starts_with(refname))
        .map(|(i, c)| (c.sha.clone(), i))
        .collect();
    match hits.as_slice() {
        [one] => Ok(Some(one.clone())),
        [] => Ok(None),
        _ => Err(Error::Meta(format!("commit prefix {refname} is ambiguous"))),
    }
}

/// One reflog observation. `old_sha` NULL = ref created; `new_sha`
/// NULL = ref deleted upstream.
#[derive(Debug, Clone)]
pub struct ReflogEntry {
    pub at: i64,
    pub refname: String,
    pub old_sha: Option<String>,
    pub new_sha: Option<String>,
    pub note: String,
}

/// The full reflog, oldest observation first. Legacy json stores have
/// no reflog (empty).
pub fn reflog(store: &Path) -> Result<Vec<ReflogEntry>> {
    if !db_path(store).exists() {
        return Ok(Vec::new());
    }
    let conn = open_db(store)?;
    let mut stmt = conn
        .prepare("SELECT at, ref, old_sha, new_sha, note FROM reflog ORDER BY seq")
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ReflogEntry {
                at: r.get(0)?,
                refname: r.get(1)?,
                old_sha: r.get(2)?,
                new_sha: r.get(3)?,
                note: r.get(4)?,
            })
        })
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err);
    rows
}

/// After a non-fast-forward re-import replaced the store, log what the
/// rewrite did to each ref the OLD store had (the import itself logged
/// the new refs as creations). The note names the retired store so the
/// pre-rewrite history stays findable.
pub fn log_rewrite(store: &Path, old_refs: &[RefMeta], retired: &Path) -> Result<()> {
    let mut conn = ensure_db(store)?;
    let tx = conn.transaction().map_err(sql_err)?;
    let at = now_secs();
    let note = format!("rewrite; previous store retired to {}", retired.display());
    for r in old_refs {
        let new_sha: Option<String> = tx
            .query_row(
                "SELECT sha FROM refs WHERE name = ?1 AND deleted_at IS NULL",
                [&r.name],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match new_sha {
            Some(ref n) if *n == r.sha => {}
            Some(n) => log_ref(&tx, at, &r.name, Some(&r.sha), Some(&n), &note)?,
            None => log_ref(&tx, at, &r.name, Some(&r.sha), None, "pruned upstream")?,
        }
    }
    tx.commit().map_err(sql_err)
}

// ------------------------------------------------------------ zstd chain

fn zstd_err(code: zstd::zstd_safe::ErrorCode) -> Error {
    Error::Chain(zstd::zstd_safe::get_error_name(code).to_string())
}

/// Compress `src`, optionally anchored on `prefix` (the next-newer
/// record). A fresh CCtx per frame: refPrefix is consumed by one
/// compression, and correctness beats context reuse in a straightedge.
fn compress(src: &[u8], prefix: Option<&[u8]>, level: i32) -> Result<Vec<u8>> {
    let mut cctx = zstd::zstd_safe::CCtx::create();
    cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))
        .map_err(zstd_err)?;
    if let Some(p) = prefix {
        cctx.ref_prefix(p).map_err(zstd_err)?;
    }
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(src.len()));
    cctx.compress2(&mut out, src).map_err(zstd_err)?;
    Ok(out)
}

fn decompress(src: &[u8], prefix: Option<&[u8]>, raw_len: usize) -> Result<Vec<u8>> {
    let mut dctx = zstd::zstd_safe::DCtx::create();
    if let Some(p) = prefix {
        dctx.ref_prefix(p).map_err(zstd_err)?;
    }
    let mut out = Vec::with_capacity(raw_len);
    dctx.decompress(&mut out, src).map_err(zstd_err)?;
    if out.len() != raw_len {
        return Err(Error::Chain(format!(
            "frame decompressed to {} bytes, expected {raw_len}",
            out.len()
        )));
    }
    Ok(out)
}

/// Encode records (newest-first) as a refPrefix chain: frame 0
/// standalone, frame i anchored on record i-1.
fn chain_bytes(records: &[Vec<u8>], level: i32) -> Result<Vec<u8>> {
    let mut chain = Vec::new();
    for (i, rec) in records.iter().enumerate() {
        let prefix = if i == 0 { None } else { Some(records[i - 1].as_slice()) };
        let frame = compress(rec, prefix, level)?;
        chain.extend_from_slice(&(rec.len() as u32).to_le_bytes());
        chain.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        chain.extend_from_slice(&frame);
    }
    Ok(chain)
}

fn standalone_total(records: &[Vec<u8>], level: i32) -> Result<u64> {
    let mut total = 0u64;
    for rec in records {
        total += compress(rec, None, level)?.len() as u64;
    }
    Ok(total)
}

fn solid_total(records: &[Vec<u8>], level: i32) -> Result<u64> {
    let mut concat = Vec::new();
    for rec in records {
        concat.extend_from_slice(rec);
    }
    Ok(compress(&concat, None, level)?.len() as u64)
}

/// The stored (view-anchored) chain: frame 0 = the newest full record
/// standalone; frame i = delta record i compressed with the previous
/// commit's FULL record — the canonical bytes of its view — as
/// refPrefix. The decoder recomputes that anchor from the reconstructed
/// view via `diff(None, view)`; both sides go through the one canonical
/// encoding, whose bit-exactness is load-bearing.
fn view_chain_bytes(
    delta_records: &[Vec<u8>],
    full_records: &[Vec<u8>],
    level: i32,
) -> Result<Vec<u8>> {
    let mut chain = Vec::new();
    for (i, rec) in delta_records.iter().enumerate() {
        let prefix = if i == 0 { None } else { Some(full_records[i - 1].as_slice()) };
        let frame = compress(rec, prefix, level)?;
        chain.extend_from_slice(&(rec.len() as u32).to_le_bytes());
        chain.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        chain.extend_from_slice(&frame);
    }
    Ok(chain)
}

/// Write the store (the view-anchored chain is the rest form) and
/// produce the encoding comparison over both record families.
pub fn write_store(
    store: &Path,
    meta: &Meta,
    delta_records: &[Vec<u8>],
    full_records: &[Vec<u8>],
    level: i32,
    report: bool,
) -> Result<Option<SizeReport>> {
    std::fs::create_dir_all(store)?;
    let chain_path = store.join("chain");
    if store_exists(store) || chain_path.exists() {
        return Err(Error::Chain(format!("store {} already populated", store.display())));
    }

    let view_chain = view_chain_bytes(delta_records, full_records, level)?;

    let mut f = std::fs::File::create(&chain_path)?;
    f.write_all(&view_chain)?;
    f.sync_all()?;
    write_meta(store, meta)?;

    if !report {
        return Ok(None);
    }
    Ok(Some(SizeReport {
        commits: delta_records.len(),
        zstd_level: level,
        full_raw: full_records.iter().map(|r| r.len() as u64).sum(),
        full_standalone: standalone_total(full_records, level)?,
        full_ref_chain: chain_bytes(full_records, level)?.len() as u64,
        delta_raw: delta_records.iter().map(|r| r.len() as u64).sum(),
        delta_standalone: standalone_total(delta_records, level)?,
        delta_ref_chain: chain_bytes(delta_records, level)?.len() as u64,
        view_ref_chain: view_chain.len() as u64,
        solid_full: solid_total(full_records, level)?,
    }))
}

/// The newest record (frame 0 — standalone by construction) without
/// walking the rest of the chain.
pub fn read_head_record(store: &Path) -> Result<Vec<u8>> {
    let buf = std::fs::read(store.join("chain"))?;
    let (raw_len, zlen) = frame_header(&buf, 0)?;
    decompress(&buf[8..8 + zlen], None, raw_len)
}

fn frame_header(buf: &[u8], pos: usize) -> Result<(usize, usize)> {
    if buf.len() - pos < 8 {
        return Err(Error::Chain("truncated frame header".into()));
    }
    let raw_len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
    let zlen = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
    if buf.len() - pos - 8 < zlen {
        return Err(Error::Chain("truncated frame body".into()));
    }
    Ok((raw_len, zlen))
}

/// Prepend `k` new commits to the front of the chain (the incremental
/// append — MIRRORS.md phase 3). `delta_records` is `k+1` records
/// newest-first: the new commits' deltas plus, last, the BRIDGE delta
/// that rebuilds the former head view from the oldest new view.
/// `full_records` is the `k` new commits' full records; new frame `i`
/// is anchored on `full_records[i-1]`, frame 0 standalone — so the
/// former frame 0 (the old head's standalone full record) is REPLACED
/// by the bridge frame, and every remaining old frame keeps its anchor
/// (the old full records are unchanged) and is copied verbatim.
///
/// Bookkeeping cost is O(new): the `k` commit rows are INSERTed at keys
/// below the current MIN(pos) (see the schema comment) and refs are
/// replaced — no existing commit row is touched.
pub fn prepend_store(
    store: &Path,
    new_commits: &[CommitMeta],
    refs: &[RefMeta],
    delta_records: &[Vec<u8>],
    full_records: &[Vec<u8>],
    level: i32,
) -> Result<()> {
    if delta_records.len() != full_records.len() + 1
        || new_commits.len() != full_records.len()
    {
        return Err(Error::Chain("prepend: need k+1 delta records for k commits".into()));
    }
    let old = std::fs::read(store.join("chain"))?;
    let (_, zlen) = frame_header(&old, 0)?;
    let tail = &old[8 + zlen..];

    let mut chain = view_chain_bytes(delta_records, full_records, level)?;
    chain.extend_from_slice(tail);

    // Chain first (tmp + rename), sqlite txn second — a crash between
    // the two leaves a chain longer than the commits table, which
    // read_store rejects loudly rather than serving a half-update.
    let chain_tmp = store.join("chain.tmp");
    let mut f = std::fs::File::create(&chain_tmp)?;
    f.write_all(&chain)?;
    f.sync_all()?;
    std::fs::rename(&chain_tmp, store.join("chain"))?;

    let mut conn = ensure_db(store)?;
    let tx = conn.transaction().map_err(sql_err)?;
    let min: i64 = tx
        .query_row("SELECT MIN(pos) FROM commits", [], |r| r.get(0))
        .map_err(sql_err)?;
    let k = new_commits.len() as i64;
    for (i, cm) in new_commits.iter().enumerate() {
        insert_commit(&tx, min - k + i as i64, cm)?;
    }
    apply_refs(&tx, refs, "")?;
    tx.commit().map_err(sql_err)
}

/// Read the store back: meta + the reconstructed VIEWS, newest-first.
/// Each frame's refPrefix anchor is recomputed from the previous view's
/// canonical full record.
pub fn read_store(store: &Path) -> Result<(Meta, Vec<depot::View>)> {
    let meta = read_meta(store)?;

    let buf = std::fs::read(store.join("chain"))?;
    let mut views: Vec<depot::View> = Vec::new();
    let mut prev_full: Option<Vec<u8>> = None;
    let mut pos = 0usize;
    while pos < buf.len() {
        if buf.len() - pos < 8 {
            return Err(Error::Chain("truncated frame header".into()));
        }
        let raw_len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        let zlen = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        if buf.len() - pos < zlen {
            return Err(Error::Chain("truncated frame body".into()));
        }
        let record = decompress(&buf[pos..pos + zlen], prev_full.as_deref(), raw_len)?;
        pos += zlen;

        let layer = depot::codec::decode(&record)?;
        let view = depot::apply(views.last(), &layer).ok_or_else(|| {
            Error::Chain(format!("frame {} resolves to nothing", views.len()))
        })?;
        prev_full = Some(depot::codec::encode(&depot::diff(None, Some(&view))));
        views.push(view);
    }
    if views.len() != meta.commits.len() {
        return Err(Error::Chain(format!(
            "{} frames but {} commits in meta",
            views.len(),
            meta.commits.len()
        )));
    }
    Ok((meta, views))
}
