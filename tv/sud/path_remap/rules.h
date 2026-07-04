/*
 * sud/path_remap/rules.h — Shared parser + prefix-lookup for the
 * --remap / --overlay / --passthrough / --fakeroot / --inramfs rule
 * list.
 *
 * The same rule list is consumed by two callers in two very different
 * compilation environments:
 *
 *   1. The freestanding sud32/sud64 wrapper, via sud/path_remap/
 *      overlay.c, which uses raw_syscall6() to walk overlay layers,
 *      manage CoW, etc.
 *
 *   2. The libc-linked sudtrace launcher, via sud/sudtrace.c, which
 *      needs to read the user's script and the shebang interpreter
 *      off disk before forking the wrapper — and so needs to apply
 *      the same path rewrites the wrapper would.
 *
 * To keep that logic in exactly one place, this TU is written to
 * compile in BOTH environments: it touches no libc headers other
 * than <stddef.h>, declares the few libc symbols it actually needs,
 * and does no IO.  Callers do their own stat/open against the
 * candidate paths the resolver returns.
 *
 * Naming convention used throughout:
 *   - "merged" / "virtual" — what the traced program sees;
 *     the LHS of `<lhs>=<rhs>` in --remap / --overlay.
 *   - "layer" — a real on-disk prefix the syscall actually hits;
 *     the RHS.  For --remap it is a single layer.  For --overlay it
 *     is the upper (writable) layer followed by the lowers
 *     (read-only) in priority order.
 */

#ifndef SUD_PATH_REMAP_RULES_H
#define SUD_PATH_REMAP_RULES_H

#include <stddef.h>
#include "sud/runtime_config.h"

/* Match the historical limits in sud/path_remap/overlay.c so we don't
 * silently truncate rules a user has been driving for a while. */
#define SUD_RULES_MAX_RULES   SUD_RC_MAX_REMAP_RULES
#define SUD_RULES_MAX_LAYERS  9     /* 1 upper + up to 8 lowers       */

enum sud_rule_kind {
    SUD_RULE_KIND_UNKNOWN     = 0,
    SUD_RULE_KIND_PASSTHROUGH = 1,
    SUD_RULE_KIND_REMAP       = 2,
    SUD_RULE_KIND_OVERLAY     = 3,
    SUD_RULE_KIND_FAKEROOT    = 4,
    SUD_RULE_KIND_INRAMFS     = 5,
};

struct sud_rule {
    enum sud_rule_kind kind;

    /* Virtual prefix the rule matches — must be absolute and have no
     * trailing slash for matching to work on a path-component
     * boundary.  Strings inside `merged` and `layers[]` alias the
     * input cfg->remap_rules entries (which themselves alias argv);
     * the rule struct never owns memory. */
    const char *merged;
    size_t      merged_len;

    /* Storage layers in priority order.
     *   --remap:    layers[0] = real, layer_count = 1.
     *   --overlay:  layers[0] = upper (may have layer_lens[0] == 0
     *               for a read-only overlay); layers[1..] = lowers.
     *   --passthrough / --fakeroot / --inramfs: layer_count = 0.
     *
     * No de-duplication or normalisation is done on the strings here:
     * the caller is expected to compose <layers[i]><tail> into a
     * destination buffer via sud_rules_compose() and stat/open the
     * result themselves. */
    const char *layers[SUD_RULES_MAX_LAYERS];
    size_t      layer_lens[SUD_RULES_MAX_LAYERS];
    int         layer_count;
};

/* Parse cfg->remap_rules into rules[0..max_rules) in CLI order.
 * Returns the number of rules populated.  Strings inside *rules
 * alias cfg->remap_rules — no allocation, no copying. */
int sud_rules_parse(const struct sud_runtime_config *cfg,
                    struct sud_rule *rules, int max_rules);

/* First-match-wins prefix lookup.  Returns the matched rule and
 * writes the path tail (a pointer into `path`, may be "" or
 * "/...") to *tail_out.  Returns NULL on no match.  `path` must be
 * absolute. */
const struct sud_rule *sud_rules_find(const struct sud_rule *rules,
                                      int rule_count, const char *path,
                                      const char **tail_out);

/* Same, but only consider rules whose kind has its bit set in
 * `kind_mask` (e.g. (1<<SUD_RULE_KIND_OVERLAY)|(1<<SUD_RULE_KIND_REMAP)).
 * The path-rewriting layer (sud/path_remap/overlay.c) uses this to
 * skip --fakeroot rules — those don't rewrite paths, only metadata,
 * and the historical CLI ordering depends on the path-rewriting
 * search ignoring them. */
const struct sud_rule *sud_rules_find_filtered(const struct sud_rule *rules,
                                               int rule_count,
                                               const char *path,
                                               unsigned kind_mask,
                                               const char **tail_out);

/* Append a NUL-terminated `tail` to a (`prefix`, `prefix_len`) base
 * into out[0..out_sz).  Returns 1 on success, 0 on overflow.  Does
 * not insert a separator; caller passes a tail starting with '/' or
 * empty. */
int sud_rules_compose(char *out, size_t out_sz,
                      const char *prefix, size_t prefix_len,
                      const char *tail);

#endif /* SUD_PATH_REMAP_RULES_H */
