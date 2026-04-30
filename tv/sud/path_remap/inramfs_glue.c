/*
 * sud/path_remap/inramfs_glue.c — Routes path-bearing syscalls into
 * the inramfs data store.
 *
 * This file holds the dispatch table that PLAN.md's Part 1 calls
 * for: a single switch on syscall number that, for each path-bearing
 * call (open/stat/mkdir/unlink/...), asks path_remap whether the
 * (dirfd, path) pair lies under the inramfs mount and — if so —
 * invokes the matching sud_inramfs_op_* helper.  No path resolution
 * happens inside sud/inramfs/* anymore; that file is now a pure
 * inode/data store keyed by absolute path and fd.
 *
 * Why this lives in path_remap and not in inramfs:
 *   - The mount prefix, the dirfd table, and the logical CWD all
 *     live in path_remap (sud/path_remap/path.c).  Keeping the
 *     dispatch table next to them means the path → "is this
 *     inramfs?" classifier is resolved without crossing layers.
 *   - inramfs's addin pre_syscall is now fd-bearing-only.  The
 *     addin order (trace → path_remap → inramfs) means path_remap
 *     gets first crack at every path-bearing syscall and short-
 *     circuits inramfs paths via this file before the kernel sees
 *     any path argument.
 *
 * Two-path syscalls (rename/link/renameat/linkat/renameat2) carry
 * their own resolve-twice helper: if the source resolves into the
 * mount but the destination does not (or vice versa), the syscall
 * is rejected with -EXDEV — the same shape the kernel surfaces for
 * cross-FS link/rename.
 *
 * utimensat with a NULL path argument is a futimens-style call on
 * the dirfd; we don't claim it here (the inramfs fd-side handlers
 * cover it via the standard fd-ownership check).
 *
 * All sud_inramfs_op_* helpers return non-negative results or
 * negative -errno values, kernel-syscall convention.  We propagate
 * those through ctx->ret unchanged.
 */

#include "sud/path_remap/inramfs_glue.h"
#include "sud/path_remap/path.h"
#include "sud/inramfs/inramfs.h"
#include "sud/raw.h"

/* libc-fs/libc.h via inramfs.h supplies the SYS_*, AT_*, O_*, F_*,
 * S_IF*, E* constants.  We deliberately don't redefine any of them. */

/* ================================================================
 * Local helpers — short-circuit + resolve.
 * ================================================================ */

static int short_circuit(struct sud_syscall_ctx *ctx, long ret)
{
    ctx->ret = ret;
    return 1;
}

/* Resolve (dirfd, path) into ctx->scratch as a NUL-terminated path
 * known to lie under the inramfs mount.  Returns:
 *    0 → success, *abs_out points into ctx->scratch
 *   -1 → not under inramfs, fall through to caller (overlay/kernel)
 *  <0 → hard error -errno (caller should short-circuit)
 *
 * The scratch buffer must be at least PATH_MAX bytes.  All path-
 * bearing syscalls under sud carry that guarantee via
 * sud_handler_alloc_scratch.
 */
static int resolve_path(struct sud_syscall_ctx *ctx,
                        int dirfd, const char *path,
                        const char **abs_out)
{
    if (!ctx->scratch || ctx->scratch_size < PATH_MAX) return -1;
    int rc = sud_pr_resolve_at_inramfs(dirfd, path,
                                       ctx->scratch, ctx->scratch_size);
    if (rc < 0) return rc;
    *abs_out = ctx->scratch;
    return 0;
}

/* Resolve a (src, dst) pair for rename/link.  The source is copied
 * into the caller-provided `src_save` buffer so that the destination
 * resolve can reuse ctx->scratch.  Cross-FS link/rename — i.e. one
 * side under the mount and the other not — is rejected with -EXDEV,
 * matching kernel semantics.  Returns the same {0, -1, -errno}
 * convention as resolve_path.
 */
static int resolve_two_paths(struct sud_syscall_ctx *ctx,
                             int src_dirfd, const char *src_path,
                             int dst_dirfd, const char *dst_path,
                             char *src_save, size_t src_save_sz,
                             const char **src_out, const char **dst_out)
{
    const char *first;
    int r = resolve_path(ctx, src_dirfd, src_path, &first);
    if (r < 0) return r;
    size_t l = strlen(first);
    if (l + 1 > src_save_sz) return -ENAMETOOLONG;
    memcpy(src_save, first, l + 1);
    *src_out = src_save;
    r = sud_pr_resolve_at_inramfs(dst_dirfd, dst_path,
                                  ctx->scratch, ctx->scratch_size);
    /* Source under mount, destination not → cross-FS. */
    if (r < 0) return -EXDEV;
    *dst_out = ctx->scratch;
    return 0;
}

