/*
 * sud/sudtrace.c — Launcher program for SUD-based process tracing.
 *
 * This is a NORMAL C program linked with libc.  It:
 *   1. Parses user arguments (-o FILE, --no-env, -- command ...)
 *   2. Builds the wrapper command (sud64/sud32 ... /path/to/binary ...)
 *   3. Sets up the output fd and environment
 *   4. Fork + exec's the wrapper
 *   5. Waits for children, emitting EXIT events
 *
 * This program does NOT set up SUD or intercept syscalls itself.
 * That is entirely the job of sud32/sud64 (sud/wrapper.c).
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <time.h>
#include <stdint.h>
#include <limits.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/syscall.h>

#include <sched.h>
#include "wire/wire.h"
#include "trace/trace.h"
#include "sud/runtime_config.h"

#ifndef __WALL
#define __WALL 0x40000000
#endif

/* Reserve two high FDs so children are unlikely to clobber them.
 *   SUD_OUTPUT_FD : the wire output file
 *   SUD_STATE_FD  : MAP_SHARED anon page with the atomic stream-id
 *                   counter (no lock — see sud/trace/event.c). */
#define SUD_OUTPUT_FD        1023
#define SUD_STATE_FD         1022
#define SUD_STATE_PAGE_SIZE  4096

/* ================================================================
 * Usage / help
 * ================================================================ */

static void usage(const char *prog)
{
    fprintf(stderr,
        "Usage: %s [options] -- command [args...]\n"
        "\n"
        "Syscall User Dispatch (SUD) based process tracer.\n"
        "Produces a binary trace (see trace/trace.h).\n"
        "Decode with: tv dump FILE\n"
        "\n"
        "Options:\n"
        "  -o FILE                       write trace to FILE (default: stdout)\n"
        "  --no-env                      strip envp passed to the target program\n"
        "  --inramfs <mount>[:<mb>]      serve <mount> from an in-RAM inode store\n"
        "                                of <mb> megabytes (default 64)\n"
        "  --inramfs-key <key>           share an existing inramfs store across\n"
        "                                sudtrace invocations (default: mint a\n"
        "                                fresh key and unlink the backing shm on\n"
        "                                exit)\n"
        "  --overlay <merged>=<upper>+<lower>[+<lower>...]\n"
        "                                overlayfs-style merged view (repeatable;\n"
        "                                empty <upper> = read-only)\n"
        "  --remap <src>=<dst>           rewrite <src> → <dst> on every path\n"
        "                                argument (repeatable)\n"
        "  --passthrough <prefix>        carve <prefix> out of any wider\n"
        "                                --overlay/--remap rule: paths under\n"
        "                                <prefix> hit the kernel directly\n"
        "                                (repeatable; list before the wider\n"
        "                                rule it carves out of)\n"
        "  --fakeroot <prefix>           pretend the traced process is uid 0\n"
        "                                under <prefix>: chown/chmod succeed\n"
        "                                without touching the kernel, stat\n"
        "                                returns the recorded uid/gid/mode,\n"
        "                                geteuid()/getuid()/&c return 0\n"
        "                                process-wide (repeatable)\n"
        "  --no-fake-exec                disable the fake-exec addin (run\n"
        "                                trivial helpers like true/false/\n"
        "                                echo/printf/sh -c <trivial> through\n"
        "                                the kernel instead of eliding them)\n"
        "  --fake-exec-deny <basename>   keep fake-exec on but exclude one\n"
        "                                builtin from elision, e.g. 'echo'\n"
        "                                (repeatable)\n"
        "  --ccache <tool>               prepend <tool> (typically /usr/bin/\n"
        "                                ccache) to argv on every exec of a\n"
        "                                compiler.  Matches gcc/g++/cc/c++/\n"
        "                                clang/clang++ basenames plus the\n"
        "                                common cross-compiler globs (*-gcc,\n"
        "                                *-gcc-[0-9]*, etc.) for free.\n"
        "                                Auto-suppresses recursion: ccache\n"
        "                                exec'ing the real compiler does NOT\n"
        "                                re-trigger the wrap.\n"
        "  --ccache-also <pattern>       attach an extra glob pattern to\n"
        "                                --ccache's tool — for vendor blobs\n"
        "                                that don't follow the gcc naming\n"
        "                                convention.  <pattern> may be a\n"
        "                                bare glob ('ti-cgt-cl*') or a full\n"
        "                                'match-kind:pattern' (repeatable)\n"
        "  --exec-strip <pattern>        drop the matched wrapper command\n"
        "                                (sudo, fakeroot-ng, env, ...) and\n"
        "                                run its inner program directly.\n"
        "                                <pattern> defaults to a basename\n"
        "                                match; built-in flag-skip tables\n"
        "                                cover sudo / fakeroot-ng / env.\n"
        "                                (repeatable)\n"
        "  --exec-as <pat>:<uid>[:<gid>] when the matched program execs,\n"
        "                                propagate <uid>/<gid> as the new\n"
        "                                pretend uid/gid for the exec\n"
        "                                subtree.  Equivalent to running\n"
        "                                that command (and its descendants)\n"
        "                                under fakeroot.  (repeatable)\n"
        "  --pretend-uid <uid>           force getuid()/geteuid() to return\n"
        "                                <uid> process-wide (the existing\n"
        "                                fakeroot uid-getter short-circuit).\n"
        "                                Implies fakeroot active.\n"
        "  --pretend-gid <gid>           same, for getgid()/getegid().\n"
        "  --cmd-rule <kind>:<spec>      raw cmd-rewrite rule, repeatable.\n"
        "                                Kinds: compiler-wrap, exec-strip,\n"
        "                                exec-as.  See --ccache /\n"
        "                                --exec-strip / --exec-as for the\n"
        "                                friendlier shorthand forms.\n"
        "  --suppress-rule <name>        prevent the named cmd-rewrite rule\n"
        "                                from firing in this subtree.  Rule\n"
        "                                names are <kind>:<match>:<pattern>,\n"
        "                                e.g. 'compiler-wrap:basename:gcc'.\n"
        "                                (repeatable)\n",
        prog);
    exit(1);
}

/* ================================================================
 * Path helpers
 * ================================================================ */

static void make_absolute_path(const char *path, char *out, size_t out_sz)
{
    if (!path || !path[0]) { out[0] = '\0'; return; }
    if (path[0] == '/') { snprintf(out, out_sz, "%s", path); return; }

    char cwd[PATH_MAX];
    if (!getcwd(cwd, sizeof(cwd))) { snprintf(out, out_sz, "%s", path); return; }
    snprintf(out, out_sz, "%s/%s", cwd, path);
}

