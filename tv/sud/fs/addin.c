#include "libc-fs/libc.h"
#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"
#include "sud/fs/client.h"
#include "sud/fs/vfs.h"
#include <asm/statfs.h>

static unsigned int g_fs_umask;

static int handled(struct sud_syscall_ctx *ctx, long result)
{
    ctx->ret = result;
    return 1;
}

static uint64_t split_offset(const long *args, int index)
{
#if defined(__x86_64__)
    return (uint64_t)args[index];
#else
    return (uint64_t)(uint32_t)args[index]
         | ((uint64_t)(uint32_t)args[index + 1] << 32);
#endif
}

struct fs_iovec {
    void *base;
    size_t length;
};

static long vector_io(int fd, const struct fs_iovec *iov, int count,
                      int writing)
{
    if (count < 0 || count > 1024) return -EINVAL;
    long total = 0;
    for (int i = 0; i < count; i++) {
        long result = writing
            ? sud_vfs_write(fd, iov[i].base, iov[i].length)
            : sud_vfs_read(fd, iov[i].base, iov[i].length);
        if (result < 0) return total ? total : result;
        total += result;
        if ((size_t)result != iov[i].length) break;
    }
    return total;
}

static long duplicate_fd(int oldfd, int newfd, int flags, int exact)
{
    long result;
    if (exact) {
#ifdef SYS_dup3
        result = raw_syscall6(SYS_dup3, oldfd, newfd, flags, 0, 0, 0);
#else
        if (flags) return -EINVAL;
        result = raw_syscall6(SYS_dup2, oldfd, newfd, 0, 0, 0, 0);
#endif
    } else if (newfd >= 0) {
#if defined(SYS_fcntl)
        result = raw_syscall6(SYS_fcntl, oldfd,
                              flags ? F_DUPFD_CLOEXEC : F_DUPFD,
                              newfd, 0, 0, 0);
#else
        result = raw_syscall6(SYS_fcntl64, oldfd,
                              flags ? F_DUPFD_CLOEXEC : F_DUPFD,
                              newfd, 0, 0, 0);
#endif
    } else {
        result = raw_syscall6(SYS_dup, oldfd, 0, 0, 0, 0, 0);
    }
    if (result < 0) return result;
    int attach = sud_vfs_dup(oldfd, (int)result);
    if (attach != 0) {
        raw_close((int)result);
        return attach;
    }
    return result;
}

static int dispatch_dup_to(struct sud_syscall_ctx *ctx,
                           int oldfd, int newfd, int flags, int is_dup3)
{
    int old_remote = sud_vfs_owns_fd(oldfd);
    int new_remote = sud_vfs_owns_fd(newfd);
    if (!old_remote && !new_remote) return 0;
    if (oldfd == newfd) {
        if (is_dup3) return handled(ctx, -EINVAL);
        return handled(ctx, old_remote ? newfd : -EBADF);
    }
    if (old_remote)
        return handled(ctx, duplicate_fd(oldfd, newfd, flags, 1));

    long result;
#ifdef SYS_dup3
    result = raw_syscall6(SYS_dup3, oldfd, newfd, flags, 0, 0, 0);
#else
    if (flags) return handled(ctx, -EINVAL);
    result = raw_syscall6(SYS_dup2, oldfd, newfd, 0, 0, 0, 0);
#endif
    if (result >= 0) (void)sud_vfs_close(newfd);
    return handled(ctx, result);
}

static int dispatch_fcntl(struct sud_syscall_ctx *ctx)
{
    int fd = (int)ctx->args[0];
    if (!sud_vfs_owns_fd(fd)) return 0;
    switch (ctx->args[1]) {
    case F_DUPFD:
        return handled(ctx, duplicate_fd(fd, (int)ctx->args[2], 0, 0));
    case F_DUPFD_CLOEXEC:
        return handled(ctx, duplicate_fd(fd, (int)ctx->args[2], 1, 0));
    case F_GETFL:
        return handled(ctx, sud_vfs_getfl(fd));
    case F_SETFL:
        return handled(ctx, sud_vfs_setfl(fd, (int)ctx->args[2]));
    default:
        return 0;
    }
}

static int setattr_mode(struct sud_syscall_ctx *ctx, int dirfd,
                        const char *path, int follow, uint32_t mode)
{
    struct fuse_setattr_in request;
    memset(&request, 0, sizeof(request));
    request.valid = FATTR_MODE;
    request.mode = mode;
    return handled(ctx, sud_vfs_setattrat(dirfd, path, follow, &request));
}

