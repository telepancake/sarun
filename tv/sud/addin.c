#include "sud/addin.h"

/*
 * Addin invocation order.
 *
 * The `pre_syscall` and `post_syscall` hooks of every registered addin
 * are invoked in the order they appear in this array.  That order
 * matters when more than one addin observes or mutates the same
 * syscall, so the contract is fixed here:
 *
 *   1. sud_trace_addin runs FIRST.  It is a pure observer: it sees
 *      the syscall arguments exactly as the traced program passed
 *      them, with no knowledge of any remapping that may follow.
 *      This is important: if the program calls open("/tmp/x"), the
 *      trace must record "/tmp/x" — the path the program actually
 *      asked for — regardless of whether a remap/overlay/inramfs
 *      rule will later rewrite or short-circuit the call.  Trace
 *      output must therefore be byte-for-byte identical regardless
 *      of which mutator addins are compiled in or active.
 *
 *   2. sud_path_remap_addin runs SECOND.  It is the path layer:
 *      it owns the chdir/getcwd/fchdir interception (CWD shadow +
 *      dirfd table), routes any path under the inramfs mount to
 *      the inramfs data store via sud_pr_inramfs_route_pre_syscall
 *      (sud/path_remap/inramfs_glue.c), and applies the overlay /
 *      remap rule table to anything else.  By the time this addin
 *      returns, every path-bearing syscall has either been short-
 *      circuited (with -errno or a synthetic fd) or had its path
 *      arg rewritten to the resolved kernel path.
 *
 *   3. sud_fake_exec_addin runs THIRD.  It intercepts execve/execveat
 *      and, for a small registry of trivial helper binaries
 *      (true/false/:), replaces the kernel exec with a per-task
 *      SYS_exit emitting the synthesised status.  Slotted after
 *      path_remap so the path it sees is already resolved to a
 *      kernel-canonical absolute path; slotted before inramfs so we
 *      never need an inramfs-served binary to back a builtin we are
 *      about to elide.  Trace fidelity is preserved because the
 *      trace addin (above) records the EXEC event from the original
 *      args before we run.
 *
 *   4. sud_inramfs_addin runs LAST.  After the Part-1 re-layering
 *      it sees only fd-bearing syscalls (read/write/lseek/dup/
 *      fcntl/mmap/munmap/copy_file_range/...): path_remap already
 *      handled all path-bearing dispatch into the inramfs data
 *      store.  fds returned by inramfs are real (memfd-backed) so
 *      this hook just has to recognise them via sud_inramfs_owns_fd
 *      and serve read/write/seek from the inramfs extents instead
 *      of the empty memfd.
 *
 * Any addin can be omitted at compile time (via the SUD_ADDIN_*
 * macros set by the Makefile from the SUD_ADDINS list); the others
 * keep working unmodified.  When SUD_ADDIN_INRAMFS is omitted,
 * sud_pr_inramfs_route_pre_syscall isn't called and path_remap
 * just runs its overlay/remap dispatch.
 */
static const struct sud_addin *const g_addins[] = {
#ifdef SUD_ADDIN_TRACE
    &sud_trace_addin,
#endif
#ifdef SUD_ADDIN_PATH_REMAP
    &sud_path_remap_addin,
#endif
#ifdef SUD_ADDIN_FAKE_EXEC
    &sud_fake_exec_addin,
#endif
#ifdef SUD_ADDIN_INRAMFS
    &sud_inramfs_addin,
#endif
    0
};

const struct sud_addin *const *sud_addins(void)
{
    return g_addins;
}

int sud_addins_wrapper_init(void)
{
    for (int i = 0; g_addins[i]; i++)
        if (g_addins[i]->wrapper_init)
            g_addins[i]->wrapper_init();
    return 0;
}

void sud_addins_target_launch(const struct sud_tracee_launch *launch)
{
    for (int i = 0; g_addins[i]; i++)
        if (g_addins[i]->target_launch)
            g_addins[i]->target_launch(launch);
}

void sud_addins_fork_child(void)
{
    for (int i = 0; g_addins[i]; i++)
        if (g_addins[i]->fork_child)
            g_addins[i]->fork_child();
}

int sud_addins_pre_syscall(struct sud_syscall_ctx *ctx)
{
    /* Snapshot the program-supplied args so that observer addins
     * (trace) can be presented with them in post_syscall, even if a
     * later mutator addin (path_remap) rewrote ctx->args[] for the
     * kernel call. */
    for (int i = 0; i < 6; i++)
        ctx->orig_args[i] = ctx->args[i];

    for (int i = 0; g_addins[i]; i++) {
        if (g_addins[i]->pre_syscall && g_addins[i]->pre_syscall(ctx))
            return 1;
    }
    return 0;
}

void sud_addins_post_syscall(const struct sud_syscall_ctx *ctx)
{
    /* Hand each addin a view of ctx in which args[] are restored to
     * the values the traced program originally passed.  This keeps
     * trace remapping-agnostic: it produces byte-identical output
     * regardless of whether path_remap is compiled in, enabled, or
     * configured with rules.  path_remap itself has no post_syscall
     * hook today, so there is no consumer that would prefer to see
     * the rewritten args here. */
    struct sud_syscall_ctx local = *ctx;
    for (int i = 0; i < 6; i++)
        local.args[i] = ctx->orig_args[i];

    for (int i = 0; g_addins[i]; i++)
        if (g_addins[i]->post_syscall)
            g_addins[i]->post_syscall(&local);
}
