/*
 * sud/path_remap/fakeroot.h — fakeroot-style uid/gid/mode override
 * layer (PLAN.md line 57).
 *
 * fakeroot is a path_remap rule kind whose path-resolution behaviour
 * is pure passthrough: the kernel sees the syscall arg untouched, the
 * file lives on the host filesystem, and reads/writes go through
 * normally.  What changes is metadata: the addin pretends the traced
 * process is uid 0 / gid 0, lets every chown/chmod succeed without
 * actually issuing the syscall, and patches subsequent stat results
 * so the program observes the metadata it set.
 *
 * Configuration via the wrapper CLI:
 *
 *   --remap-rule fakeroot:<abs_prefix>      (repeatable)
 *
 * Every absolute path under <abs_prefix> (matched on a path-component
 * boundary) is fakeroot-scoped.  When at least one fakeroot rule is
 * registered the geteuid/getuid/getegid/getgid family also returns 0
 * unconditionally — programs that gate behaviour on "are we root?"
 * (chown calls, mknod calls, package manager installers, dpkg, tar -p)
 * see the answer they need, regardless of which file they're looking
 * at.  Override storage is keyed by the kernel's (st_dev, st_ino)
 * pair, so it survives rename/hardlink and is unaffected by whether
 * the file was ever inside a fakeroot prefix.
 *
 * The override table is process-local; children inherit nothing on
 * fork (real fakeroot uses an out-of-band TCP socket to a faked-by
 * helper to share state — out of scope here).  This means a tar-style
 * "create foo, chown foo, fork+exec(stat foo)" sequence will see the
 * pre-chown uid in the child.  Documented limitation; sufficient for
 * single-process build steps which are the common case.
 */

#ifndef SUD_PATH_REMAP_FAKEROOT_H
#define SUD_PATH_REMAP_FAKEROOT_H

#include "libc-fs/libc.h"
#include "sud/addin.h"

/* Flags returned by sud_fakeroot_lookup(): which of the out-parameters
 * were filled in. */
#define SUD_FAKEROOT_HAS_UID  0x1u
#define SUD_FAKEROOT_HAS_GID  0x2u
#define SUD_FAKEROOT_HAS_MODE 0x4u

/* Initialise the fakeroot layer from the live runtime config.
 * Walks the remap-rule list and picks out every entry whose kind is
 * "fakeroot:<abs_prefix>".  Idempotent.  Called once at wrapper init
 * time from sud_pr_path_init(). */
void sud_fakeroot_init(void);

/* True iff at least one fakeroot prefix is registered.  When 0 the
 * dispatcher must skip every fakeroot hook — including the global
 * geteuid/getuid override — to keep a fakeroot-free build a no-op. */
int  sud_fakeroot_active(void);

/* True iff abs_path lies at or under any registered fakeroot prefix
 * on a path-component boundary.  abs_path must be absolute and
 * NUL-terminated.  Returns 0 if abs_path is NULL or relative. */
int  sud_fakeroot_match(const char *abs_path);

/* Record a uid/gid override for the file identified by (dev, ino).
 * Either uid or gid may be -1, meaning "do not change this field"
 * (the standard chown idiom); a previously-recorded value for that
 * field is preserved.  If no entry exists yet, one is allocated and
 * the unset field is left flagged-unset. */
void sud_fakeroot_record_chown(unsigned long long dev,
                                unsigned long long ino,
                                int uid, int gid);

/* Record a mode override for the file identified by (dev, ino).
 * Only the permission bits (mask 07777) are stored; the file-type
 * bits in mode are ignored — the kernel preserves them on disk and
 * sud_fakeroot_patch_kernel_stat() merges them with the recorded
 * permission bits when patching a stat result. */
void sud_fakeroot_record_chmod(unsigned long long dev,
                                unsigned long long ino,
                                unsigned int mode);

/* Look up overrides for (dev, ino).  Returns a bitmask of
 * SUD_FAKEROOT_HAS_* flags indicating which of *uid_out / *gid_out /
 * *mode_out were filled in (others are left untouched).  Returns 0
 * if no override is registered. */
unsigned sud_fakeroot_lookup(unsigned long long dev,
                              unsigned long long ino,
                              unsigned int *uid_out,
                              unsigned int *gid_out,
                              unsigned int *mode_out);

/* Patch a kernel-written stat buffer in place using overrides keyed
 * by the buffer's own (st_dev, st_ino).  `buf` points at user memory
 * that the kernel just filled via SYS_newfstatat / SYS_stat / etc.;
 * we re-read dev/ino out of it and overwrite uid/gid/mode if an
 * override is present.  No effect when no override matches.
 *
 * The two variants reflect the two kernel stat layouts used by the
 * x86_64 and i386 syscall ABIs respectively:
 *
 *   _kernel_stat   — newfstatat / __NR_stat / __NR_lstat / __NR_fstat
 *                    (struct stat / struct kstat).
 *   _kernel_stat64 — fstatat64 / stat64 / lstat64 / fstat64 (used by
 *                    32-bit userspace).
 */
void sud_fakeroot_patch_kernel_stat(void *buf);
void sud_fakeroot_patch_kernel_stat64(void *buf);

/* For test harnesses: drop all overrides and registered prefixes,
 * resetting the layer to its just-constructed state. */
void sud_fakeroot_reset_for_testing(void);

#endif /* SUD_PATH_REMAP_FAKEROOT_H */
