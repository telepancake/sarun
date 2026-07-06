//! v1 store compatibility: reader + one-shot migration to v2.
//!
//! v1 layout: `<store>/chain` (flat refPrefix frame file, newest-first,
//! `[u32 raw_len | u32 zstd_len | zstd]*`, frame 0 standalone, frame i
//! anchored on the previous commit's canonical full-view bytes) +
//! bookkeeping in `meta.sqlite` (kv schema=1, refs with deleted_at,
//! reflog, commits by pos) or, older still, `meta.json`.
//!
//! Migration (any open of a v1 store): read the whole v1 store, rebuild
//! it as v2 (depot chains + schema=2 sqlite, v1 reflog timestamps
//! preserved, upstream-deleted v1 refs stay deleted — their prune rows
//! are already in the reflog), then remove the v1 chain and bookkeeping.
//! The v2 sqlite is built at a temp name and renamed over the v1 one
//! last, so an interrupted migration re-runs from the intact v1 store.

use std::io::Write as _;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::{CommitMeta, Error, Meta, RefMeta, Result};

use super::{sql_err, ReflogRecord, Store};

fn compress(src: &[u8], prefix: Option<&[u8]>, level: i32) -> Result<Vec<u8>> {
    super::compress(src, prefix, level)
}

fn decompress_v1(src: &[u8], prefix: Option<&[u8]>, raw_len: usize) -> Result<Vec<u8>> {
    let mut dctx = zstd::zstd_safe::DCtx::create();
    if let Some(p) = prefix {
        dctx.ref_prefix(p)
            .map_err(|c| Error::Chain(zstd::zstd_safe::get_error_name(c).to_string()))?;
    }
    let mut out = Vec::with_capacity(raw_len);
    dctx.decompress(&mut out, src)
        .map_err(|c| Error::Chain(zstd::zstd_safe::get_error_name(c).to_string()))?;
    if out.len() != raw_len {
        return Err(Error::Chain(format!(
            "frame decompressed to {} bytes, expected {raw_len}",
            out.len()
        )));
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct V1ReflogRow {
    pub at: i64,
    pub refname: String,
    pub old_sha: Option<String>,
    pub new_sha: Option<String>,
    pub note: String,
}

fn read_legacy_json(store: &Path) -> Result<Meta> {
    let json = std::fs::read_to_string(store.join("meta.json"))?;
    serde_json::from_str(&json).map_err(|e| Error::Meta(e.to_string()))
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

/// Full v1 bookkeeping: meta (live refs, commits newest-first) + reflog.
fn read_v1_meta(store: &Path) -> Result<(Meta, Vec<V1ReflogRow>)> {
    let db = store.join("meta.sqlite");
    if !db.exists() {
        return Ok((read_legacy_json(store)?, Vec::new()));
    }
    let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(sql_err)?;
    let kv = |key: &str| -> Result<String> {
        Ok(conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(sql_err)?
            .unwrap_or_default())
    };
    let label = kv("label")?;
    let url = kv("url")?;
    let refs = {
        let mut stmt = conn
            .prepare("SELECT name, sha FROM refs WHERE deleted_at IS NULL ORDER BY name")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| Ok(RefMeta { name: r.get(0)?, sha: r.get(1)? }))
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        rows
    };
    let commits = {
        let mut stmt = conn
            .prepare(
                "SELECT sha, parents, author, committer, message, extra_headers, raw
                 FROM commits ORDER BY pos ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], row_commit)
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        rows
    };
    let reflog = {
        let mut stmt = conn
            .prepare("SELECT at, ref, old_sha, new_sha, note FROM reflog ORDER BY seq")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(V1ReflogRow {
                    at: r.get(0)?,
                    refname: r.get(1)?,
                    old_sha: r.get(2)?,
                    new_sha: r.get(3)?,
                    note: r.get(4)?,
                })
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        rows
    };
    Ok((Meta { label, url, refs, commits }, reflog))
}

/// Decode the v1 flat chain into views, newest-first (v1 read_store).
fn read_v1_views(store: &Path, n_commits: usize) -> Result<Vec<depot::View>> {
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
        let record = decompress_v1(&buf[pos..pos + zlen], prev_full.as_deref(), raw_len)?;
        pos += zlen;
        let layer = depot::codec::decode(&record)?;
        let view = depot::apply(views.last(), &layer)
            .ok_or_else(|| Error::Chain(format!("frame {} resolves to nothing", views.len())))?;
        prev_full = Some(depot::codec::encode(&depot::diff(None, Some(&view))));
        views.push(view);
    }
    if views.len() != n_commits {
        return Err(Error::Chain(format!(
            "{} frames but {} commits in meta",
            views.len(),
            n_commits
        )));
    }
    Ok(views)
}

