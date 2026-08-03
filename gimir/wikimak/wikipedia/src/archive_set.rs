//! A portable archive split at frame boundaries into bounded entity ranges.
//!
//! The files, in lexical order, are byte-for-byte the ordinary archive stream.
//! This keeps one wire format: sequential tools read the imagined
//! concatenation, while random readers resolve virtual offsets through the
//! generated title index.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::collections::VecDeque;

use crate::archive::{ArchiveError, EntityKey, EntityKind, Result};

const FILE_HEADER_BYTES: usize = 24;
const FRAME_HEADER_BYTES: usize = 64;
const FILE_MAGIC: &[u8; 8] = b"SWDUMP\0\0";
const FRAME_MAGIC: &[u8; 4] = b"FRM1";
const DICTIONARY_MAGIC: &[u8; 4] = b"DICT";
const REF_PREFIX_MAGIC: &[u8; 4] = b"PREF";
const DONE_MAGIC: &[u8; 4] = b"DONE";

pub const DEFAULT_RANGE_TARGET: u64 = 1 << 30;
pub const PART_SUFFIX: &str = ".swdump-part";

pub(crate) fn indexed_segment_name(
    segment: crate::title_index::SegmentIndexEntry,
) -> Result<String> {
    match segment.role {
        0 => Ok("0000-reference.swdump-part".to_owned()),
        1 => Ok(format!(
            "1000-p{:020}-p{:020}{PART_SUFFIX}",
            segment.first_id, segment.last_id
        )),
        2 => Ok(format!(
            "2000-u{:020}-u{:020}{PART_SUFFIX}",
            segment.first_id, segment.last_id
        )),
        3 => Ok(format!(
            "3000-g{:020}-g{:020}{PART_SUFFIX}",
            segment.first_id, segment.last_id
        )),
        4 => Ok("9999-complete.swdump-part".to_owned()),
        _ => Err(ArchiveError::Invalid("unknown archive-set segment role")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveSetSegment {
    pub name: String,
    pub virtual_start: u64,
    pub bytes: u64,
    pub kind: Option<EntityKind>,
    pub first_id: u64,
    pub last_id: u64,
}

struct Part {
    temporary: PathBuf,
    file: std::fs::File,
    bytes: u64,
    kind: Option<EntityKind>,
    first_id: u64,
    last_id: u64,
}

#[derive(Clone, Copy)]
enum PayloadDestination {
    Reference,
    Range,
}

enum WriteState {
    FileHeader(Vec<u8>),
    FrameHeader(Vec<u8>),
    Payload {
        remaining: u64,
        destination: PayloadDestination,
    },
}

/// A `Write` sink accepted by the ordinary archive writer. It routes complete
/// frame byte ranges into bounded physical files without changing archive
/// encoding.
pub struct ArchiveSetOutput {
    root: OutputRoot,
    range_target: u64,
    reference: Option<Part>,
    range: Option<Part>,
    segments: Vec<ArchiveSetSegment>,
    virtual_bytes: u64,
    serial: u64,
    state: WriteState,
    complete: bool,
    replace_root: Option<PathBuf>,
    range_boundaries: VecDeque<(EntityKind, u64, String)>,
}

enum OutputRoot {
    Temporary(tempfile::TempDir),
    Persistent(PathBuf),
}

impl OutputRoot {
    fn path(&self) -> &Path {
        match self {
            Self::Temporary(root) => root.path(),
            Self::Persistent(root) => root,
        }
    }

    fn into_path(self) -> PathBuf {
        match self {
            Self::Temporary(root) => {
                #[allow(deprecated)]
                root.into_path()
            }
            Self::Persistent(root) => root,
        }
    }
}

impl ArchiveSetOutput {
    pub fn new_in(parent: impl AsRef<Path>, range_target: u64) -> Result<Self> {
        if range_target == 0 {
            return Err(ArchiveError::Invalid("zero archive range target"));
        }
        let root = OutputRoot::Temporary(tempfile::TempDir::new_in(parent)?);
        let reference = Some(Self::new_part_at(root.path(), 0)?);
        Ok(Self {
            root,
            range_target,
            reference,
            range: None,
            segments: Vec::new(),
            virtual_bytes: 0,
            serial: 1,
            state: WriteState::FileHeader(Vec::with_capacity(FILE_HEADER_BYTES)),
            complete: false,
            replace_root: None,
            range_boundaries: VecDeque::new(),
        })
    }

    pub fn resumable_in(
        parent: impl AsRef<Path>,
        name: impl AsRef<Path>,
        range_target: u64,
    ) -> Result<Self> {
        if range_target == 0 {
            return Err(ArchiveError::Invalid("zero archive range target"));
        }
        let root = parent.as_ref().join(name);
        std::fs::create_dir_all(&root)?;
        let mut names = std::fs::read_dir(&root)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        let mut segments = Vec::new();
        let mut virtual_bytes = 0_u64;
        for name in names {
            let name = name
                .into_string()
                .map_err(|_| ArchiveError::Invalid("archive-set filename is not UTF-8"))?;
            let path = root.join(&name);
            if name.starts_with(".range-") && name.ends_with(".tmp") {
                std::fs::remove_file(path)?;
                continue;
            }
            if !name.ends_with(PART_SUFFIX) {
                return Err(ArchiveError::Invalid(
                    "resumable archive set contains an unknown file",
                ));
            }
            if name == "9999-complete.swdump-part" {
                return Err(ArchiveError::Invalid(
                    "resumable archive set is already complete",
                ));
            }
            let bytes = std::fs::metadata(&path)?.len();
            let (kind, first_id, last_id) = parse_segment_name(&name)?;
            segments.push(ArchiveSetSegment {
                name,
                virtual_start: virtual_bytes,
                bytes,
                kind,
                first_id,
                last_id,
            });
            virtual_bytes = virtual_bytes
                .checked_add(bytes)
                .ok_or(ArchiveError::FieldTooLarge)?;
        }
        if !segments.is_empty()
            && !segments
                .first()
                .is_some_and(|segment| segment.name == "0000-reference.swdump-part")
        {
            return Err(ArchiveError::Invalid(
                "resumable archive set lacks its reference part",
            ));
        }
        let mut previous = None;
        for segment in segments.iter().filter(|segment| segment.kind.is_some()) {
            let first = EntityKey {
                kind: segment.kind.expect("filtered above"),
                id: segment.first_id,
            };
            if previous.is_some_and(|boundary| first <= boundary) {
                return Err(ArchiveError::Invalid(
                    "resumable archive ranges are not strictly ordered",
                ));
            }
            previous = Some(EntityKey {
                kind: first.kind,
                id: segment.last_id,
            });
        }
        let reference = Some(Self::new_part_at(&root, 0)?);
        Ok(Self {
            root: OutputRoot::Persistent(root),
            range_target,
            reference,
            range: None,
            segments,
            virtual_bytes,
            serial: 1,
            state: WriteState::FileHeader(Vec::with_capacity(FILE_HEADER_BYTES)),
            complete: false,
            replace_root: None,
            range_boundaries: VecDeque::new(),
        })
    }

    pub fn resume_after(&self) -> Option<EntityKey> {
        self.segments.iter().rev().find_map(|segment| {
            segment.kind.map(|kind| EntityKey {
                kind,
                id: segment.last_id,
            })
        })
    }

    pub(crate) fn preserved_ref_prefix(&self) -> Result<Option<std::sync::Arc<[u8]>>> {
        if !self
            .segments
            .first()
            .is_some_and(|segment| segment.name == "0000-reference.swdump-part")
        {
            return Ok(None);
        }
        crate::archive::archive_ref_prefix_part(
            self.root.path().join("0000-reference.swdump-part"),
        )
        .map(Some)
    }

    pub fn virtual_bytes(&self) -> u64 {
        self.virtual_bytes
    }

    pub fn replacing(
        destination: impl AsRef<Path>,
        range_target: u64,
        segments: &[ArchiveSetSegment],
    ) -> Result<Self> {
        let destination = destination.as_ref();
        if !destination.is_dir() {
            return Err(ArchiveError::Invalid(
                "replacement archive set is not a directory",
            ));
        }
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut output = Self::new_in(parent, range_target)?;
        output.replace_root = Some(destination.to_path_buf());
        output.range_boundaries = segments
            .iter()
            .filter_map(|segment| {
                segment
                    .kind
                    .map(|kind| (kind, segment.last_id, segment.name.clone()))
            })
            .collect();
        Ok(output)
    }

    fn new_part_at(root: &Path, serial: u64) -> Result<Part> {
        let temporary = root.join(format!(".range-{serial:06}.tmp"));
        let file = std::fs::File::create(&temporary)?;
        Ok(Part {
            temporary,
            file,
            bytes: 0,
            kind: None,
            first_id: 0,
            last_id: 0,
        })
    }

    fn new_part(&mut self) -> Result<Part> {
        let serial = self.serial;
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        Self::new_part_at(self.root.path(), serial)
    }

    fn write_reference(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let part = self.reference.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "reference part is sealed")
        })?;
        part.file.write_all(bytes)?;
        part.bytes = part.bytes.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn write_range(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let part = self.range.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "range part is absent")
        })?;
        part.file.write_all(bytes)?;
        part.bytes = part.bytes.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn seal_reference(&mut self) -> Result<()> {
        let Some(part) = self.reference.take() else {
            return Ok(());
        };
        if self
            .segments
            .first()
            .is_some_and(|segment| segment.name == "0000-reference.swdump-part")
        {
            part.file.sync_all()?;
            drop(part.file);
            let existing = self.root.path().join("0000-reference.swdump-part");
            if !files_equal(&part.temporary, &existing)? {
                return Err(ArchiveError::Invalid(
                    "resumed archive changed the reference prefix",
                ));
            }
            std::fs::remove_file(part.temporary)?;
            return Ok(());
        }
        self.seal_part(part, "0000-reference.swdump-part".to_owned())
    }

    fn seal_range(&mut self) -> Result<()> {
        let Some(part) = self.range.take() else {
            return Ok(());
        };
        let generated_name = match part.kind {
            Some(EntityKind::Page) => format!(
                "1000-p{:020}-p{:020}{PART_SUFFIX}",
                part.first_id, part.last_id
            ),
            Some(EntityKind::User) => format!(
                "2000-u{:020}-u{:020}{PART_SUFFIX}",
                part.first_id, part.last_id
            ),
            Some(EntityKind::Global) => format!(
                "3000-g{:020}-g{:020}{PART_SUFFIX}",
                part.first_id, part.last_id
            ),
            None => return Err(ArchiveError::Invalid("range part has no entity kind")),
        };
        let name = self
            .range_boundaries
            .front()
            .filter(|(kind, last_id, _)| {
                part.kind == Some(*kind) && part.last_id == *last_id
            })
            .map(|(_, _, name)| name.clone())
            .unwrap_or(generated_name);
        self.seal_part(part, name)
    }

    fn seal_part(&mut self, mut part: Part, name: String) -> Result<()> {
        part.file.flush()?;
        part.file.sync_all()?;
        drop(part.file);
        let destination = self
            .replace_root
            .as_deref()
            .unwrap_or_else(|| self.root.path())
            .join(&name);
        if self.replace_root.is_some() && name == "0000-reference.swdump-part" {
            if !files_equal(&part.temporary, &destination)? {
                return Err(ArchiveError::Invalid(
                    "archive update changed the reference prefix",
                ));
            }
            std::fs::remove_file(&part.temporary)?;
        } else {
            std::fs::rename(&part.temporary, &destination)?;
            if let Some(root) = self.replace_root.as_deref() {
                sync_directory(root)?;
            }
        }
        let segment = ArchiveSetSegment {
            name,
            virtual_start: self.virtual_bytes,
            bytes: part.bytes,
            kind: part.kind,
            first_id: part.first_id,
            last_id: part.last_id,
        };
        self.virtual_bytes = self
            .virtual_bytes
            .checked_add(segment.bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.segments.push(segment);
        Ok(())
    }

    fn begin_frame(&mut self, header: &[u8]) -> Result<PayloadDestination> {
        if header.len() != FRAME_HEADER_BYTES {
            return Err(ArchiveError::Invalid("invalid archive frame header size"));
        }
        let magic: &[u8; 4] = header[..4].try_into().expect("four magic bytes");
        if magic == DICTIONARY_MAGIC || magic == REF_PREFIX_MAGIC {
            if self.range.is_some() {
                return Err(ArchiveError::Invalid(
                    "compression reference follows archive data",
                ));
            }
            self.write_reference(header)?;
            return Ok(PayloadDestination::Reference);
        }
        if magic == DONE_MAGIC {
            if let (Some(part), Some((kind, last_id, _))) =
                (self.range.as_ref(), self.range_boundaries.front())
            {
                if part.kind != Some(*kind) || part.last_id != *last_id {
                    return Err(ArchiveError::Invalid(
                        "replacement stream ended before a range boundary",
                    ));
                }
                self.range_boundaries.pop_front();
            }
            self.seal_reference()?;
            self.seal_range()?;
            let mut complete = self.new_part()?;
            complete.file.write_all(header)?;
            complete.bytes = header.len() as u64;
            self.seal_part(complete, "9999-complete.swdump-part".to_owned())?;
            self.complete = true;
            return Ok(PayloadDestination::Range);
        }
        if magic != FRAME_MAGIC {
            return Err(ArchiveError::Invalid("unknown archive frame magic"));
        }
        let kind = EntityKind::try_from(header[8])?;
        if EntityKind::try_from(header[9])? != kind {
            return Err(ArchiveError::Invalid("archive frame mixes entity kinds"));
        }
        let first_id = u64::from_le_bytes(header[16..24].try_into().unwrap());
        let last_id = u64::from_le_bytes(header[24..32].try_into().unwrap());
        if first_id > last_id {
            return Err(ArchiveError::Invalid("archive frame has reversed entity range"));
        }
        while let (Some(part), Some((boundary_kind, boundary_id, _))) =
            (self.range.as_ref(), self.range_boundaries.front())
        {
            let crossed = kind > *boundary_kind
                || (kind == *boundary_kind && first_id > *boundary_id);
            if !crossed {
                break;
            }
            if part.kind != Some(*boundary_kind) || part.last_id != *boundary_id {
                return Err(ArchiveError::Invalid(
                    "replacement stream crossed a range boundary inside a frame",
                ));
            }
            self.seal_range()?;
            self.range_boundaries.pop_front();
        }
        let has_preserved_boundary = self
            .range_boundaries
            .front()
            .is_some_and(|(boundary_kind, _, _)| *boundary_kind == kind);
        let split = self.range.as_ref().is_some_and(|part| {
            part.kind != Some(kind)
                || (!has_preserved_boundary && part.bytes >= self.range_target)
        });
        if split {
            self.seal_range()?;
        }
        self.seal_reference()?;
        if self.range.is_none() {
            let mut part = self.new_part()?;
            part.kind = Some(kind);
            part.first_id = first_id;
            part.last_id = last_id;
            self.range = Some(part);
        } else if let Some(part) = self.range.as_mut() {
            if first_id <= part.last_id {
                return Err(ArchiveError::Invalid(
                    "archive range frames are not strictly ordered",
                ));
            }
            part.last_id = last_id;
        }
        self.write_range(header)?;
        Ok(PayloadDestination::Range)
    }

    fn payload_bytes(header: &[u8], destination: PayloadDestination) -> Result<u64> {
        let magic = &header[..4];
        if magic == FRAME_MAGIC {
            Ok(u64::from_le_bytes(header[48..56].try_into().unwrap()))
        } else if magic == DICTIONARY_MAGIC {
            Ok(u64::from_le_bytes(header[24..32].try_into().unwrap()))
        } else if magic == REF_PREFIX_MAGIC {
            Ok(u64::from_le_bytes(header[24..32].try_into().unwrap()))
        } else if magic == DONE_MAGIC {
            let _ = destination;
            Ok(0)
        } else {
            Err(ArchiveError::Invalid("unknown archive frame magic"))
        }
    }

    pub fn segments(&self) -> &[ArchiveSetSegment] {
        &self.segments
    }

    pub fn finish(self) -> Result<CompletedArchiveSet> {
        if !self.complete {
            return Err(ArchiveError::Invalid(
                "archive set has no clean completion marker",
            ));
        }
        if !matches!(self.state, WriteState::FrameHeader(ref bytes) if bytes.is_empty()) {
            return Err(ArchiveError::Invalid("archive set ends in a partial frame"));
        }
        if !self.range_boundaries.is_empty() {
            return Err(ArchiveError::Invalid(
                "archive update did not reach every preserved range boundary",
            ));
        }
        Ok(CompletedArchiveSet {
            root: self.root,
            segments: self.segments,
            virtual_bytes: self.virtual_bytes,
            installed: self.replace_root.is_some(),
        })
    }
}

