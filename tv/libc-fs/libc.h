/*
 * libc-fs/libc.h — Public header for the libc-fs freestanding mini-libc.
 *
 * libc-fs is a self-contained static library providing just enough
 * libc functionality (types, syscall aliases, string/mem ops, I/O,
 * formatted output, mmap/proc helpers) for code built with
 * `-nostdlib -ffreestanding`.  NO glibc headers are pulled in; only
 * compiler-provided freestanding headers (`stdarg.h`, `stddef.h`,
 * `stdint.h`) and Linux UAPI (`asm/unistd.h`) are used.
 *
 * Consumers should `#include "libc-fs/libc.h"` and link against the
 * archive built by `libc-fs/Makefile` (`libc-fs.a`).
 *
 * `mpaland/printf` is an *internal* implementation detail — its
 * `printf_/snprintf_/vsnprintf_/sprintf_/vprintf_/fctprintf` symbols
 * are re-exported under the standard libc names below.  Callers must
 * never include `deps/printf/printf.h` directly.
 */

#ifndef LIBC_FS_LIBC_H
#define LIBC_FS_LIBC_H

/* ================================================================
 * Compiler-provided freestanding headers (no libc dependency)
 * ================================================================ */
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>

/* ================================================================
 * Linux UAPI headers
 * ================================================================ */
#include <asm/unistd.h>

/* ================================================================
 * SYS_xxx aliases for __NR_xxx
 * ================================================================ */
#define SYS_read           __NR_read
#define SYS_write          __NR_write
#define SYS_writev         __NR_writev
#define SYS_openat         __NR_openat
#define SYS_close          __NR_close
#define SYS_pread64        __NR_pread64
#define SYS_readlinkat     __NR_readlinkat
#define SYS_faccessat      __NR_faccessat
#define SYS_dup3           __NR_dup3
#define SYS_getcwd         __NR_getcwd
#define SYS_munmap         __NR_munmap
#define SYS_mprotect       __NR_mprotect
#define SYS_prctl          __NR_prctl
#define SYS_rt_sigaction   __NR_rt_sigaction
#define SYS_nanosleep      __NR_nanosleep
#define SYS_clone          __NR_clone
#define SYS_execve         __NR_execve
#define SYS_wait4          __NR_wait4
#define SYS_exit           __NR_exit
#define SYS_getpid         __NR_getpid
#define SYS_kill           __NR_kill
#define SYS_gettid         __NR_gettid
#define SYS_clock_gettime  __NR_clock_gettime
#define SYS_rt_sigprocmask __NR_rt_sigprocmask
#define SYS_ptrace         __NR_ptrace
#define SYS_sched_yield    __NR_sched_yield
#define SYS_sigaltstack    __NR_sigaltstack

#ifdef __NR_exit_group
#define SYS_exit_group     __NR_exit_group
#endif
#ifdef __NR_dup2
#define SYS_dup2           __NR_dup2
#endif
#ifdef __NR_fork
#define SYS_fork           __NR_fork
#endif
#ifdef __NR_mmap
#define SYS_mmap           __NR_mmap
#endif
#ifdef __NR_mmap2
#define SYS_mmap2          __NR_mmap2
#endif
#ifdef __NR_fstat
#define SYS_fstat          __NR_fstat
#endif
#ifdef __NR_fstat64
#define SYS_fstat64        __NR_fstat64
#endif
#ifdef __NR_newfstatat
#define SYS_newfstatat     __NR_newfstatat
#endif
#ifdef __NR_fstatat64
#define SYS_fstatat64      __NR_fstatat64
#endif
#ifdef __NR_getdents64
#define SYS_getdents64     __NR_getdents64
#endif
#ifdef __NR_getdents
#define SYS_getdents       __NR_getdents
#endif
#ifdef __NR_seccomp
#define SYS_seccomp        __NR_seccomp
#endif
#ifdef __NR_waitid
#define SYS_waitid         __NR_waitid
#endif
#ifdef __NR_clone3
#define SYS_clone3         __NR_clone3
#endif
#ifdef __NR_execveat
#define SYS_execveat       __NR_execveat
#endif
#ifdef __NR_vfork
#define SYS_vfork          __NR_vfork
#endif
#ifdef __NR_readlink
#define SYS_readlink       __NR_readlink
#endif
#ifdef __NR_open
#define SYS_open           __NR_open
#endif

#ifndef SYS_rt_sigreturn
#define SYS_rt_sigreturn   __NR_rt_sigreturn
#endif

/* Legacy sigreturn (i386 only — x86_64 has no SYS_sigreturn). */
#if defined(__i386__)
#ifndef SYS_sigreturn
#define SYS_sigreturn      __NR_sigreturn
#endif
#endif

/* ----------------------------------------------------------------
 * Path-bearing syscalls used by sud/path_remap and sud/handler.
 * Every consumer should rely on these aliases instead of defining
 * its own local `#ifndef SYS_xxx` fallback — one source of truth.
 * Each is conditional so we silently skip on architectures that
 * don't expose the underlying __NR_xxx (e.g. SYS_open is x86 only,
 * stat64 family is i386 only, etc.).
 * ---------------------------------------------------------------- */
