/*
 * sud/inramfs/inramfs.h — In-RAM filesystem add-in for sud.
 *
 * The inramfs add-in presents the traced program with an initially-
 * empty directory at a configured path prefix.  All filesystem
 * syscalls (open, stat, read, write, mkdir, unlink, rename, symlink,
 * mmap, ...) whose path lies under that prefix are served entirely
 * in-process from a large shared-memory region — no kernel VFS is
 * touched for those operations.  The shared region is mmap'd at a
 * fixed high address by every sud loader, so multiple processes
 * (parents, fork()'d children, exec()'d children that re-run the
 * wrapper) see the same files.
 *
 * Configuration (environment):
 *
 *   SUD_INRAMFS=<path>[:<size_mb>]
 *       Mount point and (optional) backing-region size in MiB.  The
 *       mount point is the absolute path under which programs see
 *       the in-RAM tree; size defaults to 256 MiB.
 *
 *   SUD_INRAMFS_KEY=<key>
 *       Optional override for the /dev/shm filename used as the
 *       backing object.  Defaults to a hash of the mount path.
 *
 * Dispatch order (see sud/addin.c):
 *   1. trace      — observer; sees program's original args.
 *   2. inramfs    — first mutator; if path is under the mount it
 *                   handles the syscall in-process and short-circuits.
 *   3. path_remap — only sees passthrough syscalls (paths NOT under
 *                   inramfs).  In-RAM fds returned by inramfs are
 *                   real kernel fds (from memfd_create) that
 *                   path_remap leaves untouched.
 *
 * Things this initial implementation deliberately omits:
 *   - extended attributes / ACLs / SELinux
 *   - O_DIRECT, O_DSYNC, fcntl record locks, advisory locks
 *   - hard-link semantics for directories (none allowed by POSIX)
 *
 * Things this implementation DOES support:
 *   - directories (arbitrarily nested)
 *   - regular files with sparse holes and ftruncate
 *   - symlinks (with normal symlink-loop detection)
 *   - hard links to regular files (link / linkat)
 *   - rename / renameat / renameat2 within the mount
 *   - chmod / chown / utimensat
 *   - mmap (PROT_READ / PROT_WRITE / MAP_SHARED) for files whose data
 *     lives in a single contiguous extent (the common case for newly-
 *     created files; multi-extent files are not produced by this
 *     implementation since each file's data is one extent that grows
 *     by reallocation).
 *   - low-contention locking: per-inode futex lock + a single
 *     superblock lock used only for namespace and allocator ops.
 */

#ifndef SUD_INRAMFS_INRAMFS_H
#define SUD_INRAMFS_INRAMFS_H

#include "libc-fs/libc.h"
#include "sud/addin.h"

/* ---------------------------------------------------------------- */

/* Initialize the addin: parse environment, attach (or create) the
 * shared backing region, ensure the root inode exists.  Idempotent
 * across calls and across processes; safe to invoke from
 * wrapper_init time.  After this returns, sud_inramfs_active()
 * reflects whether the mount is configured. */
void sud_inramfs_init(void);

/* True if a mount prefix is configured and the backing region is
 * attached.  All syscall hijacking short-circuits when this is 0. */
int  sud_inramfs_active(void);

/* For test harnesses: tear down the in-process state (process-local
 * fd table, cached cwd, etc.) and detach the shared region.  The
 * underlying /dev/shm file is NOT unlinked (other processes may
 * still be attached); tests that want a fresh region must unlink it
 * explicitly. */
void sud_inramfs_reset_for_testing(void);

/* For test harnesses: unlink the /dev/shm backing file used by the
 * currently-configured mount.  Safe to call after
 * sud_inramfs_reset_for_testing(). */
void sud_inramfs_unlink_backing_for_testing(void);

/* The addin descriptor exposed to sud/addin.c. */
extern const struct sud_addin sud_inramfs_addin;

/* ---------------------------------------------------------------- */
/* Public, addin-internal entry points (broken out so addin.c can
 * dispatch syscalls into them without depending on a single huge
 * dispatch function).  All return either a non-negative result or a
 * negative -errno value, in the kernel-syscall convention. */

/* Returns the resolved absolute path that `(dirfd, path)` denotes,
 * provided that resolution stays inside the inramfs mount.  Writes a
 * NUL-terminated path into `out` and returns 0 on success.  Returns
 * -1 if the path is NOT under the mount (caller should pass through
 * to the kernel) or -errno if it IS under the mount but the
 * resolution failed (e.g. ENAMETOOLONG, ENOENT for a missing dirfd
 * mapping).  Symlinks are NOT resolved here — only path-syntactic
 * normalisation (".", ".." and double slashes) is applied. */
int sud_inramfs_resolve_at(int dirfd, const char *path,
                           char *out, size_t out_sz);

/* True if `abs_path` (must be absolute, NUL-terminated) lies inside
 * the configured inramfs mount on a path-component boundary. */
int sud_inramfs_path_under_mount(const char *abs_path);

