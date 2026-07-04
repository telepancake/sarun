/*
 * sud/runtime_config.c — Parser, emitter, and global slot for the
 * wrapper-level runtime configuration.  See runtime_config.h for the
 * design rationale.
 *
 * This TU intentionally avoids both <string.h> and libc-fs/libc.h so
 * it can be linked unchanged into:
 *   • the freestanding sud32/sud64 wrapper (linked with libc-fs)
 *   • the libc-linked sudtrace launcher
 *
 * Only <stddef.h> is required.  Tiny static helpers replace the few
 * libc functions that are needed.
 */

#include "sud/runtime_config.h"

/* Forward-declare the few libc symbols we need.  Both build
 * environments (the freestanding wrapper linked against libc-fs and
 * the libc-linked sudtrace launcher) provide these — but we must
 * avoid pulling in <stdlib.h>/<string.h> directly because the
 * freestanding side cannot tolerate those headers. */
extern void *malloc(size_t);

/* ---- Global slot ------------------------------------------------- */

struct sud_runtime_config g_sud_runtime_config;
int                       g_sud_runtime_config_present;

/* ---- Tiny private libc shims ------------------------------------- */

static int rc_streq(const char *a, const char *b)
{
    if (!a || !b) return 0;
    while (*a && *a == *b) { a++; b++; }
    return *a == *b;
}

/* Decimal int parser.  Accepts optional sign and leading spaces;
 * returns 0 on a fully non-numeric input (consistent with the
 * existing libc-fs parse_int helper). */
static int rc_parse_int(const char *s)
{
    if (!s) return 0;
    while (*s == ' ' || *s == '\t') s++;
    int neg = 0;
    if (*s == '-') { neg = 1; s++; }
    else if (*s == '+') { s++; }
    int v = 0;
    while (*s >= '0' && *s <= '9') v = v * 10 + (*s++ - '0');
    return neg ? -v : v;
}

/* ---- Public API -------------------------------------------------- */

void sud_runtime_config_clear(struct sud_runtime_config *cfg)
{
    if (!cfg) return;
    cfg->no_env             = 0;
    cfg->drop_count         = 0;
    cfg->cwd                = 0;
    cfg->trace_outfile      = 0;
    cfg->inramfs_key        = 0;
    cfg->inramfs_meta_mb    = 0;
    cfg->remap_rule_count   = 0;
    for (int i = 0; i < SUD_RC_MAX_REMAP_RULES; i++)
        cfg->remap_rules[i] = 0;
    cfg->fake_exec_off       = 0;
    cfg->fake_exec_deny_count = 0;
    for (int i = 0; i < SUD_RC_MAX_FAKE_EXEC_DENY; i++)
        cfg->fake_exec_deny[i] = 0;
    cfg->cmd_rule_count = 0;
    for (int i = 0; i < SUD_RC_MAX_CMD_RULES; i++)
        cfg->cmd_rules[i] = 0;
    cfg->suppress_count = 0;
    for (int i = 0; i < SUD_RC_MAX_SUPPRESS; i++)
        cfg->suppressed[i] = 0;
    cfg->pretend_uid = -1;
    cfg->pretend_gid = -1;
}

int sud_runtime_config_parse(int argc, char **argv, int *argi,
                             struct sud_runtime_config *cfg)
{
    if (!cfg || !argi) return -1;
    int i = *argi;

