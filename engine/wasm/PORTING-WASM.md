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
