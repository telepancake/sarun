#ifndef SUD_FS_VFS_H
#define SUD_FS_VFS_H

#include "libc-fs/libc.h"
#include <linux/fuse.h>

int sud_vfs_init(const char *initial_cwd);
int sud_vfs_openat(int dirfd, const char *path, int flags,
                   unsigned int mode, unsigned int umask);
int sud_vfs_owns_fd(int fd);
int sud_vfs_export_fd(int fd, int writable);
int sud_vfs_absolutize(int dirfd, const char *path, char *output, size_t size);
long sud_vfs_read(int fd, void *buffer, size_t size);
long sud_vfs_write(int fd, const void *buffer, size_t size);
long sud_vfs_pread(int fd, void *buffer, size_t size, uint64_t offset);
long sud_vfs_pwrite(int fd, const void *buffer, size_t size, uint64_t offset);
long sud_vfs_lseek(int fd, int64_t offset, int whence);
int sud_vfs_ftruncate(int fd, uint64_t size);
int sud_vfs_fstat(int fd, void *stat_buffer);
int sud_vfs_getfl(int fd);
int sud_vfs_setfl(int fd, int flags);
int sud_vfs_close(int fd);
int sud_vfs_dup(int oldfd, int newfd);
int sud_vfs_chdir(const char *path);
int sud_vfs_fchdir(int fd);
long sud_vfs_getcwd(char *buffer, size_t size);
long sud_vfs_getdents64(int fd, void *buffer, size_t size);
long sud_vfs_readlinkat(int dirfd, const char *path, char *buffer, size_t size);
int sud_vfs_mkdirat(int dirfd, const char *path, unsigned int mode,
                    unsigned int umask);
int sud_vfs_mknodat(int dirfd, const char *path, unsigned int mode,
                    unsigned int device, unsigned int umask);
int sud_vfs_unlinkat(int dirfd, const char *path, int directory);
int sud_vfs_renameat2(int old_dirfd, const char *old_path,
                      int new_dirfd, const char *new_path,
                      unsigned int flags);
int sud_vfs_symlinkat(const char *target, int dirfd, const char *path);
int sud_vfs_linkat(int old_dirfd, const char *old_path,
                   int new_dirfd, const char *new_path, int follow);
int sud_vfs_statat(int dirfd, const char *path, int follow, void *stat_buffer);
int sud_vfs_statx(int dirfd, const char *path, int follow,
                  unsigned int mask, struct statx *stat_buffer);
int sud_vfs_accessat(int dirfd, const char *path, unsigned int mask);
int sud_vfs_fsync(int fd, int datasync);
int sud_vfs_setattrat(int dirfd, const char *path, int follow,
                      const struct fuse_setattr_in *request);
int sud_vfs_fsetattr(int fd, const struct fuse_setattr_in *request);
void sud_vfs_fork_child(void);
void sud_vfs_process_exit(void);

#endif /* SUD_FS_VFS_H */
