# asyncify suspend/resume — reference for the engine's host protocol

The smallest working proof that a wasm guest can be **suspended at a host import
and resumed**, asyncified and run **under wasmi**. This is the mechanic the
engine's real WASI host will use to checkpoint a running blob; this crate exists
so that protocol is captured in isolation, not buried in the engine.

Run it:

```
wasm-opt --asyncify --pass-arg=asyncify-imports@host.op run.wat -o run.wasm
cargo run --release            # prints the suspend -> resume trace, asserts 42
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
