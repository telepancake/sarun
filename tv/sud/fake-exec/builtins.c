/*
 * sud/fake-exec/builtins.c — Trivial vfork-safe emulators.
 *
 * Pure-status family (true / false / :): exit code is the only
 * observable effect.
 *
 * Bounded-stdout family (echo / printf): compose a single bounded
 * string into the caller's scratch buffer; the addin emits a
 * synthetic STDOUT trace event then issues raw write+exit, all from
 * inside the SIGSYS handler.
 *
 * Every emulator is a pure function of argv: no environment reads,
 * no syscalls, no allocation, no global mutation.  Safe to fire from
 * inside a child of clone(CLONE_VM|CLONE_VFORK).
 */

#include "sud/fake-exec/builtins.h"

/* ---- Pure-status family ----------------------------------------- */

static int builtin_true(int argc, char *const *argv)
{
    (void)argc; (void)argv;
    return 0;
}

static int builtin_false(int argc, char *const *argv)
{
    (void)argc; (void)argv;
    return 1;
}

/* `:` is the POSIX no-op; some systems ship /usr/bin/: as a binary
 * that is functionally identical to /usr/bin/true. */
static int builtin_colon(int argc, char *const *argv)
{
    (void)argc; (void)argv;
    return 0;
}

/* ---- Bounded-stdout family -------------------------------------- */

/* Append `src` to *dst (advancing *dst, never past `end`).  Returns
 * the number of bytes that *would* have been written even if it
 * truncates — caller compares against the remaining capacity to
 * detect overflow. */
static int append_str(char **dst, char *end, const char *src)
{
    int n = 0;
    while (*src) {
        if (*dst < end) *(*dst)++ = *src;
        src++; n++;
    }
    return n;
}

static int append_char(char **dst, char *end, char c)
{
    if (*dst < end) *(*dst)++ = c;
    return 1;
}

/* echo argv[1..argc-1] joined by single spaces, terminated by '\n'.
 *
 * We deliberately do NOT honour `-n` / `-e` / `-E` flags — the GNU
 * echo binary behaves differently across releases and the cost of a
 * misclassification is "we output an extra newline / lose the -e
 * escapes" which is observable.  The classifier (detect.c) rejects
 * any argv that starts with a `-`, so we only ever see the plain
 * "echo arg1 arg2 ..." form here. */
static int compose_echo(int argc, char *const *argv,
                        char *scratch, int scratch_size)
{
    if (!scratch || scratch_size <= 0) return -1;
    /* Real echo treats argv[1] as a flag word when it starts with `-`
     * (`-n` suppresses newline, `-e` enables backslash escapes, `-E`
     * disables them).  GNU and BSD differ on which flags exist and
     * what `--` does.  Refuse the moment we see a leading `-` so the
     * kernel can run the real binary's distro-specific behaviour. */
    if (argc >= 2 && argv[1] && argv[1][0] == '-') return -1;
    char *p   = scratch;
    char *end = scratch + scratch_size;
    int   need = 0;
    for (int i = 1; i < argc; i++) {
        if (i > 1) need += append_char(&p, end, ' ');
        if (argv[i]) need += append_str(&p, end, argv[i]);
    }
    need += append_char(&p, end, '\n');
    if (need > scratch_size) return -1;
    return (int)(p - scratch);
}

/* printf with no %-conversions: the format string is the only output
 * (extra args are ignored, matching POSIX printf when the format
 * has no conversions and no extra newline is appended).
 *
 * The classifier rejects any format containing `%` (the conservative
 * subset).  Backslash escapes are NOT interpreted — coreutils printf
 * does interpret \n / \t / \\, but the cost of getting that wrong
 * (output diverges from real binary) is higher than the cost of a
 * passthrough.  Callers that want escapes will see the format string
 * forwarded literally to the kernel's exec.
 *
 * Result: only an absolutely-literal printf "hello world\n" form is
 * accepted, where the format already contains the literal newline as
 * a single byte (e.g. shell `printf "hi\n"` after the shell has
 * already substituted the escape).  The detect.c classifier catches
 * the `%` and `\` cases and routes around us. */
