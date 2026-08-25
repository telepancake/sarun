//! Flat current-title bindings and the sparse older-interval escape hatch.
//!
//! Slot `title_id` occupies exactly eight bytes:
//! `[page_id: u32 LE | valid_since_unix_seconds: u32 LE]`.
//! `page_id == 0` is the unbound state. The timestamp applies to either
//! state, so a current deletion is represented without an auxiliary row.
//! Only intervals older than the current slot belong in
//! [`OlderTitleIntervals`].

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use memmap2::{Mmap, MmapOptions};
use rusqlite::{params, Connection};

pub const TITLE_SLOT_BYTES: u64 = 8;
pub const UNBOUND_PAGE_ID: u32 = 0;
const GENERATION_FLAG_ID: i64 = 1;

#[cfg(test)]
static APPLY_COUNTER: std::sync::Mutex<
    Option<(PathBuf, std::sync::Arc<std::sync::atomic::AtomicU64>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_apply_counter(
    root: Option<(PathBuf, std::sync::Arc<std::sync::atomic::AtomicU64>)>,
) {
    *APPLY_COUNTER.lock().unwrap() = root;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TitleBinding {
    pub page_id: u32,
    pub valid_since: u32,
}

impl TitleBinding {
    pub fn bound(page_id: u32, valid_since: u32) -> io::Result<Self> {
        if page_id == UNBOUND_PAGE_ID {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page id zero is reserved for the unbound state",
            ));
        }
        Ok(Self {
            page_id,
            valid_since,
        })
    }

    pub const fn unbound(valid_since: u32) -> Self {
        Self {
            page_id: UNBOUND_PAGE_ID,
            valid_since,
        }
    }

    pub fn try_bound(page_id: u64, valid_since: i64) -> io::Result<Self> {
        let page_id = checked_page_id(page_id, false)?;
        let valid_since = checked_unix_seconds(valid_since)?;
        Self::bound(page_id, valid_since)
    }

    pub fn try_unbound(valid_since: i64) -> io::Result<Self> {
        Ok(Self::unbound(checked_unix_seconds(valid_since)?))
    }

    pub const fn page_id(self) -> Option<u32> {
        if self.page_id == UNBOUND_PAGE_ID {
            None
        } else {
            Some(self.page_id)
        }
    }

    fn encode(self) -> [u8; TITLE_SLOT_BYTES as usize] {
        let mut bytes = [0; TITLE_SLOT_BYTES as usize];
        bytes[..4].copy_from_slice(&self.page_id.to_le_bytes());
        bytes[4..].copy_from_slice(&self.valid_since.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Self {
        Self {
            page_id: u32::from_le_bytes(bytes[..4].try_into().expect("four bytes")),
            valid_since: u32::from_le_bytes(bytes[4..].try_into().expect("four bytes")),
        }
    }
}

/// Sparse history older than the continuously-valid current slot.
pub trait OlderTitleIntervals {
    fn page_at(&self, title_id: u64, unix_seconds: u32) -> io::Result<Option<u32>>;
}

/// Mutation surface needed when title ids are rebuilt by re-sharding.
pub trait OlderTitleIntervalsMut: OlderTitleIntervals {
    fn replace(&mut self, title_id: u64, intervals: &[OlderTitleInterval]) -> io::Result<()>;

    /// Atomically rewrite every referenced title id. `remap` must contain
    /// every id currently present in the overflow store.
    fn remap_title_ids(&mut self, remap: &[(u64, u64)]) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OlderTitleInterval {
    pub start: u32,
    pub end: u32,
    pub page_id: u32,
}

impl OlderTitleInterval {
    pub fn try_new(start: i64, end: i64, page_id: u64) -> io::Result<Self> {
        let start = checked_unix_seconds(start)?;
        let end = checked_unix_seconds(end)?;
        if start >= end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "older title interval must have positive duration",
            ));
        }
        Ok(Self {
            start,
            end,
            page_id: checked_page_id(page_id, true)?,
        })
    }
}