impl Write for ArchiveSetOutput {
    fn write(&mut self, mut input: &[u8]) -> std::io::Result<usize> {
        let original = input.len();
        while !input.is_empty() {
            match &mut self.state {
                WriteState::FileHeader(header) => {
                    let take = (FILE_HEADER_BYTES - header.len()).min(input.len());
                    header.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    if header.len() == FILE_HEADER_BYTES {
                        if &header[..8] != FILE_MAGIC {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "bad archive file magic",
                            ));
                        }
                        let complete = std::mem::take(header);
                        self.write_reference(&complete)?;
                        self.state =
                            WriteState::FrameHeader(Vec::with_capacity(FRAME_HEADER_BYTES));
                    }
                }
                WriteState::FrameHeader(header) => {
                    let take = (FRAME_HEADER_BYTES - header.len()).min(input.len());
                    header.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    if header.len() == FRAME_HEADER_BYTES {
                        let complete = std::mem::take(header);
                        let destination = self.begin_frame(&complete).map_err(io_error)?;
                        let remaining =
                            Self::payload_bytes(&complete, destination).map_err(io_error)?;
                        self.state = if remaining == 0 {
                            WriteState::FrameHeader(Vec::with_capacity(FRAME_HEADER_BYTES))
                        } else {
                            WriteState::Payload {
                                remaining,
                                destination,
                            }
                        };
                    }
                }
                WriteState::Payload {
                    remaining,
                    destination,
                } => {
                    let take = usize::try_from((*remaining).min(input.len() as u64))
                        .expect("bounded by input length");
                    let bytes = &input[..take];
                    let destination = *destination;
                    let remaining = *remaining - take as u64;
                    self.state = if remaining == 0 {
                        WriteState::FrameHeader(Vec::with_capacity(FRAME_HEADER_BYTES))
                    } else {
                        WriteState::Payload {
                            remaining,
                            destination,
                        }
                    };
                    match destination {
                        PayloadDestination::Reference => self.write_reference(bytes)?,
                        PayloadDestination::Range => self.write_range(bytes)?,
                    }
                    input = &input[take..];
                }
            }
        }
        Ok(original)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(part) = self.reference.as_mut() {
            part.file.flush()?;
        }
        if let Some(part) = self.range.as_mut() {
            part.file.flush()?;
        }
        Ok(())
    }
}

