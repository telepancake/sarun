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

## D3 · Writes are writes — and capture is LAZY (first-write, not open)
No write buffers, no buffer→row reconciliation. But ALL capture cost
(copy-up/blob creation, row creation, provenance) is deferred to the FIRST
actual write op, never paid at open: builds open masses of files writably
(O_RDWR locking patterns, archive updates) without writing a byte, and
at-open capture made the old engine slog — first-write deferral was its
load-bearing perf hack and is inherited here as a rule. A writable open
serves reads from the lower file until the first write arrives; that write
triggers copy-up, after which the box's writes are pwrites to the real blob
(ordinary file semantics by construction). Capture is bookkeeping — never a
reimplementation of file behavior. Bonus the same mechanism buys: first-write
ctx.pid attribution is correct through inherited fds (fork-after-open / shell
redirects), see D5. This is the direct fix for the old engine's wbuf bug
class without losing its one good perf property.

## D4 · Uniform blob-per-file rest form
EVERY non-empty regular file's bytes live in a separate blob file under the
box's blob dir (sharded). No inline-in-DB tier, no size threshold, no
evicted/resident duality — and therefore NO consolidate phase: a finished box
is at rest the moment it exits. Compression is delegated to the host fs if
wanted. NOTE: blob storage is not overhead relative to the workload — a box
holding 100k tiny files costs the same inodes/slack the workload would have
cost the host unboxed; the box is the output in escrow. Apply can therefore
be a rename/reflink of the blob into place. The only residual is that
long-KEPT boxes are uncompressed at rest (the old deflate tier's one real
service) — a filesystem-level concern, not an engine one. The page-aligned
arena (D6) remains a contingency, now with even less motivation.

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
VERIFIED (ptyspike/): the full stack — portable-pty 0.9 (spawn on a PTY) ->
vt100 0.16 (emulate to a screen grid) -> tui-term 0.3 (ratatui widget) ->
ratatui 0.30 TestBackend — drives a real child process and renders it
HEADLESSLY, with escape-sequence emulation and the input direction both
proven. So the m5 UI half is de-risked (Zellij/wezterm use the same stack);
the remaining unknowns are engine-side (PTY allocation + bidirectional mux
frames, which depend on capture mode being ported — currently downgraded
since m3b). Order: finish engine port -> Rust mux/capture -> PTY boxes ->
ratatui client with tui-term panes. Building the pane before the client = twice.

## D8 · No migration obligations
Zero users: compatibility choices are scaffolding for OUR transition (keep
what lets existing tests run; discard when the behavioral suite covers the
ground), never obligations. The wire protocol is kept because our own tests
and UI speak it — the moment that stops being worth it, it too can change.

## D9 · Three complementary execution modes (FUSE always; PTY and brush as toggles)

These are not alternatives — sarun wants all three, composed:

  - FUSE capture — ALWAYS on. Captures the syscalls of arbitrary spawned
    binaries (cc/ld/python); it is the only layer that sees them. The base.
  - PTY mode — toggle, for interactive tty boxes (ptyspike stack; engine-held
    PTYs). Off → today's headless captured stdout/stderr.
  - brush shell — toggle, for the box's shell being our embedded brush
    (brush-core/brush-parser, verified embeddable). Sits ABOVE FUSE: it adds
    semantic context FUSE can't recover, and removes the sh-storm FUSE would
    otherwise have to capture. Independent of the other two.

What brush actually knows (precise — earlier draft overclaimed "Makefile
line", which is FALSE: make execs `sh -c '<recipe text>'`, the shell never
sees the Makefile or a line number). brush knows, for what IT runs: the exact
command string, and its internal structure — pipeline stages, redirections,
subshells, builtin-vs-exec'd-binary. Source LINE NUMBERS only when brush
interprets a script directly (a `configure` run, `brush ./x.sh`) — NOT for
make recipes, which arrive as bare recipe text. This still enriches the
process table (which already stores argv) with the shell-level command and
redirect structure that spawned each pid — a real step above raw pid+argv.

Performance: brush attacks ONLY the sh-storm. configure runs millions of
builtins (test/[/expr/echo/cd) — in-process brush forks for none; /bin/sh
forks+execs+dynlinks bash for each. Compilers still fork/exec (the bulk of
build CPU) and are untouched. Large win on configure-shaped workloads, modest
on compile-bound ones. Quantify against the exec-storm/parallel-build
benchmarks before committing; do not call it "crazy" unmeasured.

Design constraints:
  - The brush shell is an EXPLICIT per-box toggle, orthogonal to the existing
    box-mode flags (-t passthrough/no-capture, -d direct, -e record-env). NO
    silent magnitude-fallback to /bin/sh: if you select the builtin shell you
    GET the builtin shell, and a construct it doesn't implement is a VISIBLE
    error on a box you chose to run that way — never a quiet downgrade that
    leaves you unsure which shell (and which visibility/perf) you got. This
    bounds brush's compat target to "good enough for opted-in workloads", not
    "universal /bin/sh" (the autoconf tarpit). Default off → real /bin/sh.
  - Lives in the IN-BOX runner (the --inner shim), linked as a library, NOT in
    the engine process (the box must stay bwrap-isolated; brush in the engine
    address space breaches the sandbox). Reports semantic-provenance frames
    over the box channel that already exists.
  - To catch make's per-recipe `sh -c` when the toggle is on, brush is what
    /bin/sh RESOLVES TO in the box's overlay view (the overlay serves that
    virtual mapping) — scoped to that box, not the host.


Ordering: strictly after the engine port + Rust capture/mux are done. This is a
visibility/perf enhancement on a working base, not a milestone dependency.
