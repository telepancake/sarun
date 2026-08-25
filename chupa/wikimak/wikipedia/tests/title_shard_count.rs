//! The titles-pool shard count is a property of the STORE, persisted
//! in meta.db at creation (`instance_flags.title_shard_count`): exact
//! lookups route by `fnv1a(title) % count` and shard files are created
//! lazily, so nothing else on disk can recover the truth — a reader
//! assuming the CLI default against an 8-shard store would silently
//! miss titles. REAL effects pinned here:
//!
//!   * a store CREATED with 8 shards reopens read-side (derive config,
//!     `read_config`) with 8, and an exact lookup still walks exactly
//!     ONE shard (`Instance::title_scan_counts`);
//!   * an explicit mismatching count is a loud
//!     [`Error::TitleShardMismatch`], writer- and read-side alike;
//!   * a LEGACY store (flag row deleted in-test) counts as 4 — the
//!     only count the pre-persistence CLI ever built — keeps
//!     answering, and a writer open backfills the flag.

mod common;

use std::io::Cursor;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{read_config, Error, Instance, InstanceConfig};

#[cfg(unix)]
use std::os::unix::fs::symlink;

fn cfg_with(tmp: &TempDir, shards: u32) -> InstanceConfig {
    let mut cfg = common::cfg(tmp.path().to_path_buf(), 4096);
    cfg.title_shard_count = shards;
    cfg
}

fn page_xml(title: &str, id: u64) -> String {
    format!(
        r#"  <page>
    <title>{title}</title><ns>0</ns><id>{id}</id>
    <revision>
      <id>{rev}</id><timestamp>2020-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">body</text><sha1>aa</sha1>
    </revision>
  </page>
"#,
        rev = id * 10 + 1,
    )
}

/// Import "Topic Page 1..=n" (page_id = i) and flush.
fn import_titles(inst: &Instance, n: u64) {
    let mut pages = String::new();
    for i in 1..=n {
        pages.push_str(&page_xml(&format!("Topic Page {i}"), i));
    }
    let doc = format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>testwiki</dbname><base>http://x/</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
{pages}</mediawiki>"#
    );
    let mut stream = new_page_stream(Cursor::new(doc.into_bytes()));
    inst.import(&mut stream).expect("import");
    inst.flush().expect("flush");
}

fn persisted_flag(tmp: &TempDir) -> Option<i64> {
    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
    conn.query_row(
        "SELECT value FROM instance_flags WHERE key = 'title_shard_count'",
        [],
        |r| r.get(0),
    )
    .ok()
}

