# sarun-engine — design decisions (from the 2026-06 architecture review)

Decisions are numbered for reference; "open" items are explicitly undecided.

## D1 · Two programs
A statically-linkable Rust **engine** (this crate) and the Python **UI** as a
pure socket client. The wire protocol (JSON lines over the control socket, the
`subscribe` event feed, the `ui` verb set — see the Python ChannelServer) is
the ONLY contract between them. Everything behind the socket is private and
may change freely: there are no users to migrate (D8).

## D2 · Status
The port is complete: the musl engine is the standalone production binary
(multithreaded FUSE passthrough, the control protocol with `$SLOPBOX_NS`
namespacing, overlay + capture semantics, its own ratatui UI). The Python
prototype was NOT retired as originally planned — it remains the test harness
the `*_rs.py` tests import (wire client + sqlar readers) and still builds patched
pyfuse3 on first run. The behavioral suites (e2e, pjdfstest, the `*_rs.py`
tests) drive the Rust binary directly.

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

## D7 · UI
Two clients speak the wire protocol: the Python Textual client (`prototype/sarun`,
also the test rig via its headless Pilot suite) and the Rust ratatui client in
the engine binary (`engine/src/ui.rs` — `sarun` with no args, or
`sarun --once --sock PATH` for a headless one-frame render the tests assert on,
covered by `test_ui_rs.py`). The protocol is the only contract between them;
terminal-emulation crates (vt100/tui-term) back the PTY pane.

The engine-held PTY + its ratatui pane: a `pty_spawn` control
connection (control.rs `handle_pty_spawn`) makes the engine spawn a command on a
portable-pty PTY it OWNS (pty.rs `serve_pty`) and mux the master ↔ the client
over three new frames (frames.rs): FRAME_PTY_DATA (7, both directions — raw PTY
bytes), FRAME_PTY_RESIZE (8, client→engine, [rows][cols]), FRAME_PTY_EOF (9,
engine→client on child exit). The master is tee'd to an optional sink so the
session can be recorded. The ratatui client (ui.rs `PtyPane`, Pane::Pty, key
'P') feeds the FRAME_PTY_DATA stream into a vt100::Parser and renders it with a
tui_term::PseudoTerminal; focused keystrokes go back as FRAME_PTY_DATA and pane
resizes as FRAME_PTY_RESIZE. Proven end to end: pty.rs `#[cfg(test)]` (real child
→ FRAME_PTY_DATA → vt100 + tui-term → ratatui TestBackend, asserting the child's
marker is ON the grid; plus input-readback, escape-emulation, resize, and
sink-recording) and test_pty_ui_rs.py (the engine half over a real socket).
GENERIC TRANSPORT — `serve_pty` runs whatever argv the caller passes; it does
NOT presume a box or any box parameters. Spawning the argv directly (a bare
command on a PTY, like `script`/`ssh`) is a correct, first-class mode, NOT a
deficiency. Running a full captured box on the PTY is just a different argv:
pass `[<self-exe>, "run", <flags>, "--", <cmd>]` and the engine-held PTY drives
that box (with whatever -t/-d/-e/-b/-C/NAME the user chose) — no special-casing
in the mechanism, because the engine cannot and must not presume the user's box
parameters. The UI's `P` key opens a PROMPT pre-filled with a CONFIGURABLE
default (the "login command": first non-blank line of
$XDG_CONFIG_HOME/slopbox[.NS]/pty_command, else $SHELL -i / sh -i) — a default
the user edits, never an enforced choice. So the only thing saved is a
convenience command; everything else is the caller's parameter. (ui.rs
`pty_default_cmd`/`shell_split`, unit-tested.)

## D8 · No migration obligations
Zero users: compatibility choices are scaffolding for OUR transition (keep
what lets existing tests run; discard when the behavioral suite covers the
ground), never obligations. The wire protocol is kept because our own tests
and UI speak it — the moment that stops being worth it, it too can change.

## D9 · Three complementary execution modes (FUSE always; PTY and brush as toggles)

These are not alternatives — sarun wants all three, composed:

  - FUSE capture — ALWAYS on. Captures the syscalls of arbitrary spawned
    binaries (cc/ld/python); it is the only layer that sees them. The base.
  - PTY mode — toggle, for interactive tty boxes (engine-held PTYs).
    Off → today's headless captured stdout/stderr.
  - brush shell — toggle, for the box's shell being our embedded brush
    (brush-core/brush-parser, verified embeddable). Sits ABOVE FUSE: it adds
    semantic context FUSE can't recover, and removes the sh-storm FUSE would
    otherwise have to capture. Independent of the other two.
    Path-oriented reads by embedded Brush and Kati use the same `SarunFs`
    decoder directly; lifecycle and invariants are documented in
    [DESIGN-direct-filesystem.md](DESIGN-direct-filesystem.md).

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

