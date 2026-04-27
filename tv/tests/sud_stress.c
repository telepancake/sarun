/*
 * tests/sud_stress.c — freestanding stress harness for sud32/sud64.
 *
 * Compiled with -ffreestanding -nostdlib -static — the same flags used
 * for sud32/sud64 themselves.  No glibc, no musl, no libc at all.
 * Results are therefore independent of which (if any) libc is installed,
 * and the binary can be built for both 32-bit and 64-bit targets without
 * a 32-bit sysroot.
 *
 * Infrastructure re-uses sud/raw.h (raw_syscall6, raw_mmap, raw_open3,
 * raw_write, raw_close) and sud/libc.h (all types and SYS_* aliases).
 * No sud object files are linked.
 *
 * Threads use a raw clone(CLONE_VM|CLONE_THREAD|…) + inline-asm
 * trampoline that works on both i386 and x86_64.  The parent/child
 * split after clone is entirely in assembly: the child pops fn/arg from
 * the pre-set child stack, calls fn(arg), then calls SYS_exit.  The
 * parent returns with the child's TID.  Join uses CLONE_CHILD_CLEARTID
 * + futex(WAIT).
 *
 * Usage:  sud_stress <subtest> [args...]
 *         sud_stress --list
 */

/* ================================================================
 * Includes — only project-internal freestanding headers
 * ================================================================ */
#include "sud/raw.h"   /* raw_syscall6, raw_mmap, raw_write, etc. */
                       /* transitively includes sud/libc.h:         */
                       /*   SYS_* aliases, types, constants          */

/* ================================================================
 * Extra constants not (yet) in sud/libc.h
 * ================================================================ */
/* waitpid / waitid flags */
#define T_WNOHANG    1
#define T_WUNTRACED  2
#define T_WEXITED    4
#define T_WIFSTOPPED(s) (((s) & 0xff) == 0x7f)

/* waitid id-type */
#define T_P_ALL  0
#define T_P_PID  1

/* signals */
#define T_SIGALRM 14
#define T_SIGTRAP  5
#define T_SIGCHLD 17

/* errno values used in tests */
#define T_ECHILD   10
#define T_EAGAIN   11

/* itimer */
#define T_ITIMER_REAL 0
struct t_timeval   { long tv_sec; long tv_usec; };
struct t_itimerval {
    struct t_timeval it_interval;
    struct t_timeval it_value;
};

/* clone flags not in libc.h */
#define T_CLONE_FS          0x00000200UL
#define T_CLONE_FILES       0x00000400UL
#define T_CLONE_SIGHAND     0x00000800UL
#define T_CLONE_SYSVSEM     0x00040000UL
#define T_CLONE_CHILD_CLEARTID 0x00200000UL
#define T_CLONE_CHILD_SETTID   0x01000000UL

/* futex */
#define T_FUTEX_WAIT 0
#define T_FUTEX_WAKE 1

/* ptrace */
#define T_PTRACE_CONT 7

/* ================================================================
 * Required globals (extern-declared in sud/libc.h)
 * ================================================================ */
int    g_errno_value;
char **environ;          /* unused; satisfies extern decl */
void  *stdin  = (void *)0;
void  *stdout = (void *)1;
void  *stderr = (void *)2;

/* Static environment passed to every execve — deterministic, no
 * libc dependency, does not change between test runs or platforms. */
static char *g_env[] = { "PATH=/usr/bin:/bin", NULL };

/* ================================================================
 * _start — identical pattern to sud/libc.c
 * ================================================================ */
int main(int argc, char **argv);
void sudmini_start_c(uintptr_t *sp)
{
    int argc = (int)*sp++;
    char **argv = (char **)sp;
    long ret = main(argc, argv);
    raw_syscall6(SYS_exit_group, ret, 0, 0, 0, 0, 0);
    __builtin_unreachable();
}
#if defined(__x86_64__)
__asm__(
    ".text\n.globl _start\n_start:\n"
    "mov %rsp, %rdi\nandq $-16, %rsp\ncall sudmini_start_c\nhlt\n");
#else
__asm__(
    ".text\n.globl _start\n_start:\n"
    "mov %esp, %eax\nandl $-16, %esp\npush %eax\ncall sudmini_start_c\nhlt\n");
#endif

/* ================================================================
 * String utilities (no libc)
 * ================================================================ */
static size_t t_strlen(const char *s)
{
    size_t n = 0; while (*s++) n++; return n;
}
static int t_strcmp(const char *a, const char *b)
{
    while (*a && *a == *b) { a++; b++; }
    return (unsigned char)*a - (unsigned char)*b;
}
static void *t_memset(void *p, int c, size_t n)
{
    unsigned char *q = p;
    while (n--) *q++ = (unsigned char)c;
    return p;
}
static int t_atoi(const char *s)
{
    int n = 0, neg = 0;
    if (*s == '-') { neg = 1; s++; }
    while (*s >= '0' && *s <= '9') n = n*10 + (*s++ - '0');
    return neg ? -n : n;
}

