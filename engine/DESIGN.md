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
- **m4+:** musl static link (the static-link half is DONE — see "m4 status"
  below); the Python file becomes UI client + bootstrap (fetch-or-build the
  engine binary, hash-keyed cache — the same mechanism that builds patched
  pyfuse3 today). The pyfuse3 patch dies at m3.

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

## D5 · FUSE read-passthrough — GATED ON THE `passthrough` FILE RULE (path-only)
Kernel read-passthrough (kernel 6.9+, fuser opened_passthrough): a read-only
open can register a kernel backing fd so the kernel serves reads with the daemon
out of the loop (the build-read-storm win). But there is a hard kernel wall,
verified by test: **an inode with any live passthrough fd rejects every new
open-for-WRITE with EIO**, and the daemon cannot intercept it — the kernel fails
open() before any reply matters (held read fd + `>`/`>>` of the SAME file → EIO;
read→close→write or writing a DIFFERENT file → fine; the limit is per-INODE, not
per-fd, so a write open requesting passthrough EIOs too).

So passthrough is ONLY sound for files the user DECLARES host-direct — and that
is exactly what the existing `passthrough` file rule already means (host-direct:
reads served from the host, writes straight to the host, uncaptured). The
FIRST, blanket implementation guessed "any read-only open is safe" and EIO'd the
moment such a file was written — the bug. The fix is NOT a new rule: the
`passthrough` rule drives read-passthrough. A `passthrough`-ruled (or `-d`
direct) READ open gets a kernel backing fd; everything else stays daemon-served
(captured paths must — the daemon mediates copy-up + per-write ctx.pid
attribution, the load-bearing `sh -c 'cmd > out'` case). Exec opens stay
daemon-served (mmap of a passthrough-backed file EIOs). The kernel write-EIO is
thus scoped to user-declared host-direct paths, where it is the rule's own
contract, not a surprise.

CRITICAL — passthrough rules are PATH-ONLY (the parser skips any clause with a
`:` matcher, so `passthrough box:A.B …` is ignored). This is required, not a
limitation, and the nested case is why: a box-SCOPED rule (host-direct in child
A.B but CAPTURED in parent A) would make A.B read A's still-captured blob through
a passthrough fd, and A copying-up that blob would hit the per-inode write-EIO.
Path-only means a passthrough path is host-direct in EVERY box, so a child reads
it straight from the HOST — never through a parent's overlay, no copy-up, no
divergence. (Verified: test_passthrough_rule_rs.py — rule gates passthrough; a
`box:` line is ignored; a nested child reads the passthrough file from the host
directly. test_concurrent_rw_rs.py guards that CAPTURED files, never
passthrough'd, keep concurrent read+write correct.)

Host-direct metadata is host-direct too: setattr (truncate/chmod/chown/utimes)
on a `passthrough`/`-d` path goes straight to the host file, never copy-up. This
fixed the O_TRUNC bug where `printf NEW > existing_ruled_file` (whose truncate
the kernel delivers as setattr size=0) was routed through copy_up — capturing a
spurious row AND leaving the host file's tail intact (the write went host-direct,
the truncate went to a blob). Now the truncate hits the host: clean bytes, no
capture (test_passthrough_rule_rs.py covers it).

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

## D10 · Known gaps from the adversarial audit (2026-06-14) — DO NOT trust green alone

A skeptical re-audit (after `dissolve` was found hollow + rigged-green) classified
the port honestly. Recorded so these aren't lost behind a passing test count:

