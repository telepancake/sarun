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
 * The classifier is a pure function modulo writes into the caller-
 * supplied scratch buffer (which is per-handler stack, no shared
 * state).  Safe to call from inside a vfork child.
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
 * basename or canonical path.  Empty deny-list passes everything. */
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

/* Single-command shell grammar.  A token is one of:
 *   • a run of bytes from an "ordinary" set (alphanumerics + `_-./+,:=@`)
 *   • separated by exactly one ASCII space
 * Anything else (quoting, redirection, expansion, substitution,
 * command-chaining, glob characters, backslashes, tabs, multiple
 * spaces, leading/trailing space) rejects the whole command. */
static int sh_byte_is_safe(unsigned char c)
{
    if ((c >= 'a' && c <= 'z') ||
        (c >= 'A' && c <= 'Z') ||
        (c >= '0' && c <= '9'))
        return 1;
    switch (c) {
    case '_': case '-': case '.': case '/':
    case '+': case ',': case ':': case '=':
    case '@':
        return 1;
    default:
        return 0;
    }
}

int sud_fake_exec_sh_tokenise(const char *cmd,
                              char *tok_scratch, int tok_scratch_size,
                              char **argv_out, int argv_out_max,
                              int *argc_out)
{
    if (!cmd || !tok_scratch || tok_scratch_size <= 0 ||
        !argv_out || argv_out_max <= 0 || !argc_out)
        return 0;

    /* Reject empty / leading-space / trailing-space outright. */
    if (!cmd[0]) return 0;
    if (cmd[0] == ' ') return 0;

    /* Copy into scratch with NUL-separation; build pointer table. */
    int  argc = 0;
    int  off  = 0;
    int  in_token = 0;
    int  prev_space = 0;
    for (int i = 0; cmd[i]; i++) {
        unsigned char c = (unsigned char)cmd[i];
        if (c == ' ') {
            if (prev_space) return 0;       /* "foo  bar" */
            if (!in_token)  return 0;       /* defensive */
            if (off >= tok_scratch_size) return 0;
            tok_scratch[off++] = '\0';
            in_token   = 0;
            prev_space = 1;
            continue;
        }
        if (!sh_byte_is_safe(c)) return 0;
        if (!in_token) {
            if (argc >= argv_out_max) return 0;
            argv_out[argc++] = tok_scratch + off;
            in_token = 1;
        }
        if (off >= tok_scratch_size) return 0;
        tok_scratch[off++] = (char)c;
        prev_space = 0;
    }
    if (prev_space) return 0;               /* trailing space */
    if (!in_token)  return 0;               /* nothing emitted */
    if (off >= tok_scratch_size) return 0;
    tok_scratch[off++] = '\0';
    if (argc >= argv_out_max) return 0;
    argv_out[argc] = 0;

    *argc_out = argc;
    return 1;
}

/* Returns 1 iff `path` is one of the shells we recurse through. */
static int path_is_shell(const char *path)
{
    if (!path) return 0;
    return strcmp(path, "/bin/sh")    == 0 ||
           strcmp(path, "/usr/bin/sh") == 0 ||
           strcmp(path, "/bin/bash")  == 0 ||
           strcmp(path, "/usr/bin/bash") == 0;
}

/* Layout of scratch when the sh -c recursion is in play:
 *
 *   [ inner-output           | tok_scratch | argv_ptrs ]
 *
 * The inner builtin's output (if any) lives in the first half so the
 * addin can hand it to the synthetic-write helper after we return.
 * The token-storage half holds the parsed inner argv. */
struct sh_recurse_scratch {
    char  tok_scratch[256];
    char *argv[16];
};

/* Forward decl so apply_builtin can recurse for sh -c. */
static int apply_builtin(const struct sud_fake_exec_builtin *b,
                         char *const *argv, char *const *envp,
                         char *scratch, int scratch_size,
                         struct fake_exec_decision *out);

/* /bin/sh -c "<cmd>" :  recognise the shape, tokenise <cmd>, look up
 * the inner builtin, and (if elidable) emit a decision describing
 * the elision of the inner program — bypassing the shell entirely. */
static int classify_sh_dash_c(char *const *argv,
                              char *const *envp,
                              char *scratch, int scratch_size,
                              struct fake_exec_decision *out)
{
    /* Expect argv = [ "sh", "-c", "<cmd>", ... ] (anything after the
     * <cmd> would be POSIX positional parameters $0/$1/... which we
     * reject — the inner classifier wouldn't see them either). */
    int argc = 0;
    if (argv) while (argv[argc]) argc++;
    if (argc != 3) return 0;
    if (!argv[1] || strcmp(argv[1], "-c") != 0) return 0;
    const char *cmd = argv[2];
    if (!cmd) return 0;

