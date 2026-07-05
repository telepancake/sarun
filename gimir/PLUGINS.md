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
embedded interpreter — it is finishing that seam:

## Track A (recommended, near-term): Python drivers out-of-process

1. **Generic job kind `cmd`** — DONE alongside this doc. A job whose
   `src` is a shell command line (run `/bin/sh -c src`, `dest` passed
   as `$1` and as `$SARUN_MIRROR_DEST`). Any script — `uv run
   mywiki.py …` included — is now schedulable with the full state
   machine (running/error tail/stopped auto-resume/pause/force-run)
   and shows in the Mirrors pane and `sarun mirror ls`. Adding new
   mirror logic no longer requires touching the engine.
2. **`sarunmirror` Python module** — the host bindings. A pure-python
   package (stdlib-only, `uv run --with` for anything extra, matching
   the repo's no-venv rule) exposing:
   - the depot chain format: open root, read head/window, append a
     canonical or delta layer, checkpoint (port of the `depot` crate's
     file format; parity tests Rust-writes/Python-reads and reverse,
     same style as libtestsarun's sqlar tests);
   - the durability protocol: per-root flock, dirty flag
     (`meta.import_dirty`) around write sessions, watermark tables —
     so a Python driver crashing keeps the same self-repair guarantees
     the Rust drivers have;
   - an engine client: the UDS verbs (`mirror_jobs`, attach verbs) so
     a script can attach its own snapshot to a box or report progress.
3. **Wikipedia gap-fill in Python first** — new wiki logic (other
   dump series, recentchanges polling, langlinks…) lands as
   `sarunmirror`-based scripts scheduled via `cmd` jobs; whatever
   proves out and needs the speed graduates into `wikimak` later, or
   never.

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

- [x] `cmd` job kind (engine, this commit)
- [ ] `sarunmirror/` package skeleton: flock + dirty flag + bookkeeping
      helpers + engine UDS client (no depot format yet), with a demo
      driver scheduled as a `cmd` job in a test
- [ ] depot chain read in Python + parity test vs `depot` crate
- [ ] depot chain append in Python + crash-repair parity test
- [ ] (spike, optional) rustpython-vm behind a feature flag: run a
      hook script from config at flow-policy time; measure binary-size
      and compile-time cost before committing to it
