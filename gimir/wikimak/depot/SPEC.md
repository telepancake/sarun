# wikimak-depot — spec

Storage substrate for chains of refPrefix-compressed zstd frames. One
chain per page. The depot stores opaque bytes; the caller (wikipedia
layer) does all compression and decompression.

## The shape of a chain on disk

Each chain has, at any given moment:

- **One f0 frame** — the current head revision, standalone zstd-encoded
  with the chain's pretrained dict. Always present once the chain has
  been prepended to.
- **One f1 frame** — the accumulator: zero or more records (newest-first),
  zstd-encoded with refPrefix anchored against f0's record. Always
  present after at least two prepends; on the first prepend, the chain has
  f0 but no f1.
- **Zero or more cold frames** — sealed accumulators, linked. Each cold
  frame's refPrefix anchor is the LAST record (oldest, by chain order)
  of the frame immediately newer than it. So the newest cold frame is
  anchored against f1's oldest record; the next-older cold frame is
  anchored against the newest cold frame's oldest record; and so on.

Chain walk newest-first: `f0 → f1 → cold_N → cold_{N-1} → … → cold_1`.
The chain length is `2 + (number of seal events)`. There is no fixed
upper bound; cold grows as long as the chain is active.

## Tier files on disk

Under `<root>/`:

```
index                    fixed-size mmap'd array; entry per chain_id
f0/file-NNNN             append-only data files; one is the current write target
f1/file-NNNN             same
cold/cold                ONE file per depot instance, append-only
```

### Index

Fixed-size file. Each entry is 8 bytes: `[u32 file_id LE, u32 offset LE]`,
pointing at the chain's f0 frame. `(0, 0)` = chain has no frames yet.
File size = `8 * max_chain_id`. mmap'd r/w. Writing an index entry is one
aligned 8-byte pwrite, atomic on real filesystems.

### Frame format (same in all three tiers)

A frame on disk:

```
[u64 chain_id LE | u64 next_pointer LE | zstd frame bytes]
```

`next_pointer` is `[u32 file_id LE | u32 offset LE]` packed as one u64.
For f0 it points at f1; for f1 it points at the newest cold frame (or
`(0,0)` if no seals have happened); for cold[k] it points at cold[k-1]
(or `(0,0)` if k == 1). The zstd bytes are opaque to the depot.

The depot NEVER decompresses zstd. Encoding and decoding are entirely
the caller's responsibility.

### Cold file

ONE file per depot. Append-only. Never evicted, never compacted. When
the wiki instance is deleted, the cold file is `unlink`'d as a unit.
This is the entire reason cold is special: the per-instance delete
operation has to be cheap, and putting cold in one file makes it `rm`.

`file_id` for cold pointers is always 0 (one file, one id).

### f0 / f1 files

Each tier is a directory of files. New files are allocated when the
current write target hits a size threshold (default 1 GiB; configurable).
Files are named `file-NNNN` where NNNN is a 4-digit zero-padded
globally-unique-within-tier id.

Each f0/f1 file has an in-memory `bytes_deprecated` counter. Bumped
whenever a frame in that file is orphaned by a prepend (see Operations
below). The counter is not persisted; on `open` it is rebuilt by walking
the file's frames and checking each one's chain_id against the index.

When a file's deprecation ratio exceeds a threshold (default 0.5), it
becomes a victim for eviction (see Eviction below).

## Public API

```rust
pub struct Depot { /* opaque */ }

pub struct DepotConfig {
    pub root: PathBuf,
    pub max_chain_id: u64,
    pub file_size_threshold: u64,       // default ~1 GiB; rolls to a fresh f0/f1 file past this
    pub eviction_dead_ratio: f32,       // default 0.5
}

impl Depot {
    pub fn open(cfg: DepotConfig) -> Result<Self>;

    /// Replace the chain's f0 and f1 with new bytes. The depot:
    ///   1. Reads the chain's current state via the index.
    ///   2. If `seal_old_f1` is true, takes the old f1's bytes verbatim
    ///      and writes them as a new cold frame ahead of the existing
    ///      cold chain head.
    ///   3. Writes new f1 (pointer = new cold head if step 2 happened,
    ///      else inherits old f1's next-pointer).
    ///   4. Writes new f0 (pointer = new f1's location).
    ///   5. Flips the index entry.
    ///   6. Bumps bytes_deprecated on the files that held old f0/f1.
    ///
    /// `new_f1_bytes` must be `None` on the very first prepend (no f1
    /// exists yet) and `Some` otherwise. The depot does NOT compute
    /// `new_f0`/`new_f1` — the caller encoded them.
    pub fn prepend(
        &self,
        chain_id: u64,
        new_f0_bytes: &[u8],
        new_f1_bytes: Option<&[u8]>,
        seal_old_f1: bool,
    ) -> Result<()>;

    /// Read the current f0 frame bytes (zstd + 16-byte header stripped).
    /// Returns Err if the chain has no f0 yet.
    pub fn read_f0(&self, chain_id: u64) -> Result<Vec<u8>>;

    /// Read the current f1 frame bytes (zstd, header stripped).
    /// Returns Ok(None) if the chain has no f1 yet.
    pub fn read_f1(&self, chain_id: u64) -> Result<Option<Vec<u8>>>;

    /// Iterate the cold frames newest-first. Yields (zstd_bytes,).
    pub fn cold_iter(&self, chain_id: u64) -> Result<ColdIter<'_>>;

    pub fn flush(&self) -> Result<()>;

    /// Unlink the depot's cold file, all f0/f1 files, and zero the index.
    /// Used when the caller wants to delete the wiki instance.
    pub fn delete_all(self) -> Result<()>;
}

pub struct ColdIter<'a> { /* opaque */ }
impl<'a> Iterator for ColdIter<'a> {
    type Item = Result<Vec<u8>>;
}
```

