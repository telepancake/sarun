/*
 * sud/handler.c — SIGSYS signal handler and SUD setup for sudtrace.
 *
 * Contains the core SIGSYS handler that intercepts all syscalls via
 * Syscall User Dispatch, plus helper functions for installing the
 * handler and preparing child processes.
 */

#include "sud/handler.h"
#include "sud/raw.h"
#include "sud/event.h"
#include "sud/elf.h"

/* ================================================================
 * SUD selector globals — defined here, declared extern in handler.h
 * ================================================================ */
volatile unsigned char sud_selector_storage
    = SYSCALL_DISPATCH_FILTER_BLOCK;

volatile unsigned char *g_sud_selector_ptr = &sud_selector_storage;

/* ================================================================
 * SYS_chdir / SYS_fchdir — not always defined in libc.h
 * ================================================================ */
#ifndef SYS_chdir
#ifdef __NR_chdir
#define SYS_chdir __NR_chdir
#endif
#endif
#ifndef SYS_fchdir
#ifdef __NR_fchdir
#define SYS_fchdir __NR_fchdir
#endif
#endif

/* ================================================================
 * install_sigsys_handler_raw — install SIGSYS handler with raw syscall.
 * ================================================================ */
void install_sigsys_handler_raw(void)
{
#ifdef SYS_rt_sigaction
    struct kernel_sigaction_raw sa;
    memset(&sa, 0, sizeof(sa));
    sa.handler = (void (*)(int))sigsys_handler;
    /* SA_ONSTACK: run the SIGSYS handler on the alternate signal stack
     * installed by ensure_sud_altstack(). The handler is invoked for
     * every syscall the traced program makes and uses several KiB of
     * stack per frame; running in-line on the program's stack risks
     * overflow when the program is already deep (recursive code, large
     * locals, etc.). The exact failure mode of running without
     * SA_ONSTACK has not been characterised here — any SI_KERNEL
     * SIGSEGV reproducer should be captured with the diagnostic
     * handler in sud/loader.c (which dumps registers, code window,
     * recent syscalls, and /proc/self/maps) before drawing
     * conclusions. */
    sa.flags = SA_SIGINFO | SA_RESTART | SA_RESTORER | SA_ONSTACK;
    sa.restorer = sud_rt_sigreturn_restorer;
    /*
     * Block ALL signals while the SIGSYS handler is active.
     *
     * Without this, other signals (SIGCHLD, SIGALRM, SIGPIPE, etc.)
     * can be delivered during the SIGSYS handler.  Those signal
     * handlers run in the traced program's code (outside the SUD
     * allowed IP range).  If the interrupted handler makes a syscall,
     * the kernel tries to deliver a nested SIGSYS.  But SIGSYS is
     * already blocked (auto-masked during the handler), so the kernel
     * uses force_sig_fault(SIGSYS) which overrides the mask and
     * disposition, killing the process with "Bad system call".
     *
     * By blocking all signals in sa_mask, no signal handler from the
     * traced program can run during our handler.  Pending signals are
     * delivered after the handler returns (via rt_sigreturn) and the
     * original signal mask is restored.
     *
     * This is the root cause of failures in complex builds (LTO,
     * distrobox, etc.) where SIGCHLD from child process termination
     * interrupts the handler at high frequency.
     *
     * Exception: SIGSEGV (11) and SIGBUS (7) are left UNBLOCKED.
     * These synchronous fault signals must be deliverable so that a
     * diagnostic crash handler can report the faulting address and
     * register state.  When they are blocked and a fault occurs, the
     * kernel force-delivers them with SIG_DFL (termination), which
     * bypasses any installed handler and makes the crash silent.
     */
    sa.mask = ~(sud_sigset_word_t)0;
    sa.mask &= ~((sud_sigset_word_t)1 << (SIGSEGV - 1));  /* unblock SIGSEGV */
    sa.mask &= ~((sud_sigset_word_t)1 << (SIGBUS - 1));   /* unblock SIGBUS  */
    raw_syscall6(SYS_rt_sigaction, SIGSYS, (long)&sa, 0,
                 sizeof(sa.mask), 0, 0);
#endif
}

/*
 * Reset the signal mask to unblock all signals.
 *
 * Called from prepare_child_sud() for children created via clone_raw /
 * clone3_raw.  Those children do NOT go through rt_sigreturn (which would
 * normally restore the pre-handler mask).  Instead they jump directly to
 * the program's RIP.  Their signal mask is inherited from the parent at
 * clone time, which is the handler's mask (all blocked).  We must reset
 * it so the child runs with a clean signal state.
 *
 * For children created via fork (non-CLONE_VM), they DO go through
 * rt_sigreturn after the handler returns — rt_sigreturn restores the
 * pre-handler signal mask from the saved ucontext, so this reset is
 * harmlessly overwritten before the child executes user code.
 */
void reset_sigmask_raw(void)
{
#ifdef SYS_rt_sigprocmask
    sud_sigset_word_t mask = 0;
    raw_syscall6(SYS_rt_sigprocmask, SIG_SETMASK, (long)&mask, 0,
                 sizeof(mask), 0, 0);
#endif
}

