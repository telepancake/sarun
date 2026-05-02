/*
 * sud/fake-exec/addin.c — SUD addin glue for fake-exec.
 *
 * Hooks SYS_execve / SYS_execveat in pre_syscall, runs the
 * classifier, and on a positive decision issues a per-task
 * SYS_exit with the emulator's status — the kernel never
 * sees the execve.
 *
 * vfork safety: the only kernel call we issue on the elide path is
 * SYS_exit (per-task, not exit_group).  In a child of
 * clone(CLONE_VM|CLONE_VFORK) — glibc's posix_spawn pattern — this
 * terminates only the child task and wakes the suspended parent,
 * matching the behaviour of running the real helper binary.  We
 * never allocate, never call libc, never write to a global.
 *
 * Trace fidelity: trace runs first in the dispatch chain
 * (sud/addin.c) so the EXEC event is recorded from the program's
 * original syscall args before path_remap rewrites or fake-exec
 * elides them.  The matching EXIT event is recorded by trace's
 * pre_syscall on our SYS_exit.  The MVP only elides emulators with
 * no observable I/O (true/false/:), so the recorded event sequence
 * is byte-identical to a real run — only timing differs.
 */

#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"
#include "sud/fake-exec/fake_exec.h"
#include "sud/fake-exec/builtins.h"

static void fake_exec_init(void)
{
    /* Nothing to do at wrapper startup: the builtin registry is a
     * compile-time constant and the runtime config is parsed by
     * sud/wrapper.c::main before sud_addins_wrapper_init runs. */
}

/* Issue SYS_exit with the given status.  Never returns; using
 * SYS_exit (not SYS_exit_group) is essential when called from a
 * vfork child, where exit_group would terminate the whole thread
 * group of the calling task — which still includes only the child
 * (CLONE_VFORK without CLONE_THREAD), but SYS_exit is the
 * per-task primitive and matches real-helper semantics exactly. */
static void __attribute__((noreturn))
fake_exec_emit_exit(int status)
{
    raw_syscall6(SYS_exit, (long)(status & 0xff), 0, 0, 0, 0, 0);
    /* Defensive: SYS_exit cannot return on Linux, but the compiler
     * needs an unreachable terminator for noreturn. */
    for (;;) raw_syscall6(SYS_exit, 0, 0, 0, 0, 0, 0);
}

static int handle_execve(const char *path,
                         char *const *argv, char *const *envp)
{
    struct fake_exec_decision d;
    if (sud_fake_exec_classify(path, argv, envp, &d) != 0)
        return 0;
    if (d.track != FAKE_EXEC_INLINE_VFORK_SAFE)
        return 0;

    /* Commit point: do NOT return from the addin.  The execve syscall
     * never returns on success in a real run, and our caller likewise
     * expects the calling thread to disappear here. */
    fake_exec_emit_exit(d.exit_status);
}

static int fake_exec_pre_syscall(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;

#ifdef SYS_execve
    if (nr == SYS_execve) {
        const char *path  = (const char *)ctx->args[0];
        char *const *argv = (char *const *)ctx->args[1];
        char *const *envp = (char *const *)ctx->args[2];
        return handle_execve(path, argv, envp);
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
        return handle_execve(path, argv, envp);
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
