/*
 * sud/path_remap/inramfs_glue.c — Routes path-bearing syscalls into
 * the inramfs data store.
 *
 * This file holds the dispatch table that PLAN.md's Part 1 calls
 * for: a single switch on syscall number that, for each path-bearing
 * call (open/stat/mkdir/unlink/...), asks path_remap whether the
 * (dirfd, path) pair lies under the inramfs mount and — if so —
 * invokes the matching sud_inramfs_op_* helper.  No path resolution
 * happens inside sud/inramfs/ anymore; that file is now a pure
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
#include "sud/path_remap/overlay.h"
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

/* Implementation of the ticket resolver declared in inramfs_glue.h.
 * Two-step: (1) absolutise (dirfd, path) via path_remap and reject
 * paths outside the mount; (2) hand the absolute path to inramfs's
 * walker via the public sud_inramfs_resolve_inode primitive.  The
 * ticket borrows abs_path from the caller-provided abs_buf. */
int sud_pr_resolve_at_inramfs_ticket(int dirfd, const char *path,
                                     int follow, int want_parent,
                                     char *abs_buf, size_t abs_buf_sz,
                                     struct sud_pr_inramfs_ticket *out)
{
    if (!out) return -EFAULT;
    out->abs_path      = 0;
    out->leaf_idx      = 0;
    out->parent_idx    = 0;
    out->basename      = 0;
    out->basename_len  = 0;
    out->is_mount_root = 0;

    int rc = sud_pr_resolve_at_inramfs(dirfd, path, abs_buf, abs_buf_sz);
    if (rc < 0) return rc;     /* -1 (not under mount) or -errno */
    out->abs_path = abs_buf;

    /* Detect "is this exactly the mount root?".  Strip any trailing
     * slashes and compare to the mount path.  Used by callers that
     * need to surface EEXIST/EBUSY/EISDIR for ops on the root
     * itself (mkdir/rmdir/unlink). */
    {
        const char *m = sud_pr_inramfs_mount_path();
        size_t mlen = sud_pr_inramfs_mount_len();
        size_t L = strlen(abs_buf);
        while (L > 1 && abs_buf[L - 1] == '/') L--;
        if (m && L == mlen && memcmp(abs_buf, m, mlen) == 0)
            out->is_mount_root = 1;
    }

    long lrc = sud_inramfs_resolve_inode(abs_buf, follow, want_parent,
                                         &out->leaf_idx,
                                         &out->parent_idx,
                                         &out->basename,
                                         &out->basename_len);
    if (lrc < 0) return (int)lrc;
    return 0;
}

/* Resolve (dirfd, path) into a full ticket using ctx->scratch as the
 * abs-path buffer.  Returns:
 *    0 → success, *out populated (borrows from ctx->scratch).
 *   -1 → not under inramfs, fall through to caller (overlay/kernel).
 *  <0 → hard error -errno (caller should short-circuit).
 *
 * The scratch buffer must be at least PATH_MAX bytes.  All path-
 * bearing syscalls under sud carry that guarantee via
 * sud_handler_alloc_scratch.
 */
static int resolve_ticket(struct sud_syscall_ctx *ctx,
                          int dirfd, const char *path,
                          int follow, int want_parent,
                          struct sud_pr_inramfs_ticket *out)
{
    if (!ctx->scratch || ctx->scratch_size < PATH_MAX) return -1;
    return sud_pr_resolve_at_inramfs_ticket(dirfd, path, follow,
                                            want_parent,
                                            ctx->scratch,
                                            ctx->scratch_size, out);
}

/* ================================================================
 * Cross-store bridge — kills the inramfs <-> overlay EXDEV class.
 *
 * A rename/link with ONE side under the inramfs mount (/tmp) and the
 * other in the overlay tree is, at the sud layer, genuinely cross-
 * device (two backing stores). The FUSE backend presents /tmp and the
 * tree as ONE device, so a build that stages a file in /tmp and moves
 * it into the tree "just works" there. To match FUSE we copy the bytes
 * across and — for rename — drop the source, exactly as `mv` does over
 * real filesystems, but TRANSPARENTLY: a tool that issues a bare
 * rename(2) and never handles EXDEV still wins (which is the failure a
 * plain overlay-vs-inramfs EXDEV caused: the staged output vanished and
 * the next build step hit "No such file"). Directories are not bridged
 * (return -EXDEV, as the kernel does for a cross-fs directory rename).
 * ================================================================ */

/* Kernel stat layout, arch-pinned (only st_mode is read). Matches the
 * struct the *stat syscalls / sud_inramfs_op_stat_inode fill — same
 * layout sud/path_remap/overlay.c depends on. */
