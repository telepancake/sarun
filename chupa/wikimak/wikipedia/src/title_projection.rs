//! Scratch-backed, globally correct title-history projection.
//!
//! Records arrive in page-ID order. Only one page is retained while title
//! facts are emitted into bounded external-sort runs. Current-title conflicts
//! are then resolved globally with disk-backed page and owner tables.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use sha2::Digest;

use crate::archive::{ArchiveError, Record, Result, SiteInfoRecord};
use crate::title_index::{Interval, TitleIndexEntry};

const PAGE_BYTES: u64 = 40;
const ASSIGNMENT_BYTES: usize = 32;
pub(crate) const PROJECTION_FILE_MAGIC: [u8; 8] = *b"SWTPROJ\0";
pub(crate) const PROJECTION_FILE_VERSION: u32 = 2;
pub(crate) const PROJECTION_FILE_HEADER_BYTES: u64 = 16;
pub(crate) const PROJECTION_ENTRY_BYTES: u64 = 24;
const ABSENT: u64 = u64::MAX;

pub(crate) fn projection_file_bytes(entries: u64) -> Option<u64> {
    PROJECTION_FILE_HEADER_BYTES.checked_add(
        entries.checked_mul(PROJECTION_ENTRY_BYTES)?,
    )
}

pub(crate) fn legacy_projection_file_bytes(entries: u64) -> Option<u64> {
    entries.checked_mul(16)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProjectionLimits {
    pub(crate) run_bytes: usize,
    pub(crate) merge_fan_in: usize,
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self {
            run_bytes: 64 << 20,
            merge_fan_in: 32,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum FactPayload {
    Candidate {
        page_index: u64,
        rank: u8,
        start: i64,
        page_id: u64,
        namespace: i32,
    },
    Interval {
        start: i64,
        end: i64,
        page_id: u64,
        namespace: i32,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Fact {
    title: String,
    payload: FactPayload,
}

impl Fact {
    fn memory_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.title.capacity())
    }
}

struct FactSorter {
    root: PathBuf,
    limits: ProjectionLimits,
    buffered_bytes: usize,
    buffered: Vec<Fact>,
    runs: Vec<PathBuf>,
}

/// One current-title candidate, keyed by `(page_index, rank)`.
///
/// The rank lives in the low bit because page indices are necessarily below
/// 2^63. This keeps the external-sort record at 32 bytes and, more
/// importantly, lets the page table be built by one sequential join instead
/// of two seeks and a tiny write for every candidate.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CandidateAssignment {
    page_and_rank: u64,
    title_ordinal: u64,
    start: i64,
    page_id: u64,
}

impl CandidateAssignment {
    fn new(
        page_index: u64,
        rank: u8,
        title_ordinal: u64,
        start: i64,
        page_id: u64,
    ) -> Result<Self> {
        if rank > 1 || page_index >= (1_u64 << 63) {
            return Err(ArchiveError::Invalid(
                "invalid external title candidate",
            ));
        }
        Ok(Self {
            page_and_rank: (page_index << 1) | u64::from(rank),
            title_ordinal,
            start,
            page_id,
        })
    }

    fn page_index(self) -> u64 {
        self.page_and_rank >> 1
    }

    fn rank(self) -> u8 {
        (self.page_and_rank & 1) as u8
    }
}

struct AssignmentSorter {
    root: PathBuf,
    limits: ProjectionLimits,
    buffered: Vec<CandidateAssignment>,
    runs: Vec<PathBuf>,
}

impl AssignmentSorter {
    fn new(root: &Path, limits: ProjectionLimits) -> Self {
        Self {
            root: root.to_path_buf(),
            limits,
            buffered: Vec::new(),
            runs: Vec::new(),
        }
    }

    fn push(&mut self, assignment: CandidateAssignment) -> Result<()> {
        self.buffered.push(assignment);
        if self
            .buffered
            .len()
            .saturating_mul(ASSIGNMENT_BYTES)
            >= self.limits.run_bytes
        {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }
        self.buffered
            .sort_unstable_by_key(|assignment| assignment.page_and_rank);
        for adjacent in self.buffered.windows(2) {
            if adjacent[0].page_and_rank == adjacent[1].page_and_rank {
                return Err(ArchiveError::Invalid(
                    "duplicate external title candidate rank",
                ));
            }
        }
        let path = self
            .root
            .join(format!("assignment-{:08}.run", self.runs.len()));
        let mut output = BufWriter::new(std::fs::File::create(&path)?);
        for assignment in self.buffered.drain(..) {
            write_assignment(&mut output, assignment)?;
        }
        output.flush()?;
        self.runs.push(path);
        Ok(())
    }

    fn finish(mut self) -> Result<PathBuf> {
        self.flush()?;
        if self.runs.is_empty() {
            let path = self.root.join("assignments-empty.run");
            std::fs::File::create(&path)?;
            return Ok(path);
        }
        let mut stage = 0_usize;
        while self.runs.len() > 1 {
            let mut next = Vec::new();
            for (group, inputs) in self.runs.chunks(self.limits.merge_fan_in).enumerate() {
                let path = self
                    .root
                    .join(format!("assignment-merge-{stage:04}-{group:08}.run"));
                merge_assignment_runs(inputs, &path)?;
                next.push(path);
            }
            for path in &self.runs {
                std::fs::remove_file(path)?;
            }
            self.runs = next;
            stage += 1;
        }
        Ok(self.runs.pop().expect("nonempty assignment runs"))
    }
}

impl FactSorter {
    fn new(root: &Path, limits: ProjectionLimits) -> Result<Self> {
        if limits.run_bytes == 0 || limits.merge_fan_in < 2 {
            return Err(ArchiveError::Invalid(
                "title projection requires nonzero runs and merge fan-in >= 2",
            ));
        }
        Ok(Self {
            root: root.to_path_buf(),
            limits,
            buffered_bytes: 0,
            buffered: Vec::new(),
            runs: Vec::new(),
        })
    }

    fn push(&mut self, fact: Fact) -> Result<()> {
        self.buffered_bytes = self.buffered_bytes.saturating_add(fact.memory_bytes());
        self.buffered.push(fact);
        if self.buffered_bytes >= self.limits.run_bytes {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }
        self.buffered.sort_unstable();
        let path = self.root.join(format!("fact-{:08}.run", self.runs.len()));
        let mut output = BufWriter::new(std::fs::File::create(&path)?);
        for fact in self.buffered.drain(..) {
            write_fact(&mut output, &fact)?;
        }
        output.flush()?;
        self.buffered_bytes = 0;
        self.runs.push(path);
        Ok(())
    }

    fn finish(mut self) -> Result<PathBuf> {
        self.flush()?;
        if self.runs.is_empty() {
            let path = self.root.join("facts-empty.run");
            std::fs::File::create(&path)?;
            return Ok(path);
        }
        let mut stage = 0_usize;
        while self.runs.len() > 1 {
            let mut next = Vec::new();
            for (group, inputs) in self.runs.chunks(self.limits.merge_fan_in).enumerate() {
                let path = self
                    .root
                    .join(format!("fact-merge-{stage:04}-{group:08}.run"));
                merge_fact_runs(inputs, &path)?;
                next.push(path);
            }
            for path in &self.runs {
                std::fs::remove_file(path)?;
            }
            self.runs = next;
            stage += 1;
        }
        Ok(self.runs.pop().expect("nonempty runs"))
    }
}

pub(crate) struct ExternalTitleProjectionBuilder {
    scratch: tempfile::TempDir,
    site_info: SiteInfoRecord,
    limits: ProjectionLimits,
    facts: FactSorter,
    page_ids: BufWriter<std::fs::File>,
    page_ids_path: PathBuf,
    page_count: u64,
    current_page: Option<u64>,
    states: Vec<(i64, String, Option<i64>, bool)>,
    actions: Vec<(i64, crate::archive::PageActionRecord)>,
}

impl ExternalTitleProjectionBuilder {
    pub(crate) fn new_in(
        scratch: impl AsRef<Path>,
        site_info: SiteInfoRecord,
        limits: ProjectionLimits,
    ) -> Result<Self> {
        let scratch = tempfile::tempdir_in(scratch)?;
        let page_ids_path = scratch.path().join("page-ids");
        let page_ids = BufWriter::new(std::fs::File::create(&page_ids_path)?);
        let facts = FactSorter::new(scratch.path(), limits)?;
        Ok(Self {
            scratch,
            site_info,
            limits,
            facts,
            page_ids,
            page_ids_path,
            page_count: 0,
            current_page: None,
            states: Vec::new(),
            actions: Vec::new(),
        })
    }

    pub(crate) fn observe(&mut self, record: &Record) -> Result<()> {
        let page_id = match record {
            Record::PageState { page_id, .. } => *page_id,
            Record::PageAction { entity, .. }
                if entity.kind == crate::archive::EntityKind::Page =>
            {
                entity.id
            }
            _ => return Ok(()),
        };
        if self.current_page.is_some_and(|current| page_id < current) {
            return Err(ArchiveError::Invalid(
                "title projection records are not in page-ID order",
            ));
        }
        if self.current_page.is_some_and(|current| page_id != current) {
            self.finish_page()?;
        }
        self.current_page = Some(page_id);
        match record {
            Record::PageState {
                timestamp_micros,
                title,
                namespace,
                deleted,
                ..
            } => self.states.push((
                *timestamp_micros,
                title.clone(),
                *namespace,
                *deleted,
            )),
            Record::PageAction {
                timestamp_micros,
                action,
                ..
            } => self.actions.push((*timestamp_micros, action.clone())),
            _ => {}
        }
        Ok(())
    }

    fn finish_page(&mut self) -> Result<()> {
        let Some(page_id) = self.current_page.take() else {
            return Ok(());
        };
        let projection = crate::title_index::project_page_inputs(
            page_id,
            (
                std::mem::take(&mut self.states),
                std::mem::take(&mut self.actions),
            ),
            &self.site_info,
        );
        self.emit_projection(projection)
    }

    fn emit_projection(&mut self, projection: crate::title_index::Projection) -> Result<()> {
        if projection.candidates.len() > 2 {
            return Err(ArchiveError::Invalid(
                "page title projection has more than two current candidates",
            ));
        }
        let page_id = projection.page_id;
        let page_index = self.begin_projection(page_id)?;
        for interval in projection.closed {
            self.emit_interval_fact(interval)?;
        }
        self.emit_candidates(page_index, page_id, projection.candidates)
    }

    fn begin_projection(&mut self, page_id: u64) -> Result<u64> {
        self.page_ids.write_all(&page_id.to_le_bytes())?;
        let page_index = self.page_count;
        self.page_count = self
            .page_count
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        Ok(page_index)
    }

    fn emit_interval_fact(&mut self, interval: crate::title_index::Interval) -> Result<()> {
        self.facts.push(Fact {
            title: interval.title,
            payload: FactPayload::Interval {
                start: interval.start,
                end: interval.end,
                page_id: interval.page_id,
                namespace: interval.namespace,
            },
        })
    }

    fn emit_candidates(
        &mut self,
        page_index: u64,
        page_id: u64,
        candidates: Vec<crate::title_index::TitleCandidate>,
    ) -> Result<()> {
        for (rank, candidate) in candidates.into_iter().enumerate() {
            self.facts.push(Fact {
                title: candidate.title,
                payload: FactPayload::Candidate {
                    page_index,
                    rank: rank as u8,
                    start: candidate.start,
                    page_id,
                    namespace: candidate.namespace,
                },
            })?;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<ExternalTitleEntries> {
        self.finish_page()?;
        self.page_ids.flush()?;
        let facts = self.facts.finish()?;
        build_external_entries(
            self.scratch,
            facts,
            &self.page_ids_path,
            self.page_count,
            self.site_info,
            self.limits,
        )
    }
}

pub(crate) fn project_title_record_archives(
    inputs: impl IntoIterator<Item = (PathBuf, u64)>,
    site_info: SiteInfoRecord,
    scratch: impl AsRef<Path>,
    limits: ProjectionLimits,
) -> Result<ExternalTitleEntries> {
    let mut builder = ExternalTitleProjectionBuilder::new_in(scratch, site_info, limits)?;
    for (input, expected_records) in inputs {
        let mut records = crate::archive::ArchiveRecordReader::open(input)?;
        let mut observed_records = 0_u64;
        while let Some(record) = records.next_record()? {
            observed_records = observed_records
                .checked_add(1)
                .ok_or(ArchiveError::FieldTooLarge)?;
            builder.observe(&record)?;
        }
        if observed_records != expected_records {
            return Err(ArchiveError::Invalid(
                "title-projection archive record count does not match its receipt",
            ));
        }
    }
    builder.finish()
}

pub(crate) struct ExternalTitleEntries {
    scratch: Option<tempfile::TempDir>,
    path: PathBuf,
    entries: Option<memmap2::Mmap>,
    identity: [u8; 32],
}

impl ExternalTitleEntries {
    pub(crate) fn iter(&self) -> ExternalTitleEntryIter<'_> {
        ExternalTitleEntryIter {
            bytes: self.entries.as_deref().unwrap_or(&[]),
            offset: PROJECTION_FILE_HEADER_BYTES as usize,
        }
    }

    /// Open a bound projection after a full SHA-256 pass over its payload.
    ///
    /// Recovery deliberately pays O(index bytes) once. The no-follow file
    /// handle is retained for the subsequent mapping, so a same-path
    /// replacement after the check cannot change the mapped inode.
    pub(crate) fn open_bound(
        path: impl AsRef<Path>,
        expected_identity: &str,
        expected_entries: u64,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let identity_bytes = hex::decode(expected_identity)
            .map_err(|_| ArchiveError::Invalid("invalid title projection identity"))?;
        let identity: [u8; 32] = identity_bytes
            .try_into()
            .map_err(|_| ArchiveError::Invalid("invalid title projection identity"))?;
        let expected_name = format!("title-projection-{expected_identity}.entries");
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
            return Err(ArchiveError::Invalid(
                "persisted title projection does not match its structural binding",
            ));
        }
        let mut file = open_read_no_follow(&path)?;
        let bytes = file.metadata()?.len();
        if legacy_projection_file_bytes(expected_entries) == Some(bytes) {
            return Err(ArchiveError::Invalid(
                "title projection uses the legacy 16-byte format; rebuild required",
            ));
        }
        if projection_file_bytes(expected_entries) != Some(bytes) {
            return Err(ArchiveError::Invalid(
                "persisted title projection does not match its structural binding",
            ));
        }
        read_projection_header(&mut file)?;
        let observed = digest_file(&mut file)?;
        if observed != identity {
            return Err(ArchiveError::Invalid(
                "persisted title projection content identity mismatch",
            ));
        }
        let entries = if bytes == 0 {
            None
        } else {
            Some(unsafe { memmap2::MmapOptions::new().map(&file)? })
        };
        Ok(Self {
            scratch: None,
            path,
            entries,
            identity,
        })
    }

    /// Publish the projection under its content identity.
    ///
    /// The payload is synced before a no-clobber link/copy and both affected
    /// directories are synced afterwards. A receipt can therefore safely name
    /// the returned file: interruption before the receipt leaves only an
    /// unreferenced, discardable content-addressed file.
    pub(crate) fn persist_content_addressed(
        mut self,
        destination_directory: impl AsRef<Path>,
    ) -> Result<Self> {
        let destination_directory = destination_directory.as_ref();
        let destination = destination_directory.join(format!(
            "title-projection-{}.entries",
            hex::encode(self.identity)
        ));
        self.entries = None;
        open_read_no_follow(&self.path)?.sync_all()?;
        let source_parent = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        match publish_without_replacement(&self.path, &destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let matches = open_read_no_follow(&destination)
                    .and_then(|mut file| digest_file(&mut file))
                    .map(|digest| digest == self.identity);
                match matches {
                    Ok(true) if self.path != destination => std::fs::remove_file(&self.path)?,
                    Ok(true) => {}
                    Ok(false) => {
                        return self.reject_existing_destination(
                            &destination_directory,
                            "content-addressed title projection has a conflicting digest",
                        )
                    }
                    Err(error) => {
                        return self.reject_existing_destination(
                            &destination_directory,
                            &format!("cannot verify existing content-addressed title projection: {error}"),
                        )
                    }
                }
            }
            Err(error) => return Err(ArchiveError::Io(error)),
        }
        sync_directory(destination_directory)?;
        if source_parent != destination_directory {
            sync_directory(&source_parent)?;
        }
        self.scratch = None;
        let file = open_read_no_follow(&destination)?;
        let bytes = file.metadata()?.len();
        let entries = if bytes == 0 {
            None
        } else {
            Some(unsafe { memmap2::MmapOptions::new().map(&file)? })
        };
        Ok(Self {
            scratch: None,
            path: destination,
            entries,
            identity: self.identity,
        })
    }

    fn reject_existing_destination(
        mut self,
        destination_directory: &Path,
        reason: &str,
    ) -> Result<Self> {
        self.entries = None;
        match retain_conflicting_source(&self.path, destination_directory, &self.identity) {
            Ok(_) => Err(ArchiveError::Invalid(
                "content-addressed title projection conflict; new copy was retained",
            )),
            Err(error) => {
                if let Some(scratch) = self.scratch.take() {
                    #[allow(deprecated)]
                    let _source = scratch.into_path();
                }
                let _ = (reason, error);
                Err(ArchiveError::Invalid(
                    "content-addressed title projection conflict; new copy was preserved in scratch",
                ))
            }
        }
    }

    pub(crate) fn entry_count(&self) -> u64 {
        self.entries.as_ref().map_or(0, |entries| {
            (entries.len() as u64 - PROJECTION_FILE_HEADER_BYTES) / PROJECTION_ENTRY_BYTES
        })
    }

    pub(crate) fn identity_hex(&self) -> String {
        hex::encode(self.identity)
    }

    pub(crate) fn file_name(&self) -> &std::ffi::OsStr {
        self.path.file_name().expect("projection file has a name")
    }
}

pub(crate) struct ExternalTitleEntryIter<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl Iterator for ExternalTitleEntryIter<'_> {
    type Item = TitleIndexEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self
            .bytes
            .get(self.offset..self.offset.checked_add(PROJECTION_ENTRY_BYTES as usize)?)?;
        if bytes.len() != PROJECTION_ENTRY_BYTES as usize {
            return None;
        }
        self.offset += PROJECTION_ENTRY_BYTES as usize;
        Some(TitleIndexEntry {
            coded_title: u64::from_le_bytes(bytes[..8].try_into().expect("title key")),
            time: u32::from_le_bytes(bytes[8..12].try_into().expect("title time")),
            page_id: u32::from_le_bytes(bytes[12..16].try_into().expect("page ID")),
            namespace: i32::from_le_bytes(bytes[16..20].try_into().expect("namespace")),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.bytes.len().saturating_sub(self.offset)
            / PROJECTION_ENTRY_BYTES as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ExternalTitleEntryIter<'_> {}

fn build_external_entries(
    scratch: tempfile::TempDir,
    facts: PathBuf,
    page_ids_path: &Path,
    page_count: u64,
    site_info: SiteInfoRecord,
    limits: ProjectionLimits,
) -> Result<ExternalTitleEntries> {
    let mut input = BufReader::new(std::fs::File::open(&facts)?);
    let mut assignments = AssignmentSorter::new(scratch.path(), limits);
    let mut previous_title = None::<String>;
    let mut title_count = 0_u64;
    while let Some(fact) = read_fact(&mut input)? {
        if previous_title.as_deref() != Some(fact.title.as_str()) {
            previous_title = Some(fact.title.clone());
            title_count = title_count
                .checked_add(1)
                .ok_or(ArchiveError::FieldTooLarge)?;
        }
        let title_ordinal = title_count - 1;
        if let FactPayload::Candidate {
            page_index,
            rank,
            start,
            page_id,
            ..
        } = fact.payload
        {
            if page_index >= page_count {
                return Err(ArchiveError::Invalid(
                    "invalid external title candidate",
                ));
            }
            assignments.push(CandidateAssignment::new(
                page_index,
                rank,
                title_ordinal,
                start,
                page_id,
            )?)?;
        }
    }
    let assignments = assignments.finish()?;

    let page_table_path = scratch.path().join("pages");
    build_page_table(
        page_ids_path,
        &assignments,
        &page_table_path,
        page_count,
    )?;
    if page_count == 0 {
        let (path, count, identity) = EntrySorter::new(scratch.path(), limits)?.finish()?;
        debug_assert_eq!(count, 0);
        return Ok(ExternalTitleEntries {
            scratch: Some(scratch),
            path,
            entries: None,
            identity,
        });
    }
    let table = std::fs::File::open(&page_table_path)?;
    let pages = unsafe { memmap2::MmapOptions::new().map(&table)? };

    if title_count == 0 {
        let (path, count, identity) = EntrySorter::new(scratch.path(), limits)?.finish()?;
        debug_assert_eq!(count, 0);
        return Ok(ExternalTitleEntries {
            scratch: Some(scratch),
            path,
            entries: None,
            identity,
        });
    }

    let owners_path = scratch.path().join("owners");
    let mut owners_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&owners_path)?;
    let owner_bytes = title_count
        .checked_mul(8)
        .ok_or(ArchiveError::FieldTooLarge)?;
    let absent = [0xff_u8; 64 << 10];
    let mut remaining = owner_bytes;
    while remaining != 0 {
        let take = remaining.min(absent.len() as u64) as usize;
        owners_file.write_all(&absent[..take])?;
        remaining -= take as u64;
    }
    owners_file.sync_all()?;
    let mut owners = unsafe { memmap2::MmapOptions::new().map_mut(&owners_file)? };

    let queue_path = scratch.path().join("proposals");
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&queue_path)?;
    let mut queue_output = BufWriter::new(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&queue_path)?,
    );
    for page_index in 0..page_count {
        propose(
            page_index,
            0,
            &pages,
            &mut owners,
            &mut queue_output,
        )?;
    }
    queue_output.flush()?;
    let mut queue_input = BufReader::new(std::fs::File::open(&queue_path)?);
    while let Some((page_index, rank)) = read_proposal(&mut queue_input)? {
        propose(
            page_index,
            rank,
            &pages,
            &mut owners,
            &mut queue_output,
        )?;
        if queue_input.fill_buf()?.is_empty() {
            queue_output.flush()?;
        }
    }
    queue_output.flush()?;
    owners.flush()?;

    let mut entries = EntrySorter::new(scratch.path(), limits)?;
    let mut facts_input = BufReader::new(std::fs::File::open(&facts)?);
    let mut current_title = None::<String>;
    let mut current_namespace = None::<i32>;
    let mut current_ordinal = 0_u64;
    let mut intervals = Vec::new();
    while let Some(fact) = read_fact(&mut facts_input)? {
        if current_title.as_deref() != Some(fact.title.as_str()) {
            if let Some(title) = current_title.take() {
                finish_title(
                    &title,
                    current_namespace.take().ok_or(ArchiveError::Invalid(
                        "external title fact has no namespace",
                    ))?,
                    current_ordinal,
                    &mut intervals,
                    &owners,
                    &pages,
                    &site_info,
                    &mut entries,
                )?;
                current_ordinal += 1;
            }
            current_title = Some(fact.title.clone());
        }
        let namespace = match &fact.payload {
            FactPayload::Candidate { namespace, .. } | FactPayload::Interval { namespace, .. } => {
                *namespace
            }
        };
        if current_namespace.is_some_and(|previous| previous != namespace) {
            return Err(ArchiveError::Invalid(
                "external title facts disagree on namespace",
            ));
        }
        current_namespace = Some(namespace);
        if let FactPayload::Interval {
            start,
            end,
            page_id,
            namespace: interval_namespace,
        } = fact.payload
        {
            intervals.push(Interval {
                title: fact.title,
                namespace: interval_namespace,
                start,
                end,
                page_id,
            });
        }
    }
    if let Some(title) = current_title {
        finish_title(
            &title,
            current_namespace
                .take()
                .ok_or(ArchiveError::Invalid("external title fact has no namespace"))?,
            current_ordinal,
            &mut intervals,
            &owners,
            &pages,
            &site_info,
            &mut entries,
        )?;
    }
    let (path, count, identity) = entries.finish()?;
    let entries = if count == 0 {
        None
    } else {
        let file = std::fs::File::open(&path)?;
        Some(unsafe { memmap2::MmapOptions::new().map(&file)? })
    };
    Ok(ExternalTitleEntries {
        scratch: Some(scratch),
        path,
        entries,
        identity,
    })
}

