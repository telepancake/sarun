use chrono::{TimeZone, Utc};
use wikimak_wikipedia::archive::{
    AccountClass, ArchiveWriter, EntityKey, EntityKind, ManifestRecord, PageActionKind,
    PageActionRecord, PerformerRecord, Record, RevisionRecord, RevisionVisibilityRecord,
    SiteInfoRecord, SiteNamespaceRecord, UserStateRecord,
};
use wikimak_wikipedia::{
    ContributorMeta, Instance, InstanceConfig, RevisionMeta, DEFAULT_MAX_CHAIN_ID,
};

fn instance(root: std::path::PathBuf) -> Instance {
    Instance::open(InstanceConfig {
        root,
        dbname: "testwiki".into(),
        max_chain_id: DEFAULT_MAX_CHAIN_ID,
        depot: wikimak_depot::DepotConfig {
            root: std::path::PathBuf::new(),
            max_chain_id: DEFAULT_MAX_CHAIN_ID,
            file_size_threshold: 1 << 20,
            eviction_dead_ratio: 0.5,
        },
        title_shard_count: 1,
        title_seal_threshold_bytes: 1 << 20,
        f1_seal_threshold_bytes: 1 << 20,
    })
    .unwrap()
}

#[test]
fn portable_archive_initializes_a_normal_depot() {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("source.swdump");
    let root = temporary.path().join("depot");
    let mut writer = ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 1024).unwrap();
    writer
        .write(&Record::PageState {
            page_id: 7,
            timestamp_micros: 200_000_000,
            title: "Testa lapa".into(),
            namespace: Some(0),
            deleted: false,
        })
        .unwrap();
    writer
        .write(&Record::PageAction {
            entity: EntityKey {
                kind: EntityKind::Page,
                id: 7,
            },
            timestamp_micros: 150_000_000,
            action: PageActionRecord {
                log_id: Some(19),
                tie_sequence: 1,
                kind: PageActionKind::Move,
                performer: PerformerRecord {
                    local_user_id: Some(3),
                    central_user_id: None,
                    historical_name: Some("Editor".into()),
                    account_class: AccountClass::Permanent,
                },
                comment: "rename".into(),
                title_at_event: "Vecais nosaukums".into(),
                namespace_at_event: Some(0),
                resulting_deleted: Some(false),
            },
        })
        .unwrap();
    writer
        .write(&Record::Revision {
            page_id: 7,
            revision: RevisionRecord {
                meta: RevisionMeta {
                    rev_id: 11,
                    parent_id: 0,
                    ts: Utc.timestamp_opt(100, 0).unwrap(),
                    contributor: ContributorMeta::Named {
                        username: "Editor".into(),
                        user_id: 3,
                    },
                    comment: "initial".into(),
                    sha1: String::new(),
                    flags: 0,
                    text_len: 5,
                },
                has_text: true,
                text: b"hello".to_vec(),
                visibility: Some(RevisionVisibilityRecord {
                    deleted_parts: 2,
                    parts_are_suppressed: false,
                    deleted_by_page_deletion: false,
                    page_deletion_timestamp_micros: None,
                }),
                history: None,
            },
        })
        .unwrap();
    writer
        .write(&Record::UserState {
            user_id: 3,
            timestamp_micros: 100_000_000,
            state: UserStateRecord {
                current_name: Some("Editor".into()),
                central_user_id: None,
                account_class: AccountClass::Permanent,
                groups: Vec::new(),
                blocks: Vec::new(),
                bot_by: Vec::new(),
            },
        })
        .unwrap();
    writer
        .write(&Record::Manifest {
            timestamp_micros: 200_000_000,
            manifest: ManifestRecord {
                wiki_db: "testwiki".into(),
                content_snapshot: "2026-07-01".into(),
                metadata_snapshot: "2026-06".into(),
                source_files: Vec::new(),
            },
        })
        .unwrap();
    writer
        .write(&Record::SiteInfo {
            timestamp_micros: 200_000_000,
            site_info: SiteInfoRecord {
                site_name: "Test Wiki".into(),
                db_name: "testwiki".into(),
                base: "https://example.invalid/wiki/Main_Page".into(),
                generator: "MediaWiki".into(),
                case: "first-letter".into(),
                language: "lv".into(),
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
    writer.finish().unwrap();

    let instance = instance(root.clone());
    let stats =
        wikimak_wikipedia::archive::import_instance(&instance, &archive, |_| {}).unwrap();
    assert_eq!(stats.pages, 1);
    assert_eq!(stats.revisions, 1);
    assert_eq!(stats.page_actions, 1);
    assert_eq!(stats.user_records, 1);
    assert_eq!(
        instance.page_id_by_title_at("Testa lapa", None).unwrap(),
        Some(7)
    );
    assert_eq!(instance.page_head_text(7).unwrap(), Some(b"hello".to_vec()));
    assert_eq!(instance.page_actions(7).unwrap().len(), 1);
    assert_eq!(
        instance
            .revision_visibility(11)
            .unwrap()
            .unwrap()
            .deleted_parts,
        "comment"
    );
    assert_eq!(
        instance.sync_state("wiki_dbname").unwrap().as_deref(),
        Some("testwiki")
    );
    assert!(root.join("history-users-archive.swdump").is_file());
}

#[test]
fn archive_import_pretrains_before_writing_f0() {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("source.swdump");
    let root = temporary.path().join("depot");
    let mut writer =
        ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 1 << 20).unwrap();
    for page_id in 1..=128 {
        writer
            .write(&Record::PageState {
                page_id,
                timestamp_micros: 200_000_000,
                title: format!("Page {page_id}"),
                namespace: Some(0),
                deleted: false,
            })
            .unwrap();
        let mut text = format!("== Page {page_id} ==\n").into_bytes();
        while text.len() < 8 << 10 {
            text.extend_from_slice(
                b"Representative encyclopedia prose, links, templates, and table markup. ",
            );
        }
        writer
            .write(&Record::Revision {
                page_id,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: page_id,
                        parent_id: 0,
                        ts: Utc.timestamp_opt(100, 0).unwrap(),
                        contributor: ContributorMeta::Anonymous {
                            ip: "192.0.2.1".into(),
                        },
                        comment: "seed".into(),
                        sha1: String::new(),
                        flags: 0,
                        text_len: text.len() as u64,
                    },
                    has_text: true,
                    text,
                    visibility: None,
                    history: None,
                },
            })
            .unwrap();
    }
    writer.finish().unwrap();

    let instance = instance(root.clone());
    wikimak_wikipedia::archive::import_instance(&instance, &archive, |_| {}).unwrap();
    drop(instance);

    let depot = wikimak_depot::Depot::open(wikimak_depot::DepotConfig {
        root: root.join("depot"),
        max_chain_id: DEFAULT_MAX_CHAIN_ID,
        file_size_threshold: 1 << 20,
        eviction_dead_ratio: 0.5,
    })
    .unwrap();
    let f0 = depot.read_f0(1).unwrap();
    assert!(
        zstd::zstd_safe::get_dict_id_from_frame(&f0).is_some(),
        "the first imported page must already use the seed dictionary"
    );
}