/* Write unsigned long in decimal into buf (no NUL).  Returns chars written. */
static int t_fmt_ulong(char *buf, int bufsz, unsigned long n)
{
    char tmp[22]; int len = 0;
    if (n == 0) { if (bufsz > 0) buf[0] = '0'; return bufsz > 0 ? 1 : 0; }
    while (n && len < (int)sizeof(tmp)) { tmp[len++] = '0' + (int)(n % 10); n /= 10; }
    if (len > bufsz) len = bufsz;
    for (int i = 0; i < len; i++) buf[i] = tmp[len-1-i];
    return len;
}
static int t_fmt_long(char *buf, int bufsz, long n)
{
    if (n < 0 && bufsz > 1) {
        buf[0] = '-';
        return 1 + t_fmt_ulong(buf+1, bufsz-1, (unsigned long)(-(unsigned long)n));
    }
    return t_fmt_ulong(buf, bufsz, (unsigned long)n);
}

/* ================================================================
 * I/O utilities
 * ================================================================ */
static void t_write_str(int fd, const char *s)
{
    raw_write(fd, s, t_strlen(s));
}

static __attribute__((noreturn)) void t_die_msg(const char *msg, long n, int has_n)
{
    t_write_str(2, "sud_stress: ");
    t_write_str(2, msg);
    if (has_n) {
        char buf[24]; int len = t_fmt_long(buf, sizeof(buf), n);
        raw_write(2, " ", 1);
        raw_write(2, buf, (size_t)len);
    }
    raw_write(2, "\n", 1);
    raw_syscall6(SYS_exit_group, 2, 0, 0, 0, 0, 0);
    __builtin_unreachable();
}

#define t_die(msg)       t_die_msg(msg, 0, 0)
#define t_die2(msg, n)   t_die_msg(msg, (long)(n), 1)

static void t_ok(const char *name)
{
    t_write_str(1, "OK ");
    t_write_str(1, name);
    raw_write(1, "\n", 1);
}

/* ================================================================
 * Memory: mmap-based allocation
 * ================================================================ */