fn build_page_table(
    page_ids_path: &Path,
    assignments_path: &Path,
    output_path: &Path,
    page_count: u64,
) -> Result<()> {
    let mut page_ids = BufReader::new(std::fs::File::open(page_ids_path)?);
    let mut assignments = BufReader::new(std::fs::File::open(assignments_path)?);
    let mut next = read_assignment(&mut assignments)?;
    let mut output = BufWriter::new(std::fs::File::create(output_path)?);

    for page_index in 0..page_count {
        let page_id = read_u64(&mut page_ids)?.ok_or(ArchiveError::Invalid(
            "page-ID projection run ended early",
        ))?;
        let mut candidates = [(ABSENT, 0_i64); 2];
        while next.is_some_and(|assignment| assignment.page_index() == page_index) {
            let assignment = next.take().expect("matching assignment");
            if assignment.page_id != page_id {
                return Err(ArchiveError::Invalid(
                    "page candidate ID does not match page table",
                ));
            }
            let candidate = &mut candidates[usize::from(assignment.rank())];
            if candidate.0 != ABSENT {
                return Err(ArchiveError::Invalid(
                    "duplicate external title candidate rank",
                ));
            }
            *candidate = (assignment.title_ordinal, assignment.start);
            next = read_assignment(&mut assignments)?;
        }
        if next.is_some_and(|assignment| assignment.page_index() < page_index) {
            return Err(ArchiveError::Invalid(
                "external title candidate run is not sorted",
            ));
        }
        output.write_all(&page_id.to_le_bytes())?;
        for (title, start) in candidates {
            output.write_all(&title.to_le_bytes())?;
            output.write_all(&start.to_le_bytes())?;
        }
    }
    if read_u64(&mut page_ids)?.is_some() {
        return Err(ArchiveError::Invalid(
            "page-ID projection run has trailing entries",
        ));
    }
    if next.is_some() {
        return Err(ArchiveError::Invalid(
            "page candidate points outside page table",
        ));
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    Ok(())
}

fn propose(
    page_index: u64,
    rank: u8,
    pages: &[u8],
    owners: &mut [u8],
    queue: &mut impl Write,
) -> Result<()> {
    let (page_id, title, start) = page_candidate(pages, page_index, rank)?;
    if title == ABSENT {
        return Ok(());
    }
    let owner_offset = usize::try_from(
        title
            .checked_mul(8)
            .ok_or(ArchiveError::FieldTooLarge)?,
    )
    .map_err(|_| ArchiveError::FieldTooLarge)?;
    let previous = u64::from_le_bytes(
        owners[owner_offset..owner_offset + 8]
            .try_into()
            .expect("owner slice"),
    );
    if previous == ABSENT {
        owners[owner_offset..owner_offset + 8].copy_from_slice(&page_index.to_le_bytes());
        return Ok(());
    }
    let (previous_rank, previous_start, previous_page_id) =
        candidate_for_title(pages, previous, title)?;
    if (start, page_id) > (previous_start, previous_page_id) {
        owners[owner_offset..owner_offset + 8].copy_from_slice(&page_index.to_le_bytes());
        write_proposal(queue, previous, previous_rank.saturating_add(1))?;
    } else {
        write_proposal(queue, page_index, rank.saturating_add(1))?;
    }
    Ok(())
}

fn page_candidate(pages: &[u8], page_index: u64, rank: u8) -> Result<(u64, u64, i64)> {
    if rank > 1 {
        return Ok((0, ABSENT, 0));
    }
    let offset = usize::try_from(
        page_index
            .checked_mul(PAGE_BYTES)
            .ok_or(ArchiveError::FieldTooLarge)?,
    )
    .map_err(|_| ArchiveError::FieldTooLarge)?;
    let page = pages
        .get(offset..offset + PAGE_BYTES as usize)
        .ok_or(ArchiveError::Invalid("page candidate lies outside page table"))?;
    let candidate = 8 + usize::from(rank) * 16;
    Ok((
        u64::from_le_bytes(page[..8].try_into().unwrap()),
        u64::from_le_bytes(page[candidate..candidate + 8].try_into().unwrap()),
        i64::from_le_bytes(page[candidate + 8..candidate + 16].try_into().unwrap()),
    ))
}

fn candidate_for_title(
    pages: &[u8],
    page_index: u64,
    title: u64,
) -> Result<(u8, i64, u64)> {
    for rank in 0..=1 {
        let (page_id, candidate, start) = page_candidate(pages, page_index, rank)?;
        if candidate == title {
            return Ok((rank, start, page_id));
        }
    }
    Err(ArchiveError::Invalid(
        "title owner does not name one of its candidates",
    ))
}

fn finish_title(
    title: &str,
    namespace: i32,
    ordinal: u64,
    intervals: &mut Vec<Interval>,
    owners: &[u8],
    pages: &[u8],
    site_info: &SiteInfoRecord,
    entries: &mut EntrySorter,
) -> Result<()> {
    let offset = usize::try_from(
        ordinal
            .checked_mul(8)
            .ok_or(ArchiveError::FieldTooLarge)?,
    )
    .map_err(|_| ArchiveError::FieldTooLarge)?;
    let owner = u64::from_le_bytes(
        owners[offset..offset + 8]
            .try_into()
            .expect("owner slice"),
    );
    if owner != ABSENT {
        let (_, start, page_id) = candidate_for_title(pages, owner, ordinal)?;
        intervals.push(Interval {
            title: title.to_owned(),
            namespace,
            start,
            end: i64::MAX,
            page_id,
        });
    }
    intervals.sort_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then(left.end.cmp(&right.end))
            .then(left.page_id.cmp(&right.page_id))
    });
    let coded_title = crate::title_index::coded_title(title, site_info);
    let mut changes = std::collections::BTreeMap::<u32, (u32, i32)>::new();
    for (_, time, page_id, entry_namespace) in crate::title_index::ownership_changes(intervals) {
        // Multiple ownership changes may fall in the same second after the
        // archive's microsecond timestamps are projected to the index. The
        // later change in chronological order is authoritative for that key.
        changes.insert(
            crate::title_index::seconds(time),
            (
                u32::try_from(page_id).map_err(|_| ArchiveError::FieldTooLarge)?,
                entry_namespace,
            ),
        );
    }
    for (time, (page_id, entry_namespace)) in changes {
        entries.push(TitleIndexEntry {
            coded_title,
            time,
            page_id,
            namespace: entry_namespace,
        })?;
    }
    intervals.clear();
    Ok(())
}

