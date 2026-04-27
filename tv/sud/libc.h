/*
 * sud/libc.h — Freestanding mini-libc header for the sudtrace project.
 *
 * Provides all type definitions, constants, and function declarations
 * needed by the freestanding (-nostdlib -ffreestanding) sud32/sud64
 * binaries.  NO glibc headers are included; only compiler-provided
 * freestanding headers and Linux UAPI headers are used.
 */

#ifndef SUD_LIBC_H
#define SUD_LIBC_H

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
#define EINVAL         22
#define ENAMETOOLONG   36
#define ENOSYS         38

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

#define AT_FDCWD        (-100)
#define AT_EMPTY_PATH   0x1000

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

#endif /* SUD_LIBC_H */