static int setattr_owner(struct sud_syscall_ctx *ctx, int dirfd,
                         const char *path, int follow, long uid, long gid)
{
    struct fuse_setattr_in request;
    memset(&request, 0, sizeof(request));
    if (uid != -1) { request.valid |= FATTR_UID; request.uid = (uint32_t)uid; }
    if (gid != -1) { request.valid |= FATTR_GID; request.gid = (uint32_t)gid; }
    return handled(ctx, sud_vfs_setattrat(dirfd, path, follow, &request));
}

static int setattr_size(struct sud_syscall_ctx *ctx, int dirfd,
                        const char *path, uint64_t size)
{
    struct fuse_setattr_in request;
    memset(&request, 0, sizeof(request));
    request.valid = FATTR_SIZE;
    request.size = size;
    return handled(ctx, sud_vfs_setattrat(dirfd, path, 1, &request));
}

struct sud_utimbuf { long access; long modification; };
struct sud_timeval { long seconds; long microseconds; };

static int add_timestamp(struct fuse_setattr_in *request, int atime,
                         long seconds, long nanoseconds)
{
    if (nanoseconds == UTIME_OMIT) return 0;
    if (nanoseconds == UTIME_NOW) {
        request->valid |= atime ? FATTR_ATIME_NOW : FATTR_MTIME_NOW;
        return 0;
    }
    if (seconds < 0 || nanoseconds < 0 || nanoseconds >= 1000000000L)
        return -EINVAL;
    if (atime) {
        request->valid |= FATTR_ATIME;
        request->atime = (uint64_t)seconds;
        request->atimensec = (uint32_t)nanoseconds;
    } else {
        request->valid |= FATTR_MTIME;
        request->mtime = (uint64_t)seconds;
        request->mtimensec = (uint32_t)nanoseconds;
    }
    return 0;
}

static long set_times(int dirfd, const char *path, int follow,
                      const struct timespec *times)
{
    struct fuse_setattr_in request;
    memset(&request, 0, sizeof(request));
    if (!times) {
        request.valid = FATTR_ATIME_NOW | FATTR_MTIME_NOW;
    } else {
        int result = add_timestamp(&request, 1,
                                   times[0].tv_sec, times[0].tv_nsec);
        if (result != 0) return result;
        result = add_timestamp(&request, 0,
                               times[1].tv_sec, times[1].tv_nsec);
        if (result != 0) return result;
    }
    if (!path) return sud_vfs_fsetattr(dirfd, &request);
    return sud_vfs_setattrat(dirfd, path, follow, &request);
}