struct xs_stat {
#if defined(__x86_64__)
    unsigned long _a[3];
    unsigned int  st_mode;   /* offset 24 */
    unsigned int  _b[9];
    unsigned long _rest[16];
#else
    unsigned long long _a0;
    unsigned char  _pad0[4];
    unsigned long  _ino;
    unsigned int   st_mode;  /* offset 16 */
    unsigned int   _b[7];
    unsigned long  _rest[16];
#endif
};

/* Copy chunk. Kept SMALL on purpose: the bridge runs inside the SIGSYS
 * handler, whose frames must stay lean (the handler's own comment warns a
 * few KiB per frame; a large on-stack buffer here risks overrunning the
 * per-task signal stack under a deep/heavily-threaded build). A few extra
 * read/write syscalls on a big staged file is a fine trade for safety. */
#define XSTORE_BUF 2048

/* resolve_two_tickets() cross-boundary signals (positive, distinct from
 * 0 = both-inramfs, -1 = neither, other <0 = real -errno). */
enum { XS_IR_TO_OV = 1, XS_OV_TO_IR = 2 };

/* Copy an inramfs source file into an overlay destination path; for
 * rename, unlink the inramfs source afterwards. `is_link` leaves the
 * source in place and fails EEXIST if the destination already exists. */
static long xstore_ir_to_ov(int is_link,
                            const struct sud_pr_inramfs_ticket *src,
                            int dst_dirfd, const char *dst_path)
{
    if (!src->leaf_idx) return -ENOENT;
    if (sud_inramfs_inode_is_dir(src->leaf_idx)) return -EXDEV;

    struct xs_stat st;
    if (sud_inramfs_op_stat_inode(src->leaf_idx, &st) < 0) return -EIO;
    int mode = (int)(st.st_mode & 07777);

    int sfd = (int)sud_inramfs_op_open_inode(src->leaf_idx, O_RDONLY);
    if (sfd < 0) return sfd;

    char up[PATH_MAX];
    int rc = sud_overlay_resolve_at(dst_dirfd, dst_path,
                                    SUD_OVERLAY_FOR_WRITE, up, sizeof(up));
    if (rc != SUD_OVERLAY_RESOLVED) {
        sud_inramfs_op_close(sfd);
        return (rc == SUD_OVERLAY_READONLY) ? -EROFS : -EXDEV;
    }

    int oflags = O_WRONLY | O_CREAT | O_TRUNC | (is_link ? O_EXCL : 0);
    int dfd = raw_open3(up, oflags, mode);
    if (dfd < 0) { sud_inramfs_op_close(sfd); return dfd; }

    char buf[XSTORE_BUF];
    long n, err = 0;
    while ((n = sud_inramfs_op_read(sfd, buf, sizeof(buf))) > 0) {
        long off = 0;
        while (off < n) {
            long w = raw_write(dfd, buf + off, (size_t)(n - off));
            if (w <= 0) { err = (w < 0) ? w : -EIO; break; }
            off += w;
        }
        if (err) break;
    }
    if (n < 0 && !err) err = n;
    raw_close(dfd);
    sud_inramfs_op_close(sfd);
    if (err) {
        /* Leave no half-written destination the build could mistake for
         * a complete output. */
        raw_syscall6(SYS_unlinkat, AT_FDCWD, (long)up, 0, 0, 0, 0);
        return err;
    }
    if (!is_link)
        return sud_inramfs_op_unlink_at_inode(src->parent_idx,
                                              src->basename,
                                              src->basename_len);
    return 0;
}

/* Copy an overlay source file into an inramfs destination; for rename,
 * whiteout the overlay source afterwards. */