static void *t_alloc(size_t sz)
{
    /* Store size in first sizeof(size_t) bytes so t_free can munmap */
    size_t total = sz + sizeof(size_t);
    void *p = raw_mmap(NULL, total,
                       PROT_READ|PROT_WRITE,
                       MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    if ((unsigned long)p >= (unsigned long)-(unsigned long)4095) t_die("t_alloc: mmap");
    *(size_t *)p = total;
    return (char *)p + sizeof(size_t);
}
static void t_free(void *p)
{
    if (!p) return;
    void *base = (char *)p - sizeof(size_t);
    size_t total = *(size_t *)base;
    raw_syscall6(SYS_munmap, (long)base, (long)total, 0, 0, 0, 0);
}
static void *t_calloc(size_t nmemb, size_t sz)
{
    void *p = t_alloc(nmemb * sz);
    t_memset(p, 0, nmemb * sz);
    return p;
}

/* ================================================================
 * Raw thread creation (CLONE_VM | CLONE_THREAD)
 *
 * The child needs a fresh stack with fn/arg pre-pushed, plus a futex
 * word (th->tid) for the join.  After clone() returns 0 in the child,
 * the child's stack pointer is set to child_sp (we gave to clone).
 * We pop fn and arg from there in the inline asm and call fn(arg),
 * then call SYS_exit(0) to kill the thread.
 *
 * Stack layout built by t_thread_create before calling clone:
 *   [child_sp + 0 * PTR]  = fn
 *   [child_sp + 1 * PTR]  = arg
 *   [child_sp + 2 * PTR]  = 0  (fake return address)
 *
 * th->tid is initialised to 1; kernel writes the real TID on start
 * (CLONE_CHILD_SETTID) and writes 0 + wakes the futex on exit
 * (CLONE_CHILD_CLEARTID).
 * ================================================================ */
#define T_THREAD_STACK (128 * 1024)

struct t_thread {
    volatile int  tid;       /* CLEARTID/SETTID target */
    void         *stack;     /* mmap base */
    size_t        stack_sz;
};

/* Futex wait: block until *addr != val */
static void t_futex_wait_neq(volatile int *addr, int val)
{
    while (*addr == val)
        raw_syscall6(__NR_futex, (long)addr, T_FUTEX_WAIT,
                     (long)val, 0, 0, 0);
}

/*
 * Raw clone helper.  Never inlined so the compiler cannot spill parent-
 * frame locals across the clone boundary (the child sees a different
 * stack entirely).
 */
__attribute__((noinline))
static long t_clone_helper(unsigned long flags, void *child_sp,
                            volatile int *tidp)
{
#if defined(__x86_64__)
    register long _rdi __asm__("rdi") = (long)flags;
    register long _rsi __asm__("rsi") = (long)child_sp;
    register long _rdx __asm__("rdx") = 0L;
    register long _r10 __asm__("r10") = (long)tidp;
    register long _r8  __asm__("r8")  = 0L;   /* tls */
    long ret;
    __asm__ volatile(
        "movl %[nr], %%eax\n\t"
        "syscall\n\t"
        /* rax=0 in child, child_tid in parent */
        "testq %%rax, %%rax\n\t"
        "jnz 1f\n\t"
        /* === child path: rsp = child_sp === */
        /* [rsp]=fn  [rsp+8]=arg */
        "popq %%rax\n\t"            /* fn  */
        "popq %%rdi\n\t"            /* arg */
        "andq $-16, %%rsp\n\t"      /* align before call */
        "callq *%%rax\n\t"
        /* thread fn returned — exit the thread */
        "xorl %%edi, %%edi\n\t"
        "movl %[exit_nr], %%eax\n\t"
        "syscall\n\t"
        "ud2\n\t"
        "1:\n\t"
        : "=a"(ret)
        : [nr]      "i"(__NR_clone),
          [exit_nr] "i"(__NR_exit),
          "r"(_rdi), "r"(_rsi), "r"(_rdx), "r"(_r10), "r"(_r8)
        : "rcx", "r11", "memory");
    return ret;
#else
    /*
     * i386: clone(flags, stack, parent_tid, newtls, child_tid)
     * eax=NR  ebx=flags  ecx=stack  edx=parent_tid  esi=newtls  edi=child_tid
     */
    long ret;
    __asm__ volatile(
        "int $0x80\n\t"
        "testl %%eax, %%eax\n\t"
        "jnz 1f\n\t"
        /* === child path: esp = child_sp === */
        /* [esp]=fn  [esp+4]=arg */
        "popl %%eax\n\t"            /* fn  */
        "popl %%ecx\n\t"            /* arg */
        "andl $-16, %%esp\n\t"
        "subl $12, %%esp\n\t"       /* cdecl: 12 pad + 4 arg = 16-aligned before call */
        "pushl %%ecx\n\t"           /* arg */
        "call *%%eax\n\t"
        /* thread fn returned — exit the thread */
        "xorl %%ebx, %%ebx\n\t"
        "movl %[exit_nr], %%eax\n\t"
        "int $0x80\n\t"
        "ud2\n\t"
        "1:\n\t"
        : "=a"(ret)
        : "a"(__NR_clone),
          "b"((long)flags),
          "c"(child_sp),
          "d"(0L),       /* parent_tidptr = NULL */
          "S"(0L),       /* newtls = 0 */
          "D"(tidp),     /* child_tidptr */
          [exit_nr] "i"(__NR_exit)
        : "memory");
    return ret;
#endif
}

static int t_thread_create(struct t_thread *th,
                            void (*fn)(void *), void *arg)
{
    size_t stack_sz = T_THREAD_STACK;
    void *stack = raw_mmap(NULL, stack_sz,
                           PROT_READ|PROT_WRITE,
                           MAP_PRIVATE|MAP_ANONYMOUS|MAP_STACK, -1, 0);
    if ((unsigned long)stack >= (unsigned long)-(unsigned long)4095) return -1;

    th->stack    = stack;
    th->stack_sz = stack_sz;
    th->tid      = 1;   /* sentinel; kernel overwrites via SETTID */

    /* Pre-push fn/arg on child's stack top */
    void **sp = (void **)((char *)stack + stack_sz);
    *--sp = (void *)0;          /* fake return address */
    *--sp = arg;
    *--sp = (void *)fn;

    unsigned long flags =
        CLONE_VM | T_CLONE_FS | T_CLONE_FILES | T_CLONE_SIGHAND |
        CLONE_THREAD | T_CLONE_SYSVSEM |
        T_CLONE_CHILD_CLEARTID | T_CLONE_CHILD_SETTID;

    long ret = t_clone_helper(flags, sp, &th->tid);
    if (ret < 0) {
        raw_syscall6(SYS_munmap, (long)stack, (long)stack_sz, 0, 0, 0, 0);
        return (int)ret;
    }
    return 0;
}

static void t_thread_join(struct t_thread *th)
{
    /* Wait until CLEARTID writes 0 */
    while (th->tid != 0) {
        int cur = th->tid;
        if (cur == 0) break;
        raw_syscall6(__NR_futex, (long)&th->tid,
                     T_FUTEX_WAIT, (long)cur, 0, 0, 0);
    }
    raw_syscall6(SYS_munmap, (long)th->stack, (long)th->stack_sz,
                 0, 0, 0, 0);
}

/* ================================================================
 * Signal handler for the signal-storm subtest.
 * Uses raw syscalls only — safe to call from any signal handler.
 * ================================================================ */
static __attribute__((naked)) void t_sigreturn_restorer(void)
{
#if defined(__x86_64__)
    __asm__ volatile("movl %0, %%eax\nsyscall" :: "i"(__NR_rt_sigreturn) : "rax","rcx","r11","memory");
#else
    __asm__ volatile("movl %0, %%eax\nint $0x80" :: "i"(__NR_rt_sigreturn) : "eax","memory");
#endif
}

/* Kernel-compatible sigaction (matches sud/libc.c struct kernel_sigaction) */
struct t_kern_sigact {
    void (*handler)(int);
    unsigned long flags;
    void (*restorer)(void);
    sud_sigset_word_t mask;   /* 8 bytes on both i386 and x86_64 */
};

static void t_install_handler(int sig, void (*handler)(int))
{
    struct t_kern_sigact ksa;
    t_memset(&ksa, 0, sizeof(ksa));
    ksa.handler  = handler;
    ksa.restorer = t_sigreturn_restorer;
    /* SA_SIGINFO: always use rt_sigframe so SA_RESTORER + rt_sigreturn works
     * on both i386 and x86_64.  Without SA_SIGINFO on i386 the kernel uses
     * the old sigframe and the restorer must call SYS_sigreturn, not
     * SYS_rt_sigreturn — causing the SI_KERNEL SIGSEGV seen in testing. */
    ksa.flags = SA_RESTART | SA_RESTORER | SA_SIGINFO;
    ksa.mask  = 0;
    raw_syscall6(SYS_rt_sigaction, (long)sig, (long)&ksa, 0,
                 sizeof(ksa.mask), 0, 0);
}

static void t_sigalrm_busy(int sig)
{
    (void)sig;
    /* Make a few syscalls so signal-during-handler races have a target. */
    int fd = raw_open3("/dev/null", O_WRONLY, 0);
    if (fd >= 0) {
        raw_write(fd, "x", 1);
        raw_close(fd);
    }
}

/* ================================================================
 * Temporary-file path building for shebang-chain
 * ================================================================ */
static void t_build_shebang_path(char *buf, int bufsz, long pid, int idx)
{
    int p = 0;
    const char *base = "/tmp/sb_";
    while (*base && p < bufsz-1) buf[p++] = *base++;
    p += t_fmt_long(buf+p, bufsz-p-8, pid);
    if (p < bufsz-1) buf[p++] = '_';
    p += t_fmt_long(buf+p, bufsz-p-5, (long)idx);
    if (p+3 < bufsz) { buf[p]='.'; buf[p+1]='s'; buf[p+2]='h'; p+=3; }
    if (p < bufsz) buf[p] = '\0';
}

static void t_write_shebang_file(const char *path, const char *interp)
{
    int fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)path,
                               O_WRONLY|O_CREAT|O_TRUNC, 0755, 0, 0);
    if (fd < 0) t_die("open shebang file");
    raw_write(fd, "#!", 2);
    raw_write(fd, interp, t_strlen(interp));
    raw_write(fd, "\nexit 0\n", 8);
    raw_syscall6(__NR_fchmod, (long)fd, 0755, 0, 0, 0, 0);
    raw_close(fd);
}

