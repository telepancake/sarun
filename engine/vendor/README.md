# Vendored, patched upstreams (`uu_cat`, `findutils`, …)

Each crate is vendored as:

* one **pristine-import commit** — upstream source verbatim at a pinned release
  (the rebase base), and
* a **patch series** on top — never squashed.

To update: replace the pristine base with a newer upstream drop and `git rebase`
replays the patch series; conflicts surface only where upstream changed the same
lines. The base must be byte-identical to upstream — do not tidy it.

> The brush crates (`brush-core`, `brush-builtins`, …) follow the same
> pristine-import + rebaseable-patch-series discipline. They are not run as
> in-process builtins; brush *is* the shell. The update procedure below applies
> unchanged.

## What is vendored, and how it is consumed

| crate | upstream | pinned at | wired into the engine via | what we changed |
|-------|----------|-----------|---------------------------|-----------------|
| `uu_cat` | crates.io `uu_cat` (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uu_cat = { path = "vendor/uu_cat" }` in `engine/Cargo.toml` | Injected-I/O entry `cat::cat(out, out_fd, stdin, stdin_fd)` that writes the shell's logical `OpenFile` sink/source with no process-global stdio, keeping the Linux `splice(2)` fast path. A thin `uumain` bridge is retained for the standalone/`--invoke-bundled` path. |
| `uu_head` `uu_tail` `uu_wc` `uu_nl` `uu_tac` `uu_basename` `uu_dirname` `uu_seq` `uu_expr` `uu_tr` `uu_cut` `uu_uniq` `uu_sort` `uu_uname` `uu_nproc` `uu_id` `uu_whoami` | crates.io (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uu_<name> = { path = "vendor/uu_<name>" }`; a native `SimpleCommand` per util in `engine/src/brush.rs` (`<Name>Builtin`, registered in `box_builtins_opt`), each run on a fresh thread via `run_coreutil_localized` | Same model as `uu_cat`: a logical injected-I/O entry that writes the shell's logical `OpenFile` sink/source/stderr with **no process-global stdio and no `dup2`** (pipeline-safe), keeping fast paths byte-for-byte. They run IN-PROCESS **unconditionally** — there is no per-argv gate and no fork-to-the-box's-binary fallback (an earlier gate scheme that detected divergent argvs and forked an *unknown* external tool — busybox in an Alpine box — was scoured out; it was itself a divergence and masked real bugs). uucore's `show!`/`set_exit_code`/`process::exit` are removed from the live path (diagnostics accumulated+returned or written to the logical `err`; exit codes returned, e.g. `expr` 0/1/2). Where uutils genuinely diverged from POSIX/GNU the **fork is patched** to match — `uu_expr` rejects a leading-`+` integer and clamps out-of-range `substr` (see its patch commit); other differences are uutils' own (the box's coreutils *is* uutils). Each runs on a fresh thread (`run_coreutil_localized`) so uucore's thread-local localization gives it its own message bundle. `uumain` retained as a thin bridge. The read-only info utils (`uname`/`nproc`/`id`/`whoami`) share the same model but a no-stdin, no-cwd `<name>_main(args, out, err)` entry (built by `info_builtin!`) and report the box's real sysinfo/identity. Syscall-level contract (`engine/test_builtin_contract.py`, `make test-contract`) asserts in-process execution, logical-I/O, and multi-util localization. |
| filesystem ops: `uu_cp` `uu_mkdir` `uu_rmdir` `uu_rm` `uu_mv` `uu_ln` `uu_touch` `uu_readlink` `uu_realpath` `uu_mktemp` `uu_tee` `uu_chmod` `uu_chown` `uu_install` | crates.io (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uu_<name> = { path = "vendor/uu_<name>" }`; a `SimpleCommand` per util in `engine/src/brush.rs` (built by the `fs_builtin!` / `fs_builtin_stdin!` macros), each run on a fresh thread via `run_coreutil_localized`, registered UNCONDITIONALLY (not under the `bundle_coreutils` gate) | Same injected-I/O model as the stream/filter group, PLUS a **logical-cwd** rewrite: each crate gains a `<name>_main(args, cwd, out, err[, stdin])` entry that resolves every relative path operand (and a `-t`/`--target-directory` value) against the shell's logical cwd — the process is never `chdir`'d. Verbose/debug output, diagnostics, and (for `rm`/`mv`/`ln`) the interactive `-i` prompt all route through the shell's logical out/err via thread-local buffers + crate-local `show!`/`show_error!`/`show_warning!`/`prompt_yes!`/`println!` macros that SHADOW uucore's process-global ones; `set_exit_code` is shimmed to a thread-local. The `-i` prompt answer is read from the shell's **logical stdin**, NEVER the engine's fd 0 (a stray read corrupts a control channel/pipe; EOF ⇒ "no"). Diagnostics stay byte-faithful to GNU except that a resolved relative operand displays its cwd-absolute path (same property as the original `cp` port). `readlink`/`realpath`/`mktemp` (path-resolving) and `chmod`/`chown`/`install` (metadata ops) share the no-stdin `<name>_main(args, cwd, out, err)` shape built by `fs_builtin!` (`chown` routes through the patched `uucore::perms::chown_base_io`). `tee` adds the logical-stdin read (`tee_main(args, cwd, out, err, inp)`, hand-written `TeeBuiltin`): it copies stdin to the logical stdout AND its file operands, never reading the engine's fd 0. `uumain` retained as a thin bridge (real process stdio). |
| `uucore` | crates.io `uucore` (uutils/coreutils) | `0.8.0` | `[patch.crates-io] uucore = { path = "vendor/uucore" }` — redirects every vendored `uu_*` builtin crate + `brush-coreutils-builtins` to this one copy | The shared uutils runtime, patched so MULTIPLE distinct utils can run correctly in ONE process — something uutils (one util per process) never anticipated, and which the in-process builtins require. **(1)** The process-global localization caches (`UUCORE_FLUENT`/`CHECKSUM_FLUENT`/`UTIL_FLUENT` `OnceLock`s in `locale.rs`) are made **thread-local**, so each builtin's thread parses/looks up its own Fluent bundle — no first-util-wins cross-contamination (which had printed raw keys like `tac-error-open-error` for every util after the first). **(2)** `util_name()` (`lib.rs`) gains a thread-local override (`set_utility_name`, called from `brush-coreutils-builtins::prepare_uutil_runtime`) so a util's diagnostics carry its real name (`wc:`) instead of the engine's argv[0] (`sarun:`). **(3)** `build.rs` embeds sibling `uu_<util>` crates' locales (sarun's vendor forks are named plainly `uu_<util>`, not the registry's `uu_<util>-<version>`), without which a vendored uucore embeds zero per-util `.ftl` and every util prints raw keys; it also emits a `rerun-if-changed` on the vendor parent dir so adding a new sibling `uu_*` crate re-runs the locale embedding (otherwise a newly vendored util prints raw keys against a cached build). uucore's own locale unit suite (incl. `test_thread_local_isolation`) passes against the patch. |
| `findutils` | github.com/uutils/findutils, tag `0.9.1` | `0.9.1` | `findutils = { path = "vendor/findutils" }` in `engine/Cargo.toml`; builtins in `engine/src/find_builtin.rs` and `engine/src/xargs_builtin.rs`, registered in `engine/src/brush.rs` (`box_builtins_opt`) | Reduced to a **find + xargs** library (`lib.rs` = `pub mod find; pub mod xargs;`; the `locate`/`updatedb`/`testing` modules + bins removed). **find:** added `Dependencies::{get_error_output, get_input}` so diagnostics and the `-files0-from -` read go through the shell's logical stderr/stdin. **xargs:** added an `XargsIo` trait (`take_input`/`output`/`error_output`) so item input and xargs's own output/`-t`/warnings/errors go through the logical streams; `xargs_main_with_io` is the embedder entry. Both builtins run their `_main` on a worker thread that `unshare(CLONE_FS)`s + `chdir`s to the shell's logical cwd (see the builtin files for that rationale). The commands `find -exec` / `xargs` *spawn* have their stdout/stderr dup'd from the shell's logical streams (`Dependencies::{child_stdout,child_stderr}` / `XargsIo::{child_stdout,child_stderr}`), so `find … -exec cmd \; > file` and `xargs cmd | downstream` honor the box's redirects and pipes; a standalone build inherits the process fds, as upstream does. |
| brush crates: `brush-core`, `brush-builtins`, `brush-coreutils-builtins`, `brush-parser`, `brush-interactive` | github.com/reubeno/brush, commit `428f477` (PR #1181 — the `OpenFile`-Arc pipeline fd-leak fix), pre-release ahead of crates.io | `428f477` (`brush-core` 0.5.0 / `brush-parser` 0.4.0 / `brush-builtins` 0.2.0 / `brush-coreutils-builtins` 0.1.0 / `brush-interactive` 0.4.0) | `[patch.crates-io]` redirects in `engine/Cargo.toml` point every `brush-*` name at `vendor/brush-*`, so the whole dep graph resolves to one copy | Three patches over the pristine import: **(1) de-workspace** the crate manifests (inline the `*.workspace = true` metadata, drop `[lints]`, turn `path = "../sibling"` deps into plain versions) so each crate stands alone under `[patch.crates-io]`; **(2) pipeline fd hygiene** in `brush-core` — a `compose_std_command` pre_exec `close_stray_fds` hook (closes CLOEXEC-marked stray fds in the child so pipeline children don't leak a stdin-pipe writer and hang) plus a `spawned_pipeline_stage` flag so the dup2'ing `CoreutilWrapper` stays inert on the concurrent spawn path; **(3) launch-state hooks** in `brush-core` — a `LaunchState` on `ExecutionParameters` and a second pre_exec that materializes nice/setsid/SIGHUP-ignore in the child, for the `nice`/`setsid`/`nohup` exec-wrapper builtins (`engine/src/exec_wrappers.rs`). |

Provenance is in each crate's **pristine-import commit message** — find it with:

```bash
git log --oneline --grep '^vendor: import pristine'
```

Do **not** hard-code commit hashes: rebasing rewrites them. Re-derive the base
from that commit message convention.

## Updating a vendored crate to a newer upstream

Worked example: bumping `findutils` `0.9.1` → `0.10.0`. For `uu_cat`: fetch the
new crate from crates.io instead of git; skip the find/xargs trim notes. For the
**brush** crates: clone github.com/reubeno/brush at the new commit, refresh all
five `vendor/brush-*` directories in one pristine-import commit (`git log --grep
'^vendor: import pristine brush'` finds the base), then rebase replays the
de-workspace / fd-hygiene / launch-state patches; re-check `engine/Cargo.toml`'s
`[patch.crates-io]` versions still match.

```bash
cd ~/sarun

# 0. Rebase base by message — NOT a remembered hash.
BASE=$(git log --format=%H --grep '^vendor: import pristine findutils' -1)

# 1. Fetch new upstream.
cd /tmp && rm -rf fu-new
git clone --depth 1 --branch 0.10.0 https://github.com/uutils/findutils fu-new

# 2. Rebuild pristine BASE in place (same file selection: src/ Cargo.toml LICENSE README).
#    dev-only trees (tests/, test_data/, benches/, util/) are NOT vendored.
cd ~/sarun
git switch -c vendor-bump "$BASE"
rm -rf engine/vendor/findutils/{src,Cargo.toml,LICENSE,README.md}
cp -r /tmp/fu-new/src engine/vendor/findutils/src
cp /tmp/fu-new/{Cargo.toml,LICENSE,README.md} engine/vendor/findutils/
git add engine/vendor/findutils
git commit --amend -m "vendor: import pristine findutils 0.10.0 (find)"

# 3. Replay patch series.
git rebase --onto vendor-bump "$BASE" <work-branch>
#    Conflicts only where 0.10.0 touched the same lines our patches did.
#    The find+xargs trim re-applies by deleting locate/updatedb/testing.

# 4. Verify (see next section). Then:
git push --force-with-lease       # rewrites SHAs after the base — expected
git branch -D vendor-bump
```

## Verifying after an update (the regression net)

**1. Vendored lib compiles clean** (host target — fast, skips the musl zigshim):

```bash
cd engine/vendor/findutils
CARGO_TARGET_DIR=/tmp/fu cargo build --lib --target x86_64-unknown-linux-gnu   # 0 warnings
```

**2. Upstream unit suite passes against the patch.** The in-source tests need
`test_data/`, which we don't vendor — overlay the patched module onto a clean
upstream checkout and run there:

```bash
cd /tmp && rm -rf fu-test && git clone --depth 1 --branch 0.10.0 https://github.com/uutils/findutils fu-test
cp -r ~/sarun/engine/vendor/findutils/src/find/. fu-test/src/find/
cp -r ~/sarun/engine/vendor/findutils/src/xargs/. fu-test/src/xargs/
printf 'pub mod find;\npub mod xargs;\n' > fu-test/src/lib.rs
cd fu-test && CARGO_TARGET_DIR=/tmp/fu-test cargo test --lib find:: xargs::
#   All green EXCEPT test_no_permission_file_error and get_or_create_file_test
#   when run as root (root bypasses chmod 000) — confirm they fail on pristine too.
```

**3. Static musl engine builds:**

```bash
cd ~/sarun && make engine        # → engine/target/x86_64-unknown-linux-musl/release/sarun
```

**4. Behavior in a real box brush shell** (`brush-sh -- <argv>` runs in-process; no bwrap needed):

```bash
BIN=engine/target/x86_64-unknown-linux-musl/release/sarun
$BIN brush-sh -- sh -c 'cd engine && find . -maxdepth 1 -name Cargo.toml'        # logical cwd
$BIN brush-sh -- sh -c 'find /no/such 2>&1 | sed s/^/ERR:/'                       # logical stderr
$BIN brush-sh -- sh -c 'cd engine && printf "Cargo.toml\0" | find -files0-from - -maxdepth 0'  # logical stdin
```

**5. (optional) Independent blind review** — hand `git diff <base>..HEAD --
engine/vendor/<crate>` plus the glue file to a fresh agent with no hints: "what
does it do, does it work, is it safe." That is how `uu_cat` and `findutils` were
signed off.

## Conventions (the tooling above depends on these)

* Pristine-import commit message **must** start with
  `vendor: import pristine <crate> <version>`.
* Exactly one pristine-import commit per crate; everything after it is a patch.
  **Never squash** the base into patches or patches together.
* The base tree **is** upstream byte-for-byte. File-selection changes go in a
  *patch* commit, not the base.