There is no `read_frame_at(location)` and no exposed `FrameRef`. The
on-disk pointer format is internal; callers walk via `read_f0`,
`read_f1`, `cold_iter`. The depot manages all location bookkeeping.

## Operations

### Prepend (no seal)

Caller has just encoded `new_f0` (R standalone-dict) and `new_f1`
(records refPrefix-against-R). Caller invokes
`prepend(chain_id, new_f0, Some(new_f1), seal_old_f1=false)`.

The depot:
1. Reads index entry → old_f0_ref (or (0,0)).
2. If old_f0_ref == (0,0): first ever prepend.
   - Caller MUST pass `new_f1_bytes = None`. If they pass Some, error.
   - Write new f0 to current f0 file with `next_pointer = (0, 0)`. Flip
     index. Done.
3. Otherwise (chain has prior state):
   - Caller MUST pass `new_f1_bytes = Some(...)`. If None, error.
   - Read old f0's `next_pointer` (8 bytes preceding zstd) → old_f1_ref.
   - Inherit cold pointer from old f1: pread the 8-byte next_pointer of
     old f1's frame → `cold_head`.
   - Write new f1 to current f1 file with `next_pointer = cold_head`.
     Get `new_f1_loc`.
   - Write new f0 to current f0 file with `next_pointer = new_f1_loc`.
     Get `new_f0_loc`.
   - Flip index[chain_id] = new_f0_loc. (Atomic 8-byte mmap write.)
   - Bump `bytes_deprecated` on old_f0's file by old_f0's full frame
     size (header + zstd). Same for old_f1.

### Prepend with seal

Same call, but `seal_old_f1 = true`. The depot:
1. Reads old_f0_ref from index. If (0,0), error (can't seal on first
   prepend).
2. Reads old_f0's next_pointer → old_f1_ref. If (0,0), error (can't seal
   with no f1).