/* ================================================================
 * Subtest: argv-huge
 * ================================================================ */
static int t_argv_huge(int total_argv_bytes)
{
    if (total_argv_bytes <= 0) total_argv_bytes = 96 * 1024;
    int per = 64;
    int n = total_argv_bytes / per;
    if (n < 8) n = 8;

    char **argv = t_calloc((size_t)n + 2, sizeof(char *));
    argv[0] = (char *)"/bin/true";
    for (int i = 1; i <= n; i++) {
        char *s = t_alloc((size_t)per + 1);
        t_memset(s, 'A' + (i % 26), (size_t)per);
        s[per] = '\0';
        argv[i] = s;
    }
    argv[n+1] = NULL;

    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) t_die("fork");
    if (pid == 0) {
        raw_syscall6(SYS_execve, (long)"/bin/true",
                     (long)argv, (long)g_env, 0, 0, 0);
        raw_syscall6(SYS_exit, 127, 0, 0, 0, 0, 0);
        __builtin_unreachable();
    }
    int st = 0;
    if (raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0) != pid)
        t_die("wait");
    if (!WIFEXITED(st))
        t_die2("argv-huge: child died, status", (long)st);
    if (WEXITSTATUS(st) != 0)
        t_die2("argv-huge: /bin/true returned", (long)WEXITSTATUS(st));

    /* free argv entries */
    for (int i = 1; i <= n; i++) t_free(argv[i]);
    t_free(argv);

    t_ok("argv-huge");
    return 0;
}

/* ================================================================
 * Subtest: argv-near-argmax
 * ================================================================ */
static int t_argv_near_argmax(void)
{
    int total_argv_bytes = 1536 * 1024;
    int per = 64;
    int n = total_argv_bytes / per;
    if (n < 8) n = 8;

    char **argv = t_calloc((size_t)n + 2, sizeof(char *));
    argv[0] = (char *)"/bin/true";
    for (int i = 1; i <= n; i++) {
        char *s = t_alloc((size_t)per + 1);
        t_memset(s, 'A' + (i % 26), (size_t)per);
        s[per] = '\0';
        argv[i] = s;
    }
    argv[n+1] = NULL;

    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) t_die("fork");
    if (pid == 0) {
        raw_syscall6(SYS_execve, (long)"/bin/true",
                     (long)argv, (long)g_env, 0, 0, 0);
        raw_syscall6(SYS_exit, 127, 0, 0, 0, 0, 0);
        __builtin_unreachable();
    }
    int st = 0;
    if (raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0) != pid)
        t_die("wait");
    if (!WIFEXITED(st))
        t_die2("argv-near-argmax: child died, status", (long)st);
    if (WEXITSTATUS(st) != 0)
        t_die2("argv-near-argmax: /bin/true returned", (long)WEXITSTATUS(st));

    for (int i = 1; i <= n; i++) t_free(argv[i]);
    t_free(argv);

    t_ok("argv-near-argmax");
    return 0;
}