/* Find our own exe and derive the sud32/sud64 wrapper paths */
static char g_self_exe[PATH_MAX];
static char g_wrapper_32[PATH_MAX];
static char g_wrapper_64[PATH_MAX];

static void init_wrapper_paths(const char *argv0)
{
    ssize_t slen = readlink("/proc/self/exe", g_self_exe,
                            sizeof(g_self_exe) - 1);
    if (slen > 0)
        g_self_exe[slen] = '\0';
    else
        snprintf(g_self_exe, sizeof(g_self_exe), "%s", argv0);

    char *slash = strrchr(g_self_exe, '/');
    if (!slash) {
        /* No directory — assume wrappers in current dir */
        snprintf(g_wrapper_32, sizeof(g_wrapper_32), "sud32");
        snprintf(g_wrapper_64, sizeof(g_wrapper_64), "sud64");
        return;
    }

    int dirlen = (int)(slash - g_self_exe);
    snprintf(g_wrapper_32, sizeof(g_wrapper_32), "%.*s/sud32", dirlen, g_self_exe);
    snprintf(g_wrapper_64, sizeof(g_wrapper_64), "%.*s/sud64", dirlen, g_self_exe);
}

/* ================================================================
 * inramfs shm lifetime
 *
 * If the user gives us --inramfs but no --inramfs-key, we mint a
 * unique key, stash it on the runtime-config struct (so the addin
 * in the traced children attaches to the same shm files via the
 * --inramfs-key flag re-emitted on the wrapper argv), and remember
 * to unlink the backing files on launcher exit.  Net effect: a
 * sudtrace invocation owns its own backing shm and never leaves
 * `/dev/shm/sud-inramfs.*` clutter behind, while the explicit-key
 * workflow (multiple sudtrace invocations sharing the same mount)
 * still works for users who supply their own key.
 * ================================================================ */

static char g_ir_meta_path[PATH_MAX];
static char g_ir_smalldata_path[PATH_MAX];
static char g_ir_shm_prefix[PATH_MAX];   /* `/dev/shm/sud-inramfs.<key>.` */
static int  g_ir_owns_shm;              /* did we mint the key? */

/* Walk /dev/shm and unlink every entry whose basename starts with
 * `sud-inramfs.<key>.` — picks up the per-LARGE-file shms
 * (`.f.<idx>.<gen>`) that get created on the fly during a session in
 * addition to the launcher-known meta and smalldata shms.  Async-
 * signal-safe: only uses raw syscalls (open/getdents64/unlinkat/close)
 * via libc wrappers that boil down to the same. */
static void ir_unlink_per_file_shms(void)
{
    if (!g_ir_shm_prefix[0]) return;
    int dfd = open("/dev/shm", O_RDONLY | O_DIRECTORY);
    if (dfd < 0) return;
    /* Compose the basename prefix once: skip the "/dev/shm/" leading
     * component.  prefix_len is the length of `sud-inramfs.<key>.`. */
    const char *base_prefix = g_ir_shm_prefix + sizeof("/dev/shm/") - 1;
    size_t prefix_len = strlen(base_prefix);
    /* Use getdents64 directly to avoid pulling in opendir/readdir
     * machinery that may allocate; the listing is small (at most a
     * handful of files per session). */
    struct linux_dirent64 {
        unsigned long long d_ino;
        long long          d_off;
        unsigned short     d_reclen;
        unsigned char      d_type;
        char               d_name[];
    };
    char buf[4096];
    for (;;) {
        long n = syscall(SYS_getdents64, dfd, buf, sizeof(buf));
        if (n <= 0) break;
        for (long pos = 0; pos < n; ) {
            struct linux_dirent64 *de = (void *)(buf + pos);
            if (de->d_reclen == 0) break;
            if (strncmp(de->d_name, base_prefix, prefix_len) == 0)
                (void)unlinkat(dfd, de->d_name, 0);
            pos += de->d_reclen;
        }
    }
    close(dfd);
}

static void ir_unlink_backing(void)
{
    /* Idempotent: unlink and don't care about ENOENT. */
    if (g_ir_owns_shm) {
        if (g_ir_meta_path[0])      (void)unlink(g_ir_meta_path);
        if (g_ir_smalldata_path[0]) (void)unlink(g_ir_smalldata_path);
        ir_unlink_per_file_shms();
    }
}

static void ir_signal_cleanup(int sig)
{
    /* Async-signal-safe path: unlink (a syscall) is safe; we then
     * restore the default handler and re-raise so the process dies
     * with the right exit status / core-dump semantics. */
    ir_unlink_backing();
    struct sigaction sa = { 0 };
    sa.sa_handler = SIG_DFL;
    sigemptyset(&sa.sa_mask);
    sigaction(sig, &sa, NULL);
    raise(sig);
}

/* If the user wants inramfs but didn't pin a key, mint one and
 * register cleanup hooks.  Called once from main() before fork.
 * On return, *out_minted_key holds either NULL (if a key was already
 * pinned by the caller via --inramfs-key) or a malloc'd key string
 * the caller should put into cfg->inramfs_key. */
static char *ir_setup_owned_shm(const char *mount_path,
                                const char *existing_key)
{
    if (!mount_path || !mount_path[0]) return NULL;
    if (existing_key && existing_key[0]) return NULL;   /* user-managed key */

    /* Mint a key unique to this launcher: pid + monotonic ns gives
     * an unambiguous identifier across rapid invocations and across
     * pid wrap.  Hex-encoded for path safety. */
    struct timespec ts = { 0, 0 };
    clock_gettime(CLOCK_MONOTONIC, &ts);
    char key[64];
    snprintf(key, sizeof(key), "sudtrace-%d-%llx-%llx",
             (int)getpid(),
             (unsigned long long)ts.tv_sec,
             (unsigned long long)ts.tv_nsec);

    snprintf(g_ir_meta_path, sizeof(g_ir_meta_path),
             "/dev/shm/sud-inramfs.%s.meta", key);
    snprintf(g_ir_smalldata_path, sizeof(g_ir_smalldata_path),
             "/dev/shm/sud-inramfs.%s.smalldata", key);
    snprintf(g_ir_shm_prefix, sizeof(g_ir_shm_prefix),
             "/dev/shm/sud-inramfs.%s.", key);
    g_ir_owns_shm = 1;

    atexit(ir_unlink_backing);

    /* Catch the common termination signals so we still clean up
     * when the user hits ^C or kills us.  Use SA_RESETHAND so the
     * handler runs at most once; ir_signal_cleanup itself also
     * restores SIG_DFL and re-raises to preserve exit semantics
     * (e.g. shells care about WIFSIGNALED for ^C). */
    struct sigaction sa = { 0 };
    sa.sa_handler = ir_signal_cleanup;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags  = SA_RESETHAND;
    sigaction(SIGINT,  &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGHUP,  &sa, NULL);
    sigaction(SIGQUIT, &sa, NULL);

    return strdup(key);
}