struct EntrySorter {
    root: PathBuf,
    limits: ProjectionLimits,
    buffered: Vec<TitleIndexEntry>,
    runs: Vec<EntryRun>,
}

struct EntryRun {
    path: PathBuf,
    identity: [u8; 32],
}

impl EntrySorter {
    fn new(root: &Path, limits: ProjectionLimits) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            limits,
            buffered: Vec::new(),
            runs: Vec::new(),
        })
    }

    fn push(&mut self, entry: TitleIndexEntry) -> Result<()> {
        self.buffered.push(entry);
        if self
            .buffered
            .len()
            .saturating_mul(PROJECTION_ENTRY_BYTES as usize)
            >= self.limits.run_bytes
        {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }
        self.buffered.sort_unstable_by_key(|entry| {
            (entry.coded_title, entry.time, entry.page_id)
        });
        coalesce_entry_keys(&mut self.buffered);
        let path = self.root.join(format!("entry-{:08}.run", self.runs.len()));
        let mut output = DigestWriter::new(BufWriter::new(std::fs::File::create(&path)?));
        write_projection_header(&mut output)?;
        for entry in self.buffered.drain(..) {
            write_title_entry(&mut output, entry)?;
        }
        output.flush()?;
        self.runs.push(EntryRun {
            path,
            identity: output.identity(),
        });
        Ok(())
    }

    fn finish(mut self) -> Result<(PathBuf, usize, [u8; 32])> {
        self.flush()?;
        if self.runs.is_empty() {
            let path = self.root.join("entries-empty.run");
            let mut output = DigestWriter::new(BufWriter::new(std::fs::File::create(&path)?));
            write_projection_header(&mut output)?;
            output.flush()?;
            return Ok((path, 0, output.identity()));
        }
        let mut stage = 0_usize;
        while self.runs.len() > 1 {
            let mut next = Vec::new();
            for (group, inputs) in self.runs.chunks(self.limits.merge_fan_in).enumerate() {
                let path = self
                    .root
                    .join(format!("entry-merge-{stage:04}-{group:08}.run"));
                next.push(merge_entry_runs(inputs, &path)?);
            }
            for run in &self.runs {
                std::fs::remove_file(&run.path)?;
            }
            self.runs = next;
            stage += 1;
        }
        let run = self.runs.pop().expect("nonempty runs");
        let bytes = std::fs::metadata(&run.path)?.len();
        if bytes < PROJECTION_FILE_HEADER_BYTES
            || (bytes - PROJECTION_FILE_HEADER_BYTES) % PROJECTION_ENTRY_BYTES != 0
        {
            return Err(ArchiveError::Invalid(
                "external title-entry run has partial record",
            ));
        }
        Ok((
            run.path,
            usize::try_from(
                (bytes - PROJECTION_FILE_HEADER_BYTES) / PROJECTION_ENTRY_BYTES,
            )
            .map_err(|_| ArchiveError::FieldTooLarge)?,
            run.identity,
        ))
    }
}

