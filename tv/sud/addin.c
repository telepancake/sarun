#include "sud/addin.h"

static const struct sud_addin *const g_addins[] = {
#ifdef SUD_ADDIN_TRACE
    &sud_trace_addin,
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
