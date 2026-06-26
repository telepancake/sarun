# Porting brush + its builtins to a wasm blob run under wasmi

Goal: compile the vendored shell stack (brush-core/parser/builtins) and every
builtin (coreutils, find/xargs, kati, n2) to WebAssembly and run them
**in-process under [wasmi](https://github.com/wasmi-labs/wasmi)** — a pure-Rust
interpreter, so the engine stays a single static-musl binary. The collective
blob must be **asyncify-able**: `wasm-opt --asyncify` instruments it (and its
imports) so the guest can suspend/resume at any host call — the lever sarun needs
to checkpoint a running command. Semantically, running things via brush must not
change.

## The shift, and why it *simplifies*

The native in-process builtins (`engine/src/brush.rs`) jump through hoops so many
utils can share one process without corrupting each other: each coreutil is
wrapped as a `SimpleCommand` with an injected logical-I/O entry (write the
shell's `OpenFile` sink, never fd 1/2), a logical-cwd hook (never `chdir`), and a
fresh thread per call for uucore localization isolation. That whole layer exists
to fake the isolation a process would have given for free.

**A wasm blob doesn't need to cosplay a brush builtin.** It runs in its own
linear memory with its own stdio via WASI imports the host maps to the pipeline
fds — the isolation is intrinsic. So:

- Utils compile as **ordinary programs** (their plain `uumain`), not builtins.
  This **deletes** the `SimpleCommand` / logical-I/O / logical-cwd patch layer
  for ported utils (the "remove local diffs" half of the task).
- brush dispatches `cat` as a normal external command; at the exec boundary the
  engine runs the matching wasm blob **in-process via wasmi** instead of
  fork+exec. No fork, full isolation, parity.
- **Host imports are how wasm talks to the world** — WASI is itself just a set of
  host imports. Privileged/non-WASI ops (chmod/chown/uid/gid, process spawn, net)
  go through *custom* imports the engine implements natively against the overlay.
  That is the normal wasm shape, and it is also exactly the import surface
  asyncify instruments (the "introduce local diffs" half).
- **One blob or many is a free choice** — they're wasm, run in-process either
  way, each independently asyncify-able. Today the coreutils are one
  busybox-style multiplexed blob (`engine/wasm/blobs`); brush/find/kati/n2 can be
  the same blob or siblings.

## What is verified (by running it — see CLAUDE.md's one rule)

| Fact | How |
|---|---|
| coreutils compile to `wasm32-wasip1` (incl. the uucore/fluent/icu stack) | `cargo build --target wasm32-wasip1` |
| `brush-core` compiles to wasm | already `cfg(target_family="wasm")`-aware upstream (web playground); built clean |
| wasmi runs a wasm util in-process with WASI stdio, **byte-parity** vs native | `seq`/`sort`/`tr`/`basename` blob run under a wasmi+wasmi_wasi harness |
| `wasm-opt --asyncify` output still runs under wasmi | asyncified blob runs `tr`/`sort` identically |
| wasmi + wasmi_wasi build **static-musl** via `cargo zigbuild` | statically-linked ELF produced |
| binaryen `wasm-opt --asyncify` present | `apt-get install binaryen` (108) |
| **suspend a guest at a host import and resume it**, asyncified, under wasmi | `engine/wasm/asyncify-demo` — unwind on first import entry, rewind to resume, asserts the resumed result |
| the host's suspend machinery needs **no WASI** | the demo's import is a plain `wasmi::func_wrap` + `Caller`; `wasi-common` is not involved |
| the **real coreutils blob runs on our own hand-written preview1 host** with in-memory stdio (zero syscalls for I/O), byte-parity | `engine/wasm/host` — `runblob <blob> seq/tr/sort/head/tail/uniq/nl/cut` |
| **single-file suspend/resume across a full teardown** (snapshot linear memory to one file, drop everything, restore + rewind, finish) | `engine/wasm/asyncify-demo` `snapshot` bin |
| **mmap'd file as live store, used as-is, kept sparse by hole-punching** (zero-copy, persists across unmap/remap) | `engine/wasm/mmap-demo` `sparse` bin |
| **wasmi linear memory IS an mmap'd file** via public `Memory::new_static` (zero-copy, no patch) | `engine/wasm/mmap-demo` `linmem` bin |
| **a running wasm function migrates across OS processes AND fresh namespaces** (park in one process, resume in another under unshare user+mount+net) | `engine/wasm/asyncify-demo` `migrate` bin |
| **sliced tmpfs + host-owned allocator + `fallocate` reclaim** (per-process 1 GiB chunks of one sparse file; free returns pages to the OS) | `engine/wasm/heap-demo` |

### Operational notes

- **wasmi needs its `simd` feature.** LLVM autovectorizes wasm output (e.g.
  `bytecount`); without `simd`, `Module::new` fails with "SIMD support is not
  enabled".
- **No special host stack needed in release.** A debug wasmi build overflowed the
  default 8 MiB stack (huge per-frame locals in the opcode-dispatch loop); that
  was a debug artifact. A release build runs in ≤2 MiB. The engine ships
  `--release`, so the default stack is fine — do **not** spawn a giant-stack
  thread.
- **asyncify on an old wasm-opt needs the feature flags** the rust toolchain
  emits, or it dies with "error validating input":
  `--enable-simd --enable-bulk-memory --enable-sign-ext --enable-mutable-globals
  --enable-nontrapping-float-to-int --enable-multivalue --enable-reference-types`.

## The host design (settled by the demo)

The engine's host is **plain wasmi imports**, not `wasi-common`:

- **Virtual fd table, host-owned.** An fd is an engine handle whose backing is a
  real kernel fd *or* a purely in-process object (an in-memory pipe between two
  blobs, an in-memory file) — so a blob→blob pipeline moves data with **no
  syscalls**, and the engine sees every byte (provenance) and can checkpoint
  cleanly. `fd_read`/`fd_write`/`fd_seek`/`splice`/`path_open`/stat all `match`
  on the backing.
- **`splice` is polymorphic over backing**, not kernel-only: (kernel,kernel)+pipe
  → real `splice(2)`; in-memory → buffer handoff/`memcpy`; mixed → read-then-write.
  Nothing Unix is "excluded" — it's supplied from stable wasi, or as a host import
  the engine services against the real fds/overlay.
- **We implement preview1 ourselves** as wasmi imports against that fd table
  (reusing only WASI's ABI constant/struct definitions so guest std stays
  byte-compatible), plus custom imports (splice, overlay chmod/chown, provenance
  taps). `wasi-common`/`wasmi_wasi` was only the bootstrap for the leaf-util proof.
- **Asyncify suspend** is driven host-side exactly as `asyncify-demo` shows: the
  import writes the asyncify control struct into guest memory and calls the
  guest's `asyncify_start_unwind`/`start_rewind` exports. Only the **blocking**
  imports (`fd_read`/`fd_write`/`poll`/`splice`) are suspend candidates; trivial
  ones return inline.
- **Whole-system checkpoint = one backing file, used as-is (mmap, no
  serialization).** To checkpoint: unwind every running blob, **barrier** until
  all have unwound (each parked at a clean import boundary, state in linear
  memory), then the file is already current — `msync`. Layout: a fixed-layout
  header (mutable globals incl. `__stack_pointer`, fd-table metadata, pipe ring
  offsets/cursors), then page-aligned per-process **linear-memory** regions, then
  file-backed **pipe buffers**. Each instance's linear memory and each in-memory
  pipe is an `mmap(MAP_SHARED)` view into this one file, so the system mutates the
  file in place; resume = `mmap` it back, `start_rewind`, re-enter. Unused
  linear-memory pages and freed pipe bytes are **hole-punched**
  (`fallocate(PUNCH_HOLE)`, or `madvise(MADV_REMOVE)` on tmpfs; `MADV_DONTNEED`
  drops cache only) so the file's real footprint tracks live pages. The mmap +
  sparse + hole-punch mechanic is proven (`mmap-demo`); the snapshot roundtrip is
  proven (`asyncify-demo snapshot`).
- **Real-blob checkpoint is build-flags, not a runtime patch.** wasmi backs a
  linear memory with our buffer via the public `Memory::new_static(&'static mut
  [u8])` (a static buffer grows up to its capacity) — proven (`mmap-demo linmem`):
  the guest writes straight into the mmap'd file, zero-copy. So:
  (1) build blobs with `wasm-ld --import-memory` (module imports `env.memory`) and
      `--export=__stack_pointer` (so the one mutable global is snapshottable);
  (2) host mmaps the file sized to max pages (sparse), `new_static`s it as
      `env.memory`. No wasmi patch; no wasm3 (it `malloc`s its memory too).

### Two halves of the Unix surface

- **Guest side — `syscompat` shim crate.** Vendored code changes `std::os::unix`
  → `syscompat::unix` (mechanical path swap, logic + call sites unchanged).
  `syscompat` is `pub use std::os::unix::*` on Unix (zero native change); on wasm
  it supplies the equivalents: re-export stable wasi `OsStrExt`; reimplement the
  nightly-gated `FileTypeExt`/`MetadataExt`/`OpenOptionsExt` and the absent
  `PermissionsExt`/uid/gid with the **same method names** (so call sites compile),
  backed by host imports. Proven: one import path compiles unchanged on both
  targets.
- **Host side — the virtual fd table + custom preview1, above.**

This replaces the earlier per-crate `cfg`-gate + `LogicalFd` placeholder approach
(used for head/tail) with a single shim, shrinking the per-crate diff toward
pristine. head/tail will be migrated onto `syscompat`.

### Asyncify scope (measured, not assumed)

Keep the `asyncify-imports` allowlist to the genuinely-blocking imports
(`fd_read`/`fd_write`/`poll_oneoff`/`splice`); never list trivial ones
(`clock_*`/`random_get`/`args_*`/stat/chmod return inline). But **measured**: for
the coreutils blob, scoping the allowlist to `fd_read`/`fd_write` only shaved
~0.5% (8.10 → 8.06 MB) — because `fd_write` is reachable from nearly every
function, so asyncify instruments most of the call graph regardless. The real
cost is the ~2× blow-up (3.8 → 8 MB) of making a write-pervasive program fully
suspendable. So the actual levers are: **asyncify is opt-in per blob** (normal
runs use the 3.8 MB plain blob; only checkpointed blobs get instrumented), and
`asyncify-onlylist`/`removelist` to bound *which functions* are suspendable if we
only need suspension at specific points. The import allowlist is correct hygiene,
just not where the bytes are.

### Memory model: sliced file, host-owned heap, OS reclaim

- **One sparse backing file, sliced per process.** Pick a max per-process heap
  (e.g. 1 GiB); `ftruncate` the file to `NPROC * max`; each process gets `mmap`'d
  chunk `i*max` handed to `Memory::new_static` as its own 0-based linear memory.
  Sparse, so only touched pages cost — a 2 GiB logical file showed 0 blocks until use.
- **Host owns the heap.** The blob's `#[global_allocator]` is a trampoline to
  `host_alloc`/`host_dealloc` imports (memory imported, `__heap_base` exported), so
  the host has the exact live-set — no guessing the allocator's internals. (This is
  the "supply alloc callbacks" answer: allocation is in-module by default, but we
  *override* it so the host is the source of truth.)
- **Free returns pages to the OS.** On `dealloc` the host coalesces and
  `fallocate(PUNCH_HOLE)`s the freed block's page-aligned range (+ `MADV_DONTNEED`
  for RSS). Proven (`heap-demo`): alloc 64 MiB → blocks jump; free → blocks drop.
  Coarse path: on blob exit, `munmap` + punch the whole chunk. No compaction
  (linear-memory pointers are absolute). Perf note: import-per-alloc is fine for
  the mechanism; a hot path wants a hybrid (in-wasm fast path + host trim).