/* ================================================================
 * ELF class detection — determine if target is 32-bit or 64-bit
 * to pick the correct wrapper.
 * ================================================================ */

static int detect_elf_class(const char *path)
{
    unsigned char ident[16];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, ident, sizeof(ident));
    close(fd);
    if (n < 16) return 0;
    /* Check ELF magic */
    if (ident[0] != 0x7f || ident[1] != 'E' ||
        ident[2] != 'L' || ident[3] != 'F')
        return 0;
    return ident[4]; /* EI_CLASS: 1=32bit, 2=64bit */
}

/* Check if a path is a script (#! line) and if so, detect the
 * ELF class of the interpreter. */
static int detect_target_class(const char *path)
{
    int cls = detect_elf_class(path);
    if (cls) return cls;

    /* Might be a script — read first line */
    char buf[512];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 2; /* default to 64-bit */
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n < 3 || buf[0] != '#' || buf[1] != '!') return 2;
    buf[n] = '\0';

    /* Extract interpreter path */
    char *p = buf + 2;
    while (*p == ' ' || *p == '\t') p++;
    char *end = p;
    while (*end && *end != ' ' && *end != '\t' && *end != '\n') end++;
    *end = '\0';

    cls = detect_elf_class(p);
    return cls ? cls : 2; /* default to 64-bit */
}

/* ================================================================
 * Wire-format event emission.
 *
 * The launcher is itself a wire producer: it writes the version atom
 * at the head of the stream and emits EXIT events for the children it
 * reaps. With the new per-process delta design (see sud/trace/event.c), the
 * launcher gets its own stream_id from the same shared atomic counter
 * the children use, and keeps its own process-local ev_state. No
 * cross-process lock involved.
 * ================================================================ */

static int g_out_fd = -1;

/* Shared page layout — must match `struct sud_shared` in sud/trace/event.c.
 * Only one field, the atomic stream-id counter; no lock anywhere. */
struct sud_shared_launcher {
    volatile uint32_t next_stream_id;
};
static struct sud_shared_launcher *g_shared;  /* NULL until set up */
static ev_state                    g_launcher_state;
static uint32_t                    g_launcher_stream_id;

static void get_timestamp(struct timespec *ts)
{
    clock_gettime(CLOCK_REALTIME, ts);
}

/* Read /proc/<pid>/status to get Tgid */
static pid_t get_tgid_for(pid_t pid)
{
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/status", pid);
    char buf[2048];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return pid;
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) return pid;
    buf[n] = '\0';
    char *tg = strstr(buf, "Tgid:");
    if (!tg) return pid;
    tg += 5;
    while (*tg == ' ' || *tg == '\t') tg++;
    pid_t tgid = 0;
    while (*tg >= '0' && *tg <= '9')
        tgid = tgid * 10 + (*tg++ - '0');
    return tgid > 0 ? tgid : pid;
}

static pid_t get_ppid_for(pid_t pid)
{
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/status", pid);
    char buf[2048];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) return 0;
    buf[n] = '\0';
    char *p = strstr(buf, "PPid:");
    if (!p) return 0;
    p += 5;
    while (*p == ' ' || *p == '\t') p++;
    pid_t ppid = 0;
    while (*p >= '0' && *p <= '9')
        ppid = ppid * 10 + (*p++ - '0');
    return ppid;
}

/* No more cross-process lock — emit_exit builds the v2 event into a
 * stack buffer and writes it with one syscall. */

static void write_all(int fd, const void *buf, size_t len)
{
    const char *p = buf;
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, p + off, len - off);
        if (n <= 0) break;
        off += (size_t)n;
    }
}

static void emit_exit(pid_t pid, int status)
{
    if (g_out_fd < 0 || !g_shared) return;

    pid_t tgid = get_tgid_for(pid);
    pid_t ppid = get_ppid_for(pid);
    struct timespec ts;
    get_timestamp(&ts);
    uint64_t ts_ns = (uint64_t)ts.tv_sec * 1000000000ull
                   + (uint64_t)ts.tv_nsec;

    int64_t extras[4];
    if (WIFEXITED(status)) {
        extras[0] = EV_EXIT_EXITED;
        extras[1] = WEXITSTATUS(status);
        extras[2] = 0;
        extras[3] = status;
    } else if (WIFSIGNALED(status)) {
        extras[0] = EV_EXIT_SIGNALED;
        extras[1] = WTERMSIG(status);
#ifdef WCOREDUMP
        extras[2] = WCOREDUMP(status) ? 1 : 0;
#else
        extras[2] = (status & 0x80) ? 1 : 0;
#endif
        extras[3] = status;
    } else {
        extras[0] = EV_EXIT_EXITED;
        extras[1] = 0;
        extras[2] = 0;
        extras[3] = status;
    }

    uint8_t hdr[EV_HEADER_MAX];
    Dst hd = wire_dst(hdr, sizeof hdr);
    ev_build_header(&g_launcher_state, &hd, g_launcher_stream_id,
                    EV_EXIT, ts_ns,
                    pid, tgid, ppid, pid, tgid,
                    extras, 4);
    if (hd.p) {
        size_t hlen = (size_t)(hd.p - hdr);
        uint8_t buf[EV_HEADER_MAX + 16];
        Dst od = wire_dst(buf, sizeof buf);
        wire_put_pair(&od, wire_src(hdr, hlen), wire_src(NULL, 0));
        if (od.p) {
            write_all(g_out_fd, buf, (size_t)((uint8_t *)od.p - buf));
        }
    }
}

/* ================================================================
 * Build the wrapper command line
 *
 * Given user's command, inspect the target binary, choose the
 * correct sud32/sud64 wrapper, and build the exec argv:
 *
 *   sud64 [--no-env] [--drop-argv N] /path/to/ld-linux.so /path/to/binary [args...]
 *
 * For static binaries:
 *   sud64 [--no-env] /path/to/binary [args...]
 *
 * For scripts:
 *   sud64 [--no-env] [--drop-argv N] /path/to/interp [script] [args...]
 * ================================================================ */