/// Initial sparse-overflow implementation. It is deliberately hidden behind
/// the traits above so replacing it with a second flat structure does not
/// change the current-slot reader.
pub struct SqliteOlderTitleIntervals<'a> {
    conn: &'a mut Connection,
}

impl<'a> SqliteOlderTitleIntervals<'a> {
    pub fn open(conn: &'a mut Connection) -> io::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS title_interval_overflow (
                title_id INTEGER NOT NULL,
                start_s INTEGER NOT NULL,
                end_s INTEGER NOT NULL,
                page_id INTEGER NOT NULL,
                PRIMARY KEY(title_id, start_s)
             );
             CREATE INDEX IF NOT EXISTS idx_title_interval_overflow_lookup
               ON title_interval_overflow(title_id, start_s, end_s);
             CREATE TABLE IF NOT EXISTS title_slot_state (
               singleton INTEGER PRIMARY KEY CHECK(singleton=1),
               generation INTEGER NOT NULL
             );",
        )
        .map_err(sqlite_io)?;
        Ok(Self { conn })
    }

    /// Remap inside a caller-owned transaction. Generation integration uses
    /// this form so overflow ids, other title-id consumers, and the selected
    /// title-slot generation become visible in one SQLite commit, after the
    /// new flat file has already been made durable.
    pub fn remap_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
        remap: &[(u64, u64)],
    ) -> io::Result<()> {
        transaction
            .execute_batch(
                "DROP TABLE IF EXISTS temp.title_slot_remap;
                 CREATE TEMP TABLE title_slot_remap (
                   old_id INTEGER PRIMARY KEY,
                   new_id INTEGER NOT NULL UNIQUE
                 );",
            )
            .map_err(sqlite_io)?;
        {
            let mut insert = transaction
                .prepare("INSERT INTO title_slot_remap(old_id,new_id) VALUES(?1,?2)")
                .map_err(sqlite_io)?;
            for &(old_id, new_id) in remap {
                checked_sqlite_id(old_id)?;
                checked_sqlite_id(new_id)?;
                insert
                    .execute(params![old_id as i64, new_id as i64])
                    .map_err(sqlite_io)?;
            }
        }
        let missing: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM title_interval_overflow o
                 LEFT JOIN title_slot_remap m ON m.old_id=o.title_id
                 WHERE m.old_id IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_io)?;
        if missing != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "title-id remap does not cover the overflow store",
            ));
        }
        transaction
            .execute(
                "UPDATE title_interval_overflow
                 SET title_id=-1-(SELECT new_id FROM title_slot_remap m
                                  WHERE m.old_id=title_interval_overflow.title_id)",
                [],
            )
            .map_err(sqlite_io)?;
        transaction
            .execute(
                "UPDATE title_interval_overflow SET title_id=-1-title_id WHERE title_id<0",
                [],
            )
            .map_err(sqlite_io)?;
        Ok(())
    }
}

/// Generation selector for flat slots coupled to sparse overflow metadata.
/// Generation files are immutable; publishing one does not affect readers
/// until [`Self::select`] commits in the same transaction as overflow/id
/// changes.
pub struct TitleSlotGenerations;

