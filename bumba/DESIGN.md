# Bumba boundary

## Contract

Bumba owns shell parsing/execution, its optimized builtin registry (selected
uutils, `find`/`xargs`, and launch-state wrappers), Make and Ninja graph
execution, and recipe execution through a fresh logical Brush subshell.
The standalone binary owns a real command-line and terminal lifecycle: a TTY
with no script opens an interactive Reedline/Brush session, redirected stdin is
executed as a script, and help/version requests do not enter the interpreter.
POSIX-shell recipes handled by Bumba must not invoke `/bin/sh`; an explicitly
configured non-POSIX `SHELL` is delegated to rkati's external-shell path.
`sh`/`dash`/`bash` children and Bumba-owned utilities are interposed by
executable basename even when invoked through a discovered or absolute SDK
path. Discovery itself still reports the real executable, preserving configure
and feature-probe semantics. Commands not owned by Bumba remain ordinary child
processes.

The standalone runtime uses the native filesystem. An embedder can provide a
scoped read-only Kati filesystem capability, logical cwd/environment and I/O,
and a process-wide structured event sink. Events are observations, never build
state authority. Failure or absence of an event consumer does not alter build
execution.

## Current assumptions and limits

- Unix is required; Linux is the release platform exercised by the integration
  suite.
- Brush, rkati, and n2 currently expose several once-per-process hooks. Bumba
  therefore supports one runtime configuration per process while allowing
  overlapping invocations to keep cwd, environment, streams, and Kati read
  providers scoped.
- Compatibility is bounded by committed fixture builds and the retained rkati
  corpus. Bumba does not claim complete GNU Make, Bash, or Ninja equivalence.
- The host owns cancellation and any machine-wide jobserver. Bumba consumes an
  inherited GNU jobserver or an explicitly supplied endpoint. A standalone
  top-level `-jN` build owns a private, invocation-scoped FIFO jobserver that
  recursive Make and Ninja invocations inherit; it is removed when that build
  ends and is never installed in process-global environment state.

## Dependency direction

`bumba -> Brush/rkati/n2/uutils`

`Sarun -> bumba`

Bumba must never depend on Sarun. Sarun-specific box transport, provenance
storage/wire formats, UI, editor, and engine commands remain adapters in Sarun.

The repository's semantic grammar/relation engine is a different boundary. It
should become a neutral crate used by Bumba, Chupa, and Sarun, with each product
supplying its own catalog and context providers. Bumba continues to own the
Brush syntax and execution representation required to run shell programs, but
must not absorb Sarun's Prolog runtime, generated action catalog, or context
protocol merely to provide interactive analysis or completion.
