//! Read-only browsing directly from a portable Wikipedia archive.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::archive::{
    index_file, visit_frame, visit_frame_while, visit_frame_while_file, EntityKind, FrameLocation,
    ManifestRecord, Record, RevisionRecord, SiteInfoRecord,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageRevisionSummary {
    pub revision_id: u64,
    pub parent_id: u64,
    pub timestamp_micros: i64,
    pub contributor: crate::ContributorMeta,
    pub comment: String,
    pub flags: u32,
    pub text_len: u64,
    pub has_text: bool,
    pub minor: Option<bool>,
    pub visibility: Option<crate::archive::RevisionVisibilityRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveSearchKind {
    Title,
    FullText,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveSearchHit {
    pub page_id: u64,
    pub title: String,
    pub revision_id: Option<u64>,
    pub timestamp_micros: Option<i64>,
    pub snippet: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveSearchResults {
    pub hits: Vec<ArchiveSearchHit>,
    pub match_count: u64,
    pub searched_frames: usize,
    pub workers: usize,
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

    pub fn site_info(&self) -> &SiteInfoRecord {
        &self.site_info
    }

    pub fn page_id_by_title(&self, title: &str, timestamp: i64) -> Option<u64> {
        self.titles.lookup(title, timestamp, &self.site_info)
    }

    pub fn pages(&self, filter: Option<&str>, limit: usize) -> Vec<(u64, String)> {
        let filter = filter.map(str::to_lowercase);
        let mut seen = std::collections::HashSet::new();
        let mut pages = Vec::new();
        for location in &self.frames {
            if location.info.first_entity.kind != EntityKind::Page || pages.len() >= limit {
                break;
            }
            let result = visit_frame_while(&self.path, location, |record| {
                if let Record::PageState {
                    page_id,
                    title,
                    deleted,
                    ..
                } = record
                {
                    if seen.insert(page_id)
                        && !deleted
                        && filter
                            .as_ref()
                            .is_none_or(|needle| title.to_lowercase().contains(needle))
                    {
                        pages.push((page_id, title));
                    }
                }
                Ok(pages.len() < limit)
            });
            if result.is_err() {
                break;
            }
        }
        pages
    }

    pub fn first_page(&self) -> crate::archive::Result<Option<(u64, String)>> {
        for location in &self.frames {
            if location.info.first_entity.kind != EntityKind::Page {
                break;
            }
            let mut found = None;
            visit_frame_while(&self.path, location, |record| {
                if let Record::PageState {
                    page_id,
                    title,
                    deleted,
                    ..
                } = record
                {
                    if !deleted {
                        found = Some((page_id, title));
                        return Ok(false);
                    }
                }
                Ok(true)
            })?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    pub fn current_title(
        &self,
        page_id: u64,
    ) -> crate::archive::Result<Option<String>> {
        let Some(location) = self.page_frame(page_id) else {
            return Ok(None);
        };
        let mut title = None;
        visit_frame_while(&self.path, location, |record| {
            if record.page_id() != Some(page_id) {
                return Ok(true);
            }
            if let Record::PageState {
                title: candidate,
                deleted,
                ..
            } = record
            {
                if !deleted {
                    title = Some(candidate);
                }
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(title)
    }

    pub fn page_title_at(
        &self,
        page_id: u64,
        timestamp_micros: i64,
    ) -> crate::archive::Result<Option<String>> {
        let Some(location) = self.page_frame(page_id) else {
            return Ok(None);
        };
        let mut current = None;
        let mut actions = Vec::new();
        visit_frame(&self.path, location, |record| {
            match record {
                Record::PageState {
                    page_id: record_page_id,
                    timestamp_micros,
                    title,
                    namespace,
                    deleted,
                } if record_page_id == page_id && current.is_none() => {
                    let title = namespace.map_or(title.clone(), |namespace| {
                        crate::title_index::title_in_namespace(
                            &title,
                            namespace,
                            &self.site_info,
                        )
                    });
                    current = Some((timestamp_micros, title, deleted));
                }
                Record::PageAction {
                    entity,
                    timestamp_micros,
                    action,
                } if entity.kind == EntityKind::Page && entity.id == page_id => {
                    actions.push((timestamp_micros, action));
                }
                _ => {}
            }
            Ok(())
        })?;
        if timestamp_micros == i64::MAX {
            return Ok(current.and_then(|(_, title, deleted)| (!deleted).then_some(title)));
        }

        actions.sort_by_key(|(timestamp, action)| (*timestamp, action.tie_sequence));
        let mut title = None;
        let mut exists = false;
        let mut observed_action = false;
        for (timestamp, action) in actions {
            if timestamp > timestamp_micros {
                break;
            }
            observed_action = true;
            let observed = crate::title_index::full_title(&action, &self.site_info);
            match action.kind {
                crate::archive::PageActionKind::Create
                | crate::archive::PageActionKind::LoggedCreate
                | crate::archive::PageActionKind::Move
                | crate::archive::PageActionKind::Restore => {
                    exists = true;
                    title = Some(observed);
                }
                crate::archive::PageActionKind::Delete
                    if action.resulting_deleted != Some(false) =>
                {
                    exists = false;
                }
                _ => {
                    title = Some(observed);
                    if let Some(deleted) = action.resulting_deleted {
                        exists = !deleted;
                    }
                }
            }
        }
        if observed_action {
            Ok(exists.then_some(title).flatten())
        } else {
            Ok(current.and_then(|(_, title, deleted)| (!deleted).then_some(title)))
        }
    }

    pub fn revision(
        &self,
        page_id: u64,
        revision_id: u64,
    ) -> crate::archive::Result<Option<RevisionRecord>> {
        let Some(location) = self.page_frame(page_id) else {
            return Ok(None);
        };
        let mut selected = None;
        visit_frame_while(&self.path, location, |record| {
            if record.page_id() != Some(page_id) {
                return Ok(true);
            }
            if let Record::Revision { revision, .. } = record {
                if revision.meta.rev_id == revision_id {
                    selected = Some(revision);
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        Ok(selected)
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

    /// Revision history for one page, in the archive's canonical
    /// newest-to-oldest order, without retaining revision text.
    pub fn page_revisions(
        &self,
        page_id: u64,
    ) -> crate::archive::Result<Vec<PageRevisionSummary>> {
        let Some(location) = self.page_frame(page_id) else {
            return Ok(Vec::new());
        };
        let mut revisions = Vec::new();
        visit_frame_while(&self.path, location, |record| {
            if record.page_id() != Some(page_id) {
                return Ok(true);
            }
            if let Record::Revision { revision, .. } = record {
                revisions.push(PageRevisionSummary {
                    revision_id: revision.meta.rev_id,
                    parent_id: revision.meta.parent_id,
                    timestamp_micros: revision.meta.ts.timestamp_micros(),
                    contributor: revision.meta.contributor,
                    comment: revision.meta.comment,
                    flags: revision.meta.flags,
                    text_len: revision.meta.text_len,
                    has_text: revision.has_text,
                    minor: revision.history.as_ref().and_then(|history| history.minor),
                    visibility: revision.visibility,
                });
            }
            Ok(true)
        })?;
        Ok(revisions)
    }

    /// Search page frames directly. Work is claimed one frame at a time, so
    /// compressed-frame decoding and regex matching use all available cores.
    ///
    /// Title search considers the newest non-deleted PageState for each page.
    /// Full-text search considers every retained revision with text.
    pub fn search_regex(
        &self,
        regex: &regex::Regex,
        kind: ArchiveSearchKind,
        limit: usize,
    ) -> crate::archive::Result<ArchiveSearchResults> {
        let page_frames = self
            .frames
            .iter()
            .take_while(|location| location.info.first_entity.kind == EntityKind::Page)
            .collect::<Vec<_>>();
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(page_frames.len().max(1));
        let next = AtomicUsize::new(0);
        let keep = limit.max(1);

        let partials = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for _ in 0..workers {
                handles.push(scope.spawn(|| -> crate::archive::Result<_> {
                    let mut file = std::fs::File::open(&self.path)?;
                    let mut found = Vec::new();
                    let mut match_count = 0_u64;
                    loop {
                        let frame_index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(location) = page_frames.get(frame_index) else {
                            break;
                        };
                        let mut states = std::collections::HashMap::<u64, Option<String>>::new();
                        let mut revisions = Vec::new();
                        let mut sequence = 0_usize;
                        visit_frame_while_file(&mut file, location, |record| {
                            let record_sequence = sequence;
                            sequence += 1;
                            match record {
                                Record::PageState {
                                    page_id,
                                    title,
                                    namespace,
                                    deleted,
                                    ..
                                } => {
                                    if let std::collections::hash_map::Entry::Vacant(entry) =
                                        states.entry(page_id)
                                    {
                                        let state = (!deleted).then(|| {
                                            namespace.map_or(title.clone(), |namespace| {
                                                crate::title_index::title_in_namespace(
                                                    &title,
                                                    namespace,
                                                    &self.site_info,
                                                )
                                            })
                                        });
                                        if kind == ArchiveSearchKind::Title
                                            && state
                                                .as_ref()
                                                .is_some_and(|title| regex.is_match(title))
                                        {
                                            match_count += 1;
                                            if found.len() < keep * 2 {
                                                found.push((
                                                    frame_index,
                                                    record_sequence,
                                                    ArchiveSearchHit {
                                                        page_id,
                                                        title: state.clone().unwrap_or_default(),
                                                        revision_id: None,
                                                        timestamp_micros: None,
                                                        snippet: None,
                                                    },
                                                ));
                                            }
                                        }
                                        entry.insert(state);
                                    }
                                }
                                Record::Revision { page_id, revision }
                                    if kind == ArchiveSearchKind::FullText
                                        && revision.has_text =>
                                {
                                    let text = String::from_utf8_lossy(&revision.text);
                                    if let Some(matched) = regex.find(&text) {
                                        match_count += 1;
                                        if revisions.len() < keep * 2 {
                                            revisions.push((
                                                record_sequence,
                                                page_id,
                                                revision.meta.rev_id,
                                                revision.meta.ts.timestamp_micros(),
                                                search_snippet(&text, matched.start(), matched.end()),
                                            ));
                                        }
                                    }
                                }
                                _ => {}
                            }
                            Ok(true)
                        })?;
                        if kind == ArchiveSearchKind::FullText {
                            for (record_sequence, page_id, revision_id, timestamp, snippet) in
                                revisions
                            {
                                let Some(Some(title)) = states.get(&page_id) else {
                                    continue;
                                };
                                if found.len() < keep * 2 {
                                    found.push((
                                        frame_index,
                                        record_sequence,
                                        ArchiveSearchHit {
                                            page_id,
                                            title: title.clone(),
                                            revision_id: Some(revision_id),
                                            timestamp_micros: Some(timestamp),
                                            snippet: Some(snippet),
                                        },
                                    ));
                                }
                            }
                        }
                        if found.len() >= keep * 2 {
                            found.sort_by_key(|(frame, sequence, _)| (*frame, *sequence));
                            found.truncate(keep);
                        }
                    }
                    Ok((found, match_count))
                }));
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| {
                        crate::archive::ArchiveError::Invalid("archive search worker panicked")
                    })?
                })
                .collect::<crate::archive::Result<Vec<_>>>()
        })?;

        let mut hits = Vec::with_capacity(keep.saturating_mul(workers));
        let mut match_count = 0_u64;
        for (mut worker_hits, worker_count) in partials {
            hits.append(&mut worker_hits);
            match_count = match_count.saturating_add(worker_count);
        }
        hits.sort_by_key(|(frame, sequence, _)| (*frame, *sequence));
        hits.truncate(limit);
        Ok(ArchiveSearchResults {
            hits: hits.into_iter().map(|(_, _, hit)| hit).collect(),
            match_count,
            searched_frames: page_frames.len(),
            workers,
        })
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

fn search_snippet(text: &str, start: usize, end: usize) -> String {
    let prefix_chars = text[..start].chars().count();
    let matched_chars = text[start..end].chars().count();
    let first = prefix_chars.saturating_sub(60);
    let last = (prefix_chars + matched_chars + 100).min(text.chars().count());
    let mut snippet = text
        .chars()
        .skip(first)
        .take(last - first)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if first > 0 {
        snippet.insert_str(0, "…");
    }
    if last < text.chars().count() {
        snippet.push('…');
    }
    snippet
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
        assert_eq!(index.current_title(7).unwrap().as_deref(), Some("Testa lapa"));
        assert_eq!(
            index.first_page().unwrap(),
            Some((7, "Testa lapa".into()))
        );
        assert_eq!(index.pages(Some("LAPA"), 10), vec![(7, "Testa lapa".into())]);
        assert_eq!(index.revision(7, 10).unwrap().unwrap().text, b"old");
        assert_eq!(
            index.page_text_at(7, i64::MAX).unwrap(),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            index.page_text_at(7, 110_000_000).unwrap(),
            Some(b"old".to_vec())
        );
        assert_eq!(index.page_text_at(7, 1).unwrap(), None);

        let titles = index
            .search_regex(
                &regex::Regex::new("(?i)testa").unwrap(),
                ArchiveSearchKind::Title,
                10,
            )
            .unwrap();
        assert_eq!(titles.match_count, 1);
        assert_eq!(titles.hits[0].title, "Testa lapa");
        assert_eq!(titles.hits[0].revision_id, None);

        let text = index
            .search_regex(
                &regex::Regex::new("hello|old").unwrap(),
                ArchiveSearchKind::FullText,
                10,
            )
            .unwrap();
        assert_eq!(text.match_count, 2);
        assert_eq!(
            text.hits
                .iter()
                .map(|hit| hit.revision_id.unwrap())
                .collect::<Vec<_>>(),
            vec![11, 10]
        );
        assert_eq!(text.hits[0].snippet.as_deref(), Some("hello"));
    }
}