/* ================================================================
 * Subtest: argv-single-huge
 * ================================================================ */
static int t_argv_single_huge(void)
{
    size_t big_sz = 1 * 1024 * 1024;
    char *big = t_alloc(big_sz + 1);
    t_memset(big, 'X', big_sz);
    big[big_sz] = '\0';

    char *argv[] = { (char *)"/bin/true", big, NULL };

    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) t_die("fork");
    if (pid == 0) {
        raw_syscall6(SYS_execve, (long)"/bin/true",
                     (long)argv, (long)g_env, 0, 0, 0);
        raw_syscall6(SYS_exit, 126, 0, 0, 0, 0, 0);
        __builtin_unreachable();
    }
    int st = 0;
    raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0);
    if (!WIFEXITED(st))
        t_die2("argv-single-huge: child died, status", (long)st);

    t_free(big);
    t_ok("argv-single-huge");
    return 0;
}

/* ================================================================
 * Subtest: shebang-chain
 * ================================================================ */
static int t_shebang_chain(int depth)
{
    if (depth <= 0) depth = 12;
    if (depth > 14) depth = 14;

    long pid_self = raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
    char paths[15][512];
    char prev[512];
    /* Copy "/bin/sh" into prev */
    const char *sh = "/bin/sh";
    int k = 0; while (sh[k] && k < (int)sizeof(prev)-1) { prev[k]=sh[k]; k++; } prev[k]='\0';

    for (int i = 0; i < depth; i++) {
        t_build_shebang_path(paths[i], sizeof(paths[i]), pid_self, i);
        t_write_shebang_file(paths[i], prev);
        int j = 0;
        while (paths[i][j] && j < (int)sizeof(prev)-1) { prev[j]=paths[i][j]; j++; }
        prev[j] = '\0';
    }

    /* Run the deepest script with a beefy argv */
    char *argv[66];
    int ai = 0;
    argv[ai++] = prev;
    for (int i = 0; i < 32; i++) {
        char *s = t_alloc(512);
        t_memset(s, 'a' + (i % 26), 511);
        s[511] = '\0';
        argv[ai++] = s;
    }
    argv[ai] = NULL;

    long cpid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (cpid < 0) t_die("shebang fork");
    if (cpid == 0) {
        raw_syscall6(SYS_execve, (long)prev, (long)argv, (long)g_env, 0, 0, 0);
        raw_syscall6(SYS_exit, 127, 0, 0, 0, 0, 0);
        __builtin_unreachable();
    }
    int st = 0;
    raw_syscall6(SYS_wait4, (long)cpid, (long)&st, 0, 0, 0, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
        t_die2("shebang-chain: bad status", (long)st);

    /* cleanup temp files */
    for (int i = 0; i < depth; i++)
        raw_syscall6(__NR_unlinkat, AT_FDCWD, (long)paths[i], 0, 0, 0, 0);

    /* free argv padding */
    for (int i = 1; i < ai; i++) t_free(argv[i]);

    t_ok("shebang-chain");
    return 0;
}

/* ================================================================
 * Subtest: thread-exec-storm
 *
 * T CLONE_VM threads each do M cycles of fork+execve(/bin/true).
 * Exercises concurrent SIGSYS handlers sharing address space.
 * ================================================================ */
struct exec_storm_arg { int iters; volatile int quit; };

static void exec_storm_thread_fn(void *p)
{
    struct exec_storm_arg *a = (struct exec_storm_arg *)p;
    for (int i = 0; i < a->iters && !a->quit; i++) {
        long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
        if (pid < 0) t_die("storm fork");
        if (pid == 0) {
            char *av[] = { (char *)"/bin/true", NULL };
            raw_syscall6(SYS_execve, (long)"/bin/true",
                         (long)av, (long)g_env, 0, 0, 0);
            raw_syscall6(SYS_exit, 127, 0, 0, 0, 0, 0);
            __builtin_unreachable();
        }
        int st = 0;
        raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
            t_die2("storm: bad status", (long)st);
    }
}

static int t_thread_exec_storm(int threads, int iters)
{
    if (threads <= 0) threads = 8;
    if (iters   <= 0) iters   = 50;

    struct exec_storm_arg *args = t_calloc((size_t)threads,
                                           sizeof(struct exec_storm_arg));
    struct t_thread *th = t_calloc((size_t)threads, sizeof(struct t_thread));

    for (int i = 0; i < threads; i++) {
        args[i].iters = iters;
        args[i].quit  = 0;
        if (t_thread_create(&th[i], exec_storm_thread_fn, &args[i]) != 0)
            t_die("thread create");
    }
    for (int i = 0; i < threads; i++)
        t_thread_join(&th[i]);

    t_free(args);
    t_free(th);
    t_ok("thread-exec-storm");
    return 0;
}

/* ================================================================
 * Subtest: posix-spawn-storm
 *
 * T CLONE_VM threads each do M cycles of
 * clone(CLONE_VM|CLONE_VFORK)+execve — the same code path that
 * glibc's posix_spawn() uses (without depending on glibc).
 * ================================================================ */
struct spawn_storm_arg { int iters; };

/*
 * Mimic posix_spawn: clone(CLONE_VM|CLONE_VFORK|SIGCHLD), child execs,
 * parent waits.  Uses SYS_vfork for simplicity (same semantics).
 */
static long t_vfork_exec(char **argv)
{
    /* SYS_vfork: parent blocks until child execs or exits */
    volatile long pid = raw_syscall6(SYS_vfork, 0, 0, 0, 0, 0, 0);
    if (pid == 0) {
        raw_syscall6(SYS_execve, (long)argv[0], (long)argv, (long)g_env, 0, 0, 0);
        raw_syscall6(SYS_exit, 127, 0, 0, 0, 0, 0);
        __builtin_unreachable();
    }
    return pid;
}

static void spawn_storm_thread_fn(void *p)
{
    struct spawn_storm_arg *a = (struct spawn_storm_arg *)p;
    char *av[] = { (char *)"/bin/true", NULL };
    for (int i = 0; i < a->iters; i++) {
        long pid = t_vfork_exec(av);
        if (pid < 0) t_die("vfork exec failed");
        int st = 0;
        raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
            t_die2("spawn: bad status", (long)st);
    }
}

static int t_posix_spawn_storm(int threads, int iters)
{
    if (threads <= 0) threads = 8;
    if (iters   <= 0) iters   = 50;

    struct spawn_storm_arg *args = t_calloc((size_t)threads,
                                            sizeof(struct spawn_storm_arg));
    struct t_thread *th = t_calloc((size_t)threads, sizeof(struct t_thread));

    for (int i = 0; i < threads; i++) {
        args[i].iters = iters;
        if (t_thread_create(&th[i], spawn_storm_thread_fn, &args[i]) != 0)
            t_die("thread create");
    }
    for (int i = 0; i < threads; i++)
        t_thread_join(&th[i]);

    t_free(args);
    t_free(th);
    t_ok("posix-spawn-storm");
    return 0;
}

/* ================================================================
 * Subtest: vfork-exec-loop
 * ================================================================ */
static int t_vfork_exec_loop(int iters_in)
{
    int iters = iters_in <= 0 ? 200 : iters_in;
    char *av[] = { (char *)"/bin/true", NULL };
    for (int i = 0; i < iters; i++) {
        long pid = t_vfork_exec(av);
        if (pid < 0) t_die("vfork");
        int st = 0;
        if (raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0) != pid)
            t_die("vfork wait");
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
            t_die2("vfork: bad status", (long)st);
    }
    t_ok("vfork-exec-loop");
    return 0;
}

