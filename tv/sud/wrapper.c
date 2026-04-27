/*
 * sud/wrapper.c — Freestanding entry point for sud32/sud64.
 *
 * This is the SOLE purpose of the sud32/sud64 binaries:
 *   1. Parse the wrapper-specific command line
 *   2. Resolve the output FD
 *   3. Load the target ELF and jump to it with SUD enabled
 *
 * Built with -nostdlib -ffreestanding.  No libc startup code runs.
 * The only entry point is _start → sudmini_start_c → main().
 * All libc-like functions come from sud/libc.c (mini freestanding libc).
 * Formatted output comes from deps/printf (mpaland/printf).
 *
 * This program does NOT parse "-o", "--help", or fork children.
 * That is the job of the separate sudtrace launcher (sud/sudtrace.c).
 */

#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "libc-fs/fmt.h"
#include "sud/elf.h"
#include "sud/handler.h"
#include "sud/loader.h"
#include "sud/addin.h"
#include "sud/state.h"

/* ================================================================
 * Wrapper argument parsing
 *
 * The wrapper is invoked by sudtrace as:
 *   sud64 [--no-env] [--drop-argv N] /path/to/binary [args...]
 *
 * All arguments are positional; no flag parsing ambiguity.
 * ================================================================ */

static void init_wrapper_paths(void)
{
    /* Resolve our own path */
    char buf[PATH_MAX];
    ssize_t n = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
    if (n > 0) {
        buf[n] = '\0';
        /* Copy to g_self_exe */
        int i = 0;
        while (buf[i] && i < (int)sizeof(g_self_exe) - 1) {
            g_self_exe[i] = buf[i];
            i++;
        }
        g_self_exe[i] = '\0';
    }

    /* Derive sud32/sud64 paths from our location */
    char *slash = (void *)0;
    {
        char *p = g_self_exe;
        while (*p) { if (*p == '/') slash = p; p++; }
    }
    if (!slash) {
        /* No directory component — set self as the native variant */
        if (SUD_NATIVE_ELF_CLASS == ELFCLASS32) {
            int i = 0;
            while (g_self_exe[i] && i < (int)sizeof(g_self_exe32) - 1) {
                g_self_exe32[i] = g_self_exe[i]; i++;
            }
            g_self_exe32[i] = '\0';
        } else {
            int i = 0;
            while (g_self_exe[i] && i < (int)sizeof(g_self_exe64) - 1) {
                g_self_exe64[i] = g_self_exe[i]; i++;
            }
            g_self_exe64[i] = '\0';
        }
        return;
    }

    int dirlen = (int)(slash - g_self_exe);

    /* Build sud32 path */
    memcpy(g_self_exe32, g_self_exe, dirlen);
    g_self_exe32[dirlen] = '\0';
    {
        const char *suf = "/sud32";
        int i = 0;
        while (suf[i] && dirlen + i < (int)sizeof(g_self_exe32) - 1) {
            g_self_exe32[dirlen + i] = suf[i]; i++;
        }
        g_self_exe32[dirlen + i] = '\0';
    }

    /* Build sud64 path */
    memcpy(g_self_exe64, g_self_exe, dirlen);
    g_self_exe64[dirlen] = '\0';
    {
        const char *suf = "/sud64";
        int i = 0;
        while (suf[i] && dirlen + i < (int)sizeof(g_self_exe64) - 1) {
            g_self_exe64[dirlen + i] = suf[i]; i++;
        }
        g_self_exe64[dirlen + i] = '\0';
    }

    /* Verify accessibility, fall back to self if not found */
    if (raw_access(g_self_exe32, 1/*X_OK*/) != 0 &&
        SUD_NATIVE_ELF_CLASS == ELFCLASS32) {
        int i = 0;
        while (g_self_exe[i] && i < (int)sizeof(g_self_exe32) - 1) {
            g_self_exe32[i] = g_self_exe[i]; i++;
        }
        g_self_exe32[i] = '\0';
    }
    if (raw_access(g_self_exe64, 1/*X_OK*/) != 0 &&
        SUD_NATIVE_ELF_CLASS == ELFCLASS64) {
        int i = 0;
        while (g_self_exe[i] && i < (int)sizeof(g_self_exe64) - 1) {
            g_self_exe64[i] = g_self_exe[i]; i++;
        }
        g_self_exe64[i] = '\0';
    }
}

static void init_path_env(void)
{
    /* Cache $PATH for resolve_path().
     * PATH has no kernel-imposed length limit — build environments
     * (Nix, Guix, complex CI) routinely produce very long values.
     * Allocate exactly the right size via strdup (which uses mmap
     * in our freestanding malloc). */
    const char *path = getenv("PATH");
    if (!path || !path[0])
        path = "/usr/bin:/bin";
    g_path_env = strdup(path);
}

/* ================================================================
 * Rewrite /proc/self/cmdline
 *
 * Overwrite the original argv area with the visible argv so that
 * /proc/self/cmdline matches what the target process sees.
 * ================================================================ */
