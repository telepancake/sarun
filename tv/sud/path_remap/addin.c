/*
 * sud/path_remap/addin.c — Glue between the SUD addin protocol and the
 * overlay core in overlay.c.
 *
 * Responsibilities:
 *   - Run sud_overlay_init() at wrapper startup.
 *   - For every path-bearing syscall, ask overlay.c to resolve the
 *     path and rewrite the syscall arg in place (or short-circuit the
 *     syscall entirely with a -errno when whiteouts/read-only rules
 *     forbid it).
 *   - Special-case openat with O_DIRECTORY on a merged path so the
 *     traced program sees a true merged directory listing.
 *   - Special-case unlink/rmdir/unlinkat so that successful deletes of
 *     entries visible from a lower layer leave a whiteout marker in
 *     upper.
 *
 * The addin runs from inside the SIGSYS handler (see sud/handler.c).
 * All filesystem operations therefore go through raw syscalls (in
 * overlay.c) and never touch glibc errno.
 */

#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/path_remap/overlay.h"

#ifndef AT_REMOVEDIR
#define AT_REMOVEDIR 0x200
#endif

/* SYS_unlinkat is the modern delete syscall on both x86_64 and i386
 * but the libc may not alias it from __NR_unlinkat. */
#if !defined(SYS_unlinkat) && defined(__NR_unlinkat)
#define SYS_unlinkat __NR_unlinkat
#endif

/* The kernel may not have all of these symbols on every libc, so each
 * use is guarded individually. */

/* Replace ctx->args[idx] (a `const char *`) with the overlay-resolved
 * path written into ctx->scratch.  for_write selects read-vs-write
 * semantics.  Returns the SUD_OVERLAY_* result so the caller can
 * decide how to short-circuit (e.g. WHITEOUT → -ENOENT). */
static int remap_path_arg(struct sud_syscall_ctx *ctx, int idx,
                          int for_write)
{
    const char *path = (const char *)ctx->args[idx];
    if (!path || !ctx->scratch || ctx->scratch_size == 0)
        return SUD_OVERLAY_PASSTHROUGH;
    int rc = sud_overlay_resolve(path, for_write,
                                 ctx->scratch, ctx->scratch_size);
    if (rc == SUD_OVERLAY_RESOLVED)
        ctx->args[idx] = (long)ctx->scratch;
    return rc;
}

/* Same, but for *at-syscalls.  dirfd_idx and path_idx select the
 * argument indices.  The scratch buffer is split in two so we can
 * write the resolved path without trampling other inputs. */
static int remap_path_arg_at(struct sud_syscall_ctx *ctx, int dirfd_idx,
                             int path_idx, int for_write)
{
    int dirfd = (int)ctx->args[dirfd_idx];
    const char *path = (const char *)ctx->args[path_idx];
    if (!path || !ctx->scratch || ctx->scratch_size == 0)
        return SUD_OVERLAY_PASSTHROUGH;
    int rc = sud_overlay_resolve_at(dirfd, path, for_write,
                                    ctx->scratch, ctx->scratch_size);
    if (rc == SUD_OVERLAY_RESOLVED) {
        ctx->args[path_idx] = (long)ctx->scratch;
        /* Resolved path is absolute, so dirfd is no longer relevant. */
        ctx->args[dirfd_idx] = AT_FDCWD;
    }
    return rc;
}

static int short_circuit(struct sud_syscall_ctx *ctx, long err)
{
    ctx->ret = err;
    return 1;
}

/* Translate an overlay return code into either pass-through or a
 * short-circuit error.  Returns 1 if the caller should short-circuit. */
static int handle_overlay_result(struct sud_syscall_ctx *ctx, int rc)
{
    if (rc == SUD_OVERLAY_WHITEOUT) return short_circuit(ctx, -ENOENT);
    if (rc == SUD_OVERLAY_READONLY) return short_circuit(ctx, -EROFS);
    return 0;
}

