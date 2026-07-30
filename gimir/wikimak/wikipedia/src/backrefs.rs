//! Generated reverse-reference index over the newest revision of every page.
//!
//! This is deliberately a sidecar, not an archive record: rebuilding it from
//! the same archive and title index produces identical bytes.  Direct sets are
//! stored independently.  Transitive sets use Git-style bounded XOR bases,
//! preferring graph-topology candidates before a small backward heuristic
//! window.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use memmap2::Mmap;
use crate::archive::{ArchiveError, ArchiveRecordReader, Record};
pub use crate::backrefs_parse::EdgeKind;
use crate::backrefs_parse::{
    extract_report_with_namespaces, Certainty, InclusionContext, NamespaceMap, RawEdge,
};

const MAGIC: [u8; 8] = *b"SWREFS\0\0";
const VERSION: u32 = 2;
const HEADER_BYTES: usize = 56;
const ENTRY_BYTES: usize = 40;
const DIRECTORY_BYTES: usize = 24;
const MAX_XOR_OFFSET: usize = 160;
const MAX_XOR_DEPTH: u8 = 10;
const HEURISTIC_WINDOW: usize = 16;
const RECENT_BITMAP_BUDGET: usize = 8 * 1024 * 1024;
const EDGE_BYTES: usize = 24;
const EDGE_RUN_RECORDS: usize =
    (48 * 1024 * 1024) / std::mem::size_of::<EdgeRecord>();
