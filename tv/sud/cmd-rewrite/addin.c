/*
 * sud/cmd-rewrite/addin.c — SUD addin glue for cmd-rewrite.
 *
 * Hooks SYS_execve / SYS_execveat in pre_syscall, walks the rule
 * table, and on a match rewrites ctx->args[] in place.  The new
 * path / argv pointers reference memory carved from ctx->scratch
 * (the per-handler stack arena, PATH_MAX*2 bytes), valid until
 * the SIGSYS handler returns — which is exactly when handler.c
 * picks them up to build the wrapper-rewrite arena and call the
 * real execve.
 *
 * Suppression bookkeeping: when a rule fires, we append its
 * implicit name to g_sud_runtime_config.suppressed[].  handler.c's
 * wrapper-rewrite path then runs sud_runtime_config_emit() against
 * the live config, which re-emits the suppression on the child
 * wrapper's argv as --suppress-rule.  The child's wrapper.c reads
 * it back into the runtime config struct and our addin sees the
 * suppression on the next exec generation.  No env vars touched.
 *
 * Design note: we deliberately mutate ctx->args[] rather than
 * short-circuiting (returning 1).  Short-circuiting would skip
 * handler.c's special-case execve path that wraps the binary in
 * sud32/sud64 and emits the runtime config — without that, the
 * traced child loses every flag we just set.
 */

#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"
#include "sud/cmd-rewrite/cmd_rewrite.h"
#include "sud/cmd-rewrite/rules.h"
#include "libc-fs/libc.h"

/* ---- Init ------------------------------------------------------- */

static void cmd_rewrite_wrapper_init(void)
{
    sud_cmd_rules_init();
}

/* ---- Scratch bump allocator ------------------------------------- */

/* Each pre_syscall call uses ctx->scratch as a bump arena.  We carve
 * out NUL-terminated strings and pointer arrays, returning offsets
 * via *off so multiple allocations chain. */
struct bump {
    char *base;
    int   size;
    int   off;
};

static char *bump_alloc(struct bump *b, int n)
{
    if (n <= 0) return 0;
    if (b->off + n > b->size) return 0;
    char *p = b->base + b->off;
    b->off += n;
    return p;
}

static char *bump_strdup(struct bump *b, const char *s)
{
    if (!s) return 0;
    int len = 0;
    while (s[len]) len++;
    char *dst = bump_alloc(b, len + 1);
    if (!dst) return 0;
    for (int i = 0; i < len; i++) dst[i] = s[i];
    dst[len] = '\0';
    return dst;
}

/* Allocate aligned space for a (n+1)-entry char* array, leaving
 * the last slot NULL-terminated by the caller. */
static char **bump_argv(struct bump *b, int n)
{
    /* Align to sizeof(char*). */
    int pad = (int)((sizeof(char *) -
                     ((unsigned long)(b->base + b->off) & (sizeof(char *) - 1)))
                    & (sizeof(char *) - 1));
    if (b->off + pad > b->size) return 0;
    b->off += pad;
    return (char **)bump_alloc(b, (int)((n + 1) * sizeof(char *)));
}

/* ---- PATH lookup ------------------------------------------------ */

/* Resolve a bare command name (no '/') against the envp's PATH.
 * Writes the resolved absolute path into out[].  Returns 1 on hit,
 * 0 on miss.  Used by exec-strip when the inner argv[0] is a bare
 * command. */
static int faccessat_x(const char *path)
{
    /* X_OK = 1 in POSIX; libc-fs doesn't expose it as a macro so we
     * spell the literal here.  raw_syscall6 returns 0 on success. */
    long rc = raw_syscall6(SYS_faccessat, AT_FDCWD, (long)path,
                           1 /* X_OK */, 0, 0, 0);
    return rc == 0;
}

static int resolve_via_path(const char *cmd, char *const *envp,
                            char *out, int out_size)
{
    if (!cmd || !cmd[0]) return 0;
    /* If cmd contains a '/', it's already a path. */
    for (const char *p = cmd; *p; p++) if (*p == '/') {
        int n = 0;
        while (cmd[n] && n + 1 < out_size) { out[n] = cmd[n]; n++; }
        out[n] = '\0';
        return 1;
    }
    /* Find PATH=... in envp.  Fall back to /usr/bin:/bin. */
    const char *path_env = "/usr/bin:/bin";
    if (envp) {
        for (int i = 0; envp[i]; i++) {
            const char *e = envp[i];
            if (e[0]=='P' && e[1]=='A' && e[2]=='T' && e[3]=='H' && e[4]=='=') {
                path_env = e + 5;
                break;
            }
        }
    }
    const char *p = path_env;
    while (*p) {
        const char *colon = p;
        while (*colon && *colon != ':') colon++;
        int dlen = (int)(colon - p);
        /* Empty entry means "current directory" — skip; we only
         * resolve absolute matches to keep behaviour predictable. */
        if (dlen > 0 && *p == '/') {
            int off = 0;
            for (int i = 0; i < dlen && off + 1 < out_size; i++)
                out[off++] = p[i];
            if (off + 1 < out_size) out[off++] = '/';
            int j = 0;
            while (cmd[j] && off + 1 < out_size) out[off++] = cmd[j++];
            out[off] = '\0';
            if (faccessat_x(out)) return 1;
        }
        p = (*colon == ':') ? colon + 1 : colon;
    }
    out[0] = '\0';
    return 0;
}

