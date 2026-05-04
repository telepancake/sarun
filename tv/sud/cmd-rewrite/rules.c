/*
 * sud/cmd-rewrite/rules.c — Rule parser, pattern matchers, name table.
 *
 * Storage model: rule table is a fixed-size BSS array.  All string
 * fields point into either the runtime config's cmd_rules[] entries
 * (which themselves were interned by sud_runtime_config_intern, so
 * outlive the wrapper) or into g_rule_names[] (this TU's name arena
 * for generated implicit names).  Process-lifetime; no free path.
 *
 * The matcher is freestanding and signal-safe: the existing
 * libc-fs strcmp/strchr/strncmp suffice, and the glob walker is a
 * tight 30-line loop.  No syscalls.
 */

#include "sud/cmd-rewrite/rules.h"
#include "sud/runtime_config.h"
#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"

#if SUD_CMD_RULES_MAX != SUD_RC_MAX_CMD_RULES
#error "SUD_CMD_RULES_MAX must equal SUD_RC_MAX_CMD_RULES"
#endif

/* ---- Built-in flag-skip specs ----------------------------------- */

/* sudo: from `sudo -h` (1.9.x).  We deliberately omit the -e / -E /
 * --edit modes (they take a file list and don't end in a command),
 * the -i / -s shell modes, and the --version-style flags.  Anything
 * we don't recognise terminates the strip and the user falls back
 * to the kernel running real sudo. */
static const struct sud_cmd_strip_spec g_strip_sudo = {
    /* singletons */ "AbHKknPSv",
    /* arg-takers */ "ugCDcpRtTU",
    /* accept --  */ 1,
};

/* fakeroot-ng: rare in practice but included for parity.  Most flag
 * letters we don't recognise; the spec is intentionally narrow. */
static const struct sud_cmd_strip_spec g_strip_fakeroot_ng = {
    /* singletons */ "rdv",
    /* arg-takers */ "loi",
    /* accept --  */ 1,
};

/* env: `env [-i] [-u NAME] [VAR=VALUE]... CMD ARGS...`.  We don't
 * handle VAR=VALUE assignments — they would change the program's
 * envp, and that's a real semantic.  When env's argv has any token
 * containing '=' before the command, fall back to the kernel.
 * Implemented in the strip walker (see addin.c).
 *
 * Singletons: -i (clear environment).  Arg-takers: -u, --unset
 * (single letter only, the long form is rejected). */
static const struct sud_cmd_strip_spec g_strip_env = {
    /* singletons */ "i",
    /* arg-takers */ "u",
    /* accept --  */ 1,
};

const struct sud_cmd_strip_spec *
sud_cmd_strip_default_for(const char *basename)
{
    if (!basename) return 0;
    if (strcmp(basename, "sudo") == 0)        return &g_strip_sudo;
    if (strcmp(basename, "fakeroot-ng") == 0) return &g_strip_fakeroot_ng;
    if (strcmp(basename, "env") == 0)         return &g_strip_env;
    return 0;
}

/* ---- Pattern matching ------------------------------------------- */

static const char *path_basename(const char *path)
{
    if (!path) return "";
    const char *base = path;
    for (const char *p = path; *p; p++)
        if (*p == '/') base = p + 1;
    return base;
}

int sud_cmd_match_basename(const char *path, const char *basename)
{
    if (!path || !basename) return 0;
    return strcmp(path_basename(path), basename) == 0;
}

int sud_cmd_match_path(const char *path, const char *want)
{
    if (!path || !want) return 0;
    return strcmp(path, want) == 0;
}

/* Tiny fnmatch-style matcher.  Supports:
 *   *       any sequence of non-'/' bytes
 *   ?       any single non-'/' byte
 *   [abc]   character class
 *   [a-z]   character range
 *   [!abc]  negated class
 *   \\X     literal X
 *
 * Matches the *basename* of `path` against `glob`.  No FNM_PATHNAME-
 * style '/' handling — basename has no '/' anyway. */
