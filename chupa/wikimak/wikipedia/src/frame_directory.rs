//! Fixed-width, mmap-backed archive frame directory.
//!
//! Update tails and final-assembly projection use this file instead of
//! materializing every frame in a JSON receipt or a heap `Vec`. Opening
//! validates the complete structural directory, but retains only the mmap.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::archive::{ArchiveError, EntityKey, EntityKind, FrameInfo, FrameLocation, Result};

const FILE_MAGIC: [u8; 8] = *b"SWFRAME\0";
pub(crate) const FORMAT_VERSION: u32 = 1;
const HEADER_BYTES: usize = 128;
const ENTRY_BYTES: usize = 64;
const ARCHIVE_FRAME_HEADER_BYTES: u64 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FrameDirectoryEntry {
    pub(crate) first_entity: EntityKey,
    pub(crate) last_entity: EntityKey,
    pub(crate) compressed_offset: u64,
    pub(crate) records: u64,
    pub(crate) raw_bytes: u64,
    pub(crate) compressed_bytes: u64,
    pub(crate) dictionary_id: Option<u32>,
}

impl From<&FrameLocation> for FrameDirectoryEntry {
    fn from(location: &FrameLocation) -> Self {
        Self {
            first_entity: location.info.first_entity,
            last_entity: location.info.last_entity,
            compressed_offset: location.compressed_offset,
            records: location.info.records,
            raw_bytes: location.info.raw_bytes,
            compressed_bytes: location.info.compressed_bytes,
            dictionary_id: location.info.dictionary_id,
        }
    }
}

