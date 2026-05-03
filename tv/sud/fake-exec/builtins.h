/*
 * sud/fake-exec/builtins.h — Internal builtin registry.
 *
 * Each registry entry maps one helper binary (`/usr/bin/true` etc.)
 * to a vfork-safe emulator.  The registry is searched by canonical
 * absolute path; the basename field is used by `--fake-exec-deny`
 * matching.
 *
 * Two categories of emulator are supported:
 *
 *   1. Pure-status (true / false / :): exit code is the only output;
 *      `run_inline` returns it, `compose_inline` is NULL.
 *
 *   2. Bounded-stdout (echo / printf without %-conversion): the
 *      builtin emits a single bounded string to stdout before exit.
 *      `compose_inline` writes into a caller-provided scratch buffer
 *      and returns the byte count; the addin then issues one raw
 *      write+exit pair from inside the SIGSYS handler.  Trace
 *      fidelity is preserved by emitting a synthetic STDOUT event via
 *      sud_trace_emit_synthetic_write() before the raw write.
 *
 * Adding a builtin that needs richer semantics (multi-syscall I/O,
 * heap allocation, file lookups) requires Track B — synthesising the
 * vfork's wait result so the emulator can run in the parent's normal
 * handler context.  Track B is not in this header.
 */

#ifndef SUD_FAKE_EXEC_BUILTINS_H
#define SUD_FAKE_EXEC_BUILTINS_H

#include "libc-fs/libc.h"

/* Bit flags on struct sud_fake_exec_builtin::flags. */
#define FAKE_EXEC_VFORK_SAFE        (1u << 0)   /* run_inline is signal-safe */
#define FAKE_EXEC_HAS_INLINE_OUTPUT (1u << 1)   /* compose_inline emits      */

struct sud_fake_exec_builtin {
    const char *canonical_path;       /* "/usr/bin/true" */
    const char *basename;             /* "true"          */
    unsigned    flags;
    /* Returns the wait status as the unsigned low 8 bits (0..127).
     * Must not allocate, mutate globals, or call libc when
     * FAKE_EXEC_VFORK_SAFE is set. */
    int       (*run_inline)(int argc, char *const *argv);
    /* Compose the bytes that the real binary would have written to
     * its stdout, into [scratch, scratch + scratch_size).  Returns
     * the number of bytes composed (>= 0), or -1 to force a
     * passthrough (e.g. a printf with a %-conversion the emulator
     * doesn't handle, or output that won't fit in the scratch).
     *
     * Same vfork-safety constraints as run_inline.  Only consulted
     * when FAKE_EXEC_HAS_INLINE_OUTPUT is set. */
    int       (*compose_inline)(int argc, char *const *argv,
                                char *scratch, int scratch_size);
};

/* Returns the registry array, NULL-terminated.  The registry is a
 * compile-time constant; no init step is needed. */
const struct sud_fake_exec_builtin *const *sud_fake_exec_builtins(void);

/* Convenience: find a builtin by canonical absolute path or by
 * basename (e.g. "/usr/bin/true" or "true").  Returns NULL on miss. */
const struct sud_fake_exec_builtin *
sud_fake_exec_lookup(const char *path);

#endif /* SUD_FAKE_EXEC_BUILTINS_H */
