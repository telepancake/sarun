//! Read-only browsing directly from a portable Wikipedia archive.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::archive::{
    index_file, visit_frame, visit_frame_while, EntityKind, FrameLocation, ManifestRecord,
    Record, RevisionRecord, SiteInfoRecord,
};

#[derive(Debug)]
pub struct ArchiveBrowseIndex {
    path: PathBuf,
    frames: Vec<FrameLocation>,
    titles: HashMap<String, u64>,
    pages: Vec<(u64, String)>,
    site_info: SiteInfoRecord,
    manifest: Option<ManifestRecord>,
}

pub struct ArchiveAsOfView<'a> {
    archive: &'a ArchiveBrowseIndex,
    timestamp_micros: Option<i64>,
    render_timestamp_micros: i64,
    site: wikimak_wikitext::SiteConfig,
}

struct FrameScan {
    pages: Vec<(u64, Vec<String>)>,
}

impl ArchiveBrowseIndex {
    pub fn open(path: impl AsRef<Path>) -> crate::archive::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (_, frames, complete) = index_file(&path)?;
        if !complete {
            return Err(crate::archive::ArchiveError::Invalid(
                "archive has no clean completion marker",
            ));
        }
        let mut titles = HashMap::<String, u64>::new();
        let mut page_titles = BTreeMap::<u64, String>::new();
        let mut manifest = None;
        let mut site_info = None;

        let page_frame_numbers: Vec<usize> = frames
            .iter()
            .enumerate()
            .filter_map(|(frame_number, location)| {
                (location.info.first_entity.kind == EntityKind::Page).then_some(frame_number)
            })
            .collect();
        let next_frame = AtomicUsize::new(0);
        let worker_count = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(page_frame_numbers.len().max(1));
        let scanned = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                handles.push(scope.spawn(|| {
                    let mut output = Vec::new();
                    loop {
                        let job = next_frame.fetch_add(1, Ordering::Relaxed);
                        let Some(&frame_number) = page_frame_numbers.get(job) else {
                            break;
                        };
                        output.push(scan_page_frame(&path, &frames[frame_number])?);
                    }
                    crate::archive::Result::Ok(output)
                }));
            }
            let mut output = Vec::new();
            for handle in handles {
                output.extend(
                    handle
                        .join()
                        .map_err(|_| {
                            crate::archive::ArchiveError::Invalid(
                                "archive index worker panicked",
                            )
                        })??,
                );
            }
            crate::archive::Result::Ok(output)
        })?;

        for scan in scanned {
            for (page_id, current_titles) in scan.pages {
                for current_title in current_titles {
                    titles.insert(normalize_title_key(&current_title), page_id);
                    page_titles
                        .entry(page_id)
                        .and_modify(|title| {
                            if current_title < *title {
                                *title = current_title.clone();
                            }
                        })
                        .or_insert(current_title);
                }
            }
        }
        for location in &frames {
            if location.info.first_entity.kind != EntityKind::Global {
                continue;
            }
            visit_frame(&path, location, |record| {
                match record {
                    Record::Manifest {
                        manifest: record, ..
                    } => manifest = Some(record),
                    Record::SiteInfo {
                        site_info: record, ..
                    } => site_info = Some(record),
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
            pages: page_titles.into_iter().collect(),
            site_info,
            manifest,
        })
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn title_count(&self) -> usize {
        self.titles.len()
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn manifest(&self) -> Option<&ManifestRecord> {
        self.manifest.as_ref()
    }

    pub fn page_id_by_title(&self, title: &str) -> Option<u64> {
        self.titles.get(&normalize_title_key(title)).copied()
    }

    pub fn pages(&self, filter: Option<&str>, limit: usize) -> Vec<(u64, String)> {
        let filter = filter.map(|value| value.to_lowercase());
        self.pages
            .iter()
            .filter(|(_, title)| {
                filter
                    .as_ref()
                    .is_none_or(|needle| title.to_lowercase().contains(needle))
            })
            .take(limit)
            .cloned()
            .collect()
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
            server: server_from_base(&self.site_info.base),
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
        ArchiveAsOfView {
            archive: self,
            timestamp_micros,
            render_timestamp_micros: timestamp_micros
                .unwrap_or_else(|| chrono::Utc::now().timestamp_micros()),
            site,
        }
    }
}

fn scan_page_frame(
    path: &Path,
    location: &FrameLocation,
) -> crate::archive::Result<FrameScan> {
    let mut pages = Vec::<(u64, Vec<String>)>::new();
    visit_frame(path, location, |record| {
        let page_id = record.page_id().expect("page frame contains page records");
        if pages.last().is_none_or(|(last, _)| *last != page_id) {
            pages.push((page_id, Vec::new()));
        }
        if let Record::PageState { current_title, .. } = record {
            pages.last_mut().expect("inserted above").1.push(current_title);
        }
        Ok(())
    })?;
    Ok(FrameScan { pages })
}

fn server_from_base(base: &str) -> String {
    let Some(scheme_end) = base.find("://") else {
        return String::new();
    };
    let host_start = scheme_end + 3;
    let host_end = base[host_start..]
        .find('/')
        .map_or(base.len(), |offset| host_start + offset);
    base[..host_end].to_string()
}

fn normalize_title_key(title: &str) -> String {
    title.replace('_', " ").trim().to_string()
}

impl wikimak_wikitext::PageStore for ArchiveAsOfView<'_> {
    fn page_text(&self, title: &wikimak_wikitext::Title) -> Option<String> {
        let title = title.prefixed(&self.site);
        let page_id = self.archive.page_id_by_title(&title)?;
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
            .page_id_by_title(&title.prefixed(&self.site))
            .is_some()
    }

    fn page_id(&self, title: &wikimak_wikitext::Title) -> Option<u64> {
        self.archive
            .page_id_by_title(&title.prefixed(&self.site))
    }

    fn page_count(&self, namespace: Option<i32>) -> Option<u64> {
        namespace
            .is_none()
            .then_some(self.archive.page_count() as u64)
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
                timestamp_micros: i64::MAX,
                current_title: "Testa lapa".into(),
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
            .write(&Record::SiteInfo {
                timestamp_micros: i64::MAX,
                site_info: SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "testwiki".into(),
                    base: "https://test.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    namespaces: vec![crate::archive::SiteNamespaceRecord {
                        id: 0,
                        case: "first-letter".into(),
                        localized_name: String::new(),
                        aliases: Vec::new(),
                    }],
                    interwiki: Vec::new(),
                },
            })
            .unwrap();
        writer.finish().unwrap();

        let index = ArchiveBrowseIndex::open(&path).unwrap();
        assert_eq!(index.page_count(), 1);
        assert_eq!(index.page_id_by_title("Testa_lapa"), Some(7));
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