/* Classify open flags as read or write. */
static int open_is_write(long flags)
{
    int acc = (int)(flags & O_ACCMODE);
    return (acc != O_RDONLY) || (flags & (O_CREAT | O_TRUNC));
}

/* If the caller is trying to delete `(dirfd, path)`, perform the
 * delete ourselves (so we know its outcome), then create a whiteout
 * marker in upper if the same name was visible from a lower layer.
 * Returns 1 with ctx->ret set so the caller short-circuits the
 * syscall.  Performing the delete inline (rather than across a
 * pre/post hook pair) keeps state purely on the SIGSYS handler's
 * stack — important because SIGSYS handlers can run concurrently on
 * different threads.
 *
 * dirfd_idx may be -1 to indicate the syscall has no dirfd argument
 * (i.e. SYS_unlink / __NR_rmdir); in that case AT_FDCWD is used. */
static int handle_delete(struct sud_syscall_ctx *ctx, int dirfd_idx,
                         int path_idx, long unlink_nr, long unlink_flags)
{
    int   dirfd = (dirfd_idx >= 0) ? (int)ctx->args[dirfd_idx] : AT_FDCWD;
    const char *path = (const char *)ctx->args[path_idx];
    if (!path) return 0;

    /* Build the merged-absolute path BEFORE resolution so we can
     * decide whether a whiteout is needed.  This duplicates a small
     * piece of resolve_at logic, but keeping it self-contained avoids
     * exposing the abs-builder in overlay.h. */
    char merged_abs[PATH_MAX];
    if (path[0] == '/') {
        size_t n = strlen(path);
        if (n + 1 > sizeof(merged_abs)) return 0;
        memcpy(merged_abs, path, n + 1);
    } else if (dirfd == AT_FDCWD) {
        long n = raw_syscall6(SYS_readlinkat, AT_FDCWD,
                              (long)"/proc/self/cwd",
                              (long)merged_abs, sizeof(merged_abs) - 1,
                              0, 0);
        if (n < 0) return 0;
        merged_abs[n] = '\0';
        size_t cl = (size_t)n;
        size_t pl = strlen(path);
        if (cl + 1 + pl + 1 > sizeof(merged_abs)) return 0;
        merged_abs[cl] = '/';
        memcpy(merged_abs + cl + 1, path, pl + 1);
    } else {
        /* Non-AT_FDCWD relative dirfd: not on a path we track.  Let
         * it pass through unchanged. */
        return 0;
    }

    /* Resolve to the upper path (write-side resolution). */
    int rc = sud_overlay_resolve(merged_abs, 1,
                                 ctx->scratch, ctx->scratch_size);
    if (rc == SUD_OVERLAY_PASSTHROUGH) return 0;
    if (rc == SUD_OVERLAY_READONLY) return short_circuit(ctx, -EROFS);
    if (rc == SUD_OVERLAY_WHITEOUT)  return short_circuit(ctx, -ENOENT);
    if (rc != SUD_OVERLAY_RESOLVED)  return 0;

    /* Issue the delete against the upper path.  We may legitimately
     * get -ENOENT if the entry exists only in lower; in that case the
     * whiteout itself is the "delete" the caller wanted. */
    long del_ret = raw_syscall6(unlink_nr, AT_FDCWD,
                                (long)ctx->scratch, unlink_flags,
                                0, 0, 0);
    long whiteout_rc = sud_overlay_create_whiteout(merged_abs);
    if (del_ret == -ENOENT && whiteout_rc == 0) {
        /* Entry was lower-only and we successfully shadowed it.
         * Surface success to the caller. */
        return short_circuit(ctx, 0);
    }
    return short_circuit(ctx, del_ret);
}

static void path_remap_init(void)
{
    sud_overlay_init();
}

/* ---- pre_syscall dispatcher -------------------------------------- */