## D10 · Known gaps

Open correctness, data-path, and test gaps are tracked in `../AUDIT.md`
(reconciled against the code). Audit before trusting the green test count:
self-authored conformance tests share the author's blind spots.

## Build — fully-static musl binary (the only build)
The engine builds only as a fully-static x86_64 musl binary, via
`cargo-zigbuild` + `ziglang` from `uv` (no `apt` toolchain). The dynamic glibc
path is gone. `engine/.cargo/config.toml` sets `build.target` to the musl
triple, but plain `cargo build`/`cargo test` do NOT work — there is no
`musl-gcc`, so the C deps (rusqlite's bundled SQLite, onig_sys) won't compile.
`cargo zigbuild` supplies the compiler/linker (`zig cc`); use it or `make
engine`. One musl source fix: `msg_controllen` is `socklen_t` on glibc but
`size_t` on musl, so the `msg.msg_controllen` casts in `control.rs` use `as _`.
`prototype/test_musl_rs.py` checks the static-linkage guarantee (`file` + `ldd`).

## On-demand box services (svc.serve / svc.dial / svc.declare)

A box can host a server that other boxes reach WITHOUT any shared network:
the server rides the engine's control socket, not a netns. Three verbs:

- `svc.serve {name}` — an in-box process PARKS a connection as one accept
  slot of a named service. The engine holds the parked slots (`SVC_PARKED`).
- `svc.dial {name}` — a caller (the api proxy forwarding a `svc://<name>`
  upstream) becomes a raw stream that the engine splices onto a parked slot,
  byte-for-byte. This is the inbound path INTO a box the tap stack lacks.
- `svc.declare {name, argv, net}` — a box advertises that IT provides an
  on-demand service. The engine stamps the declaring box's meta:
  `svc_provide` (name), `svc_argv` (the `--` payload for the serve sub-box),
  `svc_net` (optional net mode).

**On-demand start (`control::ensure_service`).** When a box dials
`svc://<name>` and nothing is serving (a fresh engine after restart, or the
serve box was discarded), the engine finds the box whose meta declares
`svc_provide == name` and runs its `svc_argv` as a sub-box PARENTED on that
box (`<declaring-id>.SVC-<NAME>`). Parenting means the sub-box reads the
declaring box's captured files with NO apply-to-host. The start is
serialized + idempotent (concurrent callers coalesce), detached (`setsid`,
so it outlives whatever triggered it), and waits for a slot to park before
forwarding. So the service is never left running to babysit — it comes up on
first use and again after every restart.

**`oaita local` is one instance.** `oaita local` (UI: F4) ONLY downloads the
model into box `OAITA-LOCAL` (captured, never applied) and declares the
`oaita-local` service on that box; oaita.toml points at
`svc://oaita-local#/v1`. The first box call starts
`OAITA-LOCAL.SVC-OAITA-LOCAL` (llama-server + the svc bridge) on demand. Any
box — an oci image, any downloaded-server box — can advertise a server the
same way by declaring `svc_provide`/`svc_argv`.

**Debugger provider identity.** QEMU debugger composition uses exactly one
declared service name, `viros-debug`. A debug registration resolves one unique
box declaring that identity and one architecture-matching
`viros-kernel-bundle-v1` from the consumer's captured box/RO-attachment/parent
lookup chain. The result contains box IDs and provider-root-relative artifact
descriptors only; host paths, environment variables, and user debugger
arguments are not resource-selection inputs.

**Model picker (Api pane).** Which model `oaita local` downloads is a UI
choice, not a hardcoded recommendation (those go stale). The Api pane offers
a picker whose catalog is a LIVE HuggingFace query for currently-popular Q4
GGUF instruct models (`oaita.models` verb → `oaita::models::catalog`),
overridable by `{config_home}/oaita-models.toml` and backed by a labelled
offline snapshot. Picking a model runs `oaita local --model-url <url>` on a
PTY — a BOXED download, so there's no host/box confusion and nothing to
apply. The picker auto-opens the first time the Api pane is shown with
neither an external API nor a local model configured (`oaita.status` reports
`kind == "none"`); it's also on the pane's F4 action menu. A `--force` model
swap clears any stale `*.gguf` so the serve path picks up the new file.
