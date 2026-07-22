# Direct filesystem access for embedded tools

Sarun's FUSE + Brush mode has two ways to reach the same `SarunFs` instance:

1. external programs and descriptor-oriented builtins use the mounted kernel
   filesystem normally;
2. Brush's path-oriented reads and embedded Kati reads use a typed, read-only
   client connected directly to the canonical raw-FUSE decoder.

The second route prevents a Rust builtin from accidentally observing the
engine process's host filesystem merely because it used `std::fs` or libc
instead of issuing a syscall from the box. Both routes still produce the same
filesystem observations and provenance because both terminate in `SarunFs`.
There is no command-line switch, environment variable, installation-path
search, or host-path fallback for this feature.

## Automatic lifecycle

The runner creates a bounded shared-memory request ring and a private Unix
sequenced-packet lane whenever a FUSE run selects embedded Brush. It passes the
descriptors through the existing registration message with explicit roles and
installs them at fixed private descriptors only for the trusted `inner`
process. Other execution modes do not receive them.

Before it sends FUSE `INIT`, `inner` performs a synchronous identity exchange:

- it opens a pidfd for itself and sends it over the private lane;
- the engine requires both `SCM_CREDENTIALS` and exactly one pidfd, verifies
  that both identify the same host process, and publishes that identity once;
- the engine acknowledges only after publication;
- `inner` maps the ring, then closes the lane and the ring's backing memfd.

The memory mapping remains usable, but embedded code cannot discover or pass
the transport descriptors through `/proc/self/fd`. Every direct FUSE request
has its PID replaced by the immutable verified caller identity before normal
decode and attribution. Session teardown shuts down the ring, wakes waiters,
joins its workers, and removes the box export.

## Brush boundary

`brush_core::vfs::BoxVfs` is a clone-shared, typed interface for observations:
metadata with and without final-symlink following, directory enumeration,
readlink, whole-file reads, and access checks. `NativeBoxVfs` is the default.
The FUSE runner injects `DirectFsClient` into the shell builder before profile
or rc loading, so startup files, sourced scripts, pathname expansion,
completion, basic file predicates, cwd checks, and PATH lookup all observe the
box.

The client implements Linux path-walk rules itself over `LOOKUP`, including
explicit `.` and `..`, trailing slashes, relative and absolute symlinks, the
40-link limit, non-directory traversal rejection, and search permission on
every directory. Handles for `OPEN` and `OPENDIR` never leave an operation and
are always released before lookup references are forgotten. Reads use a
bounded initial allocation and continue until an empty reply rather than
trusting a possibly stale size.

Redirections, external execution, and descriptor-oriented coreutils continue
through the mounted filesystem intentionally. They need open-file-description
semantics and mutation support, not a second partial implementation. A write
made through that route is immediately visible to a following direct read
because both routes share `SarunFs`.

## Kati boundary

Kati's `FileSystemProvider` covers its read/evaluation lifetime: stat/lstat and
timestamps, reads, readlink, directory enumeration, canonicalization, and glob
expansion. Standalone/projected Kati defaults to `NativeFileSystem`. The sole
in-process `MakeBuiltin` caller installs an absolute-cwd adapter backed by the
already adopted `DirectFsClient`; absence there is a visible integration
failure instead of a silent host read.

Provider selection is a nested thread-local scope. Kati explicitly captures
and installs the scope in recipe and regeneration workers. This permits
recursive and concurrent makes without a process-global provider swap or a
lock held while one make waits for another.

Kati's writes, deletes, touches, generated Ninja/stamp output, and recipe
