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
 * All SYS_* aliases, AT_* / S_IF* / E* constants live in
 * libc-fs/libc.h — this file deliberately does not redefine them.
 *
 * The addin runs from inside the SIGSYS handler (see sud/handler.c).
 * All filesystem operations therefore go through raw syscalls (in
 * overlay.c) and never touch glibc errno.
 */

#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/path_remap/overlay.h"
#include "sud/path_remap/path.h"
#include "sud/path_remap/fakeroot.h"
#ifdef SUD_ADDIN_INRAMFS
#include "sud/inramfs/inramfs.h"
#include "sud/inramfs/path_ops.h"
#include "sud/path_remap/inramfs_glue.h"
#endif

/* Each path-bearing syscall is dispatched under `#ifdef SYS_xxx`; the
 * alias may be absent on architectures that lack the underlying
 * __NR_xxx (e.g. SYS_open on aarch64), in which case the dispatch
 * arm silently compiles out — by design. */

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
    sud_pr_path_init();
    sud_fakeroot_init();
}

/* ---- fakeroot helpers --------------------------------------------
 *
 * fakeroot is "passthrough for path resolution but tags the ticket
 * with uid/gid override metadata that the dispatcher applies to stat/
 * chown short-circuits" (PLAN.md line 57).  In practice that means:
 *
 *   - chown / lchown / fchownat under a fakeroot prefix → record the
 *     intended uid/gid (keyed by the file's dev+ino) and short-
 *     circuit the syscall with success.  The on-disk metadata is not
 *     touched (the real user typically can't chmod files they don't
 *     own); the override is replayed on every subsequent stat.
 *   - chmod / fchmodat similarly record permission bits.
 *   - stat / lstat / fstat / newfstatat / fstatat64 in post_syscall
 *     re-read dev/ino from the kernel-filled buffer and patch
 *     uid/gid/mode in place if an override exists.
 *   - getuid / geteuid / getgid / getegid / getresuid / getresgid
 *     return 0 unconditionally while fakeroot is active, so that
 *     ownership-gating code paths (`if (geteuid() == 0) chown(...)`)
 *     take the root-only branch.
 *   - setuid / seteuid / setgid / setegid / setresuid / setresgid
 *     return 0 (success) without invoking the kernel; real-fakeroot
 *     similarly no-ops these so that suid-changing helpers don't
 *     abort when running as a non-root user.
 *
 * fchown/fchmod (fd-based, no path) are not intercepted in this first
 * cut: there is no path to test against the prefix.  Documented
 * limitation; can be lifted by piping the call through
 * sud_pr_dirfd_lookup() once the dirfd table grows to cover plain
 * file fds (today it's directory-only).
 */

/* Stat the path identified by (dirfd, path) and write dev/ino into
 * the out-parameters.  Honours AT_SYMLINK_NOFOLLOW (we always pass it
 * — chown/chmod operate on the link itself when LINK is set).  All
 * filesystem ops go through raw_syscall6 because we run inside the
 * SIGSYS handler.  Returns 0 / -errno.
 *
 * The returned dev/ino are the kernel's identification of the file —
 * the same values it will later write into a stat buffer that
 * fakeroot_patch_kernel_stat consumes — so overrides keyed here will
 * be found there. */
static long fr_stat_devino(int dirfd, const char *path, int follow,
                           unsigned long long *dev_out,
                           unsigned long long *ino_out)
{
#if defined(__x86_64__)
    struct {
        unsigned long long dev;
        unsigned long long ino;
        unsigned long long _pad[20];
    } st = {0, 0, {0}};
#  ifdef SYS_newfstatat
    long rc = raw_syscall6(SYS_newfstatat, dirfd, (long)path, (long)&st,
                           follow ? 0 : AT_SYMLINK_NOFOLLOW, 0, 0);
#  else
    long rc = -ENOSYS;
#  endif
    if (rc < 0) return rc;
    *dev_out = st.dev;
    *ino_out = st.ino;
    return 0;
#else
    /* i386: use fstatat64 with the stat64 layout — st_dev is at off 0
     * and the 64-bit st_ino sits at offset 64 (the trailing field
     * after the legacy 32-bit __st_ino padding). */
    unsigned long long buf[24];
    for (int i = 0; i < 24; i++) buf[i] = 0;
#  ifdef SYS_fstatat64
    long rc = raw_syscall6(SYS_fstatat64, dirfd, (long)path, (long)buf,
                           follow ? 0 : AT_SYMLINK_NOFOLLOW, 0, 0);
#  else
    long rc = -ENOSYS;
#  endif
    if (rc < 0) return rc;
    *dev_out = buf[0];
    *ino_out = buf[8];        /* offset 64 / 8 = 8 (in 8-byte units) */
    return 0;
#endif
}