### Migrating a running function across processes / namespaces

The point of the asyncify + single-file design: execution can move between the
box process and the engine process (different namespaces) at any import boundary.

- The **continuation is the file**: linear memory (holds the asyncify call-stack),
  the `__stack_pointer`/`__asyncify_data` values (linear-memory **offsets**, not
  host pointers), globals, and the host fd-table state. Process A unwinds → the
  continuation is fully in the shared mmap; A signals B; B rewinds → the same
  function continues in B. Back-and-forth = repeat at each import boundary.
- **Position-independent**, so a different process maps it at its own address, in
  its own namespaces, and resumes — proven (`migrate`, resumer under `unshare
  --user --mount --net`).
- **fds aren't namespaced** → a `memfd` shared `MAP_SHARED` (or brokered over
  `SARUN_BROKER`) is visible on both sides of the box↔engine boundary; with
  `new_static` over that memfd in both, the continuation lives in shared memory and
  a handoff is just a signal + rewind (no copy).
- The resuming process runs **its own** host imports, so an import's effects
  execute in **that** process's namespace: a builtin's fs op resumes in the box's
  mount ns; a privileged/host op (real `splice`, overlay `chmod`, MITM proxy, slip
  pool) resumes in the engine's ns. Execution migrates to whichever side holds the
  right namespace, does the work, migrates back.

