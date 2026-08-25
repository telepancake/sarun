//! Read-only browsing directly from a portable Wikipedia archive.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::archive::{
    visit_frame_while_file, EntityKind, FrameLocation, IndexedArchiveSet, ManifestRecord, Record,
    RevisionMetadata, RevisionRecord, SiteInfoRecord,
};

/// Canonical names retained by MediaWiki for its fixed namespace IDs. Archive
/// siteinfo records retain only the localized name, so this compatibility map
/// restores the canonical spelling needed by APIs such as Scribunto. Unknown
/// extension namespace IDs deliberately remain without a canonical name.
fn fixed_namespace_canonical(id: i32) -> Option<&'static str> {
    Some(match id {
        -2 => "Media",
        -1 => "Special",
        0 => "",
        1 => "Talk",
        2 => "User",
        3 => "User talk",
        4 => "Project",
        5 => "Project talk",
        6 => "File",
        7 => "File talk",
        8 => "MediaWiki",
        9 => "MediaWiki talk",
        10 => "Template",
        11 => "Template talk",
        12 => "Help",
        13 => "Help talk",
        14 => "Category",
        15 => "Category talk",
        828 => "Module",
        829 => "Module talk",
        _ => return None,
    })
}

#[derive(Debug)]
pub struct ArchiveBrowseIndex {
    titles: Arc<crate::title_index::TitleIndex>,
    title_index_path: std::path::PathBuf,
    indexed: IndexedArchiveSet,
    backrefs: Option<Arc<crate::backrefs::BackrefIndex>>,
    site_info: SiteInfoRecord,
    manifest: Option<ManifestRecord>,
}

/// Parsed generation metadata that is safe to retain after a request ends.
/// Unlike [`ArchiveBrowseIndex`], this contains no request-owned lease. The
/// immutable archive layout and bounded segment-file cache may be retained;
/// each request reconstructs only the lease-bearing access layer.
#[derive(Clone)]
pub(crate) struct ArchiveBrowseMetadata {
    titles: Arc<crate::title_index::TitleIndex>,
    title_index_path: std::path::PathBuf,
    archive_layout: Arc<crate::archive::IndexedArchiveLayout>,
    backrefs: Option<Arc<crate::backrefs::BackrefIndex>>,
    site_info: SiteInfoRecord,
    manifest: Option<ManifestRecord>,
}

pub struct ArchiveAsOfView<'a> {
    archive: &'a ArchiveBrowseIndex,
    timestamp_micros: Option<i64>,
    render_timestamp_micros: i64,
    site: wikimak_wikitext::SiteConfig,
    max_text_bytes: Option<usize>,
    included_record_payload_limit: Option<usize>,
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