/* ---- Helpers ---------------------------------------------------- */

static int char_in(const char *set, char c)
{
    if (!set) return 0;
    for (int i = 0; set[i]; i++)
        if (set[i] == c) return 1;
    return 0;
}

static int has_eq_sign(const char *s)
{
    if (!s) return 0;
    for (int i = 0; s[i]; i++) if (s[i] == '=') return 1;
    return 0;
}

/* Walk argv past wrapper-program flags according to spec.  argv[0]
 * is the wrapper itself (already going to be dropped); we start at
 * argv[1].  Returns the index of the first argv slot that should
 * become the new argv[0], or -1 if the strip cannot complete (e.g.
 * unknown long option, or env's VAR=VALUE that we refuse to handle).
 */
static int strip_walk_argv(const struct sud_cmd_strip_spec *spec,
                           char *const *argv, int argc,
                           int is_env)
{
    int i = 1;
    while (i < argc) {
        const char *a = argv[i];
        if (!a) break;

        /* env: VAR=VALUE forms terminate the strip with refusal —
         * setting an env var is a real semantic we can't ignore. */
        if (is_env && a[0] != '-' && has_eq_sign(a)) return -1;

        if (a[0] != '-') return i;             /* first command word */

        if (a[0] == '-' && a[1] == '-' && !a[2]) {
            if (!spec || !spec->accept_ddash) return -1;
            return i + 1;                       /* word after `--` */
        }
        if (a[0] == '-' && a[1] == '-') return -1;     /* long option */

        /* Bundle of single-letter flags from the singletons set,
         * possibly ending with one arg-taker that consumes argv[i+1].
         * If any byte in the bundle is unrecognised, refuse. */
        int j = 1;
        int consumed_arg = 0;
        while (a[j]) {
            char c = a[j];
            if (char_in(spec->singletons, c)) { j++; continue; }
            if (char_in(spec->arg_takers, c)) {
                /* Either consumes the rest of this argv slot
                 * ("-uroot") or the next slot ("-u root"). */
                if (a[j + 1]) { j += (int)strlen(a + j); }
                else          { consumed_arg = 1; }
                break;
            }
            return -1;          /* unknown bundled flag */
        }
        i++;
        if (consumed_arg) i++;
    }
    return -1;
}

/* ---- Rule application ------------------------------------------- */

/* compiler-wrap: rewrite (path, argv) so argv becomes
 *   [ tool, original-argv0, original-argv1, ... ]
 * and path becomes `tool`.  Caller already verified suppression.
 * Returns 0 on success, -1 on scratch overflow. */
static int apply_compiler_wrap(struct sud_syscall_ctx *ctx,
                               const struct sud_cmd_rule *r,
                               char *const *argv, int argc,
                               struct bump *b)
{
    char *new_path = bump_strdup(b, r->tool);
    if (!new_path) return -1;
    char **new_argv = bump_argv(b, argc + 1);
    if (!new_argv) return -1;
    new_argv[0] = bump_strdup(b, r->tool);
    if (!new_argv[0]) return -1;
    for (int i = 0; i < argc; i++) {
        new_argv[i + 1] = bump_strdup(b, argv[i] ? argv[i] : "");
        if (!new_argv[i + 1]) return -1;
    }
    new_argv[argc + 1] = 0;
    ctx->args[0] = (long)new_path;
    ctx->args[1] = (long)new_argv;
    return 0;
}

/* exec-strip: argv[0] is the wrapper (sudo/fakeroot-ng/env), find
 * the first non-flag word, treat it as the new argv[0], rewrite
 * (path, argv) to start there.  PATH-resolves the new argv[0] when
 * it's a bare name. */
static int apply_exec_strip(struct sud_syscall_ctx *ctx,
                            const struct sud_cmd_rule *r,
                            char *const *argv, int argc,
                            char *const *envp,
                            const char *outer_basename,
                            struct bump *b)
{
    int is_env = (outer_basename && strcmp(outer_basename, "env") == 0);
    int new_zero = strip_walk_argv(&r->strip, argv, argc, is_env);
    if (new_zero <= 0 || new_zero >= argc) return -1;
    if (!argv[new_zero] || !argv[new_zero][0]) return -1;

    char resolved[PATH_MAX];
    if (!resolve_via_path(argv[new_zero], envp, resolved, sizeof(resolved)))
        return -1;