#if !defined(SYS_chdir) && defined(__NR_chdir)
#define SYS_chdir          __NR_chdir
#endif
#if !defined(SYS_fchdir) && defined(__NR_fchdir)
#define SYS_fchdir         __NR_fchdir
#endif
#if !defined(SYS_chroot) && defined(__NR_chroot)
#define SYS_chroot         __NR_chroot
#endif
#if !defined(SYS_unlink) && defined(__NR_unlink)
#define SYS_unlink         __NR_unlink
#endif
#if !defined(SYS_unlinkat) && defined(__NR_unlinkat)
#define SYS_unlinkat       __NR_unlinkat
#endif
#if !defined(SYS_rmdir) && defined(__NR_rmdir)
#define SYS_rmdir          __NR_rmdir
#endif
#if !defined(SYS_mkdir) && defined(__NR_mkdir)
#define SYS_mkdir          __NR_mkdir
#endif
#if !defined(SYS_mkdirat) && defined(__NR_mkdirat)
#define SYS_mkdirat        __NR_mkdirat
#endif
#if !defined(SYS_mknod) && defined(__NR_mknod)
#define SYS_mknod          __NR_mknod
#endif
#if !defined(SYS_mknodat) && defined(__NR_mknodat)
#define SYS_mknodat        __NR_mknodat
#endif
#if !defined(SYS_stat) && defined(__NR_stat)
#define SYS_stat           __NR_stat
#endif
#if !defined(SYS_lstat) && defined(__NR_lstat)
#define SYS_lstat          __NR_lstat
#endif
#if !defined(SYS_stat64) && defined(__NR_stat64)
#define SYS_stat64         __NR_stat64
#endif
#if !defined(SYS_lstat64) && defined(__NR_lstat64)
#define SYS_lstat64        __NR_lstat64
#endif
#if !defined(SYS_statx) && defined(__NR_statx)
#define SYS_statx          __NR_statx
#endif
#if !defined(SYS_access) && defined(__NR_access)
#define SYS_access         __NR_access
#endif
#if !defined(SYS_faccessat2) && defined(__NR_faccessat2)
#define SYS_faccessat2     __NR_faccessat2
#endif
#if !defined(SYS_chmod) && defined(__NR_chmod)
#define SYS_chmod          __NR_chmod
#endif
#if !defined(SYS_fchmodat) && defined(__NR_fchmodat)
#define SYS_fchmodat       __NR_fchmodat
#endif
#if !defined(SYS_chown) && defined(__NR_chown)
#define SYS_chown          __NR_chown
#endif
#if !defined(SYS_chown32) && defined(__NR_chown32)
#define SYS_chown32        __NR_chown32
#endif
#if !defined(SYS_lchown) && defined(__NR_lchown)
#define SYS_lchown         __NR_lchown
#endif
#if !defined(SYS_lchown32) && defined(__NR_lchown32)
#define SYS_lchown32       __NR_lchown32
#endif
#if !defined(SYS_fchownat) && defined(__NR_fchownat)
#define SYS_fchownat       __NR_fchownat
#endif
#if !defined(SYS_link) && defined(__NR_link)
#define SYS_link           __NR_link
#endif
#if !defined(SYS_linkat) && defined(__NR_linkat)
#define SYS_linkat         __NR_linkat
#endif
#if !defined(SYS_symlink) && defined(__NR_symlink)
#define SYS_symlink        __NR_symlink
#endif
#if !defined(SYS_symlinkat) && defined(__NR_symlinkat)
#define SYS_symlinkat      __NR_symlinkat
#endif
#if !defined(SYS_rename) && defined(__NR_rename)
#define SYS_rename         __NR_rename
#endif
#if !defined(SYS_renameat) && defined(__NR_renameat)
#define SYS_renameat       __NR_renameat
#endif
#if !defined(SYS_renameat2) && defined(__NR_renameat2)
#define SYS_renameat2      __NR_renameat2
#endif
#if !defined(SYS_truncate) && defined(__NR_truncate)
#define SYS_truncate       __NR_truncate
#endif
#if !defined(SYS_truncate64) && defined(__NR_truncate64)
#define SYS_truncate64     __NR_truncate64
#endif
#if !defined(SYS_utime) && defined(__NR_utime)
#define SYS_utime          __NR_utime
#endif
#if !defined(SYS_utimes) && defined(__NR_utimes)
#define SYS_utimes         __NR_utimes
#endif
#if !defined(SYS_utimensat) && defined(__NR_utimensat)
#define SYS_utimensat      __NR_utimensat
#endif
#if !defined(SYS_futimesat) && defined(__NR_futimesat)
#define SYS_futimesat      __NR_futimesat
#endif
#if !defined(SYS_openat2) && defined(__NR_openat2)
#define SYS_openat2        __NR_openat2
#endif
#if !defined(SYS_getxattr) && defined(__NR_getxattr)
#define SYS_getxattr       __NR_getxattr
#endif
#if !defined(SYS_lgetxattr) && defined(__NR_lgetxattr)
#define SYS_lgetxattr      __NR_lgetxattr
#endif
#if !defined(SYS_listxattr) && defined(__NR_listxattr)
#define SYS_listxattr      __NR_listxattr
#endif
#if !defined(SYS_llistxattr) && defined(__NR_llistxattr)
#define SYS_llistxattr     __NR_llistxattr
#endif
#if !defined(SYS_setxattr) && defined(__NR_setxattr)
#define SYS_setxattr       __NR_setxattr
#endif
#if !defined(SYS_lsetxattr) && defined(__NR_lsetxattr)
#define SYS_lsetxattr      __NR_lsetxattr
#endif
#if !defined(SYS_removexattr) && defined(__NR_removexattr)
#define SYS_removexattr    __NR_removexattr
#endif
#if !defined(SYS_lremovexattr) && defined(__NR_lremovexattr)
#define SYS_lremovexattr   __NR_lremovexattr
#endif