impl TitleSlotGenerations {
    /// Remove immutable slot generations other than the selector's current
    /// value. Only exact recognized filenames are touched; unrelated files
    /// and the selected forward/reverse pair are never deletion candidates.
    pub fn collect_unselected(root: impl AsRef<Path>, selected: u32) -> io::Result<()> {
        validate_generation(selected)?;
        for entry in std::fs::read_dir(root.as_ref())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let generation = name
                .strip_prefix("title-slots.")
                .or_else(|| name.strip_prefix("page-titles."))
                .and_then(|raw| raw.parse::<u32>().ok());
            if generation.is_some_and(|generation| generation != selected) {
                std::fs::remove_file(entry.path())?;
            }
        }
        sync_dir(Some(root.as_ref()))
    }

    pub fn prepare_snapshot<'tx, 'conn>(
        root: impl AsRef<Path>,
        generation: u32,
        transaction: &'tx rusqlite::Transaction<'conn>,
    ) -> io::Result<TitleSnapshotBuilder<'tx, 'conn>> {
        let path = generation_path(root.as_ref(), generation)?;
        TitleSnapshotBuilder::new_generation(path, transaction)
    }

    pub fn prepare_remapped(
        root: impl AsRef<Path>,
        generation: u32,
        old: &TitleSlots,
        remap: &[(u64, u64)],
    ) -> io::Result<PreparedTitleSlots> {
        let path = generation_path(root.as_ref(), generation)?;
        let mut prepared = TitleSlots::prepare_remapped(path, old, remap)?;
        let reverse_path = page_generation_path(root.as_ref(), generation)?;
        let reverse_tmp = tmp_path(&reverse_path);
        let reverse_file = prepare_file(&reverse_tmp)?;
        for &(old_id, new_id) in remap {
            let Some(page_id) = old.current(old_id).and_then(TitleBinding::page_id) else {
                continue;
            };
            let end = (page_id as u64 + 1) * TITLE_SLOT_BYTES;
            if reverse_file.metadata()?.len() < end {
                reverse_file.set_len(end)?;
            }
            let stored = new_id.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "title id cannot fit reverse slot",
                )
            })?;
            reverse_file.write_all_at(&stored.to_le_bytes(), page_id as u64 * TITLE_SLOT_BYTES)?;
        }
        reverse_file.sync_all()?;
        prepared.immutable_generation = true;
        prepared.reverse = Some((reverse_path, reverse_tmp));
        Ok(prepared)
    }

    pub fn select(transaction: &rusqlite::Transaction<'_>, generation: u32) -> io::Result<()> {
        validate_generation(generation)?;
        transaction
            .execute(
                "INSERT OR REPLACE INTO title_slot_state(singleton,generation)
                 VALUES(?1,?2)",
                params![GENERATION_FLAG_ID, generation],
            )
            .map_err(sqlite_io)?;
        Ok(())
    }

    pub fn selected(conn: &Connection) -> io::Result<u32> {
        let generation: i64 = conn
            .query_row(
                "SELECT generation FROM title_slot_state WHERE singleton=?1",
                [GENERATION_FLAG_ID],
                |row| row.get(0),
            )
            .map_err(sqlite_io)?;
        let generation: u32 = generation.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid title-slot generation")
        })?;
        validate_generation(generation)?;
        Ok(generation)
    }

    pub fn open_selected(root: impl AsRef<Path>, conn: &Connection) -> io::Result<TitleSlots> {
        let generation = Self::selected(conn)?;
        TitleSlots::open(generation_path(root.as_ref(), generation)?)
    }

    pub fn open_selected_page_titles(
        root: impl AsRef<Path>,
        conn: &Connection,
    ) -> io::Result<PageTitleSlots> {
        let generation = Self::selected(conn)?;
        PageTitleSlots::open(page_generation_path(root.as_ref(), generation)?)
    }

    /// Apply a small idempotent current-binding intent to the selected
    /// generation and fsync both directions. The caller must durably persist
    /// the intent before this call and clear it only after success.
    pub fn apply_current(
        root: impl AsRef<Path>,
        generation: u32,
        changes: &[(u64, TitleBinding)],
    ) -> io::Result<(TitleSlots, PageTitleSlots)> {
        #[cfg(test)]
        if let Some((watched, count)) = &*APPLY_COUNTER.lock().unwrap() {
            if watched == root.as_ref() {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let forward_path = generation_path(root.as_ref(), generation)?;
        let reverse_path = page_generation_path(root.as_ref(), generation)?;
        let forward = OpenOptions::new().read(true).write(true).open(&forward_path)?;
        let reverse = OpenOptions::new().read(true).write(true).open(&reverse_path)?;
        let before = TitleSlots::open(&forward_path)?;
        // Phase 1: clear every pre-batch reverse owner. Interleaving clears
        // with sets corrupts swaps/cycles (a later clear can erase an earlier
        // new owner).
        for &(title_id, _binding) in changes {
            if let Some(old_page) = before.current(title_id).and_then(TitleBinding::page_id) {
                reverse.write_all_at(&0u64.to_le_bytes(), old_page as u64 * TITLE_SLOT_BYTES)?;
            }
        }
        // Phase 2: install all forward bindings.
        for &(title_id, binding) in changes {
            let end = title_id
                .checked_add(1)
                .and_then(|slots| slots.checked_mul(TITLE_SLOT_BYTES))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "title id overflow"))?;
            if forward.metadata()?.len() < end {
                forward.set_len(end)?;
            }
            forward.write_all_at(&binding.encode(), title_id * TITLE_SLOT_BYTES)?;
        }
        // Phase 3: install every new reverse owner after all old owners
        // have been cleared.
        for &(title_id, binding) in changes {
            if let Some(page_id) = binding.page_id() {
                let reverse_end = (page_id as u64 + 1) * TITLE_SLOT_BYTES;
                if reverse.metadata()?.len() < reverse_end {
                    reverse.set_len(reverse_end)?;
                }
                let stored = title_id.checked_add(1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "title id cannot fit reverse slot")
                })?;
                reverse.write_all_at(&stored.to_le_bytes(), page_id as u64 * TITLE_SLOT_BYTES)?;
            }
        }
        forward.sync_all()?;
        reverse.sync_all()?;
        Ok((TitleSlots::open(forward_path)?, PageTitleSlots::open(reverse_path)?))
    }
}

