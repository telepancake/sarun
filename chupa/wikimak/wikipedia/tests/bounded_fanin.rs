use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use wikimak_wikipedia::archive::{
    ArchiveRecordReader, ArchiveWriter, CompressionSettings, Record, RevisionHistoryRecord,
    RevisionVisibilityRecord,
};
use wikimak_wikipedia::direct::merge_sorted_archives_bounded;
use wikimak_wikipedia::{ContributorMeta, RevisionMeta};

const SOURCE_COUNT: usize = 72;
const DUPLICATE_SOURCES: [usize; 3] = [0, 24, 48];

fn validation_root() -> PathBuf {
    let raw = PathBuf::from(
        env::var_os("TMPDIR").expect("bounded_fanin tests require an explicit TMPDIR"),
    );
    let canonical = fs::canonicalize(&raw).unwrap_or_else(|error| {
        panic!(
            "TMPDIR {} must be an existing directory: {error}",
            raw.display()
        )
    });
    if let Some(required_root) = env::var_os("SARUN_TEST_STORAGE_ROOT") {
        let required_root = fs::canonicalize(&required_root).unwrap_or_else(|error| {
            panic!(
                "SARUN_TEST_STORAGE_ROOT {} must be an existing directory: {error}",
                PathBuf::from(required_root).display()
            )
        });
        assert!(
            canonical != required_root && canonical.starts_with(&required_root),
            "TMPDIR {} must canonically be beneath SARUN_TEST_STORAGE_ROOT {}, not {}",
            raw.display(),
            required_root.display(),
            canonical.display()
        );
    }
    canonical
}

fn duplicate_revision(variant: usize) -> Record {
    let (contributor, comment, flags, has_text, text, text_len, visibility, history) = match variant
    {
        0 => (
            ContributorMeta::Named {
                username: "Zed".into(),
                user_id: 42,
            },
            "z-comment",
            0x01,
            false,
            Vec::new(),
            5,
            RevisionVisibilityRecord {
                deleted_parts: 0x01,
                parts_are_suppressed: false,
                deleted_by_page_deletion: false,
                page_deletion_timestamp_micros: Some(10),
            },
            RevisionHistoryRecord {
                minor: Some(false),
                content_model: Some("wikitext".into()),
                content_format: None,
                identity_reverted: None,
                first_reverting_revision_id: None,
                seconds_to_revert: None,
                identity_revert: None,
                before_page_creation: None,
                tags: vec!["beta".into()],
            },
        ),
        1 => (
            ContributorMeta::Named {
                username: "Alice".into(),
                user_id: 42,
            },
            "a-comment",
            0x04,
            true,
            b"hello".to_vec(),
            5,
            RevisionVisibilityRecord {
                deleted_parts: 0x04,
                parts_are_suppressed: true,
                deleted_by_page_deletion: true,
                page_deletion_timestamp_micros: Some(20),
            },
            RevisionHistoryRecord {
                minor: Some(true),
                content_model: Some("wikitext".into()),
                content_format: None,
                identity_reverted: None,
                first_reverting_revision_id: None,
                seconds_to_revert: None,
                identity_revert: None,
                before_page_creation: None,
                tags: vec!["alpha".into()],
            },
        ),
        2 => (
            ContributorMeta::Hidden,
            "",
            0x08,
            false,
            Vec::new(),
            5,
            RevisionVisibilityRecord {
                deleted_parts: 0x02,
                parts_are_suppressed: false,
                deleted_by_page_deletion: false,
                page_deletion_timestamp_micros: Some(15),
            },
            RevisionHistoryRecord {
                minor: None,
                content_model: Some("wikitext".into()),
                content_format: None,
                identity_reverted: None,
                first_reverting_revision_id: None,
                seconds_to_revert: None,
                identity_revert: None,
                before_page_creation: None,
                tags: vec!["beta".into(), "gamma".into()],
            },
        ),
        _ => panic!("unknown duplicate revision fixture variant {variant}"),
    };
    Record::Revision {
        page_id: 1,
        revision: wikimak_wikipedia::archive::RevisionRecord {
            meta: RevisionMeta {
                rev_id: 7,
                parent_id: 0,
                ts: Utc.timestamp_opt(100, 0).unwrap(),
                contributor,
                comment: comment.into(),
                sha1: String::new(),
                flags,
                text_len,
            },
            has_text,
            text,
            visibility: Some(visibility),
            history: Some(history),
        },
    }
}