    while (i < argc && argv[i]) {
        const char *a = argv[i];

        if (rc_streq(a, "--no-env")) {
            cfg->no_env = 1;
            i++;
            continue;
        }
        if (rc_streq(a, "--drop-argv")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            int n = rc_parse_int(argv[i + 1]);
            cfg->drop_count = (n < 0) ? 0 : n;
            i += 2;
            continue;
        }
        if (rc_streq(a, "--cwd")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            cfg->cwd = argv[i + 1];
            i += 2;
            continue;
        }
        if (rc_streq(a, "--trace-outfile")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            cfg->trace_outfile = argv[i + 1];
            i += 2;
            continue;
        }
        if (rc_streq(a, "--inramfs-key")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            cfg->inramfs_key = argv[i + 1];
            i += 2;
            continue;
        }
        if (rc_streq(a, "--inramfs-meta-mb")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            int n = rc_parse_int(argv[i + 1]);
            cfg->inramfs_meta_mb = (n < 0) ? 0 : n;
            i += 2;
            continue;
        }
        if (rc_streq(a, "--remap-rule")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            if (cfg->remap_rule_count >= SUD_RC_MAX_REMAP_RULES) return -1;
            cfg->remap_rules[cfg->remap_rule_count++] = argv[i + 1];
            i += 2;
            continue;
        }
        if (rc_streq(a, "--fake-exec")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            /* Only "off" is recognised; future values may turn the
             * addin back on with specific tuning. */
            if (rc_streq(argv[i + 1], "off"))
                cfg->fake_exec_off = 1;
            else
                cfg->fake_exec_off = 0;
            i += 2;
            continue;
        }
        if (rc_streq(a, "--fake-exec-deny")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            if (cfg->fake_exec_deny_count >= SUD_RC_MAX_FAKE_EXEC_DENY)
                return -1;
            cfg->fake_exec_deny[cfg->fake_exec_deny_count++] = argv[i + 1];
            i += 2;
            continue;
        }
        if (rc_streq(a, "--cmd-rule")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            if (cfg->cmd_rule_count >= SUD_RC_MAX_CMD_RULES) return -1;
            cfg->cmd_rules[cfg->cmd_rule_count++] = argv[i + 1];
            i += 2;
            continue;
        }
        if (rc_streq(a, "--suppress-rule")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            if (cfg->suppress_count >= SUD_RC_MAX_SUPPRESS) return -1;
            cfg->suppressed[cfg->suppress_count++] = argv[i + 1];
            i += 2;
            continue;
        }
        if (rc_streq(a, "--pretend-uid")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            cfg->pretend_uid = rc_parse_int(argv[i + 1]);
            i += 2;
            continue;
        }
        if (rc_streq(a, "--pretend-gid")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            cfg->pretend_gid = rc_parse_int(argv[i + 1]);
            i += 2;
            continue;
        }

        /* First non-flag argument: stop.  The caller treats this
         * position as the target binary path. */
        break;
    }

    *argi = i;
    return 0;
}

int sud_runtime_config_emit(const struct sud_runtime_config *cfg,
                            const char **out, int max,
                            char *int_scratch, int int_scratch_size)
{
    if (!cfg || !out || max < 0) return -1;
    /* We carve the scratch into N 16-byte slots, one per int-valued
     * flag, so they can all coexist in the emitted argv without
     * overwriting each other.  Today: drop-argv, inramfs-meta-mb,
     * pretend-uid, pretend-gid → 4 slots → 64 bytes minimum. */
    enum { RC_INT_SLOT = 16, RC_INT_SLOTS_NEEDED = 4 };
    if (!int_scratch || int_scratch_size < RC_INT_SLOT * RC_INT_SLOTS_NEEDED)
        return -1;
    int n = 0;
    int next_slot = 0;

    #define EMIT1(s) do { if (n >= max) return -1; out[n++] = (s); } while (0)
    #define EMIT2(s, v) do { if (n + 1 >= max) return -1; out[n++] = (s); out[n++] = (v); } while (0)
    /* Emit one int flag.  Borrows the next 16-byte slot from
     * int_scratch and writes the decimal representation into it. */
    #define EMIT_INT_FLAG(flag, intval) do { \
        if (next_slot >= RC_INT_SLOTS_NEEDED) return -1; \
        char *s = int_scratch + next_slot * RC_INT_SLOT; \
        next_slot++; \
        char tmp[16]; int p = 0; \
        unsigned int uv = (unsigned int)(intval); \
        do { tmp[p++] = (char)('0' + (uv % 10u)); uv /= 10u; } while (uv); \
        for (int k = 0; k < p; k++) s[k] = tmp[p - 1 - k]; \
        s[p] = '\0'; \
        EMIT2((flag), s); \
    } while (0)

    if (cfg->no_env) EMIT1("--no-env");

    if (cfg->drop_count > 0)
        EMIT_INT_FLAG("--drop-argv", cfg->drop_count);

    if (cfg->cwd && cfg->cwd[0])
        EMIT2("--cwd", cfg->cwd);
    if (cfg->trace_outfile && cfg->trace_outfile[0])
        EMIT2("--trace-outfile", cfg->trace_outfile);
    if (cfg->inramfs_key && cfg->inramfs_key[0])
        EMIT2("--inramfs-key", cfg->inramfs_key);

    if (cfg->inramfs_meta_mb > 0)
        EMIT_INT_FLAG("--inramfs-meta-mb", cfg->inramfs_meta_mb);

    int rcount = cfg->remap_rule_count;
    if (rcount > SUD_RC_MAX_REMAP_RULES) rcount = SUD_RC_MAX_REMAP_RULES;
    for (int i = 0; i < rcount; i++) {
        if (!cfg->remap_rules[i] || !cfg->remap_rules[i][0]) continue;
        EMIT2("--remap-rule", cfg->remap_rules[i]);
    }

    if (cfg->fake_exec_off)
        EMIT2("--fake-exec", "off");

    int dcount = cfg->fake_exec_deny_count;
    if (dcount > SUD_RC_MAX_FAKE_EXEC_DENY) dcount = SUD_RC_MAX_FAKE_EXEC_DENY;
    for (int i = 0; i < dcount; i++) {
        if (!cfg->fake_exec_deny[i] || !cfg->fake_exec_deny[i][0]) continue;
        EMIT2("--fake-exec-deny", cfg->fake_exec_deny[i]);
    }

    int crcount = cfg->cmd_rule_count;
    if (crcount > SUD_RC_MAX_CMD_RULES) crcount = SUD_RC_MAX_CMD_RULES;
    for (int i = 0; i < crcount; i++) {
        if (!cfg->cmd_rules[i] || !cfg->cmd_rules[i][0]) continue;
        EMIT2("--cmd-rule", cfg->cmd_rules[i]);
    }

    int scount = cfg->suppress_count;
    if (scount > SUD_RC_MAX_SUPPRESS) scount = SUD_RC_MAX_SUPPRESS;
    for (int i = 0; i < scount; i++) {
        if (!cfg->suppressed[i] || !cfg->suppressed[i][0]) continue;
        EMIT2("--suppress-rule", cfg->suppressed[i]);
    }

    if (cfg->pretend_uid >= 0)
        EMIT_INT_FLAG("--pretend-uid", cfg->pretend_uid);
    if (cfg->pretend_gid >= 0)
        EMIT_INT_FLAG("--pretend-gid", cfg->pretend_gid);

    #undef EMIT1
    #undef EMIT2
    #undef EMIT_INT_FLAG
    return n;
}