/// Rebuild a v1 store as v2 in place.
pub(crate) fn migrate(store: &Path) -> Result<()> {
    let (meta, v1_reflog) = read_v1_meta(store)?;
    let views = read_v1_views(store, meta.commits.len())?;

    // Wipe leftovers of an interrupted previous attempt.
    let tmp_db = store.join("meta.sqlite.v2");
    let depot_dir = store.join("depot");
    if depot_dir.exists() {
        std::fs::remove_dir_all(&depot_dir)?;
    }
    for f in ["meta.sqlite.v2", "meta.sqlite.v2-wal", "meta.sqlite.v2-shm"] {
        let p = store.join(f);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }

    std::fs::create_dir_all(&depot_dir)?;
    let depot = super::open_depot(store)?;
    let conn = Connection::open(&tmp_db).map_err(sql_err)?;
    super::configure(&conn)?;
    conn.execute_batch(super::SCHEMA).map_err(sql_err)?;
    {
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        for (k, v) in [
            ("schema", "2"),
            ("label", meta.label.as_str()),
            ("url", meta.url.as_str()),
            ("n_trees", "0"),
            ("n_commits", "0"),
            ("n_reflog", "0"),
        ] {
            super::kv_set(&tx, k, v)?;
        }
        tx.commit().map_err(sql_err)?;
    }
    let mut st = Store { depot, conn, root: store.to_path_buf() };
    {
        let mut ingest = super::Ingest::new(&mut st, 3)?;
        for (cm, view) in meta.commits.iter().rev().zip(views.iter().rev()) {
            let full = depot::codec::encode(&depot::diff(None, Some(view)));
            ingest.add_commit(cm, view, &full)?;
        }
        // Reflog: v1 rows verbatim, shas mapped to indices. A sha the
        // store never held (e.g. a rewrite note about a retired copy)
        // loses that side's index but keeps the row.
        let mut recs = Vec::with_capacity(v1_reflog.len().max(meta.refs.len()));
        if v1_reflog.is_empty() {
            // Oldest (json) stores had no reflog: refs become creations.
            let at = super::now_secs();
            for (i, r) in meta.refs.iter().enumerate() {
                let row = ingest.ref_row_pub(&r.sha)?;
                recs.push(ReflogRecord {
                    idx: i as u64,
                    at,
                    refname: r.name.clone(),
                    old: None,
                    new: Some(row),
                    note: String::new(),
                });
            }
        } else {
            for (i, r) in v1_reflog.iter().enumerate() {
                let row_of = |sha: &Option<String>| -> Option<(u64, u64)> {
                    sha.as_deref().and_then(|s| ingest.ref_row_pub(s).ok())
                };
                recs.push(ReflogRecord {
                    idx: i as u64,
                    at: r.at,
                    refname: r.refname.clone(),
                    old: row_of(&r.old_sha),
                    new: row_of(&r.new_sha),
                    note: r.note.clone(),
                });
            }
        }
        let ref_rows = meta
            .refs
            .iter()
            .map(|r| {
                let (c, t) = ingest.ref_row_pub(&r.sha)?;
                Ok((r.name.clone(), c, t))
            })
            .collect::<Result<Vec<_>>>()?;
        ingest.finish_migration(recs, ref_rows)?;
    }
    drop(st);

    // Swap the bookkeeping: close v1 handles (none open here), clear its
    // WAL sidecars, rename v2 over it, then drop the v1 chain + json.
    for f in ["meta.sqlite-wal", "meta.sqlite-shm"] {
        let p = store.join(f);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    std::fs::rename(&tmp_db, store.join("meta.sqlite"))?;
    for f in ["meta.sqlite.v2-wal", "meta.sqlite.v2-shm", "chain", "meta.json"] {
        let p = store.join(f);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    Ok(())
}

/// TEST-ONLY: regress a v2 store into a v1-format fixture (flat chain +
/// meta.json) at `dst` — input for the migration test.
#[doc(hidden)]
pub fn write_v1_from_v2_for_tests(src_v2: &Path, dst: &Path, level: i32) -> Result<()> {
    let meta = super::read_meta(src_v2)?;
    let st = Store::open(src_v2)?;
    let recs = st.commit_records()?;
    let views_nf = st.tree_views(None)?;
    let n_trees = st.count(super::TREES)? as usize;
    let mut views = Vec::with_capacity(recs.len());
    for r in recs.iter().rev() {
        views.push(views_nf[n_trees - 1 - r.tree_idx as usize].clone());
    }
    write_v1_store_for_tests(dst, &meta, &views, level)
}

/// TEST-ONLY: mint a v1-format store (flat chain + meta.json) from
/// commit metas and resolved views (both newest-first) — fixtures for
/// the migration test. Not reachable from any production path.
#[doc(hidden)]
pub fn write_v1_store_for_tests(
    store: &Path,
    meta: &Meta,
    views: &[depot::View],
    level: i32,
) -> Result<()> {
    std::fs::create_dir_all(store)?;
    let mut chain = Vec::new();
    let mut prev_full: Option<Vec<u8>> = None;
    for (i, view) in views.iter().enumerate() {
        let full = depot::codec::encode(&depot::diff(None, Some(view)));
        let rec = match (i, views.get(i.wrapping_sub(1))) {
            (0, _) => full.clone(),
            (_, Some(newer)) => depot::codec::encode(&depot::diff(Some(newer), Some(view))),
            _ => unreachable!(),
        };
        let frame = compress(&rec, prev_full.as_deref(), level)?;
        chain.extend_from_slice(&(rec.len() as u32).to_le_bytes());
        chain.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        chain.extend_from_slice(&frame);
        prev_full = Some(full);
    }
    let mut f = std::fs::File::create(store.join("chain"))?;
    f.write_all(&chain)?;
    f.sync_all()?;
    let json = serde_json::to_string_pretty(meta).map_err(|e| Error::Meta(e.to_string()))?;
    std::fs::write(store.join("meta.json"), json)?;
    Ok(())
}
