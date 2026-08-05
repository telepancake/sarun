//! Portable, layout-independent Wikipedia event stream.
//!
//! The outer file is a short header followed by independently compressed
//! frames. Frames end only between page ids. Records are ordered by ascending
//! page id and, within a page, descending event time. This is deliberately not
//! a depot format: it is a compact source for experiments, conversions, and
//! recovery without depending on the current live storage layout.

use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom, Take, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::{ContributorMeta, Instance, RevisionMeta};

const FILE_MAGIC: [u8; 8] = *b"SWDUMP\0\0";
const FILE_VERSION: u32 = 1;
const FILE_HEADER_LEN: usize = 24;
const FRAME_MAGIC: [u8; 4] = *b"FRM1";
const DICTIONARY_MAGIC: [u8; 4] = *b"DICT";
const REF_PREFIX_MAGIC: [u8; 4] = *b"PREF";
const DONE_MAGIC: [u8; 4] = *b"DONE";
const FRAME_HEADER_LEN: usize = 64;
const RAW_STREAM_MAGIC: [u8; 8] = *b"SWRAWREC";
const RAW_STREAM_VERSION: u32 = 1;
const RAW_STREAM_HEADER_LEN: usize = 16;
const RAW_STREAM_DONE: [u8; 8] = *b"\0RAWDONE";
pub const DEFAULT_FRAME_TARGET: usize = 4 << 20;
pub const MIRROR_FRAME_TARGET: usize = 128 << 10;
pub const MIRROR_REF_PREFIX_BYTES: usize = 16 << 20;
pub const MIRROR_REF_PREFIX_SAMPLE_BYTES: usize = 150 << 20;
pub const DEFAULT_DICTIONARY_BYTES: usize = 800 << 10;
const DICTIONARY_SAMPLE_COUNT: usize = 32 << 10;

const KIND_PAGE_STATE: u8 = 1;
const KIND_REVISION: u8 = 2;
const KIND_PAGE_ACTION: u8 = 3;
const KIND_USER_STATE: u8 = 4;
const KIND_USER_ACTION: u8 = 5;
const KIND_MANIFEST: u8 = 6;
const KIND_SITE_INFO: u8 = 7;
const PAGE_TEXT_MEMORY_LIMIT: usize = 16 << 20;
const SORT_MERGE_FAN_IN: usize = 64;
const FRAME_READ_AHEAD: usize = 1 << 20;
const FRAME_FEED_RAW_INTERVAL: usize = 1 << 20;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum EntityKind {
    Page = 1,
    User = 2,
    Global = 3,
}

