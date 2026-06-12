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

The principle: **keep the kernel page cache on every read-only open, except when
the bytes are actively changing.** What `keep_cache=True` buys is not "skip a fast
RAM copy" — it's that the kernel serves repeat reads *from its own page cache
without ever calling our `read()`*. No FUSE round-trip, no Python, no GIL. That
win is independent of whether our backing is a disk fd or a `bytes` in RAM, so it
applies to every read-only path, not just the disk one.

So the split between paths is about **coherence, not speed**:

- **Real on-disk files** (host lower, or a resident upper blob): `keep_cache=True`,
  guarded by `_autocache` — this is the bulk of the configure read storm.
- **Virtual files** (the synthesized CA bundle): immutable for the session, so
  `keep_cache=True` with an `mtime_ns=0` sentinel — re-reads stay in the kernel,
  and the sentinel still differs from any real file's mtime so a virtual→real
  transition at the same path invalidates.
- **Evicted files** (row bytes, cold): stable until mutated, so `keep_cache=True`
  keyed on the row's `(size, mtime)`.
- **The live write-buffer read path** (a read-only opener seeing a file that
  *another fd is mid-write on*): `keep_cache=False`. Here the bytes really are
  changing under the reader, so the cache must be dropped on open. This is the one
  read-only path that stays uncached, and the reason is coherence, not throughput.
- **Writable opens**: `keep_cache=False` (the bytes are changing under them).

`_autocache(inode, size, mtime_ns)` remembers the `(size, mtime_ns)` last handed to
the kernel for each inode and calls `pyfuse3.invalidate_inode()` the moment it
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

## External test batteries (wired into the suite)

Two standard FUSE batteries now run alongside the repo's `test_*.py` (they mount
the real overlay, so they skip cleanly when `/dev/fuse`/`fusermount3`/the build
toolchain aren't present). Shared plumbing — mount, fetch+build, version pinning —
lives in `bench/extsuite.py`.

- **`test_fsx.py`** — fsx, the data-integrity fuzzer. A long randomized stream of
  read / write / mmap / truncate ops on one file, checked against an in-memory
  model; aborts on the first wrong byte. A true pass/fail gate for the
  capture-write path (lazy handle → RAM buffer → pool-blob spill → copy-up →
  truncate). It opens `O_RDWR`, so it does *not* touch the read-only `keep_cache`
  path — that's the bespoke `coherence` check's job; fsx is the write-side net.

- **`test_pjdfstest.py`** — [pjdfstest](https://github.com/pjd/pjdfstest), the
  POSIX-semantics suite, used as a **regression gate, not a pass/fail oracle**. The
  overlay diverges on purpose (uid/gid squashing, `setxattr→ENOSYS`, synthetic
  dir/symlink inodes with no atime, virtual CA files), so it can't pass clean:
  ~3958 of 7093 assertions fail at steady state, almost all uid-squash permission
  subtests. The pinned suite's per-assertion failure set is checked in as
  `bench/pjdfstest_baseline.txt`; the test FAILS only on a failure *not* in the
  baseline (a real regression), reports newly-passing assertions as "fixed", and
  re-baselines under `SARUN_PJDFSTEST_UPDATE=1`. The baseline is anchored to this
  environment's kernel + the pinned revision.

This is the right shape for an intentionally-nonstandard fs: **diff the failure
set across a change, don't chase a green run.** Across the `keep_cache` change the
pjdfstest failure set was byte-identical before and after — no POSIX regression —
and fsx stayed corruption-free.

I did **not** wire up xfstests: it's kernel/fs-generic, assumes a block device or
a `_scratch_mount` it controls, and most of its coverage is fs-feature-specific
(reflink, quota, dax, log replay) — a poor fit for a multiplexed FUSE subfolder,
and far more maintenance than signal. fsx (which xfstests itself vendors) gives
the data-integrity coverage that matters here without the rig.

## What's left (out of scope here)

The remaining 2.35× is not read amplification. It's the per-op cost of serving
every FUSE request through a **single trio thread** in Python (the GIL serializes
all `read`/`write`/`lookup` handling) plus the capture-write path. A configure run
is fork/exec- and small-IO-bound, so it pays that per-op tax on millions of tiny
ops. Reducing it would mean changes of a different character (e.g. servicing FUSE
on multiple OS threads, or shrinking per-op Python work) and a separate round of
measurement — the `keep_cache` change is the single highest-leverage, lowest-risk
win and stands on its own.

## Addendum: the readdir snapshot cache (dir-listing cache)

The kernel never caches FUSE directory listings, so readdir-heavy workloads
(git status's untracked walk: one readdir per directory, every run) rebuilt the
merged scan from scratch each time — for each entry: a layer resolution, a host
lstat, and a fresh `EntryAttributes` (~14 µs/entry of mostly Python object
churn, ~85% of warm `git status` overhead; verified by `showUntrackedFiles=no`
collapsing the warm ratio from 7× to 1.9×).

`_scan_dir_cached` now snapshots each directory's merged listing, validated by
`(host dir mtime_ns, Index.dirlist_gen(rel))`. The gen is **per directory**
(plus a global epoch for prune/reparent): git writes `.git/index` every run, and
a global counter would invalidate every cached listing on any write anywhere —
measured as exactly zero cache hits. Coherence: a hit re-lstats every
disk-backed entry and reuses the cached `EntryAttributes` only when the stat is
byte-identical (synthetic entries re-resolve through the RAM mirror), so attrs
are never served staler than one fresh lstat — same doctrine as `_autocache`.

git status, 5 000 tracked files, back-to-back runs: warm 0.079 s → 0.058 s
(cold 0.59 s → 0.44 s). Runs spaced past the 1 s attr TTL are unchanged
(~0.41 s) — those are bounded by the lookup/getattr storm, which is the
single-trio-thread per-op tax above, not readdir.

## Comparing timings across revisions

The microbenchmarks behind the numbers above are committed as
`bench/workloads.py` (git-status / exec-storm / file-churn through the real
overlay vs native-under-bwrap, plus the remote-UI rpc round-trip). To compare
any two revisions, run the SAME harness on the SAME box:

    bench/workloads.py                              # current tree
    git show <rev>:sarun > /tmp/sarun_old
    SARUN_PATH=/tmp/sarun_old bench/workloads.py    # the old revision

Trust per-op overhead (µs/op) across machines and runs; treat A/B ratios as
box-local (the native baseline jitters, the additive FUSE cost doesn't).
Reference run, dev container 2026-06-14 (post dir-listing cache + engine/UI
split): git-status 89 µs/op cold · 0.058 s warm; exec-storm 1.7×;
file-churn 3.4×; rpc 0.35 ms/verb.

## Addendum: batch attr/readdir helpers in the pyfuse3 patch

The per-entry Python cost of the metadata path was dominated by struct
ceremony: 13 interpreted property assignments to build each EntryAttributes,
plus one readdir_reply call per entry. The embedded pyfuse3 patch now adds
`entry_attributes_fast()` (fills the whole fuse_entry_param in one C call) and
`readdir_reply_batch()` (packs a listing slice into the kernel buffer in one
call per directory); `_entry()` and `readdir()` use them.

Measured with bench/workloads.py, same box, SARUN_PATH=previous revision:
git-status cold 104 → 86 µs/op (−17%), warm 0.064 → 0.058 s. exec-storm and
file-churn unchanged — they are bounded by exec/copy-up and the capture write
path (provenance + sqlite per write), not attribute construction; batching
THAT write path is the next rung on the ladder.