fn merge_fact_runs(inputs: &[PathBuf], output: &Path) -> Result<()> {
    let mut readers = inputs
        .iter()
        .map(|path| std::fs::File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<(Fact, usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(fact) = read_fact(reader)? {
            heap.push(Reverse((fact, run)));
        }
    }
    let mut output = BufWriter::new(std::fs::File::create(output)?);
    while let Some(Reverse((fact, run))) = heap.pop() {
        write_fact(&mut output, &fact)?;
        if let Some(next) = read_fact(&mut readers[run])? {
            heap.push(Reverse((next, run)));
        }
    }
    output.flush()?;
    Ok(())
}

fn merge_assignment_runs(inputs: &[PathBuf], output: &Path) -> Result<()> {
    let mut readers = inputs
        .iter()
        .map(|path| std::fs::File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<(CandidateAssignment, usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(assignment) = read_assignment(reader)? {
            heap.push(Reverse((assignment, run)));
        }
    }
    let mut output = BufWriter::new(std::fs::File::create(output)?);
    let mut previous_key = None;
    while let Some(Reverse((assignment, run))) = heap.pop() {
        if previous_key == Some(assignment.page_and_rank) {
            return Err(ArchiveError::Invalid(
                "duplicate external title candidate rank",
            ));
        }
        previous_key = Some(assignment.page_and_rank);
        write_assignment(&mut output, assignment)?;
        if let Some(next) = read_assignment(&mut readers[run])? {
            heap.push(Reverse((next, run)));
        }
    }
    output.flush()?;
    Ok(())
}

fn merge_entry_runs(inputs: &[EntryRun], output: &Path) -> Result<EntryRun> {
    let mut readers = inputs
        .iter()
        .map(|run| std::fs::File::open(&run.path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    for reader in &mut readers {
        read_projection_header(reader)?;
    }
    let mut heap = BinaryHeap::<Reverse<((u64, u32, u32, i32), usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(entry) = read_title_entry(reader)? {
            heap.push(Reverse((
                (entry.coded_title, entry.time, entry.page_id, entry.namespace),
                run,
            )));
        }
    }
    let mut output_file = DigestWriter::new(BufWriter::new(std::fs::File::create(output)?));
    write_projection_header(&mut output_file)?;
    let mut pending = None::<TitleIndexEntry>;
    while let Some(Reverse(((coded_title, time, page_id, namespace), run))) = heap.pop() {
        let entry = TitleIndexEntry {
            coded_title,
            time,
            page_id,
            namespace,
        };
        if pending.as_ref().is_some_and(|previous| {
            (previous.coded_title, previous.time) != (entry.coded_title, entry.time)
        }) {
            write_title_entry(&mut output_file, pending.take().expect("pending entry"))?;
        }
        // A 63-bit hash collision is deliberately tolerated. If two distinct
        // titles collide at the same second, selecting the larger page ID is
        // deterministic and preserves the index's unique-key invariant.
        pending = Some(entry);
        if let Some(entry) = read_title_entry(&mut readers[run])? {
            heap.push(Reverse((
                (entry.coded_title, entry.time, entry.page_id, entry.namespace),
                run,
            )));
        }
    }
    if let Some(entry) = pending {
        write_title_entry(&mut output_file, entry)?;
    }
    output_file.flush()?;
    Ok(EntryRun {
        path: output.to_path_buf(),
        identity: output_file.identity(),
    })
}

fn write_assignment(output: &mut impl Write, assignment: CandidateAssignment) -> Result<()> {
    output.write_all(&assignment.page_and_rank.to_le_bytes())?;
    output.write_all(&assignment.title_ordinal.to_le_bytes())?;
    output.write_all(&assignment.start.to_le_bytes())?;
    output.write_all(&assignment.page_id.to_le_bytes())?;
    Ok(())
}

fn read_assignment(input: &mut impl Read) -> Result<Option<CandidateAssignment>> {
    let Some(page_and_rank) = read_u64(input)? else {
        return Ok(None);
    };
    Ok(Some(CandidateAssignment {
        page_and_rank,
        title_ordinal: read_u64_required(input)?,
        start: read_i64_required(input)?,
        page_id: read_u64_required(input)?,
    }))
}

struct DigestWriter<W> {
    inner: W,
    digest: sha2::Sha256,
}

impl<W> DigestWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            digest: sha2::Sha256::new(),
        }
    }

    fn identity(&self) -> [u8; 32] {
        self.digest.clone().finalize().into()
    }
}

impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.digest.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = std::fs::File::open(path)?;
    directory.sync_all()?;
    Ok(())
}

