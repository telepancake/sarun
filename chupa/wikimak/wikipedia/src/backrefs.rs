//! Generated relation indexes. Wikitext references use the newest revision of
//! every page; user-edit sets inspect contributor metadata from every revision.
//!
//! This is deliberately a sidecar, not an archive record: rebuilding it from
//! the same archive and title index produces identical bytes.  Direct sets are
//! stored independently.  Transitive sets use Git-style bounded XOR bases,
//! preferring graph-topology candidates before a small backward heuristic
//! window.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;

use memmap2::Mmap;
use crate::archive::{
    ArchiveError, ArchiveRecordReader, FrameMergeProjection, FrameMergeProjectionFactory,
    FrameRecordCursor, Record,
};
pub use crate::backrefs_parse::EdgeKind;
use crate::backrefs_parse::{
    extract_report_with_namespaces, Certainty, InclusionContext, NamespaceMap, RawEdge,
};

const MAGIC: [u8; 8] = *b"SWREFOBJ";
const PROJECTION_MAGIC: [u8; 8] = *b"SWPRJ002";
const PROJECTION_VERSION: u16 = 2;
const PROJECTION_HEADER_BYTES: usize = 24;
const PROJECTION_SET_HEADER_BYTES: usize = 24;
const HEADER_BYTES: usize = 80;
const CAPABILITY_USER_EDITS: u32 = 1 << 0;
const CAPABILITY_RAW_POSTINGS: u32 = 1 << 1;
const KNOWN_CAPABILITIES: u32 = CAPABILITY_USER_EDITS | CAPABILITY_RAW_POSTINGS;
const REQUIRED_CAPABILITIES: u32 = CAPABILITY_USER_EDITS;
const DIRECTORY_DESCRIPTOR_BYTES: usize = 40;
const PRESENCE_WORD_BYTES: usize = 20;
const MAX_XOR_OFFSET: usize = 160;
const MAX_XOR_DEPTH: u8 = 10;
const HEURISTIC_WINDOW: usize = 16;
const MAX_RECENT_KEYS: usize = 1024;
const RECENT_BITMAP_BUDGET: usize = 8 * 1024 * 1024;
const EDGE_BYTES: usize = 24;
const EDGE_RUN_RECORDS: usize =
    (48 * 1024 * 1024) / std::mem::size_of::<EdgeRecord>();