static char **build_wrapper_argv(int cmd_argc, char **cmd_argv,
                                 const struct sud_runtime_config *cfg)
{
    if (cmd_argc < 1 || !cmd_argv[0]) return NULL;

    /* Resolve the target path */
    char resolved[PATH_MAX];
    char *target = cmd_argv[0];
    if (target[0] != '/') {
        /* Search PATH */
        const char *pathenv = getenv("PATH");
        if (!pathenv) pathenv = "/usr/bin:/bin";
        int found = 0;
        while (*pathenv && !found) {
            const char *seg = pathenv;
            while (*pathenv && *pathenv != ':') pathenv++;
            int seglen = (int)(pathenv - seg);
            if (*pathenv == ':') pathenv++;
            snprintf(resolved, sizeof(resolved), "%.*s/%s", seglen, seg, target);
            if (access(resolved, X_OK) == 0) found = 1;
        }
        if (!found) {
            snprintf(resolved, sizeof(resolved), "%s", target);
        }
        target = resolved;
    } else {
        snprintf(resolved, sizeof(resolved), "%s", target);
    }

    /* Check for shebang */
    char shebang_interp[PATH_MAX] = "";
    char shebang_arg[PATH_MAX] = "";
    {
        char buf[512];
        int fd = open(resolved, O_RDONLY);
        if (fd >= 0) {
            ssize_t n = read(fd, buf, sizeof(buf) - 1);
            close(fd);
            if (n >= 3 && buf[0] == '#' && buf[1] == '!') {
                buf[n] = '\0';
                char *nl = strchr(buf, '\n');
                if (nl) *nl = '\0';
                char *p = buf + 2;
                while (*p == ' ' || *p == '\t') p++;
                char *iend = p;
                while (*iend && *iend != ' ' && *iend != '\t') iend++;
                if (*iend) {
                    *iend = '\0';
                    char *a = iend + 1;
                    while (*a == ' ' || *a == '\t') a++;
                    /* Trim trailing whitespace */
                    char *ae = a + strlen(a);
                    while (ae > a && (ae[-1] == ' ' || ae[-1] == '\t' ||
                                      ae[-1] == '\r'))
                        *--ae = '\0';
                    if (*a) snprintf(shebang_arg, sizeof(shebang_arg), "%s", a);
                }
                snprintf(shebang_interp, sizeof(shebang_interp), "%s", p);
            }
        }
    }

    /* Determine the actual executable to trace */
    const char *elf_path;
    int drop_count = 0;
    int extra_args = 0;
    char interp_buf[PATH_MAX] = "";
    char *extra_before[4] = {NULL};  /* Fixed: at most interp + arg + script */
    int extra_count = 0;

    if (shebang_interp[0]) {
        /* Script with shebang — trace through the interpreter */
        elf_path = shebang_interp;
        extra_before[extra_count++] = shebang_interp;
        if (shebang_arg[0])
            extra_before[extra_count++] = shebang_arg;
        extra_before[extra_count++] = resolved;
        /* Preserve the interpreter argv layout so the script executes with the
         * same argv the kernel would synthesize for a shebang launch. */
        drop_count = 0;
    } else {
        /* ELF binary — check if dynamic */
        int elf_class = 0;
        int fd = open(resolved, O_RDONLY);
        if (fd >= 0) {
            unsigned char ehdr[64];
            ssize_t n = read(fd, ehdr, sizeof(ehdr));
            if (n >= 20 && ehdr[0] == 0x7f && ehdr[1] == 'E' &&
                ehdr[2] == 'L' && ehdr[3] == 'F') {
                elf_class = ehdr[4];
                /* Check for dynamic linker (PT_INTERP) */
                /* Simplified: read the phdrs to find PT_INTERP */
                lseek(fd, 0, SEEK_SET);
                /* Just use the existing detect mechanism for the class;
                 * the wrapper will do the actual elf inspection */
            }
            close(fd);
            if (!elf_class) elf_class = 2; /* default 64-bit */
        }
        elf_path = resolved;
    }

    /* Determine the target ELF class */
    int target_class = detect_target_class(elf_path);
    const char *wrapper = (target_class == 1) ? g_wrapper_32 : g_wrapper_64;

    /* Build argv:
     *   wrapper [config flag block] extra_before... cmd_argv[0] cmd_argv[1:]
     *
     * The flag block is generated by sud_runtime_config_emit() so the
     * shape stays in sync with the wrapper-side parser.  We make a
     * local copy of cfg so we can override drop_count for the script-
     * vs-binary distinction without mutating the caller's config. */
    struct sud_runtime_config local_cfg = *cfg;
    if (extra_count > 0) local_cfg.drop_count = drop_count;
    else                 local_cfg.drop_count = 0;

    const char *flag_buf[SUD_RC_MAX_EMIT_ARGS];
    char        int_scratch[64];
    int n_flags = sud_runtime_config_emit(&local_cfg, flag_buf,
                                          SUD_RC_MAX_EMIT_ARGS,
                                          int_scratch, sizeof(int_scratch));
    if (n_flags < 0) return NULL;

    int max_args = 1 + n_flags + extra_count + cmd_argc + 1;
    char **args = calloc(max_args, sizeof(char *));
    if (!args) return NULL;
    int idx = 0;

    args[idx++] = strdup(wrapper);
    for (int i = 0; i < n_flags; i++)
        args[idx++] = strdup(flag_buf[i]);

    /* The wrapper itself handles ELF inspection and prepending ld-linux.
     * We just pass through the resolved target + original args.
     * For scripts, we prepend the interpreter. */
    if (extra_count > 0) {
        for (int i = 0; i < extra_count; i++)
            args[idx++] = strdup(extra_before[i]);
        /* Add the remaining original args (skip argv[0] since script path
         * is already included via extra_before) */
        for (int i = 1; i < cmd_argc; i++)
            args[idx++] = strdup(cmd_argv[i]);
    } else {
        /* Just pass through to the wrapper — it handles ELF inspection */
        for (int i = 0; i < cmd_argc; i++)
            args[idx++] = strdup(cmd_argv[i]);
    }
    args[idx] = NULL;
    return args;
}

/* ================================================================
 * Main — sudtrace launcher
 * ================================================================ */

