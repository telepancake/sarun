# asyncify suspend/resume — reference for the engine's host protocol

The smallest working proof that a wasm guest can be **suspended at a host import
and resumed**, asyncified and run **under wasmi**. This is the mechanic the
engine's real WASI host will use to checkpoint a running blob; this crate exists
so that protocol is captured in isolation, not buried in the engine.

Run it:

```
wasm-opt --asyncify --pass-arg=asyncify-imports@host.op run.wat -o run.wasm
cargo run --release --bin suspend-resume   # in-process: suspend then resume, asserts 42
cargo run --release --bin snapshot         # suspend -> ONE file -> teardown -> restore -> resume
```

## What it shows

- `run.wat`: a guest whose `run()` calls the host import `host.op()`. After
  `wasm-opt --asyncify`, the module also exports `asyncify_start_unwind`,
  `asyncify_stop_unwind`, `asyncify_start_rewind`, `asyncify_stop_rewind`,
  `asyncify_get_state`.
- `src/main.rs`: a **plain wasmi host** (`Linker::func_wrap` + `Caller`) — no
  `wasi-common`, no WASI at all. The `host.op` closure:
  - first entry (state 0): writes the asyncify control struct into guest memory
    (`[ptr]=stack_start`, `[ptr+4]=stack_end`), calls `asyncify_start_unwind`,
    returns a dummy. The guest unwinds; the top-level `run()` call returns.
  - on resume the host calls `asyncify_start_rewind(ptr)` and re-invokes `run()`;
    the guest rewinds back into `host.op` (state 2), which calls
    `asyncify_stop_rewind` and returns the real value. `run()` continues.

## Why it matters for the port

The hard part of "make the collective blob asyncify-able at its imports" is the
host-side unwind/rewind dance. This proves it works with a **hand-written wasmi
import** and the guest's asyncify exports — so the engine's host (the virtual fd
table: `fd_read`/`fd_write`/`splice`/privileged ops, each a wasmi import) can be
a suspend point the same way. WASI's role is only guest-side (letting std file/IO
compile); it plays no part in the suspend machinery.

## Single-backing-file checkpoint (`snapshot` bin)

The whole-system checkpoint: signal every running blob to asyncify-unwind, then
**barrier** until *all* have unwound (each parked at a clean import boundary with
its state in linear memory). Only then write **one file** =
`{per-process: linear memory + mutable globals + parked-import id} ⧺ {host fd
table: in-memory pipe contents + cursors}`. Resume = load the file, rebuild each
instance, restore memory/globals, `asyncify_start_rewind`, re-enter.

`snapshot` proves the core across a **full teardown** (drop engine+store+instance,
then a fresh one restores and finishes). Caveat it surfaces: the demo guest's
state is *only* linear memory; real blobs also have mutable globals (notably
`__stack_pointer` for the C shadow stack) that must be in the file too — and those
aren't exported by default, so the blob build must export them (wasm-opt) to
snapshot/restore them.
