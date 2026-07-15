// Review queries and mutations over a box's on-disk sqlar. Public action
// implementations construct the generated closed relation types; the
// temporary listener owns the remaining legacy JSON projection.

use crate::depot::BoxDepot;
use std::ffi::CStr;
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::path::PathBuf;

use crate::hostfs;

use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::OptionalExtension;
use rusqlite::params;
use similar::DiffTag;
use similar::TextDiff;

use crate::depot::blob_path;
use crate::paths;

fn sqlar_path(id: i64) -> PathBuf {
    paths::state_home().join(format!("{id}.sqlar"))
}

fn open_ro(id: i64) -> Option<Connection> {
    Connection::open_with_flags(sqlar_path(id), OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

const S_IFMT: u32 = 0o170000;
const S_IFCHR: u32 = 0o020000;
const S_IFLNK: u32 = 0o120000;

fn relation_bytes<const MAXIMUM: usize>(
    value: String,
    field: &str,
) -> Result<crate::wire::BoundedBytes<MAXIMUM>, String> {
    crate::wire::BoundedBytes::new(value.into_bytes())
        .map_err(|error| format!("{field} exceeds relation bound: {error:?}"))
}

fn relation_text<const MAXIMUM: usize>(
    value: String,
    field: &str,
) -> Result<crate::wire::BoundedText<MAXIMUM>, String> {
    crate::wire::BoundedText::new(value)
        .map_err(|error| format!("{field} exceeds relation bound: {error:?}"))
}

fn relation_list<T>(
    values: Vec<T>,
    field: &str,
) -> Result<crate::wire::BoundedVec<
    T, 0, { crate::generated_wire::LIMIT_COLLECTION_ITEMS },
>, String> {
    crate::wire::BoundedVec::new(values)
        .map_err(|error| format!("{field} exceeds relation bound: {error:?}"))
}

pub fn session_changes_typed(id: i64)
    -> Result<Vec<crate::generated_wire::ChangeRow>, String>
{
    use crate::generated_wire::{ChangeKind, ChangeRow};
    let conn = Connection::open_with_flags(
        sqlar_path(id), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    let mut statement = conn.prepare(
        "SELECT name,mode,sz FROM sqlar ORDER BY name")
        .map_err(|error| error.to_string())?;
    let rows = statement.query_map([], |row| Ok((
        row.get::<_, String>(0)?,
        row.get::<_, i64>(1)?,
        row.get::<_, i64>(2)?,
    ))).map_err(|error| error.to_string())?;
    rows.map(|row| {
        let (name, mode, size) = row.map_err(|error| error.to_string())?;
        let mode = u32::try_from(mode).map_err(|_| "stored file mode exceeds u32")?;
        let kind = if mode & S_IFMT == S_IFCHR {
            ChangeKind::Deleted
        } else if mode & S_IFMT == S_IFLNK {
            ChangeKind::Symlink
        } else {
            ChangeKind::Changed
        };
        Ok(ChangeRow {
            path: crate::wire::BoundedBytes::new(name.into_bytes())
                .map_err(|error| format!("change path exceeds relation bound: {error:?}"))?,
            kind,
            size: u64::try_from(size).map_err(|_| "negative stored change size")?,
        })
    }).collect()
}

/// The box's current bytes for `rel`: symlink target (raw in the row) or the
/// pool blob for a file. None if the row is missing or a tombstone.
fn current_bytes(id: i64, rel: &str) -> Option<Vec<u8>> {
    let conn = open_ro(id)?;
    let n = crate::depot::archive_node(&conn, rel)?;
    let (rowid, mode, data) = (n.rowid, n.mode, n.data);
    if mode & S_IFMT == S_IFCHR {
        return None; // tombstone
    }
    if let Some(d) = data {
        return Some(d); // symlink target (raw) or any inline row
    }
    std::fs::read(blob_path(id, rowid)).ok()
}

fn lower_bytes(rel: &str) -> Vec<u8> {
    let p = Path::new("/").join(rel);
    match std::fs::symlink_metadata(&p) {
        Ok(m) if !m.is_dir() => std::fs::read(&p).unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub fn current_mode(id: i64, rel: &str) -> Option<u32> {
    let conn = open_ro(id)?;
    crate::depot::archive_mode(&conn, rel)
}

fn diff_line(style: &str, text: impl Into<String>)
    -> Result<crate::generated_wire::DiffLine, String>
{
    Ok(crate::generated_wire::DiffLine {
        style: crate::wire::BoundedText::new(style.to_owned())
            .map_err(|error| format!("diff style exceeds relation bound: {error:?}"))?,
        text: crate::wire::BoundedText::new(text.into())
            .map_err(|error| format!("diff line exceeds relation bound: {error:?}"))?,
    })
}

pub fn hunks_typed(id: i64, rel: &str)
    -> Result<crate::generated_wire::FileDiff, String>
{
    use crate::generated_wire::{ChangeKind, DiffHunk, FileDiff};
    let rel = rel.trim_start_matches('/');
    let Some(mode) = current_mode(id, rel) else {
        return Ok(FileDiff::Unavailable {
            message: crate::wire::BoundedText::new("gone".to_owned())
                .map_err(|error| format!("diff error exceeds relation bound: {error:?}"))?,
        });
    };
    if mode & S_IFMT == S_IFCHR {
        return Ok(FileDiff::Deleted);
    }
    let host = Path::new("/").join(rel);
    if mode & S_IFMT == S_IFLNK {
        let target = crate::wire::BoundedBytes::new(current_bytes(id, rel).unwrap_or_default())
            .map_err(|error| format!("symlink target exceeds relation bound: {error:?}"))?;
        let kind = if host.symlink_metadata().is_ok() {
            ChangeKind::Modified
        } else {
            ChangeKind::Created
        };
        return Ok(FileDiff::Symlink { kind, target });
    }
    let cur = current_bytes(id, rel).unwrap_or_default();
    let low = lower_bytes(rel);
    let text = !cur.contains(&0) && !low.contains(&0)
        && std::str::from_utf8(&cur).is_ok() && std::str::from_utf8(&low).is_ok();
    if !text {
        let modified = host.exists();
        let kind = if modified { ChangeKind::Modified } else { ChangeKind::Created };
        let content = crate::wire::BoundedBytes::new(cur)
            .map_err(|error| format!("binary diff content exceeds relation bound: {error:?}"))?;
        let content_before = if modified && !low.is_empty() {
            Some(crate::wire::BoundedBytes::new(low).map_err(|error| format!(
                "binary base content exceeds relation bound: {error:?}"))?)
        } else {
            None
        };
        return Ok(FileDiff::Binary { kind, content, content_before });
    }
    // text: grouped unified diff, lines tagged like _build_hunks_display.
    let lo = String::from_utf8(low).map_err(|_| "invalid UTF-8 base diff")?;
    let cu = String::from_utf8(cur).map_err(|_| "invalid UTF-8 current diff")?;
    let diff = TextDiff::from_lines(&lo, &cu);
    let ll: Vec<&str> = diff.iter_old_slices().map(|s| s.trim_end_matches(['\r', '\n'])).collect();
    let ul: Vec<&str> = diff.iter_new_slices().map(|s| s.trim_end_matches(['\r', '\n'])).collect();
    let mut hunks = Vec::new();
    for (gi, group) in diff.grouped_ops(3).iter().enumerate() {
        if group.is_empty() { continue; }
        let (_, a0, _) = group[0].as_tag_tuple();
        let (_, alast, blast) = group[group.len() - 1].as_tag_tuple();
        let (_, _, b0) = group[0].as_tag_tuple();
        let mut lines = vec![diff_line("hdr",
            format!("@@ -{},{} +{},{} @@", a0.start + 1, alast.end - a0.start,
                    b0.start + 1, blast.end - b0.start))?];
        for op in group {
            let (tag, orange, nrange) = op.as_tag_tuple();
            match tag {
                DiffTag::Equal => for k in orange { lines.push(diff_line(" ", ll[k])?); },
                _ => {
                    for k in orange { lines.push(diff_line("-", ll[k])?); }
                    for k in nrange { lines.push(diff_line("+", ul[k])?); }
                }
            }
        }
        hunks.push(DiffHunk {
            index: u32::try_from(gi).map_err(|_| "diff hunk index exceeds u32")?,
            lines: crate::wire::BoundedVec::new(lines)
                .map_err(|error| format!("diff lines exceed relation bound: {error:?}"))?,
        });
    }
    Ok(FileDiff::Text {
        hunks: crate::wire::BoundedVec::new(hunks)
            .map_err(|error| format!("diff hunks exceed relation bound: {error:?}"))?,
    })
}

/// Current content of one box path as the box sees it: the captured write
/// when `rel` is in the change set, else the host file underneath. Feeds
/// the UI's document reader ('V' on Changes). Fails loudly for tombstones,
/// symlinks, and paths that exist nowhere.
pub fn file_bytes_typed(id: i64, rel: &str) -> Result<Vec<u8>, String> {
    let rel = rel.trim_start_matches('/');
    match current_mode(id, rel) {
        Some(mode) if mode & S_IFMT == S_IFCHR => {
            Err("deleted in box".into())
        }
        Some(mode) if mode & S_IFMT == S_IFLNK => {
            Err("symlink, not a document".into())
        }
        Some(_) => match current_bytes(id, rel) {
            Some(current) => Ok(current),
            None => Err("content unavailable".into()),
        },
        None => {
            let low = lower_bytes(rel);
            if low.is_empty() && !Path::new("/").join(rel).is_file() {
                Err("no such file".into())
            } else {
                Ok(low)
            }
        }
    }
}

/// The write counterpart of `file_bytes`: overwrite one box path's CURRENT
/// bytes — the editor pane's save path ('E' on Changes → Ctrl-S). The write
/// goes through `Overlay::box_write_file`, i.e. the SAME copy_up → pool
/// blob → finalize_file path the box's own FUSE writes take, so the
/// captured row is indistinguishable from a box write and the mount serves
/// it back (a live box's RAM mirror is updated by those same primitives;
/// an at-rest box is hydrated on demand). The host is never touched.
/// Fails loudly ({ok:false, error}) for tombstones, symlinks, directories,
/// binary content (either side), and paths that exist nowhere.
/// Shared guard-and-write core for the two box-file write verbs:
/// `review.write_file` (the editor save, `allow_create = false`) and
/// `box_file_write` (the oaita agent's file-write tool, `allow_create =
/// true`). Both run the IDENTICAL refusal gate — tombstone / symlink /
/// directory / binary-in-either-direction / NUL — and both stage the write
/// through `Overlay::box_write_file` (the SAME copy_up → pool blob →
/// finalize_file path a box's own FUSE writes take; the host is never
/// touched). They differ ONLY in the one documented axis below: whether a
/// path that exists NOWHERE is created (the agent authors new files) or
/// refused (the editor can't save a file it never opened). A discard-hunk
/// inline row needs no special handling here anymore — copy_up
/// re-materializes it for every writer (`BoxDepot::outline_inline_row`).
pub fn write_file_checked_typed(
    id: i64,
    rel: &str,
    bytes: &[u8],
    ov: &crate::overlay::Overlay,
    allow_create: bool,
) -> Result<u64, String> {
    let rel = rel.trim_start_matches('/');
    write_file_guard(id, rel, bytes, allow_create)?;
    ov.box_write_file(id, rel, bytes)
        .map_err(|error| format!("write {rel}: {error}"))?;
    u64::try_from(bytes.len()).map_err(|_| "written content length exceeds u64".into())
}

/// The refusal gate shared by both write verbs, separated so it is
/// unit-testable against sqlar fixtures without an Overlay. Same taxonomy as
/// `file_bytes`, plus the binary guard in both directions: neither write verb
/// may write NULs nor silently replace captured/host binary. `allow_create`
/// is the ONLY behavioral difference between the two callers (see
/// `write_file_checked`): when true a path that exists nowhere passes (the
/// agent creates a new file in the box's upper); when false it is refused.
fn write_file_guard(id: i64, rel: &str, bytes: &[u8], allow_create: bool)
    -> Result<(), String>
{
    if rel.is_empty() {
        return Err("empty path".into());
    }
    if bytes.contains(&0) {
        return Err("binary content (NUL) refused — this is a text edit verb".into());
    }
    match current_mode(id, rel) {
        Some(m) if m & S_IFMT == S_IFCHR => Err("deleted in box".into()),
        Some(m) if m & S_IFMT == S_IFLNK => Err("symlink, not editable".into()),
        Some(m) if m & S_IFMT == 0o040000 => Err("directory, not editable".into()),
        Some(m) if m & S_IFMT != 0o100000 => {
            Err(format!("not a regular file (mode {:o})", m & S_IFMT))
        }
        Some(_) => match current_bytes(id, rel) {
            Some(cur) if cur.contains(&0) => {
                Err("captured content is binary; refusing a text overwrite".into())
            }
            Some(_) => Ok(()),
            None => Err("captured content unavailable".into()),
        },
        None => {
            // Not in the change set: the edit shadows a HOST file.
            let host = Path::new("/").join(rel);
            match std::fs::symlink_metadata(&host) {
                Ok(md) => {
                    if md.file_type().is_symlink() {
                        return Err("host symlink, not editable".into());
                    }
                    if !md.is_file() {
                        return Err("host path is not a regular file".into());
                    }
                    if lower_bytes(rel).contains(&0) {
                        return Err("host content is binary; refusing a text overwrite".into());
                    }
                    Ok(())
                }
                // Exists nowhere: the editor refuses (it opens existing
                // files); the agent file-write tool creates it.
                Err(_) if allow_create => Ok(()),
                Err(_) => Err(
                    "no such file (neither captured nor on the host)".into()),
            }
        }
    }
}

/// st_mtime_ns stored for `rel` in the box's sqlar, or None.
pub fn current_mtime(id: i64, rel: &str) -> Option<i64> {
    let conn = open_ro(id)?;
    crate::depot::archive_mtime(&conn, rel)
}

/// Mirror of Python ChangeReview.decorate: per-row lazy decoration for ONE
/// changed entry — {is_text, stale, kind}. is_text = NUL-pairwise text rule,
/// stale = host mtime newer than the stored mtime, kind refined to
/// created/modified/deleted via a single host lstat.
/// Decorate a batch of paths in one go (one RPC, one server-side host stat
/// loop). Used by the UI to decorate a window of changes-pane rows without
/// paying a round-trip per row.
pub fn decorate_many_typed(id: i64, rels: &[&str])
    -> Result<Vec<crate::generated_wire::ChangeDecoration>, String>
{
    rels.iter().map(|rel| decorate_typed(id, rel)).collect()
}

/// Newest-first slice of the box's change set — the source feed for a live
/// box's "recently changed" panel in the boxes view. Sorted by sqlar.mtime
/// desc, capped at `limit`. Returns the same row shape as session_changes
/// so the UI can reuse the same render path.
pub fn recent_changes_typed(
    id: i64,
    limit: u64,
) -> Result<Vec<crate::generated_wire::ChangeRow>, String> {
    use crate::generated_wire::{ChangeKind, ChangeRow};
    let conn = Connection::open_with_flags(
        sqlar_path(id), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    let limit = i64::try_from(limit).map_err(|_| "change limit exceeds SQLite range")?;
    crate::depot::archive_recent(&conn, limit).map_err(|error| error.to_string())?
        .into_iter().map(
        |(name, mode, size, _)| {
            let mode = u32::try_from(mode).map_err(|_| "stored file mode exceeds u32")?;
            let kind = if mode & S_IFMT == S_IFCHR {
                ChangeKind::Deleted
            } else if mode & S_IFMT == S_IFLNK {
                ChangeKind::Symlink
            } else {
                ChangeKind::Changed
            };
            Ok(ChangeRow {
                path: crate::wire::BoundedBytes::new(name.into_bytes())
                    .map_err(|error| format!("change path exceeds relation bound: {error:?}"))?,
                kind,
                size: u64::try_from(size).map_err(|_| "negative stored change size")?,
            })
        },
    ).collect()
}

/// Five-list bundle for the Sessions-view right pane: newest-first
/// previews of each kind, capped at `limit` per kind. One RPC per
/// session-switch instead of five. xattr modifications ride in the
/// changes list as their own rows (kind="xattr"), tagged with the file
/// they hang off + the xattr key — they were invisible before, now
/// they aren't.
pub fn box_summary_typed(
    id: i64,
    limit: u64,
) -> Result<crate::generated_wire::BoxSummary, String> {
    use crate::generated_wire::{
        BoxSummary, ChangeKind, ChangePreview, EchoStream, EdgePreview, FailureKind,
        FailurePreview, OutputPreview, PipelinePreview, ProcessPreview,
    };
    let conn = Connection::open_with_flags(
        sqlar_path(id), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    let sql_limit = i64::try_from(limit).map_err(|_| "summary limit exceeds SQLite range")?;

    let mut changes = crate::depot::archive_recent(&conn, sql_limit)
        .map_err(|error| error.to_string())?.into_iter().map(
            |(path, mode, size, modified_at)| {
                let mode = u32::try_from(mode)
                    .map_err(|_| "stored file mode exceeds u32")?;
                Ok((modified_at, ChangePreview {
                    path: relation_bytes(path, "summary change path")?,
                    kind: if mode & S_IFMT == S_IFCHR { ChangeKind::Deleted }
                        else if mode & S_IFMT == S_IFLNK { ChangeKind::Symlink }
                        else { ChangeKind::Changed },
                    size: u64::try_from(size)
                        .map_err(|_| "negative summary change size")?,
                    modified_at,
                    xattr_key: None,
                    xattr_length: None,
                }))
            },
        ).collect::<Result<Vec<_>, String>>()?;
    if has_table_typed(&conn, "xattr")? {
        let mut statement = conn.prepare(
            "SELECT x.name, x.key, length(x.value), COALESCE(s.mtime, 0) \
             FROM xattr x LEFT JOIN sqlar s ON s.name=x.name \
             ORDER BY COALESCE(s.mtime, 0) DESC LIMIT ?1")
            .map_err(|error| error.to_string())?;
        let rows = statement.query_map([sql_limit], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?, row.get::<_, i64>(3)?,
        ))).map_err(|error| error.to_string())?;
        for row in rows {
            let (path, key, length, modified_at) = row.map_err(|error| error.to_string())?;
            changes.push((modified_at, ChangePreview {
                path: relation_bytes(path, "summary xattr path")?,
                kind: ChangeKind::Xattr,
                size: 0,
                modified_at,
                xattr_key: Some(relation_bytes(key, "summary xattr key")?),
                xattr_length: Some(u64::try_from(length)
                    .map_err(|_| "negative summary xattr length")?),
            }));
        }
    }
    changes.sort_by(|left, right| right.0.cmp(&left.0));
    let changes = changes.into_iter().take(limit as usize)
        .map(|(_, row)| row).collect();

    let mut statement = conn.prepare(
        "SELECT id, ts, stream, length(content), CAST(substr(content,1,80) AS TEXT) \
         FROM outputs ORDER BY id DESC LIMIT ?1")
        .map_err(|error| error.to_string())?;
    let rows = statement.query_map([sql_limit], |row| Ok((
        row.get::<_, i64>(0)?, row.get::<_, f64>(1)?, row.get::<_, i64>(2)?,
        row.get::<_, i64>(3)?, row.get::<_, Option<String>>(4)?,
    ))).map_err(|error| error.to_string())?;
    let mut outputs = Vec::new();
    for row in rows {
        let (id, time, stream, length, preview) = row.map_err(|error| error.to_string())?;
        outputs.push(OutputPreview {
            id: u64::try_from(id).map_err(|_| "negative output row id")?,
            time,
            stream: match stream {
                0 => EchoStream::Stdout,
                1 => EchoStream::Stderr,
                _ => return Err(format!("unknown stored output stream {stream}")),
            },
            length: u64::try_from(length).map_err(|_| "negative output length")?,
            preview: relation_text(preview.unwrap_or_default(), "output preview")?,
        });
    }
    drop(statement);

    let mut statement = conn.prepare(
        "SELECT id, tgid, exe, argv FROM process ORDER BY id DESC LIMIT ?1")
        .map_err(|error| error.to_string())?;
    let rows = statement.query_map([sql_limit], |row| Ok((
        row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?,
        row.get::<_, String>(2)?, row.get::<_, String>(3)?,
    ))).map_err(|error| error.to_string())?;
    let mut processes = Vec::new();
    for row in rows {
        let (id, tgid, executable, argv) = row.map_err(|error| error.to_string())?;
        let argv: Vec<String> = serde_json::from_str(&argv)
            .map_err(|error| format!("invalid stored process argv: {error}"))?;
        processes.push(ProcessPreview {
            id: u64::try_from(id).map_err(|_| "negative process row id")?,
            tgid: tgid.map(|value| u32::try_from(value)
                .map_err(|_| "process tgid exceeds u32")).transpose()?,
            executable: relation_bytes(executable, "process executable")?,
            argv0: relation_bytes(argv.into_iter().next().unwrap_or_default(), "process argv0")?,
        });
    }
    drop(statement);

    let mut pipelines = Vec::new();
    if has_table_typed(&conn, "brushprov")? {
        let mut statement = conn.prepare(
            "SELECT id, cmd, COALESCE(nested,0) FROM brushprov ORDER BY id DESC LIMIT ?1")
            .map_err(|error| error.to_string())?;
        let rows = statement.query_map([sql_limit], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?,
        ))).map_err(|error| error.to_string())?;
        for row in rows {
            let (id, command, nested) = row.map_err(|error| error.to_string())?;
            pipelines.push(PipelinePreview {
                id: u64::try_from(id).map_err(|_| "negative pipeline row id")?,
                command: relation_text(command, "pipeline command")?,
                nested: nested != 0,
            });
        }
    }

    let mut edges = Vec::new();
    if has_table_typed(&conn, "build_edges")? {
        let mut statement = conn.prepare(
            "SELECT id, outs, cmd FROM build_edges ORDER BY id DESC LIMIT ?1")
            .map_err(|error| error.to_string())?;
        let rows = statement.query_map([sql_limit], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))).map_err(|error| error.to_string())?;
        for row in rows {
            let (id, outputs, command) = row.map_err(|error| error.to_string())?;
            let outputs: Vec<String> = serde_json::from_str(&outputs)
                .map_err(|error| format!("invalid stored build outputs: {error}"))?;
            edges.push(EdgePreview {
                id: u64::try_from(id).map_err(|_| "negative build edge row id")?,
                output: outputs.first().cloned()
                    .map(|value| relation_bytes(value, "build edge output")).transpose()?,
                output_count: u32::try_from(outputs.len())
                    .map_err(|_| "build edge output count exceeds u32")?,
                command: command.map(|value| relation_text(value, "build edge command"))
                    .transpose()?,
            });
        }
    }

    let mut failures = Vec::new();
    if has_table_typed(&conn, "build_edges")? {
        let mut statement = conn.prepare(
            "SELECT json_extract(outs,'$[0]'), exit_code, COALESCE(output_excerpt,'') \
             FROM build_edges WHERE exit_code IS NOT NULL AND exit_code != 0 \
             ORDER BY id DESC LIMIT ?1")
            .map_err(|error| error.to_string())?;
        let rows = statement.query_map([sql_limit], |row| Ok((
            row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))).map_err(|error| error.to_string())?;
        for row in rows {
            let (label, code, excerpt) = row.map_err(|error| error.to_string())?;
            failures.push(FailurePreview {
                kind: FailureKind::Edge,
                label: relation_text(label.unwrap_or_default(), "edge failure label")?,
                code: i32::try_from(code).map_err(|_| "edge exit code exceeds i32")?,
                excerpt: relation_text(excerpt, "edge failure excerpt")?,
            });
        }
    }
    if has_table_typed(&conn, "brushprov")? {
        let mut statement = conn.prepare(
            "SELECT cmd, exit_code FROM brushprov WHERE exit_code > 0 \
             ORDER BY id DESC LIMIT ?1")
            .map_err(|error| error.to_string())?;
        let rows = statement.query_map([sql_limit], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, i64>(1)?,
        ))).map_err(|error| error.to_string())?;
        for row in rows {
            let (label, code) = row.map_err(|error| error.to_string())?;
            failures.push(FailurePreview {
                kind: FailureKind::Pipeline,
                label: relation_text(label, "pipeline failure label")?,
                code: i32::try_from(code).map_err(|_| "pipeline exit code exceeds i32")?,
                excerpt: relation_text(String::new(), "pipeline failure excerpt")?,
            });
        }
    }
    failures.truncate(limit as usize);

    let has_rows = |table: &str, predicate: &str| -> Result<bool, String> {
        if !has_table_typed(&conn, table)? { return Ok(false); }
        let sql = format!("SELECT 1 FROM {table} WHERE {predicate} LIMIT 1");
        conn.query_row(&sql, [], |_| Ok(())).optional()
            .map(|row| row.is_some()).map_err(|error| error.to_string())
    };
    Ok(BoxSummary {
        outputs: relation_list(outputs, "summary outputs")?,
        changes: relation_list(changes, "summary changes")?,
        processes: relation_list(processes, "summary processes")?,
        pipelines: relation_list(pipelines, "summary pipelines")?,
        edges: relation_list(edges, "summary edges")?,
        failures: relation_list(failures, "summary failures")?,
        has_make_variables: has_rows("makevar", "1")?,
        has_sud_trace: has_rows("sudtrace", "length(content)>0")?,
        activity: relation_list(Vec::new(), "summary activity")?,
    })
}

