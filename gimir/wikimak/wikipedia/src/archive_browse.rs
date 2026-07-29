//! Read-only browsing directly from a portable Wikipedia archive.

use std::path::{Path, PathBuf};

use crate::archive::{
    index_file, visit_frame, visit_frame_while, EntityKind, FrameLocation, ManifestRecord,
    Record, RevisionRecord, SiteInfoRecord,
};

#[derive(Debug)]
pub struct ArchiveBrowseIndex {
    path: PathBuf,
    frames: Vec<FrameLocation>,
    titles: crate::title_index::TitleIndex,
    site_info: SiteInfoRecord,
    manifest: Option<ManifestRecord>,
}

pub struct ArchiveAsOfView<'a> {
    archive: &'a ArchiveBrowseIndex,
    timestamp_micros: Option<i64>,
    render_timestamp_micros: i64,
    site: wikimak_wikitext::SiteConfig,
}

impl ArchiveBrowseIndex {
    pub fn open(
        path: impl AsRef<Path>,
        title_index: impl AsRef<Path>,
    ) -> crate::archive::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (_, frames, complete) = index_file(&path)?;
        if !complete {
            return Err(crate::archive::ArchiveError::Invalid(
                "archive has no clean completion marker",
            ));
        }
        let titles = crate::title_index::TitleIndex::open(title_index)?;
        let mut manifest = None;
        let mut site_info = None;
        for location in &frames {
            if location.info.first_entity.kind != EntityKind::Global {
                continue;
            }
            visit_frame(&path, location, |record| {
                match record {
                    Record::Manifest {
                        manifest: record, ..
                    } if manifest.is_none() => manifest = Some(record),
                    Record::SiteInfo {
                        site_info: record, ..
                    } if site_info.is_none() => site_info = Some(record),
                    _ => {}
                }
                Ok(())
            })?;
        }
        let site_info = site_info.ok_or(crate::archive::ArchiveError::Invalid(
            "archive has no siteinfo record",
        ))?;

        Ok(Self {
            path,
            frames,
            titles,
            site_info,
            manifest,
        })
    }

    pub fn title_count(&self) -> u64 {
        self.titles.entries()
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn manifest(&self) -> Option<&ManifestRecord> {
        self.manifest.as_ref()
    }

    pub fn page_id_by_title(&self, title: &str, timestamp: i64) -> Option<u64> {
        self.titles.lookup(title, timestamp, &self.site_info)
    }

    pub fn pages(&self, _filter: Option<&str>, _limit: usize) -> Vec<(u64, String)> {
        Vec::new()
    }

    pub fn revision_at(
        &self,
        page_id: u64,
        timestamp_micros: i64,
    ) -> crate::archive::Result<Option<RevisionRecord>> {
        let Some(location) = self.page_frame(page_id) else {
            return Ok(None);
        };
        let mut selected = None;
        visit_frame_while(&self.path, location, |record| {
            if record.page_id() != Some(page_id) {
                return Ok(true);
            }
            let Record::Revision { revision, .. } = record else {
                return Ok(true);
            };
            if revision.meta.ts.timestamp_micros() > timestamp_micros {
                return Ok(true);
            }
            selected = Some(revision);
            Ok(false)
        })?;
        Ok(selected)
    }

    fn page_frame(&self, page_id: u64) -> Option<&FrameLocation> {
        let page_frame_count = self.frames.partition_point(|location| {
            location.info.first_entity.kind == EntityKind::Page
        });
        let frames = &self.frames[..page_frame_count];
        let index = frames.partition_point(|location| location.info.last_entity.id < page_id);
        frames.get(index).filter(|location| {
            location.info.first_entity.id <= page_id
                && page_id <= location.info.last_entity.id
        })
    }

    pub fn page_text_at(
        &self,
        page_id: u64,
        timestamp_micros: i64,
    ) -> crate::archive::Result<Option<Vec<u8>>> {
        Ok(self
            .revision_at(page_id, timestamp_micros)?
            .and_then(|revision| revision.has_text.then_some(revision.text)))
    }

    pub fn view(&self, timestamp_micros: Option<i64>) -> ArchiveAsOfView<'_> {
        let mut site = wikimak_wikitext::SiteConfig {
            site_name: self.site_info.site_name.clone(),
            db_name: self.site_info.db_name.clone(),
            lang: self.site_info.language.clone(),
            rtl: self.site_info.rtl,
            server: self.site_info.server.clone(),
            script_path: self.site_info.script_path.clone(),
            ..wikimak_wikitext::SiteConfig::default()
        };
        site.interwiki = self
            .site_info
            .interwiki
            .iter()
            .map(|interwiki| {
                (
                    interwiki.prefix.to_lowercase(),
                    wikimak_wikitext::InterwikiEntry {
                        prefix: interwiki.prefix.clone(),
                        url: interwiki.url.clone(),
                        local_instance: None,
                    },
                )
            })
            .collect();
        for namespace in &self.site_info.namespaces {
            site.namespaces.insert(
                namespace.id,
                wikimak_wikitext::NamespaceInfo {
                    id: namespace.id,
                    canonical: String::new(),
                    localized: namespace.localized_name.clone(),
                    aliases: namespace.aliases.clone(),
                    case_first_letter: namespace.case == "first-letter",
                },
            );
        }
        for word in &self.site_info.magic_words {
            for alias in &word.aliases {
                site.magic_aliases
                    .insert(alias.clone(), word.canonical_name.clone());
                if !word.case_sensitive {
                    site.magic_aliases
                        .insert(alias.to_lowercase(), word.canonical_name.clone());
                }
            }
        }
        ArchiveAsOfView {
            archive: self,
            timestamp_micros,
            render_timestamp_micros: timestamp_micros
                .unwrap_or_else(|| chrono::Utc::now().timestamp_micros()),
            site,
        }
    }
}

