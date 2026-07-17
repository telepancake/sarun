#ifndef SUD_FS_VFS_H
#define SUD_FS_VFS_H

#include <stddef.h>
#include <stdint.h>

int sud_vfs_init(void);
int sud_vfs_openat(int dirfd, const char *path, int flags,
                   unsigned int mode, unsigned int umask);
int sud_vfs_owns_fd(int fd);
long sud_vfs_read(int fd, void *buffer, size_t size);
long sud_vfs_write(int fd, const void *buffer, size_t size);
long sud_vfs_pread(int fd, void *buffer, size_t size, uint64_t offset);
long sud_vfs_pwrite(int fd, const void *buffer, size_t size, uint64_t offset);
long sud_vfs_lseek(int fd, int64_t offset, int whence);
int sud_vfs_close(int fd);
int sud_vfs_dup(int oldfd, int newfd);
void sud_vfs_fork_child(void);
void sud_vfs_process_exit(void);

#endif /* SUD_FS_VFS_H */