impl OlderTitleIntervals for SqliteOlderTitleIntervals<'_> {
    fn page_at(&self, title_id: u64, unix_seconds: u32) -> io::Result<Option<u32>> {
        checked_sqlite_id(title_id)?;
        let page_id = self
            .conn
            .query_row(
                "SELECT page_id FROM title_interval_overflow
                 WHERE title_id=?1 AND start_s<=?2 AND end_s>?2
                 ORDER BY start_s DESC LIMIT 1",
                params![title_id as i64, unix_seconds],
                |row| row.get::<_, u32>(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(sqlite_io(other)),
            })?;
        Ok(page_id.filter(|&page_id| page_id != UNBOUND_PAGE_ID))
    }
}

impl OlderTitleIntervalsMut for SqliteOlderTitleIntervals<'_> {
    fn replace(&mut self, title_id: u64, intervals: &[OlderTitleInterval]) -> io::Result<()> {
        checked_sqlite_id(title_id)?;
        validate_intervals(intervals)?;
        let transaction = self.conn.transaction().map_err(sqlite_io)?;
        transaction
            .execute(
                "DELETE FROM title_interval_overflow WHERE title_id=?1",
                [title_id as i64],
            )
            .map_err(sqlite_io)?;
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO title_interval_overflow(title_id,start_s,end_s,page_id)
                     VALUES(?1,?2,?3,?4)",
                )
                .map_err(sqlite_io)?;
            for interval in intervals {
                insert
                    .execute(params![
                        title_id as i64,
                        interval.start,
                        interval.end,
                        interval.page_id
                    ])
                    .map_err(sqlite_io)?;
            }
        }
        transaction.commit().map_err(sqlite_io)
    }

    fn remap_title_ids(&mut self, remap: &[(u64, u64)]) -> io::Result<()> {
        let transaction = self.conn.transaction().map_err(sqlite_io)?;
        Self::remap_in_transaction(&transaction, remap)?;
        transaction.commit().map_err(sqlite_io)
    }
}

pub struct TitleSlots {
    path: PathBuf,
    map: Option<Mmap>,
}

