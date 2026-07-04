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
    void (*handler)(int);
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
 * Function declarations
 * ================================================================ */
void install_sigsys_handler_raw(void);
void reset_sigmask_raw(void);
void reenable_sud_in_child(void);
void ensure_sud_altstack(void);
void prepare_child_sud(void);
void sigsys_handler(int sig, siginfo_t *info, void *uctx_raw);

#endif /* SUD_HANDLER_H */
