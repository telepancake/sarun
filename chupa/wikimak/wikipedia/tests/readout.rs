//! `PageReadout` over an installed, immutable portable-archive generation.
//!
//! The fixture deliberately uses the same public archive and title-index
//! APIs as callers that construct a `.swdump`/`.swtitle` pair.  Publication is
//! represented by the current selector contract: immutable generation files
//! are prepared first, then the logical archive's `.swtitle` selector is
//! atomically replaced.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use depot::variant::{Blob, Readout, ReadoutKind};
use tempfile::{Builder, TempDir};
use wikimak_wikipedia::archive::{
    ArchiveWriter, CompressionSettings, ManifestRecord, Record, RevisionRecord, SiteInfoRecord,
    SiteNamespaceRecord,
};
use wikimak_wikipedia::readout::PageReadout;
use wikimak_wikipedia::{
    ContributorMeta, RevisionMeta,
    archive_browse::ArchiveBrowseIndex,
    archive_set::{ArchiveSetOutput, ArchiveSetReader},
    generation::GenerationId,
    title_index,
};

const PAGE: u64 = 7;

struct GenerationFixture {
    archive: PathBuf,
    title_index: PathBuf,
    id: GenerationId,
}

/// Keep every test-created directory on the explicitly supplied external
/// volume.  The exact test command sets both this and TMPDIR so dependencies
/// that use the process temporary directory follow the same policy.
fn temp_root() -> TempDir {
    let root = std::env::var_os("SARUN_TEST_STORAGE_ROOT")
        .map(PathBuf::from)
        .expect("SARUN_TEST_STORAGE_ROOT must point at the external test volume");
    assert!(
        root.starts_with("/Volumes/Elements"),
        "test root escaped external volume: {root:?}"
    );
    Builder::new()
        .prefix("sarun-readout-")
        .tempdir_in(root)
        .expect("create readout test directory on external volume")
}

fn timestamp(value: &str) -> DateTime<Utc> {
    value
        .parse::<chrono::DateTime<chrono::FixedOffset>>()
        .expect("fixture timestamp")
        .with_timezone(&Utc)
}

fn revision(rev_id: u64, parent_id: u64, when: &str, text: &str) -> RevisionRecord {
    RevisionRecord {
        meta: RevisionMeta {
            rev_id,
            parent_id,
            ts: timestamp(when),
            contributor: ContributorMeta::Named {
                username: "A".into(),
                user_id: 1,
            },
            comment: format!("r{rev_id}"),
            sha1: String::new(),
            flags: 0,
            text_len: text.len() as u64,
        },
        has_text: true,
        text: text.as_bytes().to_vec(),
        visibility: None,
        history: None,
    }
}

fn build_generation(parent: &Path, name: &str, include_new_head: bool) -> GenerationFixture {
    let archive = parent.join(format!("{name}.swdump"));
    let title_index_path = archive.with_extension("swtitle");
    let id = GenerationId::from_plan_bytes(name.as_bytes());

    let output = ArchiveSetOutput::new_in(parent, 1 << 20).unwrap();
    let mut writer = ArchiveWriter::with_ref_prefix(
        output,
        128,
        CompressionSettings::default(),
        b"readout fixture reference prefix",
    )
    .unwrap();
    writer
        .write(&Record::PageState {
            page_id: PAGE,
            timestamp_micros: i64::MAX,
            title: "Sarun/Design".into(),
            namespace: Some(0),
            deleted: false,
        })
        .unwrap();
    if include_new_head {
        writer
            .write(&Record::Revision {
                page_id: PAGE,
                revision: revision(102, 101, "2024-01-03T00:00:00Z", "newer head"),
            })
            .unwrap();
    }
    writer
        .write(&Record::Revision {
            page_id: PAGE,
            revision: revision(101, 100, "2024-01-02T00:00:00Z", "head text"),
        })
        .unwrap();
    writer
        .write(&Record::Revision {
            page_id: PAGE,
            revision: revision(100, 0, "2024-01-01T00:00:00Z", "old"),
        })
        .unwrap();
    writer
        .write(&Record::Manifest {
            timestamp_micros: 0,
            manifest: ManifestRecord {
                wiki_db: "readoutwiki".into(),
                content_snapshot: name.into(),
                metadata_snapshot: name.into(),
                source_files: Vec::new(),
            },
        })
        .unwrap();
    writer
        .write(&Record::SiteInfo {
            timestamp_micros: 0,
            site_info: SiteInfoRecord {
                site_name: "Readout test wiki".into(),
                db_name: "readoutwiki".into(),
                base: "https://example.invalid/wiki/Main_Page".into(),
                generator: "MediaWiki".into(),
                case: "first-letter".into(),
                language: "en".into(),
                rtl: false,
                server: "https://example.invalid".into(),
                script_path: "/w".into(),
                namespaces: vec![SiteNamespaceRecord {
                    id: 0,
                    case: "first-letter".into(),
                    localized_name: String::new(),
                    aliases: Vec::new(),
                }],
                interwiki: Vec::new(),
                magic_words: Vec::new(),
            },
        })
        .unwrap();
    let (output, _) = writer.finish().unwrap();
    output.finish().unwrap().persist(&archive).unwrap();
    title_index::build(&archive, &title_index_path, &id).unwrap();

    GenerationFixture {
        archive,
        title_index: title_index_path,
        id,
    }
}

