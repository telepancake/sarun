/*
 * sud/handler.h — SIGSYS signal handler and SUD setup for sudtrace.
 *
 * Declares the core signal handler that intercepts all syscalls via
 * Syscall User Dispatch, plus the helper functions for installing
 * the handler and preparing child processes.
 */

#ifndef SUD_HANDLER_H
#define SUD_HANDLER_H

#include "libc-fs/libc.h"

/* ================================================================
 * SUD selector globals
 *
 * The selector byte controls whether the kernel delivers SIGSYS for
 * syscalls outside the allowed IP range.  It lives in a dedicated
 * mmap page so it survives the loaded binary's glibc TLS
 * re-initialisation.
 * ================================================================ */
extern volatile unsigned char  sud_selector_storage;
extern volatile unsigned char *g_sud_selector_ptr;

#define sud_selector (*g_sud_selector_ptr)

/* ================================================================
 * kernel_sigaction_raw — raw kernel sigaction structure.
 *
 * This matches the kernel's struct sigaction layout (not glibc's),
 * used with the raw rt_sigaction syscall.
 * ================================================================ */
struct kernel_sigaction_raw {
    void (*handler)(int, siginfo_t *, void *);
    unsigned long flags;
    void (*restorer)(void);
    sud_sigset_word_t mask;
};

/* ================================================================
 * UC_* macros — access ucontext register state by architecture.
 * ================================================================ */
#if defined(__x86_64__)
#define UC_SYSCALL_NR(uc) ((long)(uc)->uc_mcontext.gregs[REG_RAX])
#define UC_ARG0(uc) ((long)(uc)->uc_mcontext.gregs[REG_RDI])
#define UC_ARG1(uc) ((long)(uc)->uc_mcontext.gregs[REG_RSI])
#define UC_ARG2(uc) ((long)(uc)->uc_mcontext.gregs[REG_RDX])
#define UC_ARG3(uc) ((long)(uc)->uc_mcontext.gregs[REG_R10])
#define UC_ARG4(uc) ((long)(uc)->uc_mcontext.gregs[REG_R8])
#define UC_ARG5(uc) ((long)(uc)->uc_mcontext.gregs[REG_R9])
#define UC_SET_RET(uc, v) ((uc)->uc_mcontext.gregs[REG_RAX] = (v))
#define UC_PC(uc) ((unsigned long)(uc)->uc_mcontext.gregs[REG_RIP])
#define UC_SP(uc) ((unsigned long)(uc)->uc_mcontext.gregs[REG_RSP])
#else
#define UC_SYSCALL_NR(uc) ((long)(uc)->uc_mcontext.gregs[REG_EAX])
#define UC_ARG0(uc) ((long)(uc)->uc_mcontext.gregs[REG_EBX])
#define UC_ARG1(uc) ((long)(uc)->uc_mcontext.gregs[REG_ECX])
#define UC_ARG2(uc) ((long)(uc)->uc_mcontext.gregs[REG_EDX])
#define UC_ARG3(uc) ((long)(uc)->uc_mcontext.gregs[REG_ESI])
#define UC_ARG4(uc) ((long)(uc)->uc_mcontext.gregs[REG_EDI])
#define UC_ARG5(uc) ((long)(uc)->uc_mcontext.gregs[REG_EBP])
#define UC_SET_RET(uc, v) ((uc)->uc_mcontext.gregs[REG_EAX] = (v))
#define UC_PC(uc) ((unsigned long)(uc)->uc_mcontext.gregs[REG_EIP])
#define UC_SP(uc) ((unsigned long)(uc)->uc_mcontext.gregs[REG_ESP])
#endif

/* ================================================================
 * Recent-syscalls ring buffer — dumped by the SIGSEGV/SIGBUS crash
 * diagnostic so the post-mortem shows what the program was doing
 * when it crashed. Per-tid lock-free single-writer (each tid only
 * writes its own SIGSYS handler) so the log is coherent without
 * locking; the crash dumper accepts that concurrent writers from
 * other threads may interleave.
 * ================================================================ */
#define SUD_SYSLOG_SIZE 32   /* power of two */
struct sud_syslog_entry {
    long nr;          /* syscall number; -1 = unused slot */
    unsigned long pc; /* PC saved in ucontext at SIGSYS entry */
    long ret;         /* syscall return (negative = -errno); LONG_MIN = no-ret */
    int  tid;         /* kernel tid that recorded this entry */
};

extern struct sud_syslog_entry g_sud_syslog[SUD_SYSLOG_SIZE];
extern volatile unsigned int g_sud_syslog_head;

/* Sentinel for entries whose handler hasn't yet completed (so the
 * syscall return value is unknown). Any real -errno is > -4096, and
 * any positive return is bounded by typical sizes; this value is a
 * deliberately rare bit pattern that cannot occur as a real return. */
#define SUD_SYSLOG_NORETURN ((long)0xDEADBEEFCAFEBABEULL)

/* ================================================================
 * Per-syscall profile: every trapped syscall's handler time (rdtsc
 * cycles) accumulated per syscall nr, per PROCESS (threads share the
 * table via atomic adds). The trace addin ships it as one EV_PROF
 * event at exit_group so a slow box can be diagnosed from its trace
 * — which syscalls burned the time, and how much of it was spent
 * waiting on the trace wire itself (g_sud_prof_wire_cycles) versus
 * doing real interception work. Costs two rdtsc + two atomic adds
 * per trap — noise against the ~1.7µs trap floor.
 * ================================================================ */
#define SUD_PROF_MAX 512   /* [SUD_PROF_MAX] = overflow bucket for nr >= 512 */
struct sud_prof_ent {
    /* _Alignas: the i386 ABI aligns u64 to 4, but the atomic add wants
     * natural (8-byte) alignment on both ELF classes. */
    _Alignas(8) unsigned long long cycles;
    unsigned int count;
    unsigned int pad_;
};
extern struct sud_prof_ent g_sud_prof[SUD_PROF_MAX + 1];
/* Cycles spent inside wire_lock + the trace-pipe write, across all
 * events this process emitted — the backpressure signal: when this
 * dominates, the box is throttled by the ENGINE's reader, not by
 * interception cost. */
extern _Alignas(8) volatile unsigned long long g_sud_prof_wire_cycles;

static inline unsigned long long sud_prof_rdtsc(void)
{
    unsigned int lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return ((unsigned long long)hi << 32) | lo;
}

/* ================================================================
 * Function declarations
 * ================================================================ */
void install_sigsys_handler_raw(void);
void reset_sigmask_raw(void);
void reenable_sud_in_child(void);
void ensure_sud_altstack(void);
void prepare_child_sud(void);
void sigsys_handler(int sig, siginfo_t *info, void *uctx_raw);

#endif /* SUD_HANDLER_H */
