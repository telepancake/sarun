# Vendored, patched upstreams (`bumba/vendor/` — generated, not tracked)

Each vendored crate is assembled at build time from:

* a **pristine pinned upstream** — a crates.io tarball verified by sha256, or a
  git commit fetched by hash (`bumba/vendor.toml`), plus
* a **patch series** on top — never squashed, one patch per logical change
  (`bumba/vendor-patches/<crate>/{files,series,NNNN-*.patch}`).

`make -C bumba vendor` runs `bumba/tools/vendor.py`, which downloads each
upstream into `bumba/.vendor-cache/`, copies the crate's file selection
(`files` — dev-only trees like tests/, benches/, util/ are not selected), and
applies the series with `git apply` into `bumba/vendor/<crate>`. Both
`bumba/vendor/` and the cache are gitignored: the repo carries only pins and
diffs, never upstream source.

`files` lists every upstream path the crate consumes, relative to the crate
root (for git sources with a `subdir`, relative to that subdir). Files the
patches *create* are not listed — they arrive with the series.

## The garden path (day-to-day commands)

Everything below is `bumba/tools/vendor.py` (also: `bumba/tools/vendor.py help`).
The loop is always: **edit the assembled tree, then capture**. You never edit
patch files by hand on the happy path.

**Change vendored code (new logical change):**

```bash
make -C bumba vendor                         # assemble (no-op when stamped)
$EDITOR bumba/vendor/<crate>/src/…           # hack until build+tests are green
python3 bumba/tools/vendor.py diff <crate>   # review your delta vs the series
python3 bumba/tools/vendor.py refresh <crate> -m "what and why"
python3 bumba/tools/vendor.py check <crate>  # series reproduces the tree
git add bumba/vendor-patches/<crate>
```

**Fix up the most recent patch instead of minting a new one:**

```bash
python3 bumba/tools/vendor.py refresh <crate> --amend
```

**Add a new external codebase:**

```bash
python3 bumba/tools/vendor.py add <crate> --version 1.2.3
python3 bumba/tools/vendor.py add <crate> --git URL --commit HASH [--subdir DIR]
# → pins it in vendor.toml, writes vendor-patches/<crate>/{files,series},
#   assembles the pristine tree. Prune dev-only trees from `files` if wanted,
#   then hack + refresh as above.
```

**Update an existing external codebase:**

```bash
$EDITOR bumba/vendor.toml         # bump version+sha256 / commit
make -C bumba vendor              # reapplies the series onto the new base
# a patch that no longer applies fails LOUDLY with re-spin instructions;
# fix that one patch, rerun. Never fold patches while re-spinning.
```

`refresh` restamps the assembled tree, so a following `make -C bumba vendor`
won't clobber what you're running; `check <crate>` is the proof the
series reproduces it. `--force [crate]` reassembles from scratch (also the
undo button: delete a bad patch from `series`, `--force`, and the tree is
back to the recorded state).

## What is vendored, and how it is consumed