/* ================================================================
 * Subtest: signal-storm
 *
 * SIGALRM fires every 1 ms.  T CLONE_VM threads fork+exec in tight
 * loops.  SIGALRM lands while some threads are inside the SIGSYS
 * handler; if the handler mismanages the signal frame or stack the
 * process crashes.
 * ================================================================ */
static volatile int g_signal_storm_quit;

struct signal_storm_arg { int iters; };

static void sigstorm_thread_fn(void *p)
{
    struct signal_storm_arg *a = (struct signal_storm_arg *)p;
    char *av[] = { (char *)"/bin/true", NULL };
    for (int i = 0; i < a->iters && !g_signal_storm_quit; i++) {
        long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
        if (pid < 0) t_die("sigstorm fork");
        if (pid == 0) {
            raw_syscall6(SYS_execve, (long)"/bin/true",
                         (long)av, (long)g_env, 0, 0, 0);
            raw_syscall6(SYS_exit, 127, 0, 0, 0, 0, 0);
            __builtin_unreachable();
        }
        int st = 0;
        raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
            t_die2("sigstorm: bad status", (long)st);
    }
}

static int t_signal_storm(int threads, int iters)
{
    if (threads <= 0) threads = 6;
    if (iters   <= 0) iters   = 40;

    t_install_handler(T_SIGALRM, t_sigalrm_busy);

    struct t_itimerval it;
    it.it_interval.tv_sec  = 0; it.it_interval.tv_usec = 1000;
    it.it_value.tv_sec     = 0; it.it_value.tv_usec    = 1000;
    raw_syscall6(__NR_setitimer, T_ITIMER_REAL, (long)&it, 0, 0, 0, 0);

    struct signal_storm_arg *args = t_calloc((size_t)threads,
                                             sizeof(struct signal_storm_arg));
    struct t_thread *th = t_calloc((size_t)threads, sizeof(struct t_thread));

    for (int i = 0; i < threads; i++) {
        args[i].iters = iters;
        if (t_thread_create(&th[i], sigstorm_thread_fn, &args[i]) != 0)
            t_die("thread create");
    }
    for (int i = 0; i < threads; i++)
        t_thread_join(&th[i]);

    g_signal_storm_quit = 1;
    struct t_itimerval off;
    t_memset(&off, 0, sizeof(off));
    raw_syscall6(__NR_setitimer, T_ITIMER_REAL, (long)&off, 0, 0, 0, 0);

    t_free(args);
    t_free(th);
    t_ok("signal-storm");
    return 0;
}

