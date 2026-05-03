/*
 * sud/fake-exec/addin.c — SUD addin glue for fake-exec.
 *
 * Hooks SYS_execve / SYS_execveat in pre_syscall, runs the
 * classifier, and on a positive decision either:
 *   • issues a per-task SYS_exit with the emulator's status, or
 *   • emits a synthetic STDOUT trace event, issues a raw SYS_write
 *     of the composed bytes, then SYS_exit — for echo/printf-style
 *     bounded-output builtins.
 *
 * The kernel never sees the execve.
 *
 * vfork safety: every kernel call we issue on the elide path is a
 * raw syscall (SYS_write, SYS_exit).  We never allocate, never call
 * libc, never write to a global from inside this TU.  The trace
 * synthetic-write helper performs its emit on the static
 * trace-process buffer; in a vfork child the parent is suspended,
 * so mutating the parent-shared encoder state is safe (it advances
 * deterministically and the parent resumes from a valid state).
 *
 * Trace fidelity: trace runs first in the dispatch chain
 * (sud/addin.c) so the EXEC event for the helper is recorded from
 * the program's original syscall args before path_remap rewrites or
 * fake-exec elides them.  The matching EXIT event is recorded by
 * trace's post_syscall on the parent's wait4/waitid return.  STDOUT
 * bytes from echo/printf are emitted via
 * sud_trace_emit_synthetic_write(), keeping the wire stream
 * byte-identical (timestamps aside) to a real run of the helper.
 */

#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"
#include "sud/fake-exec/fake_exec.h"
#include "sud/fake-exec/builtins.h"

#ifdef SUD_ADDIN_TRACE
#include "sud/trace/trace_addin.h"
#endif

static void fake_exec_init(void)
{
    /* Nothing to do at wrapper startup: the builtin registry is a
     * compile-time constant and the runtime config is parsed by
     * sud/wrapper.c::main before sud_addins_wrapper_init runs. */
}

/* Issue raw write + SYS_exit.  Never returns; using SYS_exit (not
 * SYS_exit_group) is essential when called from a vfork child, where
 * exit_group would terminate the whole thread group of the calling
 * task — which still includes only the child (CLONE_VFORK without
 * CLONE_THREAD), but SYS_exit is the per-task primitive and matches
 * real-helper semantics exactly. */
static void __attribute__((noreturn))
fake_exec_emit_write_exit(int status,
                          int out_fd, const char *buf, int len)
{
    if (out_fd >= 0 && buf && len > 0) {
        long off = 0;
        while (off < len) {
            long n = raw_syscall6(SYS_write, (long)out_fd,
                                  (long)(buf + off),
                                  (long)(len - off), 0, 0, 0);
            if (n <= 0) break;     /* EAGAIN / EPIPE / EBADF: drop */
            off += n;
        }
    }
    raw_syscall6(SYS_exit, (long)(status & 0xff), 0, 0, 0, 0, 0);
    /* Defensive: SYS_exit cannot return on Linux, but the compiler
     * needs an unreachable terminator for noreturn. */
    for (;;) raw_syscall6(SYS_exit, 0, 0, 0, 0, 0, 0);
}

static int handle_execve(struct sud_syscall_ctx *ctx,
                         const char *path,
                         char *const *argv, char *const *envp)
{
    struct fake_exec_decision d;
    if (sud_fake_exec_classify(path, argv, envp,
                               ctx->scratch, (int)ctx->scratch_size,
                               &d) != 0)
        return 0;
    if (d.track != FAKE_EXEC_INLINE_VFORK_SAFE)
        return 0;

#ifdef SUD_ADDIN_TRACE
    /* Emit the synthetic STDOUT trace event BEFORE the raw write so
     * a reader that snapshots mid-handler still sees consistent
     * ordering — the trace stream advances first, then the user-
     * visible bytes hit the real fd.  Mirrors the natural order
     * trace's post_syscall(SYS_write) would produce against a real
     * helper binary. */
    if (d.out_fd >= 0 && d.out_buf && d.out_len > 0)
        sud_trace_emit_synthetic_write(ctx->tid, d.out_fd,
                                       d.out_buf, (size_t)d.out_len);
#endif

    /* Commit point: do NOT return from the addin.  The execve syscall
     * never returns on success in a real run, and our caller likewise
     * expects the calling thread to disappear here. */
    fake_exec_emit_write_exit(d.exit_status, d.out_fd, d.out_buf, d.out_len);
}

static int fake_exec_pre_syscall(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;

#ifdef SYS_execve
    if (nr == SYS_execve) {
        const char *path  = (const char *)ctx->args[0];
        char *const *argv = (char *const *)ctx->args[1];
        char *const *envp = (char *const *)ctx->args[2];
        return handle_execve(ctx, path, argv, envp);
    }
#endif
#ifdef SYS_execveat
    if (nr == SYS_execveat) {
        /* Only elide when the path is absolute — matching the
         * conservative classifier rule.  An execveat with a relative
         * path against a dirfd would require resolving the dirfd, and
         * the path_remap addin running before us has not necessarily
         * folded that into an absolute path. */
        const char *path  = (const char *)ctx->args[1];
        char *const *argv = (char *const *)ctx->args[2];
        char *const *envp = (char *const *)ctx->args[3];
        if (!path || path[0] != '/') return 0;
        return handle_execve(ctx, path, argv, envp);
    }
#endif
    return 0;
}

const struct sud_addin sud_fake_exec_addin = {
    "fake_exec",
    fake_exec_init,
    0,
    0,
    fake_exec_pre_syscall,
    0,
};
