#ifndef SUD_FS_FUSE_CLIENT_H
#define SUD_FS_FUSE_CLIENT_H

#include <stddef.h>
#include <stdint.h>
#include <linux/fuse.h>

int sud_fuse_init(void);
int sud_fuse_lookup(uint64_t parent, const char *name,
                    struct fuse_entry_out *entry);
long sud_fuse_readlink(uint64_t inode, char *buffer, size_t size);
int sud_fuse_forget(uint64_t inode, uint64_t count);
int sud_fuse_getattr(uint64_t inode, uint64_t handle,
                     int has_handle, struct fuse_attr_out *attributes);
int sud_fuse_open(uint64_t inode, uint32_t flags,
                  struct fuse_open_out *opened);
int sud_fuse_opendir(uint64_t inode, uint32_t flags,
                     struct fuse_open_out *opened);
int sud_fuse_create(uint64_t parent, const char *name, uint32_t flags,
                    uint32_t mode, uint32_t umask,
                    struct fuse_entry_out *entry,
                    struct fuse_open_out *opened);
int sud_fuse_mkdir(uint64_t parent, const char *name, uint32_t mode,
                   uint32_t umask, struct fuse_entry_out *entry);
int sud_fuse_mknod(uint64_t parent, const char *name, uint32_t mode,
                   uint32_t rdev, uint32_t umask,
                   struct fuse_entry_out *entry);
int sud_fuse_symlink(uint64_t parent, const char *name, const char *target,
                     struct fuse_entry_out *entry);
int sud_fuse_unlink(uint64_t parent, const char *name, int directory);
int sud_fuse_rename(uint64_t old_parent, const char *old_name,
                    uint64_t new_parent, const char *new_name,
                    uint32_t flags);
int sud_fuse_link(uint64_t inode, uint64_t new_parent, const char *new_name,
                  struct fuse_entry_out *entry);
long sud_fuse_read(uint64_t inode, uint64_t handle, uint64_t offset,
                   uint32_t flags, void *buffer, size_t size);
long sud_fuse_write(uint64_t inode, uint64_t handle, uint64_t offset,
                    uint32_t flags, const void *buffer, size_t size);
int sud_fuse_flush(uint64_t inode, uint64_t handle, uint32_t flags);
int sud_fuse_release(uint64_t inode, uint64_t handle, uint32_t flags);
long sud_fuse_readdir(uint64_t inode, uint64_t handle, uint64_t offset,
                      void *buffer, size_t size);
int sud_fuse_releasedir(uint64_t inode, uint64_t handle, uint32_t flags);
int sud_fuse_setattr(uint64_t inode, const struct fuse_setattr_in *input,
                     struct fuse_attr_out *attributes);
int sud_fuse_access(uint64_t inode, uint32_t mask);
int sud_fuse_fsync(uint64_t inode, uint64_t handle, int directory,
                   int datasync);
int sud_fuse_statfs(uint64_t inode, struct fuse_kstatfs *statistics);
int sud_fuse_setxattr(uint64_t inode, const char *name, const void *value,
                      size_t size, uint32_t flags);
long sud_fuse_getxattr(uint64_t inode, const char *name,
                       void *value, size_t size);
long sud_fuse_listxattr(uint64_t inode, char *names, size_t size);
int sud_fuse_removexattr(uint64_t inode, const char *name);
int sud_fuse_fallocate(uint64_t inode, uint64_t handle, uint32_t mode,
                       uint64_t offset, uint64_t length);
long sud_fuse_lseek(uint64_t inode, uint64_t handle, uint64_t offset,
                    uint32_t whence);
int sud_fuse_getlk(uint64_t inode, uint64_t handle, uint64_t owner,
                   const struct fuse_file_lock *request, uint32_t flags,
                   struct fuse_file_lock *result);
int sud_fuse_setlk(uint64_t inode, uint64_t handle, uint64_t owner,
                   const struct fuse_file_lock *lock, uint32_t flags,
                   int blocking);

size_t sud_fuse_max_read(void);
size_t sud_fuse_max_write(void);

#endif /* SUD_FS_FUSE_CLIENT_H */