void reenable_sud_in_child(void)
{
    unsigned long off = (unsigned long)__sud_begin;
    unsigned long len = (unsigned long)__sud_end - (unsigned long)__sud_begin;
    raw_syscall6(SYS_prctl, PR_SET_SYSCALL_USER_DISPATCH,
                 PR_SYS_DISPATCH_ON, off, len,
                 (long)g_sud_selector_ptr, 0);
}

void prepare_child_sud(void)
    __attribute__((noinline));

/* ================================================================
 * ensure_sud_altstack — install a fresh per-task alternate signal
 *                        stack via mmap + sigaltstack.
 *
 * SIGSYS fires for every syscall the traced program makes, and the
 * handler does non-trivial work (build_exec_argv with multiple
 * PATH_MAX buffers, the per-call exec arena, nested raw_syscall6
 * calls, the wire emitter, etc.).  Without an alt-stack the kernel
 * delivers the signal on the program's own stack — easily corrupted
 * by deep recursion or large user frames.
 *
 * CRITICAL: each task that may take SIGSYS needs ITS OWN alt-stack
 * mapping.  Children created with CLONE_VM (posix_spawn helpers,
 * pthread workers) share the parent's address space, so they share
 * the parent's alt-stack mmap.  When parent and child both run a
 * SIGSYS handler concurrently — which is the steady state for
 * posix_spawn (CLONE_VM | CLONE_VFORK keeps the parent suspended in
 * its own SIGSYS handler while the child execs) — both kernels
 * write signal frames at the top of the same physical pages.  The
 * child's frame overwrites the parent's saved RIP/registers; on
 * sigreturn the parent jumps to garbage with leftover registers
 * (e.g. RAX = the child PID returned by clone()) and crashes.
 *
 * The fix is to re-mmap and re-sigaltstack from prepare_child_sud()
 * for every child created by clone_raw / clone3_raw / fork.  fork()
 * children would otherwise just COW the parent's mapping, but
 * re-installing one keeps the policy uniform and harmless.
 * ================================================================ */
#define SUD_ALTSTACK_SIZE (256 * 1024)
void ensure_sud_altstack(void)
{
    /* Use raw_mmap rather than raw_syscall6(SYS_mmap, ...) directly: on
     * i386 the latter would invoke the legacy sys_old_mmap (which expects
     * a `struct mmap_arg_struct *` in ebx, not 6 GPR args) and silently
     * return -EFAULT, leaving us with no alt-stack on 32-bit. Then with
     * SA_ONSTACK set on the SIGSYS handler the kernel falls back to the
     * program's regular stack, where the handler's per-call 64 KiB arena
     * (sigsys_handler_inner's `arena_buf`) overflows the program's
     * stack guard page and the recursive fault during signal-frame
     * setup gets force-killed: a silent SIGSEGV with no diagnostic. */
    void *altstack = raw_mmap(NULL, SUD_ALTSTACK_SIZE,
                              PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if ((unsigned long)altstack >= (unsigned long)-4095) return;
    struct { void *ss_sp; int ss_flags; size_t ss_size; } ss;
    ss.ss_sp    = altstack;
    ss.ss_flags = 0;
    ss.ss_size  = SUD_ALTSTACK_SIZE;
    raw_syscall6(SYS_sigaltstack, (long)&ss, 0, 0, 0, 0, 0);
}

void prepare_child_sud(void)
{
    /* Each CLONE_VM child shares its parent's alt-stack mapping.  If
     * we don't install a fresh one here, two tasks running SIGSYS
     * handlers concurrently — typical for posix_spawn (CLONE_VM |
     * CLONE_VFORK) and for pthread workers — both write signal
     * frames at the top of the same alt-stack memory.  The kernel
     * does not coordinate this between tasks, so the second
     * delivery overwrites the first task's saved RIP/registers; on
     * sigreturn the first task resumes with garbage state and
     * crashes (often manifests as RIP somewhere inside the SIGSYS
     * dispatch with RAX = the leftover return value of the most
     * recent clone()). */
    ensure_sud_altstack();
    install_sigsys_handler_raw();
    reset_sigmask_raw();
    reenable_sud_in_child();
    /* Grab a fresh wire stream_id and reset the delta encoder so this
     * child doesn't share encoding state with its parent — see
     * sud_wire_postfork() for the full rationale. */
    sud_wire_postfork();
}

/* ================================================================
 * is_proc_self_exe — check if a path is /proc/self/exe or
 * /proc/<our-pid>/exe.  Used by the SIGSYS handler to intercept
 * readlink/readlinkat and return the target program's path instead
 * of sudtrace's.
 * ================================================================ */
static int is_proc_self_exe(const char *rpath)
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
    /* /proc/<digits>/exe — check if digits match our PID */
    pid_t mypid = (pid_t)raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
    pid_t parsed = 0;
    const char *d = p;
    while (*d >= '0' && *d <= '9')
        parsed = parsed * 10 + (*d++ - '0');
    return (d > p && *d == '/' && d[1] == 'e' && d[2] == 'x' &&
            d[3] == 'e' && d[4] == '\0' && parsed == mypid);
}