static int path_remap_pre_syscall(struct sud_syscall_ctx *ctx)
{
    if (sud_overlay_rule_count() == 0) return 0;

    long nr = ctx->nr;

    /* ---- open / openat ------------------------------------------- */
#ifdef SYS_open
    if (nr == SYS_open) {
        long flags = ctx->args[1];
        if (flags & O_DIRECTORY) {
            int fd = sud_overlay_open_dir((const char *)ctx->args[0],
                                          (int)flags, (int)ctx->args[2]);
            if (fd != SUD_OVERLAY_NO_DIR) return short_circuit(ctx, fd);
        }
        int rc = remap_path_arg(ctx, 0, open_is_write(flags));
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef SYS_openat
    if (nr == SYS_openat) {
        long flags = ctx->args[2];
        if (flags & O_DIRECTORY) {
            int fd = sud_overlay_open_dir_at((int)ctx->args[0],
                                             (const char *)ctx->args[1],
                                             (int)flags, (int)ctx->args[3]);
            if (fd != SUD_OVERLAY_NO_DIR) return short_circuit(ctx, fd);
        }
        int rc = remap_path_arg_at(ctx, 0, 1, open_is_write(flags));
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_openat2
    if (nr == __NR_openat2) {
        /* openat2 takes a struct open_how at args[2]; we don't decode
         * it (would require copying out the user struct).  Best-effort:
         * resolve the path (assume read), let the kernel handle flags. */
        int rc = remap_path_arg_at(ctx, 0, 1, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif

    /* ---- stat family --------------------------------------------- */
#ifdef SYS_newfstatat
    if (nr == SYS_newfstatat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef SYS_fstatat64
    if (nr == SYS_fstatat64) {
        int rc = remap_path_arg_at(ctx, 0, 1, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_statx
    if (nr == __NR_statx) {
        int rc = remap_path_arg_at(ctx, 0, 1, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_stat
    if (nr == __NR_stat) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_lstat
    if (nr == __NR_lstat) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_stat64
    if (nr == __NR_stat64) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_lstat64
    if (nr == __NR_lstat64) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif

    /* ---- access / faccessat ------------------------------------- */
#ifdef __NR_access
    if (nr == __NR_access) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef SYS_faccessat
    if (nr == SYS_faccessat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_faccessat2
    if (nr == __NR_faccessat2) {
        int rc = remap_path_arg_at(ctx, 0, 1, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif

    /* ---- readlink ------------------------------------------------ */
#ifdef SYS_readlink
    if (nr == SYS_readlink) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef SYS_readlinkat
    if (nr == SYS_readlinkat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif

    /* ---- chdir / chroot ----------------------------------------- */
#ifdef SYS_chdir
    if (nr == SYS_chdir) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_chroot
    if (nr == __NR_chroot) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif

    /* ---- xattr ops (treated as read-side; setxattr is write) ---- */
#ifdef __NR_getxattr
    if (nr == __NR_getxattr) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_lgetxattr
    if (nr == __NR_lgetxattr) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_listxattr
    if (nr == __NR_listxattr) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_llistxattr
    if (nr == __NR_llistxattr) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_setxattr
    if (nr == __NR_setxattr) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_lsetxattr
    if (nr == __NR_lsetxattr) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_removexattr
    if (nr == __NR_removexattr) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_lremovexattr
    if (nr == __NR_lremovexattr) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif

    /* ---- mkdir / mknod / symlink / link / rename / truncate ----- */
#ifdef __NR_mkdir
    if (nr == __NR_mkdir) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_mkdirat
    if (nr == __NR_mkdirat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_mknod
    if (nr == __NR_mknod) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_mknodat
    if (nr == __NR_mknodat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_symlink
    if (nr == __NR_symlink) {
        /* arg0 is symlink target (just a string, not a fs lookup),
         * arg1 is the link path that gets created. */
        int rc = remap_path_arg(ctx, 1, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_symlinkat
    if (nr == __NR_symlinkat) {
        int rc = remap_path_arg_at(ctx, 1, 2, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_link
    if (nr == __NR_link) {
        int rc1 = remap_path_arg(ctx, 0, 0);
        if (handle_overlay_result(ctx, rc1)) return 1;
        int rc2 = remap_path_arg(ctx, 1, 1);
        return handle_overlay_result(ctx, rc2);
    }
#endif
#ifdef __NR_linkat
    if (nr == __NR_linkat) {
        int rc1 = remap_path_arg_at(ctx, 0, 1, 0);
        if (handle_overlay_result(ctx, rc1)) return 1;
        int rc2 = remap_path_arg_at(ctx, 2, 3, 1);
        return handle_overlay_result(ctx, rc2);
    }
#endif
#ifdef __NR_rename
    if (nr == __NR_rename) {
        int rc1 = remap_path_arg(ctx, 0, 1);
        if (handle_overlay_result(ctx, rc1)) return 1;
        int rc2 = remap_path_arg(ctx, 1, 1);
        return handle_overlay_result(ctx, rc2);
    }
#endif
#ifdef __NR_renameat
    if (nr == __NR_renameat) {
        int rc1 = remap_path_arg_at(ctx, 0, 1, 1);
        if (handle_overlay_result(ctx, rc1)) return 1;
        int rc2 = remap_path_arg_at(ctx, 2, 3, 1);
        return handle_overlay_result(ctx, rc2);
    }
#endif
#ifdef __NR_renameat2
    if (nr == __NR_renameat2) {
        int rc1 = remap_path_arg_at(ctx, 0, 1, 1);
        if (handle_overlay_result(ctx, rc1)) return 1;
        int rc2 = remap_path_arg_at(ctx, 2, 3, 1);
        return handle_overlay_result(ctx, rc2);
    }
#endif
#ifdef __NR_truncate
    if (nr == __NR_truncate) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_truncate64
    if (nr == __NR_truncate64) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif

    /* ---- chmod / chown / utimes (act on metadata; treated as write) */
#ifdef __NR_chmod
    if (nr == __NR_chmod) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_fchmodat
    if (nr == __NR_fchmodat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_chown
    if (nr == __NR_chown) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_chown32
    if (nr == __NR_chown32) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_lchown
    if (nr == __NR_lchown) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_lchown32
    if (nr == __NR_lchown32) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_fchownat
    if (nr == __NR_fchownat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_utime
    if (nr == __NR_utime) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_utimes
    if (nr == __NR_utimes) {
        int rc = remap_path_arg(ctx, 0, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_futimesat
    if (nr == __NR_futimesat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef __NR_utimensat
    if (nr == __NR_utimensat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 1);
        return handle_overlay_result(ctx, rc);
    }
#endif

    /* ---- delete operations: do delete + whiteout inline ---------- */
#ifdef SYS_unlink
    if (nr == SYS_unlink) {
        return handle_delete(ctx, /*dirfd_idx=*/-1, /*path_idx=*/0,
                             /*unlink_nr=*/SYS_unlinkat, /*flags=*/0);
    }
#endif
#ifdef SYS_unlinkat
    if (nr == SYS_unlinkat) {
        return handle_delete(ctx, 0, 1, SYS_unlinkat,
                             (long)ctx->args[2]);
    }
#endif
#ifdef __NR_rmdir
    if (nr == __NR_rmdir) {
        return handle_delete(ctx, /*dirfd_idx=*/-1, /*path_idx=*/0,
                             SYS_unlinkat, AT_REMOVEDIR);
    }
#endif

    /* ---- execve / execveat ------------------------------------- */
#ifdef SYS_execve
    if (nr == SYS_execve) {
        int rc = remap_path_arg(ctx, 0, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif
#ifdef SYS_execveat
    if (nr == SYS_execveat) {
        int rc = remap_path_arg_at(ctx, 0, 1, 0);
        return handle_overlay_result(ctx, rc);
    }
#endif

    return 0;
}

const struct sud_addin sud_path_remap_addin = {
    "path_remap",
    path_remap_init,
    0,
    0,
    path_remap_pre_syscall,
    0,
};