static long xstore_ov_to_ir(int is_link,
                            int src_dirfd, const char *src_path,
                            const struct sud_pr_inramfs_ticket *dst)
{
    if (is_link && dst->leaf_idx) return -EEXIST;

    /* One path buffer, reused: the read path first, then (after the copy)
     * the merged source path for removal. Keeps the handler frame small. */
    char pbuf[PATH_MAX];
    int rc = sud_overlay_resolve_at(src_dirfd, src_path,
                                    SUD_OVERLAY_FOR_READ, pbuf, sizeof(pbuf));
    if (rc == SUD_OVERLAY_PASSTHROUGH) {
        if (sud_pr_absolutise(src_dirfd, src_path, pbuf, sizeof(pbuf)) != 0)
            return -EXDEV;
    } else if (rc != SUD_OVERLAY_RESOLVED) {
        return (rc == SUD_OVERLAY_WHITEOUT) ? -ENOENT : -EXDEV;
    }

    struct xs_stat st;
#ifdef SYS_newfstatat
    if (raw_syscall6(SYS_newfstatat, AT_FDCWD, (long)pbuf, (long)&st,
                     AT_SYMLINK_NOFOLLOW, 0, 0) < 0) return -ENOENT;
#else
    if (raw_syscall6(SYS_fstatat64, AT_FDCWD, (long)pbuf, (long)&st,
                     AT_SYMLINK_NOFOLLOW, 0, 0) < 0) return -ENOENT;
#endif
    if ((st.st_mode & 0170000) == 0040000) return -EXDEV;  /* directory */
    int mode = (int)(st.st_mode & 07777);

    int sfd = raw_open(pbuf, O_RDONLY);   /* pbuf free after this open */
    if (sfd < 0) return sfd;
    int dfd = (int)sud_inramfs_op_create_open_inode(
                  dst->parent_idx, dst->basename, dst->basename_len,
                  O_WRONLY | O_CREAT | O_TRUNC, mode);
    if (dfd < 0) { raw_close(sfd); return dfd; }

    char buf[XSTORE_BUF];
    long n, err = 0;
    while ((n = raw_read(sfd, buf, sizeof(buf))) > 0) {
        long off = 0;
        while (off < n) {
            long w = sud_inramfs_op_write(dfd, buf + off, (size_t)(n - off));
            if (w <= 0) { err = (w < 0) ? w : -EIO; break; }
            off += w;
        }
        if (err) break;
    }
    if (n < 0 && !err) err = n;
    sud_inramfs_op_close(dfd);
    raw_close(sfd);
    if (err) {
        sud_inramfs_op_unlink_at_inode(dst->parent_idx, dst->basename,
                                       dst->basename_len);
        return err;
    }
    if (!is_link) {
        /* Drop the overlay source: unlink its upper copy (if any) and
         * whiteout so a lower layer doesn't resurface the old name. pbuf
         * (the read path) is done; reuse it for the merged source path. */
        char sup[PATH_MAX];
        if (sud_pr_absolutise(src_dirfd, src_path, pbuf, sizeof(pbuf)) == 0) {
            if (sud_overlay_resolve(pbuf, SUD_OVERLAY_FOR_WRITE,
                                    sup, sizeof(sup)) == SUD_OVERLAY_RESOLVED)
                raw_syscall6(SYS_unlinkat, AT_FDCWD, (long)sup, 0, 0, 0, 0);
            sud_overlay_create_whiteout(pbuf);
        }
    }
    return 0;
}

/* Resolve a (src, dst) pair for rename/link.  Both must lie under
 * the inramfs mount; mixed (one in, one out) is rejected with
 * -EXDEV, matching kernel cross-FS link/rename semantics.
 * `src_save` is a caller-owned PATH_MAX buffer that holds the src
 * absolute path while ctx->scratch is reused for the dst. */
static int resolve_two_tickets(struct sud_syscall_ctx *ctx,
                               int src_dirfd, const char *src_path,
                               int dst_dirfd, const char *dst_path,
                               char *src_save, size_t src_save_sz,
                               struct sud_pr_inramfs_ticket *src_t,
                               struct sud_pr_inramfs_ticket *dst_t)
{
    /* Resolve src first using ctx->scratch, then copy abs_path into
     * src_save and re-point the ticket at the copy.  After that
     * ctx->scratch is free for the dst resolve. */
    int r = resolve_ticket(ctx, src_dirfd, src_path,
                           0 /*follow=0 for link/rename*/,
                           1 /*want_parent*/, src_t);
    if (r < 0) {
        /* Src is outside the inramfs mount (r == -1) or a resolve error.
         * If the DESTINATION is under the mount, this is a cross-boundary
         * rename/link (overlay/host -> inramfs): claim it and return
         * XS_OV_TO_IR so the caller BRIDGES it (copy + source whiteout).
         * Letting it fall through to the overlay would kernel-rename the
         * source into a path inramfs shadows — source gone, destination
         * invisible: SILENT DATA LOSS. */
        if (r == -1) {
            int dr = sud_pr_resolve_at_inramfs_ticket(
                dst_dirfd, dst_path, 0 /*follow*/, 1 /*want_parent*/,
                ctx->scratch, ctx->scratch_size, dst_t);
            if (dr >= 0) return XS_OV_TO_IR;  /* dst inside, src outside */
        }
        return r;                          /* neither ours, or a real error */
    }
    size_t l = strlen(src_t->abs_path);
    if (l + 1 > src_save_sz) return -ENAMETOOLONG;
    /* Re-point basename into the src_save buffer so it survives
     * ctx->scratch being overwritten by the dst resolve. */
    size_t bn_off = (size_t)(src_t->basename - src_t->abs_path);
    memcpy(src_save, src_t->abs_path, l + 1);
    src_t->abs_path = src_save;
    src_t->basename = src_save + bn_off;

