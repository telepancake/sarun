/*
 * sud/path_remap/inramfs_glue.h — Path-bearing syscall router for
 * inramfs, owned by path_remap.
 *
 * After PLAN.md Part 1, path_remap is the single layer that decides
 * what to do with path-bearing syscalls.  When a path resolves under
 * the inramfs mount, path_remap routes it to inramfs's data-store
 * ops (sud_inramfs_op_*) via the function declared here.
 *
 * The dispatch table — `h_open*`, `h_stat*`, `h_chmod*`, the
 * two-path rename/link arms, the path-bearing utimensat — all
 * physically live in inramfs_glue.c rather than in
 * sud/inramfs/addin.c.  This is the "inramfs_glue.c" referenced in
 * PLAN.md's "Module / file shape after the change" section.
 *
 * inramfs/addin.c is left with the fd-bearing syscalls only
 * (read/write/lseek/close/dup/fcntl/mmap/...) plus the munmap
 * shadow-region handling and the zero-copy-fallback refusals.
 *
 * Compiled only when both SUD_ADDIN_PATH_REMAP and SUD_ADDIN_INRAMFS
 * are configured.
 */
#ifndef SUD_PATH_REMAP_INRAMFS_GLUE_H
#define SUD_PATH_REMAP_INRAMFS_GLUE_H

#include "sud/addin.h"
#include "libc-fs/libc.h"

/* ---------------------------------------------------------------- */
/* Ticket-based resolver — inode-indexed primary form                */
/* ---------------------------------------------------------------- */

/* A resolved (dirfd, path) handle for the inramfs layer.  Produced
 * by sud_pr_resolve_at_inramfs_ticket; consumed by the inode-indexed
 * `sud_inramfs_op_*_inode` and `sud_inramfs_op_*_at_inode` ops.
 *
 * Field invariants on success (return value 0):
 *   - `abs_path` is the resolved absolute path under the mount
 *     (NUL-terminated; valid for the lifetime of the caller-provided
 *     buffer).  Used by `sud_inramfs_op_open_inode` for dirfd
 *     registration; ignore it otherwise.
 *   - `leaf_idx` is the leaf inode index (1-based), or 0 when the
 *     leaf does not exist (only meaningful when `want_parent` was
 *     set — the caller is about to create the leaf).
 *   - `parent_idx`, `basename`, `basename_len` are populated only
 *     when `want_parent` was set on the resolve call.  `basename`
 *     points into `abs_path` (so the abs_path buffer must live at
 *     least as long as you intend to read the basename).
 *   - `is_mount_root` is set when (dirfd, path) names the mount
 *     root itself; mutating ops should refuse with the appropriate
 *     errno.
 */
struct sud_pr_inramfs_ticket {
    const char *abs_path;
    uint32_t    leaf_idx;
    uint32_t    parent_idx;
    const char *basename;
    size_t      basename_len;
    int         is_mount_root;
};

/* Resolve (dirfd, path) into a structured ticket.
 *
 *   - `follow`: 1 to follow trailing symlinks (stat/access),
 *               0 to leave them un-followed (lstat/symlink/readlink).
 *   - `want_parent`: 1 for namespace mutators (mkdir/unlink/rename
 *                    /symlink/link) that need parent + basename;
 *                    0 for read-side leaf ops.
 *   - `abs_buf`/`abs_buf_sz`: caller-owned scratch buffer for the
 *     absolutised path; the ticket borrows from it.
 *
 * Returns:
 *    0 — success; `out` is fully populated.  When `want_parent`
 *        is 0 a missing leaf surfaces as -ENOENT (return value);
 *        when `want_parent` is 1 a missing leaf is reflected by
 *        `leaf_idx == 0` and a successful return.
 *   -1 — (dirfd, path) is not under the inramfs mount; caller
 *        should pass through to the kernel / handle via overlay.
 *        `out` is left untouched.
 *  -errno — hard resolution failure (ENAMETOOLONG, EXDEV for an
 *        unknown dirfd, ENOENT for a missing leaf in
 *        want_parent==0 mode, etc.).
 */
int sud_pr_resolve_at_inramfs_ticket(int dirfd, const char *path,
                                     int follow, int want_parent,
                                     char *abs_buf, size_t abs_buf_sz,
                                     struct sud_pr_inramfs_ticket *out);

/* Try to handle a path-bearing syscall by routing it to the inramfs
 * data store.  Returns 1 if the syscall was short-circuited (with
 * ctx->ret holding the result, kernel-syscall convention), or 0 if
 * the call is not for inramfs (caller should continue with overlay /
 * remap dispatch or kernel pass-through).
 *
 * Safe to call when no inramfs mount is configured: returns 0
 * immediately in that case.
 */
int sud_pr_inramfs_route_pre_syscall(struct sud_syscall_ctx *ctx);

#endif /* SUD_PATH_REMAP_INRAMFS_GLUE_H */
