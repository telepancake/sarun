# Porting Unix utilities to in-process brush builtins

How `cat`, `find`, and `xargs` became in-process brush builtins, and the recipe
for porting the next one.

## The premise

`sarun` runs a box's commands through embedded brush-core in one process. A
normal Unix program owns its process: writes fd 1, reads fd 0, `chdir`s at will.
Run it in-process beside other commands and each assumption corrupts a neighbor.
The shell keeps a *logical* view — its own stdout, cwd (moves on `cd` without
`chdir`), environment — and a builtin must honor that, not the process globals.

Porting is not a recompile and not a reimplement: take the upstream
implementation and make it speak the shell's logical world without changing
behavior. The upstream stays pristine as a rebase base (`engine/vendor/README.md`);
changes are a never-squashed patch series on top, so a new upstream is a `git rebase`.

## The seams

**Logical I/O.** Every write must go to the shell's logical sinks, never fd 1/2.
`cat` takes `(out, out_fd, stdin, stdin_fd)`; `find` reuses the `Dependencies`
trait and reroutes ~21 `stderr()` sites. stdin must come from the logical input,
never `io::stdin()` — reading the engine's fd 0 consumes bytes from a control
channel or parent pipe (`find -files0-from -` hit this; fixed with `get_input()`).

**Keep the fast paths.** `cat` keeps its `splice(2)` path, gated on a real fd at
both ends, flushing before splicing so a line-buffered logical stdout isn't
leapfrogged by the kernel copy. A large minus-sign diff usually means working
code was deleted to dodge the real problem.

**Logical cwd.** `find .` walks from the kernel cwd pervasively through
`std::fs`; no single redirect point exists. Do *not* give the thread its own
kernel cwd (`unshare(CLONE_FS)` + `chdir`): `CLONE_FS` only redirects cwd, not
env/fds/`/proc/self`, so it dead-ends when the next logical piece is needed.
Instead: `Dependencies::cwd()` hands `find` the shell's working dir; a relative
start path is rooted at `cwd.join(start)` (walk never touches process cwd);
`WalkEntry::path()` is the real absolute path (fs ops correct); `display_path()`
is the relative form used only at display/match sites (`-print`, `-path`,
`-regex`, `-printf %p/%h`, `-exec {}`).

## Running sub-commands through brush

The pristine code spawns each command via `std::process::Command` — a real
`fork`+`exec`. In a box that misses builtins/functions: `xargs cat` exec'd
`/usr/bin/cat` instead of the in-process builtin. Commands must go through brush.

The primitive is `Shell::run_argv` (`vendor/brush-core/src/interp.rs`): feeds an
already-split argv to `execute_command` as literal words — full
function→builtin→external dispatch, no re-expansion (no word splitting, globbing,
alias, or quote processing). No re-expansion is load-bearing: `find -print0 |
xargs -0 rm` is safe because no shell re-touches the filenames. `env`/`nice`/
`setsid`/`nohup` call `run_argv` directly.

`find`/`xargs` run a *synchronous* findutils engine on a non-`Send` `Shell`, so
they can't `.await`. Bridge (`engine/src/builtin_exec.rs`): findutils runs on a
worker thread, submits each argv over a channel; an async executor runs each via
`run_argv` on a subshell clone and replies with the exit code. The builtin is an
async `builtins::Command`, which lets it drive the executor. The hook
(`XargsIo::submit`, `Dependencies::run`) falls back to `std::process::Command`
for the standalone binary, leaving pristine behavior untouched.

`xargs -P` parallelism is `FuturesUnordered` concurrency on the single task.
`find -exec` is serial; `-execdir` passes the entry's parent dir as per-command
cwd; `-exec +` accumulates paths and flushes one command per batch.

## The recipe

1. **Vendor pristine, patch on top.** See `engine/vendor/README.md`.
2. **Adapt, don't reimplement.** Keep every behavior and fast path.
3. **Replace process globals with logical seams:** stdout/stderr → logical sinks
   (never fd 1/2); stdin → logical input (never `io::stdin()`); cwd → logical-cwd
   hook (never `unshare(CLONE_FS)`/`chdir`). For a path-walker, root the walk at
   an absolute logical path; keep a relative display path for output/matching.
4. **Run sub-commands through `Shell::run_argv`** — never a shell string (it
   re-tokenizes and breaks argv-safety), never `std::process::Command` (bypasses
   builtins/functions). Keep the spawn path behind a default-off hook for the
   standalone binary.
5. **Match the builtin shape to the work:** a pure launcher (`env`/`nice`) is an
   async `builtins::Command` that clones the shell and `await`s `run_argv`; a
   synchronous-engine wrapper (`find`/`xargs`) runs on a worker thread and drives
   the `builtin_exec` executor.
6. **Verify by running.** Build, run real commands, observe (e.g. `cat` under
   `strace` to watch `splice` fire).

## Where to see the diffs

```bash
# Shell::run_argv (no re-expansion):
git -C engine show HEAD -- vendor/brush-core/src/interp.rs

# sync→async bridge (find + xargs):
$EDITOR engine/src/builtin_exec.rs

# xargs — logical stdin + run_argv + -P:
$EDITOR engine/vendor/findutils/src/xargs/mod.rs engine/src/xargs_builtin.rs

# find — logical I/O + logical-cwd + -exec via run_argv:
git diff <findutils-base>..HEAD -- engine/vendor/findutils/src/find
$EDITOR engine/src/find_builtin.rs
# base = git log --format=%H --grep '^vendor: import pristine findutils' -1

# env/nice/setsid/nohup launcher front-ends:
$EDITOR engine/src/exec_wrappers.rs

# cat — injected-I/O entry keeping splice:
git diff <uu_cat-base>..HEAD -- engine/vendor/uu_cat/src
# base = git log --format=%H --grep '^vendor: import pristine uu_cat' -1

# whole arc, base→patches:
git log --oneline --grep '^vendor: import pristine' --all
```