    r = sud_pr_resolve_at_inramfs_ticket(dst_dirfd, dst_path,
                                         0 /*follow=0*/, 1 /*want_parent*/,
                                         ctx->scratch, ctx->scratch_size,
                                         dst_t);
    /* Source under mount, destination not → bridge inramfs -> overlay. */
    if (r < 0) return XS_IR_TO_OV;
    return 0;
}

/* ================================================================
 * Per-syscall handlers.  One tiny function per syscall, each
 * extracting the call's own (mode/flags/buf/...) arguments from
 * ctx->args and forwarding to the matching sud_inramfs_op_*_inode
 * op with the pre-resolved inode index(es) from the ticket.
 * ================================================================ */

/* Single-path, leaf-only handler signature: leaf_idx is non-zero
 * (resolve_ticket already surfaced -ENOENT for missing leaves on
 * want_parent==0 calls).  ctx supplies the syscall args. */
typedef long (*ir_leaf_handler)(struct sud_syscall_ctx *ctx,
                                const struct sud_pr_inramfs_ticket *t);

/* Two-path handler signature: both tickets are want_parent=1; src
 * is for link's src inode + rename's src parent+basename. */
typedef long (*ir_two_path_handler)(struct sud_syscall_ctx *ctx,
                                    const struct sud_pr_inramfs_ticket *src,
                                    const struct sud_pr_inramfs_ticket *dst);

/* ---- open / openat ----
 * These are the only single-path handlers that take want_parent=1
 * (because of O_CREAT semantics).  The dispatch table below marks
 * them with .want_parent=1; everything else uses want_parent=0.
 *
 * After a successful open of a directory inode, register the
 * resulting kfd in path_remap's shared dirfd table so subsequent
 * *at(dirfd, relpath) syscalls from any addin resolve back into
 * inramfs.  inramfs's open op is a pure inode→kfd primitive (it
 * has no path knowledge); the dirfd table is owned here. */
static long h_open_creat(struct sud_syscall_ctx *c,
                         const struct sud_pr_inramfs_ticket *t)
{
    /* open(path, flags, mode) — flags @[1], mode @[2]. */
    int flags = (int)c->args[1];
    int mode  = (int)c->args[2];
    if (t->leaf_idx) {
        if ((flags & O_CREAT) && (flags & O_EXCL)) return -EEXIST;
        long fd = sud_inramfs_op_open_inode(t->leaf_idx, flags);
        if (fd >= 0 && sud_inramfs_inode_is_dir(t->leaf_idx))
            sud_pr_dirfd_register((int)fd, t->abs_path);
        return fd;
    }
    if (!(flags & O_CREAT)) return -ENOENT;
    /* Created entry is always a regular file — no dirfd reg needed. */
    return sud_inramfs_op_create_open_inode(t->parent_idx,
                                            t->basename, t->basename_len,
                                            flags, mode);
}
static long h_openat_creat(struct sud_syscall_ctx *c,
                           const struct sud_pr_inramfs_ticket *t)
{
    /* openat(dirfd, path, flags, mode) — flags @[2], mode @[3]. */
    int flags = (int)c->args[2];
    int mode  = (int)c->args[3];
    if (t->leaf_idx) {
        if ((flags & O_CREAT) && (flags & O_EXCL)) return -EEXIST;
        long fd = sud_inramfs_op_open_inode(t->leaf_idx, flags);
        if (fd >= 0 && sud_inramfs_inode_is_dir(t->leaf_idx))
            sud_pr_dirfd_register((int)fd, t->abs_path);
        return fd;
    }
    if (!(flags & O_CREAT)) return -ENOENT;
    /* Created entry is always a regular file — no dirfd reg needed. */
    return sud_inramfs_op_create_open_inode(t->parent_idx,
                                            t->basename, t->basename_len,
                                            flags, mode);
}

/* ---- stat family ---- */
static long h_stat(struct sud_syscall_ctx *c,
                   const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_stat_inode(t->leaf_idx, (void *)c->args[1]); }
static long h_fstatat(struct sud_syscall_ctx *c,
                      const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_stat_inode(t->leaf_idx, (void *)c->args[2]); }
static long h_statx(struct sud_syscall_ctx *c,
                    const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_statx_fill_inode(t->leaf_idx,
                                         (unsigned int)c->args[3],
                                         (void *)c->args[4]); }

