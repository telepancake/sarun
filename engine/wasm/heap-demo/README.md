# Sliced tmpfs + host-owned allocator + fallocate reclaim

Proves the memory model: pick a max per-process heap (1 GiB), slice one sparse
tmpfs file into per-process linear-memory chunks, and let the host own the heap so
freed memory is returned to the OS with `fallocate(PUNCH_HOLE)`.

```
cd guest && RUSTFLAGS="-C link-arg=--import-memory -C link-arg=--export=__heap_base \
  -C link-arg=--export=__stack_pointer -C link-arg=--initial-memory=8388608 \
  -C link-arg=--max-memory=1073741824" cargo build --release --target wasm32-unknown-unknown
cd ../host && cargo run --release
```

## What it shows

- **Guest** (`guest/`): a `#[global_allocator]` whose `alloc`/`dealloc` are host
  imports (`env.host_alloc`/`host_dealloc`); memory is imported (`--import-memory`),
  so the host backs it; `__heap_base` exported so the host knows where the heap
  starts. So the host is the *source of truth* for the live-set — no guessing
  dlmalloc internals. (This is the "supply alloc callbacks" answer.)
- **Host** (`host/`): one tmpfs file `ftruncate`'d to `NPROC * 1 GiB` (sparse);
  each process gets `mmap`'d chunk `i*1GiB` handed to `Memory::new_static`; a
  host-owned first-fit+coalesce heap allocates from `__heap_base` up, growing the
  (sparse) memory lazily; on `dealloc` it punches the freed block's page-aligned
  range (`fallocate(PUNCH_HOLE)` for file storage + `madvise(MADV_DONTNEED)` for
  RSS). Result: alloc 64 MiB -> blocks jump; free -> blocks drop back.

## Honest notes

- The host-owned allocator means an import crossing per alloc/free — fine for the
  mechanism, but a hot allocation path would want a hybrid (in-wasm allocator for
  small/fast allocations; host import only for large blocks or a periodic trim
  that punches fully-dead page runs). The reclaim primitive is the same either way.
- A real blob opts in by setting its `#[global_allocator]` to this trampoline.
- Reclaim is page-granular (can't compact — linear-memory pointers are absolute).