static int glob_match_str(const char *s, const char *p)
{
    while (*p) {
        if (*p == '*') {
            while (*p == '*') p++;
            if (!*p) return 1;
            for (; *s; s++)
                if (glob_match_str(s, p)) return 1;
            return 0;
        }
        if (*p == '?') {
            if (!*s) return 0;
            s++; p++;
            continue;
        }
        if (*p == '[') {
            p++;
            int neg = 0;
            if (*p == '!') { neg = 1; p++; }
            int matched = 0;
            int started = 0;
            unsigned char c = (unsigned char)*s;
            while (*p && (*p != ']' || !started)) {
                started = 1;
                unsigned char a = (unsigned char)*p++;
                if (*p == '-' && p[1] && p[1] != ']') {
                    unsigned char b = (unsigned char)p[1];
                    p += 2;
                    if (c >= a && c <= b) matched = 1;
                } else {
                    if (c == a) matched = 1;
                }
            }
            if (*p == ']') p++;
            if (matched == neg) return 0;
            if (!*s) return 0;
            s++;
            continue;
        }
        if (*p == '\\' && p[1]) p++;
        if (*s != *p) return 0;
        s++; p++;
    }
    return *s == '\0';
}

int sud_cmd_match_glob(const char *path, const char *glob)
{
    if (!path || !glob) return 0;
    return glob_match_str(path_basename(path), glob);
}

int sud_cmd_rule_matches(const struct sud_cmd_rule *r, const char *path)
{
    if (!r || !path) return 0;
    switch (r->match) {
    case SUD_CMD_MATCH_BASENAME: return sud_cmd_match_basename(path, r->pattern);
    case SUD_CMD_MATCH_GLOB:     return sud_cmd_match_glob(path, r->pattern);
    case SUD_CMD_MATCH_PATH:     return sud_cmd_match_path(path, r->pattern);
    default:                     return 0;
    }
}

/* ---- Rule table ------------------------------------------------- */

static struct sud_cmd_rule g_rules[SUD_CMD_RULES_MAX];
static int                 g_rule_count;
static int                 g_init_done;

/* Name arena: implicit rule names "<kind>:<match>:<pattern>" are
 * built once and pointed at from g_rules[].name.  Each rule consumes
 * up to SUD_CMD_RULE_NAME_MAX bytes; allow enough for the table
 * plus the ":kind:match:" prefix. */
static char g_rule_names[SUD_CMD_RULES_MAX][SUD_CMD_RULE_NAME_MAX];

const struct sud_cmd_rule *sud_cmd_rules_table(int *count_out)
{
    if (count_out) *count_out = g_rule_count;
    return g_rules;
}

/* ---- Suppression list ------------------------------------------- */

int sud_cmd_rule_is_suppressed(const char *name)
{
    if (!name || !name[0]) return 0;
    if (!g_sud_runtime_config_present) return 0;
    int n = g_sud_runtime_config.suppress_count;
    if (n > SUD_RC_MAX_SUPPRESS) n = SUD_RC_MAX_SUPPRESS;
    for (int i = 0; i < n; i++) {
        const char *s = g_sud_runtime_config.suppressed[i];
        if (s && strcmp(s, name) == 0) return 1;
    }
    return 0;
}

/* Static arena for suppression names that the addin appends at
 * runtime.  Each entry is up to SUD_CMD_RULE_NAME_MAX bytes; the
 * arena holds up to SUD_RC_MAX_SUPPRESS - (user-seeded entries). */
static char g_supp_arena[SUD_RC_MAX_SUPPRESS][SUD_CMD_RULE_NAME_MAX];
static int  g_supp_arena_used;

void sud_cmd_rule_add_suppression(const char *name)
{
    if (!name || !name[0]) return;
    if (sud_cmd_rule_is_suppressed(name)) return;
    if (g_sud_runtime_config.suppress_count >= SUD_RC_MAX_SUPPRESS) return;
    if (g_supp_arena_used >= SUD_RC_MAX_SUPPRESS) return;

    char *dst = g_supp_arena[g_supp_arena_used++];
    int   off = 0;
    while (off + 1 < SUD_CMD_RULE_NAME_MAX && name[off]) {
        dst[off] = name[off];
        off++;
    }
    dst[off] = '\0';

    g_sud_runtime_config.suppressed[g_sud_runtime_config.suppress_count++] = dst;
}

/* ---- Rule string parser ----------------------------------------- */

/* Returns the offset of the next ':' in s starting at *off, or -1
 * if not found.  Advances *off past the colon on success. */
static int eat_to_colon(const char *s, int *off, int max)
{
    int start = *off;
    while (*off < max && s[*off] && s[*off] != ':') (*off)++;
    if (*off >= max || s[*off] != ':') return -1;
    int end = *off;
    (*off)++;        /* skip the colon */
    return end - start;
}

/* Parse a "kind" token at the head of the rule string.  Returns the
 * enum and advances *off past the trailing colon, or returns
 * SUD_CMD_KIND_INVALID and leaves *off unchanged. */