/* ---- access ---- */
static long h_access_a1(struct sud_syscall_ctx *c,
                        const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_access_inode(t->leaf_idx, (int)c->args[1]); }
static long h_access_a2(struct sud_syscall_ctx *c,
                        const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_access_inode(t->leaf_idx, (int)c->args[2]); }

/* ---- mkdir / rmdir / unlink ----
 * These take parent + basename, so the dispatch row sets
 * want_parent=1.  Mount-root special-cases match Linux semantics. */
/* mknod under the mount. S_IFIFO (or an explicit S_IFREG / bare-mode
 * regular) only — sockets/devices have no inramfs representation and
 * get -EPERM, same as most real filesystems for unprivileged mknod. */
static long h_mknod_common(const struct sud_pr_inramfs_ticket *t,
                           int mode)
{
    if (t->is_mount_root || t->leaf_idx) return -EEXIST;
    int fmt = mode & S_IFMT;
    if (fmt == S_IFIFO)
        return sud_inramfs_op_mkfifo_at_inode(t->parent_idx,
                                              t->basename,
                                              t->basename_len, mode);
    if (fmt == 0 || fmt == S_IFREG) {
        long fd = sud_inramfs_op_create_open_inode(t->parent_idx,
                                                   t->basename,
                                                   t->basename_len,
                                                   O_CREAT | O_EXCL | O_WRONLY,
                                                   mode & 07777);
        if (fd < 0) return fd;
        sud_inramfs_op_close((int)fd);
        return 0;
    }
    return -EPERM;
}
static long h_mknod_a1(struct sud_syscall_ctx *c,
                       const struct sud_pr_inramfs_ticket *t)
{ return h_mknod_common(t, (int)c->args[1]); }
static long h_mknod_a2(struct sud_syscall_ctx *c,
                       const struct sud_pr_inramfs_ticket *t)
{ return h_mknod_common(t, (int)c->args[2]); }

static long h_mkdir_a1(struct sud_syscall_ctx *c,
                       const struct sud_pr_inramfs_ticket *t)
{
    if (t->is_mount_root) return -EEXIST;
    if (t->leaf_idx) return -EEXIST;
    return sud_inramfs_op_mkdir_at_inode(t->parent_idx,
                                         t->basename, t->basename_len,
                                         (int)c->args[1]);
}
static long h_mkdir_a2(struct sud_syscall_ctx *c,
                       const struct sud_pr_inramfs_ticket *t)
{
    if (t->is_mount_root) return -EEXIST;
    if (t->leaf_idx) return -EEXIST;
    return sud_inramfs_op_mkdir_at_inode(t->parent_idx,
                                         t->basename, t->basename_len,
                                         (int)c->args[2]);
}
static long h_rmdir(struct sud_syscall_ctx *c,
                    const struct sud_pr_inramfs_ticket *t)
{
    (void)c;
    if (t->is_mount_root) return -EBUSY;
    return sud_inramfs_op_rmdir_at_inode(t->parent_idx,
                                         t->basename, t->basename_len);
}
static long h_unlink(struct sud_syscall_ctx *c,
                     const struct sud_pr_inramfs_ticket *t)
{
    (void)c;
    if (t->is_mount_root) return -EISDIR;
    return sud_inramfs_op_unlink_at_inode(t->parent_idx,
                                          t->basename, t->basename_len);
}
static long h_unlinkat(struct sud_syscall_ctx *c,
                       const struct sud_pr_inramfs_ticket *t)
{
    if ((int)c->args[2] & AT_REMOVEDIR) {
        if (t->is_mount_root) return -EBUSY;
        return sud_inramfs_op_rmdir_at_inode(t->parent_idx,
                                             t->basename, t->basename_len);
    }
    if (t->is_mount_root) return -EISDIR;
    return sud_inramfs_op_unlink_at_inode(t->parent_idx,
                                          t->basename, t->basename_len);
}

/* ---- symlink (target is opaque text; the *new name* is the path
 *               that fs lookup applies to). */
static long h_symlink_a0(struct sud_syscall_ctx *c,
                         const struct sud_pr_inramfs_ticket *t)
{
    if (t->is_mount_root) return -EEXIST;
    if (t->leaf_idx) return -EEXIST;
    return sud_inramfs_op_symlink_at_inode((const char *)c->args[0],
                                           t->parent_idx,
                                           t->basename, t->basename_len);
}

/* ---- readlink ---- */
static long h_readlink_a1(struct sud_syscall_ctx *c,
                          const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_readlink_inode(t->leaf_idx,
                                       (char *)c->args[1],
                                       (size_t)c->args[2]); }