/* ----------------------------------------------------------------
 * In-RAM filesystem add-in needs futex (cross-process locking),
 * memfd_create (process-local fd cookies for inramfs handles), and
 * ftruncate (resize the shared shm segment on first attach).  These
 * are useful enough on their own to deserve canonical aliases here
 * rather than being redefined in addin code.
 * ---------------------------------------------------------------- */
#if !defined(SYS_futex) && defined(__NR_futex)
#define SYS_futex          __NR_futex
#endif
#if !defined(SYS_memfd_create) && defined(__NR_memfd_create)
#define SYS_memfd_create   __NR_memfd_create
#endif
#if !defined(SYS_ftruncate) && defined(__NR_ftruncate)
#define SYS_ftruncate      __NR_ftruncate
#endif
#if !defined(SYS_ftruncate64) && defined(__NR_ftruncate64)
#define SYS_ftruncate64    __NR_ftruncate64
#endif
#if !defined(SYS_lseek) && defined(__NR_lseek)
#define SYS_lseek          __NR_lseek
#endif
#if !defined(SYS__llseek) && defined(__NR__llseek)
#define SYS__llseek        __NR__llseek
#endif
#if !defined(SYS_pwrite64) && defined(__NR_pwrite64)
#define SYS_pwrite64       __NR_pwrite64
#endif
#if !defined(SYS_fchmod) && defined(__NR_fchmod)
#define SYS_fchmod         __NR_fchmod
#endif
#if !defined(SYS_fchown) && defined(__NR_fchown)
#define SYS_fchown         __NR_fchown
#endif
#if !defined(SYS_getuid) && defined(__NR_getuid)
#define SYS_getuid         __NR_getuid
#endif
#if !defined(SYS_getuid32) && defined(__NR_getuid32)
#define SYS_getuid32       __NR_getuid32
#endif
#if !defined(SYS_getgid) && defined(__NR_getgid)
#define SYS_getgid         __NR_getgid
#endif
#if !defined(SYS_getgid32) && defined(__NR_getgid32)
#define SYS_getgid32       __NR_getgid32
#endif

/* ================================================================
 * POSIX-like types
 * ================================================================ */
typedef long            ssize_t;
typedef int             pid_t;
typedef long            off_t;
typedef unsigned int    useconds_t;
typedef int             clockid_t;

/* ================================================================
 * sigset_t (matches glibc: 1024 bits = 128 bytes)
 * ================================================================ */
#define _NSIG_WORDS (1024 / (8 * sizeof(unsigned long)))
typedef struct { unsigned long __val[_NSIG_WORDS]; } sigset_t;

/* ================================================================
 * struct sigaction — glibc-compatible layout
 * ================================================================ */
struct sigaction {
    union {
        void (*sa_handler)(int);
        /* Second/third args are void* to avoid circular type deps */
        void (*sa_sigaction)(int, void *, void *);
    } __sigaction_handler;
    sigset_t sa_mask;
    int      sa_flags;
    void   (*sa_restorer)(void);
};
#define sa_handler __sigaction_handler.sa_handler

/* ================================================================
 * Time structures
 * ================================================================ */
struct timespec {
    long tv_sec;
    long tv_nsec;
};

/* ================================================================
 * Stat — forward-declared (only used as opaque pointer in syscall
 * wrappers; callers use stat_buf_t for actual buffers)
 * ================================================================ */
struct stat;

/* ================================================================
 * Directory entry and DIR
 * ================================================================ */
struct dirent {
    unsigned long  d_ino;
    long           d_off;
    unsigned short d_reclen;
    unsigned char  d_type;
    char           d_name[256];
};

typedef struct __dirstream DIR;

/* ================================================================
 * siginfo_t — minimal definition with the fields sudtrace uses
 * ================================================================ */