    char *new_path = bump_strdup(b, resolved);
    if (!new_path) return -1;
    int n_new = argc - new_zero;
    char **new_argv = bump_argv(b, n_new);
    if (!new_argv) return -1;
    /* New argv[0] is the bare name the user typed (or its resolved
     * form if it was already a path) — match what real sudo does
     * when it execs the inner program. */
    new_argv[0] = bump_strdup(b, argv[new_zero]);
    if (!new_argv[0]) return -1;
    for (int i = 1; i < n_new; i++) {
        const char *src = argv[new_zero + i];
        new_argv[i] = bump_strdup(b, src ? src : "");
        if (!new_argv[i]) return -1;
    }
    new_argv[n_new] = 0;
    ctx->args[0] = (long)new_path;
    ctx->args[1] = (long)new_argv;
    return 0;
}

/* exec-as: don't rewrite argv, just bump the runtime config's
 * pretend-uid/gid before the wrapper-rewrite snapshots it.  The
 * matched exec inherits the new uid; descendants inherit it via
 * the re-emitted --pretend-uid / --pretend-gid flags. */
static int apply_exec_as(const struct sud_cmd_rule *r)
{
    if (r->as_uid >= 0) g_sud_runtime_config.pretend_uid = r->as_uid;
    if (r->as_gid >= 0) g_sud_runtime_config.pretend_gid = r->as_gid;
    return 0;
}

/* ---- Pre-syscall ------------------------------------------------ */

static int handle_execve(struct sud_syscall_ctx *ctx,
                         const char *path, char *const *argv,
                         char *const *envp)
{
    int count;
    const struct sud_cmd_rule *table = sud_cmd_rules_table(&count);
    if (count <= 0) return 0;

    int argc = 0;
    if (argv) while (argv[argc]) argc++;

    struct bump b = { ctx->scratch, (int)ctx->scratch_size, 0 };

    for (int i = 0; i < count; i++) {
        const struct sud_cmd_rule *r = &table[i];
        if (sud_cmd_rule_is_suppressed(r->name)) continue;
        if (!sud_cmd_rule_matches(r, path)) continue;

        const char *base_for_strip = path;
        const char *bn = path;
        for (const char *p = path; *p; p++) if (*p == '/') bn = p + 1;
        base_for_strip = bn;

        int rc = -1;
        switch (r->kind) {
        case SUD_CMD_KIND_COMPILER_WRAP:
            rc = apply_compiler_wrap(ctx, r, argv, argc, &b);
            break;
        case SUD_CMD_KIND_EXEC_STRIP:
            rc = apply_exec_strip(ctx, r, argv, argc, envp,
                                  base_for_strip, &b);
            break;
        case SUD_CMD_KIND_EXEC_AS:
            rc = apply_exec_as(r);
            break;
        default:
            continue;
        }
        if (rc != 0) {
            /* Apply failed (scratch overflow / strip refused): leave
             * args[] alone, fall through to kernel.  Don't suppress
             * either — the next exec is a fresh chance. */
            continue;
        }

        /* Auto-suppress this rule for descendants. */
        sud_cmd_rule_add_suppression(r->name);

        /* Refresh argc/argv for any subsequent rule that wants to
         * compose: the new args may match a different rule.  In
         * practice this matters mostly for `sudo make` →
         * exec-strip → make (then exec-as if make matches). */
        path = (const char *)ctx->args[0];
        argv = (char *const *)ctx->args[1];
        argc = 0;
        if (argv) while (argv[argc]) argc++;

        if (r->kind == SUD_CMD_KIND_EXEC_STRIP ||
            r->kind == SUD_CMD_KIND_COMPILER_WRAP) {
            /* The rewrite changed the binary identity; restart
             * matching from rule 0 so a new compiler-wrap can fire
             * on the freshly-revealed inner program. */
            i = -1;
            continue;
        }
    }
    return 0;
}

static int cmd_rewrite_pre_syscall(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;
#ifdef SYS_execve
    if (nr == SYS_execve) {
        const char *path  = (const char *)ctx->args[0];
        char *const *argv = (char *const *)ctx->args[1];
        char *const *envp = (char *const *)ctx->args[2];
        if (!path || !path[0]) return 0;
        return handle_execve(ctx, path, argv, envp);
    }
#endif
#ifdef SYS_execveat
    if (nr == SYS_execveat) {
        const char *path  = (const char *)ctx->args[1];
        char *const *argv = (char *const *)ctx->args[2];
        char *const *envp = (char *const *)ctx->args[3];
        if (!path || path[0] != '/') return 0;
        return handle_execve(ctx, path, argv, envp);
    }
#endif
    return 0;
}

const struct sud_addin sud_cmd_rewrite_addin = {
    "cmd_rewrite",
    cmd_rewrite_wrapper_init,
    0,
    0,
    cmd_rewrite_pre_syscall,
    0,
};
