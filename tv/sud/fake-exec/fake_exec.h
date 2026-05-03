/*
 * sud/fake-exec/fake_exec.h — Public surface of the fake-exec addin.
 *
 * The fake-exec addin elides process execution that has provably no
 * side effects beyond what we can emulate from inside the SIGSYS
 * handler.  When a traced program execs one of the trivial helper
 * binaries (`true`/`false`/`:`/`echo`/`printf`), we replace the
 * execve syscall with a tiny vfork-safe sequence — at most one
 * `write` followed by a per-task `_exit` — instead of letting the
 * kernel run the real binary.
 *
 * A second elision shape is `/bin/sh -c "<single trivial cmd>"`:
 * when the inner command's first token resolves to a builtin and
 * the rest of the argv is shell-metacharacter-free, we recurse the
 * classifier on the inner command and elide the shell+helper pair.
 *
 * For the elidable set this skips:
 *   • the ELF load and dynamic linker run for the helper binary
 *     (and the wrapping shell, in the sh -c case)
 *   • glibc's startup code (a long sequence of mmap/openat/read/...)
 *   • the SUD selector reinitialisation in the new image
 *   • any syscalls the helper would have made before exit
 *
 * Trace fidelity: the matching EXIT event is observed by the trace
 * addin on the parent's wait() return, byte-identical to a real run.
 * Builtins that emit stdout (echo / printf) additionally call the
 * synthetic-write helper sud_trace_emit_synthetic_write() so the
 * STDOUT bytes reach the trace as if the real binary had issued the
 * write through the normal post_syscall hook.
 *
 * vfork safety: every elide path issues only raw syscalls, never
 * touches the heap, never mutates a global.  Safe to fire from inside
 * a child of clone(CLONE_VM|CLONE_VFORK) — the pattern glibc's
 * posix_spawn uses — where allocating, calling libc, or terminating
 * the thread group would corrupt or kill the parent.
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
    FAKE_EXEC_INLINE_VFORK_SAFE, /* emit (optional write +) exit inline */
    FAKE_EXEC_REFUSE,            /* hard blocklist (e.g. setuid bin)   */
};

struct sud_fake_exec_builtin;    /* opaque to callers outside the addin */

struct fake_exec_decision {
    enum fake_exec_track             track;
    const struct sud_fake_exec_builtin *builtin;
    int                              exit_status;   /* 0..127 */
    /* Optional pre-exit STDOUT write for bounded-output builtins.
     *   out_fd  == -1  → no write, just exit
     *   out_fd  >=  0  → addin issues raw write(out_fd, out_buf, out_len)
     *                    plus a synthetic STDOUT trace event before
     *                    SYS_exit.
     * out_buf points into the scratch buffer the caller passed to
     * sud_fake_exec_classify(); the caller must keep that buffer live
     * until the addin has consumed it (which it does inline before
     * returning to the SIGSYS handler context). */
    int                              out_fd;
    const char                      *out_buf;
    int                              out_len;
};

/* Classify an execve attempt.  `path` is the resolved binary path
 * (post-path_remap), `argv` and `envp` mirror the program's syscall
 * args.  `scratch` / `scratch_size` is the buffer used by output-
 * emitting builtins to compose their bytes; pass NULL/0 to refuse
 * any builtin that needs to write (the classifier then routes those
 * to PASSTHROUGH).  On return, *out describes what the addin should
 * do.
 *
 * Pure function modulo writes into `scratch`: no syscalls, no
 * allocation, no global mutation.  Safe to call from a vfork child. */
int sud_fake_exec_classify(const char *path,
                           char *const *argv,
                           char *const *envp,
                           char *scratch, int scratch_size,
                           struct fake_exec_decision *out);

/* Single-command shell-grammar check used by the /bin/sh -c "<cmd>"
 * elision path.  Returns 1 iff `cmd` is a single command with no
 * shell metacharacters, no quoting, and no whitespace runs other
 * than single SP separators between tokens.  On success, tokens are
 * written into argv_out[0..n-1] (pointers into a NUL-terminated copy
 * of `cmd` written into `tok_scratch`); *argc_out gets the count.
 *
 * Bounded-overflow checks are enforced via tok_scratch_size and
 * argv_out_max.  Returns 0 on any rejection. */
int sud_fake_exec_sh_tokenise(const char *cmd,
                              char *tok_scratch, int tok_scratch_size,
                              char **argv_out, int argv_out_max,
                              int *argc_out);

#endif /* SUD_FAKE_EXEC_H */
