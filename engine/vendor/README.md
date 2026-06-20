# Vendored, patched upstreams (`uu_cat`, `findutils`, …)

This directory holds **forks of third-party crates that we patch in place** so
they can run as in-process brush builtins. They are **not** git submodules — a
gitlink stores only a pointer to someone else's commit and tracks none of the
file contents, so our patches could not live in this repo's history. Instead
each crate is vendored as:

* one **pristine-import commit** — the upstream source, *verbatim*, at a pinned
  release; this is the rebase base, and
* a series of **patch commits** on top — our adaptation, kept as distinct
  commits and **never squashed**.

That structure is exactly what makes upstream **pullable**: to update, you
replace the pristine base with a newer upstream drop and `git rebase` replays
the patch series onto it, so a 3-way merge surfaces conflicts **only** where
upstream changed the same lines we did. Keeping the base byte-identical to
upstream is load-bearing for that; do not "tidy" the base commit.

> The brush crates (`brush-core`, `brush-builtins`, …) are also vendored here
> but for a different reason (a pre-release `brush-core` not yet on crates.io);
> they are not the patched-fork pattern this doc is about.

## What is vendored, and how it is consumed

| crate | upstream | pinned at | wired into the engine via | what we changed |
|-------|----------|-----------|---------------------------|-----------------|
| `uu_cat` | crates.io `uu_cat` (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uu_cat = { path = "vendor/uu_cat" }` in `engine/Cargo.toml` | Injected-I/O entry `cat::cat(out, out_fd, stdin, stdin_fd)` that writes the shell's logical `OpenFile` sink/source with no process-global stdio, keeping the Linux `splice(2)` fast path. A thin `uumain` bridge is retained for the standalone/`--invoke-bundled` path. |
| `findutils` | github.com/uutils/findutils, tag `0.9.1` | `0.9.1` | `findutils = { path = "vendor/findutils" }` in `engine/Cargo.toml`; builtins in `engine/src/find_builtin.rs` and `engine/src/xargs_builtin.rs`, registered in `engine/src/brush.rs` (`box_builtins_opt`) | Reduced to a **find + xargs** library (`lib.rs` = `pub mod find; pub mod xargs;`; the `locate`/`updatedb`/`testing` modules + bins removed). **find:** added `Dependencies::{get_error_output, get_input}` so diagnostics and the `-files0-from -` read go through the shell's logical stderr/stdin. **xargs:** added an `XargsIo` trait (`take_input`/`output`/`error_output`) so item input and xargs's own output/`-t`/warnings/errors go through the logical streams; `xargs_main_with_io` is the embedder entry. Both builtins run their `_main` on a worker thread that `unshare(CLONE_FS)`s + `chdir`s to the shell's logical cwd (see the builtin files for that rationale). **Known limitation:** the commands `find -exec` / `xargs` *spawn* inherit the engine's real fd 1/2, so a spawned child's stdout/stderr does NOT honor the box's logical pipes/redirects (`xargs cmd > file` / `xargs cmd | downstream` misroute). Pending the child-fd-wiring / in-process-dispatch work. |

Provenance (the exact upstream version) is recorded in each crate's
**pristine-import commit message**, which is the one true source — find it with:

```bash
git log --oneline --grep '^vendor: import pristine'
```

Do **not** rely on hard-coded commit hashes anywhere: the update procedure
rebases, which rewrites them. Always re-derive the base from that commit
message convention.

## Updating a vendored crate to a newer upstream

Worked example: bumping `findutils` `0.9.1` → `0.10.0`. (For `uu_cat`, fetch the
new crate from crates.io instead of git, and skip the find/xargs trim notes — the
shape is identical.)

```bash
cd ~/sarun

# 0. Identify the rebase base (the pristine-import commit), by message — NOT a
#    remembered hash.
BASE=$(git log --format=%H --grep '^vendor: import pristine findutils' -1)

# 1. Fetch the new upstream pristine.
cd /tmp && rm -rf fu-new
git clone --depth 1 --branch 0.10.0 https://github.com/uutils/findutils fu-new

# 2. Rebuild the pristine BASE in place: check it out, swap the vendored files
#    for the new upstream, amend. Vendor the SAME selection we vendor today —
#    src/ + Cargo.toml + LICENSE + README only (the dev-only tests/, test_data/,
#    benches/, util/ trees are intentionally NOT vendored).
cd ~/sarun
git switch -c vendor-bump "$BASE"
rm -rf engine/vendor/findutils/{src,Cargo.toml,LICENSE,README.md}
cp -r /tmp/fu-new/src engine/vendor/findutils/src
cp /tmp/fu-new/{Cargo.toml,LICENSE,README.md} engine/vendor/findutils/
git add engine/vendor/findutils
git commit --amend -m "vendor: import pristine findutils 0.10.0 (find)"

# 3. Replay our patch series onto the new pristine base.
git rebase --onto vendor-bump "$BASE" <work-branch>
#    Conflicts appear ONLY where 0.10.0 touched the same lines our patches did
#    (e.g. an upstream refactor of parse_files0_args means the get_input thread
#    re-applies there). Fix, `git add`, `git rebase --continue`.
#    The find+xargs trim (a patch commit) re-applies by deleting locate/updatedb/
#    testing from the new upstream; resolve if upstream restructured them.

# 4. Verify (see next section). Then:
git push --force-with-lease       # base→patches preserved, never squashed
git branch -D vendor-bump
```

The `--force-with-lease` is expected: a vendor bump rewrites every SHA after the
base. That is the point — the history stays `pristine base → patch → patch → …`.

## Verifying after an update (the regression net)

Run all of these; they are the same checks each patch was originally landed
against.

**1. The vendored lib compiles clean** (host target — fast, skips the musl
zigshim):

```bash
cd engine/vendor/findutils
CARGO_TARGET_DIR=/tmp/fu cargo build --lib --target x86_64-unknown-linux-gnu   # 0 warnings
```

**2. Upstream's own unit suite passes against the patch.** The in-source tests
need `test_data/`, which we do not vendor — so overlay the patched module onto a
clean upstream checkout and run there:

```bash
cd /tmp && rm -rf fu-test && git clone --depth 1 --branch 0.10.0 https://github.com/uutils/findutils fu-test
cp -r ~/sarun/engine/vendor/findutils/src/find/. fu-test/src/find/
cp -r ~/sarun/engine/vendor/findutils/src/xargs/. fu-test/src/xargs/
printf 'pub mod find;\npub mod xargs;\n' > fu-test/src/lib.rs   # the modules we vendor
cd fu-test && CARGO_TARGET_DIR=/tmp/fu-test cargo test --lib find:: xargs::
#   Expect: all green EXCEPT test_no_permission_file_error and
#   get_or_create_file_test, which fail when run as ROOT (root bypasses
#   chmod 000) — confirm they fail on pristine too, i.e. not your fault.
```

**3. The whole static musl engine builds:**

```bash
cd ~/sarun && make engine        # → engine/target/x86_64-unknown-linux-musl/release/sarun
```

**4. Behavior in a real box brush shell.** `brush-sh -- <argv>` runs the box
shell in-process with inherited fd 1/2 (no bwrap needed):

```bash
BIN=engine/target/x86_64-unknown-linux-musl/release/sarun
$BIN brush-sh -- sh -c 'cd engine && find . -maxdepth 1 -name Cargo.toml'        # logical cwd
$BIN brush-sh -- sh -c 'find /no/such 2>&1 | sed s/^/ERR:/'                       # logical stderr
$BIN brush-sh -- sh -c 'cd engine && printf "Cargo.toml\0" | find -files0-from - -maxdepth 0'  # logical stdin
```

**5. (optional but recommended) An independent blind review** of the new diff —
hand `git diff <base>..HEAD -- engine/vendor/<crate>` plus the glue file to a
fresh agent with no hints and ask only "what does it do, does it work, is it
safe." That is how both `uu_cat` and `findutils` were signed off.

## Conventions (keep these stable — the tooling above depends on them)

* The pristine import commit message **must** start with
  `vendor: import pristine <crate> <version>`.
* Exactly one pristine-import commit per crate; everything after it is a patch
  commit. **Never squash** the base into the patches or the patches together.
* The base commit's tree **is** upstream, byte-for-byte. If you need to change
  what we vendor (file selection), do it in a *patch* commit, not the base.