/* Run the fakeroot pre_syscall path for chown/chmod/getuid&c.  If we
 * decide to short-circuit, we set ctx->ret and return 1; otherwise we
 * return 0 and let the rest of the dispatcher run.
 *
 * Args are inspected from ctx->args[].  The path resolution to
 * absolute form (for the prefix check) goes through sud_pr_absolutise
 * so that AT_FDCWD / dirfd-relative chowns map to the same logical
 * path the rest of the dispatcher uses. */
static int fr_handle_chown(struct sud_syscall_ctx *ctx, int dirfd,
                           const char *path, int uid, int gid, int follow)
{
    if (!path || !path[0]) return short_circuit(ctx, -EFAULT);
    char abs[PATH_MAX];
    int rc = sud_pr_absolutise(dirfd, path, abs, sizeof(abs));
    if (rc < 0) return 0;     /* let the kernel deal with bad paths */
    if (!sud_fakeroot_match(abs)) return 0;
    unsigned long long dev = 0, ino = 0;
    long sr = fr_stat_devino(dirfd, path, follow, &dev, &ino);
    if (sr < 0) return short_circuit(ctx, sr);
    sud_fakeroot_record_chown(dev, ino, uid, gid);
    return short_circuit(ctx, 0);
}

static int fr_handle_chmod(struct sud_syscall_ctx *ctx, int dirfd,
                           const char *path, unsigned int mode, int follow)
{
    if (!path || !path[0]) return short_circuit(ctx, -EFAULT);
    char abs[PATH_MAX];
    int rc = sud_pr_absolutise(dirfd, path, abs, sizeof(abs));
    if (rc < 0) return 0;
    if (!sud_fakeroot_match(abs)) return 0;
    unsigned long long dev = 0, ino = 0;
    long sr = fr_stat_devino(dirfd, path, follow, &dev, &ino);
    if (sr < 0) return short_circuit(ctx, sr);
    sud_fakeroot_record_chmod(dev, ino, mode);
    return short_circuit(ctx, 0);
}

/* fakeroot pre_syscall dispatcher — invoked unconditionally before
 * the overlay-rule fast-path so it works in a fakeroot-only build
 * (no overlay rules registered).  Returns 1 on short-circuit. */