typedef struct {
    int si_signo;
    int si_errno;
    int si_code;
    /* _sifields starts immediately at offset 12 — no padding.
     * The kernel's siginfo_t (and compat_siginfo) has the union at
     * offset 12; the earlier _pad0 was wrong and shifted all union
     * members by 4 bytes, breaking si_pid/si_status for waitid. */
    union {
        int _pad[29];    /* total size matches kernel's siginfo_t (128 bytes) */
        struct {
            pid_t si_pid;
            unsigned int si_uid;
            int si_status;
        } _sigchld;
        struct {
            void *si_addr;   /* fault address (SIGSEGV, SIGBUS) */
        } _sigfault;
    } _sifields;
} siginfo_t;

#define si_pid     _sifields._sigchld.si_pid
#define si_status  _sifields._sigchld.si_status
#define si_addr    _sifields._sigfault.si_addr

/* ================================================================
 * ucontext_t — architecture-specific register context
 *
 * Layout must match the kernel's signal frame so that gregs offsets
 * used by the assembly clone3_raw / clone_raw helpers are correct.
 * ================================================================ */
#if defined(__x86_64__)

#define NGREG 23

enum {
    REG_R8 = 0, REG_R9, REG_R10, REG_R11,
    REG_R12, REG_R13, REG_R14, REG_R15,
    REG_RDI, REG_RSI, REG_RBP, REG_RBX,
    REG_RDX, REG_RAX, REG_RCX, REG_RSP,
    REG_RIP
};

typedef long long greg_t;
typedef greg_t gregset_t[NGREG];

/* Minimal mcontext — gregs + enough padding to match the kernel's
 * sigcontext so that uc_sigmask lands at the correct offset within
 * the kernel's signal frame.
 *
 * x86_64 sigcontext after gregs: fpstate(8) + reserved1[8](64) = 72 bytes
 * = 9 unsigned long slots. */
typedef struct {
    gregset_t gregs;
    unsigned long __pad[9];
} mcontext_t;

typedef struct ucontext_t {
    unsigned long     uc_flags;
    struct ucontext_t *uc_link;
    struct {
        void  *ss_sp;
        int    ss_flags;
        size_t ss_size;
    } uc_stack;
    mcontext_t        uc_mcontext;
    sigset_t          uc_sigmask;
} ucontext_t;

#elif defined(__i386__)

#define NGREG 19

enum {
    REG_GS = 0, REG_FS, REG_ES, REG_DS,
    REG_EDI, REG_ESI, REG_EBP, REG_ESP,
    REG_EBX, REG_EDX, REG_ECX, REG_EAX,
    REG_TRAPNO, REG_ERR, REG_EIP
};

typedef int greg_t;
typedef greg_t gregset_t[NGREG];

/* i386 sigcontext after gregs[0..18]: fpstate(4) + oldmask(4) + cr2(4)
 * = 12 bytes = 3 unsigned long slots.  This must match the kernel's
 * struct sigcontext_32 so that uc_sigmask lands at the correct offset
 * in the signal frame. */
typedef struct {
    gregset_t gregs;
    unsigned long __pad[3];
} mcontext_t;

typedef struct ucontext_t {
    unsigned long     uc_flags;
    struct ucontext_t *uc_link;
    struct {
        void  *ss_sp;
        int    ss_flags;
        size_t ss_size;
    } uc_stack;
    mcontext_t        uc_mcontext;
    sigset_t          uc_sigmask;
} ucontext_t;

#else
#error Unsupported architecture
#endif

/* ================================================================
 * Error numbers
 * ================================================================ */
#define EPERM           1
#define ENOENT          2
#define EINTR           4
#define EBADF           9
#define ENOMEM         12
#define EACCES         13
#define EEXIST         17
#define EINVAL         22
#define EROFS          30
#define ENAMETOOLONG   36
#define ENOSYS         38
#define ENOTEMPTY      39

/* Additional errnos used by the in-RAM filesystem add-in. */
#ifndef EBUSY
#define EBUSY           16
#endif
#ifndef EXDEV
#define EXDEV           18
#endif
#ifndef ENODEV
#define ENODEV          19
#endif
#ifndef ENOTDIR
#define ENOTDIR         20
#endif
#ifndef EISDIR
#define EISDIR          21
#endif
#ifndef ENFILE
#define ENFILE          23
#endif
#ifndef EMFILE
#define EMFILE          24
#endif
#ifndef ESPIPE
#define ESPIPE          29
#endif
#ifndef ELOOP
#define ELOOP           40
#endif
#ifndef ENODATA
#define ENODATA         61
#endif
#ifndef EOVERFLOW
#define EOVERFLOW       75
#endif
#ifndef ENOTSUP
#define ENOTSUP         95
#endif
#ifndef EAGAIN
#define EAGAIN          11
#endif
#ifndef EFAULT
#define EFAULT          14
#endif
#ifndef ENOSPC
#define ENOSPC          28
#endif
#ifndef EFBIG
#define EFBIG           27
#endif

