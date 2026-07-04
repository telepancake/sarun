# sud-backed boxes — cooperative shared-memory capture (WORK IN PROGRESS)

STATUS: exploratory. Nothing in this document is settled; it records the
current working hypothesis for replacing the FUSE overlay with the
cooperative, shared-memory-based mechanism imported under `tv/` (the sud
sandbox-userland stack) — and it is expected to change as practical issues
surface. Treat every section as "current thinking", not commitment.

## Why

The FUSE overlay puts the engine in the synchronous path of every file
operation a box performs: each lookup/read/write is a kernel round-trip into
the engine's fuser threads. The sud stack (`tv/sud/`) intercepts syscalls
*inside the traced process* via Syscall User Dispatch (SIGSYS on every
out-of-range syscall), applies overlayfs-clone semantics in userland
(`tv/sud/path_remap/overlay.c`: upper/lower walk, copy-up, char-0:0
whiteouts), and can serve a whole subtree from a shared-memory inode store
(`tv/sud/inramfs/`: a /dev/shm region mapped at a fixed address by every sud
process, futex-locked, lock-free lookups). Provenance comes from the trace
addin's lock-free event stream (`tv/wire/wire.h`, `tv/sud/trace/`) instead of
from being in the I/O path.

The trade: SUD is cooperative. It intercepts what the process issues; it is
not a kernel-enforced boundary the way a mount is. For sarun's use case
(capture your own builds/agents for review) that is acceptable, but it is a
real change of threat model and must stay documented.

## Seam inventory (what stays, what moves)

From the FUSE side, the only FUSE-aware type is `Overlay`
(`engine/src/overlay.rs`); `capture::BoxState` (sqlar rows + blob pool),
`review.rs` apply/discard, `hostfs.rs`, and the control plane are all
mechanism-agnostic. The contract with `runner.rs` today is just "a directory
path presenting the merged root" for `bwrap --bind`. Under sud there is no
mount: the runner launches the command under `sudtrace`/`sud64` with
`--overlay` rules instead.

Kept unchanged for now: the sqlar archive as the at-rest interchange format
(review, UI, and `prototype/libtestsarun.py` readers stay untouched). A
lower-overhead store (gimir depot mechanics: mmap flat index + framed
append-only files; strpool interning) is deliberately LAST — see the branch
discussion; `gimir/SCOPING.md` is honest that the current depot crate does
not drop in.

## Staging

1. **(this commit)** `sarun run --sud`: launch the command under `sudtrace`
   with a plain *directory* upper (`{state}/live/<id>/sud-up`) overlaid on
   `/`, then a post-exit sweep (`sud_ingest` verb → `engine/src/sud.rs`)
   ingests the upper dir into the box's existing sqlar `BoxState` —
   whiteouts (char 0:0) → whiteout rows, files → pool blobs, symlinks/dirs/
   specials → their rows. Everything downstream (review/apply/discard/UI)
   works unchanged. No bwrap, no FUSE mount participation for the box's own
   I/O; the box registers on the overlay like any other so the control plane
   and UI see it.
2. Switch the upper from a directory to **inramfs** and ingest from the
   shared region; ingest the wire trace stream for per-write process
   attribution (today the sweep attributes everything to the runner's
   process row) and live-ness (step 1 only sees writes after exit).
3. Only then: the store. Alternate `BoxState` backend on depot/strpool
   mechanics, sqlar kept as an export for the Python tooling.

## Known gaps / practical issues expected (step 1)

- **Attribution**: post-exit sweep = one writer (the runner). Real per-pid
  attribution needs the trace stream (step 2).
- **Rename/mtime fidelity**: the sweep sees final state only; intermediate
  writes, renames, and deletions-then-recreations collapse.
- **/proc, /dev, /sys, /tmp** are passthrough carve-outs (a write to /tmp is
  NOT captured — differs from FUSE boxes where /tmp is a bwrap tmpfs, and
  from --api boxes where /tmp maps into the overlay). Needs a decision.
- **Whiteout markers** are char 0:0 device nodes created by the intercepted
  unlink; unprivileged environments may refuse mknod — may need a userland
  marker convention instead.
- **Opaque dirs**: not translated yet (sud overlay.c semantics vs OCI
  `.wh..wh..opq` need mapping).
- **No isolation**: step 1 runs without bwrap, in the host pid/net
  namespaces. Whether sud boxes should still get bwrap for pid/net (without
  the mount) is open.
- **Nesting**: nested `--sud` boxes are rejected for now (FUSE nesting binds
  the parent-exposed kids dir; the sud equivalent — nested rule stacks or a
  shared inramfs key — is undesigned).
- **Escape hatches**: statically-linked targets work (SUD is not LD_PRELOAD),
  but `PTRACE_TRACEME` children currently drop interception (sud's
  documented fallback), and 32-bit targets need `sud32`.
- **Toolchain**: `sud64` is a freestanding gcc build (`make -C tv sud64
  SUD_ADDINS=...`) — outside the engine's cargo-zigbuild musl world. The tv
  import ships without its third-party single-file printf dep; it is now
  vendored at `tv/libc-fs/deps/printf/` (mpaland/printf, MIT). The C
  launcher (`sudtrace`) is absorbed into the Rust runner; translating the
  wrapper itself to `no_std` Rust is deliberately deferred — its interface
  (argv flag block in, upper dir + trace stream out) is language-neutral,
  and a rewrite buys toolchain unification at high regression risk.

## Runner/engine protocol (step 1 + launcher absorption, subject to change)

- `run --sud` registers with `want_sud: true`, `want_capture: false`,
  `net_mode: "host"`. The engine creates `live/<id>/sud-up` and acks with
  `sud_upper`.
- The runner IS the sud launcher (tv's `sudtrace` binary is no longer in
  the loop; only the freestanding `sud64` wrapper remains a foreign
  artifact, located via `$SARUN_SUD64` or PATH). The runner:
  - opens `live/<id>/sud.trace` on fd 1023 and the 4 KiB MAP_SHARED
    wire-state page (memfd, stream-id counter) on fd 1022 — the wrapper
    contract from tv/sud/sudtrace.c; every traced child inherits both;
  - writes the TRACE version atom and, from its waitpid loop, the
    launcher-side EV_EXIT events, via the Rust wire encoder
    (`engine/src/sudwire.rs` — the seed of the step-2 trace crate; its
    events decode with tv's own `tools/wiredump` interleaved with
    wrapper-emitted streams);
  - probes the target like sudtrace did (PATH resolve, shebang → run the
    interpreter with the kernel's shebang argv shape, ELF class — a
    32-bit target fails loud until sud32 is wired);
  - execs `sud64 --trace-outfile T --remap-rule passthrough:… --remap-rule
    overlay:/=<sud_upper>+/ CMD…` — the argv flag block from
    tv/sud/runtime_config.h (rule order matters: first-prefix-match wins,
    carve-outs before the wide rule).
- On exit the runner sends `{"type":"sud_ingest","sid":…}` on a fresh
  engine conn; the engine sweeps the upper into the live `BoxState`, then
  the runner drops the box channel (normal EOF teardown).
- The trace file is written but not yet consumed (step 2). Owning fd 1023
  in the runner is what lets step 2 switch from a post-exit file read to
  live streaming.