const EDGE_MERGE_FAN_IN: usize = 64;
const USER_PAGE_RUN_RECORDS: usize = (48 * 1024 * 1024) / 16;

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
static BACKREF_BUILD_FRAME_SCANS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static BACKREF_REWRITE_LOGICAL_SET_DECODES: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static BACKREF_REWRITE_DERIVED_SET_DECODES: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static BACKREF_BUILD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
static BACKREF_REWRITE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourcePage {
    pub page_id: u64,
    pub title: String,
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BuildStats {
    pub source_pages: u64,
    pub extracted_static_edges: u64,
    pub unresolved_static_edges: u64,
    pub unresolved_dynamic_targets: u64,
    pub redirect_pages: u64,
    pub users_with_edits: u64,
    pub user_page_memberships: u64,
    pub sets: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SetClass {
    DirectUnconditional,
    DirectPossible,
    TransitiveUnconditional,
    TransitivePossible,
    RawNonTopologyUnconditional,
    RawNonTopologyPossible,
    RawTopologyUnconditional,
    RawTopologyPossible,
    RawEmittedUnconditional,
    RawEmittedPossible,
    RawRedirectTarget,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SetKey {
    pub target_page_id: u64,
    pub kind: EdgeKind,
    pub class: SetClass,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LogicalSet {
    key: SetKey,
    members: Bitmap,
    topology_bases: Vec<SetKey>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Bitmap {
    /// Sorted non-zero `(word index, word)` pairs.  Page ids are the bit
    /// positions; a sparse high id therefore costs one pair, not id/8 bytes.
    words: Vec<(u64, u64)>,
}

impl Bitmap {
    fn insert(&mut self, bit: u64) {
        let word = bit / 64;
        match self.words.binary_search_by_key(&word, |entry| entry.0) {
            Ok(position) => self.words[position].1 |= 1_u64 << (bit % 64),
            Err(position) => {
                self.words.insert(position, (word, 1_u64 << (bit % 64)));
            }
        }
    }

    fn union_with(&mut self, other: &Self) {
        self.words = merge_words(&self.words, &other.words, |left, right| left | right);
    }

    fn remove(&mut self, bit: u64) {
        let word = bit / 64;
        if let Ok(position) = self.words.binary_search_by_key(&word, |entry| entry.0) {
            self.words[position].1 &= !(1_u64 << (bit % 64));
            if self.words[position].1 == 0 {
                self.words.remove(position);
            }
        }
    }

    fn difference(&self, base: &Self) -> Self {
        Self {
            words: merge_words(&self.words, &base.words, |left, right| left ^ right),
        }
    }

    fn subtract(&mut self, other: &Self) {
        self.words = merge_words(&self.words, &other.words, |left, right| left & !right);
    }

    #[cfg(test)]
    fn len(&self) -> u64 {
        self.words
            .iter()
            .map(|(_, word)| u64::from(word.count_ones()))
            .sum()
    }

    fn members(&self) -> impl Iterator<Item = u64> + '_ {
        self.words.iter().flat_map(|(word_index, word)| {
            let word_index = *word_index;
            let mut remaining = *word;
            std::iter::from_fn(move || {
                if remaining == 0 {
                    return None;
                }
                let bit = remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                Some(word_index * 64 + bit as u64)
            })
        })
    }
}

fn merge_words(
    left: &[(u64, u64)],
    right: &[(u64, u64)],
    combine: impl Fn(u64, u64) -> u64,
) -> Vec<(u64, u64)> {
    let mut output = Vec::with_capacity(left.len().saturating_add(right.len()));
    let (mut l, mut r) = (0, 0);
    while l < left.len() || r < right.len() {
        let index = match (left.get(l), right.get(r)) {
            (Some(left), Some(right)) => left.0.min(right.0),
            (Some(left), None) => left.0,
            (None, Some(right)) => right.0,
            (None, None) => break,
        };
        let left_word = left
            .get(l)
            .filter(|entry| entry.0 == index)
            .map_or(0, |entry| {
                l += 1;
                entry.1
            });
        let right_word = right
            .get(r)
            .filter(|entry| entry.0 == index)
            .map_or(0, |entry| {
                r += 1;
                entry.1
            });
        let word = combine(left_word, right_word);
        if word != 0 {
            output.push((index, word));
        }
    }
    output
}

#[derive(Clone, Copy)]
struct EncodedMeta {
    base_offset: u8,
    payload_offset: u64,
}

struct RecentSet {
    position: usize,
    depth: u8,
    members: Bitmap,
    bytes: usize,
}

struct PendingLogicalDirectory {
    kind: EdgeKind,
    class: SetClass,
    entries: Vec<(u64, u32)>,
}

struct DedupTable {
    hashes: Vec<u64>,
    object_ids: Vec<u32>,
    collisions: Vec<(u64, u32)>,
    len: usize,
}

impl DedupTable {
    fn new() -> Self {
        Self {
            hashes: vec![0; 16],
            object_ids: vec![u32::MAX; 16],
            collisions: Vec::new(),
            len: 0,
        }
    }

    fn ensure_capacity(&mut self) {
        if (self.len + 1) * 10 < self.object_ids.len() * 7 {
            return;
        }
        let new_capacity = self.hashes.len() * 2;
        let old_hashes = std::mem::replace(&mut self.hashes, vec![0; new_capacity]);
        let old_ids = std::mem::replace(&mut self.object_ids, vec![u32::MAX; new_capacity]);
        self.len = 0;
        for (hash, object_id) in old_hashes.into_iter().zip(old_ids) {
            if object_id != u32::MAX {
                self.insert_primary(hash, object_id);
            }
        }
    }

    fn primary_and_vacant(&self, hash: u64) -> (Option<u32>, usize) {
        let mask = self.object_ids.len() - 1;
        let mut position = hash as usize & mask;
        loop {
            let object_id = self.object_ids[position];
            if object_id == u32::MAX {
                return (None, position);
            }
            if self.hashes[position] == hash {
                return (Some(object_id), position);
            }
            position = (position + 1) & mask;
        }
    }

    fn insert_at(&mut self, position: usize, hash: u64, object_id: u32) {
        debug_assert_eq!(self.object_ids[position], u32::MAX);
        self.hashes[position] = hash;
        self.object_ids[position] = object_id;
        self.len += 1;
    }

    fn insert_primary(&mut self, hash: u64, object_id: u32) {
        let (existing, position) = self.primary_and_vacant(hash);
        debug_assert!(existing.is_none());
        self.insert_at(position, hash, object_id);
    }
}

struct StreamingEncoder {
    capabilities: u32,
    payload: std::fs::File,
    payload_len: u64,
    entries: Vec<EncodedMeta>,
    logical: Vec<PendingLogicalDirectory>,
    logical_count: usize,
    canonical: std::fs::File,
    canonical_len: u64,
    canonical_offsets: Vec<(u64, u64)>,
    dedup: DedupTable,
    recent: VecDeque<RecentSet>,
    recent_keys: VecDeque<(SetKey, u32)>,
    recent_bytes: usize,
    #[cfg(test)]
    forced_hash: Option<u64>,
}

impl StreamingEncoder {
    #[cfg(test)]
    fn new() -> std::io::Result<Self> {
        Self::new_with_capabilities(REQUIRED_CAPABILITIES)
    }

    fn new_in(root: &Path) -> std::io::Result<Self> {
        Self::new_in_with_capabilities(root, REQUIRED_CAPABILITIES)
    }

    fn new_with_capabilities(capabilities: u32) -> std::io::Result<Self> {
        Self::new_with_files(
            tempfile::tempfile()?,
            tempfile::tempfile()?,
            capabilities,
        )
    }

    fn new_in_with_capabilities(root: &Path, capabilities: u32) -> std::io::Result<Self> {
        Self::new_with_files(
            tempfile::tempfile_in(root)?,
            tempfile::tempfile_in(root)?,
            capabilities,
        )
    }

    fn new_with_files(
        payload: std::fs::File,
        canonical: std::fs::File,
        capabilities: u32,
    ) -> std::io::Result<Self> {
        if capabilities & REQUIRED_CAPABILITIES != REQUIRED_CAPABILITIES
            || capabilities & !KNOWN_CAPABILITIES != 0
        {
            return Err(invalid_data("invalid backref sidecar capabilities"));
        }
        Ok(Self {
            capabilities,
            payload,
            payload_len: 0,
            entries: Vec::new(),
            logical: Vec::new(),
            logical_count: 0,
            canonical,
            canonical_len: 0,
            canonical_offsets: Vec::new(),
            dedup: DedupTable::new(),
            recent: VecDeque::new(),
            recent_keys: VecDeque::new(),
            recent_bytes: 0,
            #[cfg(test)]
            forced_hash: None,
        })
    }

    fn add(&mut self, set: LogicalSet) -> std::io::Result<()> {
        if !valid_kind_class(set.key.kind, set.key.class) {
            return Err(invalid_data("invalid backref kind/class combination"));
        }
        if is_raw_class(set.key.class) && self.capabilities & CAPABILITY_RAW_POSTINGS == 0 {
            return Err(invalid_data("raw backref set without raw-posting capability"));
        }
        if set.members.words.is_empty() {
            return Ok(());
        }
        let raw = encode_sidecar_bitmap(&set.members);
        let hash = xxhash_rust::xxh3::xxh3_64(&raw);
        #[cfg(test)]
        let hash = self.forced_hash.unwrap_or(hash);
        self.dedup.ensure_capacity();
        let (primary, vacant) = self.dedup.primary_and_vacant(hash);
        if let Some(object_id) = primary {
            if self.canonical_equals(object_id, &raw)? {
                self.record_logical(set.key, object_id);
                self.record_recent_key(set.key, object_id);
                return Ok(());
            }
            for position in 0..self.dedup.collisions.len() {
                let (collision_hash, collision_id) = self.dedup.collisions[position];
                if collision_hash == hash && self.canonical_equals(collision_id, &raw)? {
                    self.record_logical(set.key, collision_id);
                    self.record_recent_key(set.key, collision_id);
                    return Ok(());
                }
            }
        }
        let raw_len = raw.len() as u64;
        let canonical_offset = self.canonical_len;
        self.canonical.seek(SeekFrom::Start(canonical_offset))?;
        self.canonical.write_all(&raw)?;
        let direct = set.key.kind != EdgeKind::UserEdits
            && matches!(
                set.key.class,
                SetClass::DirectUnconditional
                    | SetClass::DirectPossible
                    | SetClass::RawNonTopologyUnconditional
                    | SetClass::RawNonTopologyPossible
                    | SetClass::RawTopologyUnconditional
                    | SetClass::RawTopologyPossible
                    | SetClass::RawEmittedUnconditional
                    | SetClass::RawEmittedPossible
                    | SetClass::RawRedirectTarget
            );
        self.evict_stale_recent();
        let mut best = None::<(&RecentSet, Vec<u8>)>;
        if !direct {
            for key in &set.topology_bases {
                if let Some((_, object_id)) =
                    self.recent_keys.iter().rev().find(|entry| entry.0 == *key)
                {
                    if let Some(candidate) = self
                        .recent
                        .iter()
                        .find(|entry| entry.position == *object_id as usize)
                    {
                        consider_recent(&set.members, candidate, &raw, &mut best);
                    }
                }
            }
            for candidate in self.recent.iter().rev().take(HEURISTIC_WINDOW) {
                consider_recent(&set.members, candidate, &raw, &mut best);
            }
        }
        let (base_offset, depth, payload) = match best {
            Some((base, payload)) => {
                let distance = self.entries.len() - base.position;
                let distance = u8::try_from(distance)
                    .map_err(|_| invalid_data("backref XOR base distance overflow"))?;
                (distance, base.depth + 1, payload)
            }
            None => (0, 0, raw),
        };
        let object_id = u32::try_from(self.entries.len())
            .map_err(|_| invalid_data("too many backref bitmap objects"))?;
        self.payload.write_all(&payload)?;
        self.entries.push(EncodedMeta {
            base_offset,
            payload_offset: self.payload_len,
        });
        self.payload_len += payload.len() as u64;
        self.canonical_offsets
            .push((canonical_offset, raw_len));
        self.canonical_len += raw_len;
        if primary.is_some() {
            self.dedup.collisions.push((hash, object_id));
        } else {
            self.dedup.insert_at(vacant, hash, object_id);
        }
        self.record_logical(set.key, object_id);
        self.record_recent_key(set.key, object_id);
        let bitmap_bytes =
            set.members.words.capacity() * std::mem::size_of::<(u64, u64)>();
        if !direct && bitmap_bytes <= RECENT_BITMAP_BUDGET {
            self.recent.push_back(RecentSet {
                position: self.entries.len() - 1,
                depth,
                members: set.members,
                bytes: bitmap_bytes,
            });
            self.recent_bytes += bitmap_bytes;
        }
        while self.recent.len() > MAX_XOR_OFFSET
            || self.recent_bytes > RECENT_BITMAP_BUDGET
        {
            if let Some(evicted) = self.recent.pop_front() {
                self.recent_bytes -= evicted.bytes;
            }
        }
        Ok(())
    }

    fn canonical_equals(&mut self, object_id: u32, raw: &[u8]) -> std::io::Result<bool> {
        let (offset, length) = self.canonical_offsets[object_id as usize];
        if length != raw.len() as u64 {
            return Ok(false);
        }
        let mut candidate = vec![0; raw.len()];
        self.canonical.seek(SeekFrom::Start(offset))?;
        self.canonical.read_exact(&mut candidate)?;
        Ok(candidate == raw)
    }

    fn record_logical(&mut self, key: SetKey, object_id: u32) {
        let position = self
            .logical
            .iter()
            .position(|directory| directory.kind == key.kind && directory.class == key.class)
            .unwrap_or_else(|| {
                self.logical.push(PendingLogicalDirectory {
                    kind: key.kind,
                    class: key.class,
                    entries: Vec::new(),
                });
                self.logical.len() - 1
            });
        self.logical[position]
            .entries
            .push((key.target_page_id, object_id));
        self.logical_count += 1;
    }

    #[cfg(test)]
    fn logical_object(&self, key: SetKey) -> Option<u32> {
        self.logical
            .iter()
            .find(|directory| directory.kind == key.kind && directory.class == key.class)
            .and_then(|directory| {
                directory
                    .entries
                    .iter()
                    .find(|entry| entry.0 == key.target_page_id)
                    .map(|entry| entry.1)
            })
    }

    fn record_recent_key(&mut self, key: SetKey, object_id: u32) {
        self.recent_keys.push_back((key, object_id));
        while self.recent_keys.len() > MAX_RECENT_KEYS {
            self.recent_keys.pop_front();
        }
    }

    fn evict_stale_recent(&mut self) {
        let current = self.entries.len();
        while self
            .recent
            .front()
            .is_some_and(|entry| current - entry.position > MAX_XOR_OFFSET)
        {
            if let Some(evicted) = self.recent.pop_front() {
                self.recent_bytes -= evicted.bytes;
            }
        }
        while self
            .recent_keys
            .front()
            .is_some_and(|(_, object_id)| current - *object_id as usize > MAX_XOR_OFFSET)
        {
            self.recent_keys.pop_front();
        }
    }

    fn write_staged(
        mut self,
        parent: &Path,
        title_index_fingerprint: u64,
    ) -> crate::archive::Result<tempfile::NamedTempFile> {
        if self.logical_count > u32::MAX as usize {
            return Err(ArchiveError::Invalid("too many backref logical sets"));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        let directories = build_logical_directories(std::mem::take(&mut self.logical))?;
        let logical_bytes = directories.iter().try_fold(
            (directories.len() * DIRECTORY_DESCRIPTOR_BYTES) as u64,
            |total, directory| {
                total
                    .checked_add(directory.words.len() as u64 * PRESENCE_WORD_BYTES as u64)
                    .and_then(|total| {
                        total.checked_add(directory.object_ids.len() as u64 * 4)
                    })
                    .ok_or(ArchiveError::FieldTooLarge)
            },
        )?;
        let object_offsets_offset = HEADER_BYTES as u64 + logical_bytes;
        let base_offsets_offset =
            object_offsets_offset + (self.entries.len() as u64 + 1) * 8;
        let payload_offset = base_offsets_offset + self.entries.len() as u64;
        temporary.write_all(&MAGIC)?;
        temporary.write_all(&(HEADER_BYTES as u32).to_le_bytes())?;
        temporary.write_all(&self.capabilities.to_le_bytes())?;
        temporary.write_all(&(self.entries.len() as u64).to_le_bytes())?;
        temporary.write_all(&(self.logical_count as u64).to_le_bytes())?;
        temporary.write_all(&(HEADER_BYTES as u64).to_le_bytes())?;
        temporary.write_all(&object_offsets_offset.to_le_bytes())?;
        temporary.write_all(&base_offsets_offset.to_le_bytes())?;
        temporary.write_all(&payload_offset.to_le_bytes())?;
        temporary.write_all(&(directories.len() as u64).to_le_bytes())?;
        temporary.write_all(&title_index_fingerprint.to_le_bytes())?;
        write_logical_directories(temporary.as_file_mut(), &directories)?;
        for entry in &self.entries {
            temporary.write_all(&(payload_offset + entry.payload_offset).to_le_bytes())?;
        }
        temporary.write_all(&(payload_offset + self.payload_len).to_le_bytes())?;
        for entry in &self.entries {
            temporary.write_all(&[entry.base_offset])?;
        }
        self.payload.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut self.payload, temporary.as_file_mut())?;
        temporary.as_file_mut().sync_all()?;
        Ok(temporary)
    }

    fn write(
        self,
        output: impl AsRef<Path>,
        title_index_fingerprint: u64,
    ) -> crate::archive::Result<()> {
        let output = output.as_ref();
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let temporary = self.write_staged(parent, title_index_fingerprint)?;
        temporary
            .persist(output)
            .map_err(|error| ArchiveError::Io(error.error))?;
        set_readable_permissions(output)?;
        Ok(())
    }
}

fn consider_recent<'a>(
    members: &Bitmap,
    candidate: &'a RecentSet,
    raw: &[u8],
    best: &mut Option<(&'a RecentSet, Vec<u8>)>,
) {
    if candidate.depth >= MAX_XOR_DEPTH {
        return;
    }
    let delta = encode_sidecar_bitmap(&members.difference(&candidate.members));
    let current_len = best.as_ref().map_or(raw.len(), |(_, bytes)| bytes.len());
    if delta.len() < current_len {
        *best = Some((candidate, delta));
    }
}

#[derive(Default)]
struct LogicalDirectory {
    kind: Option<EdgeKind>,
    class: Option<SetClass>,
    words: Vec<(u64, u64, u32)>,
    object_ids: Vec<u32>,
}

fn build_logical_directories(
    mut logical: Vec<PendingLogicalDirectory>,
) -> std::io::Result<Vec<LogicalDirectory>> {
    logical.sort_unstable_by_key(|directory| (directory.kind, directory.class));
    let mut directories = Vec::new();
    for pending in &mut logical {
        if pending.entries.is_empty() {
            continue;
        }
        pending.entries.sort_unstable();
        let mut directory = LogicalDirectory {
            kind: Some(pending.kind),
            class: Some(pending.class),
            ..LogicalDirectory::default()
        };
        for (entry_index, (target_page_id, object_id)) in pending.entries.iter().enumerate() {
            if entry_index != 0 && pending.entries[entry_index - 1].0 == *target_page_id {
                return Err(invalid_data("duplicate backref logical key"));
            }
            let word_index = target_page_id / 64;
            let bit = 1_u64 << (target_page_id % 64);
            match directory.words.last_mut() {
                Some((last_index, word, _)) if *last_index == word_index => *word |= bit,
                _ => {
                    let rank = u32::try_from(directory.object_ids.len())
                        .map_err(|_| invalid_data("too many backref logical sets"))?;
                    directory.words.push((word_index, bit, rank));
                }
            }
            directory.object_ids.push(*object_id);
        }
        directories.push(directory);
    }
    Ok(directories)
}

fn write_logical_directories(
    output: &mut (impl Write + Seek),
    directories: &[LogicalDirectory],
) -> std::io::Result<()> {
    let mut cursor = (HEADER_BYTES + directories.len() * DIRECTORY_DESCRIPTOR_BYTES) as u64;
    for directory in directories {
        let words_offset = cursor;
        cursor += directory.words.len() as u64 * PRESENCE_WORD_BYTES as u64;
        let ids_offset = cursor;
        cursor += directory.object_ids.len() as u64 * 4;
        output.write_all(&[
            kind_byte(directory.kind.expect("logical directory kind")),
            class_byte(directory.class.expect("logical directory class")),
        ])?;
        output.write_all(&[0; 6])?;
        output.write_all(&words_offset.to_le_bytes())?;
        output.write_all(&(directory.words.len() as u64).to_le_bytes())?;
        output.write_all(&ids_offset.to_le_bytes())?;
        output.write_all(&(directory.object_ids.len() as u64).to_le_bytes())?;
    }
    for directory in directories {
        for (word_index, word, rank) in &directory.words {
            output.write_all(&word_index.to_le_bytes())?;
            output.write_all(&word.to_le_bytes())?;
            output.write_all(&rank.to_le_bytes())?;
        }
        for object_id in &directory.object_ids {
            output.write_all(&object_id.to_le_bytes())?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
struct PageData {
    title: String,
    namespace: Option<i64>,
    deleted: bool,
    text: Option<String>,
    contributors: BTreeSet<u64>,
    page_state_at: Option<i64>,
    actions: Vec<(i64, crate::archive::PageActionRecord)>,
}

/// Accumulates one archive-ordered page at a time.  The archive contract puts
/// the newest state and revision first within a page. The first revision
/// supplies current text, every revision contributes to the user-edit set,
/// and only actions later than the newest state advance title/existence.
/// Keeping this boundary handling shared is important: frame projection and
/// the bootstrap scan must select pages identically.
#[derive(Default)]
struct PageAccumulator {
    current_id: Option<u64>,
    current: PageData,
    saw_revision: bool,
}

impl PageAccumulator {
    fn flush_current(
        &mut self,
        page_id: u64,
        flush: &mut impl FnMut(u64, &mut PageData) -> crate::archive::Result<()>,
    ) -> crate::archive::Result<()> {
        apply_page_action_transitions(&mut self.current);
        flush(page_id, &mut self.current)
    }

    fn observe(
        &mut self,
        record: &Record,
        flush: &mut impl FnMut(u64, &mut PageData) -> crate::archive::Result<()>,
    ) -> crate::archive::Result<()> {
        let Some(page_id) = record.page_id() else {
            return Ok(());
        };
        if self.current_id != Some(page_id) {
            if let Some(previous) = self.current_id {
                if page_id <= previous {
                    return Err(ArchiveError::Invalid(
                        "archive page IDs are not strictly grouped and increasing",
                    ));
                }
                self.flush_current(previous, flush)?;
            }
            self.current_id = Some(page_id);
            self.current = PageData::default();
            self.saw_revision = false;
        }
        match record {
            Record::PageState {
                timestamp_micros,
                title,
                namespace,
                deleted,
                ..
            } if self.current.page_state_at.is_none() => {
                self.current.page_state_at = Some(*timestamp_micros);
                self.current.title = title.clone();
                self.current.namespace = *namespace;
                self.current.deleted = *deleted;
            }
            Record::PageAction {
                timestamp_micros,
                action,
                ..
            } => self.current.actions.push((*timestamp_micros, action.clone())),
            Record::Revision { revision, .. } => {
                if let crate::ContributorMeta::Named { user_id, .. } = &revision.meta.contributor
                {
                    if *user_id != 0 {
                        self.current.contributors.insert(*user_id);
                    }
                }
                if !self.saw_revision {
                    self.saw_revision = true;
                    self.current.text = revision
                        .has_text
                        .then(|| String::from_utf8_lossy(&revision.text).into_owned());
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(
        &mut self,
        flush: &mut impl FnMut(u64, &mut PageData) -> crate::archive::Result<()>,
    ) -> crate::archive::Result<()> {
        if let Some(page_id) = self.current_id.take() {
            self.flush_current(page_id, flush)?;
        }
        Ok(())
    }
}

fn apply_page_action_transitions(page: &mut PageData) {
    if page.page_state_at.is_none() {
        page.deleted = true;
    }
    page.actions
        .sort_by_key(|(timestamp, action)| (*timestamp, action.tie_sequence));
    let state_at = page.page_state_at;
    for (_, action) in std::mem::take(&mut page.actions)
        .into_iter()
        .filter(|(timestamp, _)| state_at.is_none_or(|state_at| *timestamp > state_at))
    {
        match &action.kind {
            crate::archive::PageActionKind::Create
            | crate::archive::PageActionKind::LoggedCreate
            | crate::archive::PageActionKind::Move
            | crate::archive::PageActionKind::Restore => {
                page.title = action.title_at_event;
                page.namespace = action.namespace_at_event;
                page.deleted = false;
            }
            crate::archive::PageActionKind::Delete
                if action.resulting_deleted != Some(false) =>
            {
                page.deleted = true;
            }
            _ => {
                page.title = action.title_at_event;
                page.namespace = action.namespace_at_event;
                if let Some(deleted) = action.resulting_deleted {
                    page.deleted = deleted;
                }
            }
        }
    }
}

#[derive(Default)]
struct RedirectFrameResult {
    entries: Vec<(u64, Option<u64>)>,
}

#[derive(Default)]
struct RelationFrameResult {
    source_pages: u64,
    extracted_static_edges: u64,
    unresolved_static_edges: u64,
    unresolved_dynamic_targets: u64,
    edges: Vec<EdgeRecord>,
}

#[derive(Default)]
struct RawFrameResult {
    raw: Vec<RawEdgeRecord>,
    user_pages: Vec<(u64, u64)>,
    source_pages: u64,
    extracted_static_edges: u64,
    unresolved_dynamic_targets: u64,
}

#[derive(Default)]
struct RawPageStats {
    source_pages: u64,
    extracted_static_edges: u64,
    unresolved_dynamic_targets: u64,
}

#[derive(Clone, Debug)]
struct RedirectWord {
    alias: String,
    case_sensitive: bool,
}

#[derive(Default)]
struct RedirectTable {
    /// Page-id ordered `(redirect page, resolved target)` entries. `None`
    /// marks a syntactically valid redirect whose target title is absent.
    entries: Vec<(u64, Option<u64>)>,
}

impl RedirectTable {
    fn push(&mut self, page_id: u64, target: Option<u64>) {
        debug_assert!(self.entries.last().is_none_or(|entry| entry.0 < page_id));
        self.entries.push((page_id, target));
    }

    fn lookup(&self, page_id: u64) -> Option<Option<u64>> {
        self.entries
            .binary_search_by_key(&page_id, |entry| entry.0)
            .ok()
            .map(|position| self.entries[position].1)
    }

    fn contains(&self, page_id: u64) -> bool {
        self.lookup(page_id).is_some()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EdgeRecord {
    kind: EdgeKind,
    target: u64,
    certainty: Certainty,
    emitted: bool,
    topology: bool,
    source: u64,
}

/// A raw posting retains the encoded target identity instead of resolving it
/// to the current page owner.  The class carries both the parser certainty
/// and the relation's emitted/topology distinction; this makes the raw
/// representation independent of the current title index.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RawEdgeRecord {
    kind: EdgeKind,
    class: SetClass,
    target: u64,
    source: u64,
}

type RawLogicalKey = (EdgeKind, SetClass, u64);

impl RawEdgeRecord {
    fn logical_key(self) -> RawLogicalKey {
        (self.kind, self.class, self.target)
    }
}

impl Ord for EdgeRecord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.kind,
            self.target,
            self.emitted,
            self.source,
            self.topology,
            self.certainty,
        )
            .cmp(&(
                other.kind,
                other.target,
                other.emitted,
                other.source,
                other.topology,
                other.certainty,
            ))
    }
}

impl PartialOrd for EdgeRecord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug)]
struct ParsedPageEdge {
    raw: RawEdge,
    emitted: bool,
    topology: bool,
}

#[cfg(test)]
type DirectSets = BTreeMap<(EdgeKind, u64, Certainty), Bitmap>;
struct CollectedEdges {
    direct: DiskSets,
    topology_seeds: DiskSets,
    graph: DiskGraph,
    effects: DiskGraph,
    redirect_misses: u64,
}

struct DiskSets {
    file: std::fs::File,
    positions: Vec<((EdgeKind, u64, Certainty), (u64, u64))>,
    len: u64,
}

impl DiskSets {
    #[cfg(test)]
    fn new() -> std::io::Result<Self> {
        Self::new_with_file(tempfile::tempfile()?)
    }

    fn new_in(root: &Path) -> std::io::Result<Self> {
        Self::new_with_file(tempfile::tempfile_in(root)?)
    }

    fn new_with_file(file: std::fs::File) -> std::io::Result<Self> {
        Ok(Self {
            file,
            positions: Vec::new(),
            len: 0,
        })
    }

    #[cfg(test)]
    fn from_memory(sets: DirectSets) -> std::io::Result<Self> {
        let mut store = Self::new()?;
        for (key, bitmap) in sets {
            store.put(key, &bitmap)?;
        }
        Ok(store)
    }

    fn put(
        &mut self,
        key: (EdgeKind, u64, Certainty),
        bitmap: &Bitmap,
    ) -> std::io::Result<()> {
        if bitmap.words.is_empty() {
            return Ok(());
        }
        let bytes = encode_bitmap(bitmap);
        self.file.seek(SeekFrom::Start(self.len))?;
        self.file.write_all(&bytes)?;
        debug_assert!(self
            .positions
            .last()
            .is_none_or(|(previous, _)| *previous < key));
        self.positions.push((key, (self.len, bytes.len() as u64)));
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn get(
        &mut self,
        key: &(EdgeKind, u64, Certainty),
    ) -> std::io::Result<Option<Bitmap>> {
        let Ok(position) = self
            .positions
            .binary_search_by_key(key, |(candidate, _)| *candidate)
        else {
            return Ok(None);
        };
        let (offset, len) = self.positions[position].1;
        self.file.seek(SeekFrom::Start(offset))?;
        let len = usize::try_from(len).map_err(|_| invalid_data("stored set is too large"))?;
        let mut bytes = vec![0; len];
        self.file.read_exact(&mut bytes)?;
        decode_bitmap(&bytes).map(Some)
    }
}

struct RawDiskSets {
    file: std::fs::File,
    positions: Vec<(RawLogicalKey, (u64, u64))>,
    len: u64,
}

impl RawDiskSets {
    fn new_in(root: &Path) -> std::io::Result<Self> {
        Self::new_with_file(tempfile::tempfile_in(root)?)
    }

    fn new_with_file(file: std::fs::File) -> std::io::Result<Self> {
        Ok(Self {
            file,
            positions: Vec::new(),
            len: 0,
        })
    }

    fn put(
        &mut self,
        key: RawLogicalKey,
        bitmap: &Bitmap,
    ) -> std::io::Result<()> {
        if bitmap.words.is_empty() {
            return Ok(());
        }
        if !is_raw_class(key.1) || !valid_kind_class(key.0, key.1) {
            return Err(invalid_data("non-raw class in raw set store"));
        }
        if self
            .positions
            .last()
            .is_some_and(|(previous, _)| *previous >= key)
        {
            return Err(invalid_data("raw set keys are not strictly ordered"));
        }
        let bytes = encode_bitmap(bitmap);
        self.file.seek(SeekFrom::Start(self.len))?;
        self.file.write_all(&bytes)?;
        self.positions.push((key, (self.len, bytes.len() as u64)));
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn get(
        &mut self,
        key: &RawLogicalKey,
    ) -> std::io::Result<Option<Bitmap>> {
        let Ok(position) = self
            .positions
            .binary_search_by_key(key, |(candidate, _)| *candidate)
        else {
            return Ok(None);
        };
        let (offset, len) = self.positions[position].1;
        self.file.seek(SeekFrom::Start(offset))?;
        let len = usize::try_from(len).map_err(|_| invalid_data("stored raw set is too large"))?;
        let mut bytes = vec![0; len];
        self.file.read_exact(&mut bytes)?;
        decode_bitmap(&bytes).map(Some)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GraphEdge {
    source: u64,
    target: u64,
    kind: EdgeKind,
    certainty: Certainty,
}

struct DiskGraph {
    forward: Option<Mmap>,
    reverse: Option<Mmap>,
}

struct PageRelations {
    edges: Vec<ParsedPageEdge>,
    dynamic_targets: u64,
}

struct TargetAccumulator {
    key: (EdgeKind, u64, bool),
    definite: Bitmap,
    possible: Bitmap,
    topology_definite: Bitmap,
    topology_possible: Bitmap,
    emissions: Vec<(u64, Certainty)>,
}

impl TargetAccumulator {
    fn new(key: (EdgeKind, u64, bool)) -> Self {
        Self {
            key,
            definite: Bitmap::default(),
            possible: Bitmap::default(),
            topology_definite: Bitmap::default(),
            topology_possible: Bitmap::default(),
            emissions: Vec::new(),
        }
    }
}

/// Build a deterministic sidecar from an archive and its generated title
/// index. Wikitext relations inspect only the newest revision of each page;
/// user-edit membership inspects every revision contributor.
pub fn build(
    archive: impl AsRef<Path>,
    title_index: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> crate::archive::Result<BuildStats> {
    let workers = usize::try_from(crate::archive::streaming_compression_workers())
        .unwrap_or(usize::MAX)
        .max(1);
    build_with_workers(archive, title_index, output, workers)
}

fn build_with_workers(
    archive: impl AsRef<Path>,
    title_index: impl AsRef<Path>,
    output: impl AsRef<Path>,
    workers: usize,
) -> crate::archive::Result<BuildStats> {
    #[cfg(test)]
    let _test_build_guard = BACKREF_BUILD_TEST_LOCK
        .lock()
        .expect("backref build test lock poisoned");
    build_with_workers_inner(archive, title_index, output, workers)
}

fn build_with_workers_inner(
    archive: impl AsRef<Path>,
    title_index: impl AsRef<Path>,
    output: impl AsRef<Path>,
    workers: usize,
) -> crate::archive::Result<BuildStats> {
    let archive = archive.as_ref();
    let title_index = title_index.as_ref();
    let output = output.as_ref();
    let output_parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let scratch = tempfile::TempDir::new_in(output_parent)?;
    let scratch = scratch.path();
    let title_index_fingerprint = file_xxh3_64(title_index)?;
    let titles = crate::title_index::TitleIndex::open(title_index)?;
    let site_info = read_site_info(archive, &titles)?;
    let namespaces = namespace_map(&site_info);
    let redirect_words = redirect_words(&site_info);
    let mut stats = BuildStats::default();
    let mut raw_spool = tempfile::tempfile_in(scratch)?;
    let mut user_spool = tempfile::tempfile_in(scratch)?;

    // The frame callback is the only semantic archive scan.  It retains title
    // identities in their coded form; resolving them before the scan would
    // make title moves and aliases impossible to update from the sidecar.
    crate::archive::process_frames_parallel(
        archive,
        workers,
        |_, _, frame| {
            #[cfg(test)]
            BACKREF_BUILD_FRAME_SCANS.fetch_add(1, Ordering::SeqCst);
            scan_frame_for_raw(frame, &site_info, &namespaces, &redirect_words)
        },
        |_, result| {
            stats.source_pages += result.source_pages;
            stats.extracted_static_edges += result.extracted_static_edges;
            stats.unresolved_dynamic_targets += result.unresolved_dynamic_targets;
            for edge in result.raw {
                write_raw_edge(&mut raw_spool, edge)?;
            }
            for pair in result.user_pages {
                write_user_page(&mut user_spool, pair.0, pair.1)?;
            }
            Ok(())
        },
    )?;
    raw_spool.sync_all()?;
    raw_spool.seek(SeekFrom::Start(0))?;
    user_spool.sync_all()?;
    user_spool.seek(SeekFrom::Start(0))?;
    let raw_sets = collect_sorted_raw_edges_in(raw_spool, scratch)?;
    let (collected, redirects, raw_missing_targets, raw_sets) =
        derive_public_edges_from_raw(raw_sets, &titles, scratch)?;
    stats.redirect_pages = redirects.len() as u64;
    stats.unresolved_static_edges += raw_missing_targets + collected.redirect_misses;
    let user_edits = collect_user_edit_pages_from_spool_in(user_spool, scratch)?;
    let (sets, users, memberships) = write_streaming_sidecar_in(
        output,
        collected.direct,
        collected.topology_seeds,
        collected.graph,
        collected.effects,
        user_edits,
        Some(raw_sets),
        title_index_fingerprint,
        scratch,
    )?;
    stats.sets = sets;
    stats.users_with_edits = users;
    stats.user_page_memberships = memberships;
    Ok(stats)
}

fn write_edge(output: &mut impl Write, edge: EdgeRecord) -> std::io::Result<()> {
    if edge.kind == EdgeKind::Redirect {
        return Err(invalid_data("redirect relation in public edge spool"));
    }
    output.write_all(&[kind_byte(edge.kind), certainty_byte(edge.certainty)])?;
    output.write_all(&[u8::from(edge.emitted)])?;
    output.write_all(&[u8::from(edge.topology)])?;
    output.write_all(&[0; 4])?;
    output.write_all(&edge.target.to_le_bytes())?;
    output.write_all(&edge.source.to_le_bytes())
}

fn scan_frame_for_raw(
    frame: &mut FrameRecordCursor,
    site_info: &crate::archive::SiteInfoRecord,
    namespaces: &NamespaceMap,
    redirect_words: &[RedirectWord],
) -> crate::archive::Result<RawFrameResult> {
    let mut result = RawFrameResult::default();
    let mut pages = PageAccumulator::default();
    while let Some(record) = frame.next_record()? {
        pages.observe(
            &record,
            &mut |page_id, page| {
                flush_raw_page(
                    page_id,
                    page,
                    &mut result,
                    site_info,
                    namespaces,
                    redirect_words,
                )
            },
        )?;
    }
    pages.finish(&mut |page_id, page| {
        flush_raw_page(
            page_id,
            page,
            &mut result,
            site_info,
            namespaces,
            redirect_words,
        )
    })?;
    Ok(result)
}

fn flush_raw_page(
    page_id: u64,
    page: &mut PageData,
    result: &mut RawFrameResult,
    site_info: &crate::archive::SiteInfoRecord,
    namespaces: &NamespaceMap,
    redirect_words: &[RedirectWord],
) -> crate::archive::Result<()> {
    for user_id in &page.contributors {
        result.user_pages.push((*user_id, page_id));
    }
    let extracted = extract_raw_page(
        page_id,
        page,
        site_info,
        namespaces,
        redirect_words,
        |edge| result.raw.push(edge),
    )?;
    result.source_pages += extracted.source_pages;
    result.extracted_static_edges += extracted.extracted_static_edges;
    result.unresolved_dynamic_targets += extracted.unresolved_dynamic_targets;
    Ok(())
}

fn extract_raw_page(
    page_id: u64,
    page: &mut PageData,
    site_info: &crate::archive::SiteInfoRecord,
    namespaces: &NamespaceMap,
    redirect_words: &[RedirectWord],
    mut emit: impl FnMut(RawEdgeRecord),
) -> crate::archive::Result<RawPageStats> {
    let mut stats = RawPageStats::default();
    if page.deleted || page.title.is_empty() {
        return Ok(stats);
    }
    stats.source_pages = 1;
    let title = match page.namespace {
        Some(namespace) => crate::title_index::title_in_namespace(&page.title, namespace, site_info),
        None => std::mem::take(&mut page.title),
    };
    let Some(text) = page.text.take() else {
        return Ok(stats);
    };
    if let Some(target_title) = parse_redirect(&text, redirect_words) {
        let normalized = namespaces.normalize_title_for_site(target_title);
        emit(RawEdgeRecord {
            kind: EdgeKind::Redirect,
            class: SetClass::RawRedirectTarget,
            target: crate::title_index::coded_title(&normalized, site_info),
            source: page_id,
        });
        return Ok(stats);
    }
    let source = SourcePage {
        page_id,
        title,
        text: Some(text.clone()),
    };
    let relations = page_relations(&source, &text, namespaces);
    stats.unresolved_dynamic_targets = relations.dynamic_targets;
    for parsed in relations.edges {
        stats.extracted_static_edges += 1;
        let normalized = namespaces.normalize_title_for_site(&parsed.raw.title);
        let class = raw_class(parsed.raw.certainty, parsed.emitted, parsed.topology);
        emit(RawEdgeRecord {
            kind: parsed.raw.kind,
            class,
            target: crate::title_index::coded_title(&normalized, site_info),
            source: page_id,
        });
    }
    Ok(stats)
}

fn raw_class(certainty: Certainty, emitted: bool, topology: bool) -> SetClass {
    match (emitted, topology, certainty) {
        (true, _, Certainty::Definite) => SetClass::RawEmittedUnconditional,
        (true, _, Certainty::Possible) => SetClass::RawEmittedPossible,
        (false, true, Certainty::Definite) => SetClass::RawTopologyUnconditional,
        (false, true, Certainty::Possible) => SetClass::RawTopologyPossible,
        (false, false, Certainty::Definite) => SetClass::RawNonTopologyUnconditional,
        (false, false, Certainty::Possible) => SetClass::RawNonTopologyPossible,
    }
}

fn read_edge(input: &mut impl Read) -> std::io::Result<Option<EdgeRecord>> {
    let mut bytes = [0_u8; EDGE_BYTES];
    let mut read = 0;
    while read < bytes.len() {
        let count = input.read(&mut bytes[read..])?;
        if count == 0 {
            if read == 0 {
                return Ok(None);
            }
            return Err(invalid_data("truncated edge spool"));
        }
        read += count;
    }
    let kind = parse_kind(bytes[0]).map_err(|_| invalid_data("unknown edge kind in spool"))?;
    if kind == EdgeKind::Redirect {
        return Err(invalid_data("redirect relation in public edge spool"));
    }
    let certainty = match bytes[1] {
        1 => Certainty::Definite,
        2 => Certainty::Possible,
        _ => return Err(invalid_data("unknown edge certainty in spool")),
    };
    Ok(Some(EdgeRecord {
        kind,
        target: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        source: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        certainty,
        emitted: match bytes[2] {
            0 => false,
            1 => true,
            _ => return Err(invalid_data("unknown emitted-edge flag in spool")),
        },
        topology: match bytes[3] {
            0 => false,
            1 => true,
            _ => return Err(invalid_data("unknown topology-edge flag in spool")),
        },
    }))
}

fn write_raw_edge(output: &mut impl Write, edge: RawEdgeRecord) -> std::io::Result<()> {
    if !is_raw_class(edge.class) || !valid_kind_class(edge.kind, edge.class) {
        return Err(invalid_data("invalid raw edge kind/class combination"));
    }
    output.write_all(&[kind_byte(edge.kind), class_byte(edge.class)])?;
    output.write_all(&[0; 6])?;
    output.write_all(&edge.target.to_le_bytes())?;
    output.write_all(&edge.source.to_le_bytes())
}

fn read_raw_edge(input: &mut impl Read) -> std::io::Result<Option<RawEdgeRecord>> {
    let mut bytes = [0_u8; EDGE_BYTES];
    let mut read = 0;
    while read < bytes.len() {
        let count = input.read(&mut bytes[read..])?;
        if count == 0 {
            if read == 0 {
                return Ok(None);
            }
            return Err(invalid_data("truncated raw edge spool"));
        }
        read += count;
    }
    if bytes[2..8] != [0; 6] {
        return Err(invalid_data("nonzero raw edge reserved bytes"));
    }
    let record = RawEdgeRecord {
        kind: parse_kind(bytes[0]).map_err(|_| invalid_data("unknown raw edge kind"))?,
        class: parse_class(bytes[1]).map_err(|_| invalid_data("unknown raw edge class"))?,
        target: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        source: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
    };
    if !is_raw_class(record.class) || !valid_kind_class(record.kind, record.class) {
        return Err(invalid_data("invalid raw edge kind/class combination"));
    }
    Ok(Some(record))
}

fn collect_sorted_raw_edges_in(
    mut spool: std::fs::File,
    scratch: &Path,
) -> crate::archive::Result<RawDiskSets> {
    let temporary = tempfile::tempdir_in(scratch)?;
    let mut runs = Vec::new();
    loop {
        let mut records = Vec::with_capacity(EDGE_RUN_RECORDS);
        while records.len() < EDGE_RUN_RECORDS {
            let Some(record) = read_raw_edge(&mut spool)? else {
                break;
            };
            if !is_raw_class(record.class) {
                return Err(ArchiveError::Invalid("non-raw class in raw edge spool"));
            }
            records.push(record);
        }
        if records.is_empty() {
            break;
        }
        records.sort_unstable();
        records.dedup();
        let path = temporary.path().join(format!("{:08}.raw", runs.len()));
        let mut output = BufWriter::new(std::fs::File::create(&path)?);
        for record in records {
            write_raw_edge(&mut output, record)?;
        }
        output.flush()?;
        runs.push(path);
    }
    let mut stage = 0_usize;
    while runs.len() > EDGE_MERGE_FAN_IN {
        let mut next = Vec::new();
        for (group, inputs) in runs.chunks(EDGE_MERGE_FAN_IN).enumerate() {
            let path = temporary
                .path()
                .join(format!("raw-merge-{stage:04}-{group:08}.run"));
            merge_raw_edge_runs(inputs, &path)?;
            next.push(path);
        }
        for path in &runs {
            std::fs::remove_file(path)?;
        }
        runs = next;
        stage += 1;
    }

    let mut readers = runs
        .iter()
        .map(|path| std::fs::File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<(RawEdgeRecord, usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = read_raw_edge(reader)? {
            heap.push(Reverse((record, run)));
        }
    }
    let mut sets = RawDiskSets::new_in(scratch)?;
    let mut current = None::<RawLogicalKey>;
    let mut members = Bitmap::default();
    while let Some(Reverse((record, run))) = heap.pop() {
        let key = record.logical_key();
        if current != Some(key) {
            if let Some(previous) = current {
                sets.put(previous, &members)?;
                members = Bitmap::default();
            }
            current = Some(key);
        }
        members.insert(record.source);
        if let Some(next) = read_raw_edge(&mut readers[run])? {
            heap.push(Reverse((next, run)));
        }
    }
    if let Some(previous) = current {
        sets.put(previous, &members)?;
    }
    Ok(sets)
}

fn merge_raw_edge_runs(
    inputs: &[std::path::PathBuf],
    output: &Path,
) -> std::io::Result<()> {
    let mut readers = inputs
        .iter()
        .map(|path| std::fs::File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<(RawEdgeRecord, usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = read_raw_edge(reader)? {
            heap.push(Reverse((record, run)));
        }
    }
    let mut writer = BufWriter::new(std::fs::File::create(output)?);
    let mut previous = None;
    while let Some(Reverse((record, run))) = heap.pop() {
        if previous != Some(record) {
            write_raw_edge(&mut writer, record)?;
            previous = Some(record);
        }
        if let Some(next) = read_raw_edge(&mut readers[run])? {
            heap.push(Reverse((next, run)));
        }
    }
    writer.flush()
}

fn derive_public_edges_from_raw(
    mut raw: RawDiskSets,
    titles: &crate::title_index::TitleIndex,
    scratch: &Path,
) -> crate::archive::Result<(CollectedEdges, RedirectTable, u64, RawDiskSets)> {
    let mut redirect_targets = BTreeMap::<u64, Option<u64>>::new();
    let redirect_keys = raw
        .positions
        .iter()
        .filter(|((kind, class, _), _)| {
            *kind == EdgeKind::Redirect && *class == SetClass::RawRedirectTarget
        })
        .map(|((_, _, target), _)| *target)
        .collect::<Vec<_>>();
    for target_code in redirect_keys {
        let members = raw
            .get(&(EdgeKind::Redirect, SetClass::RawRedirectTarget, target_code))?
            .ok_or(ArchiveError::Invalid("missing raw redirect set"))?;
        let target = titles.current_owner(target_code);
        for source in members.members() {
            if redirect_targets.insert(source, target).is_some() {
                return Err(ArchiveError::Invalid("duplicate raw redirect source"));
            }
        }
    }
    let mut redirects = RedirectTable::default();
    for (source, target) in redirect_targets {
        redirects.push(source, target);
    }

    let mut public_spool = tempfile::tempfile_in(scratch)?;
    let mut missing_targets = 0_u64;
    let keys = raw
        .positions
        .iter()
        .map(|((kind, class, target), _)| (*kind, *class, *target))
        .collect::<Vec<_>>();
    for (kind, class, target_code) in keys {
        let Some((certainty, emitted, topology)) = raw_class_attributes(class) else {
            continue;
        };
        if kind == EdgeKind::Redirect {
            return Err(ArchiveError::Invalid("non-redirect raw class for redirect kind"));
        }
        let members = raw
            .get(&(kind, class, target_code))?
            .ok_or(ArchiveError::Invalid("missing raw relation set"))?;
        let Some(immediate_target) = titles.current_owner(target_code) else {
            missing_targets += members.members().count() as u64;
            continue;
        };
        for source in members.members() {
            write_edge(
                &mut public_spool,
                EdgeRecord {
                    kind,
                    target: immediate_target,
                    certainty,
                    emitted,
                    topology,
                    source,
                },
            )?;
        }
    }
    public_spool.sync_all()?;
    public_spool.seek(SeekFrom::Start(0))?;
    let collected = collect_sorted_edges_in(public_spool, &redirects, scratch)?;
    Ok((collected, redirects, missing_targets, raw))
}

fn raw_class_attributes(class: SetClass) -> Option<(Certainty, bool, bool)> {
    Some(match class {
        SetClass::RawNonTopologyUnconditional => (Certainty::Definite, false, false),
        SetClass::RawNonTopologyPossible => (Certainty::Possible, false, false),
        SetClass::RawTopologyUnconditional => (Certainty::Definite, false, true),
        SetClass::RawTopologyPossible => (Certainty::Possible, false, true),
        SetClass::RawEmittedUnconditional => (Certainty::Definite, true, false),
        SetClass::RawEmittedPossible => (Certainty::Possible, true, false),
        SetClass::RawRedirectTarget
        | SetClass::DirectUnconditional
        | SetClass::DirectPossible
        | SetClass::TransitiveUnconditional
        | SetClass::TransitivePossible => return None,
    })
}

fn is_raw_class(class: SetClass) -> bool {
    matches!(
        class,
        SetClass::RawNonTopologyUnconditional
            | SetClass::RawNonTopologyPossible
            | SetClass::RawTopologyUnconditional
            | SetClass::RawTopologyPossible
            | SetClass::RawEmittedUnconditional
            | SetClass::RawEmittedPossible
            | SetClass::RawRedirectTarget
    )
}

fn valid_kind_class(kind: EdgeKind, class: SetClass) -> bool {
    match class {
        SetClass::RawRedirectTarget => kind == EdgeKind::Redirect,
        SetClass::RawNonTopologyUnconditional
        | SetClass::RawNonTopologyPossible
        | SetClass::RawTopologyUnconditional
        | SetClass::RawTopologyPossible
        | SetClass::RawEmittedUnconditional
        | SetClass::RawEmittedPossible => matches!(
            kind,
            EdgeKind::Template | EdgeKind::Module | EdgeKind::Category | EdgeKind::File
        ),
        SetClass::DirectUnconditional
        | SetClass::DirectPossible
        | SetClass::TransitiveUnconditional
        | SetClass::TransitivePossible => kind != EdgeKind::Redirect,
    }
}

#[cfg(test)]
fn collect_sorted_edges_with_limit(
    spool: std::fs::File,
    run_records: usize,
    redirects: &RedirectTable,
) -> crate::archive::Result<CollectedEdges> {
    let temporary = tempfile::tempdir()?;
    collect_sorted_edges_with_limit_in(spool, run_records, redirects, temporary.path())
}

fn collect_sorted_edges_in(
    spool: std::fs::File,
    redirects: &RedirectTable,
    scratch: &Path,
) -> crate::archive::Result<CollectedEdges> {
    collect_sorted_edges_with_limit_in(spool, EDGE_RUN_RECORDS, redirects, scratch)
}

fn collect_sorted_edges_with_limit_in(
    mut spool: std::fs::File,
    run_records: usize,
    redirects: &RedirectTable,
    scratch: &Path,
) -> crate::archive::Result<CollectedEdges> {
    if run_records == 0 {
        return Err(ArchiveError::Invalid("zero edge-sort run size"));
    }
    let temporary = tempfile::tempdir_in(scratch)?;
    let mut runs = Vec::new();
    let mut redirect_misses = 0_u64;
    loop {
        let mut records = Vec::with_capacity(run_records);
        while records.len() < run_records {
            let Some(mut edge) = read_edge(&mut spool)? else {
                break;
            };
            let Some(target) = follow_redirects(edge.target, redirects) else {
                redirect_misses += 1;
                continue;
            };
            edge.target = target;
            records.push(edge);
        }
        if records.is_empty() {
            break;
        }
        records.sort_unstable();
        records.dedup();
        let path = temporary.path().join(format!("{:08}.run", runs.len()));
        let mut run = BufWriter::new(std::fs::File::create(&path)?);
        for edge in records {
            write_edge(&mut run, edge)?;
        }
        run.flush()?;
        runs.push(path);
    }
    let mut stage = 0_usize;
    while runs.len() > EDGE_MERGE_FAN_IN {
        let mut next = Vec::new();
        for (group, inputs) in runs.chunks(EDGE_MERGE_FAN_IN).enumerate() {
            let path = temporary
                .path()
                .join(format!("merge-{stage:04}-{group:08}.run"));
            merge_edge_runs(inputs, &path)?;
            next.push(path);
        }
        for path in &runs {
            std::fs::remove_file(path)?;
        }
        runs = next;
        stage += 1;
    }

    let mut readers = runs
        .iter()
        .map(|path| std::fs::File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<(EdgeRecord, usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(edge) = read_edge(reader)? {
            heap.push(Reverse((edge, run)));
        }
    }
    let mut direct = DiskSets::new_in(scratch)?;
    let mut topology_seeds = DiskSets::new_in(scratch)?;
    let mut graph_spool = tempfile::tempfile_in(scratch)?;
    let mut effect_spool = tempfile::tempfile_in(scratch)?;
    let mut accumulator = None::<TargetAccumulator>;
    let mut source_key = None::<(EdgeKind, u64, bool, u64)>;
    let mut source_direct = None::<Certainty>;
    let mut source_topology = None::<Certainty>;
    while let Some(Reverse((edge, run))) = heap.pop() {
        let next_source = (edge.kind, edge.target, edge.emitted, edge.source);
        if source_key != Some(next_source) {
            if let Some(previous) = source_key {
                flush_source_accumulator(
                    accumulator.as_mut().unwrap(),
                    previous.3,
                    source_direct,
                    source_topology,
                    &mut graph_spool,
                )?;
            }
            let next_target = (edge.kind, edge.target, edge.emitted);
            if accumulator.as_ref().is_some_and(|old| old.key != next_target) {
                flush_target_accumulator(
                    accumulator.take().unwrap(),
                    &mut direct,
                    &mut topology_seeds,
                    &mut effect_spool,
                )?;
            }
            accumulator.get_or_insert_with(|| TargetAccumulator::new(next_target));
            source_key = Some(next_source);
            source_direct = None;
            source_topology = None;
        }
        if !edge.emitted {
            source_direct = Some(source_direct.map_or(edge.certainty, |old| old.min(edge.certainty)));
            if edge.topology {
                source_topology =
                    Some(source_topology.map_or(edge.certainty, |old| old.min(edge.certainty)));
            }
        } else {
            source_direct = Some(source_direct.map_or(edge.certainty, |old| old.min(edge.certainty)));
        }
        if let Some(next) = read_edge(&mut readers[run])? {
            heap.push(Reverse((next, run)));
        }
    }
    if let Some(previous) = source_key {
        flush_source_accumulator(
            accumulator.as_mut().unwrap(),
            previous.3,
            source_direct,
            source_topology,
            &mut graph_spool,
        )?;
    }
    if let Some(accumulator) = accumulator {
        flush_target_accumulator(
            accumulator,
            &mut direct,
            &mut topology_seeds,
            &mut effect_spool,
        )?;
    }
    graph_spool.seek(SeekFrom::Start(0))?;
    let graph = build_disk_graph_in(graph_spool, EDGE_RUN_RECORDS, scratch)?;
    effect_spool.seek(SeekFrom::Start(0))?;
    let effects = build_disk_graph_in(effect_spool, EDGE_RUN_RECORDS, scratch)?;
    Ok(CollectedEdges {
        direct,
        topology_seeds,
        graph,
        effects,
        redirect_misses,
    })
}

fn flush_source_accumulator(
    accumulator: &mut TargetAccumulator,
    source: u64,
    direct: Option<Certainty>,
    topology: Option<Certainty>,
    graph_spool: &mut std::fs::File,
) -> std::io::Result<()> {
    if accumulator.key.2 {
        if let Some(certainty) = direct {
            accumulator.emissions.push((source, certainty));
        }
        return Ok(());
    }
    if let Some(certainty) = direct {
        match certainty {
            Certainty::Definite => accumulator.definite.insert(source),
            Certainty::Possible => accumulator.possible.insert(source),
        }
    }
    if let Some(certainty) = topology {
        match certainty {
            Certainty::Definite => accumulator.topology_definite.insert(source),
            Certainty::Possible => accumulator.topology_possible.insert(source),
        }
        write_graph_edge(
            graph_spool,
            GraphEdge {
                source,
                target: accumulator.key.1,
                kind: accumulator.key.0,
                certainty,
            },
        )?;
    }
    Ok(())
}

fn flush_target_accumulator(
    mut accumulator: TargetAccumulator,
    direct: &mut DiskSets,
    topology: &mut DiskSets,
    effects: &mut std::fs::File,
) -> std::io::Result<()> {
    let (kind, target, is_emitted) = accumulator.key;
    if is_emitted {
        for (source, certainty) in accumulator.emissions {
            write_graph_edge(
                effects,
                GraphEdge {
                    source,
                    target,
                    kind,
                    certainty,
                },
            )?;
        }
        return Ok(());
    }
    accumulator.possible.subtract(&accumulator.definite);
    accumulator
        .topology_possible
        .subtract(&accumulator.topology_definite);
    direct.put((kind, target, Certainty::Definite), &accumulator.definite)?;
    direct.put((kind, target, Certainty::Possible), &accumulator.possible)?;
    topology.put(
        (kind, target, Certainty::Definite),
        &accumulator.topology_definite,
    )?;
    topology.put(
        (kind, target, Certainty::Possible),
        &accumulator.topology_possible,
    )
}

fn write_bitmap_edges(
    output: &mut impl Write,
    kind: EdgeKind,
    target: u64,
    certainty: Certainty,
    members: &Bitmap,
) -> std::io::Result<()> {
    for source in members.members() {
        write_edge(
            output,
            EdgeRecord {
                kind,
                target,
                certainty,
                emitted: false,
                topology: false,
                source,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
fn normalize_direct_certainty(direct: &mut DirectSets) {
    let possible = direct
        .keys()
        .filter(|(_, _, certainty)| *certainty == Certainty::Possible)
        .copied()
        .collect::<Vec<_>>();
    for key @ (kind, target, _) in possible {
        let definite = direct
            .get(&(kind, target, Certainty::Definite))
            .cloned();
        if let (Some(possible), Some(definite)) = (direct.get_mut(&key), definite) {
            possible.subtract(&definite);
        }
    }
    direct.retain(|_, members| !members.words.is_empty());
}

fn merge_edge_runs(inputs: &[std::path::PathBuf], output: &Path) -> std::io::Result<()> {
    let mut readers = inputs
        .iter()
        .map(|path| std::fs::File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<(EdgeRecord, usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(edge) = read_edge(reader)? {
            heap.push(Reverse((edge, run)));
        }
    }
    let mut writer = BufWriter::new(std::fs::File::create(output)?);
    let mut previous = None;
    while let Some(Reverse((edge, run))) = heap.pop() {
        if previous != Some(edge) {
            write_edge(&mut writer, edge)?;
            previous = Some(edge);
        }
        if let Some(next) = read_edge(&mut readers[run])? {
            heap.push(Reverse((next, run)));
        }
    }
    writer.flush()
}

fn write_user_page(output: &mut impl Write, user_id: u64, page_id: u64) -> std::io::Result<()> {
    output.write_all(&user_id.to_le_bytes())?;
    output.write_all(&page_id.to_le_bytes())
}

fn read_user_page(input: &mut impl Read) -> std::io::Result<Option<(u64, u64)>> {
    let mut bytes = [0_u8; 16];
    let mut read = 0;
    while read < bytes.len() {
        let count = input.read(&mut bytes[read..])?;
        if count == 0 {
            if read == 0 {
                return Ok(None);
            }
            return Err(invalid_data("truncated user-page pair"));
        }
        read += count;
    }
    Ok(Some((
        u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        u64::from_le_bytes(bytes[8..].try_into().unwrap()),
    )))
}

fn merge_user_page_runs(
    inputs: &[std::path::PathBuf],
    output: &mut impl Write,
) -> std::io::Result<()> {
    let mut readers = inputs
        .iter()
        .map(|path| std::fs::File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<((u64, u64), usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(pair) = read_user_page(reader)? {
            heap.push(Reverse((pair, run)));
        }
    }
    let mut previous = None;
    while let Some(Reverse((pair, run))) = heap.pop() {
        if previous != Some(pair) {
            write_user_page(output, pair.0, pair.1)?;
            previous = Some(pair);
        }
        if let Some(next) = read_user_page(&mut readers[run])? {
            heap.push(Reverse((next, run)));
        }
    }
    Ok(())
}

fn collect_user_edit_pages_in(
    archive: &Path,
    scratch: &Path,
    workers: usize,
) -> crate::archive::Result<std::fs::File> {
    collect_user_edit_pages_with_limit_in(archive, USER_PAGE_RUN_RECORDS, scratch, workers)
}

fn collect_user_edit_pages_from_spool_in(
    mut spool: std::fs::File,
    scratch: &Path,
) -> crate::archive::Result<std::fs::File> {
    let temporary = tempfile::tempdir_in(scratch)?;
    let mut runs = Vec::new();
    let mut run_index = 0_u64;
    loop {
        let mut records = Vec::with_capacity(USER_PAGE_RUN_RECORDS);
        while records.len() < USER_PAGE_RUN_RECORDS {
            let Some(pair) = read_user_page(&mut spool)? else {
                break;
            };
            records.push(pair);
        }
        if records.is_empty() {
            break;
        }
        if let Some(path) = flush_user_page_run(
            temporary.path(),
            run_index,
            0,
            &mut records,
        )? {
            runs.push(path);
        }
        run_index += 1;
    }
    let mut stage = 0_usize;
    while runs.len() > EDGE_MERGE_FAN_IN {
        let mut next = Vec::new();
        for (group, inputs) in runs.chunks(EDGE_MERGE_FAN_IN).enumerate() {
            let path = temporary
                .path()
                .join(format!("user-spool-merge-{stage:04}-{group:08}.run"));
            let mut output = BufWriter::new(std::fs::File::create(&path)?);
            merge_user_page_runs(inputs, &mut output)?;
            output.flush()?;
            next.push(path);
        }
        for path in &runs {
            std::fs::remove_file(path)?;
        }
        runs = next;
        stage += 1;
    }
    let mut output = tempfile::tempfile_in(scratch)?;
    merge_user_page_runs(&runs, &mut output)?;
    output.seek(SeekFrom::Start(0))?;
    Ok(output)
}

#[cfg(test)]
fn collect_user_edit_pages_with_limit(
    archive: &Path,
    run_records: usize,
) -> crate::archive::Result<std::fs::File> {
    let temporary = tempfile::tempdir()?;
    collect_user_edit_pages_with_limit_in(archive, run_records, temporary.path(), 1)
}

fn collect_user_edit_pages_with_limit_in(
    archive: &Path,
    run_records: usize,
    scratch: &Path,
    workers: usize,
) -> crate::archive::Result<std::fs::File> {
    if run_records == 0 {
        return Err(ArchiveError::Invalid("zero user-page sort run size"));
    }
    let temporary = tempfile::tempdir_in(scratch)?;
    let temporary_path = temporary.path().to_path_buf();
    let mut runs = Vec::new();
    crate::archive::process_frames_parallel(
        archive,
        workers,
        |sequence, _, frame| {
            let mut frame_runs = Vec::new();
            let mut records = Vec::with_capacity(run_records);
            let mut run_index = 0_usize;
            while let Some(record) = frame.next_record()? {
                let Record::Revision { page_id, revision } = record else {
                    continue;
                };
                let crate::ContributorMeta::Named { user_id, .. } = revision.meta.contributor
                else {
                    continue;
                };
                if user_id == 0 {
                    continue;
                }
                records.push((user_id, page_id));
                if records.len() == run_records {
                    if let Some(path) = flush_user_page_run(
                        &temporary_path,
                        sequence,
                        run_index,
                        &mut records,
                    )? {
                        frame_runs.push(path);
                    }
                    run_index += 1;
                }
            }
            if let Some(path) = flush_user_page_run(
                &temporary_path,
                sequence,
                run_index,
                &mut records,
            )? {
                frame_runs.push(path);
            }
            Ok(frame_runs)
        },
        |_, frame_runs| {
            runs.extend(frame_runs);
            Ok(())
        },
    )?;
    let mut stage = 0_usize;
    while runs.len() > EDGE_MERGE_FAN_IN {
        let mut next = Vec::new();
        for (group, inputs) in runs.chunks(EDGE_MERGE_FAN_IN).enumerate() {
            let path = temporary
                .path()
                .join(format!("user-merge-{stage:04}-{group:08}.run"));
            let mut output = BufWriter::new(std::fs::File::create(&path)?);
            merge_user_page_runs(inputs, &mut output)?;
            output.flush()?;
            next.push(path);
        }
        for path in &runs {
            std::fs::remove_file(path)?;
        }
        runs = next;
        stage += 1;
    }
    let mut output = tempfile::tempfile_in(scratch)?;
    merge_user_page_runs(&runs, &mut output)?;
    output.seek(SeekFrom::Start(0))?;
    Ok(output)
}

fn flush_user_page_run(
    temporary: &Path,
    sequence: u64,
    run_index: usize,
    records: &mut Vec<(u64, u64)>,
) -> std::io::Result<Option<std::path::PathBuf>> {
    if records.is_empty() {
        return Ok(None);
    }
    records.sort_unstable();
    records.dedup();
    let path = temporary.join(format!("user-frame-{sequence:016}-{run_index:08}.run"));
    let mut output = BufWriter::new(std::fs::File::create(&path)?);
    for (user_id, page_id) in records.drain(..) {
        write_user_page(&mut output, user_id, page_id)?;
    }
    output.flush()?;
    Ok(Some(path))
}

fn visit_user_edit_sets(
    input: &mut std::fs::File,
    mut visitor: impl FnMut(LogicalSet) -> crate::archive::Result<()>,
) -> crate::archive::Result<(u64, u64)> {
    input.seek(SeekFrom::Start(0))?;
    let mut current_user = None;
    let mut pages = Bitmap::default();
    let mut users = 0_u64;
    let mut memberships = 0_u64;
    while let Some((user_id, page_id)) = read_user_page(input)? {
        if current_user != Some(user_id) {
            if let Some(previous) = current_user {
                memberships += pages.members().count() as u64;
                visitor(LogicalSet {
                    key: SetKey {
                        target_page_id: previous,
                        kind: EdgeKind::UserEdits,
                        class: SetClass::DirectUnconditional,
                    },
                    members: std::mem::take(&mut pages),
                    topology_bases: Vec::new(),
                })?;
                users += 1;
            }
            current_user = Some(user_id);
        }
        pages.insert(page_id);
    }
    if let Some(user_id) = current_user {
        memberships += pages.members().count() as u64;
        visitor(LogicalSet {
            key: SetKey {
                target_page_id: user_id,
                kind: EdgeKind::UserEdits,
                class: SetClass::DirectUnconditional,
            },
            members: pages,
            topology_bases: Vec::new(),
        })?;
        users += 1;
    }
    Ok((users, memberships))
}

fn write_graph_edge(output: &mut impl Write, edge: GraphEdge) -> std::io::Result<()> {
    output.write_all(&edge.source.to_le_bytes())?;
    output.write_all(&edge.target.to_le_bytes())?;
    output.write_all(&[kind_byte(edge.kind), certainty_byte(edge.certainty)])?;
    output.write_all(&[0; 6])
}

fn read_graph_edge(input: &mut impl Read) -> std::io::Result<Option<GraphEdge>> {
    let mut bytes = [0_u8; EDGE_BYTES];
    let mut read = 0;
    while read < bytes.len() {
        let count = input.read(&mut bytes[read..])?;
        if count == 0 {
            if read == 0 {
                return Ok(None);
            }
            return Err(invalid_data("truncated graph edge"));
        }
        read += count;
    }
    Ok(Some(GraphEdge {
        source: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        target: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        kind: parse_kind(bytes[16]).map_err(|_| invalid_data("invalid graph edge kind"))?,
        certainty: match bytes[17] {
            1 => Certainty::Definite,
            2 => Certainty::Possible,
            _ => return Err(invalid_data("invalid graph edge certainty")),
        },
    }))
}

impl DiskGraph {
    #[cfg(test)]
    fn from_edges(edges: Vec<GraphEdge>) -> crate::archive::Result<Self> {
        let mut spool = tempfile::tempfile()?;
        for edge in edges {
            write_graph_edge(&mut spool, edge)?;
        }
        spool.seek(SeekFrom::Start(0))?;
        build_disk_graph(spool, EDGE_RUN_RECORDS)
    }

    fn edge_at(&self, reverse: bool, index: usize) -> GraphEdge {
        let map = if reverse {
            self.reverse.as_ref()
        } else {
            self.forward.as_ref()
        }
        .expect("edge index cannot refer to an empty graph");
        let start = index * EDGE_BYTES;
        let bytes = &map[start..start + EDGE_BYTES];
        GraphEdge {
            source: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            target: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            kind: parse_kind(bytes[16]).unwrap(),
            certainty: match bytes[17] {
                1 => Certainty::Definite,
                2 => Certainty::Possible,
                _ => unreachable!(),
            },
        }
    }

    fn range(&self, reverse: bool, source: u64) -> (usize, usize) {
        let count = if reverse {
            self.reverse.as_ref()
        } else {
            self.forward.as_ref()
        }
        .map_or(0, |map| map.len() / EDGE_BYTES);
        let mut lower = 0;
        let mut upper = count;
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            if self.edge_at(reverse, middle).source < source {
                lower = middle + 1;
            } else {
                upper = middle;
            }
        }
        let start = lower;
        upper = count;
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            if self.edge_at(reverse, middle).source <= source {
                lower = middle + 1;
            } else {
                upper = middle;
            }
        }
        (start, lower - start)
    }
}

#[cfg(test)]
fn build_disk_graph(
    spool: std::fs::File,
    run_records: usize,
) -> crate::archive::Result<DiskGraph> {
    let temporary = tempfile::tempdir()?;
    build_disk_graph_in(spool, run_records, temporary.path())
}

fn build_disk_graph_in(
    spool: std::fs::File,
    run_records: usize,
    scratch: &Path,
) -> crate::archive::Result<DiskGraph> {
    let forward_file = sort_graph_spool_in(spool, run_records, scratch)?;
    let mut reverse_spool = tempfile::tempfile_in(scratch)?;
    if let Some(map) = mmap_file(&forward_file)? {
        for index in 0..map.len() / EDGE_BYTES {
            let start = index * EDGE_BYTES;
            let bytes = &map[start..start + EDGE_BYTES];
            write_graph_edge(
                &mut reverse_spool,
                GraphEdge {
                    source: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
                    target: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                    kind: parse_kind(bytes[16])?,
                    certainty: match bytes[17] {
                        1 => Certainty::Definite,
                        2 => Certainty::Possible,
                        _ => return Err(ArchiveError::Invalid("invalid graph certainty")),
                    },
                },
            )?;
        }
    }
    reverse_spool.seek(SeekFrom::Start(0))?;
    let reverse_file = sort_graph_spool_in(reverse_spool, run_records, scratch)?;
    Ok(DiskGraph {
        forward: mmap_file(&forward_file)?,
        reverse: mmap_file(&reverse_file)?,
    })
}

fn mmap_file(file: &std::fs::File) -> std::io::Result<Option<Mmap>> {
    if file.metadata()?.len() == 0 {
        return Ok(None);
    }
    // SAFETY: the temporary file remains open for the lifetime of the map and
    // is never modified after this point.
    unsafe { memmap2::MmapOptions::new().map(file).map(Some) }
}

fn sort_graph_spool_in(
    mut spool: std::fs::File,
    run_records: usize,
    scratch: &Path,
) -> crate::archive::Result<std::fs::File> {
    if run_records == 0 {
        return Err(ArchiveError::Invalid("zero graph-sort run size"));
    }
    let temporary = tempfile::tempdir_in(scratch)?;
    let mut runs = Vec::new();
    loop {
        let mut records = Vec::with_capacity(run_records);
        while records.len() < run_records {
            let Some(edge) = read_graph_edge(&mut spool)? else {
                break;
            };
            records.push(edge);
        }
        if records.is_empty() {
            break;
        }
        records.sort_unstable();
        records.dedup();
        let path = temporary.path().join(format!("{:08}.graph", runs.len()));
        let mut output = BufWriter::new(std::fs::File::create(&path)?);
        for edge in records {
            write_graph_edge(&mut output, edge)?;
        }
        output.flush()?;
        runs.push(path);
    }
    let mut stage = 0_usize;
    while runs.len() > EDGE_MERGE_FAN_IN {
        let mut next = Vec::new();
        for (group, inputs) in runs.chunks(EDGE_MERGE_FAN_IN).enumerate() {
            let path = temporary
                .path()
                .join(format!("merge-{stage:04}-{group:08}.graph"));
            merge_graph_runs(inputs, &path)?;
            next.push(path);
        }
        for path in &runs {
            std::fs::remove_file(path)?;
        }
        runs = next;
        stage += 1;
    }
    let output = tempfile::tempfile_in(scratch)?;
    merge_graph_readers(&runs, &output)?;
    Ok(output)
}

fn merge_graph_runs(inputs: &[std::path::PathBuf], output: &Path) -> std::io::Result<()> {
    let output = std::fs::File::create(output)?;
    merge_graph_readers(inputs, &output)
}

fn merge_graph_readers(
    inputs: &[std::path::PathBuf],
    output: &std::fs::File,
) -> std::io::Result<()> {
    let mut readers = inputs
        .iter()
        .map(|path| std::fs::File::open(path).map(BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<(GraphEdge, usize)>>::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(edge) = read_graph_edge(reader)? {
            heap.push(Reverse((edge, run)));
        }
    }
    let mut writer = BufWriter::new(output.try_clone()?);
    let mut previous = None;
    while let Some(Reverse((edge, run))) = heap.pop() {
        if previous != Some(edge) {
            write_graph_edge(&mut writer, edge)?;
            previous = Some(edge);
        }
        if let Some(next) = read_graph_edge(&mut readers[run])? {
            heap.push(Reverse((next, run)));
        }
    }
    writer.flush()
}

fn read_site_info(
    archive: &Path,
    titles: &crate::title_index::TitleIndex,
) -> crate::archive::Result<crate::archive::SiteInfoRecord> {
    let indexed = crate::archive::IndexedArchiveSet::open(archive, titles)?;
    let mut left = 0;
    let mut right = titles.frame_count();
    while left < right {
        let middle = left + (right - left) / 2;
        if titles.frame(middle)?.info.first_entity.kind < crate::archive::EntityKind::Global {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    for position in left..titles.frame_count() {
        let entry = titles.frame(position)?;
        if entry.info.first_entity.kind != crate::archive::EntityKind::Global {
            continue;
        }
        let location = indexed.location(entry)?;
        let mut input = indexed.open_file(&location)?;
        let mut site_info = None;
        crate::archive::visit_frame_while_file(&mut input, &location, |record| {
            if let Record::SiteInfo {
                site_info: record_site_info,
                ..
            } = record
            {
                site_info = Some(record_site_info);
                return Ok(false);
            }
            Ok(true)
        })?;
        if let Some(site_info) = site_info {
            return Ok(site_info);
        }
    }
    Err(ArchiveError::Invalid("archive has no siteinfo record"))
}

fn visit_latest_pages(
    archive: &Path,
    site_info: &crate::archive::SiteInfoRecord,
    visitor: impl FnMut(SourcePage) -> crate::archive::Result<()>,
) -> crate::archive::Result<()> {
    let mut reader = ArchiveRecordReader::open(archive)?;
    visit_latest_pages_from_records(|| reader.next_record(), site_info, visitor)
}

fn visit_latest_pages_in_frame(
    frame: &mut FrameRecordCursor,
    site_info: &crate::archive::SiteInfoRecord,
    visitor: impl FnMut(SourcePage) -> crate::archive::Result<()>,
) -> crate::archive::Result<()> {
    visit_latest_pages_from_records(|| frame.next_record(), site_info, visitor)
}

fn visit_latest_pages_from_records(
    mut next_record: impl FnMut() -> crate::archive::Result<Option<Record>>,
    site_info: &crate::archive::SiteInfoRecord,
    mut visitor: impl FnMut(SourcePage) -> crate::archive::Result<()>,
) -> crate::archive::Result<()> {
    let mut current_id = None;
    let mut current = PageData::default();
    let mut saw_revision = false;
    while let Some(record) = next_record()? {
        let Some(page_id) = record.page_id() else {
            continue;
        };
        if current_id != Some(page_id) {
            if let Some(previous) = current_id {
                if page_id <= previous {
                    return Err(ArchiveError::Invalid(
                        "archive page IDs are not strictly grouped and increasing",
                    ));
                }
                flush_latest_page(previous, &mut current, site_info, &mut visitor)?;
            }
            current_id = Some(page_id);
            current = PageData::default();
            saw_revision = false;
        }
        match record {
            Record::PageState {
                title,
                namespace,
                deleted,
                ..
            } if current.title.is_empty() => {
                current.title = title;
                current.namespace = namespace;
                current.deleted = deleted;
            }
            Record::Revision { revision, .. } if !saw_revision => {
                saw_revision = true;
                current.text = revision
                    .has_text
                    .then(|| String::from_utf8_lossy(&revision.text).into_owned());
            }
            _ => {}
        }
    }
    if let Some(page_id) = current_id {
        flush_latest_page(page_id, &mut current, site_info, &mut visitor)?;
    }
    Ok(())
}

fn flush_latest_page(
    page_id: u64,
    page: &mut PageData,
    site_info: &crate::archive::SiteInfoRecord,
    visitor: &mut impl FnMut(SourcePage) -> crate::archive::Result<()>,
) -> crate::archive::Result<()> {
    if page.deleted || page.title.is_empty() {
        return Ok(());
    }
    let title = match page.namespace {
        Some(namespace) => {
            crate::title_index::title_in_namespace(&page.title, namespace, site_info)
        }
        None => std::mem::take(&mut page.title),
    };
    visitor(SourcePage {
        page_id,
        title,
        text: page.text.take(),
    })
}

fn follow_redirects(
    mut page_id: u64,
    redirects: &RedirectTable,
) -> Option<u64> {
    let mut seen = [0_u64; 32];
    for depth in 0..seen.len() {
        if seen[..depth].contains(&page_id) {
            return None;
        }
        seen[depth] = page_id;
        match redirects.lookup(page_id) {
            None => return Some(page_id),
            Some(None) => return None,
            Some(Some(next)) => page_id = next,
        };
    }
    None
}

#[cfg(test)]
fn build_sets(
    pages: &[SourcePage],
    resolve_title: impl FnMut(&str) -> Option<u64>,
) -> Vec<(SetKey, Vec<u64>)> {
    let namespaces = NamespaceMap::english();
    let logical = build_logical_sets(
        pages,
        resolve_title,
        &namespaces,
        &[RedirectWord {
            alias: "#REDIRECT".to_string(),
            case_sensitive: false,
        }],
    );
    logical
        .into_iter()
        .map(|set| (set.key, set.members.members().collect::<Vec<_>>()))
        .collect()
}

#[cfg(test)]
fn build_logical_sets(
    pages: &[SourcePage],
    mut resolve_title: impl FnMut(&str) -> Option<u64>,
    namespaces: &NamespaceMap,
    redirect_words: &[RedirectWord],
) -> Vec<LogicalSet> {
    let pages_by_id = pages
        .iter()
        .map(|page| (page.page_id, page))
        .collect::<BTreeMap<_, _>>();
    let mut redirects = BTreeMap::new();
    let mut redirect_pages = BTreeSet::new();
    for page in pages {
        if let Some(target_title) = page
            .text
            .as_deref()
            .and_then(|text| parse_redirect(text, redirect_words))
        {
            redirect_pages.insert(page.page_id);
            if let Some(target) = resolve_title(target_title) {
                redirects.insert(page.page_id, target);
            }
        }
    }
    let follow = |mut page_id: u64| {
        let mut seen = BTreeSet::new();
        for _ in 0..32 {
            if !seen.insert(page_id) {
                return None;
            }
            let Some(next) = redirects.get(&page_id).copied() else {
                return Some(page_id);
            };
            page_id = next;
        }
        None
    };

    let mut direct = BTreeMap::<(EdgeKind, u64, Certainty), Bitmap>::new();
    let mut topology_seeds = BTreeMap::<(EdgeKind, u64, Certainty), Bitmap>::new();
    let mut graph_edges = Vec::new();
    let mut effect_edges = Vec::new();

    for source in pages {
        if redirect_pages.contains(&source.page_id) {
            continue;
        }
        let Some(text) = source.text.as_deref() else {
            continue;
        };
        let mut resolved = BTreeMap::<(EdgeKind, u64, bool, bool), Certainty>::new();
        for parsed in page_relations(source, text, namespaces).edges {
            let raw = parsed.raw;
            let title = raw.title;
            let Some(target) = resolve_title(&title).and_then(follow) else {
                continue;
            };
            if !pages_by_id.contains_key(&target) {
                continue;
            }
            resolved
                .entry((raw.kind, target, parsed.emitted, parsed.topology))
                .and_modify(|certainty| {
                    if raw.certainty == Certainty::Definite {
                        *certainty = Certainty::Definite;
                    }
                })
                .or_insert(raw.certainty);
        }
        for ((kind, target, emitted, topology), certainty) in resolved {
            if emitted {
                effect_edges.push(GraphEdge {
                    source: source.page_id,
                    target,
                    kind,
                    certainty,
                });
                continue;
            }
            direct
                .entry((kind, target, certainty))
                .or_default()
                .insert(source.page_id);
            if topology {
                topology_seeds
                    .entry((kind, target, certainty))
                    .or_default()
                    .insert(source.page_id);
                graph_edges.push(GraphEdge {
                    source: source.page_id,
                    target,
                    kind,
                    certainty,
                });
            }
        }
    }

    normalize_direct_certainty(&mut direct);
    normalize_direct_certainty(&mut topology_seeds);
    let graph = DiskGraph::from_edges(graph_edges)
        .expect("temporary backref graph must be writable");
    let effects = DiskGraph::from_edges(effect_edges)
        .expect("temporary backref effects must be writable");
    let mut direct =
        DiskSets::from_memory(direct).expect("temporary backref sets must be writable");
    let mut topology_seeds = DiskSets::from_memory(topology_seeds)
        .expect("temporary backref sets must be writable");
    let temporary = tempfile::tempdir().expect("temporary backref scratch must be writable");
    let mut logical = Vec::new();
    visit_logical_sets(
        &mut direct,
        &mut topology_seeds,
        &graph,
        &effects,
        temporary.path(),
        |set| {
            logical.push(set);
            Ok(())
        },
    )
    .expect("temporary backref stores must be writable");
    logical
}

fn page_relations(
    source: &SourcePage,
    text: &str,
    namespaces: &NamespaceMap,
) -> PageRelations {
    if namespaces.kind_for_title(&source.title) != Some(EdgeKind::Template) {
        let extracted =
            extract_report_with_namespaces(text, InclusionContext::Page, namespaces);
        let source_kind = namespaces.kind_for_title(&source.title);
        return PageRelations {
            edges: extracted
                .edges
                .into_iter()
                .map(|raw| {
                    let topology = matches!(raw.kind, EdgeKind::Template | EdgeKind::Module)
                        || raw.kind == EdgeKind::Category
                            && source_kind == Some(EdgeKind::Category);
                    ParsedPageEdge {
                        raw,
                        emitted: false,
                        topology,
                    }
                })
                .collect(),
            dynamic_targets: extracted.dynamic_targets,
        };
    }
    let page = extract_report_with_namespaces(text, InclusionContext::Page, namespaces);
    let transclusion =
        extract_report_with_namespaces(text, InclusionContext::Transclusion, namespaces);
    let dynamic_targets = page.dynamic_targets + transclusion.dynamic_targets;
    let mut selected = BTreeMap::<(EdgeKind, String, bool, bool), Certainty>::new();
    for (edge, emitted, topology) in page
        .edges
        .into_iter()
        .map(|edge| (edge, false, false))
        .chain(
            transclusion
                .edges
                .into_iter()
                .filter(|edge| {
                    matches!(
                        edge.kind,
                        EdgeKind::Template
                            | EdgeKind::Module
                            | EdgeKind::Category
                            | EdgeKind::File
                    )
                })
                .map(|edge| {
                    let emitted = matches!(edge.kind, EdgeKind::Category | EdgeKind::File);
                    let topology =
                        matches!(edge.kind, EdgeKind::Template | EdgeKind::Module);
                    (edge, emitted, topology)
                }),
        )
    {
        selected
            .entry((edge.kind, edge.title, emitted, topology))
            .and_modify(|certainty| {
                if edge.certainty == Certainty::Definite {
                    *certainty = Certainty::Definite;
                }
            })
            .or_insert(edge.certainty);
    }
    let edges = selected
        .into_iter()
        .map(|((kind, title, emitted, topology), certainty)| ParsedPageEdge {
            raw: RawEdge {
                kind,
                title,
                certainty,
            },
            emitted,
            topology,
        })
        .collect();
    PageRelations {
        edges,
        dynamic_targets,
    }
}

fn parse_redirect<'a>(text: &'a str, words: &[RedirectWord]) -> Option<&'a str> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    let rest = words.iter().find_map(|word| {
        trimmed
            .get(..word.alias.len())
            .filter(|prefix| {
                if word.case_sensitive {
                    *prefix == word.alias.as_str()
                } else {
                    prefix.to_lowercase() == word.alias.to_lowercase()
                }
            })
            .map(|_| trimmed[word.alias.len()..].trim_start())
    })?;
    let rest = rest.strip_prefix("[[")?;
    let end = rest.find([']', '|'])?;
    let target = rest[..end].split('#').next().unwrap_or("").trim();
    (!target.is_empty()).then_some(target)
}

fn namespace_map(site_info: &crate::archive::SiteInfoRecord) -> NamespaceMap {
    let mut map = NamespaceMap::english();
    map.set_main_first_letter(site_info.case == "first-letter");
    for word in &site_info.magic_words {
        for alias in &word.aliases {
            map.add_magic_word(&word.canonical_name, alias, word.case_sensitive);
        }
    }
    for (id, kind) in [
        (10, EdgeKind::Template),
        (828, EdgeKind::Module),
        (14, EdgeKind::Category),
        (6, EdgeKind::File),
    ] {
        let Some(namespace) = site_info.namespaces.iter().find(|namespace| namespace.id == id)
        else {
            continue;
        };
        let preferred = if namespace.localized_name.is_empty() {
            match kind {
                EdgeKind::Template => "Template",
                EdgeKind::Module => "Module",
                EdgeKind::Category => "Category",
                EdgeKind::File => "File",
                EdgeKind::UserEdits => unreachable!("user edits have no MediaWiki namespace"),
                EdgeKind::Redirect => unreachable!("redirects have no MediaWiki namespace"),
            }
        } else {
            &namespace.localized_name
        };
        map.add(
            kind,
            preferred,
            namespace
                .aliases
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(namespace.localized_name.as_str())),
        );
        map.set_kind_first_letter(kind, namespace.case == "first-letter");
    }
    for namespace in &site_info.namespaces {
        let first_letter = namespace.case == "first-letter";
        map.add_known_prefix(&namespace.localized_name, first_letter);
        for alias in &namespace.aliases {
            map.add_known_prefix(alias, first_letter);
        }
    }
    for interwiki in &site_info.interwiki {
        map.add_known_prefix(&interwiki.prefix, false);
    }
    map
}

fn redirect_words(site_info: &crate::archive::SiteInfoRecord) -> Vec<RedirectWord> {
    let mut words = site_info
        .magic_words
        .iter()
        .filter(|word| word.canonical_name.eq_ignore_ascii_case("redirect"))
        .flat_map(|word| {
            word.aliases.iter().cloned().map(|alias| RedirectWord {
                alias,
                case_sensitive: word.case_sensitive,
            })
        })
        .collect::<Vec<_>>();
    if !words
        .iter()
        .any(|word| word.alias.eq_ignore_ascii_case("#REDIRECT"))
    {
        words.push(RedirectWord {
            alias: "#REDIRECT".to_string(),
            case_sensitive: false,
        });
    }
    words.sort_by(|left, right| {
        right
            .alias
            .len()
            .cmp(&left.alias.len())
            .then(left.alias.cmp(&right.alias))
            .then(left.case_sensitive.cmp(&right.case_sensitive))
    });
    words.dedup_by(|left, right| {
        left.alias == right.alias && left.case_sensitive == right.case_sensitive
    });
    words
}

struct BitmapStore {
    file: std::fs::File,
    positions: Vec<Option<(u64, u64)>>,
    len: u64,
}

impl BitmapStore {
    #[cfg(test)]
    fn new() -> std::io::Result<Self> {
        Self::new_with_file(tempfile::tempfile()?)
    }

    fn new_in(root: &Path) -> std::io::Result<Self> {
        Self::new_with_file(tempfile::tempfile_in(root)?)
    }

    fn new_with_file(file: std::fs::File) -> std::io::Result<Self> {
        Ok(Self {
            file,
            positions: Vec::new(),
            len: 0,
        })
    }

    fn put(&mut self, target: u64, bitmap: &Bitmap) -> std::io::Result<()> {
        let bytes = encode_bitmap(bitmap);
        self.file.seek(SeekFrom::Start(self.len))?;
        self.file.write_all(&bytes)?;
        let target = usize::try_from(target)
            .map_err(|_| invalid_data("bitmap-store key is too large"))?;
        if self.positions.len() <= target {
            self.positions.resize(target + 1, None);
        }
        self.positions[target] = Some((self.len, bytes.len() as u64));
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn get(&mut self, target: u64) -> std::io::Result<Option<Bitmap>> {
        let Ok(target) = usize::try_from(target) else {
            return Ok(None);
        };
        let Some((offset, len)) = self.positions.get(target).copied().flatten() else {
            return Ok(None);
        };
        self.file.seek(SeekFrom::Start(offset))?;
        let len = usize::try_from(len).map_err(|_| invalid_data("stored bitmap is too large"))?;
        let mut bytes = vec![0; len];
        self.file.read_exact(&mut bytes)?;
        decode_bitmap(&bytes).map(Some)
    }
}

fn visit_transitive_sets(
    node_info: &[(u64, EdgeKind)],
    graph: &DiskGraph,
    accepted_kinds: &[EdgeKind],
    direct: &mut DiskSets,
    extra: &mut DiskSets,
    scratch: &Path,
    mut visitor: impl FnMut(LogicalSet) -> crate::archive::Result<()>,
) -> crate::archive::Result<()> {
    let accepted = accepted_kinds.iter().copied().collect::<BTreeSet<_>>();
    if node_info.is_empty() {
        return Ok(());
    }
    if node_info.len() > u32::MAX as usize {
        return Err(ArchiveError::Invalid("backref graph has more than u32 nodes"));
    }
    let mut guaranteed = BitmapStore::new_in(scratch)?;

    for possible in [false, true] {
        let (component_of, component_offsets, component_nodes) = strongly_connected_disk(
            graph,
            &accepted,
            possible,
            node_info.len(),
        )?;
        let mut component_spool = tempfile::tempfile_in(scratch)?;
        if let Some(map) = &graph.forward {
            for index in 0..map.len() / EDGE_BYTES {
                let edge = graph.edge_at(false, index);
                if !accepted.contains(&edge.kind)
                    || !possible && edge.certainty != Certainty::Definite
                {
                    continue;
                }
                let Ok(source) = usize::try_from(edge.source) else {
                    continue;
                };
                let Ok(target) = usize::try_from(edge.target) else {
                    continue;
                };
                if source >= node_info.len() || target >= node_info.len() {
                    continue;
                }
                let child = component_of[source];
                let parent = component_of[target];
                if child != parent {
                    write_graph_edge(
                        &mut component_spool,
                        GraphEdge {
                            source: u64::from(parent),
                            target: u64::from(child),
                            kind: accepted_kinds[0],
                            certainty: Certainty::Definite,
                        },
                    )?;
                }
            }
        }
        component_spool.seek(SeekFrom::Start(0))?;
        let component_graph = build_disk_graph_in(component_spool, EDGE_RUN_RECORDS, scratch)?;
        let mut closures = BitmapStore::new_in(scratch)?;
        let component_count = component_offsets.len() - 1;
        let mut remaining = (0..component_count)
            .map(|component| {
                u32::try_from(component_graph.range(false, component as u64).1)
                    .map_err(|_| ArchiveError::Invalid("component fanout exceeds u32"))
            })
            .collect::<crate::archive::Result<Vec<_>>>()?;
        let mut ready = remaining
            .iter()
            .enumerate()
            .filter_map(|(component, count)| (*count == 0).then_some(component))
            .map(Reverse)
            .collect::<BinaryHeap<_>>();
        let mut processed = 0;
        while let Some(Reverse(component)) = ready.pop() {
            processed += 1;
            let mut closure = Bitmap::default();
            for node in &component_nodes
                [component_offsets[component] as usize..component_offsets[component + 1] as usize]
            {
                let node_id = u64::from(*node);
                let Some((target, kind)) = node_info.get(node_id as usize).copied() else {
                    continue;
                };
                if let Some(seed) =
                    direct.get(&(kind, target, Certainty::Definite))?
                {
                    closure.union_with(&seed);
                }
                if let Some(seed) =
                    extra.get(&(kind, target, Certainty::Definite))?
                {
                    closure.union_with(&seed);
                }
                if possible {
                    if let Some(seed) =
                        direct.get(&(kind, target, Certainty::Possible))?
                    {
                        closure.union_with(&seed);
                    }
                    if let Some(seed) =
                        extra.get(&(kind, target, Certainty::Possible))?
                    {
                        closure.union_with(&seed);
                    }
                }
            }
            let (child_start, child_count) =
                component_graph.range(false, component as u64);
            for index in child_start..child_start + child_count {
                let child = component_graph.edge_at(false, index).target;
                if let Some(inherited) = closures.get(child)? {
                    closure.union_with(&inherited);
                }
            }
            closures.put(component as u64, &closure)?;
            let mut previous_in_component = None;
            for node in &component_nodes
                [component_offsets[component] as usize..component_offsets[component + 1] as usize]
            {
                let node_id = u64::from(*node);
                let Some((target, kind)) = node_info.get(node_id as usize).copied() else {
                    continue;
                };
                let mut members = closure.clone();
                members.remove(target);
                if possible {
                    if let Some(base) = guaranteed.get(node_id)? {
                        members.subtract(&base);
                    }
                }
                if members.words.is_empty() {
                    continue;
                }
                let class = if possible {
                    SetClass::TransitivePossible
                } else {
                    SetClass::TransitiveUnconditional
                };
                let mut topology_bases = if possible {
                    Vec::new()
                } else {
                    (child_start..child_start + child_count)
                        .filter_map(|index| {
                            let child = component_graph.edge_at(false, index).target as usize;
                            component_nodes
                                .get(component_offsets[child] as usize)
                        })
                        .filter_map(|child| {
                            let node_id = u64::from(*child);
                            node_info.get(node_id as usize).map(|(target_page_id, kind)| SetKey {
                                target_page_id: *target_page_id,
                                kind: *kind,
                                class,
                            })
                        })
                        .collect::<Vec<_>>()
                };
                if !possible {
                    if let Some(previous) = previous_in_component {
                        topology_bases.push(SetKey {
                            target_page_id: node_info[previous as usize].0,
                            kind: node_info[previous as usize].1,
                            class,
                        });
                    }
                    guaranteed.put(node_id, &members)?;
                    previous_in_component = Some(node_id);
                }
                visitor(LogicalSet {
                    key: SetKey {
                        target_page_id: target,
                        kind,
                        class,
                    },
                    members,
                    topology_bases,
                })?;
            }
            let (parent_start, parent_count) =
                component_graph.range(true, component as u64);
            for index in parent_start..parent_start + parent_count {
                let parent = component_graph.edge_at(true, index).target as usize;
                remaining[parent] -= 1;
                if remaining[parent] == 0 {
                    ready.push(Reverse(parent));
                }
            }
        }
        if processed != component_count {
            return Err(ArchiveError::Invalid("collapsed backref graph is cyclic"));
        }
    }
    Ok(())
}

#[cfg(test)]
fn write_streaming_sidecar(
    output: impl AsRef<Path>,
    direct: DiskSets,
    topology_seeds: DiskSets,
    graph: DiskGraph,
    effects: DiskGraph,
    user_edits: std::fs::File,
    title_index_fingerprint: u64,
) -> crate::archive::Result<(u64, u64, u64)> {
    let temporary = tempfile::tempdir()?;
    write_streaming_sidecar_in(
        output,
        direct,
        topology_seeds,
        graph,
        effects,
        user_edits,
        None,
        title_index_fingerprint,
        temporary.path(),
    )
}

fn write_streaming_sidecar_in(
    output: impl AsRef<Path>,
    direct: DiskSets,
    topology_seeds: DiskSets,
    graph: DiskGraph,
    effects: DiskGraph,
    user_edits: std::fs::File,
    raw: Option<RawDiskSets>,
    title_index_fingerprint: u64,
    scratch: &Path,
) -> crate::archive::Result<(u64, u64, u64)> {
    let capabilities = REQUIRED_CAPABILITIES
        | raw
            .as_ref()
            .map_or(0, |_| CAPABILITY_RAW_POSTINGS);
    write_streaming_sidecar_in_with_capabilities(
        output,
        direct,
        topology_seeds,
        graph,
        effects,
        user_edits,
        raw,
        capabilities,
        title_index_fingerprint,
        scratch,
    )
}

fn write_streaming_sidecar_in_with_capabilities(
    output: impl AsRef<Path>,
    direct: DiskSets,
    topology_seeds: DiskSets,
    graph: DiskGraph,
    effects: DiskGraph,
    user_edits: std::fs::File,
    raw: Option<RawDiskSets>,
    capabilities: u32,
    title_index_fingerprint: u64,
    scratch: &Path,
) -> crate::archive::Result<(u64, u64, u64)> {
    let mut encoder = StreamingEncoder::new_in_with_capabilities(scratch, capabilities)?;
    let counts = add_streaming_sidecar_sets(
        &mut encoder,
        direct,
        topology_seeds,
        graph,
        effects,
        user_edits,
        raw,
        scratch,
    )?;
    encoder.write(output, title_index_fingerprint)?;
    Ok(counts)
}

fn add_streaming_sidecar_sets(
    encoder: &mut StreamingEncoder,
    mut direct: DiskSets,
    mut topology_seeds: DiskSets,
    graph: DiskGraph,
    effects: DiskGraph,
    mut user_edits: std::fs::File,
    mut raw: Option<RawDiskSets>,
    scratch: &Path,
) -> crate::archive::Result<(u64, u64, u64)> {
    visit_logical_sets(
        &mut direct,
        &mut topology_seeds,
        &graph,
        &effects,
        scratch,
        |set| {
            encoder.add(set)?;
            Ok(())
        },
    )?;
    let (users, memberships) = visit_user_edit_sets(&mut user_edits, |set| {
        encoder.add(set)?;
        Ok(())
    })?;
    if let Some(raw) = raw.as_mut() {
        let raw_keys = raw
            .positions
            .iter()
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for (kind, class, target) in raw_keys {
            let members = raw
                .get(&(kind, class, target))?
                .ok_or(ArchiveError::Invalid("missing stored raw set"))?;
            encoder.add(LogicalSet {
                key: SetKey {
                    target_page_id: target,
                    kind,
                    class,
                },
                members,
                topology_bases: Vec::new(),
            })?;
        }
    }
    let count = encoder.logical_count as u64;
    Ok((count, users, memberships))
}

fn visit_logical_sets(
    direct: &mut DiskSets,
    topology_seeds: &mut DiskSets,
    graph: &DiskGraph,
    effects: &DiskGraph,
    scratch: &Path,
    mut visitor: impl FnMut(LogicalSet) -> crate::archive::Result<()>,
) -> crate::archive::Result<()> {
    let direct_keys = direct
        .positions
        .iter()
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    for (kind, target, certainty) in &direct_keys {
        let members = direct
            .get(&(*kind, *target, *certainty))?
            .ok_or(ArchiveError::Invalid("missing stored direct set"))?;
        visitor(LogicalSet {
            key: SetKey {
                target_page_id: *target,
                kind: *kind,
                class: match certainty {
                    Certainty::Definite => SetClass::DirectUnconditional,
                    Certainty::Possible => SetClass::DirectPossible,
                },
            },
            members,
            topology_bases: Vec::new(),
        })?;
    }

    let mut effect_contributions = tempfile::tempfile_in(scratch)?;
    let mut empty_extra = DiskSets::new_in(scratch)?;
    let mut render_nodes = topology_seeds
        .positions
        .iter()
        .map(|(key, _)| key)
        .filter(|(kind, _, _)| matches!(kind, EdgeKind::Template | EdgeKind::Module))
        .map(|(kind, target, _)| (*target, *kind))
        .collect::<Vec<_>>();
    render_nodes.sort_unstable();
    render_nodes.dedup();
    let mut typed_spool = tempfile::tempfile_in(scratch)?;
    if let Some(map) = &graph.forward {
        for index in 0..map.len() / EDGE_BYTES {
            let edge = graph.edge_at(false, index);
            if !matches!(edge.kind, EdgeKind::Template | EdgeKind::Module) {
                continue;
            }
            let Ok(target) = render_nodes.binary_search(&(edge.target, edge.kind)) else {
                continue;
            };
            let first = render_nodes.partition_point(|(page_id, _)| *page_id < edge.source);
            let end = render_nodes.partition_point(|(page_id, _)| *page_id <= edge.source);
            for source in first..end {
                    write_graph_edge(
                        &mut typed_spool,
                        GraphEdge {
                            source: source as u64,
                            target: target as u64,
                            kind: edge.kind,
                            certainty: edge.certainty,
                        },
                    )?;
            }
        }
    }
    typed_spool.seek(SeekFrom::Start(0))?;
    let render_graph = build_disk_graph_in(typed_spool, EDGE_RUN_RECORDS, scratch)?;
    visit_transitive_sets(
        &render_nodes,
        &render_graph,
        &[EdgeKind::Template, EdgeKind::Module],
        topology_seeds,
        &mut empty_extra,
        scratch,
        |set| {
            let (start, count) = effects.range(false, set.key.target_page_id);
            for index in start..start + count {
                let effect = effects.edge_at(false, index);
                let certainty = match (set.key.class, effect.certainty) {
                        (SetClass::TransitiveUnconditional, Certainty::Definite) => {
                            Some(Certainty::Definite)
                        }
                        (SetClass::TransitiveUnconditional, Certainty::Possible)
                        | (SetClass::TransitivePossible, _) => {
                            Some(Certainty::Possible)
                        }
                        _ => None,
                    };
                if let Some(certainty) = certainty {
                    write_bitmap_edges(
                        &mut effect_contributions,
                        effect.kind,
                        effect.target,
                        certainty,
                        &set.members,
                    )?;
                }
            }
            visitor(set)
        },
    )?;
    effect_contributions.seek(SeekFrom::Start(0))?;
    let mut effect_sets = collect_sorted_edges_with_limit_in(
        effect_contributions,
        EDGE_RUN_RECORDS,
        &RedirectTable::default(),
        scratch,
    )?
    .direct;
    let effect_keys = effect_sets
        .positions
        .iter()
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    for (kind, target, certainty) in &effect_keys {
        if *kind != EdgeKind::File {
            continue;
        }
        let members = effect_sets
            .get(&(*kind, *target, *certainty))?
            .ok_or(ArchiveError::Invalid("missing stored file-effect set"))?;
        visitor(LogicalSet {
            key: SetKey {
                target_page_id: *target,
                kind: *kind,
                class: match certainty {
                    Certainty::Definite => SetClass::TransitiveUnconditional,
                    Certainty::Possible => SetClass::TransitivePossible,
                },
            },
            members,
            topology_bases: Vec::new(),
        })?;
    }
    let mut category_targets = Vec::new();
    for (kind, target, _) in direct
        .positions
        .iter()
        .map(|(key, _)| key)
        .chain(effect_sets.positions.iter().map(|(key, _)| key))
    {
        if *kind == EdgeKind::Category {
            category_targets.push(*target);
        }
    }
    category_targets.sort_unstable();
    category_targets.dedup();
    for target in category_targets {
        let mut guaranteed = Bitmap::default();
        for store in [&mut *direct, &mut effect_sets] {
            if let Some(members) =
                store.get(&(EdgeKind::Category, target, Certainty::Definite))?
            {
                guaranteed.union_with(&members);
            }
        }
        if !guaranteed.words.is_empty() {
            visitor(LogicalSet {
                key: SetKey {
                    target_page_id: target,
                    kind: EdgeKind::Category,
                    class: SetClass::TransitiveUnconditional,
                },
                members: guaranteed.clone(),
                topology_bases: Vec::new(),
            })?;
        }
        let mut possible = Bitmap::default();
        for store in [&mut *direct, &mut effect_sets] {
            if let Some(members) =
                store.get(&(EdgeKind::Category, target, Certainty::Possible))?
            {
                possible.union_with(&members);
            }
        }
        possible.subtract(&guaranteed);
        if !possible.words.is_empty() {
            visitor(LogicalSet {
                key: SetKey {
                    target_page_id: target,
                    kind: EdgeKind::Category,
                    class: SetClass::TransitivePossible,
                },
                members: possible,
                topology_bases: Vec::new(),
            })?;
        }
    }
    Ok(())
}

fn strongly_connected_disk(
    graph: &DiskGraph,
    accepted: &BTreeSet<EdgeKind>,
    possible: bool,
    node_count: usize,
) -> crate::archive::Result<(Vec<u32>, Vec<u32>, Vec<u32>)> {
    if node_count > u32::MAX as usize {
        return Err(ArchiveError::Invalid("backref graph has more than u32 nodes"));
    }
    let mut seen = vec![false; node_count];
    let mut finish = Vec::<u32>::with_capacity(node_count);
    for root in 0..node_count {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        let (start, count) = graph.range(false, root as u64);
        let mut stack = vec![(root as u32, start, start + count)];
        while let Some((node, next, end)) = stack.last_mut() {
            let mut descendant = None;
            while *next < *end {
                let edge = graph.edge_at(false, *next);
                *next += 1;
                if !accepted.contains(&edge.kind)
                    || !possible && edge.certainty != Certainty::Definite
                {
                    continue;
                }
                if let Ok(target) = u32::try_from(edge.target) {
                    let target = target as usize;
                    if target >= node_count {
                        continue;
                    }
                    if !seen[target] {
                        descendant = Some(target as u32);
                        break;
                    }
                }
            }
            if let Some(target) = descendant {
                seen[target as usize] = true;
                let (start, count) = graph.range(false, u64::from(target));
                stack.push((target, start, start + count));
            } else {
                finish.push(*node);
                stack.pop();
            }
        }
    }

    let mut component_of = vec![u32::MAX; node_count];
    let mut component_offsets = vec![0_u32];
    let mut component_nodes = Vec::<u32>::new();
    while let Some(root) = finish.pop() {
        if component_of[root as usize] != u32::MAX {
            continue;
        }
        let component = u32::try_from(component_offsets.len() - 1)
            .map_err(|_| ArchiveError::Invalid("backref graph has more than u32 components"))?;
        component_of[root as usize] = component;
        let member_start = component_nodes.len();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            component_nodes.push(node);
            let (start, count) = graph.range(true, u64::from(node));
            for index in start..start + count {
                let edge = graph.edge_at(true, index);
                if !accepted.contains(&edge.kind)
                    || !possible && edge.certainty != Certainty::Definite
                {
                    continue;
                }
                if let Ok(target) = u32::try_from(edge.target) {
                    let target_index = target as usize;
                    if target_index < node_count && component_of[target_index] == u32::MAX {
                        component_of[target_index] = component;
                        stack.push(target);
                    }
                }
            }
        }
        component_nodes[member_start..].sort_unstable();
        component_offsets.push(
            u32::try_from(component_nodes.len())
                .map_err(|_| ArchiveError::Invalid("backref component members exceed u32"))?,
        );
    }
    Ok((component_of, component_offsets, component_nodes))
}

#[cfg(test)]
fn strongly_connected(adjacency: &[Vec<usize>]) -> (Vec<usize>, Vec<Vec<usize>>) {
    let count = adjacency.len();
    let mut reverse = vec![Vec::new(); count];
    for (source, targets) in adjacency.iter().enumerate() {
        for target in targets {
            reverse[*target].push(source);
        }
    }
    let mut seen = vec![false; count];
    let mut finish = Vec::with_capacity(count);
    for root in 0..count {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        let mut stack = vec![(root, 0_usize)];
        while let Some((node, next)) = stack.last_mut() {
            if *next < adjacency[*node].len() {
                let target = adjacency[*node][*next];
                *next += 1;
                if !seen[target] {
                    seen[target] = true;
                    stack.push((target, 0));
                }
            } else {
                finish.push(*node);
                stack.pop();
            }
        }
    }

    let mut component_of = vec![usize::MAX; count];
    let mut components = Vec::new();
    for root in finish.into_iter().rev() {
        if component_of[root] != usize::MAX {
            continue;
        }
        let component = components.len();
        let mut members = Vec::new();
        let mut stack = vec![root];
        component_of[root] = component;
        while let Some(node) = stack.pop() {
            members.push(node);
            for source in &reverse[node] {
                if component_of[*source] == usize::MAX {
                    component_of[*source] = component;
                    stack.push(*source);
                }
            }
        }
        members.sort_unstable();
        components.push(members);
    }
    (component_of, components)
}

#[cfg(test)]
fn reverse_topological(dag: &[BTreeSet<usize>]) -> Vec<usize> {
    let mut seen = vec![false; dag.len()];
    let mut order = Vec::with_capacity(dag.len());
    for root in 0..dag.len() {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        let mut stack = vec![(root, dag[root].iter())];
        while let Some((node, children)) = stack.last_mut() {
            if let Some(child) = children.next().copied() {
                if !seen[child] {
                    seen[child] = true;
                    stack.push((child, dag[child].iter()));
                }
            } else {
                order.push(*node);
                stack.pop();
            }
        }
    }
    order
}

fn encode_sidecar_bitmap(bitmap: &Bitmap) -> Vec<u8> {
    let roaring = bitmap.members().collect::<roaring::RoaringTreemap>();
    let mut output = Vec::with_capacity(roaring.serialized_size());
    roaring
        .serialize_into(&mut output)
        .expect("writing a Roaring bitmap to memory cannot fail");
    output
}

fn decode_sidecar_bitmap(bytes: &[u8]) -> std::io::Result<Bitmap> {
    let mut input = std::io::Cursor::new(bytes);
    let roaring = roaring::RoaringTreemap::deserialize_from(&mut input)?;
    if input.position() != bytes.len() as u64 {
        return Err(invalid_data("trailing Roaring bitmap bytes"));
    }
    let mut words = Vec::<(u64, u64)>::new();
    for member in roaring {
        let word_index = member / 64;
        let bit = 1_u64 << (member % 64);
        match words.last_mut() {
            Some((last_index, word)) if *last_index == word_index => *word |= bit,
            _ => words.push((word_index, bit)),
        }
    }
    Ok(Bitmap { words })
}

fn encode_bitmap(bitmap: &Bitmap) -> Vec<u8> {
    let mut output = Vec::new();
    put_varint(&mut output, bitmap.words.len() as u64);
    let mut previous = 0_u64;
    for (entry_index, (position, word)) in bitmap.words.iter().enumerate() {
        let delta = if entry_index == 0 {
            *position
        } else {
            position.saturating_sub(previous)
        };
        put_varint(&mut output, delta);
        output.extend_from_slice(&word.to_le_bytes());
        previous = *position;
    }
    output
}

fn decode_bitmap(mut bytes: &[u8]) -> std::io::Result<Bitmap> {
    let count = usize::try_from(take_varint(&mut bytes)?)
        .map_err(|_| invalid_data("bitmap word count is too large"))?;
    if count > bytes.len() / 9 {
        return Err(invalid_data("bitmap word count exceeds payload"));
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(count)
        .map_err(|_| invalid_data("bitmap allocation is too large"))?;
    let mut previous = 0_u64;
    for position in 0..count {
        let delta = take_varint(&mut bytes)?;
        let index = if position == 0 {
            delta
        } else {
            previous
                .checked_add(delta)
                .ok_or_else(|| invalid_data("bitmap word index overflow"))?
        };
        let word = u64::from_le_bytes(take_array::<8>(&mut bytes)?);
        if word == 0 || position != 0 && index <= previous {
            return Err(invalid_data("invalid sparse bitmap word"));
        }
        output.push((index, word));
        previous = index;
    }
    if !bytes.is_empty() {
        return Err(invalid_data("trailing bitmap bytes"));
    }
    Ok(Bitmap { words: output })
}

#[cfg(test)]
fn write_sidecar(
    output: impl AsRef<Path>,
    logical: &[(SetKey, Vec<u64>)],
) -> crate::archive::Result<u64> {
    write_sidecar_with_capabilities(output, logical, REQUIRED_CAPABILITIES)
}

#[cfg(test)]
fn write_sidecar_with_capabilities(
    output: impl AsRef<Path>,
    logical: &[(SetKey, Vec<u64>)],
    capabilities: u32,
) -> crate::archive::Result<u64> {
    let mut encoder = StreamingEncoder::new_with_capabilities(capabilities)?;
    for (key, members) in logical {
        let mut bitmap = Bitmap::default();
        for member in members {
            bitmap.insert(*member);
        }
        encoder.add(LogicalSet {
            key: *key,
            members: bitmap,
            topology_bases: Vec::new(),
        })?;
    }
    let objects = encoder.entries.len() as u64;
    encoder.write(output, 0)?;
    Ok(objects)
}

#[cfg(test)]
pub(crate) fn write_test_user_only_sidecar(
    output: impl AsRef<Path>,
    title_index: impl AsRef<Path>,
) -> crate::archive::Result<()> {
    write_sidecar_with_capabilities(
        output.as_ref(),
        &[(
            SetKey {
                target_page_id: 42,
                kind: EdgeKind::UserEdits,
                class: SetClass::DirectUnconditional,
            },
            vec![1],
        )],
        REQUIRED_CAPABILITIES,
    )?;
    let mut bytes = std::fs::read(output.as_ref())?;
    bytes[72..80].copy_from_slice(&file_xxh3_64(title_index.as_ref())?.to_le_bytes());
    std::fs::write(output.as_ref(), bytes)?;
    Ok(())
}

fn file_xxh3_64(path: &Path) -> std::io::Result<u64> {
    let mut input = BufReader::new(std::fs::File::open(path)?);
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            return Ok(hasher.digest());
        }
        hasher.update(&buffer[..count]);
    }
}

fn set_readable_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct BackrefIndex {
    bytes: memmap2::Mmap,
    capabilities: u32,
    title_index_fingerprint: u64,
    object_count: usize,
    directories: Vec<DiskDirectory>,
    object_offsets_offset: usize,
    base_offsets_offset: usize,
    payload_offset: usize,
}

#[derive(Clone, Copy, Debug)]
struct DiskDirectory {
    kind: EdgeKind,
    class: SetClass,
    words_offset: usize,
    word_count: usize,
    object_ids_offset: usize,
    logical_count: usize,
}

#[derive(Clone, Copy, Debug)]
struct BitmapObject {
    base_offset: u8,
    offset: usize,
    len: usize,
}

impl BackrefIndex {
    pub fn open(path: impl AsRef<Path>) -> crate::archive::Result<Self> {
        let file = std::fs::File::open(path)?;
        let bytes = unsafe { memmap2::MmapOptions::new().map(&file)? };
        if bytes.len() < HEADER_BYTES || bytes[..8] != MAGIC {
            return Err(ArchiveError::Invalid("bad backref sidecar magic"));
        }
        if u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize != HEADER_BYTES {
            return Err(ArchiveError::Invalid("unsupported backref sidecar header"));
        }
        let capabilities = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        if capabilities & REQUIRED_CAPABILITIES != REQUIRED_CAPABILITIES {
            return Err(ArchiveError::Invalid(
                "backref sidecar lacks required user-edit capability",
            ));
        }
        if capabilities & !KNOWN_CAPABILITIES != 0 {
            return Err(ArchiveError::Invalid("backref sidecar has unknown capability"));
        }
        let usize_field = |start| {
            usize::try_from(u64::from_le_bytes(
                bytes[start..start + 8].try_into().unwrap(),
            ))
            .map_err(|_| ArchiveError::FieldTooLarge)
        };
        let object_count = usize_field(16)?;
        let logical_count = usize_field(24)?;
        let directories_offset = usize_field(32)?;
        let object_offsets_offset = usize_field(40)?;
        let base_offsets_offset = usize_field(48)?;
        let payload_offset = usize_field(56)?;
        let directory_count = usize_field(64)?;
        let title_index_fingerprint =
            u64::from_le_bytes(bytes[72..80].try_into().unwrap());
        if object_count > u32::MAX as usize || logical_count > u32::MAX as usize {
            return Err(ArchiveError::Invalid("backref count exceeds u32"));
        }
        let descriptor_end = HEADER_BYTES
            .checked_add(
                directory_count
                    .checked_mul(DIRECTORY_DESCRIPTOR_BYTES)
                    .ok_or(ArchiveError::FieldTooLarge)?,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        let expected_bases = object_offsets_offset
            .checked_add(
                object_count
                    .checked_add(1)
                    .and_then(|count| count.checked_mul(8))
                    .ok_or(ArchiveError::FieldTooLarge)?,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        let expected_payload = expected_bases
            .checked_add(object_count)
            .ok_or(ArchiveError::FieldTooLarge)?;
        if directories_offset != HEADER_BYTES
            || descriptor_end > object_offsets_offset
            || base_offsets_offset != expected_bases
            || payload_offset != expected_payload
            || payload_offset > bytes.len()
        {
            return Err(ArchiveError::Invalid("invalid backref sidecar bounds"));
        }
        let mut directories = Vec::with_capacity(directory_count);
        let mut expected_directory_data = descriptor_end;
        let mut counted_logical = 0_usize;
        let mut referenced = vec![false; object_count];
        let mut previous_identity = None;
        for directory_index in 0..directory_count {
            let start = HEADER_BYTES + directory_index * DIRECTORY_DESCRIPTOR_BYTES;
            let kind = parse_kind(bytes[start])?;
            let class = parse_class(bytes[start + 1])?;
            if !valid_kind_class(kind, class) {
                return Err(ArchiveError::Invalid(
                    "invalid backref kind/class combination",
                ));
            }
            if is_raw_class(class) && capabilities & CAPABILITY_RAW_POSTINGS == 0 {
                return Err(ArchiveError::Invalid(
                    "raw backref set without raw-posting capability",
                ));
            }
            if bytes[start + 2..start + 8] != [0; 6]
                || previous_identity.is_some_and(|previous| previous >= (kind, class))
            {
                return Err(ArchiveError::Invalid("invalid backref logical directory identity"));
            }
            previous_identity = Some((kind, class));
            let words_offset = usize_field(start + 8)?;
            let word_count = usize_field(start + 16)?;
            let object_ids_offset = usize_field(start + 24)?;
            let directory_logical_count = usize_field(start + 32)?;
            let words_end = words_offset
                .checked_add(
                    word_count
                        .checked_mul(PRESENCE_WORD_BYTES)
                        .ok_or(ArchiveError::FieldTooLarge)?,
                )
                .ok_or(ArchiveError::FieldTooLarge)?;
            let ids_end = object_ids_offset
                .checked_add(
                    directory_logical_count
                        .checked_mul(4)
                        .ok_or(ArchiveError::FieldTooLarge)?,
                )
                .ok_or(ArchiveError::FieldTooLarge)?;
            if words_offset != expected_directory_data
                || object_ids_offset != words_end
                || ids_end > object_offsets_offset
            {
                return Err(ArchiveError::Invalid("invalid backref logical directory bounds"));
            }
            let mut previous_word = None;
            let mut rank = 0_u64;
            for word_position in 0..word_count {
                let word_start = words_offset + word_position * PRESENCE_WORD_BYTES;
                let word_index =
                    u64::from_le_bytes(bytes[word_start..word_start + 8].try_into().unwrap());
                let word =
                    u64::from_le_bytes(bytes[word_start + 8..word_start + 16].try_into().unwrap());
                let stored_rank = u32::from_le_bytes(
                    bytes[word_start + 16..word_start + 20].try_into().unwrap(),
                );
                if word == 0
                    || previous_word.is_some_and(|previous| previous >= word_index)
                    || u64::from(stored_rank) != rank
                {
                    return Err(ArchiveError::Invalid("invalid backref presence bitmap"));
                }
                rank = rank
                    .checked_add(u64::from(word.count_ones()))
                    .ok_or(ArchiveError::FieldTooLarge)?;
                previous_word = Some(word_index);
            }
            if rank != directory_logical_count as u64 {
                return Err(ArchiveError::Invalid("backref presence rank mismatch"));
            }
            for logical_position in 0..directory_logical_count {
                let id_start = object_ids_offset + logical_position * 4;
                let object_id =
                    u32::from_le_bytes(bytes[id_start..id_start + 4].try_into().unwrap()) as usize;
                if object_id >= object_count {
                    return Err(ArchiveError::Invalid("invalid backref bitmap object id"));
                }
                referenced[object_id] = true;
            }
            counted_logical = counted_logical
                .checked_add(directory_logical_count)
                .ok_or(ArchiveError::FieldTooLarge)?;
            expected_directory_data = ids_end;
            directories.push(DiskDirectory {
                kind,
                class,
                words_offset,
                word_count,
                object_ids_offset,
                logical_count: directory_logical_count,
            });
        }
        if expected_directory_data != object_offsets_offset || counted_logical != logical_count {
            return Err(ArchiveError::Invalid("backref logical directory count mismatch"));
        }
        if referenced.iter().any(|referenced| !referenced) {
            return Err(ArchiveError::Invalid("unreferenced backref bitmap object"));
        }
        let offset_at = |position: usize| -> crate::archive::Result<usize> {
            let start = object_offsets_offset + position * 8;
            usize::try_from(u64::from_le_bytes(
                bytes[start..start + 8].try_into().unwrap(),
            ))
            .map_err(|_| ArchiveError::FieldTooLarge)
        };
        if offset_at(0)? != payload_offset || offset_at(object_count)? != bytes.len() {
            return Err(ArchiveError::Invalid("invalid backref payload boundaries"));
        }
        let mut depths = Vec::with_capacity(object_count);
        let mut previous_payload_position = payload_offset;
        for position in 0..object_count {
            let offset = offset_at(position)?;
            let end = offset_at(position + 1)?;
            let base = bytes[base_offsets_offset + position] as usize;
            let expected_depth = if base == 0 {
                0
            } else if base <= position {
                depths[position - base] + 1
            } else {
                u8::MAX
            };
            if base > position
                || base > MAX_XOR_OFFSET
                || expected_depth > MAX_XOR_DEPTH
                || offset != previous_payload_position
                || end < offset
                || end > bytes.len()
            {
                return Err(ArchiveError::Invalid("invalid backref bitmap object"));
            }
            previous_payload_position = end;
            depths.push(expected_depth);
        }
        Ok(Self {
            bytes,
            capabilities,
            title_index_fingerprint,
            object_count,
            directories,
            object_offsets_offset,
            base_offsets_offset,
            payload_offset,
        })
    }

    pub fn open_for_title_index(
        path: impl AsRef<Path>,
        title_index: impl AsRef<Path>,
    ) -> crate::archive::Result<Self> {
        let index = Self::open(path)?;
        if index.title_index_fingerprint != file_xxh3_64(title_index.as_ref())? {
            return Err(ArchiveError::Invalid(
                "backref sidecar title-index fingerprint mismatch",
            ));
        }
        Ok(index)
    }

    pub(crate) fn has_raw_postings(&self) -> bool {
        self.capabilities & CAPABILITY_RAW_POSTINGS != 0
    }

    pub(crate) fn logical_count(&self) -> u64 {
        self.directories
            .iter()
            .map(|directory| directory.logical_count as u64)
            .sum()
    }

    pub fn members(&self, key: SetKey) -> crate::archive::Result<Vec<u64>> {
        let Some(position) = self.find_object(key)? else {
            return Ok(Vec::new());
        };
        let mut cache = BTreeMap::new();
        let bitmap = self.decode_entry(position, &mut cache)?;
        Ok(bitmap.members().collect())
    }

    /// Page IDs having at least one revision attributed to this stable local
    /// user ID. Account-log actions are deliberately not part of this set.
    pub fn pages_edited_by(&self, local_user_id: u64) -> crate::archive::Result<Vec<u64>> {
        if self.capabilities & CAPABILITY_USER_EDITS == 0 {
            return Err(ArchiveError::Invalid(
                "backref sidecar lacks required user-edit capability",
            ));
        }
        if local_user_id == 0 {
            return Ok(Vec::new());
        }
        self.members(SetKey {
            target_page_id: local_user_id,
            kind: EdgeKind::UserEdits,
            class: SetClass::DirectUnconditional,
        })
    }

    fn decode_entry(
        &self,
        position: usize,
        cache: &mut BTreeMap<usize, Bitmap>,
    ) -> crate::archive::Result<Bitmap> {
        let mut chain = Vec::new();
        let mut current = position;
        while !cache.contains_key(&current) {
            if chain.len() > MAX_XOR_DEPTH as usize {
                return Err(ArchiveError::Invalid("backref XOR chain is too deep"));
            }
            chain.push(current);
            let entry = self.object(current)?;
            if entry.base_offset == 0 {
                break;
            }
            let distance = entry.base_offset as usize;
            if distance > current || distance > MAX_XOR_OFFSET {
                return Err(ArchiveError::Invalid("invalid backref XOR base"));
            }
            current -= distance;
        }
        let mut bitmap = cache.get(&current).cloned();
        for entry_position in chain.into_iter().rev() {
            let entry = self.object(entry_position)?;
            let delta = decode_sidecar_bitmap(
                &self.bytes[entry.offset..entry.offset + entry.len],
            )
            .map_err(ArchiveError::Io)?;
            bitmap = Some(match bitmap {
                Some(base) if entry.base_offset != 0 => delta.difference(&base),
                _ => delta,
            });
            cache.insert(entry_position, bitmap.clone().unwrap());
        }
        bitmap.ok_or(ArchiveError::Invalid("empty backref XOR chain"))
    }

    fn find_object(&self, key: SetKey) -> crate::archive::Result<Option<usize>> {
        let Some(directory) = self
            .directories
            .iter()
            .find(|directory| directory.kind == key.kind && directory.class == key.class)
            .copied()
        else {
            return Ok(None);
        };
        let wanted_word = key.target_page_id / 64;
        let mut left = 0;
        let mut right = directory.word_count;
        while left < right {
            let middle = left + (right - left) / 2;
            let start = directory.words_offset + middle * PRESENCE_WORD_BYTES;
            let candidate =
                u64::from_le_bytes(self.bytes[start..start + 8].try_into().unwrap());
            match candidate.cmp(&wanted_word) {
                std::cmp::Ordering::Less => left = middle + 1,
                std::cmp::Ordering::Greater => right = middle,
                std::cmp::Ordering::Equal => {
                    let word = u64::from_le_bytes(
                        self.bytes[start + 8..start + 16].try_into().unwrap(),
                    );
                    let bit = 1_u64 << (key.target_page_id % 64);
                    if word & bit == 0 {
                        return Ok(None);
                    }
                    let rank = u32::from_le_bytes(
                        self.bytes[start + 16..start + 20].try_into().unwrap(),
                    ) as usize
                        + (word & (bit - 1)).count_ones() as usize;
                    if rank >= directory.logical_count {
                        return Err(ArchiveError::Invalid("invalid backref presence rank"));
                    }
                    let id_start = directory.object_ids_offset + rank * 4;
                    return Ok(Some(
                        u32::from_le_bytes(
                            self.bytes[id_start..id_start + 4].try_into().unwrap(),
                        ) as usize,
                    ));
                }
            }
        }
        Ok(None)
    }

    fn object(&self, position: usize) -> crate::archive::Result<BitmapObject> {
        if position >= self.object_count {
            return Err(ArchiveError::Invalid("backref bitmap object is out of bounds"));
        }
        let start = self.object_offsets_offset + position * 8;
        let offset = usize::try_from(u64::from_le_bytes(
            self.bytes[start..start + 8].try_into().unwrap(),
        ))
        .map_err(|_| ArchiveError::FieldTooLarge)?;
        let end = usize::try_from(u64::from_le_bytes(
            self.bytes[start + 8..start + 16].try_into().unwrap(),
        ))
        .map_err(|_| ArchiveError::FieldTooLarge)?;
        if offset < self.payload_offset
            || end < offset
            || end > self.bytes.len()
        {
            return Err(ArchiveError::Invalid("invalid backref payload bounds"));
        }
        Ok(BitmapObject {
            base_offset: self.bytes[self.base_offsets_offset + position],
            offset,
            len: end - offset,
        })
    }

    fn logical_sets(&self) -> LogicalSetCursor<'_> {
        LogicalSetCursor {
            index: self,
            authoritative_only: false,
            directory_position: 0,
            logical_position: 0,
            word_position: 0,
            word_index: 0,
            word_bits: 0,
        }
    }

    fn authoritative_logical_sets(&self) -> LogicalSetCursor<'_> {
        LogicalSetCursor {
            index: self,
            authoritative_only: true,
            directory_position: 0,
            logical_position: 0,
            word_position: 0,
            word_index: 0,
            word_bits: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn logical_sets_for_test(&self) -> crate::archive::Result<Vec<(SetKey, Vec<u64>)>> {
        let mut cursor = self.logical_sets();
        let mut sets = Vec::new();
        while let Some(set) = cursor.next()? {
            sets.push((set.key, set.members.members().collect()));
        }
        Ok(sets)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LogicalSetOrderKey {
    kind: EdgeKind,
    class: SetClass,
    target_page_id: u64,
}

impl LogicalSetOrderKey {
    fn from_set(key: SetKey) -> Self {
        Self {
            kind: key.kind,
            class: key.class,
            target_page_id: key.target_page_id,
        }
    }

    fn set_key(self) -> SetKey {
        SetKey {
            target_page_id: self.target_page_id,
            kind: self.kind,
            class: self.class,
        }
    }
}

/// The concrete projection used while an archive frame is merged.  Its
/// inputs are encoded target titles, not current page owners, so a title move
/// changes only the later title-index resolution and does not require a
/// second archive scan.
#[derive(Clone)]
pub(crate) struct BackrefFrameMergeProjectionFactory {
    site_info: crate::archive::SiteInfoRecord,
    namespaces: NamespaceMap,
    redirect_words: Vec<RedirectWord>,
}

impl BackrefFrameMergeProjectionFactory {
    pub(crate) fn new(site_info: &crate::archive::SiteInfoRecord) -> Self {
        Self {
            site_info: site_info.clone(),
            namespaces: namespace_map(site_info),
            redirect_words: redirect_words(site_info),
        }
    }

    fn new_state(&self) -> BackrefFrameProjectionState {
        BackrefFrameProjectionState {
            site_info: self.site_info.clone(),
            namespaces: self.namespaces.clone(),
            redirect_words: self.redirect_words.clone(),
            pages: PageAccumulator::default(),
            sets: BTreeMap::new(),
        }
    }

    /// Project an arbitrary record source and return its memberships against
    /// an empty base.  This is the candidate-only path for new pages that do
    /// not overlap an old frame.
    pub(crate) fn project_candidate_source(
        &self,
        source: &mut dyn crate::archive::RecordSource,
    ) -> crate::archive::Result<Vec<u8>> {
        let mut state = self.new_state();
        while let Some(record) = source.next_record()? {
            state.observe_record(&record)?;
        }
        let candidate = state.finish_bytes()?;
        FrameMergeProjectionFactory::combine(
            self,
            encode_projection_sets(BTreeMap::new())?,
            candidate,
        )
    }

    /// Iterator form of [`Self::project_candidate_source`].
    pub(crate) fn project_candidate_records<I>(
        &self,
        records: I,
    ) -> crate::archive::Result<Vec<u8>>
    where
        I: IntoIterator<Item = Record>,
    {
        let mut state = self.new_state();
        for record in records {
            state.observe_record(&record)?;
        }
        let candidate = state.finish_bytes()?;
        FrameMergeProjectionFactory::combine(
            self,
            encode_projection_sets(BTreeMap::new())?,
            candidate,
        )
    }

}

struct BackrefFrameProjectionState {
    site_info: crate::archive::SiteInfoRecord,
    namespaces: NamespaceMap,
    redirect_words: Vec<RedirectWord>,
    pages: PageAccumulator,
    sets: BTreeMap<LogicalSetOrderKey, Bitmap>,
}

impl BackrefFrameProjectionState {
    fn observe_record(&mut self, record: &Record) -> crate::archive::Result<()> {
        let site_info = &self.site_info;
        let namespaces = &self.namespaces;
        let redirect_words = &self.redirect_words;
        let sets = &mut self.sets;
        self.pages.observe(record, &mut |page_id, page| {
            emit_projection_page(
                page_id,
                page,
                site_info,
                namespaces,
                redirect_words,
                sets,
            )
        })
    }

    fn finish_bytes(mut self) -> crate::archive::Result<Vec<u8>> {
        let site_info = &self.site_info;
        let namespaces = &self.namespaces;
        let redirect_words = &self.redirect_words;
        let sets = &mut self.sets;
        self.pages.finish(&mut |page_id, page| {
            emit_projection_page(
                page_id,
                page,
                site_info,
                namespaces,
                redirect_words,
                sets,
            )
        })?;
        encode_projection_sets(self.sets)
    }
}

impl FrameMergeProjection for BackrefFrameProjectionState {
    fn observe(&mut self, record: &Record) -> crate::archive::Result<()> {
        self.observe_record(record)
    }

    fn finish(self: Box<Self>) -> crate::archive::Result<Vec<u8>> {
        self.finish_bytes()
    }
}

impl FrameMergeProjectionFactory for BackrefFrameMergeProjectionFactory {
    fn new_state(&self) -> crate::archive::Result<Box<dyn FrameMergeProjection>> {
        Ok(Box::new(self.new_state()))
    }

    fn combine(
        &self,
        base: Vec<u8>,
        candidate: Vec<u8>,
    ) -> crate::archive::Result<Vec<u8>> {
        xor_projection_streams(&base, &candidate).map_err(ArchiveError::Io)
    }
}

fn emit_projection_page(
    page_id: u64,
    page: &mut PageData,
    site_info: &crate::archive::SiteInfoRecord,
    namespaces: &NamespaceMap,
    redirect_words: &[RedirectWord],
    sets: &mut BTreeMap<LogicalSetOrderKey, Bitmap>,
) -> crate::archive::Result<()> {
    for user_id in &page.contributors {
        add_projection_membership(
            sets,
            EdgeKind::UserEdits,
            SetClass::DirectUnconditional,
            *user_id,
            page_id,
        );
    }
    extract_raw_page(
        page_id,
        page,
        site_info,
        namespaces,
        redirect_words,
        |edge| {
            add_projection_membership(
                sets,
                edge.kind,
                edge.class,
                edge.target,
                edge.source,
            );
        },
    )?;
    Ok(())
}

fn add_projection_membership(
    sets: &mut BTreeMap<LogicalSetOrderKey, Bitmap>,
    kind: EdgeKind,
    class: SetClass,
    target_page_id: u64,
    member_page_id: u64,
) {
    let key = LogicalSetOrderKey {
        kind,
        class,
        target_page_id,
    };
    sets.entry(key).or_default().insert(member_page_id);
}

fn encode_projection_sets(
    sets: BTreeMap<LogicalSetOrderKey, Bitmap>,
) -> crate::archive::Result<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    let mut encoder = ProjectionDeltaEncoder::new(&mut output).map_err(ArchiveError::Io)?;
    for (key, members) in sets {
        encoder
            .write_set(key, &members)
            .map_err(ArchiveError::Io)?;
    }
    encoder.finish().map_err(ArchiveError::Io)?;
    Ok(output.into_inner())
}

struct ProjectionDeltaEncoder<W> {
    output: W,
    set_count: u64,
    previous: Option<LogicalSetOrderKey>,
}

impl<W: Write + Seek> ProjectionDeltaEncoder<W> {
    fn new(mut output: W) -> std::io::Result<Self> {
        output.write_all(&PROJECTION_MAGIC)?;
        output.write_all(&PROJECTION_VERSION.to_le_bytes())?;
        output.write_all(&(PROJECTION_HEADER_BYTES as u16).to_le_bytes())?;
        output.write_all(&0_u64.to_le_bytes())?;
        output.write_all(&[0; 4])?;
        Ok(Self {
            output,
            set_count: 0,
            previous: None,
        })
    }

    fn write_set(&mut self, key: LogicalSetOrderKey, members: &Bitmap) -> std::io::Result<()> {
        if !valid_projection_key(key) {
            return Err(invalid_data("invalid frame projection kind/class combination"));
        }
        if members.words.is_empty() {
            return Err(invalid_data("empty sparse projection set"));
        }
        if self.previous.is_some_and(|previous| key <= previous) {
            return Err(invalid_data("projection sets are not strictly ordered"));
        }
        self.output.write_all(&[kind_byte(key.kind), class_byte(key.class)])?;
        self.output.write_all(&[0; 6])?;
        self.output.write_all(&key.target_page_id.to_le_bytes())?;
        let word_count = u64::try_from(members.words.len())
            .map_err(|_| invalid_data("projection bitmap word count overflows"))?;
        self.output.write_all(&word_count.to_le_bytes())?;
        let mut previous_word = None;
        for (word_index, word) in &members.words {
            if *word == 0 || previous_word.is_some_and(|previous| *word_index <= previous) {
                return Err(invalid_data("invalid sparse projection bitmap word"));
            }
            self.output.write_all(&word_index.to_le_bytes())?;
            self.output.write_all(&word.to_le_bytes())?;
            previous_word = Some(*word_index);
        }
        self.set_count = self
            .set_count
            .checked_add(1)
            .ok_or_else(|| invalid_data("projection set count overflows"))?;
        self.previous = Some(key);
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<W> {
        self.output.seek(SeekFrom::Start(12))?;
        self.output.write_all(&self.set_count.to_le_bytes())?;
        self.output.seek(SeekFrom::End(0))?;
        self.output.flush()?;
        Ok(self.output)
    }
}

struct ProjectionDeltaReader<R> {
    input: R,
    remaining: u64,
    previous: Option<LogicalSetOrderKey>,
    checked_eof: bool,
}

impl<R: Read> ProjectionDeltaReader<R> {
    fn new(mut input: R) -> std::io::Result<Self> {
        let remaining = read_projection_delta_header(&mut input)?;
        Ok(Self {
            input,
            remaining,
            previous: None,
            checked_eof: false,
        })
    }

    fn next_set(&mut self) -> std::io::Result<Option<ProjectionSet>> {
        if self.remaining == 0 {
            if !self.checked_eof {
                let mut trailing = [0_u8; 1];
                if self.input.read(&mut trailing)? != 0 {
                    return Err(invalid_data("trailing projection delta bytes"));
                }
                self.checked_eof = true;
            }
            return Ok(None);
        }
        let mut header = [0_u8; PROJECTION_SET_HEADER_BYTES];
        self.input.read_exact(&mut header)?;
        if header[2..8] != [0; 6] {
            return Err(invalid_data("non-zero projection set reserved bytes"));
        }
        let kind = parse_kind(header[0]).map_err(|_| invalid_data("invalid projection kind"))?;
        let class = parse_class(header[1])
            .map_err(|_| invalid_data("invalid projection set class"))?;
        let key = LogicalSetOrderKey {
            kind,
            class,
            target_page_id: u64::from_le_bytes(header[8..16].try_into().unwrap()),
        };
        if !valid_projection_key(key) || self.previous.is_some_and(|previous| key <= previous) {
            return Err(invalid_data("invalid or unsorted projection set"));
        }
        let word_count = usize::try_from(u64::from_le_bytes(header[16..24].try_into().unwrap()))
            .map_err(|_| invalid_data("projection bitmap word count is too large"))?;
        if word_count == 0 {
            return Err(invalid_data("empty projection bitmap set"));
        }
        let mut words = Vec::new();
        let mut previous_word = None;
        for _ in 0..word_count {
            let mut word = [0_u8; 16];
            self.input.read_exact(&mut word)?;
            let word_index = u64::from_le_bytes(word[..8].try_into().unwrap());
            let bits = u64::from_le_bytes(word[8..].try_into().unwrap());
            if bits == 0 || previous_word.is_some_and(|previous| word_index <= previous) {
                return Err(invalid_data("invalid sparse projection bitmap word"));
            }
            words.push((word_index, bits));
            previous_word = Some(word_index);
        }
        self.remaining -= 1;
        self.previous = Some(key);
        Ok(Some(ProjectionSet {
            key,
            members: Bitmap { words },
        }))
    }
}

fn read_projection_delta_header(input: &mut impl Read) -> std::io::Result<u64> {
    let mut header = [0_u8; PROJECTION_HEADER_BYTES];
    input.read_exact(&mut header)?;
    if header[..8] != PROJECTION_MAGIC {
        return Err(invalid_data("bad projection delta magic"));
    }
    if u16::from_le_bytes(header[8..10].try_into().unwrap()) != PROJECTION_VERSION {
        return Err(invalid_data("unsupported projection delta version"));
    }
    if u16::from_le_bytes(header[10..12].try_into().unwrap())
        != PROJECTION_HEADER_BYTES as u16
    {
        return Err(invalid_data("invalid projection delta header length"));
    }
    if header[20..24] != [0; 4] {
        return Err(invalid_data("non-zero projection delta reserved bytes"));
    }
    Ok(u64::from_le_bytes(header[12..20].try_into().unwrap()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectionSet {
    key: LogicalSetOrderKey,
    members: Bitmap,
}

fn valid_projection_key(key: LogicalSetOrderKey) -> bool {
    match (key.kind, key.class) {
        (EdgeKind::UserEdits, SetClass::DirectUnconditional) => true,
        (EdgeKind::Redirect, SetClass::RawRedirectTarget) => true,
        (
            EdgeKind::Template | EdgeKind::Module | EdgeKind::Category | EdgeKind::File,
            class,
        ) => is_raw_class(class) && valid_kind_class(key.kind, class),
        _ => false,
    }
}

struct ProjectionHead {
    key: LogicalSetOrderKey,
    stream: usize,
    set: ProjectionSet,
}

impl Ord for ProjectionHead {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.stream.cmp(&other.stream))
    }
}

impl PartialOrd for ProjectionHead {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ProjectionHead {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.stream == other.stream
    }
}

impl Eq for ProjectionHead {}

struct ProjectionDeltaMerger<R> {
    readers: Vec<ProjectionDeltaReader<R>>,
    heads: BinaryHeap<Reverse<ProjectionHead>>,
}

impl<R: Read> ProjectionDeltaMerger<R> {
    fn new(mut readers: Vec<ProjectionDeltaReader<R>>) -> std::io::Result<Self> {
        let mut heads = BinaryHeap::new();
        for (stream, reader) in readers.iter_mut().enumerate() {
            if let Some(set) = reader.next_set()? {
                heads.push(Reverse(ProjectionHead {
                    key: set.key,
                    stream,
                    set,
                }));
            }
        }
        Ok(Self { readers, heads })
    }

    fn refill(&mut self, stream: usize) -> std::io::Result<()> {
        if let Some(set) = self.readers[stream].next_set()? {
            self.heads.push(Reverse(ProjectionHead {
                key: set.key,
                stream,
                set,
            }));
        }
        Ok(())
    }

    fn next_set(&mut self) -> std::io::Result<Option<ProjectionSet>> {
        loop {
            let Some(Reverse(first)) = self.heads.pop() else {
                return Ok(None);
            };
            let key = first.key;
            let mut members = first.set.members;
            self.refill(first.stream)?;
            while self.heads.peek().is_some_and(|head| head.0.key == key) {
                let Reverse(next) = self.heads.pop().expect("peeked projection head");
                members.words = merge_words(&members.words, &next.set.members.words, |left, right| {
                    left ^ right
                });
                self.refill(next.stream)?;
            }
            if !members.words.is_empty() {
                return Ok(Some(ProjectionSet { key, members }));
            }
        }
    }
}

fn xor_projection_streams(left: &[u8], right: &[u8]) -> std::io::Result<Vec<u8>> {
    let readers = vec![
        ProjectionDeltaReader::new(Cursor::new(left))?,
        ProjectionDeltaReader::new(Cursor::new(right))?,
    ];
    let mut merger = ProjectionDeltaMerger::new(readers)?;
    let mut output = Cursor::new(Vec::new());
    let mut encoder = ProjectionDeltaEncoder::new(&mut output)?;
    while let Some(set) = merger.next_set()? {
        encoder.write_set(set.key, &set.members)?;
    }
    encoder.finish()?;
    Ok(output.into_inner())
}

/// In-memory range accumulator for already-ordered frame projection streams.
/// Each completed frame is XORed into the map immediately; equal words cancel
/// and empty sets are removed.  It has no application-defined spill threshold.
pub(crate) struct ProjectionDeltaAccumulator {
    sets: BTreeMap<LogicalSetOrderKey, Bitmap>,
}

impl ProjectionDeltaAccumulator {
    pub(crate) fn new() -> Self {
        Self {
            sets: BTreeMap::new(),
        }
    }

    pub(crate) fn absorb(&mut self, bytes: &[u8]) -> crate::archive::Result<()> {
        let mut reader = ProjectionDeltaReader::new(Cursor::new(bytes)).map_err(ArchiveError::Io)?;
        while let Some(set) = reader.next_set().map_err(ArchiveError::Io)? {
            let entry = self.sets.entry(set.key).or_default();
            entry.words = merge_words(&entry.words, &set.members.words, |left, right| left ^ right);
            if entry.words.is_empty() {
                self.sets.remove(&set.key);
            }
        }
        Ok(())
    }

    pub(crate) fn write_to(&self, output: &Path) -> crate::archive::Result<u64> {
        let file = std::fs::File::create(output)?;
        let mut encoder = ProjectionDeltaEncoder::new(file).map_err(ArchiveError::Io)?;
        for (key, members) in &self.sets {
            encoder.write_set(*key, members).map_err(ArchiveError::Io)?;
        }
        let file = encoder.finish().map_err(ArchiveError::Io)?;
        file.sync_all()?;
        u64::try_from(self.sets.len())
            .map_err(|_| ArchiveError::FieldTooLarge)
    }
}

struct LogicalSetCursor<'a> {
    index: &'a BackrefIndex,
    authoritative_only: bool,
    directory_position: usize,
    logical_position: usize,
    word_position: usize,
    word_index: u64,
    word_bits: u64,
}

impl LogicalSetCursor<'_> {
    fn next(&mut self) -> crate::archive::Result<Option<LogicalSet>> {
        loop {
            let Some(directory) = self.index.directories.get(self.directory_position) else {
                return Ok(None);
            };
            if self.authoritative_only
                && !is_authoritative_rewrite_kind_class(directory.kind, directory.class)
            {
                self.directory_position += 1;
                self.logical_position = 0;
                self.word_position = 0;
                self.word_bits = 0;
                continue;
            }
            if self.logical_position >= directory.logical_count {
                self.directory_position += 1;
                self.logical_position = 0;
                self.word_position = 0;
                self.word_bits = 0;
                continue;
            }
            if self.word_bits == 0 {
                if self.word_position >= directory.word_count {
                    return Err(ArchiveError::Invalid(
                        "backref logical cursor ran past presence bitmap",
                    ));
                }
                let start = directory.words_offset + self.word_position * PRESENCE_WORD_BYTES;
                self.word_index = u64::from_le_bytes(
                    self.index.bytes[start..start + 8].try_into().unwrap(),
                );
                self.word_bits = u64::from_le_bytes(
                    self.index.bytes[start + 8..start + 16]
                        .try_into()
                        .unwrap(),
                );
                self.word_position += 1;
            }
            let bit = self.word_bits.trailing_zeros() as u64;
            self.word_bits &= self.word_bits - 1;
            let target_page_id = self
                .word_index
                .checked_mul(64)
                .and_then(|word| word.checked_add(bit))
                .ok_or(ArchiveError::FieldTooLarge)?;
            let object_start = directory.object_ids_offset + self.logical_position * 4;
            let object_id = u32::from_le_bytes(
                self.index.bytes[object_start..object_start + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            self.logical_position += 1;
            #[cfg(test)]
            if self.authoritative_only {
                BACKREF_REWRITE_LOGICAL_SET_DECODES.fetch_add(1, Ordering::SeqCst);
                if !is_authoritative_rewrite_kind_class(directory.kind, directory.class) {
                    BACKREF_REWRITE_DERIVED_SET_DECODES.fetch_add(1, Ordering::SeqCst);
                }
            }
            let members = self
                .index
                .decode_entry(object_id, &mut BTreeMap::new())?;
            return Ok(Some(LogicalSet {
                key: SetKey {
                    target_page_id,
                    kind: directory.kind,
                    class: directory.class,
                },
                members,
                topology_bases: Vec::new(),
            }));
        }
    }
}

fn is_authoritative_rewrite_kind_class(kind: EdgeKind, class: SetClass) -> bool {
    is_raw_class(class)
        || (kind == EdgeKind::UserEdits && class == SetClass::DirectUnconditional)
}

fn store_authoritative_rewrite_set(
    key: SetKey,
    members: &Bitmap,
    raw: &mut RawDiskSets,
    user_edits: &mut impl Write,
) -> crate::archive::Result<()> {
    if members.words.is_empty() {
        return Ok(());
    }
    if is_raw_class(key.class) {
        if !valid_kind_class(key.kind, key.class) {
            return Err(ArchiveError::Invalid(
                "invalid raw set kind/class during backref rewrite",
            ));
        }
        raw.put((key.kind, key.class, key.target_page_id), members)?;
        return Ok(());
    }
    if key.kind != EdgeKind::UserEdits || key.class != SetClass::DirectUnconditional {
        return Err(ArchiveError::Invalid(
            "non-authoritative set during backref rewrite",
        ));
    }
    for member_page_id in members.members() {
        write_user_page(user_edits, key.target_page_id, member_page_id)?;
    }
    Ok(())
}

/// Rewrites an existing SWREFOBJ by applying the ordered XOR bitmap streams in
/// `delta_paths` to authoritative postings and rebuilding the derived public
/// sets.  The merger keeps one current sparse bitmap per input stream and one
/// current authoritative set; it never materializes all changed memberships.
/// Only raw posting classes and UserEdits/DirectUnconditional are authoritative
/// inputs.  Old direct and transitive public directories are skipped before
/// bitmap payload decode and derived again from the resulting raw graph.
/// The old sidecar is checked against `old_title_index` before any decoding;
/// the staged result is checked against `new_title_index` before publication.
/// The supplied fingerprint is written to the staged header and therefore
/// must match the supplied new title-index path.
pub(crate) fn rewrite_backref_sidecar_with_deltas(
    old_sidecar: impl AsRef<Path>,
    old_title_index: impl AsRef<Path>,
    delta_paths: &[std::path::PathBuf],
    output: impl AsRef<Path>,
    new_title_index: impl AsRef<Path>,
    new_title_index_fingerprint: u64,
) -> crate::archive::Result<()> {
    let old_title_index = old_title_index.as_ref();
    let new_title_index = new_title_index.as_ref();
    let old = BackrefIndex::open_for_title_index(old_sidecar, old_title_index)?;
    if old.capabilities & CAPABILITY_RAW_POSTINGS == 0 {
        return Err(ArchiveError::Invalid(
            "incremental backref rewrite requires raw postings",
        ));
    }
    if file_xxh3_64(new_title_index)? != new_title_index_fingerprint {
        return Err(ArchiveError::Invalid(
            "new title-index fingerprint does not match supplied value",
        ));
    }
    let new_titles = crate::title_index::TitleIndex::open(new_title_index)?;
    let output = output.as_ref();
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let scratch = tempfile::tempdir_in(parent)?;
    let mut updated_raw = RawDiskSets::new_in(scratch.path())?;
    let user_spool = tempfile::tempfile_in(scratch.path())?;
    let mut user_output = BufWriter::new(user_spool);
    let mut old_sets = old.authoritative_logical_sets();
    let mut old_set = old_sets.next()?;
    let readers = delta_paths
        .iter()
        .map(|path| {
            let file = std::fs::File::open(path)?;
            ProjectionDeltaReader::new(BufReader::new(file))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut deltas = ProjectionDeltaMerger::new(readers).map_err(ArchiveError::Io)?;
    let mut delta_group = deltas.next_set().map_err(ArchiveError::Io)?;

    loop {
        match (old_set.take(), delta_group.take()) {
            (None, None) => break,
            (Some(set), None) => {
                store_authoritative_rewrite_set(
                    set.key,
                    &set.members,
                    &mut updated_raw,
                    &mut user_output,
                )?;
                old_set = old_sets.next()?;
            }
            (None, Some(ProjectionSet { key, members })) => {
                store_authoritative_rewrite_set(
                    key.set_key(),
                    &members,
                    &mut updated_raw,
                    &mut user_output,
                )?;
                delta_group = deltas.next_set().map_err(ArchiveError::Io)?;
            }
            (Some(set), Some(ProjectionSet { key, members })) => {
                match LogicalSetOrderKey::from_set(set.key).cmp(&key) {
                    std::cmp::Ordering::Less => {
                        store_authoritative_rewrite_set(
                            set.key,
                            &set.members,
                            &mut updated_raw,
                            &mut user_output,
                        )?;
                        old_set = old_sets.next()?;
                        delta_group = Some(ProjectionSet { key, members });
                    }
                    std::cmp::Ordering::Equal => {
                        let members = set.members.difference(&members);
                        store_authoritative_rewrite_set(
                            set.key,
                            &members,
                            &mut updated_raw,
                            &mut user_output,
                        )?;
                        old_set = old_sets.next()?;
                        delta_group = deltas.next_set().map_err(ArchiveError::Io)?;
                    }
                    std::cmp::Ordering::Greater => {
                        store_authoritative_rewrite_set(
                            key.set_key(),
                            &members,
                            &mut updated_raw,
                            &mut user_output,
                        )?;
                        old_set = Some(set);
                        delta_group = deltas.next_set().map_err(ArchiveError::Io)?;
                    }
                }
            }
        }
    }

    user_output.flush()?;
    let user_edits = user_output
        .into_inner()
        .map_err(|error| ArchiveError::Io(error.into_error()))?;
    user_edits.sync_all()?;
    let (collected, _redirects, _missing_targets, updated_raw) =
        derive_public_edges_from_raw(updated_raw, &new_titles, scratch.path())?;
    let mut encoder = StreamingEncoder::new_in_with_capabilities(scratch.path(), old.capabilities)?;
    add_streaming_sidecar_sets(
        &mut encoder,
        collected.direct,
        collected.topology_seeds,
        collected.graph,
        collected.effects,
        user_edits,
        Some(updated_raw),
        scratch.path(),
    )?;
    let staged = encoder.write_staged(parent, new_title_index_fingerprint)?;
    BackrefIndex::open_for_title_index(staged.path(), new_title_index)?;
    staged
        .persist(output)
        .map_err(|error| ArchiveError::Io(error.error))?;
    set_readable_permissions(output)?;
    Ok(())
}

pub(crate) fn title_index_fingerprint(path: impl AsRef<Path>) -> crate::archive::Result<u64> {
    Ok(file_xxh3_64(path.as_ref())?)
}

pub(crate) fn projection_delta_records(bytes: &[u8]) -> crate::archive::Result<u64> {
    let mut reader = ProjectionDeltaReader::new(Cursor::new(bytes)).map_err(ArchiveError::Io)?;
    let mut count = 0_u64;
    while reader.next_set().map_err(ArchiveError::Io)?.is_some() {
        count = count.checked_add(1).ok_or(ArchiveError::FieldTooLarge)?;
    }
    Ok(count)
}

pub(crate) fn projection_delta_file_records(
    path: impl AsRef<Path>,
) -> crate::archive::Result<u64> {
    let file = std::fs::File::open(path)?;
    let mut reader = ProjectionDeltaReader::new(BufReader::new(file)).map_err(ArchiveError::Io)?;
    let mut count = 0_u64;
    while reader.next_set().map_err(ArchiveError::Io)?.is_some() {
        count = count.checked_add(1).ok_or(ArchiveError::FieldTooLarge)?;
    }
    Ok(count)
}

pub(crate) fn projection_delta_file_declared_sets(
    path: impl AsRef<Path>,
) -> crate::archive::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    read_projection_delta_header(&mut file).map_err(ArchiveError::Io)
}

/// Combine ordered sparse bitmap streams with a k-way merge. Equal set keys
/// are XORed wordwise; equal memberships therefore cancel by parity without a
/// global set or membership collection.
#[cfg(test)]
pub(crate) fn combine_projection_deltas(
    deltas: &[Vec<u8>],
) -> crate::archive::Result<Vec<u8>> {
    let readers = deltas
        .iter()
        .map(|delta| ProjectionDeltaReader::new(Cursor::new(delta.as_slice())))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(ArchiveError::Io)?;
    let mut merger = ProjectionDeltaMerger::new(readers).map_err(ArchiveError::Io)?;
    let mut output = Cursor::new(Vec::new());
    let mut encoder = ProjectionDeltaEncoder::new(&mut output).map_err(ArchiveError::Io)?;
    while let Some(set) = merger.next_set().map_err(ArchiveError::Io)? {
        encoder
            .write_set(set.key, &set.members)
            .map_err(ArchiveError::Io)?;
    }
    encoder.finish().map_err(ArchiveError::Io)?;
    Ok(output.into_inner())
}

fn kind_byte(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Template => 1,
        EdgeKind::Module => 2,
        EdgeKind::Category => 3,
        EdgeKind::File => 4,
        EdgeKind::UserEdits => 5,
        EdgeKind::Redirect => 6,
    }
}

fn certainty_byte(certainty: Certainty) -> u8 {
    match certainty {
        Certainty::Definite => 1,
        Certainty::Possible => 2,
    }
}

fn parse_kind(value: u8) -> crate::archive::Result<EdgeKind> {
    match value {
        1 => Ok(EdgeKind::Template),
        2 => Ok(EdgeKind::Module),
        3 => Ok(EdgeKind::Category),
        4 => Ok(EdgeKind::File),
        5 => Ok(EdgeKind::UserEdits),
        6 => Ok(EdgeKind::Redirect),
        _ => Err(ArchiveError::Invalid("unknown backref edge kind")),
    }
}

fn class_byte(class: SetClass) -> u8 {
    match class {
        SetClass::DirectUnconditional => 1,
        SetClass::DirectPossible => 2,
        SetClass::TransitiveUnconditional => 3,
        SetClass::TransitivePossible => 4,
        SetClass::RawNonTopologyUnconditional => 5,
        SetClass::RawNonTopologyPossible => 6,
        SetClass::RawTopologyUnconditional => 7,
        SetClass::RawTopologyPossible => 8,
        SetClass::RawEmittedUnconditional => 9,
        SetClass::RawEmittedPossible => 10,
        SetClass::RawRedirectTarget => 11,
    }
}

fn parse_class(value: u8) -> crate::archive::Result<SetClass> {
    match value {
        1 => Ok(SetClass::DirectUnconditional),
        2 => Ok(SetClass::DirectPossible),
        3 => Ok(SetClass::TransitiveUnconditional),
        4 => Ok(SetClass::TransitivePossible),
        5 => Ok(SetClass::RawNonTopologyUnconditional),
        6 => Ok(SetClass::RawNonTopologyPossible),
        7 => Ok(SetClass::RawTopologyUnconditional),
        8 => Ok(SetClass::RawTopologyPossible),
        9 => Ok(SetClass::RawEmittedUnconditional),
        10 => Ok(SetClass::RawEmittedPossible),
        11 => Ok(SetClass::RawRedirectTarget),
        _ => Err(ArchiveError::Invalid("unknown backref set class")),
    }
}

fn put_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push(value as u8 | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn take_varint(bytes: &mut &[u8]) -> std::io::Result<u64> {
    let mut value = 0_u64;
    for (position, shift) in (0..=63).step_by(7).enumerate() {
        let byte = take_byte(bytes)?;
        if shift == 63 && byte & 0x7e != 0 {
            return Err(invalid_data("varint overflow"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if position != 0 && byte == 0 {
                return Err(invalid_data("noncanonical varint"));
            }
            return Ok(value);
        }
    }
    Err(invalid_data("overlong varint"))
}

fn take_byte(bytes: &mut &[u8]) -> std::io::Result<u8> {
    let Some((byte, rest)) = bytes.split_first() else {
        return Err(invalid_data("truncated sidecar"));
    };
    *bytes = rest;
    Ok(*byte)
}

fn take_array<const N: usize>(bytes: &mut &[u8]) -> std::io::Result<[u8; N]> {
    if bytes.len() < N {
        return Err(invalid_data("truncated sidecar"));
    }
    let value = bytes[..N].try_into().unwrap();
    *bytes = &bytes[N..];
    Ok(value)
}

fn invalid_data(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BackrefMembershipToggle {
    kind: EdgeKind,
    class: SetClass,
    target_page_id: u64,
    member_page_id: u64,
}

#[cfg(test)]
impl BackrefMembershipToggle {
    fn new(
        kind: EdgeKind,
        class: SetClass,
        target_page_id: u64,
        member_page_id: u64,
    ) -> Self {
        Self {
            kind,
            class,
            target_page_id,
            member_page_id,
        }
    }
}

#[cfg(test)]
fn encode_test_toggles(toggles: impl IntoIterator<Item = BackrefMembershipToggle>)
    -> std::io::Result<Vec<u8>>
{
    let mut sets = BTreeMap::<LogicalSetOrderKey, Bitmap>::new();
    for toggle in toggles {
        let key = LogicalSetOrderKey {
            kind: toggle.kind,
            class: toggle.class,
            target_page_id: toggle.target_page_id,
        };
        if !valid_projection_key(key) {
            return Err(invalid_data("invalid test projection key"));
        }
        let bitmap = sets.entry(key).or_default();
        let word = toggle.member_page_id / 64;
        match bitmap.words.binary_search_by_key(&word, |entry| entry.0) {
            Ok(position) => {
                bitmap.words[position].1 ^= 1_u64 << (toggle.member_page_id % 64);
                if bitmap.words[position].1 == 0 {
                    bitmap.words.remove(position);
                }
            }
            Err(position) => bitmap
                .words
                .insert(position, (word, 1_u64 << (toggle.member_page_id % 64))),
        }
    }
    encode_projection_sets(sets).map_err(|error| match error {
        ArchiveError::Io(error) => error,
        other => std::io::Error::other(other.to_string()),
    })
}

#[cfg(test)]
impl BackrefFrameMergeProjectionFactory {
    fn decode_projection_toggles(
        &self,
        bytes: &[u8],
    ) -> crate::archive::Result<Vec<BackrefMembershipToggle>> {
        let _ = self;
        decode_projection_toggles_for_test(bytes)
    }

    fn append_projection_toggles(
        bytes: &[u8],
        output: &mut Vec<BackrefMembershipToggle>,
    ) -> crate::archive::Result<()> {
        output.extend(decode_projection_toggles_for_test(bytes)?);
        Ok(())
    }
}

#[cfg(test)]
fn decode_projection_toggles_for_test(
    bytes: &[u8],
) -> crate::archive::Result<Vec<BackrefMembershipToggle>> {
    let mut reader = ProjectionDeltaReader::new(Cursor::new(bytes)).map_err(ArchiveError::Io)?;
    let mut output = Vec::new();
    while let Some(set) = reader.next_set().map_err(ArchiveError::Io)? {
        for member in set.members.members() {
            output.push(BackrefMembershipToggle::new(
                set.key.kind,
                set.key.class,
                set.key.target_page_id,
                member,
            ));
        }
    }
    Ok(output)
}

#[cfg(test)]
fn rewrite_backref_sidecar_with_toggles(
    old_sidecar: impl AsRef<Path>,
    old_title_index: impl AsRef<Path>,
    toggles: impl IntoIterator<Item = BackrefMembershipToggle>,
    output: impl AsRef<Path>,
    new_title_index: impl AsRef<Path>,
    new_title_index_fingerprint: u64,
) -> crate::archive::Result<()> {
    let output = output.as_ref();
    let parent = output.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let temporary = tempfile::tempdir_in(parent)?;
    let delta = temporary.path().join("test.swrefdelta");
    std::fs::write(&delta, encode_test_toggles(toggles).map_err(ArchiveError::Io)?)?;
    rewrite_backref_sidecar_with_deltas(
        old_sidecar,
        old_title_index,
        &[delta],
        output,
        new_title_index,
        new_title_index_fingerprint,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn disk_bitmap_stores_append_after_reads() {
        let mut first = Bitmap::default();
        first.insert(11);
        let mut second = Bitmap::default();
        second.insert(29);

        let mut sets = DiskSets::new().unwrap();
        let first_key = (EdgeKind::Template, 1, Certainty::Definite);
        let second_key = (EdgeKind::Template, 2, Certainty::Definite);
        sets.put(first_key, &first).unwrap();
        assert_eq!(sets.get(&first_key).unwrap(), Some(first.clone()));
        sets.put(second_key, &second).unwrap();
        assert_eq!(sets.get(&first_key).unwrap(), Some(first.clone()));
        assert_eq!(sets.get(&second_key).unwrap(), Some(second.clone()));

        let mut bitmaps = BitmapStore::new().unwrap();
        bitmaps.put(1, &first).unwrap();
        assert_eq!(bitmaps.get(1).unwrap(), Some(first.clone()));
        bitmaps.put(2, &second).unwrap();
        assert_eq!(bitmaps.get(1).unwrap(), Some(first));
        assert_eq!(bitmaps.get(2).unwrap(), Some(second));
    }

    fn page(page_id: u64, title: &str, text: &str) -> SourcePage {
        SourcePage {
            page_id,
            title: title.into(),
            text: Some(text.into()),
        }
    }

    fn revision(page_id: u64, revision_id: u64, at: i64, text: &str) -> Record {
        Record::Revision {
            page_id,
            revision: crate::archive::RevisionRecord {
                meta: crate::RevisionMeta {
                    rev_id: revision_id,
                    parent_id: 0,
                    ts: Utc.timestamp_opt(at, 0).unwrap(),
                    contributor: crate::ContributorMeta::Anonymous {
                        ip: "192.0.2.1".into(),
                    },
                    comment: String::new(),
                    sha1: String::new(),
                    flags: 0,
                    text_len: text.len() as u64,
                },
                has_text: true,
                text: text.as_bytes().to_vec(),
                visibility: None,
                history: None,
            },
        }
    }

    fn named_revision(
        page_id: u64,
        revision_id: u64,
        at: i64,
        text: &str,
        user_id: u64,
    ) -> Record {
        let mut record = revision(page_id, revision_id, at, text);
        let Record::Revision { revision, .. } = &mut record else {
            unreachable!()
        };
        revision.meta.contributor = crate::ContributorMeta::Named {
            username: format!("User {user_id}"),
            user_id,
        };
        record
    }

    fn projection_site_info() -> crate::archive::SiteInfoRecord {
        crate::archive::SiteInfoRecord {
            site_name: "Projection test".into(),
            db_name: "projectionwiki".into(),
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
                    id: 6,
                    case: "first-letter".into(),
                    localized_name: "File".into(),
                    aliases: vec!["Image".into()],
                },
                crate::archive::SiteNamespaceRecord {
                    id: 10,
                    case: "first-letter".into(),
                    localized_name: "Template".into(),
                    aliases: vec!["T".into()],
                },
                crate::archive::SiteNamespaceRecord {
                    id: 14,
                    case: "first-letter".into(),
                    localized_name: "Category".into(),
                    aliases: Vec::new(),
                },
                crate::archive::SiteNamespaceRecord {
                    id: 828,
                    case: "first-letter".into(),
                    localized_name: "Module".into(),
                    aliases: Vec::new(),
                },
            ],
            interwiki: Vec::new(),
            magic_words: vec![crate::archive::SiteMagicWordRecord {
                canonical_name: "redirect".into(),
                aliases: vec!["#REDIRECT".into()],
                case_sensitive: false,
            }],
        }
    }

    fn projection_page_state(
        page_id: u64,
        title: &str,
        namespace: Option<i64>,
        deleted: bool,
    ) -> Record {
        Record::PageState {
            page_id,
            timestamp_micros: 2_000_000,
            title: title.into(),
            namespace,
            deleted,
        }
    }

    fn projection_page_action(
        page_id: u64,
        timestamp_micros: i64,
        kind: crate::archive::PageActionKind,
        title: &str,
        namespace: Option<i64>,
        resulting_deleted: Option<bool>,
    ) -> Record {
        Record::PageAction {
            entity: crate::archive::EntityKey {
                kind: crate::archive::EntityKind::Page,
                id: page_id,
            },
            timestamp_micros,
            action: crate::archive::PageActionRecord {
                log_id: Some(timestamp_micros as u64),
                tie_sequence: 0,
                kind,
                performer: crate::archive::PerformerRecord {
                    local_user_id: None,
                    central_user_id: None,
                    historical_name: None,
                    account_class: crate::archive::AccountClass::Unknown,
                },
                comment: String::new(),
                title_at_event: title.into(),
                namespace_at_event: namespace,
                resulting_deleted,
            },
        }
    }

    fn projection_memberships(
        factory: &BackrefFrameMergeProjectionFactory,
        records: &[Record],
    ) -> Vec<BackrefMembershipToggle> {
        let bytes = factory.project_candidate_records(records.to_vec()).unwrap();
        factory.decode_projection_toggles(&bytes).unwrap()
    }

    fn projection_delta(
        factory: &BackrefFrameMergeProjectionFactory,
        old: &[Record],
        candidate: &[Record],
    ) -> Vec<BackrefMembershipToggle> {
        let old = factory.project_candidate_records(old.to_vec()).unwrap();
        let candidate = factory.project_candidate_records(candidate.to_vec()).unwrap();
        let delta = FrameMergeProjectionFactory::combine(factory, old, candidate).unwrap();
        factory.decode_projection_toggles(&delta).unwrap()
    }

    fn projection_code(site_info: &crate::archive::SiteInfoRecord, title: &str) -> u64 {
        let namespaces = namespace_map(site_info);
        crate::title_index::coded_title(
            &namespaces.normalize_title_for_site(title),
            site_info,
        )
    }

    struct ProjectionRecordSource {
        records: std::vec::IntoIter<Record>,
    }

    impl crate::archive::RecordSource for ProjectionRecordSource {
        fn next_record(&mut self) -> crate::archive::Result<Option<Record>> {
            Ok(self.records.next())
        }
    }

    fn resolver(pages: &[SourcePage]) -> impl FnMut(&str) -> Option<u64> + '_ {
        move |title| {
            let title = title.replace('_', " ");
            pages
                .iter()
                .find(|page| page.title.eq_ignore_ascii_case(&title))
                .map(|page| page.page_id)
        }
    }

    fn set(sets: &[(SetKey, Vec<u64>)], key: SetKey) -> Vec<u64> {
        sets.iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, members)| members.clone())
            .unwrap_or_default()
    }

    fn stamp_sidecar_title_index(path: &Path, title_index: &Path) {
        let mut bytes = std::fs::read(path).unwrap();
        bytes[72..80]
            .copy_from_slice(&file_xxh3_64(title_index).unwrap().to_le_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    fn build_test_title_index(
        title_index: &Path,
        site_info: &crate::archive::SiteInfoRecord,
        pages: &[(u64, &str, i64)],
    ) {
        let archive = title_index.with_extension("swdump");
        let mut writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 4096)
                .unwrap();
        for (page_id, title, namespace) in pages {
            writer
                .write(&Record::PageState {
                    page_id: *page_id,
                    timestamp_micros: 2_000_000,
                    title: (*title).into(),
                    namespace: Some(*namespace),
                    deleted: false,
                })
                .unwrap();
        }
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 2_000_000,
                site_info: site_info.clone(),
            })
            .unwrap();
        writer.finish().unwrap();
        crate::title_index::build(
            &archive,
            title_index,
            &crate::generation::GenerationId::from_plan_bytes(b"backrefs-rewrite-title-fixture"),
        )
        .unwrap();
    }

    fn collect_logical_sets(index: &BackrefIndex) -> Vec<(SetKey, Vec<u64>)> {
        let mut cursor = index.logical_sets();
        let mut sets = Vec::new();
        while let Some(set) = cursor.next().unwrap() {
            sets.push((set.key, set.members.members().collect()));
        }
        sets
    }

    #[test]
    fn frame_projection_identical_stream_has_no_toggles() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let records = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "{{T:Old}}", 7),
        ];
        assert!(projection_delta(&factory, &records, &records).is_empty());
    }

    #[test]
    fn frame_projection_changed_latest_text_is_an_exact_raw_xor() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let old = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "{{T:Old}}", 7),
        ];
        let candidate = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "{{T:New}}", 7),
        ];
        let mut expected = vec![
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawTopologyUnconditional,
                projection_code(&site_info, "T:Old"),
                1,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawTopologyUnconditional,
                projection_code(&site_info, "T:New"),
                1,
            ),
        ];
        expected.sort_unstable();
        assert_eq!(projection_delta(&factory, &old, &candidate), expected);
    }

    #[test]
    fn frame_projection_later_action_only_delete_removes_live_page_relations() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let state = projection_page_state(1, "Article", None, false);
        let revision = named_revision(1, 10, 1, "{{T:Old}}", 7);
        let old = vec![state.clone(), revision.clone()];
        let candidate = vec![
            projection_page_action(
                1,
                3_000_000,
                crate::archive::PageActionKind::Delete,
                "Article",
                Some(0),
                None,
            ),
            state,
            revision,
        ];
        assert_eq!(
            projection_delta(&factory, &old, &candidate),
            vec![BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawTopologyUnconditional,
                projection_code(&site_info, "T:Old"),
                1,
            )]
        );
    }

    #[test]
    fn frame_projection_later_restore_reestablishes_deleted_page_relations() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let state = projection_page_state(1, "Article", None, true);
        let revision = named_revision(1, 10, 1, "{{T:Restored}}", 7);
        let old = vec![state.clone(), revision.clone()];
        let candidate = vec![
            projection_page_action(
                1,
                3_000_000,
                crate::archive::PageActionKind::Restore,
                "Article",
                Some(0),
                Some(false),
            ),
            state,
            revision,
        ];
        assert_eq!(
            projection_delta(&factory, &old, &candidate),
            vec![BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawTopologyUnconditional,
                projection_code(&site_info, "T:Restored"),
                1,
            )]
        );
    }

    #[test]
    fn frame_projection_later_actions_apply_in_chronological_order() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let state = projection_page_state(1, "Article", None, false);
        let revision = named_revision(1, 10, 1, "{{T:Live}}", 7);
        let old = vec![state.clone(), revision.clone()];
        let candidate = vec![
            projection_page_action(
                1,
                4_000_000,
                crate::archive::PageActionKind::Restore,
                "Article",
                Some(0),
                Some(false),
            ),
            projection_page_action(
                1,
                3_000_000,
                crate::archive::PageActionKind::Delete,
                "Article",
                Some(0),
                None,
            ),
            state,
            revision,
        ];
        assert!(projection_delta(&factory, &old, &candidate).is_empty());
    }

    #[test]
    fn frame_projection_later_move_changes_source_namespace_class() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let state = projection_page_state(1, "Article", None, false);
        let revision = named_revision(1, 10, 1, "[[Category:C]]", 7);
        let old = vec![state.clone(), revision.clone()];
        let candidate = vec![
            projection_page_action(
                1,
                3_000_000,
                crate::archive::PageActionKind::Move,
                "Article",
                Some(14),
                Some(false),
            ),
            state,
            revision,
        ];
        let target = projection_code(&site_info, "Category:C");
        let mut expected = vec![
            BackrefMembershipToggle::new(
                EdgeKind::Category,
                SetClass::RawNonTopologyUnconditional,
                target,
                1,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Category,
                SetClass::RawTopologyUnconditional,
                target,
                1,
            ),
        ];
        expected.sort_unstable();
        assert_eq!(projection_delta(&factory, &old, &candidate), expected);
    }

    #[test]
    fn frame_projection_redirect_replacement_is_an_exact_xor() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let old = vec![
            projection_page_state(1, "Old", None, false),
            named_revision(1, 10, 1, "#REDIRECT [[T:Old]]", 7),
        ];
        let candidate = vec![
            projection_page_state(1, "Old", None, false),
            named_revision(1, 10, 1, "#REDIRECT [[T:New]]", 7),
        ];
        let mut expected = vec![
            BackrefMembershipToggle::new(
                EdgeKind::Redirect,
                SetClass::RawRedirectTarget,
                projection_code(&site_info, "T:Old"),
                1,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Redirect,
                SetClass::RawRedirectTarget,
                projection_code(&site_info, "T:New"),
                1,
            ),
        ];
        expected.sort_unstable();
        assert_eq!(projection_delta(&factory, &old, &candidate), expected);
    }

    #[test]
    fn frame_projection_page_deletion_removes_current_relations_but_not_users() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let old = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "{{T:Old}}", 7),
        ];
        let candidate = vec![
            projection_page_state(1, "Article", None, true),
            named_revision(1, 10, 1, "{{T:Old}}", 7),
        ];
        let expected = vec![BackrefMembershipToggle::new(
            EdgeKind::Template,
            SetClass::RawTopologyUnconditional,
            projection_code(&site_info, "T:Old"),
            1,
        )];
        assert_eq!(projection_delta(&factory, &old, &candidate), expected);
    }

    #[test]
    fn frame_projection_source_namespace_change_changes_relation_class() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let old = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "[[Category:C]]", 7),
        ];
        let candidate = vec![
            projection_page_state(1, "Article", Some(14), false),
            named_revision(1, 10, 1, "[[Category:C]]", 7),
        ];
        let target = projection_code(&site_info, "Category:C");
        let mut expected = vec![
            BackrefMembershipToggle::new(
                EdgeKind::Category,
                SetClass::RawNonTopologyUnconditional,
                target,
                1,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Category,
                SetClass::RawTopologyUnconditional,
                target,
                1,
            ),
        ];
        expected.sort_unstable();
        assert_eq!(projection_delta(&factory, &old, &candidate), expected);
    }

    #[test]
    fn frame_projection_contributor_replacement_tracks_all_retained_named_users() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let old = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "", 7),
            named_revision(1, 9, 0, "", 8),
        ];
        let candidate = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "", 9),
            named_revision(1, 9, 0, "", 8),
        ];
        let mut expected = vec![
            BackrefMembershipToggle::new(
                EdgeKind::UserEdits,
                SetClass::DirectUnconditional,
                7,
                1,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::UserEdits,
                SetClass::DirectUnconditional,
                9,
                1,
            ),
        ];
        expected.sort_unstable();
        assert_eq!(projection_delta(&factory, &old, &candidate), expected);
    }

    #[test]
    fn frame_projection_candidate_only_uses_the_same_state_and_codec() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let records = vec![
            projection_page_state(2, "Article", None, false),
            named_revision(2, 10, 1, "{{T:New}}", 9),
        ];
        let memberships = projection_memberships(&factory, &records);
        let mut source = ProjectionRecordSource {
            records: records.clone().into_iter(),
        };
        let source_memberships = factory
            .project_candidate_source(&mut source)
            .and_then(|bytes| factory.decode_projection_toggles(&bytes))
            .unwrap();
        assert_eq!(source_memberships, memberships);
        assert_eq!(
            memberships,
            vec![
                BackrefMembershipToggle::new(
                    EdgeKind::Template,
                    SetClass::RawTopologyUnconditional,
                    projection_code(&site_info, "T:New"),
                    2,
                ),
                BackrefMembershipToggle::new(
                    EdgeKind::UserEdits,
                    SetClass::DirectUnconditional,
                    9,
                    2,
                ),
            ]
        );
    }

    #[test]
    fn frame_projection_matches_canonical_archive_merge() {
        let temporary = tempfile::tempdir().unwrap();
        let base_path = temporary.path().join("base.swdump");
        let update_path = temporary.path().join("update.swdump");
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let base_records = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "{{T:Old}}", 7),
        ];
        let update_records = vec![named_revision(1, 10, 1, "{{T:New}}", 7)];
        let candidate_records = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "{{T:New}}", 7),
        ];
        for (path, records) in [(&base_path, &base_records), (&update_path, &update_records)] {
            let mut writer = crate::archive::ArchiveWriter::new(
                std::fs::File::create(path).unwrap(),
                4096,
            )
            .unwrap();
            for record in records {
                writer.write(record).unwrap();
            }
            writer.finish().unwrap();
        }
        let mut merged_records = Vec::new();
        crate::archive::visit_merged_record_sources(
            vec![
                Box::new(crate::archive::ArchiveRecordReader::open(&base_path).unwrap()),
                Box::new(crate::archive::ArchiveRecordReader::open(&update_path).unwrap()),
            ],
            |record| merged_records.push(record.clone()),
        )
        .unwrap();
        assert_eq!(
            projection_delta(&factory, &base_records, &merged_records),
            projection_delta(&factory, &base_records, &candidate_records),
        );
    }

    #[test]
    fn frame_projection_codec_rejects_malformed_bytes_atomically() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let records = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "{{T:New}}", 9),
        ];
        let valid = factory.project_candidate_records(records).unwrap();
        let mut malformed = vec![
            valid.clone(),
            valid.clone(),
            valid.clone(),
            valid.clone(),
            valid.clone(),
            valid.clone(),
        ];
        malformed[0][0] ^= 1;
        malformed[1][8] = 99;
        malformed[2][20] = 1;
        let truncated_len = malformed[3].len() - 1;
        malformed[3].truncate(truncated_len);
        malformed[4][2] = 1;
        malformed[5][26] = kind_byte(EdgeKind::UserEdits);
        for bytes in malformed {
            assert!(factory.decode_projection_toggles(&bytes).is_err());
            let mut output = vec![BackrefMembershipToggle::new(
                EdgeKind::UserEdits,
                SetClass::DirectUnconditional,
                99,
                99,
            )];
            assert!(
                BackrefFrameMergeProjectionFactory::append_projection_toggles(&bytes, &mut output)
                    .is_err()
            );
            assert_eq!(output.len(), 1);
        }
    }

    #[test]
    fn projection_repeated_reference_is_one_set_membership() {
        let site_info = projection_site_info();
        let factory = BackrefFrameMergeProjectionFactory::new(&site_info);
        let records = vec![
            projection_page_state(1, "Article", None, false),
            named_revision(1, 10, 1, "{{T:New}} {{T:New}}", 9),
        ];
        assert_eq!(
            projection_memberships(&factory, &records),
            vec![
                BackrefMembershipToggle::new(
                    EdgeKind::Template,
                    SetClass::RawTopologyUnconditional,
                    projection_code(&site_info, "T:New"),
                    1,
                ),
                BackrefMembershipToggle::new(
                    EdgeKind::UserEdits,
                    SetClass::DirectUnconditional,
                    9,
                    1,
                ),
            ]
        );
    }

    #[test]
    fn projection_stream_merge_matches_independent_set_parity_oracle() {
        let a = |member_page_id| {
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawTopologyUnconditional,
                100,
                member_page_id,
            )
        };
        let b = |member_page_id| {
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawNonTopologyUnconditional,
                101,
                member_page_id,
            )
        };
        let user = |target_page_id, member_page_id| {
            BackrefMembershipToggle::new(
                EdgeKind::UserEdits,
                SetClass::DirectUnconditional,
                target_page_id,
                member_page_id,
            )
        };
        let streams = vec![
            vec![a(1), a(1), a(64), a(1 << 40), b(10)],
            vec![a(64), a(1 << 40), a(65), b(10), b(11), user(7, 1)],
            vec![a(65), b(11), b(12), user(7, 1), user(8, u64::MAX - 1)],
        ];
        let mut expected = BTreeMap::<LogicalSetOrderKey, BTreeSet<u64>>::new();
        for stream in &streams {
            for toggle in stream {
                let key = LogicalSetOrderKey {
                    kind: toggle.kind,
                    class: toggle.class,
                    target_page_id: toggle.target_page_id,
                };
                let members = expected.entry(key).or_default();
                if !members.insert(toggle.member_page_id) {
                    members.remove(&toggle.member_page_id);
                }
                if members.is_empty() {
                    expected.remove(&key);
                }
            }
        }
        let encoded = streams
            .iter()
            .map(|stream| encode_test_toggles(stream.iter().copied()).unwrap())
            .collect::<Vec<_>>();
        let merged = combine_projection_deltas(&encoded).unwrap();
        let actual = decode_projection_toggles_for_test(&merged).unwrap();
        let mut actual_sets = BTreeMap::<LogicalSetOrderKey, BTreeSet<u64>>::new();
        for toggle in actual {
            actual_sets
                .entry(LogicalSetOrderKey {
                    kind: toggle.kind,
                    class: toggle.class,
                    target_page_id: toggle.target_page_id,
                })
                .or_default()
                .insert(toggle.member_page_id);
        }
        assert_eq!(actual_sets, expected);
        assert_eq!(projection_delta_records(&merged).unwrap(), 2);
        assert_eq!(actual_sets.values().flatten().count(), 2);
        assert!(actual_sets
            .values()
            .any(|members| members.contains(&(u64::MAX - 1))));
    }

    #[test]
    fn projection_range_accumulator_writes_one_canonical_delta_after_frame_xors() {
        let streams = [
            encode_test_toggles([
                BackrefMembershipToggle::new(
                    EdgeKind::Template,
                    SetClass::RawTopologyUnconditional,
                    100,
                    64,
                ),
                BackrefMembershipToggle::new(
                    EdgeKind::Template,
                    SetClass::RawNonTopologyUnconditional,
                    101,
                    12,
                ),
            ])
            .unwrap(),
            encode_test_toggles([
                BackrefMembershipToggle::new(
                    EdgeKind::Template,
                    SetClass::RawTopologyUnconditional,
                    100,
                    64,
                ),
                BackrefMembershipToggle::new(
                    EdgeKind::UserEdits,
                    SetClass::DirectUnconditional,
                    8,
                    u64::MAX - 1,
                ),
            ])
            .unwrap(),
        ];
        let mut accumulator = ProjectionDeltaAccumulator::new();
        for stream in &streams {
            accumulator.absorb(stream).unwrap();
        }
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("range.swrefdelta");
        assert_eq!(accumulator.write_to(&output).unwrap(), 2);
        assert_eq!(projection_delta_file_records(&output).unwrap(), 2);
        let actual = decode_projection_toggles_for_test(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(
            actual,
            vec![
                BackrefMembershipToggle::new(
                    EdgeKind::Template,
                    SetClass::RawNonTopologyUnconditional,
                    101,
                    12,
                ),
                BackrefMembershipToggle::new(
                    EdgeKind::UserEdits,
                    SetClass::DirectUnconditional,
                    8,
                    u64::MAX - 1,
                ),
            ]
        );
    }

    #[test]
    fn projection_delta_reader_rejects_old_and_noncanonical_bitmap_encodings() {
        let valid = encode_test_toggles([
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawTopologyUnconditional,
                100,
                0,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawTopologyUnconditional,
                100,
                64,
            ),
        ])
        .unwrap();
        let mut old_magic = valid.clone();
        old_magic[..8].copy_from_slice(b"SWPRJ001");
        assert!(decode_projection_toggles_for_test(&old_magic).is_err());

        let mut empty_set = valid.clone();
        empty_set[40..48].fill(0);
        assert!(decode_projection_toggles_for_test(&empty_set).is_err());

        let mut huge_truncated = valid.clone();
        huge_truncated[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        huge_truncated.truncate(PROJECTION_HEADER_BYTES + PROJECTION_SET_HEADER_BYTES);
        assert!(decode_projection_toggles_for_test(&huge_truncated).is_err());

        let mut zero_word = valid.clone();
        zero_word[56..64].fill(0);
        assert!(decode_projection_toggles_for_test(&zero_word).is_err());

        let mut unsorted_words = valid;
        unsorted_words[48..56].copy_from_slice(&1_u64.to_le_bytes());
        unsorted_words[64..72].copy_from_slice(&0_u64.to_le_bytes());
        assert!(decode_projection_toggles_for_test(&unsorted_words).is_err());
    }

    #[test]
    fn membership_toggle_rewrite_differentially_preserves_and_changes_sets() {
        let _rewrite_guard = BACKREF_REWRITE_TEST_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let site_info = projection_site_info();
        let old_title_index = temporary.path().join("old.swtitle");
        let new_title_index = temporary.path().join("new.swtitle");
        let old_sidecar = temporary.path().join("old.swrefs");
        let new_sidecar = temporary.path().join("new.swrefs");
        build_test_title_index(&old_title_index, &site_info, &[(10, "Target", 10)]);
        build_test_title_index(&new_title_index, &site_info, &[(10, "Target", 10)]);
        let target = projection_code(&site_info, "T:Target");
        let stale_direct = SetKey {
            target_page_id: 10,
            kind: EdgeKind::Template,
            class: SetClass::DirectUnconditional,
        };
        let stale_transitive = SetKey {
            target_page_id: 10,
            kind: EdgeKind::Template,
            class: SetClass::TransitiveUnconditional,
        };
        let raw_non_topology = SetKey {
            target_page_id: target,
            kind: EdgeKind::Template,
            class: SetClass::RawNonTopologyUnconditional,
        };
        let raw_topology = SetKey {
            target_page_id: target,
            kind: EdgeKind::Template,
            class: SetClass::RawTopologyUnconditional,
        };
        let user_edits = SetKey {
            target_page_id: 7,
            kind: EdgeKind::UserEdits,
            class: SetClass::DirectUnconditional,
        };
        let capabilities = REQUIRED_CAPABILITIES | CAPABILITY_RAW_POSTINGS;
        write_sidecar_with_capabilities(
            &old_sidecar,
            &[
                (stale_direct, vec![999]),
                (stale_transitive, vec![998]),
                (raw_non_topology, vec![1, 4]),
                (raw_topology, vec![2]),
                (user_edits, vec![6]),
            ],
            capabilities,
        )
        .unwrap();
        stamp_sidecar_title_index(&old_sidecar, &old_title_index);

        let mut toggles = vec![
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawNonTopologyUnconditional,
                target,
                1,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawNonTopologyUnconditional,
                target,
                8,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawNonTopologyUnconditional,
                target,
                4,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawNonTopologyUnconditional,
                target,
                4,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawTopologyUnconditional,
                target,
                2,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::UserEdits,
                SetClass::DirectUnconditional,
                7,
                8,
            ),
        ];
        toggles.sort();

        let decodes_before = BACKREF_REWRITE_LOGICAL_SET_DECODES.load(Ordering::SeqCst);
        let derived_decodes_before =
            BACKREF_REWRITE_DERIVED_SET_DECODES.load(Ordering::SeqCst);
        rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &old_title_index,
            toggles,
            &new_sidecar,
            &new_title_index,
            file_xxh3_64(&new_title_index).unwrap(),
        )
        .unwrap();
        assert_eq!(
            BACKREF_REWRITE_LOGICAL_SET_DECODES.load(Ordering::SeqCst) - decodes_before,
            3,
            "rewrite must decode each authoritative old set exactly once"
        );
        assert_eq!(
            BACKREF_REWRITE_DERIVED_SET_DECODES.load(Ordering::SeqCst)
                - derived_decodes_before,
            0,
            "rewrite must skip derived/public directories before payload decode"
        );
        let rewritten = BackrefIndex::open_for_title_index(&new_sidecar, &new_title_index).unwrap();
        assert_eq!(rewritten.capabilities, capabilities);
        assert_eq!(rewritten.members(raw_non_topology).unwrap(), vec![4, 8]);
        assert!(rewritten.members(raw_topology).unwrap().is_empty());
        assert_eq!(rewritten.members(user_edits).unwrap(), vec![6, 8]);
        assert_eq!(
            rewritten
                .members(SetKey {
                    target_page_id: 10,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                })
                .unwrap(),
            vec![4, 8]
        );
        assert_eq!(rewritten.members(stale_direct).unwrap(), vec![4, 8]);
        assert!(!rewritten.members(stale_direct).unwrap().contains(&999));
        assert!(rewritten.members(stale_transitive).unwrap().is_empty());
    }

    #[test]
    fn rewrite_resolves_unchanged_raw_target_against_new_title_owner() {
        let _rewrite_guard = BACKREF_REWRITE_TEST_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let site_info = projection_site_info();
        let old_title_index = temporary.path().join("old.swtitle");
        let new_title_index = temporary.path().join("new.swtitle");
        let old_sidecar = temporary.path().join("old.swrefs");
        let new_sidecar = temporary.path().join("new.swrefs");
        build_test_title_index(&old_title_index, &site_info, &[(10, "Target", 10)]);
        build_test_title_index(&new_title_index, &site_info, &[(20, "Target", 10)]);
        let target = projection_code(&site_info, "T:Target");
        let raw = SetKey {
            target_page_id: target,
            kind: EdgeKind::Template,
            class: SetClass::RawNonTopologyUnconditional,
        };
        let stale = SetKey {
            target_page_id: 10,
            kind: EdgeKind::Template,
            class: SetClass::DirectUnconditional,
        };
        write_sidecar_with_capabilities(
            &old_sidecar,
            &[(stale, vec![1]), (raw, vec![1])],
            REQUIRED_CAPABILITIES | CAPABILITY_RAW_POSTINGS,
        )
        .unwrap();
        stamp_sidecar_title_index(&old_sidecar, &old_title_index);

        rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &old_title_index,
            Vec::<BackrefMembershipToggle>::new(),
            &new_sidecar,
            &new_title_index,
            file_xxh3_64(&new_title_index).unwrap(),
        )
        .unwrap();
        let rewritten = BackrefIndex::open_for_title_index(&new_sidecar, &new_title_index).unwrap();
        assert_eq!(rewritten.members(raw).unwrap(), vec![1]);
        assert_eq!(rewritten.members(stale).unwrap(), Vec::<u64>::new());
        assert_eq!(
            rewritten
                .members(SetKey {
                    target_page_id: 20,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                })
                .unwrap(),
            vec![1]
        );
    }

    #[test]
    fn rewrite_redirect_toggle_rebuilds_direct_and_transitive_public_sets() {
        let _rewrite_guard = BACKREF_REWRITE_TEST_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let site_info = projection_site_info();
        let old_title_index = temporary.path().join("old.swtitle");
        let new_title_index = temporary.path().join("new.swtitle");
        let old_sidecar = temporary.path().join("old.swrefs");
        let new_sidecar = temporary.path().join("new.swrefs");
        let pages = [
            (2, "Alias", 10),
            (10, "Old", 10),
            (20, "New", 10),
            (30, "Wrapper", 10),
        ];
        build_test_title_index(&old_title_index, &site_info, &pages);
        build_test_title_index(&new_title_index, &site_info, &pages);
        let alias = projection_code(&site_info, "T:Alias");
        let old_target = projection_code(&site_info, "T:Old");
        let new_target = projection_code(&site_info, "T:New");
        let wrapper = projection_code(&site_info, "T:Wrapper");
        let alias_relation = SetKey {
            target_page_id: alias,
            kind: EdgeKind::Template,
            class: SetClass::RawTopologyUnconditional,
        };
        let wrapper_relation = SetKey {
            target_page_id: wrapper,
            kind: EdgeKind::Template,
            class: SetClass::RawTopologyUnconditional,
        };
        let redirect_old = SetKey {
            target_page_id: old_target,
            kind: EdgeKind::Redirect,
            class: SetClass::RawRedirectTarget,
        };
        let stale_direct = SetKey {
            target_page_id: 10,
            kind: EdgeKind::Template,
            class: SetClass::DirectUnconditional,
        };
        let stale_transitive = SetKey {
            target_page_id: 10,
            kind: EdgeKind::Template,
            class: SetClass::TransitiveUnconditional,
        };
        write_sidecar_with_capabilities(
            &old_sidecar,
            &[
                (stale_direct, vec![30]),
                (stale_transitive, vec![1]),
                (alias_relation, vec![30]),
                (wrapper_relation, vec![1]),
                (redirect_old, vec![2]),
            ],
            REQUIRED_CAPABILITIES | CAPABILITY_RAW_POSTINGS,
        )
        .unwrap();
        stamp_sidecar_title_index(&old_sidecar, &old_title_index);

        let mut toggles = vec![
            BackrefMembershipToggle::new(
                EdgeKind::Redirect,
                SetClass::RawRedirectTarget,
                old_target,
                2,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Redirect,
                SetClass::RawRedirectTarget,
                new_target,
                2,
            ),
        ];
        toggles.sort_unstable();
        rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &old_title_index,
            toggles,
            &new_sidecar,
            &new_title_index,
            file_xxh3_64(&new_title_index).unwrap(),
        )
        .unwrap();
        let rewritten = BackrefIndex::open_for_title_index(&new_sidecar, &new_title_index).unwrap();
        assert!(rewritten.members(redirect_old).unwrap().is_empty());
        assert_eq!(
            rewritten
                .members(SetKey {
                    target_page_id: new_target,
                    kind: EdgeKind::Redirect,
                    class: SetClass::RawRedirectTarget,
                })
                .unwrap(),
            vec![2]
        );
        assert_eq!(
            rewritten
                .members(SetKey {
                    target_page_id: 20,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                })
                .unwrap(),
            vec![30]
        );
        assert_eq!(
            rewritten
                .members(SetKey {
                    target_page_id: 20,
                    kind: EdgeKind::Template,
                    class: SetClass::TransitiveUnconditional,
                })
                .unwrap(),
            vec![1, 30]
        );
        assert!(rewritten.members(stale_direct).unwrap().is_empty());
        assert!(rewritten.members(stale_transitive).unwrap().is_empty());
    }

    #[test]
    fn membership_toggle_rewrite_validates_titles_and_toggle_order() {
        let _rewrite_guard = BACKREF_REWRITE_TEST_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let site_info = projection_site_info();
        let old_title_index = temporary.path().join("old.swtitle");
        let new_title_index = temporary.path().join("new.swtitle");
        let wrong_title_index = temporary.path().join("wrong.swtitle");
        let old_sidecar = temporary.path().join("old.swrefs");
        let new_sidecar = temporary.path().join("new.swrefs");
        build_test_title_index(&old_title_index, &site_info, &[(1, "Target", 10)]);
        build_test_title_index(&new_title_index, &site_info, &[(1, "Target", 10)]);
        build_test_title_index(&wrong_title_index, &site_info, &[(2, "Other", 10)]);
        let target = projection_code(&site_info, "T:Target");
        let raw = SetKey {
            target_page_id: target,
            kind: EdgeKind::Template,
            class: SetClass::RawNonTopologyUnconditional,
        };
        write_sidecar_with_capabilities(
            &old_sidecar,
            &[(raw, vec![3])],
            REQUIRED_CAPABILITIES | CAPABILITY_RAW_POSTINGS,
        )
        .unwrap();
        stamp_sidecar_title_index(&old_sidecar, &old_title_index);

        assert!(rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &wrong_title_index,
            Vec::<BackrefMembershipToggle>::new(),
            &new_sidecar,
            &new_title_index,
            file_xxh3_64(&new_title_index).unwrap(),
        )
        .is_err());
        assert!(!new_sidecar.exists());

        assert!(rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &old_title_index,
            Vec::<BackrefMembershipToggle>::new(),
            &new_sidecar,
            &new_title_index,
            file_xxh3_64(&wrong_title_index).unwrap(),
        )
        .is_err());
        assert!(!new_sidecar.exists());

        let invalid_class = BackrefMembershipToggle::new(
            EdgeKind::Template,
            SetClass::DirectUnconditional,
            target,
            3,
        );
        assert!(rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &old_title_index,
            [invalid_class],
            &new_sidecar,
            &new_title_index,
            file_xxh3_64(&new_title_index).unwrap(),
        )
        .is_err());
        assert!(!new_sidecar.exists());

        let first = BackrefMembershipToggle::new(
            EdgeKind::Template,
            SetClass::RawNonTopologyUnconditional,
            target + 1,
            3,
        );
        let second = BackrefMembershipToggle::new(
            EdgeKind::Template,
            SetClass::RawNonTopologyUnconditional,
            target,
            3,
        );
        assert!(rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &old_title_index,
            [first, second],
            &new_sidecar,
            &new_title_index,
            file_xxh3_64(&new_title_index).unwrap(),
        )
        .is_err());
        assert!(!new_sidecar.exists());
    }

    #[test]
    fn raw_capability_and_classes_survive_zero_and_changed_toggle_rewrites() {
        let _rewrite_guard = BACKREF_REWRITE_TEST_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let site_info = projection_site_info();
        let old_title_index = temporary.path().join("old.swtitle");
        let new_title_index = temporary.path().join("new.swtitle");
        let old_sidecar = temporary.path().join("old.swrefs");
        let zero_sidecar = temporary.path().join("zero.swrefs");
        let changed_sidecar = temporary.path().join("changed.swrefs");
        build_test_title_index(&old_title_index, &site_info, &[]);
        build_test_title_index(&new_title_index, &site_info, &[]);
        let logical = vec![
            (
                SetKey {
                    target_page_id: 101,
                    kind: EdgeKind::Template,
                    class: SetClass::RawNonTopologyUnconditional,
                },
                vec![1, 2],
            ),
            (
                SetKey {
                    target_page_id: 102,
                    kind: EdgeKind::Module,
                    class: SetClass::RawNonTopologyPossible,
                },
                vec![3],
            ),
            (
                SetKey {
                    target_page_id: 103,
                    kind: EdgeKind::Category,
                    class: SetClass::RawTopologyUnconditional,
                },
                vec![4],
            ),
            (
                SetKey {
                    target_page_id: 104,
                    kind: EdgeKind::File,
                    class: SetClass::RawTopologyPossible,
                },
                vec![5],
            ),
            (
                SetKey {
                    target_page_id: 105,
                    kind: EdgeKind::Category,
                    class: SetClass::RawEmittedUnconditional,
                },
                vec![6],
            ),
            (
                SetKey {
                    target_page_id: 106,
                    kind: EdgeKind::File,
                    class: SetClass::RawEmittedPossible,
                },
                vec![7],
            ),
            (
                SetKey {
                    target_page_id: 107,
                    kind: EdgeKind::Redirect,
                    class: SetClass::RawRedirectTarget,
                },
                vec![8],
            ),
        ];
        let capabilities = REQUIRED_CAPABILITIES | CAPABILITY_RAW_POSTINGS;
        write_sidecar_with_capabilities(&old_sidecar, &logical, capabilities).unwrap();
        stamp_sidecar_title_index(&old_sidecar, &old_title_index);

        rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &old_title_index,
            Vec::<BackrefMembershipToggle>::new(),
            &zero_sidecar,
            &new_title_index,
            file_xxh3_64(&new_title_index).unwrap(),
        )
        .unwrap();
        let zero = BackrefIndex::open_for_title_index(&zero_sidecar, &new_title_index).unwrap();
        assert_eq!(zero.capabilities, capabilities);
        let mut expected = logical.clone();
        expected.sort_by_key(|(key, _)| (key.kind, key.class, key.target_page_id));
        assert_eq!(collect_logical_sets(&zero), expected);

        let mut toggles = vec![
            BackrefMembershipToggle::new(
                EdgeKind::Template,
                SetClass::RawNonTopologyUnconditional,
                101,
                1,
            ),
            BackrefMembershipToggle::new(
                EdgeKind::Redirect,
                SetClass::RawRedirectTarget,
                107,
                9,
            ),
        ];
        toggles.sort();
        rewrite_backref_sidecar_with_toggles(
            &old_sidecar,
            &old_title_index,
            toggles,
            &changed_sidecar,
            &new_title_index,
            file_xxh3_64(&new_title_index).unwrap(),
        )
        .unwrap();
        let changed =
            BackrefIndex::open_for_title_index(&changed_sidecar, &new_title_index).unwrap();
        assert_eq!(changed.capabilities, capabilities);
        assert_eq!(
            changed
                .members(SetKey {
                    target_page_id: 101,
                    kind: EdgeKind::Template,
                    class: SetClass::RawNonTopologyUnconditional,
                })
                .unwrap(),
            vec![2],
        );
        assert_eq!(
            changed
                .members(SetKey {
                    target_page_id: 107,
                    kind: EdgeKind::Redirect,
                    class: SetClass::RawRedirectTarget,
                })
                .unwrap(),
            vec![8, 9],
        );
        for class in [
            SetClass::RawNonTopologyUnconditional,
            SetClass::RawNonTopologyPossible,
            SetClass::RawTopologyUnconditional,
            SetClass::RawTopologyPossible,
            SetClass::RawEmittedUnconditional,
            SetClass::RawEmittedPossible,
            SetClass::RawRedirectTarget,
        ] {
            assert!(changed
                .directories
                .iter()
                .any(|directory| directory.class == class));
        }
    }

    #[test]
    fn malformed_kind_class_combinations_are_rejected() {
        let invalid = [
            (EdgeKind::Redirect, SetClass::DirectUnconditional),
            (EdgeKind::Redirect, SetClass::RawTopologyUnconditional),
            (EdgeKind::Template, SetClass::RawRedirectTarget),
            (EdgeKind::UserEdits, SetClass::RawNonTopologyPossible),
        ];
        for (kind, class) in invalid {
            let temporary = tempfile::tempdir().unwrap();
            let sidecar = temporary.path().join("invalid.swrefs");
            assert!(write_sidecar_with_capabilities(
                &sidecar,
                &[(
                    SetKey {
                        target_page_id: 1,
                        kind,
                        class,
                    },
                    vec![2],
                )],
                REQUIRED_CAPABILITIES | CAPABILITY_RAW_POSTINGS,
            )
            .is_err());
            assert!(!sidecar.exists());
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[
            kind_byte(EdgeKind::Redirect),
            class_byte(SetClass::RawNonTopologyUnconditional),
        ]);
        bytes.extend_from_slice(&[0; 22]);
        assert!(read_raw_edge(&mut bytes.as_slice()).is_err());

        let temporary = tempfile::tempdir().unwrap();
        let sidecar = temporary.path().join("mutated.swrefs");
        write_sidecar(
            &sidecar,
            &[(
                SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                },
                vec![2],
            )],
        )
        .unwrap();
        let mut bytes = std::fs::read(&sidecar).unwrap();
        bytes[HEADER_BYTES] = kind_byte(EdgeKind::Redirect);
        std::fs::write(&sidecar, bytes).unwrap();
        assert!(BackrefIndex::open(&sidecar).is_err());
    }

    #[test]
    fn template_containment_runs_from_dependency_to_all_callers() {
        let pages = vec![
            page(1, "Template:Leaf", "leaf"),
            page(2, "Template:Middle", "{{Leaf}}"),
            page(3, "Article", "{{Middle}}"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Template,
                    class: SetClass::TransitiveUnconditional,
                },
            ),
            vec![2, 3],
        );
    }

    #[test]
    fn module_dependencies_propagate_through_template_callers() {
        let pages = vec![
            page(1, "Module:Leaf", ""),
            page(2, "Template:Middle", "{{#invoke:Leaf|main}}"),
            page(3, "Article", "{{Middle}}"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Module,
                    class: SetClass::TransitiveUnconditional,
                },
            ),
            vec![2, 3],
        );
    }

    #[test]
    fn one_page_can_have_template_and_module_relation_identities() {
        let pages = vec![
            page(1, "Module:M", ""),
            page(2, "Article", "{{Module:M}}{{#invoke:M|main}}"),
        ];
        let sets = build_sets(&pages, |title| match title {
            "Template:Module:M" | "Module:M" => Some(1),
            "Article" => Some(2),
            _ => None,
        });
        for kind in [EdgeKind::Template, EdgeKind::Module] {
            assert_eq!(
                set(
                    &sets,
                    SetKey {
                        target_page_id: 1,
                        kind,
                        class: SetClass::TransitiveUnconditional,
                    },
                ),
                vec![2],
            );
        }
    }

    #[test]
    fn cycles_are_collapsed_before_transitive_propagation() {
        let pages = vec![
            page(1, "Template:A", "{{B}}"),
            page(2, "Template:B", "{{A}}"),
            page(3, "Article", "{{A}}"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        let a = set(
            &sets,
            SetKey {
                target_page_id: 1,
                kind: EdgeKind::Template,
                class: SetClass::TransitiveUnconditional,
            },
        );
        assert!(a.contains(&2));
        assert!(a.contains(&3));
    }

    #[test]
    fn unknown_conditional_branches_are_possible_not_unconditional() {
        let pages = vec![
            page(1, "Template:A", ""),
            page(2, "Template:B", ""),
            page(3, "Article", "{{#if:{{unknown}}|{{A}}|{{B}}}}"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        for target in [1, 2] {
            assert!(set(
                &sets,
                SetKey {
                    target_page_id: target,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                },
            )
            .is_empty());
            assert_eq!(
                set(
                    &sets,
                    SetKey {
                        target_page_id: target,
                        kind: EdgeKind::Template,
                        class: SetClass::DirectPossible,
                    },
                ),
                vec![3],
            );
        }
    }

    #[test]
    fn effective_category_membership_does_not_expand_subcategories() {
        let pages = vec![
            page(1, "Category:Parent", ""),
            page(2, "Category:Child", "[[Category:Parent]]"),
            page(3, "Article", "[[Category:Child]]"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Category,
                    class: SetClass::TransitiveUnconditional,
                },
            ),
            vec![2],
        );
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 2,
                    kind: EdgeKind::Category,
                    class: SetClass::TransitiveUnconditional,
                },
            ),
            vec![3],
        );
    }

    #[test]
    fn target_redirects_are_followed() {
        let pages = vec![
            page(1, "Template:Old", "#REDIRECT [[Template:New]]"),
            page(2, "Template:New", ""),
            page(3, "Article", "{{Old}}"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 2,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                },
            ),
            vec![3],
        );
    }

    #[test]
    fn category_redirect_target_is_not_itself_a_membership() {
        let pages = vec![
            page(1, "Category:Old", "#REDIRECT [[Category:New]]"),
            page(2, "Category:New", ""),
            page(3, "Article", "[[Category:Old]]"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 2,
                    kind: EdgeKind::Category,
                    class: SetClass::DirectUnconditional,
                },
            ),
            vec![3],
        );
    }

    #[test]
    fn template_dependencies_use_transclusion_context() {
        let pages = vec![
            page(1, "Template:Leaf", ""),
            page(2, "Template:Wrong", ""),
            page(
                3,
                "Template:Outer",
                "<noinclude>{{Wrong}}</noinclude><includeonly>{{Leaf}}</includeonly>",
            ),
            page(4, "Article", "{{Outer}}"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Template,
                    class: SetClass::TransitiveUnconditional,
                },
            ),
            vec![3, 4],
        );
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 2,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                },
            ),
            vec![3],
        );
        assert!(set(
            &sets,
            SetKey {
                target_page_id: 2,
                kind: EdgeKind::Template,
                class: SetClass::TransitiveUnconditional,
            },
        )
        .is_empty());
    }

    #[test]
    fn includeonly_file_reference_reaches_template_callers() {
        let pages = vec![
            page(1, "File:Map.png", ""),
            page(
                2,
                "Template:Map",
                "<includeonly>[[File:Map.png]]</includeonly>",
            ),
            page(3, "Article", "{{Map}}"),
            page(4, "Literal", "[[File:Map.png]]"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::File,
                    class: SetClass::DirectUnconditional,
                },
            ),
            vec![4],
        );
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::File,
                    class: SetClass::TransitiveUnconditional,
                },
            ),
            vec![3],
        );
    }

    #[test]
    fn template_emitted_categories_reach_transcluding_pages() {
        let pages = vec![
            page(1, "Category:Emitted", ""),
            page(
                2,
                "Template:Inner",
                "<includeonly>[[Category:Emitted]]</includeonly>",
            ),
            page(3, "Template:Outer", "{{Inner}}"),
            page(4, "Article", "{{Outer}}"),
        ];
        let sets = build_sets(&pages, resolver(&pages));
        assert!(set(
            &sets,
            SetKey {
                target_page_id: 1,
                kind: EdgeKind::Category,
                class: SetClass::DirectUnconditional,
            },
        )
        .is_empty());
        assert_eq!(
            set(
                &sets,
                SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Category,
                    class: SetClass::TransitiveUnconditional,
                },
            ),
            vec![3, 4],
        );
    }

    #[test]
    fn streaming_sidecar_propagates_template_emitted_categories() {
        let mut direct = DirectSets::new();
        let mut callers = Bitmap::default();
        callers.insert(3);
        direct.insert(
            (EdgeKind::Template, 2, Certainty::Definite),
            callers,
        );
        let graph = DiskGraph::from_edges(vec![GraphEdge {
            source: 3,
            target: 2,
            kind: EdgeKind::Template,
            certainty: Certainty::Definite,
        }])
        .unwrap();
        let effects = DiskGraph::from_edges(vec![GraphEdge {
            source: 2,
            target: 1,
            kind: EdgeKind::Category,
            certainty: Certainty::Definite,
        }])
        .unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("emitted.swrefs");
        let topology_seeds = direct.clone();
        write_streaming_sidecar(
            &path,
            DiskSets::from_memory(direct).unwrap(),
            DiskSets::from_memory(topology_seeds).unwrap(),
            graph,
            effects,
            tempfile::tempfile().unwrap(),
            0,
        )
        .unwrap();
        let index = BackrefIndex::open(&path).unwrap();
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Category,
                    class: SetClass::TransitiveUnconditional,
                })
                .unwrap(),
            vec![3],
        );
    }

    #[test]
    fn sparse_high_page_ids_do_not_expand_the_bitmap_universe() {
        let mut bitmap = Bitmap::default();
        bitmap.insert((1_u64 << 39) + 7);
        bitmap.insert(3);
        let encoded = encode_bitmap(&bitmap);
        assert!(encoded.len() < 32);
        assert_eq!(decode_bitmap(&encoded).unwrap(), bitmap);
        let sidecar_encoded = encode_sidecar_bitmap(&bitmap);
        assert_eq!(decode_sidecar_bitmap(&sidecar_encoded).unwrap(), bitmap);
    }

    #[test]
    fn external_edge_runs_merge_and_deduplicate_with_a_tiny_memory_bound() {
        let mut spool = tempfile::tempfile().unwrap();
        let edges = [
            EdgeRecord {
                kind: EdgeKind::Template,
                target: 7,
                certainty: Certainty::Definite,
                emitted: false,
                topology: true,
                source: (1_u64 << 39) + 1,
            },
            EdgeRecord {
                kind: EdgeKind::Template,
                target: 7,
                certainty: Certainty::Definite,
                emitted: false,
                topology: true,
                source: 2,
            },
            EdgeRecord {
                kind: EdgeKind::Template,
                target: 7,
                certainty: Certainty::Definite,
                emitted: false,
                topology: true,
                source: (1_u64 << 39) + 1,
            },
            EdgeRecord {
                kind: EdgeKind::Category,
                target: 9,
                certainty: Certainty::Possible,
                emitted: true,
                topology: false,
                source: 4,
            },
        ];
        for edge in edges {
            write_edge(&mut spool, edge).unwrap();
        }
        spool.seek(SeekFrom::Start(0)).unwrap();
        let mut collected = collect_sorted_edges_with_limit(
            spool,
            2,
            &RedirectTable::default(),
        )
        .unwrap();
        assert_eq!(collected.redirect_misses, 0);
        assert_eq!(
            collected
                .direct
                .get(&(EdgeKind::Template, 7, Certainty::Definite))
                .unwrap()
                .unwrap()
                .members()
                .collect::<Vec<_>>(),
            vec![2, (1_u64 << 39) + 1],
        );
        let effect = collected.effects.edge_at(false, collected.effects.range(false, 4).0);
        assert_eq!(
            effect,
            GraphEdge {
                source: 4,
                target: 9,
                kind: EdgeKind::Category,
                certainty: Certainty::Possible,
            }
        );

        let mut many = tempfile::tempfile().unwrap();
        for source in 0..EDGE_MERGE_FAN_IN as u64 + 3 {
            write_edge(
                &mut many,
                EdgeRecord {
                    kind: EdgeKind::Template,
                    target: 11,
                    certainty: Certainty::Definite,
                    emitted: false,
                    topology: true,
                    source,
                },
            )
            .unwrap();
        }
        many.seek(SeekFrom::Start(0)).unwrap();
        let mut collected = collect_sorted_edges_with_limit(
            many,
            1,
            &RedirectTable::default(),
        )
        .unwrap();
        assert_eq!(
            collected
                .direct
                .get(&(EdgeKind::Template, 11, Certainty::Definite))
                .unwrap()
                .unwrap()
                .len(),
            EDGE_MERGE_FAN_IN as u64 + 3,
        );
    }

    #[test]
    fn topology_base_is_physically_before_and_used() {
        let base_key = SetKey {
            target_page_id: 20,
            kind: EdgeKind::Template,
            class: SetClass::TransitiveUnconditional,
        };
        let target_key = SetKey {
            target_page_id: 10,
            kind: EdgeKind::Template,
            class: SetClass::TransitiveUnconditional,
        };
        let mut base = Bitmap::default();
        base.insert(100);
        base.insert(200);
        let mut target = base.clone();
        target.insert(300);
        let mut encoder = StreamingEncoder::new().unwrap();
        encoder
            .add(LogicalSet {
                key: base_key,
                members: base,
                topology_bases: Vec::new(),
            })
            .unwrap();
        encoder
            .add(LogicalSet {
                key: target_key,
                members: target,
                topology_bases: vec![base_key],
            })
            .unwrap();
        let target_position = encoder.logical_object(target_key).unwrap() as usize;
        let base_position = encoder.logical_object(base_key).unwrap() as usize;
        assert!(encoder.entries[target_position].base_offset > 0);
        assert_eq!(
            target_position - encoder.entries[target_position].base_offset as usize,
            base_position,
        );
    }

    #[test]
    fn physically_stale_xor_candidates_are_not_truncated_to_u8() {
        let base_key = SetKey {
            target_page_id: 1,
            kind: EdgeKind::Template,
            class: SetClass::TransitiveUnconditional,
        };
        let mut base = Bitmap::default();
        base.insert(100);
        base.insert(200);
        let mut encoder = StreamingEncoder::new().unwrap();
        encoder
            .add(LogicalSet {
                key: base_key,
                members: base.clone(),
                topology_bases: Vec::new(),
            })
            .unwrap();
        for page_id in 2..=302 {
            let mut members = Bitmap::default();
            members.insert(10_000 + page_id);
            encoder
                .add(LogicalSet {
                    key: SetKey {
                        target_page_id: page_id,
                        kind: EdgeKind::Template,
                        class: SetClass::DirectUnconditional,
                    },
                    members,
                    topology_bases: Vec::new(),
                })
                .unwrap();
        }
        let mut target = base;
        target.insert(300);
        let target_key = SetKey {
            target_page_id: 303,
            kind: EdgeKind::Template,
            class: SetClass::TransitiveUnconditional,
        };
        encoder
            .add(LogicalSet {
                key: target_key,
                members: target,
                topology_bases: vec![base_key],
            })
            .unwrap();
        let target_position = encoder.logical_object(target_key).unwrap() as usize;
        assert_eq!(encoder.entries[target_position].base_offset, 0);
    }

    #[test]
    fn iterative_graph_walks_handle_deep_chains() {
        let count = 100_000;
        let mut graph = vec![Vec::new(); count];
        let mut dag = vec![BTreeSet::new(); count];
        for node in 0..count - 1 {
            graph[node].push(node + 1);
            dag[node].insert(node + 1);
        }
        let (_, components) = strongly_connected(&graph);
        assert_eq!(components.len(), count);
        assert_eq!(reverse_topological(&dag).len(), count);
    }

    #[test]
    fn localized_redirect_magic_word_is_recognized() {
        let words = vec![RedirectWord {
            alias: "#ПЕРЕНАПРАВЛЕНИЕ".to_string(),
            case_sensitive: false,
        }];
        assert_eq!(
            parse_redirect("#перенаправление [[Шаблон:Цель]]", &words),
            Some("Шаблон:Цель"),
        );
        assert_eq!(
            parse_redirect("#перенаправление [[Шаблон:Цель#Раздел]]", &words),
            Some("Шаблон:Цель"),
        );
        let case_sensitive = vec![RedirectWord {
            alias: "#REDIRECT".to_string(),
            case_sensitive: true,
        }];
        assert_eq!(
            parse_redirect("#redirect [[Template:Target]]", &case_sensitive),
            None,
        );
    }

    #[test]
    fn one_pass_raw_build_matches_reference_across_resolution_and_effects() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("differential.swdump");
        let titles = temporary.path().join("differential.swtitle");
        let sidecar = temporary.path().join("differential.swrefs");
        let site_info = crate::archive::SiteInfoRecord {
            site_name: "Differential".into(),
            db_name: "differentialwiki".into(),
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
                    id: 6,
                    case: "first-letter".into(),
                    localized_name: "File".into(),
                    aliases: vec!["Image".into()],
                },
                crate::archive::SiteNamespaceRecord {
                    id: 10,
                    case: "first-letter".into(),
                    localized_name: "Template".into(),
                    aliases: vec!["T".into()],
                },
                crate::archive::SiteNamespaceRecord {
                    id: 14,
                    case: "first-letter".into(),
                    localized_name: "Category".into(),
                    aliases: Vec::new(),
                },
                crate::archive::SiteNamespaceRecord {
                    id: 828,
                    case: "first-letter".into(),
                    localized_name: "Module".into(),
                    aliases: Vec::new(),
                },
            ],
            interwiki: Vec::new(),
            magic_words: Vec::new(),
        };
        let page_inputs = [
            (1, "Current", 10, "", Some(7), None),
            (2, "RedirectA", 10, "#REDIRECT [[T:Current]]", None, None),
            (3, "RedirectB", 10, "#REDIRECT [[Template:RedirectA]]", None, None),
            (4, "CycleA", 10, "#REDIRECT [[Template:CycleB]]", None, None),
            (5, "CycleB", 10, "#REDIRECT [[Template:CycleA]]", None, None),
            (
                6,
                "Article",
                0,
                "{{T:RedirectB}}{{#if:{{unknown}}|{{T:Current}}|}}{{T:Missing}}{{T:CycleA}}{{T:Old}}",
                Some(7),
                None,
            ),
            (
                7,
                "Emitter",
                10,
                "<includeonly>[[Category:Cat]][[File:Pic.png]]</includeonly>",
                None,
                None,
            ),
            (8, "Caller", 0, "{{Emitter}}", Some(7), Some((8, "{{T:Missing old}}"))),
            (9, "Cat", 14, "", None, None),
            (10, "Pic.png", 6, "", None, None),
            (
                11,
                "Possible only",
                0,
                "{{#if:{{unknown}}|{{T:Current}}|}}",
                None,
                None,
            ),
            (12, "Old", 10, "", None, None),
            (13, "CatAlias", 14, "#REDIRECT [[Category:Cat]]", None, None),
            (14, "Categorized", 0, "[[Category:CatAlias]]", None, None),
        ];
        let mut writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 256)
                .unwrap();
        for (page_id, local_title, namespace, text, user, older) in page_inputs {
            writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: 300_000_000,
                    title: local_title.into(),
                    namespace: Some(namespace),
                    deleted: false,
                })
                .unwrap();
            let latest = user.map_or_else(
                || revision(page_id, page_id * 10 + 2, 200, text),
                |user_id| named_revision(page_id, page_id * 10 + 2, 200, text, user_id),
            );
            writer.write(&latest).unwrap();
            if page_id == 1 {
                writer
                    .write(&Record::PageAction {
                        entity: crate::archive::EntityKey {
                            kind: crate::archive::EntityKind::Page,
                            id: page_id,
                        },
                        timestamp_micros: 150_000_000,
                        action: crate::archive::PageActionRecord {
                            log_id: Some(1),
                            tie_sequence: 1,
                            kind: crate::archive::PageActionKind::Move,
                            performer: crate::archive::PerformerRecord {
                                local_user_id: Some(7),
                                central_user_id: None,
                                historical_name: Some("User 7".into()),
                                account_class: crate::archive::AccountClass::Permanent,
                            },
                            comment: String::new(),
                            title_at_event: "Old".into(),
                            namespace_at_event: Some(10),
                            resulting_deleted: Some(false),
                        },
                    })
                    .unwrap();
            }
            if let Some((user_id, older_text)) = older {
                writer
                    .write(&named_revision(
                        page_id,
                        page_id * 10 + 1,
                        100,
                        older_text,
                        user_id,
                    ))
                    .unwrap();
            }
        }
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 300_000_000,
                site_info: site_info.clone(),
            })
            .unwrap();
        writer.finish().unwrap();
        crate::title_index::build(
            &archive,
            &titles,
            &crate::generation::GenerationId::from_plan_bytes(b"backrefs-raw-differential"),
        )
        .unwrap();
        let title_index = crate::title_index::TitleIndex::open(&titles).unwrap();
        let namespaces = namespace_map(&site_info);
        let current_pages = page_inputs
            .iter()
            .map(|(page_id, local_title, namespace, text, _, _)| SourcePage {
                page_id: *page_id,
                title: crate::title_index::title_in_namespace(local_title, *namespace, &site_info),
                text: Some((*text).into()),
            })
            .collect::<Vec<_>>();
        let owners = current_pages
            .iter()
            .map(|page| {
                (
                    crate::title_index::coded_title(&page.title, &site_info),
                    page.page_id,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let reference = build_logical_sets(
            &current_pages,
            |title| {
                let normalized = namespaces.normalize_title_for_site(title);
                owners
                    .get(&crate::title_index::coded_title(&normalized, &site_info))
                    .copied()
            },
            &namespaces,
            &[RedirectWord {
                alias: "#REDIRECT".into(),
                case_sensitive: false,
            }],
        );
        let expected = reference
            .into_iter()
            .map(|set| (set.key, set.members.members().collect::<Vec<_>>()))
            .collect::<BTreeMap<_, _>>();

        let _build_guard = BACKREF_BUILD_TEST_LOCK.lock().unwrap();
        let callbacks_before = BACKREF_BUILD_FRAME_SCANS.load(Ordering::SeqCst);
        build_with_workers_inner(&archive, &titles, &sidecar, 3).unwrap();
        let callbacks = BACKREF_BUILD_FRAME_SCANS.load(Ordering::SeqCst) - callbacks_before;
        assert_eq!(callbacks, title_index.frame_count() as u64);
        let index = BackrefIndex::open_for_title_index(&sidecar, &titles).unwrap();
        assert_eq!(
            index.capabilities,
            REQUIRED_CAPABILITIES | CAPABILITY_RAW_POSTINGS,
        );
        let actual = collect_logical_sets(&index)
            .into_iter()
            .filter(|(key, _)| !is_raw_class(key.class) && key.kind != EdgeKind::UserEdits)
            .collect::<BTreeMap<_, _>>();
        assert_eq!(actual, expected);
        assert_eq!(index.pages_edited_by(7).unwrap(), vec![1, 6, 8]);
        assert_eq!(index.pages_edited_by(8).unwrap(), vec![8]);

        let coded = |title: &str| {
            crate::title_index::coded_title(
                &namespaces.normalize_title_for_site(title),
                &site_info,
            )
        };
        assert_eq!(title_index.current_owner(coded("T:Current")), Some(1));
        assert_eq!(title_index.current_owner(coded("T:Old")), Some(12));
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: coded("T:RedirectB"),
                    kind: EdgeKind::Template,
                    class: SetClass::RawTopologyUnconditional,
                })
                .unwrap(),
            vec![6],
        );
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: coded("T:Missing"),
                    kind: EdgeKind::Template,
                    class: SetClass::RawTopologyUnconditional,
                })
                .unwrap(),
            vec![6],
        );
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: coded("T:Current"),
                    kind: EdgeKind::Redirect,
                    class: SetClass::RawRedirectTarget,
                })
                .unwrap(),
            vec![2],
        );
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: coded("Category:Cat"),
                    kind: EdgeKind::Category,
                    class: SetClass::RawEmittedUnconditional,
                })
                .unwrap(),
            vec![7],
        );
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                })
                .unwrap(),
            vec![6],
        );
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectPossible,
                })
                .unwrap(),
            vec![11],
        );
        assert!(index
            .members(SetKey {
                target_page_id: 4,
                kind: EdgeKind::Template,
                class: SetClass::DirectUnconditional,
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn archive_build_streams_only_latest_revisions_and_reports_misses() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("sample.swdump");
        let titles = temporary.path().join("sample.swtitle");
        let sidecar = temporary.path().join("sample.swrefs");
        let mut writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 1)
                .unwrap();
        for (page_id, title, namespace, latest, older) in [
            (1, "Цель", 10, "", ""),
            (
                2,
                "Статья",
                0,
                "{{Шаблон:Старое имя}}",
                "{{Шаблон:Missing in old revision}}",
            ),
            (
                3,
                "С пропусками",
                0,
                "{{Шаблон:Missing}}[[Category:{{dynamic}}]]",
                "",
            ),
            (
                4,
                "Старое имя",
                10,
                "#ПЕРЕНАПРАВЛЕНИЕ [[Шаблон:Промежуточное имя]]",
                "",
            ),
            (
                5,
                "Промежуточное имя",
                10,
                "#ПЕРЕНАПРАВЛЕНИЕ [[Шаблон:Цель]]",
                "",
            ),
        ] {
            writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: 300_000_000,
                    title: title.into(),
                    namespace: Some(namespace),
                    deleted: false,
            })
            .unwrap();
            let latest_revision = match page_id {
                2 => named_revision(page_id, page_id * 10 + 2, 200, latest, 7),
                3 => named_revision(page_id, page_id * 10 + 2, 200, latest, 8),
                _ => revision(page_id, page_id * 10 + 2, 200, latest),
            };
            writer.write(&latest_revision).unwrap();
            if !older.is_empty() {
                let older_revision = if page_id == 2 {
                    named_revision(page_id, page_id * 10 + 1, 100, older, 7)
                } else {
                    revision(page_id, page_id * 10 + 1, 100, older)
                };
                writer.write(&older_revision).unwrap();
            }
        }
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 300_000_000,
                site_info: crate::archive::SiteInfoRecord {
                    site_name: "Тест".into(),
                    db_name: "testwiki".into(),
                    base: String::new(),
                    generator: String::new(),
                    case: "first-letter".into(),
                    language: "ru".into(),
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
                            localized_name: "Шаблон".into(),
                            aliases: vec!["Template".into()],
                        },
                    ],
                    interwiki: Vec::new(),
                    magic_words: vec![crate::archive::SiteMagicWordRecord {
                        canonical_name: "redirect".into(),
                        aliases: vec!["#ПЕРЕНАПРАВЛЕНИЕ".into()],
                        case_sensitive: false,
                    }],
                },
            })
            .unwrap();
        writer.finish().unwrap();
        crate::title_index::build(
            &archive,
            &titles,
            &crate::generation::GenerationId::from_plan_bytes(b"backrefs-test-archive"),
        )
        .unwrap();
        let titles_index = crate::title_index::TitleIndex::open(&titles).unwrap();
        let site_info = read_site_info(&archive, &titles_index).unwrap();
        let mut serial_pages = Vec::new();
        visit_latest_pages(&archive, &site_info, |page| {
            serial_pages.push(page);
            Ok(())
        })
        .unwrap();
        let mut parallel_pages = Vec::new();
        crate::archive::process_frames_parallel(
            &archive,
            2,
            |_, _, frame| {
                let mut pages = Vec::new();
                visit_latest_pages_in_frame(frame, &site_info, |page| {
                    pages.push(page);
                    Ok(())
                })?;
                Ok(pages)
            },
            |_, pages| {
                parallel_pages.extend(pages);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(parallel_pages, serial_pages);

        let stats = build_with_workers(&archive, &titles, &sidecar, 1).unwrap();
        assert_eq!(stats.source_pages, 5);
        assert_eq!(stats.redirect_pages, 2);
        assert_eq!(stats.users_with_edits, 2);
        assert_eq!(stats.user_page_memberships, 2);
        // One explicit missing template plus the template used to compute the
        // dynamic category target.  The older revision's missing target is
        // deliberately absent.
        assert_eq!(stats.unresolved_static_edges, 2);
        assert!(stats.unresolved_dynamic_targets >= 1);
        let index = BackrefIndex::open_for_title_index(&sidecar, &titles).unwrap();
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                })
                .unwrap(),
            vec![2],
        );
        assert_eq!(
            index
                .members(SetKey {
                    target_page_id: 1,
                    kind: EdgeKind::Template,
                    class: SetClass::TransitiveUnconditional,
                })
                .unwrap(),
            vec![2],
        );
        let second = temporary.path().join("second.swrefs");
        let parallel_stats = build_with_workers(&archive, &titles, &second, 2).unwrap();
        assert_eq!(parallel_stats, stats);
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            std::fs::read(&second).unwrap(),
        );
    }

    #[test]
    fn user_edit_sets_index_pages_not_actions_or_repeated_revisions() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("users.swdump");
        let titles = temporary.path().join("users.swtitle");
        let sidecar = temporary.path().join("users.swrefs");
        let mut writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 4096)
                .unwrap();
        for (page_id, revisions) in [
            (
                1,
                vec![
                    named_revision(1, 12, 200, "new", 7),
                    named_revision(1, 11, 100, "old", 7),
                ],
            ),
            (
                2,
                vec![
                    named_revision(2, 22, 200, "new", 7),
                    named_revision(2, 21, 100, "old", 8),
                ],
            ),
            (3, vec![revision(3, 31, 100, "anonymous")]),
            (4, vec![named_revision(4, 41, 100, "zero", 0)]),
        ] {
            writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: 300_000_000,
                    title: format!("Page {page_id}"),
                    namespace: Some(0),
                    deleted: false,
                })
                .unwrap();
            for revision in revisions {
                writer.write(&revision).unwrap();
            }
        }
        writer
            .write(&Record::UserAction {
                entity: crate::archive::EntityKey {
                    kind: crate::archive::EntityKind::User,
                    id: 9,
                },
                timestamp_micros: 300,
                action: crate::archive::UserActionRecord {
                    log_id: Some(1),
                    tie_sequence: 1,
                    kind: crate::archive::UserActionKind::Create,
                    performer: crate::archive::PerformerRecord {
                        local_user_id: Some(9),
                        central_user_id: None,
                        historical_name: Some("Action only".into()),
                        account_class: crate::archive::AccountClass::Permanent,
                    },
                    comment: String::new(),
                    historical_name: Some("Action only".into()),
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
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 300_000_000,
                site_info: crate::archive::SiteInfoRecord {
                    site_name: "Users".into(),
                    db_name: "userswiki".into(),
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
                },
            })
            .unwrap();
        writer.finish().unwrap();
        let mut tiny_runs = collect_user_edit_pages_with_limit(&archive, 1).unwrap();
        let mut tiny_sets = Vec::new();
        assert_eq!(
            visit_user_edit_sets(&mut tiny_runs, |set| {
                tiny_sets.push((set.key.target_page_id, set.members.members().collect::<Vec<_>>()));
                Ok(())
            })
            .unwrap(),
            (2, 3),
        );
        assert_eq!(tiny_sets, vec![(7, vec![1, 2]), (8, vec![2])]);
        crate::title_index::build(
            &archive,
            &titles,
            &crate::generation::GenerationId::from_plan_bytes(
                b"backrefs-transitive-test-archive",
            ),
        )
        .unwrap();
        let stats = build(&archive, &titles, &sidecar).unwrap();
        assert_eq!(stats.users_with_edits, 2);
        assert_eq!(stats.user_page_memberships, 3);
        let index = BackrefIndex::open_for_title_index(&sidecar, &titles).unwrap();
        assert_eq!(index.pages_edited_by(7).unwrap(), vec![1, 2]);
        assert_eq!(index.pages_edited_by(8).unwrap(), vec![2]);
        assert!(index.pages_edited_by(9).unwrap().is_empty());
        assert!(index.pages_edited_by(0).unwrap().is_empty());
    }

    #[test]
    fn deterministic_sidecar_round_trip() {
        let logical = vec![
            (
                SetKey {
                    target_page_id: 9,
                    kind: EdgeKind::Template,
                    class: SetClass::DirectUnconditional,
                },
                vec![1, 4, 130],
            ),
            (
                SetKey {
                    target_page_id: 9,
                    kind: EdgeKind::Template,
                    class: SetClass::TransitiveUnconditional,
                },
                vec![1, 4, 7, 130],
            ),
        ];
        let temporary = tempfile::tempdir().unwrap();
        let left = temporary.path().join("left.swrefs");
        let right = temporary.path().join("right.swrefs");
        write_sidecar(&left, &logical).unwrap();
        write_sidecar(&right, &logical).unwrap();
        assert_eq!(std::fs::read(&left).unwrap(), std::fs::read(&right).unwrap());
        let index = BackrefIndex::open(&left).unwrap();
        assert_eq!(index.members(logical[1].0).unwrap(), logical[1].1);
    }

    #[test]
    fn logical_directories_rank_page_ids_and_share_bitmap_objects() {
        let members = vec![2, 65, 9000];
        let keys = [
            SetKey {
                target_page_id: 1,
                kind: EdgeKind::Template,
                class: SetClass::DirectUnconditional,
            },
            SetKey {
                target_page_id: 63,
                kind: EdgeKind::Template,
                class: SetClass::DirectUnconditional,
            },
            SetKey {
                target_page_id: 64,
                kind: EdgeKind::Template,
                class: SetClass::DirectUnconditional,
            },
            SetKey {
                target_page_id: 1 << 39,
                kind: EdgeKind::Category,
                class: SetClass::TransitivePossible,
            },
        ];
        let logical = keys
            .iter()
            .map(|key| (*key, members.clone()))
            .collect::<Vec<_>>();
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("shared.swrefs");
        assert_eq!(write_sidecar(&path, &logical).unwrap(), 1);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            u64::from_le_bytes(bytes[64..72].try_into().unwrap()),
            2,
        );
        let index = BackrefIndex::open(&path).unwrap();
        for key in keys {
            assert_eq!(index.members(key).unwrap(), members);
        }
        assert!(index
            .members(SetKey {
                target_page_id: 62,
                kind: EdgeKind::Template,
                class: SetClass::DirectUnconditional,
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn dedup_hash_collisions_compare_exact_bitmap_bytes() {
        let mut encoder = StreamingEncoder::new().unwrap();
        encoder.forced_hash = Some(7);
        for (page_id, member) in [(1, 10), (2, 20), (3, 20)] {
            let mut members = Bitmap::default();
            members.insert(member);
            encoder
                .add(LogicalSet {
                    key: SetKey {
                        target_page_id: page_id,
                        kind: EdgeKind::File,
                        class: SetClass::DirectUnconditional,
                    },
                    members,
                    topology_bases: Vec::new(),
                })
                .unwrap();
        }
        assert_eq!(encoder.entries.len(), 2);
        assert_eq!(encoder.dedup.collisions.len(), 1);
        let ids = [1, 2, 3].map(|target_page_id| {
            encoder
                .logical_object(SetKey {
                    target_page_id,
                    kind: EdgeKind::File,
                    class: SetClass::DirectUnconditional,
                })
                .unwrap()
        });
        assert_ne!(ids[0], ids[1]);
        assert_eq!(ids[1], ids[2]);
    }

    #[test]
    fn empty_logical_sets_are_absent() {
        let key = SetKey {
            target_page_id: 9,
            kind: EdgeKind::File,
            class: SetClass::DirectPossible,
        };
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("empty.swrefs");
        assert_eq!(write_sidecar(&path, &[(key, Vec::new())]).unwrap(), 0);
        let index = BackrefIndex::open(&path).unwrap();
        assert!(index.members(key).unwrap().is_empty());
        assert!(index.pages_edited_by(7).unwrap().is_empty());
    }

    #[test]
    fn sidecar_requires_user_edit_capability() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("old.swrefs");
        write_sidecar(&path, &[]).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[12..16].copy_from_slice(&0_u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(BackrefIndex::open(&path).is_err());
    }

    #[test]
    fn sidecar_attachment_requires_exact_title_index() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("attached.swrefs");
        let matching = temporary.path().join("matching.swtitle");
        let other = temporary.path().join("other.swtitle");
        std::fs::write(&matching, b"exact title-index bytes").unwrap();
        std::fs::write(&other, b"different title-index bytes").unwrap();
        write_sidecar(&path, &[]).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[72..80]
            .copy_from_slice(&file_xxh3_64(&matching).unwrap().to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        BackrefIndex::open_for_title_index(&path, &matching).unwrap();
        assert!(BackrefIndex::open_for_title_index(&path, &other).is_err());
    }

    #[test]
    fn sidecar_rejects_bad_bases_and_malformed_roaring_payloads() {
        let logical = vec![(
            SetKey {
                target_page_id: 9,
                kind: EdgeKind::Template,
                class: SetClass::DirectUnconditional,
            },
            vec![1],
        )];
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("corrupt.swrefs");
        write_sidecar(&path, &logical).unwrap();
        let original = std::fs::read(&path).unwrap();

        let mut bad_base = original.clone();
        let base_offsets_offset =
            u64::from_le_bytes(bad_base[48..56].try_into().unwrap()) as usize;
        bad_base[base_offsets_offset] = 1;
        std::fs::write(&path, bad_base).unwrap();
        assert!(BackrefIndex::open(&path).is_err());

        let mut bad_count = original.clone();
        let payload_offset =
            u64::from_le_bytes(bad_count[56..64].try_into().unwrap()) as usize;
        bad_count[payload_offset] = 0xff;
        std::fs::write(&path, bad_count).unwrap();
        let index = BackrefIndex::open(&path).unwrap();
        assert!(index.members(logical[0].0).is_err());

        let mut truncated = original.clone();
        truncated.pop();
        std::fs::write(&path, truncated).unwrap();
        assert!(BackrefIndex::open(&path).is_err());

        let mut trailing = original;
        trailing.push(0);
        let object_count =
            u64::from_le_bytes(trailing[16..24].try_into().unwrap()) as usize;
        let object_offsets_offset =
            u64::from_le_bytes(trailing[40..48].try_into().unwrap()) as usize;
        let final_boundary = object_offsets_offset + object_count * 8;
        let trailing_len = trailing.len() as u64;
        trailing[final_boundary..final_boundary + 8]
            .copy_from_slice(&trailing_len.to_le_bytes());
        std::fs::write(&path, trailing).unwrap();
        let index = BackrefIndex::open(&path).unwrap();
        assert!(index.members(logical[0].0).is_err());
    }
}