static int fakeroot_pre_syscall(struct sud_syscall_ctx *ctx)
{
    if (!sud_fakeroot_active()) return 0;
    long nr = ctx->nr;

    /* uid / gid getters — return 0 process-wide while fakeroot is on.
     * The single getter syscalls (getuid/geteuid/getgid/getegid) take
     * no args; getresuid/getresgid take three uid_t* out-params and
     * we have to write all three.  Writing them as 0 makes the
     * "running as root" illusion consistent. */
#ifdef __NR_getuid
    if (nr == __NR_getuid)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_geteuid
    if (nr == __NR_geteuid) return short_circuit(ctx, 0);
#endif
#ifdef __NR_getgid
    if (nr == __NR_getgid)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_getegid
    if (nr == __NR_getegid) return short_circuit(ctx, 0);
#endif
#ifdef __NR_getuid32
    if (nr == __NR_getuid32)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_geteuid32
    if (nr == __NR_geteuid32) return short_circuit(ctx, 0);
#endif
#ifdef __NR_getgid32
    if (nr == __NR_getgid32)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_getegid32
    if (nr == __NR_getegid32) return short_circuit(ctx, 0);
#endif
#ifdef __NR_getresuid
    if (nr == __NR_getresuid) {
        unsigned int *r = (unsigned int *)ctx->args[0];
        unsigned int *e = (unsigned int *)ctx->args[1];
        unsigned int *s = (unsigned int *)ctx->args[2];
        if (r) *r = 0;
        if (e) *e = 0;
        if (s) *s = 0;
        return short_circuit(ctx, 0);
    }
#endif
#ifdef __NR_getresgid
    if (nr == __NR_getresgid) {
        unsigned int *r = (unsigned int *)ctx->args[0];
        unsigned int *e = (unsigned int *)ctx->args[1];
        unsigned int *s = (unsigned int *)ctx->args[2];
        if (r) *r = 0;
        if (e) *e = 0;
        if (s) *s = 0;
        return short_circuit(ctx, 0);
    }
#endif
#ifdef __NR_getresuid32
    if (nr == __NR_getresuid32) {
        unsigned int *r = (unsigned int *)ctx->args[0];
        unsigned int *e = (unsigned int *)ctx->args[1];
        unsigned int *s = (unsigned int *)ctx->args[2];
        if (r) *r = 0;
        if (e) *e = 0;
        if (s) *s = 0;
        return short_circuit(ctx, 0);
    }
#endif
#ifdef __NR_getresgid32
    if (nr == __NR_getresgid32) {
        unsigned int *r = (unsigned int *)ctx->args[0];
        unsigned int *e = (unsigned int *)ctx->args[1];
        unsigned int *s = (unsigned int *)ctx->args[2];
        if (r) *r = 0;
        if (e) *e = 0;
        if (s) *s = 0;
        return short_circuit(ctx, 0);
    }
#endif

    /* uid / gid setters — accept any value and report success without
     * touching the kernel.  Real fakeroot does the same (a non-root
     * process can't actually setuid; pretending success keeps suid-
     * aware build steps from aborting). */
#ifdef __NR_setuid
    if (nr == __NR_setuid)   return short_circuit(ctx, 0);
#endif
#ifdef __NR_seteuid
    if (nr == __NR_seteuid)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_setgid
    if (nr == __NR_setgid)   return short_circuit(ctx, 0);
#endif
#ifdef __NR_setegid
    if (nr == __NR_setegid)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_setresuid
    if (nr == __NR_setresuid) return short_circuit(ctx, 0);
#endif
#ifdef __NR_setresgid
    if (nr == __NR_setresgid) return short_circuit(ctx, 0);
#endif
#ifdef __NR_setreuid
    if (nr == __NR_setreuid)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_setregid
    if (nr == __NR_setregid)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_setuid32
    if (nr == __NR_setuid32)   return short_circuit(ctx, 0);
#endif
#ifdef __NR_seteuid32
    if (nr == __NR_seteuid32)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_setgid32
    if (nr == __NR_setgid32)   return short_circuit(ctx, 0);
#endif
#ifdef __NR_setegid32
    if (nr == __NR_setegid32)  return short_circuit(ctx, 0);
#endif
#ifdef __NR_setresuid32
    if (nr == __NR_setresuid32) return short_circuit(ctx, 0);
#endif
#ifdef __NR_setresgid32
    if (nr == __NR_setresgid32) return short_circuit(ctx, 0);
#endif

    /* chown family.  We need (path, dirfd, uid, gid, follow_links). */
#ifdef __NR_chown
    if (nr == __NR_chown)
        return fr_handle_chown(ctx, AT_FDCWD,
                               (const char *)ctx->args[0],
                               (int)ctx->args[1], (int)ctx->args[2], 1);
#endif
#ifdef __NR_lchown
    if (nr == __NR_lchown)
        return fr_handle_chown(ctx, AT_FDCWD,
                               (const char *)ctx->args[0],
                               (int)ctx->args[1], (int)ctx->args[2], 0);
#endif
#ifdef __NR_chown32
    if (nr == __NR_chown32)
        return fr_handle_chown(ctx, AT_FDCWD,
                               (const char *)ctx->args[0],
                               (int)ctx->args[1], (int)ctx->args[2], 1);
#endif
#ifdef __NR_lchown32
    if (nr == __NR_lchown32)
        return fr_handle_chown(ctx, AT_FDCWD,
                               (const char *)ctx->args[0],
                               (int)ctx->args[1], (int)ctx->args[2], 0);
#endif
#ifdef __NR_fchownat
    if (nr == __NR_fchownat) {
        int flags  = (int)ctx->args[4];
        int follow = (flags & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
        return fr_handle_chown(ctx, (int)ctx->args[0],
                               (const char *)ctx->args[1],
                               (int)ctx->args[2], (int)ctx->args[3], follow);
    }
#endif

    /* chmod family. */
#ifdef __NR_chmod
    if (nr == __NR_chmod)
        return fr_handle_chmod(ctx, AT_FDCWD,
                               (const char *)ctx->args[0],
                               (unsigned int)ctx->args[1], 1);
#endif
#ifdef __NR_fchmodat
    if (nr == __NR_fchmodat) {
        /* fchmodat's flags arg is largely ignored by the kernel
         * (AT_SYMLINK_NOFOLLOW is unsupported and returns ENOTSUP);
         * we treat it as follow=1 to match kernel behaviour. */
        return fr_handle_chmod(ctx, (int)ctx->args[0],
                               (const char *)ctx->args[1],
                               (unsigned int)ctx->args[2], 1);
    }
#endif

    return 0;
}

/* fakeroot post_syscall — patch stat-family results in place using
 * the override table.  Runs after the kernel call (or after a
 * passthrough short-circuit by another addin) so the user buffer is
 * already populated.  No-op when the call failed (ctx->ret < 0) or
 * when there's no matching override. */
static void fakeroot_post_syscall(const struct sud_syscall_ctx *ctx)
{
    if (!sud_fakeroot_active()) return;
    if (ctx->ret < 0) return;
    long nr = ctx->nr;

#ifdef SYS_newfstatat
    if (nr == SYS_newfstatat) {
        sud_fakeroot_patch_kernel_stat((void *)ctx->args[2]);
        return;
    }
#endif
#ifdef __NR_stat
    if (nr == __NR_stat) {
        sud_fakeroot_patch_kernel_stat((void *)ctx->args[1]);
        return;
    }
#endif
#ifdef __NR_lstat
    if (nr == __NR_lstat) {
        sud_fakeroot_patch_kernel_stat((void *)ctx->args[1]);
        return;
    }
#endif
#ifdef __NR_fstat
    if (nr == __NR_fstat) {
        sud_fakeroot_patch_kernel_stat((void *)ctx->args[1]);
        return;
    }
#endif
#ifdef __NR_fstatat64
    if (nr == __NR_fstatat64) {
        sud_fakeroot_patch_kernel_stat64((void *)ctx->args[2]);
        return;
    }
#endif
#ifdef __NR_stat64
    if (nr == __NR_stat64) {
        sud_fakeroot_patch_kernel_stat64((void *)ctx->args[1]);
        return;
    }
#endif
#ifdef __NR_lstat64
    if (nr == __NR_lstat64) {
        sud_fakeroot_patch_kernel_stat64((void *)ctx->args[1]);
        return;
    }
#endif
#ifdef __NR_fstat64
    if (nr == __NR_fstat64) {
        sud_fakeroot_patch_kernel_stat64((void *)ctx->args[1]);
        return;
    }
#endif
}

/* ---- chdir / getcwd / fchdir interception -----------------------
 *
 * The kernel only knows about real filesystem paths.  When the
 * traced program chdirs into a remapped or inramfs path the kernel
 * would return ENOENT; we shadow the user-visible CWD in path.c
 * (g_logical_cwd) and park the kernel CWD at "/" so /proc/self/cwd
 * is at least resolvable.  Subsequent AT_FDCWD-relative path
 * resolution (sud_pr_absolutise) consults the shadow first.
 *
 * For inramfs paths we additionally validate that the destination is
 * a real inramfs directory via sud_inramfs_op_chdir.  The validation
 * is gated on SUD_ADDIN_INRAMFS so path_remap can be built and
 * tested standalone.
 *
 * Handlers return:
 *   1 — short-circuited; ctx->ret holds the syscall result.
 *   0 — not handled; let the kernel run the call.
 */

#ifdef SUD_ADDIN_INRAMFS
static long inramfs_chdir_validate(const char *abs)
{
    return sud_inramfs_op_chdir(abs);
}
#else
/* Stub used when the inramfs addin is compiled out (e.g. dispatcher
 * test-only builds).  Unreachable in practice: without inramfs there
 * is no `--remap-rule inramfs:` parser, so `sud_pr_inramfs_mount_path()`
 * always returns NULL and the chdir handler's pre-check short-
 * circuits before this stub is called.  Asserting via -ENOSYS would
 * be a behaviour change in dispatcher-test builds, so we return 0
 * (the path_remap rewrite path runs afterwards regardless). */
static long inramfs_chdir_validate(const char *abs) { (void)abs; return 0; }
#endif

static int handle_chdir(struct sud_syscall_ctx *ctx)
{
    const char *path = (const char *)ctx->args[0];
    if (!path) return short_circuit(ctx, -EFAULT);

    /* Step 1: classify against the inramfs mount.  inramfs paths are
     * not visible to the kernel (the addin's data lives in memfds),
     * so we must intercept and shadow them here. */
    if (sud_pr_inramfs_mount_path()) {
        char abs[PATH_MAX];
        int rc = sud_pr_absolutise(AT_FDCWD, path, abs, sizeof(abs));
        if (rc == 0 && sud_pr_inramfs_path_under_mount(abs)) {
            long r = inramfs_chdir_validate(abs);
            if (r < 0) return short_circuit(ctx, r);
            /* Park the kernel CWD at "/" so /proc/self/cwd doesn't
             * claim a path the kernel can't resolve. */
            raw_syscall6(SYS_chdir, (long)"/", 0, 0, 0, 0, 0);
            sud_pr_cwd_set(abs);
            return short_circuit(ctx, 0);
        }
        /* Hard error from absolutise (e.g. -ENAMETOOLONG): surface. */
        if (rc < 0 && rc != -EXDEV) return short_circuit(ctx, rc);
    }

    /* Step 2: the destination is on the host FS.  Drop any stale
     * shadow (the kernel CWD is about to become authoritative again),
     * then fall through with the path arg overlay-resolved when an
     * overlay rule applies. */
    sud_pr_cwd_set(0);

    if (sud_overlay_rule_count() == 0) return 0;
    int rc = remap_path_arg(ctx, 0, 0);
    return handle_overlay_result(ctx, rc);
}

static int handle_getcwd(struct sud_syscall_ctx *ctx)
{
    const char *lcwd = sud_pr_cwd_get();
    if (!lcwd) return 0;            /* fall through to kernel */

    char *buf = (char *)ctx->args[0];
    size_t size = (size_t)ctx->args[1];
    if (!buf) return short_circuit(ctx, -EFAULT);
    size_t l = strlen(lcwd);
    if (l + 1 > size) return short_circuit(ctx, -ERANGE);
    memcpy(buf, lcwd, l + 1);
    /* Linux getcwd(2) returns the buffer length including NUL on
     * success.  Glibc/musl wrap this and return the buffer pointer;
     * raw syscall semantics are what we honour here. */
    return short_circuit(ctx, (long)(l + 1));
}

static int handle_fchdir(struct sud_syscall_ctx *ctx)
{
    int fd = (int)ctx->args[0];
    /* If fd was opened against an inramfs (or overlay synthetic)
     * directory we know its absolute path via the shared dirfd
     * table.  Validate and shadow. */
    const char *base = sud_pr_dirfd_lookup(fd);
    if (base) {
        if (sud_pr_inramfs_path_under_mount(base)) {
            long r = inramfs_chdir_validate(base);
            if (r < 0) return short_circuit(ctx, r);
            raw_syscall6(SYS_chdir, (long)"/", 0, 0, 0, 0, 0);
            sud_pr_cwd_set(base);
            return short_circuit(ctx, 0);
        }
        /* Fall through with the shadow updated to the (host) base
         * path: the kernel will validate the fd and our shadow
         * matches what /proc/self/cwd will report after success. */
        sud_pr_cwd_set(base);
        return 0;
    }
    /* Unknown / host fd: a successful fchdir takes us outside any
     * shadowed directory.  Clear shadow pre-emptively (if the
     * kernel call subsequently fails the next chdir resets it,
     * which is a strictly better outcome than a stale shadow
     * silently mis-routing relative paths). */
    sud_pr_cwd_set(0);
    return 0;
}

/* ---- pre_syscall dispatcher -------------------------------------- */

static int path_remap_pre_syscall(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;

    /* chdir/getcwd/fchdir are intercepted unconditionally — they
     * keep the logical-CWD shadow consistent across remap, overlay,
     * and inramfs.  No early-out on rule_count because inramfs
     * (which keeps its own runtime-config slot) may be active even
     * when no overlay rule is configured. */
#ifdef SYS_chdir
    if (nr == SYS_chdir)  return handle_chdir(ctx);
#endif
#ifdef SYS_fchdir
    if (nr == SYS_fchdir) return handle_fchdir(ctx);
#endif
#ifdef SYS_getcwd
    if (nr == SYS_getcwd) return handle_getcwd(ctx);
#endif

    /* Route inramfs-mounted paths to the inramfs data store BEFORE
     * consulting the overlay rule table.  inramfs_glue handles
     * open/stat/mkdir/unlink/symlink/readlink/chmod/chown/utimensat/
     * truncate/rename/link via sud_inramfs_op_*; the kernel never
     * sees the path arg.  Returns 0 if the syscall isn't path-
     * bearing or the path isn't under the inramfs mount. */
#ifdef SUD_ADDIN_INRAMFS
    if (sud_pr_inramfs_route_pre_syscall(ctx)) return 1;
#endif

    /* fakeroot pre_syscall — intercept chown/chmod under a fakeroot
     * prefix and the global geteuid/setuid family.  Runs ahead of the
     * overlay-rule fast-path so a fakeroot-only configuration (no
     * overlay rules) still gets its hooks. */
    if (fakeroot_pre_syscall(ctx)) return 1;

    if (sud_overlay_rule_count() == 0) return 0;

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
    fakeroot_post_syscall,
};