fn unique_page_records(source_index: usize) -> [Record; 2] {
    let page_id = 1_000 + source_index as u64;
    [
        Record::PageState {
            page_id,
            timestamp_micros: (300 + source_index) as i64 * 1_000_000,
            title: format!("Fixture page {source_index:03}"),
            namespace: Some(0),
            deleted: false,
        },
        Record::Revision {
            page_id,
            revision: wikimak_wikipedia::archive::RevisionRecord {
                meta: RevisionMeta {
                    rev_id: 10_000 + source_index as u64,
                    parent_id: 0,
                    ts: Utc.timestamp_opt(200 + source_index as i64, 0).unwrap(),
                    contributor: ContributorMeta::Named {
                        username: format!("User {source_index:03}"),
                        user_id: 1_000 + source_index as u64,
                    },
                    comment: format!("unique revision {source_index:03}"),
                    sha1: String::new(),
                    flags: 0,
                    text_len: 0,
                },
                has_text: false,
                text: Vec::new(),
                visibility: None,
                history: None,
            },
        },
    ]
}

fn fixture_records(source_index: usize) -> Vec<Record> {
    let mut records = Vec::with_capacity(4);
    if let Some(variant) = DUPLICATE_SOURCES
        .iter()
        .position(|duplicate_source| *duplicate_source == source_index)
    {
        records.push(duplicate_revision(variant));
    }
    records.extend(unique_page_records(source_index));
    records
}

fn write_fixture(path: &Path, source_index: usize) {
    let mut writer = ArchiveWriter::new(File::create(path).unwrap(), 256).unwrap();
    for record in fixture_records(source_index) {
        writer.write(&record).unwrap();
    }
    writer.finish().unwrap();
}

fn read_records(path: &Path) -> Vec<Record> {
    let mut reader = ArchiveRecordReader::open(path).unwrap();
    let mut records = Vec::new();
    while let Some(record) = reader.next_record().unwrap() {
        records.push(record);
    }
    records
}

fn expected_records_in_archive_order() -> Vec<Record> {
    // Page entities are ascending, and each fixture page's state timestamp is
    // newer than its revision timestamp.
    let mut expected = vec![duplicate_revision(1)];
    for source_index in 0..SOURCE_COUNT {
        expected.extend(unique_page_records(source_index));
    }
    expected
}

fn assert_contract_records(records: &[Record]) {
    let expected_unique = (0..SOURCE_COUNT)
        .flat_map(unique_page_records)
        .collect::<Vec<_>>();
    assert_eq!(records.len(), expected_unique.len() + 1);

    for expected in &expected_unique {
        assert_eq!(
            records.iter().filter(|record| *record == expected).count(),
            1,
            "unique fixture record must occur exactly once: {expected:?}"
        );
    }

    let duplicate_records = records
        .iter()
        .filter(|record| {
            matches!(
                *record,
                Record::Revision {
                    page_id: 1,
                    revision
                } if revision.meta.rev_id == 7
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(duplicate_records.len(), 1);
    assert_eq!(*duplicate_records[0], duplicate_revision(1));

    assert_eq!(records, expected_records_in_archive_order().as_slice());
}

fn shuffled_source_order() -> Vec<usize> {
    // Multiplication by 37 is a permutation modulo 72. It keeps duplicate
    // source fixtures 0, 24, and 48 in separate 24-source batches, just as
    // the natural order does, while changing source order at every boundary.
    (0..SOURCE_COUNT)
        .map(|position| (position * 37) % SOURCE_COUNT)
        .collect()
}

#[test]
fn hierarchical_merge_preserves_complete_records_for_natural_and_shuffled_sources() {
    let root = tempfile::Builder::new()
        .prefix("bounded-fanin-")
        .tempdir_in(validation_root())
        .unwrap();
    let input_dir = root.path().join("inputs");
    fs::create_dir(&input_dir).unwrap();

    let mut fixture_paths = Vec::with_capacity(SOURCE_COUNT);
    for source_index in 0..SOURCE_COUNT {
        let path = input_dir.join(format!("source-{source_index:03}.swdump"));
        write_fixture(&path, source_index);
        fixture_paths.push(path);
    }

    let natural_inputs = fixture_paths.clone();
    let shuffled_inputs = shuffled_source_order()
        .into_iter()
        .map(|source_index| fixture_paths[source_index].clone())
        .collect::<Vec<_>>();
    let natural_output = root.path().join("natural.swdump");
    let shuffled_output = root.path().join("shuffled.swdump");
    let compression = CompressionSettings {
        level: 1,
        ..CompressionSettings::default()
    };

    let natural_stats =
        merge_sorted_archives_bounded(&natural_inputs, &natural_output, 256, compression).unwrap();
    let shuffled_stats =
        merge_sorted_archives_bounded(&shuffled_inputs, &shuffled_output, 256, compression)
            .unwrap();
    assert_eq!(natural_stats.1, (SOURCE_COUNT * 2 + 1) as u64);
    assert_eq!(shuffled_stats, natural_stats);

    let natural_records = read_records(&natural_output);
    let shuffled_records = read_records(&shuffled_output);
    assert_contract_records(&natural_records);
    assert_contract_records(&shuffled_records);
    assert_eq!(natural_records, shuffled_records);

    let generated_intermediates = fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("bounded-merge-")
        })
        .collect::<Vec<_>>();
    assert!(
        generated_intermediates.is_empty(),
        "owned intermediate workspaces must be cleaned after success"
    );
}