impl PageRevisionSummary {
    fn from_metadata(metadata: RevisionMetadata) -> Self {
        let RevisionMetadata {
            meta,
            has_text,
            visibility,
            minor,
        } = metadata;
        Self {
            revision_id: meta.rev_id,
            parent_id: meta.parent_id,
            timestamp_micros: meta.ts.timestamp_micros(),
            contributor: meta.contributor,
            comment: meta.comment,
            flags: meta.flags,
            text_len: meta.text_len,
            has_text,
            minor,
            visibility,
        }
    }
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

const VERIFY_SAMPLE_PAGE_COUNT: usize = 8;
const VERIFY_MAX_REVISION_BODIES: usize = 24;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ArchiveVerifyRevision {
    pub role: &'static str,
    pub revision_id: u64,
    pub has_text: bool,
    pub text_len: u64,
    pub text_read: bool,
    pub observed_text_len: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ArchiveVerifyPage {
    pub page_id: u64,
    pub title: String,
    pub revisions: Vec<ArchiveVerifyRevision>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ArchiveVerifyReport {
    pub schema: u32,
    pub operation: &'static str,
    pub selected_generation: String,
    pub sample_strategy: &'static str,
    pub sampled_pages: Vec<ArchiveVerifyPage>,
    pub revision_bodies_read: usize,
    pub max_revision_bodies: usize,
}

/// Bounded, restartable text reader for one page's newest-to-oldest revision
/// stream. Sequential older-revision reads continue from the live zstd
/// decoder. A small byte-bounded cache makes short backward moves cheap;
/// neither metadata discovery nor browsing retains the page's entire text
/// history.
pub struct PageRevisionTextCursor {
    file: std::fs::File,
    location: FrameLocation,
    page_id: u64,
    frame: crate::archive::FrameRecordCursor,
    next_revision: usize,
    cache: std::collections::VecDeque<(usize, std::sync::Arc<[u8]>)>,
    cache_bytes: usize,
    cache_limit: usize,
    included_record_payload_limit: Option<usize>,
}

/// Bounded, restartable metadata reader for one page's newest-to-oldest
/// revision stream. It never retains the page's complete revision history;
/// explicit scans keep only the same small cache used by ordinary access.
pub struct PageRevisionSummaryCursor {
    file: std::fs::File,
    location: FrameLocation,
    page_id: u64,
    frame: crate::archive::FrameRecordCursor,
    next_revision: usize,
    cache: std::collections::VecDeque<(
        usize,
        std::sync::Arc<PageRevisionSummary>,
    )>,
    cache_bytes: usize,
    cache_limit: usize,
    included_record_payload_limit: usize,
    known_count: Option<usize>,
    observed_count: usize,
    #[cfg(test)]
    restarts: usize,
}

impl PageRevisionSummaryCursor {
    const CACHE_ENTRY_LIMIT: usize = 8;

    fn restart(&mut self) -> crate::archive::Result<()> {
        self.frame = crate::archive::open_frame_cursor_file(&self.file, &self.location)?;
        self.next_revision = 0;
        #[cfg(test)]
        {
            self.restarts = self.restarts.saturating_add(1);
        }
        Ok(())
    }

    fn cached(&mut self, index: usize) -> Option<std::sync::Arc<PageRevisionSummary>> {
        let position = self.cache.iter().position(|(cached, _)| *cached == index)?;
        let entry = self.cache.remove(position)?;
        let summary = entry.1.clone();
        self.cache.push_back(entry);
        Some(summary)
    }

    fn summary_owned_bytes(summary: &PageRevisionSummary) -> usize {
        let contributor_bytes = match &summary.contributor {
            crate::ContributorMeta::Anonymous { ip } => ip.capacity(),
            crate::ContributorMeta::Named { username, .. } => username.capacity(),
            crate::ContributorMeta::Hidden => 0,
        };
        std::mem::size_of::<PageRevisionSummary>()
            .saturating_add(contributor_bytes)
            .saturating_add(summary.comment.capacity())
    }

    fn remember(
        &mut self,
        index: usize,
        summary: PageRevisionSummary,
    ) -> std::sync::Arc<PageRevisionSummary> {
        let summary = std::sync::Arc::new(summary);
        let owned_bytes = Self::summary_owned_bytes(&summary);
        if owned_bytes > self.cache_limit {
            return summary;
        }
        if let Some(position) = self.cache.iter().position(|(cached, _)| *cached == index) {
            if let Some((_, removed)) = self.cache.remove(position) {
                self.cache_bytes = self
                    .cache_bytes
                    .saturating_sub(Self::summary_owned_bytes(&removed));
            }
        }
        while self.cache.len() >= Self::CACHE_ENTRY_LIMIT
            || self.cache_bytes.saturating_add(owned_bytes) > self.cache_limit
        {
            let Some((_, removed)) = self.cache.pop_front() else {
                break;
            };
            self.cache_bytes = self
                .cache_bytes
                .saturating_sub(Self::summary_owned_bytes(&removed));
        }
        self.cache_bytes = self.cache_bytes.saturating_add(owned_bytes);
        self.cache.push_back((index, summary.clone()));
        summary
    }

    fn next_summary(&mut self) -> crate::archive::Result<Option<PageRevisionSummary>> {
        let Some(metadata) = self
            .frame
            .next_revision_metadata_with_payload_limit(
                self.page_id,
                self.included_record_payload_limit,
            )?
        else {
            self.known_count = Some(self.next_revision);
            return Ok(None);
        };
        let index = self.next_revision;
        self.next_revision = self.next_revision.saturating_add(1);
        self.observed_count = self.observed_count.max(self.next_revision);
        let summary = self.remember(index, PageRevisionSummary::from_metadata(metadata));
        Ok(Some(summary.as_ref().clone()))
    }

    pub fn summary(
        &mut self,
        revision_index: usize,
    ) -> crate::archive::Result<Option<PageRevisionSummary>> {
        if let Some(summary) = self.cached(revision_index) {
            return Ok(Some(summary.as_ref().clone()));
        }
        if revision_index < self.next_revision {
            self.restart()?;
        }
        loop {
            let Some(summary) = self.next_summary()? else {
                return Ok(None);
            };
            let index = self.next_revision.saturating_sub(1);
            if index == revision_index {
                return Ok(Some(summary));
            }
        }
    }

    pub fn known_count(&self) -> Option<usize> {
        self.known_count
    }

    pub fn observed_count(&self) -> usize {
        self.observed_count
    }

    pub fn cache_entries(&self) -> usize {
        self.cache.len()
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }

    #[cfg(test)]
    fn restart_count(&self) -> usize {
        self.restarts
    }

    pub fn scan_count(&mut self) -> crate::archive::Result<usize> {
        self.restart()?;
        self.known_count = None;
        loop {
            if self.next_summary()?.is_none() {
                return Ok(self.known_count.unwrap_or(self.next_revision));
            }
        }
    }

    pub fn find_at_or_before(
        &mut self,
        timestamp_micros: i64,
    ) -> crate::archive::Result<Option<usize>> {
        for index in 0.. {
            let Some(summary) = self.summary(index)? else {
                return Ok(None);
            };
            if summary.timestamp_micros <= timestamp_micros {
                return Ok(Some(index));
            }
        }
        unreachable!()
    }
}

impl PageRevisionTextCursor {
    const NEWEST_CACHE_ENTRIES: usize = 3;
    const TRAIL_CACHE_ENTRIES: usize = 5;

    fn restart(&mut self) -> crate::archive::Result<()> {
        self.frame = crate::archive::open_frame_cursor_file(&self.file, &self.location)?;
        self.next_revision = 0;
        Ok(())
    }

    fn cached(&mut self, index: usize) -> Option<std::sync::Arc<[u8]>> {
        let position = self.cache.iter().position(|(cached, _)| *cached == index)?;
        let entry = self.cache.remove(position)?;
        let text = entry.1.clone();
        self.cache.push_back(entry);
        Some(text)
    }

    fn remember(&mut self, index: usize, text: Vec<u8>) -> std::sync::Arc<[u8]> {
        let text = std::sync::Arc::<[u8]>::from(text);
        if text.len() > self.cache_limit {
            return text;
        }
        let entry_limit = Self::NEWEST_CACHE_ENTRIES + Self::TRAIL_CACHE_ENTRIES;
        while self.cache.len() >= entry_limit
            || self.cache_bytes.saturating_add(text.len()) > self.cache_limit
        {
            let position = self
                .cache
                .iter()
                .position(|(cached, _)| *cached >= Self::NEWEST_CACHE_ENTRIES)
                .unwrap_or(0);
            let Some((_, removed)) = self.cache.remove(position) else {
                break;
            };
            self.cache_bytes = self.cache_bytes.saturating_sub(removed.len());
        }
        self.cache_bytes = self.cache_bytes.saturating_add(text.len());
        self.cache.push_back((index, text.clone()));
        text
    }

    pub fn text(
        &mut self,
        revision_index: usize,
    ) -> crate::archive::Result<Option<std::sync::Arc<[u8]>>> {
        if let Some(text) = self.cached(revision_index) {
            return Ok(Some(text));
        }
        if revision_index < self.next_revision {
            self.restart()?;
        }
        while let Some(record) = match self.included_record_payload_limit {
            Some(limit) => self
                .frame
                .next_revision_record_with_payload_limit(self.page_id, limit),
            None => self.frame.next_record(),
        }? {
            let Some(record_page_id) = record.page_id() else {
                continue;
            };
            if record_page_id < self.page_id {
                continue;
            }
            if record_page_id > self.page_id {
                return Ok(None);
            }
            let Record::Revision { revision, .. } = record else {
                continue;
            };
            let index = self.next_revision;
            self.next_revision = self.next_revision.saturating_add(1);
            let text = revision
                .has_text
                .then(|| self.remember(index, revision.text));
            if index == revision_index {
                return Ok(text);
            }
        }
        Ok(None)
    }
}

impl ArchiveBrowseIndex {
    /// Open the generation selected by one logical mirror destination.
    ///
    /// If selector replacement races the first open, retry exactly when the
    /// selected generation changed; an unchanged error remains an error.
    pub fn open_installed(destination: impl AsRef<Path>) -> crate::archive::Result<Self> {
        let backrefs = destination.as_ref().with_extension("swrefs");
        crate::installation_lifecycle::with_serving_pair(
            destination.as_ref(),
            |selected| {
                Self::open_with_backrefs(&selected.archive, &selected.title, &backrefs)
                    .map_err(|error| error.to_string())
            },
        )
        .map_err(|error| crate::archive::ArchiveError::Io(std::io::Error::other(error)))
    }

    pub fn open(
        path: impl AsRef<Path>,
        title_index: impl AsRef<Path>,
    ) -> crate::archive::Result<Self> {
        let path = path.as_ref();
        let title_index = title_index.as_ref();
        Self::open_with_backrefs(path, title_index, &path.with_extension("swrefs"))
    }

    fn open_with_backrefs(
        path: &Path,
        title_index: &Path,
        backref_path: &Path,
    ) -> crate::archive::Result<Self> {
        let titles = Arc::new(crate::title_index::TitleIndex::open(title_index)?);
        let backrefs = Self::load_optional_backrefs(backref_path, title_index);
        let frame_count = titles.frame_count();
        if frame_count == 0 {
            return Err(crate::archive::ArchiveError::Invalid(
                "title index contains no archive frames",
            ));
        }
        let indexed = IndexedArchiveSet::open(path, titles.as_ref())?;
        let mut manifest = None;
        let mut site_info = None;
        let mut left = 0;
        let mut right = frame_count;
        while left < right {
            let middle = left + (right - left) / 2;
            if titles.frame(middle)?.info.first_entity.kind < EntityKind::Global {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        for position in left..frame_count {
            let location = indexed.location(titles.frame(position)?)?;
            if location.info.first_entity.kind != EntityKind::Global {
                continue;
            }
            let mut frame_file = indexed.open_file(&location)?;
            visit_frame_while_file(&mut frame_file, &location, |record| {
                match record {
                    Record::Manifest {
                        manifest: record, ..
                    } if manifest.is_none() => manifest = Some(record),
                    Record::SiteInfo {
                        site_info: record, ..
                    } if site_info.is_none() => site_info = Some(record),
                    _ => {}
                }
                Ok(true)
            })?;
        }
        let site_info = site_info.ok_or(crate::archive::ArchiveError::Invalid(
            "archive has no siteinfo record",
        ))?;

        Ok(Self {
            titles,
            title_index_path: title_index.to_path_buf(),
            indexed,
            backrefs,
            site_info,
            manifest,
        })
    }

    fn load_optional_backrefs(
        backref_path: &Path,
        title_index: &Path,
    ) -> Option<Arc<crate::backrefs::BackrefIndex>> {
        if !backref_path.is_file() {
            return None;
        }
        match crate::backrefs::BackrefIndex::open_for_title_index(backref_path, title_index) {
            Ok(index) => Some(Arc::new(index)),
            Err(error) => {
                eprintln!(
                    "wikimak browse: ignoring unusable optional backrefs {}: {error}",
                    backref_path.display()
                );
                None
            }
        }
    }

    /// Reopen only the request-scoped archive access layer from parsed
    /// metadata. The returned index owns a fresh shared generation lease via
    /// [`IndexedArchiveSet`]; immutable layout validation is not repeated.
    pub(crate) fn open_request(
        metadata: &ArchiveBrowseMetadata,
        path: impl AsRef<Path>,
    ) -> crate::archive::Result<Self> {
        Self::open_request_with_backrefs(metadata, path, None)
    }

    /// Reopen a request after the optional backreference sidecar was replaced
    /// in the same selected generation. The immutable archive layout remains
    /// shared; only the sidecar is reopened.
    pub(crate) fn open_request_with_backrefs(
        metadata: &ArchiveBrowseMetadata,
        path: impl AsRef<Path>,
        backref_path: Option<&Path>,
    ) -> crate::archive::Result<Self> {
        let indexed = IndexedArchiveSet::open_with_layout(
            path,
            Arc::clone(&metadata.archive_layout),
        )?;
        let backrefs = match backref_path {
            Some(path) => Self::load_optional_backrefs(path, &metadata.title_index_path),
            None => metadata.backrefs.as_ref().map(Arc::clone),
        };
        Ok(Self {
            titles: Arc::clone(&metadata.titles),
            title_index_path: metadata.title_index_path.clone(),
            indexed,
            backrefs,
            site_info: metadata.site_info.clone(),
            manifest: metadata.manifest.clone(),
        })
    }

    pub(crate) fn metadata(&self) -> ArchiveBrowseMetadata {
        ArchiveBrowseMetadata {
            titles: Arc::clone(&self.titles),
            title_index_path: self.title_index_path.clone(),
            archive_layout: self.indexed.layout(),
            backrefs: self.backrefs.as_ref().map(Arc::clone),
            site_info: self.site_info.clone(),
            manifest: self.manifest.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn title_index_identity(&self) -> *const crate::title_index::TitleIndex {
        Arc::as_ptr(&self.titles)
    }

    fn visit_frame(
        &self,
        location: &FrameLocation,
        mut visitor: impl FnMut(Record) -> crate::archive::Result<()>,
    ) -> crate::archive::Result<()> {
        self.visit_frame_while(location, |record| {
            visitor(record)?;
            Ok(true)
        })
    }

    fn visit_frame_while(
        &self,
        location: &FrameLocation,
        visitor: impl FnMut(Record) -> crate::archive::Result<bool>,
    ) -> crate::archive::Result<()> {
        let mut file = self.indexed.open_file(location)?;
        visit_frame_while_file(&mut file, location, visitor)
    }

    pub fn title_count(&self) -> u64 {
        self.titles.entries()
    }

    pub fn frame_count(&self) -> usize {
        self.titles.frame_count()
    }

    pub fn generation_id(&self) -> &crate::generation::GenerationId {
        self.titles.generation_id()
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

    /// Current members of one category from the generated reverse-reference
    /// sidecar. `None` means the archive has no sidecar; an empty vector is a
    /// valid indexed category with no certain members. The sidecar describes
    /// newest-page state, so callers rendering an historical `as-of` view must
    /// not present this as historical membership.
    pub fn category_member_titles(
        &self,
        category_page_id: u64,
    ) -> crate::archive::Result<Option<Vec<String>>> {
        let Some(backrefs) = self.backrefs.as_ref() else {
            return Ok(None);
        };
        let page_ids = backrefs.members(crate::backrefs::SetKey {
            target_page_id: category_page_id,
            kind: crate::backrefs::EdgeKind::Category,
            class: crate::backrefs::SetClass::TransitiveUnconditional,
        })?;
        let mut selected_frames = Vec::<(usize, Vec<u64>)>::new();
        for page_id in page_ids {
            let Some(frame_index) = self.page_frame_position(page_id)? else {
                continue;
            };
            match selected_frames.last_mut() {
                Some((last_frame, pages)) if *last_frame == frame_index => {
                    pages.push(page_id);
                }
                _ => selected_frames.push((frame_index, vec![page_id])),
            }
        }

        let mut members = Vec::<(u64, String)>::new();
        for (frame_index, selected_pages) in selected_frames {
            let location = self.frame(frame_index)?;
            self.visit_frame_while(&location, |record| {
                let Record::PageState {
                    page_id,
                    title,
                    namespace,
                    deleted,
                    ..
                } = record
                else {
                    return Ok(true);
                };
                if deleted || selected_pages.binary_search(&page_id).is_err() {
                    return Ok(true);
                }
                let title = match namespace {
                    Some(namespace) => crate::title_index::title_in_namespace(
                        &title,
                        namespace,
                        &self.site_info,
                    ),
                    None => title,
                };
                members.push((page_id, title));
                Ok(true)
            })?;
        }
        members.sort_by_key(|(page_id, _)| *page_id);
        Ok(Some(members.into_iter().map(|(_, title)| title).collect()))
    }

    pub fn pages(&self, filter: Option<&str>, limit: usize) -> Vec<(u64, String)> {
        let filter = filter.map(str::to_lowercase);
        let mut seen = std::collections::HashSet::new();
        let mut pages = Vec::new();
        for position in 0..self.frame_count() {
            let Ok(location) = self.frame(position) else {
                break;
            };
            if location.info.first_entity.kind != EntityKind::Page || pages.len() >= limit {
                break;
            }
            let result = self.visit_frame_while(&location, |record| {
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

    /// Select eight live pages at deterministic, evenly spaced page-frame
    /// positions.  The search around each target is only a tie-breaker for
    /// deleted or continuation-only frames; it never changes the ordering or
    /// uses randomness.
    pub fn deterministic_sample_pages(
        &self,
        count: usize,
    ) -> crate::archive::Result<Vec<(u64, String)>> {
        if count == 0 {
            return Err(crate::archive::ArchiveError::Invalid(
                "verification sample cannot be empty",
            ));
        }
        let page_frame_count = self.page_frame_count()?;
        if page_frame_count < count {
            return Err(crate::archive::ArchiveError::Invalid(
                "archive has fewer page frames than the verification sample",
            ));
        }

        let mut selected = Vec::with_capacity(count);
        let mut seen = std::collections::HashSet::with_capacity(count);
        for sample_index in 0..count {
            let target = if count == 1 {
                0
            } else {
                ((sample_index as u128)
                    * (page_frame_count.saturating_sub(1) as u128)
                    / ((count - 1) as u128)) as usize
            };
            let mut found = None;
            for distance in 0..page_frame_count {
                let positions = [
                    target.checked_sub(distance),
                    (distance != 0)
                        .then_some(target.saturating_add(distance))
                        .filter(|position| *position < page_frame_count),
                ];
                for position in positions.into_iter().flatten() {
                    let Some(candidate) = self
                        .first_live_page_in_frame(position)
                        .map_err(|error| {
                            crate::archive::ArchiveError::Io(std::io::Error::other(format!(
                                "read verification sample page frame {position}: {error}"
                            )))
                        })?
                    else {
                        continue;
                    };
                    if seen.insert(candidate.0) {
                        found = Some(candidate);
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            let Some(candidate) = found else {
                return Err(crate::archive::ArchiveError::Invalid(
                    "archive has fewer live pages than the verification sample",
                ));
            };
            selected.push(candidate);
        }
        Ok(selected)
    }

    /// Read a deterministic, bounded revision sample from this opened
    /// generation.  Metadata history is read through `page_revisions`; only
    /// oldest/middle/newest bodies are then read through `revision_text`.
    /// The returned report retains no revision body.
    pub fn verify_deterministic_sample(&self) -> crate::archive::Result<ArchiveVerifyReport> {
        let sampled = self.deterministic_sample_pages(VERIFY_SAMPLE_PAGE_COUNT)?;
        let mut sampled_pages = Vec::with_capacity(sampled.len());
        let mut revision_bodies_read = 0;

        for (page_id, title) in sampled {
            let history = self.page_revisions(page_id).map_err(|error| {
                crate::archive::ArchiveError::Io(std::io::Error::other(format!(
                    "read history for page {page_id} ({title:?}): {error}"
                )))
            })?;
            let mut revisions = Vec::new();
            let positions = match history.len() {
                0 => Vec::new(),
                1 => vec![(0, "only")],
                2 => vec![(1, "oldest"), (0, "newest")],
                length => vec![
                    (length - 1, "oldest"),
                    ((length - 1) / 2, "middle"),
                    (0, "newest"),
                ],
            };

            for (position, role) in positions {
                let summary = history.get(position).ok_or_else(|| {
                    crate::archive::ArchiveError::Io(std::io::Error::other(format!(
                        "verification history position {position} is absent for page {page_id} ({title:?})"
                    )))
                })?;
                if revision_bodies_read >= VERIFY_MAX_REVISION_BODIES {
                    return Err(crate::archive::ArchiveError::Io(std::io::Error::other(
                        "verification revision-body bound exceeded",
                    )));
                }
                revision_bodies_read += 1;
                let text = self
                    .revision_text(page_id, summary.revision_id, 0, usize::MAX)
                    .map_err(|error| {
                        crate::archive::ArchiveError::Io(std::io::Error::other(format!(
                            "read {role} revision {} for page {page_id} ({title:?}): {error}",
                            summary.revision_id
                        )))
                    })?;
                let observed_text_len = text.as_ref().map(|bytes| bytes.len() as u64);
                if summary.has_text != text.is_some()
                    || (summary.has_text
                        && observed_text_len != Some(summary.text_len))
                {
                    return Err(crate::archive::ArchiveError::Io(std::io::Error::other(
                        format!(
                            "text availability/length mismatch for {role} revision {} on page {page_id} ({title:?}): metadata has_text={}, text_len={}, observed={observed_text_len:?}",
                            summary.revision_id,
                            summary.has_text,
                            summary.text_len,
                        ),
                    )));
                }
                revisions.push(ArchiveVerifyRevision {
                    role,
                    revision_id: summary.revision_id,
                    has_text: summary.has_text,
                    text_len: summary.text_len,
                    text_read: text.is_some(),
                    observed_text_len,
                });
                drop(text);
            }
            sampled_pages.push(ArchiveVerifyPage {
                page_id,
                title,
                revisions,
            });
        }

        Ok(ArchiveVerifyReport {
            schema: 1,
            operation: "wikimak verify",
            selected_generation: self.generation_id().as_str().to_owned(),
            sample_strategy: "eight evenly spaced live page-frame positions",
            sampled_pages,
            revision_bodies_read,
            max_revision_bodies: VERIFY_MAX_REVISION_BODIES,
        })
    }

    fn first_live_page_in_frame(
        &self,
        position: usize,
    ) -> crate::archive::Result<Option<(u64, String)>> {
        let location = self.frame(position)?;
        let mut found = None;
        self.visit_frame_while(&location, |record| {
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
        Ok(found)
    }

    pub fn first_page(&self) -> crate::archive::Result<Option<(u64, String)>> {
        for position in 0..self.frame_count() {
            let location = self.frame(position)?;
            if location.info.first_entity.kind != EntityKind::Page {
                break;
            }
            let mut found = None;
            self.visit_frame_while(&location, |record| {
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
        let Some(location) = self.page_frame(page_id)? else {
            return Ok(None);
        };
        let mut title = None;
        self.visit_frame_while(&location, |record| {
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
        let Some(location) = self.page_frame(page_id)? else {
            return Ok(None);
        };
        let mut current = None;
        let mut actions = Vec::new();
        self.visit_frame(&location, |record| {
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
        let Some(location) = self.page_frame(page_id)? else {
            return Ok(None);
        };
        let mut selected = None;
        self.visit_frame_while(&location, |record| {
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

    /// Read one pinned revision's text without materializing a complete
    /// [`RevisionRecord`] or its revision history. Metadata is scanned with
    /// the text-skipping cursor first; the text cursor then decodes records
    /// sequentially to the selected revision, retaining only the returned
    /// byte slice.
    pub fn revision_text(
        &self,
        page_id: u64,
        revision_id: u64,
        cache_limit: usize,
        included_record_payload_limit: usize,
    ) -> crate::archive::Result<Option<std::sync::Arc<[u8]>>> {
        let Some((revision_index, summary)) = self.revision_metadata(
            page_id,
            revision_id,
            included_record_payload_limit,
        )? else {
            return Ok(None);
        };
        if !summary.has_text {
            return Ok(None);
        }
        self.revision_text_at_index(
            page_id,
            revision_index,
            cache_limit,
            included_record_payload_limit,
        )
    }

    /// Locate one revision using metadata only. The returned index is the
    /// canonical newest-to-oldest position used by [`PageRevisionTextCursor`].
    pub fn revision_metadata(
        &self,
        page_id: u64,
        revision_id: u64,
        included_record_payload_limit: usize,
    ) -> crate::archive::Result<Option<(usize, PageRevisionSummary)>> {
        let Some(mut summaries) = self.page_revision_summary_cursor(
            page_id,
            0,
            included_record_payload_limit,
        )? else {
            return Ok(None);
        };
        for revision_index in 0.. {
            let Some(summary) = summaries.summary(revision_index)? else {
                return Ok(None);
            };
            if summary.revision_id == revision_id {
                return Ok(Some((revision_index, summary)));
            }
        }
        unreachable!()
    }

    /// Decode the text at an already located revision index. With a zero
    /// cache limit, earlier revisions are decoded one at a time and dropped;
    /// only the returned text remains owned by the caller.
    pub fn revision_text_at_index(
        &self,
        page_id: u64,
        revision_index: usize,
        cache_limit: usize,
        included_record_payload_limit: usize,
    ) -> crate::archive::Result<Option<std::sync::Arc<[u8]>>> {
        let Some(mut texts) = self.page_revision_text_cursor_bounded(
            page_id,
            cache_limit,
            included_record_payload_limit,
        )? else {
            return Ok(None);
        };
        texts.text(revision_index)
    }

    pub fn revision_at(
        &self,
        page_id: u64,
        timestamp_micros: i64,
    ) -> crate::archive::Result<Option<RevisionRecord>> {
        let Some(location) = self.page_frame(page_id)? else {
            return Ok(None);
        };
        let mut selected = None;
        self.visit_frame_while(&location, |record| {
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
        let Some(mut cursor) = self.page_revision_summary_cursor(
            page_id,
            usize::MAX,
            usize::MAX,
        )? else {
            return Ok(Vec::new());
        };
        let mut revisions = Vec::new();
        for index in 0.. {
            let Some(summary) = cursor.summary(index)? else {
                break;
            };
            revisions.push(summary);
        }
        Ok(revisions)
    }

    pub fn page_revision_summary_cursor(
        &self,
        page_id: u64,
        cache_bytes: usize,
        included_record_payload_limit: usize,
    ) -> crate::archive::Result<Option<PageRevisionSummaryCursor>> {
        let Some(location) = self.page_frame(page_id)? else {
            return Ok(None);
        };
        let file = self.indexed.open_file(&location)?;
        let frame = crate::archive::open_frame_cursor_file(&file, &location)?;
        Ok(Some(PageRevisionSummaryCursor {
            file,
            location,
            page_id,
            frame,
            next_revision: 0,
            cache: std::collections::VecDeque::new(),
            cache_bytes: 0,
            cache_limit: cache_bytes,
            included_record_payload_limit,
            known_count: None,
            observed_count: 0,
            #[cfg(test)]
            restarts: 0,
        }))
    }

    pub fn page_revision_text_cursor(
        &self,
        page_id: u64,
        cache_limit: usize,
    ) -> crate::archive::Result<Option<PageRevisionTextCursor>> {
        self.page_revision_text_cursor_with_payload_limit(page_id, cache_limit, None)
    }

    pub fn page_revision_text_cursor_bounded(
        &self,
        page_id: u64,
        cache_limit: usize,
        included_record_payload_limit: usize,
    ) -> crate::archive::Result<Option<PageRevisionTextCursor>> {
        self.page_revision_text_cursor_with_payload_limit(
            page_id,
            cache_limit,
            Some(included_record_payload_limit),
        )
    }

    fn page_revision_text_cursor_with_payload_limit(
        &self,
        page_id: u64,
        cache_limit: usize,
        included_record_payload_limit: Option<usize>,
    ) -> crate::archive::Result<Option<PageRevisionTextCursor>> {
        let Some(location) = self.page_frame(page_id)? else {
            return Ok(None);
        };
        let file = self.indexed.open_file(&location)?;
        let frame = crate::archive::open_frame_cursor_file(&file, &location)?;
        Ok(Some(PageRevisionTextCursor {
            file,
            location,
            page_id,
            frame,
            next_revision: 0,
            cache: std::collections::VecDeque::new(),
            cache_bytes: 0,
            cache_limit,
            included_record_payload_limit,
        }))
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
        let page_frame_count = self.page_frame_count()?;
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(page_frame_count.max(1));
        let next = AtomicUsize::new(0);
        let keep = limit.max(1);

        let partials = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for _ in 0..workers {
                handles.push(scope.spawn(|| -> crate::archive::Result<_> {
                    let mut found = Vec::new();
                    let mut match_count = 0_u64;
                    loop {
                        let frame_index = next.fetch_add(1, Ordering::Relaxed);
                        if frame_index >= page_frame_count {
                            break;
                        }
                        let location = self.frame(frame_index)?;
                        let mut file = self.indexed.open_file(&location)?;
                        let mut states = std::collections::HashMap::<u64, Option<String>>::new();
                        let mut revisions = Vec::new();
                        let mut sequence = 0_usize;
                        visit_frame_while_file(&mut file, &location, |record| {
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
            searched_frames: page_frame_count,
            workers,
        })
    }

    /// Find edits made by one registered revision contributor. The backref
    /// sidecar first narrows the work to pages edited by the local user ID;
    /// each selected page's linear history then supplies the exact edits.
    pub fn contributor_edits(
        &self,
        contributor: &crate::ContributorMeta,
        limit: usize,
    ) -> crate::archive::Result<ArchiveSearchResults> {
        let crate::ContributorMeta::Named { user_id, .. } = contributor else {
            return Err(crate::archive::ArchiveError::Invalid(
                "contributor edit lookup requires a registered local user",
            ));
        };
        if *user_id == 0 {
            return Err(crate::archive::ArchiveError::Invalid(
                "contributor edit lookup requires a nonzero local user id",
            ));
        }
        let backrefs = self.backrefs.as_ref().ok_or(
            crate::archive::ArchiveError::Invalid(
                "archive has no backref sidecar",
            ),
        )?;
        let page_ids = backrefs.pages_edited_by(*user_id)?;
        let mut selected_frames = Vec::<(usize, Vec<u64>)>::new();
        for page_id in page_ids {
            let Some(frame_index) = self.page_frame_position(page_id)? else {
                continue;
            };
            match selected_frames.last_mut() {
                Some((last_frame, pages)) if *last_frame == frame_index => {
                    pages.push(page_id);
                }
                _ => selected_frames.push((frame_index, vec![page_id])),
            }
        }
        let selected_frame_count = selected_frames.len();
        if selected_frame_count == 0 {
            return Ok(ArchiveSearchResults {
                hits: Vec::new(),
                match_count: 0,
                searched_frames: 0,
                workers: 1,
            });
        }
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(selected_frame_count);
        let next = AtomicUsize::new(0);
        let keep = limit.max(1);

        let partials = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for _ in 0..workers {
                handles.push(scope.spawn(|| -> crate::archive::Result<_> {
                    let mut found = Vec::new();
                    let mut match_count = 0_u64;
                    loop {
                        let selected_index = next.fetch_add(1, Ordering::Relaxed);
                        let Some((frame_index, selected_pages)) =
                            selected_frames.get(selected_index)
                        else {
                            break;
                        };
                        let location = self.frame(*frame_index)?;
                        let mut file = self.indexed.open_file(&location)?;
                        let mut titles = std::collections::HashMap::<u64, String>::new();
                        let mut edits = Vec::new();
                        let mut sequence = 0_usize;
                        visit_frame_while_file(&mut file, &location, |record| {
                            let record_sequence = sequence;
                            sequence += 1;
                            let Some(page_id) = record.page_id() else {
                                return Ok(true);
                            };
                            if selected_pages.binary_search(&page_id).is_err() {
                                return Ok(true);
                            }
                            match record {
                                Record::PageState {
                                    page_id,
                                    title,
                                    namespace,
                                    ..
                                } => {
                                    titles.entry(page_id).or_insert_with(|| {
                                        namespace.map_or(title.clone(), |namespace| {
                                            crate::title_index::title_in_namespace(
                                                &title,
                                                namespace,
                                                &self.site_info,
                                            )
                                        })
                                    });
                                }
                                Record::Revision { page_id, revision }
                                    if same_contributor(
                                        &revision.meta.contributor,
                                        contributor,
                                    ) =>
                                {
                                    match_count = match_count.saturating_add(1);
                                    edits.push((
                                        record_sequence,
                                        page_id,
                                        revision.meta.rev_id,
                                        revision.meta.ts.timestamp_micros(),
                                        revision.meta.comment,
                                    ));
                                }
                                _ => {}
                            }
                            Ok(true)
                        })?;
                        for (record_sequence, page_id, revision_id, timestamp, comment) in edits {
                            let Some(title) = titles.get(&page_id) else {
                                continue;
                            };
                            found.push((
                                *frame_index,
                                record_sequence,
                                ArchiveSearchHit {
                                    page_id,
                                    title: title.clone(),
                                    revision_id: Some(revision_id),
                                    timestamp_micros: Some(timestamp),
                                    snippet: (!comment.is_empty()).then_some(comment),
                                },
                            ));
                        }
                        if found.len() > keep {
                            found.sort_by(|left, right| {
                                right
                                    .2
                                    .timestamp_micros
                                    .cmp(&left.2.timestamp_micros)
                                    .then_with(|| left.0.cmp(&right.0))
                                    .then_with(|| left.1.cmp(&right.1))
                            });
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
                        crate::archive::ArchiveError::Invalid(
                            "contributor edit-search worker panicked",
                        )
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
        hits.sort_by(|left, right| {
            right
                .2
                .timestamp_micros
                .cmp(&left.2.timestamp_micros)
                .then_with(|| left.0.cmp(&right.0))
                .then_with(|| left.1.cmp(&right.1))
        });
        hits.truncate(limit);
        Ok(ArchiveSearchResults {
            hits: hits.into_iter().map(|(_, _, hit)| hit).collect(),
            match_count,
            searched_frames: selected_frame_count,
            workers,
        })
    }

    fn frame(&self, position: usize) -> crate::archive::Result<FrameLocation> {
        self.indexed.location(self.titles.frame(position)?)
    }

    fn first_frame_after_kind(
        &self,
        kind: EntityKind,
    ) -> crate::archive::Result<usize> {
        let mut left = 0;
        let mut right = self.frame_count();
        while left < right {
            let middle = left + (right - left) / 2;
            if self.titles.frame(middle)?.info.first_entity.kind <= kind {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        Ok(left)
    }

    fn page_frame_count(&self) -> crate::archive::Result<usize> {
        self.first_frame_after_kind(EntityKind::Page)
    }

    fn page_frame(&self, page_id: u64) -> crate::archive::Result<Option<FrameLocation>> {
        let Some(position) = self.page_frame_position(page_id)? else {
            return Ok(None);
        };
        Ok(Some(self.frame(position)?))
    }

    fn page_frame_position(&self, page_id: u64) -> crate::archive::Result<Option<usize>> {
        let page_frame_count = self.page_frame_count()?;
        let mut left = 0;
        let mut right = page_frame_count;
        while left < right {
            let middle = left + (right - left) / 2;
            if self.titles.frame(middle)?.info.last_entity.id < page_id {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        if left >= page_frame_count {
            return Ok(None);
        }
        let frame = self.titles.frame(left)?;
        Ok((frame.info.first_entity.id <= page_id
            && page_id <= frame.info.last_entity.id)
            .then_some(left))
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

    pub fn page_text_at_bounded(
        &self,
        page_id: u64,
        timestamp_micros: i64,
        max_text_bytes: usize,
        included_record_payload_limit: usize,
    ) -> crate::archive::Result<Option<Vec<u8>>> {
        let Some(mut summaries) = self.page_revision_summary_cursor(
            page_id,
            0,
            included_record_payload_limit,
        )? else {
            return Ok(None);
        };
        let Some(revision_index) = summaries.find_at_or_before(timestamp_micros)? else {
            return Ok(None);
        };
        let Some(summary) = summaries.summary(revision_index)? else {
            return Ok(None);
        };
        let max_text_bytes_u64 = u64::try_from(max_text_bytes).unwrap_or(u64::MAX);
        if summary.has_text && summary.text_len > max_text_bytes_u64 {
            return Err(crate::archive::ArchiveError::PageTextTooLarge {
                page_id,
                bytes: summary.text_len,
                limit: max_text_bytes,
            });
        }
        let Some(mut texts) = self.page_revision_text_cursor_bounded(
            page_id,
            0,
            included_record_payload_limit,
        )? else {
            return Ok(None);
        };
        let Some(text) = texts.text(revision_index)? else {
            return Ok(None);
        };
        if text.len() > max_text_bytes {
            return Err(crate::archive::ArchiveError::PageTextTooLarge {
                page_id,
                bytes: text.len() as u64,
                limit: max_text_bytes,
            });
        }
        Ok(Some(text.as_ref().to_vec()))
    }

    pub fn view(&self, timestamp_micros: Option<i64>) -> ArchiveAsOfView<'_> {
        self.view_with_limits(timestamp_micros, None, None)
    }

    pub fn view_bounded(
        &self,
        timestamp_micros: Option<i64>,
        max_text_bytes: usize,
        included_record_payload_limit: usize,
    ) -> ArchiveAsOfView<'_> {
        self.view_with_limits(
            timestamp_micros,
            Some(max_text_bytes),
            Some(included_record_payload_limit),
        )
    }

    fn view_with_limits(
        &self,
        timestamp_micros: Option<i64>,
        max_text_bytes: Option<usize>,
        included_record_payload_limit: Option<usize>,
    ) -> ArchiveAsOfView<'_> {
        let language = effective_content_language(
            &self.site_info.language,
            &self.site_info.db_name,
        );
        // Older siteinfo captures treated the mere presence of the API's
        // `rtl` field as true.  Direction is determined from the content
        // language here as well, so those archives remain readable instead
        // of laying out Latvian and other LTR wikis backwards.
        let rtl = crate::asof::RTL_LANGS.contains(&language.as_str());
        let mut site = wikimak_wikitext::SiteConfig {
            site_name: self.site_info.site_name.clone(),
            db_name: self.site_info.db_name.clone(),
            lang: language,
            rtl,
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
                    canonical: fixed_namespace_canonical(namespace.id)
                        .unwrap_or_default()
                        .to_owned(),
                    localized: namespace.localized_name.clone(),
                    aliases: namespace.aliases.clone(),
                    case_first_letter: namespace.case == "first-letter",
                },
            );
        }
        for word in &self.site_info.magic_words {
            let canonical = word.canonical_name.trim_start_matches('#').trim_end_matches(':');
            if !canonical.is_empty() {
                site.magic_aliases
                    .entry(canonical.to_string())
                    .or_insert_with(|| canonical.to_string());
                if !word.case_sensitive {
                    site.magic_aliases
                        .entry(canonical.to_lowercase())
                        .or_insert_with(|| canonical.to_string());
                }
            }
            for alias in &word.aliases {
                let token = alias
                    .trim_start_matches('#')
                    .trim_end_matches(':')
                    .trim();
                if token.is_empty() || canonical.is_empty() {
                    continue;
                }
                // Several MediaWiki extensions intentionally reuse a short
                // alias (notably `time`).  Keep the first canonical mapping,
                // which is the core built-in, rather than letting an
                // extension's later row silently break `#time`.
                site.magic_aliases
                    .entry(token.to_string())
                    .or_insert_with(|| canonical.to_string());
                if !word.case_sensitive {
                    site.magic_aliases
                        .entry(token.to_lowercase())
                        .or_insert_with(|| canonical.to_string());
                }
            }
        }
        ArchiveAsOfView {
            archive: self,
            timestamp_micros,
            render_timestamp_micros: timestamp_micros
                .unwrap_or_else(|| chrono::Utc::now().timestamp_micros()),
            site,
            max_text_bytes,
            included_record_payload_limit,
        }
    }
}

fn effective_content_language(language: &str, db_name: &str) -> String {
    if !language.trim().is_empty() {
        return language.to_string();
    }
    db_name
        .strip_suffix("wiki")
        .filter(|code| {
            (2..=3).contains(&code.len())
                && code.bytes().all(|byte| byte.is_ascii_lowercase())
        })
        .unwrap_or_default()
        .to_string()
}

fn same_contributor(
    candidate: &crate::ContributorMeta,
    wanted: &crate::ContributorMeta,
) -> bool {
    match (candidate, wanted) {
        (
            crate::ContributorMeta::Named {
                user_id: candidate_id,
                username: candidate_name,
            },
            crate::ContributorMeta::Named {
                user_id: wanted_id,
                username: wanted_name,
            },
        ) => {
            if *wanted_id != 0 {
                candidate_id == wanted_id
            } else {
                candidate_name == wanted_name
            }
        }
        (
            crate::ContributorMeta::Anonymous { ip: candidate },
            crate::ContributorMeta::Anonymous { ip: wanted },
        ) => candidate == wanted,
        (crate::ContributorMeta::Hidden, crate::ContributorMeta::Hidden) => false,
        _ => false,
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
        let text = match (
            self.max_text_bytes,
            self.included_record_payload_limit,
        ) {
            (Some(max_text_bytes), Some(included_record_payload_limit)) => self
                .archive
                .page_text_at_bounded(
                    page_id,
                    self.timestamp_micros.unwrap_or(i64::MAX),
                    max_text_bytes,
                    included_record_payload_limit,
                )
                .ok()
                .flatten()?,
            _ => self
                .archive
                .page_text_at(
                    page_id,
                    self.timestamp_micros.unwrap_or(i64::MAX),
                )
                .ok()
                .flatten()?,
        };
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
        Some(self.archive.titles.current_page_count(namespace))
    }

    fn category_members(
        &self,
        category: &wikimak_wikitext::Title,
    ) -> Option<Vec<wikimak_wikitext::Title>> {
        if category.ns != wikimak_wikitext::title::NS_CATEGORY
            || self.timestamp_micros.is_some()
        {
            return None;
        }
        let category_id = self.archive.page_id_by_title(
            &category.prefixed(&self.site),
            i64::MAX,
        )?;
        let titles = self
            .archive
            .category_member_titles(category_id)
            .ok()??;
        let mut members = titles
            .into_iter()
            .map(|title| wikimak_wikitext::Title::parse(&title, &self.site))
            .collect::<Vec<_>>();
        // The sidecar intentionally stores only page IDs, not MediaWiki's
        // per-membership sort keys. Alphabetical title order is the useful
        // deterministic fallback for browsing.
        members.sort_by_key(|title| title.prefixed(&self.site).to_lowercase());
        Some(members)
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

    #[test]
    fn legacy_wikipedia_database_name_supplies_missing_language_only() {
        assert_eq!(effective_content_language("", "ruwiki"), "ru");
        assert_eq!(effective_content_language("lv", "ruwiki"), "lv");
        assert_eq!(effective_content_language("", "commonswiki"), "");
    }
    use crate::archive::{ArchiveWriter, Record};
    use crate::{ContributorMeta, RevisionMeta};
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use wikimak_scribunto::LuaInvoker;
    use wikimak_wikitext::{Frame, ModuleInvoker, PageStore};

    fn verification_revision(
        page_id: u64,
        revision_id: u64,
        text: &[u8],
        has_text: bool,
    ) -> RevisionRecord {
        RevisionRecord {
            meta: RevisionMeta {
                rev_id: revision_id,
                parent_id: revision_id.saturating_sub(1),
                ts: Utc.timestamp_opt(revision_id as i64, 0).unwrap(),
                contributor: ContributorMeta::Hidden,
                comment: format!("page {page_id} revision {revision_id}"),
                sha1: String::new(),
                flags: 0,
                text_len: text.len() as u64,
            },
            has_text,
            text: text.to_vec(),
            visibility: None,
            history: None,
        }
    }

    fn verification_fixture(root: &std::path::Path) -> (PathBuf, ArchiveBrowseIndex) {
        let archive = root.join("verify.swdump");
        let output = crate::archive_set::ArchiveSetOutput::new_in(root, 1 << 20).unwrap();
        let mut writer = ArchiveWriter::with_ref_prefix(
            output,
            1,
            crate::archive::CompressionSettings::default(),
            b"verification fixture reference prefix",
        )
        .unwrap();
        for page_id in 1..=8 {
            writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: i64::MAX,
                    title: format!("Verification page {page_id}"),
                    namespace: None,
                    deleted: false,
                })
                .unwrap();
            for revision_number in (1..=3).rev() {
                let revision_id = page_id * 100 + revision_number;
                let (text, has_text) = if page_id == 4 && revision_number == 3 {
                    (Vec::new(), true)
                } else if page_id == 8 && revision_number == 2 {
                    (Vec::new(), false)
                } else {
                    (format!("page-{page_id}-revision-{revision_number}").into_bytes(), true)
                };
                writer
                    .write(&Record::Revision {
                        page_id,
                        revision: verification_revision(
                            page_id,
                            revision_id,
                            &text,
                            has_text,
                        ),
                    })
                    .unwrap();
            }
        }
        writer
            .write(&Record::Manifest {
                timestamp_micros: i64::MAX,
                manifest: crate::archive::ManifestRecord {
                    wiki_db: "verificationwiki".into(),
                    content_snapshot: "fixture".into(),
                    metadata_snapshot: "fixture".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: i64::MAX,
                site_info: crate::archive::SiteInfoRecord {
                    site_name: "Verification fixture".into(),
                    db_name: "verificationwiki".into(),
                    base: "https://example.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: "https://example.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (output, _) = writer.finish().unwrap();
        output.finish().unwrap().persist(&archive).unwrap();

        let title_index = archive.with_extension("swtitle");
        let generation_id = crate::generation::GenerationId::from_plan_bytes(b"verify-fixture");
        crate::title_index::build(&archive, &title_index, &generation_id).unwrap();
        let index = ArchiveBrowseIndex::open(&archive, &title_index).unwrap();
        (archive, index)
    }

    #[cfg(unix)]
    #[test]
    fn cached_layout_rejects_archive_root_replacement_at_same_path() {
        let temporary = tempfile::tempdir().unwrap();
        let (archive, index) = verification_fixture(temporary.path());
        assert!(archive.is_dir(), "the range fixture must exercise cached child files");
        let metadata = index.metadata();
        let displaced = temporary.path().join("displaced-archive.swdump");

        std::fs::rename(&archive, &displaced).unwrap();
        std::fs::create_dir(&archive).unwrap();

        let error = ArchiveBrowseIndex::open_request(&metadata, &archive).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("archive request root was replaced after layout validation")
        );
    }

    fn scribunto_archive_fixture(root: &std::path::Path) -> ArchiveBrowseIndex {
        let archive = root.join("scribunto-namespace.swdump");
        let mut writer = ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 1).unwrap();
        let pages = [
            (1, "Население/STA-00N", Some(10), "population template"),
            (
                2,
                "Arguments",
                Some(828),
                r#"
                    local p = {}
                    function p.getArgs(frame, options)
                        local parent = frame:getParent()
                        if parent then
                            for _, wrapper in ipairs(options.wrappers or {}) do
                                if parent.title == wrapper then
                                    return parent.args
                                end
                            end
                        end
                        return frame.args
                    end
                    return p
                "#,
            ),
            (
                3,
                "Round",
                Some(828),
                r#"
                    local arguments = require('Module:Arguments')
                    local p = {}
                    function p.main(frame)
                        local args = arguments.getArgs(frame, { wrappers = { 'Template:Round' } })
                        return args['1'] .. '!'
                    end
                    return p
                "#,
            ),
            (
                4,
                "Statistical",
                Some(828),
                r#"
                    local p = {}
                    function p.main(frame)
                        local templatename = 'Население/STA-00N'
                        local page = mw.title.new(templatename, 10)
                        return tostring(page.namespace) .. '|' .. (page:getContent() or 'nil')
                    end
                    return p
                "#,
            ),
        ];
        for (page_id, title, namespace, text) in pages {
            writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: 10_000_000,
                    title: title.into(),
                    namespace,
                    deleted: false,
                })
                .unwrap();
            writer
                .write(&Record::Revision {
                    page_id,
                    revision: verification_revision(page_id, page_id as u64, text.as_bytes(), true),
                })
                .unwrap();
        }
        writer
            .write(&Record::Manifest {
                timestamp_micros: i64::MAX,
                manifest: crate::archive::ManifestRecord {
                    wiki_db: "ruwiki".into(),
                    content_snapshot: "fixture".into(),
                    metadata_snapshot: "fixture".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: i64::MAX,
                site_info: SiteInfoRecord {
                    site_name: "Russian Wikipedia fixture".into(),
                    db_name: "ruwiki".into(),
                    base: "https://ru.wikipedia.org/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "ru".into(),
                    rtl: false,
                    server: "https://ru.wikipedia.org".into(),
                    script_path: "/w".into(),
                    namespaces: vec![
                        crate::archive::SiteNamespaceRecord {
                            id: 0,
                            case: "first-letter".into(),
                            localized_name: String::new(),
                            aliases: Vec::new(),
                        },
                        crate::archive::SiteNamespaceRecord {
                            id: 10,
                            case: "first-letter".into(),
                            localized_name: "Шаблон".into(),
                            aliases: Vec::new(),
                        },
                        crate::archive::SiteNamespaceRecord {
                            id: 11,
                            case: "first-letter".into(),
                            localized_name: "Обсуждение шаблона".into(),
                            aliases: Vec::new(),
                        },
                        crate::archive::SiteNamespaceRecord {
                            id: 828,
                            case: "first-letter".into(),
                            localized_name: "Модуль".into(),
                            aliases: Vec::new(),
                        },
                        crate::archive::SiteNamespaceRecord {
                            id: 829,
                            case: "first-letter".into(),
                            localized_name: "Обсуждение модуля".into(),
                            aliases: Vec::new(),
                        },
                        crate::archive::SiteNamespaceRecord {
                            id: 1000,
                            case: "first-letter".into(),
                            localized_name: "Расширение".into(),
                            aliases: Vec::new(),
                        },
                    ],
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        writer.finish().unwrap();

        let title_index = root.join("scribunto-namespace.swtitle");
        crate::title_index::build(
            &archive,
            &title_index,
            &crate::generation::GenerationId::from_plan_bytes(b"scribunto-namespace-test"),
        )
        .unwrap();
        ArchiveBrowseIndex::open(&archive, &title_index).unwrap()
    }

    #[test]
    fn archive_view_reconstructs_fixed_canonical_namespace_names() {
        let temporary = tempfile::tempdir().unwrap();
        let index = scribunto_archive_fixture(temporary.path());
        let view = index.view(None);

        assert_eq!(view.site().namespaces[&10].canonical, "Template");
        assert_eq!(view.site().namespaces[&11].canonical, "Template talk");
        assert_eq!(view.site().namespaces[&828].canonical, "Module");
        assert_eq!(view.site().namespaces[&829].canonical, "Module talk");
        assert!(view.site().namespaces[&1000].canonical.is_empty());
    }

    #[test]
    fn archive_view_scribunto_uses_canonical_template_titles() {
        let temporary = tempfile::tempdir().unwrap();
        let index = scribunto_archive_fixture(temporary.path());
        let view = index.view(None);
        let invoker = LuaInvoker::default();

        let numeric_title_frame = Frame {
            args: BTreeMap::new(),
            parent: None,
            title: "Модуль:Statistical".into(),
        };
        assert_eq!(
            invoker
                .invoke("Statistical", "main", &numeric_title_frame, &view)
                .unwrap(),
            "10|population template"
        );

        let wrapper_frame = Frame {
            args: BTreeMap::new(),
            parent: Some(Box::new(Frame {
                args: BTreeMap::from([("1".into(), "Россия".into())]),
                parent: None,
                title: "Шаблон:Round".into(),
            })),
            title: "Модуль:Round".into(),
        };
        assert_eq!(
            invoker
                .invoke("Round", "main", &wrapper_frame, &view)
                .unwrap(),
            "Россия!"
        );
    }

    #[test]
    fn indexes_titles_and_reads_one_page_from_its_frame() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.swdump");
        let path = temporary.path().join("sample.swdump");
        let mut writer = ArchiveWriter::new(std::fs::File::create(&source).unwrap(), 1).unwrap();
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
        for rev_id in (2_u64..=9).rev() {
            let text = format!("old-{rev_id}").into_bytes();
            writer
                .write(&Record::Revision {
                    page_id: 7,
                    revision: RevisionRecord {
                        meta: RevisionMeta {
                            rev_id,
                            parent_id: rev_id + 1,
                            ts: Utc.timestamp_opt((rev_id * 10) as i64, 0).unwrap(),
                            contributor: ContributorMeta::Hidden,
                            comment: String::new(),
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
        let output =
            crate::archive_set::ArchiveSetOutput::new_in(temporary.path(), 1024).unwrap();
        let bootstrap = tempfile::tempfile_in(temporary.path()).unwrap();
        let (output, _, _, _) =
            crate::archive::merge_many_archives_bootstrapping_ref_prefix(
                &[source],
                output,
                bootstrap,
                128,
                crate::archive::CompressionSettings::default(),
                1 << 20,
                512 << 10,
            )
            .unwrap();
        output.finish().unwrap().persist(&path).unwrap();

        let title_index = temporary.path().join("sample.swtitle");
        crate::title_index::build(
            &path,
            &title_index,
            &crate::generation::GenerationId::from_plan_bytes(b"archive-browse-test"),
        )
        .unwrap();
        let (_, archive_frames, complete) = crate::archive::index_file(&path).unwrap();
        assert!(complete);
        assert_eq!(
            std::fs::metadata(&title_index).unwrap().len(),
            96 + 16
                + archive_frames.len() as u64 * 64
                + crate::archive_set::ArchiveSetReader::open(&path)
                    .unwrap()
                    .segments()
                    .len() as u64
                    * 40
        );
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
        let mut cursor = index
            .page_revision_text_cursor(7, 1024)
            .unwrap()
            .unwrap();
        assert_eq!(cursor.text(0).unwrap().unwrap().as_ref(), b"hello");
        assert_eq!(
            cursor.next_revision, 1,
            "the live zstd cursor must not prefetch older revisions"
        );
        assert_eq!(cursor.text(1).unwrap().unwrap().as_ref(), b"old");
        assert_eq!(cursor.next_revision, 2);
        assert_eq!(cursor.text(0).unwrap().unwrap().as_ref(), b"hello");
        assert_eq!(
            cursor.next_revision, 2,
            "a short newer move uses the bounded cache without restarting"
        );

        let mut summaries = index
            .page_revision_summary_cursor(7, 512, 1 << 20)
            .unwrap()
            .unwrap();
        assert_eq!(summaries.known_count(), None);
        assert_eq!(summaries.observed_count(), 0);
        assert_eq!(summaries.cache_entries(), 0);
        assert_eq!(summaries.summary(0).unwrap().unwrap().revision_id, 11);
        assert_eq!(summaries.observed_count(), 1);
        assert_eq!(summaries.known_count(), None);
        assert!(summaries.cache_entries() <= 8);
        assert!(summaries.cache_bytes() <= 512);
        assert_eq!(summaries.summary(9).unwrap().unwrap().revision_id, 2);
        assert_eq!(summaries.observed_count(), 10);
        assert_eq!(summaries.known_count(), None);
        assert_eq!(summaries.summary(0).unwrap().unwrap().revision_id, 11);
        assert!(summaries.restart_count() >= 1);
        assert_eq!(summaries.scan_count().unwrap(), 10);
        assert_eq!(summaries.known_count(), Some(10));
        assert_eq!(summaries.observed_count(), 10);
        assert!(summaries.cache_entries() <= 8);
        assert!(summaries.cache_bytes() <= 512);
        assert_eq!(
            summaries
                .find_at_or_before(110_000_000)
                .unwrap(),
            Some(1)
        );
        let mut interleaved_summaries = index
            .page_revision_summary_cursor(7, 512, 1 << 20)
            .unwrap()
            .unwrap();
        let mut interleaved_text = index
            .page_revision_text_cursor_bounded(7, 1024, 1 << 20)
            .unwrap()
            .unwrap();
        assert_eq!(interleaved_summaries.scan_count().unwrap(), 10);
        assert_eq!(
            interleaved_text.text(0).unwrap().unwrap().as_ref(),
            b"hello",
            "a retained metadata cursor must not move a retained text cursor's file offset",
        );
        assert!(matches!(
            index.page_text_at_bounded(7, i64::MAX, 4, 1 << 20),
            Err(crate::archive::ArchiveError::PageTextTooLarge {
                page_id: 7,
                bytes: 5,
                limit: 4,
            })
        ));
        let mut bounded_text = index
            .page_revision_text_cursor_bounded(7, 1024, 4)
            .unwrap()
            .unwrap();
        assert!(matches!(
            bounded_text.text(0),
            Err(crate::archive::ArchiveError::RecordPayloadTooLarge {
                kind: 2,
                ..
            })
        ));

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
                &regex::Regex::new("^(hello|old)$").unwrap(),
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

        let old_generation = temporary.path().join("old-generation.swdump");
        std::fs::rename(&path, old_generation).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            index.page_text_at(7, i64::MAX).unwrap(),
            Some(b"hello".to_vec()),
            "an attached reader must keep reading its opened generation"
        );
    }

    #[test]
    fn bounded_view_applies_limits_to_recursive_template_reads() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("bounded-view.swdump");
        let mut writer = ArchiveWriter::new(
            std::fs::File::create(&archive).unwrap(),
            1,
        )
        .unwrap();
        writer
            .write(&Record::PageState {
                page_id: 1,
                timestamp_micros: 2_000_000,
                title: "Main Page".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        writer
            .write(&Record::Revision {
                page_id: 1,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: 1,
                        parent_id: 0,
                        ts: Utc.timestamp_opt(1, 0).unwrap(),
                        contributor: ContributorMeta::Hidden,
                        comment: String::new(),
                        sha1: String::new(),
                        flags: 0,
                        text_len: 16,
                    },
                    has_text: true,
                    text: b"{{Template:Box}}".to_vec(),
                    visibility: None,
                    history: None,
                },
            })
            .unwrap();
        writer
            .write(&Record::PageState {
                page_id: 2,
                timestamp_micros: 2_000_000,
                title: "Box".into(),
                namespace: Some(10),
                deleted: false,
            })
            .unwrap();
        let template_text = b"oversized-template-body".to_vec();
        writer
            .write(&Record::Revision {
                page_id: 2,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: 2,
                        parent_id: 0,
                        ts: Utc.timestamp_opt(1, 0).unwrap(),
                        contributor: ContributorMeta::Hidden,
                        comment: String::new(),
                        sha1: String::new(),
                        flags: 0,
                        text_len: template_text.len() as u64,
                    },
                    has_text: true,
                    text: template_text,
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
                    language: "en".into(),
                    rtl: false,
                    server: "https://test.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: vec![
                        crate::archive::SiteNamespaceRecord {
                            id: 0,
                            case: "first-letter".into(),
                            localized_name: String::new(),
                            aliases: Vec::new(),
                        },
                        crate::archive::SiteNamespaceRecord {
                            id: 10,
                            case: "first-letter".into(),
                            localized_name: "Template".into(),
                            aliases: Vec::new(),
                        },
                    ],
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        writer.finish().unwrap();

        let title_index = temporary.path().join("bounded-view.swtitle");
        crate::title_index::build(
            &archive,
            &title_index,
            &crate::generation::GenerationId::from_plan_bytes(b"bounded-view-test"),
        )
        .unwrap();
        let index = ArchiveBrowseIndex::open(&archive, &title_index).unwrap();
        let view = index.view_bounded(None, 16, 1 << 20);
        let main = wikimak_wikitext::Title::parse("Main Page", view.site());
        let template = wikimak_wikitext::Title::parse("Template:Box", view.site());
        assert_eq!(
            wikimak_wikitext::PageStore::page_text(&view, &main).as_deref(),
            Some("{{Template:Box}}")
        );
        assert_eq!(
            wikimak_wikitext::PageStore::page_text(&view, &template),
            None,
            "recursive template reads use the same page bound"
        );
        let rendered = wikimak_wikitext::render(
            &view,
            &main,
            "{{Template:Box}}",
            &wikimak_wikitext::RenderOptions::default(),
        );
        assert!(!rendered.html.contains("oversized-template-body"));
    }

    #[test]
    fn indexed_category_members_are_available_to_the_archive_view() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("category.swdump");
        let mut writer = ArchiveWriter::new(
            std::fs::File::create(&archive).unwrap(),
            1,
        )
        .unwrap();
        writer
            .write(&Record::PageState {
                page_id: 1,
                timestamp_micros: 2_000_000,
                title: "Birds".into(),
                namespace: Some(14),
                deleted: false,
            })
            .unwrap();
        writer
            .write(&Record::PageState {
                page_id: 2,
                timestamp_micros: 2_000_000,
                title: "Sparrow".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        writer
            .write(&Record::Revision {
                page_id: 2,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: 2,
                        parent_id: 0,
                        ts: Utc.timestamp_opt(1, 0).unwrap(),
                        contributor: ContributorMeta::Hidden,
                        comment: String::new(),
                        sha1: String::new(),
                        flags: 0,
                        text_len: 18,
                    },
                    has_text: true,
                    text: b"[[Category:Birds]]".to_vec(),
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
                    language: "en".into(),
                    rtl: false,
                    server: "https://test.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: vec![
                        crate::archive::SiteNamespaceRecord {
                            id: 0,
                            case: "first-letter".into(),
                            localized_name: String::new(),
                            aliases: Vec::new(),
                        },
                        crate::archive::SiteNamespaceRecord {
                            id: 14,
                            case: "first-letter".into(),
                            localized_name: "Category".into(),
                            aliases: Vec::new(),
                        },
                    ],
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        writer.finish().unwrap();

        let title_index = temporary.path().join("category.swtitle");
        crate::title_index::build(
            &archive,
            &title_index,
            &crate::generation::GenerationId::from_plan_bytes(
                b"archive-browse-search-test",
            ),
        )
        .unwrap();
        let sidecar = temporary.path().join("category.swrefs");
        crate::backrefs::build(&archive, &title_index, &sidecar).unwrap();
        let index = ArchiveBrowseIndex::open(&archive, &title_index).unwrap();
        assert_eq!(
            index.category_member_titles(1).unwrap(),
            Some(vec!["Sparrow".to_string()])
        );
        let view = index.view(None);
        let category = wikimak_wikitext::Title::parse(
            "Category:Birds",
            wikimak_wikitext::PageStore::site(&view),
        );
        let rendered = wikimak_wikitext::render(
            &view,
            &category,
            "description",
            &wikimak_wikitext::RenderOptions {
                link_prefix: "/wiki/".into(),
                ..Default::default()
            },
        );
        assert!(rendered.html.contains("Sparrow"));

        std::fs::write(&sidecar, b"stale optional index").unwrap();
        let without_stale_backrefs =
            ArchiveBrowseIndex::open(&archive, &title_index).unwrap();
        assert_eq!(
            without_stale_backrefs.page_text_at(2, i64::MAX).unwrap(),
            Some(b"[[Category:Birds]]".to_vec()),
            "a stale optional backref sidecar must not make archive content unreadable"
        );
    }

    #[test]
    fn verification_sample_is_deterministic_and_spans_page_frames() {
        let temporary = tempfile::tempdir().unwrap();
        let (_archive, index) = verification_fixture(temporary.path());

        let first = index.deterministic_sample_pages(8).unwrap();
        let second = index.deterministic_sample_pages(8).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 8);
        assert_eq!(first.first().unwrap().0, 1);
        assert_eq!(first.last().unwrap().0, 8);
        assert_eq!(first.iter().map(|(page_id, _)| *page_id).collect::<Vec<_>>(), (1..=8).collect::<Vec<_>>());
    }

    #[test]
    fn verification_reads_revision_order_and_empty_or_unavailable_text() {
        let temporary = tempfile::tempdir().unwrap();
        let (_archive, index) = verification_fixture(temporary.path());

        let report = index.verify_deterministic_sample().unwrap();
        assert_eq!(report.revision_bodies_read, 24);
        assert_eq!(report.max_revision_bodies, 24);

        let page_one = report
            .sampled_pages
            .iter()
            .find(|page| page.page_id == 1)
            .unwrap();
        assert_eq!(
            page_one
                .revisions
                .iter()
                .map(|revision| (revision.role, revision.revision_id))
                .collect::<Vec<_>>(),
            vec![("oldest", 101), ("middle", 102), ("newest", 103)]
        );

        let empty = report
            .sampled_pages
            .iter()
            .find(|page| page.page_id == 4)
            .unwrap()
            .revisions
            .iter()
            .find(|revision| revision.revision_id == 403)
            .unwrap();
        assert!(empty.has_text);
        assert!(empty.text_read);
        assert_eq!(empty.text_len, 0);
        assert_eq!(empty.observed_text_len, Some(0));

        let unavailable = report
            .sampled_pages
            .iter()
            .find(|page| page.page_id == 8)
            .unwrap()
            .revisions
            .iter()
            .find(|revision| revision.revision_id == 802)
            .unwrap();
        assert!(!unavailable.has_text);
        assert!(!unavailable.text_read);
        assert_eq!(unavailable.observed_text_len, None);

        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"selected_generation\""));
        assert!(json.contains("\"page_id\":1"));
        assert!(json.contains("\"revision_id\":101"));
    }

    #[test]
    fn verification_attributes_a_corrupt_selected_segment() {
        let temporary = tempfile::tempdir().unwrap();
        let (archive, index) = verification_fixture(temporary.path());
        let segment = crate::archive_set::ArchiveSetReader::open(&archive)
            .unwrap()
            .segments()
            .iter()
            .find(|segment| segment.kind == Some(EntityKind::Page))
            .unwrap()
            .name
            .clone();
        std::fs::OpenOptions::new()
            .write(true)
            .open(archive.join(segment))
            .unwrap()
            .set_len(0)
            .unwrap();

        let error = index.verify_deterministic_sample().unwrap_err().to_string();
        assert!(error.contains("read") || error.contains("corrupt"), "{error}");
        assert!(error.contains("page"), "{error}");
    }
}