impl FrameDirectoryEntry {
    pub(crate) fn frame_info(self) -> FrameInfo {
        FrameInfo {
            first_entity: self.first_entity,
            last_entity: self.last_entity,
            records: self.records,
            raw_bytes: self.raw_bytes,
            compressed_bytes: self.compressed_bytes,
            dictionary_id: self.dictionary_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FrameDirectorySummary {
    pub(crate) identity: [u8; 32],
    pub(crate) bytes: u64,
    pub(crate) frames: u64,
    pub(crate) records: u64,
    pub(crate) dictionary_id: Option<u32>,
    pub(crate) first_entity: Option<EntityKey>,
    pub(crate) last_entity: Option<EntityKey>,
}

#[derive(Debug)]
pub(crate) struct FrameDirectory {
    bytes: memmap2::Mmap,
    summary: FrameDirectorySummary,
    count: usize,
}

impl FrameDirectory {
    /// Map and structurally validate one directory.
    ///
    /// Cost: one file open, one mmap, and one 64-byte read per frame. The file
    /// descriptor is dropped before return and retained memory is constant
    /// apart from the virtual mapping.
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let file_bytes = file.metadata()?.len();
        if file_bytes < HEADER_BYTES as u64 {
            return Err(ArchiveError::Invalid("frame directory is shorter than its header"));
        }
        // The mapping remains valid after `file` is dropped and therefore does
        // not retain one descriptor per opened directory.
        let bytes = unsafe { memmap2::MmapOptions::new().map(&file)? };
        drop(file);
        if bytes[..8] != FILE_MAGIC
            || read_u32(&bytes, 8) != FORMAT_VERSION
            || read_u32(&bytes, 12) as usize != HEADER_BYTES
            || read_u32(&bytes, 16) as usize != ENTRY_BYTES
            || bytes[112..HEADER_BYTES].iter().any(|byte| *byte != 0)
        {
            return Err(ArchiveError::Invalid("unknown frame directory format"));
        }
        let frames = read_u64(&bytes, 24);
        let count = usize::try_from(frames).map_err(|_| ArchiveError::FieldTooLarge)?;
        let expected_bytes = (HEADER_BYTES as u64)
            .checked_add(
                frames
                    .checked_mul(ENTRY_BYTES as u64)
                    .ok_or(ArchiveError::FieldTooLarge)?,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        if read_u64(&bytes, 64) != HEADER_BYTES as u64
            || read_u64(&bytes, 72) != expected_bytes
            || file_bytes != expected_bytes
        {
            return Err(ArchiveError::Invalid("frame directory has invalid array bounds"));
        }
        let records = read_u64(&bytes, 32);
        let dictionary_id = match read_u32(&bytes, 20) {
            0 => None,
            value => Some(value),
        };
        let identity = bytes[80..112].try_into().expect("identity byte count");
        let (first_entity, last_entity) = if frames == 0 {
            if records != 0
                || dictionary_id.is_some()
                || bytes[40..64].iter().any(|byte| *byte != 0)
            {
                return Err(ArchiveError::Invalid(
                    "empty frame directory has nonempty bounds",
                ));
            }
            (None, None)
        } else {
            if bytes[42..48].iter().any(|byte| *byte != 0) {
                return Err(ArchiveError::Invalid(
                    "frame directory header reserved bytes are nonzero",
                ));
            }
            (
                Some(EntityKey {
                    kind: EntityKind::try_from(bytes[40])?,
                    id: read_u64(&bytes, 48),
                }),
                Some(EntityKey {
                    kind: EntityKind::try_from(bytes[41])?,
                    id: read_u64(&bytes, 56),
                }),
            )
        };
        let summary = FrameDirectorySummary {
            identity,
            bytes: expected_bytes,
            frames,
            records,
            dictionary_id,
            first_entity,
            last_entity,
        };
        let directory = Self {
            bytes,
            summary,
            count,
        };
        directory.validate_entries()?;
        Ok(directory)
    }

    pub(crate) fn open_bound(
        path: impl AsRef<Path>,
        expected_identity: [u8; 32],
    ) -> Result<Self> {
        let directory = Self::open(path)?;
        directory.require_identity(expected_identity)?;
        Ok(directory)
    }

    pub(crate) fn require_identity(&self, expected: [u8; 32]) -> Result<()> {
        if self.summary.identity != expected {
            return Err(ArchiveError::Invalid("frame directory identity mismatch"));
        }
        Ok(())
    }

    pub(crate) fn require_archive_bounds(&self, archive_bytes: u64) -> Result<()> {
        if self
            .get(self.count.saturating_sub(1))
            .ok()
            .is_some_and(|entry| {
                entry
                    .compressed_offset
                    .checked_add(entry.compressed_bytes)
                    .is_none_or(|end| end > archive_bytes)
            })
        {
            return Err(ArchiveError::Invalid(
                "frame directory extends beyond its archive",
            ));
        }
        Ok(())
    }

    pub(crate) fn summary(&self) -> FrameDirectorySummary {
        self.summary
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    pub(crate) fn get(&self, position: usize) -> Result<FrameDirectoryEntry> {
        if position >= self.count {
            return Err(ArchiveError::Invalid(
                "frame directory position is out of bounds",
            ));
        }
        decode_entry(self.entry_bytes(position))
    }

    pub(crate) fn iter(&self) -> FrameDirectoryIter<'_> {
        FrameDirectoryIter {
            directory: self,
            position: 0,
        }
    }

    /// Return the first frame whose physical offset is at least `offset`.
    pub(crate) fn lower_bound_offset(&self, offset: u64) -> usize {
        let mut left = 0;
        let mut right = self.count;
        while left < right {
            let middle = left + (right - left) / 2;
            if entry_offset(self.entry_bytes(middle)) < offset {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        left
    }

    pub(crate) fn index_of_offset(&self, offset: u64) -> Option<usize> {
        let position = self.lower_bound_offset(offset);
        (position < self.count && entry_offset(self.entry_bytes(position)) == offset)
            .then_some(position)
    }

    /// Return the first frame that may contain an entity newer than `boundary`.
    ///
    /// Because entity groups never cross frames and frame bounds are strictly
    /// ordered, every earlier frame is wholly at or before the boundary.
    /// Resume code may open the returned suffix and discard records through the
    /// exact receipted entity without scanning source frames from zero.
    pub(crate) fn first_after_entity(&self, boundary: EntityKey) -> usize {
        let mut left = 0;
        let mut right = self.count;
        while left < right {
            let middle = left + (right - left) / 2;
            let last = entry_last_entity(self.entry_bytes(middle));
            if last <= boundary {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        left
    }

    fn entry_bytes(&self, position: usize) -> &[u8] {
        let start = HEADER_BYTES + position * ENTRY_BYTES;
        &self.bytes[start..start + ENTRY_BYTES]
    }

    fn validate_entries(&self) -> Result<()> {
        let mut previous: Option<FrameDirectoryEntry> = None;
        let mut records = 0_u64;
        for position in 0..self.count {
            let entry = decode_entry(self.entry_bytes(position))?;
            validate_entry(entry)?;
            if previous.is_some_and(|prior| {
                prior.last_entity >= entry.first_entity
                    || prior.compressed_offset >= entry.compressed_offset
                    || prior
                        .compressed_offset
                        .checked_add(prior.compressed_bytes)
                        .is_none_or(|end| end > entry.compressed_offset)
            }) {
                return Err(ArchiveError::Invalid(
                    "frame directory entries are not strictly ordered",
                ));
            }
            if entry.dictionary_id != self.summary.dictionary_id {
                return Err(ArchiveError::Invalid(
                    "frame directory changes dictionary ID",
                ));
            }
            records = records
                .checked_add(entry.records)
                .ok_or(ArchiveError::FieldTooLarge)?;
            previous = Some(entry);
        }
        if records != self.summary.records
            || previous.map(|entry| entry.last_entity) != self.summary.last_entity
            || self
                .get(0)
                .ok()
                .map(|entry| entry.first_entity)
                != self.summary.first_entity
        {
            return Err(ArchiveError::Invalid(
                "frame directory summary disagrees with its entries",
            ));
        }
        Ok(())
    }
}

pub(crate) struct FrameDirectoryIter<'a> {
    directory: &'a FrameDirectory,
    position: usize,
}

impl Iterator for FrameDirectoryIter<'_> {
    type Item = Result<FrameDirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.directory.count {
            return None;
        }
        let entry = self.directory.get(self.position);
        self.position += 1;
        Some(entry)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.directory.count - self.position;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for FrameDirectoryIter<'_> {}

pub(crate) fn write_from_archive(
    archive: impl AsRef<Path>,
    path: impl AsRef<Path>,
    identity: [u8; 32],
) -> Result<FrameDirectorySummary> {
    let mut writer = FrameDirectoryWriter::new(path.as_ref(), identity)?;
    crate::archive::visit_file_frame_headers(archive, |info, compressed_offset| {
        writer.push(FrameDirectoryEntry {
            first_entity: info.first_entity,
            last_entity: info.last_entity,
            compressed_offset,
            records: info.records,
            raw_bytes: info.raw_bytes,
            compressed_bytes: info.compressed_bytes,
            dictionary_id: info.dictionary_id,
        })
    })?;
    writer.finish()
}

/// Build one virtual frame directory for an archive-set directory.
///
/// Data parts contain ordinary frame header/payload pairs without a file
/// header. Their local payload offsets are translated into offsets in the
/// imagined concatenation recorded by the title index. Reference and
/// completion parts contain no data frames and are omitted.
///
/// Recovery cost: one buffered sequential pass over compressed range bytes,
/// exactly `128 + 64 * frames` output bytes, one input descriptor at a time,
/// and constant retained memory. Normal assembly emits this directory from
/// write-time frame metadata and does not call this reconstruction path.
pub(crate) fn write_from_archive_set(
    archive: impl AsRef<Path>,
    path: impl AsRef<Path>,
    identity: [u8; 32],
) -> Result<FrameDirectorySummary> {
    #[cfg(test)]
    ARCHIVE_SET_DIRECTORY_RECONSTRUCTIONS.with(|reads| reads.set(reads.get() + 1));
    let archive = archive.as_ref();
    if !archive.is_dir() {
        return Err(ArchiveError::Invalid(
            "whole-set frame directory requires an archive-set directory",
        ));
    }
    let set = crate::archive_set::ArchiveSetReader::open(archive)?;
    let mut writer = FrameDirectoryWriter::new(path.as_ref(), identity)?;
    for segment in set.segments().iter().filter(|segment| segment.kind.is_some()) {
        crate::archive::visit_data_segment_frame_headers(
            archive.join(&segment.name),
            |info, local_offset| {
                let compressed_offset = segment
                    .virtual_start
                    .checked_add(local_offset)
                    .ok_or(ArchiveError::FieldTooLarge)?;
                writer.push(FrameDirectoryEntry {
                    first_entity: info.first_entity,
                    last_entity: info.last_entity,
                    compressed_offset,
                    records: info.records,
                    raw_bytes: info.raw_bytes,
                    compressed_bytes: info.compressed_bytes,
                    dictionary_id: info.dictionary_id,
                })
            },
        )?;
    }
    writer.finish()
}

pub(crate) fn write_from_archive_segment(
    segment: impl AsRef<Path>,
    path: impl AsRef<Path>,
    identity: [u8; 32],
) -> Result<FrameDirectorySummary> {
    #[cfg(test)]
    ARCHIVE_SEGMENT_DIRECTORY_READS.with(|reads| reads.set(reads.get() + 1));
    let mut writer = FrameDirectoryWriter::new(path.as_ref(), identity)?;
    crate::archive::visit_data_segment_frame_headers(
        segment,
        |info, compressed_offset| {
            writer.push(FrameDirectoryEntry {
                first_entity: info.first_entity,
                last_entity: info.last_entity,
                compressed_offset,
                records: info.records,
                raw_bytes: info.raw_bytes,
                compressed_bytes: info.compressed_bytes,
                dictionary_id: info.dictionary_id,
            })
        },
    )?;
    writer.finish()
}

#[cfg(test)]
thread_local! {
    static ARCHIVE_SEGMENT_DIRECTORY_READS: std::cell::Cell<usize> =
        std::cell::Cell::new(0);
    static ARCHIVE_SET_DIRECTORY_RECONSTRUCTIONS: std::cell::Cell<usize> =
        std::cell::Cell::new(0);
}

#[cfg(test)]
pub(crate) fn test_archive_segment_directory_reads() -> usize {
    ARCHIVE_SEGMENT_DIRECTORY_READS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_test_archive_set_directory_reconstructions() {
    ARCHIVE_SET_DIRECTORY_RECONSTRUCTIONS.with(|reads| reads.set(0));
}

#[cfg(test)]
pub(crate) fn test_archive_set_directory_reconstructions() -> usize {
    ARCHIVE_SET_DIRECTORY_RECONSTRUCTIONS.with(std::cell::Cell::get)
}

/// Build a data-segment directory from frame metadata collected by the
/// archive-set writer while it emitted the segment.
///
/// The metadata path deliberately retains the structural checks that matter
/// at this boundary: every frame entry must be valid and ordered, frame
/// payloads must be contiguous from the segment's first 64-byte header to its
/// final byte, and the directory's independent counts/bounds are checked by
/// `FrameDirectoryWriter`. It therefore avoids reopening the completed data
/// segment without turning a caller-provided count into an unchecked index.
pub(crate) fn write_from_archive_entries(
    entries: &[FrameDirectoryEntry],
    archive_bytes: u64,
    path: impl AsRef<Path>,
    identity: [u8; 32],
) -> Result<FrameDirectorySummary> {
    let mut writer = FrameDirectoryWriter::new(path.as_ref(), identity)?;
    let mut expected_offset = ARCHIVE_FRAME_HEADER_BYTES;
    for entry in entries.iter().copied() {
        if entry.compressed_offset != expected_offset {
            return Err(ArchiveError::Invalid(
                "write-time frame metadata has a non-contiguous segment offset",
            ));
        }
        let payload_end = entry
            .compressed_offset
            .checked_add(entry.compressed_bytes)
            .ok_or(ArchiveError::FieldTooLarge)?;
        expected_offset = payload_end
            .checked_add(ARCHIVE_FRAME_HEADER_BYTES)
            .ok_or(ArchiveError::FieldTooLarge)?;
        writer.push(entry)?;
    }
    let complete = if entries.is_empty() {
        archive_bytes == 0
    } else {
        expected_offset == archive_bytes
            .checked_add(ARCHIVE_FRAME_HEADER_BYTES)
            .ok_or(ArchiveError::FieldTooLarge)?
    };
    if !complete {
        return Err(ArchiveError::Invalid(
            "write-time frame metadata does not cover the complete segment",
        ));
    }
    writer.finish()
}

pub(crate) struct FrameDirectoryWriter {
    temporary: tempfile::NamedTempFile,
    destination: std::path::PathBuf,
    parent: std::path::PathBuf,
    identity: [u8; 32],
    frames: u64,
    records: u64,
    dictionary_id: Option<u32>,
    first_entity: Option<EntityKey>,
    last: Option<FrameDirectoryEntry>,
}

impl FrameDirectoryWriter {
    pub(crate) fn new(path: &Path, identity: [u8; 32]) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // A frame directory is itself the durable publication for this
        // derived index. Fresh update ranges have no frame-directory parent
        // until their first replacement is written, so establish that owned
        // output namespace before placing the same-filesystem atomic file in
        // it. The temporary file and final rename remain in one directory.
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&[0; HEADER_BYTES])?;
        Ok(Self {
            temporary,
            destination: path.to_path_buf(),
            parent: parent.to_path_buf(),
            identity,
            frames: 0,
            records: 0,
            dictionary_id: None,
            first_entity: None,
            last: None,
        })
    }

    pub(crate) fn push(&mut self, entry: FrameDirectoryEntry) -> Result<()> {
        validate_entry(entry)?;
        if self.frames == 0 {
            self.dictionary_id = entry.dictionary_id;
        } else if self.dictionary_id != entry.dictionary_id {
            return Err(ArchiveError::Invalid(
                "frame directory changes dictionary ID",
            ));
        }
        if self.last.is_some_and(|prior| {
            prior.last_entity >= entry.first_entity
                || prior.compressed_offset >= entry.compressed_offset
                || prior
                    .compressed_offset
                    .checked_add(prior.compressed_bytes)
                    .is_none_or(|end| end > entry.compressed_offset)
        }) {
            return Err(ArchiveError::Invalid(
                "frame directory entries are not strictly ordered",
            ));
        }
        let encoded = encode_entry(entry)?;
        self.temporary.write_all(&encoded)?;
        self.frames = self
            .frames
            .checked_add(1)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.records = self
            .records
            .checked_add(entry.records)
            .ok_or(ArchiveError::FieldTooLarge)?;
        self.first_entity.get_or_insert(entry.first_entity);
        self.last = Some(entry);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<FrameDirectorySummary> {
        let bytes = (HEADER_BYTES as u64)
            .checked_add(
                self.frames
                    .checked_mul(ENTRY_BYTES as u64)
                    .ok_or(ArchiveError::FieldTooLarge)?,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        if self.temporary.as_file_mut().stream_position()? != bytes {
            return Err(ArchiveError::Invalid(
                "frame directory writer produced an invalid size",
            ));
        }
        let summary = FrameDirectorySummary {
            identity: self.identity,
            bytes,
            frames: self.frames,
            records: self.records,
            dictionary_id: self.dictionary_id,
            first_entity: self.first_entity,
            last_entity: self.last.map(|entry| entry.last_entity),
        };
        let header = encode_header(summary);
        self.temporary.as_file_mut().seek(SeekFrom::Start(0))?;
        self.temporary.write_all(&header)?;
        self.temporary.as_file_mut().sync_all()?;
        self.temporary
            .persist(&self.destination)
            .map_err(|error| ArchiveError::Io(error.error))?;
        sync_directory(&self.parent)?;
        Ok(summary)
    }
}

fn encode_header(summary: FrameDirectorySummary) -> [u8; HEADER_BYTES] {
    let mut bytes = [0_u8; HEADER_BYTES];
    bytes[..8].copy_from_slice(&FILE_MAGIC);
    bytes[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
    bytes[16..20].copy_from_slice(&(ENTRY_BYTES as u32).to_le_bytes());
    bytes[20..24].copy_from_slice(&summary.dictionary_id.unwrap_or(0).to_le_bytes());
    bytes[24..32].copy_from_slice(&summary.frames.to_le_bytes());
    bytes[32..40].copy_from_slice(&summary.records.to_le_bytes());
    if let (Some(first), Some(last)) = (summary.first_entity, summary.last_entity) {
        bytes[40] = first.kind as u8;
        bytes[41] = last.kind as u8;
        bytes[48..56].copy_from_slice(&first.id.to_le_bytes());
        bytes[56..64].copy_from_slice(&last.id.to_le_bytes());
    }
    bytes[64..72].copy_from_slice(&(HEADER_BYTES as u64).to_le_bytes());
    bytes[72..80].copy_from_slice(&summary.bytes.to_le_bytes());
    bytes[80..112].copy_from_slice(&summary.identity);
    bytes
}

fn encode_entry(entry: FrameDirectoryEntry) -> Result<[u8; ENTRY_BYTES]> {
    if entry.dictionary_id == Some(0) {
        return Err(ArchiveError::Invalid(
            "frame directory dictionary ID zero is reserved",
        ));
    }
    let mut bytes = [0_u8; ENTRY_BYTES];
    bytes[0] = entry.first_entity.kind as u8;
    bytes[1] = entry.last_entity.kind as u8;
    bytes[8..16].copy_from_slice(&entry.first_entity.id.to_le_bytes());
    bytes[16..24].copy_from_slice(&entry.last_entity.id.to_le_bytes());
    bytes[24..32].copy_from_slice(&entry.compressed_offset.to_le_bytes());
    bytes[32..40].copy_from_slice(&entry.records.to_le_bytes());
    bytes[40..48].copy_from_slice(&entry.raw_bytes.to_le_bytes());
    bytes[48..56].copy_from_slice(&entry.compressed_bytes.to_le_bytes());
    bytes[56..60].copy_from_slice(&entry.dictionary_id.unwrap_or(0).to_le_bytes());
    Ok(bytes)
}

fn decode_entry(bytes: &[u8]) -> Result<FrameDirectoryEntry> {
    if bytes[2..8].iter().any(|byte| *byte != 0)
        || bytes[60..64].iter().any(|byte| *byte != 0)
    {
        return Err(ArchiveError::Invalid(
            "frame directory entry reserved bytes are nonzero",
        ));
    }
    let dictionary_id = read_u32(bytes, 56);
    Ok(FrameDirectoryEntry {
        first_entity: EntityKey {
            kind: EntityKind::try_from(bytes[0])?,
            id: read_u64(bytes, 8),
        },
        last_entity: EntityKey {
            kind: EntityKind::try_from(bytes[1])?,
            id: read_u64(bytes, 16),
        },
        compressed_offset: read_u64(bytes, 24),
        records: read_u64(bytes, 32),
        raw_bytes: read_u64(bytes, 40),
        compressed_bytes: read_u64(bytes, 48),
        dictionary_id: (dictionary_id != 0).then_some(dictionary_id),
    })
}

fn validate_entry(entry: FrameDirectoryEntry) -> Result<()> {
    if entry.first_entity.kind != entry.last_entity.kind
        || entry.first_entity > entry.last_entity
        || entry.records == 0
        || entry.compressed_bytes == 0
        || entry
            .compressed_offset
            .checked_add(entry.compressed_bytes)
            .is_none()
        || entry.dictionary_id == Some(0)
    {
        return Err(ArchiveError::Invalid("invalid frame directory entry"));
    }
    Ok(())
}

fn entry_offset(bytes: &[u8]) -> u64 {
    read_u64(bytes, 24)
}

fn entry_last_entity(bytes: &[u8]) -> EntityKey {
    EntityKey {
        kind: EntityKind::try_from(bytes[1]).expect("validated frame-directory entity kind"),
        id: read_u64(bytes, 16),
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 bytes"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64 bytes"))
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, offset: u64) -> FrameDirectoryEntry {
        FrameDirectoryEntry {
            first_entity: EntityKey {
                kind: EntityKind::Page,
                id,
            },
            last_entity: EntityKey {
                kind: EntityKind::Page,
                id,
            },
            compressed_offset: offset,
            records: id + 1,
            raw_bytes: 1_000 + id,
            compressed_bytes: 100,
            dictionary_id: Some(7),
        }
    }

    fn write_directory<I>(
        path: &Path,
        identity: [u8; 32],
        entries: I,
    ) -> Result<FrameDirectorySummary>
    where
        I: IntoIterator<Item = FrameDirectoryEntry>,
    {
        let mut writer = FrameDirectoryWriter::new(path, identity)?;
        for entry in entries {
            writer.push(entry)?;
        }
        writer.finish()
    }

    #[test]
    fn round_trip_and_binary_lookups_are_lazy() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("frames.swframe");
        let identity = [9; 32];
        let summary = write_directory(
            &path,
            identity,
            [entry(1, 200), entry(3, 400), entry(8, 900)],
        )
        .unwrap();
        assert_eq!(summary.bytes, (HEADER_BYTES + 3 * ENTRY_BYTES) as u64);
        assert_eq!(summary.records, 15);

        let directory = FrameDirectory::open_bound(&path, identity).unwrap();
        assert_eq!(directory.summary(), summary);
        assert_eq!(directory.len(), 3);
        assert_ne!(directory.len(), 0);
        assert_eq!(directory.index_of_offset(400), Some(1));
        assert_eq!(directory.index_of_offset(401), None);
        assert_eq!(directory.lower_bound_offset(401), 2);
        assert_eq!(
            directory.first_after_entity(EntityKey {
                kind: EntityKind::Page,
                id: 3,
            }),
            2
        );
        assert_eq!(
            directory.first_after_entity(EntityKey {
                kind: EntityKind::Page,
                id: 8,
            }),
            3
        );
        assert!(directory.require_identity([8; 32]).is_err());
    }

    #[test]
    fn empty_directory_has_no_synthetic_bounds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("empty.swframe");
        let summary = write_directory(&path, [1; 32], []).unwrap();
        assert_eq!(summary.frames, 0);
        assert_eq!(summary.records, 0);
        assert_eq!(summary.first_entity, None);
        assert_eq!(summary.last_entity, None);
        assert_eq!(FrameDirectory::open(path).unwrap().len(), 0);
    }

    #[test]
    fn writer_establishes_a_fresh_destination_parent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ranges/frame-directories/new.swframe");

        let summary = write_directory(&path, [7; 32], [entry(1, 200)]).unwrap();

        assert_eq!(summary.frames, 1);
        assert_eq!(
            FrameDirectory::open_bound(&path, [7; 32]).unwrap().len(),
            1
        );
    }

    #[test]
    fn writer_rejects_disordered_entries_without_publishing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("bad.swframe");
        let error = write_directory(&path, [0; 32], [entry(2, 400), entry(1, 500)])
            .unwrap_err();
        assert!(matches!(error, ArchiveError::Invalid(_)));
        assert!(!path.exists());
    }

    #[test]
    fn open_rejects_corrupt_bounds_order_and_reserved_bytes() {
        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("good.swframe");
        write_directory(&good, [3; 32], [entry(1, 200), entry(2, 400)]).unwrap();

        for (name, offset, value) in [
            ("magic", 0, 0_u8),
            ("header-reserved", 20, 1),
            ("entry-reserved", HEADER_BYTES + 2, 1),
            ("entry-kind", HEADER_BYTES, 9),
        ] {
            let path = root.path().join(name);
            std::fs::copy(&good, &path).unwrap();
            let mut bytes = std::fs::read(&path).unwrap();
            bytes[offset] = value;
            std::fs::write(&path, bytes).unwrap();
            assert!(FrameDirectory::open(path).is_err(), "{name}");
        }

        let path = root.path().join("order");
        std::fs::copy(&good, &path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[HEADER_BYTES + ENTRY_BYTES + 8..HEADER_BYTES + ENTRY_BYTES + 16]
            .copy_from_slice(&1_u64.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(FrameDirectory::open(path).is_err());
    }

    #[test]
    fn open_rejects_truncation_and_count_overflow_before_iteration() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("frames.swframe");
        write_directory(&path, [4; 32], [entry(1, 200)]).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len((HEADER_BYTES + ENTRY_BYTES - 1) as u64)
            .unwrap();
        assert!(FrameDirectory::open(&path).is_err());

        let overflow = root.path().join("overflow.swframe");
        let mut header = encode_header(FrameDirectorySummary {
            identity: [5; 32],
            bytes: HEADER_BYTES as u64,
            frames: 0,
            records: 0,
            dictionary_id: None,
            first_entity: None,
            last_entity: None,
        });
        header[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(&overflow, header).unwrap();
        assert!(matches!(
            FrameDirectory::open(overflow),
            Err(ArchiveError::FieldTooLarge)
        ));
    }

    #[test]
    fn large_directory_does_not_materialize_entries_on_open_or_iteration() {
        const COUNT: u64 = 100_000;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.swframe");
        let produced = std::cell::Cell::new(0_u64);
        let entries = (0..COUNT).map(|id| {
            produced.set(produced.get() + 1);
            entry(id, 1_000 + id * 100)
        });
        write_directory(&path, [6; 32], entries).unwrap();
        assert_eq!(produced.get(), COUNT);

        let directory = FrameDirectory::open(&path).unwrap();
        assert_eq!(directory.len(), COUNT as usize);
        assert_eq!(directory.lower_bound_offset(1_000 + (COUNT - 3) * 100), COUNT as usize - 3);
        // The directory owns an mmap and scalar summary, not one Rust object
        // per frame. Increasing COUNT therefore does not change this value.
        assert!(std::mem::size_of::<FrameDirectory>() < 256);
    }

    #[test]
    fn archive_headers_stream_into_directory_and_reader_consumes_mmap_suffix() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("tail.swdump");
        let mut writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 1)
                .unwrap();
        for page_id in 1..=3 {
            writer
                .write(&crate::archive::Record::PageState {
                    page_id,
                    timestamp_micros: 10,
                    title: format!("Page {page_id}"),
                    namespace: Some(0),
                    deleted: false,
                })
                .unwrap();
        }
        writer.finish().unwrap();

        let path = root.path().join("frames.swframe");
        let identity = [7; 32];
        let summary = write_from_archive(&archive, &path, identity).unwrap();
        assert_eq!(summary.frames, 3);
        assert_eq!(summary.records, 3);
        let directory = std::sync::Arc::new(
            FrameDirectory::open_bound(&path, identity).unwrap(),
        );
        let second_offset = directory.get(1).unwrap().compressed_offset;
        let mut reader = crate::archive::ArchiveRecordReader::open_frame_directory(
            &archive,
            std::sync::Arc::clone(&directory),
            1,
        )
        .unwrap();
        assert_eq!(reader.remaining_frame_count(), 2);
        let record = reader.next_record().unwrap().unwrap();
        assert_eq!(record.entity().id, 2);
        assert_eq!(reader.current_frame_offset(), Some(second_offset));
        assert_eq!(reader.current_frame_records_read(), 1);
        assert_eq!(reader.next_record().unwrap().unwrap().entity().id, 3);
        assert_eq!(reader.next_record().unwrap(), None);
        assert_eq!(reader.current_frame_offset(), None);
    }

    #[test]
    fn range_segment_headers_stream_with_local_offsets_and_require_clean_eof() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("source.swdump");
        let mut writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 1)
                .unwrap();
        for page_id in 1..=3 {
            writer
                .write(&crate::archive::Record::PageState {
                    page_id,
                    timestamp_micros: 10,
                    title: format!("Page {page_id}"),
                    namespace: Some(0),
                    deleted: false,
                })
                .unwrap();
        }
        writer.finish().unwrap();
        let whole_directory_path = root.path().join("whole.swframe");
        write_from_archive(&archive, &whole_directory_path, [1; 32]).unwrap();
        let whole = FrameDirectory::open(&whole_directory_path).unwrap();
        let first = whole.get(0).unwrap();
        let last = whole.get(whole.len() - 1).unwrap();
        const ARCHIVE_FRAME_HEADER_BYTES: u64 = 64;
        let segment_start = first.compressed_offset - ARCHIVE_FRAME_HEADER_BYTES;
        let segment_end = last.compressed_offset + last.compressed_bytes;
        let archive_bytes = std::fs::read(&archive).unwrap();
        let segment = root.path().join("range.swdump-part");
        std::fs::write(
            &segment,
            &archive_bytes[segment_start as usize..segment_end as usize],
        )
        .unwrap();

        let directory_path = root.path().join("range.swframe");
        let summary =
            write_from_archive_segment(&segment, &directory_path, [2; 32]).unwrap();
        assert_eq!(summary.frames, 3);
        let directory = FrameDirectory::open(&directory_path).unwrap();
        assert_eq!(
            directory.get(0).unwrap().compressed_offset,
            ARCHIVE_FRAME_HEADER_BYTES
        );
        assert_eq!(
            directory.get(2).unwrap().compressed_offset,
            last.compressed_offset - segment_start
        );

        let truncated = root.path().join("truncated.swdump-part");
        std::fs::write(
            &truncated,
            &archive_bytes[segment_start as usize..segment_end as usize - 1],
        )
        .unwrap();
        let rejected = root.path().join("rejected.swframe");
        assert!(write_from_archive_segment(&truncated, &rejected, [3; 32]).is_err());
        assert!(!rejected.exists());
        }
    }

    #[test]
    fn whole_set_directory_uses_virtual_offsets_and_reads_a_suffix() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("wiki.swdump");
        let output = crate::archive_set::ArchiveSetOutput::new_in(root.path(), 256).unwrap();
        let mut writer = crate::archive::ArchiveWriter::with_ref_prefix(
            output,
            1,
            crate::archive::CompressionSettings::default(),
            b"whole set frame directory prefix",
        )
        .unwrap();
        for page_id in 1..=12_u64 {
            writer
                .write(&crate::archive::Record::PageState {
                    page_id,
                    timestamp_micros: page_id as i64,
                    title: format!("Page {page_id}"),
                    namespace: Some(0),
                    deleted: false,
                })
                .unwrap();
        }
        let (output, frames) = writer.finish().unwrap();
        output.finish().unwrap().persist(&archive).unwrap();
        assert!(crate::archive_set::ArchiveSetReader::open(&archive)
            .unwrap()
            .segments()
            .iter()
            .filter(|segment| segment.kind.is_some())
            .count()
            > 1);

        let path = root.path().join("wiki.swframe");
        let identity = [0x5a; 32];
        let summary = write_from_archive_set(&archive, &path, identity).unwrap();
        assert_eq!(summary.frames, frames);
        let directory = std::sync::Arc::new(FrameDirectory::open_bound(&path, identity).unwrap());
        let (_, indexed, complete) = crate::archive::index_file(&archive).unwrap();
        assert!(complete);
        assert_eq!(indexed.len(), directory.len());
        for (position, location) in indexed.iter().enumerate() {
            assert_eq!(
                FrameDirectoryEntry::from(location),
                directory.get(position).unwrap()
            );
        }

        let start = directory.len() / 2;
        let expected_records = directory
            .iter()
            .skip(start)
            .map(|entry| entry.unwrap().records)
            .sum::<u64>();
        let mut suffix = crate::archive::ArchiveRecordReader::open_frame_directory(
            &archive,
            directory,
            start,
        )
        .unwrap();
        let mut actual_records = 0_u64;
        while suffix.next_record().unwrap().is_some() {
            actual_records += 1;
        }
        assert_eq!(actual_records, expected_records);
    }