impl wikimak_wikitext::PageStore for ArchiveAsOfView<'_> {
    fn page_text(&self, title: &wikimak_wikitext::Title) -> Option<String> {
        let title = title.prefixed(&self.site);
        let at = self.timestamp_micros.unwrap_or(i64::MAX);
        let page_id = self.archive.page_id_by_title(&title, at)?;
        let text = self
            .archive
            .page_text_at(
                page_id,
                self.timestamp_micros.unwrap_or(i64::MAX),
            )
            .ok()
            .flatten()?;
        Some(String::from_utf8_lossy(&text).into_owned())
    }

    fn page_exists(&self, title: &wikimak_wikitext::Title) -> bool {
        self.archive
            .page_id_by_title(
                &title.prefixed(&self.site),
                self.timestamp_micros.unwrap_or(i64::MAX),
            )
            .is_some()
    }

    fn page_id(&self, title: &wikimak_wikitext::Title) -> Option<u64> {
        self.archive
            .page_id_by_title(
                &title.prefixed(&self.site),
                self.timestamp_micros.unwrap_or(i64::MAX),
            )
    }

    fn page_count(&self, namespace: Option<i32>) -> Option<u64> {
        let _ = namespace;
        None
    }

    fn site(&self) -> &wikimak_wikitext::SiteConfig {
        &self.site
    }

    fn timestamp_micros(&self) -> i64 {
        self.render_timestamp_micros
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{ArchiveWriter, Record};
    use crate::{ContributorMeta, RevisionMeta};
    use chrono::{TimeZone, Utc};

    #[test]
    fn indexes_titles_and_reads_one_page_from_its_frame() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("sample.swdump");
        let mut writer = ArchiveWriter::new(std::fs::File::create(&path).unwrap(), 1).unwrap();
        writer
            .write(&Record::PageState {
                page_id: 7,
                timestamp_micros: Utc.timestamp_opt(200, 0).unwrap().timestamp_micros(),
                title: "Testa lapa".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        writer
            .write(&Record::Revision {
                page_id: 7,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: 11,
                        parent_id: 0,
                        ts: Utc.timestamp_opt(123, 0).unwrap(),
                        contributor: ContributorMeta::Hidden,
                        comment: String::new(),
                        sha1: String::new(),
                        flags: 0,
                        text_len: 5,
                    },
                    has_text: true,
                    text: b"hello".to_vec(),
                    visibility: None,
                    history: None,
                },
            })
            .unwrap();
        writer
            .write(&Record::Revision {
                page_id: 7,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: 10,
                        parent_id: 0,
                        ts: Utc.timestamp_opt(100, 0).unwrap(),
                        contributor: ContributorMeta::Hidden,
                        comment: String::new(),
                        sha1: String::new(),
                        flags: 0,
                        text_len: 3,
                    },
                    has_text: true,
                    text: b"old".to_vec(),
                    visibility: None,
                    history: None,
                },
            })
            .unwrap();
        writer
            .write(&Record::Manifest {
                timestamp_micros: i64::MAX,
                manifest: crate::archive::ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "2026-07-29".into(),
                    metadata_snapshot: "2026-07".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: i64::MAX,
                site_info: SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "testwiki".into(),
                    base: "https://test.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "lv".into(),
                    rtl: false,
                    server: "https://test.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: vec![crate::archive::SiteNamespaceRecord {
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

        let title_index = temporary.path().join("sample.swtitle");
        crate::title_index::build(&path, &title_index).unwrap();
        assert_eq!(std::fs::metadata(&title_index).unwrap().len(), 16);
        let index = ArchiveBrowseIndex::open(&path, &title_index).unwrap();
        assert_eq!(index.page_id_by_title("Testa_lapa", i64::MAX), Some(7));
        assert_eq!(
            index.page_text_at(7, i64::MAX).unwrap(),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            index.page_text_at(7, 110_000_000).unwrap(),
            Some(b"old".to_vec())
        );
        assert_eq!(index.page_text_at(7, 1).unwrap(), None);
    }
}