static enum sud_cmd_rule_kind parse_kind(const char *s, int *off)
{
    int start = *off;
    int len = eat_to_colon(s, off, SUD_CMD_RULE_STR_MAX);
    if (len <= 0) { *off = start; return SUD_CMD_KIND_INVALID; }
    const char *p = s + start;
    if (len == 13 && memcmp(p, "compiler-wrap", 13) == 0)
        return SUD_CMD_KIND_COMPILER_WRAP;
    if (len == 10 && memcmp(p, "exec-strip", 10) == 0)
        return SUD_CMD_KIND_EXEC_STRIP;
    if (len == 7  && memcmp(p, "exec-as",  7) == 0)
        return SUD_CMD_KIND_EXEC_AS;
    *off = start;
    return SUD_CMD_KIND_INVALID;
}

static enum sud_cmd_match_kind parse_match_kind(const char *s, int *off)
{
    int start = *off;
    int len = eat_to_colon(s, off, SUD_CMD_RULE_STR_MAX);
    if (len <= 0) { *off = start; return SUD_CMD_MATCH_INVALID; }
    const char *p = s + start;
    if (len == 8 && memcmp(p, "basename", 8) == 0) return SUD_CMD_MATCH_BASENAME;
    if (len == 4 && memcmp(p, "glob",     4) == 0) return SUD_CMD_MATCH_GLOB;
    if (len == 4 && memcmp(p, "path",     4) == 0) return SUD_CMD_MATCH_PATH;
    *off = start;
    return SUD_CMD_MATCH_INVALID;
}

static int parse_int_field(const char *s, int *off, int *out)
{
    if (!s[*off]) return -1;
    int neg = 0;
    if (s[*off] == '-') { neg = 1; (*off)++; }
    int v = 0;
    int started = 0;
    while (s[*off] >= '0' && s[*off] <= '9') {
        v = v * 10 + (s[*off] - '0');
        (*off)++;
        started = 1;
    }
    if (!started) return -1;
    *out = neg ? -v : v;
    return 0;
}

/* Build the implicit name "<kind>:<match>:<pattern>" into `buf`.
 * Returns 0 on success, -1 if the name doesn't fit. */
static int build_rule_name(char *buf, int buflen,
                           enum sud_cmd_rule_kind kind,
                           enum sud_cmd_match_kind match,
                           const char *pattern)
{
    const char *kstr = "?";
    switch (kind) {
    case SUD_CMD_KIND_COMPILER_WRAP: kstr = "compiler-wrap"; break;
    case SUD_CMD_KIND_EXEC_STRIP:    kstr = "exec-strip";    break;
    case SUD_CMD_KIND_EXEC_AS:       kstr = "exec-as";       break;
    default: return -1;
    }
    const char *mstr = "?";
    switch (match) {
    case SUD_CMD_MATCH_BASENAME: mstr = "basename"; break;
    case SUD_CMD_MATCH_GLOB:     mstr = "glob";     break;
    case SUD_CMD_MATCH_PATH:     mstr = "path";     break;
    default: return -1;
    }
    int n = snprintf(buf, buflen, "%s:%s:%s", kstr, mstr, pattern ? pattern : "");
    if (n < 0 || n >= buflen) return -1;
    return 0;
}

/* Parse one "<kind>:<match>:<pattern>[:<extra>]" string into a rule
 * slot.  Returns 0 on success, -1 on a malformed string.
 *
 * Pattern and extras are *aliased* directly into the source string
 * — the caller has already interned cmd_rules[] via
 * sud_runtime_config_intern, so the pointers stay valid. */
