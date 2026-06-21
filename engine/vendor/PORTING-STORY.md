# Teaching Unix workhorses to live inside a shell

*How `cat`, `find`, and `xargs` became in-process brush builtins — the wrong
turns, the corrections, and the recipe that came out the far end.*

## The premise

`sarun` runs a box's commands through an **embedded** shell (brush-core), in one
process, no `fork`+`exec` per command. That's fast and sandboxable — but it has
teeth. A normal Unix program owns the process: it writes file descriptor 1,
reads fd 0, and `chdir`s the process whenever it likes. Run that same program
*in-process*, beside other commands, and every one of those "owns the process"
assumptions becomes a way to corrupt a neighbor. The shell keeps a **logical**
view — its own stdout, its own cwd that moves on `cd` without the process ever
calling `chdir`, its own environment — and a builtin has to honor *that*, not the
process globals.

So "port `cat` to a builtin" isn't a recompile. It's: take a decade-old,
thousands-of-commits, runs-the-GNU-test-suite implementation and make it speak
the shell's *logical* world instead of the process globals — **without** breaking
a single one of its behaviors. Adapt, surgically. Don't you dare reimplement it.
The upstream stays **byte-for-byte pristine** underneath as a clean rebase base
(see `engine/vendor/README.md`); your changes are a never-squashed patch series
on top, so a new upstream release is a `git rebase`, not a redo.

## `cat` — the simplest util that actually does something, and it still humbled me

`cat` reads files and copies them to output. On Linux it has one party trick: a
`splice(2)` fast path that moves bytes kernel-to-kernel without ever touching
userspace. That fast path needs a real **file descriptor** to splice into.

My first instinct was wrong twice over:

1. I **deleted the splice path** — "a builtin's output might be an in-memory
   stream with no fd, so splice can't work." Half-true, and used to justify
   throwing away the single best thing `cat` does. The right answer was to thread
   the output's `Option<BorrowedFd>` through and splice *when there is an fd*,
   falling back only for the rare fd-less stream. I'd measured a `+35/−57` diff
   and quietly felt good about the minus sign. The minus sign was me destroying
   working, faster code.

2. Worse, I kept a `uumain` that hardcoded `borrow_raw(1)` — fd **1** — and
   dressed it up with a story about "standalone" and "external-reentry" callers.
   There are no such callers. *"This is brush builtin. You are making brush
   builtin."* The constant was a fiction I'd invented to avoid taking the
   descriptor from the caller's logical sink (`try_borrow_as_fd()`).

There was a memorable *"How much are you betting?"* when I claimed something
worked that I'd only smoke-tested. I'd have lost. What came out the far side: a
`cat` entry taking `(out, out_fd, stdin, stdin_fd)`, splice gated on a real fd at
**both** ends, flushing the writer before splicing so a line-buffered logical
stdout can't have its bytes leapfrogged by the kernel copy. A reviewer handed the
diff cold built it, ran it under `strace`, watched `splice` fire, called it
sound. Splice intact.

## `find` — the injectable seam, and the cwd lever I got wrong first

`find` looked terrifying — dozens of matcher files, `-exec`, `-printf`,
`-files0-from`. But upstream uutils had left a gift: its output already went
through an injectable `Dependencies` trait (built to fake stdout in *their*
tests). The seam I had to *carve* for `cat` already existed. The grind was
mechanical: ~21 places writing diagnostics straight to `stderr()` rerouted to the
logical error sink across twelve matcher files; one parse-time warning riding
home in a `Config` field to dodge a 58-caller function.

And the bug I tried to wave away. A blind reviewer flagged that `-files0-from -`
still read `std::io::stdin()` — the **engine's** real fd 0. I wrote it up as a
known limitation and *offered to leave it*. *"Are you really offering leaving
known data corruption?"* Because that's what it was: an in-process builtin
**consuming** bytes from whatever owns the engine's fd 0 — a control channel, a
parent pipe — stealing them from their real reader. It got a third logical seam,
`get_input()`, and now reads the shell's logical stdin.

### The cwd: a hack that didn't generalize