fn io_error(error: ArchiveError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let mut left = std::io::BufReader::new(std::fs::File::open(left)?);
    let mut right = std::io::BufReader::new(std::fs::File::open(right)?);
    let mut left_bytes = [0_u8; 64 << 10];
    let mut right_bytes = [0_u8; 64 << 10];
    loop {
        let left_count = left.read(&mut left_bytes)?;
        let right_count = right.read(&mut right_bytes)?;
        if left_count != right_count || left_bytes[..left_count] != right_bytes[..right_count] {
            return Ok(false);
        }
        if left_count == 0 {
            return Ok(true);
        }
    }
}

pub struct CompletedArchiveSet {
    root: OutputRoot,
    pub segments: Vec<ArchiveSetSegment>,
    pub virtual_bytes: u64,
    installed: bool,
}

impl CompletedArchiveSet {
    pub fn persist(self, destination: impl AsRef<Path>) -> Result<()> {
        if self.installed {
            return Err(ArchiveError::Invalid(
                "replacement archive set is already installed",
            ));
        }
        let destination = destination.as_ref();
        if destination.exists() {
            return Err(ArchiveError::Invalid(
                "archive-set destination already exists",
            ));
        }
        sync_directory(self.root.path())?;
        let path = self.root.into_path();
        std::fs::rename(path, destination)?;
        sync_directory(
            destination
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?;
        Ok(())
    }

    pub fn finish_replacement(self) -> Result<()> {
        if !self.installed {
            return Err(ArchiveError::Invalid(
                "new archive set has not been installed",
            ));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub struct ArchiveSetReader {
    root: PathBuf,
    segments: Vec<ArchiveSetSegment>,
    position: u64,
    length: u64,
    open: Option<(usize, std::fs::File)>,
}

impl ArchiveSetReader {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(ArchiveError::Invalid("archive set is not a directory"));
        }
        let mut names = std::fs::read_dir(&root)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        let mut segments = Vec::new();
        let mut virtual_start = 0_u64;
        for name in names {
            let name = name
                .into_string()
                .map_err(|_| ArchiveError::Invalid("archive-set filename is not UTF-8"))?;
            if !name.ends_with(PART_SUFFIX) {
                return Err(ArchiveError::Invalid(
                    "archive set contains an unknown file",
                ));
            }
            let bytes = std::fs::metadata(root.join(&name))?.len();
            let (kind, first_id, last_id) = parse_segment_name(&name)?;
            segments.push(ArchiveSetSegment {
                name,
                virtual_start,
                bytes,
                kind,
                first_id,
                last_id,
            });
            virtual_start = virtual_start
                .checked_add(bytes)
                .ok_or(ArchiveError::FieldTooLarge)?;
        }
        if !segments
            .first()
            .is_some_and(|segment| segment.name == "0000-reference.swdump-part")
            || !segments
                .last()
                .is_some_and(|segment| segment.name == "9999-complete.swdump-part")
        {
            return Err(ArchiveError::Invalid(
                "archive set lacks reference or completion part",
            ));
        }
        Ok(Self {
            root,
            segments,
            position: 0,
            length: virtual_start,
            open: None,
        })
    }

    pub fn segments(&self) -> &[ArchiveSetSegment] {
        &self.segments
    }

    fn segment_at(&self, position: u64) -> Option<usize> {
        self.segments
            .partition_point(|segment| segment.virtual_start <= position)
            .checked_sub(1)
            .filter(|index| {
                let segment = &self.segments[*index];
                position < segment.virtual_start + segment.bytes
            })
    }
}

impl Read for ArchiveSetReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() || self.position == self.length {
            return Ok(0);
        }
        let mut written = 0;
        while written < output.len() && self.position < self.length {
            let index = self.segment_at(self.position).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "virtual archive offset has no segment",
                )
            })?;
            if self.open.as_ref().map(|(open, _)| *open) != Some(index) {
                self.open = Some((
                    index,
                    std::fs::File::open(self.root.join(&self.segments[index].name))?,
                ));
            }
            let segment = &self.segments[index];
            let local = self.position - segment.virtual_start;
            let available = usize::try_from((segment.bytes - local).min(
                (output.len() - written) as u64,
            ))
            .expect("bounded by output length");
            let file = &mut self.open.as_mut().expect("opened above").1;
            file.seek(SeekFrom::Start(local))?;
            file.read_exact(&mut output[written..written + available])?;
            self.position += available as u64;
            written += available;
        }
        Ok(written)
    }
}

