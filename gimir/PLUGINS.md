# Scripting the mirrors — plan

The ask: sarun should be a **stable engine**, and mirroring logic should
be writable in a standard, popular language with host bindings in a
module — so that "add missing wikipedia logic" is a script edit, not a
full engine rebuild.

## What the architecture already gives us

The seam is already out-of-process. The scheduler (`engine/src/
mirrors.rs`) spawns *driver binaries from PATH* and only records their
exit/stderr; the engine never links the fetch logic (it has no HTTP
stack at all). The stores are plain SQLite files with documented
shapes, and `prototype/libtestsarun.py` (5k lines, stdlib `sqlite3` +
`zlib` only) is standing proof that Python reads/writes sarun's SQLite
formats with full parity against the Rust side — the parity tests
already enforce it for sqlar.

So the cheapest route to "logic in Python, engine stable" is not an
embedded interpreter — it is finishing that seam. And NOT by porting
the store formats to Python: two codebases owning one on-disk format
would make the durability guarantees only as strong as the weaker
writer. The single-owner rule:

> Anything that defines bytes-at-rest or crash-safety — chain format,
> dirty flag, flock, checkpoint, repair — is Rust, in ONE place,
> reached through a narrow verb. What gets scripted is corpus logic:
> what to fetch next, which series, how to parse upstream, cooldown
> policy. That's the code that churns; it never touches a depot page.

CORRECTED (2026-07-05, see ATTACH-CONVERGENCE.md "North star"): the
"one place" is THE DEPOT — not each mirror crate's private store.
Per-mirror ingest verbs (`wikimak ingest --stdin`) were still the
crow's-nest layering: three app crates owning three formats built on
depot internals. The engine exposes the depot API itself (chains,
append, head/history, readout, attach) as verbs; missing capabilities
get added to the depot, not built beside it; mirror crates stop
owning bytes-at-rest. The scripting story is unchanged in spirit and
simpler in practice: business logic (keys, revision identity,
encode/decode policy, display) in Python against a stable algebra-
shaped API — out-of-process first, embedded rustpython as a later
optimization, since the binding surface no longer depends on any
mirror's format.

## Track A (recommended, near-term): Python drivers out-of-process

1. **Generic job kind `cmd`** — DONE alongside this doc. A job whose
   `src` is a shell command line (run `/bin/sh -c src`, `dest` passed
   as `$1` and as `$SARUN_MIRROR_DEST`). Any script — `uv run
   mywiki.py …` included — is now schedulable with the full state
   machine (running/error tail/stopped auto-resume/pause/force-run)
   and shows in the Mirrors pane and `sarun mirror ls`. Adding new
   mirror logic no longer requires touching the engine.
2. **Store porcelain on the existing drivers** — split each driver
   into the two halves it already almost has. The store half (Rust,
   stable, rarely rebuilt): an ingest verb per driver — `wikimak
   ingest --stdin` reading a framed stream of revision records
   (ndjson header line + payload bytes), appending under the
   EXISTING flock/dirty-flag/repair protocol, echoing appended revids;
   likewise `gitdepot`/`ietfmak`. No format code is duplicated — the
   same crate code is reached over a pipe. The acquisition half
   (script, hot-swappable): decides what's new upstream and feeds the
   pipe.
3. **`sarunmirror` Python module** — the host bindings, which is NOT
   a format library. Pure-python (stdlib-only; `uv run --with` for
   extras, per the no-venv rule):
   - record framing + subprocess wrappers for the porcelain verbs;
   - bookkeeping-sqlite helpers (watermarks, cooldowns) — scripts may
     own bookkeeping because §3 already requires it to be disposable
     and re-derivable from the chain; a script can't corrupt what the
     depot cares about;
   - an engine UDS client (`mirror_jobs`, attach verbs) so a script
     can attach its snapshot to a box or report progress.
   `libtestsarun`-style Python format code remains what it is today: a
   TEST oracle for parity, never a production writer.
4. **Wikipedia gap-fill in Python first** — recentchanges polling is
   then ~100 lines: query the API for revids past the watermark,
   fetch content, stream records into `wikimak ingest`, bump the
   watermark; scheduled as a `cmd` job. Other dump series, langlinks,
   another wiki — same shape. Whatever proves out and needs the speed
   graduates into Rust later, or never.

Why out-of-process is the right default for *mirroring*: the work is
I/O-bound (interpreter speed is irrelevant), a crashed driver is
already contained + surfaced (`error`/`stopped` states, stderr tail),
and the fetch side is destined for tap boxes anyway (MIRRORS.md) —
a child process is exactly the unit that moves into a box; an embedded
interpreter never can.

## Track B (later, only if in-process hooks are wanted): embedding

If we ever want scripts *inside* the engine — per-flow network policy,
in-engine transforms on attach — the candidates, assessed against the
hard constraint (fully static musl binary, cargo-zigbuild, one file):

| option | verdict |
|---|---|
| **RustPython** (`rustpython-vm`, freeze-stdlib) | The one to spike. Pure Rust → static-musl links like everything else; stdlib frozen into the binary (no libpython/stdlib-on-disk). Costs: big compile, CPython-subset semantics, ~10-50× slower — fine for hooks, wrong for bulk loops (which Track A keeps out-of-process anyway). |
| PyO3 + embedded CPython | Rejected: static libpython on musl is a known tarpit, needs a stdlib tree on disk, breaks the one-binary property. |
| Lua (`mlua`, vendored) | Easiest embed, but fails the "standard popular language" requirement for data logic. |
| WASM (`wasmtime` + componentized plugins) | Most general plugin ABI, any language; heavy dependency and the Python→component toolchain is still awkward. Revisit if third-party plugins become a goal. |

Decision rule: embed only for hooks that must run in the engine's
address space at sub-millisecond cost. All mirroring logic stays
Track A regardless.

## Chips

- [x] `cmd` job kind (engine)
- [ ] `wikimak ingest --stdin`: framed record stream → chain append
      under the existing durability protocol; crash mid-stream test
      (kill the ingest, reopen repairs)
- [ ] `sarunmirror/` package: framing + porcelain wrappers +
      bookkeeping helpers + engine UDS client, with a demo driver
      scheduled as a `cmd` job in a test
- [ ] first real script driver: wikipedia recentchanges polling
- [ ] ingest verbs for `gitdepot` / `ietfmak`
- [ ] (spike, optional) rustpython-vm behind a feature flag: run a
      hook script from config at flow-policy time; measure binary-size
      and compile-time cost before committing to it