impl TitleSlots {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        if len % TITLE_SLOT_BYTES != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "title-slot file length is not divisible by eight",
            ));
        }
        let map = if len == 0 {
            None
        } else {
            Some(unsafe { MmapOptions::new().map(&file)? })
        };
        // A prepared rebuild is invisible until rename. Once a valid main
        // file opens, any abandoned preparation can be discarded.
        match std::fs::remove_file(tmp_path(&path)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {} // read-only attachments may not be allowed to clean
        }
        Ok(Self { path, map })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn slot_count(&self) -> u64 {
        self.map
            .as_ref()
            .map_or(0, |map| map.len() as u64 / TITLE_SLOT_BYTES)
    }

    pub fn current(&self, title_id: u64) -> Option<TitleBinding> {
        let offset = title_id.checked_mul(TITLE_SLOT_BYTES)?;
        let end = offset.checked_add(TITLE_SLOT_BYTES)?;
        let map = self.map.as_ref()?;
        let bytes = map.get(offset as usize..end as usize)?;
        Some(TitleBinding::decode(bytes))
    }

    pub fn page_at(
        &self,
        title_id: u64,
        unix_seconds: u32,
        older: &impl OlderTitleIntervals,
    ) -> io::Result<Option<u32>> {
        if let Some(current) = self.current(title_id) {
            if unix_seconds >= current.valid_since {
                return Ok(current.page_id());
            }
        }
        older.page_at(title_id, unix_seconds)
    }

    pub fn prepare_rebuild(
        path: impl AsRef<Path>,
        bindings: &[(u64, TitleBinding)],
    ) -> io::Result<PreparedTitleSlots> {
        let path = path.as_ref().to_path_buf();
        let tmp_path = tmp_path(&path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let max_id = bindings.iter().map(|(id, _)| *id).max();
        let len = max_id.map_or(Ok(0), |id| {
            id.checked_add(1)
                .and_then(|slots| slots.checked_mul(TITLE_SLOT_BYTES))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "title id overflow"))
        })?;
        let file = prepare_file(&tmp_path)?;
        file.set_len(len)?;
        for &(title_id, binding) in bindings {
            file.write_all_at(&binding.encode(), title_id * TITLE_SLOT_BYTES)?;
        }
        file.sync_all()?;
        sync_dir(path.parent())?;
        Ok(PreparedTitleSlots {
            path,
            tmp_path,
            immutable_generation: false,
            reverse: None,
        })
    }

    pub fn prepare_remapped(
        path: impl AsRef<Path>,
        old: &TitleSlots,
        remap: &[(u64, u64)],
    ) -> io::Result<PreparedTitleSlots> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp_path = tmp_path(&path);
        let max_id = remap.iter().map(|(_, new_id)| *new_id).max();
        let len = slot_file_len(max_id)?;
        let file = prepare_file(&tmp_path)?;
        file.set_len(len)?;
        for &(old_id, new_id) in remap {
            let binding = old.current(old_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "remap names an absent old title id",
                )
            })?;
            file.write_all_at(&binding.encode(), new_id * TITLE_SLOT_BYTES)?;
        }
        file.sync_all()?;
        sync_dir(path.parent())?;
        Ok(PreparedTitleSlots {
            path,
            tmp_path,
            immutable_generation: false,
            reverse: None,
        })
    }

    pub fn atomic_rebuild(
        path: impl AsRef<Path>,
        bindings: &[(u64, TitleBinding)],
    ) -> io::Result<Self> {
        Self::prepare_rebuild(path, bindings)?.commit()
    }
}

/// Bounded full-snapshot writer. The caller owns `transaction` and must:
///
/// 1. stream titles through [`Self::push_title`];
/// 2. call [`Self::finish`] and publish the returned prepared file;
/// 3. update its selected-generation flag and other title-id consumers;
/// 4. commit `transaction`.
///
/// Thus a crash before step 4 leaves only an unselected generation; after
/// step 4 the complete flat file and its sparse overflow become visible
/// together.
pub struct TitleSnapshotBuilder<'tx, 'conn> {
    path: PathBuf,
    tmp_path: PathBuf,
    file: File,
    reverse_path: PathBuf,
    reverse_tmp_path: PathBuf,
    reverse_file: File,
    transaction: &'tx rusqlite::Transaction<'conn>,
}

