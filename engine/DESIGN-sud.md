# SUD execution transport

SUD is one execution transport for the shared sarun filesystem. It is not a
second overlay implementation and has no selectable filesystem modes.

## Boundary

The production wrapper contains exactly two fixed adapters:

1. the compact binary trace/provenance stream; and
2. the SarunFs syscall adapter.

The syscall adapter translates intercepted Linux syscalls into canonical FUSE
requests. A sealed shared memfd at fd 1021 contains independently reclaimable
request/reply slots; futexes wake producers and the engine worker. The worker
passes each request to the same virtiofsd decoder and scoped `SarunFs` used by
the FUSE and QEMU transports. Path resolution, overlay precedence, copy-up,
whiteouts, capture, rules, attachments, and synthetic nodes therefore exist
only in `SarunFs`.

The transport may maintain process-local state needed to preserve Linux file
descriptor and cwd semantics. It must not make filesystem policy decisions.

## Descriptor export

Some Linux operations need a kernel file descriptor rather than a byte reply:
ELF loading, file-backed mmap, and `execveat(AT_EMPTY_PATH)`. For those cases a
Unix `SOCK_SEQPACKET` lane at fd 1022 transfers an fd with `SCM_RIGHTS`. The
request names an already-open canonical SarunFs handle; no path or policy is
resolved on the fd lane. Writable shared mappings request a writable export so
copy-up and capture happen before the fd is returned.

The lane is serialized across forked tracees, reclaims a lock owned by a dead
thread, and closes with the box. It is exceptional data movement, not another
filesystem interface.

## Namespace boundary

`/proc`, `/dev`, and `/sys` are process-local pseudo-filesystems. Their paths,
cwd values, dirfds, and local `/proc/self/fd/N` links stay in the traced process
namespace. A `/proc/self/fd/N` referring to a virtual SarunFs descriptor is
recognized by the adapter and resolves to its canonical handle/path instead.

All other filesystem syscalls use the ring. Virtual open-file descriptions are
shared across dup/fork, preserving offsets and lock ownership. Logical cwd is
carried in wrapper argv across exec so relative resolution remains stable
without environment-variable state.

## Lifecycle

The runner creates the trace channel, filesystem ring, and fd lane and passes
them as ordered registration descriptors. The engine owns the SarunFs worker
and fd exporter. The compact trace is applied live; EOF finalizes provenance
and capture. There is no upper-directory sweep or post-exit in-memory
filesystem import.

The standalone SUD launcher, inramfs, path-remap overlay, fake-exec,
command-rewrite, and selectable add-in matrix were displaced by this boundary
and have been deleted. New SUD filesystem semantics belong in `SarunFs`, where
all three transports receive them.

## Validation constraint

Both i386 and x86_64 wrappers and their ring/fd-lane fixtures are built and run
on the current aarch64 host. A live x86 SUD process cannot run through the
host's qemu-user/binfmt path because that emulator rejects
`PR_SET_SYSCALL_USER_DISPATCH`; live parity therefore additionally requires a
native x86 Linux host. This is an emulator limitation, not a retained runtime
fallback.