static long h_readlink_a2(struct sud_syscall_ctx *c,
                          const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_readlink_inode(t->leaf_idx,
                                       (char *)c->args[2],
                                       (size_t)c->args[3]); }

/* ---- chmod / chown ---- */
static long h_chmod_a1(struct sud_syscall_ctx *c,
                       const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_chmod_inode(t->leaf_idx, (int)c->args[1]); }
static long h_chmod_a2(struct sud_syscall_ctx *c,
                       const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_chmod_inode(t->leaf_idx, (int)c->args[2]); }
static long h_chown(struct sud_syscall_ctx *c,
                    const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_chown_inode(t->leaf_idx,
                                    (int)c->args[1], (int)c->args[2]); }
static long h_chown_at2(struct sud_syscall_ctx *c,
                        const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_chown_inode(t->leaf_idx,
                                    (int)c->args[2], (int)c->args[3]); }

/* ---- truncate ---- */
static long h_truncate(struct sud_syscall_ctx *c,
                       const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_truncate_inode(t->leaf_idx, (off_t)c->args[1]); }

/* ---- utimensat ---- */
static long h_utimensat(struct sud_syscall_ctx *c,
                        const struct sud_pr_inramfs_ticket *t)
{ return sud_inramfs_op_utimens_inode(t->leaf_idx,
                                      (const struct timespec *)c->args[2]); }

/* ---- rename / link (two-path) ---- */
static long h_rename(struct sud_syscall_ctx *c,
                     const struct sud_pr_inramfs_ticket *src,
                     const struct sud_pr_inramfs_ticket *dst)
{
    (void)c;
    return sud_inramfs_op_rename_at_inode(src->parent_idx,
                                          src->basename, src->basename_len,
                                          dst->parent_idx,
                                          dst->basename, dst->basename_len,
                                          0);
}
static long h_renameat2(struct sud_syscall_ctx *c,
                        const struct sud_pr_inramfs_ticket *src,
                        const struct sud_pr_inramfs_ticket *dst)
{
    return sud_inramfs_op_rename_at_inode(src->parent_idx,
                                          src->basename, src->basename_len,
                                          dst->parent_idx,
                                          dst->basename, dst->basename_len,
                                          (unsigned int)c->args[4]);
}
static long h_link(struct sud_syscall_ctx *c,
                   const struct sud_pr_inramfs_ticket *src,
                   const struct sud_pr_inramfs_ticket *dst)
{
    (void)c;
    if (!src->leaf_idx) return -ENOENT;
    return sud_inramfs_op_link_at_inode(src->leaf_idx,
                                        dst->parent_idx,
                                        dst->basename, dst->basename_len);
}

/* ================================================================
 * Path-bearing dispatch table.
 *
 * Encoding:
 *   .nr           — syscall number we match.  Rows are skipped at
 *                   build time (via #ifdef SYS_xxx) on architectures
 *                   that lack the underlying __NR_xxx.
 *   .dirfd_idx    — args[] index of the dirfd to combine with the
 *                   pathname.  -1 means "use AT_FDCWD".
 *   .path_idx     — args[] index of the pathname.
 *   .follow       — 1 to follow trailing symlinks, 0 to leave them.
 *   .want_parent  — 1 for handlers that need parent + basename
 *                   (mkdir/unlink/rmdir/symlink/openat-with-CREAT);
 *                   0 for read-side leaf ops.  The dispatcher
 *                   forwards both modes by always populating both
 *                   leaf and (when requested) parent fields of the
 *                   ticket, so leaf-only handlers can simply read
 *                   `t->leaf_idx`.
 *   .h            — handler function, called once the ticket is
 *                   resolved.
 *
 * Two-path syscalls (rename/link/renameat/linkat/renameat2) live in
 * a separate dispatch arm below because their resolve step is a
 * pair, not a single lookup.
 * ================================================================ */
struct ir_path_row {
    long             nr;
    signed char      dirfd_idx;
    signed char      path_idx;
    signed char      follow;
    signed char      want_parent;
    ir_leaf_handler  h;
};

#define ROW(SYSNR, DIRFDIDX, PATHIDX, FOLLOW, WANT_PARENT, H) \
    { SYSNR, (DIRFDIDX), (PATHIDX), (FOLLOW), (WANT_PARENT), (H) }

