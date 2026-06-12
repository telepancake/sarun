# sarun-engine — design decisions (from the 2026-06 architecture review)

Decisions are numbered for reference; "open" items are explicitly undecided.

## D1 · Two programs
A statically-linkable Rust **engine** (this crate) and the Python **UI** as a
pure socket client. The wire protocol (JSON lines over the control socket, the
`subscribe` event feed, the `ui` verb set — see the Python ChannelServer) is
the ONLY contract between them. Everything behind the socket is private and
may change freely: there are no users to migrate (D8).

## D2 · Milestones
- **m1 (done):** multithreaded read-only passthrough (fuser 0.17, n_threads).
  Scaling proof vs the Python engine: bench/parallel_metadata.py — 4
  concurrent cold git-status walks: python 1.69 s, rust 1t 0.21 s, rust 8t
  0.14 s. The durable language factor is ~3× (fuse-overlayfs comparison);
  threading is the rest. Honest target for m3: full semantics at ≤2× native
  under `make -j8`.
- **m2:** control socket speaking the existing protocol + namespace-aware
  paths ($SLOPBOX_NS, same layout rules as the Python engine), until the
  Python conformance tests (test_remote_ui_control_plane, test_attach_remote,
  the e2e engine section) pass against the Rust binary unmodified.
- **m3:** overlay + capture semantics, driven black-box by the behavioral
  suite (e2e, pjdfstest, attach parity). Format-groveling Python unit tests
  are implementation tests of the OLD engine and retire with it.
- **m4+:** musl static link; the Python file becomes UI client + bootstrap
  (fetch-or-build the engine binary, hash-keyed cache — the same mechanism
  that builds patched pyfuse3 today). The pyfuse3 patch dies at m3.

## D3 · Writes are writes
No write buffers, no buffer→row reconciliation. Open-for-write creates/clones
a real upper blob file; the box's writes are pwrites to it (ordinary file
semantics by construction). Capture is bookkeeping (who opened, when closed,
final stat) — never a reimplementation of file behavior. This is the direct
fix for the old engine's wbuf bug class.

## D4 · Uniform blob-per-file rest form
EVERY non-empty regular file's bytes live in a separate blob file under the
box's blob dir (sharded). No inline-in-DB tier, no size threshold, no
evicted/resident duality — and therefore NO consolidate phase: a finished box
is at rest the moment it exits. Compression is delegated to the host fs if
wanted. Known cost: tiny-file-heavy boxes pay inodes + block slack; the
page-aligned arena (D6) is the contingency if that ever measures as pain.

## D5 · FUSE passthrough backing fds — READ-ONLY opens only
Because bytes are always real files (D4), read-only opens may register
backing fds (kernel 6.9+, fuser opened_passthrough) and let the kernel serve
reads with the daemon out of the loop — where the measured pain lives (build
read storms). WRITE opens stay daemon-served: per-WRITE ctx.pid attribution
is load-bearing, not a nicety — the common fork-after-open case (`sh -c
'cmd > out'`: the SHELL opens, the child writes through the inherited fd)
means per-open attribution would credit half a build's outputs to /bin/sh,
and the writer/last_writer tags that file rules match against only mean
anything if each write is attributed through shared fds. (This per-write pid
is the entire reason the old engine patched pyfuse3; fuser exposes it
natively via Request::pid.) Fallback to daemon-served reads on older kernels.

## D6 · Storage for the index (OPEN: rusqlite vs redb)
The index (paths, modes, whiteouts, writers, process/provenance/env/outputs)
is small and key/range-shaped: 2 joins in the old schema, both point lookups.
- rusqlite: keeps the .sqlar-inspectable-with-stock-tools product property.
- redb: single-language build, simpler; add an `export` verb producing a
  standalone sqlar (inlining blobs) for the inspectability/interchange story.
The at-rest box is in EITHER case "index file + blob dir" — it was never a
single file (the old 64 KiB threshold already split it); a true single-file
artifact is an export operation, not a rest form.
- The page-aligned arena file (one file, aligned extents, FICLONERANGE
  extraction when a backing fd or host apply needs a standalone file) is a
  deliberate m4+ experiment, NOT m3: it requires writing an allocator, and
  per-file blobs delegate allocation to the host fs.

## D7 · UI language
Textual client stays through m2-m4 as reference implementation and test rig
(headless Pilot parity suite). A ratatui client is an optional m5 — by then
the protocol is the only contract, terminal-emulation crates (vt100/termwiz)
cover the PTY-pane feature, and ratatui's TestBackend covers headless golden
tests. The PTY/tmux feature (engine-held PTYs over the existing mux frames)
slots naturally after m3.

## D8 · No migration obligations
Zero users: compatibility choices are scaffolding for OUR transition (keep
what lets existing tests run; discard when the behavioral suite covers the
ground), never obligations. The wire protocol is kept because our own tests
and UI speak it — the moment that stops being worth it, it too can change.