CONFIRMED:
 - C1 apply metadata restoration: mtime restore is now tested (cross-checked) and
   works. owner/xattr restore use Rust-ONLY side tables (xattr/ownership/rdev) that
   the Python schema lacks — so they restore nothing for Python-written boxes and a
   Python client won't restore them from a Rust box. This is an asymmetric,
   Rust-only extension (acceptable per D8 zero-users, but the "Python clients work
   unmodified" claim does NOT hold for owner/xattr). owner/xattr apply-restore
   remain effectively untested (chown is squashed; no setfattr probe).
 - C2 nested boxes: read-through-parent and dissolve copy-down are now DONE.
   resolve(bid, rel) walks the parent chain (whiteout→Absent, own entry→that,
   else parent, root→host); attr/copy-up/readdir all use it, proven by a real
   invariant test (child reads + copies up FROM a parent-only file). dissolve of
   a box WITH children no longer refuses: it copies every path the parent
   captured DOWN into each child lacking its own entry (review::copy_down_entry),
   so the child's merged view survives the parent being freed, then re-parents
   the children onto the dissolving box's own parent (overlay.set_box_parent +
   sqlar meta) — tested on the finished-box path with a discard rule (the file
   never hits the host, so only copy-down can preserve the child's view).
   Nested LAUNCH is now DONE too: the runner detects in-box by the presence of
   the engine socket bind-mounted at /tmp/.slopbox/ui.sock (it forwards that
   socket into every box), sends a single-segment `relname` plus its own pidfd
   over SCM_RIGHTS, and roots bwrap by binding the parent-exposed
   /<KIDS_DIR>/<id>. The engine derives the host pid from the pidfd
   (/proc/self/fdinfo) and the enclosing box from the /proc PPid ancestry
   (box_runpids map) — never trusting any pid/sid from the message body — then
   parents the new box under it. Tested end-to-end (test_engine_rs nested
   section): a box run inside a box parents under the enclosing box and reads a
   parent-only file through read-through-parent. NOTE: the register fd-peek must
   poll for the bytes first — a non-blocking peek races the runner's sendmsg and
   silently drops the pidfd, which then mis-derives the parent (fixed).
   LIVE-child copy-down is now correct too: dissolve no longer refuses a running
   child — copy_down_entry and the re-parent meta write route through the live
   BoxState (its one connection + RAM `kinds` mirror via overlay.live_box) when
   the child is mounted, so the running FUSE view serves the copied-down entry
   immediately (no rival on-disk handle racing the serve thread). Tested: a file
   written only in the parent overlay, never touched by a LIVE child, still
   reads through the child's mount after the parent is dissolved (discard rule,
   so only copy-down can preserve it). The same fix retires S5: the `rename`
   verb now writes the name meta through the live BoxState when the box is
   running, not a second connection.

   Echo chaining is now DONE — the nested cluster is complete. Capture switched
   from teeing to the live-echo mux: --inner makes the box-root sink files its
   child's stdout/stderr (every write flows through the overlay, recorded with
   per-write pid attribution) and the engine frames those bytes back as ECHO
   over the box's one muxed connection; --inner replays them to its real fd 1/2.
   For a NESTED box that fd 1/2 is the parent's sink, so the child's output
   chains UP level by level to the top-level terminal. MUTE/UNMUTE solve the
   re-capture problem: --inner sends a MUTE frame carrying its own pidfd; the
   engine resolves its host tgid and, while muted, ECHOes that pid's sink writes
   onward but does NOT record them — so a child's readback passing up through an
   ancestor sink is captured exactly once, at its origin box, never multiplied.
   ECHO_DONE (sent when the box's last sink releases at child exit) lets --inner
   drain the tail without truncation before closing. Tested: a nested child's
   stdout marker chains to the top-level runner, is recorded in the CHILD box,
   and is NOT re-recorded in the PARENT (MUTE). The PTY/tmux feature (D7/D9) now
   has its mux foundation.

WEAK TESTS (not proven wrong, but self-graded by shape, not Python-equality):
 - S1 hunks: NOW cross-checked against Python's _build_hunks_display byte-for-byte
   on a 2-hunk change (fixed). But similar(Myers) vs difflib(Ratcliff-Obershelp)
   can diverge on repeated/moved lines — equality holds on tested inputs only.
   struct_finish and patch_text are still shape-only.
 - S2 proc_info/writer_id/first_writer_id: shape-only, and tested on a single-writer
   box where first==last (a swapped writer/last_writer mapping would pass).
 - S3 capture provenance first-vs-last-writer: never exercised (single writer).
 - S4 untested code: process_env, box_drop, special-node (fifo/dev) APPLY path,
   the top-level control-type CLI variants beyond patch/rename.
 - S5 FIXED: rename of a LIVE box (and dissolve copy-down / re-parent into a live
   child) now route through the live BoxState's one connection + RAM mirror
   (overlay.live_box), not a rival on-disk handle. Live paths are tested.

THE REAL FIX (methodological): self-authored conformance tests share the author's
blind spots (dissolve proved it). The port should be re-grounded on (a) cross-engine
EQUALITY checks against the Python functions on the same sqlar (done for hunks; do
for proc_info/session_changes/struct/patch), and (b) the actual Python behavioral
suite + pjdfstest run against the Rust mount — which has NEVER been done. Until then,
treat the conformance green count as necessary, not sufficient.

## m4 status — fully-static musl binary (DONE)
The engine builds as a fully-static x86_64 musl binary with no dynamic libc, a
truly portable single executable. The DEFAULT `cargo build --release` is
unchanged (dynamic glibc, what the test harness builds).

Build:
```
rustup target add x86_64-unknown-linux-musl   # rust-std for musl
apt-get install -y musl-tools                  # provides musl-gcc
cd engine && cargo build --release --target x86_64-unknown-linux-musl
```
`engine/.cargo/config.toml` scopes the musl-only setup so the default target is
untouched: it sets `linker = "musl-gcc"` for the musl target and
`CC_x86_64-unknown-linux-musl = "musl-gcc"` so the `cc` crate compiles
rusqlite's bundled SQLite C with the musl ABI (the one real sticking point —
fuser/libc were fine). One source fix was needed for musl: `msg_controllen` is
`socklen_t` (u32) on glibc but `size_t` (usize) on musl, so the two
`msg.msg_controllen = cmsg.len()` assignments in `src/control.rs` now cast
`as _` (target-correct, ABI-neutral on glibc).

Proof (this machine):
```
$ file   target/x86_64-unknown-linux-musl/release/sarun
… ELF 64-bit … static-pie linked … statically linked
$ ldd    target/x86_64-unknown-linux-musl/release/sarun
        statically linked
```
~4.8 MB (vs ~4.7 MB glibc dynamic). VERIFIED it still WORKS statically:
`sarun engine` brings up its control socket and a real `sarun run -- echo …`
box runs against it and exits 0 — FUSE mount + bwrap function under the static
binary. Guarded by the standalone `test_musl_rs.py` (self-skips with a clear
message when the musl target hasn't been built — never a fake pass).