static int fs_pre_syscall(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;
#if defined(__x86_64__)
    if (nr == SYS_mmap && sud_vfs_owns_fd((int)ctx->args[4])) {
        int flags = (int)ctx->args[3];
        int writable = ((flags & MAP_SHARED) == MAP_SHARED)
                    && ((int)ctx->args[2] & PROT_WRITE);
        int backing = sud_vfs_export_fd((int)ctx->args[4], writable);
        if (backing < 0) return handled(ctx, backing);
        void *mapped = raw_mmap((void *)ctx->args[0], (size_t)ctx->args[1],
                                (int)ctx->args[2], flags, backing,
                                (off_t)ctx->args[5]);
        raw_close(backing);
        return handled(ctx, (long)mapped);
    }
#else
    if (nr == SYS_mmap2 && sud_vfs_owns_fd((int)ctx->args[4])) {
        int flags = (int)ctx->args[3];
        int writable = ((flags & MAP_SHARED) == MAP_SHARED)
                    && ((int)ctx->args[2] & PROT_WRITE);
        int backing = sud_vfs_export_fd((int)ctx->args[4], writable);
        if (backing < 0) return handled(ctx, backing);
        uint64_t page_offset = (uint64_t)(uint32_t)ctx->args[5];
        if (page_offset > (UINT64_MAX >> 12)) {
            raw_close(backing);
            return handled(ctx, -EOVERFLOW);
        }
        void *mapped = raw_mmap((void *)ctx->args[0], (size_t)ctx->args[1],
                                (int)ctx->args[2], flags, backing,
                                (off_t)(page_offset << 12));
        raw_close(backing);
        return handled(ctx, (long)mapped);
    }
#endif
#ifdef SYS_open
    if (nr == SYS_open)
        return handled(ctx, sud_vfs_openat(AT_FDCWD,
                         (const char *)ctx->args[0], (int)ctx->args[1],
                         (unsigned int)ctx->args[2], g_fs_umask));
#endif
    if (nr == SYS_openat)
        return handled(ctx, sud_vfs_openat((int)ctx->args[0],
                         (const char *)ctx->args[1], (int)ctx->args[2],
                         (unsigned int)ctx->args[3], g_fs_umask));
#ifdef SYS_creat
    if (nr == SYS_creat)
        return handled(ctx, sud_vfs_openat(AT_FDCWD,
                         (const char *)ctx->args[0],
                         O_CREAT | O_WRONLY | O_TRUNC,
                         (unsigned int)ctx->args[1], g_fs_umask));
#endif
    if (nr == SYS_read && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_read((int)ctx->args[0],
                         (void *)ctx->args[1], (size_t)ctx->args[2]));
    if (nr == SYS_write && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_write((int)ctx->args[0],
                         (const void *)ctx->args[1], (size_t)ctx->args[2]));
    if (nr == SYS_pread64 && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_pread((int)ctx->args[0],
                         (void *)ctx->args[1], (size_t)ctx->args[2],
                         split_offset(ctx->args, 3)));
    if (nr == SYS_pwrite64 && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_pwrite((int)ctx->args[0],
                         (const void *)ctx->args[1], (size_t)ctx->args[2],
                         split_offset(ctx->args, 3)));
    if (nr == SYS_readv && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, vector_io((int)ctx->args[0],
                         (const struct fs_iovec *)ctx->args[1],
                         (int)ctx->args[2], 0));
    if (nr == SYS_writev && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, vector_io((int)ctx->args[0],
                         (const struct fs_iovec *)ctx->args[1],
                         (int)ctx->args[2], 1));
    if (nr == SYS_lseek && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_lseek((int)ctx->args[0],
                         (int64_t)ctx->args[1], (int)ctx->args[2]));
#ifdef SYS__llseek
    if (nr == SYS__llseek && sud_vfs_owns_fd((int)ctx->args[0])) {
        int64_t offset = (int64_t)(((uint64_t)(uint32_t)ctx->args[1] << 32)
                                 | (uint32_t)ctx->args[2]);
        long result = sud_vfs_lseek((int)ctx->args[0], offset,
                                    (int)ctx->args[4]);
        if (result >= 0 && ctx->args[3]) {
            *(uint64_t *)ctx->args[3] = (uint64_t)result;
            result = 0;
        }
        return handled(ctx, result);
    }
#endif
    if (nr == SYS_close && sud_vfs_owns_fd((int)ctx->args[0])) {
        int result = sud_vfs_close((int)ctx->args[0]);
        int closed = raw_close((int)ctx->args[0]);
        return handled(ctx, result != 0 ? result : closed);
    }
#ifdef SYS_dup
    if (nr == SYS_dup && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, duplicate_fd((int)ctx->args[0], -1, 0, 0));
#endif
#ifdef SYS_dup2
    if (nr == SYS_dup2)
        return dispatch_dup_to(ctx, (int)ctx->args[0],
                               (int)ctx->args[1], 0, 0);
#endif
#ifdef SYS_dup3
    if (nr == SYS_dup3)
        return dispatch_dup_to(ctx, (int)ctx->args[0],
                               (int)ctx->args[1], (int)ctx->args[2], 1);
#endif
#ifdef SYS_fcntl
    if (nr == SYS_fcntl) return dispatch_fcntl(ctx);
#endif
#ifdef SYS_fcntl64
    if (nr == SYS_fcntl64) return dispatch_fcntl(ctx);
#endif
    if (nr == SYS_fstat && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fstat((int)ctx->args[0],
                         (void *)ctx->args[1]));
#ifdef SYS_fstat64
    if (nr == SYS_fstat64 && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fstat((int)ctx->args[0],
                         (void *)ctx->args[1]));
#endif
#ifdef SYS_ftruncate
    if (nr == SYS_ftruncate && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_ftruncate((int)ctx->args[0],
                         (uint64_t)ctx->args[1]));
#endif
#ifdef SYS_ftruncate64
    if (nr == SYS_ftruncate64 && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_ftruncate((int)ctx->args[0],
                         split_offset(ctx->args, 1)));
#endif
#ifdef SYS_fallocate
    if (nr == SYS_fallocate && sud_vfs_owns_fd((int)ctx->args[0])) {
#if defined(__x86_64__)
        return handled(ctx, sud_vfs_fallocate((int)ctx->args[0],
                         (unsigned int)ctx->args[1],
                         (uint64_t)ctx->args[2], (uint64_t)ctx->args[3]));
#else
        return handled(ctx, sud_vfs_fallocate((int)ctx->args[0],
                         (unsigned int)ctx->args[1],
                         split_offset(ctx->args, 2),
                         split_offset(ctx->args, 4)));
#endif
    }
#endif
#ifdef SYS_getdents64
    if (nr == SYS_getdents64 && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_getdents64((int)ctx->args[0],
                         (void *)ctx->args[1], (size_t)ctx->args[2]));
#endif
#ifdef SYS_stat
    if (nr == SYS_stat)
        return handled(ctx, sud_vfs_statat(AT_FDCWD,
                         (const char *)ctx->args[0], 1,
                         (void *)ctx->args[1]));
#endif
#ifdef SYS_lstat
    if (nr == SYS_lstat)
        return handled(ctx, sud_vfs_statat(AT_FDCWD,
                         (const char *)ctx->args[0], 0,
                         (void *)ctx->args[1]));
#endif
#ifdef SYS_stat64
    if (nr == SYS_stat64)
        return handled(ctx, sud_vfs_statat(AT_FDCWD,
                         (const char *)ctx->args[0], 1,
                         (void *)ctx->args[1]));
#endif
#ifdef SYS_lstat64
    if (nr == SYS_lstat64)
        return handled(ctx, sud_vfs_statat(AT_FDCWD,
                         (const char *)ctx->args[0], 0,
                         (void *)ctx->args[1]));
#endif
#ifdef SYS_newfstatat
    if (nr == SYS_newfstatat) {
        int flags = (int)ctx->args[3];
        const char *path = (const char *)ctx->args[1];
        if ((!path || !path[0]) && !(flags & AT_EMPTY_PATH))
            return handled(ctx, -ENOENT);
        return handled(ctx, sud_vfs_statat((int)ctx->args[0], path,
                         !(flags & AT_SYMLINK_NOFOLLOW),
                         (void *)ctx->args[2]));
    }
#endif
#ifdef SYS_fstatat64
    if (nr == SYS_fstatat64) {
        int flags = (int)ctx->args[3];
        const char *path = (const char *)ctx->args[1];
        if ((!path || !path[0]) && !(flags & AT_EMPTY_PATH))
            return handled(ctx, -ENOENT);
        return handled(ctx, sud_vfs_statat((int)ctx->args[0], path,
                         !(flags & AT_SYMLINK_NOFOLLOW),
                         (void *)ctx->args[2]));
    }
#endif
#ifdef SYS_statx
    if (nr == SYS_statx) {
        int flags = (int)ctx->args[2];
        const char *path = (const char *)ctx->args[1];
        if ((!path || !path[0]) && !(flags & AT_EMPTY_PATH))
            return handled(ctx, -ENOENT);
        return handled(ctx, sud_vfs_statx((int)ctx->args[0], path,
                         !(flags & AT_SYMLINK_NOFOLLOW),
                         (unsigned int)ctx->args[3],
                         (struct statx *)ctx->args[4]));
    }
#endif
#ifdef SYS_statfs
    if (nr == SYS_statfs)
        return handled(ctx, sud_vfs_statfsat(AT_FDCWD,
                         (const char *)ctx->args[0],
                         (void *)ctx->args[1], 0));
#endif
#ifdef SYS_fstatfs
    if (nr == SYS_fstatfs && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fstatfs((int)ctx->args[0],
                         (void *)ctx->args[1], 0));
#endif
#ifdef SYS_statfs64
    if (nr == SYS_statfs64) {
        if ((size_t)ctx->args[1] != sizeof(struct statfs64))
            return handled(ctx, -EINVAL);
        return handled(ctx, sud_vfs_statfsat(AT_FDCWD,
                         (const char *)ctx->args[0],
                         (void *)ctx->args[2], 1));
    }
#endif
#ifdef SYS_fstatfs64
    if (nr == SYS_fstatfs64 && sud_vfs_owns_fd((int)ctx->args[0])) {
        if ((size_t)ctx->args[1] != sizeof(struct statfs64))
            return handled(ctx, -EINVAL);
        return handled(ctx, sud_vfs_fstatfs((int)ctx->args[0],
                         (void *)ctx->args[2], 1));
    }
#endif
#ifdef SYS_getxattr
    if (nr == SYS_getxattr)
        return handled(ctx, sud_vfs_getxattrat(AT_FDCWD,
                         (const char *)ctx->args[0], 1,
                         (const char *)ctx->args[1], (void *)ctx->args[2],
                         (size_t)ctx->args[3]));
#endif
#ifdef SYS_lgetxattr
    if (nr == SYS_lgetxattr)
        return handled(ctx, sud_vfs_getxattrat(AT_FDCWD,
                         (const char *)ctx->args[0], 0,
                         (const char *)ctx->args[1], (void *)ctx->args[2],
                         (size_t)ctx->args[3]));
#endif
#ifdef SYS_fgetxattr
    if (nr == SYS_fgetxattr && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fgetxattr((int)ctx->args[0],
                         (const char *)ctx->args[1], (void *)ctx->args[2],
                         (size_t)ctx->args[3]));
#endif
#ifdef SYS_listxattr
    if (nr == SYS_listxattr)
        return handled(ctx, sud_vfs_listxattrat(AT_FDCWD,
                         (const char *)ctx->args[0], 1,
                         (char *)ctx->args[1], (size_t)ctx->args[2]));
#endif
#ifdef SYS_llistxattr
    if (nr == SYS_llistxattr)
        return handled(ctx, sud_vfs_listxattrat(AT_FDCWD,
                         (const char *)ctx->args[0], 0,
                         (char *)ctx->args[1], (size_t)ctx->args[2]));
#endif
#ifdef SYS_flistxattr
    if (nr == SYS_flistxattr && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_flistxattr((int)ctx->args[0],
                         (char *)ctx->args[1], (size_t)ctx->args[2]));
#endif
#ifdef SYS_setxattr
    if (nr == SYS_setxattr)
        return handled(ctx, sud_vfs_setxattrat(AT_FDCWD,
                         (const char *)ctx->args[0], 1,
                         (const char *)ctx->args[1],
                         (const void *)ctx->args[2], (size_t)ctx->args[3],
                         (unsigned int)ctx->args[4]));
#endif
#ifdef SYS_lsetxattr
    if (nr == SYS_lsetxattr)
        return handled(ctx, sud_vfs_setxattrat(AT_FDCWD,
                         (const char *)ctx->args[0], 0,
                         (const char *)ctx->args[1],
                         (const void *)ctx->args[2], (size_t)ctx->args[3],
                         (unsigned int)ctx->args[4]));
#endif
#ifdef SYS_fsetxattr
    if (nr == SYS_fsetxattr && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fsetxattr((int)ctx->args[0],
                         (const char *)ctx->args[1],
                         (const void *)ctx->args[2], (size_t)ctx->args[3],
                         (unsigned int)ctx->args[4]));
#endif
#ifdef SYS_removexattr
    if (nr == SYS_removexattr)
        return handled(ctx, sud_vfs_removexattrat(AT_FDCWD,
                         (const char *)ctx->args[0], 1,
                         (const char *)ctx->args[1]));
#endif
#ifdef SYS_lremovexattr
    if (nr == SYS_lremovexattr)
        return handled(ctx, sud_vfs_removexattrat(AT_FDCWD,
                         (const char *)ctx->args[0], 0,
                         (const char *)ctx->args[1]));
#endif
#ifdef SYS_fremovexattr
    if (nr == SYS_fremovexattr && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fremovexattr((int)ctx->args[0],
                         (const char *)ctx->args[1]));
#endif
    if (nr == SYS_readlinkat)
        return handled(ctx, sud_vfs_readlinkat((int)ctx->args[0],
                         (const char *)ctx->args[1], (char *)ctx->args[2],
                         (size_t)ctx->args[3]));
#ifdef SYS_readlink
    if (nr == SYS_readlink)
        return handled(ctx, sud_vfs_readlinkat(AT_FDCWD,
                         (const char *)ctx->args[0], (char *)ctx->args[1],
                         (size_t)ctx->args[2]));
#endif
    if (nr == SYS_chdir)
        return handled(ctx, sud_vfs_chdir((const char *)ctx->args[0]));
    if (nr == SYS_fchdir && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fchdir((int)ctx->args[0]));
#ifdef SYS_getcwd
    if (nr == SYS_getcwd)
        return handled(ctx, sud_vfs_getcwd((char *)ctx->args[0],
                                           (size_t)ctx->args[1]));
#endif
#ifdef SYS_umask
    if (nr == SYS_umask) {
        unsigned int previous = g_fs_umask;
        g_fs_umask = (unsigned int)ctx->args[0] & 0777u;
        return handled(ctx, previous);
    }
#endif
#ifdef SYS_access
    if (nr == SYS_access)
        return handled(ctx, sud_vfs_accessat(AT_FDCWD,
                         (const char *)ctx->args[0],
                         (unsigned int)ctx->args[1]));
#endif
#ifdef SYS_faccessat
    if (nr == SYS_faccessat)
        return handled(ctx, sud_vfs_accessat((int)ctx->args[0],
                         (const char *)ctx->args[1],
                         (unsigned int)ctx->args[2]));
#endif
#ifdef SYS_faccessat2
    if (nr == SYS_faccessat2)
        return handled(ctx, sud_vfs_accessat((int)ctx->args[0],
                         (const char *)ctx->args[1],
                         (unsigned int)ctx->args[2]));
#endif
#ifdef SYS_chmod
    if (nr == SYS_chmod)
        return setattr_mode(ctx, AT_FDCWD, (const char *)ctx->args[0],
                            1, (uint32_t)ctx->args[1]);
#endif
#ifdef SYS_fchmodat
    if (nr == SYS_fchmodat)
        return setattr_mode(ctx, (int)ctx->args[0],
                            (const char *)ctx->args[1], 1,
                            (uint32_t)ctx->args[2]);
#endif
#ifdef SYS_fchmod
    if (nr == SYS_fchmod && sud_vfs_owns_fd((int)ctx->args[0])) {
        struct fuse_setattr_in request;
        memset(&request, 0, sizeof(request));
        request.valid = FATTR_MODE;
        request.mode = (uint32_t)ctx->args[1];
        return handled(ctx, sud_vfs_fsetattr((int)ctx->args[0], &request));
    }
#endif
#ifdef SYS_chown
    if (nr == SYS_chown)
        return setattr_owner(ctx, AT_FDCWD, (const char *)ctx->args[0], 1,
                             ctx->args[1], ctx->args[2]);
#endif
#ifdef SYS_lchown
    if (nr == SYS_lchown)
        return setattr_owner(ctx, AT_FDCWD, (const char *)ctx->args[0], 0,
                             ctx->args[1], ctx->args[2]);
#endif
#ifdef SYS_fchownat
    if (nr == SYS_fchownat)
        return setattr_owner(ctx, (int)ctx->args[0],
                             (const char *)ctx->args[1],
                             !((int)ctx->args[4] & AT_SYMLINK_NOFOLLOW),
                             ctx->args[2], ctx->args[3]);
#endif
#ifdef SYS_fchown
    if (nr == SYS_fchown && sud_vfs_owns_fd((int)ctx->args[0])) {
        struct fuse_setattr_in request;
        memset(&request, 0, sizeof(request));
        if (ctx->args[1] != -1) {
            request.valid |= FATTR_UID;
            request.uid = (uint32_t)ctx->args[1];
        }
        if (ctx->args[2] != -1) {
            request.valid |= FATTR_GID;
            request.gid = (uint32_t)ctx->args[2];
        }
        return handled(ctx, sud_vfs_fsetattr((int)ctx->args[0], &request));
    }
#endif
#ifdef SYS_truncate
    if (nr == SYS_truncate)
        return setattr_size(ctx, AT_FDCWD, (const char *)ctx->args[0],
                            (uint64_t)ctx->args[1]);
#endif
#ifdef SYS_truncate64
    if (nr == SYS_truncate64)
        return setattr_size(ctx, AT_FDCWD, (const char *)ctx->args[0],
                            split_offset(ctx->args, 1));
#endif
#ifdef SYS_utimensat
    if (nr == SYS_utimensat) {
        int flags = (int)ctx->args[3];
        if (flags & ~AT_SYMLINK_NOFOLLOW) return handled(ctx, -EINVAL);
        const char *path = (const char *)ctx->args[1];
        if (!path && !sud_vfs_owns_fd((int)ctx->args[0])) return 0;
        return handled(ctx, set_times((int)ctx->args[0], path,
                         !(flags & AT_SYMLINK_NOFOLLOW),
                         (const struct timespec *)ctx->args[2]));
    }
#endif
#ifdef SYS_utime
    if (nr == SYS_utime) {
        const struct sud_utimbuf *input =
            (const struct sud_utimbuf *)ctx->args[1];
        struct timespec times[2];
        const struct timespec *pointer = 0;
        if (input) {
            times[0].tv_sec = input->access;
            times[0].tv_nsec = 0;
            times[1].tv_sec = input->modification;
            times[1].tv_nsec = 0;
            pointer = times;
        }
        return handled(ctx, set_times(AT_FDCWD, (const char *)ctx->args[0],
                                      1, pointer));
    }
#endif
#ifdef SYS_utimes
    if (nr == SYS_utimes) {
        const struct sud_timeval *input =
            (const struct sud_timeval *)ctx->args[1];
        struct timespec times[2];
        const struct timespec *pointer = 0;
        if (input) {
            if (input[0].microseconds < 0 || input[0].microseconds >= 1000000
                || input[1].microseconds < 0 || input[1].microseconds >= 1000000)
                return handled(ctx, -EINVAL);
            times[0].tv_sec = input[0].seconds;
            times[0].tv_nsec = input[0].microseconds * 1000;
            times[1].tv_sec = input[1].seconds;
            times[1].tv_nsec = input[1].microseconds * 1000;
            pointer = times;
        }
        return handled(ctx, set_times(AT_FDCWD, (const char *)ctx->args[0],
                                      1, pointer));
    }
#endif
#ifdef SYS_futimesat
    if (nr == SYS_futimesat) {
        const struct sud_timeval *input =
            (const struct sud_timeval *)ctx->args[2];
        struct timespec times[2];
        const struct timespec *pointer = 0;
        if (input) {
            if (input[0].microseconds < 0 || input[0].microseconds >= 1000000
                || input[1].microseconds < 0 || input[1].microseconds >= 1000000)
                return handled(ctx, -EINVAL);
            times[0].tv_sec = input[0].seconds;
            times[0].tv_nsec = input[0].microseconds * 1000;
            times[1].tv_sec = input[1].seconds;
            times[1].tv_nsec = input[1].microseconds * 1000;
            pointer = times;
        }
        const char *path = (const char *)ctx->args[1];
        if (!path && !sud_vfs_owns_fd((int)ctx->args[0])) return 0;
        return handled(ctx, set_times((int)ctx->args[0], path, 1, pointer));
    }
#endif
#ifdef SYS_fsync
    if (nr == SYS_fsync && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fsync((int)ctx->args[0], 0));
#endif
#ifdef SYS_fdatasync
    if (nr == SYS_fdatasync && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_fsync((int)ctx->args[0], 1));
#endif
#ifdef SYS_mkdir
    if (nr == SYS_mkdir)
        return handled(ctx, sud_vfs_mkdirat(AT_FDCWD,
                         (const char *)ctx->args[0],
                         (unsigned int)ctx->args[1], g_fs_umask));
#endif
#ifdef SYS_mkdirat
    if (nr == SYS_mkdirat)
        return handled(ctx, sud_vfs_mkdirat((int)ctx->args[0],
                         (const char *)ctx->args[1],
                         (unsigned int)ctx->args[2], g_fs_umask));
#endif
#ifdef SYS_mknod
    if (nr == SYS_mknod)
        return handled(ctx, sud_vfs_mknodat(AT_FDCWD,
                         (const char *)ctx->args[0],
                         (unsigned int)ctx->args[1],
                         (unsigned int)ctx->args[2], g_fs_umask));
#endif
#ifdef SYS_mknodat
    if (nr == SYS_mknodat)
        return handled(ctx, sud_vfs_mknodat((int)ctx->args[0],
                         (const char *)ctx->args[1],
                         (unsigned int)ctx->args[2],
                         (unsigned int)ctx->args[3], g_fs_umask));
#endif
#ifdef SYS_unlink
    if (nr == SYS_unlink)
        return handled(ctx, sud_vfs_unlinkat(AT_FDCWD,
                         (const char *)ctx->args[0], 0));
#endif
#ifdef SYS_rmdir
    if (nr == SYS_rmdir)
        return handled(ctx, sud_vfs_unlinkat(AT_FDCWD,
                         (const char *)ctx->args[0], 1));
#endif
#ifdef SYS_unlinkat
    if (nr == SYS_unlinkat) {
        int flags = (int)ctx->args[2];
        if (flags & ~AT_REMOVEDIR) return handled(ctx, -EINVAL);
        return handled(ctx, sud_vfs_unlinkat((int)ctx->args[0],
                         (const char *)ctx->args[1],
                         !!(flags & AT_REMOVEDIR)));
    }
#endif
#ifdef SYS_rename
    if (nr == SYS_rename)
        return handled(ctx, sud_vfs_renameat2(AT_FDCWD,
                         (const char *)ctx->args[0], AT_FDCWD,
                         (const char *)ctx->args[1], 0));
#endif
#ifdef SYS_renameat
    if (nr == SYS_renameat)
        return handled(ctx, sud_vfs_renameat2((int)ctx->args[0],
                         (const char *)ctx->args[1], (int)ctx->args[2],
                         (const char *)ctx->args[3], 0));
#endif
#ifdef SYS_renameat2
    if (nr == SYS_renameat2)
        return handled(ctx, sud_vfs_renameat2((int)ctx->args[0],
                         (const char *)ctx->args[1], (int)ctx->args[2],
                         (const char *)ctx->args[3],
                         (unsigned int)ctx->args[4]));
#endif
#ifdef SYS_symlink
    if (nr == SYS_symlink)
        return handled(ctx, sud_vfs_symlinkat((const char *)ctx->args[0],
                         AT_FDCWD, (const char *)ctx->args[1]));
#endif
#ifdef SYS_symlinkat
    if (nr == SYS_symlinkat)
        return handled(ctx, sud_vfs_symlinkat((const char *)ctx->args[0],
                         (int)ctx->args[1], (const char *)ctx->args[2]));
#endif
#ifdef SYS_link
    if (nr == SYS_link)
        return handled(ctx, sud_vfs_linkat(AT_FDCWD,
                         (const char *)ctx->args[0], AT_FDCWD,
                         (const char *)ctx->args[1], 0));
#endif
#ifdef SYS_linkat
    if (nr == SYS_linkat) {
        int flags = (int)ctx->args[4];
        if (flags & ~AT_SYMLINK_FOLLOW) return handled(ctx, -EINVAL);
        return handled(ctx, sud_vfs_linkat((int)ctx->args[0],
                         (const char *)ctx->args[1], (int)ctx->args[2],
                         (const char *)ctx->args[3],
                         !!(flags & AT_SYMLINK_FOLLOW)));
    }
#endif
#ifdef SYS_copy_file_range
    if (nr == SYS_copy_file_range
        && (sud_vfs_owns_fd((int)ctx->args[0])
            || sud_vfs_owns_fd((int)ctx->args[2])))
        return handled(ctx, -ENOSYS);
#endif
#ifdef SYS_sendfile
    if (nr == SYS_sendfile
        && (sud_vfs_owns_fd((int)ctx->args[0])
            || sud_vfs_owns_fd((int)ctx->args[1])))
        return handled(ctx, -EINVAL);
#endif
#ifdef SYS_splice
    if (nr == SYS_splice
        && (sud_vfs_owns_fd((int)ctx->args[0])
            || sud_vfs_owns_fd((int)ctx->args[2])))
        return handled(ctx, -EINVAL);
#endif
#ifdef SYS_exit_group
    if (nr == SYS_exit_group) sud_vfs_process_exit();
#endif
    return 0;
}

static void fs_wrapper_init(void)
{
    const char *cwd = 0;
    if (g_sud_runtime_config_present) cwd = g_sud_runtime_config.cwd;
    long old = raw_syscall6(SYS_umask, 0, 0, 0, 0, 0, 0);
    if (old >= 0) {
        g_fs_umask = (unsigned int)old;
        (void)raw_syscall6(SYS_umask, old, 0, 0, 0, 0, 0);
    }
    int result = sud_vfs_init(cwd);
    if (result != 0) {
        const char message[] = "sud: cannot initialize SarunFs transport\n";
        raw_write(2, message, sizeof(message) - 1);
        _exit(127);
    }
}

static void fs_fork_child(void)
{
    sud_fs_client_fork_child();
    sud_vfs_fork_child();
}

const struct sud_addin sud_fs_addin = {
    "fs",
    fs_wrapper_init,
    0,
    fs_fork_child,
    fs_pre_syscall,
    0,
};