static void rewrite_cmdline(int orig_argc, char **orig_argv,
                            int vis_argc, char **vis_argv)
{
    if (vis_argc <= 0 || !vis_argv || !orig_argv) return;

    char *area_start = orig_argv[0];
    char *area_end = orig_argv[orig_argc - 1] +
                     strlen(orig_argv[orig_argc - 1]) + 1;
    size_t area_size = area_end - area_start;

    size_t off = 0;
    for (int i = 0; i < vis_argc && off < area_size; i++) {
        size_t len = strlen(vis_argv[i]) + 1;
        if (off + len > area_size) {
            size_t fit = area_size - off;
            memcpy(area_start + off, vis_argv[i], fit);
            area_start[area_size - 1] = '\0';
            off = area_size;
            break;
        }
        memcpy(area_start + off, vis_argv[i], len);
        off += len;
    }
    if (off < area_size)
        memset(area_start + off, 0, area_size - off);
}

/* ================================================================
 * main — wrapper entry point
 * ================================================================ */
int main(int argc, char **argv)
{
    init_wrapper_paths();
    init_path_env();

    /* Parse wrapper arguments:
     *   sud64 [--no-env] [--drop-argv N] /path/to/binary [args...] */
    int argi = 1;
    int drop_count = 0;

    if (argi < argc && argv[argi] && strcmp(argv[argi], "--no-env") == 0) {
        g_trace_exec_env = 0;
        argi++;
    }
    if (argi + 1 < argc && argv[argi] &&
        strcmp(argv[argi], "--drop-argv") == 0) {
        drop_count = parse_int(argv[argi + 1]);
        if (drop_count < 0) drop_count = 0;
        argi += 2;
    }

    if (argi >= argc || !argv[argi]) {
        const char msg[] = "sud: missing target binary\n";
        raw_write(2, msg, sizeof(msg) - 1);
        _exit(1);
    }

    if (drop_count > argc - argi)
        drop_count = 0;

    /* Resolve target path */
    char resolved[PATH_MAX];
    if (!resolve_path(argv[argi], resolved, sizeof(resolved))) {
        fprintf(stderr, "sud: cannot find '%s'\n", argv[argi]);
        _exit(127);
    }

    sud_addins_wrapper_init();

    /* Make safe copies of run_argv for cmdline rewrite.
     * Allocated once at startup based on actual argc — no fixed-size
     * truncation.  The freestanding malloc uses mmap, so this is fine
     * for a one-time allocation. */
    int run_argc = argc - argi;
    char **run_argv = calloc((size_t)run_argc + 1, sizeof(char *));
    if (!run_argv) {
        const char msg[] = "sud: out of memory for argv\n";
        raw_write(2, msg, sizeof(msg) - 1);
        _exit(1);
    }
    for (int i = 0; i < run_argc; i++)
        run_argv[i] = strdup(argv[argi + i]);
    run_argv[run_argc] = NULL;

    /* If the resolved target is a dynamically linked ELF, we need to
     * load it via its PT_INTERP (ld-linux).  Prepend the dynamic linker
     * to run_argv so that load_and_run_elf loads ld-linux as the primary
     * ELF and the target binary as the secondary.  Without this, jumping
     * to a dynamic binary's entry point without ld-linux causes SIGSEGV. */
    char elf_interp_buf[PATH_MAX];
    const char *load_path = resolved;
    int dyn = check_elf_dynamic(resolved, elf_interp_buf,
                                sizeof(elf_interp_buf), NULL);
    if (dyn == 1) {
        int new_run_argc = run_argc + 1;
        char **new_run_argv = calloc((size_t)new_run_argc + 1, sizeof(char *));
        if (!new_run_argv) {
            const char msg[] = "sud: out of memory for argv\n";
            raw_write(2, msg, sizeof(msg) - 1);
            _exit(1);
        }
        new_run_argv[0] = strdup(elf_interp_buf);
        if (!new_run_argv[0]) {
            const char msg[] = "sud: out of memory for interp path\n";
            raw_write(2, msg, sizeof(msg) - 1);
            _exit(1);
        }
        for (int i = 0; i < run_argc; i++)
            new_run_argv[i + 1] = run_argv[i];
        new_run_argv[new_run_argc] = NULL;

        /* The old run_argv string pointers are reused in new_run_argv,
         * so just free the old array shell (not the strings). */
        free(run_argv);
        run_argv = new_run_argv;
        run_argc = new_run_argc;
        drop_count++;
        load_path = elf_interp_buf;
    }

    /* Rewrite /proc/self/cmdline */
    {
        int vis_argc = run_argc - drop_count;
        char **vis_argv = run_argv + drop_count;
        rewrite_cmdline(argc, argv, vis_argc, vis_argv);
    }

    /* Load the ELF and jump — never returns */
    load_and_run_elf(load_path, run_argc, run_argv, drop_count);
}
