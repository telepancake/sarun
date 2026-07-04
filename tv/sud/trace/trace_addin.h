/*
 * sud/trace/trace_addin.h — Minimal public surface of the trace addin
 * for cross-addin synthetic-event injection.
 *
 * The trace addin is otherwise self-contained (it exports only the
 * sud_trace_addin descriptor in sud/addin.h).  Two situations need
 * out-of-band event injection:
 *
 *   • fake-exec elides /usr/bin/echo and /usr/bin/printf with a raw
 *     SYS_write that bypasses the SIGSYS handler.  Without help,
 *     the trace would lose the STDOUT event the real binary would
 *     have produced via the post_syscall(SYS_write) hook.
 *     fake-exec's addin calls sud_trace_emit_synthetic_write() just
 *     before the raw write, replicating the post_syscall path.
 *
 *   • [Step C] /bin/sh -c "<trivial cmd>" elision needs a synthetic
 *     EXEC event for the inner program (currently emitted only at
 *     wrapper startup via target_launch).  That helper is added
 *     here when it lands.
 *
 * The header is intentionally tiny so addins outside trace/ depend
 * on the absolute minimum.  Only callable when SUD_ADDIN_TRACE is
 * defined; callers must guard their #include with the same macro. */

#ifndef SUD_TRACE_ADDIN_H
#define SUD_TRACE_ADDIN_H

#include "libc-fs/libc.h"

/* Emit a synthetic STDOUT/STDERR write event into the trace stream,
 * as if the running thread had issued a SYS_write(fd, buf, len) and
 * the trace post_syscall hook had observed it.  Matches the wire
 * shape produced by emit_write_event() — same EV type, same delta
 * encoding state — so downstream consumers cannot distinguish a
 * synthetic write from a real one beyond the wall-clock timestamp.
 *
 * Vfork safety: relies only on raw syscalls and the process-local
 * trace encoder state.  In a vfork child the parent is suspended,
 * so advancing the encoder there is safe — when the parent resumes,
 * its next emit will encode a delta from whatever state the child
 * left, and the decoder reconstructs the same sequence either way.
 *
 * fd 1 is treated as STDOUT, fd 2 as STDERR; any other fd is
 * silently dropped (matches the trace post_syscall(SYS_write)
 * classifier, which only records writes to fd 1/2).  Calls with
 * len == 0 are no-ops. */
void sud_trace_emit_synthetic_write(pid_t tid, int fd,
                                    const void *buf, size_t len);

#endif /* SUD_TRACE_ADDIN_H */