/* Futex op codes used by inramfs's cross-process locks. */
#ifndef FUTEX_WAIT
#define FUTEX_WAIT          0
#endif
#ifndef FUTEX_WAKE
#define FUTEX_WAKE          1
#endif
#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG  128
#endif
/* inramfs uses *non*-private futexes because the futex word lives in
 * a MAP_SHARED region accessed by multiple processes; FUTEX_PRIVATE_FLAG
 * is only valid for single-process use. */

/* memfd_create flags (used by inramfs to allocate process-local fd
 * cookies that the kernel recognises for close/dup/poll). */
#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC         0x0001
#endif
#ifndef MFD_ALLOW_SEALING
#define MFD_ALLOW_SEALING   0x0002
#endif

/* lseek whence constants. */
#ifndef SEEK_SET
#define SEEK_SET 0
#endif
#ifndef SEEK_CUR
#define SEEK_CUR 1
#endif
#ifndef SEEK_END
#define SEEK_END 2
#endif

extern int g_errno_value;
#define errno g_errno_value

/* ================================================================
 * Open/fcntl constants
 * ================================================================ */
#define O_RDONLY        0
#define O_WRONLY        1
#define O_RDWR          2
#define O_ACCMODE       3
#define O_CREAT         0100
#define O_EXCL          0200
#define O_TRUNC         01000
#define O_APPEND        02000
#define O_NONBLOCK      04000
#define O_DIRECTORY     0200000
#define O_NOFOLLOW      0400000
#define O_CLOEXEC       02000000
#define O_TMPFILE       020200000

#define AT_FDCWD            (-100)
#define AT_SYMLINK_NOFOLLOW  0x100
#define AT_REMOVEDIR         0x200
#define AT_EMPTY_PATH        0x1000

/* ================================================================
 * File mode bits — st_mode
 * ================================================================ */
#define S_IFMT  0170000
#define S_IFDIR 0040000
#define S_IFCHR 0020000
#define S_IFBLK 0060000
#define S_IFREG 0100000
#define S_IFIFO 0010000
#define S_IFLNK 0120000
#define S_IFSOCK 0140000

/* Permission and special-mode bits used by inramfs's mode handling. */
#ifndef S_ISUID
#define S_ISUID  04000
#endif
#ifndef S_ISGID
#define S_ISGID  02000
#endif
#ifndef S_ISVTX
#define S_ISVTX  01000
#endif
#ifndef S_IRWXU
#define S_IRWXU  00700
#endif
#ifndef S_IRWXG
#define S_IRWXG  00070
#endif
#ifndef S_IRWXO
#define S_IRWXO  00007
#endif
#ifndef S_ISDIR
#define S_ISDIR(m)  (((m) & S_IFMT) == S_IFDIR)
#endif
#ifndef S_ISREG
#define S_ISREG(m)  (((m) & S_IFMT) == S_IFREG)
#endif
#ifndef S_ISLNK
#define S_ISLNK(m)  (((m) & S_IFMT) == S_IFLNK)
#endif

/* DT_* constants for getdents64(2) d_type field. */
#ifndef DT_UNKNOWN
#define DT_UNKNOWN 0
#define DT_FIFO    1
#define DT_CHR     2
#define DT_DIR     4
#define DT_BLK     6
#define DT_REG     8
#define DT_LNK    10
#define DT_SOCK   12
#endif

/* AT_* additions used by inramfs (utimensat, etc.) */
#ifndef UTIME_NOW
#define UTIME_NOW   ((1L << 30) - 1L)
#endif
#ifndef UTIME_OMIT
#define UTIME_OMIT  ((1L << 30) - 2L)
#endif

/* Linux directory-entry record as written by getdents64(2). */
struct linux_dirent64 {
    uint64_t       d_ino;
    int64_t        d_off;
    unsigned short d_reclen;
    unsigned char  d_type;
    char           d_name[];
};

/* ================================================================
 * Memory mapping
 * ================================================================ */
#define PROT_NONE       0x0
#define PROT_READ       0x1
#define PROT_WRITE      0x2
#define PROT_EXEC       0x4
#define MAP_SHARED      0x01
#define MAP_PRIVATE     0x02
#define MAP_FIXED       0x10
#define MAP_ANONYMOUS   0x20
#define MAP_FAILED      ((void *)-1)

/* Newer mmap flags used by the inramfs add-in to claim its high-
 * address region without trampling an existing mapping.  May be
 * absent on very old kernels; if so, addin code falls back to plain
 * MAP_FIXED.  Numeric value matches Linux UAPI. */
#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif

/* ================================================================
 * Signals
 * ================================================================ */
#define SIGBUS           7
#define SIGSEGV          11
#define SIGCHLD         17
#define SIGSYS          31
#define SIGKILL         9
#define SIGSTOP         19
#define SA_SIGINFO      0x00000004
#define SA_RESTART      0x10000000
#define SA_RESTORER     0x04000000
#define SA_ONSTACK      0x08000000
#define SIG_SETMASK     2
#define SI_KERNEL       0x80

/* ================================================================
 * Limits and clock
 * ================================================================ */
