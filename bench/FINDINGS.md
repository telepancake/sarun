# Overlay performance investigation — the configure slowdown

## TL;DR

A `./configure` run is **~4.4× slower** through sarun's FUSE overlay than native.
The dominant cause is that every read-only `open()` returned `keep_cache=False`,
so the kernel **threw away the page cache on every open**. A configure run
fork/exec's thousands of short-lived processes (`sh`, `cc1`, `ld`, `expr`,
`grep`, conftest binaries) — each exec re-reads the same `/bin/sh`, `libc`,
`cc1` and headers *through FUSE from scratch*. Letting read-only opens keep the
page cache (with mtime/size-based invalidation for coherence) removes ~47% of
the overlay overhead.

| scenario | `./configure` (best of 3) | vs native |
|---|---:|---:|
| native (same bwrap isolation, no overlay) | 6.04 s | 1.00× |
| overlay, `keep_cache=False` (before) | 26.78 s | 4.43× |
| overlay, `keep_cache=True` + `_autocache` (after) | 14.18 s | 2.35× |

## How to reproduce

The host needs `fuse3` (`fusermount3` + `libfuse3-dev`) and `bubblewrap`; the
harness pulls `pyfuse3`/`trio` itself via `uv`.

```sh
# 1. build a configure that does a realistic spread of probes (~130 AC_CHECK_*)
bench/gen_project.sh /root/benchproj

# 2. native baseline — same bwrap isolation, real fs bound read-only
python3 bench/overlay_bench.py native  --proj /root/benchproj --runs 3

# 3. through the real multiplexed overlay (mounts it, binds it as / under bwrap)
uv run bench/overlay_bench.py overlay  --proj /root/benchproj --runs 3

# 4. coherence proof for the keep_cache change (see below)
uv run bench/overlay_bench.py coherence --proj /root/benchproj
```

`overlay_bench.py` loads sarun's own `OverlayMount`/`Index`, mounts the single
overlay, registers one session, then runs the workload under the *same* bwrap
flags the real runner uses — binding `<mnt>/<sid>` as `/`. The `native` mode runs
the identical command under identical bwrap isolation but binds the real `/`
read-only, so the **only** difference between the two numbers is the FUSE overlay
in the I/O path.

## Why it was slow

The hot read path was fine on metadata — `lookup`/`getattr`/`readdir` answer
from the in-RAM `Index` mirror, not SQLite, and attr/entry timeouts are 1.0 s, so
repeated `stat()`s of the same path are served by the kernel. The cost was in
**file content reads**:

- Every `open()` returned `pyfuse3.FileInfo(..., keep_cache=False)`.
- `keep_cache=False` tells the kernel to **invalidate the inode's page cache at
  open time**. So even though `direct_io=False` lets the kernel cache pages, that
  cache never survived to the *next* open of the same file.
- configure's workload is overwhelmingly "exec a tiny program, exit, repeat".
  Each exec maps the binary + its shared libs; each `AC_CHECK_*` runs `cc1` over a
  conftest, re-reading the compiler, `libc`, and the system headers. With the
  cache dumped on every open, all of that crosses the FUSE boundary —
  kernel → userspace → single-threaded Python `read()` → `os.pread` — on every
  single exec.

## The fix

`keep_cache=True` on **read-only opens of a real on-disk file** (host lower, or a
resident upper blob), plus a small `auto_cache`-style guard so a changed file is
never served stale:

- `MultiplexOverlayFs.open()` now returns `keep_cache=(not writable)`. Writable
  opens, and the RAM/evicted/virtual fast-paths, still pass `keep_cache=False`
  (they serve from RAM — there's no FUSE re-read to avoid — and a `False` open
  naturally flushes any stale cache for that inode).
- New `_autocache(inode, st)`: remembers the `(size, mtime_ns)` last handed to the
  kernel for each inode and calls `pyfuse3.invalidate_inode()` the moment it
  changes. This mirrors libfuse's `auto_cache` option (which is a *high-level*
  libfuse feature, not a `-o` mount option pyfuse3 accepts, so it's implemented
  here directly).

### Why this is coherent

Three independent reasons a stale read can't slip through:

1. **Write-through.** When the box mutates a file *through the overlay*, the write
   crosses the same inode and the kernel drops the read cache as a side effect.
2. **Explicit `_autocache`.** Every read-only open re-resolves the file's real
   `(size, mtime)` and invalidates the inode if it moved since we last cached it —
   independent of the kernel's attr-cache timing, so it also closes the up-to-1 s
   window the attr/entry timeout would otherwise leave open, and it works on older
   kernels that lack `FUSE_AUTO_INVAL_DATA`.
3. **Kernel `FUSE_AUTO_INVAL_DATA`** (modern kernels, negotiated by libfuse3) does
   the same mtime/size check as a backstop.

`invalidate_inode` is safe to call inline here: per the pyfuse3 docs it only
blocks under writeback caching, and this filesystem keeps
`enable_writeback_cache = False`.

This keeps the same conservative coherence contract the 1.0 s attr/entry timeouts
already chose (a content change that preserves *both* size and mtime is not
detected — exactly the standard `auto_cache`/attr-timeout tradeoff).

### Coherence test

`overlay_bench.py coherence` mounts the real overlay and, for same-size / grow /
shrink mutations, checks two mutation classes:

- **(A) write through the overlay** (a box copy-up), and
- **(B) change the underlying lower host file behind the overlay's back** — the
  case `keep_cache` actually threatens, since no write crosses the overlay inode.

Every case must read back the new bytes on the next open. With the fix in place
all reads are fresh; class (B) is the one that exercises `_autocache` specifically
(on a kernel without `FUSE_AUTO_INVAL_DATA` it would go stale without it).

## What's left (out of scope here)

The remaining 2.35× is not read amplification. It's the per-op cost of serving
every FUSE request through a **single trio thread** in Python (the GIL serializes
all `read`/`write`/`lookup` handling) plus the capture-write path. A configure run
is fork/exec- and small-IO-bound, so it pays that per-op tax on millions of tiny
ops. Reducing it would mean changes of a different character (e.g. servicing FUSE
on multiple OS threads, or shrinking per-op Python work) and a separate round of
measurement — the `keep_cache` change is the single highest-leverage, lowest-risk
win and stands on its own.
