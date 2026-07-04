/*
 * sud/cmd-rewrite/rules.h — Rule types and matchers for cmd-rewrite.
 *
 * cmd-rewrite owns three rule kinds, all of which share a common
 * "match a basename / glob / abs-path" predicate:
 *
 *   compiler-wrap  — prepend a wrapping tool (typically ccache) to
 *                    argv when the binary about to be exec'd matches.
 *   exec-strip     — strip the matching wrapper command (sudo,
 *                    fakeroot-ng, env, ...) from argv and run the
 *                    inner program directly.  Each rule carries a
 *                    flag-skip spec describing which of the
 *                    wrapper's own option words to consume.
 *   exec-as        — propagate a pretend-uid (and optional gid) via
 *                    the runtime config, so the matched process and
 *                    every descendant gets the existing fakeroot
 *                    addin's getuid/geteuid short-circuit value.
 *
 * Each rule has an implicit name = "<kind>:<match-kind>:<pattern>",
 * used for the cross-cutting suppression mechanism (PLAN.md cmd-
 * rewrite section): once a rule fires, its name is auto-appended to
 * the outgoing wrapper's --suppress-rule list, killing recursive
 * re-firing in descendants without the user having to enumerate
 * every wrapper-program pairing manually.
 *
 * Header is freestanding-safe: the rules table is a fixed-size array
 * sitting in BSS, populated in sud_cmd_rules_init() from the
 * runtime config's cmd_rules[] strings.  No allocator dependency.
 */

#ifndef SUD_CMD_REWRITE_RULES_H
#define SUD_CMD_REWRITE_RULES_H

#include "libc-fs/libc.h"

#define SUD_CMD_RULES_MAX        SUD_RC_MAX_CMD_RULES_FROM_RC
/* Avoid pulling runtime_config.h here just for the constant — the
 * .c file does the include and asserts the equivalence. */
#define SUD_RC_MAX_CMD_RULES_FROM_RC 32

/* Maximum length of a serialised rule "<kind>:<spec>" string. */
#define SUD_CMD_RULE_STR_MAX     256
/* Maximum length of an implicit rule name "<kind>:<match>:<pattern>". */
#define SUD_CMD_RULE_NAME_MAX    192

enum sud_cmd_rule_kind {
    SUD_CMD_KIND_INVALID      = 0,
    SUD_CMD_KIND_COMPILER_WRAP,
    SUD_CMD_KIND_EXEC_STRIP,
    SUD_CMD_KIND_EXEC_AS,
};

enum sud_cmd_match_kind {
    SUD_CMD_MATCH_INVALID  = 0,
    SUD_CMD_MATCH_BASENAME,
    SUD_CMD_MATCH_GLOB,
    SUD_CMD_MATCH_PATH,
};

/* exec-strip flag-skip spec: which option words to consume after the
 * stripped wrapper.  Each char in `singletons` is a one-letter flag
 * with no argument (sudo's -A, -E, -H, -K, -k, -n, -P, -S, -V, ...).
 * Each char in `arg_takers` is a one-letter flag whose next word is
 * its argument (sudo's -u <user>, -p <prompt>, ...).  Anything not in
 * either set, or an unknown long option, terminates the skip.
 *
 * `accept_ddash` says whether the spec accepts `--` as an explicit
 * end-of-flags terminator.  Set for every well-known wrapper. */
struct sud_cmd_strip_spec {
    char  singletons[24];     /* NUL-terminated */
    char  arg_takers[16];     /* NUL-terminated */
    int   accept_ddash;
};

/* Parsed-and-cached rule.  Strings (`pattern`, `tool`) refer into
 * the original cmd_rules[] entry — no copying — so the rule table's
 * lifetime is bounded by the runtime config's. */
struct sud_cmd_rule {
    enum sud_cmd_rule_kind  kind;
    enum sud_cmd_match_kind match;
    /* Match pattern: basename / fnmatch glob / absolute path,
     * depending on `match`. */
    const char             *pattern;
    /* Implicit name: "<kind>:<match>:<pattern>".  Stored separately
     * so suppression checks don't have to re-build it on every
     * dispatch.  Lives in g_rule_names[]. */
    const char             *name;

    /* compiler-wrap: tool to prepend ("/usr/bin/ccache" etc.). */
    const char             *tool;

    /* exec-strip: flag-skip spec.  `strip_default` means: use the
     * built-in spec for known wrappers (sudo, fakeroot-ng, env);
     * otherwise the rule carries its own. */
    int                     strip_default;
    struct sud_cmd_strip_spec strip;

    /* exec-as: uid/gid to propagate.  -1 means "inherit". */
    int                     as_uid;
    int                     as_gid;
};

/* ---- Module API ------------------------------------------------- */

/* Parse the runtime config's cmd_rules[] strings into the internal
 * rule table.  Idempotent.  Bad rule strings are logged and skipped;
 * one bad rule does not invalidate the others. */
void sud_cmd_rules_init(void);

/* Return the parsed rule table.  *count_out is set to the number of
 * valid rules.  Pointer is stable for the process lifetime. */
const struct sud_cmd_rule *sud_cmd_rules_table(int *count_out);

/* Returns 1 iff the rule named `name` is in the runtime config's
 * suppression list (either user-supplied via --suppress-rule or
 * auto-added by a prior firing). */
int sud_cmd_rule_is_suppressed(const char *name);

/* Append `name` to the runtime config's suppressed[] list.  No-op if
 * already present or if the list is full.  Used after a rule fires
 * so the wrapper-rewrite re-emits the suppression to the child. */
void sud_cmd_rule_add_suppression(const char *name);

/* Match predicates — exposed for unit tests. */
int sud_cmd_match_basename(const char *path, const char *basename);
int sud_cmd_match_glob(const char *path, const char *glob);
int sud_cmd_match_path(const char *path, const char *want);

/* Dispatch a rule against `path` (resolved binary path) and `argv`.
 * Returns 1 on a match (caller honours rule.kind), 0 otherwise. */
int sud_cmd_rule_matches(const struct sud_cmd_rule *r,
                         const char *path);

/* Default flag-skip specs, exposed so tests can verify them. */
const struct sud_cmd_strip_spec *sud_cmd_strip_default_for(const char *basename);

#endif /* SUD_CMD_REWRITE_RULES_H */
