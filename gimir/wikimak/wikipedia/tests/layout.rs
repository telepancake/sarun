//! Layout tests. SPEC §"Layout under `root`" and §"sqlite schema".

mod common;

use rusqlite::Connection;
use tempfile::TempDir;

use common::make_instance;

// ---------------------------------------------------------------------------
// open_creates_layout
//
// Open a fresh root; assert every on-disk artifact named in SPEC exists.
// ---------------------------------------------------------------------------

#[test]
fn open_creates_layout() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);

    let root = tmp.path();
    assert!(root.join("depot").is_dir(), "depot/ dir must exist");
    assert!(root.join("depot").join("index").is_file(), "depot/index file");
    assert!(root.join("depot").join("f0").is_dir(), "depot/f0/ dir");
    assert!(root.join("depot").join("f1").is_dir(), "depot/f1/ dir");
    assert!(
        root.join("depot").join("cold").join("cold").is_file(),
        "depot/cold/cold file"
    );
    assert!(root.join("titles").is_dir(), "titles/ dir");
    assert!(root.join("meta.db").is_file(), "meta.db file");

    drop(instance);
}

// ---------------------------------------------------------------------------
// open_meta_db_has_schema_tables
//
// SPEC §"sqlite schema (sketch)" pins six tables. All must exist after
// Instance::open.
// ---------------------------------------------------------------------------

#[test]
fn open_meta_db_has_schema_tables() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);

    let conn = Connection::open(tmp.path().join("meta.db")).expect("open meta.db");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepare");
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for expected in [
        "category_intervals",
        "page_to_title_id",
        "parts_seen",
        "siteinfo_snapshots",
        "title_id_to_page",
        "title_intervals",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "meta.db missing table {expected}; have {names:?}"
        );
    }

    drop(instance);
}

// ---------------------------------------------------------------------------
// open_then_reopen_no_op
//
// Open + drop + reopen: no errors, layout unchanged, no spurious files.
// ---------------------------------------------------------------------------

#[test]
fn open_then_reopen_no_op() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);
    drop(instance);

    let before: Vec<_> = walkdir_relative(tmp.path());

    let instance2 = make_instance(&tmp, 1024);
    drop(instance2);

    let after: Vec<_> = walkdir_relative(tmp.path());
    assert_eq!(before, after, "reopen must not add or remove files");
}

fn walkdir_relative(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                out.push(rel);
                if p.is_dir() {
                    walk(&p, root, out);
                }
            }
        }
    }
    walk(root, root, &mut out);
    out.sort();
    out
}
