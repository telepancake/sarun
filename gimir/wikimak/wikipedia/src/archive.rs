//! Portable, layout-independent Wikipedia event stream.
//!
//! The outer file is a short header followed by independently compressed
//! frames. Frames end only between page ids. Records are ordered by ascending
//! page id and, within a page, descending event time. This is deliberately not
//! a depot format: it is a compact source for experiments, conversions, and
//! recovery without depending on the current live storage layout.

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Take, Write};
use std::path::Path;

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
const KIND_HISTORY_EVENT: u8 = 4;
const PAGE_TEXT_MEMORY_LIMIT: usize = 16 << 20;

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
}

pub type Result<T> = std::result::Result<T, ArchiveError>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevisionVisibilityRecord {
    pub source_partition: String,
    pub deleted_parts: String,
    pub parts_are_suppressed: bool,
    pub deleted_by_page_deletion: bool,
    pub page_deletion_timestamp: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageActionRecord {
    pub source_key: String,
    pub source_partition: String,
    pub event_log_id: Option<i64>,
    pub source_ordinal: u64,
    pub event_type: String,
    pub timestamp: String,
    pub comment: String,
    pub actor_id: Option<i64>,
    pub actor_name: String,
    pub historical_title: String,
    pub current_title: String,
    pub historical_namespace: Option<i64>,
    pub current_namespace: Option<i64>,
    pub page_deleted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionRecord {
    pub meta: RevisionMeta,
    pub text: Vec<u8>,
    pub visibility: Option<RevisionVisibilityRecord>,
}

/// A complete row from a MediaWiki History user event.
///
/// The common entity key and timestamp live in the record envelope. The
/// remaining original TSV columns are retained in schema order. `None` is an
/// upstream null/empty field; non-empty fields are unescaped UTF-8 bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryEventRecord {
    pub source_partition: String,
    pub source_ordinal: u64,
    pub schema_columns: u16,
    pub fields: Vec<Option<Vec<u8>>>,
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
        page_id: u64,
        timestamp_micros: i64,
        action: PageActionRecord,
    },
    HistoryEvent {
        entity: EntityKey,
        timestamp_micros: i64,
        event: HistoryEventRecord,
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
            | Self::Revision { page_id, .. }
            | Self::PageAction { page_id, .. } => EntityKey {
                kind: EntityKind::Page,
                id: *page_id,
            },
            Self::HistoryEvent { entity, .. } => *entity,
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
            | Self::Unknown {
                timestamp_micros, ..
            } => *timestamp_micros,
            Self::HistoryEvent {
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

struct FrameBuilder {
    encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
    first_entity: EntityKey,
    last_entity: EntityKey,
    records: u64,
    raw_bytes: u64,
    next_probe_raw: u64,
}

impl FrameBuilder {
    fn new(entity: EntityKey) -> Result<Self> {
        Ok(Self {
            encoder: zstd::stream::write::Encoder::new(Vec::new(), 3)?,
            first_entity: entity,
            last_entity: entity,
            records: 0,
            raw_bytes: 0,
            next_probe_raw: 0,
        })
    }

    fn compressed_so_far(&self) -> usize {
        self.encoder.get_ref().len()
    }
}

pub struct ArchiveWriter<W: Write> {
    output: W,
    frame_target: usize,
    frame: Option<FrameBuilder>,
    last_entity: Option<EntityKey>,
    last_timestamp: i64,
    frames: u64,
}

impl<W: Write> ArchiveWriter<W> {
    pub fn new(mut output: W, frame_target: usize) -> Result<Self> {
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
                if frame.compressed_so_far() < self.frame_target
                    && frame.raw_bytes >= frame.next_probe_raw
                {
                    frame.encoder.flush()?;
                    let compressed = frame.compressed_so_far() as u64;
                    let deficit = (self.frame_target as u64).saturating_sub(compressed);
                    let estimated_raw = if compressed == 0 {
                        self.frame_target as u64
                    } else {
                        deficit
                            .saturating_mul(frame.raw_bytes)
                            .saturating_div(compressed)
                    };
                    frame.next_probe_raw = frame
                        .raw_bytes
                        .saturating_add(estimated_raw.max(self.frame_target as u64));
                }
                if frame.compressed_so_far() >= self.frame_target {
                    self.seal_frame()?;
                }
            }
        }
        if self.frame.is_none() {
            self.frame = Some(FrameBuilder::new(entity)?);
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
        visitor(record)?;
    }
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
        .map(|action| {
            let timestamp = parse_timestamp_micros(&action.timestamp)?;
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
            .map(|(action, timestamp)| (*timestamp, 0_u8, action.source_ordinal));
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
                    text: revision.text,
                    visibility: visibility.get(&revision_id).cloned(),
                },
            })?;
            stats.revisions += 1;
        } else {
            let (action, timestamp_micros) = actions.next().expect("key exists");
            writer.write(&Record::PageAction {
                page_id,
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
    fn collect(revisions: ArchiveRevisionIter) -> Result<Self> {
        let mut entries = Vec::new();
        let mut memory_bytes = 0_usize;
        let mut file = None;
        for revision in revisions {
            let revision = revision?;
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
            text,
            visibility: None,
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
                    text,
                    visibility: None,
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
        Record::HistoryEvent { event, .. } => history_event_wire_len(event),
        Record::Unknown { payload, kind, .. } => return Ok((*kind, payload.len() as u64)),
    }?;
    let kind = match record {
        Record::PageState { .. } => KIND_PAGE_STATE,
        Record::Revision { .. } => KIND_REVISION,
        Record::PageAction { .. } => KIND_PAGE_ACTION,
        Record::HistoryEvent { .. } => KIND_HISTORY_EVENT,
        Record::Unknown { kind, .. } => *kind,
    };
    Ok((kind, size))
}

fn write_record_payload<W: Write>(out: &mut W, record: &Record) -> Result<()> {
    match record {
        Record::PageState { current_title, .. } => write_string(out, current_title)?,
        Record::Revision { revision, .. } => write_revision(out, revision)?,
        Record::PageAction { action, .. } => write_action(out, action)?,
        Record::HistoryEvent { event, .. } => write_history_event(out, event)?,
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
        bytes_wire_len(&revision.text)?,
        1,
        revision
            .visibility
            .as_ref()
            .map(visibility_wire_len)
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
    write_bytes(out, &revision.text)?;
    match &revision.visibility {
        Some(visibility) => {
            out.write_all(&[1])?;
            write_visibility(out, visibility)?;
        }
        None => out.write_all(&[0])?,
    }
    Ok(())
}

fn action_wire_len(action: &PageActionRecord) -> Result<u64> {
    checked_sum(&[
        string_wire_len(&action.source_key)?,
        string_wire_len(&action.source_partition)?,
        option_i64_wire_len(action.event_log_id),
        8,
        string_wire_len(&action.event_type)?,
        string_wire_len(&action.timestamp)?,
        string_wire_len(&action.comment)?,
        option_i64_wire_len(action.actor_id),
        string_wire_len(&action.actor_name)?,
        string_wire_len(&action.historical_title)?,
        string_wire_len(&action.current_title)?,
        option_i64_wire_len(action.historical_namespace),
        option_i64_wire_len(action.current_namespace),
        1,
    ])
}

fn write_action<W: Write>(out: &mut W, action: &PageActionRecord) -> Result<()> {
    write_string(out, &action.source_key)?;
    write_string(out, &action.source_partition)?;
    write_option_i64(out, action.event_log_id)?;
    out.write_all(&action.source_ordinal.to_le_bytes())?;
    write_string(out, &action.event_type)?;
    write_string(out, &action.timestamp)?;
    write_string(out, &action.comment)?;
    write_option_i64(out, action.actor_id)?;
    write_string(out, &action.actor_name)?;
    write_string(out, &action.historical_title)?;
    write_string(out, &action.current_title)?;
    write_option_i64(out, action.historical_namespace)?;
    write_option_i64(out, action.current_namespace)?;
    out.write_all(&[u8::from(action.page_deleted)])?;
    Ok(())
}

fn history_event_wire_len(event: &HistoryEventRecord) -> Result<u64> {
    if usize::from(event.schema_columns) != event.fields.len() {
        return Err(ArchiveError::Invalid(
            "user event column count does not match schema",
        ));
    }
    let mut size = checked_sum(&[
        string_wire_len(&event.source_partition)?,
        8,
        2,
        varint_len(event.fields.len() as u64) as u64,
    ])?;
    for field in &event.fields {
        size = size.checked_add(1).ok_or(ArchiveError::FieldTooLarge)?;
        if let Some(value) = field {
            size = size
                .checked_add(bytes_wire_len(value)?)
                .ok_or(ArchiveError::FieldTooLarge)?;
        }
    }
    Ok(size)
}

fn write_history_event<W: Write>(out: &mut W, event: &HistoryEventRecord) -> Result<()> {
    if usize::from(event.schema_columns) != event.fields.len() {
        return Err(ArchiveError::Invalid(
            "user event column count does not match schema",
        ));
    }
    write_string(out, &event.source_partition)?;
    out.write_all(&event.source_ordinal.to_le_bytes())?;
    out.write_all(&event.schema_columns.to_le_bytes())?;
    write_varint(out, event.fields.len() as u64)?;
    for field in &event.fields {
        match field {
            Some(value) => {
                out.write_all(&[1])?;
                write_bytes(out, value)?;
            }
            None => out.write_all(&[0])?,
        }
    }
    Ok(())
}

fn visibility_wire_len(visibility: &RevisionVisibilityRecord) -> Result<u64> {
    checked_sum(&[
        string_wire_len(&visibility.source_partition)?,
        string_wire_len(&visibility.deleted_parts)?,
        1,
        1,
        string_wire_len(&visibility.page_deletion_timestamp)?,
    ])
}

fn write_visibility<W: Write>(out: &mut W, visibility: &RevisionVisibilityRecord) -> Result<()> {
    write_string(out, &visibility.source_partition)?;
    write_string(out, &visibility.deleted_parts)?;
    out.write_all(&[u8::from(visibility.parts_are_suppressed)])?;
    out.write_all(&[u8::from(visibility.deleted_by_page_deletion)])?;
    write_string(out, &visibility.page_deletion_timestamp)?;
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
        KIND_PAGE_ACTION if entity.kind == EntityKind::Page => Record::PageAction {
            page_id: entity.id,
            timestamp_micros: timestamp,
            action: read_action(&mut input)?,
        },
        KIND_HISTORY_EVENT => Record::HistoryEvent {
            entity,
            timestamp_micros: timestamp,
            event: read_history_event(&mut input)?,
        },
        KIND_PAGE_STATE | KIND_REVISION | KIND_PAGE_ACTION => {
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

fn read_history_event(input: &mut &[u8]) -> Result<HistoryEventRecord> {
    let source_partition = read_string(input)?;
    let source_ordinal = read_u64(input)?;
    let schema_columns = read_u16(input)?;
    let (field_count, _) = read_varint(input)?;
    if field_count != u64::from(schema_columns) {
        return Err(ArchiveError::Invalid(
            "user event column count does not match schema",
        ));
    }
    let mut fields = Vec::with_capacity(usize::from(schema_columns));
    for _ in 0..schema_columns {
        fields.push(match read_u8(input)? {
            0 => None,
            1 => Some(read_bytes(input)?),
            _ => return Err(ArchiveError::Invalid("invalid field marker")),
        });
    }
    Ok(HistoryEventRecord {
        source_partition,
        source_ordinal,
        schema_columns,
        fields,
    })
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
    let text = read_bytes(input)?;
    let visibility = match read_u8(input)? {
        0 => None,
        1 => Some(read_visibility(input)?),
        _ => return Err(ArchiveError::Invalid("invalid visibility marker")),
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
        text,
        visibility,
    })
}

fn read_action(input: &mut &[u8]) -> Result<PageActionRecord> {
    Ok(PageActionRecord {
        source_key: read_string(input)?,
        source_partition: read_string(input)?,
        event_log_id: read_option_i64(input)?,
        source_ordinal: read_u64(input)?,
        event_type: read_string(input)?,
        timestamp: read_string(input)?,
        comment: read_string(input)?,
        actor_id: read_option_i64(input)?,
        actor_name: read_string(input)?,
        historical_title: read_string(input)?,
        current_title: read_string(input)?,
        historical_namespace: read_option_i64(input)?,
        current_namespace: read_option_i64(input)?,
        page_deleted: match read_u8(input)? {
            0 => false,
            1 => true,
            _ => return Err(ArchiveError::Invalid("invalid page-deleted marker")),
        },
    })
}

fn read_visibility(input: &mut &[u8]) -> Result<RevisionVisibilityRecord> {
    let source_partition = read_string(input)?;
    let deleted_parts = read_string(input)?;
    let parts_are_suppressed = read_bool(input)?;
    let deleted_by_page_deletion = read_bool(input)?;
    let page_deletion_timestamp = read_string(input)?;
    Ok(RevisionVisibilityRecord {
        source_partition,
        deleted_parts,
        parts_are_suppressed,
        deleted_by_page_deletion,
        page_deletion_timestamp,
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

fn read_u32(input: &mut &[u8]) -> Result<u32> {
    let bytes = take_bytes(input, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u16(input: &mut &[u8]) -> Result<u16> {
    let bytes = take_bytes(input, 2)?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
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
                text: text.to_vec(),
                visibility: None,
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
        let record = Record::HistoryEvent {
            entity: EntityKey {
                kind: EntityKind::User,
                id: 42,
            },
            timestamp_micros: 123,
            event: HistoryEventRecord {
                source_partition: "2026-06".into(),
                source_ordinal: 9,
                schema_columns: 3,
                fields: vec![Some(b"wiki".to_vec()), None, Some(b"rename".to_vec())],
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
}