#define PATH_MAX        4096
#define CLOCK_REALTIME  0
#define __WALL          0x40000000

/* ================================================================
 * Wait status macros
 * ================================================================ */
#define WIFEXITED(s)    (((s) & 0x7f) == 0)
#define WEXITSTATUS(s)  (((s) & 0xff00) >> 8)
#define WIFSIGNALED(s)  (((signed char)(((s) & 0x7f) + 1) >> 1) > 0)
#define WTERMSIG(s)     ((s) & 0x7f)

/* ================================================================
 * siginfo si_code values for SIGCHLD
 * ================================================================ */
#define CLD_EXITED      1
#define CLD_KILLED      2
#define CLD_DUMPED      3

/* ================================================================
 * ELF types and constants
 *
 * Define the subset of ELF structures needed by sudtrace directly,
 * since the system elf.h may pull in glibc headers.
 * ================================================================ */

/* ELF identification */
#define EI_NIDENT  16
#define ELFCLASS32 1
#define ELFCLASS64 2

/* ELF segment types */
#define PT_NULL    0
#define PT_LOAD    1
#define PT_DYNAMIC 2
#define PT_INTERP  3
#define PT_PHDR    6

/* ELF segment flags */
#define PF_X       0x1
#define PF_W       0x2
#define PF_R       0x4

/* Auxiliary vector types */
#define AT_NULL    0
#define AT_PHDR    3
#define AT_PHENT   4
#define AT_PHNUM   5
#define AT_ENTRY   9

/* ELF magic */
#define ELFMAG     "\177ELF"

/* --- 32-bit ELF --- */

typedef uint16_t Elf32_Half;
typedef uint32_t Elf32_Word;
typedef int32_t  Elf32_Sword;
typedef uint32_t Elf32_Addr;
typedef uint32_t Elf32_Off;

typedef struct {
    unsigned char e_ident[EI_NIDENT];
    Elf32_Half  e_type;
    Elf32_Half  e_machine;
    Elf32_Word  e_version;
    Elf32_Addr  e_entry;
    Elf32_Off   e_phoff;
    Elf32_Off   e_shoff;
    Elf32_Word  e_flags;
    Elf32_Half  e_ehsize;
    Elf32_Half  e_phentsize;
    Elf32_Half  e_phnum;
    Elf32_Half  e_shentsize;
    Elf32_Half  e_shnum;
    Elf32_Half  e_shstrndx;
} Elf32_Ehdr;

typedef struct {
    Elf32_Word  p_type;
    Elf32_Off   p_offset;
    Elf32_Addr  p_vaddr;
    Elf32_Addr  p_paddr;
    Elf32_Word  p_filesz;
    Elf32_Word  p_memsz;
    Elf32_Word  p_flags;
    Elf32_Word  p_align;
} Elf32_Phdr;

typedef struct {
    uint32_t a_type;
    union { uint32_t a_val; } a_un;
} Elf32_auxv_t;

/* --- 64-bit ELF --- */

typedef uint16_t Elf64_Half;
typedef uint32_t Elf64_Word;
typedef int32_t  Elf64_Sword;
typedef uint64_t Elf64_Addr;
typedef uint64_t Elf64_Off;
typedef uint64_t Elf64_Xword;
typedef int64_t  Elf64_Sxword;

typedef struct {
    unsigned char e_ident[EI_NIDENT];
    Elf64_Half  e_type;
    Elf64_Half  e_machine;
    Elf64_Word  e_version;
    Elf64_Addr  e_entry;
    Elf64_Off   e_phoff;
    Elf64_Off   e_shoff;
    Elf64_Word  e_flags;
    Elf64_Half  e_ehsize;
    Elf64_Half  e_phentsize;
    Elf64_Half  e_phnum;
    Elf64_Half  e_shentsize;
    Elf64_Half  e_shnum;
    Elf64_Half  e_shstrndx;
} Elf64_Ehdr;

typedef struct {
    Elf64_Word   p_type;
    Elf64_Word   p_flags;
    Elf64_Off    p_offset;
    Elf64_Addr   p_vaddr;
    Elf64_Addr   p_paddr;
    Elf64_Xword  p_filesz;
    Elf64_Xword  p_memsz;
    Elf64_Xword  p_align;
} Elf64_Phdr;

typedef struct {
    uint64_t a_type;
    union { uint64_t a_val; } a_un;
} Elf64_auxv_t;

/* ================================================================
 * Architecture-specific ELF typedefs
 * ================================================================ */
#if defined(__x86_64__)
#define SUD_NATIVE_ELF_CLASS ELFCLASS64
#define SUD_VARIANT_NAME "sud64"
typedef Elf64_Ehdr   sud_elf_ehdr_t;
typedef Elf64_Phdr   sud_elf_phdr_t;
typedef Elf64_auxv_t sud_auxv_t;
#elif defined(__i386__)
#define SUD_NATIVE_ELF_CLASS ELFCLASS32
#define SUD_VARIANT_NAME "sud32"
typedef Elf32_Ehdr   sud_elf_ehdr_t;
typedef Elf32_Phdr   sud_elf_phdr_t;
typedef Elf32_auxv_t sud_auxv_t;
#else
#error Unsupported architecture
#endif

