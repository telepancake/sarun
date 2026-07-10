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
    // Derive on a fresh root = the CLI default 4 (what every legacy
    // store was actually built with).
    let inst = Instance::open(cfg_with(&tmp, 0)).expect("create (derive → 4)");
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