impl Seek for ArchiveSetReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let position = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.length) + i128::from(delta),
        };
        if position < 0 || position > i128::from(self.length) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "virtual archive seek is out of bounds",
            ));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

fn parse_segment_name(name: &str) -> Result<(Option<EntityKind>, u64, u64)> {
    if name == "0000-reference.swdump-part" || name == "9999-complete.swdump-part" {
        return Ok((None, 0, 0));
    }
    let (kind, prefix) = if name.starts_with("1000-p") {
        (EntityKind::Page, "1000-p")
    } else if name.starts_with("2000-u") {
        (EntityKind::User, "2000-u")
    } else if name.starts_with("3000-g") {
        (EntityKind::Global, "3000-g")
    } else {
        return Err(ArchiveError::Invalid("unknown archive-set range filename"));
    };
    let stem = name
        .strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(PART_SUFFIX))
        .ok_or(ArchiveError::Invalid("malformed archive-set range filename"))?;
    let separator = match kind {
        EntityKind::Page => "-p",
        EntityKind::User => "-u",
        EntityKind::Global => "-g",
    };
    let (first, last) = stem
        .split_once(separator)
        .ok_or(ArchiveError::Invalid("malformed archive-set range filename"))?;
    let first = first
        .parse()
        .map_err(|_| ArchiveError::Invalid("malformed archive-set first id"))?;
    let last = last
        .parse()
        .map_err(|_| ArchiveError::Invalid("malformed archive-set last id"))?;
    if first > last {
        return Err(ArchiveError::Invalid("reversed archive-set id range"));
    }
    Ok((Some(kind), first, last))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::archive::{ArchiveReader, ArchiveWriter, CompressionSettings, Record};
    use crate::{ContributorMeta, RevisionMeta};

    fn revision(page_id: u64) -> Record {
        let text = (0..2048)
            .map(|offset| (page_id as u8).wrapping_add(offset as u8))
            .collect::<Vec<_>>();
        Record::Revision {
            page_id,
            revision: crate::archive::RevisionRecord {
                meta: RevisionMeta {
                    rev_id: page_id,
                    parent_id: 0,
                    ts: Utc.timestamp_micros(page_id as i64).single().unwrap(),
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
        }
    }

    #[test]
    fn physical_parts_are_one_logical_archive() {
        let mut records = (1..=64).map(revision).collect::<Vec<_>>();
        records.push(Record::Manifest {
            timestamp_micros: 1,
            manifest: crate::archive::ManifestRecord {
                wiki_db: "testwiki".into(),
                content_snapshot: "2026-07-01".into(),
                metadata_snapshot: "2026-07".into(),
                source_files: Vec::new(),
            },
        });
        let mut source = ArchiveWriter::new(Vec::new(), 1).unwrap();
        for record in &records {
            source.write(record).unwrap();
        }
        let (source, _) = source.finish().unwrap();

        let parent = tempfile::tempdir().unwrap();
        let output = ArchiveSetOutput::new_in(parent.path(), 1024).unwrap();
        let (output, _) = crate::archive::repack_with_ref_prefix(
            Cursor::new(source),
            output,
            128,
            CompressionSettings::default(),
            64 << 10,
            4 << 10,
        )
        .unwrap();
        let completed = output.finish().unwrap();
        let destination = parent.path().join("testwiki.swdump");
        completed.persist(&destination).unwrap();

        let set = ArchiveSetReader::open(&destination).unwrap();
        let page_parts = set
            .segments()
            .iter()
            .filter(|segment| segment.kind == Some(EntityKind::Page))
            .count();
        assert!(page_parts > 1);

        let mut reader = ArchiveReader::new(set).unwrap();
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
    fn resumable_output_discards_only_the_open_range() {
        let records = (1..=64).map(revision).collect::<Vec<_>>();
        let parent = tempfile::tempdir().unwrap();
        let name = "assembly-test.partial";
        {
            let output = ArchiveSetOutput::resumable_in(parent.path(), name, 1024).unwrap();
            let mut writer = ArchiveWriter::new(output, 128).unwrap();
            for record in records.iter().take(40) {
                writer.write(record).unwrap();
            }
        }

        let output = ArchiveSetOutput::resumable_in(parent.path(), name, 1024).unwrap();
        let boundary = output
            .resume_after()
            .expect("at least one range was sealed");
        assert!(boundary.id < 40);
        let temporary_parts = parent
            .path()
            .join(name)
            .read_dir()
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
                    .then_some(entry.path())
            })
            .collect::<Vec<_>>();
        assert_eq!(temporary_parts.len(), 1);
        assert_eq!(std::fs::metadata(&temporary_parts[0]).unwrap().len(), 0);

        let mut writer = ArchiveWriter::new(output, 128).unwrap();
        for record in records
            .iter()
            .filter(|record| record.entity() > boundary)
        {
            writer.write(record).unwrap();
        }
        let (output, _) = writer.finish().unwrap();
        let completed = output.finish().unwrap();
        let destination = parent.path().join("testwiki.swdump");
        completed.persist(&destination).unwrap();

        let mut reader = ArchiveReader::new(ArchiveSetReader::open(destination).unwrap()).unwrap();
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
    fn replacement_preserves_ranges_and_streams_one_logical_merge() {
        let records = (1..=64).map(revision).collect::<Vec<_>>();
        let parent = tempfile::tempdir().unwrap();
        let source_path = parent.path().join("source.swdump");
        let mut source =
            ArchiveWriter::new(std::fs::File::create(&source_path).unwrap(), 1).unwrap();
        for record in &records {
            source.write(record).unwrap();
        }
        source.finish().unwrap();
        let output = ArchiveSetOutput::new_in(parent.path(), 1024).unwrap();
        let (output, _) = crate::archive::repack_with_ref_prefix(
            std::fs::File::open(&source_path).unwrap(),
            output,
            128,
            CompressionSettings::default(),
            64 << 10,
            4 << 10,
        )
        .unwrap();
        let destination = parent.path().join("testwiki.swdump");
        output.finish().unwrap().persist(&destination).unwrap();

        let before = ArchiveSetReader::open(&destination).unwrap();
        let before_ranges = before
            .segments()
            .iter()
            .filter_map(|segment| {
                segment.kind.map(|kind| crate::archive::EntityKey {
                    kind,
                    id: segment.last_id,
                })
            })
            .collect::<Vec<_>>();
        let update_path = parent.path().join("update.swdump");
        let mut update =
            ArchiveWriter::new(std::fs::File::create(&update_path).unwrap(), 1).unwrap();
        update.write(&revision(32)).unwrap();
        update.write(&revision(65)).unwrap();
        update.finish().unwrap();

        let output = ArchiveSetOutput::replacing(
            &destination,
            1024,
            before.segments(),
        )
        .unwrap();
        let inputs = vec![destination.clone(), update_path];
        let (output, _, _) =
            crate::archive::merge_many_archives_reusing_ref_prefix_at_boundaries(
                &destination,
                &inputs,
                output,
                128,
                CompressionSettings::default(),
                before_ranges,
            )
            .unwrap();
        output
            .finish()
            .unwrap()
            .finish_replacement()
            .unwrap();

        let after = ArchiveSetReader::open(&destination).unwrap();
        let mut reader = ArchiveReader::new(after).unwrap();
        let mut page_ids = Vec::new();
        while let Some(mut frame) = reader.next_frame().unwrap() {
            while let Some(record) = frame.next_record().unwrap() {
                if let Record::Revision { page_id, .. } = record {
                    page_ids.push(page_id);
                }
            }
        }
        assert!(reader.is_complete());
        assert_eq!(page_ids, (1..=65).collect::<Vec<_>>());
    }
}