/// Prepare one immutable generation, then publish only its selector.  This
/// mirrors the production on-disk contract without reaching into the
/// installation module, which is intentionally private to the library.
fn publish_generation(fixture: &GenerationFixture, destination: &Path) -> PathBuf {
    let generations = destination.with_extension("generations");
    std::fs::create_dir_all(&generations).unwrap();
    let generation = generations.join(fixture.id.as_str());
    std::fs::create_dir_all(&generation).unwrap();

    let archive = ArchiveSetReader::open(&fixture.archive).unwrap();
    for segment in archive.segments() {
        std::fs::copy(
            fixture.archive.join(&segment.name),
            generation.join(&segment.name),
        )
        .unwrap();
    }
    std::fs::File::open(&generation.join("9999-complete.swdump-part"))
        .unwrap()
        .sync_all()
        .unwrap();

    let pending = generations.join(format!("{}.swtitle.pending", fixture.id.as_str()));
    std::fs::copy(&fixture.title_index, &pending).unwrap();
    std::fs::File::open(&pending).unwrap().sync_all().unwrap();
    let selector = destination.with_extension("swtitle");
    std::fs::rename(pending, &selector).unwrap();
    std::fs::File::open(&generations)
        .unwrap()
        .sync_all()
        .unwrap();
    selector
}

fn mirrored() -> (TempDir, PathBuf, GenerationFixture, GenerationFixture) {
    let tmp = temp_root();
    let first = build_generation(tmp.path(), "first", false);
    let second = build_generation(tmp.path(), "second", true);
    let destination = tmp.path().join("readout.swdump");
    publish_generation(&first, &destination);
    (tmp, destination, first, second)
}

#[test]
fn serves_exactly_the_pinned_revision_and_sanitizes_the_leaf() {
    let (_tmp, destination, _first, _second) = mirrored();
    // `/` in the title sanitizes to `_` — the leaf name is one component.
    let readout = PageReadout::new(destination.clone(), PAGE, Some("Sarun/Design"), 101);
    let name = b"Sarun_Design.txt".to_vec();

    let root = readout.entry(&[]).unwrap();
    assert_eq!(root.kind, ReadoutKind::Branch);
    assert_eq!(root.blob_len, None);
    assert_eq!(readout.children(&[]), vec![name.clone()]);

    let leaf = readout.entry(&[&name]).unwrap();
    assert_eq!(leaf.kind, ReadoutKind::Leaf);
    assert_eq!(leaf.blob_len, Some(9));
    assert_eq!(
        readout.blob(&[&name]),
        Some(Blob::Bytes(b"head text".to_vec()))
    );
    assert!(readout.children(&[&name]).is_empty());

    let old = PageReadout::new(destination, PAGE, Some("Sarun/Design"), 100);
    assert_eq!(old.blob(&[&name]), Some(Blob::Bytes(b"old".to_vec())));
    assert_eq!(old.entry(&[&name]).unwrap().blob_len, Some(3));
}

#[test]
fn metadata_lookup_and_text_lookup_are_separate_bounded_streams() {
    let (_tmp, destination, _first, _second) = mirrored();
    let archive = ArchiveBrowseIndex::open_installed(&destination).unwrap();

    // Metadata lookup must identify the revision and its exact length without
    // asking the full RevisionRecord decoder for the page text.
    let (index, summary) = archive
        .revision_metadata(PAGE, 101, usize::MAX)
        .unwrap()
        .expect("pinned revision metadata");
    assert_eq!(index, 0);
    assert_eq!(summary.revision_id, 101);
    assert_eq!(summary.text_len, 9);
    assert!(summary.has_text);

    // The text cursor is independently positioned by that metadata index and
    // does not retain a page-history cache when the limit is zero.
    let text = archive
        .revision_text_at_index(PAGE, index, 0, usize::MAX)
        .unwrap()
        .expect("pinned revision text");
    assert_eq!(&*text, b"head text");
}