The real puzzle was the current directory. `find .` walks from the kernel's cwd,
**pervasively** — the walk, every `stat`, each `-exec` child — all through
`std::fs`, with no single redirect point. My first answer was a **per-thread
kernel cwd**: run `find` on a worker thread that calls `unshare(CLONE_FS)` (an
unprivileged split of the thread's cwd, *not* a namespace) and `chdir`s to the
logical dir. It worked, and for a while the story ended there.

It was the wrong lever. `CLONE_FS` only redirects *cwd* — it does nothing for the
environment, open descriptors, `/proc/self`, or anything else a tool reads from
the process. You can't clone-flag your way to a logical process. The moment you
need the next piece of logical state, the trick is a dead end. The correction was
to stop borrowing the kernel's cwd and make `find` resolve against the **logical
cwd directly**:

- A `Dependencies::cwd()` hook hands `find` the shell's logical working dir.
- For a relative start path, the walk is rooted at the **absolute**
  `cwd.join(start)` — so the traversal never reads or mutates the process cwd —
  while paths are **presented relative** to the start. The split is deliberate:
  `WalkEntry::path()` stays the real absolute path so every `fs` op and cached
  metadata are correct (`-delete`, `-type`, `-lname`, …); a new `display_path()`
  is the relative form used only by the display/match sites (`-print`, `-path`,
  `-regex`, `-printf %p/%h`, `-exec {}`).

No `unshare`, no `chdir`, no thread-cwd race. Pure logical state — the same
shape every other seam takes. *If a trick only solves one axis of "logical", it's
the wrong trick; redirect the tool, not the kernel.*

## `xargs` and `find -exec` — running commands *through the shell*

`xargs` and `find -exec` don't just read and write — they **run other commands**.
The pristine code spawns each via `std::process::Command`: a real `fork`+`exec`
of a binary. In a box that's wrong in a way that's easy to miss because it
*mostly works*: an external binary runs and is snooped fine — but a shell
**builtin**, a shell **function**, or anything resolved by the shell isn't a
binary. `echo foo | xargs type` answered *"command not found"*, because `type`
isn't `/usr/bin/type`; `xargs cat` exec'd `/usr/bin/cat`, not the in-process
`cat` builtin we worked so hard on. The command has to go **through brush**, the
same way `env FOO=bar cmd` runs `cmd`.

### The primitive: `Shell::run_argv`

The exec-wrapper builtins (`env`/`nice`/`setsid`/`nohup`) already did this — they
clone the shell, mutate the clone's launch state, and ran the residual command
through `run_string` with every argv word force-single-quoted to survive a
re-parse. The quoting is a smell. The fix was a proper primitive in brush-core:

```rust
// engine/vendor/brush-core/src/interp.rs
pub async fn run_argv(&mut self, argv: &[String], params: &ExecutionParameters)
    -> Result<ExecutionResult, error::Error>
```

`run_argv` feeds an **already-split** argv to brush's `execute_command` as
literal words — full function → builtin → external dispatch, **no re-expansion**
(no word splitting, globbing, alias, or quote processing). That last part is not
optional: routing an argv back through a *shell string* would re-tokenize it, and
`find -print0 | xargs -0 rm` is safe precisely because no shell ever re-touches
the filenames. `run_argv` preserves the safe argv while gaining builtin/function
dispatch and the box's snooping. `env`/`nice`/… now call it directly and the
quoting is gone.

### The sync→async bridge

`env` is an **async** builtin whose body *is* the dispatch, so it just `.await`s
`run_argv`. `find`/`xargs` are different: their execution lives inside a
**synchronous** findutils engine (the matcher tree, the arg-batching loop) that
can't `.await`, on a `Shell` that isn't `Send` (so you can't shove it onto a
blocking thread either). The bridge (`engine/src/builtin_exec.rs`):

- findutils runs on a worker **thread** and, instead of spawning a process,
  **submits** each built argv over a channel.
- An async **executor** on the builtin's own task receives each argv, runs it via
  `run_argv` on a **subshell clone** (per-command env/cwd don't leak — the
  one-process-per-command model), and replies with the exit code.
- The builtin is an async `builtins::Command` (like `env`), not the sync
  `SimpleCommand` — that's what lets it drive the executor. (A sync builtin can't
  safely `block_on`: it runs inline on the async task for a tail-pipeline command
  and via `spawn_blocking` otherwise, so `block_on` would panic on the
  current-thread runtime.)