fn open_read_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(path)
    }
}

fn digest_file(file: &mut std::fs::File) -> std::io::Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0))?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0_u8; 64 << 10];
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        digest.update(&buffer[..bytes]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(digest.finalize().into())
}

fn publish_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(source, destination) {
        Ok(()) => {
            std::fs::remove_file(source)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_create_new(source, destination)
        }
        Err(error) => Err(error),
    }
}

fn copy_create_new(source: &Path, destination: &Path) -> std::io::Result<()> {
    let mut input = open_read_no_follow(source)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    std::fs::remove_file(source)?;
    Ok(())
}

fn retain_conflicting_source(
    source: &Path,
    destination_directory: &Path,
    identity: &[u8; 32],
) -> std::io::Result<PathBuf> {
    let identity = hex::encode(identity);
    for suffix in 0_u64.. {
        let retained = destination_directory.join(format!(
            "title-projection-{identity}.conflict-{suffix}.entries"
        ));
        match std::fs::hard_link(source, &retained) {
            Ok(()) => {
                std::fs::remove_file(source)?;
                return Ok(retained);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
                match copy_create_new(source, &retained) {
                    Ok(()) => return Ok(retained),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other(
        "exhausted title projection conflict names",
    ))
}

fn coalesce_entry_keys(entries: &mut Vec<TitleIndexEntry>) {
    let mut write = 0;
    for read in 0..entries.len() {
        if write != 0
            && (entries[write - 1].coded_title, entries[write - 1].time)
                == (entries[read].coded_title, entries[read].time)
        {
            entries[write - 1] = entries[read];
        } else {
            entries[write] = entries[read];
            write += 1;
        }
    }
    entries.truncate(write);
}

fn write_fact(output: &mut impl Write, fact: &Fact) -> Result<()> {
    let title = fact.title.as_bytes();
    output.write_all(
        &u32::try_from(title.len())
            .map_err(|_| ArchiveError::FieldTooLarge)?
            .to_le_bytes(),
    )?;
    output.write_all(title)?;
    match fact.payload {
        FactPayload::Candidate {
            page_index,
            rank,
            start,
            page_id,
            namespace,
        } => {
            output.write_all(&[0, rank])?;
            output.write_all(&page_index.to_le_bytes())?;
            output.write_all(&start.to_le_bytes())?;
            output.write_all(&0_i64.to_le_bytes())?;
            output.write_all(&page_id.to_le_bytes())?;
            output.write_all(&namespace.to_le_bytes())?;
        }
        FactPayload::Interval {
            start,
            end,
            page_id,
            namespace,
        } => {
            output.write_all(&[1, 0])?;
            output.write_all(&0_u64.to_le_bytes())?;
            output.write_all(&start.to_le_bytes())?;
            output.write_all(&end.to_le_bytes())?;
            output.write_all(&page_id.to_le_bytes())?;
            output.write_all(&namespace.to_le_bytes())?;
        }
    }
    Ok(())
}

fn read_fact(input: &mut impl Read) -> Result<Option<Fact>> {
    let Some(length) = read_u32(input)? else {
        return Ok(None);
    };
    let mut title = vec![0; usize::try_from(length).map_err(|_| ArchiveError::FieldTooLarge)?];
    input.read_exact(&mut title)?;
    let title = String::from_utf8(title)
        .map_err(|_| ArchiveError::Invalid("external title fact is not UTF-8"))?;
    let mut kind = [0_u8; 2];
    input.read_exact(&mut kind)?;
    let page_index = read_u64_required(input)?;
    let start = read_i64_required(input)?;
    let end = read_i64_required(input)?;
    let page_id = read_u64_required(input)?;
    let namespace = read_i32_required(input)?;
    let payload = match kind[0] {
        0 => FactPayload::Candidate {
            page_index,
            rank: kind[1],
            start,
            page_id,
            namespace,
        },
        1 => FactPayload::Interval {
            start,
            end,
            page_id,
            namespace,
        },
        _ => {
            return Err(ArchiveError::Invalid(
                "external title fact has unknown kind",
            ))
        }
    };
    Ok(Some(Fact { title, payload }))
}

fn write_title_entry(output: &mut impl Write, entry: TitleIndexEntry) -> Result<()> {
    output.write_all(&entry.coded_title.to_le_bytes())?;
    output.write_all(&entry.time.to_le_bytes())?;
    output.write_all(&entry.page_id.to_le_bytes())?;
    output.write_all(&entry.namespace.to_le_bytes())?;
    output.write_all(&[0; 4])?;
    Ok(())
}

fn read_title_entry(input: &mut impl Read) -> Result<Option<TitleIndexEntry>> {
    let Some(coded_title) = read_u64(input)? else {
        return Ok(None);
    };
    let time = read_u32(input)?.ok_or(ArchiveError::Invalid(
        "external title entry is truncated",
    ))?;
    let page_id = read_u32(input)?.ok_or(ArchiveError::Invalid(
        "external title entry is truncated",
    ))?;
    let namespace = read_i32_required(input)?;
    let mut reserved = [0_u8; 4];
    input.read_exact(&mut reserved)?;
    if reserved != [0; 4] {
        return Err(ArchiveError::Invalid(
            "external title entry reserved bytes are nonzero",
        ));
    }
    Ok(Some(TitleIndexEntry {
        coded_title,
        time,
        page_id,
        namespace,
    }))
}

fn write_projection_header(output: &mut impl Write) -> Result<()> {
    output.write_all(&PROJECTION_FILE_MAGIC)?;
    output.write_all(&PROJECTION_FILE_VERSION.to_le_bytes())?;
    output.write_all(&(PROJECTION_ENTRY_BYTES as u32).to_le_bytes())?;
    Ok(())
}

fn read_projection_header(input: &mut impl Read) -> Result<()> {
    let mut header = [0_u8; PROJECTION_FILE_HEADER_BYTES as usize];
    input.read_exact(&mut header)?;
    if header[..8] != PROJECTION_FILE_MAGIC {
        return Err(ArchiveError::Invalid(
            "title projection uses an obsolete format; rebuild required",
        ));
    }
    if u32::from_le_bytes(header[8..12].try_into().expect("projection version"))
        != PROJECTION_FILE_VERSION
        || u32::from_le_bytes(header[12..16].try_into().expect("projection entry bytes"))
            != PROJECTION_ENTRY_BYTES as u32
    {
        return Err(ArchiveError::Invalid(
            "unsupported title projection format; rebuild required",
        ));
    }
    Ok(())
}

fn write_proposal(output: &mut impl Write, page_index: u64, rank: u8) -> Result<()> {
    if rank > 1 {
        return Ok(());
    }
    output.write_all(&page_index.to_le_bytes())?;
    output.write_all(&[rank; 8])?;
    Ok(())
}

fn read_proposal(input: &mut impl Read) -> Result<Option<(u64, u8)>> {
    let Some(page_index) = read_u64(input)? else {
        return Ok(None);
    };
    let mut rank = [0_u8; 8];
    input.read_exact(&mut rank)?;
    Ok(Some((page_index, rank[0])))
}

fn read_u32(input: &mut impl Read) -> Result<Option<u32>> {
    let mut bytes = [0_u8; 4];
    let present = read_optional_exact(input, &mut bytes)?;
    Ok(present.then(|| u32::from_le_bytes(bytes)))
}

fn read_u64(input: &mut impl Read) -> Result<Option<u64>> {
    let mut bytes = [0_u8; 8];
    let present = read_optional_exact(input, &mut bytes)?;
    Ok(present.then(|| u64::from_le_bytes(bytes)))
}

fn read_u64_required(input: &mut impl Read) -> Result<u64> {
    read_u64(input)?.ok_or(ArchiveError::Invalid("external title fact is truncated"))
}

fn read_i64_required(input: &mut impl Read) -> Result<i64> {
    let mut bytes = [0_u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(i64::from_le_bytes(bytes))
}

fn read_i32_required(input: &mut impl Read) -> Result<i32> {
    let mut bytes = [0_u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_optional_exact(input: &mut impl Read, output: &mut [u8]) -> Result<bool> {
    let mut read = 0;
    while read < output.len() {
        let count = input.read(&mut output[read..])?;
        if count == 0 {
            if read == 0 {
                return Ok(false);
            }
            return Err(ArchiveError::Invalid(
                "external title run has a partial record",
            ));
        }
        read += count;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> SiteInfoRecord {
        SiteInfoRecord {
            site_name: String::new(),
            db_name: "testwiki".into(),
            base: String::new(),
            generator: String::new(),
            case: "first-letter".into(),
            language: "en".into(),
            rtl: false,
            server: String::new(),
            script_path: String::new(),
            namespaces: vec![crate::archive::SiteNamespaceRecord {
                id: 0,
                case: "first-letter".into(),
                localized_name: String::new(),
                aliases: Vec::new(),
            }],
            interwiki: Vec::new(),
            magic_words: Vec::new(),
        }
    }

    fn tiny_limits() -> ProjectionLimits {
        ProjectionLimits {
            run_bytes: 1,
            merge_fan_in: 2,
        }
    }

    fn candidate(title: &str, start: i64) -> crate::title_index::TitleCandidate {
        crate::title_index::TitleCandidate {
            title: title.into(),
            namespace: 0,
            start,
        }
    }

    #[test]
    fn empty_projection_has_an_empty_infallible_iterator() {
        let root = tempfile::tempdir().unwrap();
        let entries = ExternalTitleProjectionBuilder::new_in(root.path(), site(), tiny_limits())
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(entries.iter().len(), 0);
    }

    #[test]
    fn losing_current_title_advances_to_the_second_candidate() {
        let root = tempfile::tempdir().unwrap();
        let site = site();
        let mut builder =
            ExternalTitleProjectionBuilder::new_in(root.path(), site.clone(), tiny_limits())
                .unwrap();
        builder
            .emit_projection(crate::title_index::Projection {
                page_id: 10,
                closed: Vec::new(),
                candidates: vec![
                    candidate("Shared", 100_000_000),
                    candidate("Older", 50_000_000),
                ],
            })
            .unwrap();
        builder
            .emit_projection(crate::title_index::Projection {
                page_id: 20,
                closed: Vec::new(),
                candidates: vec![candidate("Shared", 200_000_000)],
            })
            .unwrap();
        let entries = builder.finish().unwrap();
        let actual = entries.iter().collect::<Vec<_>>();
        let mut expected = vec![
            TitleIndexEntry {
                coded_title: crate::title_index::coded_title("Older", &site),
                time: 50,
                page_id: 10,
                namespace: 0,
            },
            TitleIndexEntry {
                coded_title: crate::title_index::coded_title("Shared", &site),
                time: 200,
                page_id: 20,
                namespace: 0,
            },
        ];
        expected.sort_unstable_by_key(|entry| (entry.coded_title, entry.time));
        assert_eq!(actual, expected);
    }

    #[test]
    fn same_second_changes_collapse_to_the_last_ownership_state() {
        let root = tempfile::tempdir().unwrap();
        let site = site();
        let mut builder =
            ExternalTitleProjectionBuilder::new_in(root.path(), site.clone(), tiny_limits())
                .unwrap();
        builder
            .emit_projection(crate::title_index::Projection {
                page_id: 7,
                closed: vec![Interval {
                    title: "Brief".into(),
                    namespace: 0,
                    start: 1_100_000,
                    end: 1_200_000,
                    page_id: 7,
                }],
                candidates: Vec::new(),
            })
            .unwrap();
        let entries = builder.finish().unwrap();
        assert_eq!(
            entries.iter().collect::<Vec<_>>(),
            vec![TitleIndexEntry {
                coded_title: crate::title_index::coded_title("Brief", &site),
                time: 1,
                page_id: 0,
                namespace: 0,
            }]
        );
    }

    #[test]
    fn fact_memory_bytes_counts_string_capacity() {
        let mut title = String::with_capacity(4096);
        title.push('x');
        let fact = Fact {
            title,
            payload: FactPayload::Candidate {
                page_index: 0,
                rank: 0,
                start: 0,
                page_id: 1,
                namespace: 0,
            },
        };
        assert_eq!(fact.memory_bytes(), size_of::<Fact>() + 4096);
    }

    #[test]
    fn same_size_existing_corruption_preserves_both_copies() {
        let root = tempfile::tempdir().unwrap();
        let entries = ExternalTitleProjectionBuilder::new_in(root.path(), site(), tiny_limits())
            .unwrap();
        let entries = {
            let mut entries = entries;
            entries
                .emit_projection(crate::title_index::Projection {
                    page_id: 1,
                    closed: Vec::new(),
                    candidates: vec![candidate("Stable", 1_000_000)],
                })
                .unwrap();
            entries.finish().unwrap()
        };
        let identity = entries.identity_hex();
        let source = entries.path.clone();
        let original = std::fs::read(&source).unwrap();
        let destination = root.path().join(format!("title-projection-{identity}.entries"));
        let mut corrupt = original.clone();
        corrupt[0] ^= 1;
        std::fs::write(&destination, &corrupt).unwrap();
        assert!(entries.persist_content_addressed(root.path()).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), corrupt);
        let retained = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.to_string_lossy().contains(".conflict-"))
            .expect("new copy retained under a distinct name");
        assert_eq!(std::fs::read(retained).unwrap(), original);
    }

    #[test]
    fn open_bound_rejects_same_size_replacement() {
        let root = tempfile::tempdir().unwrap();
        let mut builder = ExternalTitleProjectionBuilder::new_in(root.path(), site(), tiny_limits())
            .unwrap();
        builder
            .emit_projection(crate::title_index::Projection {
                page_id: 1,
                closed: Vec::new(),
                candidates: vec![candidate("Stable", 1_000_000)],
            })
            .unwrap();
        let entries = builder.finish().unwrap().persist_content_addressed(root.path()).unwrap();
        let expected = entries.iter().collect::<Vec<_>>();
        let identity = entries.identity_hex();
        let path = root.path().join(entries.file_name());
        assert_eq!(
            ExternalTitleEntries::open_bound(&path, &identity, expected.len() as u64)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            expected
        );
        let replacement = root.path().join("replacement.entries");
        let mut corrupt = std::fs::read(&path).unwrap();
        corrupt[0] ^= 1;
        std::fs::write(&replacement, corrupt).unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(ExternalTitleEntries::open_bound(&path, &identity, expected.len() as u64).is_err());
    }
}