fn persisted_generation(tmp: &TempDir) -> u32 {
    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
    conn.query_row(
        "SELECT value FROM instance_flags WHERE key = 'title_pool_generation'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .unwrap()
    .unwrap_or(0) as u32
}

fn title_generation_dir(root: &Path, generation: u32) -> PathBuf {
    if generation == 0 {
        root.join("titles")
    } else {
        root.join(format!("titles-g{generation}"))
    }
}

fn title_receipt_path(root: &Path, generation: u32) -> PathBuf {
    if generation == 0 {
        root.join(".titles.receipt")
    } else {
        root.join(format!(".titles-g{generation}.receipt"))
    }
}

/// Build a stale but fully receipted title generation without invoking the
/// production reshard switch. This lets the following tests mutate exactly
/// one owned shard before writer reopen runs stale-generation GC.
fn make_stale_receipted_generation(tmp: &TempDir) -> PathBuf {
    let inst = Instance::open(cfg_with(tmp, 1)).expect("create title store");
    import_titles(&inst, 16);
    drop(inst);
    let selected = persisted_generation(tmp);
    let source_dir = title_generation_dir(tmp.path(), selected);
    let stale_generation = 99;
    let stale_dir = title_generation_dir(tmp.path(), stale_generation);
    std::fs::create_dir(&stale_dir).unwrap();
    for entry in std::fs::read_dir(&source_dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("shard-") {
            std::fs::copy(entry.path(), stale_dir.join(name)).unwrap();
        }
    }
    let mut receipt: Value = serde_json::from_slice(
        &std::fs::read(title_receipt_path(tmp.path(), selected)).unwrap(),
    )
    .unwrap();
    receipt["generation"] = Value::from(stale_generation);
    std::fs::write(
        title_receipt_path(tmp.path(), stale_generation),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    stale_dir
}

fn find_quarantined_bytes(root: &Path, expected: &[u8]) -> Option<PathBuf> {
    std::fs::read_dir(root.join(".title-pool-quarantine"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| std::fs::read(path).ok().as_deref() == Some(expected))
}

/// Exact lookup through the dictionary must walk exactly ONE shard,
/// once — the mis-routing symptom of a wrong count is a silent miss,
/// so both the answer and the walk shape are asserted.
fn assert_exact_lookup_one_shard(inst: &Instance) {
    let base = inst.title_scan_counts();
    let got = inst.page_id_by_title_at("Topic Page 7", None).expect("lookup");
    assert_eq!(got, Some(7), "exact lookup resolves through the derived count");
    let delta: Vec<u64> =
        inst.title_scan_counts().iter().zip(&base).map(|(a, b)| a - b).collect();
    assert_eq!(
        delta.iter().sum::<u64>(),
        1,
        "exact lookup walked exactly ONE shard, once (delta {delta:?})"
    );
}

// ---------------------------------------------------------------------------
// created_count_persists_and_read_side_derives_it
//
// Create with 8 shards → the flag is persisted; a derive-config
// read-side open (read_config, what the engine's attach verb and the
// pinned readout use) resolves 8 — not the old hardwired CLI default —
// and exact lookups stay one-shard. A derive-config WRITER reopen
// resolves 8 too.
// ---------------------------------------------------------------------------
#[test]
fn created_count_persists_and_read_side_derives_it() {
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(cfg_with(&tmp, 8)).expect("create with 8 shards");
    import_titles(&inst, 32);
    assert_eq!(persisted_flag(&tmp), Some(8), "creation persisted the count");
    drop(inst); // release the exclusive flock

    let r = Instance::open_read(read_config(tmp.path().to_path_buf()))
        .expect("read-side derive open");
    assert_eq!(r.title_shard_count(), 8, "reader derived the store's count");
    assert_eq!(r.title_scan_counts().len(), 8, "pool opened with all 8 shards");
    assert_exact_lookup_one_shard(&r);
    drop(r);

    let w = Instance::open(cfg_with(&tmp, 0)).expect("writer derive open");
    assert_eq!(w.title_shard_count(), 8, "writer derived the store's count");
    assert_exact_lookup_one_shard(&w);
}

// ---------------------------------------------------------------------------
// explicit_mismatch_is_loud
//
// A nonzero configured count that disagrees with the persisted one is
// refused with `TitleShardMismatch` naming both counts — writer- and
// read-side — while the matching explicit count still opens.
// ---------------------------------------------------------------------------
#[test]
fn explicit_mismatch_is_loud() {
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(cfg_with(&tmp, 8)).expect("create with 8 shards");
    import_titles(&inst, 4);
    drop(inst);

    match Instance::open(cfg_with(&tmp, 4)).map(|_| ()).unwrap_err() {
        Error::TitleShardMismatch { on_disk, requested, .. } => {
            assert_eq!((on_disk, requested), (8, 4));
        }
        other => panic!("writer mismatch must be TitleShardMismatch, got {other}"),
    }
    match Instance::open_read(cfg_with(&tmp, 2)).map(|_| ()).unwrap_err() {
        Error::TitleShardMismatch { on_disk, requested, .. } => {
            assert_eq!((on_disk, requested), (8, 2));
        }
        other => panic!("reader mismatch must be TitleShardMismatch, got {other}"),
    }

    // The matching explicit count still opens, both sides.
    Instance::open(cfg_with(&tmp, 8)).expect("matching writer open");
    Instance::open_read(cfg_with(&tmp, 8)).expect("matching reader open");
}

#[test]
fn fresh_derived_store_starts_with_256_small_shards() {
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(cfg_with(&tmp, 0)).expect("fresh derived open");
    assert_eq!(inst.title_shard_count(), 256);
    assert_eq!(persisted_flag(&tmp), Some(256));
}

#[test]
fn oversized_shards_repeat_double_and_reopen_with_remapped_ids() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = cfg_with(&tmp, 1);
    cfg.title_seal_threshold_bytes = 100;
    let inst = Instance::open(cfg).expect("create tiny-target pool");
    import_titles(&inst, 200);
    let grown = inst.title_shard_count();
    assert!(grown >= 8, "one large shard should require repeated doubling: {grown}");
    assert!(grown.is_power_of_two());
    drop(inst);

    let reopened = Instance::open_read(read_config(tmp.path().to_path_buf())).expect("reopen");
    assert_eq!(reopened.title_shard_count(), grown);
    for id in 1..=200 {
        let title = format!("Topic Page {id}");
        assert_eq!(
            reopened.page_id_by_title_at(&title, None).expect("lookup"),
            Some(id),
            "{title} survived dense-id remap"
        );
    }
    drop(reopened);

    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
    let generation: i64 = conn
        .query_row(
            "SELECT value FROM instance_flags WHERE key = 'title_pool_generation'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let generation_dir = tmp.path().join(format!("titles-g{generation}"));
    assert!(generation_dir.is_dir(), "selected generation exists");
    assert!(
        !tmp.path().join("titles").exists(),
        "generation zero was collected only after the committed switch"
    );
    let title_generations: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            name == "titles" || name.to_str().is_some_and(|n| n.starts_with("titles-g"))
        })
        .collect();
    assert_eq!(title_generations.len(), 1, "only the selected generation remains");
    for entry in std::fs::read_dir(generation_dir).unwrap() {
        let entry = entry.unwrap();
        assert!(
            entry.metadata().unwrap().len() <= 100,
            "final measured shard exceeds configured target: {:?}",
            entry.path()
        );
    }
}

#[test]
fn uncommitted_new_generation_is_ignored_on_reopen() {
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(cfg_with(&tmp, 4)).expect("create");
    import_titles(&inst, 16);
    drop(inst);

    // This is the filesystem state immediately before the SQLite switch:
    // a complete-or-partial new generation can exist, but no flag selects it.
    std::fs::create_dir_all(tmp.path().join("titles-g99/foreign/nested")).unwrap();
    std::fs::write(tmp.path().join("titles-g99/shard-0000"), []).unwrap();
    std::fs::write(
        tmp.path().join("titles-g99/foreign/nested/sentinel"),
        b"user-owned",
    )
    .unwrap();
    for name in ["titles-g0", "titles-g01", "titles-backup"] {
        std::fs::create_dir(tmp.path().join(name)).unwrap();
    }

    let reopened = Instance::open_read(read_config(tmp.path().to_path_buf())).expect("old opens");
    assert_eq!(reopened.title_shard_count(), 4);
    assert_eq!(
        reopened.page_id_by_title_at("Topic Page 7", None).unwrap(),
        Some(7)
    );
    drop(reopened);
    assert!(tmp.path().join("titles").is_dir(), "reader preserved selected generation");
    assert!(
        tmp.path().join("titles-g99").is_dir(),
        "read-only reopen does not mutate orphan state"
    );

    let writer = Instance::open(cfg_with(&tmp, 0)).expect("writer reopens selected generation");
    assert_eq!(writer.title_shard_count(), 4);
    assert_eq!(writer.page_id_by_title_at("Topic Page 7", None).unwrap(), Some(7));
    assert!(tmp.path().join("titles").is_dir(), "GC never removes selected generation");
    assert!(
        !tmp.path().join("titles-g99").exists(),
        "writer reopen retires the uncommitted generation"
    );
    let quarantine = tmp.path().join(".title-pool-quarantine");
    let retained = std::fs::read_dir(quarantine)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("foreign/nested/sentinel").exists())
        .expect("nested foreign data survives in the title-pool quarantine");
    assert_eq!(
        std::fs::read(retained.join("foreign/nested/sentinel")).unwrap(),
        b"user-owned"
    );
    for name in ["titles-g0", "titles-g01", "titles-backup"] {
        assert!(
            tmp.path().join(name).is_dir(),
            "GC ignores non-canonical or unrelated directory {name}"
        );
    }
}