/* ================================================================
 * sigsys_diag_dump — dump ucontext register state to stderr.
 *
 * Enabled at compile time with -DSUDTRACE_SIGSYS_DIAG.  Prints the
 * saved signal frame (from uc) at handler ENTRY and EXIT so that
 * changes to the frame inside the handler can be detected.  Also
 * prints the live stack pointer at the moment of the call so that
 * stack-overlap / alt-stack overflow can be detected.
 *
 * Build with:  make SIGSYS_DIAG=1 sud32   (or sud64)
 * ================================================================ */
#ifdef SUDTRACE_SIGSYS_DIAG
#include "sud/fmt.h"
static void sigsys_diag_dump(const char *tag, ucontext_t *uc,
                              unsigned long sp_now)
{
    char msg[640];
    char *p = msg;

    /* Header */
    p = fmt_str(p, "\nsudtrace: SIGSYS_DIAG ");
    p = fmt_str(p, tag);
    /* PID/TID — multi-process traces interleave diagnostics from
     * many tracees on the same fd, so identifying which one produced
     * each line is essential. raw_gettid() returns the LWP (kernel
     * tid); raw_syscall6(SYS_getpid) returns the thread-group leader. */
    p = fmt_str(p, " pid=");
    p = fmt_int(p, (int)raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0));
    p = fmt_str(p, " tid=");
    p = fmt_int(p, (int)raw_gettid());
    p = fmt_ch(p, '\n');

    /* uc pointer and live stack pointer */
    p = fmt_str(p, "  uc=0x");
#if defined(__x86_64__)
    p = fmt_hex_ul(p, (unsigned long)uc, 16);
    p = fmt_str(p, "  sp_now=0x");
    p = fmt_hex_ul(p, sp_now, 16);
#else
    p = fmt_hex_ul(p, (unsigned long)uc, 8);
    p = fmt_str(p, "  sp_now=0x");
    p = fmt_hex_ul(p, sp_now, 8);
#endif
    p = fmt_ch(p, '\n');

    /* Saved instruction and stack pointers, and syscall register */