static int compose_printf(int argc, char *const *argv,
                          char *scratch, int scratch_size)
{
    if (argc < 2 || !argv[1]) return -1;
    if (!scratch || scratch_size <= 0) return -1;
    /* coreutils printf:
     *   • leading `-` may be a flag (`--help`, `--version`)
     *   • any `%` introduces a conversion (we don't emulate them)
     *   • any `\` introduces an escape (we don't emulate them either)
     * Refuse any of those — the kernel runs the real binary. */
    if (argv[1][0] == '-') return -1;
    for (const char *q = argv[1]; *q; q++) {
        if (*q == '%' || *q == '\\') return -1;
    }
    char *p   = scratch;
    char *end = scratch + scratch_size;
    int   need = append_str(&p, end, argv[1]);
    if (need > scratch_size) return -1;
    return (int)(p - scratch);
}

static int run_zero(int argc, char *const *argv)
{
    (void)argc; (void)argv;
    return 0;
}

/* ---- Registry --------------------------------------------------- */

static const struct sud_fake_exec_builtin g_builtin_true = {
    "/usr/bin/true", "true", FAKE_EXEC_VFORK_SAFE, builtin_true, 0,
};
static const struct sud_fake_exec_builtin g_builtin_true_alt = {
    "/bin/true", "true", FAKE_EXEC_VFORK_SAFE, builtin_true, 0,
};
static const struct sud_fake_exec_builtin g_builtin_false = {
    "/usr/bin/false", "false", FAKE_EXEC_VFORK_SAFE, builtin_false, 0,
};
static const struct sud_fake_exec_builtin g_builtin_false_alt = {
    "/bin/false", "false", FAKE_EXEC_VFORK_SAFE, builtin_false, 0,
};
static const struct sud_fake_exec_builtin g_builtin_colon = {
    "/usr/bin/:", ":", FAKE_EXEC_VFORK_SAFE, builtin_colon, 0,
};

static const struct sud_fake_exec_builtin g_builtin_echo = {
    "/usr/bin/echo", "echo",
    FAKE_EXEC_VFORK_SAFE | FAKE_EXEC_HAS_INLINE_OUTPUT,
    run_zero, compose_echo,
};
static const struct sud_fake_exec_builtin g_builtin_echo_alt = {
    "/bin/echo", "echo",
    FAKE_EXEC_VFORK_SAFE | FAKE_EXEC_HAS_INLINE_OUTPUT,
    run_zero, compose_echo,
};
static const struct sud_fake_exec_builtin g_builtin_printf = {
    "/usr/bin/printf", "printf",
    FAKE_EXEC_VFORK_SAFE | FAKE_EXEC_HAS_INLINE_OUTPUT,
    run_zero, compose_printf,
};
static const struct sud_fake_exec_builtin g_builtin_printf_alt = {
    "/bin/printf", "printf",
    FAKE_EXEC_VFORK_SAFE | FAKE_EXEC_HAS_INLINE_OUTPUT,
    run_zero, compose_printf,
};

static const struct sud_fake_exec_builtin *const g_builtins[] = {
    &g_builtin_true,
    &g_builtin_true_alt,
    &g_builtin_false,
    &g_builtin_false_alt,
    &g_builtin_colon,
    &g_builtin_echo,
    &g_builtin_echo_alt,
    &g_builtin_printf,
    &g_builtin_printf_alt,
    0,
};

const struct sud_fake_exec_builtin *const *sud_fake_exec_builtins(void)
{
    return g_builtins;
}

/* Match by full canonical path first (the strongest signal — if the
 * caller did the PATH lookup themselves we know exactly which file
 * is being execed); fall back to basename match for callers that
 * passed a bare name and expect us to recognise common helpers. */
const struct sud_fake_exec_builtin *
sud_fake_exec_lookup(const char *path)
{
    if (!path || !path[0]) return 0;

    /* Locate basename = last segment after '/'. */
    const char *base = path;
    for (const char *p = path; *p; p++)
        if (*p == '/') base = p + 1;

    for (int i = 0; g_builtins[i]; i++) {
        const struct sud_fake_exec_builtin *b = g_builtins[i];
        if (b->canonical_path && strcmp(b->canonical_path, path) == 0)
            return b;
    }
    /* Basename match only if the caller's path did NOT contain '/' —
     * an absolute path that didn't match canonical_path is something
     * the user explicitly named, and we should not silently substitute
     * a different binary. */
    int has_slash = 0;
    for (const char *p = path; *p; p++) if (*p == '/') { has_slash = 1; break; }
    if (!has_slash) {
        for (int i = 0; g_builtins[i]; i++) {
            const struct sud_fake_exec_builtin *b = g_builtins[i];
            if (b->basename && strcmp(b->basename, base) == 0)
                return b;
        }
    }
    return 0;
}