/* The high-level operations.  Each takes an *absolute* path that
 * lives under the mount (callers should have validated this with
 * sud_inramfs_path_under_mount).  Returns 0 / fd / nbytes on
 * success and -errno on failure, matching kernel syscall semantics. */
long sud_inramfs_op_open(const char *abs_path, int flags, int mode);
long sud_inramfs_op_close(int fd);
long sud_inramfs_op_read(int fd, void *buf, size_t count);
long sud_inramfs_op_write(int fd, const void *buf, size_t count);
long sud_inramfs_op_pread(int fd, void *buf, size_t count, off_t off);
long sud_inramfs_op_pwrite(int fd, const void *buf, size_t count, off_t off);
long sud_inramfs_op_lseek(int fd, off_t off, int whence);
long sud_inramfs_op_ftruncate(int fd, off_t length);
long sud_inramfs_op_truncate(const char *abs_path, off_t length);
long sud_inramfs_op_fstat(int fd, void *st_buf);          /* writes a struct stat for the running ABI */
long sud_inramfs_op_stat(const char *abs_path, void *st_buf, int follow);
long sud_inramfs_op_statx(const char *abs_path, int follow,
                          unsigned int mask, void *statx_buf);
long sud_inramfs_op_mkdir(const char *abs_path, int mode);
long sud_inramfs_op_rmdir(const char *abs_path);
long sud_inramfs_op_unlink(const char *abs_path);
long sud_inramfs_op_rename(const char *abs_oldpath,
                           const char *abs_newpath, unsigned int flags);
long sud_inramfs_op_symlink(const char *target, const char *abs_linkpath);
long sud_inramfs_op_readlink(const char *abs_path, char *buf, size_t bufsz);
long sud_inramfs_op_link(const char *abs_oldpath, const char *abs_newpath);
long sud_inramfs_op_chmod(const char *abs_path, int mode);
long sud_inramfs_op_fchmod(int fd, int mode);
long sud_inramfs_op_chown(const char *abs_path, int uid, int gid, int follow);
long sud_inramfs_op_fchown(int fd, int uid, int gid);
long sud_inramfs_op_utimensat(const char *abs_path,
                              const struct timespec ts[2], int follow);
long sud_inramfs_op_futimens(int fd, const struct timespec ts[2]);
long sud_inramfs_op_access(const char *abs_path, int mode);
long sud_inramfs_op_getdents64(int fd, void *buf, size_t count);
/* Returns the (addr, length) for a successful mmap, or MAP_FAILED on
 * failure with errno-value in *err.  Maps the fd's underlying shm
 * extent into the caller's address space.  Only PROT_READ/WRITE and
 * MAP_SHARED|MAP_PRIVATE are supported.  fd must refer to an
 * inramfs-owned regular file. */
void *sud_inramfs_op_mmap(void *addr, size_t length, int prot, int flags,
                          int fd, off_t offset, int *err);

/* Returns 1 if fd is owned by inramfs (i.e. was returned by a
 * previous inramfs open), 0 otherwise.  Used by addin.c to decide
 * whether fd-bearing syscalls (read/write/lseek/...) should hijack. */
int sud_inramfs_owns_fd(int fd);

/* Duplicate an inramfs-owned fd.
 *
 *   sud_inramfs_op_dup(oldfd):                  dup(2)
 *   sud_inramfs_op_dup3(oldfd, newfd, flags):   dup2(2) / dup3(2)
 *                                               (flags == O_CLOEXEC ok)
 *   sud_inramfs_op_fcntl_dupfd(oldfd, minfd, cloexec):
 *                                               fcntl F_DUPFD[_CLOEXEC]
 *
 * On success returns the new fd (which is registered in the inramfs
 * fd table, sharing the same inode/flags as oldfd).  Each duplicated
 * fd has its own seek position (a documented divergence from
 * Linux's "shared open file description" for dup2/3 — exercising
 * shared positions across dup'd fds is rare in build workloads). */
long sud_inramfs_op_dup(int oldfd);
long sud_inramfs_op_dup3(int oldfd, int newfd, int flags);
long sud_inramfs_op_fcntl_dupfd(int oldfd, int minfd, int cloexec);

/* Return the file-status flags (O_RDONLY/O_WRONLY/O_RDWR | O_APPEND)
 * for the given inramfs-owned fd.  Mirrors fcntl(F_GETFL). */
long sud_inramfs_op_fcntl_getfl(int fd);

/* Update the file-status flags on the given inramfs-owned fd.  Only
 * O_APPEND/O_NONBLOCK can be changed (per fcntl(F_SETFL) semantics);
 * the access mode is preserved.  Mirrors fcntl(F_SETFL). */
long sud_inramfs_op_fcntl_setfl(int fd, int flags);

/* Fill a `struct statx` (kernel ABI) for the given path.  `mask` is
 * the caller's STATX_* mask; we always return the basic fields
 * regardless.  `follow` controls trailing-symlink resolution. */
long sud_inramfs_op_statx_fill(const char *abs_path, int follow,
                               unsigned int mask, void *statx_buf);

#endif /* SUD_INRAMFS_INRAMFS_H */
