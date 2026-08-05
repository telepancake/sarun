//! Scratch-backed, globally correct title-history projection.
//!
//! Records arrive in page-ID order. Only one page is retained while title
//! facts are emitted into bounded external-sort runs. Current-title conflicts
//! are then resolved globally with disk-backed page and owner tables.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use sha2::Digest;

use crate::archive::{ArchiveError, Record, Result, SiteInfoRecord};
use crate::title_index::{Interval, TitleIndexEntry};

const PAGE_BYTES: u64 = 40;
const ASSIGNMENT_BYTES: usize = 32;
const ABSENT: u64 = u64::MAX;

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
    },
    Interval {
        start: i64,
        end: i64,
        page_id: u64,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Fact {
    title: String,
    payload: FactPayload,
}

impl Fact {
    fn memory_bytes(&self) -> usize {
        self.title.len().saturating_add(48)
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
        self.page_ids.write_all(&page_id.to_le_bytes())?;
        let page_index = self.page_count;
        self.page_count = self
            .page_count
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        for interval in projection.closed {
            self.facts.push(Fact {
                title: interval.title,
                payload: FactPayload::Interval {
                    start: interval.start,
                    end: interval.end,
                    page_id,
                },
            })?;
        }
        for (rank, (title, start)) in projection.candidates.into_iter().enumerate() {
            self.facts.push(Fact {
                title,
                payload: FactPayload::Candidate {
                    page_index,
                    rank: rank as u8,
                    start,
                    page_id,
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
            offset: 0,
        }
    }

    /// Open a structurally bound projection without checksumming its payload.
    ///
    /// The writer computed the content identity inline before publication.
    /// Recovery validates the content-addressed name and exact fixed-width
    /// extent from the receipt; it does not turn startup into an O(index
    /// bytes) checksum pass.
    pub(crate) fn open_bound(
        path: impl AsRef<Path>,
        expected_identity: &str,
        expected_entries: u64,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let bytes = std::fs::metadata(&path)?.len();
        let identity_bytes = hex::decode(expected_identity)
            .map_err(|_| ArchiveError::Invalid("invalid title projection identity"))?;
        let identity: [u8; 32] = identity_bytes
            .try_into()
            .map_err(|_| ArchiveError::Invalid("invalid title projection identity"))?;
        let expected_name = format!("title-projection-{expected_identity}.entries");
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
            || bytes % 16 != 0
            || bytes / 16 != expected_entries
        {
            return Err(ArchiveError::Invalid(
                "persisted title projection does not match its structural binding",
            ));
        }
        let entries = if bytes == 0 {
            None
        } else {
            let file = std::fs::File::open(&path)?;
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
    /// The payload is synced before the rename and both affected directories
    /// are synced afterwards. A receipt can therefore safely name the returned
    /// file: interruption before the receipt leaves only an unreferenced,
    /// discardable content-addressed file.
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
        std::fs::File::open(&self.path)?.sync_all()?;
        let source_parent = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        match std::fs::rename(&self.path, &destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::metadata(&destination)?.len()
                    != std::fs::metadata(&self.path)?.len()
                {
                    return Err(ArchiveError::Invalid(
                        "content-addressed title projection has wrong extent",
                    ));
                }
                std::fs::remove_file(&self.path)?;
            }
            Err(error) => return Err(ArchiveError::Io(error)),
        }
        sync_directory(destination_directory)?;
        if source_parent != destination_directory {
            sync_directory(&source_parent)?;
        }
        self.scratch = None;
        let file = std::fs::File::open(&destination)?;
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

    pub(crate) fn entry_count(&self) -> u64 {
        self.entries.as_ref().map_or(0, |entries| entries.len() as u64 / 16)
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
        let bytes = self.bytes.get(self.offset..self.offset.checked_add(16)?)?;
        if bytes.len() != 16 {
            return None;
        }
        self.offset += 16;
        Some(TitleIndexEntry {
            coded_title: u64::from_le_bytes(bytes[..8].try_into().expect("title key")),
            time: u32::from_le_bytes(bytes[8..12].try_into().expect("title time")),
            page_id: u32::from_le_bytes(bytes[12..16].try_into().expect("page ID")),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.bytes.len() - self.offset) / 16;
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
    let mut current_ordinal = 0_u64;
    let mut intervals = Vec::new();
    while let Some(fact) = read_fact(&mut facts_input)? {
        if current_title.as_deref() != Some(fact.title.as_str()) {
            if let Some(title) = current_title.take() {
                finish_title(
                    &title,
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
        if let FactPayload::Interval {
            start,
            end,
            page_id,
        } = fact.payload
        {
            intervals.push(Interval {
                title: fact.title,
                start,
                end,
                page_id,
            });
        }
    }
    if let Some(title) = current_title {
        finish_title(
            &title,
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
    let mut changes = std::collections::BTreeMap::<u32, u32>::new();
    for (_, time, page_id) in crate::title_index::ownership_changes(intervals) {
        // Multiple ownership changes may fall in the same second after the
        // archive's microsecond timestamps are projected to the index. The
        // later change in chronological order is authoritative for that key.
        changes.insert(
            crate::title_index::seconds(time),
            u32::try_from(page_id).map_err(|_| ArchiveError::FieldTooLarge)?,
        );
    }
    for (time, page_id) in changes {
        entries.push(TitleIndexEntry {
            coded_title,
            time,
            page_id,
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
        if self.buffered.len().saturating_mul(16) >= self.limits.run_bytes {
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
            std::fs::File::create(&path)?;
            return Ok((path, 0, sha2::Sha256::digest([]).into()));
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
        if bytes % 16 != 0 {
            return Err(ArchiveError::Invalid(
                "external title-entry run has partial record",
            ));
        }
        Ok((
            run.path,
            usize::try_from(bytes / 16).map_err(|_| ArchiveError::FieldTooLarge)?,
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
    let mut heap = BinaryHeap::<Reverse<((u64, u32, u32), usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(entry) = read_title_entry(reader)? {
            heap.push(Reverse(((entry.coded_title, entry.time, entry.page_id), run)));
        }
    }
    let mut output_file = DigestWriter::new(BufWriter::new(std::fs::File::create(output)?));
    let mut pending = None::<TitleIndexEntry>;
    while let Some(Reverse(((coded_title, time, page_id), run))) = heap.pop() {
        let entry = TitleIndexEntry {
            coded_title,
            time,
            page_id,
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
            heap.push(Reverse(((entry.coded_title, entry.time, entry.page_id), run)));
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
        } => {
            output.write_all(&[0, rank])?;
            output.write_all(&page_index.to_le_bytes())?;
            output.write_all(&start.to_le_bytes())?;
            output.write_all(&0_i64.to_le_bytes())?;
            output.write_all(&page_id.to_le_bytes())?;
        }
        FactPayload::Interval {
            start,
            end,
            page_id,
        } => {
            output.write_all(&[1, 0])?;
            output.write_all(&0_u64.to_le_bytes())?;
            output.write_all(&start.to_le_bytes())?;
            output.write_all(&end.to_le_bytes())?;
            output.write_all(&page_id.to_le_bytes())?;
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
    let payload = match kind[0] {
        0 => FactPayload::Candidate {
            page_index,
            rank: kind[1],
            start,
            page_id,
        },
        1 => FactPayload::Interval {
            start,
            end,
            page_id,
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
    Ok(())
}

fn read_title_entry(input: &mut impl Read) -> Result<Option<TitleIndexEntry>> {
    let Some(coded_title) = read_u64(input)? else {
        return Ok(None);
    };
    Ok(Some(TitleIndexEntry {
        coded_title,
        time: read_u32(input)?.ok_or(ArchiveError::Invalid(
            "external title entry is truncated",
        ))?,
        page_id: read_u32(input)?.ok_or(ArchiveError::Invalid(
            "external title entry is truncated",
        ))?,
    }))
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
                    ("Shared".into(), 100_000_000),
                    ("Older".into(), 50_000_000),
                ],
            })
            .unwrap();
        builder
            .emit_projection(crate::title_index::Projection {
                page_id: 20,
                closed: Vec::new(),
                candidates: vec![("Shared".into(), 200_000_000)],
            })
            .unwrap();
        let entries = builder.finish().unwrap();
        let actual = entries.iter().collect::<Vec<_>>();
        let mut expected = vec![
            TitleIndexEntry {
                coded_title: crate::title_index::coded_title("Older", &site),
                time: 50,
                page_id: 10,
            },
            TitleIndexEntry {
                coded_title: crate::title_index::coded_title("Shared", &site),
                time: 200,
                page_id: 20,
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
            }]
        );
    }
}