const EDGE_MERGE_FAN_IN: usize = 64;

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
    pub sets: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SetClass {
    DirectUnconditional,
    DirectPossible,
    TransitiveUnconditional,
    TransitivePossible,
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

#[cfg(test)]
#[derive(Clone, Debug)]
struct EncodedSet {
    key: SetKey,
    base_offset: u16,
    depth: u8,
    payload: Vec<u8>,
    resolved: Option<Bitmap>,
}

#[derive(Clone, Copy)]
struct EncodedMeta {
    key: SetKey,
    base_offset: u16,
    depth: u8,
    payload_offset: u64,
    payload_len: u64,
}

struct RecentSet {
    position: usize,
    key: SetKey,
    depth: u8,
    members: Bitmap,
    bytes: usize,
}

struct StreamingEncoder {
    payload: std::fs::File,
    payload_len: u64,
    entries: Vec<EncodedMeta>,
    recent: VecDeque<RecentSet>,
    recent_bytes: usize,
}

impl StreamingEncoder {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            payload: tempfile::tempfile()?,
            payload_len: 0,
            entries: Vec::new(),
            recent: VecDeque::new(),
            recent_bytes: 0,
        })
    }

    fn add(&mut self, set: LogicalSet) -> std::io::Result<()> {
        let raw = encode_bitmap(&set.members);
        let direct = matches!(
            set.key.class,
            SetClass::DirectUnconditional | SetClass::DirectPossible
        );
        let mut best = None::<(&RecentSet, Vec<u8>)>;
        if !direct {
            for key in &set.topology_bases {
                if let Some(candidate) = self.recent.iter().find(|entry| entry.key == *key) {
                    consider_recent(&set.members, candidate, &raw, &mut best);
                }
            }
            for candidate in self.recent.iter().rev().take(HEURISTIC_WINDOW) {
                consider_recent(&set.members, candidate, &raw, &mut best);
            }
        }
        let (base_offset, depth, payload) = best.map_or((0, 0, raw), |(base, payload)| {
            (
                (self.entries.len() - base.position) as u16,
                base.depth + 1,
                payload,
            )
        });
        self.payload.write_all(&payload)?;
        self.entries.push(EncodedMeta {
            key: set.key,
            base_offset,
            depth,
            payload_offset: self.payload_len,
            payload_len: payload.len() as u64,
        });
        self.payload_len += payload.len() as u64;
        let bitmap_bytes =
            set.members.words.capacity() * std::mem::size_of::<(u64, u64)>();
        if !direct && bitmap_bytes <= RECENT_BITMAP_BUDGET {
            self.recent.push_back(RecentSet {
                position: self.entries.len() - 1,
                key: set.key,
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

    fn write(mut self, output: impl AsRef<Path>) -> crate::archive::Result<()> {
        let output = output.as_ref();
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        let entries_offset = HEADER_BYTES as u64;
        let directory_offset =
            entries_offset + self.entries.len() as u64 * ENTRY_BYTES as u64;
        let payload_offset =
            directory_offset + self.entries.len() as u64 * DIRECTORY_BYTES as u64;
        temporary.write_all(&MAGIC)?;
        temporary.write_all(&VERSION.to_le_bytes())?;
        temporary.write_all(&(HEADER_BYTES as u32).to_le_bytes())?;
        temporary.write_all(&(self.entries.len() as u64).to_le_bytes())?;
        temporary.write_all(&entries_offset.to_le_bytes())?;
        temporary.write_all(&directory_offset.to_le_bytes())?;
        temporary.write_all(&payload_offset.to_le_bytes())?;
        temporary.write_all(&0_u64.to_le_bytes())?;
        for entry in &self.entries {
            write_entry(
                temporary.as_file_mut(),
                entry.key,
                entry.base_offset,
                entry.depth,
                payload_offset + entry.payload_offset,
                entry.payload_len,
            )?;
        }
        let mut directory = self
            .entries
            .iter()
            .enumerate()
            .map(|(position, entry)| (entry.key, position as u64))
            .collect::<Vec<_>>();
        directory.sort_by_key(|entry| entry.0);
        write_directory(temporary.as_file_mut(), &directory)?;
        self.payload.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut self.payload, temporary.as_file_mut())?;
        temporary.as_file_mut().sync_all()?;
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
    let delta = encode_bitmap(&members.difference(&candidate.members));
    let current_len = best.as_ref().map_or(raw.len(), |(_, bytes)| bytes.len());
    if delta.len() < current_len {
        *best = Some((candidate, delta));
    }
}

#[derive(Clone, Debug)]
struct PageData {
    title: String,
    namespace: Option<i64>,
    deleted: bool,
    text: Option<String>,
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
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
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
/// index.  Only the first (newest) revision record of each page is examined.
pub fn build(
    archive: impl AsRef<Path>,
    title_index: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> crate::archive::Result<BuildStats> {
    let archive = archive.as_ref();
    let titles = crate::title_index::TitleIndex::open(title_index)?;
    let site_info = read_site_info(archive)?;
    let namespaces = namespace_map(&site_info);
    let redirect_words = redirect_words(&site_info);
    let mut redirects = RedirectTable::default();
    let mut stats = BuildStats::default();
    let mut edge_spool = tempfile::tempfile()?;

    // Redirect resolution must be complete before edges are resolved: archive
    // page order does not guarantee that a redirect target was already seen.
    visit_latest_pages(archive, &site_info, |page| {
        let Some(text) = page.text.as_deref() else {
            return Ok(());
        };
        if let Some(target_title) = parse_redirect(text, &redirect_words) {
            let target = titles.lookup(
                &namespaces.normalize_title_for_site(target_title),
                i64::MAX,
                &site_info,
            );
            redirects.push(page.page_id, target);
        }
        Ok(())
    })?;
    stats.redirect_pages = redirects.len() as u64;

    visit_latest_pages(archive, &site_info, |page| {
        stats.source_pages += 1;
        // A redirect's link is its target, not a normal page-body relation
        // (notably, category redirects must not become category members).
        if redirects.contains(page.page_id) {
            return Ok(());
        }
        let Some(text) = page.text.as_deref() else {
            return Ok(());
        };
        let mut seen = BTreeSet::new();
        let relations = page_relations(&page, text, &namespaces);
        stats.unresolved_dynamic_targets += relations.dynamic_targets;
        for parsed in relations.edges {
            let raw = parsed.raw;
            stats.extracted_static_edges += 1;
            let Some(target) = titles
                .lookup(
                    &namespaces.normalize_title_for_site(&raw.title),
                    i64::MAX,
                    &site_info,
                )
                .and_then(|target| follow_redirects(target, &redirects))
            else {
                stats.unresolved_static_edges += 1;
                continue;
            };
            if !seen.insert((
                raw.kind,
                target,
                raw.certainty,
                parsed.emitted,
                parsed.topology,
            )) {
                continue;
            }
            write_edge(
                &mut edge_spool,
                EdgeRecord {
                    kind: raw.kind,
                    target,
                    certainty: raw.certainty,
                    emitted: parsed.emitted,
                    topology: parsed.topology,
                    source: page.page_id,
                },
            )?;
        }
        Ok(())
    })?;
    edge_spool.sync_all()?;
    edge_spool.seek(SeekFrom::Start(0))?;
    let collected = collect_sorted_edges(edge_spool, &redirects)?;
    stats.unresolved_static_edges += collected.redirect_misses;
    stats.sets = write_streaming_sidecar(
        output,
        collected.direct,
        collected.topology_seeds,
        collected.graph,
        collected.effects,
    )?;
    Ok(stats)
}

fn write_edge(output: &mut impl Write, edge: EdgeRecord) -> std::io::Result<()> {
    output.write_all(&[kind_byte(edge.kind), certainty_byte(edge.certainty)])?;
    output.write_all(&[u8::from(edge.emitted)])?;
    output.write_all(&[u8::from(edge.topology)])?;
    output.write_all(&[0; 4])?;
    output.write_all(&edge.target.to_le_bytes())?;
    output.write_all(&edge.source.to_le_bytes())
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
    let kind = match bytes[0] {
        1 => EdgeKind::Template,
        2 => EdgeKind::Module,
        3 => EdgeKind::Category,
        4 => EdgeKind::File,
        _ => return Err(invalid_data("unknown edge kind in spool")),
    };
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

fn collect_sorted_edges(
    spool: std::fs::File,
    redirects: &RedirectTable,
) -> crate::archive::Result<CollectedEdges> {
    collect_sorted_edges_with_limit(spool, EDGE_RUN_RECORDS, redirects)
}

fn collect_sorted_edges_with_limit(
    mut spool: std::fs::File,
    run_records: usize,
    redirects: &RedirectTable,
) -> crate::archive::Result<CollectedEdges> {
    if run_records == 0 {
        return Err(ArchiveError::Invalid("zero edge-sort run size"));
    }
    let temporary = tempfile::tempdir()?;
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
    let mut direct = DiskSets::new()?;
    let mut topology_seeds = DiskSets::new()?;
    let mut graph_spool = tempfile::tempfile()?;
    let mut effect_spool = tempfile::tempfile()?;
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
    let graph = build_disk_graph(graph_spool, EDGE_RUN_RECORDS)?;
    effect_spool.seek(SeekFrom::Start(0))?;
    let effects = build_disk_graph(effect_spool, EDGE_RUN_RECORDS)?;
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

fn build_disk_graph(
    spool: std::fs::File,
    run_records: usize,
) -> crate::archive::Result<DiskGraph> {
    let forward_file = sort_graph_spool(spool, run_records)?;
    let mut reverse_spool = tempfile::tempfile()?;
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
    let reverse_file = sort_graph_spool(reverse_spool, run_records)?;
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

fn sort_graph_spool(
    mut spool: std::fs::File,
    run_records: usize,
) -> crate::archive::Result<std::fs::File> {
    if run_records == 0 {
        return Err(ArchiveError::Invalid("zero graph-sort run size"));
    }
    let temporary = tempfile::tempdir()?;
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
    let output = tempfile::tempfile()?;
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

fn read_site_info(archive: &Path) -> crate::archive::Result<crate::archive::SiteInfoRecord> {
    let mut reader = ArchiveRecordReader::open(archive)?;
    while let Some(record) = reader.next_record()? {
        if let Record::SiteInfo { site_info, .. } = record {
            return Ok(site_info);
        }
    }
    Err(ArchiveError::Invalid("archive has no siteinfo record"))
}

fn visit_latest_pages(
    archive: &Path,
    site_info: &crate::archive::SiteInfoRecord,
    mut visitor: impl FnMut(SourcePage) -> crate::archive::Result<()>,
) -> crate::archive::Result<()> {
    let mut reader = ArchiveRecordReader::open(archive)?;
    let mut current_id = None;
    let mut current = PageData {
        title: String::new(),
        namespace: None,
        deleted: false,
        text: None,
    };
    let mut saw_revision = false;
    while let Some(record) = reader.next_record()? {
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
            current = PageData {
                title: String::new(),
                namespace: None,
                deleted: false,
                text: None,
            };
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
    let mut logical = Vec::new();
    visit_logical_sets(
        &mut direct,
        &mut topology_seeds,
        &graph,
        &effects,
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
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
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
    mut visitor: impl FnMut(LogicalSet) -> crate::archive::Result<()>,
) -> crate::archive::Result<()> {
    let accepted = accepted_kinds.iter().copied().collect::<BTreeSet<_>>();
    if node_info.is_empty() {
        return Ok(());
    }
    if node_info.len() > u32::MAX as usize {
        return Err(ArchiveError::Invalid("backref graph has more than u32 nodes"));
    }
    let mut guaranteed = BitmapStore::new()?;

    for possible in [false, true] {
        let (component_of, component_offsets, component_nodes) = strongly_connected_disk(
            graph,
            &accepted,
            possible,
            node_info.len(),
        )?;
        let mut component_spool = tempfile::tempfile()?;
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
        let component_graph = build_disk_graph(component_spool, EDGE_RUN_RECORDS)?;
        let mut closures = BitmapStore::new()?;
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

fn write_streaming_sidecar(
    output: impl AsRef<Path>,
    mut direct: DiskSets,
    mut topology_seeds: DiskSets,
    graph: DiskGraph,
    effects: DiskGraph,
) -> crate::archive::Result<u64> {
    let mut encoder = StreamingEncoder::new()?;
    visit_logical_sets(
        &mut direct,
        &mut topology_seeds,
        &graph,
        &effects,
        |set| {
            encoder.add(set)?;
            Ok(())
        },
    )?;
    let count = encoder.entries.len() as u64;
    encoder.write(output)?;
    Ok(count)
}

fn visit_logical_sets(
    direct: &mut DiskSets,
    topology_seeds: &mut DiskSets,
    graph: &DiskGraph,
    effects: &DiskGraph,
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

    let mut effect_contributions = tempfile::tempfile()?;
    let mut empty_extra = DiskSets::new()?;
    let mut render_nodes = topology_seeds
        .positions
        .iter()
        .map(|(key, _)| key)
        .filter(|(kind, _, _)| matches!(kind, EdgeKind::Template | EdgeKind::Module))
        .map(|(kind, target, _)| (*target, *kind))
        .collect::<Vec<_>>();
    render_nodes.sort_unstable();
    render_nodes.dedup();
    let mut typed_spool = tempfile::tempfile()?;
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
    let render_graph = build_disk_graph(typed_spool, EDGE_RUN_RECORDS)?;
    visit_transitive_sets(
        &render_nodes,
        &render_graph,
        &[EdgeKind::Template, EdgeKind::Module],
        topology_seeds,
        &mut empty_extra,
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
    let mut effect_sets = collect_sorted_edges_with_limit(
        effect_contributions,
        EDGE_RUN_RECORDS,
        &RedirectTable::default(),
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
    let mut category_pages = Vec::new();
    for (kind, target, _) in direct
        .positions
        .iter()
        .map(|(key, _)| key)
        .chain(effect_sets.positions.iter().map(|(key, _)| key))
    {
        if *kind == EdgeKind::Category {
            category_pages.push(*target);
        }
    }
    category_pages.sort_unstable();
    category_pages.dedup();
    let category_info = category_pages
        .iter()
        .map(|target| (*target, EdgeKind::Category))
        .collect::<Vec<_>>();
    let mut category_spool = tempfile::tempfile()?;
    if let Some(map) = &graph.forward {
        for index in 0..map.len() / EDGE_BYTES {
            let edge = graph.edge_at(false, index);
            if edge.kind != EdgeKind::Category {
                continue;
            }
            let Ok(source) = category_pages.binary_search(&edge.source) else {
                continue;
            };
            let Ok(target) = category_pages.binary_search(&edge.target) else {
                continue;
            };
            write_graph_edge(
                &mut category_spool,
                GraphEdge {
                    source: source as u64,
                    target: target as u64,
                    kind: EdgeKind::Category,
                    certainty: edge.certainty,
                },
            )?;
        }
    }
    category_spool.seek(SeekFrom::Start(0))?;
    let category_graph = build_disk_graph(category_spool, EDGE_RUN_RECORDS)?;
    visit_transitive_sets(
        &category_info,
        &category_graph,
        &[EdgeKind::Category],
        direct,
        &mut effect_sets,
        visitor,
    )?;
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

#[cfg(test)]
fn encode_sets(mut sets: Vec<LogicalSet>) -> Vec<EncodedSet> {
    sets.sort_by_key(|set| {
        (
            matches!(
                set.key.class,
                SetClass::TransitiveUnconditional | SetClass::TransitivePossible
            ),
            set.key.class,
            set.key.kind,
            set.members.len(),
            set.key.target_page_id,
        )
    });
    let mut encoded = Vec::<EncodedSet>::with_capacity(sets.len());
    let mut positions = BTreeMap::<SetKey, usize>::new();
    for set in sets {
        let raw = encode_bitmap(&set.members);
        let direct = matches!(
            set.key.class,
            SetClass::DirectUnconditional | SetClass::DirectPossible
        );
        let mut best = None::<(usize, Vec<u8>)>;
        if !direct {
            for candidate in &set.topology_bases {
                let Some(position) = positions.get(candidate).copied() else {
                    continue;
                };
                consider_base(&set, &encoded, position, &raw, &mut best);
            }
            let start = encoded.len().saturating_sub(HEURISTIC_WINDOW);
            for position in start..encoded.len() {
                consider_base(&set, &encoded, position, &raw, &mut best);
            }
        }
        let (base_offset, depth, payload) = best.map_or((0, 0, raw), |(base, payload)| {
            (
                (encoded.len() - base) as u16,
                encoded[base].depth + 1,
                payload,
            )
        });
        positions.insert(set.key, encoded.len());
        encoded.push(EncodedSet {
            key: set.key,
            base_offset,
            depth,
            payload,
            resolved: Some(set.members),
        });
        if encoded.len() > MAX_XOR_OFFSET {
            let expired = encoded.len() - MAX_XOR_OFFSET - 1;
            encoded[expired].resolved = None;
        }
    }
    encoded
}

#[cfg(test)]
fn consider_base(
    set: &LogicalSet,
    encoded: &[EncodedSet],
    position: usize,
    raw: &[u8],
    best: &mut Option<(usize, Vec<u8>)>,
) {
    let distance = encoded.len().saturating_sub(position);
    if distance == 0
        || distance > MAX_XOR_OFFSET
        || encoded[position].depth >= MAX_XOR_DEPTH
    {
        return;
    }
    let Some(base) = encoded[position].resolved.as_ref() else {
        return;
    };
    let delta = encode_bitmap(&set.members.difference(base));
    let current_len = best.as_ref().map_or(raw.len(), |(_, bytes)| bytes.len());
    if delta.len() < current_len {
        *best = Some((position, delta));
    }
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
    let sets = logical
        .iter()
        .map(|(key, members)| {
            let mut bitmap = Bitmap::default();
            for member in members {
                bitmap.insert(*member);
            }
            LogicalSet {
                key: *key,
                members: bitmap,
                topology_bases: Vec::new(),
            }
        })
        .collect();
    let encoded = encode_sets(sets);
    write_encoded(output, &encoded)?;
    Ok(encoded.len() as u64)
}

#[cfg(test)]
fn write_encoded(
    output: impl AsRef<Path>,
    encoded: &[EncodedSet],
) -> crate::archive::Result<()> {
    let output = output.as_ref();
    let parent = output.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let entries_offset = HEADER_BYTES as u64;
    let directory_offset = entries_offset + encoded.len() as u64 * ENTRY_BYTES as u64;
    let payload_offset =
        directory_offset + encoded.len() as u64 * DIRECTORY_BYTES as u64;
    temporary.write_all(&MAGIC)?;
    temporary.write_all(&VERSION.to_le_bytes())?;
    temporary.write_all(&(HEADER_BYTES as u32).to_le_bytes())?;
    temporary.write_all(&(encoded.len() as u64).to_le_bytes())?;
    temporary.write_all(&entries_offset.to_le_bytes())?;
    temporary.write_all(&directory_offset.to_le_bytes())?;
    temporary.write_all(&payload_offset.to_le_bytes())?;
    temporary.write_all(&0_u64.to_le_bytes())?;
    let mut offset = payload_offset;
    for entry in encoded {
        temporary.write_all(&entry.key.target_page_id.to_le_bytes())?;
        temporary.write_all(&[kind_byte(entry.key.kind), class_byte(entry.key.class), entry.depth, 0])?;
        temporary.write_all(&entry.base_offset.to_le_bytes())?;
        temporary.write_all(&[0; 2])?;
        temporary.write_all(&offset.to_le_bytes())?;
        temporary.write_all(&(entry.payload.len() as u64).to_le_bytes())?;
        temporary.write_all(&[0; 8])?;
        offset += entry.payload.len() as u64;
    }
    let mut directory = encoded
        .iter()
        .enumerate()
        .map(|(position, entry)| (entry.key, position as u64))
        .collect::<Vec<_>>();
    directory.sort_by_key(|entry| entry.0);
    for (key, position) in directory {
        temporary.write_all(&key.target_page_id.to_le_bytes())?;
        temporary.write_all(&[kind_byte(key.kind), class_byte(key.class)])?;
        temporary.write_all(&[0; 6])?;
        temporary.write_all(&position.to_le_bytes())?;
    }
    for entry in encoded {
        temporary.write_all(&entry.payload)?;
    }
    temporary.as_file_mut().sync_all()?;
    temporary.persist(output).map_err(|error| ArchiveError::Io(error.error))?;
    Ok(())
}

fn write_entry(
    output: &mut impl Write,
    key: SetKey,
    base_offset: u16,
    depth: u8,
    payload_offset: u64,
    payload_len: u64,
) -> std::io::Result<()> {
    output.write_all(&key.target_page_id.to_le_bytes())?;
    output.write_all(&[kind_byte(key.kind), class_byte(key.class), depth, 0])?;
    output.write_all(&base_offset.to_le_bytes())?;
    output.write_all(&[0; 2])?;
    output.write_all(&payload_offset.to_le_bytes())?;
    output.write_all(&payload_len.to_le_bytes())?;
    output.write_all(&[0; 8])
}

fn write_directory(
    output: &mut impl Write,
    directory: &[(SetKey, u64)],
) -> std::io::Result<()> {
    for (key, position) in directory {
        output.write_all(&key.target_page_id.to_le_bytes())?;
        output.write_all(&[kind_byte(key.kind), class_byte(key.class)])?;
        output.write_all(&[0; 6])?;
        output.write_all(&position.to_le_bytes())?;
    }
    Ok(())
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
    entry_count: usize,
    entries_offset: usize,
    directory_offset: usize,
    payload_offset: usize,
}

#[derive(Clone, Copy, Debug)]
struct DiskEntry {
    base_offset: u16,
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
        if u32::from_le_bytes(bytes[8..12].try_into().unwrap()) != VERSION
            || u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize != HEADER_BYTES
        {
            return Err(ArchiveError::Invalid("unsupported backref sidecar version"));
        }
        let entry_count = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
        let entries_offset = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
        let directory_offset = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
        let payload_offset = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
        let reserved = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
        if reserved != 0 {
            return Err(ArchiveError::Invalid("unknown backref sidecar header field"));
        }
        let expected_directory = HEADER_BYTES
            .checked_add(
                entry_count
                    .checked_mul(ENTRY_BYTES)
                    .ok_or(ArchiveError::FieldTooLarge)?,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        let expected_payload = expected_directory
            .checked_add(
                entry_count
                    .checked_mul(DIRECTORY_BYTES)
                    .ok_or(ArchiveError::FieldTooLarge)?,
            )
            .ok_or(ArchiveError::FieldTooLarge)?;
        if entries_offset != HEADER_BYTES
            || directory_offset != expected_directory
            || payload_offset != expected_payload
            || payload_offset > bytes.len()
        {
            return Err(ArchiveError::Invalid("invalid backref sidecar bounds"));
        }
        let mut previous_key = None;
        let mut seen_physical = vec![false; entry_count];
        for position in 0..entry_count {
            let start = directory_offset + position * DIRECTORY_BYTES;
            let entry = &bytes[start..start + DIRECTORY_BYTES];
            let key = SetKey {
                target_page_id: u64::from_le_bytes(entry[..8].try_into().unwrap()),
                kind: parse_kind(entry[8])?,
                class: parse_class(entry[9])?,
            };
            let physical = u64::from_le_bytes(entry[16..24].try_into().unwrap()) as usize;
            if previous_key.is_some_and(|previous| previous >= key) || physical >= entry_count {
                return Err(ArchiveError::Invalid("invalid backref key directory"));
            }
            if std::mem::replace(&mut seen_physical[physical], true) {
                return Err(ArchiveError::Invalid("duplicate backref physical entry"));
            }
            let physical_start = entries_offset + physical * ENTRY_BYTES;
            let physical_entry = &bytes[physical_start..physical_start + ENTRY_BYTES];
            let physical_key = SetKey {
                target_page_id: u64::from_le_bytes(physical_entry[..8].try_into().unwrap()),
                kind: parse_kind(physical_entry[8])?,
                class: parse_class(physical_entry[9])?,
            };
            if physical_key != key {
                return Err(ArchiveError::Invalid("backref directory key mismatch"));
            }
            previous_key = Some(key);
        }
        let mut expected_payload_position = payload_offset;
        let mut depths = Vec::with_capacity(entry_count);
        for position in 0..entry_count {
            let start = entries_offset + position * ENTRY_BYTES;
            let entry = &bytes[start..start + ENTRY_BYTES];
            let depth = entry[10];
            let base = u16::from_le_bytes(entry[12..14].try_into().unwrap()) as usize;
            let offset = u64::from_le_bytes(entry[16..24].try_into().unwrap()) as usize;
            let len = u64::from_le_bytes(entry[24..32].try_into().unwrap()) as usize;
            let expected_depth = if base == 0 {
                0
            } else if base <= position {
                depths[position - base] + 1
            } else {
                u8::MAX
            };
            if base > position
                || base > MAX_XOR_OFFSET
                || depth > MAX_XOR_DEPTH
                || depth != expected_depth
                || offset != expected_payload_position
            {
                return Err(ArchiveError::Invalid("invalid backref physical entry"));
            }
            expected_payload_position = offset
                .checked_add(len)
                .ok_or(ArchiveError::FieldTooLarge)?;
            if expected_payload_position > bytes.len() {
                return Err(ArchiveError::Invalid("invalid backref payload bounds"));
            }
            depths.push(depth);
        }
        if expected_payload_position != bytes.len() {
            return Err(ArchiveError::Invalid("trailing backref sidecar bytes"));
        }
        Ok(Self {
            bytes,
            entry_count,
            entries_offset,
            directory_offset,
            payload_offset,
        })
    }

    pub fn members(&self, key: SetKey) -> crate::archive::Result<Vec<u64>> {
        let Some(position) = self.find_key(key)? else {
            return Ok(Vec::new());
        };
        let mut cache = BTreeMap::new();
        let bitmap = self.decode_entry(position, &mut cache)?;
        Ok(bitmap.members().collect())
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
            let entry = self.entry(current)?;
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
            let entry = self.entry(entry_position)?;
            let delta =
                decode_bitmap(&self.bytes[entry.offset..entry.offset + entry.len])
                    .map_err(ArchiveError::Io)?;
            bitmap = Some(match bitmap {
                Some(base) if entry.base_offset != 0 => delta.difference(&base),
                _ => delta,
            });
            cache.insert(entry_position, bitmap.clone().unwrap());
        }
        bitmap.ok_or(ArchiveError::Invalid("empty backref XOR chain"))
    }

    fn find_key(&self, key: SetKey) -> crate::archive::Result<Option<usize>> {
        let mut left = 0;
        let mut right = self.entry_count;
        while left < right {
            let middle = left + (right - left) / 2;
            let start = self.directory_offset + middle * DIRECTORY_BYTES;
            let entry = &self.bytes[start..start + DIRECTORY_BYTES];
            let candidate = SetKey {
                target_page_id: u64::from_le_bytes(entry[..8].try_into().unwrap()),
                kind: parse_kind(entry[8])?,
                class: parse_class(entry[9])?,
            };
            match candidate.cmp(&key) {
                std::cmp::Ordering::Less => left = middle + 1,
                std::cmp::Ordering::Greater => right = middle,
                std::cmp::Ordering::Equal => {
                    return Ok(Some(
                        u64::from_le_bytes(entry[16..24].try_into().unwrap()) as usize,
                    ));
                }
            }
        }
        Ok(None)
    }

    fn entry(&self, position: usize) -> crate::archive::Result<DiskEntry> {
        if position >= self.entry_count {
            return Err(ArchiveError::Invalid("backref entry position is out of bounds"));
        }
        let start = self.entries_offset + position * ENTRY_BYTES;
        let entry = &self.bytes[start..start + ENTRY_BYTES];
        let offset = u64::from_le_bytes(entry[16..24].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(entry[24..32].try_into().unwrap()) as usize;
        if offset < self.payload_offset
            || offset
                .checked_add(len)
                .is_none_or(|end| end > self.bytes.len())
        {
            return Err(ArchiveError::Invalid("invalid backref payload bounds"));
        }
        Ok(DiskEntry {
            base_offset: u16::from_le_bytes(entry[12..14].try_into().unwrap()),
            offset,
            len,
        })
    }
}

fn kind_byte(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Template => 1,
        EdgeKind::Module => 2,
        EdgeKind::Category => 3,
        EdgeKind::File => 4,
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
        _ => Err(ArchiveError::Invalid("unknown backref edge kind")),
    }
}

fn class_byte(class: SetClass) -> u8 {
    match class {
        SetClass::DirectUnconditional => 1,
        SetClass::DirectPossible => 2,
        SetClass::TransitiveUnconditional => 3,
        SetClass::TransitivePossible => 4,
    }
}

fn parse_class(value: u8) -> crate::archive::Result<SetClass> {
    match value {
        1 => Ok(SetClass::DirectUnconditional),
        2 => Ok(SetClass::DirectPossible),
        3 => Ok(SetClass::TransitiveUnconditional),
        4 => Ok(SetClass::TransitivePossible),
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
    fn category_membership_is_transitive_through_subcategories() {
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
            vec![2, 3],
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
                "Template:Categorizer",
                "<includeonly>[[Category:Emitted]]</includeonly>",
            ),
            page(3, "Article", "{{Categorizer}}"),
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
            vec![3],
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
        let encoded = encode_sets(vec![
            LogicalSet {
                key: target_key,
                members: target,
                topology_bases: vec![base_key],
            },
            LogicalSet {
                key: base_key,
                members: base,
                topology_bases: Vec::new(),
            },
        ]);
        let target_position = encoded
            .iter()
            .position(|entry| entry.key == target_key)
            .unwrap();
        assert!(encoded[target_position].base_offset > 0);
        assert_eq!(
            encoded[target_position - encoded[target_position].base_offset as usize].key,
            base_key,
        );
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
    fn archive_build_streams_only_latest_revisions_and_reports_misses() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("sample.swdump");
        let titles = temporary.path().join("sample.swtitle");
        let sidecar = temporary.path().join("sample.swrefs");
        let mut writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 4096)
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
            writer
                .write(&revision(page_id, page_id * 10 + 2, 200, latest))
                .unwrap();
            if !older.is_empty() {
                writer
                    .write(&revision(page_id, page_id * 10 + 1, 100, older))
                    .unwrap();
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
        crate::title_index::build(&archive, &titles).unwrap();
        let stats = build(&archive, &titles, &sidecar).unwrap();
        assert_eq!(stats.source_pages, 5);
        assert_eq!(stats.redirect_pages, 2);
        // One explicit missing template plus the template used to compute the
        // dynamic category target.  The older revision's missing target is
        // deliberately absent.
        assert_eq!(stats.unresolved_static_edges, 2);
        assert!(stats.unresolved_dynamic_targets >= 1);
        let index = BackrefIndex::open(&sidecar).unwrap();
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
        build(&archive, &titles, &second).unwrap();
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            std::fs::read(&second).unwrap(),
        );
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
    fn sidecar_rejects_forged_depths_and_absurd_bitmap_counts() {
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

        let mut bad_depth = original.clone();
        bad_depth[HEADER_BYTES + 10] = 1;
        std::fs::write(&path, bad_depth).unwrap();
        assert!(BackrefIndex::open(&path).is_err());

        let mut bad_count = original;
        let payload_offset =
            u64::from_le_bytes(bad_count[40..48].try_into().unwrap()) as usize;
        bad_count[payload_offset..payload_offset + 10]
            .copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]);
        std::fs::write(&path, bad_count).unwrap();
        let index = BackrefIndex::open(&path).unwrap();
        assert!(index.members(logical[0].0).is_err());
    }
}
