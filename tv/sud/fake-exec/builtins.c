/*
 * sud/fake-exec/builtins.c — Trivial vfork-safe emulators.
 *
 * The MVP set is the three pure-status builtins that GNU make and
 * autoconf scripts call most frequently and whose entire observable
 * effect is the exit code:
 *
 *   /usr/bin/true   → exit 0    /bin/true   alias
 *   /usr/bin/false  → exit 1    /bin/false  alias
 *   /usr/bin/:      → exit 0   (POSIX shell `:` builtin; some
 *                                 distributions ship a binary too)
 *
 * Each emulator is a pure function of argv: no environment reads, no
 * I/O, no allocation, no syscalls.  They are therefore safe to fire
 * from inside a child of clone(CLONE_VM|CLONE_VFORK) without faking
 * the vfork — the only kernel call that ever happens is the
 * SYS_exit issued by the addin after run_inline returns.
 *
 * Future builtins that need to emit output (echo, printf) require
 * either a vfork-safe write-via-raw_syscall6 path here, or a
 * Track-B run_rich path that runs in the parent's handler context
 * after the vfork child has exited.  Neither is wired in the MVP.
 */

#include "sud/fake-exec/builtins.h"

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

static const struct sud_fake_exec_builtin g_builtin_true = {
    "/usr/bin/true", "true", FAKE_EXEC_VFORK_SAFE, builtin_true,
};
static const struct sud_fake_exec_builtin g_builtin_true_alt = {
    "/bin/true", "true", FAKE_EXEC_VFORK_SAFE, builtin_true,
};
static const struct sud_fake_exec_builtin g_builtin_false = {
    "/usr/bin/false", "false", FAKE_EXEC_VFORK_SAFE, builtin_false,
};
static const struct sud_fake_exec_builtin g_builtin_false_alt = {
    "/bin/false", "false", FAKE_EXEC_VFORK_SAFE, builtin_false,
};
static const struct sud_fake_exec_builtin g_builtin_colon = {
    "/usr/bin/:", ":", FAKE_EXEC_VFORK_SAFE, builtin_colon,
};

static const struct sud_fake_exec_builtin *const g_builtins[] = {
    &g_builtin_true,
    &g_builtin_true_alt,
    &g_builtin_false,
    &g_builtin_false_alt,
    &g_builtin_colon,
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