/* ================================================================
 * SUD constants (may not be in older kernel headers)
 * ================================================================ */
#ifndef PR_SET_SYSCALL_USER_DISPATCH
#define PR_SET_SYSCALL_USER_DISPATCH 59
#endif
#ifndef PR_SYS_DISPATCH_OFF
#define PR_SYS_DISPATCH_OFF 0
#endif
#ifndef PR_SYS_DISPATCH_ON
#define PR_SYS_DISPATCH_ON  1
#endif
#ifndef SYSCALL_DISPATCH_FILTER_ALLOW
#define SYSCALL_DISPATCH_FILTER_ALLOW 0
#endif
#ifndef SYSCALL_DISPATCH_FILTER_BLOCK
#define SYSCALL_DISPATCH_FILTER_BLOCK 1
#endif

/* ================================================================
 * Seccomp constants
 * ================================================================ */
#ifndef PR_SET_SECCOMP
#define PR_SET_SECCOMP 22
#endif
#ifndef PR_GET_SECCOMP
#define PR_GET_SECCOMP 21
#endif
#ifndef SECCOMP_MODE_DISABLED
#define SECCOMP_MODE_DISABLED 0
#endif
#ifndef SECCOMP_MODE_STRICT
#define SECCOMP_MODE_STRICT 1
#endif
#ifndef SECCOMP_MODE_FILTER
#define SECCOMP_MODE_FILTER 2
#endif
#ifndef SECCOMP_SET_MODE_STRICT
#define SECCOMP_SET_MODE_STRICT 0
#endif
#ifndef SECCOMP_SET_MODE_FILTER
#define SECCOMP_SET_MODE_FILTER 1
#endif
#ifndef SECCOMP_GET_ACTION_AVAIL
#define SECCOMP_GET_ACTION_AVAIL 2
#endif
#ifndef SECCOMP_GET_NOTIF_SIZES
#define SECCOMP_GET_NOTIF_SIZES 3
#endif

/* ================================================================
 * si_code values for SIGSYS
 * ================================================================ */
#ifndef SYS_SECCOMP
#define SYS_SECCOMP 1
#endif
#ifndef SYS_USER_DISPATCH
#define SYS_USER_DISPATCH 2
#endif

/* ================================================================
 * Clone / process flags
 * ================================================================ */
#define CLONE_VM             0x00000100
#define CLONE_VFORK          0x00004000
#define CLONE_THREAD         0x00010000
#define CLONE_ARGS_STACK_OFFSET 40  /* struct clone_args.stack */

/* ================================================================
 * Ptrace
 * ================================================================ */
#define PTRACE_TRACEME       0

/* ================================================================
 * Architecture-specific SUD sigset word type
 *
 * On i386 the kernel's rt_sigaction mask is 64 bits (unsigned long long).
 * On x86_64 it is 64 bits (unsigned long).
 * ================================================================ */
#if defined(__i386__)
typedef unsigned long long sud_sigset_word_t;
#else
typedef unsigned long sud_sigset_word_t;
#endif

/* ================================================================
 * stat_buf_t — opaque padded buffer for safe use with kernel stat
 * syscalls.  On i386, fstat64 writes 96 bytes into an 88-byte
 * struct stat, so we always use a generously-sized buffer.
 * Code accesses fields via cast: ((struct stat *)&buf)->st_dev etc.
 * ================================================================ */
typedef union { char _data[256]; } stat_buf_t;

/* ================================================================
 * MMAP2 page shift for i386
 * ================================================================ */
#define MINI_MMAP2_SHIFT 12

/* ================================================================
 * FILE as void*; stdin/stdout/stderr as fd-based handles
 * ================================================================ */
typedef void FILE;
extern FILE *stdin;
extern FILE *stdout;
extern FILE *stderr;

/* ================================================================
 * String/macro helpers
 * ================================================================ */
#define STR_VALUE(x) #x
#define STR(x) STR_VALUE(x)

/* ================================================================
 * Linker-provided symbols marking sudtrace's own address range
 * ================================================================ */
extern char __sud_begin[];
extern char __sud_end[];

/* ================================================================
 * Assembly-defined signal restorer
 * ================================================================ */
extern void sud_rt_sigreturn_restorer(void);

/* ================================================================
 * Environment
 * ================================================================ */
extern char **environ;

/* ================================================================
 * Function prototypes — libc replacements
 * ================================================================ */

/* Process control */
void _exit(int status) __attribute__((noreturn));
void exit(int status) __attribute__((noreturn));

/* errno */
int *__errno_location(void);

/* Memory operations */
void *memset(void *dst, int c, size_t n);
void *memcpy(void *dst, const void *src, size_t n);
void *memmove(void *dst, const void *src, size_t n);
int   memcmp(const void *a, const void *b, size_t n);
void *memchr(const void *s, int c, size_t n);