/* ================================================================
 * Subtest: sigchld-spawn
 *
 * Regression test for the i386 sigreturn-mismatch crash.
 *
 * The trigger is a user signal handler installed *without* SA_SIGINFO
 * — typically a SIGCHLD handler — combined with a child process that
 * exits and induces signal delivery to the parent.  On i386 the kernel
 * builds the legacy struct sigframe for non-SA_SIGINFO handlers, which
 * must be unwound through SYS_sigreturn (#119).  A buggy SUD handler
 * that always patches the user's restorer to one calling
 * SYS_rt_sigreturn (#173) makes the kernel parse the frame at the
 * wrong offsets on return and load garbage into the user registers,
 * yielding an instant SI_KERNEL SIGSEGV (typically EIP = ESP = 0).
 *
 * The repro is bog-standard: install a no-op SIGCHLD handler with no
 * extra flags, then vfork+execve+wait4 a few hundred times.
 * ================================================================ */
static void t_sigchld_noop(int sig)
{
    (void)sig;
}

static int t_sigchld_spawn(int iters_in)
{
    int iters = iters_in <= 0 ? 200 : iters_in;

    /* Install a SIGCHLD handler WITHOUT SA_SIGINFO — this is what
     * a typical libc-style sigaction(SIGCHLD, &{.sa_handler=fn}, NULL)
     * call ends up sending to the kernel.  We must not force
     * SA_SIGINFO here or the bug we're guarding against won't trigger.
     *
     * Note: the .restorer we pass is irrelevant — SUD's rt_sigaction
     * interceptor always patches the restorer.  What matters is that
     * SA_SIGINFO is *unset* so that the SUD interceptor selects the
     * legacy-frame restorer (which calls SYS_sigreturn).  If SUD
     * incorrectly selects the rt restorer, the kernel will iret with
     * garbage registers on the *first* SIGCHLD delivery and crash. */
    struct t_kern_sigact ksa;
    t_memset(&ksa, 0, sizeof(ksa));
    ksa.handler  = t_sigchld_noop;
    ksa.restorer = t_sigreturn_restorer;
    ksa.flags    = SA_RESTART | SA_RESTORER;   /* deliberately no SA_SIGINFO */
    ksa.mask     = 0;
    raw_syscall6(SYS_rt_sigaction, T_SIGCHLD, (long)&ksa, 0,
                 sizeof(ksa.mask), 0, 0);

    char *av[] = { (char *)"/bin/true", NULL };
    for (int i = 0; i < iters; i++) {
        long pid = t_vfork_exec(av);
        if (pid < 0) t_die("sigchld-spawn vfork");
        int st = 0;
        if (raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0) != pid)
            t_die("sigchld-spawn wait");
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
            t_die2("sigchld-spawn: bad status", (long)st);
    }

    /* Restore SIGCHLD to default to leave the process clean. */
    t_memset(&ksa, 0, sizeof(ksa));
    ksa.flags = SA_RESTORER;
    ksa.restorer = t_sigreturn_restorer;
    raw_syscall6(SYS_rt_sigaction, T_SIGCHLD, (long)&ksa, 0,
                 sizeof(ksa.mask), 0, 0);

    t_ok("sigchld-spawn");
    return 0;
}

/* ================================================================
 * Subtest: ptrace-traceme
 * ================================================================ */
static int t_ptrace_traceme(void)
{
    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) t_die("fork");
    if (pid == 0) {
        long r = raw_syscall6(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0, 0, 0);
        if (r != 0) {
            raw_syscall6(SYS_exit, 77, 0, 0, 0, 0, 0);
            __builtin_unreachable();
        }
        /* raise(SIGSTOP) via kill(getpid(), SIGSTOP) */
        long self = raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
        raw_syscall6(SYS_kill, self, SIGSTOP, 0, 0, 0, 0);
        /* exec after parent continues us */
        char *av[] = { (char *)"/bin/true", NULL };
        raw_syscall6(SYS_execve, (long)"/bin/true",
                     (long)av, (long)g_env, 0, 0, 0);
        raw_syscall6(SYS_exit, 127, 0, 0, 0, 0, 0);
        __builtin_unreachable();
    }

    int st = 0;
    raw_syscall6(SYS_wait4, (long)pid, (long)&st, T_WUNTRACED, 0, 0, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 77) {
        t_write_str(1, "SKIP ptrace-traceme (no PTRACE)\n");
        return 0;
    }
    if (!T_WIFSTOPPED(st)) t_die2("traceme: not stopped", (long)st);

    raw_syscall6(SYS_ptrace, T_PTRACE_CONT, pid, 0, 0, 0, 0);
    for (;;) {
        long w = raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0);
        if (w <= 0) break;
        if (WIFEXITED(st) || WIFSIGNALED(st)) break;
        raw_syscall6(SYS_ptrace, T_PTRACE_CONT, pid, 0, 0, 0, 0);
    }
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0)
        t_die2("traceme: bad final status", (long)st);

    t_ok("ptrace-traceme");
    return 0;
}