/// Search the box's recorded makefile variable assignments (the makevar
/// table). `name_pat` / `value_pat` are cmd_match text globs — a bare word
/// matches as a substring, empty matches everything. Rows come back in
/// assignment order so a value's history reads top-to-bottom.
pub fn makevars_typed(
    id: i64, name_pat: &str, value_pat: &str, limit: u64, any: bool,
) -> Result<Vec<crate::generated_wire::MakeVariableRow>, String> {
    use crate::generated_wire::MakeVariableRow;
    let conn = Connection::open_with_flags(
        sqlar_path(id), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    if !has_table_typed(&conn, "makevar")? { return Ok(Vec::new()); }
    if limit == 0 { return Ok(Vec::new()); }
    let mut out = vec![];
    // edge_id / pipeline_id resolve the capture-time anchors (recipe edge's
    // primary output / pipeline uid) to the row ids the cross-pane "ids"
    // filters key on, so the UI can navigate without another round-trip.
    let mut st = conn.prepare(
        "SELECT m.id, m.name, m.loc, m.value, m.make_dir, m.rhs, m.refs,
                m.edge_out, m.uid, m.flags,
                (SELECT e.id FROM build_edges e
                  WHERE json_extract(e.outs,'$[0]') = m.edge_out LIMIT 1),
                (SELECT p.id FROM brushprov p WHERE p.uid = m.uid LIMIT 1)
         FROM makevar m ORDER BY m.id")
        .map_err(|error| error.to_string())?;
    let it = st.query_map([], |r| Ok((
        r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
        r.get::<_, String>(3)?, r.get::<_, String>(4)?,
        r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?,
        r.get::<_, Option<String>>(7)?, r.get::<_, Option<i64>>(8)?,
        r.get::<_, Option<String>>(9)?,
        r.get::<_, Option<i64>>(10)?, r.get::<_, Option<i64>>(11)?,
    ))).map_err(|error| error.to_string())?;
    for row in it
    {
        let (rid, name, loc, value, make_dir, rhs, refs, edge_out, uid, flags,
             edge_id, pipeline_id) = row.map_err(|error| error.to_string())?;
        let name_ok = name_pat.is_empty()
            || crate::rules::cmd_match(name_pat, &name);
        let value_ok = value_pat.is_empty()
            || crate::rules::cmd_match(value_pat, &value);
        if any { if !(name_ok || value_ok) { continue; } }
        else if !(name_ok && value_ok) { continue; }
        let bytes = |value: String, field: &str| crate::wire::BoundedBytes::new(value.into_bytes())
            .map_err(|error| format!("make variable {field} exceeds relation bound: {error:?}"));
        out.push(MakeVariableRow {
            id: u64::try_from(rid).map_err(|_| "negative make variable row id")?,
            name: bytes(name, "name")?,
            location: bytes(loc, "location")?,
            value: bytes(value, "value")?,
            make_directory: bytes(make_dir, "directory")?,
            rhs: bytes(rhs.unwrap_or_default(), "rhs")?,
            references: bytes(refs.unwrap_or_default(), "references")?,
            flags: crate::wire::BoundedText::new(flags.unwrap_or_default())
                .map_err(|error| format!("make variable flags exceed relation bound: {error:?}"))?,
            edge_output: edge_out.map(|value| bytes(value, "edge output")).transpose()?,
            pipeline_uid: uid.map(|value| u64::try_from(value)
                .map_err(|_| "negative make variable pipeline uid")).transpose()?,
            edge: edge_id.map(|value| u64::try_from(value)
                .map_err(|_| "negative make variable edge id")).transpose()?,
            pipeline: pipeline_id.map(|value| u64::try_from(value)
                .map_err(|_| "negative make variable pipeline id")).transpose()?,
        });
        if out.len() as u64 >= limit { break; }
    }
    Ok(out)
}

/// Map provenance row ids between the three linked domains — "process"
/// (process table row ids), "pipeline" (brushprov row ids) and "edge"
/// (build_edges row ids) — for the cross-pane generated filters. Every list
/// view keys its "ids" filter off one of these: procs use process row ids
/// directly, outputs match on process_id, changes' writer ids ARE process
/// row ids, pipelines and build-edges use their own row ids.
///
/// Links: process.brush_pipeline_id ↔ brushprov.id is a direct key. There is
/// no edge↔pipeline key in the schema, so that hop is the edge's EXECUTION
/// WINDOW: brushprov rows whose spawn_ts falls inside [started_ts, ended_ts]
/// (open-ended while the recipe is still running; an edge that never ran has
/// no members). process↔edge composes the two hops via pipelines.
pub fn map_ids_typed(
    id: i64,
    from: crate::generated_wire::ProvenanceDomain,
    ids: &[u64],
    to: crate::generated_wire::ProvenanceDomain,
) -> Result<Vec<u64>, String> {
    if ids.is_empty() || from == to { return Ok(ids.to_vec()); }
    let conn = Connection::open_with_flags(
        sqlar_path(id), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    let ids = ids.iter().map(|id| i64::try_from(*id)
        .map_err(|_| "provenance row id exceeds SQLite range"))
        .collect::<Result<Vec<_>, _>>()?;
    map_ids_conn(&conn, from, &ids, to)?.into_iter().map(|id|
        u64::try_from(id).map_err(|_| "negative mapped provenance row id".into())).collect()
}

fn map_ids_conn(
    conn: &Connection,
    from: crate::generated_wire::ProvenanceDomain,
    ids: &[i64],
    to: crate::generated_wire::ProvenanceDomain,
) -> Result<Vec<i64>, String> {
    use crate::generated_wire::ProvenanceDomain::{Edge, Pipeline, Process};
    if ids.is_empty() || from == to {
        return Ok(ids.to_vec());
    }
    let inlist = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let query = |sql: String| -> Result<Vec<i64>, String> {
        let mut statement = conn.prepare(&sql).map_err(|error| error.to_string())?;
        let rows = statement.query_map([], |row| row.get::<_, i64>(0))
            .map_err(|error| error.to_string())?;
        rows.map(|row| row.map_err(|error| error.to_string())).collect()
    };
    // Small slack on the window ends: the edge started/done stamps and the
    // pipeline's spawn_ts are written by different threads around the same
    // instant.
    match (from, to) {
        (Process, Pipeline) => query(format!(
            "SELECT DISTINCT brush_pipeline_id FROM process \
             WHERE id IN ({inlist}) AND brush_pipeline_id > 0")),
        (Pipeline, Process) => query(format!(
            "SELECT id FROM process WHERE brush_pipeline_id IN ({inlist})")),
        (Pipeline, Edge) => {
            if !has_table_typed(conn, "build_edges")? { return Ok(vec![]); }
            // Exact link: pipelines are stamped with the edge whose recipe
            // spawned them (record JSON `edge_out` == the edge's outs[0]).
            query(format!(
                "SELECT DISTINCT e.id FROM build_edges e, brushprov b \
                 WHERE b.id IN ({inlist}) \
                   AND json_extract(b.record,'$.edge_out') IS NOT NULL \
                   AND json_extract(b.record,'$.edge_out') = \
                       json_extract(e.outs,'$[0]')"))
        }
        (Edge, Pipeline) => {
            if !has_table_typed(conn, "build_edges")? { return Ok(vec![]); }
            query(format!(
                "SELECT DISTINCT b.id FROM brushprov b, build_edges e \
                 WHERE e.id IN ({inlist}) \
                   AND json_extract(b.record,'$.edge_out') IS NOT NULL \
                   AND json_extract(b.record,'$.edge_out') = \
                       json_extract(e.outs,'$[0]')"))
        }
        (Process, Edge) => {
            let pipes = map_ids_conn(conn, Process, ids, Pipeline)?;
            map_ids_conn(conn, Pipeline, &pipes, Edge)
        }
        (Edge, Process) => {
            let pipes = map_ids_conn(conn, Edge, ids, Pipeline)?;
            map_ids_conn(conn, Pipeline, &pipes, Process)
        }
        _ => Ok(vec![]),
    }
}

/// The causal neighborhood of one pipeline (brushprov row): the pipeline
/// that STARTED it (parent_uid chain, one hop), the pipelines IT started,
/// and the build edge whose recipe it belongs to (record.edge_out). This is
/// the "this started that" context the Pipelines detail pane shows so a
/// failure can be walked up to its root cause without guessing.
pub fn pipeline_context_typed(
    id: i64,
    prov_id: u64,
) -> Result<crate::generated_wire::PipelineContext, String> {
    use crate::generated_wire::{PipelineContext, PipelineContextItem};
    let conn = Connection::open_with_flags(
        sqlar_path(id), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    let prov_id = i64::try_from(prov_id).map_err(|_| "pipeline id exceeds SQLite range")?;
    let (uid, parent_uid, edge_out) = conn.query_row(
        "SELECT uid, parent_uid,                 COALESCE(json_extract(record,'$.edge_out'),'')          FROM brushprov WHERE id=?1",
        [prov_id],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?)))
        .map_err(|error| error.to_string())?;
    let item = |row: (i64, String, Option<i64>)| -> Result<PipelineContextItem, String> {
        Ok(PipelineContextItem {
            id: u64::try_from(row.0).map_err(|_| "negative pipeline row id")?,
            command: crate::wire::BoundedText::new(row.1)
                .map_err(|error| format!("pipeline command exceeds relation bound: {error:?}"))?,
            exit_code: row.2.map(|code| i32::try_from(code)
                .map_err(|_| "pipeline exit code exceeds i32")).transpose()?,
        })
    };
    let parent = if parent_uid > 0 {
        conn.query_row("SELECT id, cmd, exit_code FROM brushprov WHERE uid=?1", [parent_uid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .optional().map_err(|error| error.to_string())?.map(item).transpose()?
    } else { None };
    let mut children = vec![];
    if uid > 0 {
        let mut statement = conn.prepare(
            "SELECT id, cmd, exit_code FROM brushprov WHERE parent_uid=?1 ORDER BY id LIMIT 40")
            .map_err(|error| error.to_string())?;
        let rows = statement.query_map([uid], |row| Ok((
            row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|error| error.to_string())?;
        for row in rows {
            children.push(item(row.map_err(|error| error.to_string())?)?);
        }
    }
    Ok(PipelineContext {
        parent,
        children: crate::wire::BoundedVec::new(children)
            .map_err(|error| format!("pipeline children exceed relation bound: {error:?}"))?,
        edge_output: (!edge_out.is_empty()).then(|| crate::wire::BoundedBytes::new(
            edge_out.into_bytes()).map_err(|error|
                format!("pipeline edge output exceeds relation bound: {error:?}")))
            .transpose()?,
    })
}

fn has_table_typed(conn: &Connection, name: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name], |_| Ok(()),
    ).optional().map(|row| row.is_some()).map_err(|error| error.to_string())
}

pub fn decorate_typed(id: i64, rel: &str)
    -> Result<crate::generated_wire::ChangeDecoration, String>
{
    use crate::generated_wire::{ChangeDecoration, ChangeKind};
    let rel = rel.trim_start_matches('/');
    let Some(mode) = current_mode(id, rel) else {
        return Ok(ChangeDecoration {
            is_text: false, stale: false, kind: ChangeKind::Changed,
        });
    };
    let host = Path::new("/").join(rel);
    if mode & S_IFMT == S_IFCHR {
        return Ok(ChangeDecoration {
            is_text: false, stale: false, kind: ChangeKind::Deleted,
        });
    }
    // is_text: both base and current NUL-free, and not a symlink/tombstone.
    let is_text = if mode & S_IFMT == S_IFLNK {
        false
    } else {
        match current_bytes(id, rel) {
            Some(cur) if !cur.contains(&0) => !lower_bytes(rel).contains(&0),
            _ => false,
        }
    };
    let hstat = host.symlink_metadata();
    let exists = hstat.is_ok();
    let kind = if exists { ChangeKind::Modified } else { ChangeKind::Created };
    let mut stale = false;
    if let Ok(md) = &hstat {
        if let Some(cm) = current_mtime(id, rel) {
            use std::os::unix::fs::MetadataExt;
            let host_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
            stale = host_ns > cm;
        }
    }
    Ok(ChangeDecoration { is_text, stale, kind })
}

// ── host-mutating review actions (top-level boxes; nested promotion deferred) ──
use std::time::Duration;

fn open_rw(id: i64) -> Option<Connection> {
    let c = Connection::open(sqlar_path(id)).ok()?;
    c.busy_timeout(Duration::from_secs(3)).ok()?;
    Some(c)
}

/// The nesting context for apply/discard/finalize: how to find a box's PARENT,
/// its immediate CHILDREN, and any box's live BoxState (RAM mirror). Built from
/// the engine's `Overlay` (live boxes) + on-disk discovery (at-rest parent/child
/// links). When there is no overlay (a stale/non-server caller), every box is
/// treated as at-rest and links come from the on-disk sqlar meta alone — so the
/// nested semantics still hold for finished boxes.
///
/// A box's apply with a parent PROMOTES into that parent's overlay (a nested
/// pending change); only a TOP-LEVEL box's apply reaches the real host. A
/// discard copies each path DOWN into immediate children that inherit it before
/// the row is dropped.
pub struct NestCtx {
    overlay: Option<crate::overlay::Overlay>,
}

impl NestCtx {
    pub fn new(overlay: Option<crate::overlay::Overlay>) -> Self {
        Self { overlay }
    }

    /// `id`'s parent box id, from on-disk discovery (the authoritative sqlar
    /// meta) — same answer whether or not the box is running.
    fn parent_of(&self, id: i64) -> Option<i64> {
        crate::discover::discover().get(&id).and_then(|b| b.parent)
    }

    /// `id`'s immediate child box ids (parent_box_id == id), live + at-rest.
    fn children_of(&self, id: i64) -> Vec<i64> {
        crate::discover::discover().values()
            .filter(|b| b.parent == Some(id) && b.box_id != id)
            .map(|b| b.box_id).collect()
    }

    /// `id`'s live BoxState, used ONLY to refresh the in-RAM mirror after a
    /// write so a running FUSE mount stays coherent — never to change the
    /// logical read/write result, which is always the sqlar's.
    fn live(&self, id: i64) -> Option<std::sync::Arc<crate::capture::BoxState>> {
        self.overlay.as_ref().and_then(|o| o.live_box(id))
    }

    /// D-parent: is `id`'s `readonly_parent` flag set? Read from the sqlar meta —
    /// same answer running or not. The flag is a child's ATTITUDE toward its
    /// parent; it stops `apply` from promoting captured changes into the parent.
    fn readonly_parent_of(&self, id: i64) -> bool {
        crate::discover::box_meta(id).get("readonly_parent")
            .map(String::as_str) == Some("1")
    }
}

fn row_of(conn: &Connection, rel: &str) -> Option<(i64, u32, Option<Vec<u8>>)> {
    crate::depot::archive_node(conn, rel).map(|n| (n.rowid, n.mode, n.data))
}

fn consume(conn: &Connection, id: i64, rel: &str, rowid: i64) {
    crate::depot::archive_delete(conn, rel);
    let _ = std::fs::remove_file(blob_path(id, rowid));
}

const S_IFIFO: u32 = 0o010000;
const S_IFBLK: u32 = 0o060000;

/// `lsetxattr` on the leaf beneath the already-resolved `parent` dir fd,
/// surfacing the OS error instead of dropping it (audit H4). Mirrors
/// `hostfs::setxattr_at` byte for byte (the same `/proc/self/fd/<parent>/<leaf>`
/// confinement that does not follow the final symlink), except it returns a
/// Result so a failed restore can abort the apply rather than be silently lost.
/// Lives here (not hostfs) only because hostfs is out of scope for this change.
fn setxattr_at_checked(parent: BorrowedFd, name: &CStr, key: &CStr, val: &[u8])
    -> Result<(), String> {
    let leaf = name.to_str().map_err(|_| "non-utf8 leaf name".to_string())?;
    let path = format!("/proc/self/fd/{}/{}", parent.as_raw_fd(), leaf);
    let cpath = CString::new(path).map_err(|_| "NUL in xattr path".to_string())?;
    // SAFETY: valid C strings and byte buffer.
    let r = unsafe {
        libc::lsetxattr(cpath.as_ptr(), key.as_ptr(),
                        val.as_ptr().cast(), val.len(), 0)
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// Atomically write `bytes` (with exact `mode`) to the leaf `name` beneath the
/// already-resolved `parent` dir fd (audit C2'): write a sibling temp file in
/// the SAME directory, fsync it, then `renameat` it over the target — so an
/// error mid-write can never truncate or corrupt the host file that's already
/// there. On any failure the temp is unlinked and prior host content is
/// untouched. The C2 ancestor-symlink guard is preserved by the caller, which
/// resolved `parent` with per-component O_NOFOLLOW; this helper only ever
/// touches names directly under that fd. As the OLD `hostfs::write_file_at`
/// did, a pre-existing SYMLINK at the leaf is refused (we must not replace a
/// box-planted symlink with content — that was the C2-class escape the leaf
/// O_NOFOLLOW check guarded against), so we lstat first and bail on a symlink.
fn write_file_atomic_at(parent: BorrowedFd, name: &CStr, bytes: &[u8], mode: u32)
    -> Result<(), String> {
    // Refuse to clobber a symlink leaf (parity with write_file_at's O_NOFOLLOW
    // open, which errored on an existing symlink rather than following it).
    if let Some(st) = hostfs::lstat_at(parent, name) {
        if st.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err("refusing to overwrite a symlink leaf".into());
        }
    }
    // Unique sibling temp name under the SAME parent dir (same filesystem → the
    // rename is atomic). Created O_EXCL|O_NOFOLLOW so it can never race onto an
    // attacker-planted name or symlink.
    let leaf = name.to_str().map_err(|_| "non-utf8 leaf name".to_string())?;
    let tmp_name = format!(".sarun-apply-tmp-{}-{}-{}",
        std::process::id(), apply_tmp_seq(), leaf);
    // Cap the length so a very long leaf can't push us past NAME_MAX (255).
    let tmp_name: String = tmp_name.chars().take(250).collect();
    let ctmp = CString::new(tmp_name).map_err(|_| "NUL in temp name".to_string())?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
        | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid dirfd and C string; variadic mode arg for O_CREAT.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), ctmp.as_ptr(), flags, mode & 0o7777) };
    if fd < 0 {
        return Err(format!("create temp: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fresh owned fd; File takes ownership and closes it on drop.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    // Helper to clean up the temp on any failure below.
    let cleanup = || { let _ = hostfs::unlink_at(parent, &ctmp); };
    let write_res = (|| -> Result<(), String> {
        use std::io::Write;
        let mut f = std::fs::File::from(owned);
        f.write_all(bytes).map_err(|e| format!("write temp: {e}"))?;
        // O_CREAT's mode is umask-masked, so set the exact mode explicitly and
        // surface any failure (audit H4: a 0600 file must not land world-readable).
        // SAFETY: valid open fd.
        if unsafe { libc::fchmod(f.as_raw_fd(), mode & 0o7777) } != 0 {
            return Err(format!("set mode: {}", std::io::Error::last_os_error()));
        }
        // Flush the data to disk before the rename so a crash can't leave a
        // renamed-but-empty file shadowing the prior content.
        f.flush().map_err(|e| format!("flush temp: {e}"))?;
        // SAFETY: valid open fd.
        if unsafe { libc::fsync(f.as_raw_fd()) } != 0 {
            return Err(format!("fsync temp: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    })();
    if let Err(e) = write_res {
        cleanup();
        return Err(e);
    }
    // Atomic replace. renameat never follows a symlink at either end, so the
    // target is replaced as a whole — no write-through-symlink escape.
    // SAFETY: valid dirfds and C strings.
    let r = unsafe {
        libc::renameat(parent.as_raw_fd(), ctmp.as_ptr(),
                       parent.as_raw_fd(), name.as_ptr())
    };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        cleanup();
        return Err(format!("rename temp into place: {e}"));
    }
    Ok(())
}

/// Monotonic counter making each apply temp file name unique within this process.
fn apply_tmp_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

/// Restore mtime / owner / xattrs onto a just-materialized leaf. Audit H4:
/// xattr (and mode, set at create time in hostfs) failures must NOT be reported
/// as a successful apply — they are surfaced as an Err. Owner is legitimately
/// best-effort: an unprivileged host user's `lchown` EPERMs on a uid/gid it
/// can't assign, so a failed owner restore is INTENTIONALLY swallowed (the
/// content is correct; only the recorded uid/gid couldn't be reproduced).
/// xattrs, by contrast, can carry security-relevant state (capabilities, ACLs,
/// SELinux labels), so a dropped setxattr is a real divergence — collected and
/// returned.
fn restore_metadata_at(conn: &Connection, rel: &str, parent: BorrowedFd, leaf: &CStr, mtime_ns: i64)
    -> Result<(), String> {
    // mtime (atime = mtime): drives downstream make/rebuild decisions.
    hostfs::utimens_at(parent, leaf, mtime_ns);
    // owner: best-effort. lchown EPERMs for an unprivileged host user (it cannot
    // give a file away to another uid/gid), which is the common case here, so a
    // failure is expected and deliberately ignored — see the doc comment above.
    if let Ok((uid, gid)) = conn.query_row(
        "SELECT uid,gid FROM ownership WHERE name=?1", [rel],
        |r| Ok((r.get::<_,i64>(0)? as u32, r.get::<_,i64>(1)? as u32))) {
        hostfs::chown_at(parent, leaf, uid, gid);
    }
    // xattrs: surface failures. A dropped setxattr can silently lose a security
    // attribute (file caps / ACL / label), so the FIRST failure aborts and is
    // returned rather than logged-and-forgotten.
    if let Ok(mut st) = conn.prepare("SELECT key,value FROM xattr WHERE name=?1") {
        if let Ok(rows) = st.query_map([rel], |r|
            Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?))) {
            for (k, v) in rows.flatten() {
                let ck = CString::new(k.clone())
                    .map_err(|_| format!("xattr key '{k}' contains NUL"))?;
                setxattr_at_checked(parent, leaf, &ck, &v)
                    .map_err(|e| format!("setxattr '{k}': {e}"))?;
            }
        }
    }
    Ok(())
}

fn materialize(conn: &Connection, id: i64, rel: &str) -> Result<(), String> {
    let root = hostfs::open_root().map_err(|e| format!("open root: {e}"))?;
    materialize_at(root.as_fd(), conn, id, rel)
}

/// Write the captured change `rel` onto the host beneath `root`, never following
/// a symlinked component of `rel` (audit C2: a box-planted symlink must not let
/// an apply escape onto an arbitrary host path). `root` is `/` in production and
/// a temp dir under test. Every host mutation goes through `hostfs`'s `*at`
/// helpers, which resolve the parent with per-component `O_NOFOLLOW` and refuse
/// to write/delete through a symlink at the leaf.
fn materialize_at(root: BorrowedFd, conn: &Connection, id: i64, rel: &str) -> Result<(), String> {
    let (rowid, mode, data) = row_of(conn, rel).ok_or("not in archive")?;
    let mtime_ns: i64 = crate::depot::archive_mtime(conn, rel).unwrap_or(0);

    if mode & S_IFMT == S_IFCHR {
        // char-device row == deletion tombstone (the Python convention). Resolve
        // WITHOUT creating ancestors; if the parent path doesn't exist (or an
        // ancestor is a symlink we refuse to follow), there is nothing to
        // delete — a no-op, matching the old exists()-guarded behavior.
        let Ok((parent, leaf)) = hostfs::parent_beneath(root, rel, false) else {
            return Ok(());
        };
        hostfs::remove_tree_at(parent.as_fd(), &leaf)?;
        return Ok(());
    }

    // All creating branches resolve (and create) the parent dirs beneath root,
    // refusing any symlinked ancestor.
    let (parent, leaf) = hostfs::parent_beneath(root, rel, true)?;
    let parent = parent.as_fd();

    if mode & S_IFMT == S_IFLNK {
        let tgt = data.ok_or("symlink row has no target")?;
        hostfs::symlink_at(parent, &leaf, &tgt)?;
    } else if mode & S_IFMT == 0o040000 {
        hostfs::mkdir_at(parent, &leaf, mode)?;
    } else if mode & S_IFMT == S_IFIFO || mode & S_IFMT == S_IFBLK {
        // fifo / block device: recreate the node on the host.
        let rdev: i64 = conn.query_row("SELECT dev FROM rdev WHERE name=?1", [rel],
                                       |r| r.get(0)).unwrap_or(0);
        hostfs::mknod_at(parent, &leaf, mode, rdev as u64)?;
    } else {
        let bytes = match data {
            Some(d) => d,
            None => std::fs::read(blob_path(id, rowid)).map_err(|e| e.to_string())?,
        };
        // Audit C2': atomic temp-then-rename in the SAME parent dir (resolved
        // above with per-component O_NOFOLLOW), so an error mid-write can never
        // truncate or corrupt the host file already there.
        write_file_atomic_at(parent, &leaf, &bytes, mode)?;
    }
    // Audit H4: a metadata-restore failure (xattr / mode) must NOT be reported
    // as a successful apply, so it propagates.
    restore_metadata_at(conn, rel, parent, &leaf, mtime_ns)?;
    Ok(())
}

fn paths_arg(id: i64, paths: &[&str]) -> Vec<String> {
    if !paths.is_empty() {
        return paths.iter().map(|path| (*path).to_owned()).collect();
    }
    changed_paths(id)
}

/// Audit M1: has the real host file at `rel` changed since this box captured it?
/// True when the host path exists and its mtime is strictly newer than the
/// mtime stored in the box's sqlar (the moment of capture). Same comparison the
/// `decorate` `stale` flag uses, lifted into a hard pre-apply gate. A path the
/// box created (no host file) or one with no recorded capture mtime is NOT
/// considered stale. Conservative: only refuses on a positive staleness signal,
/// so the normal apply path (host unchanged) is untouched.
///
/// The guard is about not clobbering newer host CONTENT with a stale box write,
/// so it applies only to content rows. A tombstone (S_IFCHR deletion) carries no
/// content and a fixed `mtime=0`, so the mtime comparison is meaningless for it
/// (it would otherwise refuse every deletion, since any live host file is newer
/// than 0); deletions are never gated here.
fn host_changed_since_capture(id: i64, rel: &str) -> bool {
    let rel = rel.trim_start_matches('/');
    let Some((_, mode, _)) = open_ro(id).and_then(|c| row_of(&c, rel)) else {
        return false;
    };
    if mode & S_IFMT == S_IFCHR { return false; } // deletion tombstone: not gated
    let host = Path::new("/").join(rel);
    let Ok(md) = host.symlink_metadata() else { return false };
    let Some(cap_ns) = current_mtime(id, rel) else { return false };
    use std::os::unix::fs::MetadataExt;
    let host_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
    host_ns > cap_ns
}

/// apply == PROMOTE into the parent overlay (a nested box) or WRITE the host
/// (a top-level box). Mirror of Python ChangeReview.apply. For each path: a box
/// WITH a parent promotes the captured change into the parent's overlay (a
/// pending change in the parent box), routed through the parent's live BoxState
/// when running, else its at-rest sqlar; a TOP-LEVEL box materializes the change
/// onto the real host. On success the path is consumed from this box's archive.
///
/// Audit H3: this reads the box's pool blobs, which a live FUSE write may be
/// mid-`write_at` on, so it must only run on a STOPPED box. The running-box
/// guard lives at the control-plane callers (control.rs `apply`/`discard` and
/// `review.apply`/`review.discard`), where the engine's `box_pids` live-set is
/// in reach — mirroring how `dissolve` guards itself.
///
/// TODO (audit C3): this multi-path apply is NOT transactional. It
/// materializes-then-consumes per path, so an error at path N leaves
/// 1..N-1 already written to the host AND consumed from the archive, with N..
/// still pending — there is no "nothing happened" rollback. A full fix
/// (stage all paths, then commit-or-rollback as one unit) is a large redesign
/// deliberately deferred; the per-path staleness guard below at least refuses
/// to silently clobber a host file that changed since capture.
fn relation_path(path: String) -> Result<crate::generated_wire::Path, String> {
    crate::wire::BoundedBytes::new(path.into_bytes())
        .map_err(|error| format!("path exceeds relation bound: {error:?}"))
}

fn relation_path_error(
    path: Option<String>,
    message: String,
) -> Result<crate::generated_wire::PathError, String> {
    Ok(crate::generated_wire::PathError {
        path: path.map(relation_path).transpose()?,
        message: crate::wire::BoundedText::new(message)
            .map_err(|error| format!("error message exceeds relation bound: {error:?}"))?,
    })
}

pub fn apply_typed(
    id: i64,
    paths: &[&str],
    ctx: &NestCtx,
) -> Result<crate::generated_wire::ApplyResult, String> {
    let Some(conn) = open_rw(id) else {
        return Ok(crate::generated_wire::ApplyResult {
            applied: crate::wire::BoundedVec::new(Vec::new()).unwrap(),
            errors: crate::wire::BoundedVec::new(vec![relation_path_error(
                None,
                "no archive".into(),
            )?])
            .unwrap(),
        });
    };
    let parent = ctx.parent_of(id);
    // D-parent: a child marked `readonly_parent` REFUSES to promote into its
    // parent — its captured changes can be reviewed/discarded but never leak
    // up the box stack. Same flag also blocks the top-level host-materialize
    // when a no-parent box has it set (e.g. an OCI rootfs that should never
    // touch the host). The error string is the same shape Python returns so
    // the UI's error pane works uniformly.
    let ro_parent = ctx.readonly_parent_of(id);
    let mut applied = Vec::new();
    let mut errors = Vec::new();
    for rel in paths_arg(id, paths) {
        let rel = rel.trim_start_matches('/').to_string();
        // Sibling preservation FIRST (fail-closed): the promote below mutates
        // the parent/host every sibling reads through — each sibling that
        // inherits `rel` snapshots its current view as its own row before the
        // bytes underneath it change. Direct children of the applied box need
        // nothing: they read the same content through the promoted row.
        let result = if ro_parent {
            Err("parent is read-only (--readonly-parent); apply refused".into())
        } else if let Err(e) = preserve_sibling_views(id, &rel, ctx) {
            Err(e)
        } else { match parent {
            Some(p) => {
                // Nested box: promote into the parent's overlay, not the host.
                let plive = ctx.live(p);
                promote_into_parent(id, p, plive.as_deref(), &rel)
            }
            None => {
                // Top-level: write the real host. Audit M1 — refuse to silently
                // overwrite a host file that changed AFTER this box captured it
                // (the host mtime is newer than the stored capture mtime). The
                // `decorate` stale flag is the same advisory the UI shows; here
                // it becomes a hard refusal so a concurrent edit isn't clobbered
                // without the user knowing. The user can re-capture / re-run to
                // pick up the new baseline.
                if host_changed_since_capture(id, &rel) {
                    Err("host file changed since capture (stale); apply refused \
                         — re-run the box to pick up the new contents".into())
                } else {
                    materialize(&conn, id, &rel)
                }
            }
        }};
        match result {
            Ok(()) => {
                if let Some((rowid, _, _)) = row_of(&conn, &rel) {
                    consume(&conn, id, &rel, rowid);
                }
                applied.push(relation_path(rel)?);
            }
            Err(error) => errors.push(relation_path_error(Some(rel), error)?),
        }
    }
    Ok(crate::generated_wire::ApplyResult {
        applied: crate::wire::BoundedVec::new(applied)
            .map_err(|error| format!("apply result exceeds relation bound: {error:?}"))?,
        errors: crate::wire::BoundedVec::new(errors)
            .map_err(|error| format!("apply errors exceed relation bound: {error:?}"))?,
    })
}

/// discard == drop each change from the box WITHOUT writing the host — but first
/// copy it DOWN into any immediate child that inherits it, so the child's merged
/// view is unchanged. Mirror of Python ChangeReview.discard. A copy-down failure
/// for a path leaves that path in place (errored) — the child must not lose its
/// inherited view.
pub fn discard_typed(
    id: i64,
    paths: &[&str],
    ctx: &NestCtx,
) -> Result<crate::generated_wire::DiscardResult, String> {
    let mut discarded = Vec::new();
    let mut errors = Vec::new();
    let children = |b: i64| ctx.children_of(b);
    let resolve = |b: i64| ctx.live(b);
    if let Some(conn) = open_rw(id) {
        for rel in paths_arg(id, paths) {
            let rel = rel.trim_start_matches('/').to_string();
            if let Err(e) = copydown_to_children(id, &rel, &children, &resolve) {
                errors.push(relation_path_error(Some(rel), e)?);
                continue;
            }
            if let Some((rowid, _, _)) = row_of(&conn, &rel) {
                consume(&conn, id, &rel, rowid);
                discarded.push(relation_path(rel)?);
            }
        }
    }
    Ok(crate::generated_wire::DiscardResult {
        discarded: crate::wire::BoundedVec::new(discarded)
            .map_err(|error| format!("discard result exceeds relation bound: {error:?}"))?,
        errors: crate::wire::BoundedVec::new(errors)
            .map_err(|error| format!("discard errors exceed relation bound: {error:?}"))?,
    })
}

/// Split bytes into lines on '\n', keeping the terminator on each line (the last
/// line keeps whatever it had). join(result) == data exactly — byte-exact splice
/// (mirror of Python ut_split).
fn ut_split(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![];
    let parts: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    for p in &parts[..parts.len() - 1] {
        let mut l = p.to_vec();
        l.push(b'\n');
        out.push(l);
    }
    if let Some(last) = parts.last() {
        if !last.is_empty() {
            out.push(last.to_vec());
        }
    }
    out
}

/// (lower byte-lines, upper byte-lines, grouped opcodes) for a text change, or
/// None for a non-text change. Each group is a Vec of (tag, i1, i2, j1, j2),
/// matching Python difflib.get_grouped_opcodes(3) tuple shape so the splice math
/// (a1,a2,b1,b2 = g[0][1], g[-1][2], g[0][3], g[-1][4]) carries over verbatim.
type Group = Vec<(DiffTag, usize, usize, usize, usize)>;
fn hunk_groups(id: i64, rel: &str) -> Option<(Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Group>)> {
    let rel = rel.trim_start_matches('/');
    let mode = current_mode(id, rel)?;
    if mode & S_IFMT == S_IFCHR || mode & S_IFMT == S_IFLNK {
        return None;
    }
    let cur = current_bytes(id, rel)?;
    if cur.contains(&0) {
        return None;
    }
    let low = lower_bytes(rel);
    if low.contains(&0) {
        return None;
    }
    let ll = ut_split(&low);
    let ul = ut_split(&cur);
    // Group via the SAME line-diff path hunks() uses (cross-checked equal to
    // Python difflib), then carry the indices onto the raw byte-line vectors so
    // the splice stays byte-exact (CR/CRLF, missing final newline preserved).
    let lo = String::from_utf8(low.clone()).ok()?;
    let cu = String::from_utf8(cur.clone()).ok()?;
    let diff = TextDiff::from_lines(&lo, &cu);
    let mut groups = vec![];
    for g in diff.grouped_ops(3) {
        if g.is_empty() {
            continue;
        }
        let mut group = vec![];
        for op in &g {
            let (tag, o, n) = op.as_tag_tuple();
            group.push((tag, o.start, o.end, n.start, n.end));
        }
        groups.push(group);
    }
    Some((ll, ul, groups))
}

/// Write `new_lower` (a sequence of raw byte-lines) to the host at `rel`,
/// refusing to write through a symlink. Mirror of Python _write_host_hunk.
fn write_host_hunk(rel: &str, new_lower: &[Vec<u8>]) -> Result<(), String> {
    // Same symlink-safety as materialize (audit C2): resolve the parent beneath
    // `/` without following any symlinked component, and refuse to write through
    // a symlink at the leaf. Preserve the existing file's mode (this is an
    // in-place text edit, not a fresh capture).
    let root = hostfs::open_root().map_err(|error| format!("open root: {error}"))?;
    let (parent, leaf) = hostfs::parent_beneath(root.as_fd(), rel, true)?;
    let bytes: Vec<u8> = new_lower.concat();
    hostfs::write_file_preserve_mode_at(parent.as_fd(), &leaf, &bytes)
}

/// After a hunk op the diff is gone exactly when the stored current bytes equal
/// the host's bytes; drop the row + pool blob then (mirror of SqlarSource.settle).
fn settle(id: i64, rel: &str) {
    let rel = rel.trim_start_matches('/');
    let cur = current_bytes(id, rel).unwrap_or_default();
    if cur == lower_bytes(rel) {
        if let Some(conn) = open_rw(id) {
            if let Some((rowid, _, _)) = row_of(&conn, rel) {
                consume(&conn, id, rel, rowid);
            }
        }
    }
}

/// Revert bytes back into the box's current state for `rel` (discard_hunk): write
/// the new bytes inline into the sqlar row's data and drop the stale pool blob so
/// it can't shadow the new content. Mirror of SqlarSource.write_current.
fn write_current(id: i64, rel: &str, data: &[u8]) -> Result<(), String> {
    let rel = rel.trim_start_matches('/');
    let conn = open_rw(id).ok_or("archive unavailable")?;
    let rowid = crate::depot::archive_write_inline(&conn, rel, data)
        .map_err(|error| error.to_string())?;
    if let Some(r) = rowid {
        match std::fs::remove_file(blob_path(id, r)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

/// apply_hunk: splice ONE hunk group onto the host. The box already contains it,
/// so that hunk simply stops being a difference. Byte-exact on raw byte-lines.
/// Mirror of Python ChangeReview.apply_hunk.
pub fn apply_hunk_typed(id: i64, rel: &str, index: u32) -> Result<(), String> {
    let Some((ll, ul, groups)) = hunk_groups(id, rel) else {
        return Err("not a text change".into());
    };
    let index = index as usize;
    if index >= groups.len() {
        return Err("stale hunk".into());
    }
    let g = &groups[index];
    let a1 = g[0].1;
    let a2 = g[g.len() - 1].2;
    let b1 = g[0].3;
    let b2 = g[g.len() - 1].4;
    let mut new_lower: Vec<Vec<u8>> = vec![];
    new_lower.extend_from_slice(&ll[..a1]);
    new_lower.extend_from_slice(&ul[b1..b2]);
    new_lower.extend_from_slice(&ll[a2..]);
    write_host_hunk(rel, &new_lower)?;
    settle(id, rel);
    Ok(())
}

/// discard_hunk: revert one hunk in the box (back to the host's bytes at that
/// range). Mirror of Python ChangeReview.discard_hunk.
pub fn discard_hunk_typed(id: i64, rel: &str, index: u32) -> Result<(), String> {
    let Some((ll, ul, groups)) = hunk_groups(id, rel) else {
        return Err("not a text change".into());
    };
    let index = index as usize;
    if index >= groups.len() {
        return Err("stale hunk".into());
    }
    let g = &groups[index];
    let a1 = g[0].1;
    let a2 = g[g.len() - 1].2;
    let b1 = g[0].3;
    let b2 = g[g.len() - 1].4;
    let mut new_upper: Vec<Vec<u8>> = vec![];
    new_upper.extend_from_slice(&ul[..b1]);
    new_upper.extend_from_slice(&ll[a1..a2]);
    new_upper.extend_from_slice(&ul[b2..]);
    let bytes: Vec<u8> = new_upper.concat();
    write_current(id, rel, &bytes)?;
    settle(id, rel);
    Ok(())
}

/// One source entry's full record (the sqlar row + its side-table rows), read
/// once from the source box's at-rest sqlar so the writers below never re-read.
struct SrcEntry {
    rowid: i64,
    mode: u32,
    mtime: i64,
    sz: i64,
    data: Option<Vec<u8>>,
    opaque: i64,
    owner: Option<(i64, i64)>,
    rdev: Option<i64>,
    xattrs: Vec<(String, Vec<u8>)>,
}

/// Read `rel`'s complete record from `src`'s on-disk sqlar (row + ownership +
/// rdev + xattrs). None if the source has no such row.
fn read_src_entry(src: i64, rel: &str) -> Option<SrcEntry> {
    let pc = open_ro(src)?;
    let n = crate::depot::archive_node(&pc, rel)?;
    let (rowid, mode, mtime, sz, data, opaque) =
        (n.rowid, n.mode as i64, n.mtime, n.sz, n.data, n.opaque as i64);
    let owner: Option<(i64, i64)> = pc.query_row(
        "SELECT uid,gid FROM ownership WHERE name=?1", [rel],
        |r| Ok((r.get(0)?, r.get(1)?))).ok();
    let rdev: Option<i64> = pc.query_row("SELECT dev FROM rdev WHERE name=?1", [rel],
                                         |r| r.get(0)).ok();
    let mut xattrs: Vec<(String, Vec<u8>)> = vec![];
    if let Ok(mut st) = pc.prepare("SELECT key,value FROM xattr WHERE name=?1") {
        if let Ok(rows) = st.query_map([rel], |r|
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))) {
            xattrs = rows.flatten().collect();
        }
    }
    Some(SrcEntry { rowid, mode: mode as u32, mtime, sz, data, opaque, owner, rdev, xattrs })
}

/// Does `id`'s OWN view resolve `rel` to "whiteout" (a tombstone), "present"
/// (file/symlink/dir/special), or None? Mirror of Python ChangeReview._own_kind.
///
/// Reads the box's sqlar — the single authoritative store. Every overlay write
/// (live FUSE handler OR offline promote/copy-down) is write-through to the
/// sqlar, and a live box's in-RAM `kinds` mirror is a strict subset of it (a
/// whiteout is an `S_IFCHR` row, etc.), so the answer does NOT depend on whether
/// a process is running in the box.
fn own_kind(id: i64, rel: &str) -> Option<&'static str> {
    let conn = open_ro(id)?;
    let mode: u32 = crate::depot::archive_mode(&conn, rel)?;
    Some(if mode & S_IFMT == S_IFCHR { "whiteout" } else { "present" })
}

/// Does `id`'s LOWER (what it INHERITS, ignoring its own overlay) currently
/// resolve `rel` to a PRESENT entry? Walks the parent chain to the host —
/// mirror of Python ChangeReview._lower_has:
///   - no parent (top-level box): whether the host path exists or is a symlink;
///   - has parent p: inspect p's OWN entry — a whiteout means deleted (False);
///     a present entry means True; no own entry → recurse into p's lower.
/// Reads the authoritative sqlar of each box in the chain (parent links from
/// on-disk discovery), so the result does NOT depend on whether any box in the
/// chain is running. A `seen` set + depth cap guard a circular parent chain.
pub fn lower_has(id: i64, rel: &str) -> bool {
    let rel = rel.trim_start_matches('/');
    let boxes = crate::discover::discover();
    let mut cur = id;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let Some(psid) = boxes.get(&cur).and_then(|b| b.parent) else {
            let host = Path::new("/").join(rel);
            return host.symlink_metadata().is_ok();
        };
        if !seen.insert(psid) {
            return false; // cycle in the parent chain: stop safely
        }
        match own_kind(psid, rel) {
            Some("whiteout") => return false,
            Some(_) => return true,
            None => cur = psid,
        }
    }
    false // depth exceeded: treat as not found
}

/// Write a source entry RECORD into a destination box's overlay — ONE write path
/// (the destination's authoritative sqlar) shared by dissolve copy-down and
/// nested apply-promote, with a live destination's RAM mirror refreshed
/// afterward. Does not depend on whether a process runs in the destination.
///
/// For a tombstone source (a deletion), `tombstone_as_whiteout` chooses the
/// outcome: true → write a whiteout into the destination (the deletion must
/// shadow whatever the destination's lower still resolves); false → just drop
/// the destination's own row + blob (its lower has nothing here, so no shadow is
/// needed — a plain row-drop, mirroring _promote_into_parent's delete branch).
fn promote_record(e: &SrcEntry, src: i64, dst: i64,
                  dst_live: Option<&crate::capture::BoxState>,
                  rel: &str, tombstone_as_whiteout: bool) -> Result<(), String> {
    let kind = e.mode & S_IFMT;

    // ONE write path: write the destination's authoritative sqlar (a fresh RW
    // connection — fine for a live box too; SQLite serializes writers and the
    // box's own writes are autocommit). Whether or not a process is running in
    // the destination is irrelevant to WHAT is written; a running box just gets
    // its in-RAM `kinds` mirror refreshed afterward (see the reload_entry tail)
    // so its FUSE mount serves the new state.
    let result: Result<(), String> = (|| {
        let cc = open_rw(dst).ok_or("destination archive unavailable")?;
        if kind == S_IFCHR && !tombstone_as_whiteout {
            // Lower has nothing here: drop the destination's own row + blob.
            if let Some((rowid, _, _)) = row_of(&cc, rel) {
                consume(&cc, dst, rel, rowid);
            }
            return Ok(());
        }
        // INSERT OR REPLACE so an apply-promote OVERWRITES the destination's
        // prior view; drop any stale blob the replaced row named first. (A
        // copy-down never reaches here for an already-present destination — its
        // caller guards on has_own.)
        if let Some((old_rowid, _, _)) = row_of(&cc, rel) {
            let _ = std::fs::remove_file(blob_path(dst, old_rowid));
        }
        let new_rowid = crate::depot::archive_upsert(
            &cc, rel, e.mode, e.mtime, e.sz, e.data.as_deref(), e.opaque)?;
        if kind == 0o100000 {
            let s = blob_path(src, e.rowid);
            if s.exists() {
                let dstb = blob_path(dst, new_rowid);
                if let Some(p) = dstb.parent() {
                    std::fs::create_dir_all(p).map_err(|x| x.to_string())?;
                }
                std::fs::copy(&s, &dstb).map_err(|x| x.to_string())?;
            }
        }
        // Propagate these like the row + blob above: a dropped ownership /
        // rdev / xattr write would promote the file with the wrong uid/gid,
        // a missing device number, or lost xattrs while apply reported
        // success — a silent partial-corruption. Fail the apply instead.
        if let Some((u, g)) = e.owner {
            cc.execute("INSERT OR REPLACE INTO ownership(name,uid,gid) \
                        VALUES(?1,?2,?3)", params![rel, u, g])
              .map_err(|x| x.to_string())?;
        }
        if let Some(dev) = e.rdev {
            cc.execute("INSERT OR REPLACE INTO rdev(name,dev) VALUES(?1,?2)",
                       params![rel, dev]).map_err(|x| x.to_string())?;
        }
        for (k, v) in &e.xattrs {
            cc.execute("INSERT OR REPLACE INTO xattr(name,key,value) \
                        VALUES(?1,?2,?3)", params![rel, k, v])
              .map_err(|x| x.to_string())?;
        }
        Ok(())
        // cc is dropped here, releasing the write lock BEFORE the mirror refresh.
    })();

    result?;
    // Cache-coherence, not a behavioral branch: a running destination re-reads
    // the one row we just wrote into its in-RAM mirror so its FUSE mount is
    // consistent. An at-rest destination has no mirror — nothing to do.
    if let Some(cb) = dst_live {
        cb.reload_entry(rel);
    }
    Ok(())
}

/// Promote `rel`'s captured change from `box_id` (the box being APPLIED) INTO
/// `parent`'s overlay — a nested apply captures the change as a PENDING change
/// in the parent box instead of writing the host. Mirror of Python
/// _promote_into_parent. `parent_live` routes the write through the parent's
/// live BoxState (RAM mirror) when the parent is running. A deletion promotes as
/// a whiteout iff the PARENT's own lower (its parent chain) still resolves rel to
/// a present entry; otherwise it drops the parent's own row.
pub fn promote_into_parent(box_id: i64, parent: i64,
                           parent_live: Option<&crate::capture::BoxState>,
                           rel: &str) -> Result<(), String> {
    let rel = rel.trim_start_matches('/');
    let Some(e) = read_src_entry(box_id, rel) else {
        return Err("not in archive".into());
    };
    let tombstone_as_whiteout = lower_has(parent, rel);
    promote_record(&e, box_id, parent, parent_live, rel, tombstone_as_whiteout)
}

/// apply's SIBLING preservation (the three-action model: apply promotes a
/// box's changes UP into its parent — or the host — but must not change any
/// OTHER box's merged view). `a`'s direct children are safe by construction
/// (they read the same bytes through `a`'s consumed row's destination), but
/// `a`'s SIBLINGS — every other box with the same parent (for a top-level
/// box: every other top-level box) — read the parent/host `a` is about to
/// mutate. Before the promote, each sibling that inherits `rel` (no own row)
/// gets its CURRENT view snapshotted as its own row: the first parent-chain
/// box owning `rel` copies down (a whiteout stays a whiteout via
/// copy_down_entry), and a chain miss snapshots the real host entry — where
/// an ABSENT host path becomes a whiteout, so a sibling doesn't suddenly see
/// the file `a` is newly creating. Fail-closed: an error means the caller
/// must NOT promote this path.
pub fn preserve_sibling_views(a: i64, rel: &str, ctx: &NestCtx)
    -> Result<(), String> {
    let rel = rel.trim_start_matches('/');
    let boxes = crate::discover::discover();
    let parent = boxes.get(&a).and_then(|b| b.parent);
    let sibs: Vec<i64> = boxes.values()
        .filter(|b| b.parent == parent && b.box_id != a)
        .map(|b| b.box_id).collect();
    if sibs.is_empty() {
        return Ok(());
    }
    // The source of the siblings' current view of `rel`: the first box in the
    // parent chain (starting AT the parent) with an own row; None = the view
    // falls through every box to the real host.
    let mut src: Option<i64> = None;
    let mut cur = parent;
    let mut seen = std::collections::HashSet::new();
    while let Some(o) = cur {
        if !seen.insert(o) { break; }
        if own_kind(o, rel).is_some() { src = Some(o); break; }
        cur = boxes.get(&o).and_then(|b| b.parent);
    }
    for s in sibs {
        let live = ctx.live(s);
        match src {
            Some(o) => copy_down_entry(o, s, rel, live.as_deref())
                .map_err(|e| format!("preserve sibling {s}: {e}"))?,
            None => snapshot_host_into(s, live.as_deref(), rel)
                .map_err(|e| format!("preserve sibling {s}: {e}"))?,
        }
    }
    Ok(())
}

/// Snapshot the HOST's current entry at `rel` into box `dst` as its own row —
/// sibling preservation when the old view fell through the whole chain to the
/// real host. Only if `dst` has no own row (same guard as copy_down_entry).
/// An ABSENT host path snapshots as a whiteout: the sibling's old view was
/// "no such file" and must stay that way once the host gains the file.
fn snapshot_host_into(dst: i64, dst_live: Option<&crate::capture::BoxState>,
                      rel: &str) -> Result<(), String> {
    let rel = rel.trim_start_matches('/');
    let has = open_ro(dst)
        .map(|c| crate::depot::archive_exists(&c, rel))
        .unwrap_or(false);
    if has {
        return Ok(());
    }
    let host = Path::new("/").join(rel);
    let cc = open_rw(dst).ok_or("destination archive unavailable")?;
    let md = host.symlink_metadata();
    let result: Result<(), String> = (|| {
        let Ok(md) = md else {
            crate::depot::archive_upsert(&cc, rel, S_IFCHR, 0, 0, None, 0)?;
            return Ok(());
        };
        use std::os::unix::fs::MetadataExt;
        let mode = md.mode();
        let mtime_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
        match mode & S_IFMT {
            S_IFLNK => {
                let tgt = std::fs::read_link(&host)
                    .map_err(|e| e.to_string())?;
                let bytes = tgt.as_os_str().as_encoded_bytes().to_vec();
                crate::depot::archive_upsert(&cc, rel, mode, mtime_ns,
                                             bytes.len() as i64,
                                             Some(&bytes), 0)?;
            }
            0o040000 => {
                crate::depot::archive_upsert(&cc, rel, mode, mtime_ns,
                                             0, None, 0)?;
            }
            0o100000 => {
                let rowid = crate::depot::archive_upsert(
                    &cc, rel, mode, mtime_ns, md.size() as i64, None, 0)?;
                let dstb = blob_path(dst, rowid);
                if let Some(p) = dstb.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::copy(&host, &dstb).map_err(|e| e.to_string())?;
            }
            // fifo / block device: node + device number. A host CHAR device
            // is unrepresentable (S_IFCHR rows are the tombstone convention);
            // fail closed rather than snapshot a "deleted" view.
            k if k == S_IFIFO || k == S_IFBLK => {
                crate::depot::archive_upsert(&cc, rel, mode, mtime_ns,
                                             0, None, 0)?;
                cc.execute("INSERT OR REPLACE INTO rdev(name,dev) \
                            VALUES(?1,?2)", params![rel, md.rdev() as i64])
                  .map_err(|e| e.to_string())?;
            }
            _ => return Err(format!(
                "host {rel}: unrepresentable file type {:o}", mode & S_IFMT)),
        }
        cc.execute("INSERT OR REPLACE INTO ownership(name,uid,gid) \
                    VALUES(?1,?2,?3)", params![rel, md.uid(), md.gid()])
          .map_err(|e| e.to_string())?;
        Ok(())
    })();
    drop(cc);
    result?;
    if let Some(cb) = dst_live {
        cb.reload_entry(rel);
    }
    Ok(())
}

/// Copy a single parent entry DOWN into a child box, but ONLY if the child has
/// no entry of its own for that path. This preserves the child's merged view
/// (read-through-parent) at the instant the parent is dissolved OR a parent path
/// is discarded: a path the child inherited from the parent (never touched
/// itself) would change once the parent's row is dropped, so we snapshot the
/// parent's version into the child first. If the child already has its own row
/// for `rel`, its view is self-contained and we leave it untouched.
///
/// `child_live` is the child's live `BoxState` when a process is running in it —
/// used ONLY to refresh the child's RAM mirror after the write (via
/// promote_record), never to change what is read or written. Whether the child
/// "has its own entry" is read from its authoritative sqlar regardless. Files
/// copy the parent blob into the child's pool under a fresh rowid;
/// symlinks/tombstones/special carry their row data + side tables. A tombstone
/// copies down AS a whiteout (the child keeps seeing 'absent').
pub fn copy_down_entry(parent: i64, child: i64, rel: &str,
                       child_live: Option<&crate::capture::BoxState>)
    -> Result<(), String> {
    let rel = rel.trim_start_matches('/');
    // Child already speaks for this path (its own sqlar row exists) — its view
    // is self-contained, nothing to copy down. Read the sqlar, not a live mirror.
    let has = open_ro(child)
        .map(|c| crate::depot::archive_exists(&c, rel))
        .unwrap_or(false);
    if has {
        return Ok(());
    }
    let Some(e) = read_src_entry(parent, rel) else {
        return Err("parent has no such entry".into());
    };
    promote_record(&e, parent, child, child_live, rel, /*tombstone_as_whiteout=*/true)
}

/// Copy `rel` DOWN into every immediate child of `sid` that inherits it (has no
/// own entry) BEFORE `sid`'s own row is dropped — so discarding from `sid` never
/// changes a child's merged view. Mirror of Python _copydown_to_children.
/// `children_of(sid)` lists the immediate child box ids (live + at-rest);
/// `resolve_live(c)` is each child's live BoxState when running. Returns
/// Err(msg) if any child copy-down failed (the caller MUST NOT then drop the
/// row — the child would lose its inherited view).
fn copydown_to_children<C, F>(sid: i64, rel: &str, children_of: &C, resolve_live: &F)
    -> Result<(), String>
    where C: Fn(i64) -> Vec<i64>,
          F: Fn(i64) -> Option<std::sync::Arc<crate::capture::BoxState>> {
    let kids = children_of(sid);
    if kids.is_empty() {
        return Ok(());
    }
    // Source claims this path; if its bytes can't be read, fail closed.
    if read_src_entry(sid, rel).is_none() {
        return Err(format!("copy-down: {rel} not readable from source"));
    }
    for child in kids {
        let live = resolve_live(child);
        copy_down_entry(sid, child, rel, live.as_deref())
            .map_err(|e| format!("copy-down into {child}: {e}"))?;
    }
    Ok(())
}

/// Rewrite a child box's parent pointer in its sqlar meta (the on-disk source
/// discover() reads `parent_box_id` from). `new` = Some(grandparent) reparents;
/// None promotes the child to top-level (deletes the key).
pub fn set_parent_meta(child: i64, new: Option<i64>) -> Result<(), String> {
    let cc = open_rw(child).ok_or("child archive unavailable")?;
    match new {
        Some(p) => cc.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('parent_box_id',?1)",
            params![p.to_string()]),
        None => cc.execute("DELETE FROM meta WHERE key='parent_box_id'", []),
    }.map_err(|e| e.to_string())?;
    Ok(())
}

/// Mark a child box's sqlar as `no_host_fallback=1` — the closure bit that stops
/// resolve()/scan_dir() falling absent paths through to the real host. Used by
/// dissolve(): when the box being freed carried the closure (an OCI image's
/// --no-parent base), each child must inherit it, or re-parenting onto the
/// grandparent (often top-level) would silently re-open the child to the host.
/// The on-disk write is for at-rest children; a live child flips its in-RAM
/// atomic via BoxState::set_no_host_fallback as well.
pub fn set_no_host_meta(child: i64) -> Result<(), String> {
    let cc = open_rw(child).ok_or("child archive unavailable")?;
    cc.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('no_host_fallback','1')",
        [],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

/// All changed paths a box captured (apply- and discard-bound alike) — the set
/// a child may have inherited a view of through this box.
pub fn changed_paths(id: i64) -> Vec<String> {
    session_changes_typed(id).unwrap_or_default().into_iter()
        .filter_map(|change| String::from_utf8(change.path.as_slice().to_vec()).ok())
        .collect()
}

/// Unified diff for the whole box (the `patch` CLI verb). Per changed path: a
/// git-style ---/+++ header and the text hunks, or a one-line note for
/// binary/symlink/deleted. Best-effort, human-facing.
pub fn patch_text(id: i64) -> Vec<u8> {
    let mut out = String::new();
    let changes = session_changes_typed(id).unwrap_or_default();
    for change in changes {
        let rel = std::str::from_utf8(change.path.as_slice()).unwrap_or("");
        out.push_str(&format!("--- a/{rel}\n+++ b/{rel}\n"));
        let diff = match hunks_typed(id, rel) {
            Ok(diff) => diff,
            Err(error) => {
                out.push_str(&format!("# error: {error}\n"));
                continue;
            }
        };
        match diff {
            crate::generated_wire::FileDiff::Text { hunks } => {
                for hunk in hunks.into_inner() {
                    for line in hunk.lines.into_inner() {
                        let style = line.style.as_str();
                        let prefix = if style == "hdr" { "" } else { style };
                        out.push_str(&format!("{prefix}{}\n", line.text.as_str()));
                    }
                }
            }
            crate::generated_wire::FileDiff::Deleted => {
                out.push_str("# deleted (non-text)\n");
            }
            crate::generated_wire::FileDiff::Symlink { kind, .. }
            | crate::generated_wire::FileDiff::Binary { kind, .. } => {
                let kind = match kind {
                    crate::generated_wire::ChangeKind::Modified => "modified",
                    crate::generated_wire::ChangeKind::Created => "created",
                    _ => "changed",
                };
                out.push_str(&format!("# {kind} (non-text)\n"));
            }
            crate::generated_wire::FileDiff::Unavailable { message } => {
                out.push_str(&format!("# error: {}\n", message.as_str()));
            }
        }
    }
    out.into_bytes()
}

// ── structural diff (binary detail pane) ────────────────────────────────────
// Mirrors the Python ChangeReview.structural_diff_{quick,finish}: sniff the
// type of a binary change's bytes, pick a differ argv template (readelf -Wa for
// ELF, ar/unzip/tar for other recognized types), run that differ on the base
// and current bytes INSIDE a locked-down bwrap sandbox, and return a unified
// diff of the two textual dumps. The quick verb returns the type line(s) + the
// header immediately plus a job id; the finish verb runs the (heavy, sandboxed)
// dump synchronously in its handler thread and returns the full line list.
// Results materialize in the generated closed relation types. The temporary
// JSON listener projects those values for the current UI at its outer edge.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

const STRUCT_MAX: usize = 4 * 1024 * 1024;
const SANDBOX_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

struct StructJob {
    argv: Vec<String>,
    base: Vec<u8>,
    cur: Vec<u8>,
    head: Vec<crate::generated_wire::StructuralLine>,
}

fn job_registry() -> &'static StdMutex<HashMap<u64, StructJob>> {
    static REG: OnceLock<StdMutex<HashMap<u64, StructJob>>> = OnceLock::new();
    REG.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn next_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn pair(style: &str, text: impl Into<String>)
    -> Result<crate::generated_wire::StructuralLine, String>
{
    Ok(crate::generated_wire::StructuralLine {
        style: crate::wire::BoundedText::new(style.to_owned())
            .map_err(|error| format!("structural style exceeds relation bound: {error:?}"))?,
        text: crate::wire::BoundedText::new(text.into())
            .map_err(|error| format!("structural line exceeds relation bound: {error:?}"))?,
    })
}

fn bounded_lines(lines: Vec<crate::generated_wire::StructuralLine>)
    -> Result<crate::wire::BoundedVec<
        crate::generated_wire::StructuralLine,
        0,
        { crate::generated_wire::LIMIT_COLLECTION_ITEMS },
    >, String>
{
    crate::wire::BoundedVec::new(lines)
        .map_err(|error| format!("structural lines exceed relation bound: {error:?}"))
}

/// Best-effort type sniff. Read the common magic numbers directly (no libmagic
/// dependency); fall back to `file --brief` for anything else. Produces strings
/// `differ_for` matches against ("ELF", "ar archive", …).
fn struct_type(data: &[u8]) -> String {
    if data.len() >= 4 && &data[..4] == b"\x7fELF" {
        return "ELF binary".to_string();
    }
    if data.len() >= 8 && &data[..8] == b"!<arch>\n" {
        return "current ar archive".to_string();
    }
    if data.len() >= 2 && &data[..2] == b"PK" {
        return "Zip archive data".to_string();
    }
    if data.len() >= 2 && &data[..2] == b"\x1f\x8b" {
        return "gzip compressed data".to_string();
    }
    if let Some(t) = file_type(&data[..data.len().min(65536)]) {
        if !t.is_empty() {
            return t;
        }
    }
    "data".to_string()
}

/// Shell out to `file --brief` on the leading bytes (best-effort fallback).
fn file_type(data: &[u8]) -> Option<String> {
    let tmp = scratch_file("sniff", data).ok()?;
    let out = std::process::Command::new("file")
        .arg("--brief").arg(&tmp).output().ok();
    let _ = std::fs::remove_file(&tmp);
    let out = out?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Pick (argv_template, label) for a recognized binary type, else None.
/// `{in}` is the placeholder for the input path inside the sandbox. Mirrors the
/// Python differ_for choices for the tools available here.
fn differ_for(mtype: &str, data: &[u8]) -> Option<(Vec<String>, String)> {
    if data.is_empty() {
        return None;
    }
    let mt = mtype.to_lowercase();
    let v = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if mt.contains("elf") {
        return Some((v(&["readelf", "-Wa", "{in}"]), "ELF (readelf -Wa)".into()));
    }
    if mt.contains("ar archive") {
        return Some((v(&["ar", "t", "{in}"]), "ar archive (ar t)".into()));
    }
    if mt.contains("zip archive") || &data[..data.len().min(2)] == b"PK" {
        return Some((v(&["unzip", "-l", "{in}"]), "zip (unzip -l)".into()));
    }
    if mt.contains("tar archive") || mt.contains("gzip compressed")
        || mt.contains("bzip2") || mt.contains("xz compressed") {
        return Some((v(&["tar", "-tvf", "{in}"]), "tar (tar -tvf)".into()));
    }
    None
}

/// FAST half: type line(s) + differ selection. When `job` is absent the lines
/// are the complete result (unrecognized type or over the size cap).
pub fn struct_quick(id: i64, rel: &str)
    -> Result<crate::generated_wire::StructuralQuick, String>
{
    use crate::generated_wire::StructuralQuick;
    let rel = rel.trim_start_matches('/');
    let base = lower_bytes(rel);
    let cur = current_bytes(id, rel).unwrap_or_default();
    let mut lines = Vec::new();
    if !base.is_empty() && !cur.is_empty() {
        lines.push(pair("type", format!("type (base): {}", struct_type(&base)))?);
        lines.push(pair("type", format!("type (current): {}", struct_type(&cur)))?);
    } else {
        let sniff = if cur.is_empty() { &base } else { &cur };
        lines.push(pair("type", format!("type: {}", struct_type(sniff)))?);
    }
    let sniff = if cur.is_empty() { base.clone() } else { cur.clone() };
    let Some((argv, label)) = differ_for(&struct_type(&sniff), &sniff) else {
        return Ok(StructuralQuick { lines: bounded_lines(lines)?, job: None });
    };
    lines.push(pair("hdr", format!(
        "\u{2500}\u{2500} structural diff \u{b7} {label} \u{2500}\u{2500}"))?);
    if base.len() > STRUCT_MAX || cur.len() > STRUCT_MAX {
        lines.push(pair("dim", format!("(skipped: file exceeds {STRUCT_MAX} bytes)"))?);
        return Ok(StructuralQuick { lines: bounded_lines(lines)?, job: None });
    }
    let jid = next_id();
    job_registry().lock().unwrap().insert(jid, StructJob {
        argv, base, cur, head: lines.clone(),
    });
    Ok(StructuralQuick { lines: bounded_lines(lines)?, job: Some(jid) })
}

/// SLOW half: run the sandboxed dump(s) for `job` and build the unified
/// structural diff.
pub fn struct_finish(job_id: u64)
    -> Result<crate::generated_wire::StructuralDiff, String>
{
    use crate::generated_wire::StructuralDiff;
    let Some(job) = job_registry().lock().unwrap().remove(&job_id) else {
        return Ok(StructuralDiff {
            lines: bounded_lines(vec![pair("err", "unknown struct job")?])?,
        });
    };
    let mut lines = job.head.clone();
    let dump = |data: &[u8]| -> String {
        if data.is_empty() {
            return String::new();
        }
        match run_on_untrusted(&job.argv, data) {
            Ok(out) => out,
            Err(e) => format!("<parser error: {e}>"),
        }
    };
    if !job.base.is_empty() && !job.cur.is_empty() {
        let bd = dump(&job.base);
        let cd = dump(&job.cur);
        let diff = TextDiff::from_lines(&bd, &cd);
        let bl: Vec<&str> = diff.iter_old_slices()
            .map(|s| s.trim_end_matches(['\r', '\n'])).collect();
        let cl: Vec<&str> = diff.iter_new_slices()
            .map(|s| s.trim_end_matches(['\r', '\n'])).collect();
        let mut any = false;
        for group in diff.grouped_ops(3) {
            if group.is_empty() { continue; }
            let (_, a0, _) = group[0].as_tag_tuple();
            let (_, alast, blast) = group[group.len() - 1].as_tag_tuple();
            let (_, _, b0) = group[0].as_tag_tuple();
            lines.push(pair("@", format!("@@ -{},{} +{},{} @@",
                a0.start + 1, alast.end - a0.start, b0.start + 1, blast.end - b0.start))?);
            any = true;
            for op in &group {
                let (tag, orange, nrange) = op.as_tag_tuple();
                match tag {
                    DiffTag::Equal => for k in orange { lines.push(pair(" ", bl[k])?); },
                    _ => {
                        for k in orange { lines.push(pair("-", bl[k])?); }
                        for k in nrange { lines.push(pair("+", cl[k])?); }
                    }
                }
            }
        }
        if !any {
            lines.push(pair("dim", "(structural dumps identical)")?);
        }
    } else {
        let which_side = if job.cur.is_empty() { "base" } else { "current" };
        lines.push(pair("dim", format!("({which_side} only)"))?);
        let data = if job.cur.is_empty() { &job.base } else { &job.cur };
        for ln in dump(data).split('\n') {
            lines.push(pair(" ", ln.trim_end_matches('\r'))?);
        }
    }
    Ok(StructuralDiff { lines: bounded_lines(lines)? })
}

pub fn struct_cancel(job_id: u64) {
    job_registry().lock().unwrap().remove(&job_id);
}

/// Write `data` to a uniquely-named scratch file under the system temp dir.
fn scratch_file(tag: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir();
    let p = dir.join(format!("sarun-ut-{tag}-{}-{}", std::process::id(), next_id()));
    std::fs::write(&p, data)?;
    Ok(p)
}

/// Run `argv` (with a {in} placeholder) over untrusted `data` inside a throwaway
/// bwrap, as the Python run_on_untrusted does: the bytes go to a temp dir that
/// is ro-bound into a `--unshare-*` / `--cap-drop ALL` / `--die-with-parent`
/// sandbox with `/` mounted read-only, and {in} resolves to the path inside.
/// If bwrap is unavailable, runs the differ directly on the host temp file
/// (noted in any error). Output is capped at 256 KiB. Never panics.
fn run_on_untrusted(argv: &[String], data: &[u8]) -> Result<String, String> {
    // A dedicated dir so we can ro-bind exactly the input into the sandbox.
    let dir = std::env::temp_dir()
        .join(format!("sarun-utd-{}-{}", std::process::id(), next_id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let host_in = dir.join("in");
    let res = (|| {
        std::fs::write(&host_in, data).map_err(|e| e.to_string())?;
        let inside_dir = "/tmp/ut";
        let inside_in = format!("{inside_dir}/in");
        let is_in = |a: &str| a.starts_with('{') && a.ends_with('}')
            && &a[1..a.len() - 1] == "in";
        let out = if which("bwrap") {
            let mut cmd = std::process::Command::new("bwrap");
            cmd.args(["--unshare-pid", "--unshare-ipc", "--unshare-uts",
                      "--unshare-net", "--die-with-parent", "--new-session",
                      "--cap-drop", "ALL", "--ro-bind", "/", "/",
                      "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
            cmd.arg("--ro-bind").arg(&dir).arg(inside_dir);
            cmd.args(["--chdir", inside_dir, "--clearenv",
                      "--setenv", "PATH", SANDBOX_PATH, "--"]);
            cmd.args(argv.iter().map(|a| if is_in(a) { inside_in.clone() }
                                       else { a.clone() }));
            cmd.stdin(std::process::Stdio::null());
            cmd.output().map_err(|e| format!("spawn failed: {e}"))?
        } else {
            let real: Vec<String> = argv.iter().map(|a| if is_in(a) {
                host_in.to_string_lossy().into_owned() } else { a.clone() }).collect();
            std::process::Command::new(&real[0]).args(&real[1..])
                .stdin(std::process::Stdio::null())
                .output().map_err(|e| format!("spawn failed (no bwrap): {e}"))?
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let capped: String = stdout.chars().take(256 * 1024).collect();
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let msg: String = err.trim().chars().take(2000).collect();
            return Err(if msg.is_empty() {
                format!("exit {:?}", out.status.code()) } else { msg });
        }
        Ok(capped)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    res
}

fn which(prog: &str) -> bool {
    std::env::var_os("PATH").map(|paths| {
        std::env::split_paths(&paths).any(|p| p.join(prog).is_file())
    }).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{BoxState, Entry};

    /// The unified write path: promoting a change into a LIVE parent box writes
    /// the parent's authoritative sqlar AND refreshes its in-RAM `kinds` mirror,
    /// so a running FUSE mount serves the promoted file without caring that the
    /// write came from review rather than the box's own handler. (Audit: overlay
    /// read/write must not behave differently based on whether a process runs.)
    #[test]
    fn promote_into_live_parent_refreshes_the_ram_mirror() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir()
            .join(format!("sarun-promote-{}-{:?}", std::process::id(),
                          std::time::SystemTime::now()));
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: state_home() is derived from XDG_STATE_HOME; no concurrent test
        // in this binary reads it (the others use temp_dir()).
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let (parent_id, child_id) = (9001, 9002);
        let parent = BoxState::create(parent_id).unwrap(); // the LIVE destination
        let child = BoxState::create(child_id).unwrap();

        // Child captures a regular-file change: foo.txt = "hi".
        let rid = child.ensure_file_row("foo.txt", 0o100644, 0);
        let cblob = crate::depot::blob_path(child_id, rid);
        std::fs::create_dir_all(cblob.parent().unwrap()).unwrap();
        std::fs::write(&cblob, b"hi").unwrap();
        child.finalize_file("foo.txt", 2, 0, 0);

        assert!(parent.entry("foo.txt").is_none(), "precondition: parent empty");

        promote_into_parent(child_id, parent_id, Some(&parent), "foo.txt").unwrap();

        // sqlar got the row...
        let mode: i64 = {
            let c = parent.conn.lock().unwrap();
            c.query_row("SELECT mode FROM sqlar WHERE name='foo.txt'", [], |r| r.get(0))
                .expect("row in parent sqlar")
        };
        assert_eq!(mode as u32 & S_IFMT, 0o100000, "promoted as a regular file");

        // ...AND the live mirror was refreshed with the right rowid + bytes.
        match parent.entry("foo.txt") {
            Some(Entry::File { rowid, .. }) => {
                let pblob = crate::depot::blob_path(parent_id, rowid);
                assert_eq!(std::fs::read(&pblob).unwrap(), b"hi", "promoted blob copied");
            }
            Some(_) => panic!("live parent mirror has wrong entry kind after promote"),
            None => panic!("live parent mirror NOT refreshed by promote (still absent)"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The editor-save refusal gate (`review.write_file`): tombstones,
    /// symlinks, binary content (either direction) and paths that exist
    /// nowhere all refuse with a SPECIFIC error; a captured text row and a
    /// host-file fallback pass.
    #[test]
    fn write_file_guard_refuses_tombstone_symlink_binary_missing() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir()
            .join(format!("sarun-wguard-{}-{:?}", std::process::id(),
                          std::time::SystemTime::now()));
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: serialized by TEST_STATE_HOME_LOCK (same convention as the
        // promote test above).
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let id = 9101;
        let b = BoxState::create(id).unwrap();
        b.set_whiteout("gone.txt", 0);
        b.set_symlink("ln.txt", Path::new("/etc/hosts"), 0);
        let put = |rel: &str, bytes: &[u8]| {
            let rid = b.ensure_file_row(rel, 0o100644, 0);
            let bp = blob_path(id, rid);
            std::fs::create_dir_all(bp.parent().unwrap()).unwrap();
            std::fs::write(&bp, bytes).unwrap();
            b.finalize_file(rel, bytes.len() as i64, 0, 0);
        };
        put("bin.dat", b"\x7fELF\0\0junk");
        put("ok.txt", b"hello\n");

        // The refusal matrix holds for BOTH callers — the editor save
        // (allow_create = false) AND the oaita box_file_write agent tool
        // (allow_create = true) share this exact gate; only the nowhere-path
        // arm differs (asserted below). Every refusal here must fire in both.
        let err = |rel: &str, bytes: &[u8]| {
            let e = write_file_guard(id, rel, bytes, false).expect_err(rel).to_string();
            // The shared gate: identical under allow_create for all these.
            assert_eq!(write_file_guard(id, rel, bytes, true).expect_err(rel).to_string(),
                       e, "guard for {rel} must not depend on allow_create");
            e
        };
        assert!(err("gone.txt", b"x").contains("deleted"), "tombstone refused");
        assert!(err("ln.txt", b"x").contains("symlink"), "symlink refused");
        assert!(err("bin.dat", b"x").contains("binary"), "captured binary refused");
        assert!(err("ok.txt", b"a\0b").contains("binary"), "NUL payload refused");
        // Nowhere-path: the ONE axis that differs. The editor refuses it; the
        // agent tool (allow_create) creates it.
        let missing = tmp.join("absent.txt");
        let missing_rel = missing.to_str().unwrap().trim_start_matches('/');
        assert!(write_file_guard(id, missing_rel, b"x", false)
                    .expect_err("editor").to_string().contains("no such file"),
                "editor refuses a nowhere-path");
        assert!(write_file_guard(id, missing_rel, b"x", true).is_ok(),
                "agent tool creates a nowhere-path");
        // …but a NUL payload to that nowhere-path is still refused for the
        // agent (the binary guard precedes the existence check).
        assert!(write_file_guard(id, missing_rel, b"a\0b", true)
                    .expect_err("agent NUL").to_string().contains("binary"),
                "agent tool still refuses a NUL payload");
        assert!(write_file_guard(id, "ok.txt", b"new\n", false).is_ok(),
                "captured text row passes");
        // Host fallback: a real host file outside the change set passes.
        let host = tmp.join("host.txt");
        std::fs::write(&host, "host\n").unwrap();
        let host_rel = host.to_str().unwrap().trim_start_matches('/');
        assert!(write_file_guard(id, host_rel, b"edited\n", false).is_ok(),
                "host-file fallback passes");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// BUG 1 root-fix, depot row states: a discard-hunk revert leaves an
    /// INLINE regular-file row (bytes in `data`, blob dropped);
    /// `outline_inline_row` must materialize it back to a blob-backed row so
    /// copy_up (hence the box's own re-run write) can source it. No-op for
    /// already-blob-backed rows and for absent paths.
    #[test]
    fn outline_inline_row_materializes_reverted_row() {
        use crate::depot::BoxDepot;
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir()
            .join(format!("sarun-outline-{}-{:?}", std::process::id(),
                          std::time::SystemTime::now()));
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: serialized by TEST_STATE_HOME_LOCK.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let id = 9102;
        let b = BoxState::create(id).unwrap();
        // A normal blob-backed capture row.
        let rid = b.ensure_file_row("f.txt", 0o100644, 0);
        let bp = blob_path(id, rid);
        std::fs::create_dir_all(bp.parent().unwrap()).unwrap();
        std::fs::write(&bp, b"orig\n").unwrap();
        b.finalize_file("f.txt", 5, 0, 0);
        // Already blob-backed → no-op.
        assert!(!b.outline_inline_row("f.txt").unwrap(),
                "blob-backed row is a no-op");
        // Simulate discard_hunk's revert: inline the bytes, drop the blob.
        {
            let conn = open_rw(id).unwrap();
            crate::depot::archive_write_inline(&conn, "f.txt", b"reverted\n").unwrap();
        }
        std::fs::remove_file(&bp).ok();
        assert!(!bp.exists(), "revert dropped the pool blob");
        // Now the row is inline — copy_up could not source it. Materialize.
        assert!(b.outline_inline_row("f.txt").unwrap(),
                "inline row is materialized");
        assert_eq!(std::fs::read(&bp).unwrap(), b"reverted\n",
                   "inline bytes are now the pool blob");
        {
            let conn = open_ro(id).unwrap();
            let n = crate::depot::archive_node(&conn, "f.txt").unwrap();
            assert!(n.data.is_none(), "inline column cleared after materialize");
        }
        // Absent path → no-op, not an error.
        assert!(!b.outline_inline_row("nope.txt").unwrap(),
                "absent path is a no-op");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
