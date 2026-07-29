//! Portable, layout-independent Wikipedia event stream.
//!
//! The outer file is a short header followed by independently compressed
//! frames. Frames end only between page ids. Records are ordered by ascending
//! page id and, within a page, descending event time. This is deliberately not
//! a depot format: it is a compact source for experiments, conversions, and
//! recovery without depending on the current live storage layout.

use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Take, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::{ContributorMeta, Instance, RevisionMeta};

const FILE_MAGIC: [u8; 8] = *b"SWDUMP\0\0";
const FILE_VERSION: u32 = 1;
const FILE_HEADER_LEN: usize = 24;
const FRAME_MAGIC: [u8; 4] = *b"FRM1";
const DONE_MAGIC: [u8; 4] = *b"DONE";
const FRAME_HEADER_LEN: usize = 64;
pub const DEFAULT_FRAME_TARGET: usize = 4 << 20;

const KIND_PAGE_STATE: u8 = 1;
const KIND_REVISION: u8 = 2;
const KIND_PAGE_ACTION: u8 = 3;
const KIND_USER_STATE: u8 = 4;
const KIND_USER_ACTION: u8 = 5;
const KIND_MANIFEST: u8 = 6;
const KIND_SITE_INFO: u8 = 7;
const PAGE_TEXT_MEMORY_LIMIT: usize = 16 << 20;
const HISTORY_SORT_RUN_BYTES: usize = 64 << 20;
const SORT_MERGE_FAN_IN: usize = 64;

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
pub struct SiteInfoRecord {
    pub site_name: String,
    pub db_name: String,
    pub base: String,
    pub generator: String,
    pub case: String,
    pub namespaces: Vec<SiteNamespaceRecord>,
    pub interwiki: Vec<SiteInterwikiRecord>,
}

struct PendingRecord {
    record: Record,
}

/// Bounded external sorter for typed archive records.
pub(crate) struct RecordSorter {
    temporary: tempfile::TempDir,
    buffered: Vec<PendingRecord>,
    buffered_bytes: usize,
    runs: Vec<std::path::PathBuf>,
}

impl RecordSorter {
    pub(crate) fn new_in(root: &Path) -> Result<Self> {
        Ok(Self {
            temporary: tempfile::TempDir::new_in(root)?,
            buffered: Vec::new(),
            buffered_bytes: 0,
            runs: Vec::new(),
        })
    }