static const struct ir_path_row ir_path_dispatch[] = {
    /* open / openat — open's dirfd is implicit AT_FDCWD.
     * want_parent=1 because O_CREAT may need parent + basename. */
#ifdef SYS_open
    ROW(SYS_open,    -1, 0, 1, 1, h_open_creat),
#endif
#ifdef SYS_openat
    ROW(SYS_openat,   0, 1, 1, 1, h_openat_creat),
#endif

    /* stat family — leaf only.  Per-row follow flag is constant for
     * stat/lstat; fstatat/statx parse AT_SYMLINK_NOFOLLOW per call,
     * so they're handled separately below. */
#ifdef SYS_stat
    ROW(SYS_stat,    -1, 0, 1, 0, h_stat),
#endif
#ifdef SYS_lstat
    ROW(SYS_lstat,   -1, 0, 0, 0, h_stat),
#endif
#ifdef SYS_stat64
    ROW(SYS_stat64,  -1, 0, 1, 0, h_stat),
#endif
#ifdef SYS_lstat64
    ROW(SYS_lstat64, -1, 0, 0, 0, h_stat),
#endif

    /* access family */
#ifdef SYS_access
    ROW(SYS_access,     -1, 0, 1, 0, h_access_a1),
#endif
#ifdef SYS_faccessat
    ROW(SYS_faccessat,   0, 1, 1, 0, h_access_a2),
#endif
#ifdef SYS_faccessat2
    ROW(SYS_faccessat2,  0, 1, 1, 0, h_access_a2),
#endif

    /* directory ops — want_parent=1. */
#ifdef SYS_mkdir
    ROW(SYS_mkdir,    -1, 0, 0, 1, h_mkdir_a1),
#endif
#ifdef SYS_mknod
    ROW(SYS_mknod,    -1, 0, 0, 1, h_mknod_a1),
#endif
#ifdef SYS_mknodat
    ROW(SYS_mknodat,   0, 1, 0, 1, h_mknod_a2),
#endif
#ifdef SYS_mkdirat
    ROW(SYS_mkdirat,   0, 1, 0, 1, h_mkdir_a2),
#endif
#ifdef SYS_rmdir
    ROW(SYS_rmdir,    -1, 0, 0, 1, h_rmdir),
#endif
#ifdef SYS_unlink
    ROW(SYS_unlink,   -1, 0, 0, 1, h_unlink),
#endif
#ifdef SYS_unlinkat
    ROW(SYS_unlinkat,  0, 1, 0, 1, h_unlinkat),
#endif

    /* symlink — for symlink the *new name* is the path arg
     * (args[1] for symlink, args[2] for symlinkat); the target
     * string is opaque text. */
#ifdef SYS_symlink
    ROW(SYS_symlink,   -1, 1, 0, 1, h_symlink_a0),
#endif
#ifdef SYS_symlinkat
    ROW(SYS_symlinkat,  1, 2, 0, 1, h_symlink_a0),
#endif
#ifdef SYS_readlink
    ROW(SYS_readlink,  -1, 0, 0, 0, h_readlink_a1),
#endif
#ifdef SYS_readlinkat
    ROW(SYS_readlinkat, 0, 1, 0, 0, h_readlink_a2),
#endif

    /* chmod / chown — leaf only. */
#ifdef SYS_chmod
    ROW(SYS_chmod,    -1, 0, 1, 0, h_chmod_a1),
#endif
#ifdef SYS_fchmodat
    ROW(SYS_fchmodat,  0, 1, 1, 0, h_chmod_a2),
#endif
#ifdef SYS_chown
    ROW(SYS_chown,    -1, 0, 1, 0, h_chown),
#endif
#ifdef SYS_lchown
    ROW(SYS_lchown,   -1, 0, 0, 0, h_chown),
#endif

    /* truncate */
#ifdef SYS_truncate
    ROW(SYS_truncate,   -1, 0, 1, 0, h_truncate),
#endif
#ifdef SYS_truncate64
    ROW(SYS_truncate64, -1, 0, 1, 0, h_truncate),
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
        struct sud_pr_inramfs_ticket t;
        int r = resolve_ticket(ctx, dirfd,
                               (const char *)ctx->args[row->path_idx],
                               row->follow, row->want_parent, &t);
        if (r == -1) return 0;          /* not under mount */
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, row->h(ctx, &t));
    }
    return 0;
}

