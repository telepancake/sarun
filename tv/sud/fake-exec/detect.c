/*
 * sud/fake-exec/detect.c — Classifier for "is this exec elidable?"
 *
 * The classifier is the only place that decides whether to skip a
 * given execve.  It is deliberately conservative: any envp setting
 * that could change the observable behaviour of the real binary
 * forces a passthrough.  In particular we refuse to elide when the
 * dynamic linker's well-known knobs are present, because the user
 * may be running with a wrapped libc whose behaviour we cannot
 * predict from argv alone.
 *
 * The classifier is a pure function (no syscalls, no allocation, no
 * global mutation) so it is safe to call from inside a vfork child.
 */

#include "sud/fake-exec/fake_exec.h"
#include "sud/fake-exec/builtins.h"
#include "sud/runtime_config.h"
#include "libc-fs/libc.h"

/* Returns 1 iff `s` starts with the literal `prefix`. */
static int starts_with(const char *s, const char *prefix)
{
    if (!s || !prefix) return 0;
    while (*prefix) {
        if (*s++ != *prefix++) return 0;
    }
    return 1;
}

/* Reject envp entries that could change the binary's observable
 * behaviour.  This is intentionally conservative — the cost of a
 * false negative is "we run the real binary", which is exactly what
 * the kernel would do without us. */
static int envp_is_dangerous(char *const *envp)
{
    if (!envp) return 0;
    for (int i = 0; envp[i]; i++) {
        const char *e = envp[i];
        if (starts_with(e, "LD_PRELOAD=")     ||
            starts_with(e, "LD_LIBRARY_PATH=") ||
            starts_with(e, "LD_AUDIT=")        ||
            starts_with(e, "LD_DEBUG=")        ||
            starts_with(e, "LD_BIND_NOW=")     ||
            starts_with(e, "LD_BIND_NOT="))
            return 1;
    }
    return 0;
}

/* Returns 1 iff the configured deny-list mentions this builtin's
 * basename.  Empty deny-list passes everything. */
static int builtin_is_denied(const struct sud_fake_exec_builtin *b)
{
    if (!g_sud_runtime_config_present) return 0;
    int n = g_sud_runtime_config.fake_exec_deny_count;
    if (n > SUD_RC_MAX_FAKE_EXEC_DENY) n = SUD_RC_MAX_FAKE_EXEC_DENY;
    for (int i = 0; i < n; i++) {
        const char *deny = g_sud_runtime_config.fake_exec_deny[i];
        if (!deny || !deny[0]) continue;
        if (b->canonical_path && strcmp(deny, b->canonical_path) == 0) return 1;
        if (b->basename       && strcmp(deny, b->basename) == 0)       return 1;
    }
    return 0;
}

int sud_fake_exec_classify(const char *path,
                           char *const *argv,
                           char *const *envp,
                           struct fake_exec_decision *out)
{
    if (!out) return -1;
    out->track       = FAKE_EXEC_PASSTHROUGH;
    out->builtin     = 0;
    out->exit_status = 0;

    if (g_sud_runtime_config_present && g_sud_runtime_config.fake_exec_off)
        return 0;

    if (!path || !path[0]) return 0;

    const struct sud_fake_exec_builtin *b = sud_fake_exec_lookup(path);
    if (!b) return 0;
    if (builtin_is_denied(b)) return 0;

    if (envp_is_dangerous(envp)) return 0;

    /* All MVP builtins are vfork-safe pure-status emulators. */
    if (!(b->flags & FAKE_EXEC_VFORK_SAFE)) return 0;

    int argc = 0;
    if (argv) while (argv[argc]) argc++;

    int status = b->run_inline(argc, argv);
    if (status < 0) status = 0;
    if (status > 127) status = 127;

    out->track       = FAKE_EXEC_INLINE_VFORK_SAFE;
    out->builtin     = b;
    out->exit_status = status;
    return 0;
}
