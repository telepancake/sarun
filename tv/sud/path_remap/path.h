/*
 * sud/path_remap/path.h — Path-layer state owned by path_remap.
 *
 * After the Part-1 re-layering (see PLAN.md) path_remap is the only
 * layer that understands pathnames.  This header is its public face
 * for the rest of sud:
 *
 *   - logical CWD shadow  (g_logical_cwd in old inramfs/addin.c)
 *   - dirfd → logical-path table  (g_fdtab[].dir_path in old code)
 *   - absolutise(dirfd, path) → absolute path
 *   - inramfs mount-prefix knowledge  (parses --remap-rule inramfs:<path>;
 *     used by inramfs's vfs.c walker for prefix stripping)
 *   - chdir/getcwd/fchdir interception that maintains the shadow
 *
 * inramfs/* no longer holds any of this state.  The inramfs addin
 * registers a directory fd here (sud_pr_dirfd_register) when it opens
 * an inramfs directory so that subsequent (dirfd, relpath) syscalls
 * can be absolutised through the shared dirfd table.
 */
#ifndef SUD_PATH_REMAP_PATH_H
#define SUD_PATH_REMAP_PATH_H

#include "libc-fs/libc.h"
#include "sud/addin.h"

/* ---------------------------------------------------------------- */
/* Logical CWD shadow                                                */
/* ---------------------------------------------------------------- */

/* Read the live runtime-config --cwd value into the shadow exactly
 * once per process.  Idempotent.  Called lazily by sud_pr_absolutise
 * before consulting the shadow. */
void sud_pr_cwd_seed_from_runtime_config(void);

/* Set or clear the logical CWD.  Pass NULL/empty to clear.  When
 * non-empty, must be an absolute path.  The new value is also
 * written back to g_sud_runtime_config.cwd so child wrappers
 * inherit it via the --cwd argv flag. */
void sud_pr_cwd_set(const char *abs_path);

/* Returns a pointer to the logical-CWD string (NUL-terminated,
 * absolute) if active, or NULL if no logical CWD is set.  The
 * pointer is valid until the next sud_pr_cwd_set call. */
const char *sud_pr_cwd_get(void);

/* Read /proc/self/cwd into `out` via raw_syscall.  Returns 0 / -errno.
 * Used as the fallback when no logical CWD is set. */
long sud_pr_read_kernel_cwd(char *out, size_t out_sz);

/* For test harnesses: clear the in-process logical-CWD shadow and
 * the seeded flag.  Does NOT mutate runtime config. */
void sud_pr_cwd_reset_for_testing(void);

/* ---------------------------------------------------------------- */
/* dirfd → logical-path table                                        */
/* ---------------------------------------------------------------- */

/* Register an absolute logical path for a real kernel fd.  Used by
 * inramfs when opening a directory (the underlying memfd has no
 * usable kernel path, so without this entry a subsequent
 * openat(dirfd, "foo") could not be resolved).  May also be used by
 * overlay synthetic dirs.  abs_path must be NUL-terminated.  No
 * effect if abs_path is NULL/empty.  An existing entry for fd is
 * replaced. */
void sud_pr_dirfd_register(int fd, const char *abs_path);

/* Look up the absolute path registered for fd.  Returns a pointer
 * into internal storage (valid until the next register/forget call
 * for the same fd) or NULL if no entry exists. */
const char *sud_pr_dirfd_lookup(int fd);

/* Forget any entry for fd (called from close/dup2-replace paths). */
void sud_pr_dirfd_forget(int fd);

/* For test harnesses: drop all registered entries. */
void sud_pr_dirfd_reset_for_testing(void);

/* ---------------------------------------------------------------- */
/* (dirfd, path) → absolute path                                     */
/* ---------------------------------------------------------------- */

/* Compose an absolute path from (dirfd, path):
 *   - path absolute → copied straight through.
 *   - dirfd == AT_FDCWD → joined with the logical CWD (or
 *     /proc/self/cwd if no logical CWD).
 *   - otherwise → joined with sud_pr_dirfd_lookup(dirfd) when present;
 *     -EXDEV if no entry (caller should fall through to the kernel).
 *
 * Returns 0 on success, -errno on failure (-EFAULT if path is NULL,
 * -ENAMETOOLONG if the result doesn't fit, -EXDEV for unknown dirfd).
 */
int sud_pr_absolutise(int dirfd, const char *path,
                      char *out, size_t out_sz);

/* ---------------------------------------------------------------- */
/* inramfs mount-prefix knowledge                                    */
/* ---------------------------------------------------------------- */

/* Parse the runtime-config remap-rule list and pick out any
 * inramfs:<abs_path> entry as the inramfs mount prefix.  Idempotent.
 * Called from sud_pr_path_init() at wrapper init time. */
void sud_pr_inramfs_init_from_runtime_config(void);

/* For test harnesses: install the inramfs mount path directly.
 * Pass NULL/empty to clear. */
void sud_pr_inramfs_mount_set(const char *abs_path);

/* Returns the absolute mount-path of the inramfs region, or NULL
 * if none configured.  Pointer is process-lifetime. */
const char *sud_pr_inramfs_mount_path(void);
size_t      sud_pr_inramfs_mount_len(void);

/* True iff abs_path (must be absolute, NUL-terminated) lies inside
 * the configured inramfs mount on a path-component boundary. */
int sud_pr_inramfs_path_under_mount(const char *abs_path);

/* Resolve (dirfd, path) into an absolute path AND test whether it
 * lies inside the inramfs mount.  Returns:
 *    0 — success and inside inramfs (out is the absolute path)
 *   -1 — outside inramfs (caller should pass through / use overlay)
 *   <0 — hard error -errno
 */
int sud_pr_resolve_at_inramfs(int dirfd, const char *path,
                              char *out, size_t out_sz);

/* ---------------------------------------------------------------- */
/* Lifecycle                                                          */
/* ---------------------------------------------------------------- */

/* Wrapper-init entry point — called once at process start by
 * path_remap's addin init hook.  Wires the runtime-config-driven
 * state (--cwd, inramfs mount prefix from --remap-rule). */
void sud_pr_path_init(void);

#endif /* SUD_PATH_REMAP_PATH_H */
