/*
 * sud/loader.c — In-process ELF loader.
 *
 * Loads a target ELF binary into memory, sets up SUD, and jumps
 * to its entry point.  Used by wrapper mode (sud32/sud64).
 */

#include "sud/libc.h"
#include "sud/raw.h"
#include "sud/fmt.h"
#include "sud/event.h"
#include "sud/elf.h"
#include "sud/handler.h"
#include "sud/loader.h"
#include "deps/printf/printf.h"

/*
 * crash_diagnostic_handler — SIGSEGV handler for debugging.
 *
 * When the loaded binary crashes, this handler prints the faulting address,
 * instruction pointer, register state, and a memory/stack/syscall window to
 * stderr.  This output helps diagnose ELF loader issues (wrong segment
 * mapping, bad auxv, etc.) without needing an external debugger.
 *
 * The handler uses only raw_write (via write to fd 2) which is async-signal-safe.
 */

/*
 * crash_diagnostic_handler — SIGSEGV/SIGBUS handler that dumps everything
 * needed to diagnose a crash in a stripped production build:
 *
 *   - Signal name, si_code (SI_KERNEL vs SEGV_MAPERR/ACCERR vs BUS_*)
 *   - Fault address (si_addr) and PID/TID
 *   - Full GPRs and segment registers
 *   - 64 bytes around the faulting IP, read via /proc/self/mem so an
 *     unmapped IP doesn't recursively SEGV the dumper
 *   - 16 stack words at and above the faulting SP
 *   - Thread count (entries in /proc/self/task)
 *   - Last ~16 syscalls dispatched by the SUD handler with PC + return,
 *     so we can see what the program was doing leading up to the fault
 *   - /proc/self/maps so RIP/fault addr can be resolved to a module
 *     without rebuilding with the same toolchain
 *
 * Async-signal-safe: only uses raw_syscall6, no malloc/printf/locale.
 */

/* Append a NUL-terminated string. */
static int append_str(char *buf, int pos, int max, const char *s)
{
    while (*s && pos < max) buf[pos++] = *s++;
    return pos;
}

/* Append a single char. */
static int append_ch(char *buf, int pos, int max, char c)
{
    if (pos < max) buf[pos++] = c;
    return pos;
}

/* Append a fixed-width hex value (digits chars). */
static int append_hex_w(char *buf, int pos, int max,
                       unsigned long val, int digits)
{
    static const char hx[] = "0123456789abcdef";
    if (pos + digits > max) return pos;
    for (int i = digits - 1; i >= 0; i--) {
        buf[pos + i] = hx[val & 0xf];
        val >>= 4;
    }
    return pos + digits;
}

/* Append a decimal value (signed). */
static int append_dec(char *buf, int pos, int max, long val)
{
    char tmp[24];
    int neg = val < 0, n = 0;
    unsigned long u = neg ? (unsigned long)(-val) : (unsigned long)val;
    do { tmp[n++] = '0' + (u % 10); u /= 10; } while (u);
    if (neg && pos < max) buf[pos++] = '-';
    for (int i = 0; i < n && pos < max; i++) buf[pos++] = tmp[n - 1 - i];
    return pos;
}

/* Width of a pointer in hex digits for the running architecture. */
#define PTR_HEX (int)(sizeof(unsigned long) * 2)

static int append_ptr(char *buf, int pos, int max, unsigned long val)
{
    pos = append_str(buf, pos, max, "0x");
    return append_hex_w(buf, pos, max, val, PTR_HEX);
}

/* Flush partial buffer to stderr and reset. */
static int flush_buf(char *buf, int *pos)
{
    if (*pos > 0) raw_write(2, buf, (size_t)*pos);
    *pos = 0;
    return 0;
}

/* pread of /proc/self/mem with a full unsigned 64-bit offset. The
 * raw_pread() helper takes off_t which is signed 32-bit on i386; an
 * address above 2 GiB (typical libc/altstack range) would sign-extend
 * to a negative 64-bit offset and the kernel returns -EINVAL. */
static ssize_t pread_addr(int fd, void *buf, size_t count, unsigned long addr)
{
#if defined(__x86_64__)
    return (ssize_t)raw_syscall6(SYS_pread64, fd, (long)buf, count,
                                 (long)addr, 0, 0);
#else
    unsigned long long off = (unsigned long long)addr;
    return (ssize_t)raw_syscall6(SYS_pread64, fd, (long)buf, count,
                                 (uint32_t)off, (uint32_t)(off >> 32), 0);
#endif
}

/* Dump 16-byte hex+ASCII rows of `len` bytes starting at `base`,
 * read via /proc/self/mem so an unmapped page returns -EIO/-EFAULT
 * rather than crashing the dumper. mem_fd may be -1 (skip dump). */