#[test]
fn same_size_replacement_is_quarantined_from_receipted_stale_generation() {
    let tmp = TempDir::new().unwrap();
    let stale_dir = make_stale_receipted_generation(&tmp);
    let shard = stale_dir.join("shard-0000");
    let mut replacement = std::fs::read(&shard).unwrap();
    replacement[0] ^= 0xff;
    std::fs::remove_file(&shard).unwrap();
    std::fs::write(&shard, &replacement).unwrap();

    Instance::open(cfg_with(&tmp, 1)).expect("selected generation still opens");
    assert!(!stale_dir.exists(), "stale namespace is retired after inspection");
    let retained = find_quarantined_bytes(tmp.path(), &replacement)
        .expect("same-size replacement survives in quarantine");
    assert_eq!(std::fs::read(retained).unwrap(), replacement);
}

#[test]
fn malformed_stale_title_receipt_is_preserved_and_blocks_cleanup() {
    let tmp = TempDir::new().unwrap();
    let stale_dir = make_stale_receipted_generation(&tmp);
    let receipt_path = title_receipt_path(tmp.path(), 99);
    std::fs::write(&receipt_path, b"{\"generation\":99}").unwrap();

    let error = Instance::open(cfg_with(&tmp, 1)).map(|_| ()).unwrap_err();
    assert!(matches!(error, Error::Corrupt(_)));
    assert!(stale_dir.exists(), "malformed receipt prevents destructive cleanup");
    assert_eq!(std::fs::read(receipt_path).unwrap(), b"{\"generation\":99}");
}

#[cfg(unix)]
#[test]
fn symlinked_stale_title_shard_is_claimed_without_following_and_preserved() {
    let tmp = TempDir::new().unwrap();
    let stale_dir = make_stale_receipted_generation(&tmp);
    let shard = stale_dir.join("shard-0000");
    let bytes = std::fs::metadata(&shard).unwrap().len() as usize;
    let target = tmp.path().join("title-target");
    std::fs::write(&target, vec![0x33; bytes]).unwrap();
    std::fs::remove_file(&shard).unwrap();
    symlink(&target, &shard).unwrap();

    Instance::open(cfg_with(&tmp, 1)).expect("selected generation still opens");
    assert!(target.exists(), "symlink target was never followed or removed");
    assert!(!stale_dir.exists());
    let quarantine = tmp.path().join(".title-pool-quarantine");
    let retained = std::fs::read_dir(quarantine)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()))
        .expect("symlink itself survives in quarantine");
    assert_eq!(std::fs::read_link(retained).unwrap(), target);
}
