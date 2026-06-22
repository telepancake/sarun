# Porting Unix utilities to in-process brush builtins

How `cat`, `find`, and `xargs` became in-process brush builtins, and the recipe
for porting the next one.

## The premise

`sarun` runs a box's commands through an embedded shell (brush-core) in one
process — no `fork`+`exec` per command. A normal Unix program owns its process:
it writes fd 1, reads fd 0, and `chdir`s whenever it likes. Run that same
program in-process beside other commands and each of those assumptions corrupts
a neighbor. The shell keeps a *logical* view — its own stdout, its own cwd that
moves on `cd` without the process ever calling `chdir`, its own environment —
and a builtin must honor that, not the process globals.

So porting a util is not a recompile and not a reimplement: take the upstream
implementation and make it speak the shell's logical world instead of the
process globals, without changing a single behavior. The upstream stays
byte-for-byte pristine underneath as a clean rebase base (`engine/vendor/
README.md`); the changes are a never-squashed patch series on top, so a new
upstream release is a `git rebase`.

## The seams

**Logical I/O.** Every diagnostic and data write must go to the shell's logical
sinks, never fd 1/2. `cat` takes `(out, out_fd, stdin, stdin_fd)`; `find` reuses
uutils' existing `Dependencies` trait and reroutes ~21 `stderr()` sites across
twelve matcher files. stdin must come from the shell's logical input, never
`io::stdin()` — an in-process builtin reading the engine's fd 0 *consumes* bytes
from whatever owns it (a control channel, a parent pipe), which is data
corruption (`find -files0-from -` hit this; fixed with a `get_input()` seam).

**Keep the fast paths.** `cat` keeps its `splice(2)` path, gated on a real fd at
both ends, flushing the writer before splicing so a line-buffered logical stdout
isn't leapfrogged by the kernel copy. A large minus-sign diff usually means
working code was deleted to dodge the real problem.

**Logical cwd.** `find .` walks from the kernel cwd pervasively — the walk,
every `stat`, each `-exec` child — through `std::fs`, with no single redirect
point. Do *not* give the thread its own kernel cwd (`unshare(CLONE_FS)` +
`chdir`): `CLONE_FS` only redirects cwd and nothing else logical (env, fds,
`/proc/self`), so it dead-ends the moment you need the next piece of logical
state. Instead resolve against the logical cwd directly: a `Dependencies::cwd()`
hook hands `find` the shell's working dir; a relative start path is rooted at the
absolute `cwd.join(start)` so the walk never touches the process cwd, while
`WalkEntry::path()` stays the real absolute path (so `fs` ops and cached metadata
are correct) and a separate `display_path()` is the relative form used only by
display/match sites (`-print`, `-path`, `-regex`, `-printf %p/%h`, `-exec {}`).

## Running sub-commands through brush

`xargs` and `find -exec` run other commands. The pristine code spawns each via
`std::process::Command` — a real `fork`+`exec` of a binary. In a box that misses
shell builtins, functions, and anything else the shell resolves: `echo foo |
xargs type` answered "command not found", and `xargs cat` exec'd `/usr/bin/cat`
instead of the in-process `cat` builtin. The command must go through brush.

The primitive is `Shell::run_argv` (`vendor/brush-core/src/interp.rs`): it feeds
an already-split argv to `execute_command` as literal words — full
function→builtin→external dispatch, no re-expansion (no word splitting, globbing,
alias, or quote processing). The no-re-expansion part is load-bearing: routing
an argv back through a shell *string* would re-tokenize it, and `find -print0 |
xargs -0 rm` is safe precisely because no shell re-touches the filenames.
`env`/`nice`/`setsid`/`nohup` call `run_argv` directly.

`find`/`xargs` live inside a *synchronous* findutils engine on a `Shell` that
isn't `Send`, so they can't `.await` or move to a blocking thread. The bridge
(`engine/src/builtin_exec.rs`): findutils runs on a worker thread and *submits*
each built argv over a channel; an async executor on the builtin's own task runs
each via `run_argv` on a subshell clone (per-command env/cwd don't leak) and
replies with the exit code. The builtin is an async `builtins::Command` (like
`env`), which is what lets it drive the executor. The findutils side is behind a
hook (`XargsIo::submit`, `Dependencies::run`) that falls back to
`std::process::Command` for the standalone binary, so pristine behavior is
untouched.

`xargs -P` parallelism is executor concurrency (`FuturesUnordered` on the single
task), and it matches brush's own `&`/`wait` semantics: external commands
parallelize, in-process builtins serialize on the shell's task. `find -exec` is
serial (find dispatches one at a time); `-execdir` passes the entry's parent dir
as the per-command cwd; `-exec +` accumulates paths and flushes one command per
batch (no `execve` arg limit applies in-process).

## The recipe

1. **Vendor pristine, patch on top.** Import upstream verbatim at a pinned
   release (one commit), then a never-squashed patch series. See
   `engine/vendor/README.md`.
2. **Adapt, don't reimplement.** Keep every behavior and every fast path.
3. **Replace process globals with logical seams:** stdout/stderr → the shell's
   logical sinks (never fd 1/2); stdin → the shell's logical input (never
   `io::stdin()`); cwd → a logical-cwd hook (never `unshare(CLONE_FS)`/`chdir`).
   For a path-walker, root the walk at an absolute logical path but keep a
   relative display path for output/matching.
4. **Run sub-commands through `Shell::run_argv`** — never a shell string (it
   re-tokenizes and breaks argv-safety), never `std::process::Command` (it
   bypasses builtins/functions/snooping). Keep the spawn path behind a
   default-off hook for the standalone binary.
5. **Match the builtin shape to the work:** a pure launcher (`env`/`nice`) is an
   async `builtins::Command` that clones the shell and `await`s `run_argv`; a
   wrapper around a synchronous engine (`find`/`xargs`) runs the engine on a
   worker thread and drives the `builtin_exec` executor.
6. **Verify by running.** Build it, run real commands, observe (e.g. `cat` under
   `strace` to watch `splice` fire). Don't claim it works untested.

## Where to see the diffs

```bash
# The run-through-brush primitive (no re-expansion of the argv):
git -C engine show HEAD -- vendor/brush-core/src/interp.rs   # Shell::run_argv

# The sync->async execution bridge shared by find + xargs:
$EDITOR engine/src/builtin_exec.rs

# xargs — logical stdin + commands routed through brush + -P:
$EDITOR engine/vendor/findutils/src/xargs/mod.rs engine/src/xargs_builtin.rs

# find — the logical I/O + logical-cwd seams + -exec routed through brush:
git diff <findutils-pristine-base>..HEAD -- engine/vendor/findutils/src/find
$EDITOR engine/src/find_builtin.rs
#   base = git log --format=%H --grep '^vendor: import pristine findutils' -1

# env / nice / setsid / nohup — the launcher front-ends that use run_argv:
$EDITOR engine/src/exec_wrappers.rs

# cat — the injected-I/O entry that keeps splice:
git diff <uu_cat-pristine-base>..HEAD -- engine/vendor/uu_cat/src
#   base = git log --format=%H --grep '^vendor: import pristine uu_cat' -1

# the whole arc, base -> patches, never squashed:
git log --oneline --grep '^vendor: import pristine' --all
```
