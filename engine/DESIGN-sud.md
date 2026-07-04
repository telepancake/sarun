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

1. **(done)** `sarun run --sud`: launch the command under the sud wrapper
   with a plain *directory* upper (`{state}/live/<id>/sud-up`) overlaid on
   `/`, then a post-exit sweep (`sud_ingest` verb → `engine/src/sud.rs`)
   ingests the upper dir into the box's existing sqlar `BoxState` —
   whiteouts (char 0:0) → whiteout rows, files → pool blobs, symlinks/dirs/
   specials → their rows. Everything downstream (review/apply/discard/UI)
   works unchanged. No bwrap, no FUSE mount participation for the box's own
   I/O; the box registers on the overlay like any other so the control plane
   and UI see it.
1.5. **(done)** Absorb the launcher (tv's `sudtrace`) into the Rust runner;
   `engine/src/sudwire.rs` encodes the launcher side of the TRACE stream.
2. **(done — trace streaming)** The runner's fd 1023 is a pipe whose read
   end rides to the engine with register (second SCM_RIGHTS fd, the slot
   tap boxes use). `sud::stream_events` consumes the TRACE stream live:
   EXEC events snapshot each process row from /proc *while the process is
   alive* (`writer_for`), OPEN-for-write events build the rel→writer map
   the sweep uses for per-file attribution (relative paths resolved
   against per-tgid EV_CWD state; dirfd-relative opens fall back to the
   runner row), STDOUT/STDERR events land in the box's outputs table, and
   the raw bytes tee to `live/<id>/sud.trace` at rest. `sud_ingest` waits
   for pipe EOF (the runner closes fd 1023 before requesting the sweep)
   so the attribution map is complete.
2.5. **(done — inramfs /tmp)** `/tmp` is a per-box inramfs mount rather
   than a passthrough stopgap: the engine mints a per-run key
   (`sarun<pid>b<id>`), the wrapper serves `/tmp` from the shared-memory
   store (`--remap-rule inramfs:/tmp --inramfs-key …`), and at sweep the
   engine parses the store's `/dev/shm/sud-inramfs.<key>.{meta,smalldata,
   f.*}` region FROM OUTSIDE (`engine/src/sudir.rs`, offset-addressed —
   no fixed-address mapping needed) into the sqlar under `tmp/…`, then
   unlinks the shms. Mixed 32/64 is the hard case and works: a 64-bit
   process maps the region; a 32-bit process can't fit the 8 GiB data
   space in its address space, so it pread/pwrites the smalldata shm and
   promotes large files to per-file shms — all captured. Two upstream
   fixes were required for cross-class store sharing:
     - `internal.h`: `struct sud_ir_inode` is 76 B on i386 vs 80 B on
       x86_64 (a trailing 12-B union after an 8-aligned body), so a
       64-init region gave a 32-bit attacher a wrong inode stride. Pinned
       both structs with `aligned(8)` + `_Static_assert` on size.
     - `super.c` / `libc.h`: a 32-bit `open()` of the 8 GiB smalldata shm
       needs `O_LARGEFILE` (else EOVERFLOW → attach aborts → `/tmp`
       silently reverts to host), and the creating truncate needs the
       i386 split-arg `ftruncate64`.
   Still pending: live file rows (writes appear at sweep, not as they
   happen); OPEN attribution under /tmp currently often lands on the
   runner row.
3. Only then: the store. Alternate `BoxState` backend on depot/strpool
   mechanics, sqlar kept as an export for the Python tooling.

## Composition: same-in-same nesting (implemented), mixed (sketch)

Same-in-same nesting preserves the full model on both sides, and both
sides use the SAME shape: **flattening**. FUSE-in-FUSE was never
mount-in-mount — it is one multi-box mount whose resolve() walks the
parent chain; sud-in-sud is likewise never wrapper-in-wrapper (both
wrappers link at one fixed text address, and the outer wrapper's execve
interception would wrap the inner wrapper binary) — it is ONE wrapper
invocation whose overlay rule stacks the layers.

**sud-in-sud (implemented)**: `sarun run --sud PARENT.CHILD -- cmd`
(launched from the host, dotted-name parent resolution — the existing
register path). Register validates the chain is all-sud and AT REST,
materializes each ancestor's captured state from its sqlar
(`sud::export_box` — the inverse of the sweep: blob-pool hardlinks,
char-0:0 whiteouts, dirs/symlinks/specials) into `live/<id>/sud-lower-
<aid>`, and acks the lower list; the runner emits
`overlay:/=<up>+<lower…>+/`. A rerun exports its OWN prior state as the
nearest lower (the FUSE analog is load_mirror) and starts from a clean
upper. The sqlar is authoritative — the stale sud-up dir is never used
as a lower, so apply/discard done between runs are honored. Upstream
patch: the overlay resolve walk now honors whiteouts found in MIDDLE
lowers (the dir-listing merge already did); rules.h caps one rule at 9
layers, so chains deeper than 7 ancestors fail loud.

**In-box nesting (implemented)**: a `run --sud` issued from INSIDE a
running sud box derives its enclosing box from the runner's /proc
ancestry (same identity path relname registration uses). A RUNNING
ancestor's truth is its LIVE upper directory stacked on its register-
time layer list (recorded in `sud::set_layers`); an at-rest ancestor is
exported from its sqlar as before. Two mechanics make this safe:
- The outer wrapper's execve interception passes a sud-wrapper target
  through UNWRAPPED (elf.c): execve replaces the process image, so the
  inner wrapper simply takes over with its own composed flag block —
  no wrapper-in-wrapper, no address collision.
- The nested runner never replumbs fds 1022/1023 in its own process
  (it is itself traced by the outer wrapper, whose trace addin writes
  outer events — with stream ids from the OUTER counter page — to fd
  1023 there); the inner contract fds are installed in the child
  between fork and exec, and the launcher writes its version atom +
  EXIT events through the pipe fd directly.

**Mixed 32/64 (verified)**: one box can cross classes both ways — a
64-bit shell spawning a 32-bit static binary (wrapped by sud32 via the
wrapper's dir-sibling convention) and a 32-bit binary exec'ing /bin/sh:
both capture into the same upper with correct attribution. This is the
environment inramfs testing needs — on 32-bit the store cannot fit in
the address space and the transfer paths must be exercised.

Gaps: opaque-dir semantics don't exist in the sud overlay; host-
launched (dotted-name) nesting still requires at-rest ancestors.

**32-bit (implemented)**: the runner probes the target's ELF class and
picks sud32/sud64 (`$SARUN_SUD32`, or sud64's dir sibling — the same
convention the wrapper itself uses for cross-class children). Verified:
a static i386 binary traced by sud32 captures into the upper with
attribution and output rows. This unblocks the inramfs upper (the
in-RAM store must be mappable from both wrapper classes).

Mixed quadrants (sketch only):

- **FUSE box nested in a sud box**: works structurally today — the inner
  runner dials the engine as a host runner (a sud box sets no
  SARUN_BROKER), bwrap binds `<mnt>/<id>` as usual. The sud overlay must
  NOT swallow writes that go through the FUSE mount (they belong to the
  inner box's capture), so the engine's mountpoint is a passthrough
  carve-out in the sud rule stack. Gap: parentage — the inner box
  registers as top-level because parent derivation currently only runs
  for in-box (`relname`) registrations; deriving the enclosing sud box
  from the /proc ancestry (box_runpids already has the sud runner's pid)
  would nest it properly.
- **sud box nested in a FUSE box**: rejected today. The wrong way is to
  run sud64 inside the bwrap mount (upper paths under the engine state
  dir resolve through the parent's FUSE lower — the child's captured
  writes would themselves become parent-box captured writes). The right
  way is rule composition against the parent's *host-side* mount:
  `overlay:/=<child-up>+<mnt>/<parent-id>` — reads traverse the parent's
  merged view (whiteouts and all), writes land in the child's own upper.
- **sud in sud**: never wrapper-in-wrapper — both wrappers link at the
  same fixed text address, and the outer wrapper's execve interception
  would try to wrap the inner wrapper. Flatten instead: the engine knows
  the parent chain, so a nested sud box is ONE wrapper invocation with a
  composed rule stack, `overlay:/=<child-up>+<parent-up>+/` (the overlay
  rule syntax already takes multiple lowers, and the upper-side whiteout
  markers are honored during lower walks).
- **FUSE box whose PARENT is a sud box**: the FUSE resolve() walks parent
  BoxStates, but a live sud box's writes sit in its upper dir, not in its
  BoxState until sweep — the child would see a stale parent. Needs either
  live row ingest (streaming writes into the BoxState as they happen) or
  a resolve() fallback that consults the parent's sud upper directory.

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