/* ---- Interning --------------------------------------------------- */

static char *rc_strdup(const char *s)
{
    if (!s) return 0;
    size_t n = 0;
    while (s[n]) n++;
    char *p = (char *)malloc(n + 1);
    if (!p) return 0;
    for (size_t i = 0; i < n; i++) p[i] = s[i];
    p[n] = '\0';
    return p;
}

void sud_runtime_config_intern(struct sud_runtime_config *cfg)
{
    if (!cfg) return;
    if (cfg->cwd && cfg->cwd[0])
        cfg->cwd = rc_strdup(cfg->cwd);
    if (cfg->trace_outfile && cfg->trace_outfile[0])
        cfg->trace_outfile = rc_strdup(cfg->trace_outfile);
    if (cfg->inramfs_key && cfg->inramfs_key[0])
        cfg->inramfs_key = rc_strdup(cfg->inramfs_key);
    int rcount = cfg->remap_rule_count;
    if (rcount > SUD_RC_MAX_REMAP_RULES) rcount = SUD_RC_MAX_REMAP_RULES;
    for (int i = 0; i < rcount; i++) {
        if (cfg->remap_rules[i] && cfg->remap_rules[i][0])
            cfg->remap_rules[i] = rc_strdup(cfg->remap_rules[i]);
    }
    int dcount = cfg->fake_exec_deny_count;
    if (dcount > SUD_RC_MAX_FAKE_EXEC_DENY) dcount = SUD_RC_MAX_FAKE_EXEC_DENY;
    for (int i = 0; i < dcount; i++) {
        if (cfg->fake_exec_deny[i] && cfg->fake_exec_deny[i][0])
            cfg->fake_exec_deny[i] = rc_strdup(cfg->fake_exec_deny[i]);
    }
    int crcount = cfg->cmd_rule_count;
    if (crcount > SUD_RC_MAX_CMD_RULES) crcount = SUD_RC_MAX_CMD_RULES;
    for (int i = 0; i < crcount; i++) {
        if (cfg->cmd_rules[i] && cfg->cmd_rules[i][0])
            cfg->cmd_rules[i] = rc_strdup(cfg->cmd_rules[i]);
    }
    int scount = cfg->suppress_count;
    if (scount > SUD_RC_MAX_SUPPRESS) scount = SUD_RC_MAX_SUPPRESS;
    for (int i = 0; i < scount; i++) {
        if (cfg->suppressed[i] && cfg->suppressed[i][0])
            cfg->suppressed[i] = rc_strdup(cfg->suppressed[i]);
    }
}

void sud_runtime_config_test_install(const struct sud_runtime_config *src)
{
    sud_runtime_config_clear(&g_sud_runtime_config);
    if (src) {
        g_sud_runtime_config = *src;
        sud_runtime_config_intern(&g_sud_runtime_config);
    }
    g_sud_runtime_config_present = 1;
}

void sud_runtime_config_test_clear(void)
{
    sud_runtime_config_clear(&g_sud_runtime_config);
    g_sud_runtime_config_present = 0;
}

void sud_runtime_config_set_cwd(struct sud_runtime_config *cfg,
                                const char *new_cwd)
{
    if (!cfg) return;
    if (!new_cwd || !new_cwd[0]) {
        cfg->cwd = 0;
        return;
    }
    cfg->cwd = rc_strdup(new_cwd);
}
