//! Immutable title-history and frame-range indexes generated from an archive.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use crate::archive::{PageActionKind, Record, SiteInfoRecord};

const FILE_MAGIC: [u8; 8] = *b"SWTITLE\0";
const FILE_VERSION: u32 = 2;
const HEADER_BYTES: usize = 64;
const ENTRY_BYTES: usize = 16;
const FRAME_ENTRY_BYTES: usize = 64;
const SEGMENT_ENTRY_BYTES: usize = 40;

#[derive(Clone)]
struct Interval {
    title: String,
    start: i64,
    end: i64,
    page_id: u64,
}

struct Projection {
    page_id: u64,
    closed: Vec<Interval>,
    candidates: Vec<(String, i64)>,
}

#[derive(Debug)]
pub struct TitleIndex {
    bytes: memmap2::Mmap,
    title_offset: usize,
    title_count: usize,
    frame_offset: usize,
    frame_count: usize,
    segment_offset: usize,
    segment_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FrameIndexEntry {
    pub(crate) info: crate::archive::FrameInfo,
    pub(crate) compressed_offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SegmentIndexEntry {
    pub(crate) role: u8,
    pub(crate) first_id: u64,
    pub(crate) last_id: u64,
    pub(crate) virtual_start: u64,
    pub(crate) bytes: u64,
}

type PageTitleInputs = (
    Vec<(i64, String, Option<i64>, bool)>,
    Vec<(i64, crate::archive::PageActionRecord)>,
);

/// Title history accumulated while a sorted archive is being written.
///
/// Keeping this projection beside the merge avoids decoding every revision
/// text a second time merely to recover the comparatively tiny page-state and
/// page-action subset used by the title index.
pub(crate) struct TitleIndexBuilder {
    site_info: Option<(i64, SiteInfoRecord)>,
    pages: BTreeMap<u64, PageTitleInputs>,
}

impl TitleIndexBuilder {
    pub(crate) fn new() -> Self {
        Self {
            site_info: None,
            pages: BTreeMap::new(),
        }
    }

    pub(crate) fn observe(&mut self, record: &Record) {
        match record {
            Record::PageState {
                page_id,
                timestamp_micros,
                title,
                namespace,
                deleted,
            } => self.pages.entry(*page_id).or_default().0.push((
                *timestamp_micros,
                title.clone(),
                *namespace,
                *deleted,
            )),
            Record::PageAction {
                entity,
                timestamp_micros,
                action,
            } if entity.kind == crate::archive::EntityKind::Page => {
                self.pages
                    .entry(entity.id)
                    .or_default()
                    .1
                    .push((*timestamp_micros, action.clone()));
            }
            Record::SiteInfo {
                timestamp_micros,
                site_info,
            } if self
                .site_info
                .as_ref()
                .is_none_or(|(current, _)| timestamp_micros > current) =>
            {
                self.site_info = Some((*timestamp_micros, site_info.clone()));
            }
            _ => {}
        }
    }

    pub(crate) fn finish(
        self,
        archive: impl AsRef<Path>,
        output: impl AsRef<Path>,
    ) -> crate::archive::Result<u64> {
        write_index(
            archive.as_ref(),
            output.as_ref(),
            self.site_info.map(|(_, site_info)| site_info),
            self.pages,
        )
    }
}

pub fn build(
    archive: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> crate::archive::Result<u64> {
    let mut builder = TitleIndexBuilder::new();
    let workers = std::env::var("SARUN_WIKIMAK_CPU_BUDGET")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
    let mut last_progress = std::time::Instant::now();
    crate::archive::visit_title_records_parallel(archive.as_ref(), workers, |record| {
        builder.observe(&record);
    }, |completed, total| {
        if completed == total || last_progress.elapsed() >= std::time::Duration::from_secs(2) {
            eprintln!("title index metadata scan: {completed}/{total} frames");
            last_progress = std::time::Instant::now();
        }
    })?;
    builder.finish(archive, output)
}

fn write_index(
    archive: &Path,
    output: &Path,
    site_info: Option<SiteInfoRecord>,
    pages: BTreeMap<u64, PageTitleInputs>,
) -> crate::archive::Result<u64> {
    let (_, frames, complete) = crate::archive::index_file(archive)?;
    if !complete {
        return Err(crate::archive::ArchiveError::Invalid(
            "archive has no clean completion marker",
        ));
    }
    let site_info = site_info.ok_or(crate::archive::ArchiveError::Invalid(
        "archive has no siteinfo record",
    ))?;

    let mut projections = Vec::new();
    for (page_id, (mut states, mut actions)) in pages {
        for (_, title, namespace, _) in &mut states {
            if let Some(namespace) = namespace {
                *title = title_in_namespace(title, *namespace, &site_info);
            }
        }
        states.sort_by(|left, right| {
            right
                .2
                .is_some()
                .cmp(&left.2.is_some())
                .then(right.0.cmp(&left.0))
                .then(left.1.cmp(&right.1))
        });
        states.dedup();
        actions.sort_by_key(|(timestamp, action)| (*timestamp, action.tie_sequence));
        let state = states.first().map(|(at, title, namespace, deleted)| {
            (title.as_str(), *at, namespace.is_some(), *deleted)
        });
        projections.push(project_page(
            page_id,
            state,
            &actions,
            &site_info,
        ));
    }

    let mut intervals = assign_current_titles(&projections)?;
    intervals.extend(
        projections
            .into_iter()
            .flat_map(|projection| projection.closed),
    );
    intervals.sort_by(|left, right| {
        left.title
            .cmp(&right.title)
            .then(left.start.cmp(&right.start))
            .then(left.end.cmp(&right.end))
            .then(left.page_id.cmp(&right.page_id))
    });
    intervals.dedup_by(|left, right| {
        left.title == right.title
            && left.start == right.start
            && left.end == right.end
            && left.page_id == right.page_id
    });

    let mut changes = BTreeMap::<(u64, u32), u32>::new();
    for (title, time, page_id) in ownership_changes(&intervals) {
        changes.insert(
            (coded_title(title, &site_info), seconds(time)),
            u32::try_from(page_id)
                .map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?,
        );
    }

    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let title_count =
        u64::try_from(changes.len()).map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
    let frame_count =
        u64::try_from(frames.len()).map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
    let segments = if archive.is_dir() {
        crate::archive_set::ArchiveSetReader::open(archive)?
            .segments()
            .to_vec()
    } else {
        Vec::new()
    };
    let segment_count =
        u64::try_from(segments.len()).map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
    let frame_offset = HEADER_BYTES
        .checked_add(
            changes
                .len()
                .checked_mul(ENTRY_BYTES)
                .ok_or(crate::archive::ArchiveError::FieldTooLarge)?,
        )
        .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
    let segment_offset = frame_offset
        .checked_add(
            frames
                .len()
                .checked_mul(FRAME_ENTRY_BYTES)
                .ok_or(crate::archive::ArchiveError::FieldTooLarge)?,
        )
        .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
    temporary.write_all(&FILE_MAGIC)?;
    temporary.write_all(&FILE_VERSION.to_le_bytes())?;
    temporary.write_all(&(HEADER_BYTES as u32).to_le_bytes())?;
    temporary.write_all(&title_count.to_le_bytes())?;
    temporary.write_all(&frame_count.to_le_bytes())?;
    temporary.write_all(&segment_count.to_le_bytes())?;
    temporary.write_all(&(HEADER_BYTES as u64).to_le_bytes())?;
    temporary.write_all(&(frame_offset as u64).to_le_bytes())?;
    temporary.write_all(&(segment_offset as u64).to_le_bytes())?;
    for ((title, time), page_id) in &changes {
        temporary.write_all(&title.to_le_bytes())?;
        temporary.write_all(&time.to_le_bytes())?;
        temporary.write_all(&page_id.to_le_bytes())?;
    }
    for frame in &frames {
        temporary.write_all(&[frame.info.first_entity.kind as u8])?;
        temporary.write_all(&[frame.info.last_entity.kind as u8])?;
        temporary.write_all(&[0; 6])?;
        temporary.write_all(&frame.info.first_entity.id.to_le_bytes())?;
        temporary.write_all(&frame.info.last_entity.id.to_le_bytes())?;
        temporary.write_all(&frame.compressed_offset.to_le_bytes())?;
        temporary.write_all(&frame.info.records.to_le_bytes())?;
        temporary.write_all(&frame.info.raw_bytes.to_le_bytes())?;
        temporary.write_all(&frame.info.compressed_bytes.to_le_bytes())?;
        temporary.write_all(&frame.info.dictionary_id.unwrap_or(0).to_le_bytes())?;
        temporary.write_all(&[0; 4])?;
    }
    for segment in &segments {
        let role = match segment.kind {
            Some(crate::archive::EntityKind::Page) => 1,
            Some(crate::archive::EntityKind::User) => 2,
            Some(crate::archive::EntityKind::Global) => 3,
            None if segment.name.starts_with("0000-") => 0,
            None if segment.name.starts_with("9999-") => 4,
            None => {
                return Err(crate::archive::ArchiveError::Invalid(
                    "unknown archive-set segment role",
                ))
            }
        };
        temporary.write_all(&[role])?;
        temporary.write_all(&[0; 7])?;
        temporary.write_all(&segment.first_id.to_le_bytes())?;
        temporary.write_all(&segment.last_id.to_le_bytes())?;
        temporary.write_all(&segment.virtual_start.to_le_bytes())?;
        temporary.write_all(&segment.bytes.to_le_bytes())?;
    }
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(output)
        .map_err(|error| crate::archive::ArchiveError::Io(error.error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(changes.len() as u64)
}

impl TitleIndex {
    pub fn open(path: impl AsRef<Path>) -> crate::archive::Result<Self> {
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() < HEADER_BYTES as u64 {
            return Err(crate::archive::ArchiveError::Invalid(
                "title index has no complete header",
            ));
        }
        // The mapping is read-only and remains valid for the lifetime of `file`.
        let bytes = unsafe { memmap2::MmapOptions::new().map(&file)? };
        if bytes[..8] != FILE_MAGIC
            || u32::from_le_bytes(bytes[8..12].try_into().expect("version bytes"))
                != FILE_VERSION
            || u32::from_le_bytes(bytes[12..16].try_into().expect("header bytes")) as usize
                != HEADER_BYTES
        {
            return Err(crate::archive::ArchiveError::Invalid(
                "unknown title index format",
            ));
        }
        let title_count = usize::try_from(u64::from_le_bytes(
            bytes[16..24].try_into().expect("title count bytes"),
        ))
        .map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
        let frame_count = usize::try_from(u64::from_le_bytes(
            bytes[24..32].try_into().expect("frame count bytes"),
        ))
        .map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
        let segment_count = usize::try_from(u64::from_le_bytes(
            bytes[32..40].try_into().expect("segment count bytes"),
        ))
        .map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
        let title_offset = usize::try_from(u64::from_le_bytes(
            bytes[40..48].try_into().expect("title offset bytes"),
        ))
        .map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
        let frame_offset = usize::try_from(u64::from_le_bytes(
            bytes[48..56].try_into().expect("frame offset bytes"),
        ))
        .map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
        let segment_offset = usize::try_from(u64::from_le_bytes(
            bytes[56..64].try_into().expect("segment offset bytes"),
        ))
        .map_err(|_| crate::archive::ArchiveError::FieldTooLarge)?;
        let expected_frame_offset = title_offset
            .checked_add(
                title_count
                    .checked_mul(ENTRY_BYTES)
                    .ok_or(crate::archive::ArchiveError::FieldTooLarge)?,
            )
            .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
        let expected_segment_offset = frame_offset
            .checked_add(
                frame_count
                    .checked_mul(FRAME_ENTRY_BYTES)
                    .ok_or(crate::archive::ArchiveError::FieldTooLarge)?,
            )
            .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
        let expected_len = segment_offset
            .checked_add(
                segment_count
                    .checked_mul(SEGMENT_ENTRY_BYTES)
                    .ok_or(crate::archive::ArchiveError::FieldTooLarge)?,
            )
            .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
        if title_offset != HEADER_BYTES
            || frame_offset != expected_frame_offset
            || segment_offset != expected_segment_offset
            || bytes.len() != expected_len
        {
            return Err(crate::archive::ArchiveError::Invalid(
                "title index arrays have invalid bounds",
            ));
        }
        Ok(Self {
            bytes,
            title_offset,
            title_count,
            frame_offset,
            frame_count,
            segment_offset,
            segment_count,
        })
    }

    pub fn lookup(
        &self,
        title: &str,
        timestamp_micros: i64,
        site_info: &SiteInfoRecord,
    ) -> Option<u64> {
        let key = coded_title(title, site_info);
        let time = seconds(timestamp_micros);
        let index = self
            .binary_search_by(|position| self.key_time(position).cmp(&(key, time)));
        let position = match index {
            Ok(position) => position,
            Err(0) => return None,
            Err(position) => position - 1,
        };
        let (stored_key, _) = self.key_time(position);
        let page_id = self.page_id(position);
        (stored_key == key && page_id != 0).then_some(u64::from(page_id))
    }

    pub fn entries(&self) -> u64 {
        self.len() as u64
    }

    /// Count the page titles currently owned by a page (the last mapping for
    /// each encoded title), optionally restricted to one namespace.  This is
    /// a cheap mmap walk over the sorted title array; it does not decompress
    /// archive frames and it does not count historical ownership intervals.
    pub fn current_page_count(&self, namespace: Option<i32>) -> u64 {
        let mut count = 0_u64;
        let mut position = 0;
        while position < self.len() {
            let key = self.key_time(position).0;
            let mut end = position + 1;
            while end < self.len() && self.key_time(end).0 == key {
                end += 1;
            }
            let current_page = self.page_id(end - 1);
            if current_page != 0
                && namespace.is_none_or(|wanted| coded_namespace(key) == Some(wanted))
            {
                count += 1;
            }
            position = end;
        }
        count
    }

    pub(crate) fn frame_count(&self) -> usize {
        self.frame_count
    }

    pub(crate) fn frame(&self, position: usize) -> crate::archive::Result<FrameIndexEntry> {
        let start = self
            .frame_offset
            .checked_add(
                position
                    .checked_mul(FRAME_ENTRY_BYTES)
                    .ok_or(crate::archive::ArchiveError::FieldTooLarge)?,
            )
            .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
        let entry = self
            .bytes
            .get(start..start + FRAME_ENTRY_BYTES)
            .ok_or(crate::archive::ArchiveError::Invalid(
                "frame index position is out of bounds",
            ))?;
        let first_kind = crate::archive::EntityKind::try_from(entry[0])?;
        let last_kind = crate::archive::EntityKind::try_from(entry[1])?;
        let dictionary_id =
            u32::from_le_bytes(entry[56..60].try_into().expect("dictionary id bytes"));
        Ok(FrameIndexEntry {
            info: crate::archive::FrameInfo {
                first_entity: crate::archive::EntityKey {
                    kind: first_kind,
                    id: u64::from_le_bytes(entry[8..16].try_into().expect("first id bytes")),
                },
                last_entity: crate::archive::EntityKey {
                    kind: last_kind,
                    id: u64::from_le_bytes(entry[16..24].try_into().expect("last id bytes")),
                },
                records: u64::from_le_bytes(
                    entry[32..40].try_into().expect("record count bytes"),
                ),
                raw_bytes: u64::from_le_bytes(
                    entry[40..48].try_into().expect("raw byte bytes"),
                ),
                compressed_bytes: u64::from_le_bytes(
                    entry[48..56].try_into().expect("compressed byte bytes"),
                ),
                dictionary_id: (dictionary_id != 0).then_some(dictionary_id),
            },
            compressed_offset: u64::from_le_bytes(
                entry[24..32].try_into().expect("compressed offset bytes"),
            ),
        })
    }

    pub(crate) fn segment_count(&self) -> usize {
        self.segment_count
    }

    pub(crate) fn segment(
        &self,
        position: usize,
    ) -> crate::archive::Result<SegmentIndexEntry> {
        let start = self
            .segment_offset
            .checked_add(
                position
                    .checked_mul(SEGMENT_ENTRY_BYTES)
                    .ok_or(crate::archive::ArchiveError::FieldTooLarge)?,
            )
            .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
        let entry = self
            .bytes
            .get(start..start + SEGMENT_ENTRY_BYTES)
            .ok_or(crate::archive::ArchiveError::Invalid(
                "segment index position is out of bounds",
            ))?;
        if entry[1..8].iter().any(|byte| *byte != 0) || entry[0] > 4 {
            return Err(crate::archive::ArchiveError::Invalid(
                "segment index entry is malformed",
            ));
        }
        Ok(SegmentIndexEntry {
            role: entry[0],
            first_id: u64::from_le_bytes(entry[8..16].try_into().unwrap()),
            last_id: u64::from_le_bytes(entry[16..24].try_into().unwrap()),
            virtual_start: u64::from_le_bytes(entry[24..32].try_into().unwrap()),
            bytes: u64::from_le_bytes(entry[32..40].try_into().unwrap()),
        })
    }

    fn len(&self) -> usize {
        self.title_count
    }

    pub(crate) fn entry_count(&self) -> usize {
        self.title_count
    }

    fn key_time(&self, position: usize) -> (u64, u32) {
        let start = self.title_offset + position * ENTRY_BYTES;
        let entry = &self.bytes[start..start + ENTRY_BYTES];
        (
            u64::from_le_bytes(entry[..8].try_into().expect("eight title bytes")),
            u32::from_le_bytes(entry[8..12].try_into().expect("four time bytes")),
        )
    }

    fn page_id(&self, position: usize) -> u32 {
        let start = self.title_offset + position * ENTRY_BYTES;
        let entry = &self.bytes[start..start + ENTRY_BYTES];
        u32::from_le_bytes(entry[12..].try_into().expect("four page-id bytes"))
    }

    fn binary_search_by(
        &self,
        mut compare: impl FnMut(usize) -> std::cmp::Ordering,
    ) -> Result<usize, usize> {
        let mut left = 0;
        let mut right = self.len();
        while left < right {
            let middle = left + (right - left) / 2;
            match compare(middle) {
                std::cmp::Ordering::Less => left = middle + 1,
                std::cmp::Ordering::Greater => right = middle,
                std::cmp::Ordering::Equal => return Ok(middle),
            }
        }
        Err(left)
    }
}

fn ownership_changes(intervals: &[Interval]) -> Vec<(&str, i64, u64)> {
    let mut output = Vec::new();
    let mut first = 0;
    while first < intervals.len() {
        let title = intervals[first].title.as_str();
        let mut last = first + 1;
        while last < intervals.len() && intervals[last].title == title {
            last += 1;
        }
        let mut events = Vec::with_capacity((last - first) * 2);
        for interval in &intervals[first..last] {
            events.push((interval.start, true, interval.start, interval.page_id));
            if interval.end != i64::MAX {
                events.push((interval.end, false, interval.start, interval.page_id));
            }
        }
        events.sort_by_key(|(time, starts, start, page_id)| {
            (*time, *starts, *start, *page_id)
        });
        let mut active = BTreeMap::<(i64, u64), u32>::new();
        let mut owner = 0_u64;
        let mut position = 0;
        while position < events.len() {
            let time = events[position].0;
            while position < events.len() && events[position].0 == time {
                let (_, starts, start, page_id) = events[position];
                if starts {
                    *active.entry((start, page_id)).or_default() += 1;
                } else if let Some(count) = active.get_mut(&(start, page_id)) {
                    *count -= 1;
                    if *count == 0 {
                        active.remove(&(start, page_id));
                    }
                }
                position += 1;
            }
            let next_owner = active.last_key_value().map_or(0, |((_, page_id), _)| *page_id);
            if next_owner != owner {
                output.push((title, time, next_owner));
                owner = next_owner;
            }
        }
        first = last;
    }
    output
}

fn project_page(
    page_id: u64,
    current_title: Option<(&str, i64, bool, bool)>,
    actions: &[(i64, crate::archive::PageActionRecord)],
    site: &SiteInfoRecord,
) -> Projection {
    let mut title = None::<String>;
    let mut exists = false;
    let mut since = i64::MIN;
    let mut closed = Vec::new();
    for (at, action) in actions {
        if exists && since < *at {
            if let Some(title) = &title {
                closed.push(interval(title, since, *at, page_id));
            }
        }
        let observed = full_title(action, site);
        match action.kind {
            PageActionKind::Create
            | PageActionKind::LoggedCreate
            | PageActionKind::Move
            | PageActionKind::Restore => {
                exists = true;
                title = Some(observed);
            }
            PageActionKind::Delete if action.resulting_deleted != Some(false) => {
                exists = false;
            }
            _ => {
                title = Some(observed);
                if let Some(deleted) = action.resulting_deleted {
                    exists = !deleted;
                }
            }
        }
        since = *at;
    }
    let simulated = exists.then_some(title).flatten().map(|title| (title, since));
    let stated = current_title
        .filter(|(_, _, _, deleted)| !deleted)
        .map(|(title, at, _, _)| (normalize(title), at));
    let mut candidates = Vec::new();
    if current_title.is_some_and(|(_, _, _, deleted)| deleted) {
        // A current deletion observation is authoritative. Historical actions
        // still contribute the closed intervals above, but cannot resurrect it.
    } else if current_title.is_some_and(|(_, _, reliable, _)| reliable) {
        candidates.extend(stated);
        candidates.extend(simulated);
    } else {
        candidates.extend(simulated);
        candidates.extend(stated);
    }
    candidates.dedup_by(|left, right| left.0 == right.0);
    candidates.retain(|(title, _)| !title.is_empty());
    Projection {
        page_id,
        closed,
        candidates,
    }
}

fn assign_current_titles(projections: &[Projection]) -> crate::archive::Result<Vec<Interval>> {
    let mut assigned = projections
        .iter()
        .map(|projection| (!projection.candidates.is_empty()).then_some(0_usize))
        .collect::<Vec<_>>();
    loop {
        let mut owners = BTreeMap::<&str, Vec<usize>>::new();
        for (index, choice) in assigned.iter().enumerate() {
            if let Some(choice) = choice {
                owners
                    .entry(&projections[index].candidates[*choice].0)
                    .or_default()
                    .push(index);
            }
        }
        let conflicts = owners
            .values()
            .filter(|owners| owners.len() > 1)
            .cloned()
            .collect::<Vec<_>>();
        if conflicts.is_empty() {
            break;
        }
        let mut changed = false;
        for conflict in conflicts {
            for index in conflict {
                if let Some((choice, _)) = projections[index]
                    .candidates
                    .iter()
                    .enumerate()
                    .skip(1)
                    .find(|(_, (title, _))| !owners.contains_key(title.as_str()))
                {
                    assigned[index] = Some(choice);
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            let conflict = owners
                .into_iter()
                .find(|(_, owners)| owners.len() > 1)
                .expect("conflicts were not empty");
            return Err(crate::archive::ArchiveError::Conflict(format!(
                "current title {:?} is claimed by pages {:?}",
                conflict.0,
                conflict
                    .1
                    .iter()
                    .map(|index| projections[*index].page_id)
                    .collect::<Vec<_>>(),
            )));
        }
    }
    Ok(assigned
        .into_iter()
        .enumerate()
        .filter_map(|(index, choice)| {
            choice.map(|choice| {
                let projection = &projections[index];
                let (title, start) = &projection.candidates[choice];
                interval(title, *start, i64::MAX, projection.page_id)
            })
        })
        .collect())
}

fn interval(title: &str, start: i64, end: i64, page_id: u64) -> Interval {
    Interval {
        title: normalize(title),
        start,
        end,
        page_id,
    }
}

fn coded_title(title: &str, site: &SiteInfoRecord) -> u64 {
    let (namespace, title) = split_title(title, site);
    let mut symbols = namespace_varint(namespace);
    symbols.extend_from_slice(title.as_bytes());
    let bits = symbols
        .iter()
        .map(|byte| if common_symbol(*byte).is_some() { 7 } else { 10 })
        .sum::<usize>()
        + 2;
    if bits <= 63 {
        let mut output = 0_u64;
        let mut used = 0_usize;
        for byte in symbols {
            if let Some(symbol) = common_symbol(byte) {
                put_bits(&mut output, &mut used, u64::from(symbol), 7);
            } else {
                put_bits(&mut output, &mut used, 0b10, 2);
                put_bits(&mut output, &mut used, u64::from(byte), 8);
            }
        }
        put_bits(&mut output, &mut used, 0b11, 2);
        output
    } else {
        let hash = xxhash_rust::xxh3::xxh3_64_with_seed(title.as_bytes(), namespace as u64);
        (1_u64 << 63) | (hash & ((1_u64 << 63) - 1))
    }
}

fn put_bits(output: &mut u64, used: &mut usize, value: u64, width: usize) {
    *output |= value << (63 - *used - width);
    *used += width;
}

/// Recover the namespace prefix from a short title key.  Long keys are
/// intentionally hashed (the high bit is set), so their namespace cannot be
/// recovered and the caller must treat them as an unclassified title.
fn coded_namespace(key: u64) -> Option<i32> {
    if key >> 63 != 0 {
        return None;
    }
    let mut used = 0_usize;
    let mut value = 0_u64;
    let mut shift = 0_u32;
    for _ in 0..10 {
        let first = read_bits(key, &mut used, 1)?;
        let byte = if first == 0 {
            u8::try_from(read_bits(key, &mut used, 6)?).ok()?
        } else {
            let second = read_bits(key, &mut used, 1)?;
            if second != 0 {
                return None;
            }
            u8::try_from(read_bits(key, &mut used, 8)?).ok()?
        };
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            let signed = (value >> 1) as i64 ^ -((value & 1) as i64);
            return i32::try_from(signed).ok();
        }
        shift += 7;
    }
    None
}

fn read_bits(key: u64, used: &mut usize, width: usize) -> Option<u64> {
    if width == 0 || *used + width > 63 {
        return None;
    }
    let shift = 63 - *used - width;
    let mask = if width == 64 { u64::MAX } else { (1_u64 << width) - 1 };
    *used += width;
    Some((key >> shift) & mask)
}

fn common_symbol(byte: u8) -> Option<u8> {
    match byte {
        b' ' => Some(0),
        b'A'..=b'Z' => Some(byte - b'A' + 1),
        b'a'..=b'z' => Some(byte - b'a' + 27),
        b'0'..=b'9' => Some(byte - b'0' + 53),
        b'_' => Some(63),
        _ => None,
    }
}

fn namespace_varint(namespace: i64) -> Vec<u8> {
    let mut value = ((namespace << 1) ^ (namespace >> 63)) as u64;
    let mut output = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        output.push(byte | u8::from(value != 0) << 7);
        if value == 0 {
            return output;
        }
    }
}

fn split_title(title: &str, site: &SiteInfoRecord) -> (i64, String) {
    let title = normalize(title);
    if let Some((prefix, local)) = title.split_once(':') {
        let folded = prefix.to_lowercase();
        if let Some(namespace) = site.namespaces.iter().find(|namespace| {
            namespace.localized_name.to_lowercase() == folded
                || namespace
                    .aliases
                    .iter()
                    .any(|alias| alias.to_lowercase() == folded)
        }) {
            return (i64::from(namespace.id), local.to_owned());
        }
    }
    (0, title)
}

pub(crate) fn full_title(
    action: &crate::archive::PageActionRecord,
    site: &SiteInfoRecord,
) -> String {
    title_in_namespace(
        &action.title_at_event,
        action.namespace_at_event.unwrap_or(0),
        site,
    )
}

pub(crate) fn title_in_namespace(
    title: &str,
    namespace: i64,
    site: &SiteInfoRecord,
) -> String {
    let prefix = i32::try_from(namespace)
        .ok()
        .and_then(|id| site.namespaces.iter().find(|namespace| namespace.id == id))
        .map(|namespace| namespace.localized_name.as_str())
        .unwrap_or("");
    if prefix.is_empty() {
        title.to_owned()
    } else {
        format!("{prefix}:{title}")
    }
}

fn normalize(title: &str) -> String {
    title.replace('_', " ").trim().to_string()
}

fn seconds(timestamp_micros: i64) -> u32 {
    timestamp_micros
        .div_euclid(1_000_000)
        .clamp(0, i64::from(u32::MAX)) as u32
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
                    aliases: vec!["T".into()],
                },
            ],
            interwiki: Vec::new(),
            magic_words: Vec::new(),
        }
    }

    #[test]
    fn parallel_projection_keeps_the_newest_siteinfo() {
        let record = |timestamp_micros, site_name: &str| {
            let mut site_info = site();
            site_info.site_name = site_name.into();
            Record::SiteInfo {
                timestamp_micros,
                site_info,
            }
        };
        for records in [
            [record(20, "new"), record(10, "old")],
            [record(10, "old"), record(20, "new")],
        ] {
            let mut builder = TitleIndexBuilder::new();
            for record in records {
                builder.observe(&record);
            }
            assert_eq!(
                builder
                    .site_info
                    .as_ref()
                    .map(|(_, site_info)| site_info.site_name.as_str()),
                Some("new")
            );
        }
    }

    #[test]
    fn short_titles_are_prefix_coded_and_long_titles_are_hashed() {
        let site = site();
        assert_eq!(coded_title("Template:X", &site), coded_title("T:X", &site));
        assert_eq!(coded_title("Test", &site) >> 63, 0);
        assert_eq!(coded_namespace(coded_title("Test", &site)), Some(0));
        assert_eq!(coded_namespace(coded_title("Template:X", &site)), Some(10));
        assert_eq!(
            coded_title("This title is deliberately much longer than sixty three coded bits", &site)
                >> 63,
            1
        );
        assert_eq!(
            coded_namespace(coded_title(
                "This title is deliberately much longer than sixty three coded bits",
                &site,
            )),
            None
        );
    }
}