#if defined(__x86_64__)
    p = fmt_str(p, "  RIP=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[REG_RIP], 16);
    p = fmt_str(p, "  RSP=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[REG_RSP], 16);
    p = fmt_str(p, "  RAX=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[REG_RAX], 16);
    p = fmt_ch(p, '\n');
#else
    p = fmt_str(p, "  EIP=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[REG_EIP], 8);
    p = fmt_str(p, "  ESP=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[REG_ESP], 8);
    p = fmt_str(p, "  EAX=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[REG_EAX], 8);
    p = fmt_ch(p, '\n');
    /* Segment registers — GS/FS and CS/SS; CS=gregs[15], SS=gregs[18].
     * SI_KERNEL on i386 is commonly caused by bad segment state at iret. */
    p = fmt_str(p, "  GS=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[REG_GS], 8);
    p = fmt_str(p, "  FS=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[REG_FS], 8);
    p = fmt_str(p, "  CS=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[15], 8);
    p = fmt_str(p, "  SS=0x");
    p = fmt_hex_ul(p, (unsigned long)uc->uc_mcontext.gregs[18], 8);
    p = fmt_ch(p, '\n');
#endif

    /* uc_stack — shows which stack the kernel used for signal delivery */
    p = fmt_str(p, "  uc_stack.ss_sp=0x");
#if defined(__x86_64__)
    p = fmt_hex_ul(p, (unsigned long)uc->uc_stack.ss_sp, 16);
#else
    p = fmt_hex_ul(p, (unsigned long)uc->uc_stack.ss_sp, 8);
#endif
    p = fmt_str(p, " ss_flags=");
    p = fmt_int(p, uc->uc_stack.ss_flags);
    p = fmt_str(p, " ss_size=");
    p = fmt_ulong(p, (unsigned long)uc->uc_stack.ss_size);
    p = fmt_ch(p, '\n');

    raw_write(2, msg, (size_t)(p - msg));
}
#endif /* SUDTRACE_SIGSYS_DIAG */

/* ================================================================
 * SIGSYS handler — the core of SUD tracing.
 *
 * When a process with SUD enabled makes a syscall and the instruction
 * pointer is outside [__sud_begin, __sud_end), the kernel delivers
 * SIGSYS.  This handler:
 *   1. Reads the syscall number and arguments from the ucontext
 *   2. Performs the real syscall (with selector = ALLOW)
 *   3. Logs the relevant trace event
 *   4. Stores the return value back in the ucontext
 *
 * Since the handler runs in the traced process itself, we have direct
 * access to its memory (no ptrace or /proc/mem needed for pointers).
 * ================================================================ */
static void sigsys_handler_inner(int sig, siginfo_t *info, void *uctx_raw);

void sigsys_handler(int sig, siginfo_t *info, void *uctx_raw)
{
    ucontext_t *uc = (ucontext_t *)uctx_raw;
    long nr = UC_SYSCALL_NR(uc);
    unsigned long pc = UC_PC(uc);
    int tid = (int)raw_gettid();

    /* Allocate a ring slot up-front and mark it "in progress" so that
     * if the inner handler crashes (bad pointer, etc.) the crash
     * dumper can still see what syscall we were processing. */
    unsigned int slot =
        __atomic_fetch_add(&g_sud_syslog_head, 1, __ATOMIC_RELAXED)
        & (SUD_SYSLOG_SIZE - 1);
    g_sud_syslog[slot].nr  = nr;
    g_sud_syslog[slot].pc  = pc;
    g_sud_syslog[slot].tid = tid;
    g_sud_syslog[slot].ret = SUD_SYSLOG_NORETURN;
    /* Make the new entry visible to a crash on another thread before
     * we run any user-pointer-touching code. */
    __atomic_thread_fence(__ATOMIC_RELEASE);

    sigsys_handler_inner(sig, info, uctx_raw);

    /* Record the final return value the handler placed into the ucontext.
     * The inner handler ends with `UC_SET_RET(uc, ret)` which writes the
     * syscall return into REG_RAX/REG_EAX — i.e. into the same gregs slot
     * that originally held the syscall number. So reading UC_SYSCALL_NR
     * here yields the return value, not the syscall number we entered with.
     * For execve(2) on success this code is unreachable (kernel never
     * returns to us), and the slot stays NORETURN — which the dumper
     * labels as `<in-progress>`. */
    g_sud_syslog[slot].ret = (long)UC_SYSCALL_NR(uc);
}

static void sigsys_handler_inner(int sig, siginfo_t *info, void *uctx_raw)
{
    ucontext_t *uc = (ucontext_t *)uctx_raw;
    (void)sig;

#ifdef SUDTRACE_SIGSYS_DIAG
    {
        unsigned long sp_now;
#if defined(__x86_64__)
        __asm__ volatile("mov %%rsp, %0" : "=r"(sp_now));
#else
        __asm__ volatile("mov %%esp, %0" : "=r"(sp_now));
#endif
        sigsys_diag_dump("ENTRY", uc, sp_now);
    }
#endif

    /* Distinguish SUD-generated SIGSYS from seccomp-generated SIGSYS.
     *
     * SUD SIGSYS: si_code == SYS_USER_DISPATCH (2)
     * seccomp SIGSYS: si_code == SYS_SECCOMP (1)
     *
     * If a seccomp filter was somehow installed (e.g. inherited from a
     * parent before sudtrace started, or via a mechanism we didn't
     * intercept), the handler's own raw_syscall6 calls can trigger
     * seccomp SIGSYS.  Since SIGSYS is masked while we're in this
     * handler (SA_NODEFER not set), the kernel force-delivers the
     * signal, which would kill the process.
     *
     * For any non-SUD SIGSYS that does reach us, return -ENOSYS so
     * the program sees a clean error instead of crashing. */
    if (info->si_code != SYS_USER_DISPATCH) {
        UC_SET_RET(uc, -ENOSYS);
        return;
    }

    /* No selector toggling needed: all syscalls made from within
     * sudtrace's code are in the allowed IP range [__sud_begin,
     * __sud_end) and pass the kernel's SUD check regardless of the
     * selector byte value.  This is critical for multi-threaded
     * programs — a shared selector byte with toggling would race
     * between concurrent SIGSYS handlers on different threads. */

    pid_t tid = raw_gettid();

    long nr  = UC_SYSCALL_NR(uc);
    long a0  = UC_ARG0(uc);
    long a1  = UC_ARG1(uc);
    long a2  = UC_ARG2(uc);
    long a3  = UC_ARG3(uc);
    long a4  = UC_ARG4(uc);
    long a5  = UC_ARG5(uc);

    long ret;

    /*
     * ptrace(PTRACE_TRACEME) — a child process wants a ptrace-based
     * tracer (e.g. nested uproctrace, fakeroot-ng) to trace it.
     *
     * Execute the real ptrace call, then disable SUD for this process.
     * Rationale: ptrace and SUD both intercept syscalls.  If both are
     * active, ptrace sees the SIGSYS handler's internal syscalls instead
     * of the original ones, which confuses the sub-tracer.  Disabling
     * SUD lets the sub-tracer get clean syscall visibility while
     * sudtrace's parent still monitors this process via waitpid()
     * for EXEC/EXIT events.
     */
    if (nr == SYS_ptrace && a0 == 0 /* PTRACE_TRACEME */) {
        long r = raw_syscall6(SYS_ptrace, 0 /* PTRACE_TRACEME */,
                              0, 0, 0, 0, 0);
        if (r == 0) {
            /* Disable SUD: the sub-tracer now manages this process. */
            raw_syscall6(SYS_prctl, PR_SET_SYSCALL_USER_DISPATCH,
                         PR_SYS_DISPATCH_OFF, 0, 0, 0, 0);
            UC_SET_RET(uc, 0);
            return;
        }
        UC_SET_RET(uc, r);  /* raw kernel negative errno */
        return;
    }

    /*
     * seccomp() — the traced process is trying to install seccomp filters.
     *
     * Seccomp filters apply to ALL syscalls in this process, including
     * the ones made by sudtrace's SIGSYS handler via raw_syscall6().
     * If a restrictive filter is installed, handler-internal syscalls
     * like openat, clock_gettime, readlinkat, write, etc. would be
     * blocked — killing the process with "Bad system call" (SIGSYS
     * from seccomp while SIGSYS is already masked in the handler).
     *
     * Emulate success for filter/strict installation without actually
     * installing anything.  This is safe: sudtrace already intercepts
     * every syscall, making seccomp restrictions redundant.  Query
     * operations (GET_ACTION_AVAIL, GET_NOTIF_SIZES) pass through so
     * programs that probe seccomp support see correct results.
     */
#ifdef SYS_seccomp
    if (nr == SYS_seccomp) {
        if (a0 == SECCOMP_SET_MODE_STRICT || a0 == SECCOMP_SET_MODE_FILTER) {
            /* Safe: sudtrace already intercepts every syscall, making
             * seccomp restrictions redundant for the traced process. */
            UC_SET_RET(uc, 0);
            return;
        }
        /* Query operations: let the kernel answer */
        ret = raw_syscall6(nr, a0, a1, a2, a3, a4, a5);
        UC_SET_RET(uc, ret);
        return;
    }
#endif

    /*
     * prctl() — intercept operations that conflict with SUD tracing.
     *
     * PR_SET_SECCOMP: same issue as seccomp() above — installing a
     *   seccomp filter would break the handler.  Emulate success.
     *
     * PR_SET_SYSCALL_USER_DISPATCH: the traced program could disable
     *   or reconfigure SUD, which would break tracing entirely (the
     *   handler would no longer be called, or worse, the new allowed
     *   IP range wouldn't include sudtrace's code, causing recursive
     *   SIGSYS when the handler makes syscalls).  Block silently.
     *
     * All other prctl operations pass through normally.
     */
    if (nr == SYS_prctl) {
        if (a0 == PR_SET_SECCOMP) {
            UC_SET_RET(uc, 0);
            return;
        }
        if (a0 == PR_SET_SYSCALL_USER_DISPATCH) {
            UC_SET_RET(uc, 0);
            return;
        }
        /* All other prctl operations: pass through */
        ret = raw_syscall6(nr, a0, a1, a2, a3, a4, a5);
        UC_SET_RET(uc, ret);
        return;
    }

    /*
     * Special handling for execve: rewrite argv to go through sudtrace.
     */
    if (nr == SYS_execve) {
        const char *fn = (const char *)a0;
        char **orig_argv = (char **)a1;

        /* If the program handed us a NULL or empty filename, we have no
         * binary to classify or shebang-resolve.  Forwarding to the kernel
         * lets it produce the canonical -EFAULT/-ENOENT instead of us
         * crashing in resolve_path() trying to dereference cmd[0]. */
        if (!fn || !fn[0]) {
            ret = raw_syscall6(SYS_execve, a0, a1, a2, 0, 0, 0);
            UC_SET_RET(uc, ret);
            return;
        }

        int orig_argc = 0;
        if (orig_argv)
            while (orig_argv[orig_argc]) orig_argc++;

        /* Per-call arena, sized to fit any conforming argv (bounded by
         * ARG_MAX and our own prepend headroom).  mmap rather than a
         * fixed stack buffer — the previous 64 KiB stack arena
         * silently truncated argv on large execs (e.g. linker command
         * lines), which then either failed in the kernel or NULL-deref'd
         * inside the handler.  See tests/sud_stress.c::argv-huge. */
        size_t arena_size = exec_arena_size_for(fn, orig_argv, orig_argc);
        void *arena_buf = raw_mmap(NULL, arena_size,
                                   PROT_READ | PROT_WRITE,
                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if ((unsigned long)arena_buf >= (unsigned long)-4095) {
            /* True OOM — there is no fallback that would let sud do its
             * job here.  Surface the kernel's own error to the caller. */
            UC_SET_RET(uc, -ENOMEM);
            return;
        }

        struct sud_arena arena;
        sud_arena_init(&arena, arena_buf, arena_size);

        int build_argc = orig_argc > 0 ? orig_argc : 1;
        char **build_argv = sud_arena_alloc(&arena,
                                ((size_t)build_argc + 1) * sizeof(char *));
        char **new_argv = NULL;
        if (build_argv) {
            build_argv[0] = sud_arena_strdup(&arena, fn);
            for (int i = 1; i < orig_argc; i++)
                build_argv[i] = sud_arena_strdup(&arena, orig_argv[i]);
            build_argv[build_argc] = NULL;

            new_argv = build_exec_argv(&arena, build_argc, build_argv);
        }

        if (new_argv) {
            ret = raw_syscall6(SYS_execve, (long)new_argv[0],
                               (long)new_argv, a2, 0, 0, 0);
            /* On success exec replaces the process; we only reach here
             * on failure.  Unmap the arena before returning. */
        } else {
            /* Arena was sized by exec_arena_size_for() to fit, so
             * build_exec_argv returning NULL here means a sizing bug.
             * Surface ENOMEM rather than silently breaking the exec. */
            ret = -ENOMEM;
        }
        raw_syscall6(SYS_munmap, (long)arena_buf, arena_size, 0, 0, 0, 0);

        UC_SET_RET(uc, ret);
        return;
    }

#ifdef SYS_execveat
    if (nr == SYS_execveat) {
        const char *fn = (const char *)a1;
        char **orig_argv = (char **)a2;
        long flags = a4;

#ifdef AT_EMPTY_PATH
        if ((flags & AT_EMPTY_PATH) && fn && fn[0] == '\0') {
            ret = raw_syscall6(SYS_execveat, a0, a1, a2, a3, a4, 0);
            UC_SET_RET(uc, ret);
            return;
        }
#endif
        if (flags != 0) {
            ret = raw_syscall6(SYS_execveat, a0, a1, a2, a3, a4, 0);
            UC_SET_RET(uc, ret);
            return;
        }

        char resolved_fn[PATH_MAX];
        if (!resolve_execveat_path((int)a0, fn, flags,
                                       resolved_fn, sizeof(resolved_fn))) {
            ret = raw_syscall6(SYS_execveat, a0, a1, a2, a3, a4, 0);
            UC_SET_RET(uc, ret);
            return;
        }

        int orig_argc = 0;
        if (orig_argv)
            while (orig_argv[orig_argc]) orig_argc++;

        size_t arena_size = exec_arena_size_for(resolved_fn, orig_argv, orig_argc);
        void *arena_buf = raw_mmap(NULL, arena_size,
                                   PROT_READ | PROT_WRITE,
                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if ((unsigned long)arena_buf >= (unsigned long)-4095) {
            UC_SET_RET(uc, -ENOMEM);
            return;
        }

        struct sud_arena arena;
        sud_arena_init(&arena, arena_buf, arena_size);

        int build_argc = orig_argc > 0 ? orig_argc : 1;
        char **build_argv = sud_arena_alloc(&arena,
                                ((size_t)build_argc + 1) * sizeof(char *));
        char **new_argv = NULL;
        if (build_argv) {
            build_argv[0] = sud_arena_strdup(&arena, resolved_fn);
            for (int i = 1; i < orig_argc; i++)
                build_argv[i] = sud_arena_strdup(&arena, orig_argv[i]);
            build_argv[build_argc] = NULL;

            new_argv = build_exec_argv(&arena, build_argc, build_argv);
        }

        if (new_argv) {
            ret = raw_syscall6(SYS_execve, (long)new_argv[0],
                               (long)new_argv, a3, 0, 0, 0);
        } else {
            /* Arena was sized to fit; NULL here means a sizing bug. */
            ret = -ENOMEM;
        }
        raw_syscall6(SYS_munmap, (long)arena_buf, arena_size, 0, 0, 0, 0);

        UC_SET_RET(uc, ret);
        return;
    }
#endif

    /*
     * clone3/clone — thread/process creation needs special handling.
     *
     * Two cases:
     *
     * A) CLONE_VM with a separate child stack (pthread/clone3 spawn):
     *    The child starts executing inside this handler on a NEW stack.
     *    The compiler's RSP-relative code for local variables (including
     *    `uc`) references wrong addresses on the new stack → SIGSEGV.
     *    Use dedicated assembly (clone3_raw/clone_raw) that saves the uc
     *    pointer in r12, and in the child restores the full program
     *    register state from the ucontext and jumps directly to the
     *    program's saved RIP.  The child never returns to C code here.
     *
     * B) Anything else (fork/vfork without a reusable post-syscall state):
     *    The child can return through the handler normally via
     *    rt_sigreturn, or for clone3 vfork-style spawns we can force
     *    glibc's vfork fallback by reporting ENOSYS.
     */
#ifndef CLONE_VM
#define CLONE_VM 0x00000100
#endif
#ifndef CLONE_VFORK
#define CLONE_VFORK 0x00004000
#endif
#ifndef CLONE_THREAD
#define CLONE_THREAD 0x00010000
#endif
#ifdef SYS_clone3
    if (nr == SYS_clone3) {
        /* clone3: flags are at offset 0, stack at CLONE_ARGS_STACK_OFFSET */
        unsigned long long c3_flags = 0;
        unsigned long long c3_stack = 0;
        if (a0) {
            c3_flags = *(unsigned long long *)a0;
            c3_stack =
                *(unsigned long long *)((char *)a0 + CLONE_ARGS_STACK_OFFSET);
        }
        if ((c3_flags & CLONE_VFORK) && !(c3_flags & CLONE_THREAD)) {
            /* glibc uses clone3(CLONE_VM|CLONE_VFORK|...) for posix_spawn-
             * style exec helpers.  Force its vfork fallback, which returns
             * through code paths that we already reinitialize correctly. */
            UC_SET_RET(uc, -ENOSYS);
            return;
        }
        if ((c3_flags & CLONE_VM) && c3_stack) {
            ret = clone3_raw(a0, a1, uc);
            /* Only parent reaches here; child jumped to program's RIP */
        } else {
            ret = raw_syscall6(nr, a0, a1, a2, a3, a4, a5);
            /* Both parent and child reach here */
            if (ret == 0) {
                /* Child created while handling SIGSYS can inherit blocked
                 * SIGSYS state and stale SUD task config. Reinstall both. */
                prepare_child_sud();
            }
        }
        UC_SET_RET(uc, ret);
        return;
    }
#endif
    if (nr == SYS_clone) {
        /* clone: flags are in a0 (rdi) */
        unsigned long c_flags = (unsigned long)a0;
        if ((c_flags & CLONE_VM) && a1 != 0) {
#if defined(__x86_64__)
            ret = clone_raw(a0, a1, a2, a3, a4, uc);
#else
            ret = clone_raw(a0, a1, a2, a4, a3, uc);
#endif
            /* Only parent reaches here */
        } else {
            ret = raw_syscall6(nr, a0, a1, a2, a3, a4, a5);
            /* Both parent and child reach here */
            if (ret == 0) {
                /* Child created while handling SIGSYS can inherit blocked
                 * SIGSYS state and stale SUD task config. Reinstall both. */
                prepare_child_sud();
            }
        }
        UC_SET_RET(uc, ret);
        return;
    }

#ifdef SYS_vfork
    if (nr == SYS_vfork) {
        /* Real vfork shares the parent's live stack frame. If the child hits
         * another SIGSYS before exec, the kernel can overwrite the parent's
         * suspended signal frame and corrupt its eventual rt_sigreturn state.
         * Emulate vfork with a plain fork so parent/child stacks diverge. */
        ret = raw_syscall6(SYS_clone, SIGCHLD, 0, 0, 0, 0, 0);
        if (ret == 0)
            prepare_child_sud();
        UC_SET_RET(uc, ret);
        return;
    }
#endif
#ifdef SYS_fork
    if (nr == SYS_fork) {
        ret = raw_syscall6(nr, a0, a1, a2, a3, a4, a5);
        if (ret == 0)
            prepare_child_sud();
        UC_SET_RET(uc, ret);
        return;
    }
#endif

#ifdef SYS_readlinkat
    /* Intercept readlinkat to mask /proc/self/exe and /proc/<pid>/exe.
     *
     * When the traced program reads the /proc/self/exe symlink (or the
     * /proc/<pid>/exe variant with its own PID), the kernel returns the
     * path to sudtrace (sud64/sud32).  This confuses programs like Perl
     * that use /proc/self/exe to determine $^X.
     *
     * readlinkat(dirfd=a0, pathname=a1, buf=a2, bufsz=a3) */
    if (nr == SYS_readlinkat && g_target_exe[0] &&
        is_proc_self_exe((const char *)a1)) {
        size_t tlen = strlen(g_target_exe);
        char *obuf = (char *)a2;
        size_t obsz = (size_t)a3;
        if (tlen > obsz) tlen = obsz;
        memcpy(obuf, g_target_exe, tlen);
        UC_SET_RET(uc, (long)tlen);
        return;
    }
#endif

#ifdef SYS_readlink
    /* Also intercept the legacy readlink(pathname=a0, buf=a1, bufsz=a2)
     * syscall.  glibc's readlink() uses this on x86_64. */
    if (nr == SYS_readlink && g_target_exe[0] &&
        is_proc_self_exe((const char *)a0)) {
        size_t tlen = strlen(g_target_exe);
        char *obuf = (char *)a1;
        size_t obsz = (size_t)a2;
        if (tlen > obsz) tlen = obsz;
        memcpy(obuf, g_target_exe, tlen);
        UC_SET_RET(uc, (long)tlen);
        return;
    }
#endif

#ifdef SYS_rt_sigaction
    if (nr == SYS_rt_sigaction) {
        struct kernel_sigaction {
            void (*handler)(int);
            unsigned long flags;
            void (*restorer)(void);
            sud_sigset_word_t mask;
        };
        const struct kernel_sigaction *act =
            (const struct kernel_sigaction *)a1;
        if (a0 == SIGSYS && act) {
            UC_SET_RET(uc, 0);
            return;
        }
        if (act) {
            struct kernel_sigaction patched = *act;
            patched.flags |= SA_RESTORER;
            /* On i386 the kernel selects the signal frame layout (and
             * the matching sigreturn syscall) from SA_SIGINFO.  Without
             * SA_SIGINFO it builds a legacy sigframe which must be
             * unwound via SYS_sigreturn (#119); with SA_SIGINFO it
             * builds a struct rt_sigframe which must be unwound via
             * SYS_rt_sigreturn (#173).  Calling the wrong sigreturn
             * makes the kernel parse the frame at the wrong offsets and
             * load garbage into the user registers (typically EIP=ESP=0
             * with NULL segment selectors → SI_KERNEL SIGSEGV on iret).
             * x86_64 has only the rt flavour. */
#if defined(__i386__)
            if (patched.flags & SA_SIGINFO)
                patched.restorer = sud_rt_sigreturn_restorer;
            else
                patched.restorer = sud_sigreturn_restorer;
#else
            patched.restorer = sud_rt_sigreturn_restorer;
#endif
            ret = raw_syscall6(nr, a0, (long)&patched, a2, a3, a4, a5);
        } else {
            ret = raw_syscall6(nr, a0, a1, a2, a3, a4, a5);
        }
        UC_SET_RET(uc, ret);
        return;
    }
#endif

    /* Execute the real syscall using raw inline assembly.
     *
     * We must NOT use the C library's syscall() wrapper here because it
     * returns -1 on error (setting errno).  The traced program expects the
     * raw kernel return value in RAX (e.g. -ENOSYS, -EPERM).  Using the
     * wrapper would map every error to -1, which breaks glibc internals
     * in the traced program (e.g. clone3→clone fallback in pthread_create).
     *
     * Restore the program's original signal mask for the duration of the
     * syscall.  The SIGSYS handler's sa_mask blocks ALL signals, which
     * prevents SIGCHLD from interrupting blocking syscalls.  Programs
     * like GNU make rely on SIGCHLD to interrupt blocking reads on the
     * jobserver pipe; without this, the build deadlocks: make is stuck
     * in read() waiting for a job token, but can't reap the finished
     * child (whose token it needs) because SIGCHLD never interrupts the
     * read.
     *
     * Restoring the pre-handler mask (from uc->uc_sigmask) allows signals
     * to be delivered during the syscall.  If a signal handler (e.g. for
     * SIGCHLD) makes a syscall, the kernel delivers a nested SIGSYS which
     * re-enters this handler safely — the handler uses only local state
     * and the write lock is not held at this point. */
    {
        sud_sigset_word_t saved_mask;
        sud_sigset_word_t prog_mask;
        memcpy(&prog_mask, &uc->uc_sigmask, sizeof(prog_mask));
        raw_syscall6(SYS_rt_sigprocmask, SIG_SETMASK, (long)&prog_mask,
                     (long)&saved_mask, sizeof(prog_mask), 0, 0);

        ret = raw_syscall6(nr, a0, a1, a2, a3, a4, a5);

        raw_syscall6(SYS_rt_sigprocmask, SIG_SETMASK, (long)&saved_mask,
                     0, sizeof(saved_mask), 0, 0);
    }

    /* Post-syscall tracing */

#ifdef SYS_openat
    if (nr == SYS_openat) {
        const char *path = (const char *)a1;
        emit_open_event(tid, path, (int)a2, ret);
    }
#endif
#ifdef SYS_open
    if (nr == SYS_open) {
        const char *path = (const char *)a0;
        emit_open_event(tid, path, (int)a1, ret);
    }
#endif

#ifdef SYS_unlinkat
    if (nr == SYS_unlinkat && ret == 0) {
        const char *path = (const char *)a1;
        emit_unlink_event(tid, path, ret);
    }
#endif
#ifdef SYS_unlink
    if (nr == SYS_unlink && ret == 0) {
        const char *path = (const char *)a0;
        emit_unlink_event(tid, path, ret);
    }
#endif

#ifdef SYS_chdir
    if (nr == SYS_chdir) {
        if (ret == 0)
            emit_cwd_event(tid);
    }
#endif
#ifdef SYS_fchdir
    if (nr == SYS_fchdir) {
        if (ret == 0)
            emit_cwd_event(tid);
    }
#endif

    if (nr == SYS_write) {
        unsigned int fd = (unsigned int)a0;
        if (ret > 0) {
            if (fd == 2) {
                emit_write_event(tid, "STDERR", (const void *)a1,
                                 (size_t)ret);
            } else if (fd == 1 && fd1_is_creator_stdout(tid)) {
                emit_write_event(tid, "STDOUT", (const void *)a1,
                                 (size_t)ret);
            }
        }
    }

    /* Intercept wait4/waitid to emit EXIT events for reaped children.
     *
     * Without this, child EXIT events would only be emitted from
     * the wrapper / normal-mode parent wait loops.  Now that wrapper
     * mode no longer forks a dedicated wait-loop process, the SIGSYS
     * handler must emit EXIT events when the traced program reaps
     * its children. */
#ifdef SYS_wait4
    if (nr == SYS_wait4 && ret > 0) {
        /* wait4(pid, wstatus_ptr, options, rusage):
         *   ret > 0 → a child was reaped, ret = child pid
         *   a1 = pointer to wstatus in traced program's memory */
        int wstatus = 0;
        if (a1) wstatus = *(int *)a1;
        if (WIFEXITED(wstatus) || WIFSIGNALED(wstatus))
            emit_exit_event((pid_t)ret, wstatus);
    }
#endif
#ifdef SYS_waitid
    if (nr == SYS_waitid && ret == 0) {
        /* waitid(idtype, id, siginfo_ptr, options):
         *   ret == 0 → success; siginfo at a2 has child info */
        if (a2) {
            siginfo_t *si = (siginfo_t *)a2;
            if (si->si_pid > 0 &&
                (si->si_code == CLD_EXITED || si->si_code == CLD_KILLED ||
                 si->si_code == CLD_DUMPED)) {
                int wstatus;
                if (si->si_code == CLD_EXITED)
                    wstatus = si->si_status << 8;
                else {
                    wstatus = si->si_status & 0x7f;
                    if (si->si_code == CLD_DUMPED)
                        wstatus |= 0x80;
                }
                emit_exit_event(si->si_pid, wstatus);
            }
        }
    }
#endif

#ifdef SUDTRACE_SIGSYS_DIAG
    {
        unsigned long sp_now;
#if defined(__x86_64__)
        __asm__ volatile("mov %%rsp, %0" : "=r"(sp_now));
#else
        __asm__ volatile("mov %%esp, %0" : "=r"(sp_now));
#endif
        sigsys_diag_dump("EXIT ", uc, sp_now);
    }
#endif
    UC_SET_RET(uc, ret);
}