impl<'tx, 'conn> TitleSnapshotBuilder<'tx, 'conn> {
    fn new_generation(
        path: PathBuf,
        transaction: &'tx rusqlite::Transaction<'conn>,
    ) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let slot_tmp_path = tmp_path(&path);
        let file = prepare_file(&slot_tmp_path)?;
        let generation = generation_from_path(&path)?;
        let root = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "generation path has no parent")
        })?;
        let reverse_path = page_generation_path(root, generation)?;
        let reverse_tmp_path = tmp_path(&reverse_path);
        let reverse_file = prepare_file(&reverse_tmp_path)?;
        transaction
            .execute("DELETE FROM title_interval_overflow", [])
            .map_err(sqlite_io)?;
        Ok(Self {
            path,
            tmp_path: slot_tmp_path,
            file,
            reverse_path,
            reverse_tmp_path,
            reverse_file,
            transaction,
        })
    }

    pub fn push_title(
        &mut self,
        title_id: u64,
        current_page_id: u64,
        current_since: i64,
        older: &[(i64, i64, u64)],
    ) -> io::Result<()> {
        checked_sqlite_id(title_id)?;
        let current = if current_page_id == UNBOUND_PAGE_ID as u64 {
            TitleBinding::try_unbound(current_since)?
        } else {
            TitleBinding::try_bound(current_page_id, current_since)?
        };
        let end = title_id
            .checked_add(1)
            .and_then(|slots| slots.checked_mul(TITLE_SLOT_BYTES))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "title id overflow"))?;
        if self.file.metadata()?.len() < end {
            self.file.set_len(end)?;
        }
        self.file
            .write_all_at(&current.encode(), title_id * TITLE_SLOT_BYTES)?;
        if let Some(page_id) = current.page_id() {
            let reverse_end = (page_id as u64 + 1) * TITLE_SLOT_BYTES;
            if self.reverse_file.metadata()?.len() < reverse_end {
                self.reverse_file.set_len(reverse_end)?;
            }
            let encoded_title_id = title_id.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "title id cannot fit reverse slot",
                )
            })?;
            self.reverse_file.write_all_at(
                &encoded_title_id.to_le_bytes(),
                page_id as u64 * TITLE_SLOT_BYTES,
            )?;
        }

        let mut previous_end = None;
        for &(start, end, page_id) in older {
            let interval = OlderTitleInterval::try_new(start, end, page_id)?;
            if interval.end > current.valid_since
                || previous_end.is_some_and(|prior| interval.start < prior)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "snapshot older intervals overlap or extend into the current binding",
                ));
            }
            self.transaction
                .execute(
                    "INSERT INTO title_interval_overflow(title_id,start_s,end_s,page_id)
                     VALUES(?1,?2,?3,?4)",
                    params![
                        title_id as i64,
                        interval.start,
                        interval.end,
                        interval.page_id
                    ],
                )
                .map_err(sqlite_io)?;
            previous_end = Some(interval.end);
        }
        Ok(())
    }

    pub fn finish(self) -> io::Result<PreparedTitleSlots> {
        self.file.sync_all()?;
        self.reverse_file.sync_all()?;
        sync_dir(self.path.parent())?;
        Ok(PreparedTitleSlots {
            path: self.path,
            tmp_path: self.tmp_path,
            immutable_generation: true,
            reverse: Some((self.reverse_path, self.reverse_tmp_path)),
        })
    }
}

pub struct PreparedTitleSlots {
    path: PathBuf,
    tmp_path: PathBuf,
    immutable_generation: bool,
    reverse: Option<(PathBuf, PathBuf)>,
}

impl PreparedTitleSlots {
    /// Durably publish this file. For a re-shard this makes the new
    /// generation available but does not select it; the caller next commits
    /// overflow/id remaps and its generation flag in one metadata transaction.
    pub fn commit(self) -> io::Result<TitleSlots> {
        if self.immutable_generation && self.path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "title-slot generation already exists",
            ));
        }
        if let Some((path, _)) = &self.reverse {
            if self.immutable_generation && path.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "page-title generation already exists",
                ));
            }
        }
        std::fs::rename(&self.tmp_path, &self.path)?;
        if let Some((path, tmp_path)) = &self.reverse {
            std::fs::rename(tmp_path, path)?;
        }
        sync_dir(self.path.parent())?;
        TitleSlots::open(self.path)
    }
}

