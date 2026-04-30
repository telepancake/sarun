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