    pub(crate) fn push(&mut self, record: Record) -> Result<()> {
        let (_, payload) = record_wire_size(&record)?;
        self.buffered_bytes = self
            .buffered_bytes
            .saturating_add(usize::try_from(payload).unwrap_or(usize::MAX))
            .saturating_add(32);
        self.buffered.push(PendingRecord { record });
        if self.buffered_bytes >= HISTORY_SORT_RUN_BYTES {
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
        encoder.finish()?.sync_all()?;
        self.runs.push(path);
        self.buffered_bytes = 0;
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
        current_title: String,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameInfo {
    pub first_entity: EntityKey,
    pub last_entity: EntityKey,
    pub records: u64,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameLocation {
    pub info: FrameInfo,
    pub compressed_offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompressionSettings {
    pub level: i32,
    pub checksum: bool,
    pub long_distance_matching: bool,
    pub window_log: Option<u32>,
    pub target_block_size: Option<u32>,
}

impl Default for CompressionSettings {
    fn default() -> Self {
        Self {
            level: 3,
            checksum: false,
            long_distance_matching: false,
            window_log: None,
            target_block_size: None,
        }
    }
}

struct FrameBuilder {
    encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
    first_entity: EntityKey,
    last_entity: EntityKey,
    records: u64,
    raw_bytes: u64,
}

impl FrameBuilder {
    fn new(entity: EntityKey, settings: CompressionSettings) -> Result<Self> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), settings.level)?;
        encoder.include_checksum(settings.checksum)?;
        encoder.long_distance_matching(settings.long_distance_matching)?;
        if let Some(window_log) = settings.window_log {
            encoder.window_log(window_log)?;
        }
        encoder.set_target_cblock_size(settings.target_block_size)?;
        Ok(Self {
            encoder,
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

pub struct ArchiveWriter<W: Write> {
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
    frame: Option<FrameBuilder>,
    last_entity: Option<EntityKey>,
    last_timestamp: i64,
    frames: u64,
}

impl<W: Write> ArchiveWriter<W> {
    pub fn new(output: W, frame_target: usize) -> Result<Self> {
        Self::with_compression(output, frame_target, CompressionSettings::default())
    }

    pub fn with_compression(
        mut output: W,
        frame_target: usize,
        compression: CompressionSettings,
    ) -> Result<Self> {
        if frame_target == 0 {
            return Err(ArchiveError::Invalid("zero frame target"));
        }
        output.write_all(&FILE_MAGIC)?;
        output.write_all(&FILE_VERSION.to_le_bytes())?;
        output.write_all(&0_u32.to_le_bytes())?;
        output.write_all(&(frame_target as u64).to_le_bytes())?;
        Ok(Self {
            output,
            frame_target,
            compression,
            frame: None,
            last_entity: None,
            last_timestamp: i64::MAX,
            frames: 0,
        })
    }

    pub fn write(&mut self, record: &Record) -> Result<()> {
        let entity = record.entity();
        let timestamp = record.timestamp_micros();
        let new_entity = self.last_entity != Some(entity);
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
                frame.encoder.flush()?;
                if frame.last_entity.kind != entity.kind
                    || frame.compressed_so_far() >= self.frame_target
                {
                    self.seal_frame()?;
                }
            }
        }
        if self.frame.is_none() {
            self.frame = Some(FrameBuilder::new(entity, self.compression)?);
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
        self.output.write_all(&[0; 8])?;
        self.output.write_all(&compressed)?;
        self.frames += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<(W, u64)> {
        self.seal_frame()?;
        self.output.write_all(&DONE_MAGIC)?;
        self.output.write_all(&[0; FRAME_HEADER_LEN - 4])?;
        self.output.flush()?;
        Ok((self.output, self.frames))
    }
}

pub struct ArchiveReader<R: Read> {
    input: BufReader<R>,
    pub frame_target: u64,
    complete: bool,
    last_frame_entity: Option<EntityKey>,
}

impl<R: Read> ArchiveReader<R> {
    pub fn new(input: R) -> Result<Self> {
        let mut input = BufReader::new(input);
        let frame_target = read_file_header(&mut input)?;
        Ok(Self {
            input,
            frame_target,
            complete: false,
            last_frame_entity: None,
        })
    }

    pub fn next_frame(
        &mut self,
    ) -> Result<Option<ArchiveFrameReader<BorrowedFrameDecoder<'_, R>>>> {
        let mut header = [0_u8; FRAME_HEADER_LEN];
        let mut filled = 0;
        while filled < header.len() {
            match self.input.read(&mut header[filled..])? {
                0 if filled == 0 => return Ok(None),
                0 => return Err(ArchiveError::Invalid("truncated frame header")),
                count => filled += count,
            }
        }
        let Some(info) = parse_frame_header(&header)? else {
            self.complete = true;
            return Ok(None);
        };
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
        let decoder = zstd::stream::read::Decoder::new(limited)?.single_frame();
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
    zstd::stream::read::Decoder<'static, BufReader<Take<&'a mut BufReader<R>>>>;

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
    let mut file = BufReader::new(std::fs::File::open(path)?);
    let frame_target = read_file_header(&mut file)?;
    let mut locations = Vec::new();
    let mut previous = None;
    loop {
        let mut header = [0_u8; FRAME_HEADER_LEN];
        let mut filled = 0;
        while filled < header.len() {
            match file.read(&mut header[filled..])? {
                0 if filled == 0 => return Ok((frame_target, locations, false)),
                0 => return Err(ArchiveError::Invalid("truncated frame header")),
                count => filled += count,
            }
        }
        let Some(info) = parse_frame_header(&header)? else {
            return Ok((frame_target, locations, true));
        };
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
        });
        file.seek(SeekFrom::Current(
            info.compressed_bytes
                .try_into()
                .map_err(|_| ArchiveError::FieldTooLarge)?,
        ))?;
    }
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
    file.seek(SeekFrom::Start(location.compressed_offset))?;
    let decoder =
        zstd::stream::read::Decoder::new(file.take(location.info.compressed_bytes))?.single_frame();
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

type OwnedFrameDecoder = zstd::stream::read::Decoder<'static, BufReader<Take<std::fs::File>>>;

pub struct ArchiveRecordReader {
    path: PathBuf,
    frames: std::vec::IntoIter<FrameLocation>,
    current: Option<ArchiveFrameReader<OwnedFrameDecoder>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RepackStats {
    pub input_frames: u64,
    pub output_frames: u64,
    pub records: u64,
    pub input_raw_bytes: u64,
    pub input_compressed_bytes: u64,
}

pub fn repack<R: Read, W: Write>(
    input: R,
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(W, RepackStats)> {
    let mut reader = ArchiveReader::new(input)?;
    let mut writer = ArchiveWriter::with_compression(output, frame_target, compression)?;
    let mut stats = RepackStats::default();
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
    let (output, output_frames) = writer.finish()?;
    stats.output_frames = output_frames;
    Ok((output, stats))
}

impl ArchiveRecordReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (_, frames, complete) = index_file(&path)?;
        if !complete {
            return Err(ArchiveError::Invalid(
                "archive has no clean completion marker",
            ));
        }
        Ok(Self {
            path,
            frames: frames.into_iter(),
            current: None,
        })
    }

    pub fn next_record(&mut self) -> Result<Option<Record>> {
        loop {
            if let Some(frame) = self.current.as_mut() {
                if let Some(record) = frame.next_record()? {
                    return Ok(Some(record));
                }
                self.current = None;
            }
            let Some(location) = self.frames.next() else {
                return Ok(None);
            };
            self.current = Some(open_owned_frame(&self.path, &location)?);
        }
    }
}

fn open_owned_frame(
    path: &Path,
    location: &FrameLocation,
) -> Result<ArchiveFrameReader<OwnedFrameDecoder>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(location.compressed_offset))?;
    let decoder =
        zstd::stream::read::Decoder::new(file.take(location.info.compressed_bytes))?.single_frame();
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

pub fn concatenate_archives<W: Write>(
    inputs: &[PathBuf],
    mut output: W,
    frame_target: usize,
) -> Result<(W, u64)> {
    output.write_all(&FILE_MAGIC)?;
    output.write_all(&FILE_VERSION.to_le_bytes())?;
    output.write_all(&0_u32.to_le_bytes())?;
    output.write_all(&(frame_target as u64).to_le_bytes())?;
    let mut previous = None;
    let mut frame_count = 0_u64;
    for input in inputs {
        let (_, frames, complete) = index_file(input)?;
        if !complete {
            return Err(ArchiveError::Invalid(
                "archive segment has no completion marker",
            ));
        }
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
    merge_many_archives_with_compression_in(
        inputs,
        output,
        frame_target,
        compression,
        std::env::temp_dir(),
    )
}

pub fn merge_many_archives_with_compression_in<W: Write>(
    inputs: &[PathBuf],
    output: W,
    frame_target: usize,
    compression: CompressionSettings,
    scratch_parent: impl AsRef<Path>,
) -> Result<(W, u64, u64)> {
    let mut sorter = RecordSorter::new_in(scratch_parent.as_ref())?;
    for input in inputs {
        let mut reader = ArchiveRecordReader::open(input)?;
        while let Some(record) = reader.next_record()? {
            sorter.push(record)?;
        }
    }
    let (output, frames, records, _) =
        sorter.finish_with_compression(output, frame_target, compression)?;
    Ok((output, frames, records))
}

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
                current_title: left,
                ..
            },
            Record::PageState {
                current_title: right,
                ..
            },
        ) => left.cmp(right),
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
    output.write_all(&[0; 8])?;
    Ok(())
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
    };
    if header[10..16].iter().any(|byte| *byte != 0) || header[56..64].iter().any(|byte| *byte != 0)
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
        let mut payload = vec![0; payload_len];
        self.decoder.read_exact(&mut payload)?;
        self.raw_bytes_read = self
            .raw_bytes_read
            .checked_add(
                1 + id_bytes as u64 + 8 + 1 + payload_len_bytes as u64 + payload_len as u64,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        if let Some(last_entity) = self.last_entity {
            if entity < last_entity || (entity == last_entity && timestamp > self.last_timestamp) {
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
        Ok(Some(decode_record(entity, timestamp, kind, payload)?))
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
    let mut revisions = PageRevisionSpool::collect(revisions)?.peekable();
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
    if let Some(current_title) = instance.page_current_title(page_id)? {
        writer.write(&Record::PageState {
            page_id,
            timestamp_micros: i64::MAX,
            current_title,
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
    fn collect<E>(
        revisions: impl IntoIterator<Item = std::result::Result<RevisionRecord, E>>,
    ) -> Result<Self>
    where
        ArchiveError: From<E>,
    {
        let mut entries = Vec::new();
        let mut memory_bytes = 0_usize;
        let mut file = None;
        for revision in revisions {
            let revision = revision.map_err(ArchiveError::from)?;
            memory_bytes = memory_bytes.saturating_add(revision.text.len());
            entries.push(SpooledRevision {
                meta: revision.meta,
                text: Some(revision.text),
                offset: 0,
                len: 0,
            });
            if memory_bytes > PAGE_TEXT_MEMORY_LIMIT && file.is_none() {
                let mut spool = tempfile::tempfile()?;
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

pub(crate) fn write_content_page<W: Write>(
    writer: &mut ArchiveWriter<W>,
    page_id: u64,
    current_title: String,
    revisions: impl IntoIterator<Item = Result<RevisionRecord>>,
) -> Result<u64> {
    writer.write(&Record::PageState {
        page_id,
        timestamp_micros: i64::MAX,
        current_title,
    })?;
    let mut count = 0_u64;
    for revision in PageRevisionSpool::collect(revisions)? {
        writer.write(&Record::Revision {
            page_id,
            revision: revision?,
        })?;
        count += 1;
    }
    Ok(count)
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
        Record::PageState { current_title, .. } => string_wire_len(current_title),
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

fn write_record_payload<W: Write>(out: &mut W, record: &Record) -> Result<()> {
    match record {
        Record::PageState { current_title, .. } => write_string(out, current_title)?,
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
    checked_sum(&parts)
}

fn write_site_info(out: &mut impl Write, site_info: &SiteInfoRecord) -> Result<()> {
    write_string(out, &site_info.site_name)?;
    write_string(out, &site_info.db_name)?;
    write_string(out, &site_info.base)?;
    write_string(out, &site_info.generator)?;
    write_string(out, &site_info.case)?;
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
    Ok(())
}

fn decode_record(entity: EntityKey, timestamp: i64, kind: u8, payload: Vec<u8>) -> Result<Record> {
    let mut input = payload.as_slice();
    let record = match kind {
        KIND_PAGE_STATE if entity.kind == EntityKind::Page => Record::PageState {
            page_id: entity.id,
            timestamp_micros: timestamp,
            current_title: read_string(&mut input)?,
        },
        KIND_REVISION if entity.kind == EntityKind::Page => Record::Revision {
            page_id: entity.id,
            revision: read_revision(&mut input, timestamp)?,
        },
        KIND_PAGE_ACTION if matches!(entity.kind, EntityKind::Page | EntityKind::Global) => Record::PageAction {
            entity,
            timestamp_micros: timestamp,
            action: read_action(&mut input)?,
        },
        KIND_USER_STATE if entity.kind == EntityKind::User => Record::UserState {
            user_id: entity.id,
            timestamp_micros: timestamp,
            state: read_user_state(&mut input)?,
        },
        KIND_USER_ACTION if matches!(entity.kind, EntityKind::User | EntityKind::Global) => Record::UserAction {
            entity,
            timestamp_micros: timestamp,
            action: read_user_action(&mut input)?,
        },
        KIND_MANIFEST if entity.kind == EntityKind::Global && entity.id == 0 => Record::Manifest {
            timestamp_micros: timestamp,
            manifest: read_manifest(&mut input)?,
        },
        KIND_SITE_INFO if entity.kind == EntityKind::Global && entity.id == 1 => Record::SiteInfo {
            timestamp_micros: timestamp,
            site_info: read_site_info(&mut input)?,
        },
        KIND_PAGE_STATE | KIND_REVISION | KIND_PAGE_ACTION | KIND_USER_STATE
        | KIND_USER_ACTION | KIND_MANIFEST | KIND_SITE_INFO => {
            return Err(ArchiveError::Invalid(
                "record kind is incompatible with entity kind",
            ))
        }
        _ => {
            return Ok(Record::Unknown {
                entity,
                timestamp_micros: timestamp,
                kind,
                payload,
            })
        }
    };
    if !input.is_empty() {
        return Err(ArchiveError::Invalid("record payload has trailing bytes"));
    }
    Ok(record)
}

fn read_revision(input: &mut &[u8], timestamp: i64) -> Result<RevisionRecord> {
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

fn read_action(input: &mut &[u8]) -> Result<PageActionRecord> {
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

fn read_visibility(input: &mut &[u8]) -> Result<RevisionVisibilityRecord> {
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

fn read_account_class(input: &mut &[u8]) -> Result<AccountClass> {
    match read_u8(input)? {
        0 => Ok(AccountClass::Unknown),
        1 => Ok(AccountClass::Anonymous),
        2 => Ok(AccountClass::Temporary),
        3 => Ok(AccountClass::Permanent),
        4 => Ok(AccountClass::Hidden),
        _ => Err(ArchiveError::Invalid("invalid account class")),
    }
}

fn read_performer(input: &mut &[u8]) -> Result<PerformerRecord> {
    Ok(PerformerRecord {
        local_user_id: read_option_u64(input)?,
        central_user_id: read_option_u64(input)?,
        historical_name: read_option_string(input)?,
        account_class: read_account_class(input)?,
    })
}

fn read_action_kind(input: &mut &[u8]) -> Result<PageActionKind> {
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

fn read_revision_history(input: &mut &[u8]) -> Result<RevisionHistoryRecord> {
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

fn read_user_state(input: &mut &[u8]) -> Result<UserStateRecord> {
    Ok(UserStateRecord {
        current_name: read_option_string(input)?,
        central_user_id: read_option_u64(input)?,
        account_class: read_account_class(input)?,
        groups: read_strings(input)?,
        blocks: read_strings(input)?,
        bot_by: read_strings(input)?,
    })
}

fn read_user_action_kind(input: &mut &[u8]) -> Result<UserActionKind> {
    match read_u8(input)? {
        0 => Ok(UserActionKind::Create),
        1 => Ok(UserActionKind::Rename),
        2 => Ok(UserActionKind::GroupsChanged),
        3 => Ok(UserActionKind::BlocksChanged),
        255 => Ok(UserActionKind::Other(read_string(input)?)),
        _ => Err(ArchiveError::Invalid("invalid user action kind")),
    }
}

fn read_user_action(input: &mut &[u8]) -> Result<UserActionRecord> {
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

fn read_manifest(input: &mut &[u8]) -> Result<ManifestRecord> {
    Ok(ManifestRecord {
        wiki_db: read_string(input)?,
        content_snapshot: read_string(input)?,
        metadata_snapshot: read_string(input)?,
        source_files: read_strings(input)?,
    })
}

fn read_site_info(input: &mut &[u8]) -> Result<SiteInfoRecord> {
    let site_name = read_string(input)?;
    let db_name = read_string(input)?;
    let base = read_string(input)?;
    let generator = read_string(input)?;
    let case = read_string(input)?;
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
    Ok(SiteInfoRecord {
        site_name,
        db_name,
        base,
        generator,
        case,
        namespaces,
        interwiki,
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

fn read_u32(input: &mut &[u8]) -> Result<u32> {
    let bytes = take_bytes(input, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u64(input: &mut &[u8]) -> Result<u64> {
    let bytes = take_bytes(input, 8)?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_string(input: &mut &[u8]) -> Result<String> {
    String::from_utf8(read_bytes(input)?)
        .map_err(|_| ArchiveError::Invalid("archive string is not UTF-8"))
}

fn read_bytes(input: &mut &[u8]) -> Result<Vec<u8>> {
    let (len, _) = read_varint(input)?;
    let len = len.try_into().map_err(|_| ArchiveError::FieldTooLarge)?;
    Ok(take_bytes(input, len)?.to_vec())
}

fn read_option_i64(input: &mut &[u8]) -> Result<Option<i64>> {
    match read_u8(input)? {
        0 => Ok(None),
        1 => {
            let bytes = take_bytes(input, 8)?;
            Ok(Some(i64::from_le_bytes(bytes.try_into().unwrap())))
        }
        _ => Err(ArchiveError::Invalid("invalid optional integer marker")),
    }
}

fn read_option_u64(input: &mut &[u8]) -> Result<Option<u64>> {
    match read_u8(input)? {
        0 => Ok(None),
        1 => Ok(Some(read_varint(input)?.0)),
        _ => Err(ArchiveError::Invalid("invalid optional integer marker")),
    }
}

fn read_option_bool(input: &mut &[u8]) -> Result<Option<bool>> {
    match read_u8(input)? {
        0 => Ok(None),
        1 => Ok(Some(false)),
        2 => Ok(Some(true)),
        _ => Err(ArchiveError::Invalid("invalid optional boolean marker")),
    }
}

fn read_option_string(input: &mut &[u8]) -> Result<Option<String>> {
    match read_u8(input)? {
        0 => Ok(None),
        1 => Ok(Some(read_string(input)?)),
        _ => Err(ArchiveError::Invalid("invalid optional string marker")),
    }
}

fn read_strings(input: &mut &[u8]) -> Result<Vec<String>> {
    let count: usize = read_varint(input)?
        .0
        .try_into()
        .map_err(|_| ArchiveError::FieldTooLarge)?;
    (0..count).map(|_| read_string(input)).collect()
}

fn read_bool(input: &mut &[u8]) -> Result<bool> {
    match read_u8(input)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ArchiveError::Invalid("invalid boolean")),
    }
}

fn take_bytes<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    if input.len() < len {
        return Err(ArchiveError::Invalid("truncated record payload"));
    }
    let (value, rest) = input.split_at(len);
    *input = rest;
    Ok(value)
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
    fn round_trip_and_page_aligned_frames() {
        let mut writer = ArchiveWriter::new(Vec::new(), 1).unwrap();
        let records = vec![
            Record::PageState {
                page_id: 1,
                timestamp_micros: 20,
                current_title: "One".into(),
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