static void dump_mem(char *buf, int *pos, int max, int mem_fd,
                     unsigned long base, size_t len, const char *label)
{
    *pos = append_str(buf, *pos, max, "  ");
    *pos = append_str(buf, *pos, max, label);
    *pos = append_str(buf, *pos, max, " @ ");
    *pos = append_ptr(buf, *pos, max, base);
    *pos = append_ch(buf, *pos, max, '\n');
    if (mem_fd < 0) {
        *pos = append_str(buf, *pos, max,
                          "    (cannot open /proc/self/mem)\n");
        return;
    }
    unsigned char row[16];
    for (size_t off = 0; off < len; off += 16) {
        if (max - *pos < 80) flush_buf(buf, pos);
        ssize_t n = pread_addr(mem_fd, row, 16, base + off);
        *pos = append_str(buf, *pos, max, "    ");
        *pos = append_ptr(buf, *pos, max, base + off);
        *pos = append_str(buf, *pos, max, ": ");
        if (n <= 0) {
            *pos = append_str(buf, *pos, max, "(unreadable)\n");
            continue;
        }
        for (ssize_t i = 0; i < n; i++) {
            *pos = append_hex_w(buf, *pos, max, row[i], 2);
            *pos = append_ch(buf, *pos, max, ' ');
        }
        for (ssize_t i = n; i < 16; i++)
            *pos = append_str(buf, *pos, max, "   ");
        *pos = append_ch(buf, *pos, max, '|');
        for (ssize_t i = 0; i < n; i++) {
            char c = (row[i] >= 0x20 && row[i] < 0x7f) ? (char)row[i] : '.';
            *pos = append_ch(buf, *pos, max, c);
        }
        *pos = append_str(buf, *pos, max, "|\n");
    }
}

/* Count entries (excluding . and ..) in a directory via getdents64.
 * Returns -1 on open failure. */
static int count_dir_entries(const char *path)
{
    int fd = raw_open(path, O_RDONLY | O_DIRECTORY);
    if (fd < 0) return -1;
    char buf[4096];
    int count = 0;
    for (;;) {
        long n = raw_getdents64(fd, buf, sizeof(buf));
        if (n <= 0) break;
        long off = 0;
        while (off < n) {
            /* dirent64: ino(8) off(8) reclen(2) type(1) name[] */
            unsigned short reclen = *(unsigned short *)(buf + off + 16);
            const char *name = buf + off + 19;
            if (!(name[0] == '.' &&
                  (name[1] == '\0' ||
                   (name[1] == '.' && name[2] == '\0'))))
                count++;
            if (reclen == 0) break;
            off += reclen;
        }
    }
    raw_close(fd);
    return count;
}

/* Copy the contents of /proc/self/maps to fd 2 in chunks. */
static void dump_maps(void)
{
    int fd = raw_open("/proc/self/maps", O_RDONLY);
    if (fd < 0) {
        const char m[] = "  /proc/self/maps: (unreadable)\n";
        raw_write(2, m, sizeof(m) - 1);
        return;
    }
    const char hdr[] = "  /proc/self/maps:\n";
    raw_write(2, hdr, sizeof(hdr) - 1);
    char buf[4096];
    for (;;) {
        ssize_t n = raw_read(fd, buf, sizeof(buf));
        if (n <= 0) break;
        raw_write(2, buf, (size_t)n);
    }
    raw_close(fd);
}

/* Dump the recent-syscalls ring. We print up to SUD_SYSLOG_SIZE
 * most-recent entries, oldest-first. */
static void dump_syslog(char *buf, int *pos, int max)
{
    *pos = append_str(buf, *pos, max, "  Recent syscalls (oldest first):\n");
    unsigned int head = __atomic_load_n(&g_sud_syslog_head, __ATOMIC_RELAXED);
    /* If head < SIZE we have head valid entries; otherwise SIZE. */
    unsigned int n = head < SUD_SYSLOG_SIZE ? head : SUD_SYSLOG_SIZE;
    unsigned int start = head - n;
    for (unsigned int i = 0; i < n; i++) {
        if (max - *pos < 96) flush_buf(buf, pos);
        unsigned int idx = (start + i) & (SUD_SYSLOG_SIZE - 1);
        struct sud_syslog_entry e = g_sud_syslog[idx];
        if (e.nr < 0) continue;  /* never-written slot */
        *pos = append_str(buf, *pos, max, "    [");
        *pos = append_dec(buf, *pos, max, (long)i);
        *pos = append_str(buf, *pos, max, "] tid=");
        *pos = append_dec(buf, *pos, max, (long)e.tid);
        *pos = append_str(buf, *pos, max, " nr=");
        *pos = append_dec(buf, *pos, max, e.nr);
        *pos = append_str(buf, *pos, max, " pc=");
        *pos = append_ptr(buf, *pos, max, e.pc);
        *pos = append_str(buf, *pos, max, " ret=");
        if (e.ret == SUD_SYSLOG_NORETURN)
            *pos = append_str(buf, *pos, max, "(in-progress)");
        else
            *pos = append_dec(buf, *pos, max, e.ret);
        *pos = append_ch(buf, *pos, max, '\n');
    }
}