    if (scratch_size < (int)sizeof(struct sh_recurse_scratch)) return 0;
    struct sh_recurse_scratch *sr =
        (struct sh_recurse_scratch *)(scratch + scratch_size
                                      - sizeof(struct sh_recurse_scratch));
    int   inner_scratch_size = scratch_size
                               - (int)sizeof(struct sh_recurse_scratch);
    char *inner_scratch      = scratch;

    int inner_argc = 0;
    if (!sud_fake_exec_sh_tokenise(cmd,
                                   sr->tok_scratch, sizeof(sr->tok_scratch),
                                   sr->argv, (int)(sizeof(sr->argv)
                                                   / sizeof(sr->argv[0])),
                                   &inner_argc))
        return 0;
    if (inner_argc < 1) return 0;

    /* Look up the inner argv[0] as a builtin.  Both absolute paths
     * and bare basenames are accepted — sud_fake_exec_lookup applies
     * the same rules it applies on the top level. */
    const struct sud_fake_exec_builtin *b = sud_fake_exec_lookup(sr->argv[0]);
    if (!b) return 0;
    if (builtin_is_denied(b)) return 0;
    if (!(b->flags & FAKE_EXEC_VFORK_SAFE)) return 0;

    /* Inner classification.  Re-uses apply_builtin so envp checks /
     * output composition / status emission are handled identically to
     * the direct-execve path. */
    return apply_builtin(b, sr->argv, envp,
                         inner_scratch, inner_scratch_size, out);
}

/* Common tail: validate envp, run the builtin, optionally compose
 * output, and fill *out.  Used by both the direct-execve path and
 * the sh -c recursion. */
static int apply_builtin(const struct sud_fake_exec_builtin *b,
                         char *const *argv, char *const *envp,
                         char *scratch, int scratch_size,
                         struct fake_exec_decision *out)
{
    if (envp_is_dangerous(envp)) return 0;
    if (!(b->flags & FAKE_EXEC_VFORK_SAFE)) return 0;

    int argc = 0;
    if (argv) while (argv[argc]) argc++;

    int status = b->run_inline ? b->run_inline(argc, argv) : 0;
    if (status < 0)   status = 0;
    if (status > 127) status = 127;

    out->track       = FAKE_EXEC_INLINE_VFORK_SAFE;
    out->builtin     = b;
    out->exit_status = status;
    out->out_fd      = -1;
    out->out_buf     = 0;
    out->out_len     = 0;

    if (b->flags & FAKE_EXEC_HAS_INLINE_OUTPUT) {
        if (!b->compose_inline || !scratch || scratch_size <= 0) {
            /* Output-emitting builtin without a buffer to compose
             * into → can't elide.  Force passthrough. */
            out->track   = FAKE_EXEC_PASSTHROUGH;
            out->builtin = 0;
            return 0;
        }
        int n = b->compose_inline(argc, argv, scratch, scratch_size);
        if (n < 0) {
            out->track   = FAKE_EXEC_PASSTHROUGH;
            out->builtin = 0;
            return 0;
        }
        out->out_fd  = 1;     /* STDOUT_FILENO */
        out->out_buf = scratch;
        out->out_len = n;
    }
    return 0;
}

int sud_fake_exec_classify(const char *path,
                           char *const *argv,
                           char *const *envp,
                           char *scratch, int scratch_size,
                           struct fake_exec_decision *out)
{
    if (!out) return -1;
    out->track       = FAKE_EXEC_PASSTHROUGH;
    out->builtin     = 0;
    out->exit_status = 0;
    out->out_fd      = -1;
    out->out_buf     = 0;
    out->out_len     = 0;

    if (g_sud_runtime_config_present && g_sud_runtime_config.fake_exec_off)
        return 0;

    if (!path || !path[0]) return 0;

    /* /bin/sh -c "<single trivial cmd>" — recurse before the direct
     * lookup so we elide the shell wrapper too.  classify_sh_dash_c
     * is a no-op (return 0) on anything that isn't recognisably
     * elidable; the caller falls through to the direct lookup. */
    if (path_is_shell(path)) {
        struct fake_exec_decision tmp = *out;
        if (classify_sh_dash_c(argv, envp, scratch, scratch_size, &tmp) == 0
            && tmp.track == FAKE_EXEC_INLINE_VFORK_SAFE) {
            *out = tmp;
            return 0;
        }
        /* Shell present but not elidable — keep going so we still
         * reject any chance of treating /bin/sh itself as a builtin. */
        return 0;
    }

    const struct sud_fake_exec_builtin *b = sud_fake_exec_lookup(path);
    if (!b) return 0;
    if (builtin_is_denied(b)) return 0;

    return apply_builtin(b, argv, envp, scratch, scratch_size, out);
}
