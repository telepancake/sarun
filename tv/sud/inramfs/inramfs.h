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
 * Configuration (cmdline flags on sud32/sud64; see sud/wrapper.c
 * and sud/runtime_config.h):
 *
 *   --remap-rule inramfs:<path>:<key>
 *       Mount point (consumed by path_remap, which routes prefix
 *       matches to inramfs's inode ops); the inramfs layer never
 *       sees the path.
 *
 *   --inramfs-key <key>
 *       Backing /dev/shm filename used by inramfs (defaults to a
 *       hash of the mount path, set by sudtrace).
 *
 *   --inramfs-meta-mb <N>
 *       Metadata region size in MiB (defaults to 64).
 *
 * Dispatch order (see sud/addin.c):
 *   1. trace      — observer; sees program's original args.
 *   2. path_remap — owns all path resolution; for paths under an
 *                   inramfs prefix, drives this addin's inode-level
 *                   ops directly via sud/path_remap/inramfs_glue.c.
 *   3. inramfs    — only its fd-bearing pre_syscall hooks run here
 *                   (read/write/lseek/close/dup/fcntl/mmap/...);
 *                   path-bearing dispatch is gone (lives in glue).
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

/* Initialize the addin: parse runtime config, attach (or create) the
 * shared backing region, ensure the root inode exists.  Idempotent
 * across calls and across processes; safe to invoke from
 * wrapper_init time.  The data store attaches whenever either
 * `--inramfs-key` or a `--remap-rule inramfs:<path>` is configured;
 * neither requires the other.  After this returns,
 * sud_inramfs_active() reflects whether the data store is attached. */
void sud_inramfs_init(void);

/* True if the inramfs data store is attached in this process — i.e.
 * the shared metadata + smalldata regions are mapped and the root
 * inode is present.  Independent of any path_remap mount: a process
 * may attach the data store via `--inramfs-key` alone (e.g. an
 * in-process unit test that drives the inode/data API directly).
 * All fd-bearing syscall hijacking short-circuits when this is 0. */
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
 * negative -errno value, in the kernel-syscall convention.
 *
 * The API has two flavours:
 *
 *   - **Inode-indexed** (`*_inode` / `*_at_inode`): the primary, hot-
 *     path API.  Callers (path_remap's inramfs_glue) resolve
 *     (dirfd, path) → inode_idx via `sud_pr_resolve_at_inramfs_ticket`
 *     once per syscall and then call these forms; the inramfs layer
 *     never re-walks the path.  Mutating ops that need parent +
 *     basename take those as arguments (`*_at_inode`).
 *
 *   - **Path-based** (legacy abs_path forms): thin wrappers that walk
 *     the absolute path and dispatch to the inode form.  Retained
 *     for `sud/loader.c` and `sud/elf.c` (which only call them on
 *     the rare exec lookup path) and for the inramfs unit tests
 *     (which exercise the data store directly).  Path resolution
 *     itself lives in `sud/path_remap/path.h`.
 */

/* ---- Public ticket-resolver primitive ----
 *
 * Walk an absolute path under the mount and return the inode
 * index/indexes needed by the inode-indexed op API.  Used by
 * sud/path_remap/inramfs_glue.c (where the cross-layer ticket
 * abstraction lives) so the glue doesn't have to include
 * sud/inramfs/internal.h.
 *
 *   - `follow`: 1 to follow trailing symlinks on the leaf.
 *   - `want_parent`: 1 to also resolve parent + basename (for
 *     namespace mutators); 0 for read-side leaf ops.
 *
 * On success returns 0:
 *   - When `want_parent == 0`: *leaf_idx_out is the leaf inode
 *     (>= 1; missing leaf surfaces as -ENOENT).
 *   - When `want_parent == 1`: *parent_idx_out, *basename_out,
 *     *basename_len_out are populated.  *leaf_idx_out is the
 *     leaf inode if it exists, or 0 if it doesn't (caller is
 *     about to create it).  basename_out points into abs_path.
 *
 * On failure returns -errno (parent missing, ENOTDIR mid-walk,
 * ENOENT on want_parent==0, etc.).  Output pointers may be NULL
 * to skip; out-params are zero-initialised on entry. */
long sud_inramfs_resolve_inode(const char *abs_path, int follow,
                               int want_parent,
                               uint32_t *leaf_idx_out,
                               uint32_t *parent_idx_out,
                               const char **basename_out,
                               size_t *basename_len_out);

/* ---- Inode-indexed read-side ops (leaf inode known) ---- */
long sud_inramfs_op_open_inode(uint32_t inode_idx, int flags,
                               const char *abs_path);
long sud_inramfs_op_create_open_inode(uint32_t parent_idx,
                                      const char *name, size_t name_len,
                                      int flags, int mode,
                                      const char *abs_path);
long sud_inramfs_op_stat_inode(uint32_t inode_idx, void *st_buf);
long sud_inramfs_op_chmod_inode(uint32_t inode_idx, int mode);
long sud_inramfs_op_chown_inode(uint32_t inode_idx, int uid, int gid);
long sud_inramfs_op_truncate_inode(uint32_t inode_idx, off_t length);
long sud_inramfs_op_utimens_inode(uint32_t inode_idx,
                                  const struct timespec ts[2]);
long sud_inramfs_op_access_inode(uint32_t inode_idx, int mode);
long sud_inramfs_op_readlink_inode(uint32_t inode_idx,
                                   char *buf, size_t bufsz);
long sud_inramfs_op_get_kfd_inode(uint32_t inode_idx);
long sud_inramfs_op_statx_fill_inode(uint32_t inode_idx,
                                     unsigned int mask, void *statx_buf);

/* ---- Inode-indexed namespace mutators (parent + basename) ---- */
long sud_inramfs_op_mkdir_at_inode(uint32_t parent_idx,
                                   const char *name, size_t name_len,
                                   int mode);
long sud_inramfs_op_rmdir_at_inode(uint32_t parent_idx,
                                   const char *name, size_t name_len);
long sud_inramfs_op_unlink_at_inode(uint32_t parent_idx,
                                    const char *name, size_t name_len);
long sud_inramfs_op_symlink_at_inode(const char *target,
                                     uint32_t parent_idx,
                                     const char *name, size_t name_len);
long sud_inramfs_op_link_at_inode(uint32_t src_inode_idx,
                                  uint32_t dst_parent_idx,
                                  const char *dst_name, size_t dst_name_len);
long sud_inramfs_op_rename_at_inode(uint32_t old_par,
                                    const char *old_base, size_t old_blen,
                                    uint32_t new_par,
                                    const char *new_base, size_t new_blen,
                                    unsigned int flags);

/* ---- Legacy path-based wrappers (walk + dispatch) ----
 *
 * Path-based forms (`sud_inramfs_op_open(abs_path,...)`,
 * `..._stat`, `..._mkdir`, ...) live in
 * `sud/inramfs/path_ops.h`, kept out of this header so that
 * `inramfs.h` exposes only the data primitives per PLAN.md.
 *
 * The fd-bearing forms (`..._read`, `..._write`, `..._lseek`,
 * `..._close`, `..._fstat`, `..._fchmod`, `..._fchown`,
 * `..._futimens`, `..._ftruncate`, `..._getdents64`) are pure
 * data primitives and remain below. */

/* ---- fd-bearing data primitives (no path argument) ---- */
long sud_inramfs_op_close(int fd);
long sud_inramfs_op_read(int fd, void *buf, size_t count);
long sud_inramfs_op_write(int fd, const void *buf, size_t count);
long sud_inramfs_op_pread(int fd, void *buf, size_t count, off_t off);
long sud_inramfs_op_pwrite(int fd, const void *buf, size_t count, off_t off);
long sud_inramfs_op_lseek(int fd, off_t off, int whence);
long sud_inramfs_op_ftruncate(int fd, off_t length);
long sud_inramfs_op_fstat(int fd, void *st_buf);          /* writes a struct stat for the running ABI */
long sud_inramfs_op_fchmod(int fd, int mode);
long sud_inramfs_op_fchown(int fd, int uid, int gid);
long sud_inramfs_op_futimens(int fd, const struct timespec ts[2]);
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

#endif /* SUD_INRAMFS_INRAMFS_H */