/* Recursion guard: if the diagnostic handler itself faults, _exit. */
static volatile int g_in_crash_dumper = 0;

static void crash_diagnostic_handler(int sig, siginfo_t *info, void *uctx_raw)
{
    /* If the dumper itself crashed (e.g. /proc/self/mem read of a bad
     * stack address recursively faulted), bail out immediately. */
    if (__atomic_exchange_n(&g_in_crash_dumper, 1, __ATOMIC_ACQ_REL)) {
        _exit(128 + sig);
    }

    ucontext_t *uc = (ucontext_t *)uctx_raw;
    char buf[4096];
    int pos = 0;
    const int max = (int)sizeof(buf);

    /* Header */
    pos = append_str(buf, pos, max,
                     "\nsudtrace: CRASH DIAGNOSTIC\n  Signal: ");
    pos = append_str(buf, pos, max,
                     sig == SIGSEGV ? "SIGSEGV" :
                     sig == SIGBUS  ? "SIGBUS"  : "?");
    pos = append_ch(buf, pos, max, '\n');

    /* si_code */
    pos = append_str(buf, pos, max, "  si_code: ");
    pos = append_dec(buf, pos, max, info->si_code);
    if (info->si_code == SI_KERNEL)
        pos = append_str(buf, pos, max, " (SI_KERNEL)");
    else if (sig == SIGSEGV) {
        if (info->si_code == 1) pos = append_str(buf, pos, max, " (SEGV_MAPERR)");
        else if (info->si_code == 2) pos = append_str(buf, pos, max, " (SEGV_ACCERR)");
    }
    pos = append_ch(buf, pos, max, '\n');

    /* PID/TID + fault addr */
    pos = append_str(buf, pos, max, "  pid=");
    pos = append_dec(buf, pos, max,
                     raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0));
    pos = append_str(buf, pos, max, " tid=");
    pos = append_dec(buf, pos, max, raw_gettid());
    pos = append_str(buf, pos, max, " threads=");
    pos = append_dec(buf, pos, max, count_dir_entries("/proc/self/task"));
    pos = append_ch(buf, pos, max, '\n');

    pos = append_str(buf, pos, max, "  Fault addr: ");
    pos = append_ptr(buf, pos, max, (unsigned long)info->si_addr);
    pos = append_ch(buf, pos, max, '\n');

    /* Registers — full GPR + segment dump */
#if defined(__x86_64__)
    static const struct { const char *name; int idx; } gp[] = {
        {"RIP", REG_RIP}, {"RSP", REG_RSP}, {"RBP", REG_RBP},
        {"RAX", REG_RAX}, {"RBX", REG_RBX}, {"RCX", REG_RCX}, {"RDX", REG_RDX},
        {"RDI", REG_RDI}, {"RSI", REG_RSI},
        {"R8 ", REG_R8 }, {"R9 ", REG_R9 }, {"R10", REG_R10}, {"R11", REG_R11},
        {"R12", REG_R12}, {"R13", REG_R13}, {"R14", REG_R14}, {"R15", REG_R15},
    };
    for (unsigned i = 0; i < sizeof(gp)/sizeof(gp[0]); i++) {
        if ((i & 3) == 0) {
            if (i) pos = append_ch(buf, pos, max, '\n');
            pos = append_str(buf, pos, max, "  ");
        } else {
            pos = append_ch(buf, pos, max, ' ');
        }
        pos = append_str(buf, pos, max, gp[i].name);
        pos = append_ch(buf, pos, max, '=');
        pos = append_ptr(buf, pos, max,
                         (unsigned long)uc->uc_mcontext.gregs[gp[i].idx]);
    }
    pos = append_ch(buf, pos, max, '\n');
    unsigned long pc_ul = (unsigned long)uc->uc_mcontext.gregs[REG_RIP];
    unsigned long sp_ul = (unsigned long)uc->uc_mcontext.gregs[REG_RSP];
