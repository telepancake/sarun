/*
 * sud/fake-exec/builtins.h — Internal builtin registry.
 *
 * Each registry entry maps one helper binary (`/usr/bin/true` etc.)
 * to a vfork-safe emulator.  The registry is searched by canonical
 * absolute path; the basename field is used by `--fake-exec-deny`
 * matching.  All emulators in the MVP are pure functions of argv —
 * they emit no I/O and consult no environment variables — so the
 * exit status is the only output.
 *
 * Adding a builtin that needs to write output (e.g. echo) requires
 * either a vfork-safe `run_inline` that issues the write via a raw
 * syscall, or a Track-B `run_rich` that runs in the parent's handler
 * context after the vfork child has exited.  The MVP ships with the
 * pure-status variants only.
 */

#ifndef SUD_FAKE_EXEC_BUILTINS_H
#define SUD_FAKE_EXEC_BUILTINS_H

#include "libc-fs/libc.h"

/* Bit flags on struct sud_fake_exec_builtin::flags. */
#define FAKE_EXEC_VFORK_SAFE   (1u << 0)   /* run_inline is signal-safe */

struct sud_fake_exec_builtin {
    const char *canonical_path;       /* "/usr/bin/true" */
    const char *basename;             /* "true"          */
    unsigned    flags;
    /* Returns the wait status as the unsigned low 8 bits (0..127).
     * Must not allocate, mutate globals, or call libc when
     * FAKE_EXEC_VFORK_SAFE is set. */
    int       (*run_inline)(int argc, char *const *argv);
};

/* Returns the registry array, NULL-terminated.  The registry is a
 * compile-time constant; no init step is needed. */
const struct sud_fake_exec_builtin *const *sud_fake_exec_builtins(void);

/* Convenience: find a builtin by canonical absolute path or by
 * basename (e.g. "/usr/bin/true" or "true").  Returns NULL on miss. */
const struct sud_fake_exec_builtin *
sud_fake_exec_lookup(const char *path);

#endif /* SUD_FAKE_EXEC_BUILTINS_H */