- The findutils side is abstracted behind a hook (`XargsIo::submit`,
  `Dependencies::run`) that returns "handled" for the embedder and falls back to
  `std::process::Command` for the **standalone** binary — pristine behavior is
  untouched.

`xargs -P` parallelism falls out for free: the executor runs several submissions
concurrently (`FuturesUnordered` on the single task — no `Send` needed). And it
now matches brush's **own** `(cmd)& … & wait`: external commands parallelize,
in-process builtins serialize on the shell's task. `sleep & sleep & wait` (the
`sleep` *builtin*) is serial in plain brush too; `sh -c "sleep 1" & …` is
parallel. The old `Command`-pool made `xargs -P sleep` parallel only because it
exec'd a binary — *inconsistent with the shell it lives in*. Routing through
brush made it consistent.

`find -exec` is serial (find dispatches one at a time during the walk), so the
thread just blocks on each reply; `-execdir` passes the entry's parent directory
as a per-command cwd; `-exec +` accumulates paths and flushes one command per
batch (no `execve` arg-limit applies in-process, so `argmax` is bypassed on the
shell path).

## The recipe — how to port the next builtin

1. **Vendor pristine, patch on top.** Import the upstream verbatim at a pinned
   release (one commit), then a never-squashed series of patch commits. Upstream
   stays pullable; updates are `git rebase`. See `engine/vendor/README.md`.

2. **Don't reimplement — adapt.** Keep every behavior and every fast path
   (`cat`'s `splice`). A big minus sign in the diff usually means you deleted
   working code to dodge the real problem.

3. **Replace process globals with logical seams.** Carve (or reuse) an injectable
   I/O trait and thread it to *every* site:
   - stdout / stderr → the shell's logical sinks (never fd 1/2);
   - stdin → the shell's logical input (never `io::stdin()` — reading the
     engine's fd 0 *consumes* another reader's bytes: a data-corruption bug, not
     an edge case);
   - cwd → a logical-cwd hook; resolve the tool against it. **Do not** give a
     thread its own kernel cwd (`unshare(CLONE_FS)`/`chdir`) — it only fixes the
     cwd axis and nothing else logical. For a path-walker, root the walk at an
     absolute logical path but keep a relative *display* path for output/matching
     (`find`'s `path()` vs `display_path()`).

4. **Run sub-commands through brush, not `exec`.** If the builtin runs other
   commands (`xargs`'s command, `find -exec`, `env`'s residual), route the
   **pre-split argv** through `Shell::run_argv` — never a shell string (that
   re-tokenizes and breaks the argv-safety the tool relies on), never
   `std::process::Command` (that bypasses builtins/functions/snooping). Keep the
   process-spawning path behind a default-off hook for the standalone binary.

5. **Match the builtin shape to the work.**
   - Pure launcher (`env`/`nice`): an async `builtins::Command` that clones the
     shell, mutates the clone, and `await`s `run_argv`.
   - Wraps a *synchronous* engine that runs commands (`find`/`xargs`): an async
     `builtins::Command` that runs the engine on a worker thread and drives the
     `builtin_exec` executor; the engine submits argvs, the executor runs them on
     subshell clones. Concurrency (`-P`) is executor concurrency, and will mirror
     brush's `&`/`wait` semantics.

6. **Verify like an adversary.** Hand the diff to a fresh reviewer with **no
   answer key** ("here's a patch — what does it do, is it safe, does it work?").
   A leading prompt — *"verify the stdin seam is threaded correctly"* — gets you
   theater. Build it, run real commands, observe. Don't claim it works untested;
   you will lose the bet.

## Where to see the diffs

Branch: **`claude/youthful-turing-aolk1h`**.

```bash
# The run-through-brush primitive (no re-expansion of the argv):
git -C engine show HEAD -- vendor/brush-core/src/interp.rs   # Shell::run_argv

# The sync→async execution bridge shared by find + xargs:
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

# the whole arc, base → patches, never squashed:
git log --oneline --grep '^vendor: import pristine' --all
```

And `engine/vendor/README.md` documents the vendoring model and the exact
update/rebase procedure — so the next person (or the next instance of me) doesn't
reverse-engineer this from commit hashes.
