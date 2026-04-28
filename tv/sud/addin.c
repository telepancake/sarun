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
 *      asked for — regardless of whether a SUD_OVERLAY/SUD_REMAP/
 *      SUD_INRAMFS rule will later rewrite or short-circuit the call.
 *      Trace output must therefore be byte-for-byte identical
 *      regardless of which mutator addins are compiled in or active.
 *
 *   2. sud_inramfs_addin runs SECOND.  It is the first mutator:
 *      paths that fall under the in-RAM filesystem mount short-
 *      circuit immediately, returning a real (memfd-backed) fd or
 *      a -errno without ever reaching the kernel.  fds returned by
 *      inramfs are also recognised by inramfs's hijack of fd-based
 *      ops (read/write/lseek/...) so subsequent operations on them
 *      are served from the shared shm region.  Paths NOT under the
 *      mount fall through unchanged to path_remap.
 *
 *   3. sud_path_remap_addin runs THIRD.  Its pre_syscall mutates
 *      ctx->args[i] (overlay path resolution) so that the kernel
 *      receives the rewritten path, and may short-circuit the
 *      syscall entirely (whiteout → -ENOENT, read-only overlay →
 *      -EROFS, merged-dir openat → synthetic fd).  Because inramfs
 *      ran first, any args still pointing at an inramfs-mount path
 *      have already been intercepted; path_remap sees only paths
 *      that should be passed through to the kernel filesystem.
 *
 * Any addin can be omitted at compile time (via the SUD_ADDIN_*
 * macros set by the Makefile from the SUD_ADDINS list); the others
 * keep working unmodified.
 */
static const struct sud_addin *const g_addins[] = {
#ifdef SUD_ADDIN_TRACE
    &sud_trace_addin,
#endif
#ifdef SUD_ADDIN_INRAMFS
    &sud_inramfs_addin,
#endif
#ifdef SUD_ADDIN_PATH_REMAP
    &sud_path_remap_addin,
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