/* Two-path syscalls: rename/link family. */
static int dispatch_two_path(struct sud_syscall_ctx *ctx,
                             int src_dirfd_idx, int src_path_idx,
                             int dst_dirfd_idx, int dst_path_idx,
                             ir_two_path_handler h, int is_link, long xflags)
{
    char src_save[PATH_MAX];
    int src_dirfd = (src_dirfd_idx < 0) ? AT_FDCWD
                                        : (int)ctx->args[src_dirfd_idx];
    int dst_dirfd = (dst_dirfd_idx < 0) ? AT_FDCWD
                                        : (int)ctx->args[dst_dirfd_idx];
    const char *src_path = (const char *)ctx->args[src_path_idx];
    const char *dst_path = (const char *)ctx->args[dst_path_idx];
    struct sud_pr_inramfs_ticket src_t, dst_t;
    int r = resolve_two_tickets(ctx, src_dirfd, src_path, dst_dirfd, dst_path,
                                src_save, sizeof(src_save), &src_t, &dst_t);
    if (r == -1) return 0;
    if (r == XS_IR_TO_OV || r == XS_OV_TO_IR) {
        /* Cross-store rename/link: bridge it (copy + source drop) so it
         * behaves like the single-device FUSE backend. renameat2 flags
         * (EXCHANGE/NOREPLACE/WHITEOUT) have no atomic cross-store form —
         * surface the kernel's cross-fs -EXDEV so the caller copes. */
        if (xflags != 0) return short_circuit(ctx, -EXDEV);
        if (r == XS_IR_TO_OV)
            return short_circuit(ctx,
                xstore_ir_to_ov(is_link, &src_t, dst_dirfd, dst_path));
        return short_circuit(ctx,
            xstore_ov_to_ir(is_link, src_dirfd, src_path, &dst_t));
    }
    if (r < 0)   return short_circuit(ctx, r);
    return short_circuit(ctx, h(ctx, &src_t, &dst_t));
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
        return dispatch_two_path(ctx, -1, 0, -1, 1, h_rename, 0, 0);
#endif
#ifdef SYS_renameat
    if (nr == SYS_renameat)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_rename, 0, 0);
#endif
#ifdef SYS_renameat2
    if (nr == SYS_renameat2)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_renameat2, 0,
                                 (long)ctx->args[4]);
#endif
#ifdef SYS_link
    if (nr == SYS_link)
        return dispatch_two_path(ctx, -1, 0, -1, 1, h_link, 1, 0);
#endif
#ifdef SYS_linkat
    if (nr == SYS_linkat)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_link, 1, 0);
#endif

    /* fstatat / statx parse AT_SYMLINK_NOFOLLOW from per-call flags,
     * so we resolve with the runtime-derived `follow` rather than
     * a constant on a dispatch row. */
#ifdef SYS_newfstatat
    if (nr == SYS_newfstatat) {
        int follow = ((int)ctx->args[3] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
        struct sud_pr_inramfs_ticket t;
        int r = resolve_ticket(ctx, (int)ctx->args[0],
                               (const char *)ctx->args[1], follow, 0, &t);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, h_fstatat(ctx, &t));
    }
#endif
#ifdef SYS_fstatat64
    if (nr == SYS_fstatat64) {
        int follow = ((int)ctx->args[3] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
        struct sud_pr_inramfs_ticket t;
        int r = resolve_ticket(ctx, (int)ctx->args[0],
                               (const char *)ctx->args[1], follow, 0, &t);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, h_fstatat(ctx, &t));
    }
#endif
#ifdef SYS_statx
    if (nr == SYS_statx) {
        int follow = ((int)ctx->args[2] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
        struct sud_pr_inramfs_ticket t;
        int r = resolve_ticket(ctx, (int)ctx->args[0],
                               (const char *)ctx->args[1], follow, 0, &t);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, h_statx(ctx, &t));
    }
#endif
#ifdef SYS_fchownat
    if (nr == SYS_fchownat) {
        int follow = ((int)ctx->args[4] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
        struct sud_pr_inramfs_ticket t;
        int r = resolve_ticket(ctx, (int)ctx->args[0],
                               (const char *)ctx->args[1], follow, 0, &t);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, h_chown_at2(ctx, &t));
    }
#endif

    /* utimensat: a NULL path means "operate on dirfd as if it were
     * an open fd" (futimens semantics).  That case is handled by
     * inramfs's fd-bearing dispatch (sud_inramfs_op_futimens), so
     * we only claim the path-bearing form here.  Per-call follow
     * flag matches fstatat. */
#ifdef SYS_utimensat
    if (nr == SYS_utimensat) {
        const char *p = (const char *)ctx->args[1];
        if (!p) return 0;
        int follow = ((int)ctx->args[3] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
        struct sud_pr_inramfs_ticket t;
        int r = resolve_ticket(ctx, (int)ctx->args[0], p, follow, 0, &t);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, h_utimensat(ctx, &t));
    }
#endif

    /* Everything else (open/stat/mkdir/unlink/...) lives in the
     * single-path dispatch table. */
    return dispatch_single_path(ctx);
}
