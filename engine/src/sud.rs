// sud-backed boxes, step 1 (see engine/DESIGN-sud.md — WORK IN PROGRESS).
// The box ran under tv's sudtrace with a plain directory upper overlaid on
// `/`; this module sweeps that upper directory into the box's sqlar
// BoxState after the command exits, so review/apply/discard/UI work on a
// sud box exactly as on a FUSE box. Post-exit sweep = final state only:
// every row is attributed to the runner's process row until the wire trace
// stream is ingested (step 2).

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::capture::BoxState;
use crate::capture::blob_path;

/// Walk `upper` (the sud overlay's upper directory) and mirror it into the
/// box's sqlar. Char-0:0 device nodes are the sud/overlayfs whiteout marker
/// and become whiteout rows. Returns (rows written, errors).
pub fn ingest_upper(b: &BoxState, upper: &Path, runpid: u32)
                    -> (usize, Vec<String>) {
    let writer = if runpid > 0 { b.writer_for(runpid) } else { 0 };
    let mut n = 0usize;
    let mut errs = Vec::new();
    walk(b, upper, "", writer, &mut n, &mut errs);
    (n, errs)
}

fn walk(b: &BoxState, dir: &Path, rel: &str, writer: i64,
        n: &mut usize, errs: &mut Vec<String>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => { errs.push(format!("{}: {e}", dir.display())); return; }
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            errs.push(format!("{}: non-utf8 name", dir.display()));
            continue;
        };
        let crel = if rel.is_empty() { name.to_string() }
                   else { format!("{rel}/{name}") };
        let p = ent.path();
        let md = match p.symlink_metadata() {
            Ok(m) => m,
            Err(e) => { errs.push(format!("{crel}: {e}")); continue; }
        };
        let mode = md.mode();
        let ftype = md.file_type();
        if ftype.is_dir() {
            b.set_dir(&crel, mode, writer);
            *n += 1;
            walk(b, &p, &crel, writer, n, errs);
        } else if ftype.is_symlink() {
            match std::fs::read_link(&p) {
                Ok(t) => { b.set_symlink(&crel, &t, writer); *n += 1; }
                Err(e) => errs.push(format!("{crel}: readlink: {e}")),
            }
        } else if mode & 0o170000 == 0o020000 && md.rdev() == 0 {
            // char 0:0 — the overlayfs whiteout marker sud's overlay uses.
            b.set_whiteout(&crel, writer);
            *n += 1;
        } else if ftype.is_file() {
            let rowid = b.ensure_file_row(&crel, mode, writer);
            let bp = blob_path(b.id, rowid);
            if let Some(parent) = bp.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::copy(&p, &bp) {
                Ok(sz) => {
                    let mtime_ns = md.mtime()
                        .saturating_mul(1_000_000_000)
                        .saturating_add(md.mtime_nsec());
                    b.finalize_file(&crel, sz as i64, mtime_ns, writer);
                    *n += 1;
                }
                Err(e) => errs.push(format!("{crel}: blob copy: {e}")),
            }
        } else {
            // fifo / real device node.
            b.set_special(&crel, mode, md.rdev(), writer);
            *n += 1;
        }
    }
}
