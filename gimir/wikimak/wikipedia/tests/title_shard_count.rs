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

use rusqlite::Connection;
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{read_config, Error, Instance, InstanceConfig};

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

// ---------------------------------------------------------------------------
// legacy_store_defaults_to_4_and_writer_backfills
//
// A store from before the flag existed (simulated by deleting the kv
// row) counts as 4 — every store the pre-persistence CLI ever built
// was 4-shard — so reads keep working; the next WRITER open backfills
// the flag, a reader never writes it.
// ---------------------------------------------------------------------------
#[test]
fn legacy_store_defaults_to_4_and_writer_backfills() {
    let tmp = TempDir::new().unwrap();
    // Build the historical four-shard shape explicitly, then remove its
    // modern flag to simulate a store from before count persistence.
    let inst = Instance::open(cfg_with(&tmp, 4)).expect("create with legacy layout");
    assert_eq!(inst.title_shard_count(), 4);
    import_titles(&inst, 32);
    drop(inst);

    // Rewind to the legacy state: no flag row.
    Connection::open(tmp.path().join("meta.db"))
        .unwrap()
        .execute("DELETE FROM instance_flags WHERE key = 'title_shard_count'", [])
        .unwrap();
    assert_eq!(persisted_flag(&tmp), None, "legacy state verified");

    let r = Instance::open_read(read_config(tmp.path().to_path_buf()))
        .expect("read-side open of a legacy store");
    assert_eq!(r.title_shard_count(), 4, "legacy default is 4");
    assert_exact_lookup_one_shard(&r);
    drop(r);
    assert_eq!(persisted_flag(&tmp), None, "a reader never backfills the flag");

    let w = Instance::open(cfg_with(&tmp, 0)).expect("writer open of a legacy store");
    assert_eq!(w.title_shard_count(), 4);
    assert_eq!(persisted_flag(&tmp), Some(4), "the writer backfilled the flag");
    assert_exact_lookup_one_shard(&w);
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
    let dangling: i64 = conn
        .query_row(
            "SELECT
               (SELECT COUNT(*) FROM page_to_title_id p
                LEFT JOIN title_id_to_page t ON t.title_id=p.title_id
                WHERE t.title_id IS NULL)
             + (SELECT COUNT(*) FROM title_intervals i
                LEFT JOIN title_id_to_page t ON t.title_id=i.title_id
                WHERE i.title_id IS NOT NULL AND t.title_id IS NULL)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dangling, 0, "all dependent dense ids were remapped");
}

#[test]
fn uncommitted_new_generation_is_ignored_on_reopen() {
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(cfg_with(&tmp, 4)).expect("create");
    import_titles(&inst, 16);
    drop(inst);

    // This is the filesystem state immediately before the SQLite switch:
    // a complete-or-partial new generation can exist, but no flag selects it.
    std::fs::create_dir(tmp.path().join("titles-g99")).unwrap();
    std::fs::write(tmp.path().join("titles-g99/shard-0000"), []).unwrap();
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
        "writer reopen collects the uncommitted generation"
    );
    for name in ["titles-g0", "titles-g01", "titles-backup"] {
        assert!(
            tmp.path().join(name).is_dir(),
            "GC ignores non-canonical or unrelated directory {name}"
        );
    }
}