#else
    static const struct { const char *name; int idx; } gp[] = {
        {"EIP", REG_EIP}, {"ESP", REG_ESP}, {"EBP", REG_EBP},
        {"EAX", REG_EAX}, {"EBX", REG_EBX}, {"ECX", REG_ECX}, {"EDX", REG_EDX},
        {"ESI", REG_ESI}, {"EDI", REG_EDI},
        {"GS ", REG_GS }, {"FS ", REG_FS }, {"ES ", REG_ES }, {"DS ", REG_DS },
    };
    for (unsigned i = 0; i < sizeof(gp)/sizeof(gp[0]); i++) {
        if ((i & 3) == 0) {
            if (i) pos = append_ch(buf, pos, max, '\n');
            pos = append_str(buf, pos, max, "  ");
        } else {
            pos = append_ch(buf, pos, max, ' ');
        }
        pos = append_str(buf, pos, max, gp[i].name);
        pos = append_ch(buf, pos, max, '=');
        pos = append_ptr(buf, pos, max,
                         (unsigned long)uc->uc_mcontext.gregs[gp[i].idx]);
    }
    pos = append_ch(buf, pos, max, '\n');
    /* CS/SS aren't named in the i386 REG_ enum; their gregs indices
     * are 15 and 18 (see sud/libc.h). */
    pos = append_str(buf, pos, max, "  CS=");
    pos = append_ptr(buf, pos, max,
                     (unsigned long)uc->uc_mcontext.gregs[15]);
    pos = append_str(buf, pos, max, " SS=");
    pos = append_ptr(buf, pos, max,
                     (unsigned long)uc->uc_mcontext.gregs[18]);
    pos = append_ch(buf, pos, max, '\n');
    unsigned long pc_ul = (unsigned long)uc->uc_mcontext.gregs[REG_EIP];
    unsigned long sp_ul = (unsigned long)uc->uc_mcontext.gregs[REG_ESP];
#endif

    flush_buf(buf, &pos);

    /* Open /proc/self/mem for safe pointer reads. */
    int mem_fd = raw_open("/proc/self/mem", O_RDONLY);

    /* Bytes around the faulting instruction — 16 before, 48 after.
     * On most x86 mispredictions/jumps the offending instruction is at
     * RIP itself; reading a small window before helps when it's the
     * tail of a longer instruction or when RIP landed mid-prologue. */
    {
        unsigned long start = pc_ul >= 16 ? pc_ul - 16 : 0;
        dump_mem(buf, &pos, max, mem_fd, start, 64, "Code window");
    }
    flush_buf(buf, &pos);

    /* Stack window — the next few qwords/dwords at RSP. */
    dump_mem(buf, &pos, max, mem_fd, sp_ul,
             16 * sizeof(unsigned long), "Stack window");
    flush_buf(buf, &pos);

    if (mem_fd >= 0) raw_close(mem_fd);

    /* Recent syscalls dispatched by the SUD handler. */
    dump_syslog(buf, &pos, max);
    flush_buf(buf, &pos);

    /* Process memory map — last so it doesn't push everything else
     * off-screen if the user's terminal scrollback is small. */
    dump_maps();

    /* Re-raise with default handler to get a core dump. */
    {
        struct kernel_sigaction_raw dfl;
        memset(&dfl, 0, sizeof(dfl));
        dfl.handler = (void (*)(int))0; /* SIG_DFL */
        dfl.flags = SA_RESTORER;
        dfl.restorer = sud_rt_sigreturn_restorer;
        raw_syscall6(SYS_rt_sigaction, sig, (long)&dfl, 0,
                     sizeof(dfl.mask), 0, 0);
    }
    raw_syscall6(SYS_kill, raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0),
                 sig, 0, 0, 0, 0);
    _exit(128 + sig);
}