/* String operations */
size_t strlen(const char *s);
int    strcmp(const char *a, const char *b);
int    strncmp(const char *a, const char *b, size_t n);
char  *strchr(const char *s, int c);
char  *strrchr(const char *s, int c);
char  *strstr(const char *haystack, const char *needle);
char  *strdup(const char *s);
char  *strerror(int errnum);
char  *strtok_r(char *str, const char *delim, char **saveptr);

/* Memory allocation */
void *malloc(size_t size);
void  free(void *ptr);
void *calloc(size_t nmemb, size_t size);
void *realloc(void *ptr, size_t size);

/* Formatted output */
int vsnprintf(char *dst, size_t size, const char *fmt, va_list ap);
int snprintf(char *dst, size_t size, const char *fmt, ...);
int fprintf(FILE *stream, const char *fmt, ...);
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream);
void perror(const char *s);

/* File I/O */
int     access(const char *path, int mode);
int     open(const char *path, int flags, ...);
int     close(int fd);
ssize_t read(int fd, void *buf, size_t count);
ssize_t pread(int fd, void *buf, size_t count, off_t offset);
ssize_t write(int fd, const void *buf, size_t count);
ssize_t readlink(const char *path, char *buf, size_t bufsz);
int     dup2(int oldfd, int newfd);
int     fstat(int fd, struct stat *st);
int     fstatat(int dirfd, const char *path, struct stat *st, int flags);
int     __fxstat(int ver, int fd, struct stat *st);
int     __fxstatat(int ver, int dirfd, const char *path, struct stat *st, int flags);

/* Working directory */
char *getcwd(char *buf, size_t size);
char *realpath(const char *path, char *resolved_path);

/* Memory mapping */
void *mmap(void *addr, size_t len, int prot, int flags, int fd, off_t offset);
int   munmap(void *addr, size_t len);
int   mprotect(void *addr, size_t len, int prot);

/* Process control */
int   prctl(int option, unsigned long arg2, unsigned long arg3,
            unsigned long arg4, unsigned long arg5);
int   sigemptyset(sigset_t *set);
int   sigaction(int signum, const struct sigaction *act, struct sigaction *oldact);
int   usleep(useconds_t usec);
pid_t fork(void);
int   execv(const char *path, char *const argv[]);
pid_t waitpid(pid_t pid, int *status, int options);

/* Environment */
char *getenv(const char *name);
int   setenv(const char *name, const char *value, int overwrite);
int   unsetenv(const char *name);

/* Directory operations */
DIR           *opendir(const char *name);
struct dirent *readdir(DIR *dirp);
int            closedir(DIR *dirp);

/* ---- Additional ELF / ABI constants ---- */
#ifndef EI_CLASS
#define EI_CLASS  4
#endif
#ifndef SELFMAG
#define SELFMAG   4
#endif
#ifndef ET_DYN
#define ET_DYN    3
#endif
#ifndef ET_EXEC
#define ET_EXEC   2
#endif
#ifndef AT_BASE
#define AT_BASE   7
#endif
#ifndef AT_EXECFN
#define AT_EXECFN 31
#endif
#ifndef MAP_STACK
#define MAP_STACK 0x20000
#endif
#ifndef PR_SET_NAME
#define PR_SET_NAME 15
#endif
#ifndef STDOUT_FILENO
#define STDOUT_FILENO 1
#endif
#ifndef STDERR_FILENO
#define STDERR_FILENO 2
#endif

/* ================================================================
 * Formatted output — re-exported from the internal mpaland/printf.
 *
 * libc-fs vendors mpaland/printf under `libc-fs/deps/printf/`.  That
 * library defines `printf_/snprintf_/vsnprintf_/sprintf_/vprintf_/
 * fctprintf` symbols and uses macros to redirect the standard names
 * to them, so its consumers always see the standard libc spelling.
 *
 * We mirror that scheme here so consumers of libc-fs see the standard
 * names (`printf`, `snprintf`, `vsnprintf`, `sprintf`, `vprintf`,
 * `fctprintf`) without ever including `deps/printf/printf.h`.
 * ================================================================ */
int  printf_  (const char *fmt, ...);
int  sprintf_ (char *buf, const char *fmt, ...);
int  snprintf_(char *buf, size_t count, const char *fmt, ...);
int  vsnprintf_(char *buf, size_t count, const char *fmt, va_list va);
int  vprintf_ (const char *fmt, va_list va);
int  fctprintf(void (*out)(char ch, void *arg), void *arg,
               const char *fmt, ...);

/* The *snprintf prototypes that some headers above already declared
 * become straightforward macro aliases for the underscored versions.
 * `fprintf` is supplied directly by libc-fs (see libc.c) — it is NOT
 * routed through mpaland/printf because it requires per-call fd state. */
#define printf    printf_
#define sprintf   sprintf_
#define snprintf  snprintf_
#define vsnprintf vsnprintf_
#define vprintf   vprintf_

#endif /* LIBC_FS_LIBC_H */