/* ================================================================
 * Per-syscall handlers.  One tiny function per syscall, each
 * extracting the call's own (mode/flags/buf/...) arguments from
 * ctx->args and forwarding to the matching sud_inramfs_op_* op
 * with the pre-resolved absolute path.
 * ================================================================ */

typedef long (*ir_path_handler)(struct sud_syscall_ctx *ctx,
                                const char *abs);
typedef long (*ir_two_path_handler)(struct sud_syscall_ctx *ctx,
                                    const char *src, const char *dst);

static long h_open_creat(struct sud_syscall_ctx *c, const char *abs)
{
    /* open(path, flags, mode) — flags @[1], mode @[2]. */
    return sud_inramfs_op_open(abs, (int)c->args[1], (int)c->args[2]);
}
static long h_openat_creat(struct sud_syscall_ctx *c, const char *abs)
{
    /* openat(dirfd, path, flags, mode) — flags @[2], mode @[3]. */
    return sud_inramfs_op_open(abs, (int)c->args[2], (int)c->args[3]);
}

static long h_stat_follow  (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_stat(abs, (void *)c->args[1], 1); }
static long h_stat_nofollow(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_stat(abs, (void *)c->args[1], 0); }
static long h_fstatat(struct sud_syscall_ctx *c, const char *abs)
{
    int follow = ((int)c->args[3] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
    return sud_inramfs_op_stat(abs, (void *)c->args[2], follow);
}

static long h_statx(struct sud_syscall_ctx *c, const char *abs)
{
    int follow = ((int)c->args[2] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
    return sud_inramfs_op_statx_fill(abs, follow,
                                     (unsigned int)c->args[3],
                                     (void *)c->args[4]);
}

static long h_access_a1(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_access(abs, (int)c->args[1]); }
static long h_access_a2(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_access(abs, (int)c->args[2]); }

static long h_mkdir_a1 (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_mkdir(abs, (int)c->args[1]); }
static long h_mkdir_a2 (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_mkdir(abs, (int)c->args[2]); }
static long h_rmdir    (struct sud_syscall_ctx *c, const char *abs)
{ (void)c; return sud_inramfs_op_rmdir(abs); }
static long h_unlink   (struct sud_syscall_ctx *c, const char *abs)
{ (void)c; return sud_inramfs_op_unlink(abs); }
static long h_unlinkat (struct sud_syscall_ctx *c, const char *abs)
{
    return ((int)c->args[2] & AT_REMOVEDIR)
        ? sud_inramfs_op_rmdir(abs)
        : sud_inramfs_op_unlink(abs);
}

static long h_symlink_a0(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_symlink((const char *)c->args[0], abs); }

static long h_readlink_a1(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_readlink(abs, (char *)c->args[1], (size_t)c->args[2]); }
static long h_readlink_a2(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_readlink(abs, (char *)c->args[2], (size_t)c->args[3]); }

static long h_chmod_a1 (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_chmod(abs, (int)c->args[1]); }
static long h_chmod_a2 (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_chmod(abs, (int)c->args[2]); }
static long h_chown    (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_chown(abs, (int)c->args[1], (int)c->args[2], 1); }
static long h_lchown   (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_chown(abs, (int)c->args[1], (int)c->args[2], 0); }
static long h_fchownat (struct sud_syscall_ctx *c, const char *abs)
{
    int follow = ((int)c->args[4] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
    return sud_inramfs_op_chown(abs, (int)c->args[2], (int)c->args[3], follow);
}

static long h_truncate (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_truncate(abs, (off_t)c->args[1]); }

static long h_utimensat(struct sud_syscall_ctx *c, const char *abs)
{
    int follow = ((int)c->args[3] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
    return sud_inramfs_op_utimensat(abs,
        (const struct timespec *)c->args[2], follow);
}

static long h_rename(struct sud_syscall_ctx *c, const char *src, const char *dst)
{ (void)c; return sud_inramfs_op_rename(src, dst, 0); }
static long h_renameat2(struct sud_syscall_ctx *c, const char *src, const char *dst)
{ return sud_inramfs_op_rename(src, dst, (unsigned int)c->args[4]); }
static long h_link(struct sud_syscall_ctx *c, const char *src, const char *dst)
{ (void)c; return sud_inramfs_op_link(src, dst); }

/* ================================================================
 * Path-bearing dispatch table.
 *
 * Encoding:
 *   .nr        — syscall number we match.  Rows are skipped at build
 *                time (via #ifdef SYS_xxx) on architectures that lack
 *                the underlying __NR_xxx.
 *   .dirfd_idx — args[] index of the dirfd to combine with the
 *                pathname.  -1 means "use AT_FDCWD".
 *   .path_idx  — args[] index of the pathname.
 *   .path_h    — handler function called once the path is resolved.
 *
 * Two-path syscalls (rename/link/renameat/linkat/renameat2) live in
 * a separate dispatch arm below because their resolve step is a
 * pair, not a single lookup.
 * ================================================================ */
struct ir_path_row {
    long             nr;
    signed char      dirfd_idx;
    signed char      path_idx;
    ir_path_handler  path_h;
};

#define ROW(SYSNR, DIRFDIDX, PATHIDX, H) \
    { SYSNR, (DIRFDIDX), (PATHIDX), (H) }

static const struct ir_path_row ir_path_dispatch[] = {
    /* open / openat — open's dirfd is implicit AT_FDCWD. */
#ifdef SYS_open
    ROW(SYS_open,    -1, 0, h_open_creat),
#endif
#ifdef SYS_openat
    ROW(SYS_openat,   0, 1, h_openat_creat),
#endif

    /* stat family */
#ifdef SYS_stat
    ROW(SYS_stat,    -1, 0, h_stat_follow),
#endif
#ifdef SYS_lstat
    ROW(SYS_lstat,   -1, 0, h_stat_nofollow),
#endif
#ifdef SYS_stat64
    ROW(SYS_stat64,  -1, 0, h_stat_follow),
#endif
#ifdef SYS_lstat64
    ROW(SYS_lstat64, -1, 0, h_stat_nofollow),
#endif
#ifdef SYS_newfstatat
    ROW(SYS_newfstatat, 0, 1, h_fstatat),
#endif
#ifdef SYS_fstatat64
    ROW(SYS_fstatat64,  0, 1, h_fstatat),
#endif
#ifdef SYS_statx
    ROW(SYS_statx,      0, 1, h_statx),
#endif

    /* access family */
#ifdef SYS_access
    ROW(SYS_access,     -1, 0, h_access_a1),
#endif
#ifdef SYS_faccessat
    ROW(SYS_faccessat,   0, 1, h_access_a2),
#endif
#ifdef SYS_faccessat2
    ROW(SYS_faccessat2,  0, 1, h_access_a2),
#endif

    /* directory ops */
#ifdef SYS_mkdir
    ROW(SYS_mkdir,    -1, 0, h_mkdir_a1),
#endif
#ifdef SYS_mkdirat
    ROW(SYS_mkdirat,   0, 1, h_mkdir_a2),
#endif
#ifdef SYS_rmdir
    ROW(SYS_rmdir,    -1, 0, h_rmdir),
#endif
#ifdef SYS_unlink
    ROW(SYS_unlink,   -1, 0, h_unlink),
#endif
#ifdef SYS_unlinkat
    ROW(SYS_unlinkat,  0, 1, h_unlinkat),
#endif

    /* symlink/readlink — for symlink the *new name* is the path that
     * fs lookup applies to (args[1] for symlink, args[2] for
     * symlinkat); the target string is opaque text. */
#ifdef SYS_symlink
    ROW(SYS_symlink,   -1, 1, h_symlink_a0),
#endif
#ifdef SYS_symlinkat
    ROW(SYS_symlinkat,  1, 2, h_symlink_a0),
#endif
#ifdef SYS_readlink
    ROW(SYS_readlink,  -1, 0, h_readlink_a1),
#endif
#ifdef SYS_readlinkat
    ROW(SYS_readlinkat, 0, 1, h_readlink_a2),
#endif

    /* chmod / chown */
#ifdef SYS_chmod
    ROW(SYS_chmod,    -1, 0, h_chmod_a1),
#endif
#ifdef SYS_fchmodat
    ROW(SYS_fchmodat,  0, 1, h_chmod_a2),
#endif
#ifdef SYS_chown
    ROW(SYS_chown,    -1, 0, h_chown),
#endif
#ifdef SYS_lchown
    ROW(SYS_lchown,   -1, 0, h_lchown),
#endif
#ifdef SYS_fchownat
    ROW(SYS_fchownat,  0, 1, h_fchownat),
#endif

    /* truncate */
#ifdef SYS_truncate
    ROW(SYS_truncate,   -1, 0, h_truncate),
#endif
#ifdef SYS_truncate64
    ROW(SYS_truncate64, -1, 0, h_truncate),
#endif
};

#define IR_PATH_DISPATCH_LEN \
    ((int)(sizeof(ir_path_dispatch)/sizeof(ir_path_dispatch[0])))

static int dispatch_single_path(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;
    for (int i = 0; i < IR_PATH_DISPATCH_LEN; i++) {
        const struct ir_path_row *row = &ir_path_dispatch[i];
        if (row->nr != nr) continue;
        int dirfd = (row->dirfd_idx < 0) ? AT_FDCWD
                                         : (int)ctx->args[row->dirfd_idx];
        const char *abs;
        int r = resolve_path(ctx, dirfd,
                             (const char *)ctx->args[row->path_idx], &abs);
        if (r == -1) return 0;          /* not under mount */
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, row->path_h(ctx, abs));
    }
    return 0;
}

/* Two-path syscalls: rename/link family. */
static int dispatch_two_path(struct sud_syscall_ctx *ctx,
                             int src_dirfd_idx, int src_path_idx,
                             int dst_dirfd_idx, int dst_path_idx,
                             ir_two_path_handler h)
{
    char src_save[PATH_MAX];
    int src_dirfd = (src_dirfd_idx < 0) ? AT_FDCWD
                                        : (int)ctx->args[src_dirfd_idx];
    int dst_dirfd = (dst_dirfd_idx < 0) ? AT_FDCWD
                                        : (int)ctx->args[dst_dirfd_idx];
    const char *src, *dst;
    int r = resolve_two_paths(ctx,
                              src_dirfd, (const char *)ctx->args[src_path_idx],
                              dst_dirfd, (const char *)ctx->args[dst_path_idx],
                              src_save, sizeof(src_save), &src, &dst);
    if (r == -1) return 0;
    if (r < 0)   return short_circuit(ctx, r);
    return short_circuit(ctx, h(ctx, src, dst));
}

/* ================================================================
 * Public entry point.
 * ================================================================ */

int sud_pr_inramfs_route_pre_syscall(struct sud_syscall_ctx *ctx)
{
    /* No mount → nothing to route.  This is the hot exit when
     * the inramfs addin is built in but no `--remap-rule inramfs:`
     * has been configured for this process. */
    if (!sud_pr_inramfs_mount_path()) return 0;

    long nr = ctx->nr;

    /* Two-path ops live outside the table because their resolve
     * step is a pair, not a single lookup.  Match them first
     * because rename/link don't appear in ir_path_dispatch[]. */
#ifdef SYS_rename
    if (nr == SYS_rename)
        return dispatch_two_path(ctx, -1, 0, -1, 1, h_rename);
#endif
#ifdef SYS_renameat
    if (nr == SYS_renameat)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_rename);
#endif
#ifdef SYS_renameat2
    if (nr == SYS_renameat2)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_renameat2);
#endif
#ifdef SYS_link
    if (nr == SYS_link)
        return dispatch_two_path(ctx, -1, 0, -1, 1, h_link);
#endif
#ifdef SYS_linkat
    if (nr == SYS_linkat)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_link);
#endif

    /* utimensat: a NULL path means "operate on dirfd as if it were
     * an open fd" (futimens semantics).  That case is handled by
     * inramfs's fd-bearing dispatch (sud_inramfs_op_futimens), so
     * we only claim the path-bearing form here. */
#ifdef SYS_utimensat
    if (nr == SYS_utimensat) {
        const char *p = (const char *)ctx->args[1];
        if (!p) return 0;
        const char *abs;
        int r = resolve_path(ctx, (int)ctx->args[0], p, &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, h_utimensat(ctx, abs));
    }
#endif

    /* Everything else (open/stat/mkdir/unlink/...) lives in the
     * single-path dispatch table. */
    return dispatch_single_path(ctx);
}
