/*
 * sud/path_remap/rules.c — Pure parser + prefix lookup for the
 * --remap-rule list.  See rules.h for design rationale.
 *
 * This TU intentionally avoids both <string.h> and libc-fs/libc.h so
 * it compiles unchanged into:
 *   • the freestanding sud32/sud64 wrapper (linked with libc-fs)
 *   • the libc-linked sudtrace launcher
 *
 * Only <stddef.h> (via rules.h) is required.  All string handling is
 * done in tiny static helpers below.
 */

#include "sud/path_remap/rules.h"

/* ---- Local string shims (no libc dependency) ---- */

static int rules_strncmp(const char *a, const char *b, size_t n)
{
    while (n--) {
        unsigned char ca = (unsigned char)*a++;
        unsigned char cb = (unsigned char)*b++;
        if (ca != cb) return (int)ca - (int)cb;
        if (!ca) return 0;
    }
    return 0;
}

static size_t rules_strlen(const char *s)
{
    size_t n = 0;
    while (s[n]) n++;
    return n;
}

static const char *rules_strchr(const char *s, char c)
{
    while (*s) {
        if (*s == c) return s;
        s++;
    }
    return c == '\0' ? s : 0;
}

static int is_path_boundary(char c) { return c == '\0' || c == '/'; }

/* ---- Public API ---- */

int sud_rules_compose(char *out, size_t out_sz,
                      const char *prefix, size_t prefix_len,
                      const char *tail)
{
    if (!out || out_sz == 0 || !prefix || !tail) return 0;
    size_t tail_len = rules_strlen(tail);
    if (prefix_len + tail_len + 1 > out_sz) return 0;
    for (size_t i = 0; i < prefix_len; i++) out[i] = prefix[i];
    for (size_t i = 0; i <= tail_len; i++) out[prefix_len + i] = tail[i];
    return 1;
}

/* Parse one --remap-rule string into *r.  Returns 1 on success, 0 on
 * a malformed/ignored entry.  The kind token is whatever precedes the
 * first ':'; the rest of the string is the kind-specific spec. */
static enum sud_rule_kind classify_kind(const char *kind, size_t klen)
{
    if (klen == 11 && rules_strncmp(kind, "passthrough", 11) == 0)
        return SUD_RULE_KIND_PASSTHROUGH;
    if (klen ==  5 && rules_strncmp(kind, "remap",        5) == 0)
        return SUD_RULE_KIND_REMAP;
    if (klen ==  7 && rules_strncmp(kind, "overlay",      7) == 0)
        return SUD_RULE_KIND_OVERLAY;
    if (klen ==  8 && rules_strncmp(kind, "fakeroot",     8) == 0)
        return SUD_RULE_KIND_FAKEROOT;
    if (klen ==  7 && rules_strncmp(kind, "inramfs",      7) == 0)
        return SUD_RULE_KIND_INRAMFS;
    return SUD_RULE_KIND_UNKNOWN;
}

static size_t trim_trailing_slash(const char *s, size_t n)
{
    while (n > 1 && s[n - 1] == '/') n--;
    return n;
}

static int parse_one(struct sud_rule *r, const char *spec)
{
    r->merged = 0;
    r->merged_len = 0;
    r->layer_count = 0;

    /* Single-prefix rule kinds.  Strip trailing slashes (except a
     * bare "/") so the prefix-match is unambiguous, mirroring
     * sud/path_remap/overlay.c::parse_passthrough_segment. */
    if (r->kind == SUD_RULE_KIND_PASSTHROUGH ||
        r->kind == SUD_RULE_KIND_FAKEROOT ||
        r->kind == SUD_RULE_KIND_INRAMFS) {
        size_t n = trim_trailing_slash(spec, rules_strlen(spec));
        if (n == 0 || spec[0] != '/') return 0;
        r->merged = spec;
        r->merged_len = n;
        return 1;
    }

    /* "<merged>=<layer>+<layer>+..." — used by both --remap (single
     * layer) and --overlay (upper + lowers). */
    if (r->kind == SUD_RULE_KIND_REMAP ||
        r->kind == SUD_RULE_KIND_OVERLAY) {
        const char *eq = rules_strchr(spec, '=');
        if (!eq || eq == spec) return 0;
        r->merged = spec;
        r->merged_len = (size_t)(eq - spec);

        const char *p = eq + 1;
        if (r->kind == SUD_RULE_KIND_REMAP) {
            size_t llen = rules_strlen(p);
            if (llen == 0) return 0;
            r->layers[0] = p;
            r->layer_lens[0] = llen;
            r->layer_count = 1;
            return 1;
        }

        /* Overlay: each '+'-separated path is one layer.  The first
         * may be empty (read-only overlay).  Empty trailing layers
         * are silently dropped, matching the historical parser. */
        while (*p && r->layer_count < SUD_RULES_MAX_LAYERS) {
            const char *plus = rules_strchr(p, '+');
            size_t llen = plus ? (size_t)(plus - p) : rules_strlen(p);
            r->layers[r->layer_count] = p;
            r->layer_lens[r->layer_count] = llen;
            r->layer_count++;
            if (!plus) break;
            p = plus + 1;
        }
        /* Need at least one non-empty layer to be useful. */
        for (int i = 0; i < r->layer_count; i++)
            if (r->layer_lens[i] > 0) return 1;
        return 0;
    }

    return 0;
}

int sud_rules_parse(const struct sud_runtime_config *cfg,
                    struct sud_rule *rules, int max_rules)
{
    if (!cfg || !rules || max_rules <= 0) return 0;
    int n = 0;
    for (int i = 0; i < cfg->remap_rule_count && n < max_rules; i++) {
        const char *r = cfg->remap_rules[i];
        if (!r) continue;
        const char *colon = rules_strchr(r, ':');
        if (!colon) continue;
        size_t klen = (size_t)(colon - r);

        struct sud_rule *out = &rules[n];
        out->kind = classify_kind(r, klen);
        if (out->kind == SUD_RULE_KIND_UNKNOWN) continue;
        if (parse_one(out, colon + 1)) n++;
    }
    return n;
}

const struct sud_rule *sud_rules_find_filtered(const struct sud_rule *rules,
                                               int rule_count,
                                               const char *path,
                                               unsigned kind_mask,
                                               const char **tail_out)
{
    if (!rules || rule_count <= 0 || !path || path[0] != '/') return 0;
    for (int i = 0; i < rule_count; i++) {
        const struct sud_rule *r = &rules[i];
        if (!r->merged || r->merged_len == 0) continue;
        if (kind_mask && !(kind_mask & (1u << (unsigned)r->kind))) continue;
        if (rules_strncmp(path, r->merged, r->merged_len) != 0) continue;
        if (!is_path_boundary(path[r->merged_len])) continue;
        if (tail_out) *tail_out = path + r->merged_len;
        return r;
    }
    return 0;
}

const struct sud_rule *sud_rules_find(const struct sud_rule *rules,
                                      int rule_count, const char *path,
                                      const char **tail_out)
{
    return sud_rules_find_filtered(rules, rule_count, path, 0, tail_out);
}
