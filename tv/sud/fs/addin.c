#include "libc-fs/libc.h"
#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"
#include "sud/fs/client.h"
#include "sud/fs/vfs.h"

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

static int fs_pre_syscall(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;
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
#ifdef SYS_getdents64
    if (nr == SYS_getdents64 && sud_vfs_owns_fd((int)ctx->args[0]))
        return handled(ctx, sud_vfs_getdents64((int)ctx->args[0],
                         (void *)ctx->args[1], (size_t)ctx->args[2]));
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
