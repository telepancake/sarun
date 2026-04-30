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
    if (!int_scratch || int_scratch_size < 32) return -1;
    int n = 0;
    /* Split the scratch into two halves so two int-valued flags can
     * coexist in the output without overwriting each other. */
    char *scratch_a = int_scratch;
    char *scratch_b = int_scratch + (int_scratch_size / 2);
    int   half_b   = int_scratch_size - (int_scratch_size / 2);
    int   half_a   = int_scratch_size / 2;

    #define EMIT1(s) do { if (n >= max) return -1; out[n++] = (s); } while (0)
    #define EMIT2(s, v) do { if (n + 1 >= max) return -1; out[n++] = (s); out[n++] = (v); } while (0)

    if (cfg->no_env) EMIT1("--no-env");

    if (cfg->drop_count > 0) {
        int v = cfg->drop_count;
        char tmp[16];
        int p = 0;
        unsigned int uv = (unsigned int)v;
        do { tmp[p++] = (char)('0' + (uv % 10u)); uv /= 10u; } while (uv);
        if (p + 1 > half_a) return -1;
        for (int k = 0; k < p; k++) scratch_a[k] = tmp[p - 1 - k];
        scratch_a[p] = '\0';
        EMIT2("--drop-argv", scratch_a);
    }

    if (cfg->cwd && cfg->cwd[0])
        EMIT2("--cwd", cfg->cwd);
    if (cfg->trace_outfile && cfg->trace_outfile[0])
        EMIT2("--trace-outfile", cfg->trace_outfile);
    if (cfg->inramfs_key && cfg->inramfs_key[0])
        EMIT2("--inramfs-key", cfg->inramfs_key);

    if (cfg->inramfs_meta_mb > 0) {
        int v = cfg->inramfs_meta_mb;
        char tmp[16];
        int p = 0;
        unsigned int uv = (unsigned int)v;
        do { tmp[p++] = (char)('0' + (uv % 10u)); uv /= 10u; } while (uv);
        if (p + 1 > half_b) return -1;
        for (int k = 0; k < p; k++) scratch_b[k] = tmp[p - 1 - k];
        scratch_b[p] = '\0';
        EMIT2("--inramfs-meta-mb", scratch_b);
    }

    int rcount = cfg->remap_rule_count;
    if (rcount > SUD_RC_MAX_REMAP_RULES) rcount = SUD_RC_MAX_REMAP_RULES;
    for (int i = 0; i < rcount; i++) {
        if (!cfg->remap_rules[i] || !cfg->remap_rules[i][0]) continue;
        EMIT2("--remap-rule", cfg->remap_rules[i]);
    }

    #undef EMIT1
    #undef EMIT2
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
}
