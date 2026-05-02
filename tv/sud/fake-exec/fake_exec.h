/*
 * sud/fake-exec/fake_exec.h — Public surface of the fake-exec addin.
 *
 * The fake-exec addin elides process execution that has provably no
 * side effects beyond what we can emulate from inside the SIGSYS
 * handler.  When a traced program execs one of the trivial helper
 * binaries (`/usr/bin/true`, `/usr/bin/false`, `:` and friends), we
 * replace the execve syscall with a tiny vfork-safe sequence — at
 * most one `write` followed by a per-task `_exit` — instead of
 * letting the kernel run the real binary.
 *
 * For the MVP this skips:
 *   • the ELF load and dynamic linker run for the helper binary
 *   • glibc's startup code (a long sequence of mmap/openat/read/...)
 *   • the SUD selector reinitialisation in the new image
 *   • any syscalls the helper would have made before exit
 *
 * The trace addin runs first in the dispatch chain, so the EXEC event
 * for the helper is recorded from the program-supplied args before
 * we elide.  The matching EXIT event is recorded by trace's pre_syscall
 * on the SYS_exit we issue.  Trace output is therefore byte-identical
 * (timestamps aside) to what would have been recorded had the helper
 * actually run.
 *
 * vfork safety: the inline emit path issues only raw syscalls, never
 * touches the heap, never mutates a global.  This makes it safe to
 * fire from inside a child of clone(CLONE_VM|CLONE_VFORK) — the
 * pattern glibc's posix_spawn uses — where allocating, calling libc,
 * or terminating the thread group would corrupt or kill the parent.
 */

#ifndef SUD_FAKE_EXEC_H
#define SUD_FAKE_EXEC_H

#include "sud/addin.h"

extern const struct sud_addin sud_fake_exec_addin;

/* ---- Decision shape -------------------------------------------------
 *
 * Exposed for the unit tests in sud/fake-exec/tests/ — production code
 * inside the addin uses the classifier directly. */

enum fake_exec_track {
    FAKE_EXEC_PASSTHROUGH = 0,   /* let the kernel run the real binary */
    FAKE_EXEC_INLINE_VFORK_SAFE, /* emit write+exit inline (Track A)   */
    FAKE_EXEC_REFUSE,            /* hard blocklist (e.g. setuid bin)   */
};

struct sud_fake_exec_builtin;    /* opaque to callers outside the addin */

struct fake_exec_decision {
    enum fake_exec_track             track;
    const struct sud_fake_exec_builtin *builtin;
    int                              exit_status;   /* 0..127            */
};

/* Classify an execve attempt.  `path` is the resolved binary path
 * (post-path_remap), `argv` and `envp` mirror the program's syscall
 * args.  On return, *out describes what the addin should do.
 *
 * Pure function: no side effects, no syscalls.  Safe to call from a
 * vfork child. */
int sud_fake_exec_classify(const char *path,
                           char *const *argv,
                           char *const *envp,
                           struct fake_exec_decision *out);

#endif /* SUD_FAKE_EXEC_H */