| crate | upstream | pinned at | wired into Bumba via | what we changed |
|-------|----------|-----------|---------------------------|-----------------|
| `uu_cat` | crates.io `uu_cat` (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uu_cat = { path = "vendor/uu_cat" }` in `bumba/Cargo.toml` | Injected-I/O entry `cat::cat(out, out_fd, stdin, stdin_fd)` that writes the shell's logical `OpenFile` sink/source with no process-global stdio, keeping the Linux `splice(2)` fast path. A thin `uumain` bridge is retained for the standalone/`--invoke-bundled` path. |
| `uu_head` `uu_tail` `uu_wc` `uu_nl` `uu_tac` `uu_basename` `uu_dirname` `uu_seq` `uu_expr` `uu_tr` `uu_cut` `uu_uniq` `uu_sort` `uu_uname` `uu_nproc` `uu_id` `uu_whoami` | crates.io (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uu_<name> = { path = "vendor/uu_<name>" }`; a native `SimpleCommand` per util in `bumba/src/coreutils.rs` (`<Name>Builtin`, registered in `box_builtins_opt`), executed inline through `run_coreutil_scoped` | Same model as `uu_cat`: a logical injected-I/O entry that writes the shell's logical `OpenFile` sink/source/stderr with **no process-global stdio and no `dup2`** (pipeline-safe), keeping fast paths byte-for-byte. They run IN-PROCESS **unconditionally** — there is no per-argv gate and no fork-to-the-box's-binary fallback (an earlier gate scheme that detected divergent argvs and forked an *unknown* external tool — busybox in an Alpine box — was scoured out; it was itself a divergence and masked real bugs). uucore's `show!`/`set_exit_code`/`process::exit` are removed from the live path (diagnostics accumulated+returned or written to the logical `err`; exit codes returned, e.g. `expr` 0/1/2). Where uutils genuinely diverged from POSIX/GNU the **fork is patched** to match — `uu_expr` rejects a leading-`+` integer and clamps out-of-range `substr` (see its patch commit); other differences are uutils' own (the box's coreutils *is* uutils). Scoped guards install and restore the shell environment, umask, utility name, and keyed localization without creating an OS thread per call. `uumain` remains a thin bridge. The read-only info utils (`uname`/`nproc`/`id`/`whoami`) share the same model but a no-stdin, no-cwd `<name>_main(args, out, err)` entry (built by `info_builtin!`) and report the box's real sysinfo/identity. Syscall-level contract (`engine/test_builtin_contract.py`, `make test-contract`) asserts in-process execution, logical-I/O, and multi-util localization. |
| filesystem ops: `uu_cp` `uu_mkdir` `uu_rmdir` `uu_rm` `uu_mv` `uu_ln` `uu_touch` `uu_readlink` `uu_realpath` `uu_mktemp` `uu_tee` `uu_chmod` `uu_chown` `uu_install` | crates.io (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uu_<name> = { path = "vendor/uu_<name>" }`; a `SimpleCommand` per util in `bumba/src/coreutils.rs` (built by the `fs_builtin!` / `fs_builtin_stdin!` macros), executed inline through `run_coreutil_scoped`, registered UNCONDITIONALLY (not under the `bundle_coreutils` gate) | Same injected-I/O model as the stream/filter group, PLUS a **logical-cwd** rewrite: each crate gains a `<name>_main(args, cwd, out, err[, stdin])` entry that resolves every relative path operand (and a `-t`/`--target-directory` value) against the shell's logical cwd — the process is never `chdir`'d. Verbose/debug output, diagnostics, and (for `rm`/`mv`/`ln`) the interactive `-i` prompt all route through the shell's logical out/err via thread-local buffers + crate-local `show!`/`show_error!`/`show_warning!`/`prompt_yes!`/`println!` macros that SHADOW uucore's process-global ones; `set_exit_code` is shimmed to a thread-local and every entry resets/drains its per-thread call state. The `-i` prompt answer is read from the shell's **logical stdin**, NEVER the engine's fd 0 (a stray read corrupts a control channel/pipe; EOF ⇒ "no"). Diagnostics stay byte-faithful to GNU except that a resolved relative operand displays its cwd-absolute path (same property as the original `cp` port). `readlink`/`realpath`/`mktemp` (path-resolving) and `chmod`/`chown`/`install` (metadata ops) share the no-stdin `<name>_main(args, cwd, out, err)` shape built by `fs_builtin!` (`chown` routes through the patched `uucore::perms::chown_base_io`). `tee` adds the logical-stdin read (`tee_main(args, cwd, out, err, inp)`, hand-written `TeeBuiltin`): it copies stdin to the logical stdout AND its file operands, never reading the engine's fd 0. `uumain` retained as a thin bridge (real process stdio). |
| `uucore` | crates.io `uucore` (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uucore = { path = "vendor/uucore" }` — redirects every vendored `uu_*` builtin crate + `brush-coreutils-builtins` to this one copy | The shared uutils runtime, patched so MULTIPLE distinct utils can run correctly in ONE process — something uutils (one util per process) never anticipated, and which the in-process builtins require. **(1)** Localization is a scoped, per-thread cache keyed by utility **and locale**. Parsed Fluent bundles are reused, resources are owned rather than leaked, and dropping the guard restores the caller's binding; this prevents first-util-wins cross-contamination without a fresh OS thread per invocation. **(2)** `util_name()` (`lib.rs`) has an allocation-free scoped override so a util's diagnostics carry its real name (`wc:`) instead of the engine's argv[0] (`bumba:`), including nested calls. **(3)** The logical environment and umask use matching scoped guards, installed before localization reads `LANG`/`LC_*`. **(4)** `build.rs` embeds sibling `uu_<util>` crates' locales (Bumba's vendor forks are named plainly `uu_<util>`, not the registry's `uu_<util>-<version>`), and reruns when the vendor parent changes. uucore's locale and logical-context unit tests exercise switching and restoration. |
| `findutils` | github.com/uutils/findutils, tag `0.9.1` | `0.9.1` | `findutils = { path = "vendor/findutils" }` in `bumba/Cargo.toml`; builtins in `bumba/src/find.rs` and `bumba/src/xargs.rs`, registered in `bumba/src/coreutils.rs` (`box_builtins_opt`) | Reduced to a **find + xargs** library (`lib.rs` = `pub mod find; pub mod xargs;`; the `locate`/`updatedb`/`testing` modules + bins removed). **find:** added `Dependencies::{get_error_output, get_input}` so diagnostics and the `-files0-from -` read go through the shell's logical stderr/stdin. **xargs:** added an `XargsIo` trait (`take_input`/`output`/`error_output`) so item input and xargs's own output/`-t`/warnings/errors go through the logical streams; `xargs_main_with_io` is the embedder entry. Both builtins run their `_main` on a worker thread (the findutils engine is synchronous; the `Shell` isn't `Send`); `find` resolves relative start paths against the shell's logical cwd via a `Dependencies::cwd` hook — NOT `unshare(CLONE_FS)`/`chdir` (see `find_builtin.rs` + PORTING-STORY for why that dead-ends), and `find -exec`/`xargs` commands run through `Shell::run_argv` on subshell clones via the `builtin_exec` sync→async bridge. The commands `find -exec` / `xargs` *spawn* have their stdout/stderr dup'd from the shell's logical streams (`Dependencies::{child_stdout,child_stderr}` / `XargsIo::{child_stdout,child_stderr}`), so `find … -exec cmd \; > file` and `xargs cmd | downstream` honor the box's redirects and pipes; a standalone build inherits the process fds, as upstream does. |
| brush crates: `brush-core`, `brush-builtins`, `brush-coreutils-builtins`, `brush-parser`, `brush-interactive` | github.com/reubeno/brush, commit `428f477` (PR #1181 — the `OpenFile`-Arc pipeline fd-leak fix), pre-release ahead of crates.io | `428f477` (`brush-core` 0.5.0 / `brush-parser` 0.4.0 / `brush-builtins` 0.2.0 / `brush-coreutils-builtins` 0.1.0 / `brush-interactive` 0.4.0) | `[patch.crates-io]` redirects in `bumba/Cargo.toml` point every `brush-*` name at `vendor/brush-*`, so the whole dep graph resolves to one copy | Three patches over the pristine import: **(1) de-workspace** the crate manifests (inline the `*.workspace = true` metadata, drop `[lints]`, turn `path = "../sibling"` deps into plain versions) so each crate stands alone under `[patch.crates-io]`; **(2) pipeline fd hygiene** in `brush-core` — a `compose_std_command` pre_exec `close_stray_fds` hook (closes CLOEXEC-marked stray fds in the child so pipeline children don't leak a stdin-pipe writer and hang) plus a `spawned_pipeline_stage` flag so the dup2'ing `CoreutilWrapper` stays inert on the concurrent spawn path; **(3) launch-state hooks** in `brush-core` — a `LaunchState` on `ExecutionParameters` and a second pre_exec that materializes nice/setsid/SIGHUP-ignore in the child, for the `nice`/`setsid`/`nohup` exec-wrapper builtins (`bumba/src/exec_wrappers.rs`). |

## Updating a vendored crate to a newer upstream

1. Point `bumba/vendor.toml` at the new version/commit (for crates.io, put the
   new tarball's sha256 — `curl -fsSL https://static.crates.io/crates/<c>/<c>-<v>.crate | sha256sum`).
2. Refresh the selection: diff the old and new upstream trees; add/remove paths
   in `vendor-patches/<crate>/files` for files upstream added or dropped.
3. `make vendor`. `git apply` fails loudly on any patch upstream has drifted
   under; re-spin that patch against the new base (apply the earlier patches,
   make the change by hand, `git diff --no-prefix`-style regenerate — the patch
   files are plain `git format-patch` output, so editing the hunks directly is
   also fine). Do NOT fold patches together while re-spinning.
4. Verify (next section).

## Verifying after an update (the regression net)

**1. Vendored lib compiles clean** (host target — fast, skips the musl zigshim):

```bash
cd bumba/vendor/findutils
CARGO_TARGET_DIR=/tmp/fu cargo build --lib --target x86_64-unknown-linux-gnu   # 0 warnings
```

**2. Upstream unit suite passes against the patch.** The in-source tests need
`test_data/`, which we don't vendor — overlay the patched module onto a clean
upstream checkout and run there:

```bash
cd /tmp && rm -rf fu-test && git clone --depth 1 --branch 0.10.0 https://github.com/uutils/findutils fu-test
cp -r ~/sarun/bumba/vendor/findutils/src/find/. fu-test/src/find/
cp -r ~/sarun/bumba/vendor/findutils/src/xargs/. fu-test/src/xargs/
printf 'pub mod find;\npub mod xargs;\n' > fu-test/src/lib.rs
cd fu-test && CARGO_TARGET_DIR=/tmp/fu-test cargo test --lib find:: xargs::
#   All green EXCEPT test_no_permission_file_error and get_or_create_file_test
#   when run as root (root bypasses chmod 000) — confirm they fail on pristine too.
```

**3. Standalone Bumba build and integration suite:**

```bash
make -C bumba test
```

This covers pipelines of injected-I/O utilities, `find`/`xargs`, launch-state
wrappers, and non-trivial C projects built by both Make and Ninja.

**4. Linux and syscall-level behavior:**

```bash
orb -m ubuntu sh -lc 'cd /mnt/mac/Users/USER/PATH/TO/bumba && cargo test --locked'
strace -f -e trace=execve bumba -c 'printf "b\na\n" | sort | uniq | wc -l'
```

The builtin-only trace should contain no child `execve`. Build traces may
contain compilers and commands explicitly named by recipes, but not `/bin/sh`,
external Make/Ninja, or utilities supplied by Bumba.

**5. (optional) Independent blind review** — hand `git diff <base>..HEAD --
bumba/vendor/<crate>` plus the glue file to a fresh agent with no hints: "what
does it do, does it work, is it safe." That is how `uu_cat` and `findutils` were
signed off.

## Conventions (the tooling above depends on these)

* One patch per logical change, numbered `NNNN-<slug>.patch`, ordered by
  `series`. **Never squash** patches together while re-spinning.
* The assembled base (selection, before patches) **is** upstream byte-for-byte.
  File-selection changes are made in `files`, never by editing fetched source.
* Patch files are `git format-patch` output (subject + body preserved) so the
  why of every delta travels with the diff.
