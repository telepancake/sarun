#ifndef SUD_ADDIN_H
#define SUD_ADDIN_H

#include "libc-fs/libc.h"

struct sud_syscall_ctx {
    long nr;
    long args[6];
    long ret;
    pid_t tid;
    char *scratch;
    size_t scratch_size;
};

struct sud_tracee_launch {
    const char *path;
    int argc;
    char **argv;
    int drop_count;
    const char *visible_exe;
    int visible_argc;
    char **visible_argv;
};

struct sud_launcher_config {
    const char *outfile;
    int no_env;
};

struct sud_addin {
    const char *name;
    void (*wrapper_init)(void);
    void (*target_launch)(const struct sud_tracee_launch *launch);
    void (*fork_child)(void);
    int  (*pre_syscall)(struct sud_syscall_ctx *ctx);
    void (*post_syscall)(const struct sud_syscall_ctx *ctx);
};

const struct sud_addin *const *sud_addins(void);
int sud_addins_wrapper_init(void);
void sud_addins_target_launch(const struct sud_tracee_launch *launch);
void sud_addins_fork_child(void);
int sud_addins_pre_syscall(struct sud_syscall_ctx *ctx);
void sud_addins_post_syscall(const struct sud_syscall_ctx *ctx);

extern const struct sud_addin sud_trace_addin;
extern const struct sud_addin sud_path_remap_addin;

#endif /* SUD_ADDIN_H */
