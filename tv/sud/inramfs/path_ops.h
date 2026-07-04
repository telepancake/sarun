/*
 * sud/inramfs/path_ops.h — Legacy path-based wrappers around the
 * inode-indexed inramfs op API.
 *
 * Per PLAN.md ("Header (inramfs.h) exposes only the data primitives"),
 * the canonical inramfs public surface is the inode-indexed +
 * fd-bearing op set declared in `sud/inramfs/inramfs.h`.  The
 * declarations in this file are *thin walkers*: each takes an
 * absolute path under the inramfs mount, walks it once via
 * `sud_ir_walk` / `sud_ir_walk_parent` (in vfs.c), and dispatches
 * to the inode form.  They exist for two narrow caller sets:
 *
 *   1. The handler-time exec lookup path (`sud/loader.c`,
 *      `sud/elf.c`) which legitimately holds an absolute path
 *      and wants a quick "is this in inramfs? if so, open it" hop
 *      without going through path_remap's full ticket machinery.
 *   2. The path_remap chdir interceptor (`sud/path_remap/addin.c`)
 *      which validates that the new logical-cwd target is an
 *      existing directory inside inramfs.
 *   3. The inramfs unit tests (`sud/inramfs/tests/test_inramfs.c`)
 *      which still drive most of the data store via these forms.
 *      Migrating the tests to the inode-indexed API is tracked
 *      separately in PLAN.md.
 *
 * Anyone NOT in those three groups should call the inode-indexed
 * ops in `inramfs.h` directly, after using the resolver primitive
 * `sud_inramfs_resolve_inode` (or the higher-level
 * `sud_pr_resolve_at_inramfs_ticket` in
 * `sud/path_remap/inramfs_glue.h`) to obtain a parent / leaf
 * inode index.
 *
 * Any path passed in MUST be absolute and lie under the configured
 * inramfs mount; out-of-mount paths surface as -EXDEV/-ENOENT
 * depending on the operation.
 */

#ifndef SUD_INRAMFS_PATH_OPS_H
#define SUD_INRAMFS_PATH_OPS_H

#include "libc-fs/libc.h"

/* ---- Path-based read-side ops ---- */
long sud_inramfs_op_open(const char *abs_path, int flags, int mode);
long sud_inramfs_op_stat(const char *abs_path, void *st_buf, int follow);
long sud_inramfs_op_access(const char *abs_path, int mode);
long sud_inramfs_op_readlink(const char *abs_path, char *buf, size_t bufsz);
long sud_inramfs_op_truncate(const char *abs_path, off_t length);

/* ---- Path-based namespace mutators ---- */
long sud_inramfs_op_mkdir(const char *abs_path, int mode);
long sud_inramfs_op_rmdir(const char *abs_path);
long sud_inramfs_op_unlink(const char *abs_path);
long sud_inramfs_op_rename(const char *abs_oldpath,
                           const char *abs_newpath, unsigned int flags);
long sud_inramfs_op_symlink(const char *target, const char *abs_linkpath);
long sud_inramfs_op_link(const char *abs_oldpath, const char *abs_newpath);
long sud_inramfs_op_chmod(const char *abs_path, int mode);
long sud_inramfs_op_chown(const char *abs_path, int uid, int gid, int follow);
long sud_inramfs_op_utimensat(const char *abs_path,
                              const struct timespec ts[2], int follow);

/* Validate that `abs_path` (under the mount) names an existing
 * directory.  Returns 0 on success, -errno otherwise.  Used by
 * the path_remap chdir interceptor to confirm the new logical
 * CWD before publishing it. */
long sud_inramfs_op_chdir(const char *abs_path);

/* Return a real *kernel* fd for `abs_path`, suitable for the
 * kernel's own pread/mmap (e.g. the ELF loader in `sud/loader.c`).
 * If the file is currently SMALL, it is promoted to LARGE first
 * (since SMALL files have no individual kernel fd — they live as
 * runs in the shared smalldata shm).  Returned fd is O_CLOEXEC and
 * owned by the caller (close with raw_close).  Returns -errno on
 * failure. */
long sud_inramfs_op_get_kfd(const char *abs_path);

/* Fill a `struct statx` (kernel ABI) for the given path.  `mask`
 * is the caller's STATX_* mask; we always return the basic fields.
 * `follow` controls trailing-symlink resolution. */
long sud_inramfs_op_statx_fill(const char *abs_path, int follow,
                               unsigned int mask, void *statx_buf);

#endif /* SUD_INRAMFS_PATH_OPS_H */