void load_and_run_elf(const char *path, int argc, char **argv,
                      int drop_count)
{
    /* Compute the visible argv (what the target process should see) */
    int vis_argc = argc - drop_count;
    char **vis_argv = argv + drop_count;
    if (vis_argc < 0) vis_argc = 0;

    /* Record the target program's resolved path so the SIGSYS handler can
     * mask /proc/self/exe and other identity queries. */
    {
        const char *tgt = (vis_argc > 0 && vis_argv[0]) ? vis_argv[0] : path;
        char tgt_resolved[PATH_MAX];
        if (resolve_path(tgt, tgt_resolved, sizeof(tgt_resolved)))
            snprintf_(g_target_exe, sizeof(g_target_exe), "%s", tgt_resolved);
        else
            snprintf_(g_target_exe, sizeof(g_target_exe), "%s", tgt);
    }

    pid_t self = raw_gettid();
    const char *event_exe = (vis_argc > 0 && vis_argv[0]) ? vis_argv[0] : path;
    emit_cwd_event(self);
    emit_exec_event(self, event_exe, vis_argc, vis_argv);
    emit_inherited_open_events(self);

    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        perror("sudtrace: open ELF");
        _exit(127);
    }

    sud_elf_ehdr_t ehdr;
    if (read(fd, &ehdr, sizeof(ehdr)) != (ssize_t)sizeof(ehdr)) {
        fprintf(stderr, "sudtrace: cannot read ELF header\n");
        close(fd);
        _exit(127);
    }

    if (memcmp(ehdr.e_ident, ELFMAG, SELFMAG) != 0 ||
        ehdr.e_ident[EI_CLASS] != SUD_NATIVE_ELF_CLASS) {
        fprintf(stderr, "sudtrace: not a valid native ELF for %s: %s\n",
                SUD_VARIANT_NAME, path);
        close(fd);
        _exit(127);
    }

    /* For PIE/ET_DYN binaries (e.g. ld.so), p_vaddr starts at 0.
     * We need to pick a base address because mmap(0, ..., MAP_FIXED) fails
     * due to vm.mmap_min_addr.  For ET_EXEC, base stays 0 (use as-is). */
    unsigned long load_base = 0;
    if (ehdr.e_type == ET_DYN) {
        unsigned long lo = ~0UL, hi = 0;
        for (int i = 0; i < ehdr.e_phnum; i++) {
            sud_elf_phdr_t phdr;
            if (pread(fd, &phdr, sizeof(phdr),
                      ehdr.e_phoff + i * ehdr.e_phentsize) != (ssize_t)sizeof(phdr))
                continue;
            if (phdr.p_type != PT_LOAD) continue;
            unsigned long seg_lo = phdr.p_vaddr & ~0xfffUL;
            unsigned long seg_hi = (phdr.p_vaddr + phdr.p_memsz + 0xfff)
                                    & ~0xfffUL;
            if (seg_lo < lo) lo = seg_lo;
            if (seg_hi > hi) hi = seg_hi;
        }
        if (hi > lo) {
            void *hint = mmap(NULL, hi - lo, PROT_NONE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (hint == MAP_FAILED) {
                fprintf(stderr, "sudtrace: cannot reserve %lu bytes for PIE\n",
                        hi - lo);
                close(fd);
                _exit(127);
            }
            load_base = (unsigned long)hint - lo;
            munmap(hint, hi - lo);
        }
    }

    /* Load PT_LOAD segments */
    for (int i = 0; i < ehdr.e_phnum; i++) {
        sud_elf_phdr_t phdr;
        if (pread(fd, &phdr, sizeof(phdr),
                  ehdr.e_phoff + i * ehdr.e_phentsize) != (ssize_t)sizeof(phdr))
            continue;

        if (phdr.p_type != PT_LOAD) continue;

        unsigned long vaddr = load_base + phdr.p_vaddr;
        unsigned long page_offset = vaddr & 0xfff;
        unsigned long map_addr = vaddr - page_offset;
        unsigned long map_size = phdr.p_memsz + page_offset;
        map_size = (map_size + 0xfff) & ~0xfffUL;

        int prot = 0;
        if (phdr.p_flags & PF_R) prot |= PROT_READ;
        if (phdr.p_flags & PF_W) prot |= PROT_WRITE;
        if (phdr.p_flags & PF_X) prot |= PROT_EXEC;

        void *mapped = mmap((void *)map_addr, map_size,
                           PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                           -1, 0);
        if (mapped == MAP_FAILED) {
            fprintf(stderr, "sudtrace: mmap segment at %#lx: %s\n",
                    map_addr, strerror(errno));
            close(fd);
            _exit(127);
        }

        if (phdr.p_filesz > 0) {
            if (pread(fd, (char *)mapped + page_offset, phdr.p_filesz,
                      phdr.p_offset) != (ssize_t)phdr.p_filesz) {
                fprintf(stderr, "sudtrace: failed to read segment\n");
                close(fd);
                _exit(127);
            }
        }

        mprotect(mapped, map_size, prot);
    }

    /* If drop_count > 0, the primary ELF is an intermediate loader
     * (ld-linux) and we also need to load the target program. */
    unsigned long target_load_base = 0;
    sud_elf_ehdr_t target_ehdr;
    unsigned long target_phdr_addr = 0;
    int target_fd = -1;
    memset(&target_ehdr, 0, sizeof(target_ehdr));

    if (drop_count > 0 && argc > 1 && argv[1]) {
        /* The target binary is always argv[1]: the first argument after
         * the main ELF (ld-linux).  For example:
         *   argv = [ld-linux, /bin/bash, ./script.sh, arg]  drop_count=2
         *   → target is /bin/bash (argv[1]), NOT ./script.sh (vis_argv[0])
         * When drop_count == 1, argv[1] == vis_argv[0] so this is safe. */
        char target_resolved[PATH_MAX];
        const char *target_name = argv[1];
        if (!resolve_path(target_name, target_resolved, sizeof(target_resolved)))
            snprintf_(target_resolved, sizeof(target_resolved), "%s", target_name);

        target_fd = open(target_resolved, O_RDONLY);
        if (target_fd >= 0) {
            if (read(target_fd, &target_ehdr, sizeof(target_ehdr)) == (ssize_t)sizeof(target_ehdr) &&
                memcmp(target_ehdr.e_ident, ELFMAG, SELFMAG) == 0 &&
                target_ehdr.e_ident[EI_CLASS] == SUD_NATIVE_ELF_CLASS) {

                /* Determine target load base */
                if (target_ehdr.e_type == ET_DYN) {
                    unsigned long lo = ~0UL, hi = 0;
                    for (int i = 0; i < target_ehdr.e_phnum; i++) {
                        sud_elf_phdr_t phdr;
                        if (pread(target_fd, &phdr, sizeof(phdr),
                                  target_ehdr.e_phoff + i * target_ehdr.e_phentsize) != (ssize_t)sizeof(phdr))
                            continue;
                        if (phdr.p_type != PT_LOAD) continue;
                        unsigned long seg_lo = phdr.p_vaddr & ~0xfffUL;
                        unsigned long seg_hi = (phdr.p_vaddr + phdr.p_memsz + 0xfff)
                                                & ~0xfffUL;
                        if (seg_lo < lo) lo = seg_lo;
                        if (seg_hi > hi) hi = seg_hi;
                    }
                    if (hi > lo) {
                        void *hint = mmap(NULL, hi - lo, PROT_NONE,
                                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                        if (hint != MAP_FAILED) {
                            target_load_base = (unsigned long)hint - lo;
                            munmap(hint, hi - lo);
                        }
                    }
                }

                /* Load target PT_LOAD segments */
                for (int i = 0; i < target_ehdr.e_phnum; i++) {
                    sud_elf_phdr_t phdr;
                    if (pread(target_fd, &phdr, sizeof(phdr),
                              target_ehdr.e_phoff + i * target_ehdr.e_phentsize) != (ssize_t)sizeof(phdr))
                        continue;
                    if (phdr.p_type != PT_LOAD) continue;

                    unsigned long vaddr = target_load_base + phdr.p_vaddr;
                    unsigned long page_offset = vaddr & 0xfff;
                    unsigned long map_addr = vaddr - page_offset;
                    unsigned long map_size = phdr.p_memsz + page_offset;
                    map_size = (map_size + 0xfff) & ~0xfffUL;

                    int prot = 0;
                    if (phdr.p_flags & PF_R) prot |= PROT_READ;
                    if (phdr.p_flags & PF_W) prot |= PROT_WRITE;
                    if (phdr.p_flags & PF_X) prot |= PROT_EXEC;

                    void *mapped = mmap((void *)map_addr, map_size,
                                       PROT_READ | PROT_WRITE,
                                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                                       -1, 0);
                    if (mapped == MAP_FAILED) continue;

                    if (phdr.p_filesz > 0) {
                        pread(target_fd, (char *)mapped + page_offset,
                              phdr.p_filesz, phdr.p_offset);
                    }
                    mprotect(mapped, map_size, prot);
                }

                /* Find target's phdr address in memory */
                for (int i = 0; i < target_ehdr.e_phnum; i++) {
                    sud_elf_phdr_t phdr;
                    if (pread(target_fd, &phdr, sizeof(phdr),
                              target_ehdr.e_phoff + i * target_ehdr.e_phentsize) != (ssize_t)sizeof(phdr))
                        continue;
                    if (phdr.p_type == PT_PHDR) {
                        target_phdr_addr = target_load_base + phdr.p_vaddr;
                        break;
                    }
                }
                if (!target_phdr_addr) {
                    for (int i = 0; i < target_ehdr.e_phnum; i++) {
                        sud_elf_phdr_t phdr;
                        if (pread(target_fd, &phdr, sizeof(phdr),
                                  target_ehdr.e_phoff + i * target_ehdr.e_phentsize) != (ssize_t)sizeof(phdr))
                            continue;
                        if (phdr.p_type == PT_LOAD &&
                            target_ehdr.e_phoff >= phdr.p_offset &&
                            target_ehdr.e_phoff < phdr.p_offset + phdr.p_filesz) {
                            target_phdr_addr = target_load_base + phdr.p_vaddr +
                                               (target_ehdr.e_phoff - phdr.p_offset);
                            break;
                        }
                    }
                }
                if (!target_phdr_addr)
                    target_phdr_addr = target_load_base + target_ehdr.e_phoff;
            } else {
                close(target_fd);
                target_fd = -1;
                drop_count = 0;
                vis_argc = argc;
                vis_argv = argv;
            }
        } else {
            drop_count = 0;
            vis_argc = argc;
            vis_argv = argv;
        }
    }

    /* Install a fresh per-task alternate signal stack.  See
     * ensure_sud_altstack() in this file for the rationale. */
    ensure_sud_altstack();

    /* Install SIGSYS handler */
    install_sigsys_handler_raw();

    /* Allocate SUD selector byte in a dedicated mmap page */
    void *sel_page = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (sel_page == MAP_FAILED) {
        perror("sudtrace: mmap selector");
        _exit(127);
    }
    volatile unsigned char *sel = (volatile unsigned char *)sel_page;
    *sel = SYSCALL_DISPATCH_FILTER_BLOCK;
    g_sud_selector_ptr = sel;

    reset_sigmask_raw();

    /* Enable SUD */
    unsigned long off = (unsigned long)__sud_begin;
    unsigned long len = (unsigned long)__sud_end - (unsigned long)__sud_begin;

    if (prctl(PR_SET_SYSCALL_USER_DISPATCH, PR_SYS_DISPATCH_ON,
              off, len, (unsigned long)sel) < 0) {
        perror("sudtrace: prctl(PR_SET_SYSCALL_USER_DISPATCH)");
        fprintf(stderr, "  Requires CONFIG_SYSCALL_USER_DISPATCH=y "
                "(Linux 5.11+).\n");
        _exit(127);
    }

    /* Set process name to target basename */
    if (g_target_exe[0]) {
        const char *bn = g_target_exe;
        const char *sl = g_target_exe;
        while (*sl) { if (*sl == '/') bn = sl + 1; sl++; }
        prctl(PR_SET_NAME, (unsigned long)bn, 0, 0, 0);
    }

    /* Build the new stack */
    size_t stack_size = 8 * 1024 * 1024;
    void *stack_base = mmap(NULL, stack_size,
                           PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK,
                           -1, 0);
    if (stack_base == MAP_FAILED) {
        perror("sudtrace: mmap stack");
        _exit(127);
    }

    unsigned long *sp = (unsigned long *)((char *)stack_base + stack_size);

    extern char **environ;
    int envc = 0;
    if (environ)
        while (environ[envc]) envc++;

    int total_slots = 1 + vis_argc + 1 + envc + 1 + 128;
    sp -= total_slots;
    sp = (unsigned long *)((unsigned long)sp & ~0xfUL);

    int idx = 0;
    sp[idx++] = vis_argc;
    for (int i = 0; i < vis_argc; i++)
        sp[idx++] = (unsigned long)vis_argv[i];
    sp[idx++] = 0;
    for (int i = 0; i < envc; i++)
        sp[idx++] = (unsigned long)environ[i];
    sp[idx++] = 0;

    /* Copy auxv, patching relevant entries */
    {
        unsigned long phdr_addr = 0;
        for (int i = 0; i < ehdr.e_phnum; i++) {
            sud_elf_phdr_t phdr;
            if (pread(fd, &phdr, sizeof(phdr),
                      ehdr.e_phoff + i * ehdr.e_phentsize) != (ssize_t)sizeof(phdr))
                continue;
            if (phdr.p_type == PT_PHDR) {
                phdr_addr = load_base + phdr.p_vaddr;
                break;
            }
        }
        if (!phdr_addr) {
            for (int i = 0; i < ehdr.e_phnum; i++) {
                sud_elf_phdr_t phdr;
                if (pread(fd, &phdr, sizeof(phdr),
                          ehdr.e_phoff + i * ehdr.e_phentsize) != (ssize_t)sizeof(phdr))
                    continue;
                if (phdr.p_type == PT_LOAD &&
                    ehdr.e_phoff >= phdr.p_offset &&
                    ehdr.e_phoff < phdr.p_offset + phdr.p_filesz) {
                    phdr_addr = load_base + phdr.p_vaddr +
                                (ehdr.e_phoff - phdr.p_offset);
                    break;
                }
            }
        }
        if (!phdr_addr)
            phdr_addr = load_base + ehdr.e_phoff;

        int use_interp_mode = (drop_count > 0 && target_fd >= 0);

        int aux_fd2 = open("/proc/self/auxv", O_RDONLY);
        if (aux_fd2 >= 0) {
            sud_auxv_t avbuf[64];
            ssize_t n = read(aux_fd2, avbuf, sizeof(avbuf));
            close(aux_fd2);
            if (n > 0) {
                int auxc = n / (int)sizeof(sud_auxv_t);
                for (int i = 0; i < auxc; i++) {
                    if (use_interp_mode) {
                        switch (avbuf[i].a_type) {
                        case AT_ENTRY:
                            avbuf[i].a_un.a_val = target_load_base +
                                                  target_ehdr.e_entry;
                            break;
                        case AT_PHDR:
                            avbuf[i].a_un.a_val = target_phdr_addr;
                            break;
                        case AT_PHNUM:
                            avbuf[i].a_un.a_val = target_ehdr.e_phnum;
                            break;
                        case AT_PHENT:
                            avbuf[i].a_un.a_val = target_ehdr.e_phentsize;
                            break;
                        case AT_BASE:
                            avbuf[i].a_un.a_val = load_base;
                            break;
#ifdef AT_EXECFN
                        case AT_EXECFN:
                            if (g_target_exe[0])
                                avbuf[i].a_un.a_val =
                                    (unsigned long)g_target_exe;
                            break;
#endif
                        }
                    } else {
                        switch (avbuf[i].a_type) {
                        case AT_ENTRY:
                            avbuf[i].a_un.a_val = load_base + ehdr.e_entry;
                            break;
                        case AT_PHDR:
                            avbuf[i].a_un.a_val = phdr_addr;
                            break;
                        case AT_PHNUM:
                            avbuf[i].a_un.a_val = ehdr.e_phnum;
                            break;
                        case AT_PHENT:
                            avbuf[i].a_un.a_val = ehdr.e_phentsize;
                            break;
                        case AT_BASE:
                            avbuf[i].a_un.a_val = 0;
                            break;
#ifdef AT_EXECFN
                        case AT_EXECFN:
                            if (g_target_exe[0])
                                avbuf[i].a_un.a_val =
                                    (unsigned long)g_target_exe;
                            break;
#endif
                        }
                    }

                    sp[idx++] = avbuf[i].a_type;
                    sp[idx++] = avbuf[i].a_un.a_val;

                    if (avbuf[i].a_type == AT_NULL) break;
                }
            }
        }
    }

    close(fd);
    if (target_fd >= 0)
        close(target_fd);

    /* Install a diagnostic SIGSEGV/SIGBUS handler to capture crash details.
     * This runs in place of the default handler and prints the fault
     * address and register state before aborting, which is invaluable
     * for debugging issues with the in-process loader.
     *
     * SA_ONSTACK: use an alternate signal stack so the handler can run
     * even when the main stack is corrupted (which is common for
     * SI_KERNEL crashes where iret fails due to bad segment state).
     *
     * Note: the alt sigstack is set up earlier (before SUD is enabled,
     * for the SIGSYS handler) and is generously sized; SIGSEGV/SIGBUS
     * happen rarely enough that sharing it is fine. */
    {
        struct kernel_sigaction_raw segv_sa;
        memset(&segv_sa, 0, sizeof(segv_sa));
        segv_sa.handler = (void (*)(int))crash_diagnostic_handler;
        segv_sa.flags = SA_SIGINFO | SA_RESTART | SA_RESTORER | SA_ONSTACK;
        segv_sa.restorer = sud_rt_sigreturn_restorer;
        segv_sa.mask = 0;
        raw_syscall6(SYS_rt_sigaction, SIGSEGV, (long)&segv_sa, 0,
                     sizeof(segv_sa.mask), 0, 0);
        raw_syscall6(SYS_rt_sigaction, SIGBUS, (long)&segv_sa, 0,
                     sizeof(segv_sa.mask), 0, 0);
    }

    /* Jump to the entry point */
    unsigned long entry = load_base + ehdr.e_entry;

#if defined(__x86_64__)
    __asm__ volatile(
        "mov %0, %%rsp\n\t"
        "push %1\n\t"
        "xor %%rax, %%rax\n\t"
        "xor %%rbx, %%rbx\n\t"
        "xor %%rcx, %%rcx\n\t"
        "xor %%rdx, %%rdx\n\t"
        "xor %%rsi, %%rsi\n\t"
        "xor %%rdi, %%rdi\n\t"
        "xor %%rbp, %%rbp\n\t"
        "xor %%r8, %%r8\n\t"
        "xor %%r9, %%r9\n\t"
        "xor %%r10, %%r10\n\t"
        "xor %%r11, %%r11\n\t"
        "xor %%r12, %%r12\n\t"
        "xor %%r13, %%r13\n\t"
        "xor %%r14, %%r14\n\t"
        "xor %%r15, %%r15\n\t"
        "ret\n\t"
        :
        : "r"(sp), "r"(entry)
        : "memory"
    );
#else
    __asm__ volatile(
        "mov %0, %%esp\n\t"
        "push %1\n\t"
        "xor %%eax, %%eax\n\t"
        "xor %%ebx, %%ebx\n\t"
        "xor %%ecx, %%ecx\n\t"
        "xor %%edx, %%edx\n\t"
        "xor %%esi, %%esi\n\t"
        "xor %%edi, %%edi\n\t"
        "xor %%ebp, %%ebp\n\t"
        "ret\n\t"
        :
        : "r"(sp), "r"(entry)
        : "memory"
    );
#endif

    __builtin_unreachable();
}