## The per-crate porting recipe

The vendored crates are still consumed by the *native* engine (which calls their
logical-I/O entries), so we cannot delete the patch — we **`cfg`-gate the
unix-only additions** so the crate compiles for both targets:

1. The wasm blob calls each util's plain `uumain` (the standalone entry the
   vendored forks retain). On wasm, `uumain` must not route through the
   unix-fd logical entry.
2. Gate `use std::os::fd::*` / `use std::os::unix::*` and the splice/`copy_file_range`
   fast paths behind `#[cfg(unix)]`; the portable read/write loop is the wasm path.
3. For utils whose logical entry takes `BorrowedFd` params: make the fd params a
   `cfg`-conditional type (unix: `Option<BorrowedFd>`, wasm: absent/`None`) so the
   one signature compiles on both, with the fd-using blocks `#[cfg(unix)]`.

### Bucket status (coreutils)

- **Compile clean to WASI today** (in the blob): `nl cut tr uniq sort basename
  dirname seq`. (Several only *mention* fds in comments; `sort` was already
  wasm-gated upstream.)
- **Need fd-param gating** (genuine splice/logical-fd patch): `cat head tail wc`.
- **Need source patches**: `tac` (`mmap` → buffered read), `expr` (`onig` C dep →
  pure-Rust regex; otherwise needs a WASI C sysroot).
- **Need custom host imports for parity** (the sandbox cannot do these
  faithfully): `chmod chown install id ln touch` + stat reads (uid/gid/mode).

## Build

```
make wasm-blobs     # build the coreutils blob + asyncify it (needs binaryen)
```

Artifacts under `engine/wasm/blobs/target/wasm32-wasip1/release/`:
`coreutils.wasm` (raw) and `coreutils.asyncify.wasm` (instrumented).

## Open / next

- Gate `cat head tail wc`, then `tac`/`expr`; add back to the blob.
- Define the **custom host-import surface** (privileged fs ops, process spawn,
  net) and implement it in the engine's wasmi runner; port the host-import bucket.
- Embed the wasmi+WASI runner in the engine and hook brush-core's external-command
  path to run a blob in-process (deleting the cosplay layer for ported utils).
- Pull `brush-core` + find/kati/n2 into the blob(s).
- Specify the **asyncify-imports** list and implement the host-side asyncify
  protocol (start/stop unwind/rewind, secondary stack) to actually suspend/resume.
