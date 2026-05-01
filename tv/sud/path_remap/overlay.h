/*
 * sud/path_remap/overlay.h — Overlayfs-style path remapping for sud.
 *
 * The path_remap addin presents the traced program with a merged view
 * of multiple "lower" directories and one optional "upper" directory,
 * matching the semantics of the Linux kernel overlayfs:
 *
 *   - read lookups walk upper, then lower1, lower2, ... and return
 *     the first hit;
 *   - writes are redirected to the upper layer (parent directories
 *     are auto-created in upper to mirror the merged tree);
 *   - whiteouts (character devices with rdev == 0, the same on-disk
 *     marker overlayfs uses) in upper hide the corresponding name in
 *     all lower layers.
 *
 * Configuration via wrapper CLI flags (parsed by sud/wrapper.c into
 * g_sud_runtime_config; --remap-rule may be repeated):
 *
 *   --remap-rule overlay:<merged>=<upper>+<lower1>[+<lower2>...]
 *       Full overlay rule.  The first path after `=` is the upper
 *       (writable) layer.  Each `+`-separated path that follows is a
 *       lower (read-only) layer, in priority order.  An empty upper
 *       (`overlay:<merged>=+<lower1>+<lower2>`) makes the rule
 *       read-only: writes return -EROFS.
 *
 *   --remap-rule remap:<src>=<dst>
 *       Single-layer remap (treated as a degenerate overlay rule with
 *       `dst` as both upper and the only lower — i.e. simple
 *       bidirectional path rewriting).
 *
 *   --remap-rule passthrough:<prefix>
 *       Explicit "leave the syscall arg untouched" rule for any path
 *       under <prefix>.  Useful as an escape hatch for sub-prefixes
 *       of a wider overlay/remap rule: list the passthrough rule
 *       BEFORE the wider rule so find_rule()'s first-match-wins loop
 *       picks it.  Has no upper/lowers; for_write is irrelevant
 *       (read and write both pass through identically).
 *
 * All overlay rule paths must be absolute and contain no trailing
 * slash; the merged prefix is matched on a path-component boundary so
 * that `/m` does not match `/mfoo`.
 */

#ifndef SUD_PATH_REMAP_OVERLAY_H
#define SUD_PATH_REMAP_OVERLAY_H

#include "libc-fs/libc.h"

/* Result of sud_overlay_resolve(): */
enum {
    /* Path is not under any overlay rule — caller should leave the
     * argument untouched. */
    SUD_OVERLAY_PASSTHROUGH = 0,
    /* Path was resolved; `out` contains the rewritten absolute path. */
    SUD_OVERLAY_RESOLVED    = 1,
    /* Path is under an overlay rule but is masked by a whiteout in
     * upper, so the syscall must fail with -ENOENT. */
    SUD_OVERLAY_WHITEOUT    = 2,
    /* Path is under an overlay rule with no upper, so a write attempt
     * must fail with -EROFS. */
    SUD_OVERLAY_READONLY    = 3,
};

/* Parse SUD_OVERLAY and SUD_REMAP from the environment.  Idempotent:
 * subsequent calls are no-ops.  Safe to call from wrapper_init time
 * (uses libc malloc which uses mmap). */
void sud_overlay_init(void);

/* Number of overlay rules currently configured (0 means the addin
 * is effectively a no-op). */
int sud_overlay_rule_count(void);

/* Resolve `path` (absolute) against the configured overlay rules.
 *
 *   for_write != 0  — caller intends to create / modify the named
 *                     entry, so resolution returns the upper-layer
 *                     path and ensures parent directories exist in
 *                     upper (best-effort copy-up).
 *   for_write == 0  — caller is reading, so resolution walks upper
 *                     then each lower in order and returns the first
 *                     existing entry.
 *
 * On SUD_OVERLAY_RESOLVED the rewritten path is written to `out`
 * (NUL-terminated, no overflow).  On SUD_OVERLAY_PASSTHROUGH /
 * _WHITEOUT / _READONLY, `out` is left untouched.
 */
int sud_overlay_resolve(const char *path, int for_write,
                        char *out, size_t out_sz);

/* Same as sud_overlay_resolve(), but for the *at-syscall variants:
 *
 *   - If `path` is absolute, dirfd is ignored and the absolute path
 *     is resolved as above.
 *   - If `path` is relative and dirfd == AT_FDCWD, the current working
 *     directory is read from /proc/self/cwd and prepended.
 *   - If `path` is relative and dirfd is a real fd opened against
 *     a synthetic merged directory (created earlier by
 *     sud_overlay_open_dir), the merged path of that dirfd is used as
 *     the base.  Otherwise the call passes through.
 */
int sud_overlay_resolve_at(int dirfd, const char *path, int for_write,
                           char *out, size_t out_sz);

/* Construct an overlayfs whiteout (S_IFCHR | 0, rdev=0) at the upper
 * location corresponding to the merged `path`.  Returns 0 on success
 * or a negative errno.  Used after a successful unlink/rmdir of an
 * entry that was visible from a lower layer. */
int sud_overlay_create_whiteout(const char *path);

/* Open a merged directory: materialize a synthetic directory under
 * /tmp/.sud-overlay/<pid>-<seq>/ populated with one symlink per merged
 * entry (skipping whiteouts, deduplicating by name with upper-then-
 * lowers priority), then open it with the requested flags.
 *
 * Returns:
 *   >= 0           — fd of the synthetic directory (caller should
 *                    return this to the traced program);
 *   negative errno — error to surface to the traced program;
 *   special value INT_MIN (== SUD_OVERLAY_NO_DIR) if the path is not
 *   under any overlay rule (caller should fall back to the normal
 *   path-resolution + raw open).
 */
#define SUD_OVERLAY_NO_DIR ((int)0x80000000)
int sud_overlay_open_dir(const char *path, int flags, int mode);

/* Same as sud_overlay_open_dir but accepts a (dirfd, path) pair using
 * the same dirfd semantics as sud_overlay_resolve_at(). */
int sud_overlay_open_dir_at(int dirfd, const char *path, int flags, int mode);

/* For test harnesses: reset all internal overlay state (rules, fd
 * tracking, synthetic-dir counter).  Not used at runtime. */
void sud_overlay_reset_for_testing(void);

#endif /* SUD_PATH_REMAP_OVERLAY_H */