int main(int argc, char **argv)
{
    init_wrapper_paths(argv[0]);

    /* Parse options.  All configuration enters via flags here —
     * sudtrace no longer reads any SUD_* environment variable.
     * (The PATH lookup in build_wrapper_argv is the only remaining
     * getenv in this file and is for resolving the user's command,
     * not sud configuration.) */
    const char *outfile = NULL;
    int cmd_start = -1;
    int no_env = 0;
    const char *cli_inramfs = NULL;       /* "<mount>" or "<mount>:<mb>" */
    const char *cli_inramfs_key = NULL;
    const char *cli_overlay[SUD_RC_MAX_REMAP_RULES];
    int         cli_overlay_n = 0;
    const char *cli_remap[SUD_RC_MAX_REMAP_RULES];
    int         cli_remap_n = 0;
    const char *cli_passthrough[SUD_RC_MAX_REMAP_RULES];
    int         cli_passthrough_n = 0;
    const char *cli_fakeroot[SUD_RC_MAX_REMAP_RULES];
    int         cli_fakeroot_n = 0;
    int         cli_no_fake_exec = 0;
    const char *cli_fake_exec_deny[SUD_RC_MAX_FAKE_EXEC_DENY];
    int         cli_fake_exec_deny_n = 0;
    /* cmd-rewrite addin flags. */
    const char *cli_cmd_rule[SUD_RC_MAX_CMD_RULES];
    int         cli_cmd_rule_n = 0;
    const char *cli_suppress[SUD_RC_MAX_SUPPRESS];
    int         cli_suppress_n = 0;
    const char *cli_ccache_tool = NULL;            /* --ccache <tool> */
    const char *cli_ccache_extra[SUD_RC_MAX_CMD_RULES];
    int         cli_ccache_extra_n = 0;
    const char *cli_exec_strip[SUD_RC_MAX_CMD_RULES];
    int         cli_exec_strip_n = 0;
    const char *cli_exec_as[SUD_RC_MAX_CMD_RULES];
    int         cli_exec_as_n = 0;
    int         cli_pretend_uid = -1;
    int         cli_pretend_gid = -1;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--") == 0) {
            cmd_start = i + 1;
            break;
        }
        if (strcmp(argv[i], "-o") == 0 && i + 1 < argc) {
            outfile = argv[++i];
        } else if (strcmp(argv[i], "--no-env") == 0) {
            no_env = 1;
        } else if (strcmp(argv[i], "--inramfs") == 0 && i + 1 < argc) {
            cli_inramfs = argv[++i];
        } else if (strcmp(argv[i], "--inramfs-key") == 0 && i + 1 < argc) {
            cli_inramfs_key = argv[++i];
        } else if (strcmp(argv[i], "--overlay") == 0 && i + 1 < argc) {
            if (cli_overlay_n >= SUD_RC_MAX_REMAP_RULES) {
                fprintf(stderr, "sudtrace: too many --overlay rules "
                                "(max %d)\n", SUD_RC_MAX_REMAP_RULES);
                exit(1);
            }
            cli_overlay[cli_overlay_n++] = argv[++i];
        } else if (strcmp(argv[i], "--remap") == 0 && i + 1 < argc) {
            if (cli_remap_n >= SUD_RC_MAX_REMAP_RULES) {
                fprintf(stderr, "sudtrace: too many --remap rules "
                                "(max %d)\n", SUD_RC_MAX_REMAP_RULES);
                exit(1);
            }
            cli_remap[cli_remap_n++] = argv[++i];
        } else if (strcmp(argv[i], "--passthrough") == 0 && i + 1 < argc) {
            if (cli_passthrough_n >= SUD_RC_MAX_REMAP_RULES) {
                fprintf(stderr, "sudtrace: too many --passthrough rules "
                                "(max %d)\n", SUD_RC_MAX_REMAP_RULES);
                exit(1);
            }
            cli_passthrough[cli_passthrough_n++] = argv[++i];
        } else if (strcmp(argv[i], "--fakeroot") == 0 && i + 1 < argc) {
            if (cli_fakeroot_n >= SUD_RC_MAX_REMAP_RULES) {
                fprintf(stderr, "sudtrace: too many --fakeroot rules "
                                "(max %d)\n", SUD_RC_MAX_REMAP_RULES);
                exit(1);
            }
            cli_fakeroot[cli_fakeroot_n++] = argv[++i];
        } else if (strcmp(argv[i], "--no-fake-exec") == 0) {
            cli_no_fake_exec = 1;
        } else if (strcmp(argv[i], "--fake-exec-deny") == 0 && i + 1 < argc) {
            if (cli_fake_exec_deny_n >= SUD_RC_MAX_FAKE_EXEC_DENY) {
                fprintf(stderr, "sudtrace: too many --fake-exec-deny rules "
                                "(max %d)\n", SUD_RC_MAX_FAKE_EXEC_DENY);
                exit(1);
            }
            cli_fake_exec_deny[cli_fake_exec_deny_n++] = argv[++i];
        } else if (strcmp(argv[i], "--cmd-rule") == 0 && i + 1 < argc) {
            if (cli_cmd_rule_n >= SUD_RC_MAX_CMD_RULES) {
                fprintf(stderr, "sudtrace: too many --cmd-rule rules "
                                "(max %d)\n", SUD_RC_MAX_CMD_RULES);
                exit(1);
            }
            cli_cmd_rule[cli_cmd_rule_n++] = argv[++i];
        } else if (strcmp(argv[i], "--suppress-rule") == 0 && i + 1 < argc) {
            if (cli_suppress_n >= SUD_RC_MAX_SUPPRESS) {
                fprintf(stderr, "sudtrace: too many --suppress-rule entries "
                                "(max %d)\n", SUD_RC_MAX_SUPPRESS);
                exit(1);
            }
            cli_suppress[cli_suppress_n++] = argv[++i];
        } else if (strcmp(argv[i], "--ccache") == 0 && i + 1 < argc) {
            cli_ccache_tool = argv[++i];
        } else if (strcmp(argv[i], "--ccache-also") == 0 && i + 1 < argc) {
            if (cli_ccache_extra_n >= SUD_RC_MAX_CMD_RULES) {
                fprintf(stderr, "sudtrace: too many --ccache-also patterns "
                                "(max %d)\n", SUD_RC_MAX_CMD_RULES);
                exit(1);
            }
            cli_ccache_extra[cli_ccache_extra_n++] = argv[++i];
        } else if (strcmp(argv[i], "--exec-strip") == 0 && i + 1 < argc) {
            if (cli_exec_strip_n >= SUD_RC_MAX_CMD_RULES) {
                fprintf(stderr, "sudtrace: too many --exec-strip rules "
                                "(max %d)\n", SUD_RC_MAX_CMD_RULES);
                exit(1);
            }
            cli_exec_strip[cli_exec_strip_n++] = argv[++i];
        } else if (strcmp(argv[i], "--exec-as") == 0 && i + 1 < argc) {
            if (cli_exec_as_n >= SUD_RC_MAX_CMD_RULES) {
                fprintf(stderr, "sudtrace: too many --exec-as rules "
                                "(max %d)\n", SUD_RC_MAX_CMD_RULES);
                exit(1);
            }
            cli_exec_as[cli_exec_as_n++] = argv[++i];
        } else if (strcmp(argv[i], "--pretend-uid") == 0 && i + 1 < argc) {
            cli_pretend_uid = atoi(argv[++i]);
        } else if (strcmp(argv[i], "--pretend-gid") == 0 && i + 1 < argc) {
            cli_pretend_gid = atoi(argv[++i]);
        } else if (strcmp(argv[i], "-h") == 0 ||
                   strcmp(argv[i], "--help") == 0) {
            usage(argv[0]);
        } else if (argv[i][0] == '-' && argv[i][1] != '\0') {
            fprintf(stderr, "sudtrace: unknown option '%s'\n", argv[i]);
            usage(argv[0]);
        } else {
            cmd_start = i;
            break;
        }
    }

    if (cmd_start < 0 || cmd_start >= argc)
        usage(argv[0]);

    /* Build the runtime config we'll hand to the wrapper.  Every
     * value comes from a flag we just parsed; sudtrace never calls
     * getenv/setenv/unsetenv on SUD_* (PLAN.md Part 2 invariant). */
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.no_env = no_env;

    /* --inramfs <mount>[:<mb>]  →  "--remap-rule inramfs:<mount>"
     * (+ "--inramfs-meta-mb <N>" if a size was given).  The mount
     * spec is forwarded verbatim so the wrapper-side path_remap
     * parser sees exactly the user's text. */
    char *inramfs_mount = NULL;
    if (cli_inramfs && cli_inramfs[0]) {
        const char *e = cli_inramfs;
        const char *colon = strchr(e, ':');
        size_t plen = colon ? (size_t)(colon - e) : strlen(e);
        inramfs_mount = strndup(e, plen);
        char *rule = malloc(plen + sizeof("inramfs:") + 1);
        if (!rule) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
        memcpy(rule, "inramfs:", sizeof("inramfs:") - 1);
        memcpy(rule + sizeof("inramfs:") - 1, e, plen);
        rule[sizeof("inramfs:") - 1 + plen] = '\0';
        if (cfg.remap_rule_count < SUD_RC_MAX_REMAP_RULES)
            cfg.remap_rules[cfg.remap_rule_count++] = rule;

        if (colon && colon[1]) {
            int mb = atoi(colon + 1);
            if (mb > 0) cfg.inramfs_meta_mb = mb;
        }
    }

    /* --inramfs-key.  If the user did not pin a key, ir_setup_owned_shm()
     * mints one for us (and registers cleanup of the backing shms). */
    if (cli_inramfs_key && cli_inramfs_key[0])
        cfg.inramfs_key = strdup(cli_inramfs_key);
    char *minted_key = ir_setup_owned_shm(inramfs_mount, cfg.inramfs_key);
    if (minted_key) cfg.inramfs_key = minted_key;

    /* --passthrough <prefix>  (repeatable)  →  "--remap-rule passthrough:<prefix>".
     * Emitted BEFORE --overlay / --remap so find_rule()'s first-match-wins
     * loop picks the passthrough rule when a path lies in a sub-prefix
     * carved out of a wider overlay/remap. */
    for (int i = 0; i < cli_passthrough_n; i++) {
        const char *spec = cli_passthrough[i];
        size_t slen = strlen(spec);
        if (slen == 0 ||
            cfg.remap_rule_count >= SUD_RC_MAX_REMAP_RULES) continue;
        char *rule = malloc(slen + sizeof("passthrough:") + 1);
        if (!rule) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
        memcpy(rule, "passthrough:", sizeof("passthrough:") - 1);
        memcpy(rule + sizeof("passthrough:") - 1, spec, slen + 1);
        cfg.remap_rules[cfg.remap_rule_count++] = rule;
    }

    /* --overlay <spec>  (repeatable)  →  "--remap-rule overlay:<spec>". */
    for (int i = 0; i < cli_overlay_n; i++) {
        const char *spec = cli_overlay[i];
        size_t slen = strlen(spec);
        if (slen == 0 ||
            cfg.remap_rule_count >= SUD_RC_MAX_REMAP_RULES) continue;
        char *rule = malloc(slen + sizeof("overlay:") + 1);
        if (!rule) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
        memcpy(rule, "overlay:", sizeof("overlay:") - 1);
        memcpy(rule + sizeof("overlay:") - 1, spec, slen + 1);
        cfg.remap_rules[cfg.remap_rule_count++] = rule;
    }

    /* --fakeroot <prefix>  (repeatable)  →  "--remap-rule fakeroot:<prefix>".
     * Path resolution under fakeroot is pure passthrough; what changes is
     * metadata (chown/chmod recorded, stat patched, geteuid/&c forced to 0).
     * Emitted ahead of --remap so a path that lives under both a fakeroot
     * prefix and a remap rule still sees the remap (the addins consume
     * fakeroot rules independently of overlay's first-match-wins loop). */
    for (int i = 0; i < cli_fakeroot_n; i++) {
        const char *spec = cli_fakeroot[i];
        size_t slen = strlen(spec);
        if (slen == 0 ||
            cfg.remap_rule_count >= SUD_RC_MAX_REMAP_RULES) continue;
        char *rule = malloc(slen + sizeof("fakeroot:") + 1);
        if (!rule) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
        memcpy(rule, "fakeroot:", sizeof("fakeroot:") - 1);
        memcpy(rule + sizeof("fakeroot:") - 1, spec, slen + 1);
        cfg.remap_rules[cfg.remap_rule_count++] = rule;
    }

    /* --remap <src>=<dst>  (repeatable)  →  "--remap-rule remap:<spec>". */
    for (int i = 0; i < cli_remap_n; i++) {
        const char *spec = cli_remap[i];
        size_t slen = strlen(spec);
        if (slen == 0 ||
            cfg.remap_rule_count >= SUD_RC_MAX_REMAP_RULES) continue;
        char *rule = malloc(slen + sizeof("remap:") + 1);
        if (!rule) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
        memcpy(rule, "remap:", sizeof("remap:") - 1);
        memcpy(rule + sizeof("remap:") - 1, spec, slen + 1);
        cfg.remap_rules[cfg.remap_rule_count++] = rule;
    }

    /* --no-fake-exec  →  "--fake-exec off" on the wrapper argv.
     * --fake-exec-deny <basename>  (repeatable)  →  forwarded verbatim.
     * Both flags only matter when the fake-exec addin is compiled into
     * the wrapper; the parser ignores unknown values harmlessly when
     * it isn't.  The default (fake_exec_off = 0) leaves the addin
     * active. */
    cfg.fake_exec_off = cli_no_fake_exec;
    for (int i = 0; i < cli_fake_exec_deny_n; i++) {
        if (cfg.fake_exec_deny_count >= SUD_RC_MAX_FAKE_EXEC_DENY) break;
        cfg.fake_exec_deny[cfg.fake_exec_deny_count++] =
            strdup(cli_fake_exec_deny[i]);
    }

    /* cmd-rewrite addin wiring.  --cmd-rule passes through verbatim;
     * --suppress-rule too.  --ccache, --exec-strip, --exec-as expand
     * into one or more --cmd-rule strings each.
     *
     * --ccache <tool>    → compiler-wrap rules for the canonical
     *                      compiler basenames (gcc/g++/cc/c++/clang/
     *                      clang++) plus their version-suffix and
     *                      cross-target variants via globs
     *                      (*-gcc, *-gcc-[0-9]*, etc).  Covers the
     *                      vendor-cross-compiler case without
     *                      needing per-toolchain configuration.
     * --ccache-also <pat> attaches one extra glob pattern to the
     *                      same tool (e.g. "ti-cgt-cl*" for vendors
     *                      that don't follow the gcc naming
     *                      convention). */
    static const char *const ccache_default_globs[] = {
        "basename:gcc",  "basename:g++",  "basename:cc",  "basename:c++",
        "basename:clang", "basename:clang++",
        "glob:*-gcc",     "glob:*-g++",
        "glob:*-gcc-[0-9]*",  "glob:*-g++-[0-9]*",
        "glob:gcc-[0-9]*",    "glob:g++-[0-9]*",
        "glob:*-cc",          "glob:*-clang",
    };
    if (cli_ccache_tool) {
        for (size_t i = 0;
             i < sizeof(ccache_default_globs)/sizeof(ccache_default_globs[0]);
             i++) {
            if (cfg.cmd_rule_count >= SUD_RC_MAX_CMD_RULES) break;
            const char *m = ccache_default_globs[i];
            size_t mlen = strlen(m), tlen = strlen(cli_ccache_tool);
            char *r = malloc(sizeof("compiler-wrap:") - 1 + mlen + 1 + tlen + 1);
            if (!r) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
            char *p = r;
            memcpy(p, "compiler-wrap:", sizeof("compiler-wrap:") - 1);
            p += sizeof("compiler-wrap:") - 1;
            memcpy(p, m, mlen); p += mlen;
            *p++ = ':';
            memcpy(p, cli_ccache_tool, tlen + 1);
            cfg.cmd_rules[cfg.cmd_rule_count++] = r;
        }
        for (int i = 0; i < cli_ccache_extra_n; i++) {
            if (cfg.cmd_rule_count >= SUD_RC_MAX_CMD_RULES) break;
            const char *spec = cli_ccache_extra[i];
            /* When the spec's leading token isn't a match-kind keyword,
             * default to glob (the most useful for cross-compilers). */
            const char *colon2 = strchr(spec, ':');
            size_t klen2 = colon2 ? (size_t)(colon2 - spec) : 0;
            int long_form = colon2 && (
                (klen2 == 8 && strncmp(spec, "basename", 8) == 0) ||
                (klen2 == 4 && strncmp(spec, "glob",     4) == 0) ||
                (klen2 == 4 && strncmp(spec, "path",     4) == 0));
            const char *prefix = long_form ? "compiler-wrap:" : "compiler-wrap:glob:";
            size_t plen = strlen(prefix);
            size_t slen = strlen(spec);
            size_t tlen = strlen(cli_ccache_tool);
            char *r = malloc(plen + slen + 1 + tlen + 1);
            if (!r) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
            char *p = r;
            memcpy(p, prefix, plen); p += plen;
            memcpy(p, spec, slen); p += slen;
            *p++ = ':';
            memcpy(p, cli_ccache_tool, tlen + 1);
            cfg.cmd_rules[cfg.cmd_rule_count++] = r;
        }
    }

    /* --exec-strip <pattern> shorthand: when the spec's leading
     * token (up to first ':') isn't a match-kind keyword, prepend
     * "exec-strip:basename:".  So `--exec-strip sudo` →
     * "exec-strip:basename:sudo", but
     * `--exec-strip glob:fakeroot-*` → "exec-strip:glob:fakeroot-*". */
    for (int i = 0; i < cli_exec_strip_n; i++) {
        if (cfg.cmd_rule_count >= SUD_RC_MAX_CMD_RULES) break;
        const char *spec = cli_exec_strip[i];
        const char *colon = strchr(spec, ':');
        size_t klen = colon ? (size_t)(colon - spec) : 0;
        int long_form = colon && (
            (klen == 8 && strncmp(spec, "basename", 8) == 0) ||
            (klen == 4 && strncmp(spec, "glob",     4) == 0) ||
            (klen == 4 && strncmp(spec, "path",     4) == 0));
        const char *prefix = long_form ? "exec-strip:" : "exec-strip:basename:";
        size_t plen = strlen(prefix), slen = strlen(spec);
        char *r = malloc(plen + slen + 1);
        if (!r) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
        char *p = r;
        memcpy(p, prefix, plen); p += plen;
        memcpy(p, spec, slen + 1);
        cfg.cmd_rules[cfg.cmd_rule_count++] = r;
    }

    /* --exec-as <pattern>:<uid>[:<gid>] (shorthand) or
     * --exec-as <match-kind>:<pattern>:<uid>[:<gid>] (long form).
     *
     * We treat the spec as long-form iff the first token (up to the
     * first `:`) is exactly "basename", "glob" or "path"; otherwise
     * it's the shorthand and we prepend "exec-as:basename:". */
    for (int i = 0; i < cli_exec_as_n; i++) {
        if (cfg.cmd_rule_count >= SUD_RC_MAX_CMD_RULES) break;
        const char *spec = cli_exec_as[i];
        const char *colon = strchr(spec, ':');
        size_t klen = colon ? (size_t)(colon - spec) : 0;
        int long_form = colon && (
            (klen == 8 && strncmp(spec, "basename", 8) == 0) ||
            (klen == 4 && strncmp(spec, "glob",     4) == 0) ||
            (klen == 4 && strncmp(spec, "path",     4) == 0));
        const char *prefix = long_form ? "exec-as:" : "exec-as:basename:";
        size_t plen = strlen(prefix), slen = strlen(spec);
        char *r = malloc(plen + slen + 1);
        if (!r) { fprintf(stderr, "sudtrace: oom\n"); exit(1); }
        char *p = r;
        memcpy(p, prefix, plen); p += plen;
        memcpy(p, spec, slen + 1);
        cfg.cmd_rules[cfg.cmd_rule_count++] = r;
    }

    /* --cmd-rule entries (verbatim). */
    for (int i = 0; i < cli_cmd_rule_n; i++) {
        if (cfg.cmd_rule_count >= SUD_RC_MAX_CMD_RULES) break;
        cfg.cmd_rules[cfg.cmd_rule_count++] = strdup(cli_cmd_rule[i]);
    }

    /* --suppress-rule entries (verbatim). */
    for (int i = 0; i < cli_suppress_n; i++) {
        if (cfg.suppress_count >= SUD_RC_MAX_SUPPRESS) break;
        cfg.suppressed[cfg.suppress_count++] = strdup(cli_suppress[i]);
    }

    cfg.pretend_uid = cli_pretend_uid;
    cfg.pretend_gid = cli_pretend_gid;

    /* Setup output */
    char abs_out[PATH_MAX];
    if (outfile) {
        make_absolute_path(outfile, abs_out, sizeof(abs_out));
        int ofd = open(abs_out, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (ofd < 0) { perror("sudtrace: open output"); exit(1); }
        g_out_fd = ofd;
        cfg.trace_outfile = abs_out;
    } else {
        g_out_fd = STDOUT_FILENO;
    }

    /* Move output to a high fd so children don't clobber it */
    {
        int high_fd = dup2(g_out_fd, SUD_OUTPUT_FD);
        if (high_fd >= 0) {
            if (g_out_fd != STDOUT_FILENO && g_out_fd != STDERR_FILENO)
                close(g_out_fd);
            g_out_fd = high_fd;
            /* NOT FD_CLOEXEC: wrapper child inherits the fd */
        }
    }

    /* Set up the shared wire state page.
     *
     * memfd_create gives us an anonymous file that survives fork+exec.
     * The page now holds only an atomic stream-id counter (no lock, no
     * shared ev_state — see sud/trace/event.c). We dup it to SUD_STATE_FD
     * so every traced child inherits the fd and grabs its own
     * stream_id via sud_wire_init(). The launcher does the same so
     * its own EXIT events sit on a distinct stream. */
    {
        int mfd = (int)syscall(SYS_memfd_create, "sud_wire_state", 0u);
        if (mfd < 0) {
            perror("sudtrace: memfd_create");
            exit(1);
        }
        if (ftruncate(mfd, SUD_STATE_PAGE_SIZE) < 0) {
            perror("sudtrace: ftruncate state");
            exit(1);
        }
        int sfd = dup2(mfd, SUD_STATE_FD);
        if (sfd < 0) { perror("sudtrace: dup2 state"); exit(1); }
        if (mfd != SUD_STATE_FD) close(mfd);
        void *p = mmap(NULL, SUD_STATE_PAGE_SIZE, PROT_READ | PROT_WRITE,
                       MAP_SHARED, sfd, 0);
        if (p == MAP_FAILED) {
            perror("sudtrace: mmap state");
            exit(1);
        }
        g_shared = (struct sud_shared_launcher *)p;
        /* ftruncate zero-fills, so next_stream_id starts at 0; first
         * fetch-and-add returns 0 and we use the post-increment value
         * (1) as our stream_id. Children then get 2, 3, … */
        g_launcher_stream_id =
            __sync_fetch_and_add(&g_shared->next_stream_id, 1u) + 1u;
    }

    /* Write the trace version atom once at the head of the output. */
    {
        uint8_t buf[16];
        Dst d = wire_dst(buf, sizeof buf);
        wire_put_u64(&d, TRACE_VERSION);
        if (d.p) {
            write_all(g_out_fd, buf, (size_t)((uint8_t *)d.p - buf));
        }
    }

    /* Build wrapper command */
    int cmd_argc = argc - cmd_start;
    char **exec_argv = build_wrapper_argv(cmd_argc, argv + cmd_start, &cfg);
    if (!exec_argv) {
        fprintf(stderr, "sudtrace: failed to build wrapper command\n");
        exit(1);
    }

    /* Fork the child */
    pid_t child = fork();
    if (child < 0) { perror("sudtrace: fork"); exit(1); }

    if (child == 0) {
        execv(exec_argv[0], exec_argv);
        perror("sudtrace: exec");
        _exit(127);
    }

    /* Free exec_argv */
    for (int i = 0; exec_argv[i]; i++)
        free(exec_argv[i]);
    free(exec_argv);

    /* Main wait loop — emit EXIT events for reaped children.
     *
     * waitpid(-1, ..., __WALL) blocks until any descendant changes
     * state. We only care about real terminations (WIFEXITED /
     * WIFSIGNALED); anything else (WIFSTOPPED, WIFCONTINUED, the
     * occasional 0 return some kernels produce on tracee races) is
     * a no-op — keep waiting.
     *
     * If we treat non-terminating events as terminations the launcher
     * either spins (the kernel keeps re-delivering the same stop
     * notification while we re-check the same condition) or, worse,
     * exits before the original child has actually finished. */
    for (;;) {
        int wstatus = 0;
        pid_t wpid = waitpid(-1, &wstatus, __WALL);
        if (wpid < 0) {
            if (errno == EINTR) continue;
            /* ECHILD = nothing left to reap; any other errno is a
             * permanent failure. Either way, exit the loop. */
            break;
        }
        if (wpid == 0) continue;            /* should not happen without WNOHANG */
        if (!WIFEXITED(wstatus) && !WIFSIGNALED(wstatus))
            continue;                       /* WIFSTOPPED / WIFCONTINUED */

        pid_t tgid = get_tgid_for(wpid);
        if (wpid == tgid || wpid == child)
            emit_exit(wpid, wstatus);

        if (wpid == child) break;
    }

    if (g_out_fd >= 0 && g_out_fd != STDOUT_FILENO &&
        g_out_fd != STDERR_FILENO)
        close(g_out_fd);

    return 0;
}
