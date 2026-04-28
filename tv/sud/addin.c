#include "sud/addin.h"

/*
 * Addin invocation order.
 *
 * The `pre_syscall` and `post_syscall` hooks of every registered addin
 * are invoked in the order they appear in this array.  That order
 * matters when more than one addin observes or mutates the same
 * syscall, so the contract is fixed here:
 *
 *   1. sud_path_remap_addin runs FIRST.  Its pre_syscall mutates
 *      ctx->args[i] (overlay path resolution) and may short-circuit
 *      the syscall entirely (whiteout → -ENOENT, read-only overlay →
 *      -EROFS, merged-dir openat → synthetic fd).
 *
 *   2. sud_trace_addin runs SECOND.  Its pre_syscall and
 *      post_syscall hooks therefore observe ctx->args after
 *      path_remap has rewritten them — meaning trace records the
 *      paths the kernel actually saw.  trace itself contains no
 *      remap-aware code; it just reads ctx->args like any other
 *      observer.  When path_remap short-circuits a syscall the
 *      dispatcher returns early (see handler.c) and trace is
 *      correctly bypassed for that syscall, since no kernel-level
 *      operation occurred from the tracee's point of view that
 *      wasn't already accounted for by path_remap.
 *
 * Either addin can be omitted at compile time (via the SUD_ADDIN_*
 * macros set by the Makefile from the SUD_ADDINS list); the other
 * keeps working unmodified.
 */
static const struct sud_addin *const g_addins[] = {
#ifdef SUD_ADDIN_PATH_REMAP
    &sud_path_remap_addin,
#endif
#ifdef SUD_ADDIN_TRACE
    &sud_trace_addin,
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
    for (int i = 0; g_addins[i]; i++) {
        if (g_addins[i]->pre_syscall && g_addins[i]->pre_syscall(ctx))
            return 1;
    }
    return 0;
}

void sud_addins_post_syscall(const struct sud_syscall_ctx *ctx)
{
    for (int i = 0; g_addins[i]; i++)
        if (g_addins[i]->post_syscall)
            g_addins[i]->post_syscall(ctx);
}