#[test]
fn id_fallback_name_is_a_single_leaf() {
    let (_tmp, destination, _first, _second) = mirrored();
    let readout = PageReadout::new(destination, PAGE, None, 101);
    assert_eq!(readout.children(&[]), vec![b"page-7.txt".to_vec()]);
    assert_eq!(readout.entry(&[b"wrong.txt"]), None);
    assert_eq!(readout.blob(&[b"wrong.txt"]), None);
    assert_eq!(readout.entry(&[b"page-7.txt", b"deeper"]), None);
    assert_eq!(readout.blob(&[]), None);
}

#[test]
fn missing_page_revision_or_store_is_a_miss() {
    let (_tmp, destination, _first, _second) = mirrored();

    let missing_page = PageReadout::new(destination.clone(), 42, Some("Nope"), 100);
    assert_eq!(missing_page.entry(&[]), None);
    assert!(missing_page.children(&[]).is_empty());
    assert_eq!(missing_page.blob(&[b"Nope.txt"]), None);

    let missing_revision = PageReadout::new(destination, PAGE, Some("Sarun/Design"), 999);
    assert_eq!(missing_revision.entry(&[]), None);
    assert_eq!(missing_revision.blob(&[b"Sarun_Design.txt"]), None);

    let ghost = temp_root().path().join("no-such-store.swdump");
    let missing_store = PageReadout::new(ghost.clone(), PAGE, Some("Sarun/Design"), 101);
    assert_eq!(missing_store.entry(&[]), None);
    assert_eq!(missing_store.blob(&[b"Sarun_Design.txt"]), None);
    assert!(!ghost.exists());
    assert!(!ghost.with_extension("swtitle").exists());
    assert!(!ghost.with_extension("generations").exists());
}

#[test]
fn failed_open_is_retried_after_the_store_becomes_readable() {
    let tmp = temp_root();
    let generation = build_generation(tmp.path(), "late", false);
    let destination = tmp.path().join("late.swdump");
    let readout = PageReadout::new(destination.clone(), PAGE, Some("Sarun/Design"), 101);
    let name = b"Sarun_Design.txt";

    // The first lookup fails before any selector or generation exists. That
    // failure must not become a permanent negative cache entry.
    assert_eq!(readout.blob(&[name]), None);

    publish_generation(&generation, &destination);
    assert_eq!(
        readout.blob(&[name]),
        Some(Blob::Bytes(b"head text".to_vec()))
    );
}

#[test]
fn resolved_readout_survives_publication_and_fresh_reads_follow_selector() {
    let (_tmp, destination, _first, second) = mirrored();
    let name = b"Sarun_Design.txt".to_vec();
    let attached = PageReadout::new(destination.clone(), PAGE, Some("Sarun/Design"), 101);
    assert_eq!(
        attached.blob(&[&name]),
        Some(Blob::Bytes(b"head text".to_vec()))
    );

    publish_generation(&second, &destination);

    // The readout has already resolved its bytes and remains pinned to that
    // value even though a new selector is now visible.
    assert_eq!(
        attached.blob(&[&name]),
        Some(Blob::Bytes(b"head text".to_vec()))
    );

    let fresh_new = PageReadout::new(destination.clone(), PAGE, None, 102);
    assert_eq!(
        fresh_new.blob(&[b"page-7.txt"]),
        Some(Blob::Bytes(b"newer head".to_vec()))
    );
    let fresh_old = PageReadout::new(destination, PAGE, Some("Sarun/Design"), 100);
    assert_eq!(fresh_old.blob(&[&name]), Some(Blob::Bytes(b"old".to_vec())));

    // The second generation is a full-history generation, not a head-only
    // replacement: the old pinned revision remains resolvable after publish.
}

#[test]
fn reader_lease_remains_valid_across_selector_publication() {
    let (_tmp, destination, first, second) = mirrored();
    let old_generation = destination
        .with_extension("generations")
        .join(first.id.as_str());
    let leased = ArchiveBrowseIndex::open_installed(&destination).unwrap();
    assert_eq!(
        leased.revision(PAGE, 101).unwrap().unwrap().text,
        b"head text"
    );

    publish_generation(&second, &destination);

    // Publication changes the selector, not the immutable generation opened
    // by an existing reader. The old generation must remain present while
    // that reader lease is alive and the leased index must still read it.
    assert!(old_generation.exists());
    assert_eq!(
        leased.revision(PAGE, 101).unwrap().unwrap().text,
        b"head text"
    );
    let selected = ArchiveBrowseIndex::open_installed(&destination).unwrap();
    assert_eq!(
        selected.revision(PAGE, 102).unwrap().unwrap().text,
        b"newer head"
    );
}