3. Reads old f1's full frame bytes: `[chain_id_le | next_pointer_le |
   zstd_bytes]` where next_pointer is the old cold head.
4. Writes the new cold frame to the cold file:
   `[chain_id | next_pointer = old_cold_head | old_f1_zstd_bytes_verbatim]`.
   Get `new_cold_off`. The old f1's zstd bytes are reused verbatim — no
   re-encode. The bytes are still refPrefix-anchored against the record
   that becomes new f1's sole content, which is correct.
5. Write new f1 to current f1 file with
   `next_pointer = (cold_file_id, new_cold_off)`. Get `new_f1_loc`.
6. Write new f0 to current f0 file with
   `next_pointer = new_f1_loc`. Get `new_f0_loc`.
7. Flip index.
8. Bump `bytes_deprecated` on old f0's file (by old_f0_size) and old f1's
   file (by old_f1_size).

### Eviction of an f0 or f1 file

Triggered when any file in the tier has `bytes_deprecated / file_size >
eviction_dead_ratio`. The depot may run this opportunistically during
`flush` or on a dedicated `maybe_evict` call (left to implementer; tests
don't pin which). The eviction algorithm:

Let `V` = the victim file (in tier T ∈ {f0, f1}).
Let `L` = the current write target in tier T.

For each frame at offset `O` in `V`, walk sequentially:

1. Read the 8-byte chain_id from the frame header.
2. Look up `index[chain_id]` → `current_f0_loc`.
3. If T == f0:
   - If `current_f0_loc == (V_id, O)`: LIVE. Else: DEAD.
4. If T == f1:
   - Read current_f0_loc's frame; its next_pointer is `current_f1_loc`.
   - If `current_f1_loc == (V_id, O)`: LIVE. Else: DEAD.
5. If LIVE:
   - Copy the entire frame (header + zstd, verbatim) to `L`'s tail. Get
     `new_off`.
   - If `L` would overflow `file_size_threshold`, roll `L` first.
   - Patch the pointer that referenced `(V_id, O)`:
     - If T == f0: index[chain_id] = (L_id, new_off). One mmap write.
     - If T == f1: pwrite 8 bytes at `current_f0_loc.offset + 8`
       (the next_pointer position in f0's frame header).
6. If DEAD: skip.

After the walk: fsync L, fsync touched f0 files (only if T == f1), fsync
the index (only if T == f0). Then `unlink(V)`. Reset L's in-memory
deprecation counter for V (it's gone).

Cost per live frame: one verbatim copy + one 8-byte pwrite. Cost per dead
frame: two 8-byte preads (chain_id, then the f0 next_pointer check for
T == f1). No decompression. No re-encoding.

### Open

1. `mkdir -p` the tier dirs.
2. mmap the index file (creating zero-filled if absent at `max_chain_id *
   8` bytes).
3. List existing files in f0/, f1/, cold/. Allocate the next file id =
   `max + 1` per tier.
4. Rebuild in-memory `bytes_deprecated` for each f0/f1 file: walk frames,
   look up index, count dead.
5. Open the cold file (creating empty if absent).
6. Done.

Open cost: O(total f0+f1 bytes on disk) for the deprecation rebuild. For
a small instance this is microseconds; for enwiki (~90 GB f0 + similar
f1) it's tens of seconds. Acceptable for a personal-machine tool. (A
sidecar persisting the counter is permitted by the spec if the
implementer wants faster open; not required.)

### Read paths

`read_f0(chain_id)`:
1. Look up index[chain_id]. If (0,0): Err NoFrame.
2. pread the frame's header (16 bytes) at the f0 location. Get the
   zstd-bytes length from the next frame's offset or from a stored
   length. WAIT — we don't store frame length in the header. Implementer:
   either store it (extend the header to `[chain_id | next_pointer |
   u32 zstd_len]` = 20 bytes), or use `zstd::find_frame_compressed_size`
   on a streaming read. Pick one and pin it.

   **Resolution (pinned by SPEC):** extend frame header to 20 bytes:
   `[u64 chain_id | u64 next_pointer | u32 zstd_len]`. Saves repeated
   zstd-header probing on every read. Cost: 4 bytes per frame.
3. pread `zstd_len` bytes for the zstd payload. Return.

`read_f1(chain_id)`:
1. Look up index → f0_loc. If (0,0): Err NoFrame.
2. pread f0's header (16 bytes). Get its next_pointer = f1_loc.
3. If f1_loc == (0,0): return Ok(None).
4. pread f1's header, then its zstd bytes. Return Ok(Some(zstd_bytes)).

`cold_iter(chain_id)`:
1. Look up index → f0_loc. Read f0's next_pointer → f1_loc.
2. If f1_loc == (0,0): return empty iter.
3. Read f1's next_pointer → cold_head.
4. If cold_head == (0,0): return empty iter.
5. Walk cold frames via next_pointer chain. Each yields its zstd bytes.

## Crash-safety contract

- Same as strpool's: a `flush()` makes all preceding prepends durable. A
  crash without flush may lose recent prepends. There is NO recovery
  code, NO journal, NO magic, NO checksums.
- Prepend ordering: write the new cold frame (if sealing) → fsync cold;
  write the new f1 → fsync f1; write the new f0 → fsync f0; flip index
  → fsync index. The index flip is the atomic commit point.
- A crash before the flip: the index still points at old state, all old
  frames are intact and reachable, the orphan bytes in the new locations
  are dead and will be naturally deprecated on the next prepend touching
  those files.
- A crash mid-eviction: the source frame is still in V (not yet
  unlinked), pointers still point at it. The duplicate in L is dead and
  will be deprecated. Restart of eviction is safe and idempotent (it
  will find the now-live frame at L and not re-copy).

## Limits

- `chain_id < max_chain_id`. Opening with a different `max_chain_id`
  than the index file's recorded size is an error.
- One frame's zstd bytes < `u32::MAX` (zstd_len is u32). Wikipedia
  revisions max out around 2 MiB; not a concern.
- Per-file size < `u32::MAX` (offset is u32). With ~1 GiB file-size
  threshold this is well within bounds.
- One depot instance = one wiki. The cold file holds only this
  instance's cold frames. The caller runs one depot per wiki.

## Out of scope — do NOT add

- Tombstone bitmaps, sidecars, journals, write-ahead logs.
- Magic numbers, CRCs, checksums anywhere on disk.
- Recovery commands, repair utilities, "fsck depot".
- Multi-frame-per-chain in f0 or f1. Each chain has exactly one f0 and
  zero-or-one f1.
- Chain walks via next_pointer chasing in f0 (there is no chain of f0s;
  f0 is one frame).
- Generic frame-anywhere relocation. The depot supports two location
  changes only: prepend (which always writes to the current write target)
  and eviction (which migrates frames from a victim file to the current
  write target in the same tier).
- Cross-tier GC. f0 is evicted only from f0; f1 only from f1; cold is
  never evicted, only `unlink`'d as a whole on instance delete.
- Auto-tuning of thresholds. Caller sets them at open and they don't
  change.
- Async, multithreading inside the depot. Single `&Depot` is callable
  by one client at a time; honor that with a single mutex.
- Built-in compression. The depot's job is to STORE the bytes the
  caller hands it, not to encode anything.
- Knowledge of refPrefix, dictionaries, or zstd content. The depot is
  byte-opaque past the frame header.