static int parse_rule(const char *s, struct sud_cmd_rule *r, char *name_out)
{
    int off = 0;
    r->kind          = parse_kind(s, &off);
    if (r->kind == SUD_CMD_KIND_INVALID) return -1;
    r->match         = parse_match_kind(s, &off);
    if (r->match == SUD_CMD_MATCH_INVALID) return -1;

    /* Extract the pattern: bytes from off until the next ':' or end. */
    int pat_start = off;
    while (s[off] && s[off] != ':') off++;
    if (off == pat_start) return -1;
    /* Aliasing trick: rather than copying, we advance off past any
     * following ':' and rely on the original string having a NUL or
     * ':' at the right place.  Since the source string is interned
     * (mutable on our side), we patch a NUL where the ':' was — the
     * rule's pattern then ends cleanly. */
    int pat_end = off;
    int has_more = (s[off] == ':');
    /* Cast away const: cmd_rules[] entries were strdup'd by the
     * runtime_config interner, so they're our writable storage. */
    char *m = (char *)s;
    if (has_more) m[pat_end] = '\0';        /* terminate pattern */
    r->pattern = m + pat_start;
    if (has_more) off++;                    /* step past patched NUL */

    r->tool          = 0;
    r->strip_default = 0;
    r->strip.singletons[0] = '\0';
    r->strip.arg_takers[0] = '\0';
    r->strip.accept_ddash  = 0;
    r->as_uid        = -1;
    r->as_gid        = -1;

    if (r->kind == SUD_CMD_KIND_COMPILER_WRAP) {
        if (!has_more || !s[off]) return -1;
        r->tool = m + off;
        /* tool runs to end of string. */
    } else if (r->kind == SUD_CMD_KIND_EXEC_STRIP) {
        if (!has_more || !s[off]) {
            /* Default flag-skip spec from the basename of the
             * pattern; on lookup miss the rule still works but only
             * skips a literal `--` terminator. */
            r->strip_default = 1;
            const struct sud_cmd_strip_spec *def =
                sud_cmd_strip_default_for(r->pattern);
            if (def) r->strip = *def;
            else     r->strip.accept_ddash = 1;
        } else {
            /* Custom spec: "<singletons>:<arg_takers>:<accept_ddash>".
             * Each section is optional; missing sections terminate
             * the parse. */
            int sing_start = off;
            while (s[off] && s[off] != ':') off++;
            int sing_len = off - sing_start;
            if (sing_len >= (int)sizeof(r->strip.singletons))
                sing_len = sizeof(r->strip.singletons) - 1;
            for (int i = 0; i < sing_len; i++)
                r->strip.singletons[i] = s[sing_start + i];
            r->strip.singletons[sing_len] = '\0';

            if (s[off] == ':') {
                off++;
                int arg_start = off;
                while (s[off] && s[off] != ':') off++;
                int arg_len = off - arg_start;
                if (arg_len >= (int)sizeof(r->strip.arg_takers))
                    arg_len = sizeof(r->strip.arg_takers) - 1;
                for (int i = 0; i < arg_len; i++)
                    r->strip.arg_takers[i] = s[arg_start + i];
                r->strip.arg_takers[arg_len] = '\0';
            }
            if (s[off] == ':') {
                off++;
                if (s[off] == '0' || s[off] == '1')
                    r->strip.accept_ddash = (s[off] - '0');
            } else {
                r->strip.accept_ddash = 1;
            }
        }
    } else if (r->kind == SUD_CMD_KIND_EXEC_AS) {
        if (!has_more) return -1;
        if (parse_int_field(s, &off, &r->as_uid) != 0) return -1;
        if (s[off] == ':') {
            off++;
            if (parse_int_field(s, &off, &r->as_gid) != 0) return -1;
        }
    }

    /* Build the implicit name. */
    if (build_rule_name(name_out, SUD_CMD_RULE_NAME_MAX,
                        r->kind, r->match, r->pattern) != 0)
        return -1;
    r->name = name_out;
    return 0;
}

/* Test-only hook: clear the init-done flag and the suppression
 * arena so a unit test can call sud_cmd_rules_init() repeatedly
 * with different runtime configs.  Not exposed in the production
 * header — tests forward-declare and reach in. */
void sud_cmd_rules_reset_for_test(void)
{
    g_init_done = 0;
    g_rule_count = 0;
    g_supp_arena_used = 0;
}

void sud_cmd_rules_init(void)
{
    if (g_init_done) return;
    g_init_done = 1;
    g_rule_count = 0;

    if (!g_sud_runtime_config_present) return;
    int n = g_sud_runtime_config.cmd_rule_count;
    if (n > SUD_CMD_RULES_MAX) n = SUD_CMD_RULES_MAX;
    for (int i = 0; i < n; i++) {
        const char *r = g_sud_runtime_config.cmd_rules[i];
        if (!r || !r[0]) continue;
        if (g_rule_count >= SUD_CMD_RULES_MAX) break;
        struct sud_cmd_rule *slot = &g_rules[g_rule_count];
        char *name = g_rule_names[g_rule_count];
        if (parse_rule(r, slot, name) == 0)
            g_rule_count++;
        /* Malformed rules silently skipped — the launcher is
         * responsible for surfacing CLI errors before getting here. */
    }
}