/* ================================================================
 * Subtest: execve-null
 * ================================================================ */
static int t_execve_null(void)
{
    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) t_die("fork");
    if (pid == 0) {
        char *av[] = { NULL };
        /* Raw syscall — bypasses any libc checks */
        raw_syscall6(SYS_execve, 0L, (long)av, (long)g_env, 0, 0, 0);
        raw_syscall6(SYS_exit, 99, 0, 0, 0, 0, 0);
        __builtin_unreachable();
    }
    int st = 0;
    raw_syscall6(SYS_wait4, (long)pid, (long)&st, 0, 0, 0, 0);
    if (!WIFEXITED(st))
        t_die2("execve-null: child died (sud crashed?)", (long)st);
    if (WEXITSTATUS(st) != 99)
        t_die2("execve-null: unexpected exit", (long)WEXITSTATUS(st));
    t_ok("execve-null");
    return 0;
}

/* ================================================================
 * Subtest: waitid-tight
 * ================================================================ */
static int t_waitid_tight(int rounds)
{
    if (rounds <= 0) rounds = 200;
    for (int r = 0; r < rounds; r++) {
        long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
        if (pid < 0) t_die("waitid fork");
        if (pid == 0) {
            raw_syscall6(SYS_exit, 0, 0, 0, 0, 0, 0);
            __builtin_unreachable();
        }
        siginfo_t si;
        t_memset(&si, 0, sizeof(si));
        for (;;) {
            long rc = raw_syscall6(SYS_waitid,
                                   T_P_PID, (long)pid,
                                   (long)&si,
                                   T_WEXITED | T_WNOHANG,
                                   0, 0);
            if (rc == 0 && si.si_pid == (int)pid) break;
            if (rc == -(long)T_ECHILD) break;
            if (rc < 0 && rc != -(long)T_EAGAIN) break;
        }
    }
    t_ok("waitid-tight");
    return 0;
}

/* ================================================================
 * Dispatcher
 * ================================================================ */
static void list_tests(void)
{
    static const char *names[] = {
        "argv-huge [bytes]",
        "argv-near-argmax",
        "argv-single-huge",
        "shebang-chain [depth]",
        "thread-exec-storm [threads] [iters]",
        "posix-spawn-storm [threads] [iters]",
        "vfork-exec-loop [iters]",
        "signal-storm [threads] [iters]",
        "sigchld-spawn [iters]",
        "ptrace-traceme",
        "execve-null",
        "waitid-tight [rounds]",
        NULL
    };
    for (const char **p = names; *p; p++) {
        t_write_str(1, *p);
        raw_write(1, "\n", 1);
    }
}

int main(int argc, char **argv)
{
    if (argc < 2 || t_strcmp(argv[1], "--list") == 0) {
        list_tests();
        return argc < 2 ? 2 : 0;
    }
    const char *t = argv[1];
    if (t_strcmp(t, "argv-huge") == 0)
        return t_argv_huge(argc > 2 ? t_atoi(argv[2]) : 0);
    if (t_strcmp(t, "argv-near-argmax") == 0)
        return t_argv_near_argmax();
    if (t_strcmp(t, "argv-single-huge") == 0)
        return t_argv_single_huge();
    if (t_strcmp(t, "shebang-chain") == 0)
        return t_shebang_chain(argc > 2 ? t_atoi(argv[2]) : 0);
    if (t_strcmp(t, "thread-exec-storm") == 0)
        return t_thread_exec_storm(argc > 2 ? t_atoi(argv[2]) : 0,
                                   argc > 3 ? t_atoi(argv[3]) : 0);
    if (t_strcmp(t, "posix-spawn-storm") == 0)
        return t_posix_spawn_storm(argc > 2 ? t_atoi(argv[2]) : 0,
                                   argc > 3 ? t_atoi(argv[3]) : 0);
    if (t_strcmp(t, "vfork-exec-loop") == 0)
        return t_vfork_exec_loop(argc > 2 ? t_atoi(argv[2]) : 0);
    if (t_strcmp(t, "signal-storm") == 0)
        return t_signal_storm(argc > 2 ? t_atoi(argv[2]) : 0,
                              argc > 3 ? t_atoi(argv[3]) : 0);
    if (t_strcmp(t, "sigchld-spawn") == 0)
        return t_sigchld_spawn(argc > 2 ? t_atoi(argv[2]) : 0);
    if (t_strcmp(t, "ptrace-traceme") == 0)
        return t_ptrace_traceme();
    if (t_strcmp(t, "execve-null") == 0)
        return t_execve_null();
    if (t_strcmp(t, "waitid-tight") == 0)
        return t_waitid_tight(argc > 2 ? t_atoi(argv[2]) : 0);

    t_write_str(2, "unknown subtest: ");
    t_write_str(2, t);
    raw_write(2, "\n", 1);
    list_tests();
    return 2;
}