/// Reverse of the current title slots: `page_id` directly indexes one u64.
/// Zero means no current title; otherwise the stored word is `title_id + 1`,
/// preserving title id zero without requiring relational reverse mappings.
pub struct PageTitleSlots {
    map: Option<Mmap>,
}

impl PageTitleSlots {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len % TITLE_SLOT_BYTES != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "page-title file length is not divisible by eight",
            ));
        }
        let map = if len == 0 {
            None
        } else {
            Some(unsafe { MmapOptions::new().map(&file)? })
        };
        match std::fs::remove_file(tmp_path(path)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
        Ok(Self { map })
    }

    pub fn current_title_id(&self, page_id: u32) -> Option<u64> {
        let offset = page_id as usize * TITLE_SLOT_BYTES as usize;
        let bytes = self
            .map
            .as_ref()?
            .get(offset..offset + TITLE_SLOT_BYTES as usize)?;
        let stored = u64::from_le_bytes(bytes.try_into().expect("eight bytes"));
        stored.checked_sub(1)
    }
}

fn validate_intervals(intervals: &[OlderTitleInterval]) -> io::Result<()> {
    let mut previous_end = None;
    for interval in intervals {
        if interval.start >= interval.end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "older title interval must have positive duration",
            ));
        }
        if previous_end.is_some_and(|end| interval.start < end) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "older title intervals overlap or are unsorted",
            ));
        }
        previous_end = Some(interval.end);
    }
    Ok(())
}

fn checked_sqlite_id(id: u64) -> io::Result<()> {
    if id > i64::MAX as u64 {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "title id exceeds SQLite integer",
        ))
    } else {
        Ok(())
    }
}

fn checked_page_id(page_id: u64, allow_unbound: bool) -> io::Result<u32> {
    let page_id: u32 = page_id
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "page id exceeds u32"))?;
    if !allow_unbound && page_id == UNBOUND_PAGE_ID {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "page id zero is reserved for the unbound state",
        ));
    }
    Ok(page_id)
}

fn checked_unix_seconds(unix_seconds: i64) -> io::Result<u32> {
    unix_seconds.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "title-binding timestamp is outside the u32 Unix-second range",
        )
    })
}

fn sqlite_io(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}

fn prepare_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)
}

fn slot_file_len(max_id: Option<u64>) -> io::Result<u64> {
    max_id.map_or(Ok(0), |id| {
        id.checked_add(1)
            .and_then(|slots| slots.checked_mul(TITLE_SLOT_BYTES))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "title id overflow"))
    })
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

fn validate_generation(generation: u32) -> io::Result<()> {
    if generation == 0 {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "title-slot generation zero is reserved",
        ))
    } else {
        Ok(())
    }
}

fn generation_path(root: &Path, generation: u32) -> io::Result<PathBuf> {
    validate_generation(generation)?;
    Ok(root.join(format!("title-slots.{generation}")))
}

fn page_generation_path(root: &Path, generation: u32) -> io::Result<PathBuf> {
    validate_generation(generation)?;
    Ok(root.join(format!("page-titles.{generation}")))
}

fn generation_from_path(path: &Path) -> io::Result<u32> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid title-slot generation path",
            )
        })?;
    let suffix = name.strip_prefix("title-slots.").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "non-generation title-slot path",
        )
    })?;
    let generation: u32 = suffix.parse().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid title-slot generation")
    })?;
    if generation.to_string() != suffix {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "non-canonical title-slot generation",
        ));
    }
    validate_generation(generation)?;
    Ok(generation)
}

fn sync_dir(dir: Option<&Path>) -> io::Result<()> {
    if let Some(dir) = dir {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}
