/*
 * sud/elf.h — ELF inspection, shebang detection, path resolution,
 *             and exec argv building.
 *
 * Unified implementations using only raw syscalls (signal-safe).
 * The old libc-based duplicates are eliminated; these functions
 * work in both the SIGSYS handler and the parent process.
 */

#ifndef SUD_ELF_H
#define SUD_ELF_H

#include "libc-fs/libc.h"

/* Shebang/ELF inspection */
int check_shebang(const char *path, char *interp, int interp_sz,
                  char *arg, int arg_sz);
void trim_interp(char *interp);
int inspect_elf_dynamic_fd(int fd, char *interp, int interp_sz,
                           int *elf_class);
int check_elf_dynamic(const char *path, char *interp, int interp_sz,
                      int *elf_class);

/* Path resolution */
int resolve_path(const char *cmd, char *out, int out_sz);
int resolve_execveat_path(int dirfd, const char *path, long flags,
                          char *out, int out_sz);

/* Exec argv building */
char **build_exec_argv(struct sud_arena *a, int argc, char **argv);
void free_exec_argv(char **args);

/* Compute a sufficient arena size for build_exec_argv() given the
 * inputs.  Used by the SIGSYS handler to mmap a per-call arena large
 * enough that strdup never returns NULL — sud must produce a correctly
 * rewritten argv, never silently fall back. */
size_t exec_arena_size_for(const char *fn, char **argv, int argc);

/* Self exe helper */
const char *self_exe_for_class(int elf_class);

#endif /* SUD_ELF_H */
