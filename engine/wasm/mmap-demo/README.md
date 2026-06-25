# mmap-as-is backing store with sparse hole-punching — reference

Proves the storage half of the single-backing-file checkpoint (PORTING-WASM.md):
a `MAP_SHARED` file used **as-is** as live mutable storage — no serialization —
kept **sparse** by releasing backing for unused regions, persisting zero-copy
across unmap/remap.

```
cargo run --release
# ftruncate 256KiB -> 0 blocks (sparse)
# touch 2 pages    -> 16 blocks (only touched pages cost storage)
# punch page 40    -> 8 blocks  (storage released)
# munmap + remap   -> page 0 still 0xAB (state persists, zero-copy)
```

## Primitives (precise)

- **Live store:** `mmap(MAP_SHARED)` over a file `ftruncate`'d to the address
  window. Writes go straight to the file's pages; a checkpoint is `msync` (or just
  letting the kernel flush) — no copy.
- **Release storage (punch holes):** `fallocate(FALLOC_FL_PUNCH_HOLE |
  FALLOC_FL_KEEP_SIZE, off, len)` for a disk file; `madvise(MADV_REMOVE)` is the
  tmpfs/shmem equivalent. This frees backing blocks; the region reads back as zero.
- **Drop cache (not storage):** `madvise(MADV_DONTNEED)` on a file map only evicts
  the cached pages (re-read from file on next touch). Use it to drop a stale page
  from the live mapping *after* punching. It does NOT free file blocks on a disk file.

## How it plugs into the checkpoint

The single backing file is laid out as: a fixed-layout header (mutable globals
incl. `__stack_pointer`, fd-table metadata, pipe ring offsets/cursors), then
page-aligned per-process **linear-memory** regions, then file-backed **pipe
buffers**. Each wasm instance's linear memory and each in-memory pipe is an mmap
view into this one file, so the running system mutates the file in place. Unused
linear-memory pages (a blob using 2 MiB of a 4 GiB-capable space) and freed pipe
bytes are hole-punched, so the file's real footprint tracks live pages.

## The remaining blocker (honest)

wasmi 2.0 owns its linear-memory buffer internally (`CoreMemory`, grown via
`Vec`/`Box<[u8]>`); it exposes `data`/`data_mut`/`data_ptr`/`grow` but **no hook
to back a memory with host-provided (mmap'd) storage**. So "the file is the live
linear memory" needs a wasmi patch (vendor + patch, like brush/uu_*/n2/kati) to
accept an external memory backing. Until then a checkpoint of guest memory is a
copy into the file rather than zero-copy. The host fd table / pipe buffers can be
mmap-as-is today (they're our own allocations).