impl TryFrom<u8> for EntityKind {
    type Error = ArchiveError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Page),
            2 => Ok(Self::User),
            3 => Ok(Self::Global),
            _ => Err(ArchiveError::Invalid("unknown entity kind")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EntityKey {
    pub kind: EntityKind,
    pub id: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Mirror(#[from] crate::Error),
    #[error("invalid archive: {0}")]
    Invalid(&'static str),
    #[error(
        "archive records are out of order: previous {previous:?} at {previous_timestamp}, \
         current {current:?} at {current_timestamp}"
    )]
    OutOfOrder {
        previous: EntityKey,
        previous_timestamp: i64,
        current: EntityKey,
        current_timestamp: i64,
    },
    #[error("archive field is too large")]
    FieldTooLarge,
    #[error("invalid stored page-action timestamp {0:?}")]
    InvalidTimestamp(String),
    #[error("archive merge conflict: {0}")]
    Conflict(String),
}

pub type Result<T> = std::result::Result<T, ArchiveError>;

#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct RevisionVisibilityRecord {
    pub deleted_parts: u8,
    pub parts_are_suppressed: bool,
    pub deleted_by_page_deletion: bool,
    pub page_deletion_timestamp_micros: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum AccountClass {
    Unknown = 0,
    Anonymous = 1,
    Temporary = 2,
    Permanent = 3,
    Hidden = 4,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PerformerRecord {
    pub local_user_id: Option<u64>,
    pub central_user_id: Option<u64>,
    pub historical_name: Option<String>,
    pub account_class: AccountClass,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PageActionKind {
    Create,
    LoggedCreate,
    Move,
    Delete,
    Restore,
    Merge,
    Other(String),
}

impl PageActionKind {
    pub fn from_name(name: &str) -> Self {
        match name {
            "create" => Self::Create,
            "create-page" => Self::LoggedCreate,
            "move" => Self::Move,
            "delete" => Self::Delete,
            "restore" => Self::Restore,
            "merge" => Self::Merge,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageActionRecord {
    pub log_id: Option<u64>,
    pub tie_sequence: u64,
    pub kind: PageActionKind,
    pub performer: PerformerRecord,
    pub comment: String,
    pub title_at_event: String,
    pub namespace_at_event: Option<i64>,
    pub resulting_deleted: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionRecord {
    pub meta: RevisionMeta,
    pub has_text: bool,
    pub text: Vec<u8>,
    pub visibility: Option<RevisionVisibilityRecord>,
    pub history: Option<RevisionHistoryRecord>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RevisionHistoryRecord {
    pub minor: Option<bool>,
    pub content_model: Option<String>,
    pub content_format: Option<String>,
    pub identity_reverted: Option<bool>,
    pub first_reverting_revision_id: Option<u64>,
    pub seconds_to_revert: Option<u64>,
    pub identity_revert: Option<bool>,
    pub before_page_creation: Option<bool>,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UserStateRecord {
    pub current_name: Option<String>,
    pub central_user_id: Option<u64>,
    pub account_class: AccountClass,
    pub groups: Vec<String>,
    pub blocks: Vec<String>,
    pub bot_by: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum UserActionKind {
    Create,
    Rename,
    GroupsChanged,
    BlocksChanged,
    Other(String),
}

impl UserActionKind {
    pub fn from_name(name: &str) -> Self {
        match name {
            "create" => Self::Create,
            "rename" => Self::Rename,
            "altergroups" => Self::GroupsChanged,
            "alterblocks" => Self::BlocksChanged,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UserActionRecord {
    pub log_id: Option<u64>,
    pub tie_sequence: u64,
    pub kind: UserActionKind,
    pub performer: PerformerRecord,
    pub comment: String,
    pub historical_name: Option<String>,
    pub groups: Vec<String>,
    pub blocks: Vec<String>,
    pub bot_by: Vec<String>,
    pub created_by: u8,
    pub registration_timestamp_micros: Option<i64>,
    pub creation_timestamp_micros: Option<i64>,
    pub first_edit_timestamp_micros: Option<i64>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ManifestRecord {
    pub wiki_db: String,
    pub content_snapshot: String,
    pub metadata_snapshot: String,
    pub source_files: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SiteNamespaceRecord {
    pub id: i32,
    pub case: String,
    pub localized_name: String,
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SiteInterwikiRecord {
    pub prefix: String,
    pub url: String,
    pub is_local: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SiteMagicWordRecord {
    pub canonical_name: String,
    pub aliases: Vec<String>,
    pub case_sensitive: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SiteInfoRecord {
    pub site_name: String,
    pub db_name: String,
    pub base: String,
    pub generator: String,
    pub case: String,
    pub language: String,
    pub rtl: bool,
    pub server: String,
    pub script_path: String,
    pub namespaces: Vec<SiteNamespaceRecord>,
    pub interwiki: Vec<SiteInterwikiRecord>,
    pub magic_words: Vec<SiteMagicWordRecord>,
}

struct PendingRecord {
    record: Record,
}

/// In-memory sorter for typed archive records.
///
/// Normal imports retain records until the final sort and let the operating
/// system page cold payloads. Tests and specialized callers may explicitly
/// call the private run-flush path when they need an external merge.
pub(crate) struct RecordSorter {
    temporary: tempfile::TempDir,
    buffered: Vec<PendingRecord>,
    buffered_wire_bytes: usize,
    run_target: Option<usize>,
    runs: Vec<std::path::PathBuf>,
}

impl RecordSorter {
    pub(crate) fn new_in(root: &Path) -> Result<Self> {
        Ok(Self {
            temporary: tempfile::TempDir::new_in(root)?,
            buffered: Vec::new(),
            buffered_wire_bytes: 0,
            run_target: None,
            runs: Vec::new(),
        })
    }

    pub(crate) fn new_with_run_target(root: &Path, run_target: usize) -> Result<Self> {
        if run_target == 0 {
            return Err(ArchiveError::Invalid("zero sort run target"));
        }
        let mut sorter = Self::new_in(root)?;
        sorter.run_target = Some(run_target);
        Ok(sorter)
    }

    pub(crate) fn push(&mut self, record: Record) -> Result<()> {
        let entity = record.entity();
        let (_, payload_len) = record_wire_size(&record)?;
        let wire_bytes = 1_u64
            .checked_add(varint_len(entity.id) as u64)
            .and_then(|bytes| bytes.checked_add(8 + 1))
            .and_then(|bytes| bytes.checked_add(varint_len(payload_len) as u64))
            .and_then(|bytes| bytes.checked_add(payload_len))
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.buffered_wire_bytes = self
            .buffered_wire_bytes
            .checked_add(
                usize::try_from(wire_bytes).map_err(|_| ArchiveError::FieldTooLarge)?,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.buffered.push(PendingRecord { record });
        if self
            .run_target
            .is_some_and(|target| self.buffered_wire_bytes >= target)
        {
            self.flush_run()?;
        }
        Ok(())
    }

    fn flush_run(&mut self) -> Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }
        self.buffered
            .sort_by(|left, right| record_order(&left.record, &right.record));
        let path = self
            .temporary
            .path()
            .join(format!("run-{:08}.zst", self.runs.len()));
        let file = std::fs::File::create(&path)?;
        let mut encoder = zstd::stream::write::Encoder::new(file, 1)?;
        for pending in self.buffered.drain(..) {
            write_sort_run_record(&mut encoder, &pending.record)?;
        }
        self.buffered_wire_bytes = 0;
        encoder.finish()?.sync_all()?;
        self.runs.push(path);
        Ok(())
    }

    pub(crate) fn finish<W: Write>(
        self,
        output: W,
        frame_target: usize,
    ) -> Result<(W, u64, u64)> {
        let (output, frames, _, user_actions) = self.finish_with_compression(
            output,
            frame_target,
            CompressionSettings::default(),
        )?;
        Ok((output, frames, user_actions))
    }

    fn finish_with_compression<W: Write>(
        mut self,
        output: W,
        frame_target: usize,
        compression: CompressionSettings,
    ) -> Result<(W, u64, u64, u64)> {
        if self.runs.is_empty() {
            self.buffered
                .sort_by(|left, right| record_order(&left.record, &right.record));
            let mut writer = ArchiveWriter::with_compression(output, frame_target, compression)?;
            let mut records = 0_u64;
            let mut user_actions = 0_u64;
            let mut current = None;
            for pending in self.buffered {
                let record = pending.record;
                if let Some(previous) = current.take() {
                    if records_coalesce(&previous, &record) {
                        current = Some(coalesce_records(previous, record)?);
                    } else {
                        user_actions += u64::from(matches!(
                            &previous,
                            Record::UserAction { .. }
                        ));
                        writer.write(&previous)?;
                        records += 1;
                        current = Some(record);
                    }
                } else {
                    current = Some(record);
                }
            }
            if let Some(record) = current {
                user_actions += u64::from(matches!(&record, Record::UserAction { .. }));
                writer.write(&record)?;
                records += 1;
            }
            let (output, frames) = writer.finish()?;
            return Ok((output, frames, records, user_actions));
        }
        self.flush_run()?;
        self.collapse_runs()?;
        let mut readers = self
            .runs
            .iter()
            .map(|path| SortRunReader::open(path))
            .collect::<Result<Vec<_>>>()?;
        let mut heads = BinaryHeap::new();
        for (run, reader) in readers.iter_mut().enumerate() {
            if let Some(record) = reader.next_record()? {
                heads.push(SortRunHead { run, record });
            }
        }
        let mut writer = ArchiveWriter::with_compression(output, frame_target, compression)?;
        let mut records = 0_u64;
        let mut user_actions = 0_u64;
        while let Some(head) = heads.pop() {
            let mut record = head.record;
            if let Some(next) = readers[head.run].next_record()? {
                heads.push(SortRunHead {
                    run: head.run,
                    record: next,
                });
            }
            while heads
                .peek()
                .is_some_and(|other| records_coalesce(&record, &other.record))
            {
                let other = heads.pop().expect("peeked");
                record = coalesce_records(record, other.record)?;
                if let Some(next) = readers[other.run].next_record()? {
                    heads.push(SortRunHead {
                        run: other.run,
                        record: next,
                    });
                }
            }
            user_actions += u64::from(matches!(record, Record::UserAction { .. }));
            writer.write(&record)?;
            records += 1;
        }
        let (output, frames) = writer.finish()?;
        Ok((output, frames, records, user_actions))
    }

    fn collapse_runs(&mut self) -> Result<()> {
        let mut pass = 0_usize;
        while self.runs.len() > SORT_MERGE_FAN_IN {
            let previous = std::mem::take(&mut self.runs);
            let mut next = Vec::with_capacity(previous.len().div_ceil(SORT_MERGE_FAN_IN));
            for (batch, paths) in previous.chunks(SORT_MERGE_FAN_IN).enumerate() {
                if paths.len() == 1 {
                    next.push(paths[0].clone());
                    continue;
                }
                let output = self
                    .temporary
                    .path()
                    .join(format!("merge-{pass:04}-{batch:08}.zst"));
                merge_sort_runs(paths, &output)?;
                for path in paths {
                    std::fs::remove_file(path)?;
                }
                next.push(output);
            }
            self.runs = next;
            pass += 1;
        }
        Ok(())
    }
}

fn merge_sort_runs(inputs: &[PathBuf], output: &Path) -> Result<()> {
    let mut readers = inputs
        .iter()
        .map(|path| SortRunReader::open(path))
        .collect::<Result<Vec<_>>>()?;
    let mut heads = BinaryHeap::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = reader.next_record()? {
            heads.push(SortRunHead { run, record });
        }
    }
    let file = std::fs::File::create(output)?;
    let mut encoder = zstd::stream::write::Encoder::new(file, 1)?;
    while let Some(head) = heads.pop() {
        let mut record = head.record;
        if let Some(next) = readers[head.run].next_record()? {
            heads.push(SortRunHead {
                run: head.run,
                record: next,
            });
        }
        while heads
            .peek()
            .is_some_and(|other| records_coalesce(&record, &other.record))
        {
            let other = heads.pop().expect("peeked");
            record = coalesce_records(record, other.record)?;
            if let Some(next) = readers[other.run].next_record()? {
                heads.push(SortRunHead {
                    run: other.run,
                    record: next,
                });
            }
        }
        write_sort_run_record(&mut encoder, &record)?;
    }
    encoder.finish()?.sync_all()?;
    Ok(())
}

fn write_sort_run_record(output: &mut impl Write, record: &Record) -> Result<()> {
    let entity = record.entity();
    output.write_all(&[entity.kind as u8])?;
    output.write_all(&entity.id.to_le_bytes())?;
    output.write_all(&record.timestamp_micros().to_le_bytes())?;
    let (kind, payload_len) = record_wire_size(record)?;
    output.write_all(&[kind])?;
    output.write_all(&payload_len.to_le_bytes())?;
    write_record_payload(output, record)
}

struct SortRunReader {
    decoder: zstd::stream::read::Decoder<'static, BufReader<std::fs::File>>,
}

impl SortRunReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            decoder: zstd::stream::read::Decoder::new(std::fs::File::open(path)?)?,
        })
    }

    fn next_record(&mut self) -> Result<Option<Record>> {
        let mut kind = [0_u8; 1];
        if self.decoder.read(&mut kind)? == 0 {
            return Ok(None);
        }
        let entity_kind = EntityKind::try_from(kind[0])?;
        let id = read_u64_from(&mut self.decoder)?;
        let timestamp_micros = read_i64(&mut self.decoder)?;
        let kind = read_u8(&mut self.decoder)?;
        let payload_len = read_u64_from(&mut self.decoder)?;
        let payload_len = usize::try_from(payload_len).map_err(|_| ArchiveError::FieldTooLarge)?;
        let mut payload = vec![0_u8; payload_len];
        self.decoder.read_exact(&mut payload)?;
        decode_record(
            EntityKey {
                kind: entity_kind,
                id,
            },
            timestamp_micros,
            kind,
            payload,
        )
        .map(Some)
    }
}

struct SortRunHead {
    run: usize,
    record: Record,
}

impl PartialEq for SortRunHead {
    fn eq(&self, other: &Self) -> bool {
        record_order(&self.record, &other.record) == std::cmp::Ordering::Equal
            && self.run == other.run
    }
}

impl Eq for SortRunHead {}

impl PartialOrd for SortRunHead {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SortRunHead {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        record_order(&other.record, &self.record).then_with(|| other.run.cmp(&self.run))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Record {
    PageState {
        page_id: u64,
        timestamp_micros: i64,
        title: String,
        namespace: Option<i64>,
        deleted: bool,
    },
    Revision {
        page_id: u64,
        revision: RevisionRecord,
    },
    PageAction {
        entity: EntityKey,
        timestamp_micros: i64,
        action: PageActionRecord,
    },
    UserState {
        user_id: u64,
        timestamp_micros: i64,
        state: UserStateRecord,
    },
    UserAction {
        entity: EntityKey,
        timestamp_micros: i64,
        action: UserActionRecord,
    },
    Manifest {
        timestamp_micros: i64,
        manifest: ManifestRecord,
    },
    SiteInfo {
        timestamp_micros: i64,
        site_info: SiteInfoRecord,
    },
    Unknown {
        entity: EntityKey,
        timestamp_micros: i64,
        kind: u8,
        payload: Vec<u8>,
    },
}

impl Record {
    pub fn entity(&self) -> EntityKey {
        match self {
            Self::PageState { page_id, .. }
            | Self::Revision { page_id, .. } => EntityKey {
                kind: EntityKind::Page,
                id: *page_id,
            },
            Self::PageAction { entity, .. } | Self::UserAction { entity, .. } => *entity,
            Self::UserState { user_id, .. } => EntityKey {
                kind: EntityKind::User,
                id: *user_id,
            },
            Self::Manifest { .. } => EntityKey {
                kind: EntityKind::Global,
                id: 0,
            },
            Self::SiteInfo { .. } => EntityKey {
                kind: EntityKind::Global,
                id: 1,
            },
            Self::Unknown { entity, .. } => *entity,
        }
    }

    pub fn page_id(&self) -> Option<u64> {
        (self.entity().kind == EntityKind::Page).then_some(self.entity().id)
    }

    pub fn timestamp_micros(&self) -> i64 {
        match self {
            Self::PageState {
                timestamp_micros, ..
            }
            | Self::PageAction {
                timestamp_micros, ..
            }
            | Self::UserState {
                timestamp_micros, ..
            }
            | Self::UserAction {
                timestamp_micros, ..
            }
            | Self::Manifest {
                timestamp_micros, ..
            }
            | Self::SiteInfo {
                timestamp_micros, ..
            }
            | Self::Unknown {
                timestamp_micros, ..
            } => *timestamp_micros,
            Self::Revision { revision, .. } => revision.meta.ts.timestamp_micros(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportStats {
    pub pages: u64,
    pub revisions: u64,
    pub page_actions: u64,
    pub user_actions: u64,
    pub frames: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DepotImportStats {
    pub pages: u64,
    pub revisions: u64,
    pub page_actions: u64,
    pub user_records: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameInfo {
    pub first_entity: EntityKey,
    pub last_entity: EntityKey,
    pub records: u64,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    pub dictionary_id: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameLocation {
    pub info: FrameInfo,
    pub compressed_offset: u64,
    reference: Option<CompressionReference>,
    physical_segment: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CompressionReference {
    Dictionary(std::sync::Arc<[u8]>),
    RefPrefix(std::sync::Arc<[u8]>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompressionSettings {
    pub level: i32,
    pub checksum: bool,
    pub long_distance_matching: bool,
    pub window_log: Option<u32>,
    pub target_block_size: Option<u32>,
    pub workers: u32,
}

impl Default for CompressionSettings {
    fn default() -> Self {
        Self {
            level: 3,
            checksum: false,
            long_distance_matching: false,
            window_log: None,
            target_block_size: None,
            workers: 0,
        }
    }
}

struct FrameBuilder<'a> {
    encoder: zstd::stream::write::Encoder<'a, Vec<u8>>,
    _prepared_reference: Option<
        std::sync::Arc<zstd::dict::EncoderDictionary<'static>>,
    >,
    first_entity: EntityKey,
    last_entity: EntityKey,
    records: u64,
    raw_bytes: u64,
}

impl<'a> FrameBuilder<'a> {
    fn new(
        entity: EntityKey,
        settings: CompressionSettings,
        prepared_reference: Option<
            &std::sync::Arc<zstd::dict::EncoderDictionary<'static>>,
        >,
        reference_window_log: Option<u32>,
    ) -> Result<Self> {
        let held_reference = prepared_reference.cloned();
        let encoder = match held_reference.as_ref() {
            Some(dictionary) => {
                zstd::stream::write::Encoder::with_prepared_dictionary(
                    Vec::new(),
                    dictionary,
                )?
            }
            None => zstd::stream::write::Encoder::new(Vec::new(), settings.level)?,
        };
        let mut encoder = encoder;
        encoder.include_checksum(settings.checksum)?;
        encoder.long_distance_matching(settings.long_distance_matching)?;
        if settings.workers != 0 {
            encoder.multithread(settings.workers)?;
        }
        let window_log = settings.window_log.or(reference_window_log);
        if let Some(window_log) = window_log {
            encoder.window_log(window_log)?;
        }
        encoder.set_target_cblock_size(settings.target_block_size)?;
        Ok(Self {
            encoder,
            _prepared_reference: held_reference,
            first_entity: entity,
            last_entity: entity,
            records: 0,
            raw_bytes: 0,
        })
    }

    fn compressed_so_far(&self) -> usize {
        self.encoder.get_ref().len()
    }
}

fn configure_streaming_context(
    context: &mut zstd::zstd_safe::CCtx<'static>,
    settings: CompressionSettings,
    dictionary: &zstd::dict::EncoderDictionary<'static>,
    prefix_bytes: usize,
) -> Result<()> {
    use zstd::zstd_safe::CParameter;
    context
        .set_parameter(CParameter::ChecksumFlag(settings.checksum))
        .map_err(zstd_error)?;
    context
        .set_parameter(CParameter::EnableLongDistanceMatching(
            settings.long_distance_matching,
        ))
        .map_err(zstd_error)?;
    context
        .set_parameter(CParameter::WindowLog(
            settings
                .window_log
                .unwrap_or_else(|| ref_prefix_window_log(prefix_bytes)),
        ))
        .map_err(zstd_error)?;
    if let Some(bytes) = settings.target_block_size {
        context
            .set_parameter(CParameter::TargetCBlockSize(bytes))
            .map_err(zstd_error)?;
    }
    context
        .ref_cdict(dictionary.as_cdict())
        .map_err(zstd_error)?;
    Ok(())
}

fn zstd_error(code: usize) -> ArchiveError {
    ArchiveError::Io(io::Error::other(
        zstd::zstd_safe::get_error_name(code).to_owned(),
    ))
}

struct StreamingPump {
    written: u64,
    remaining: u64,
}

fn pump_streaming_context(
    context: &mut zstd::zstd_safe::CCtx<'static>,
    raw: &[u8],
    directive: zstd::zstd_safe::zstd_sys::ZSTD_EndDirective,
    compressed: &mut impl Write,
    scratch: &mut [u8],
) -> Result<StreamingPump> {
    let mut input = zstd::zstd_safe::InBuffer::around(raw);
    let mut total = 0_u64;
    loop {
        let (remaining, written) = {
            let mut output =
                zstd::zstd_safe::OutBuffer::around(scratch);
            let remaining = context
                .compress_stream2(
                    &mut output,
                    &mut input,
                    directive,
                )
                .map_err(zstd_error)?;
            (remaining, output.pos())
        };
        compressed.write_all(&scratch[..written])?;
        total = total
            .checked_add(written as u64)
            .ok_or(ArchiveError::FieldTooLarge)?;
        let input_consumed = input.pos() == raw.len();
        if input_consumed
            && (matches!(
                directive,
                zstd::zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_continue
            ) || remaining == 0)
        {
            return Ok(StreamingPump {
                written: total,
                remaining: remaining
                    .try_into()
                    .map_err(|_| ArchiveError::FieldTooLarge)?,
            });
        }
    }
}

pub(crate) struct StreamingArchiveWriter<W: Write> {
    output: W,
    frame_target: u64,
    context: zstd::zstd_safe::CCtx<'static>,
    _reference: zstd::dict::EncoderDictionary<'static>,
    compressed: Vec<u8>,
    compressed_bytes: u64,
    pending_compressed_bytes: u64,
    feed_interval: usize,
    pending: Vec<u8>,
    scratch: Vec<u8>,
    first_entity: Option<EntityKey>,
    last_entity: Option<EntityKey>,
    output_frontier: Option<EntityKey>,
    last_timestamp: i64,
    records: u64,
    raw_bytes: u64,
    frames_written: u64,
    sealed_raw_bytes: u64,
    sealed_compressed_bytes: u64,
}

impl<W: Write> StreamingArchiveWriter<W> {
    pub(crate) fn new(
        mut output: W,
        frame_target: usize,
        mut compression: CompressionSettings,
        prefix: &[u8],
        workers: usize,
    ) -> Result<Self> {
        if frame_target == 0 || prefix.is_empty() || workers == 0 {
            return Err(ArchiveError::Invalid(
                "invalid streaming archive writer configuration",
            ));
        }
        compression.workers = 0;
        output.write_all(&FILE_MAGIC)?;
        output.write_all(&FILE_VERSION.to_le_bytes())?;
        output.write_all(&0_u32.to_le_bytes())?;
        output.write_all(&(frame_target as u64).to_le_bytes())?;
        write_ref_prefix_frame(&mut output, prefix, compression)?;
        let reference = zstd::dict::EncoderDictionary::copy(prefix, compression.level);
        let mut context = zstd::zstd_safe::CCtx::create();
        configure_streaming_context(&mut context, compression, &reference, prefix.len())?;
        context
            .set_parameter(zstd::zstd_safe::CParameter::NbWorkers(
                workers.try_into().unwrap_or(u32::MAX),
            ))
            .map_err(zstd_error)?;
        context
            .set_parameter(zstd::zstd_safe::CParameter::JobSize(
                FRAME_FEED_RAW_INTERVAL as u32,
            ))
            .map_err(zstd_error)?;
        Ok(Self {
            output,
            frame_target: frame_target as u64,
            context,
            _reference: reference,
            compressed: Vec::new(),
            compressed_bytes: 0,
            pending_compressed_bytes: 0,
            feed_interval: FRAME_FEED_RAW_INTERVAL
                .min(frame_target.saturating_mul(8))
                .max(1),
            pending: Vec::with_capacity(
                FRAME_FEED_RAW_INTERVAL
                    .min(frame_target.saturating_mul(8))
                    .max(1),
            ),
            scratch: vec![0_u8; zstd::zstd_safe::CCtx::out_size()],
            first_entity: None,
            last_entity: None,
            output_frontier: None,
            last_timestamp: i64::MAX,
            records: 0,
            raw_bytes: 0,
            frames_written: 0,
            sealed_raw_bytes: 0,
            sealed_compressed_bytes: 0,
        })
    }

    fn estimated_compressed_bytes(&self) -> u64 {
        let progression = self.context.get_frame_progression();
        let produced = progression
            .produced
            .max(
                self.compressed_bytes
                    .saturating_add(self.pending_compressed_bytes),
            );
        let uncompressed = self.raw_bytes.saturating_sub(progression.consumed);
        let (ratio_numerator, ratio_denominator) =
            if progression.consumed >= 64 << 10 && progression.produced != 0 {
                (progression.produced, progression.consumed)
            } else if self.sealed_raw_bytes != 0 {
                (self.sealed_compressed_bytes, self.sealed_raw_bytes)
            } else {
                (1, 4)
            };
        let estimated_pending = (u128::from(uncompressed)
            .saturating_mul(u128::from(ratio_numerator))
            .saturating_add(u128::from(ratio_denominator - 1))
            / u128::from(ratio_denominator))
        .min(u128::from(u64::MAX)) as u64;
        produced.saturating_add(estimated_pending)
    }

    pub(crate) fn write(&mut self, record: &Record) -> Result<()> {
        let entity = record.entity();
        let timestamp = record.timestamp_micros();
        let new_entity = self.last_entity != Some(entity);
        if let Some(previous) = self.output_frontier {
            if entity <= previous {
                return Err(ArchiveError::OutOfOrder {
                    previous,
                    previous_timestamp: i64::MIN,
                    current: entity,
                    current_timestamp: timestamp,
                });
            }
        }
        if let Some(previous) = self.last_entity {
            if entity < previous || (!new_entity && timestamp > self.last_timestamp) {
                return Err(ArchiveError::OutOfOrder {
                    previous,
                    previous_timestamp: self.last_timestamp,
                    current: entity,
                    current_timestamp: timestamp,
                });
            }
            if new_entity
                && (entity.kind != previous.kind
                    || self.estimated_compressed_bytes() >= self.frame_target)
            {
                self.seal_frame()?;
            }
        }
        if self.first_entity.is_none() {
            self.first_entity = Some(entity);
        }
        {
            let mut sink = StreamingRecordSink { writer: self };
            write_record_wire(&mut sink, record)?;
        }
        self.records = self
            .records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.last_entity = Some(entity);
        self.last_timestamp = timestamp;
        Ok(())
    }

    fn feed_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pumped = pump_streaming_context(
            &mut self.context,
            &self.pending,
            zstd::zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_continue,
            &mut self.compressed,
            &mut self.scratch,
        )?;
        self.compressed_bytes = self
            .compressed_bytes
            .checked_add(pumped.written)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.pending_compressed_bytes = pumped.remaining;
        self.pending.clear();
        Ok(())
    }

    fn seal_frame(&mut self) -> Result<()> {
        let Some(first_entity) = self.first_entity else {
            return Ok(());
        };
        let last_entity = self
            .last_entity
            .ok_or(ArchiveError::Invalid("streaming frame has no last entity"))?;
        let pumped = pump_streaming_context(
            &mut self.context,
            &self.pending,
            zstd::zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_end,
            &mut self.compressed,
            &mut self.scratch,
        )?;
        self.compressed_bytes = self
            .compressed_bytes
            .checked_add(pumped.written)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.pending_compressed_bytes = 0;
        self.pending.clear();
        write_frame_header(
            &mut self.output,
            FrameInfo {
                first_entity,
                last_entity,
                records: self.records,
                raw_bytes: self.raw_bytes,
                compressed_bytes: self.compressed_bytes,
                dictionary_id: None,
            },
        )?;
        self.output.write_all(&self.compressed)?;
        self.frames_written = self
            .frames_written
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.sealed_raw_bytes = self
            .sealed_raw_bytes
            .checked_add(self.raw_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.sealed_compressed_bytes = self
            .sealed_compressed_bytes
            .checked_add(self.compressed_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.output_frontier = Some(last_entity);
        self.compressed.clear();
        self.context
            .reset(zstd::zstd_safe::ResetDirective::SessionOnly)
            .map_err(zstd_error)?;
        self.compressed_bytes = 0;
        self.pending_compressed_bytes = 0;
        self.first_entity = None;
        self.last_entity = None;
        self.last_timestamp = i64::MAX;
        self.records = 0;
        self.raw_bytes = 0;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<(W, u64)> {
        self.seal_frame()?;
        self.output.write_all(&DONE_MAGIC)?;
        self.output.write_all(&[0; FRAME_HEADER_LEN - 4])?;
        self.output.flush()?;
        Ok((self.output, self.frames_written))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CopiedFrameStats {
    pub(crate) frames: u64,
    pub(crate) records: u64,
    pub(crate) raw_bytes: u64,
    pub(crate) compressed_bytes: u64,
}

struct ParallelFrameJob {
    sequence: u64,
    first_entity: EntityKey,
    last_entity: EntityKey,
    records: u64,
    raw: Vec<u8>,
}

struct ParallelFrameResult {
    sequence: u64,
    info: FrameInfo,
    payload: ParallelFramePayload,
}

enum ParallelFramePayload {
    Compressed(Vec<u8>),
    Copied {
        source: std::fs::File,
        payload_offset: u64,
    },
}

/// An ordered, bounded pool for independently compressed final frames.
///
/// Zstd's internal worker pool cannot usefully parallelize frames whose raw
/// size is normally below one zstd job. This writer instead assigns complete
/// frames to independent single-threaded contexts. Only the output thread
/// writes bytes, in sequence order. At most `workers` submitted raw frames and
/// one result per worker are retained, plus an indivisible current entity.
pub(crate) struct ParallelArchiveWriter<W: Write> {
    output: Option<W>,
    frame_target: u64,
    senders: Vec<std::sync::mpsc::SyncSender<Option<ParallelFrameJob>>>,
    results: std::sync::mpsc::Receiver<Result<ParallelFrameResult>>,
    threads: Vec<std::thread::JoinHandle<()>>,
    pending_results: BTreeMap<u64, ParallelFrameResult>,
    current_raw: Vec<u8>,
    current_first: Option<EntityKey>,
    current_last: Option<EntityKey>,
    submitted_frontier: Option<EntityKey>,
    current_last_timestamp: i64,
    current_records: u64,
    next_submit: u64,
    next_write: u64,
    outstanding: usize,
    next_worker: usize,
    frames_written: u64,
    sealed_raw_bytes: u64,
    sealed_compressed_bytes: u64,
}

impl<W: Write> ParallelArchiveWriter<W> {
    pub(crate) fn new(
        mut output: W,
        frame_target: usize,
        mut compression: CompressionSettings,
        prefix: &[u8],
        workers: usize,
    ) -> Result<Self> {
        if frame_target == 0 || prefix.is_empty() || workers == 0 {
            return Err(ArchiveError::Invalid(
                "invalid parallel archive writer configuration",
            ));
        }
        compression.workers = 0;
        output.write_all(&FILE_MAGIC)?;
        output.write_all(&FILE_VERSION.to_le_bytes())?;
        output.write_all(&0_u32.to_le_bytes())?;
        output.write_all(&(frame_target as u64).to_le_bytes())?;
        write_ref_prefix_frame(&mut output, prefix, compression)?;

        let prefix: Arc<[u8]> = Arc::from(prefix);
        let (result_sender, results) = std::sync::mpsc::channel();
        let mut senders = Vec::with_capacity(workers);
        let mut threads = Vec::with_capacity(workers);
        for _ in 0..workers {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let results = result_sender.clone();
            let prefix = Arc::clone(&prefix);
            threads.push(std::thread::spawn(move || {
                parallel_frame_worker(receiver, results, prefix, compression);
            }));
            senders.push(sender);
        }
        drop(result_sender);
        Ok(Self {
            output: Some(output),
            frame_target: frame_target as u64,
            senders,
            results,
            threads,
            pending_results: BTreeMap::new(),
            current_raw: Vec::with_capacity(frame_target.saturating_mul(4)),
            current_first: None,
            current_last: None,
            submitted_frontier: None,
            current_last_timestamp: i64::MAX,
            current_records: 0,
            next_submit: 0,
            next_write: 0,
            outstanding: 0,
            next_worker: 0,
            frames_written: 0,
            sealed_raw_bytes: 0,
            sealed_compressed_bytes: 0,
        })
    }

    fn estimated_current_compressed_bytes(&self) -> u64 {
        if self.sealed_raw_bytes == 0 || self.sealed_compressed_bytes == 0 {
            return (self.current_raw.len() as u64).div_ceil(4);
        }
        (u128::from(self.current_raw.len() as u64)
            .saturating_mul(u128::from(self.sealed_compressed_bytes))
            .div_ceil(u128::from(self.sealed_raw_bytes)))
        .min(u128::from(u64::MAX)) as u64
    }

    pub(crate) fn write(&mut self, record: &Record) -> Result<()> {
        let entity = record.entity();
        let timestamp = record.timestamp_micros();
        let new_entity = self.current_last != Some(entity);
        if let Some(previous) = self.submitted_frontier {
            if entity <= previous {
                return Err(ArchiveError::OutOfOrder {
                    previous,
                    previous_timestamp: i64::MIN,
                    current: entity,
                    current_timestamp: timestamp,
                });
            }
        }
        if let Some(previous) = self.current_last {
            if entity < previous || (!new_entity && timestamp > self.current_last_timestamp) {
                return Err(ArchiveError::OutOfOrder {
                    previous,
                    previous_timestamp: self.current_last_timestamp,
                    current: entity,
                    current_timestamp: timestamp,
                });
            }
            if new_entity
                && (entity.kind != previous.kind
                    || self.estimated_current_compressed_bytes() >= self.frame_target)
            {
                self.submit_current()?;
            }
        }
        if self.current_first.is_none() {
            self.current_first = Some(entity);
        }
        write_record_wire(&mut self.current_raw, record)?;
        self.current_records = self
            .current_records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.current_last = Some(entity);
        self.current_last_timestamp = timestamp;
        Ok(())
    }

    fn submit_current(&mut self) -> Result<()> {
        let Some(first_entity) = self.current_first.take() else {
            return Ok(());
        };
        while self
            .outstanding
            .saturating_add(self.pending_results.len())
            >= self.senders.len()
        {
            self.collect_one()?;
        }
        let last_entity = self
            .current_last
            .take()
            .ok_or(ArchiveError::Invalid("parallel frame has no last entity"))?;
        let job = ParallelFrameJob {
            sequence: self.next_submit,
            first_entity,
            last_entity,
            records: self.current_records,
            raw: std::mem::replace(
                &mut self.current_raw,
                Vec::with_capacity(self.frame_target.saturating_mul(4) as usize),
            ),
        };
        self.submitted_frontier = Some(last_entity);
        let worker = self.next_worker;
        self.senders[worker]
            .send(Some(job))
            .map_err(|_| ArchiveError::Invalid("parallel compressor stopped"))?;
        self.next_worker = (worker + 1) % self.senders.len();
        self.next_submit = self
            .next_submit
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.outstanding += 1;
        self.current_last_timestamp = i64::MAX;
        self.current_records = 0;
        Ok(())
    }

    fn collect_one(&mut self) -> Result<()> {
        let result = self
            .results
            .recv()
            .map_err(|_| ArchiveError::Invalid("parallel compressor stopped"))??;
        if self.outstanding == 0 {
            return Err(ArchiveError::Invalid(
                "parallel compressor returned an unexpected result",
            ));
        }
        self.outstanding -= 1;
        self.accept_result(result)
    }

    fn accept_result(&mut self, result: ParallelFrameResult) -> Result<()> {
        if result.sequence < self.next_write
            || result.sequence >= self.next_submit
            || self.pending_results.insert(result.sequence, result).is_some()
        {
            return Err(ArchiveError::Invalid(
                "parallel compressor returned an invalid sequence",
            ));
        }
        while let Some(result) = self.pending_results.remove(&self.next_write) {
            let output = self
                .output
                .as_mut()
                .ok_or(ArchiveError::Invalid("parallel archive writer is finished"))?;
            write_frame_header(output, result.info)?;
            match result.payload {
                ParallelFramePayload::Compressed(compressed) => {
                    output.write_all(&compressed)?;
                }
                ParallelFramePayload::Copied {
                    mut source,
                    payload_offset,
                } => {
                    source.seek(SeekFrom::Start(payload_offset))?;
                    let copied = io::copy(
                        &mut source.take(result.info.compressed_bytes),
                        output,
                    )?;
                    if copied != result.info.compressed_bytes {
                        return Err(ArchiveError::Invalid(
                            "copied frame payload is truncated",
                        ));
                    }
                }
            }
            self.frames_written = self
                .frames_written
                .checked_add(1)
                .ok_or(ArchiveError::FieldTooLarge)?;
            self.sealed_raw_bytes = self
                .sealed_raw_bytes
                .checked_add(result.info.raw_bytes)
                .ok_or(ArchiveError::FieldTooLarge)?;
            self.sealed_compressed_bytes = self
                .sealed_compressed_bytes
                .checked_add(result.info.compressed_bytes)
                .ok_or(ArchiveError::FieldTooLarge)?;
            self.next_write = self
                .next_write
                .checked_add(1)
                .ok_or(ArchiveError::FieldTooLarge)?;
        }
        Ok(())
    }

    /// Seal pending changed records, then enqueue one already-compressed frame
    /// in the same ordered output sequence.
    ///
    /// The source frame must use the same refPrefix as this writer. At most
    /// `workers` compressed jobs or completed frames are retained; the sole
    /// output owner drains them in sequence order.
    pub(crate) fn append_compressed_frame(
        &mut self,
        source: &std::fs::File,
        entry: crate::frame_directory::FrameDirectoryEntry,
    ) -> Result<CopiedFrameStats> {
        self.submit_current()?;
        let info = entry.frame_info();
        validate_frame_dictionary(info, None)?;
        if self
            .submitted_frontier
            .is_some_and(|previous| previous >= info.first_entity)
        {
            return Err(ArchiveError::Invalid(
                "copied frame is not after the output frontier",
            ));
        }
        while self
            .outstanding
            .saturating_add(self.pending_results.len())
            >= self.senders.len()
        {
            self.collect_one()?;
        }
        let header_offset = entry
            .compressed_offset
            .checked_sub(FRAME_HEADER_LEN as u64)
            .ok_or(ArchiveError::Invalid(
                "copied frame payload has no preceding header",
            ))?;
        let mut source = source.try_clone()?;
        source.seek(SeekFrom::Start(header_offset))?;
        let mut header = [0_u8; FRAME_HEADER_LEN];
        source.read_exact(&mut header)?;
        if parse_frame_header(&header)? != Some(info) {
            return Err(ArchiveError::Invalid(
                "copied frame header disagrees with its directory",
            ));
        }
        let result = ParallelFrameResult {
            sequence: self.next_submit,
            info,
            payload: ParallelFramePayload::Copied {
                source,
                payload_offset: entry.compressed_offset,
            },
        };
        self.next_submit = self
            .next_submit
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.submitted_frontier = Some(info.last_entity);
        self.accept_result(result)?;
        Ok(CopiedFrameStats {
            frames: 1,
            records: info.records,
            raw_bytes: info.raw_bytes,
            compressed_bytes: info.compressed_bytes,
        })
    }

    #[cfg(test)]
    fn buffered_frames(&self) -> usize {
        self.outstanding.saturating_add(self.pending_results.len())
    }

    #[cfg(test)]
    fn buffered_frame_limit(&self) -> usize {
        self.senders.len()
    }

    fn stop_workers(&mut self) {
        for sender in &self.senders {
            let _ = sender.send(None);
        }
        self.senders.clear();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }

    pub(crate) fn finish(mut self) -> Result<(W, u64)> {
        self.submit_current()?;
        while self.outstanding != 0 {
            self.collect_one()?;
        }
        if self.next_write != self.next_submit || !self.pending_results.is_empty() {
            return Err(ArchiveError::Invalid(
                "parallel compressor result sequence is incomplete",
            ));
        }
        self.stop_workers();
        let mut output = self
            .output
            .take()
            .ok_or(ArchiveError::Invalid("parallel archive writer is finished"))?;
        output.write_all(&DONE_MAGIC)?;
        output.write_all(&[0; FRAME_HEADER_LEN - 4])?;
        output.flush()?;
        Ok((output, self.frames_written))
    }
}

impl<W: Write> Drop for ParallelArchiveWriter<W> {
    fn drop(&mut self) {
        self.stop_workers();
    }
}

fn parallel_frame_worker(
    jobs: std::sync::mpsc::Receiver<Option<ParallelFrameJob>>,
    results: std::sync::mpsc::Sender<Result<ParallelFrameResult>>,
    prefix: Arc<[u8]>,
    compression: CompressionSettings,
) {
    let reference = zstd::dict::EncoderDictionary::copy(&prefix, compression.level);
    let mut context = zstd::zstd_safe::CCtx::create();
    let configured =
        configure_streaming_context(&mut context, compression, &reference, prefix.len());
    while let Ok(Some(job)) = jobs.recv() {
        let result = configured
            .as_ref()
            .map_err(|error| ArchiveError::Io(io::Error::other(error.to_string())))
            .and_then(|_| compress_parallel_frame(&mut context, job));
        if results.send(result).is_err() {
            break;
        }
    }
}

fn compress_parallel_frame(
    context: &mut zstd::zstd_safe::CCtx<'static>,
    job: ParallelFrameJob,
) -> Result<ParallelFrameResult> {
    let raw_bytes =
        u64::try_from(job.raw.len()).map_err(|_| ArchiveError::FieldTooLarge)?;
    let mut compressed = Vec::new();
    let mut scratch = vec![0_u8; zstd::zstd_safe::CCtx::out_size()];
    pump_streaming_context(
        context,
        &job.raw,
        zstd::zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_end,
        &mut compressed,
        &mut scratch,
    )?;
    context
        .reset(zstd::zstd_safe::ResetDirective::SessionOnly)
        .map_err(zstd_error)?;
    Ok(ParallelFrameResult {
        sequence: job.sequence,
        info: FrameInfo {
            first_entity: job.first_entity,
            last_entity: job.last_entity,
            records: job.records,
            raw_bytes,
            compressed_bytes: u64::try_from(compressed.len())
                .map_err(|_| ArchiveError::FieldTooLarge)?,
            dictionary_id: None,
        },
        payload: ParallelFramePayload::Compressed(compressed),
    })
}

struct StreamingRecordSink<'a, W: Write> {
    writer: &'a mut StreamingArchiveWriter<W>,
}

impl<W: Write> Write for StreamingRecordSink<'_, W> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let written = bytes.len();
        while !bytes.is_empty() {
            let available = self.writer.feed_interval - self.writer.pending.len();
            let take = available.min(bytes.len());
            self.writer.pending.extend_from_slice(&bytes[..take]);
            self.writer.raw_bytes = self
                .writer
                .raw_bytes
                .checked_add(take as u64)
                .ok_or_else(|| io::Error::other("archive frame is too large"))?;
            bytes = &bytes[take..];
            if self.writer.pending.len() == self.writer.feed_interval {
                self.writer
                    .feed_pending()
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer
            .feed_pending()
            .map_err(|error| io::Error::other(error.to_string()))
    }
}

pub struct ArchiveWriter<'a, W: Write> {
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
    frame: Option<FrameBuilder<'a>>,
    last_entity: Option<EntityKey>,
    last_timestamp: i64,
    frames: u64,
    ref_prefix: Option<&'a [u8]>,
    prepared_reference:
        Option<std::sync::Arc<zstd::dict::EncoderDictionary<'static>>>,
    dictionary_id: Option<u32>,
    range_boundaries: std::collections::VecDeque<EntityKey>,
}

impl<W: Write> ArchiveWriter<'static, W> {
    pub fn new(output: W, frame_target: usize) -> Result<Self> {
        Self::with_compression(output, frame_target, CompressionSettings::default())
    }

    pub fn with_compression(
        output: W,
        frame_target: usize,
        compression: CompressionSettings,
    ) -> Result<Self> {
        Self::with_compression_and_dictionary(output, frame_target, compression, None)
    }

    fn with_compression_and_dictionary(
        mut output: W,
        frame_target: usize,
        compression: CompressionSettings,
        dictionary: Option<Vec<u8>>,
    ) -> Result<Self> {
        if frame_target == 0 {
            return Err(ArchiveError::Invalid("zero frame target"));
        }
        output.write_all(&FILE_MAGIC)?;
        output.write_all(&FILE_VERSION.to_le_bytes())?;
        output.write_all(&0_u32.to_le_bytes())?;
        output.write_all(&(frame_target as u64).to_le_bytes())?;
        let dictionary = dictionary.map(std::sync::Arc::<[u8]>::from);
        let dictionary_id = dictionary
            .as_deref()
            .map(dictionary_id)
            .transpose()?;
        let prepared_reference = dictionary.as_deref().map(|dictionary| {
            std::sync::Arc::new(zstd::dict::EncoderDictionary::copy(
                dictionary,
                compression.level,
            ))
        });
        if let (Some(dictionary), Some(dictionary_id)) =
            (dictionary.as_deref(), dictionary_id)
        {
            write_dictionary_frame(&mut output, dictionary, dictionary_id, compression)?;
        }
        Ok(Self {
            output,
            frame_target,
            compression,
            frame: None,
            last_entity: None,
            last_timestamp: i64::MAX,
            frames: 0,
            ref_prefix: None,
            prepared_reference,
            dictionary_id,
            range_boundaries: std::collections::VecDeque::new(),
        })
    }
}

impl<'a, W: Write> ArchiveWriter<'a, W> {
    pub fn with_ref_prefix(
        output: W,
        frame_target: usize,
        compression: CompressionSettings,
        prefix: &'a [u8],
    ) -> Result<Self> {
        Self::with_compression_and_ref_prefix(output, frame_target, compression, prefix)
    }

    fn with_compression_and_ref_prefix(
        mut output: W,
        frame_target: usize,
        compression: CompressionSettings,
        prefix: &'a [u8],
    ) -> Result<Self> {
        if frame_target == 0 {
            return Err(ArchiveError::Invalid("zero frame target"));
        }
        if prefix.is_empty() {
            return Err(ArchiveError::Invalid("empty reference prefix"));
        }
        output.write_all(&FILE_MAGIC)?;
        output.write_all(&FILE_VERSION.to_le_bytes())?;
        output.write_all(&0_u32.to_le_bytes())?;
        output.write_all(&(frame_target as u64).to_le_bytes())?;
        write_ref_prefix_frame(&mut output, prefix, compression)?;
        let prepared_reference = Some(std::sync::Arc::new(
            zstd::dict::EncoderDictionary::copy(prefix, compression.level),
        ));
        Ok(Self {
            output,
            frame_target,
            compression,
            frame: None,
            last_entity: None,
            last_timestamp: i64::MAX,
            frames: 0,
            ref_prefix: Some(prefix),
            prepared_reference,
            dictionary_id: None,
            range_boundaries: std::collections::VecDeque::new(),
        })
    }

    pub fn write(&mut self, record: &Record) -> Result<()> {
        let entity = record.entity();
        let timestamp = record.timestamp_micros();
        let new_entity = self.last_entity != Some(entity);
        if new_entity {
            while let (Some(last), Some(boundary)) =
                (self.last_entity, self.range_boundaries.front().copied())
            {
                if last.kind > boundary.kind
                    || (last.kind == boundary.kind && last.id > boundary.id)
                {
                    return Err(ArchiveError::Invalid(
                        "archive range boundary precedes written records",
                    ));
                }
                if entity.kind > boundary.kind
                    || (entity.kind == boundary.kind && entity.id > boundary.id)
                {
                    if last.kind != boundary.kind || last.id != boundary.id {
                        return Err(ArchiveError::Invalid(
                            "archive range boundary is not an entity boundary",
                        ));
                    }
                    self.seal_frame()?;
                    self.range_boundaries.pop_front();
                    continue;
                }
                break;
            }
        }
        if let Some(last_entity) = self.last_entity {
            if entity < last_entity || (!new_entity && timestamp > self.last_timestamp) {
                return Err(ArchiveError::OutOfOrder {
                    previous: last_entity,
                    previous_timestamp: self.last_timestamp,
                    current: entity,
                    current_timestamp: timestamp,
                });
            }
        }
        if new_entity {
            if let Some(frame) = self.frame.as_mut() {
                if frame.last_entity.kind != entity.kind
                    || frame.compressed_so_far() >= self.frame_target
                    || frame.raw_bytes
                        >= self.frame_target.saturating_mul(8) as u64
                {
                    self.seal_frame()?;
                }
            }
        }
        if self.frame.is_none() {
            self.frame = Some(FrameBuilder::new(
                entity,
                self.compression,
                self.prepared_reference.as_ref(),
                self.ref_prefix
                    .map(|prefix| ref_prefix_window_log(prefix.len())),
            )?);
        }

        let frame = self.frame.as_mut().expect("created above");
        frame.last_entity = entity;
        frame.encoder.write_all(&[entity.kind as u8])?;
        write_varint(&mut frame.encoder, entity.id)?;
        frame.encoder.write_all(&timestamp.to_le_bytes())?;
        let (kind, payload_len) = record_wire_size(record)?;
        frame.encoder.write_all(&[kind])?;
        write_varint(&mut frame.encoder, payload_len)?;
        write_record_payload(&mut frame.encoder, record)?;
        frame.raw_bytes = frame
            .raw_bytes
            .checked_add(
                1 + varint_len(entity.id) as u64
                    + 8
                    + 1
                    + varint_len(payload_len) as u64
                    + payload_len,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        frame.records += 1;
        self.last_entity = Some(entity);
        self.last_timestamp = timestamp;
        Ok(())
    }

    fn seal_frame(&mut self) -> Result<()> {
        let Some(frame) = self.frame.take() else {
            return Ok(());
        };
        let compressed = frame.encoder.finish()?;
        let native_dictionary_id =
            zstd::zstd_safe::get_dict_id_from_frame(&compressed).map(u32::from);
        if native_dictionary_id.is_some() && native_dictionary_id != self.dictionary_id {
            return Err(ArchiveError::Invalid(
                "zstd frame references an unexpected dictionary",
            ));
        }
        self.output.write_all(&FRAME_MAGIC)?;
        self.output
            .write_all(&(FRAME_HEADER_LEN as u32).to_le_bytes())?;
        self.output.write_all(&[frame.first_entity.kind as u8])?;
        self.output.write_all(&[frame.last_entity.kind as u8])?;
        self.output.write_all(&[0; 6])?;
        self.output
            .write_all(&frame.first_entity.id.to_le_bytes())?;
        self.output.write_all(&frame.last_entity.id.to_le_bytes())?;
        self.output.write_all(&frame.records.to_le_bytes())?;
        self.output.write_all(&frame.raw_bytes.to_le_bytes())?;
        self.output
            .write_all(&(compressed.len() as u64).to_le_bytes())?;
        self.output
            .write_all(&native_dictionary_id.unwrap_or(0).to_le_bytes())?;
        self.output.write_all(&[0; 4])?;
        self.output.write_all(&compressed)?;
        self.frames += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<(W, u64)> {
        while let Some(boundary) = self.range_boundaries.pop_front() {
            if self.last_entity != Some(boundary) {
                return Err(ArchiveError::Invalid(
                    "archive range boundary was not reached",
                ));
            }
            self.seal_frame()?;
        }
        self.seal_frame()?;
        self.output.write_all(&DONE_MAGIC)?;
        self.output.write_all(&[0; FRAME_HEADER_LEN - 4])?;
        self.output.flush()?;
        Ok((self.output, self.frames))
    }

    pub(crate) fn set_range_boundaries(&mut self, boundaries: Vec<EntityKey>) -> Result<()> {
        if boundaries
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(ArchiveError::Invalid(
                "archive range boundaries are not strictly ordered",
            ));
        }
        if self.last_entity.is_some() || self.frame.is_some() {
            return Err(ArchiveError::Invalid(
                "archive range boundaries were set after writing began",
            ));
        }
        self.range_boundaries = boundaries.into();
        Ok(())
    }
}

pub struct ArchiveReader<R: Read> {
    input: BufReader<R>,
    pub frame_target: u64,
    complete: bool,
    last_frame_entity: Option<EntityKey>,
    pending_header: Option<[u8; FRAME_HEADER_LEN]>,
    reference: Option<CompressionReference>,
    dictionary_id: Option<u32>,
}

impl<R: Read> ArchiveReader<R> {
    pub fn new(input: R) -> Result<Self> {
        let mut input = BufReader::new(input);
        let frame_target = read_file_header(&mut input)?;
        let pending_header = read_frame_header_or_eof(&mut input)?;
        let (pending_header, reference, dictionary_id) =
            if let Some(header) = pending_header {
                if let Some(info) = parse_dictionary_header(&header)? {
                    let dictionary = read_dictionary_payload(&mut input, info)?;
                    (
                        None,
                        Some(CompressionReference::Dictionary(
                            std::sync::Arc::<[u8]>::from(dictionary),
                        )),
                        Some(info.id),
                    )
                } else if let Some(info) = parse_ref_prefix_header(&header)? {
                    let prefix = read_ref_prefix_payload(&mut input, info)?;
                    (
                        None,
                        Some(CompressionReference::RefPrefix(
                            std::sync::Arc::<[u8]>::from(prefix),
                        )),
                        None,
                    )
                } else {
                    (Some(header), None, None)
                }
            } else {
                (None, None, None)
            };
        Ok(Self {
            input,
            frame_target,
            complete: false,
            last_frame_entity: None,
            pending_header,
            reference,
            dictionary_id,
        })
    }

    pub fn next_frame(
        &mut self,
    ) -> Result<Option<ArchiveFrameReader<BorrowedFrameDecoder<'_, R>>>> {
        let header = match self.pending_header.take() {
            Some(header) => Some(header),
            None => read_frame_header_or_eof(&mut self.input)?,
        };
        let Some(header) = header else {
            return Ok(None);
        };
        let Some(info) = parse_frame_header(&header)? else {
            self.complete = true;
            return Ok(None);
        };
        validate_frame_dictionary(info, self.dictionary_id)?;
        if self
            .last_frame_entity
            .is_some_and(|previous| previous >= info.first_entity)
        {
            return Err(ArchiveError::Invalid(
                "entity group is split or frames are out of order",
            ));
        }
        self.last_frame_entity = Some(info.last_entity);
        let limited = (&mut self.input).take(info.compressed_bytes);
        let compressed = compressed_frame_reader(limited, info)?;
        let decoder = frame_decoder(
            BufReader::with_capacity(FRAME_READ_AHEAD, compressed),
            info,
            self.reference.as_ref(),
        )?;
        Ok(Some(ArchiveFrameReader {
            decoder,
            info,
            records_read: 0,
            raw_bytes_read: 0,
            last_entity: None,
            last_timestamp: i64::MAX,
            finished: false,
        }))
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

pub type BorrowedFrameDecoder<'a, R> =
    zstd::stream::read::Decoder<
        'a,
        BufReader<std::io::Chain<Cursor<Vec<u8>>, Take<&'a mut BufReader<R>>>>,
    >;

pub struct ArchiveFrameReader<D: Read> {
    decoder: D,
    info: FrameInfo,
    records_read: u64,
    raw_bytes_read: u64,
    last_entity: Option<EntityKey>,
    last_timestamp: i64,
    finished: bool,
}

pub fn index_file(path: impl AsRef<Path>) -> Result<(u64, Vec<FrameLocation>, bool)> {
    let path = path.as_ref();
    if path.is_dir() {
        index_reader(crate::archive_set::ArchiveSetReader::open(path)?)
    } else {
        let file = std::fs::File::open(path)?;
        index_open_file(&file)
    }
}

pub(crate) fn has_clean_completion_marker(path: impl AsRef<Path>) -> Result<bool> {
    let mut file = std::fs::File::open(path)?;
    if file.metadata()?.len() < FRAME_HEADER_LEN as u64 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-(FRAME_HEADER_LEN as i64)))?;
    let mut marker = [0_u8; FRAME_HEADER_LEN];
    file.read_exact(&mut marker)?;
    Ok(marker[..4] == DONE_MAGIC && marker[4..].iter().all(|byte| *byte == 0))
}

pub(crate) fn index_open_file(
    file: &std::fs::File,
) -> Result<(u64, Vec<FrameLocation>, bool)> {
    index_reader(file.try_clone()?)
}

/// Visit data-frame headers without retaining their locations or reading
/// compressed record payloads.
///
/// Cost: one file-header read, one 64-byte read per frame/reference, seeks
/// across compressed payloads, one descriptor, and constant memory.
pub(crate) fn visit_file_frame_headers(
    path: impl AsRef<Path>,
    mut visitor: impl FnMut(FrameInfo, u64) -> Result<()>,
) -> Result<()> {
    let mut file = BufReader::new(std::fs::File::open(path)?);
    let _ = read_file_header(&mut file)?;
    let mut pending_header = read_frame_header_or_eof(&mut file)?;
    let mut active_dictionary_id = None;
    if let Some(header) = pending_header {
        if let Some(info) = parse_dictionary_header(&header)? {
            active_dictionary_id = Some(info.id);
            file.seek(SeekFrom::Current(
                info.compressed_bytes
                    .try_into()
                    .map_err(|_| ArchiveError::FieldTooLarge)?,
            ))?;
            pending_header = None;
        } else if let Some(info) = parse_ref_prefix_header(&header)? {
            file.seek(SeekFrom::Current(
                info.compressed_bytes
                    .try_into()
                    .map_err(|_| ArchiveError::FieldTooLarge)?,
            ))?;
            pending_header = None;
        }
    }
    let mut previous = None;
    loop {
        let header = match pending_header.take() {
            Some(header) => Some(header),
            None => read_frame_header_or_eof(&mut file)?,
        };
        let Some(header) = header else {
            return Err(ArchiveError::Invalid(
                "archive has no clean completion marker",
            ));
        };
        let Some(info) = parse_frame_header(&header)? else {
            return Ok(());
        };
        validate_frame_dictionary(info, active_dictionary_id)?;
        if previous.is_some_and(|entity| entity >= info.first_entity) {
            return Err(ArchiveError::Invalid(
                "entity group is split or frames are out of order",
            ));
        }
        previous = Some(info.last_entity);
        let compressed_offset = file.stream_position()?;
        visitor(info, compressed_offset)?;
        file.seek(SeekFrom::Current(
            info.compressed_bytes
                .try_into()
                .map_err(|_| ArchiveError::FieldTooLarge)?,
        ))?;
    }
}

/// Visit a range-set data part containing only FRAME header/payload pairs.
///
/// A clean physical EOF is the completion boundary. Offsets passed to the
/// visitor are local to the part. RefPrefix-compressed range parts carry no
/// zstd dictionary ID, so any dictionary-bearing frame is rejected.
pub(crate) fn visit_data_segment_frame_headers(
    path: impl AsRef<Path>,
    mut visitor: impl FnMut(FrameInfo, u64) -> Result<()>,
) -> Result<()> {
    let file = std::fs::File::open(path)?;
    let file_bytes = file.metadata()?.len();
    let mut file = BufReader::new(file);
    let mut previous = None;
    let mut frames = 0_u64;
    loop {
        let Some(header) = read_frame_header_or_eof(&mut file)? else {
            if frames == 0 {
                return Err(ArchiveError::Invalid(
                    "archive data segment contains no frames",
                ));
            }
            return Ok(());
        };
        let info = parse_frame_header(&header)?.ok_or(ArchiveError::Invalid(
            "archive data segment contains a completion marker",
        ))?;
        validate_frame_dictionary(info, None)?;
        if previous.is_some_and(|entity| entity >= info.first_entity) {
            return Err(ArchiveError::Invalid(
                "entity group is split or segment frames are out of order",
            ));
        }
        previous = Some(info.last_entity);
        let compressed_offset = file.stream_position()?;
        let end = compressed_offset
            .checked_add(info.compressed_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        if end > file_bytes {
            return Err(ArchiveError::Invalid(
                "archive data segment has a truncated frame payload",
            ));
        }
        visitor(info, compressed_offset)?;
        file.seek(SeekFrom::Start(end))?;
        frames = frames.checked_add(1).ok_or(ArchiveError::FieldTooLarge)?;
    }
}

pub(crate) fn index_reader(
    file: impl Read + Seek,
) -> Result<(u64, Vec<FrameLocation>, bool)> {
    let mut file = BufReader::new(file);
    let frame_target = read_file_header(&mut file)?;
    let mut locations = Vec::new();
    let mut previous = None;
    let mut pending_header = read_frame_header_or_eof(&mut file)?;
    let mut reference = None;
    let mut active_dictionary_id = None;
    if let Some(header) = pending_header {
        if let Some(info) = parse_dictionary_header(&header)? {
            let bytes = read_dictionary_payload(&mut file, info)?;
            reference = Some(CompressionReference::Dictionary(
                std::sync::Arc::<[u8]>::from(bytes),
            ));
            active_dictionary_id = Some(info.id);
            pending_header = None;
        } else if let Some(info) = parse_ref_prefix_header(&header)? {
            let bytes = read_ref_prefix_payload(&mut file, info)?;
            reference = Some(CompressionReference::RefPrefix(
                std::sync::Arc::<[u8]>::from(bytes),
            ));
            pending_header = None;
        }
    }
    loop {
        let header = match pending_header.take() {
            Some(header) => Some(header),
            None => read_frame_header_or_eof(&mut file)?,
        };
        let Some(header) = header else {
            return Ok((frame_target, locations, false));
        };
        let Some(info) = parse_frame_header(&header)? else {
            return Ok((frame_target, locations, true));
        };
        validate_frame_dictionary(info, active_dictionary_id)?;
        if previous.is_some_and(|entity| entity >= info.first_entity) {
            return Err(ArchiveError::Invalid(
                "entity group is split or frames are out of order",
            ));
        }
        previous = Some(info.last_entity);
        let compressed_offset = file.stream_position()?;
        locations.push(FrameLocation {
            info,
            compressed_offset,
            reference: reference.clone(),
            physical_segment: None,
        });
        file.seek(SeekFrom::Current(
            info.compressed_bytes
                .try_into()
                .map_err(|_| ArchiveError::FieldTooLarge)?,
        ))?;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct IndexedArchiveSet {
    segments: Vec<crate::title_index::SegmentIndexEntry>,
    direct_file: Option<std::sync::Arc<std::fs::File>>,
    directory: Option<std::sync::Arc<ArchiveDirectoryFiles>>,
    reference: Option<CompressionReference>,
    active_dictionary_id: Option<u32>,
}

#[derive(Debug)]
struct ArchiveDirectoryFiles {
    root: std::fs::File,
    cache: std::sync::Mutex<VecDeque<(usize, std::sync::Arc<std::fs::File>)>>,
}

const ARCHIVE_SEGMENT_FILE_CACHE: usize = 8;

#[derive(Debug)]
pub(crate) struct ArchiveCleanupLease {
    _file: std::fs::File,
}

impl IndexedArchiveSet {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        titles: &crate::title_index::TitleIndex,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            if titles.segment_count() != 0 {
                return Err(ArchiveError::Invalid(
                    "single-file archive has archive-set segments",
                ));
            }
            let file = std::sync::Arc::new(std::fs::File::open(&root)?);
            lock_archive_shared(&file)?;
            let (_, locations, complete) = index_open_file(&file)?;
            if !complete {
                return Err(ArchiveError::Invalid(
                    "single-file archive lacks completion marker",
                ));
            }
            if locations.len() != titles.frame_count() {
                return Err(ArchiveError::Invalid(
                    "single-file archive frame count disagrees with title index",
                ));
            }
            for (position, location) in locations.iter().enumerate() {
                let indexed = titles.frame(position)?;
                if indexed.info != location.info
                    || indexed.compressed_offset != location.compressed_offset
                {
                    return Err(ArchiveError::Invalid(
                        "single-file archive frame disagrees with title index",
                    ));
                }
            }
            let reference = locations
                .first()
                .and_then(|location| location.reference.clone());
            if locations
                .iter()
                .any(|location| location.reference != reference)
            {
                return Err(ArchiveError::Invalid(
                    "single-file archive changes compression reference",
                ));
            }
            let active_dictionary_id = locations
                .iter()
                .find_map(|location| location.info.dictionary_id);
            return Ok(Self {
                segments: Vec::new(),
                direct_file: Some(file),
                directory: None,
                reference,
                active_dictionary_id,
            });
        }
        if titles.segment_count() < 3 {
            return Err(ArchiveError::Invalid(
                "Wikipedia archive is not a range-file set",
            ));
        }
        let root_file = std::fs::File::open(&root)?;
        lock_archive_shared(&root_file)?;
        let directory = std::sync::Arc::new(ArchiveDirectoryFiles {
            root: root_file,
            cache: std::sync::Mutex::new(VecDeque::new()),
        });
        let mut segments = Vec::with_capacity(titles.segment_count());
        let mut expected_start = 0_u64;
        for position in 0..titles.segment_count() {
            let segment = titles.segment(position)?;
            if segment.virtual_start != expected_start {
                return Err(ArchiveError::Invalid(
                    "archive-set index has a virtual-offset gap",
                ));
            }
            let name = crate::archive_set::indexed_segment_name(segment)?;
            let file = open_archive_child(&directory.root, &name)?;
            if file.metadata()?.len() != segment.bytes {
                return Err(ArchiveError::Invalid(
                    "archive-set segment size does not match its index",
                ));
            }
            expected_start = expected_start
                .checked_add(segment.bytes)
                .ok_or(ArchiveError::FieldTooLarge)?;
            segments.push(segment);
        }
        if segments.first().map(|segment| segment.role) != Some(0)
            || segments.last().map(|segment| segment.role) != Some(4)
        {
            return Err(ArchiveError::Invalid(
                "archive-set index lacks reference or completion segment",
            ));
        }
        let mut previous_data = None;
        for segment in &segments {
            if (1..=3).contains(&segment.role) {
                if segment.first_id > segment.last_id
                    || previous_data.is_some_and(|(role, last_id)| {
                        segment.role < role
                            || (segment.role == role && segment.first_id <= last_id)
                    })
                {
                    return Err(ArchiveError::Invalid(
                        "archive-set index has overlapping or unordered entity ranges",
                    ));
                }
                previous_data = Some((segment.role, segment.last_id));
            } else if segment.first_id != 0 || segment.last_id != 0 {
                return Err(ArchiveError::Invalid(
                    "archive-set control segment has an entity range",
                ));
            }
        }

        let reference_name = crate::archive_set::indexed_segment_name(segments[0])?;
        let mut input = BufReader::new(open_archive_child(
            &directory.root,
            &reference_name,
        )?);
        let _ = read_file_header(&mut input)?;
        let header = read_frame_header_or_eof(&mut input)?.ok_or(
            ArchiveError::Invalid("archive-set reference segment is empty"),
        )?;
        let (reference, active_dictionary_id) =
            if let Some(info) = parse_dictionary_header(&header)? {
                let bytes = read_dictionary_payload(&mut input, info)?;
                (
                    Some(CompressionReference::Dictionary(
                        std::sync::Arc::<[u8]>::from(bytes),
                    )),
                    Some(info.id),
                )
            } else if let Some(info) = parse_ref_prefix_header(&header)? {
                let bytes = read_ref_prefix_payload(&mut input, info)?;
                (
                    Some(CompressionReference::RefPrefix(
                        std::sync::Arc::<[u8]>::from(bytes),
                    )),
                    None,
                )
            } else {
                return Err(ArchiveError::Invalid(
                    "archive-set reference segment has no compression reference",
                ));
            };
        if read_frame_header_or_eof(&mut input)?.is_some() {
            return Err(ArchiveError::Invalid(
                "archive-set reference segment contains data frames",
            ));
        }

        let completion_name = crate::archive_set::indexed_segment_name(
            *segments.last().expect("checked"),
        )?;
        let mut completion = open_archive_child(&directory.root, &completion_name)?;
        let done = read_frame_header_or_eof(&mut completion)?
            .ok_or(ArchiveError::Invalid("archive-set completion segment is empty"))?;
        if parse_frame_header(&done)?.is_some()
            || read_frame_header_or_eof(&mut completion)?.is_some()
        {
            return Err(ArchiveError::Invalid(
                "archive-set completion segment is malformed",
            ));
        }
        Ok(Self {
            segments,
            direct_file: None,
            directory: Some(directory),
            reference,
            active_dictionary_id,
        })
    }

    pub(crate) fn location(
        &self,
        entry: crate::title_index::FrameIndexEntry,
    ) -> Result<FrameLocation> {
        if self.direct_file.is_some() {
            validate_frame_dictionary(entry.info, self.active_dictionary_id)?;
            return Ok(FrameLocation {
                info: entry.info,
                compressed_offset: entry.compressed_offset,
                reference: self.reference.clone(),
                physical_segment: None,
            });
        }
        let position = self
            .segments
            .partition_point(|segment| segment.virtual_start <= entry.compressed_offset)
            .checked_sub(1)
            .ok_or(ArchiveError::Invalid(
                "frame offset precedes archive-set segments",
            ))?;
        let segment = &self.segments[position];
        if !(1..=3).contains(&segment.role) {
            return Err(ArchiveError::Invalid(
                "frame points into a non-data archive-set segment",
            ));
        }
        let expected_kind = match segment.role {
            1 => EntityKind::Page,
            2 => EntityKind::User,
            3 => EntityKind::Global,
            _ => unreachable!("checked above"),
        };
        if entry.info.first_entity.kind != expected_kind
            || entry.info.last_entity.kind != expected_kind
            || entry.info.first_entity.id < segment.first_id
            || entry.info.last_entity.id > segment.last_id
        {
            return Err(ArchiveError::Invalid(
                "frame entity range disagrees with its archive-set segment",
            ));
        }
        let local = entry.compressed_offset - segment.virtual_start;
        let end = local
            .checked_add(entry.info.compressed_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        if local < FRAME_HEADER_LEN as u64 || end > segment.bytes {
            return Err(ArchiveError::Invalid(
                "frame points outside its archive-set segment",
            ));
        }
        validate_frame_dictionary(entry.info, self.active_dictionary_id)?;
        Ok(FrameLocation {
            info: entry.info,
            compressed_offset: local,
            reference: self.reference.clone(),
            physical_segment: Some(position),
        })
    }

    pub(crate) fn open_file(&self, location: &FrameLocation) -> Result<std::fs::File> {
        let Some(position) = location.physical_segment else {
            return self
                .direct_file
                .as_ref()
                .ok_or(ArchiveError::Invalid(
                    "archive frame has no physical file",
                ))?
                .try_clone()
                .map_err(ArchiveError::Io);
        };
        let segment = *self
            .segments
            .get(position)
            .ok_or(ArchiveError::Invalid(
                "archive-set frame segment is out of bounds",
            ))?;
        let directory = self
            .directory
            .as_ref()
            .ok_or(ArchiveError::Invalid(
                "archive-set frame has no directory lease",
            ))?;
        let mut cache = directory
            .cache
            .lock()
            .map_err(|_| ArchiveError::Invalid("archive file cache is poisoned"))?;
        if let Some(found) = cache
            .iter()
            .position(|(cached_position, _)| *cached_position == position)
        {
            let (_, file) = cache
                .remove(found)
                .expect("file cache position was found");
            let opened = file.try_clone()?;
            cache.push_back((position, file));
            return Ok(opened);
        }
        let name = crate::archive_set::indexed_segment_name(segment)?;
        let file = std::sync::Arc::new(open_archive_child(&directory.root, &name)?);
        let opened = file.try_clone()?;
        cache.push_back((position, file));
        while cache.len() > ARCHIVE_SEGMENT_FILE_CACHE {
            cache.pop_front();
        }
        Ok(opened)
    }
}

#[cfg(unix)]
fn open_archive_child(root: &std::fs::File, name: &str) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = CString::new(name)
        .map_err(|_| ArchiveError::Invalid("archive segment name contains NUL"))?;
    let descriptor = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(ArchiveError::Io(io::Error::last_os_error()));
    }
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

#[cfg(not(unix))]
fn open_archive_child(_root: &std::fs::File, _name: &str) -> Result<std::fs::File> {
    Err(ArchiveError::Invalid(
        "archive-set directory leases require openat",
    ))
}

#[cfg(unix)]
fn lock_archive_shared(file: &std::fs::File) -> Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(ArchiveError::Io(error));
        }
    }
}

#[cfg(not(unix))]
fn lock_archive_shared(_file: &std::fs::File) -> Result<()> {
    Ok(())
}

/// Acquire the exclusive lease required before deleting a displaced archive
/// generation. `None` means an existing reader still owns that generation;
/// cleanup must be deferred rather than invalidating its lazy segment opens.
pub(crate) fn try_acquire_archive_cleanup_lease(
    path: impl AsRef<Path>,
) -> Result<Option<ArchiveCleanupLease>> {
    let file = std::fs::File::open(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(ArchiveError::Io(error));
        }
    }
    Ok(Some(ArchiveCleanupLease { _file: file }))
}

pub fn visit_frame(
    path: impl AsRef<Path>,
    location: &FrameLocation,
    mut visitor: impl FnMut(Record) -> Result<()>,
) -> Result<()> {
    visit_frame_while(path, location, |record| {
        visitor(record)?;
        Ok(true)
    })
}

pub fn visit_frame_while(
    path: impl AsRef<Path>,
    location: &FrameLocation,
    mut visitor: impl FnMut(Record) -> Result<bool>,
) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    visit_frame_while_file(&mut file, location, &mut visitor)
}

pub(crate) fn visit_frame_while_file(
    file: &mut std::fs::File,
    location: &FrameLocation,
    mut visitor: impl FnMut(Record) -> Result<bool>,
) -> Result<()> {
    file.seek(SeekFrom::Start(location.compressed_offset))?;
    let compressed = compressed_frame_reader(
        (&mut *file).take(location.info.compressed_bytes),
        location.info,
    )?;
    let decoder = frame_decoder(
        BufReader::with_capacity(FRAME_READ_AHEAD, compressed),
        location.info,
        location.reference.as_ref(),
    )?;
    let mut frame = ArchiveFrameReader {
        decoder,
        info: location.info,
        records_read: 0,
        raw_bytes_read: 0,
        last_entity: None,
        last_timestamp: i64::MAX,
        finished: false,
    };
    while let Some(record) = frame.next_record()? {
        if !visitor(record)? {
            return Ok(());
        }
    }
    Ok(())
}

pub struct ArchiveRecordReader {
    source: ArchiveRecordSource,
    frames: ArchiveFrameSequence,
    current: Option<ArchiveFrameReader<OwnedFrameDecoder>>,
    current_frame_offset: Option<u64>,
    completed_compressed_bytes: Option<Arc<AtomicU64>>,
}

enum ArchiveFrameSequence {
    Owned(std::vec::IntoIter<FrameLocation>),
    Directory {
        directory: Arc<crate::frame_directory::FrameDirectory>,
        position: usize,
        reference: Option<CompressionReference>,
    },
}

impl ArchiveFrameSequence {
    fn next_location(&mut self) -> Result<Option<FrameLocation>> {
        match self {
            Self::Owned(frames) => Ok(frames.next()),
            Self::Directory {
                directory,
                position,
                reference,
            } => {
                if *position >= directory.len() {
                    return Ok(None);
                }
                let entry = directory.get(*position)?;
                *position += 1;
                Ok(Some(FrameLocation {
                    info: entry.frame_info(),
                    compressed_offset: entry.compressed_offset,
                    reference: reference.clone(),
                    physical_segment: None,
                }))
            }
        }
    }

    fn remaining_count(&self) -> usize {
        match self {
            Self::Owned(frames) => frames.len(),
            Self::Directory {
                directory,
                position,
                ..
            } => directory.len() - *position,
        }
    }

}

enum ArchiveRecordSource {
    File {
        path: PathBuf,
        input: Option<OwnedInput>,
    },
    Set {
        segments: Vec<(u64, PathBuf)>,
        active_segment: Option<usize>,
        input: Option<OwnedInput>,
    },
}

impl ArchiveRecordSource {
    fn file(path: PathBuf) -> Self {
        Self::File { path, input: None }
    }

    fn set(segments: Vec<(u64, PathBuf)>) -> Self {
        Self::Set {
            segments,
            active_segment: None,
            input: None,
        }
    }
}

impl Clone for ArchiveRecordSource {
    fn clone(&self) -> Self {
        match self {
            Self::File { path, .. } => Self::file(path.clone()),
            Self::Set { segments, .. } => Self::set(segments.clone()),
        }
    }
}

pub(crate) trait RecordSource {
    fn next_record(&mut self) -> Result<Option<Record>>;
}

pub(crate) struct SequentialRecordGroups {
    groups: std::vec::IntoIter<Vec<PathBuf>>,
    current: Option<SortedArchiveMerge<'static>>,
    last_entity: Option<EntityKey>,
    at_group_start: bool,
    completed_compressed_bytes: Arc<AtomicU64>,
}

impl SequentialRecordGroups {
    /// Open only the currently consumed page-range group.
    ///
    /// Groups must be ordered and non-overlapping. Parts inside one group may
    /// overlap and are merged with bounded fan-in. Descriptor cost is
    /// O(parts in the current group), never O(all content targets).
    pub(crate) fn open_paths(
        groups: Vec<Vec<PathBuf>>,
        completed_compressed_bytes: Arc<AtomicU64>,
    ) -> Self {
        Self {
            groups: groups.into_iter(),
            current: None,
            last_entity: None,
            at_group_start: false,
            completed_compressed_bytes,
        }
    }
}

impl RecordSource for SequentialRecordGroups {
    fn next_record(&mut self) -> Result<Option<Record>> {
        loop {
            if let Some(current) = self.current.as_mut() {
                if let Some(record) = current.next_record()? {
                    let entity = record.entity();
                    if self.at_group_start
                        && self.last_entity.is_some_and(|last| entity <= last)
                    {
                        return Err(ArchiveError::Invalid(
                            "sequential archive groups overlap or are out of order",
                        ));
                    }
                    self.at_group_start = false;
                    self.last_entity = Some(entity);
                    return Ok(Some(record));
                }
                self.current = None;
            }
            let Some(group) = self.groups.next() else {
                return Ok(None);
            };
            if group.is_empty() {
                continue;
            }
            self.current = Some(SortedArchiveMerge::open_accounted(
                &group,
                &self.completed_compressed_bytes,
            )?);
            self.at_group_start = true;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootstrapMergePhase {
    Sampling,
    Distilling,
    Replaying,
    Merging,
}

trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

type OwnedInput = Box<dyn ReadSeek>;

type OwnedFrameDecoder = zstd::stream::read::Decoder<
    'static,
    BufReader<std::io::Chain<Cursor<Vec<u8>>, Take<OwnedInput>>>,
>;

pub(crate) struct FrameRecordCursor {
    inner: ArchiveFrameReader<OwnedFrameDecoder>,
}

impl FrameRecordCursor {
    pub(crate) fn next_record(&mut self) -> Result<Option<Record>> {
        self.inner.next_record()
    }
}

impl RecordSource for FrameRecordCursor {
    fn next_record(&mut self) -> Result<Option<Record>> {
        FrameRecordCursor::next_record(self)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RepackStats {
    pub input_frames: u64,
    pub output_frames: u64,
    pub records: u64,
    pub input_raw_bytes: u64,
    pub input_compressed_bytes: u64,
    pub dictionary_bytes: u64,
    pub compressed_dictionary_bytes: u64,
    pub ref_prefix_bytes: u64,
    pub compressed_ref_prefix_bytes: u64,
    pub sample_bytes: u64,
}

pub fn repack<R: Read + Seek, W: Write>(
    input: R,
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(W, RepackStats)> {
    repack_inner(input, output, frame_target, compression, None)
}

/// Write the archive's exact self-delimiting record wire sequence without
/// compression frames or a compression reference.
pub fn export_raw_record_stream(
    input: impl AsRef<Path>,
    mut output: impl Write,
) -> Result<u64> {
    output.write_all(&RAW_STREAM_MAGIC)?;
    output.write_all(&RAW_STREAM_VERSION.to_le_bytes())?;
    output.write_all(&0_u32.to_le_bytes())?;
    let mut reader = ArchiveRecordReader::open(input)?;
    let mut records = 0_u64;
    while let Some(record) = reader.next_record()? {
        output.write_all(&encode_record_wire(&record)?)?;
        records = records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
    output.write_all(&RAW_STREAM_DONE)?;
    Ok(records)
}

/// Read an explicitly selected raw record stream and frame it as a normal
/// archive. The completion marker makes an exact-record-boundary truncation
/// distinguishable from a successfully completed stream.
pub fn import_raw_record_stream<R: Read, W: Write>(
    mut input: R,
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(W, u64, u64)> {
    let mut header = [0_u8; RAW_STREAM_HEADER_LEN];
    input.read_exact(&mut header)?;
    if header[..8] != RAW_STREAM_MAGIC
        || u32::from_le_bytes(header[8..12].try_into().unwrap()) != RAW_STREAM_VERSION
        || u32::from_le_bytes(header[12..16].try_into().unwrap()) != 0
    {
        return Err(ArchiveError::Invalid("unknown raw record stream format"));
    }
    let mut writer = ArchiveWriter::with_compression(output, frame_target, compression)?;
    let mut records = 0_u64;
    loop {
        let mut first = [0_u8; 1];
        match input.read(&mut first)? {
            0 => {
                return Err(ArchiveError::Invalid(
                    "raw record stream lacks completion marker",
                ))
            }
            1 if first[0] == RAW_STREAM_DONE[0] => {
                let mut rest = [0_u8; RAW_STREAM_DONE.len() - 1];
                input.read_exact(&mut rest)?;
                if rest != RAW_STREAM_DONE[1..] {
                    return Err(ArchiveError::Invalid(
                        "malformed raw record stream completion marker",
                    ));
                }
                if input.read(&mut first)? != 0 {
                    return Err(ArchiveError::Invalid(
                        "raw record stream has trailing bytes",
                    ));
                }
                let (output, frames) = writer.finish()?;
                return Ok((output, frames, records));
            }
            1 => {}
            _ => unreachable!("one-byte read returned more than one byte"),
        }
        let entity = EntityKey {
            kind: EntityKind::try_from(first[0])?,
            id: read_varint(&mut input)?.0,
        };
        let timestamp = read_i64(&mut input)?;
        let kind = read_u8(&mut input)?;
        let payload_len: usize = read_varint(&mut input)?
            .0
            .try_into()
            .map_err(|_| ArchiveError::FieldTooLarge)?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(payload_len)
            .map_err(|_| ArchiveError::FieldTooLarge)?;
        payload.resize(payload_len, 0);
        input.read_exact(&mut payload)?;
        writer.write(&decode_record(entity, timestamp, kind, payload)?)?;
        records = records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
}

pub fn repack_with_dictionary<R: Read + Seek, W: Write>(
    input: R,
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
    dictionary_capacity: usize,
) -> Result<(W, RepackStats)> {
    if dictionary_capacity == 0 {
        return Err(ArchiveError::Invalid("zero dictionary capacity"));
    }
    repack_inner(
        input,
        output,
        frame_target,
        compression,
        Some(dictionary_capacity),
    )
}

pub fn repack_with_ref_prefix<R: Read + Seek, W: Write>(
    mut input: R,
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
    sample_capacity: usize,
    prefix_capacity: usize,
) -> Result<(W, RepackStats)> {
    if sample_capacity == 0 || prefix_capacity == 0 {
        return Err(ArchiveError::Invalid(
            "zero reference-prefix sample or capacity",
        ));
    }
    if sample_capacity <= prefix_capacity {
        return Err(ArchiveError::Invalid(
            "reference-prefix samples must be larger than the prefix",
        ));
    }
    let samples = archive_ref_prefix_samples(&mut input, sample_capacity)?;
    input.seek(SeekFrom::Start(0))?;
    let sample_bytes = samples.iter().try_fold(0_usize, |total, sample| {
        total
            .checked_add(sample.len())
            .ok_or(ArchiveError::FieldTooLarge)
    })?;
    if sample_bytes < prefix_capacity {
        return Err(ArchiveError::Invalid(
            "not enough archive data to distill requested reference prefix",
        ));
    }
    let prefix = distill_ref_prefix(&samples, prefix_capacity, compression.level)?;
    drop(samples);
    let mut reader = ArchiveReader::new(input)?;
    let writer = StreamingArchiveWriter::new(
        output,
        frame_target,
        compression,
        &prefix,
        usize::try_from(streaming_compression_workers())
            .unwrap_or(usize::MAX),
    )?;
    let mut stats = RepackStats {
        ref_prefix_bytes: prefix.len() as u64,
        compressed_ref_prefix_bytes: compressed_dictionary_size(&prefix, compression)? as u64,
        sample_bytes: sample_bytes as u64,
        ..RepackStats::default()
    };
    let (output, output_frames) =
        repack_records_streaming(&mut reader, writer, &mut stats)?;
    stats.output_frames = output_frames;
    Ok((output, stats))
}

fn repack_inner<R: Read + Seek, W: Write>(
    mut input: R,
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
    dictionary_capacity: Option<usize>,
) -> Result<(W, RepackStats)> {
    let dictionary = if let Some(capacity) = dictionary_capacity {
        let samples = archive_dictionary_samples(&mut input)?;
        input.seek(SeekFrom::Start(0))?;
        let sample_bytes = samples.iter().try_fold(0_usize, |total, sample| {
            total
                .checked_add(sample.len())
                .ok_or(ArchiveError::FieldTooLarge)
        })?;
        if samples.len() < 8 || sample_bytes < capacity {
            return Err(ArchiveError::Invalid(
                "not enough archive data to train requested dictionary",
            ));
        }
        Some(crate::frames::train_dictionary(&samples, capacity)?)
    } else {
        None
    };
    let mut reader = ArchiveReader::new(input)?;
    let writer = ArchiveWriter::with_compression_and_dictionary(
        output,
        frame_target,
        compression,
        dictionary.clone(),
    )?;
    let mut stats = RepackStats::default();
    if let Some(dictionary) = dictionary.as_deref() {
        stats.dictionary_bytes = dictionary.len() as u64;
        stats.compressed_dictionary_bytes =
            compressed_dictionary_size(dictionary, compression)? as u64;
    }
    let (output, output_frames) = repack_records(&mut reader, writer, &mut stats)?;
    stats.output_frames = output_frames;
    Ok((output, stats))
}

fn repack_records<'a, R: Read, W: Write>(
    reader: &mut ArchiveReader<R>,
    mut writer: ArchiveWriter<'a, W>,
    stats: &mut RepackStats,
) -> Result<(W, u64)> {
    while let Some(mut frame) = reader.next_frame()? {
        let info = frame.info();
        stats.input_frames += 1;
        stats.input_raw_bytes = stats
            .input_raw_bytes
            .checked_add(info.raw_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        stats.input_compressed_bytes = stats
            .input_compressed_bytes
            .checked_add(info.compressed_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        while let Some(record) = frame.next_record()? {
            writer.write(&record)?;
            stats.records += 1;
        }
    }
    if !reader.is_complete() {
        return Err(ArchiveError::Invalid(
            "archive has no clean completion marker",
        ));
    }
    writer.finish()
}

fn repack_records_streaming<R: Read, W: Write>(
    reader: &mut ArchiveReader<R>,
    mut writer: StreamingArchiveWriter<W>,
    stats: &mut RepackStats,
) -> Result<(W, u64)> {
    while let Some(mut frame) = reader.next_frame()? {
        let info = frame.info();
        stats.input_frames += 1;
        stats.input_raw_bytes = stats
            .input_raw_bytes
            .checked_add(info.raw_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        stats.input_compressed_bytes = stats
            .input_compressed_bytes
            .checked_add(info.compressed_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        while let Some(record) = frame.next_record()? {
            writer.write(&record)?;
            stats.records += 1;
        }
    }
    if !reader.is_complete() {
        return Err(ArchiveError::Invalid(
            "archive has no clean completion marker",
        ));
    }
    writer.finish()
}

fn archive_dictionary_samples(input: &mut (impl Read + Seek)) -> Result<Vec<Vec<u8>>> {
    input.seek(SeekFrom::Start(0))?;
    let mut reader = ArchiveReader::new(input)?;
    let mut selected = std::collections::BTreeMap::<(u64, u64), Vec<u8>>::new();
    let mut ordinal = 0_u64;
    while let Some(mut frame) = reader.next_frame()? {
        while let Some(record) = frame.next_record()? {
            let bytes = encode_record_wire(&record)?;
            let key = (xxhash_rust::xxh3::xxh3_64(&bytes), ordinal);
            if selected.len() < DICTIONARY_SAMPLE_COUNT
                || selected
                    .last_key_value()
                    .is_some_and(|(last, _)| key < *last)
            {
                selected.insert(key, bytes);
                if selected.len() > DICTIONARY_SAMPLE_COUNT {
                    selected.pop_last();
                }
            }
            ordinal = ordinal
                .checked_add(1)
                .ok_or(ArchiveError::FieldTooLarge)?;
        }
    }
    if !reader.is_complete() {
        return Err(ArchiveError::Invalid(
            "archive has no clean completion marker",
        ));
    }
    Ok(selected.into_values().collect())
}

fn archive_ref_prefix_samples(
    input: &mut (impl Read + Seek),
    target_bytes: usize,
) -> Result<Vec<Vec<u8>>> {
    input.seek(SeekFrom::Start(0))?;
    let mut reader = ArchiveReader::new(input)?;
    let mut selected = std::collections::BTreeMap::<(u64, u64), Vec<u8>>::new();
    let mut selected_bytes = 0_usize;
    let mut ordinal = 0_u64;
    while let Some(mut frame) = reader.next_frame()? {
        while let Some(record) = frame.next_record()? {
            let bytes = encode_record_wire(&record)?;
            if bytes.len() <= target_bytes {
                let key = (xxhash_rust::xxh3::xxh3_64(&bytes), ordinal);
                let eligible = selected_bytes < target_bytes
                    || selected
                        .last_key_value()
                        .is_some_and(|(last, _)| key < *last);
                if eligible {
                    selected_bytes = selected_bytes
                        .checked_add(bytes.len())
                        .ok_or(ArchiveError::FieldTooLarge)?;
                    selected.insert(key, bytes);
                    while selected_bytes > target_bytes {
                        let (_, removed) = selected
                            .pop_last()
                            .ok_or(ArchiveError::Invalid("empty sample set"))?;
                        selected_bytes -= removed.len();
                    }
                }
            }
            ordinal = ordinal
                .checked_add(1)
                .ok_or(ArchiveError::FieldTooLarge)?;
        }
    }
    if !reader.is_complete() {
        return Err(ArchiveError::Invalid(
            "archive has no clean completion marker",
        ));
    }
    Ok(selected.into_values().collect())
}

pub(crate) fn distill_ref_prefix(
    samples: &[Vec<u8>],
    capacity: usize,
    compression_level: i32,
) -> Result<Vec<u8>> {
    const HEADER_ALLOWANCE: usize = 128 << 10;
    let trained_capacity = capacity
        .checked_add(HEADER_ALLOWANCE)
        .ok_or(ArchiveError::FieldTooLarge)?;
    let sample_sizes = samples.iter().map(Vec::len).collect::<Vec<_>>();
    let sample_bytes = samples.iter().try_fold(Vec::new(), |mut output, sample| {
        output
            .try_reserve(sample.len())
            .map_err(|_| ArchiveError::FieldTooLarge)?;
        output.extend_from_slice(sample);
        Ok::<_, ArchiveError>(output)
    })?;
    let mut trained = vec![0_u8; trained_capacity];
    let mut parameters = zstd::zstd_safe::zstd_sys::ZDICT_fastCover_params_t {
        k: 0,
        d: 8,
        f: 20,
        steps: 4,
        nbThreads: std::thread::available_parallelism()
            .map(|threads| threads.get())
            .unwrap_or(1)
            .try_into()
            .map_err(|_| ArchiveError::FieldTooLarge)?,
        splitPoint: 0.0,
        accel: 1,
        shrinkDict: 0,
        shrinkDictMaxRegression: 0,
        zParams: zstd::zstd_safe::zstd_sys::ZDICT_params_t {
            compressionLevel: compression_level,
            notificationLevel: 0,
            dictID: 0,
        },
    };
    // SAFETY: all buffers and the size array remain live and immutable for
    // the call, and the output pointer covers `trained.len()` writable bytes.
    let trained_bytes = unsafe {
        zstd::zstd_safe::zstd_sys::ZDICT_optimizeTrainFromBuffer_fastCover(
            trained.as_mut_ptr().cast(),
            trained.len(),
            sample_bytes.as_ptr().cast(),
            sample_sizes.as_ptr(),
            sample_sizes
                .len()
                .try_into()
                .map_err(|_| ArchiveError::FieldTooLarge)?,
            &mut parameters,
        )
    };
    if unsafe { zstd::zstd_safe::zstd_sys::ZDICT_isError(trained_bytes) } != 0 {
        return Err(ArchiveError::Invalid(
            "zstd could not distill a reference prefix",
        ));
    }
    trained.truncate(trained_bytes);
    // SAFETY: `trained` is the complete successful result from zstd's trainer.
    let header_bytes = unsafe {
        zstd::zstd_safe::zstd_sys::ZDICT_getDictHeaderSize(
            trained.as_ptr().cast(),
            trained.len(),
        )
    };
    if unsafe { zstd::zstd_safe::zstd_sys::ZDICT_isError(header_bytes) } != 0 {
        return Err(ArchiveError::Invalid(
            "zstd produced an invalid trained dictionary",
        ));
    }
    let content = trained
        .get(header_bytes..)
        .ok_or(ArchiveError::Invalid("invalid trained dictionary header"))?;
    if content.len() >= capacity {
        return Ok(content[content.len() - capacity..].to_vec());
    }
    let filler_bytes = capacity - content.len();
    let mut prefix = Vec::with_capacity(capacity);
    for sample in samples {
        let remaining = filler_bytes - prefix.len();
        if remaining == 0 {
            break;
        }
        prefix.extend_from_slice(&sample[..sample.len().min(remaining)]);
    }
    if prefix.len() != filler_bytes {
        return Err(ArchiveError::Invalid(
            "not enough sample content to fill reference prefix",
        ));
    }
    prefix.extend_from_slice(content);
    Ok(prefix)
}

struct NewestRevisionSamples {
    target_bytes: usize,
    samples: Vec<Vec<u8>>,
    sample_bytes: usize,
    page_id: Option<u64>,
    newest: Option<RevisionRecord>,
}

impl NewestRevisionSamples {
    fn new(target_bytes: usize) -> Result<Self> {
        if target_bytes == 0 {
            return Err(ArchiveError::Invalid("zero reference-prefix sample capacity"));
        }
        Ok(Self {
            target_bytes,
            samples: Vec::new(),
            sample_bytes: 0,
            page_id: None,
            newest: None,
        })
    }

    fn observe(&mut self, record: &Record) -> Result<()> {
        let page_id = record.page_id();
        if page_id != self.page_id {
            self.finish_page()?;
            self.page_id = page_id;
        }
        let Record::Revision { revision, .. } = record else {
            return Ok(());
        };
        if !revision.has_text {
            return Ok(());
        }
        let replace = self.newest.as_ref().map_or(true, |current| {
            revision.meta.ts > current.meta.ts
                || (revision.meta.ts == current.meta.ts
                    && revision.meta.rev_id > current.meta.rev_id)
        });
        if replace {
            self.newest = Some(revision.clone());
        }
        Ok(())
    }

    fn finish_page(&mut self) -> Result<()> {
        let (Some(page_id), Some(revision)) = (self.page_id, self.newest.take()) else {
            return Ok(());
        };
        let sample = encode_record_wire(&Record::Revision { page_id, revision })?;
        if sample.len() <= self.target_bytes {
            self.sample_bytes = self
                .sample_bytes
                .checked_add(sample.len())
                .ok_or(ArchiveError::FieldTooLarge)?;
            self.samples.push(sample);
        }
        Ok(())
    }

    fn ready(&self) -> bool {
        self.sample_bytes >= self.target_bytes
    }

    fn finish(mut self) -> Result<(Vec<Vec<u8>>, usize)> {
        self.finish_page()?;
        Ok((self.samples, self.sample_bytes))
    }
}

fn open_receipted_archive_source(
    path: &Path,
) -> Result<(
    ArchiveRecordSource,
    u64,
    Option<CompressionReference>,
    Option<u32>,
)> {
    let reference_path = if path.is_dir() {
        path.join("0000-reference.swdump-part")
    } else {
        path.to_path_buf()
    };
    let mut input = BufReader::new(std::fs::File::open(&reference_path)?);
    let _ = read_file_header(&mut input)?;
    let header = read_frame_header_or_eof(&mut input)?;
    let (reference, dictionary_id) = match header {
        Some(header) => {
            if let Some(info) = parse_dictionary_header(&header)? {
                (
                    Some(CompressionReference::Dictionary(
                        std::sync::Arc::<[u8]>::from(read_dictionary_payload(
                            &mut input,
                            info,
                        )?),
                    )),
                    Some(info.id),
                )
            } else if let Some(info) = parse_ref_prefix_header(&header)? {
                (
                    Some(CompressionReference::RefPrefix(
                        std::sync::Arc::<[u8]>::from(read_ref_prefix_payload(
                            &mut input,
                            info,
                        )?),
                    )),
                    None,
                )
            } else {
                let _ = parse_frame_header(&header)?.ok_or(ArchiveError::Invalid(
                    "archive begins with an unknown compression-reference header",
                ))?;
                (None, None)
            }
        }
        None => (None, None),
    };
    let (source, source_bytes) = if path.is_dir() {
        let set = crate::archive_set::ArchiveSetReader::open(path)?;
        let source_bytes = set
            .segments()
            .last()
            .map_or(0, |segment| segment.virtual_start.saturating_add(segment.bytes));
        let segments = set
            .segments()
            .iter()
            .map(|segment| (segment.virtual_start, path.join(&segment.name)))
            .collect();
        (ArchiveRecordSource::set(segments), source_bytes)
    } else {
        let bytes = std::fs::metadata(path)?.len();
        (ArchiveRecordSource::file(path.to_path_buf()), bytes)
    };
    Ok((source, source_bytes, reference, dictionary_id))
}

impl ArchiveRecordReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (frames, complete, source) = if path.is_dir() {
            let set = crate::archive_set::ArchiveSetReader::open(&path)?;
            let segments = set.segments().to_vec();
            let (_, frames, complete) = index_reader(set)?;
            let paths = segments
                .iter()
                .map(|segment| (segment.virtual_start, path.join(&segment.name)))
                .collect();
            (frames, complete, ArchiveRecordSource::set(paths))
        } else {
            let file = std::fs::File::open(&path)?;
            let (_, frames, complete) = index_open_file(&file)?;
            (frames, complete, ArchiveRecordSource::file(path))
        };
        if !complete {
            return Err(ArchiveError::Invalid(
                "archive has no clean completion marker",
            ));
        }
        Ok(Self {
            source,
            frames: ArchiveFrameSequence::Owned(frames.into_iter()),
            current: None,
            current_frame_offset: None,
            completed_compressed_bytes: None,
        })
    }

    pub(crate) fn open_accounted(
        path: impl AsRef<Path>,
        completed_compressed_bytes: Arc<AtomicU64>,
    ) -> Result<Self> {
        let mut reader = Self::open(path)?;
        reader.completed_compressed_bytes = Some(completed_compressed_bytes);
        Ok(reader)
    }

    /// Open a mmap-backed, structurally validated frame suffix.
    ///
    /// The directory is retained by `Arc`; no frame metadata is copied into a
    /// `Vec`. Cost is one archive-reference read and one scalar comparison
    /// against the directory's already validated uniform dictionary ID.
    pub(crate) fn open_frame_directory(
        path: impl AsRef<Path>,
        directory: Arc<crate::frame_directory::FrameDirectory>,
        start_position: usize,
    ) -> Result<Self> {
        if start_position > directory.len() {
            return Err(ArchiveError::Invalid(
                "frame directory cursor is out of bounds",
            ));
        }
        let path = path.as_ref().to_path_buf();
        let (source, source_bytes, reference, dictionary_id) =
            open_receipted_archive_source(&path)?;
        directory.require_archive_bounds(source_bytes)?;
        if directory.summary().dictionary_id != dictionary_id {
            return Err(ArchiveError::Invalid(
                "frame directory dictionary ID disagrees with its archive",
            ));
        }
        Ok(Self {
            source,
            frames: ArchiveFrameSequence::Directory {
                directory,
                position: start_position,
                reference,
            },
            current: None,
            current_frame_offset: None,
            completed_compressed_bytes: None,
        })
    }

    pub(crate) fn open_frame_directory_accounted(
        path: impl AsRef<Path>,
        directory: Arc<crate::frame_directory::FrameDirectory>,
        start_position: usize,
        completed_compressed_bytes: Arc<AtomicU64>,
    ) -> Result<Self> {
        let mut reader = Self::open_frame_directory(path, directory, start_position)?;
        reader.completed_compressed_bytes = Some(completed_compressed_bytes);
        Ok(reader)
    }

    pub(crate) fn remaining_frame_count(&self) -> usize {
        self.frames.remaining_count()
    }

    pub(crate) fn current_frame_offset(&self) -> Option<u64> {
        self.current_frame_offset
    }

    pub(crate) fn current_frame_records_read(&self) -> u64 {
        self.current
            .as_ref()
            .map_or(0, |frame| frame.records_read)
    }

    pub fn next_record(&mut self) -> Result<Option<Record>> {
        loop {
            if self.current.is_some() {
                if let Some(record) = self
                    .current
                    .as_mut()
                    .expect("checked current frame")
                    .next_record()?
                {
                    return Ok(Some(record));
                }
                let frame = self.current.take().expect("checked current frame");
                if let Some(completed) = &self.completed_compressed_bytes {
                    completed.fetch_add(frame.info.compressed_bytes, Ordering::Relaxed);
                }
                return_owned_frame_input(&mut self.source, frame);
                self.current_frame_offset = None;
            }
            let Some(location) = self.frames.next_location()? else {
                return Ok(None);
            };
            self.current_frame_offset = Some(location.compressed_offset);
            self.current = Some(open_owned_frame(&mut self.source, &location)?);
        }
    }
}

impl RecordSource for ArchiveRecordReader {
    fn next_record(&mut self) -> Result<Option<Record>> {
        ArchiveRecordReader::next_record(self)
    }
}

fn open_owned_frame(
    source: &mut ArchiveRecordSource,
    location: &FrameLocation,
) -> Result<ArchiveFrameReader<OwnedFrameDecoder>> {
    let (input, offset) = match source {
        ArchiveRecordSource::File { path, input } => (
            input
                .take()
                .map(Ok)
                .unwrap_or_else(|| std::fs::File::open(path).map(|file| Box::new(file) as OwnedInput))?,
            location.compressed_offset,
        ),
        ArchiveRecordSource::Set {
            segments,
            active_segment,
            input,
        } => {
            let position = segments
                .partition_point(|(virtual_start, _)| {
                    *virtual_start <= location.compressed_offset
                })
                .checked_sub(1)
                .ok_or(ArchiveError::Invalid(
                    "frame offset precedes archive-set segments",
                ))?;
            if *active_segment != Some(position) {
                *input = None;
                *active_segment = Some(position);
            }
            let input = input
                .take()
                .map(Ok)
                .unwrap_or_else(|| {
                    std::fs::File::open(&segments[position].1)
                        .map(|file| Box::new(file) as OwnedInput)
                })?;
            (input, location.compressed_offset - segments[position].0)
        }
    };
    open_owned_frame_input_at(input, location, offset)
}

fn return_owned_frame_input(
    source: &mut ArchiveRecordSource,
    frame: ArchiveFrameReader<OwnedFrameDecoder>,
) {
    let buffered = frame.decoder.finish();
    let chain = buffered.into_inner();
    let (_, compressed) = chain.into_inner();
    let input = compressed.into_inner();
    match source {
        ArchiveRecordSource::File { input: slot, .. }
        | ArchiveRecordSource::Set { input: slot, .. } => *slot = Some(input),
    }
}

fn open_owned_frame_file(
    file: std::fs::File,
    location: &FrameLocation,
) -> Result<ArchiveFrameReader<OwnedFrameDecoder>> {
    open_owned_frame_input_at(Box::new(file), location, location.compressed_offset)
}

fn open_owned_frame_input_at(
    mut input: OwnedInput,
    location: &FrameLocation,
    offset: u64,
) -> Result<ArchiveFrameReader<OwnedFrameDecoder>> {
    input.seek(SeekFrom::Start(offset))?;
    let compressed = compressed_frame_reader(
        input.take(location.info.compressed_bytes),
        location.info,
    )?;
    let decoder = owned_frame_decoder(
        BufReader::with_capacity(FRAME_READ_AHEAD, compressed),
        location.info,
        location.reference.as_ref(),
    )?;
    Ok(ArchiveFrameReader {
        decoder,
        info: location.info,
        records_read: 0,
        raw_bytes_read: 0,
        last_entity: None,
        last_timestamp: i64::MAX,
        finished: false,
    })
}

pub(crate) fn open_frame_cursor_file(
    file: &std::fs::File,
    location: &FrameLocation,
) -> Result<FrameRecordCursor> {
    Ok(FrameRecordCursor {
        inner: open_owned_frame_file(file.try_clone()?, location)?,
    })
}

pub fn concatenate_archives<W: Write>(
    inputs: &[PathBuf],
    mut output: W,
    frame_target: usize,
) -> Result<(W, u64)> {
    let mut indexed = Vec::with_capacity(inputs.len());
    let mut reference: Option<Option<CompressionReference>> = None;
    for input in inputs {
        let (_, frames, complete) = index_file(input)?;
        if !complete {
            return Err(ArchiveError::Invalid(
                "archive segment has no completion marker",
            ));
        }
        let candidate = frames
            .first()
            .and_then(|location| location.reference.clone());
        if frames
            .iter()
            .any(|location| location.reference != candidate)
        {
            return Err(ArchiveError::Invalid(
                "archive changes compression reference between frames",
            ));
        }
        match reference.as_ref() {
            Some(current) if current != &candidate => {
                return Err(ArchiveError::Invalid(
                    "archive segments use different compression references",
                ));
            }
            None => reference = Some(candidate),
            _ => {}
        }
        indexed.push((input, frames));
    }
    output.write_all(&FILE_MAGIC)?;
    output.write_all(&FILE_VERSION.to_le_bytes())?;
    output.write_all(&0_u32.to_le_bytes())?;
    output.write_all(&(frame_target as u64).to_le_bytes())?;
    match reference.as_ref().and_then(Option::as_ref) {
        Some(CompressionReference::Dictionary(dictionary)) => {
            write_dictionary_frame(
                &mut output,
                dictionary,
                dictionary_id(dictionary)?,
                CompressionSettings::default(),
            )?;
        }
        Some(CompressionReference::RefPrefix(prefix)) => {
            write_ref_prefix_frame(
                &mut output,
                prefix,
                CompressionSettings::default(),
            )?;
        }
        None => {}
    }
    let mut previous = None;
    let mut frame_count = 0_u64;
    for (input, frames) in indexed {
        let mut source = std::fs::File::open(input)?;
        for location in frames {
            if previous.is_some_and(|entity| entity >= location.info.first_entity) {
                return Err(ArchiveError::Invalid(
                    "archive segments overlap or are out of order",
                ));
            }
            write_frame_header(&mut output, location.info)?;
            source.seek(SeekFrom::Start(location.compressed_offset))?;
            io::copy(
                &mut (&mut source).take(location.info.compressed_bytes),
                &mut output,
            )?;
            previous = Some(location.info.last_entity);
            frame_count += 1;
        }
    }
    output.write_all(&DONE_MAGIC)?;
    output.write_all(&[0; FRAME_HEADER_LEN - 4])?;
    output.flush()?;
    Ok((output, frame_count))
}

pub fn merge_archives<W: Write>(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    output: W,
    frame_target: usize,
) -> Result<(W, u64, u64)> {
    merge_many_archives(
        &[left.as_ref().to_path_buf(), right.as_ref().to_path_buf()],
        output,
        frame_target,
    )
}

pub fn merge_many_archives<W: Write>(
    inputs: &[PathBuf],
    output: W,
    frame_target: usize,
) -> Result<(W, u64, u64)> {
    merge_many_archives_with_compression(
        inputs,
        output,
        frame_target,
        CompressionSettings::default(),
    )
}

pub fn merge_many_archives_with_compression<W: Write>(
    inputs: &[PathBuf],
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(W, u64, u64)> {
    // Keep enough descriptor headroom for the output, executable, telemetry,
    // and test harness even when the process soft limit is only 48.
    const PATH_MERGE_FAN_IN: usize = 24;
    if inputs.len() > PATH_MERGE_FAN_IN {
        let scratch_parent = inputs
            .first()
            .and_then(|path| path.parent())
            .unwrap_or_else(|| Path::new("."));
        let scratch = tempfile::tempdir_in(scratch_parent)?;
        let mut stage_inputs = inputs.to_vec();
        let mut stage = 0_usize;
        while stage_inputs.len() > PATH_MERGE_FAN_IN {
            let mut next = Vec::with_capacity(
                stage_inputs.len().div_ceil(PATH_MERGE_FAN_IN),
            );
            for (group, paths) in stage_inputs
                .chunks(PATH_MERGE_FAN_IN)
                .enumerate()
            {
                let path = scratch
                    .path()
                    .join(format!("merge-{stage:04}-{group:08}.swdump"));
                let writer = ArchiveWriter::with_compression(
                    std::fs::File::create(&path)?,
                    frame_target,
                    compression,
                )?;
                merge_sorted_archives(paths, writer)?;
                next.push(path);
            }
            stage_inputs = next;
            stage = stage
                .checked_add(1)
                .ok_or(ArchiveError::FieldTooLarge)?;
        }
        let writer = ArchiveWriter::with_compression(output, frame_target, compression)?;
        return merge_sorted_archives(&stage_inputs, writer);
    }
    let writer = ArchiveWriter::with_compression(output, frame_target, compression)?;
    merge_sorted_archives(inputs, writer)
}

/// Merge already-open sorted sources with ordinary archive compression.
///
/// This is the bounded-fan-in counterpart of `merge_many_archives...` for
/// sources such as `SequentialRecordGroups` that open only their current
/// nonoverlapping page range. It neither discovers nor records source paths.
pub(crate) fn merge_record_sources_with_compression<'a, W: Write>(
    inputs: Vec<Box<dyn RecordSource + 'a>>,
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(W, u64, u64)> {
    let mut merge = SortedArchiveMerge::new(inputs)?;
    let mut writer = ArchiveWriter::with_compression(output, frame_target, compression)?;
    let mut records = 0_u64;
    while let Some(record) = merge.next_record()? {
        writer.write(&record)?;
        records = records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
    let (output, frames) = writer.finish()?;
    Ok((output, frames, records))
}

/// Visit the deterministic coalesced merge of already-open sorted sources
/// without materializing another archive.
pub(crate) fn visit_merged_record_sources<'a, F: FnMut(&Record)>(
    inputs: Vec<Box<dyn RecordSource + 'a>>,
    mut visit: F,
) -> Result<u64> {
    let mut merge = SortedArchiveMerge::new(inputs)?;
    let mut records = 0_u64;
    while let Some(record) = merge.next_record()? {
        visit(&record);
        records = records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
    Ok(records)
}

/// Merge sorted archives while bootstrapping a reference prefix from the
/// newest text-bearing revision of each page.
///
/// Records written before enough complete page samples have been collected
/// are kept in `bootstrap`, then replayed once into the final writer. Thus the
/// amount repacked is bounded by the sample target rather than the archive.
#[allow(clippy::too_many_arguments)]
pub fn merge_many_archives_bootstrapping_ref_prefix<W: Write>(
    inputs: &[PathBuf],
    output: W,
    bootstrap: std::fs::File,
    frame_target: usize,
    compression: CompressionSettings,
    sample_capacity: usize,
    prefix_capacity: usize,
) -> Result<(W, u64, u64, RepackStats)> {
    merge_many_archives_bootstrapping_ref_prefix_observing(
        inputs,
        output,
        bootstrap,
        frame_target,
        compression,
        sample_capacity,
        prefix_capacity,
        |_| {},
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_many_archives_bootstrapping_ref_prefix_observing<
    W: Write,
    F: FnMut(&Record),
>(
    inputs: &[PathBuf],
    output: W,
    bootstrap: std::fs::File,
    frame_target: usize,
    compression: CompressionSettings,
    sample_capacity: usize,
    prefix_capacity: usize,
    observe: F,
) -> Result<(W, u64, u64, RepackStats)> {
    if prefix_capacity == 0 || sample_capacity <= prefix_capacity {
        return Err(ArchiveError::Invalid(
            "reference-prefix samples must be larger than the prefix",
        ));
    }
    let sources = inputs
        .iter()
        .map(|path| {
            ArchiveRecordReader::open(path)
                .map(|reader| Box::new(reader) as Box<dyn RecordSource>)
        })
        .collect::<Result<Vec<_>>>()?;
    merge_record_sources_bootstrapping_ref_prefix_observing(
        sources,
        output,
        bootstrap,
        frame_target,
        compression,
        sample_capacity,
        prefix_capacity,
        observe,
        |_, _, _| {},
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_record_sources_bootstrapping_ref_prefix<'a, W: Write>(
    inputs: Vec<Box<dyn RecordSource + 'a>>,
    output: W,
    bootstrap: std::fs::File,
    frame_target: usize,
    compression: CompressionSettings,
    sample_capacity: usize,
    prefix_capacity: usize,
) -> Result<(W, u64, u64, RepackStats)> {
    merge_record_sources_bootstrapping_ref_prefix_observing(
        inputs,
        output,
        bootstrap,
        frame_target,
        compression,
        sample_capacity,
        prefix_capacity,
        |_| {},
        |_, _, _| {},
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_record_sources_bootstrapping_ref_prefix_observing<
    'a,
    W: Write,
    F: FnMut(&Record),
    P: FnMut(BootstrapMergePhase, u64, u64),
>(
    inputs: Vec<Box<dyn RecordSource + 'a>>,
    output: W,
    bootstrap: std::fs::File,
    frame_target: usize,
    compression: CompressionSettings,
    sample_capacity: usize,
    prefix_capacity: usize,
    observe: F,
    phase_progress: P,
) -> Result<(W, u64, u64, RepackStats)> {
    merge_record_sources_bootstrapping_ref_prefix_observing_after(
        inputs,
        output,
        bootstrap,
        frame_target,
        compression,
        sample_capacity,
        prefix_capacity,
        None,
        observe,
        phase_progress,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_record_sources_bootstrapping_ref_prefix_observing_after<
    'a,
    W: Write,
    F: FnMut(&Record),
    P: FnMut(BootstrapMergePhase, u64, u64),
>(
    inputs: Vec<Box<dyn RecordSource + 'a>>,
    output: W,
    bootstrap: std::fs::File,
    frame_target: usize,
    compression: CompressionSettings,
    sample_capacity: usize,
    prefix_capacity: usize,
    resume_after: Option<EntityKey>,
    mut observe: F,
    mut phase_progress: P,
) -> Result<(W, u64, u64, RepackStats)> {
    if prefix_capacity == 0 || sample_capacity <= prefix_capacity {
        return Err(ArchiveError::Invalid(
            "reference-prefix samples must be larger than the prefix",
        ));
    }
    let mut merge = SortedArchiveMerge::new(inputs)?;
    let mut bootstrap_writer =
        ArchiveWriter::with_compression(bootstrap, frame_target, compression)?;
    let mut sampler = NewestRevisionSamples::new(sample_capacity)?;
    let mut records = 0_u64;
    phase_progress(
        BootstrapMergePhase::Sampling,
        0,
        sample_capacity as u64,
    );

    while !sampler.ready() {
        let Some(record) = merge.next_record()? else {
            break;
        };
        sampler.observe(&record)?;
        observe(&record);
        bootstrap_writer.write(&record)?;
        records = records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
    let (mut bootstrap, bootstrap_frames) = bootstrap_writer.finish()?;
    bootstrap.seek(SeekFrom::Start(0))?;
    let (samples, sample_bytes) = sampler.finish()?;
    if sample_bytes == 0 {
        return Err(ArchiveError::Invalid(
            "archive has no text-bearing revisions for a reference prefix",
        ));
    }
    phase_progress(
        BootstrapMergePhase::Distilling,
        sample_bytes as u64,
        prefix_capacity as u64,
    );
    let prefix = if sample_bytes >= prefix_capacity {
        distill_ref_prefix(&samples, prefix_capacity, compression.level)?
    } else {
        let mut prefix = Vec::with_capacity(sample_bytes);
        for sample in &samples {
            prefix.extend_from_slice(sample);
        }
        prefix
    };
    drop(samples);

    let mut writer = ParallelArchiveWriter::new(
        output,
        frame_target,
        compression,
        &prefix,
        usize::try_from(streaming_compression_workers())
            .unwrap_or(usize::MAX),
    )?;
    let mut bootstrap_reader = ArchiveReader::new(bootstrap)?;
    let mut replayed = 0_u64;
    phase_progress(BootstrapMergePhase::Replaying, replayed, records);
    while let Some(mut frame) = bootstrap_reader.next_frame()? {
        while let Some(record) = frame.next_record()? {
            if resume_after.is_none_or(|boundary| record.entity() > boundary) {
                writer.write(&record)?;
            }
            replayed = replayed
                .checked_add(1)
                .ok_or(ArchiveError::FieldTooLarge)?;
            if replayed % 4096 == 0 {
                phase_progress(BootstrapMergePhase::Replaying, replayed, records);
            }
        }
    }
    phase_progress(BootstrapMergePhase::Replaying, replayed, records);
    if !bootstrap_reader.is_complete() {
        return Err(ArchiveError::Invalid(
            "bootstrap archive has no clean completion marker",
        ));
    }
    drop(bootstrap_reader);
    phase_progress(BootstrapMergePhase::Merging, records, 0);
    while let Some(record) = merge.next_record()? {
        observe(&record);
        if resume_after.is_none_or(|boundary| record.entity() > boundary) {
            writer.write(&record)?;
        }
        records = records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
    let (output, output_frames) = writer.finish()?;
    let stats = RepackStats {
        input_frames: bootstrap_frames,
        output_frames,
        records,
        ref_prefix_bytes: prefix.len() as u64,
        compressed_ref_prefix_bytes: compressed_dictionary_size(&prefix, compression)? as u64,
        sample_bytes: sample_bytes as u64,
        ..RepackStats::default()
    };
    Ok((output, output_frames, records, stats))
}

/// Merge already-sorted archives directly into the final refPrefix-compressed
/// representation. The reference prefix is reused from `reference_archive`;
/// it is immutable compression context, not state derived from the update.
pub fn merge_many_archives_reusing_ref_prefix<W: Write>(
    reference_archive: impl AsRef<Path>,
    inputs: &[PathBuf],
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(W, u64, u64)> {
    let (_, frames, complete) = index_file(reference_archive)?;
    if !complete {
        return Err(ArchiveError::Invalid(
            "reference archive has no clean completion marker",
        ));
    }
    let prefix = frames
        .first()
        .and_then(|frame| frame.reference.as_ref())
        .and_then(|reference| match reference {
            CompressionReference::RefPrefix(prefix) => Some(prefix.clone()),
            CompressionReference::Dictionary(_) => None,
        })
        .ok_or(ArchiveError::Invalid(
            "reference archive has no reference prefix",
        ))?;
    let writer = ArchiveWriter::with_compression_and_ref_prefix(
        output,
        frame_target,
        compression,
        &prefix,
    )?;
    merge_sorted_archives(inputs, writer)
}

pub fn merge_many_archives_reusing_ref_prefix_at_boundaries<W: Write>(
    reference_archive: impl AsRef<Path>,
    inputs: &[PathBuf],
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
    boundaries: Vec<EntityKey>,
) -> Result<(W, u64, u64)> {
    let (_, frames, complete) = index_file(reference_archive)?;
    if !complete {
        return Err(ArchiveError::Invalid(
            "reference archive has no clean completion marker",
        ));
    }
    let prefix = frames
        .first()
        .and_then(|frame| frame.reference.as_ref())
        .and_then(|reference| match reference {
            CompressionReference::RefPrefix(prefix) => Some(prefix.clone()),
            CompressionReference::Dictionary(_) => None,
        })
        .ok_or(ArchiveError::Invalid(
            "reference archive has no reference prefix",
        ))?;
    let mut writer = ArchiveWriter::with_compression_and_ref_prefix(
        output,
        frame_target,
        compression,
        &prefix,
    )?;
    writer.set_range_boundaries(boundaries)?;
    merge_sorted_archives(inputs, writer)
}

pub fn archive_compression_reference_identity(
    archive: impl AsRef<Path>,
) -> Result<crate::generation::CompressionReferenceIdentity> {
    let archive = archive.as_ref();
    let path = if archive.is_dir() {
        archive.join("0000-reference.swdump-part")
    } else {
        archive.to_path_buf()
    };
    let mut input = BufReader::new(std::fs::File::open(path)?);
    let _ = read_file_header(&mut input)?;
    let header = read_frame_header_or_eof(&mut input)?.ok_or(
        ArchiveError::Invalid("archive has no compression-reference frame"),
    )?;
    if let Some(info) = parse_dictionary_header(&header)? {
        return Ok(crate::generation::CompressionReferenceIdentity::Dictionary {
            dictionary_id: info.id,
            raw_bytes: info.raw_bytes,
            compressed_bytes: info.compressed_bytes,
        });
    }
    if let Some(info) = parse_ref_prefix_header(&header)? {
        return Ok(crate::generation::CompressionReferenceIdentity::RefPrefix {
            xxh3_64: info.hash,
            raw_bytes: info.raw_bytes,
            compressed_bytes: info.compressed_bytes,
        });
    }
    Err(ArchiveError::Invalid(
        "archive has no compression-reference frame",
    ))
}

pub(crate) fn archive_ref_prefix_part(
    part: impl AsRef<Path>,
) -> Result<std::sync::Arc<[u8]>> {
    let reader = ArchiveReader::new(std::fs::File::open(part)?)?;
    match reader.reference {
        Some(CompressionReference::RefPrefix(prefix)) => Ok(prefix),
        Some(CompressionReference::Dictionary(_)) => Err(ArchiveError::Invalid(
            "archive reference part contains a dictionary instead of a reference prefix",
        )),
        None => Err(ArchiveError::Invalid(
            "archive reference part has no reference prefix",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_record_sources_reusing_ref_prefix_observing_after<
    'a,
    W: Write,
    F: FnMut(&Record),
>(
    inputs: Vec<Box<dyn RecordSource + 'a>>,
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
    prefix: &[u8],
    resume_after: Option<EntityKey>,
    mut observe: F,
) -> Result<(W, u64, u64, RepackStats)> {
    let mut merge = SortedArchiveMerge::new(inputs)?;
    let mut writer = ParallelArchiveWriter::new(
        output,
        frame_target,
        compression,
        prefix,
        usize::try_from(streaming_compression_workers())
            .unwrap_or(usize::MAX),
    )?;
    let mut records = 0_u64;
    while let Some(record) = merge.next_record()? {
        observe(&record);
        if resume_after.is_none_or(|boundary| record.entity() > boundary) {
            writer.write(&record)?;
        }
        records = records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
    let (output, output_frames) = writer.finish()?;
    Ok((
        output,
        output_frames,
        records,
        RepackStats {
            output_frames,
            records,
            ref_prefix_bytes: prefix.len() as u64,
            ..RepackStats::default()
        },
    ))
}

struct BoundedPendingSource<'a> {
    source: &'a mut dyn RecordSource,
    pending: &'a mut Option<Record>,
    last_entity: EntityKey,
}

impl RecordSource for BoundedPendingSource<'_> {
    fn next_record(&mut self) -> Result<Option<Record>> {
        let record = match self.pending.take() {
            Some(record) => record,
            None => match self.source.next_record()? {
                Some(record) => record,
                None => return Ok(None),
            },
        };
        if record.entity() > self.last_entity {
            *self.pending = Some(record);
            Ok(None)
        } else {
            Ok(Some(record))
        }
    }
}

/// Decode and merge exactly one base frame with sorted update records through
/// that frame's final entity. Records after the frame remain in `pending`.
pub(crate) fn merge_frame_with_sorted_source<W: Write>(
    writer: &mut ParallelArchiveWriter<W>,
    base: &std::fs::File,
    entry: crate::frame_directory::FrameDirectoryEntry,
    prefix: Arc<[u8]>,
    source: &mut dyn RecordSource,
    pending: &mut Option<Record>,
) -> Result<u64> {
    let location = FrameLocation {
        info: entry.frame_info(),
        compressed_offset: entry.compressed_offset,
        reference: Some(CompressionReference::RefPrefix(prefix)),
        physical_segment: None,
    };
    let frame = open_frame_cursor_file(base, &location)?;
    let updates = BoundedPendingSource {
        source,
        pending,
        last_entity: entry.last_entity,
    };
    let mut merge = SortedArchiveMerge::new(vec![
        Box::new(frame) as Box<dyn RecordSource>,
        Box::new(updates) as Box<dyn RecordSource>,
    ])?;
    let mut records = 0_u64;
    while let Some(record) = merge.next_record()? {
        writer.write(&record)?;
        records = records
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
    Ok(records)
}

pub(crate) fn streaming_compression_workers() -> u32 {
    std::env::var("SARUN_WIKIMAK_CPU_BUDGET")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(1, usize::from)
                .try_into()
                .unwrap_or(u32::MAX)
        })
}

fn merge_sorted_archives<'a, W: Write>(
    inputs: &[PathBuf],
    mut writer: ArchiveWriter<'a, W>,
) -> Result<(W, u64, u64)> {
    let mut merge = SortedArchiveMerge::open(inputs)?;
    let mut records = 0_u64;
    while let Some(record) = merge.next_record()? {
        writer.write(&record)?;
        records += 1;
    }
    let (output, frames) = writer.finish()?;
    Ok((output, frames, records))
}

struct SortedArchiveMerge<'a> {
    readers: Vec<Box<dyn RecordSource + 'a>>,
    heads: BinaryHeap<SortRunHead>,
}

impl SortedArchiveMerge<'static> {
    fn open(inputs: &[PathBuf]) -> Result<Self> {
        let readers = inputs
            .iter()
            .map(|path| {
                ArchiveRecordReader::open(path)
                    .map(|reader| Box::new(reader) as Box<dyn RecordSource>)
            })
            .collect::<Result<Vec<_>>>()?;
        Self::new(readers)
    }

    fn open_accounted(
        inputs: &[PathBuf],
        completed_compressed_bytes: &Arc<AtomicU64>,
    ) -> Result<Self> {
        let readers = inputs
            .iter()
            .map(|path| {
                ArchiveRecordReader::open_accounted(
                    path,
                    Arc::clone(completed_compressed_bytes),
                )
                .map(|reader| Box::new(reader) as Box<dyn RecordSource>)
            })
            .collect::<Result<Vec<_>>>()?;
        Self::new(readers)
    }
}

impl<'a> SortedArchiveMerge<'a> {
    fn new(mut readers: Vec<Box<dyn RecordSource + 'a>>) -> Result<Self> {
        if readers.len() > MAX_SORTED_MERGE_FAN_IN {
            return Err(ArchiveError::Invalid(
                "sorted archive merge exceeds its fixed input fan-in",
            ));
        }
        let mut heads = BinaryHeap::new();
        for (source, reader) in readers.iter_mut().enumerate() {
            if let Some(record) = reader.next_record()? {
                heads.push(SortRunHead {
                    run: source,
                    record,
                });
            }
        }
        Ok(Self { readers, heads })
    }

    fn next_record(&mut self) -> Result<Option<Record>> {
        let Some(head) = self.heads.pop() else {
            return Ok(None);
        };
        let mut record = head.record;
        if let Some(next) = self.readers[head.run].next_record()? {
            self.heads.push(SortRunHead {
                run: head.run,
                record: next,
            });
        }
        while self
            .heads
            .peek()
            .is_some_and(|other| records_coalesce(&record, &other.record))
        {
            let other = self.heads.pop().expect("peeked above");
            record = coalesce_records(record, other.record)?;
            if let Some(next) = self.readers[other.run].next_record()? {
                self.heads.push(SortRunHead {
                    run: other.run,
                    record: next,
                });
            }
        }
        Ok(Some(record))
    }
}

pub(crate) const MAX_SORTED_MERGE_FAN_IN: usize = 64;

fn records_coalesce(left: &Record, right: &Record) -> bool {
    match (left, right) {
        (
            Record::Revision {
                page_id: left_page,
                revision: left,
            },
            Record::Revision {
                page_id: right_page,
                revision: right,
            },
        ) => left_page == right_page && left.meta.rev_id == right.meta.rev_id,
        (
            Record::UserState {
                user_id: left_id,
                timestamp_micros: left_timestamp,
                ..
            },
            Record::UserState {
                user_id: right_id,
                timestamp_micros: right_timestamp,
                ..
            },
        ) => left_id == right_id && left_timestamp == right_timestamp,
        _ => record_order(left, right) == std::cmp::Ordering::Equal,
    }
}

fn coalesce_records(left: Record, right: Record) -> Result<Record> {
    match (left, right) {
        (
            Record::Revision {
                page_id,
                revision: mut left,
            },
            Record::Revision {
                revision: right, ..
            },
        ) => {
            if left.meta.rev_id != right.meta.rev_id
                || left.meta.parent_id != right.meta.parent_id
                || left.meta.ts != right.meta.ts
            {
                return Err(ArchiveError::Invalid(
                    "conflicting identity for one revision id",
                ));
            }
            if left.has_text && right.has_text && left.text != right.text {
                return Err(ArchiveError::Invalid(
                    "conflicting text for one revision id",
                ));
            }
            left.meta.contributor = merge_contributor(
                left.meta.rev_id,
                left.meta.contributor,
                right.meta.contributor,
            )?;
            left.meta.comment = merge_comment(left.meta.comment, right.meta.comment);
            left.meta.flags |= right.meta.flags;
            left.meta.text_len = left.meta.text_len.max(right.meta.text_len);
            if !left.has_text && right.has_text {
                left.has_text = true;
                left.text = right.text;
            }
            left.visibility = merge_visibility(left.visibility, right.visibility);
            left.history = merge_revision_history(left.history, right.history)?;
            Ok(Record::Revision {
                page_id,
                revision: left,
            })
        }
        (
            Record::PageAction {
                entity,
                timestamp_micros,
                mut action,
            },
            Record::PageAction { action: other, .. },
        ) if page_actions_same_identity(&action, &other) => {
            action = merge_page_action(action, other)?;
            Ok(Record::PageAction {
                entity,
                timestamp_micros,
                action,
            })
        }
        (
            Record::UserAction {
                entity,
                timestamp_micros,
                mut action,
            },
            Record::UserAction { action: other, .. },
        ) if user_actions_same_identity(&action, &other) => {
            action = merge_user_action(action, other)?;
            Ok(Record::UserAction {
                entity,
                timestamp_micros,
                action,
            })
        }
        (
            Record::UserState {
                user_id,
                timestamp_micros,
                state,
            },
            Record::UserState { state: other, .. },
        ) => Ok(Record::UserState {
            user_id,
            timestamp_micros,
            state: merge_user_state(state, other)?,
        }),
        (left, right) if left == right => Ok(left),
        _ => Err(ArchiveError::Invalid("records cannot be coalesced")),
    }
}

fn merge_contributor(
    revision_id: u64,
    left: ContributorMeta,
    right: ContributorMeta,
) -> Result<ContributorMeta> {
    if left == right {
        Ok(left)
    } else {
        match (left, right) {
            (ContributorMeta::Hidden, right) => Ok(right),
            (left, ContributorMeta::Hidden) => Ok(left),
            (
                ContributorMeta::Named {
                    username: left,
                    user_id: left_id,
                },
                ContributorMeta::Named {
                    username: right,
                    user_id: right_id,
                },
            ) if left_id == right_id => Ok(ContributorMeta::Named {
                username: left.min(right),
                user_id: left_id,
            }),
            (left, right) => Err(ArchiveError::Conflict(format!(
                "revision {revision_id} has contributors {left:?} and {right:?}"
            ))),
        }
    }
}

fn merge_comment(left: String, right: String) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right,
        (_, true) => left,
        _ => left.max(right),
    }
}

fn merge_visibility(
    left: Option<RevisionVisibilityRecord>,
    right: Option<RevisionVisibilityRecord>,
) -> Option<RevisionVisibilityRecord> {
    match (left, right) {
        (None, value) | (value, None) => value,
        (Some(left), Some(right)) => Some(RevisionVisibilityRecord {
            deleted_parts: left.deleted_parts | right.deleted_parts,
            parts_are_suppressed: left.parts_are_suppressed || right.parts_are_suppressed,
            deleted_by_page_deletion: left.deleted_by_page_deletion
                || right.deleted_by_page_deletion,
            page_deletion_timestamp_micros: left
                .page_deletion_timestamp_micros
                .max(right.page_deletion_timestamp_micros),
        }),
    }
}

fn merge_revision_history(
    left: Option<RevisionHistoryRecord>,
    right: Option<RevisionHistoryRecord>,
) -> Result<Option<RevisionHistoryRecord>> {
    let (left, right) = match (left, right) {
        (Some(left), Some(right)) => (left, right),
        (None, value) | (value, None) => return Ok(value),
    };
    let mut tags = left.tags;
    tags.extend(right.tags);
    tags.sort();
    tags.dedup();
    Ok(Some(RevisionHistoryRecord {
        minor: merge_optional_bool(left.minor, right.minor),
        content_model: merge_optional_string(left.content_model, right.content_model)?,
        content_format: merge_optional_string(left.content_format, right.content_format)?,
        identity_reverted: merge_optional_bool(
            left.identity_reverted,
            right.identity_reverted,
        ),
        first_reverting_revision_id: merge_optional_equal(
            left.first_reverting_revision_id,
            right.first_reverting_revision_id,
        )?,
        seconds_to_revert: merge_optional_equal(
            left.seconds_to_revert,
            right.seconds_to_revert,
        )?,
        identity_revert: merge_optional_bool(left.identity_revert, right.identity_revert),
        before_page_creation: merge_optional_bool(
            left.before_page_creation,
            right.before_page_creation,
        ),
        tags,
    }))
}

fn merge_optional_bool(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left || right),
        (None, value) | (value, None) => value,
    }
}

fn merge_optional_string(left: Option<String>, right: Option<String>) -> Result<Option<String>> {
    match (left, right) {
        (Some(left), Some(right)) if left != right => {
            Err(ArchiveError::Invalid("conflicting revision history metadata"))
        }
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn merge_optional_equal<T: Eq>(left: Option<T>, right: Option<T>) -> Result<Option<T>> {
    match (left, right) {
        (Some(left), Some(right)) if left != right => {
            Err(ArchiveError::Invalid("conflicting revision history metadata"))
        }
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn merge_page_action(
    mut left: PageActionRecord,
    right: PageActionRecord,
) -> Result<PageActionRecord> {
    left.tie_sequence = left.tie_sequence.min(right.tie_sequence);
    left.kind = left.kind.min(right.kind);
    left.performer = merge_performer(left.performer, right.performer)?;
    left.comment = merge_comment(left.comment, right.comment);
    left.title_at_event = left.title_at_event.max(right.title_at_event);
    left.namespace_at_event = left.namespace_at_event.max(right.namespace_at_event);
    left.resulting_deleted = merge_optional_bool(
        left.resulting_deleted,
        right.resulting_deleted,
    );
    Ok(left)
}

fn merge_user_action(
    mut left: UserActionRecord,
    right: UserActionRecord,
) -> Result<UserActionRecord> {
    left.tie_sequence = left.tie_sequence.min(right.tie_sequence);
    left.kind = left.kind.min(right.kind);
    left.performer = merge_performer(left.performer, right.performer)?;
    left.comment = merge_comment(left.comment, right.comment);
    left.historical_name = left.historical_name.max(right.historical_name);
    merge_strings(&mut left.groups, right.groups);
    merge_strings(&mut left.blocks, right.blocks);
    merge_strings(&mut left.bot_by, right.bot_by);
    left.created_by |= right.created_by;
    left.registration_timestamp_micros = left
        .registration_timestamp_micros
        .max(right.registration_timestamp_micros);
    left.creation_timestamp_micros = left
        .creation_timestamp_micros
        .max(right.creation_timestamp_micros);
    left.first_edit_timestamp_micros = left
        .first_edit_timestamp_micros
        .max(right.first_edit_timestamp_micros);
    Ok(left)
}

fn merge_user_state(
    mut left: UserStateRecord,
    right: UserStateRecord,
) -> Result<UserStateRecord> {
    left.current_name = left.current_name.max(right.current_name);
    left.central_user_id = merge_optional_identity(
        left.central_user_id,
        right.central_user_id,
        "conflicting central user ids",
    )?;
    left.account_class = left.account_class.max(right.account_class);
    merge_strings(&mut left.groups, right.groups);
    merge_strings(&mut left.blocks, right.blocks);
    merge_strings(&mut left.bot_by, right.bot_by);
    Ok(left)
}

fn merge_performer(
    mut left: PerformerRecord,
    right: PerformerRecord,
) -> Result<PerformerRecord> {
    left.local_user_id = merge_optional_identity(
        left.local_user_id,
        right.local_user_id,
        "local performer id",
    )?;
    left.central_user_id = merge_optional_identity(
        left.central_user_id,
        right.central_user_id,
        "central performer id",
    )?;
    left.historical_name = left.historical_name.max(right.historical_name);
    left.account_class = left.account_class.max(right.account_class);
    Ok(left)
}

fn merge_optional_identity<T: Eq + std::fmt::Debug>(
    left: Option<T>,
    right: Option<T>,
    field: &'static str,
) -> Result<Option<T>> {
    match (left, right) {
        (Some(left), Some(right)) if left == right => Ok(Some(left)),
        (Some(left), Some(right)) => Err(ArchiveError::Conflict(format!(
            "conflicting {field} values {left:?} and {right:?}"
        ))),
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn merge_strings(left: &mut Vec<String>, right: Vec<String>) {
    left.extend(right);
    left.sort();
    left.dedup();
}

fn record_order(left: &Record, right: &Record) -> std::cmp::Ordering {
    left.entity()
        .cmp(&right.entity())
        .then_with(|| right.timestamp_micros().cmp(&left.timestamp_micros()))
        .then_with(|| record_order_rank(left).cmp(&record_order_rank(right)))
        .then_with(|| record_value_order(left, right))
}

fn record_order_rank(record: &Record) -> u8 {
    match record {
        Record::PageState { .. } => 0,
        Record::Revision { .. } => 1,
        Record::PageAction { .. } => 2,
        Record::UserState { .. } => 0,
        Record::UserAction { .. } => 1,
        Record::Manifest { .. } => 0,
        Record::SiteInfo { .. } => 0,
        Record::Unknown { .. } => 255,
    }
}

fn record_value_order(left: &Record, right: &Record) -> std::cmp::Ordering {
    match (left, right) {
        (
            Record::PageState {
                title: left_title,
                namespace: left_namespace,
                deleted: left_deleted,
                ..
            },
            Record::PageState {
                title: right_title,
                namespace: right_namespace,
                deleted: right_deleted,
                ..
            },
        ) => (left_deleted, left_namespace, left_title).cmp(&(
            right_deleted,
            right_namespace,
            right_title,
        )),
        (
            Record::Revision { revision: left, .. },
            Record::Revision {
                revision: right, ..
            },
        ) => right.meta.rev_id.cmp(&left.meta.rev_id),
        (
            Record::PageAction { action: left, .. },
            Record::PageAction { action: right, .. },
        ) => compare_page_actions(left, right),
        (
            Record::UserState { state: left, .. },
            Record::UserState { state: right, .. },
        ) => left.cmp(right),
        (
            Record::UserAction { action: left, .. },
            Record::UserAction { action: right, .. },
        ) => compare_user_actions(left, right),
        (
            Record::Manifest { manifest: left, .. },
            Record::Manifest {
                manifest: right, ..
            },
        ) => left.cmp(right),
        (
            Record::SiteInfo {
                site_info: left, ..
            },
            Record::SiteInfo {
                site_info: right, ..
            },
        ) => left.cmp(right),
        (
            Record::Unknown {
                kind: left_kind,
                payload: left_payload,
                ..
            },
            Record::Unknown {
                kind: right_kind,
                payload: right_payload,
                ..
            },
        ) => (left_kind, left_payload).cmp(&(right_kind, right_payload)),
        _ => std::cmp::Ordering::Equal,
    }
}

fn page_action_content(
    action: &PageActionRecord,
) -> (
    &PageActionKind,
    Option<u64>,
    Option<u64>,
    Option<i64>,
) {
    (
        &action.kind,
        action.performer.local_user_id,
        action.performer.central_user_id,
        action.namespace_at_event,
    )
}

fn user_action_content(
    action: &UserActionRecord,
) -> (
    &UserActionKind,
    Option<u64>,
    Option<u64>,
    u8,
) {
    (
        &action.kind,
        action.performer.local_user_id,
        action.performer.central_user_id,
        action.created_by,
    )
}

fn compare_page_actions(
    left: &PageActionRecord,
    right: &PageActionRecord,
) -> std::cmp::Ordering {
    match (left.log_id, right.log_id) {
        (Some(left_id), Some(right_id)) => {
            (left_id, &left.kind).cmp(&(right_id, &right.kind))
        }
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => page_action_content(left).cmp(&page_action_content(right)),
    }
}

fn compare_user_actions(
    left: &UserActionRecord,
    right: &UserActionRecord,
) -> std::cmp::Ordering {
    match (left.log_id, right.log_id) {
        (Some(left_id), Some(right_id)) => {
            (left_id, &left.kind).cmp(&(right_id, &right.kind))
        }
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => user_action_content(left).cmp(&user_action_content(right)),
    }
}

fn page_actions_same_identity(left: &PageActionRecord, right: &PageActionRecord) -> bool {
    match (left.log_id, right.log_id) {
        (Some(left_id), Some(right_id)) => left_id == right_id && left.kind == right.kind,
        (None, None) => page_action_content(left) == page_action_content(right),
        _ => false,
    }
}

fn user_actions_same_identity(left: &UserActionRecord, right: &UserActionRecord) -> bool {
    match (left.log_id, right.log_id) {
        (Some(left_id), Some(right_id)) => left_id == right_id && left.kind == right.kind,
        (None, None) => user_action_content(left) == user_action_content(right),
        _ => false,
    }
}

fn write_frame_header(output: &mut impl Write, info: FrameInfo) -> Result<()> {
    output.write_all(&FRAME_MAGIC)?;
    output.write_all(&(FRAME_HEADER_LEN as u32).to_le_bytes())?;
    output.write_all(&[info.first_entity.kind as u8])?;
    output.write_all(&[info.last_entity.kind as u8])?;
    output.write_all(&[0; 6])?;
    output.write_all(&info.first_entity.id.to_le_bytes())?;
    output.write_all(&info.last_entity.id.to_le_bytes())?;
    output.write_all(&info.records.to_le_bytes())?;
    output.write_all(&info.raw_bytes.to_le_bytes())?;
    output.write_all(&info.compressed_bytes.to_le_bytes())?;
    output.write_all(&info.dictionary_id.unwrap_or(0).to_le_bytes())?;
    output.write_all(&[0; 4])?;
    Ok(())
}

#[derive(Clone, Copy)]
struct DictionaryFrameInfo {
    id: u32,
    raw_bytes: u64,
    compressed_bytes: u64,
}

#[derive(Clone, Copy)]
struct RefPrefixFrameInfo {
    hash: u64,
    raw_bytes: u64,
    compressed_bytes: u64,
}

fn dictionary_id(dictionary: &[u8]) -> Result<u32> {
    zstd::zstd_safe::get_dict_id_from_dict(dictionary)
        .map(u32::from)
        .ok_or(ArchiveError::Invalid("trained dictionary has no id"))
}

fn configure_encoder<W: Write>(
    mut encoder: zstd::stream::write::Encoder<'static, W>,
    settings: CompressionSettings,
) -> Result<zstd::stream::write::Encoder<'static, W>> {
    encoder.include_checksum(settings.checksum)?;
    encoder.long_distance_matching(settings.long_distance_matching)?;
    if let Some(window_log) = settings.window_log {
        encoder.window_log(window_log)?;
    }
    encoder.set_target_cblock_size(settings.target_block_size)?;
    Ok(encoder)
}

fn compress_dictionary(
    dictionary: &[u8],
    settings: CompressionSettings,
) -> Result<Vec<u8>> {
    let encoder = zstd::stream::write::Encoder::new(Vec::new(), settings.level)?;
    let mut encoder = configure_encoder(encoder, settings)?;
    encoder.write_all(dictionary)?;
    Ok(encoder.finish()?)
}

fn compressed_dictionary_size(
    dictionary: &[u8],
    settings: CompressionSettings,
) -> Result<usize> {
    Ok(compress_dictionary(dictionary, settings)?.len())
}

fn write_dictionary_frame(
    output: &mut impl Write,
    dictionary: &[u8],
    id: u32,
    settings: CompressionSettings,
) -> Result<()> {
    let compressed = compress_dictionary(dictionary, settings)?;
    output.write_all(&DICTIONARY_MAGIC)?;
    output.write_all(&(FRAME_HEADER_LEN as u32).to_le_bytes())?;
    output.write_all(&id.to_le_bytes())?;
    output.write_all(&[0; 4])?;
    output.write_all(&(dictionary.len() as u64).to_le_bytes())?;
    output.write_all(&(compressed.len() as u64).to_le_bytes())?;
    output.write_all(&[0; 32])?;
    output.write_all(&compressed)?;
    Ok(())
}

fn write_ref_prefix_frame(
    output: &mut impl Write,
    prefix: &[u8],
    settings: CompressionSettings,
) -> Result<()> {
    let compressed = compress_dictionary(prefix, settings)?;
    output.write_all(&REF_PREFIX_MAGIC)?;
    output.write_all(&(FRAME_HEADER_LEN as u32).to_le_bytes())?;
    output.write_all(&xxhash_rust::xxh3::xxh3_64(prefix).to_le_bytes())?;
    output.write_all(&(prefix.len() as u64).to_le_bytes())?;
    output.write_all(&(compressed.len() as u64).to_le_bytes())?;
    output.write_all(&[0; 32])?;
    output.write_all(&compressed)?;
    Ok(())
}

fn read_frame_header_or_eof(
    input: &mut impl Read,
) -> Result<Option<[u8; FRAME_HEADER_LEN]>> {
    let mut header = [0_u8; FRAME_HEADER_LEN];
    let mut filled = 0;
    while filled < header.len() {
        match input.read(&mut header[filled..])? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(ArchiveError::Invalid("truncated frame header")),
            count => filled += count,
        }
    }
    Ok(Some(header))
}

fn parse_dictionary_header(
    header: &[u8; FRAME_HEADER_LEN],
) -> Result<Option<DictionaryFrameInfo>> {
    if header[..4] != DICTIONARY_MAGIC {
        return Ok(None);
    }
    if u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize != FRAME_HEADER_LEN {
        return Err(ArchiveError::Invalid("unsupported dictionary frame header"));
    }
    let info = DictionaryFrameInfo {
        id: u32::from_le_bytes(header[8..12].try_into().unwrap()),
        raw_bytes: u64::from_le_bytes(header[16..24].try_into().unwrap()),
        compressed_bytes: u64::from_le_bytes(header[24..32].try_into().unwrap()),
    };
    if info.id == 0
        || info.raw_bytes == 0
        || info.compressed_bytes == 0
        || header[12..16].iter().any(|byte| *byte != 0)
        || header[32..].iter().any(|byte| *byte != 0)
    {
        return Err(ArchiveError::Invalid("malformed dictionary frame"));
    }
    Ok(Some(info))
}

fn parse_ref_prefix_header(
    header: &[u8; FRAME_HEADER_LEN],
) -> Result<Option<RefPrefixFrameInfo>> {
    if header[..4] != REF_PREFIX_MAGIC {
        return Ok(None);
    }
    if u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize != FRAME_HEADER_LEN {
        return Err(ArchiveError::Invalid(
            "unsupported reference-prefix frame header",
        ));
    }
    let info = RefPrefixFrameInfo {
        hash: u64::from_le_bytes(header[8..16].try_into().unwrap()),
        raw_bytes: u64::from_le_bytes(header[16..24].try_into().unwrap()),
        compressed_bytes: u64::from_le_bytes(header[24..32].try_into().unwrap()),
    };
    if info.raw_bytes == 0
        || info.compressed_bytes == 0
        || header[32..].iter().any(|byte| *byte != 0)
    {
        return Err(ArchiveError::Invalid(
            "malformed reference-prefix frame",
        ));
    }
    Ok(Some(info))
}

fn read_dictionary_payload(
    input: &mut impl Read,
    info: DictionaryFrameInfo,
) -> Result<Vec<u8>> {
    let mut decoder =
        zstd::stream::read::Decoder::new(input.take(info.compressed_bytes))?.single_frame();
    let mut dictionary = Vec::with_capacity(
        usize::try_from(info.raw_bytes).map_err(|_| ArchiveError::FieldTooLarge)?,
    );
    decoder.read_to_end(&mut dictionary)?;
    if dictionary.len() as u64 != info.raw_bytes {
        return Err(ArchiveError::Invalid("dictionary frame size mismatch"));
    }
    if dictionary_id(&dictionary)? != info.id {
        return Err(ArchiveError::Invalid("dictionary frame id mismatch"));
    }
    Ok(dictionary)
}

fn read_ref_prefix_payload(
    input: &mut impl Read,
    info: RefPrefixFrameInfo,
) -> Result<Vec<u8>> {
    let mut decoder =
        zstd::stream::read::Decoder::new(input.take(info.compressed_bytes))?.single_frame();
    let mut prefix = Vec::with_capacity(
        usize::try_from(info.raw_bytes).map_err(|_| ArchiveError::FieldTooLarge)?,
    );
    decoder.read_to_end(&mut prefix)?;
    if prefix.len() as u64 != info.raw_bytes {
        return Err(ArchiveError::Invalid(
            "reference-prefix frame size mismatch",
        ));
    }
    if xxhash_rust::xxh3::xxh3_64(&prefix) != info.hash {
        return Err(ArchiveError::Invalid(
            "reference-prefix frame hash mismatch",
        ));
    }
    Ok(prefix)
}

fn validate_frame_dictionary(info: FrameInfo, active: Option<u32>) -> Result<()> {
    if info.dictionary_id.is_some() && info.dictionary_id != active {
        return Err(ArchiveError::Invalid(
            "data frame references unavailable dictionary",
        ));
    }
    Ok(())
}

fn compressed_frame_reader<R: Read>(
    mut compressed: Take<R>,
    info: FrameInfo,
) -> Result<std::io::Chain<Cursor<Vec<u8>>, Take<R>>> {
    let prefix_len = usize::try_from(info.compressed_bytes.min(18))
        .map_err(|_| ArchiveError::FieldTooLarge)?;
    if prefix_len == 0 {
        return Err(ArchiveError::Invalid("empty compressed data frame"));
    }
    let mut prefix = vec![0; prefix_len];
    compressed.read_exact(&mut prefix)?;
    let native = zstd::zstd_safe::get_dict_id_from_frame(&prefix).map(u32::from);
    if native != info.dictionary_id {
        return Err(ArchiveError::Invalid(
            "data frame dictionary header does not match zstd frame",
        ));
    }
    Ok(Cursor::new(prefix).chain(compressed))
}

fn frame_decoder<'a, R: std::io::BufRead>(
    input: R,
    info: FrameInfo,
    reference: Option<&'a CompressionReference>,
) -> Result<zstd::stream::read::Decoder<'a, R>> {
    let mut decoder = match reference {
        Some(CompressionReference::Dictionary(dictionary)) => {
            if info.dictionary_id.is_none() {
                return Err(ArchiveError::Invalid(
                    "data frame does not reference the archive dictionary",
                ));
            }
            zstd::stream::read::Decoder::with_dictionary(input, dictionary)?
        }
        Some(CompressionReference::RefPrefix(prefix)) => {
            if info.dictionary_id.is_some() {
                return Err(ArchiveError::Invalid(
                    "reference-prefix frame has a dictionary id",
                ));
            }
            zstd::stream::read::Decoder::with_ref_prefix(input, prefix)?
        }
        None => {
            if info.dictionary_id.is_some() {
                return Err(ArchiveError::Invalid(
                    "data frame references unavailable dictionary",
                ));
            }
            zstd::stream::read::Decoder::with_dictionary(input, &[])?
        }
    };
    if let Some(CompressionReference::RefPrefix(prefix)) = reference {
        decoder.window_log_max(ref_prefix_window_log(prefix.len()))?;
    }
    Ok(decoder.single_frame())
}

fn owned_frame_decoder<R: std::io::BufRead>(
    input: R,
    info: FrameInfo,
    reference: Option<&CompressionReference>,
) -> Result<zstd::stream::read::Decoder<'static, R>> {
    let dictionary = match reference {
        Some(CompressionReference::Dictionary(dictionary)) => {
            if info.dictionary_id.is_none() {
                return Err(ArchiveError::Invalid(
                    "data frame does not reference the archive dictionary",
                ));
            }
            dictionary.as_ref()
        }
        Some(CompressionReference::RefPrefix(prefix)) => {
            if info.dictionary_id.is_some() {
                return Err(ArchiveError::Invalid(
                    "reference-prefix frame has a dictionary id",
                ));
            }
            prefix.as_ref()
        }
        None => {
            if info.dictionary_id.is_some() {
                return Err(ArchiveError::Invalid(
                    "data frame references unavailable dictionary",
                ));
            }
            &[]
        }
    };
    let mut decoder = zstd::stream::read::Decoder::with_dictionary(input, dictionary)?;
    if let Some(CompressionReference::RefPrefix(prefix)) = reference {
        decoder.window_log_max(ref_prefix_window_log(prefix.len()))?;
    }
    Ok(decoder.single_frame())
}

fn ref_prefix_window_log(prefix_bytes: usize) -> u32 {
    // zstd rejects windowLog values below its format minimum even when the
    // reference itself is smaller (notably for tiny test and private wikis).
    (usize::BITS - prefix_bytes.leading_zeros()).max(10)
}

fn read_file_header(input: &mut impl Read) -> Result<u64> {
    let mut header = [0_u8; FILE_HEADER_LEN];
    input.read_exact(&mut header)?;
    if header[..8] != FILE_MAGIC {
        return Err(ArchiveError::Invalid("bad file magic"));
    }
    if u32::from_le_bytes(header[8..12].try_into().unwrap()) != FILE_VERSION {
        return Err(ArchiveError::Invalid("unsupported file version"));
    }
    if u32::from_le_bytes(header[12..16].try_into().unwrap()) != 0 {
        return Err(ArchiveError::Invalid("unknown file flags"));
    }
    Ok(u64::from_le_bytes(header[16..24].try_into().unwrap()))
}

fn parse_frame_header(header: &[u8; FRAME_HEADER_LEN]) -> Result<Option<FrameInfo>> {
    if header[..4] == DONE_MAGIC {
        if header[4..].iter().any(|byte| *byte != 0) {
            return Err(ArchiveError::Invalid("malformed completion marker"));
        }
        return Ok(None);
    }
    if header[..4] != FRAME_MAGIC {
        return Err(ArchiveError::Invalid("bad frame magic"));
    }
    if u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize != FRAME_HEADER_LEN {
        return Err(ArchiveError::Invalid("unsupported frame header"));
    }
    let info = FrameInfo {
        first_entity: EntityKey {
            kind: EntityKind::try_from(header[8])?,
            id: u64::from_le_bytes(header[16..24].try_into().unwrap()),
        },
        last_entity: EntityKey {
            kind: EntityKind::try_from(header[9])?,
            id: u64::from_le_bytes(header[24..32].try_into().unwrap()),
        },
        records: u64::from_le_bytes(header[32..40].try_into().unwrap()),
        raw_bytes: u64::from_le_bytes(header[40..48].try_into().unwrap()),
        compressed_bytes: u64::from_le_bytes(header[48..56].try_into().unwrap()),
        dictionary_id: match u32::from_le_bytes(header[56..60].try_into().unwrap()) {
            0 => None,
            id => Some(id),
        },
    };
    if header[10..16].iter().any(|byte| *byte != 0)
        || header[60..64].iter().any(|byte| *byte != 0)
    {
        return Err(ArchiveError::Invalid("unknown frame flags"));
    }
    if info.first_entity > info.last_entity {
        return Err(ArchiveError::Invalid("reversed frame entity range"));
    }
    if info.first_entity.kind != info.last_entity.kind {
        return Err(ArchiveError::Invalid("frame mixes entity kinds"));
    }
    Ok(Some(info))
}

impl<D: Read> ArchiveFrameReader<D> {
    pub fn info(&self) -> FrameInfo {
        self.info
    }

    pub fn next_record(&mut self) -> Result<Option<Record>> {
        self.next_record_matching(|_, _| true)
    }

    fn next_title_record(&mut self) -> Result<Option<Record>> {
        self.next_record_matching(|entity, kind| {
            (entity.kind == EntityKind::Page
                && matches!(kind, KIND_PAGE_STATE | KIND_PAGE_ACTION))
                || (entity.kind == EntityKind::Global
                    && entity.id == 1
                    && kind == KIND_SITE_INFO)
        })
    }

    fn next_record_matching(
        &mut self,
        include: impl Fn(EntityKey, u8) -> bool,
    ) -> Result<Option<Record>> {
        loop {
            if self.records_read == self.info.records {
                self.finish_frame()?;
                return Ok(None);
            }
            let entity_kind = EntityKind::try_from(read_u8(&mut self.decoder)?)?;
            let (entity_id, id_bytes) = read_varint(&mut self.decoder)?;
            let entity = EntityKey {
                kind: entity_kind,
                id: entity_id,
            };
            let timestamp = read_i64(&mut self.decoder)?;
            let kind = read_u8(&mut self.decoder)?;
            let (payload_len, payload_len_bytes) = read_varint(&mut self.decoder)?;
            let payload_len: usize = payload_len
                .try_into()
                .map_err(|_| ArchiveError::FieldTooLarge)?;
            let record = if include(entity, kind) {
                let mut payload =
                    (&mut self.decoder).take(payload_len as u64);
                let record = decode_record_from_reader(
                    entity,
                    timestamp,
                    kind,
                    &mut payload,
                )?;
                if payload.limit() != 0 {
                    return Err(ArchiveError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated record payload",
                    )));
                }
                Some(record)
            } else {
                let copied = io::copy(
                    &mut (&mut self.decoder).take(payload_len as u64),
                    &mut io::sink(),
                )?;
                if copied != payload_len as u64 {
                    return Err(ArchiveError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated record payload",
                    )));
                }
                None
            };
            self.raw_bytes_read = self
                .raw_bytes_read
                .checked_add(
                    1 + id_bytes as u64
                        + 8
                        + 1
                        + payload_len_bytes as u64
                        + payload_len as u64,
                )
                .ok_or(ArchiveError::FieldTooLarge)?;
            if let Some(last_entity) = self.last_entity {
                if entity < last_entity
                    || (entity == last_entity && timestamp > self.last_timestamp)
                {
                    return Err(ArchiveError::OutOfOrder {
                        previous: last_entity,
                        previous_timestamp: self.last_timestamp,
                        current: entity,
                        current_timestamp: timestamp,
                    });
                }
            }
            self.last_entity = Some(entity);
            self.last_timestamp = timestamp;
            self.records_read += 1;
            if record.is_some() {
                return Ok(record);
            }
        }
    }

    fn finish_frame(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let mut extra = [0_u8; 1];
        if self.decoder.read(&mut extra)? != 0 {
            return Err(ArchiveError::Invalid("frame has trailing raw bytes"));
        }
        if self.raw_bytes_read != self.info.raw_bytes {
            return Err(ArchiveError::Invalid("frame raw length mismatch"));
        }
        if self.last_entity.is_some_and(|entity| {
            self.info.first_entity > entity || self.info.last_entity != entity
        }) {
            return Err(ArchiveError::Invalid("frame page range mismatch"));
        }
        self.finished = true;
        Ok(())
    }
}

pub(crate) fn visit_title_records_parallel(
    path: impl AsRef<Path>,
    workers: usize,
    mut visitor: impl FnMut(Record),
    mut progress: impl FnMut(usize, usize),
) -> Result<()> {
    let ArchiveRecordReader {
        source,
        frames,
        current: _,
        current_frame_offset: _,
        completed_compressed_bytes: _,
    } = ArchiveRecordReader::open(path)?;
    let frames = match frames {
        ArchiveFrameSequence::Owned(frames) => {
            frames.collect::<std::collections::VecDeque<_>>()
        }
        ArchiveFrameSequence::Directory { .. } => {
            return Err(ArchiveError::Invalid(
                "parallel title scan requires an owned frame inventory",
            ))
        }
    };
    let frame_count = frames.len();
    if frame_count == 0 {
        return Ok(());
    }
    let workers = workers.min(frame_count).max(1);
    let queue = std::sync::Arc::new(std::sync::Mutex::new(frames));
    let (sender, receiver) =
        std::sync::mpsc::sync_channel::<Result<Vec<Record>>>(workers * 2);
    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            let mut source = source.clone();
            let queue = std::sync::Arc::clone(&queue);
            let sender = sender.clone();
            scope.spawn(move || loop {
                let Some(location) = queue.lock().expect("frame queue poisoned").pop_front()
                else {
                    return;
                };
                let result = (|| {
                    let mut frame = open_owned_frame(&mut source, &location)?;
                    let mut records = Vec::new();
                    while let Some(record) = frame.next_title_record()? {
                        records.push(record);
                    }
                    return_owned_frame_input(&mut source, frame);
                    Ok(records)
                })();
                if sender.send(result).is_err() {
                    return;
                }
            });
        }
        drop(sender);
        for completed in 0..frame_count {
            for record in receiver
                .recv()
                .map_err(|_| ArchiveError::Invalid("title-index worker stopped"))??
            {
                visitor(record);
            }
            progress(completed + 1, frame_count);
        }
        Ok(())
    })
}

pub fn export_instance<W: Write>(
    instance: &Instance,
    output: W,
    frame_target: usize,
) -> Result<ExportStats> {
    let mut writer = ArchiveWriter::new(output, frame_target)?;
    let mut stats = ExportStats::default();
    export_page(instance, &mut writer, 0, &mut stats)?;
    stats.pages = 0;
    let mut after = None;
    loop {
        let page_ids = instance.archive_page_ids_after(after, 4096)?;
        if page_ids.is_empty() {
            break;
        }
        for page_id in page_ids {
            after = Some(page_id);
            export_page(instance, &mut writer, page_id, &mut stats)?;
        }
    }
    if let Some(name) = instance.sync_state("history_user_archive")? {
        let name_path = Path::new(&name);
        if name_path.file_name() != Some(name_path.as_os_str()) {
            return Err(ArchiveError::Invalid("invalid typed user archive name"));
        }
        let path = instance.root().join(name_path);
        let (_, frames, complete) = index_file(&path)?;
        if !complete {
            return Err(ArchiveError::Invalid(
                "typed user archive has no completion marker",
            ));
        }
        for frame in frames {
            visit_frame(&path, &frame, |record| match record {
                Record::UserState { .. } | Record::UserAction { .. } => {
                    stats.user_actions += u64::from(matches!(record, Record::UserAction { .. }));
                    writer.write(&record)
                }
                _ => Err(ArchiveError::Invalid(
                    "typed user archive contains a non-user record",
                )),
            })?;
        }
    }
    let (_, frames) = writer.finish()?;
    stats.frames = frames;
    Ok(stats)
}

#[derive(Default)]
struct DepotImportPage {
    page_id: u64,
    saw_page_state: bool,
    current_title: Option<(String, Option<i64>, i64)>,
    revisions: Vec<RevisionRecord>,
    actions: Vec<(i64, PageActionRecord)>,
}

pub fn import_instance(
    instance: &Instance,
    input: impl AsRef<Path>,
    progress: impl Fn(DepotImportStats),
) -> Result<DepotImportStats> {
    let input = input.as_ref();
    let dictionary_pretrained = if instance.has_active_revision_dictionary()? {
        false
    } else {
        let samples = archive_revision_dictionary_samples(input)?;
        instance.prepare_seed_revision_dictionary(&samples)?.trained
    };
    let mut reader = ArchiveRecordReader::open(input)?;
    let mut page = DepotImportPage::default();
    let mut stats = DepotImportStats::default();
    let mut import_stats = crate::ImportStats::default();
    let mut user_writer = None;
    let mut manifest = None;
    let mut site_info = None;

    while let Some(record) = reader.next_record()? {
        if record.entity().kind == EntityKind::Page {
            let page_id = record.entity().id;
            if page.page_id != page_id && !depot_import_page_empty(&page) {
                import_depot_page(instance, std::mem::take(&mut page), &mut import_stats, &mut stats)?;
                if stats.pages % 10_000 == 0 {
                    progress(stats);
                }
            }
            page.page_id = page_id;
            match record {
                Record::PageState {
                    timestamp_micros,
                    title,
                    namespace,
                    deleted,
                    ..
                } if !page.saw_page_state => {
                    page.saw_page_state = true;
                    page.current_title =
                        (!deleted).then_some((title, namespace, timestamp_micros));
                }
                Record::Revision { revision, .. } => page.revisions.push(revision),
                Record::PageAction {
                    timestamp_micros,
                    action,
                    ..
                } => page.actions.push((timestamp_micros, action)),
                Record::PageState { .. } => {}
                Record::Unknown { .. } => {
                    return Err(ArchiveError::Invalid(
                        "cannot project an unknown page record into the depot",
                    ));
                }
                _ => unreachable!("page entity has a page record"),
            }
            continue;
        }

        if !depot_import_page_empty(&page) {
            import_depot_page(instance, std::mem::take(&mut page), &mut import_stats, &mut stats)?;
            progress(stats);
        }
        match record {
            Record::UserState { .. } | Record::UserAction { .. } => {
                let writer = match user_writer.take() {
                    Some(writer) => writer,
                    None => ArchiveWriter::new(
                        tempfile::NamedTempFile::new_in(&instance.root)?,
                        DEFAULT_FRAME_TARGET,
                    )?,
                };
                let mut writer = writer;
                writer.write(&record)?;
                user_writer = Some(writer);
                stats.user_records += 1;
            }
            Record::PageAction {
                entity,
                timestamp_micros,
                action,
            } if entity.kind == EntityKind::Global => {
                let inner = instance.inner.lock().expect("instance mutex poisoned");
                let transaction = inner
                    .conn
                    .unchecked_transaction()
                    .map_err(crate::Error::from)?;
                store_archive_action(
                    &transaction,
                    0,
                    timestamp_micros,
                    "",
                    None,
                    &action,
                )?;
                transaction.commit().map_err(crate::Error::from)?;
                stats.page_actions += 1;
            }
            Record::Manifest {
                timestamp_micros,
                manifest: record,
            } if manifest.is_none() => manifest = Some((timestamp_micros, record)),
            Record::SiteInfo {
                timestamp_micros,
                site_info: record,
            } if site_info.is_none() => site_info = Some((timestamp_micros, record)),
            Record::Manifest { .. } | Record::SiteInfo { .. } => {}
            Record::Unknown { .. } => {
                return Err(ArchiveError::Invalid(
                    "cannot project an unknown non-page record into the depot",
                ));
            }
            _ => {
                return Err(ArchiveError::Invalid(
                    "record kind does not match its non-page entity",
                ));
            }
        }
    }

    if !depot_import_page_empty(&page) {
        import_depot_page(instance, page, &mut import_stats, &mut stats)?;
    }
    {
        let mut inner = instance.inner.lock().expect("instance mutex poisoned");
        crate::instance::finish_title_slot_intent(&instance.root, &mut inner)?;
    }
    if let Some(writer) = user_writer {
        let (temporary, _) = writer.finish()?;
        temporary.as_file().sync_all()?;
        let name = "history-users-archive.swdump";
        temporary
            .persist(instance.root.join(name))
            .map_err(|error| ArchiveError::Io(error.error))?;
        instance.set_sync_state("history_user_archive", name)?;
    }
    if let Some((timestamp_micros, record)) = site_info {
        store_archive_siteinfo(instance, timestamp_micros, &record)?;
    }
    if let Some((_, record)) = manifest {
        instance.set_sync_state("wiki_dbname", &record.wiki_db)?;
        instance.set_sync_state("full_snapshot_date", &record.content_snapshot)?;
        instance.set_sync_state("incremental_date", &record.content_snapshot)?;
        instance.set_sync_state("history_frontier_snapshot", &record.metadata_snapshot)?;
    }
    instance.flush()?;
    if !dictionary_pretrained {
        instance.finalize_seed_revision_dictionary()?;
    }
    instance.flush()?;
    progress(stats);
    Ok(stats)
}

fn archive_revision_dictionary_samples(input: &Path) -> Result<Vec<Vec<u8>>> {
    let limit = crate::instance::REVISION_SAMPLE_COUNT;
    let mut reader = ArchiveRecordReader::open(input)?;
    let mut selected = std::collections::BTreeMap::<(u64, u64), Vec<u8>>::new();
    let mut page_id = None;
    let mut head = None;
    while let Some(record) = reader.next_record()? {
        if record.entity().kind != EntityKind::Page {
            break;
        }
        let current_page = record.entity().id;
        if page_id != Some(current_page) {
            if let (Some(previous_page), Some(revision)) = (page_id, head.take()) {
                select_archive_dictionary_sample(&mut selected, limit, previous_page, revision);
            }
            page_id = Some(current_page);
        }
        if let Record::Revision { revision, .. } = record {
            // Page records are newest-first. Sample the record that will
            // actually become f0, including a visibility-only head whose
            // text is unavailable; revision ids are not an ordering key.
            if head.is_none() {
                head = Some(revision);
            }
        }
    }
    if let (Some(page_id), Some(revision)) = (page_id, head) {
        select_archive_dictionary_sample(&mut selected, limit, page_id, revision);
    }
    Ok(selected.into_values().collect())
}

fn select_archive_dictionary_sample(
    selected: &mut std::collections::BTreeMap<(u64, u64), Vec<u8>>,
    limit: usize,
    page_id: u64,
    revision: RevisionRecord,
) {
    let key = (crate::instance::sample_hash(page_id), page_id);
    if selected.len() < limit || selected.last_key_value().is_some_and(|(last, _)| key < *last) {
        selected.insert(
            key,
            crate::revision::encode_revision(&revision.meta, &revision.text),
        );
        if selected.len() > limit {
            selected.pop_last();
        }
    }
}

fn depot_import_page_empty(page: &DepotImportPage) -> bool {
    !page.saw_page_state
        && page.current_title.is_none()
        && page.revisions.is_empty()
        && page.actions.is_empty()
}

fn import_depot_page(
    instance: &Instance,
    page: DepotImportPage,
    import_stats: &mut crate::ImportStats,
    stats: &mut DepotImportStats,
) -> Result<()> {
    if page.page_id != 0 && (page.saw_page_state || !page.revisions.is_empty()) {
        stats.pages += 1;
    }
    stats.revisions += page.revisions.len() as u64;
    stats.page_actions += page.actions.len() as u64;
    if depot_page_is_complete(instance, &page)? {
        return Ok(());
    }

    let earliest_revision = page
        .revisions
        .iter()
        .map(|revision| revision.meta.ts.timestamp_micros())
        .min();
    let current_title = page.current_title.as_ref().map(|(title, _, _)| title.as_str());
    if page.page_id != 0 && (!page.revisions.is_empty() || current_title.is_some()) {
        let already_imported = {
            let inner = instance.inner.lock().expect("instance mutex poisoned");
            inner
                .depot
                .has_chain(page.page_id)
                .map_err(crate::Error::from)?
        };
        let records = if already_imported {
            Vec::new()
        } else {
            page.revisions
                .iter()
                .map(|revision| crate::revision::encode_revision(&revision.meta, &revision.text))
                .collect()
        };
        crate::import::import_encoded_page(
            instance,
            page.page_id,
            current_title,
            earliest_revision.or_else(|| page.current_title.as_ref().map(|entry| entry.2)),
            records,
            import_stats,
        )?;
    }

    let current_namespace = page.current_title.as_ref().and_then(|entry| entry.1);
    let current_title = page
        .current_title
        .as_ref()
        .map(|entry| entry.0.as_str())
        .unwrap_or("");
    let inner = instance.inner.lock().expect("instance mutex poisoned");
    let transaction = inner
        .conn
        .unchecked_transaction()
        .map_err(crate::Error::from)?;
    for revision in &page.revisions {
        if let Some(visibility) = &revision.visibility {
            store_archive_visibility(
                &transaction,
                page.page_id,
                revision.meta.rev_id,
                visibility,
            )?;
        }
    }
    for (timestamp_micros, action) in page.actions {
        store_archive_action(
            &transaction,
            page.page_id,
            timestamp_micros,
            current_title,
            current_namespace,
            &action,
        )?;
    }
    transaction.commit().map_err(crate::Error::from)?;
    Ok(())
}

fn depot_page_is_complete(instance: &Instance, page: &DepotImportPage) -> Result<bool> {
    let has_content = if page.revisions.is_empty() {
        true
    } else {
        let inner = instance.inner.lock().expect("instance mutex poisoned");
        inner
            .depot
            .has_chain(page.page_id)
            .map_err(crate::Error::from)?
    };
    if !has_content {
        return Ok(false);
    }
    let expected_title = page.current_title.as_ref().map(|entry| entry.0.as_str());
    if instance.page_current_title(page.page_id)?.as_deref() != expected_title {
        return Ok(false);
    }
    let expected_visibility = page
        .revisions
        .iter()
        .filter(|revision| revision.visibility.is_some())
        .count() as i64;
    let inner = instance.inner.lock().expect("instance mutex poisoned");
    let actions: i64 = inner
        .conn
        .query_row(
            "SELECT COUNT(*) FROM page_actions
             WHERE source_partition='archive' AND page_id=?1",
            [i64::try_from(page.page_id).map_err(|_| ArchiveError::FieldTooLarge)?],
            |row| row.get(0),
        )
        .map_err(crate::Error::from)?;
    let visibility: i64 = inner
        .conn
        .query_row(
            "SELECT COUNT(*) FROM revision_visibility
             WHERE source_partition='archive' AND page_id=?1",
            [i64::try_from(page.page_id).map_err(|_| ArchiveError::FieldTooLarge)?],
            |row| row.get(0),
        )
        .map_err(crate::Error::from)?;
    Ok(actions == page.actions.len() as i64 && visibility == expected_visibility)
}

fn store_archive_visibility(
    transaction: &rusqlite::Transaction<'_>,
    page_id: u64,
    revision_id: u64,
    visibility: &RevisionVisibilityRecord,
) -> Result<()> {
    let mut parts = Vec::new();
    if visibility.deleted_parts & 1 != 0 {
        parts.push("text");
    }
    if visibility.deleted_parts & 2 != 0 {
        parts.push("comment");
    }
    if visibility.deleted_parts & 4 != 0 {
        parts.push("user");
    }
    transaction
        .execute(
        "INSERT OR REPLACE INTO revision_visibility(
            revision_id,page_id,source_partition,deleted_parts,
            parts_are_suppressed,deleted_by_page_deletion,page_deletion_timestamp
         ) VALUES(?1,?2,'archive',?3,?4,?5,?6)",
        rusqlite::params![
            i64::try_from(revision_id).map_err(|_| ArchiveError::FieldTooLarge)?,
            i64::try_from(page_id).map_err(|_| ArchiveError::FieldTooLarge)?,
            parts.join(","),
            visibility.parts_are_suppressed,
            visibility.deleted_by_page_deletion,
            visibility
                .page_deletion_timestamp_micros
                .map(timestamp_string)
                .transpose()?
                .unwrap_or_default(),
        ],
        )
        .map_err(crate::Error::from)?;
    Ok(())
}

fn store_archive_action(
    transaction: &rusqlite::Transaction<'_>,
    page_id: u64,
    timestamp_micros: i64,
    current_title: &str,
    current_namespace: Option<i64>,
    action: &PageActionRecord,
) -> Result<()> {
    let event_type = match &action.kind {
        PageActionKind::Create => "create",
        PageActionKind::LoggedCreate => "create-page",
        PageActionKind::Move => "move",
        PageActionKind::Delete => "delete",
        PageActionKind::Restore => "restore",
        PageActionKind::Merge => "merge",
        PageActionKind::Other(name) => name,
    };
    let source_key = match action.log_id {
        Some(log_id) => format!("archive:log:{log_id}"),
        None => format!(
            "archive:page:{page_id}:{timestamp_micros}:{}:{event_type}",
            action.tie_sequence
        ),
    };
    transaction
        .execute(
        "INSERT OR REPLACE INTO page_actions(
            source_key,source_partition,event_log_id,source_ordinal,event_type,event_timestamp,
            event_comment,actor_id,actor_name,page_id,title_historical,title_current,
            namespace_historical,namespace_current,page_deleted
         ) VALUES(?1,'archive',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        rusqlite::params![
            source_key,
            action.log_id.and_then(|value| i64::try_from(value).ok()),
            i64::try_from(action.tie_sequence).map_err(|_| ArchiveError::FieldTooLarge)?,
            event_type,
            timestamp_string(timestamp_micros)?,
            action.comment,
            action
                .performer
                .local_user_id
                .and_then(|value| i64::try_from(value).ok()),
            action.performer.historical_name.as_deref().unwrap_or(""),
            (page_id != 0).then(|| i64::try_from(page_id).ok()).flatten(),
            action.title_at_event,
            current_title,
            action.namespace_at_event,
            current_namespace,
            action.resulting_deleted.unwrap_or(false),
        ],
        )
        .map_err(crate::Error::from)?;
    Ok(())
}

fn timestamp_string(timestamp_micros: i64) -> Result<String> {
    chrono::DateTime::from_timestamp_micros(timestamp_micros)
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
        .ok_or_else(|| ArchiveError::InvalidTimestamp(timestamp_micros.to_string()))
}

fn store_archive_siteinfo(
    instance: &Instance,
    captured_at: i64,
    site_info: &SiteInfoRecord,
) -> Result<()> {
    let namespaces = site_info
        .namespaces
        .iter()
        .map(|namespace| {
            serde_json::json!({
                "id": namespace.id,
                "canonical": "",
                "localized": namespace.localized_name,
                "case": namespace.case,
                "aliases": namespace.aliases,
            })
        })
        .collect::<Vec<_>>();
    let payload = serde_json::to_vec(&serde_json::json!({
        "site_name": site_info.site_name,
        "db_name": site_info.db_name,
        "base": site_info.base,
        "generator": site_info.generator,
        "case": site_info.case,
        "namespaces": namespaces,
    }))
    .map_err(|_| ArchiveError::Invalid("cannot encode archive siteinfo"))?;
    let inner = instance.inner.lock().expect("instance mutex poisoned");
    let transaction = inner
        .conn
        .unchecked_transaction()
        .map_err(crate::Error::from)?;
    transaction
        .execute(
        "INSERT OR REPLACE INTO siteinfo_snapshots(captured_at,json) VALUES(?1,?2)",
        rusqlite::params![captured_at, payload],
        )
        .map_err(crate::Error::from)?;
    for interwiki in &site_info.interwiki {
        transaction
            .execute(
            "INSERT OR REPLACE INTO interwiki_map(captured_at,prefix,url,is_local)
             VALUES(?1,?2,?3,0)",
            rusqlite::params![captured_at, interwiki.prefix, interwiki.url],
            )
            .map_err(crate::Error::from)?;
    }
    transaction.commit().map_err(crate::Error::from)?;
    Ok(())
}

fn export_page<W: Write>(
    instance: &Instance,
    writer: &mut ArchiveWriter<W>,
    page_id: u64,
    stats: &mut ExportStats,
) -> Result<()> {
    let revisions = ArchiveRevisionIter {
        inner: std::sync::Arc::clone(&instance.inner),
        walk: crate::instance::WalkState::new_snapshot(page_id),
    };
    let mut no_progress = |_: &'static str, _: u64, _: u64| {};
    let mut revisions =
        PageRevisionSpool::collect_in(revisions, instance.root(), &mut no_progress)?.peekable();
    let mut actions = instance
        .archive_page_actions(page_id)?
        .into_iter()
        .map(|(action, timestamp)| {
            let timestamp = parse_timestamp_micros(&timestamp)?;
            Ok((action, timestamp))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .peekable();
    let visibility: HashMap<u64, RevisionVisibilityRecord> = instance
        .archive_revision_visibility(page_id)?
        .into_iter()
        .collect();
    if let Some(title) = instance.page_current_title(page_id)? {
        writer.write(&Record::PageState {
            page_id,
            timestamp_micros: chrono::Utc::now().timestamp_micros(),
            title,
            namespace: None,
            deleted: false,
        })?;
    }

    loop {
        let revision_key = match revisions.peek() {
            Some(Ok(revision)) => Some((
                revision.meta.ts.timestamp_micros(),
                1_u8,
                revision.meta.rev_id,
            )),
            Some(Err(_)) => return Err(revisions.next().expect("peeked").unwrap_err()),
            None => None,
        };
        let action_key = actions
            .peek()
            .map(|(action, timestamp)| (*timestamp, 0_u8, action.tie_sequence));
        if revision_key.is_none() && action_key.is_none() {
            break;
        }
        if revision_key >= action_key {
            let revision = revisions.next().expect("key exists")?;
            let revision_id = revision.meta.rev_id;
            writer.write(&Record::Revision {
                page_id,
                revision: RevisionRecord {
                    meta: revision.meta,
                    has_text: revision.has_text,
                    text: revision.text,
                    visibility: visibility.get(&revision_id).cloned(),
                    history: None,
                },
            })?;
            stats.revisions += 1;
        } else {
            let (action, timestamp_micros) = actions.next().expect("key exists");
            writer.write(&Record::PageAction {
                entity: EntityKey {
                    kind: EntityKind::Page,
                    id: page_id,
                },
                timestamp_micros,
                action,
            })?;
            stats.page_actions += 1;
        }
    }
    if page_id != 0 {
        stats.pages += 1;
    }
    Ok(())
}

struct ArchiveRevisionIter {
    inner: std::sync::Arc<std::sync::Mutex<crate::instance::InstanceInner>>,
    walk: crate::instance::WalkState,
}

struct SpooledRevision {
    meta: RevisionMeta,
    text: Option<Vec<u8>>,
    offset: u64,
    len: u64,
}

struct PageRevisionSpool {
    revisions: std::vec::IntoIter<SpooledRevision>,
    file: Option<std::fs::File>,
}

impl PageRevisionSpool {
    fn collect_in<E>(
        revisions: impl IntoIterator<Item = std::result::Result<RevisionRecord, E>>,
        spill_dir: &Path,
        progress: &mut dyn FnMut(&'static str, u64, u64),
    ) -> Result<Self>
    where
        ArchiveError: From<E>,
    {
        let mut entries = Vec::new();
        let mut memory_bytes = 0_usize;
        let mut text_bytes = 0_u64;
        let mut file = None;
        for revision in revisions {
            let revision = revision.map_err(ArchiveError::from)?;
            memory_bytes = memory_bytes.saturating_add(revision.text.len());
            text_bytes = text_bytes.saturating_add(revision.text.len() as u64);
            entries.push(SpooledRevision {
                meta: revision.meta,
                text: Some(revision.text),
                offset: 0,
                len: 0,
            });
            progress("spooling revisions", entries.len() as u64, text_bytes);
            if memory_bytes > PAGE_TEXT_MEMORY_LIMIT && file.is_none() {
                let mut spool = tempfile::tempfile_in(spill_dir).map_err(|error| {
                    ArchiveError::Io(std::io::Error::new(
                        error.kind(),
                        format!(
                            "cannot create revision-text spill in {}: {error}",
                            spill_dir.display()
                        ),
                    ))
                })?;
                spill_texts(&mut spool, &mut entries)?;
                file = Some(spool);
            } else if let Some(spool) = file.as_mut() {
                let entry = entries.last_mut().expect("just pushed");
                let text = entry.text.take().expect("new text");
                entry.offset = spool.stream_position()?;
                entry.len = text.len() as u64;
                spool.write_all(&text)?;
            }
        }
        entries.sort_by(|left, right| {
            right
                .meta
                .ts
                .cmp(&left.meta.ts)
                .then_with(|| right.meta.rev_id.cmp(&left.meta.rev_id))
        });
        Ok(Self {
            revisions: entries.into_iter(),
            file,
        })
    }
}

impl Iterator for PageRevisionSpool {
    type Item = Result<RevisionRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.revisions.next()?;
        let text = match entry.text {
            Some(text) => text,
            None => {
                let file = self.file.as_mut().expect("spilled text has file");
                if let Err(error) = file.seek(SeekFrom::Start(entry.offset)) {
                    return Some(Err(error.into()));
                }
                let Ok(len) = usize::try_from(entry.len) else {
                    return Some(Err(ArchiveError::FieldTooLarge));
                };
                let mut text = vec![0; len];
                if let Err(error) = file.read_exact(&mut text) {
                    return Some(Err(error.into()));
                }
                text
            }
        };
        Some(Ok(RevisionRecord {
            meta: entry.meta,
            has_text: true,
            text,
            visibility: None,
            history: None,
        }))
    }
}

fn spill_texts(file: &mut std::fs::File, entries: &mut [SpooledRevision]) -> Result<()> {
    for entry in entries {
        let text = entry.text.take().expect("memory-backed text");
        entry.offset = file.stream_position()?;
        entry.len = text.len() as u64;
        file.write_all(&text)?;
    }
    Ok(())
}

impl Iterator for ArchiveRevisionIter {
    type Item = crate::Result<RevisionRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        let inner = self.inner.lock().expect("instance mutex poisoned");
        match self
            .walk
            .next_record(&inner.depot, &inner.revision_dictionaries)
        {
            Ok(Some(record)) => Some(crate::revision::decode_revision(record).map(
                |(meta, text)| RevisionRecord {
                    meta,
                    has_text: true,
                    text,
                    visibility: None,
                    history: None,
                },
            )),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

fn parse_timestamp_micros(value: &str) -> Result<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_micros())
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .map(|timestamp| timestamp.and_utc().timestamp_micros())
        })
        .map_err(|_| ArchiveError::InvalidTimestamp(value.to_owned()))
}

fn record_wire_size(record: &Record) -> Result<(u8, u64)> {
    let size = match record {
        Record::PageState {
            title, namespace, ..
        } => checked_sum(&[option_i64_wire_len(*namespace), string_wire_len(title)?, 1]),
        Record::Revision { revision, .. } => revision_wire_len(revision),
        Record::PageAction { action, .. } => action_wire_len(action),
        Record::UserState { state, .. } => user_state_wire_len(state),
        Record::UserAction { action, .. } => user_action_wire_len(action),
        Record::Manifest { manifest, .. } => manifest_wire_len(manifest),
        Record::SiteInfo { site_info, .. } => site_info_wire_len(site_info),
        Record::Unknown { payload, kind, .. } => return Ok((*kind, payload.len() as u64)),
    }?;
    let kind = match record {
        Record::PageState { .. } => KIND_PAGE_STATE,
        Record::Revision { .. } => KIND_REVISION,
        Record::PageAction { .. } => KIND_PAGE_ACTION,
        Record::UserState { .. } => KIND_USER_STATE,
        Record::UserAction { .. } => KIND_USER_ACTION,
        Record::Manifest { .. } => KIND_MANIFEST,
        Record::SiteInfo { .. } => KIND_SITE_INFO,
        Record::Unknown { kind, .. } => *kind,
    };
    Ok((kind, size))
}

fn encode_record_wire(record: &Record) -> Result<Vec<u8>> {
    let entity = record.entity();
    let (_, payload_len) = record_wire_size(record)?;
    let capacity = 1_u64
        .checked_add(varint_len(entity.id) as u64)
        .and_then(|size| size.checked_add(8 + 1 + varint_len(payload_len) as u64))
        .and_then(|size| size.checked_add(payload_len))
        .ok_or(ArchiveError::FieldTooLarge)?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(capacity).map_err(|_| ArchiveError::FieldTooLarge)?,
    );
    write_record_wire(&mut bytes, record)?;
    Ok(bytes)
}

fn write_record_wire(output: &mut impl Write, record: &Record) -> Result<()> {
    let entity = record.entity();
    let (kind, payload_len) = record_wire_size(record)?;
    output.write_all(&[entity.kind as u8])?;
    write_varint(output, entity.id)?;
    output.write_all(&record.timestamp_micros().to_le_bytes())?;
    output.write_all(&[kind])?;
    write_varint(output, payload_len)?;
    write_record_payload(output, record)
}

fn write_record_payload<W: Write>(out: &mut W, record: &Record) -> Result<()> {
    match record {
        Record::PageState {
            title,
            namespace,
            deleted,
            ..
        } => {
            write_option_i64(out, *namespace)?;
            write_string(out, title)?;
            out.write_all(&[u8::from(*deleted)])?;
        }
        Record::Revision { revision, .. } => write_revision(out, revision)?,
        Record::PageAction { action, .. } => write_action(out, action)?,
        Record::UserState { state, .. } => write_user_state(out, state)?,
        Record::UserAction { action, .. } => write_user_action(out, action)?,
        Record::Manifest { manifest, .. } => write_manifest(out, manifest)?,
        Record::SiteInfo { site_info, .. } => write_site_info(out, site_info)?,
        Record::Unknown { payload, .. } => out.write_all(payload)?,
    }
    Ok(())
}

fn revision_wire_len(revision: &RevisionRecord) -> Result<u64> {
    let (contributor, _) = contributor_bytes(&revision.meta.contributor);
    checked_sum(&[
        4,
        8,
        8,
        8,
        1,
        string_wire_len(contributor)?,
        string_wire_len(&revision.meta.comment)?,
        1,
        bytes_wire_len(&revision.text)?,
        1,
        revision
            .visibility
            .as_ref()
            .map(visibility_wire_len)
            .transpose()?
            .unwrap_or(0),
        1,
        revision
            .history
            .as_ref()
            .map(revision_history_wire_len)
            .transpose()?
            .unwrap_or(0),
    ])
}

fn write_revision<W: Write>(out: &mut W, revision: &RevisionRecord) -> Result<()> {
    out.write_all(&revision.meta.flags.to_le_bytes())?;
    out.write_all(&revision.meta.rev_id.to_le_bytes())?;
    out.write_all(&revision.meta.parent_id.to_le_bytes())?;
    let (contributor, user_id) = contributor_bytes(&revision.meta.contributor);
    out.write_all(&user_id.to_le_bytes())?;
    out.write_all(&[contributor_kind(&revision.meta.contributor)])?;
    write_string(out, contributor)?;
    write_string(out, &revision.meta.comment)?;
    out.write_all(&[u8::from(revision.has_text)])?;
    write_bytes(out, &revision.text)?;
    match &revision.visibility {
        Some(visibility) => {
            out.write_all(&[1])?;
            write_visibility(out, visibility)?;
        }
        None => out.write_all(&[0])?,
    }
    match &revision.history {
        Some(history) => {
            out.write_all(&[1])?;
            write_revision_history(out, history)?;
        }
        None => out.write_all(&[0])?,
    }
    Ok(())
}

fn action_wire_len(action: &PageActionRecord) -> Result<u64> {
    checked_sum(&[
        option_u64_wire_len(action.log_id),
        varint_len(action.tie_sequence) as u64,
        action_kind_wire_len(&action.kind)?,
        performer_wire_len(&action.performer)?,
        string_wire_len(&action.comment)?,
        string_wire_len(&action.title_at_event)?,
        option_i64_wire_len(action.namespace_at_event),
        option_bool_wire_len(action.resulting_deleted),
    ])
}

fn write_action<W: Write>(out: &mut W, action: &PageActionRecord) -> Result<()> {
    write_option_u64(out, action.log_id)?;
    write_varint(out, action.tie_sequence)?;
    write_action_kind(out, &action.kind)?;
    write_performer(out, &action.performer)?;
    write_string(out, &action.comment)?;
    write_string(out, &action.title_at_event)?;
    write_option_i64(out, action.namespace_at_event)?;
    write_option_bool(out, action.resulting_deleted)?;
    Ok(())
}

fn visibility_wire_len(visibility: &RevisionVisibilityRecord) -> Result<u64> {
    checked_sum(&[
        1,
        1,
        1,
        option_i64_wire_len(visibility.page_deletion_timestamp_micros),
    ])
}

fn write_visibility<W: Write>(out: &mut W, visibility: &RevisionVisibilityRecord) -> Result<()> {
    out.write_all(&[visibility.deleted_parts])?;
    out.write_all(&[u8::from(visibility.parts_are_suppressed)])?;
    out.write_all(&[u8::from(visibility.deleted_by_page_deletion)])?;
    write_option_i64(out, visibility.page_deletion_timestamp_micros)?;
    Ok(())
}

fn performer_wire_len(performer: &PerformerRecord) -> Result<u64> {
    checked_sum(&[
        option_u64_wire_len(performer.local_user_id),
        option_u64_wire_len(performer.central_user_id),
        option_string_wire_len(performer.historical_name.as_deref())?,
        1,
    ])
}

fn write_performer(out: &mut impl Write, performer: &PerformerRecord) -> Result<()> {
    write_option_u64(out, performer.local_user_id)?;
    write_option_u64(out, performer.central_user_id)?;
    write_option_string(out, performer.historical_name.as_deref())?;
    out.write_all(&[performer.account_class as u8])?;
    Ok(())
}

fn action_kind_wire_len(kind: &PageActionKind) -> Result<u64> {
    match kind {
        PageActionKind::Other(name) => checked_sum(&[1, string_wire_len(name)?]),
        _ => Ok(1),
    }
}

fn write_action_kind(out: &mut impl Write, kind: &PageActionKind) -> Result<()> {
    let code = match kind {
        PageActionKind::Create => 0,
        PageActionKind::LoggedCreate => 1,
        PageActionKind::Move => 2,
        PageActionKind::Delete => 3,
        PageActionKind::Restore => 4,
        PageActionKind::Merge => 5,
        PageActionKind::Other(_) => 255,
    };
    out.write_all(&[code])?;
    if let PageActionKind::Other(name) = kind {
        write_string(out, name)?;
    }
    Ok(())
}

fn revision_history_wire_len(history: &RevisionHistoryRecord) -> Result<u64> {
    checked_sum(&[
        option_bool_wire_len(history.minor),
        option_string_wire_len(history.content_model.as_deref())?,
        option_string_wire_len(history.content_format.as_deref())?,
        option_bool_wire_len(history.identity_reverted),
        option_u64_wire_len(history.first_reverting_revision_id),
        option_u64_wire_len(history.seconds_to_revert),
        option_bool_wire_len(history.identity_revert),
        option_bool_wire_len(history.before_page_creation),
        strings_wire_len(&history.tags)?,
    ])
}

fn write_revision_history(out: &mut impl Write, history: &RevisionHistoryRecord) -> Result<()> {
    write_option_bool(out, history.minor)?;
    write_option_string(out, history.content_model.as_deref())?;
    write_option_string(out, history.content_format.as_deref())?;
    write_option_bool(out, history.identity_reverted)?;
    write_option_u64(out, history.first_reverting_revision_id)?;
    write_option_u64(out, history.seconds_to_revert)?;
    write_option_bool(out, history.identity_revert)?;
    write_option_bool(out, history.before_page_creation)?;
    write_strings(out, &history.tags)?;
    Ok(())
}

fn user_state_wire_len(state: &UserStateRecord) -> Result<u64> {
    checked_sum(&[
        option_string_wire_len(state.current_name.as_deref())?,
        option_u64_wire_len(state.central_user_id),
        1,
        strings_wire_len(&state.groups)?,
        strings_wire_len(&state.blocks)?,
        strings_wire_len(&state.bot_by)?,
    ])
}

fn write_user_state(out: &mut impl Write, state: &UserStateRecord) -> Result<()> {
    write_option_string(out, state.current_name.as_deref())?;
    write_option_u64(out, state.central_user_id)?;
    out.write_all(&[state.account_class as u8])?;
    write_strings(out, &state.groups)?;
    write_strings(out, &state.blocks)?;
    write_strings(out, &state.bot_by)?;
    Ok(())
}

fn user_action_kind_wire_len(kind: &UserActionKind) -> Result<u64> {
    match kind {
        UserActionKind::Other(name) => checked_sum(&[1, string_wire_len(name)?]),
        _ => Ok(1),
    }
}

fn write_user_action_kind(out: &mut impl Write, kind: &UserActionKind) -> Result<()> {
    let code = match kind {
        UserActionKind::Create => 0,
        UserActionKind::Rename => 1,
        UserActionKind::GroupsChanged => 2,
        UserActionKind::BlocksChanged => 3,
        UserActionKind::Other(_) => 255,
    };
    out.write_all(&[code])?;
    if let UserActionKind::Other(name) = kind {
        write_string(out, name)?;
    }
    Ok(())
}

fn user_action_wire_len(action: &UserActionRecord) -> Result<u64> {
    checked_sum(&[
        option_u64_wire_len(action.log_id),
        varint_len(action.tie_sequence) as u64,
        user_action_kind_wire_len(&action.kind)?,
        performer_wire_len(&action.performer)?,
        string_wire_len(&action.comment)?,
        option_string_wire_len(action.historical_name.as_deref())?,
        strings_wire_len(&action.groups)?,
        strings_wire_len(&action.blocks)?,
        strings_wire_len(&action.bot_by)?,
        1,
        option_i64_wire_len(action.registration_timestamp_micros),
        option_i64_wire_len(action.creation_timestamp_micros),
        option_i64_wire_len(action.first_edit_timestamp_micros),
    ])
}

fn write_user_action(out: &mut impl Write, action: &UserActionRecord) -> Result<()> {
    write_option_u64(out, action.log_id)?;
    write_varint(out, action.tie_sequence)?;
    write_user_action_kind(out, &action.kind)?;
    write_performer(out, &action.performer)?;
    write_string(out, &action.comment)?;
    write_option_string(out, action.historical_name.as_deref())?;
    write_strings(out, &action.groups)?;
    write_strings(out, &action.blocks)?;
    write_strings(out, &action.bot_by)?;
    out.write_all(&[action.created_by])?;
    write_option_i64(out, action.registration_timestamp_micros)?;
    write_option_i64(out, action.creation_timestamp_micros)?;
    write_option_i64(out, action.first_edit_timestamp_micros)?;
    Ok(())
}

fn manifest_wire_len(manifest: &ManifestRecord) -> Result<u64> {
    checked_sum(&[
        string_wire_len(&manifest.wiki_db)?,
        string_wire_len(&manifest.content_snapshot)?,
        string_wire_len(&manifest.metadata_snapshot)?,
        strings_wire_len(&manifest.source_files)?,
    ])
}

fn write_manifest(out: &mut impl Write, manifest: &ManifestRecord) -> Result<()> {
    write_string(out, &manifest.wiki_db)?;
    write_string(out, &manifest.content_snapshot)?;
    write_string(out, &manifest.metadata_snapshot)?;
    write_strings(out, &manifest.source_files)?;
    Ok(())
}

fn site_info_wire_len(site_info: &SiteInfoRecord) -> Result<u64> {
    let mut parts = vec![
        string_wire_len(&site_info.site_name)?,
        string_wire_len(&site_info.db_name)?,
        string_wire_len(&site_info.base)?,
        string_wire_len(&site_info.generator)?,
        string_wire_len(&site_info.case)?,
        string_wire_len(&site_info.language)?,
        1,
        string_wire_len(&site_info.server)?,
        string_wire_len(&site_info.script_path)?,
        varint_len(site_info.namespaces.len() as u64) as u64,
    ];
    for namespace in &site_info.namespaces {
        parts.extend([
            4,
            string_wire_len(&namespace.case)?,
            string_wire_len(&namespace.localized_name)?,
            strings_wire_len(&namespace.aliases)?,
        ]);
    }
    parts.push(varint_len(site_info.interwiki.len() as u64) as u64);
    for interwiki in &site_info.interwiki {
        parts.extend([
            string_wire_len(&interwiki.prefix)?,
            string_wire_len(&interwiki.url)?,
            1,
        ]);
    }
    parts.push(varint_len(site_info.magic_words.len() as u64) as u64);
    for word in &site_info.magic_words {
        parts.extend([
            string_wire_len(&word.canonical_name)?,
            strings_wire_len(&word.aliases)?,
            1,
        ]);
    }
    checked_sum(&parts)
}

fn write_site_info(out: &mut impl Write, site_info: &SiteInfoRecord) -> Result<()> {
    write_string(out, &site_info.site_name)?;
    write_string(out, &site_info.db_name)?;
    write_string(out, &site_info.base)?;
    write_string(out, &site_info.generator)?;
    write_string(out, &site_info.case)?;
    write_string(out, &site_info.language)?;
    out.write_all(&[u8::from(site_info.rtl)])?;
    write_string(out, &site_info.server)?;
    write_string(out, &site_info.script_path)?;
    write_varint(out, site_info.namespaces.len() as u64)?;
    for namespace in &site_info.namespaces {
        out.write_all(&namespace.id.to_le_bytes())?;
        write_string(out, &namespace.case)?;
        write_string(out, &namespace.localized_name)?;
        write_strings(out, &namespace.aliases)?;
    }
    write_varint(out, site_info.interwiki.len() as u64)?;
    for interwiki in &site_info.interwiki {
        write_string(out, &interwiki.prefix)?;
        write_string(out, &interwiki.url)?;
        out.write_all(&[u8::from(interwiki.is_local)])?;
    }
    write_varint(out, site_info.magic_words.len() as u64)?;
    for word in &site_info.magic_words {
        write_string(out, &word.canonical_name)?;
        write_strings(out, &word.aliases)?;
        out.write_all(&[u8::from(word.case_sensitive)])?;
    }
    Ok(())
}

fn decode_record(
    entity: EntityKey,
    timestamp: i64,
    kind: u8,
    payload: Vec<u8>,
) -> Result<Record> {
    let payload_len = payload.len() as u64;
    let mut input = Cursor::new(payload);
    let record =
        decode_record_from_reader(entity, timestamp, kind, &mut input)?;
    if input.position() != payload_len {
        return Err(ArchiveError::Invalid(
            "record payload has trailing bytes",
        ));
    }
    Ok(record)
}

fn decode_record_from_reader(
    entity: EntityKey,
    timestamp: i64,
    kind: u8,
    input: &mut impl Read,
) -> Result<Record> {
    let record = match kind {
        KIND_PAGE_STATE if entity.kind == EntityKind::Page => Record::PageState {
            page_id: entity.id,
            timestamp_micros: timestamp,
            namespace: read_option_i64(&mut *input)?,
            title: read_string(&mut *input)?,
            deleted: read_bool(&mut *input)?,
        },
        KIND_REVISION if entity.kind == EntityKind::Page => Record::Revision {
            page_id: entity.id,
            revision: read_revision(&mut *input, timestamp)?,
        },
        KIND_PAGE_ACTION if matches!(entity.kind, EntityKind::Page | EntityKind::Global) => Record::PageAction {
            entity,
            timestamp_micros: timestamp,
            action: read_action(&mut *input)?,
        },
        KIND_USER_STATE if entity.kind == EntityKind::User => Record::UserState {
            user_id: entity.id,
            timestamp_micros: timestamp,
            state: read_user_state(&mut *input)?,
        },
        KIND_USER_ACTION if matches!(entity.kind, EntityKind::User | EntityKind::Global) => Record::UserAction {
            entity,
            timestamp_micros: timestamp,
            action: read_user_action(&mut *input)?,
        },
        KIND_MANIFEST if entity.kind == EntityKind::Global && entity.id == 0 => Record::Manifest {
            timestamp_micros: timestamp,
            manifest: read_manifest(&mut *input)?,
        },
        KIND_SITE_INFO if entity.kind == EntityKind::Global && entity.id == 1 => Record::SiteInfo {
            timestamp_micros: timestamp,
            site_info: read_site_info(&mut *input)?,
        },
        KIND_PAGE_STATE | KIND_REVISION | KIND_PAGE_ACTION | KIND_USER_STATE
        | KIND_USER_ACTION | KIND_MANIFEST | KIND_SITE_INFO => {
            return Err(ArchiveError::Invalid(
                "record kind is incompatible with entity kind",
            ))
        }
        _ => {
            let mut payload = Vec::new();
            input.read_to_end(&mut payload)?;
            return Ok(Record::Unknown {
                entity,
                timestamp_micros: timestamp,
                kind,
                payload,
            })
        }
    };
    Ok(record)
}

fn read_revision(
    input: &mut impl Read,
    timestamp: i64,
) -> Result<RevisionRecord> {
    let flags = read_u32(input)?;
    let rev_id = read_u64(input)?;
    let parent_id = read_u64(input)?;
    let user_id = read_u64(input)?;
    let kind = read_u8(input)?;
    let contributor_name = read_string(input)?;
    let contributor = match kind {
        0 => ContributorMeta::Anonymous {
            ip: contributor_name,
        },
        1 => ContributorMeta::Named {
            username: contributor_name,
            user_id,
        },
        2 if contributor_name.is_empty() => ContributorMeta::Hidden,
        _ => return Err(ArchiveError::Invalid("invalid contributor")),
    };
    let comment = read_string(input)?;
    let has_text = read_bool(input)?;
    let text = read_bytes(input)?;
    let visibility = match read_u8(input)? {
        0 => None,
        1 => Some(read_visibility(input)?),
        _ => return Err(ArchiveError::Invalid("invalid visibility marker")),
    };
    let history = match read_u8(input)? {
        0 => None,
        1 => Some(read_revision_history(input)?),
        _ => return Err(ArchiveError::Invalid("invalid revision history marker")),
    };
    let ts = DateTime::<Utc>::from_timestamp_micros(timestamp)
        .ok_or(ArchiveError::Invalid("revision timestamp out of range"))?;
    Ok(RevisionRecord {
        meta: RevisionMeta {
            rev_id,
            parent_id,
            ts,
            contributor,
            comment,
            sha1: String::new(),
            flags,
            text_len: text.len() as u64,
        },
        has_text,
        text,
        visibility,
        history,
    })
}

fn read_action(input: &mut impl Read) -> Result<PageActionRecord> {
    Ok(PageActionRecord {
        log_id: read_option_u64(input)?,
        tie_sequence: read_varint(input)?.0,
        kind: read_action_kind(input)?,
        performer: read_performer(input)?,
        comment: read_string(input)?,
        title_at_event: read_string(input)?,
        namespace_at_event: read_option_i64(input)?,
        resulting_deleted: read_option_bool(input)?,
    })
}

fn read_visibility(
    input: &mut impl Read,
) -> Result<RevisionVisibilityRecord> {
    let deleted_parts = read_u8(input)?;
    let parts_are_suppressed = read_bool(input)?;
    let deleted_by_page_deletion = read_bool(input)?;
    let page_deletion_timestamp_micros = read_option_i64(input)?;
    Ok(RevisionVisibilityRecord {
        deleted_parts,
        parts_are_suppressed,
        deleted_by_page_deletion,
        page_deletion_timestamp_micros,
    })
}

fn read_account_class(input: &mut impl Read) -> Result<AccountClass> {
    match read_u8(input)? {
        0 => Ok(AccountClass::Unknown),
        1 => Ok(AccountClass::Anonymous),
        2 => Ok(AccountClass::Temporary),
        3 => Ok(AccountClass::Permanent),
        4 => Ok(AccountClass::Hidden),
        _ => Err(ArchiveError::Invalid("invalid account class")),
    }
}

fn read_performer(input: &mut impl Read) -> Result<PerformerRecord> {
    Ok(PerformerRecord {
        local_user_id: read_option_u64(input)?,
        central_user_id: read_option_u64(input)?,
        historical_name: read_option_string(input)?,
        account_class: read_account_class(input)?,
    })
}

fn read_action_kind(input: &mut impl Read) -> Result<PageActionKind> {
    match read_u8(input)? {
        0 => Ok(PageActionKind::Create),
        1 => Ok(PageActionKind::LoggedCreate),
        2 => Ok(PageActionKind::Move),
        3 => Ok(PageActionKind::Delete),
        4 => Ok(PageActionKind::Restore),
        5 => Ok(PageActionKind::Merge),
        255 => Ok(PageActionKind::Other(read_string(input)?)),
        _ => Err(ArchiveError::Invalid("invalid page action kind")),
    }
}

fn read_revision_history(
    input: &mut impl Read,
) -> Result<RevisionHistoryRecord> {
    Ok(RevisionHistoryRecord {
        minor: read_option_bool(input)?,
        content_model: read_option_string(input)?,
        content_format: read_option_string(input)?,
        identity_reverted: read_option_bool(input)?,
        first_reverting_revision_id: read_option_u64(input)?,
        seconds_to_revert: read_option_u64(input)?,
        identity_revert: read_option_bool(input)?,
        before_page_creation: read_option_bool(input)?,
        tags: read_strings(input)?,
    })
}

fn read_user_state(input: &mut impl Read) -> Result<UserStateRecord> {
    Ok(UserStateRecord {
        current_name: read_option_string(input)?,
        central_user_id: read_option_u64(input)?,
        account_class: read_account_class(input)?,
        groups: read_strings(input)?,
        blocks: read_strings(input)?,
        bot_by: read_strings(input)?,
    })
}

fn read_user_action_kind(input: &mut impl Read) -> Result<UserActionKind> {
    match read_u8(input)? {
        0 => Ok(UserActionKind::Create),
        1 => Ok(UserActionKind::Rename),
        2 => Ok(UserActionKind::GroupsChanged),
        3 => Ok(UserActionKind::BlocksChanged),
        255 => Ok(UserActionKind::Other(read_string(input)?)),
        _ => Err(ArchiveError::Invalid("invalid user action kind")),
    }
}

fn read_user_action(input: &mut impl Read) -> Result<UserActionRecord> {
    Ok(UserActionRecord {
        log_id: read_option_u64(input)?,
        tie_sequence: read_varint(input)?.0,
        kind: read_user_action_kind(input)?,
        performer: read_performer(input)?,
        comment: read_string(input)?,
        historical_name: read_option_string(input)?,
        groups: read_strings(input)?,
        blocks: read_strings(input)?,
        bot_by: read_strings(input)?,
        created_by: read_u8(input)?,
        registration_timestamp_micros: read_option_i64(input)?,
        creation_timestamp_micros: read_option_i64(input)?,
        first_edit_timestamp_micros: read_option_i64(input)?,
    })
}

fn read_manifest(input: &mut impl Read) -> Result<ManifestRecord> {
    Ok(ManifestRecord {
        wiki_db: read_string(input)?,
        content_snapshot: read_string(input)?,
        metadata_snapshot: read_string(input)?,
        source_files: read_strings(input)?,
    })
}

fn read_site_info(input: &mut impl Read) -> Result<SiteInfoRecord> {
    let site_name = read_string(input)?;
    let db_name = read_string(input)?;
    let base = read_string(input)?;
    let generator = read_string(input)?;
    let case = read_string(input)?;
    let language = read_string(input)?;
    let rtl = read_u8(input)? != 0;
    let server = read_string(input)?;
    let script_path = read_string(input)?;
    let namespace_count = usize::try_from(read_varint(input)?.0)
        .map_err(|_| ArchiveError::FieldTooLarge)?;
    let mut namespaces = Vec::with_capacity(namespace_count);
    for _ in 0..namespace_count {
        namespaces.push(SiteNamespaceRecord {
            id: read_u32(input)? as i32,
            case: read_string(input)?,
            localized_name: read_string(input)?,
            aliases: read_strings(input)?,
        });
    }
    let interwiki_count = usize::try_from(read_varint(input)?.0)
        .map_err(|_| ArchiveError::FieldTooLarge)?;
    let mut interwiki = Vec::with_capacity(interwiki_count);
    for _ in 0..interwiki_count {
        interwiki.push(SiteInterwikiRecord {
            prefix: read_string(input)?,
            url: read_string(input)?,
            is_local: read_u8(input)? != 0,
        });
    }
    let magic_word_count = usize::try_from(read_varint(input)?.0)
        .map_err(|_| ArchiveError::FieldTooLarge)?;
    let mut magic_words = Vec::with_capacity(magic_word_count);
    for _ in 0..magic_word_count {
        magic_words.push(SiteMagicWordRecord {
            canonical_name: read_string(input)?,
            aliases: read_strings(input)?,
            case_sensitive: read_u8(input)? != 0,
        });
    }
    Ok(SiteInfoRecord {
        site_name,
        db_name,
        base,
        generator,
        case,
        language,
        rtl,
        server,
        script_path,
        namespaces,
        interwiki,
        magic_words,
    })
}

fn contributor_kind(contributor: &ContributorMeta) -> u8 {
    match contributor {
        ContributorMeta::Anonymous { .. } => 0,
        ContributorMeta::Named { .. } => 1,
        ContributorMeta::Hidden => 2,
    }
}

fn contributor_bytes(contributor: &ContributorMeta) -> (&str, u64) {
    match contributor {
        ContributorMeta::Anonymous { ip } => (ip, 0),
        ContributorMeta::Named { username, user_id } => (username, *user_id),
        ContributorMeta::Hidden => ("", 0),
    }
}

fn checked_sum(parts: &[u64]) -> Result<u64> {
    parts.iter().try_fold(0_u64, |sum, value| {
        sum.checked_add(*value).ok_or(ArchiveError::FieldTooLarge)
    })
}

fn string_wire_len(value: &str) -> Result<u64> {
    bytes_wire_len(value.as_bytes())
}

fn bytes_wire_len(value: &[u8]) -> Result<u64> {
    let len = u64::try_from(value.len()).map_err(|_| ArchiveError::FieldTooLarge)?;
    Ok(varint_len(len) as u64 + len)
}

fn option_i64_wire_len(value: Option<i64>) -> u64 {
    if value.is_some() {
        9
    } else {
        1
    }
}

fn option_u64_wire_len(value: Option<u64>) -> u64 {
    1 + value.map_or(0, |value| varint_len(value) as u64)
}

fn option_bool_wire_len(_: Option<bool>) -> u64 {
    1
}

fn option_string_wire_len(value: Option<&str>) -> Result<u64> {
    Ok(1 + value.map(string_wire_len).transpose()?.unwrap_or(0))
}

fn strings_wire_len(values: &[String]) -> Result<u64> {
    let mut size = varint_len(values.len() as u64) as u64;
    for value in values {
        size = size
            .checked_add(string_wire_len(value)?)
            .ok_or(ArchiveError::FieldTooLarge)?;
    }
    Ok(size)
}

fn write_string<W: Write>(out: &mut W, value: &str) -> Result<()> {
    write_bytes(out, value.as_bytes())
}

fn write_bytes<W: Write>(out: &mut W, value: &[u8]) -> Result<()> {
    write_varint(out, value.len() as u64)?;
    out.write_all(value)?;
    Ok(())
}

fn write_option_i64<W: Write>(out: &mut W, value: Option<i64>) -> Result<()> {
    match value {
        Some(value) => {
            out.write_all(&[1])?;
            out.write_all(&value.to_le_bytes())?;
        }
        None => out.write_all(&[0])?,
    }
    Ok(())
}

fn write_option_u64(out: &mut impl Write, value: Option<u64>) -> Result<()> {
    match value {
        Some(value) => {
            out.write_all(&[1])?;
            write_varint(out, value)?;
        }
        None => out.write_all(&[0])?,
    }
    Ok(())
}

fn write_option_bool(out: &mut impl Write, value: Option<bool>) -> Result<()> {
    out.write_all(&[match value {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }])?;
    Ok(())
}

fn write_option_string(out: &mut impl Write, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => {
            out.write_all(&[1])?;
            write_string(out, value)?;
        }
        None => out.write_all(&[0])?,
    }
    Ok(())
}

fn write_strings(out: &mut impl Write, values: &[String]) -> Result<()> {
    write_varint(out, values.len() as u64)?;
    for value in values {
        write_string(out, value)?;
    }
    Ok(())
}

fn write_varint<W: Write>(out: &mut W, mut value: u64) -> Result<()> {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.write_all(&[byte])?;
        if value == 0 {
            return Ok(());
        }
    }
}

fn varint_len(mut value: u64) -> usize {
    let mut bytes = 1;
    while value >= 0x80 {
        value >>= 7;
        bytes += 1;
    }
    bytes
}

fn read_varint<R: Read>(input: &mut R) -> Result<(u64, usize)> {
    let mut value = 0_u64;
    for index in 0..10 {
        let byte = read_u8(input)?;
        if index == 9 && byte > 1 {
            return Err(ArchiveError::Invalid("varint overflow"));
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(ArchiveError::Invalid("varint overflow"))
}

fn read_u8<R: Read>(input: &mut R) -> Result<u8> {
    let mut value = [0_u8; 1];
    input.read_exact(&mut value)?;
    Ok(value[0])
}

fn read_i64<R: Read>(input: &mut R) -> Result<i64> {
    let mut value = [0_u8; 8];
    input.read_exact(&mut value)?;
    Ok(i64::from_le_bytes(value))
}

fn read_u64_from(input: &mut impl Read) -> Result<u64> {
    let mut value = [0_u8; 8];
    input.read_exact(&mut value)?;
    Ok(u64::from_le_bytes(value))
}

fn read_u32(input: &mut impl Read) -> Result<u32> {
    let mut bytes = [0_u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(input: &mut impl Read) -> Result<u64> {
    let mut bytes = [0_u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_string(input: &mut impl Read) -> Result<String> {
    String::from_utf8(read_bytes(input)?)
        .map_err(|_| ArchiveError::Invalid("archive string is not UTF-8"))
}

fn read_bytes(input: &mut impl Read) -> Result<Vec<u8>> {
    let (len, _) = read_varint(input)?;
    let len = len.try_into().map_err(|_| ArchiveError::FieldTooLarge)?;
    let mut bytes = vec![0_u8; len];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_option_i64(input: &mut impl Read) -> Result<Option<i64>> {
    match read_u8(input)? {
        0 => Ok(None),
        1 => {
            let mut bytes = [0_u8; 8];
            input.read_exact(&mut bytes)?;
            Ok(Some(i64::from_le_bytes(bytes)))
        }
        _ => Err(ArchiveError::Invalid("invalid optional integer marker")),
    }
}

fn read_option_u64(input: &mut impl Read) -> Result<Option<u64>> {
    match read_u8(input)? {
        0 => Ok(None),
        1 => Ok(Some(read_varint(input)?.0)),
        _ => Err(ArchiveError::Invalid("invalid optional integer marker")),
    }
}

fn read_option_bool(input: &mut impl Read) -> Result<Option<bool>> {
    match read_u8(input)? {
        0 => Ok(None),
        1 => Ok(Some(false)),
        2 => Ok(Some(true)),
        _ => Err(ArchiveError::Invalid("invalid optional boolean marker")),
    }
}

fn read_option_string(input: &mut impl Read) -> Result<Option<String>> {
    match read_u8(input)? {
        0 => Ok(None),
        1 => Ok(Some(read_string(input)?)),
        _ => Err(ArchiveError::Invalid("invalid optional string marker")),
    }
}

fn read_strings(input: &mut impl Read) -> Result<Vec<String>> {
    let count: usize = read_varint(input)?
        .0
        .try_into()
        .map_err(|_| ArchiveError::FieldTooLarge)?;
    (0..count).map(|_| read_string(input)).collect()
}

fn read_bool(input: &mut impl Read) -> Result<bool> {
    match read_u8(input)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ArchiveError::Invalid("invalid boolean")),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use chrono::TimeZone;

    use super::*;

    fn revision(page_id: u64, rev_id: u64, timestamp: i64, text: &[u8]) -> Record {
        Record::Revision {
            page_id,
            revision: RevisionRecord {
                meta: RevisionMeta {
                    rev_id,
                    parent_id: rev_id.saturating_sub(1),
                    ts: Utc.timestamp_micros(timestamp).single().unwrap(),
                    contributor: ContributorMeta::Named {
                        username: "Editor".into(),
                        user_id: 7,
                    },
                    comment: "edit".into(),
                    sha1: String::new(),
                    flags: 0,
                    text_len: text.len() as u64,
                },
                has_text: true,
                text: text.to_vec(),
                visibility: None,
                history: None,
            },
        }
    }

    #[test]
    fn indexed_archive_set_opens_a_complete_direct_file() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("wiki.swdump");
        let titles = temporary.path().join("wiki.swtitle");
        let mut writer =
            ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 1).unwrap();
        writer
            .write(&Record::PageState {
                page_id: 1,
                timestamp_micros: 20,
                title: "One".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        writer.write(&revision(1, 1, 10, b"text")).unwrap();
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 30,
                site_info: SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "testwiki".into(),
                    base: String::new(),
                    generator: String::new(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: String::new(),
                    script_path: String::new(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        writer.finish().unwrap();
        crate::title_index::build(
            &archive,
            &titles,
            &crate::generation::GenerationId::from_plan_bytes(b"archive-indexed-set-test"),
        )
        .unwrap();

        let titles = crate::title_index::TitleIndex::open(&titles).unwrap();
        assert_eq!(titles.segment_count(), 0);
        let indexed = IndexedArchiveSet::open(&archive, &titles).unwrap();
        let location = indexed.location(titles.frame(0).unwrap()).unwrap();
        assert!(location.physical_segment.is_none());
        assert!(indexed.open_file(&location).unwrap().metadata().unwrap().len() > 0);
    }

    #[cfg(unix)]
    #[test]
    fn indexed_range_reader_opens_an_unread_segment_after_publication_rename() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("wiki.swdump");
        let displaced = temporary.path().join("wiki.displaced.swdump");
        let titles_path = temporary.path().join("wiki.swtitle");
        let output = crate::archive_set::ArchiveSetOutput::new_in(temporary.path(), 256).unwrap();
        let mut writer = ArchiveWriter::with_ref_prefix(
            output,
            1,
            CompressionSettings::default(),
            b"indexed range reader reference prefix",
        )
        .unwrap();
        for page_id in 1..=12_u64 {
            writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: page_id as i64,
                    title: format!("Page {page_id}"),
                    namespace: Some(0),
                    deleted: false,
                })
                .unwrap();
        }
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 20,
                site_info: SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "testwiki".into(),
                    base: String::new(),
                    generator: String::new(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: String::new(),
                    script_path: String::new(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (output, _) = writer.finish().unwrap();
        output.finish().unwrap().persist(&archive).unwrap();
        crate::title_index::build(
            &archive,
            &titles_path,
            &crate::generation::GenerationId::from_plan_bytes(b"range-reader-rename"),
        )
        .unwrap();
        let titles = crate::title_index::TitleIndex::open(&titles_path).unwrap();
        assert!(titles.segment_count() > 3);
        let indexed = IndexedArchiveSet::open(&archive, &titles).unwrap();
        let location = indexed
            .location(titles.frame(titles.frame_count() - 1).unwrap())
            .unwrap();

        std::fs::rename(&archive, &displaced).unwrap();
        std::fs::create_dir(&archive).unwrap();
        assert!(try_acquire_archive_cleanup_lease(&displaced)
            .unwrap()
            .is_none());
        let mut file = indexed.open_file(&location).unwrap();
        let mut records = 0;
        visit_frame_while_file(&mut file, &location, |_| {
            records += 1;
            Ok(true)
        })
        .unwrap();
        assert!(records > 0);
        drop(indexed);
        assert!(try_acquire_archive_cleanup_lease(&displaced)
            .unwrap()
            .is_some());
    }

    #[test]
    fn raw_record_stream_round_trips_every_record_kind() {
        let performer = PerformerRecord {
            local_user_id: Some(7),
            central_user_id: None,
            historical_name: Some("Editor".into()),
            account_class: AccountClass::Permanent,
        };
        let records = vec![
            Record::PageState {
                page_id: 1,
                timestamp_micros: 300,
                title: "One".into(),
                namespace: Some(0),
                deleted: false,
            },
            revision(1, 1, 200, b"text"),
            Record::PageAction {
                entity: EntityKey {
                    kind: EntityKind::Page,
                    id: 1,
                },
                timestamp_micros: 100,
                action: PageActionRecord {
                    log_id: Some(1),
                    tie_sequence: 1,
                    kind: PageActionKind::Move,
                    performer: performer.clone(),
                    comment: "move".into(),
                    title_at_event: "Old".into(),
                    namespace_at_event: Some(0),
                    resulting_deleted: Some(false),
                },
            },
            Record::UserState {
                user_id: 2,
                timestamp_micros: 300,
                state: UserStateRecord {
                    current_name: Some("Editor".into()),
                    central_user_id: None,
                    account_class: AccountClass::Permanent,
                    groups: vec!["user".into()],
                    blocks: Vec::new(),
                    bot_by: Vec::new(),
                },
            },
            Record::UserAction {
                entity: EntityKey {
                    kind: EntityKind::User,
                    id: 2,
                },
                timestamp_micros: 200,
                action: UserActionRecord {
                    log_id: Some(2),
                    tie_sequence: 2,
                    kind: UserActionKind::Rename,
                    performer,
                    comment: "rename".into(),
                    historical_name: Some("Old editor".into()),
                    groups: Vec::new(),
                    blocks: Vec::new(),
                    bot_by: Vec::new(),
                    created_by: 0,
                    registration_timestamp_micros: None,
                    creation_timestamp_micros: None,
                    first_edit_timestamp_micros: None,
                },
            },
            Record::Manifest {
                timestamp_micros: 300,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "2026-07-30".into(),
                    metadata_snapshot: "2026-07".into(),
                    source_files: vec!["source".into()],
                },
            },
            Record::SiteInfo {
                timestamp_micros: 300,
                site_info: SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "testwiki".into(),
                    base: String::new(),
                    generator: String::new(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: String::new(),
                    script_path: String::new(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            },
            Record::Unknown {
                entity: EntityKey {
                    kind: EntityKind::Global,
                    id: 2,
                },
                timestamp_micros: 300,
                kind: 0x80,
                payload: vec![0, 1, 0xff, 2],
            },
        ];
        let source = tempfile::NamedTempFile::new().unwrap();
        let mut writer =
            ArchiveWriter::new(std::fs::File::create(source.path()).unwrap(), 64).unwrap();
        for record in &records {
            writer.write(record).unwrap();
        }
        writer.finish().unwrap();

        let mut raw = Vec::new();
        assert_eq!(
            export_raw_record_stream(source.path(), &mut raw).unwrap(),
            records.len() as u64,
        );
        assert_eq!(&raw[..8], &RAW_STREAM_MAGIC);
        let (archive, _, count) = import_raw_record_stream(
            Cursor::new(&raw),
            Vec::new(),
            64,
            CompressionSettings::default(),
        )
        .unwrap();
        assert_eq!(count, records.len() as u64);
        let mut reader = ArchiveReader::new(Cursor::new(archive)).unwrap();
        let mut decoded = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                decoded.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(decoded, records);

        let rebuilt = tempfile::NamedTempFile::new().unwrap();
        let mut writer =
            ArchiveWriter::new(std::fs::File::create(rebuilt.path()).unwrap(), 64).unwrap();
        for record in &decoded {
            writer.write(record).unwrap();
        }
        writer.finish().unwrap();
        let mut second_raw = Vec::new();
        export_raw_record_stream(rebuilt.path(), &mut second_raw).unwrap();
        assert_eq!(second_raw, raw);
    }

    #[test]
    fn raw_record_stream_requires_an_exact_completion_marker() {
        let source = tempfile::NamedTempFile::new().unwrap();
        let mut writer =
            ArchiveWriter::new(std::fs::File::create(source.path()).unwrap(), 64).unwrap();
        writer.write(&revision(1, 1, 10, b"text")).unwrap();
        writer.finish().unwrap();
        let mut raw = Vec::new();
        export_raw_record_stream(source.path(), &mut raw).unwrap();

        let mut truncated = raw.clone();
        truncated.pop();
        assert!(import_raw_record_stream(
            Cursor::new(truncated),
            Vec::new(),
            64,
            CompressionSettings::default(),
        )
        .is_err());
        raw.push(1);
        assert!(import_raw_record_stream(
            Cursor::new(raw),
            Vec::new(),
            64,
            CompressionSettings::default(),
        )
        .is_err());
    }

    #[test]
    fn round_trip_and_page_aligned_frames() {
        let mut writer = ArchiveWriter::new(Vec::new(), 1).unwrap();
        let records = vec![
            Record::PageState {
                page_id: 1,
                timestamp_micros: 20,
                title: "One".into(),
                namespace: None,
                deleted: false,
            },
            revision(1, 2, 20, b"new"),
            revision(1, 1, 10, b"old"),
            revision(2, 3, 30, b"other"),
        ];
        for record in &records {
            writer.write(record).unwrap();
        }
        let (bytes, frames) = writer.finish().unwrap();
        assert_eq!(frames, 2);

        let mut reader = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        let mut decoded = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            let info = frame.info();
            while let Some(record) = frame.next_record().unwrap() {
                assert!((info.first_entity..=info.last_entity).contains(&record.entity()));
                decoded.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(decoded, records);
        let Record::Revision { revision, .. } = &decoded[1] else {
            panic!("revision");
        };
        assert!(revision.meta.sha1.is_empty());
    }

    #[test]
    fn prefix_sampling_chooses_newest_revision_not_first_encountered() {
        let old = revision(7, 10, 10, b"old");
        let newest = revision(7, 12, 30, b"newest");
        let middle = revision(7, 11, 20, b"middle");
        let mut samples = NewestRevisionSamples::new(1 << 20).unwrap();
        for record in [&old, &newest, &middle] {
            samples.observe(record).unwrap();
        }
        samples
            .observe(&Record::PageState {
                page_id: 8,
                timestamp_micros: 40,
                title: "Next".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        let (actual, _) = samples.finish().unwrap();
        assert_eq!(actual, vec![encode_record_wire(&newest).unwrap()]);
    }

    #[test]
    fn merge_bootstraps_prefix_then_continues_same_sorted_stream() {
        let directory = tempfile::tempdir().unwrap();
        let input_path = directory.path().join("input.swdump");
        let mut input = ArchiveWriter::new(
            std::fs::File::create(&input_path).unwrap(),
            1 << 20,
        )
        .unwrap();
        let mut expected = Vec::new();
        for page_id in 1..=24 {
            let newest_text = (0..8192)
                .map(|offset| (page_id as u8).wrapping_add(offset as u8))
                .collect::<Vec<_>>();
            let records = [
                revision(page_id, page_id * 2, 20, &newest_text),
                revision(page_id, page_id * 2 - 1, 10, b"old"),
            ];
            for record in records {
                input.write(&record).unwrap();
                expected.push(record);
            }
        }
        input.finish().unwrap();

        let bootstrap = tempfile::tempfile_in(directory.path()).unwrap();
        let (output, _, records, stats) = merge_many_archives_bootstrapping_ref_prefix(
            &[input_path],
            Vec::new(),
            bootstrap,
            4096,
            CompressionSettings::default(),
            64 << 10,
            8 << 10,
        )
        .unwrap();
        assert_eq!(records, expected.len() as u64);
        assert_eq!(stats.ref_prefix_bytes, 8 << 10);

        let mut reader = ArchiveReader::new(Cursor::new(output)).unwrap();
        assert!(matches!(
            reader.reference,
            Some(CompressionReference::RefPrefix(_))
        ));
        let mut actual = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                actual.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(actual, expected);
    }

    #[test]
    fn repack_preserves_records_and_uses_requested_frame_target() {
        let records = vec![
            revision(1, 2, 20, &vec![b'a'; 8192]),
            revision(1, 1, 10, &vec![b'b'; 8192]),
            revision(2, 3, 30, &vec![b'c'; 8192]),
        ];
        let mut source = ArchiveWriter::new(Vec::new(), 1 << 20).unwrap();
        for record in &records {
            source.write(record).unwrap();
        }
        let (source, source_frames) = source.finish().unwrap();
        assert_eq!(source_frames, 1);

        let (repacked, stats) = repack(
            Cursor::new(source),
            Vec::new(),
            1,
            CompressionSettings {
                level: 7,
                checksum: true,
                ..CompressionSettings::default()
            },
        )
        .unwrap();
        assert_eq!(stats.input_frames, 1);
        assert_eq!(stats.output_frames, 2);
        assert_eq!(stats.records, records.len() as u64);

        let mut reader = ArchiveReader::new(Cursor::new(repacked)).unwrap();
        let mut decoded = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                decoded.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(decoded, records);
    }

    #[test]
    fn dictionary_repack_stores_dictionary_first_and_supports_random_reads() {
        let records = (1..=256)
            .map(|page_id| {
                let mut text = format!(
                    "== Article {page_id} ==\n{{{{Infobox|name=Article {page_id}}}}}\n"
                )
                .into_bytes();
                while text.len() < 4096 {
                    text.extend_from_slice(
                        b"Shared encyclopedia prose with [[links]], templates, and table markup. ",
                    );
                }
                revision(page_id, page_id, page_id as i64, &text)
            })
            .collect::<Vec<_>>();
        let mut source = ArchiveWriter::new(Vec::new(), 1 << 20).unwrap();
        for record in &records {
            source.write(record).unwrap();
        }
        let (source, _) = source.finish().unwrap();

        let (repacked, stats) = repack_with_dictionary(
            Cursor::new(source),
            Vec::new(),
            32 << 10,
            CompressionSettings {
                level: 7,
                ..CompressionSettings::default()
            },
            32 << 10,
        )
        .unwrap();
        assert_eq!(&repacked[FILE_HEADER_LEN..FILE_HEADER_LEN + 4], &DICTIONARY_MAGIC);
        assert!(stats.dictionary_bytes > 0);
        assert!(stats.dictionary_bytes <= 32 << 10);
        assert!(stats.compressed_dictionary_bytes > 0);

        let mut reader = ArchiveReader::new(Cursor::new(&repacked)).unwrap();
        let mut decoded = Vec::new();
        let mut dictionary_frames = 0;
        while let Some(mut frame) = reader.next_frame().unwrap() {
            dictionary_frames += u64::from(frame.info().dictionary_id.is_some());
            while let Some(record) = frame.next_record().unwrap() {
                decoded.push(record);
            }
        }
        assert!(reader.is_complete());
        assert!(dictionary_frames > 0);
        assert_eq!(decoded, records);

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &repacked).unwrap();
        let (_, frames, complete) = index_file(file.path()).unwrap();
        assert!(complete);
        assert!(!frames.is_empty());
        let first = frames
            .iter()
            .find(|frame| frame.info.dictionary_id.is_some())
            .expect("at least one frame uses the dictionary");
        let mut compressed = vec![0; first.info.compressed_bytes as usize];
        let mut source = std::fs::File::open(file.path()).unwrap();
        source.seek(SeekFrom::Start(first.compressed_offset)).unwrap();
        source.read_exact(&mut compressed).unwrap();
        assert_eq!(
            zstd::zstd_safe::get_dict_id_from_frame(&compressed).map(u32::from),
            first.info.dictionary_id
        );
        let mut random = Vec::new();
        visit_frame(file.path(), first, |record| {
            random.push(record);
            Ok(())
        })
        .unwrap();
        assert!(!random.is_empty());

        let mut mismatched = repacked;
        let header = first.compressed_offset as usize - FRAME_HEADER_LEN;
        mismatched[header + 56..header + 60].fill(0);
        let mut reader = ArchiveReader::new(Cursor::new(mismatched)).unwrap();
        assert!(matches!(
            reader.next_frame(),
            Err(ArchiveError::Invalid(
                "data frame dictionary header does not match zstd frame"
            ))
        ));
    }

    #[test]
    fn ref_prefix_repack_stores_prefix_first_and_supports_random_reads() {
        let records = (1..=384_u64)
            .map(|page_id| {
                let mut text = format!(
                    "Page {page_id}: shared encyclopedia prose, [[links]], {{{{templates}}}}, \
                     tables, dates, and Latvian words. "
                )
                .into_bytes();
                let seed = xxhash_rust::xxh3::xxh3_64(&page_id.to_le_bytes());
                while text.len() < 4096 {
                    text.extend_from_slice(&seed.to_le_bytes());
                    text.extend_from_slice(
                        b" encyclopedia revision text with recurring MediaWiki syntax ",
                    );
                }
                revision(page_id, page_id, page_id as i64, &text)
            })
            .collect::<Vec<_>>();
        let mut source = ArchiveWriter::new(Vec::new(), 1 << 20).unwrap();
        for record in &records {
            source.write(record).unwrap();
        }
        let (source, _) = source.finish().unwrap();

        let (repacked, stats) = repack_with_ref_prefix(
            Cursor::new(source),
            Vec::new(),
            32 << 10,
            CompressionSettings {
                level: 7,
                ..CompressionSettings::default()
            },
            1 << 20,
            64 << 10,
        )
        .unwrap();
        assert_eq!(
            &repacked[FILE_HEADER_LEN..FILE_HEADER_LEN + 4],
            &REF_PREFIX_MAGIC
        );
        assert_eq!(stats.ref_prefix_bytes, 64 << 10);
        assert!(stats.compressed_ref_prefix_bytes > 0);
        assert!(stats.sample_bytes <= 1 << 20);

        let mut reader = ArchiveReader::new(Cursor::new(&repacked)).unwrap();
        let mut decoded = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            assert_eq!(frame.info().dictionary_id, None);
            while let Some(record) = frame.next_record().unwrap() {
                decoded.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(decoded, records);

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &repacked).unwrap();
        let (_, frames, complete) = index_file(file.path()).unwrap();
        assert!(complete);
        let middle = &frames[frames.len() / 2];
        let mut random = Vec::new();
        visit_frame(file.path(), middle, |record| {
            random.push(record);
            Ok(())
        })
        .unwrap();
        assert!(!random.is_empty());
    }

    #[test]
    fn frames_never_mix_page_user_and_global_records() {
        let records = [
            revision(1, 1, 10, b"page"),
            Record::UserState {
                user_id: 2,
                timestamp_micros: 10,
                state: UserStateRecord {
                    current_name: Some("Editor".into()),
                    central_user_id: None,
                    account_class: AccountClass::Permanent,
                    groups: Vec::new(),
                    blocks: Vec::new(),
                    bot_by: Vec::new(),
                },
            },
            Record::Manifest {
                timestamp_micros: 10,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "2026-07-29".into(),
                    metadata_snapshot: "2026-07".into(),
                    source_files: Vec::new(),
                },
            },
        ];
        let mut writer = ArchiveWriter::new(Vec::new(), 1 << 20).unwrap();
        for record in &records {
            writer.write(record).unwrap();
        }
        let (bytes, frames) = writer.finish().unwrap();
        assert_eq!(frames, 3);

        let mut reader = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        for expected in [EntityKind::Page, EntityKind::User, EntityKind::Global] {
            let mut frame = reader.next_frame().unwrap().unwrap();
            let info = frame.info();
            assert_eq!(info.first_entity.kind, expected);
            assert_eq!(info.last_entity.kind, expected);
            while let Some(record) = frame.next_record().unwrap() {
                assert_eq!(record.entity().kind, expected);
                assert!(
                    (info.first_entity.id..=info.last_entity.id).contains(&record.entity().id)
                );
            }
        }
        assert!(reader.next_frame().unwrap().is_none());
        assert!(reader.is_complete());
    }

    #[test]
    fn rejects_bad_order() {
        let mut writer = ArchiveWriter::new(Vec::new(), 1024).unwrap();
        writer.write(&revision(2, 1, 10, b"x")).unwrap();
        assert!(matches!(
            writer.write(&revision(1, 2, 20, b"x")),
            Err(ArchiveError::OutOfOrder { .. })
        ));
    }

    #[test]
    fn sealed_prefix_survives_truncated_tail() {
        let mut writer = ArchiveWriter::new(Vec::new(), 1).unwrap();
        writer.write(&revision(1, 1, 10, b"first")).unwrap();
        writer.write(&revision(2, 2, 10, b"second")).unwrap();
        let (mut bytes, _) = writer.finish().unwrap();
        bytes.truncate(bytes.len() - 10);

        let mut reader = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        let mut first = reader.next_frame().unwrap().unwrap();
        assert_eq!(first.next_record().unwrap().unwrap().page_id(), Some(1));
        assert!(first.next_record().unwrap().is_none());
        drop(first);
        let mut second = reader.next_frame().unwrap().unwrap();
        assert_eq!(second.next_record().unwrap().unwrap().page_id(), Some(2));
        assert!(second.next_record().unwrap().is_none());
        drop(second);
        assert!(reader.next_frame().is_err());
    }

    #[test]
    fn user_events_round_trip_under_user_key() {
        let record = Record::UserAction {
            entity: EntityKey {
                kind: EntityKind::User,
                id: 42,
            },
            timestamp_micros: 123,
            action: UserActionRecord {
                log_id: Some(9),
                tie_sequence: 9,
                kind: UserActionKind::Rename,
                performer: PerformerRecord {
                    local_user_id: Some(7),
                    central_user_id: None,
                    historical_name: Some("Admin".into()),
                    account_class: AccountClass::Permanent,
                },
                comment: String::new(),
                historical_name: Some("Old name".into()),
                groups: Vec::new(),
                blocks: Vec::new(),
                bot_by: Vec::new(),
                created_by: 0,
                registration_timestamp_micros: None,
                creation_timestamp_micros: None,
                first_edit_timestamp_micros: None,
            },
        };
        let mut writer = ArchiveWriter::new(Vec::new(), 1024).unwrap();
        writer.write(&record).unwrap();
        let (bytes, _) = writer.finish().unwrap();
        let mut reader = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        let mut frame = reader.next_frame().unwrap().unwrap();
        assert_eq!(frame.next_record().unwrap(), Some(record));
        assert!(frame.next_record().unwrap().is_none());
    }

    #[test]
    fn frame_index_supports_independent_ordered_filters() {
        let mut writer = ArchiveWriter::new(Vec::new(), 1).unwrap();
        let records = vec![
            revision(1, 1, 10, b"one"),
            revision(2, 2, 20, b"two"),
            revision(3, 3, 30, b"three"),
        ];
        for record in &records {
            writer.write(record).unwrap();
        }
        let (bytes, _) = writer.finish().unwrap();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        file.flush().unwrap();
        let (_, frames, complete) = index_file(file.path()).unwrap();
        assert!(complete);
        assert!(frames.len() >= 2);
        let mut filtered = Vec::new();
        for frame in &frames {
            visit_frame(file.path(), frame, |record| {
                filtered.push(record);
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(filtered, records);
    }

    #[test]
    fn record_sorter_orders_user_groups_and_time() {
        let temporary = tempfile::TempDir::new().unwrap();
        let mut sorter = RecordSorter::new_in(temporary.path()).unwrap();
        for (user, timestamp, ordinal) in [(2, 10, 1), (1, 10, 2), (1, 20, 3)] {
            sorter
                .push(Record::UserAction {
                    entity: EntityKey {
                        kind: EntityKind::User,
                        id: user,
                    },
                    timestamp_micros: timestamp,
                    action: UserActionRecord {
                        log_id: None,
                        tie_sequence: ordinal,
                        kind: UserActionKind::Rename,
                        performer: PerformerRecord {
                            local_user_id: None,
                            central_user_id: None,
                            historical_name: None,
                            account_class: AccountClass::Unknown,
                        },
                        comment: String::new(),
                        historical_name: None,
                        groups: Vec::new(),
                        blocks: Vec::new(),
                        bot_by: Vec::new(),
                        created_by: 0,
                        registration_timestamp_micros: None,
                        creation_timestamp_micros: None,
                        first_edit_timestamp_micros: None,
                    },
                })
                .unwrap();
            sorter.flush_run().unwrap();
        }
        let (bytes, _, events) = sorter.finish(Vec::new(), 1024).unwrap();
        assert_eq!(events, 3);
        let mut reader = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        let mut keys = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                keys.push((record.entity(), record.timestamp_micros()));
            }
        }
        assert_eq!(
            keys,
            [
                (
                    EntityKey {
                        kind: EntityKind::User,
                        id: 1
                    },
                    20
                ),
                (
                    EntityKey {
                        kind: EntityKind::User,
                        id: 1
                    },
                    10
                ),
                (
                    EntityKey {
                        kind: EntityKind::User,
                        id: 2
                    },
                    10
                ),
            ]
        );
    }

    #[test]
    fn record_sorter_hierarchically_merges_more_runs_than_open_file_limit() {
        let temporary = tempfile::TempDir::new().unwrap();
        let mut sorter = RecordSorter::new_in(temporary.path()).unwrap();
        for page_id in (1..=130).rev() {
            sorter
                .push(revision(page_id, page_id, page_id as i64, b"x"))
                .unwrap();
            sorter.flush_run().unwrap();
        }
        let (bytes, _, _) = sorter.finish(Vec::new(), 1024).unwrap();
        let mut archive = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        let mut page_ids = Vec::new();
        while let Some(mut frame) = archive.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                page_ids.push(record.page_id().unwrap());
            }
        }
        assert_eq!(page_ids, (1..=130).collect::<Vec<_>>());
        assert!(archive.is_complete());
    }

    #[test]
    fn compressed_segments_concatenate_and_archives_merge() {
        let temporary = tempfile::TempDir::new().unwrap();
        let first = temporary.path().join("first.swdump");
        let second = temporary.path().join("second.swdump");
        for (path, record) in [
            (&first, revision(1, 1, 10, b"one")),
            (&second, revision(2, 2, 20, b"two")),
        ] {
            let mut writer = ArchiveWriter::new(std::fs::File::create(path).unwrap(), 1).unwrap();
            writer.write(&record).unwrap();
            writer.finish().unwrap();
        }
        let joined = temporary.path().join("joined.swdump");
        concatenate_archives(&[first, second], std::fs::File::create(&joined).unwrap(), 1).unwrap();
        let history = temporary.path().join("history.swdump");
        let mut writer =
            ArchiveWriter::new(std::fs::File::create(&history).unwrap(), 1024).unwrap();
        writer
            .write(&Record::PageAction {
                entity: EntityKey {
                    kind: EntityKind::Page,
                    id: 1,
                },
                timestamp_micros: 9,
                action: PageActionRecord {
                    log_id: None,
                    tie_sequence: 1,
                    kind: PageActionKind::Move,
                    performer: PerformerRecord {
                        local_user_id: None,
                        central_user_id: None,
                        historical_name: None,
                        account_class: AccountClass::Unknown,
                    },
                    comment: String::new(),
                    title_at_event: "One".into(),
                    namespace_at_event: Some(0),
                    resulting_deleted: Some(false),
                },
            })
            .unwrap();
        writer.finish().unwrap();
        let merged = temporary.path().join("merged.swdump");
        merge_archives(
            &joined,
            &history,
            std::fs::File::create(&merged).unwrap(),
            1024,
        )
        .unwrap();
        let mut reader = ArchiveRecordReader::open(&merged).unwrap();
        let mut keys = Vec::new();
        while let Some(record) = reader.next_record().unwrap() {
            keys.push((record.entity(), record.timestamp_micros()));
        }
        assert_eq!(
            keys,
            [
                (
                    EntityKey {
                        kind: EntityKind::Page,
                        id: 1
                    },
                    10
                ),
                (
                    EntityKey {
                        kind: EntityKind::Page,
                        id: 1
                    },
                    9
                ),
                (
                    EntityKey {
                        kind: EntityKind::Page,
                        id: 2
                    },
                    20
                ),
            ]
        );
    }

    #[test]
    fn merge_coalesces_typed_revision_annotation_with_xml_text() {
        let temporary = tempfile::TempDir::new().unwrap();
        let content = temporary.path().join("content.swdump");
        let metadata = temporary.path().join("metadata.swdump");
        let base = revision(7, 11, 123, b"archived text");
        let Record::Revision {
            revision: base_revision,
            ..
        } = &base
        else {
            unreachable!()
        };
        let mut shell = base_revision.clone();
        shell.has_text = false;
        shell.text.clear();
        shell.history = Some(RevisionHistoryRecord {
            minor: Some(true),
            content_model: Some("wikitext".into()),
            content_format: Some("text/x-wiki".into()),
            identity_reverted: Some(false),
            first_reverting_revision_id: None,
            seconds_to_revert: None,
            identity_revert: Some(false),
            before_page_creation: Some(false),
            tags: vec!["visualeditor".into()],
        });
        for (path, record) in [
            (&content, base),
            (
                &metadata,
                Record::Revision {
                    page_id: 7,
                    revision: shell,
                },
            ),
        ] {
            let mut writer =
                ArchiveWriter::new(std::fs::File::create(path).unwrap(), 1024).unwrap();
            writer.write(&record).unwrap();
            writer.finish().unwrap();
        }
        let merged = temporary.path().join("merged.swdump");
        merge_archives(
            &content,
            &metadata,
            std::fs::File::create(&merged).unwrap(),
            1024,
        )
        .unwrap();
        let mut reader = ArchiveRecordReader::open(&merged).unwrap();
        let Record::Revision { revision, .. } = reader.next_record().unwrap().unwrap() else {
            panic!("revision")
        };
        assert!(revision.has_text);
        assert_eq!(revision.text, b"archived text");
        assert_eq!(
            revision.history.unwrap().tags,
            ["visualeditor".to_string()]
        );
        assert!(reader.next_record().unwrap().is_none());
    }

    #[test]
    fn merge_is_commutative_associative_and_idempotent() {
        let temporary = tempfile::TempDir::new().unwrap();
        let action = |tie_sequence| Record::PageAction {
            entity: EntityKey {
                kind: EntityKind::Page,
                id: 7,
            },
            timestamp_micros: 122,
            action: PageActionRecord {
                log_id: Some(44),
                tie_sequence,
                kind: PageActionKind::Move,
                performer: PerformerRecord {
                    local_user_id: Some(3),
                    central_user_id: None,
                    historical_name: Some("Mover".into()),
                    account_class: AccountClass::Permanent,
                },
                comment: if tie_sequence == 2 {
                    "move with normalized detail".into()
                } else {
                    "move".into()
                },
                title_at_event: "Old".into(),
                namespace_at_event: Some(0),
                resulting_deleted: Some(false),
            },
        };
        let content = revision(7, 11, 123, b"archived text");
        let shell = |minor, tag: &str| {
            let Record::Revision {
                revision: mut shell,
                ..
            } = revision(7, 11, 123, b"archived text")
            else {
                unreachable!()
            };
            shell.has_text = false;
            shell.text.clear();
            shell.history = Some(RevisionHistoryRecord {
                minor: Some(minor),
                content_model: Some("wikitext".into()),
                content_format: Some("text/x-wiki".into()),
                identity_reverted: None,
                first_reverting_revision_id: None,
                seconds_to_revert: None,
                identity_revert: None,
                before_page_creation: None,
                tags: vec![tag.into()],
            });
            Record::Revision {
                page_id: 7,
                revision: shell,
            }
        };
        let a = temporary.path().join("a.swdump");
        let b = temporary.path().join("b.swdump");
        let c = temporary.path().join("c.swdump");
        write_test_archive(&a, &[content, action(9)]);
        write_test_archive(&b, &[shell(false, "b"), action(2)]);
        write_test_archive(&c, &[shell(true, "a"), revision(8, 12, 124, b"next")]);

        let ab = temporary.path().join("ab.swdump");
        let ba = temporary.path().join("ba.swdump");
        merge_test_archives(&[a.clone(), b.clone()], &ab);
        merge_test_archives(&[b.clone(), a.clone()], &ba);
        assert_eq!(std::fs::read(&ab).unwrap(), std::fs::read(&ba).unwrap());

        let aa = temporary.path().join("aa.swdump");
        merge_test_archives(&[a.clone(), a.clone()], &aa);
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&aa).unwrap());

        let ab_c = temporary.path().join("ab-c.swdump");
        merge_test_archives(&[ab, c.clone()], &ab_c);
        let bc = temporary.path().join("bc.swdump");
        merge_test_archives(&[b, c], &bc);
        let a_bc = temporary.path().join("a-bc.swdump");
        merge_test_archives(&[a, bc], &a_bc);
        assert_eq!(
            std::fs::read(&ab_c).unwrap(),
            std::fs::read(&a_bc).unwrap()
        );

        let mut reader = ArchiveRecordReader::open(ab_c).unwrap();
        let Record::Revision { revision, .. } = reader.next_record().unwrap().unwrap() else {
            panic!("revision")
        };
        let history = revision.history.unwrap();
        assert_eq!(history.minor, Some(true));
        assert_eq!(history.tags, ["a".to_string(), "b".to_string()]);
        let Record::PageAction { action, .. } = reader.next_record().unwrap().unwrap() else {
            panic!("action")
        };
        assert_eq!(action.tie_sequence, 2);
        assert_eq!(action.comment, "move with normalized detail");
    }

    #[test]
    fn streaming_merge_reuses_reference_prefix() {
        let temporary = tempfile::TempDir::new().unwrap();
        let prefix = vec![b'x'; 1024];
        let base = temporary.path().join("base.swdump");
        let mut base_writer = ArchiveWriter::with_compression_and_ref_prefix(
            std::fs::File::create(&base).unwrap(),
            1024,
            CompressionSettings::default(),
            &prefix,
        )
        .unwrap();
        base_writer
            .write(&revision(7, 11, 123, b"archived text"))
            .unwrap();
        base_writer.finish().unwrap();

        let update = temporary.path().join("update.swdump");
        write_test_archive(&update, &[revision(8, 12, 124, b"new page")]);
        let output = temporary.path().join("merged.swdump");
        let (_, frames, records) = merge_many_archives_reusing_ref_prefix(
            &base,
            &[base.clone(), update],
            std::fs::File::create(&output).unwrap(),
            1024,
            CompressionSettings::default(),
        )
        .unwrap();
        assert_eq!(records, 2);
        assert!(frames > 0);
        let (_, locations, complete) = index_file(output).unwrap();
        assert!(complete);
        assert!(locations.iter().all(|location| {
            matches!(
                location.reference.as_ref(),
                Some(CompressionReference::RefPrefix(stored)) if stored.as_ref() == prefix
            )
        }));
    }

    #[test]
    fn page_history_is_streamed_without_splitting_its_frame() {
        let prefix = vec![b'p'; 64 << 10];
        let compression = CompressionSettings {
            level: 1,
            ..CompressionSettings::default()
        };
        let mut writer = StreamingArchiveWriter::new(
            Vec::new(),
            128 << 10,
            compression,
            &prefix,
            2,
        )
        .unwrap();
        let large = vec![b'x'; (32 << 20) + 1];
        writer.write(&revision(1, 2, 20, &large)).unwrap();
        assert!(
            writer.context.get_frame_progression().currentJobID > 1,
            "a large streaming frame must be divided among zstd workers"
        );
        writer.write(&revision(1, 1, 10, b"old")).unwrap();
        let (archive, frames) = writer.finish().unwrap();
        assert_eq!(frames, 1);

        let mut reader = ArchiveReader::new(Cursor::new(archive)).unwrap();
        let mut frame = reader.next_frame().unwrap().unwrap();
        let first = frame.next_record().unwrap().unwrap();
        let second = frame.next_record().unwrap().unwrap();
        assert!(frame.next_record().unwrap().is_none());
        drop(frame);
        assert!(reader.next_frame().unwrap().is_none());
        assert!(reader.is_complete());
        assert!(matches!(
            first,
            Record::Revision {
                revision: RevisionRecord { meta, .. },
                ..
            } if meta.rev_id == 2 && meta.text_len == large.len() as u64
        ));
        assert!(matches!(
            second,
            Record::Revision {
                revision: RevisionRecord { meta, .. },
                ..
            } if meta.rev_id == 1
        ));
    }

    #[test]
    fn merge_resume_reuses_prefix_after_the_last_sealed_entity_range() {
        let directory = tempfile::tempdir().unwrap();
        let input_path = directory.path().join("input.swdump");
        let records = (1..=128_u64)
            .map(|page_id| {
                let text = vec![page_id as u8; 4096];
                revision(page_id, page_id, page_id as i64, &text)
            })
            .collect::<Vec<_>>();
        let mut input =
            ArchiveWriter::new(std::fs::File::create(&input_path).unwrap(), 1024).unwrap();
        for record in &records {
            input.write(record).unwrap();
        }
        input.finish().unwrap();

        let name = "assembly-test.partial";
        let output = crate::archive_set::ArchiveSetOutput::resumable_in(
            directory.path(),
            name,
            4096,
        )
        .unwrap();
        let bootstrap = tempfile::tempfile_in(directory.path()).unwrap();
        let (output, _, _, _) = merge_many_archives_bootstrapping_ref_prefix(
            &[input_path.clone()],
            output,
            bootstrap,
            256,
            CompressionSettings::default(),
            64 << 10,
            4 << 10,
        )
        .unwrap();
        drop(output.finish().unwrap());

        let partial = directory.path().join(name);
        std::fs::remove_file(partial.join("9999-complete.swdump-part")).unwrap();
        let mut ranges = std::fs::read_dir(&partial)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with("1000-p"))
            .collect::<Vec<_>>();
        ranges.sort();
        assert!(ranges.len() > 1);
        std::fs::remove_file(partial.join(ranges.pop().unwrap())).unwrap();

        let output = crate::archive_set::ArchiveSetOutput::resumable_in(
            directory.path(),
            name,
            4096,
        )
        .unwrap();
        let resume_after = output.resume_after().unwrap();
        let prefix = output.preserved_ref_prefix().unwrap().unwrap();
        let sources: Vec<Box<dyn RecordSource>> = vec![Box::new(
            ArchiveRecordReader::open(&input_path).unwrap(),
        )];
        let mut observed = 0_u64;
        let (output, _, _, _) = merge_record_sources_reusing_ref_prefix_observing_after(
            sources,
            output,
            256,
            CompressionSettings::default(),
            &prefix,
            Some(resume_after),
            |_| observed += 1,
        )
        .unwrap();
        assert_eq!(observed, records.len() as u64);
        let destination = directory.path().join("complete.swdump");
        output.finish().unwrap().persist(&destination).unwrap();

        let mut reader = ArchiveReader::new(
            crate::archive_set::ArchiveSetReader::open(destination).unwrap(),
        )
        .unwrap();
        let mut decoded = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                decoded.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(decoded, records);
    }

    #[cfg(unix)]
    #[test]
    fn many_archive_merge_survives_low_descriptor_limit() {
        const CHILD_ROOT: &str = "WIKIMAK_LOW_DESCRIPTOR_MERGE_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
            limit.rlim_cur = 48;
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);

            let mut inputs = std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            inputs.sort();
            let (_, frames, records) =
                merge_many_archives(&inputs, Vec::new(), 1024).unwrap();
            assert!(frames > 0);
            assert_eq!(records, 96);
            return;
        }

        let temporary = tempfile::tempdir().unwrap();
        for index in 0..96_u64 {
            let path = temporary.path().join(format!("{index:03}.swdump"));
            write_test_archive(&path, &[revision(index + 1, index + 1, 1, b"text")]);
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("archive::tests::many_archive_merge_survives_low_descriptor_limit")
            .arg("--nocapture")
            .env(CHILD_ROOT, temporary.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(std::fs::read_dir(temporary.path())
            .unwrap()
            .all(|entry| entry.unwrap().file_type().unwrap().is_file()));
    }

    #[test]
    fn hierarchical_merge_cleans_intermediates_after_error() {
        let temporary = tempfile::tempdir().unwrap();
        let mut inputs = Vec::new();
        for index in 0..25_u64 {
            let path = temporary.path().join(format!("{index:03}.swdump"));
            write_test_archive(&path, &[revision(index + 1, index + 1, 1, b"text")]);
            inputs.push(path);
        }
        std::fs::write(&inputs[7], b"not an archive").unwrap();
        assert!(merge_many_archives(&inputs, Vec::new(), 1024).is_err());
        assert!(std::fs::read_dir(temporary.path())
            .unwrap()
            .all(|entry| entry.unwrap().file_type().unwrap().is_file()));
    }

    #[test]
    fn compressed_frame_copy_preserves_bytes_and_output_frontier() {
        let temporary = tempfile::tempdir().unwrap();
        let source_path = temporary.path().join("source.swdump");
        let prefix = b"compressed frame copy reference prefix";
        let mut source = StreamingArchiveWriter::new(
            std::fs::File::create(&source_path).unwrap(),
            1,
            CompressionSettings::default(),
            prefix,
            1,
        )
        .unwrap();
        let page_one = revision(1, 1, 1, b"one");
        let page_three = revision(3, 3, 3, b"three");
        source.write(&page_one).unwrap();
        source.write(&page_three).unwrap();
        source.finish().unwrap();
        let (_, locations, complete) = index_file(&source_path).unwrap();
        assert!(complete);
        assert_eq!(locations.len(), 2);

        let mut source_file = std::fs::File::open(&source_path).unwrap();
        let mut target = ParallelArchiveWriter::new(
            Vec::new(),
            1,
            CompressionSettings::default(),
            prefix,
            1,
        )
        .unwrap();
        assert_eq!(
            target
                .append_compressed_frame(
                    &mut source_file,
                    crate::frame_directory::FrameDirectoryEntry::from(&locations[0]),
                )
                .unwrap()
                .records,
            1
        );
        let page_two = revision(2, 2, 2, b"two");
        target.write(&page_two).unwrap();
        target
            .append_compressed_frame(
                &mut source_file,
                crate::frame_directory::FrameDirectoryEntry::from(&locations[1]),
            )
            .unwrap();
        let (bytes, frames) = target.finish().unwrap();
        assert_eq!(frames, 3);
        let mut reader = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        let mut actual = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                actual.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(actual, [page_one.clone(), page_two, page_three]);

        let mut bad_target = ParallelArchiveWriter::new(
            Vec::new(),
            1,
            CompressionSettings::default(),
            prefix,
            1,
        )
        .unwrap();
        bad_target.write(&page_one).unwrap();
        bad_target.write(&revision(4, 4, 4, b"four")).unwrap();
        assert!(matches!(
            bad_target.append_compressed_frame(
                &mut source_file,
                crate::frame_directory::FrameDirectoryEntry::from(&locations[1]),
            ),
            Err(ArchiveError::Invalid("copied frame is not after the output frontier"))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn directory_lease_survives_rename_and_defers_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let installed = temporary.path().join("installed.swdump");
        let displaced = temporary.path().join("displaced.swdump");
        std::fs::create_dir(&installed).unwrap();
        std::fs::write(installed.join("1000-p0000000001-p0000000001.swdump-part"), b"old")
            .unwrap();

        let root = std::fs::File::open(&installed).unwrap();
        lock_archive_shared(&root).unwrap();
        std::fs::rename(&installed, &displaced).unwrap();
        std::fs::create_dir(&installed).unwrap();
        std::fs::write(
            installed.join("1000-p0000000001-p0000000001.swdump-part"),
            b"new",
        )
        .unwrap();

        let mut old_child =
            open_archive_child(&root, "1000-p0000000001-p0000000001.swdump-part").unwrap();
        let mut bytes = Vec::new();
        old_child.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"old");
        assert!(try_acquire_archive_cleanup_lease(&displaced)
            .unwrap()
            .is_none());
        drop(root);
        assert!(try_acquire_archive_cleanup_lease(&displaced)
            .unwrap()
            .is_some());
    }

    #[test]
    fn parallel_archive_writer_preserves_frame_and_record_order() {
        let prefix = b"parallel archive reference prefix";
        let mut writer = ParallelArchiveWriter::new(
            Vec::new(),
            1,
            CompressionSettings {
                level: 1,
                ..CompressionSettings::default()
            },
            prefix,
            4,
        )
        .unwrap();
        let mut expected = Vec::new();
        for page_id in 1..=64_u64 {
            for revision_id in (1..=3_u64).rev() {
                let record = revision(
                    page_id,
                    page_id * 10 + revision_id,
                    revision_id as i64,
                    format!("page {page_id} revision {revision_id}").as_bytes(),
                );
                writer.write(&record).unwrap();
                expected.push(record);
            }
        }
        let (bytes, frames) = writer.finish().unwrap();
        assert_eq!(frames, 64);

        let mut reader = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        let mut actual = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                actual.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(actual, expected);
    }

    #[test]
    fn parallel_archive_writer_orders_copied_changed_and_copied_frames() {
        let temporary = tempfile::tempdir().unwrap();
        let source_path = temporary.path().join("source.swdump");
        let prefix = b"parallel mixed-frame reference prefix";
        let page_one = revision(1, 1, 1, b"one");
        let page_two = revision(2, 2, 2, b"two changed");
        let page_three = revision(3, 3, 3, b"three");
        let mut source = StreamingArchiveWriter::new(
            std::fs::File::create(&source_path).unwrap(),
            1,
            CompressionSettings::default(),
            prefix,
            1,
        )
        .unwrap();
        source.write(&page_one).unwrap();
        source.write(&page_three).unwrap();
        source.finish().unwrap();
        let (_, source_frames, complete) = index_file(&source_path).unwrap();
        assert!(complete);
        assert_eq!(source_frames.len(), 2);

        let mut source_file = std::fs::File::open(&source_path).unwrap();
        let mut writer = ParallelArchiveWriter::new(
            Vec::new(),
            1,
            CompressionSettings {
                level: 1,
                ..CompressionSettings::default()
            },
            prefix,
            2,
        )
        .unwrap();
        let first = writer
            .append_compressed_frame(
                &mut source_file,
                crate::frame_directory::FrameDirectoryEntry::from(&source_frames[0]),
            )
            .unwrap();
        assert_eq!(first.frames, 1);
        assert_eq!(first.records, 1);
        assert!(writer.buffered_frames() <= writer.buffered_frame_limit());
        writer.write(&page_two).unwrap();
        assert!(writer.buffered_frames() <= writer.buffered_frame_limit());
        let last = writer
            .append_compressed_frame(
                &mut source_file,
                crate::frame_directory::FrameDirectoryEntry::from(&source_frames[1]),
            )
            .unwrap();
        assert_eq!(last.frames, 1);
        assert_eq!(last.records, 1);
        assert!(writer.buffered_frames() <= writer.buffered_frame_limit());
        let (bytes, frames) = writer.finish().unwrap();
        assert_eq!(frames, 3);

        let (_, target_frames, complete) = index_reader(Cursor::new(&bytes)).unwrap();
        assert!(complete);
        assert_eq!(target_frames.len(), 3);
        let source_bytes = std::fs::read(&source_path).unwrap();
        for (source, target) in [
            (&source_frames[0], &target_frames[0]),
            (&source_frames[1], &target_frames[2]),
        ] {
            let source_start = source.compressed_offset as usize;
            let source_end = source_start + source.info.compressed_bytes as usize;
            let target_start = target.compressed_offset as usize;
            let target_end = target_start + target.info.compressed_bytes as usize;
            assert_eq!(
                &source_bytes[source_start..source_end],
                &bytes[target_start..target_end],
            );
        }

        let mut reader = ArchiveReader::new(Cursor::new(bytes)).unwrap();
        let mut actual = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                actual.push(record);
            }
        }
        assert!(reader.is_complete());
        assert_eq!(actual, [page_one, page_two, page_three]);
    }

    fn write_test_archive(path: &Path, records: &[Record]) {
        let mut writer =
            ArchiveWriter::new(std::fs::File::create(path).unwrap(), 1024).unwrap();
        for record in records {
            writer.write(record).unwrap();
        }
        writer.finish().unwrap();
    }

    fn merge_test_archives(inputs: &[PathBuf], output: &Path) {
        merge_many_archives(
            inputs,
            std::fs::File::create(output).unwrap(),
            1024,
        )
        .unwrap();
    }
}
