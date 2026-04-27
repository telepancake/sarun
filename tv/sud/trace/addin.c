#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/trace/event.h"
#include "sud/elf.h"
#include "sud/state.h"

static int trace_is_proc_self_exe(const char *rpath)
{
    if (!rpath) return 0;
    const char *p = rpath;
    if (p[0] != '/' || p[1] != 'p' || p[2] != 'r' ||
        p[3] != 'o' || p[4] != 'c' || p[5] != '/') return 0;
    p += 6;
    if (p[0] == 's' && p[1] == 'e' && p[2] == 'l' &&
        p[3] == 'f' && p[4] == '/') {
        p += 5;
        return (p[0] == 'e' && p[1] == 'x' && p[2] == 'e' && p[3] == '\0');
    }
    pid_t mypid = (pid_t)raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
    pid_t parsed = 0;
    const char *d = p;
    while (*d >= '0' && *d <= '9')
        parsed = parsed * 10 + (*d++ - '0');
    return (d > p && *d == '/' && d[1] == 'e' && d[2] == 'x' &&
            d[3] == 'e' && d[4] == '\0' && parsed == mypid);
}

static void trace_wrapper_init(void)
{
    stat_buf_t stbuf;
    if (fstat(SUD_OUTPUT_FD, (struct stat *)&stbuf) == 0) {
        g_out_fd = SUD_OUTPUT_FD;
    } else {
        const char *out_path = getenv(SUDTRACE_OUTFILE_ENV);
        if (out_path && out_path[0]) {
            int ofd = open(out_path, O_WRONLY | O_CREAT | O_APPEND, 0644);
            g_out_fd = ofd >= 0 ? ofd : STDOUT_FILENO;
        } else {
            g_out_fd = STDOUT_FILENO;
        }
    }
    sud_wire_init();
    g_creator_stdout_valid =
        (fstat(STDOUT_FILENO, (struct stat *)&g_creator_stdout_stbuf) == 0);
}

static void trace_target_launch(const struct sud_tracee_launch *launch)
{
    if (!launch) return;
    if (launch->visible_exe) {
        char resolved[PATH_MAX];
        if (resolve_path(launch->visible_exe, resolved, sizeof(resolved)))
            snprintf(g_target_exe, sizeof(g_target_exe), "%s", resolved);
        else
            snprintf(g_target_exe, sizeof(g_target_exe), "%s", launch->visible_exe);
    }
    emit_cwd_event(raw_gettid());
    emit_exec_event(raw_gettid(), launch->visible_exe,
                    launch->visible_argc, launch->visible_argv);
    emit_inherited_open_events(raw_gettid());
}

static void trace_fork_child(void)
{
    sud_wire_postfork();
}

static int trace_pre_syscall(struct sud_syscall_ctx *ctx)
{
#ifdef SYS_readlinkat
    if (ctx->nr == SYS_readlinkat && g_target_exe[0] &&
        trace_is_proc_self_exe((const char *)ctx->args[1])) {
        size_t tlen = strlen(g_target_exe);
        char *obuf = (char *)ctx->args[2];
        size_t obsz = (size_t)ctx->args[3];
        if (tlen > obsz) tlen = obsz;
        memcpy(obuf, g_target_exe, tlen);
        ctx->ret = (long)tlen;
        return 1;
    }
#endif
#ifdef SYS_readlink
    if (ctx->nr == SYS_readlink && g_target_exe[0] &&
        trace_is_proc_self_exe((const char *)ctx->args[0])) {
        size_t tlen = strlen(g_target_exe);
        char *obuf = (char *)ctx->args[1];
        size_t obsz = (size_t)ctx->args[2];
        if (tlen > obsz) tlen = obsz;
        memcpy(obuf, g_target_exe, tlen);
        ctx->ret = (long)tlen;
        return 1;
    }
#endif
    return 0;
}

static void trace_post_syscall(const struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr, ret = ctx->ret;
    long a0 = ctx->args[0], a1 = ctx->args[1], a2 = ctx->args[2];
#ifdef SYS_openat
    if (nr == SYS_openat) emit_open_event(ctx->tid, (const char *)a1, (int)a2, ret);
#endif
#ifdef SYS_open
    if (nr == SYS_open) emit_open_event(ctx->tid, (const char *)a0, (int)a1, ret);
#endif
#ifdef SYS_unlinkat
    if (nr == SYS_unlinkat && ret == 0) emit_unlink_event(ctx->tid, (const char *)a1, ret);
#endif
#ifdef SYS_unlink
    if (nr == SYS_unlink && ret == 0) emit_unlink_event(ctx->tid, (const char *)a0, ret);
#endif
#ifdef SYS_chdir
    if (nr == SYS_chdir && ret == 0) emit_cwd_event(ctx->tid);
#endif
#ifdef SYS_fchdir
    if (nr == SYS_fchdir && ret == 0) emit_cwd_event(ctx->tid);
#endif
    if (nr == SYS_write && ret > 0) {
        unsigned int fd = (unsigned int)a0;
        if (fd == 2) emit_write_event(ctx->tid, "STDERR", (const void *)a1, (size_t)ret);
        else if (fd == 1 && fd1_is_creator_stdout(ctx->tid))
            emit_write_event(ctx->tid, "STDOUT", (const void *)a1, (size_t)ret);
    }
#ifdef SYS_wait4
    if (nr == SYS_wait4 && ret > 0) {
        int wstatus = 0;
        if (a1) wstatus = *(int *)a1;
        if (WIFEXITED(wstatus) || WIFSIGNALED(wstatus))
            emit_exit_event((pid_t)ret, wstatus);
    }
#endif
#ifdef SYS_waitid
    if (nr == SYS_waitid && ret == 0 && ctx->args[2]) {
        siginfo_t *si = (siginfo_t *)ctx->args[2];
        if (si->si_pid > 0 &&
            (si->si_code == CLD_EXITED || si->si_code == CLD_KILLED ||
             si->si_code == CLD_DUMPED)) {
            int wstatus;
            if (si->si_code == CLD_EXITED) wstatus = si->si_status << 8;
            else {
                wstatus = si->si_status & 0x7f;
                if (si->si_code == CLD_DUMPED) wstatus |= 0x80;
            }
            emit_exit_event(si->si_pid, wstatus);
        }
    }
#endif
}

const struct sud_addin sud_trace_addin = {
    "trace",
    trace_wrapper_init,
    trace_target_launch,
    trace_fork_child,
    trace_pre_syscall,
    trace_post_syscall,
};
