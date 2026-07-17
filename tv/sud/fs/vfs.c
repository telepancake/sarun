#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"
#include "sud/raw.h"
#include "sud/fs/client.h"
#include "sud/fs/fuse_client.h"
#include "sud/fs/fd_lane.h"
#include "sud/fs/vfs.h"

#define SUD_OFD_MAGIC UINT32_C(0x53464f44) /* "SFOD" */
#define SUD_OFD_VERSION 1u
#define SUD_OFD_MAP_SIZE 8192u
#define SUD_FD_TABLE_SIZE 2048u

struct sud_remote_ofd {
    uint32_t magic;
    uint32_t version;
    uint32_t lock;
    uint32_t refs;
    uint64_t inode;
    uint64_t handle;
    uint64_t offset;
    uint64_t lookup_count;
    uint32_t flags;
    uint32_t open_flags;
    uint32_t mode;
    uint32_t path_len;
    char path[PATH_MAX];
};

struct fd_entry {
    int fd;
    struct sud_remote_ofd *description;
};

static struct fd_entry g_fds[SUD_FD_TABLE_SIZE];
static uint32_t g_fd_lock;
static int g_fd_initialized;
static uint32_t g_cwd_lock;
static char g_cwd[PATH_MAX];

static long truncate_fd(int fd, uint64_t size)
{
#if defined(__i386__)
    return raw_syscall6(SYS_ftruncate64, fd, (long)(uint32_t)size,
                        (long)(uint32_t)(size >> 32), 0, 0, 0);
#else
    return raw_syscall6(SYS_ftruncate, fd, (long)size, 0, 0, 0, 0);
#endif
}

static void local_lock(uint32_t *word)
{
    uint32_t me = (uint32_t)raw_gettid();
    for (;;) {
        uint32_t expected = 0;
        if (__atomic_compare_exchange_n(word, &expected, me, 0,
                                        __ATOMIC_ACQUIRE,
                                        __ATOMIC_RELAXED)) return;
        struct timespec timeout = { 1, 0 };
        raw_syscall6(SYS_futex, (long)word, FUTEX_WAIT,
                     expected, (long)&timeout, 0, 0);
        char proc[48];
        snprintf(proc, sizeof(proc), "/proc/%u", expected);
        if (expected && raw_access(proc, 0) != 0) {
            uint32_t dead = expected;
            if (__atomic_compare_exchange_n(word, &dead, me, 0,
                                            __ATOMIC_ACQUIRE,
                                            __ATOMIC_RELAXED)) return;
        }
    }
}

static void local_unlock(uint32_t *word)
{
    __atomic_store_n(word, 0u, __ATOMIC_RELEASE);
    raw_syscall6(SYS_futex, (long)word, FUTEX_WAKE, 1, 0, 0, 0);
}

static void fd_table_init(void)
{
    if (g_fd_initialized) return;
    for (unsigned int i = 0; i < SUD_FD_TABLE_SIZE; i++) g_fds[i].fd = -1;
    g_fd_initialized = 1;
}

static struct fd_entry *fd_lookup_locked(int fd)
{
    for (unsigned int i = 0; i < SUD_FD_TABLE_SIZE; i++)
        if (g_fds[i].fd == fd) return &g_fds[i];
    return 0;
}

static int fd_insert_locked(int fd, struct sud_remote_ofd *description)
{
    for (unsigned int i = 0; i < SUD_FD_TABLE_SIZE; i++) {
        if (g_fds[i].fd == -1) {
            g_fds[i].fd = fd;
            g_fds[i].description = description;
            return 0;
        }
    }
    return -EMFILE;
}

static struct sud_remote_ofd *map_description(int fd)
{
    void *mapping = raw_mmap(0, SUD_OFD_MAP_SIZE, PROT_READ | PROT_WRITE,
                             MAP_SHARED, fd, 0);
    if ((unsigned long)mapping >= (unsigned long)-4095) return 0;
    struct sud_remote_ofd *description = mapping;
    if (description->magic != SUD_OFD_MAGIC
        || description->version != SUD_OFD_VERSION) {
        raw_syscall6(SYS_munmap, (long)mapping, SUD_OFD_MAP_SIZE, 0, 0, 0, 0);
        return 0;
    }
    return description;
}

static int is_remote_memfd(int fd)
{
    char proc[64];
    char target[96];
    snprintf(proc, sizeof(proc), "/proc/self/fd/%d", fd);
    long length = raw_readlink(proc, target, sizeof(target) - 1);
    if (length <= 0 || (size_t)length >= sizeof(target)) return 0;
    target[length] = '\0';
    return strstr(target, "/memfd:sarun-sud-fs-ofd") != 0;
}

static struct sud_remote_ofd *description_for(int fd)
{
    if (fd < 0) return 0;
    fd_table_init();
    local_lock(&g_fd_lock);
    struct fd_entry *entry = fd_lookup_locked(fd);
    if (entry) {
        struct sud_remote_ofd *description = entry->description;
        local_unlock(&g_fd_lock);
        return description;
    }
    local_unlock(&g_fd_lock);
    if (!is_remote_memfd(fd)) return 0;
    struct sud_remote_ofd *description = map_description(fd);
    if (!description) return 0;
    local_lock(&g_fd_lock);
    int result = fd_insert_locked(fd, description);
    local_unlock(&g_fd_lock);
    if (result != 0) {
        raw_syscall6(SYS_munmap, (long)description, SUD_OFD_MAP_SIZE, 0, 0, 0, 0);
        return 0;
    }
    return description;
}

static void ofd_lock(struct sud_remote_ofd *description)
{
    uint32_t me = (uint32_t)raw_gettid();
    for (;;) {
        uint32_t expected = 0;
        if (__atomic_compare_exchange_n(&description->lock, &expected, me, 0,
                                        __ATOMIC_ACQUIRE,
                                        __ATOMIC_RELAXED)) return;
        struct timespec timeout = { 1, 0 };
        raw_syscall6(SYS_futex, (long)&description->lock, FUTEX_WAIT,
                     expected, (long)&timeout, 0, 0);
        char proc[48];
        snprintf(proc, sizeof(proc), "/proc/%u", expected);
        if (expected && raw_access(proc, 0) != 0) {
            uint32_t dead = expected;
            if (__atomic_compare_exchange_n(&description->lock, &dead, me, 0,
                                            __ATOMIC_ACQUIRE,
                                            __ATOMIC_RELAXED)) return;
        }
    }
}

static void ofd_unlock(struct sud_remote_ofd *description)
{
    __atomic_store_n(&description->lock, 0u, __ATOMIC_RELEASE);
    raw_syscall6(SYS_futex, (long)&description->lock, FUTEX_WAKE, 1, 0, 0, 0);
}

static int absolute_at(int dirfd, const char *path, char *output, size_t size)
{
    if (!path || !path[0]) return -ENOENT;
    if (path[0] == '/') {
        size_t length = strlen(path);
        if (length >= size) return -ENAMETOOLONG;
        memcpy(output, path, length + 1);
        return 0;
    }
    char base[PATH_MAX];
    if (dirfd == AT_FDCWD) {
        local_lock(&g_cwd_lock);
        size_t length = strlen(g_cwd);
        if (!length || length >= sizeof(base)) {
            local_unlock(&g_cwd_lock);
            return -ENOENT;
        }
        memcpy(base, g_cwd, length + 1);
        local_unlock(&g_cwd_lock);
    } else {
        struct sud_remote_ofd *description = description_for(dirfd);
        if (description && (description->mode & S_IFMT) == S_IFDIR) {
            if (description->path_len >= sizeof(base)) return -ENAMETOOLONG;
            memcpy(base, description->path, description->path_len + 1);
        } else {
            char proc[64];
            snprintf(proc, sizeof(proc), "/proc/self/fd/%d", dirfd);
            long result = raw_readlink(proc, base, sizeof(base) - 1);
            if (result < 0) return (int)result;
            if ((size_t)result >= sizeof(base) - 1) return -ENAMETOOLONG;
            base[result] = '\0';
        }
    }
    size_t base_len = strlen(base);
    size_t path_len = strlen(path);
    if (base_len + 1 + path_len >= size) return -ENAMETOOLONG;
    memcpy(output, base, base_len);
    if (base_len == 0 || output[base_len - 1] != '/') output[base_len++] = '/';
    memcpy(output + base_len, path, path_len + 1);
    return 0;
}

int sud_vfs_absolutize(int dirfd, const char *path,
                       char *output, size_t size)
{
    return absolute_at(dirfd, path, output, size);
}

struct resolved_node {
    uint64_t inode;
    uint64_t lookup_count;
    struct fuse_attr attr;
};

static void resolved_forget(struct resolved_node *node)
{
    if (node->lookup_count) {
        (void)sud_fuse_forget(node->inode, node->lookup_count);
        node->lookup_count = 0;
    }
}

static int normalize_absolute(const char *path, char *output, size_t size)
{
    if (!path || path[0] != '/') return -EINVAL;
    size_t used = 1;
    if (size < 2) return -ENAMETOOLONG;
    output[0] = '/';
    output[1] = '\0';
    const char *cursor = path;
    while (*cursor) {
        while (*cursor == '/') cursor++;
        if (!*cursor) break;
        const char *end = cursor;
        while (*end && *end != '/') end++;
        size_t length = (size_t)(end - cursor);
        if (length == 1 && cursor[0] == '.') {
            cursor = end;
            continue;
        }
        if (length == 2 && cursor[0] == '.' && cursor[1] == '.') {
            if (used > 1) {
                if (output[used - 1] == '/') used--;
                while (used > 1 && output[used - 1] != '/') used--;
                output[used] = '\0';
            }
            cursor = end;
            continue;
        }
        if (length > 255) return -ENAMETOOLONG;
        if (used > 1 && output[used - 1] != '/') {
            if (used + 1 >= size) return -ENAMETOOLONG;
            output[used++] = '/';
        }
        if (used + length >= size) return -ENAMETOOLONG;
        memcpy(output + used, cursor, length);
        used += length;
        output[used] = '\0';
        cursor = end;
    }
    return 0;
}

static int resolve_absolute_full(const char *path, struct resolved_node *node,
                                 int follow_final, char *canonical)
{
    char pending[PATH_MAX];
    int result = normalize_absolute(path, pending, sizeof(pending));
    if (result != 0) return result;
    unsigned int symlinks = 0;

restart:
    memset(node, 0, sizeof(*node));
    uint64_t parent = FUSE_ROOT_ID;
    const char *cursor = pending;
    while (*cursor) {
        while (*cursor == '/') cursor++;
        if (!*cursor) break;
        const char *end = cursor;
        while (*end && *end != '/') end++;
        size_t length = (size_t)(end - cursor);
        if (length == 0 || length > 255) return -ENAMETOOLONG;
        char component[256];
        memcpy(component, cursor, length);
        component[length] = '\0';
        struct fuse_entry_out entry;
        result = sud_fuse_lookup(parent, component, &entry);
        if (result != 0) {
            resolved_forget(node);
            return result;
        }
        resolved_forget(node);
        const char *remaining = end;
        while (*remaining == '/') remaining++;
        int is_final = !*remaining;
        if ((entry.attr.mode & S_IFMT) == S_IFLNK
            && (!is_final || follow_final)) {
            if (++symlinks > 40) {
                (void)sud_fuse_forget(entry.nodeid, 1);
                return -ELOOP;
            }
            char target[PATH_MAX];
            long target_len = sud_fuse_readlink(entry.nodeid, target,
                                                 sizeof(target) - 1);
            (void)sud_fuse_forget(entry.nodeid, 1);
            if (target_len < 0) return (int)target_len;
            if ((size_t)target_len >= sizeof(target)) return -ENAMETOOLONG;
            target[target_len] = '\0';
            char expanded[PATH_MAX * 2];
            size_t used = 0;
            if (target[0] != '/') {
                size_t prefix = (size_t)(cursor - pending);
                if (prefix >= sizeof(expanded)) return -ENAMETOOLONG;
                memcpy(expanded, pending, prefix);
                used = prefix;
            }
            size_t target_size = (size_t)target_len;
            size_t remaining_size = strlen(end);
            if (used + target_size + remaining_size >= sizeof(expanded))
                return -ENAMETOOLONG;
            memcpy(expanded + used, target, target_size);
            used += target_size;
            memcpy(expanded + used, end, remaining_size + 1);
            result = normalize_absolute(expanded, pending, sizeof(pending));
            if (result != 0) return result;
            goto restart;
        }
        node->inode = entry.nodeid;
        node->lookup_count = 1;
        node->attr = entry.attr;
        parent = entry.nodeid;
        cursor = end;
    }
    if (!node->lookup_count) {
        struct fuse_attr_out attributes;
        result = sud_fuse_getattr(FUSE_ROOT_ID, 0, 0, &attributes);
        if (result != 0) return result;
        node->inode = FUSE_ROOT_ID;
        node->lookup_count = 0;
        node->attr = attributes.attr;
    }
    if (canonical) memcpy(canonical, pending, strlen(pending) + 1);
    return 0;
}

static int resolve_absolute(const char *path, struct resolved_node *node)
{
    return resolve_absolute_full(path, node, 1, 0);
}

static int resolve_parent(int dirfd, const char *path, struct resolved_node *parent,
                          char *name, size_t name_size, char *absolute)
{
    int result = absolute_at(dirfd, path, absolute, PATH_MAX);
    if (result != 0) return result;
    size_t length = strlen(absolute);
    while (length > 1 && absolute[length - 1] == '/') absolute[--length] = '\0';
    char *slash = strrchr(absolute, '/');
    if (!slash || !slash[1]) return -EINVAL;
    size_t name_len = strlen(slash + 1);
    if (name_len >= name_size) return -ENAMETOOLONG;
    memcpy(name, slash + 1, name_len + 1);
    if (slash == absolute) slash[1] = '\0';
    else *slash = '\0';
    return resolve_absolute(absolute, parent);
}

static int create_placeholder(uint64_t inode, uint64_t handle,
                              uint64_t lookup_count,
                              const struct fuse_attr *attr,
                              const struct fuse_open_out *opened,
                              int flags, const char *path)
{
    int memfd_flags = (flags & O_CLOEXEC) ? MFD_CLOEXEC : 0;
    int fd = (int)raw_syscall6(SYS_memfd_create,
                               (long)"sarun-sud-fs-ofd", memfd_flags,
                               0, 0, 0, 0);
    if (fd < 0) return fd;
    long result = truncate_fd(fd, SUD_OFD_MAP_SIZE);
    if (result < 0) { raw_close(fd); return (int)result; }
    struct sud_remote_ofd *description = raw_mmap(
        0, SUD_OFD_MAP_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if ((unsigned long)description >= (unsigned long)-4095) {
        raw_close(fd);
        return (int)(long)description;
    }
    memset(description, 0, sizeof(*description));
    description->magic = SUD_OFD_MAGIC;
    description->version = SUD_OFD_VERSION;
    description->refs = 1;
    description->inode = inode;
    description->handle = handle;
    description->lookup_count = lookup_count;
    description->flags = (uint32_t)flags;
    description->open_flags = opened->open_flags;
    description->mode = attr->mode;
    description->path_len = (uint32_t)strlen(path);
    if (description->path_len >= sizeof(description->path)) {
        raw_syscall6(SYS_munmap, (long)description, SUD_OFD_MAP_SIZE, 0, 0, 0, 0);
        raw_close(fd);
        return -ENAMETOOLONG;
    }
    memcpy(description->path, path, description->path_len + 1);
    fd_table_init();
    local_lock(&g_fd_lock);
    result = fd_insert_locked(fd, description);
    local_unlock(&g_fd_lock);
    if (result != 0) {
        raw_syscall6(SYS_munmap, (long)description, SUD_OFD_MAP_SIZE, 0, 0, 0, 0);
        raw_close(fd);
        return (int)result;
    }
    return fd;
}

int sud_vfs_init(const char *initial_cwd)
{
    int result = sud_fs_client_init();
    if (result != 0) return result;
    fd_table_init();
    if (!g_cwd[0]) {
        if (initial_cwd && initial_cwd[0] == '/') {
            size_t length = strlen(initial_cwd);
            if (length >= sizeof(g_cwd)) return -ENAMETOOLONG;
            memcpy(g_cwd, initial_cwd, length + 1);
        } else {
            long length = raw_syscall6(SYS_getcwd, (long)g_cwd,
                                       sizeof(g_cwd), 0, 0, 0, 0);
            if (length < 0) return (int)length;
        }
    }
    return sud_fuse_init();
}

int sud_vfs_export_fd(int fd, int writable)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    if ((description->mode & S_IFMT) != S_IFREG) return -ENODEV;
    if (writable && (description->flags & O_ACCMODE) == O_RDONLY)
        return -EACCES;
    return sud_fs_export_fd(description->handle,
                            writable ? SUD_FS_FD_EXPORT_WRITE : 0);
}

int sud_vfs_openat(int dirfd, const char *path, int flags,
                   unsigned int mode, unsigned int umask)
{
    char absolute[PATH_MAX];
    int result = absolute_at(dirfd, path, absolute, sizeof(absolute));
    if (result != 0) return result;
    struct resolved_node node;
    char canonical[PATH_MAX];
    result = resolve_absolute_full(absolute, &node,
                                   !(flags & O_NOFOLLOW), canonical);
    struct fuse_open_out opened;
    if (result == 0) {
        if ((flags & O_CREAT) && (flags & O_EXCL)) {
            resolved_forget(&node);
            return -EEXIST;
        }
        int is_directory = (node.attr.mode & S_IFMT) == S_IFDIR;
        if (!is_directory && (node.attr.mode & S_IFMT) == S_IFLNK
            && (flags & O_NOFOLLOW)) {
            resolved_forget(&node);
            return -ELOOP;
        }
        if ((flags & O_DIRECTORY) && !is_directory) {
            resolved_forget(&node);
            return -ENOTDIR;
        }
        if (is_directory && (flags & (O_CREAT | O_TRUNC))) {
            resolved_forget(&node);
            return -EISDIR;
        }
        if ((flags & O_TRUNC) && (flags & O_ACCMODE) != O_RDONLY) {
            struct fuse_setattr_in request;
            struct fuse_attr_out attributes;
            memset(&request, 0, sizeof(request));
            request.valid = FATTR_SIZE;
            request.size = 0;
            result = sud_fuse_setattr(node.inode, &request, &attributes);
            if (result != 0) { resolved_forget(&node); return result; }
            node.attr = attributes.attr;
        }
        result = is_directory
            ? sud_fuse_opendir(node.inode, (uint32_t)flags, &opened)
            : sud_fuse_open(node.inode, (uint32_t)flags, &opened);
        if (result != 0) { resolved_forget(&node); return result; }
        memcpy(absolute, canonical, strlen(canonical) + 1);
    } else if (result == -ENOENT && (flags & O_CREAT)) {
        char parent_path[PATH_MAX];
        char name[256];
        struct resolved_node parent;
        result = resolve_parent(dirfd, path, &parent, name, sizeof(name), parent_path);
        if (result != 0) return result;
        struct fuse_entry_out entry;
        result = sud_fuse_create(parent.inode, name, (uint32_t)flags,
                                 (mode & 07777) | S_IFREG, umask,
                                 &entry, &opened);
        resolved_forget(&parent);
        if (result != 0) return result;
        node.inode = entry.nodeid;
        node.lookup_count = 1;
        node.attr = entry.attr;
        result = absolute_at(dirfd, path, absolute, sizeof(absolute));
        if (result != 0) {
            (void)sud_fuse_release(node.inode, opened.fh, (uint32_t)flags);
            resolved_forget(&node);
            return result;
        }
    } else {
        return result;
    }
    int fd = create_placeholder(node.inode, opened.fh, node.lookup_count,
                                &node.attr,
                                &opened, flags, absolute);
    if (fd < 0) {
        if ((node.attr.mode & S_IFMT) == S_IFDIR)
            (void)sud_fuse_releasedir(node.inode, opened.fh,
                                      (uint32_t)flags);
        else
            (void)sud_fuse_release(node.inode, opened.fh, (uint32_t)flags);
        resolved_forget(&node);
        return fd;
    }
    return fd;
}

int sud_vfs_owns_fd(int fd)
{
    return description_for(fd) != 0;
}

long sud_vfs_pread(int fd, void *buffer, size_t size, uint64_t offset)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    if ((description->mode & S_IFMT) == S_IFDIR) return -EISDIR;
    if ((description->flags & O_ACCMODE) == O_WRONLY) return -EBADF;
    size_t done = 0;
    while (done < size) {
        long count = sud_fuse_read(description->inode, description->handle,
                                   offset + done, description->flags,
                                   (unsigned char *)buffer + done, size - done);
        if (count < 0) return done ? (long)done : count;
        if (count == 0) break;
        done += (size_t)count;
    }
    return (long)done;
}

long sud_vfs_read(int fd, void *buffer, size_t size)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    if ((description->mode & S_IFMT) == S_IFDIR) return -EISDIR;
    if ((description->flags & O_ACCMODE) == O_WRONLY) return -EBADF;
    ofd_lock(description);
    long result = sud_vfs_pread(fd, buffer, size, description->offset);
    if (result > 0) description->offset += (uint64_t)result;
    ofd_unlock(description);
    return result;
}

long sud_vfs_pwrite(int fd, const void *buffer, size_t size, uint64_t offset)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    if ((description->mode & S_IFMT) == S_IFDIR) return -EISDIR;
    if ((description->flags & O_ACCMODE) == O_RDONLY) return -EBADF;
    size_t done = 0;
    while (done < size) {
        long count = sud_fuse_write(description->inode, description->handle,
                                    offset + done, description->flags,
                                    (const unsigned char *)buffer + done,
                                    size - done);
        if (count < 0) return done ? (long)done : count;
        if (count == 0) break;
        done += (size_t)count;
    }
    return (long)done;
}

long sud_vfs_write(int fd, const void *buffer, size_t size)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    ofd_lock(description);
    if (description->flags & O_APPEND) {
        struct fuse_attr_out attributes;
        int result = sud_fuse_getattr(description->inode, description->handle,
                                      1, &attributes);
        if (result != 0) { ofd_unlock(description); return result; }
        description->offset = attributes.attr.size;
    }
    long result = sud_vfs_pwrite(fd, buffer, size, description->offset);
    if (result > 0) description->offset += (uint64_t)result;
    ofd_unlock(description);
    return result;
}

long sud_vfs_lseek(int fd, int64_t offset, int whence)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    ofd_lock(description);
    int64_t base;
    if (whence == SEEK_SET) base = 0;
    else if (whence == SEEK_CUR) base = (int64_t)description->offset;
    else if (whence == SEEK_END) {
        struct fuse_attr_out attributes;
        int result = sud_fuse_getattr(description->inode, description->handle,
                                      1, &attributes);
        if (result != 0) { ofd_unlock(description); return result; }
        base = (int64_t)attributes.attr.size;
    } else {
        ofd_unlock(description);
        return -EINVAL;
    }
    if ((offset < 0 && base < -offset) || (offset > 0 && base > INT64_MAX - offset)) {
        ofd_unlock(description);
        return -EINVAL;
    }
    int64_t next = base + offset;
    if (next < 0) { ofd_unlock(description); return -EINVAL; }
    description->offset = (uint64_t)next;
    ofd_unlock(description);
    return (long)next;
}

int sud_vfs_ftruncate(int fd, uint64_t size)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    if ((description->flags & O_ACCMODE) == O_RDONLY) return -EINVAL;
    struct fuse_setattr_in request;
    struct fuse_attr_out attributes;
    memset(&request, 0, sizeof(request));
    request.valid = FATTR_SIZE | FATTR_FH;
    request.fh = description->handle;
    request.size = size;
    return sud_fuse_setattr(description->inode, &request, &attributes);
}

#if defined(__x86_64__)
struct sud_kernel_stat {
    unsigned long st_dev, st_ino, st_nlink;
    unsigned int st_mode, st_uid, st_gid;
    int pad;
    unsigned long st_rdev;
    long st_size, st_blksize, st_blocks;
    long st_atime, st_atime_nsec;
    long st_mtime, st_mtime_nsec;
    long st_ctime, st_ctime_nsec;
    long unused[3];
};
#else
struct sud_kernel_stat {
    unsigned long long st_dev;
    unsigned char pad0[4];
    unsigned long old_ino;
    unsigned int st_mode, st_nlink;
    unsigned long st_uid, st_gid;
    unsigned long long st_rdev;
    unsigned char pad3[4];
    long long st_size;
    unsigned long st_blksize;
    unsigned long long st_blocks;
    unsigned long st_atime, st_atime_nsec;
    unsigned long st_mtime, st_mtime_nsec;
    unsigned long st_ctime, st_ctime_nsec;
    unsigned long long st_ino;
};
#endif

static void fill_stat(void *buffer, const struct fuse_attr *attr)
{
    struct sud_kernel_stat *st = buffer;
    memset(st, 0, sizeof(*st));
    st->st_dev = 0;
    st->st_mode = attr->mode;
    st->st_nlink = attr->nlink;
    st->st_uid = attr->uid;
    st->st_gid = attr->gid;
    st->st_rdev = attr->rdev;
    st->st_size = attr->size;
    st->st_blksize = attr->blksize;
    st->st_blocks = attr->blocks;
    st->st_atime = attr->atime;
    st->st_atime_nsec = attr->atimensec;
    st->st_mtime = attr->mtime;
    st->st_mtime_nsec = attr->mtimensec;
    st->st_ctime = attr->ctime;
    st->st_ctime_nsec = attr->ctimensec;
#if defined(__x86_64__)
    st->st_ino = attr->ino;
#else
    st->old_ino = (unsigned long)attr->ino;
    st->st_ino = attr->ino;
#endif
}

int sud_vfs_fstat(int fd, void *stat_buffer)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description || !stat_buffer) return -EBADF;
    struct fuse_attr_out attributes;
    int result = sud_fuse_getattr(description->inode, description->handle,
                                  1, &attributes);
    if (result == 0) fill_stat(stat_buffer, &attributes.attr);
    return result;
}

static void fill_statx(struct statx *st, const struct fuse_attr *attr)
{
    memset(st, 0, sizeof(*st));
    st->stx_mask = STATX_BASIC_STATS;
    st->stx_blksize = attr->blksize;
    st->stx_nlink = attr->nlink;
    st->stx_uid = attr->uid;
    st->stx_gid = attr->gid;
    st->stx_mode = (uint16_t)attr->mode;
    st->stx_ino = attr->ino;
    st->stx_size = attr->size;
    st->stx_blocks = attr->blocks;
    st->stx_atime.tv_sec = attr->atime;
    st->stx_atime.tv_nsec = attr->atimensec;
    st->stx_mtime.tv_sec = attr->mtime;
    st->stx_mtime.tv_nsec = attr->mtimensec;
    st->stx_ctime.tv_sec = attr->ctime;
    st->stx_ctime.tv_nsec = attr->ctimensec;
    st->stx_rdev_major = (uint32_t)(attr->rdev >> 8);
    st->stx_rdev_minor = (uint32_t)(attr->rdev & 0xff);
}

int sud_vfs_statat(int dirfd, const char *path, int follow, void *stat_buffer)
{
    if (!path || !stat_buffer) return -EFAULT;
    if (!path[0]) return sud_vfs_fstat(dirfd, stat_buffer);
    char absolute[PATH_MAX];
    int result = absolute_at(dirfd, path, absolute, sizeof(absolute));
    if (result != 0) return result;
    struct resolved_node node;
    memset(&node, 0, sizeof(node));
    result = resolve_absolute_full(absolute, &node, follow, 0);
    if (result == 0) fill_stat(stat_buffer, &node.attr);
    resolved_forget(&node);
    return result;
}

int sud_vfs_statx(int dirfd, const char *path, int follow,
                  unsigned int mask, struct statx *stat_buffer)
{
    (void)mask;
    if (!path || !stat_buffer) return -EFAULT;
    struct fuse_attr attributes;
    int result;
    if (!path[0]) {
        struct sud_remote_ofd *description = description_for(dirfd);
        if (!description) return -EBADF;
        struct fuse_attr_out output;
        result = sud_fuse_getattr(description->inode, description->handle,
                                  1, &output);
        attributes = output.attr;
    } else {
        char absolute[PATH_MAX];
        result = absolute_at(dirfd, path, absolute, sizeof(absolute));
        if (result != 0) return result;
        struct resolved_node node;
        memset(&node, 0, sizeof(node));
        result = resolve_absolute_full(absolute, &node, follow, 0);
        if (result == 0) attributes = node.attr;
        resolved_forget(&node);
    }
    if (result == 0) fill_statx(stat_buffer, &attributes);
    return result;
}

int sud_vfs_accessat(int dirfd, const char *path, unsigned int mask)
{
    char absolute[PATH_MAX];
    int result = absolute_at(dirfd, path, absolute, sizeof(absolute));
    if (result != 0) return result;
    struct resolved_node node;
    memset(&node, 0, sizeof(node));
    result = resolve_absolute_full(absolute, &node, 1, 0);
    if (result == 0 && mask) result = sud_fuse_access(node.inode, mask);
    resolved_forget(&node);
    return result;
}

int sud_vfs_fsync(int fd, int datasync)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    return sud_fuse_fsync(description->inode, description->handle,
                          (description->mode & S_IFMT) == S_IFDIR, datasync);
}

int sud_vfs_setattrat(int dirfd, const char *path, int follow,
                      const struct fuse_setattr_in *request)
{
    char absolute[PATH_MAX];
    int result = absolute_at(dirfd, path, absolute, sizeof(absolute));
    if (result != 0) return result;
    struct resolved_node node;
    memset(&node, 0, sizeof(node));
    result = resolve_absolute_full(absolute, &node, follow, 0);
    if (result == 0) {
        struct fuse_attr_out attributes;
        result = sud_fuse_setattr(node.inode, request, &attributes);
    }
    resolved_forget(&node);
    return result;
}

int sud_vfs_fsetattr(int fd, const struct fuse_setattr_in *request)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    struct fuse_setattr_in input = *request;
    input.valid |= FATTR_FH;
    input.fh = description->handle;
    struct fuse_attr_out attributes;
    return sud_fuse_setattr(description->inode, &input, &attributes);
}

int sud_vfs_getfl(int fd)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    return (int)(description->flags
                 & (O_ACCMODE | O_APPEND | O_NONBLOCK));
}

int sud_vfs_setfl(int fd, int flags)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    ofd_lock(description);
    description->flags = (description->flags & ~(O_APPEND | O_NONBLOCK))
                       | (flags & (O_APPEND | O_NONBLOCK));
    ofd_unlock(description);
    return 0;
}

int sud_vfs_chdir(const char *path)
{
    char absolute[PATH_MAX];
    int result = absolute_at(AT_FDCWD, path, absolute, sizeof(absolute));
    if (result != 0) return result;
    struct resolved_node node;
    char canonical[PATH_MAX];
    result = resolve_absolute_full(absolute, &node, 1, canonical);
    if (result != 0) return result;
    if ((node.attr.mode & S_IFMT) != S_IFDIR) result = -ENOTDIR;
    resolved_forget(&node);
    if (result != 0) return result;
    local_lock(&g_cwd_lock);
    size_t length = strlen(canonical);
    memcpy(g_cwd, canonical, length + 1);
    local_unlock(&g_cwd_lock);
    return 0;
}

int sud_vfs_fchdir(int fd)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    if ((description->mode & S_IFMT) != S_IFDIR) return -ENOTDIR;
    local_lock(&g_cwd_lock);
    memcpy(g_cwd, description->path, description->path_len + 1);
    local_unlock(&g_cwd_lock);
    return 0;
}

long sud_vfs_getcwd(char *buffer, size_t size)
{
    if (!buffer) return -EFAULT;
    local_lock(&g_cwd_lock);
    size_t length = strlen(g_cwd) + 1;
    if (length > size) {
        local_unlock(&g_cwd_lock);
        return -ERANGE;
    }
    memcpy(buffer, g_cwd, length);
    local_unlock(&g_cwd_lock);
    return (long)length;
}

long sud_vfs_getdents64(int fd, void *buffer, size_t size)
{
    struct sud_remote_ofd *description = description_for(fd);
    if (!description) return -EBADF;
    if ((description->mode & S_IFMT) != S_IFDIR) return -ENOTDIR;
    unsigned char raw[SUD_FS_SLOT_DATA];
    ofd_lock(description);
    long count = sud_fuse_readdir(description->inode, description->handle,
                                  description->offset, raw, sizeof(raw));
    if (count <= 0) {
        ofd_unlock(description);
        return count;
    }
    size_t input_offset = 0;
    size_t output_offset = 0;
    while (input_offset < (size_t)count) {
        struct fuse_dirent *entry = (struct fuse_dirent *)(raw + input_offset);
        if ((size_t)count - input_offset < FUSE_NAME_OFFSET) {
            ofd_unlock(description);
            return -EPROTO;
        }
        size_t fuse_length = FUSE_DIRENT_ALIGN(FUSE_NAME_OFFSET
                                               + entry->namelen);
        if (entry->namelen > 255 || fuse_length > (size_t)count - input_offset) {
            ofd_unlock(description);
            return -EPROTO;
        }
        size_t output_length = (sizeof(struct linux_dirent64)
                                + entry->namelen + 1 + 7) & ~(size_t)7;
        if (output_length > size - output_offset) break;
        struct linux_dirent64 *out =
            (struct linux_dirent64 *)((unsigned char *)buffer + output_offset);
        out->d_ino = entry->ino;
        out->d_off = entry->off;
        out->d_reclen = (unsigned short)output_length;
        out->d_type = (unsigned char)entry->type;
        memcpy(out->d_name, entry->name, entry->namelen);
        out->d_name[entry->namelen] = '\0';
        size_t used = sizeof(*out) + entry->namelen + 1;
        if (output_length > used)
            memset((unsigned char *)out + used, 0, output_length - used);
        output_offset += output_length;
        input_offset += fuse_length;
        description->offset = entry->off;
    }
    ofd_unlock(description);
    if (output_offset == 0 && count > 0) return -EINVAL;
    return (long)output_offset;
}

long sud_vfs_readlinkat(int dirfd, const char *path, char *buffer, size_t size)
{
    char absolute[PATH_MAX];
    int result = absolute_at(dirfd, path, absolute, sizeof(absolute));
    if (result != 0) return result;
    struct resolved_node node;
    result = resolve_absolute_full(absolute, &node, 0, 0);
    if (result != 0) return result;
    if ((node.attr.mode & S_IFMT) != S_IFLNK) result = -EINVAL;
    else result = (int)sud_fuse_readlink(node.inode, buffer, size);
    resolved_forget(&node);
    return result;
}

int sud_vfs_mkdirat(int dirfd, const char *path, unsigned int mode,
                    unsigned int umask)
{
    char parent_path[PATH_MAX];
    char name[256];
    struct resolved_node parent;
    int result = resolve_parent(dirfd, path, &parent, name, sizeof(name),
                                parent_path);
    if (result != 0) return result;
    struct fuse_entry_out entry;
    result = sud_fuse_mkdir(parent.inode, name,
                            (mode & 07777) | S_IFDIR, umask, &entry);
    resolved_forget(&parent);
    if (result == 0 && entry.nodeid != FUSE_ROOT_ID)
        (void)sud_fuse_forget(entry.nodeid, 1);
    return result;
}

int sud_vfs_mknodat(int dirfd, const char *path, unsigned int mode,
                    unsigned int device, unsigned int umask)
{
    char parent_path[PATH_MAX];
    char name[256];
    struct resolved_node parent;
    int result = resolve_parent(dirfd, path, &parent, name, sizeof(name),
                                parent_path);
    if (result != 0) return result;
    struct fuse_entry_out entry;
    result = sud_fuse_mknod(parent.inode, name, mode, device, umask, &entry);
    resolved_forget(&parent);
    if (result == 0 && entry.nodeid != FUSE_ROOT_ID)
        (void)sud_fuse_forget(entry.nodeid, 1);
    return result;
}

int sud_vfs_unlinkat(int dirfd, const char *path, int directory)
{
    char parent_path[PATH_MAX];
    char name[256];
    struct resolved_node parent;
    int result = resolve_parent(dirfd, path, &parent, name, sizeof(name),
                                parent_path);
    if (result != 0) return result;
    result = sud_fuse_unlink(parent.inode, name, directory);
    resolved_forget(&parent);
    return result;
}

int sud_vfs_renameat2(int old_dirfd, const char *old_path,
                      int new_dirfd, const char *new_path,
                      unsigned int flags)
{
    char old_parent_path[PATH_MAX];
    char new_parent_path[PATH_MAX];
    char old_name[256];
    char new_name[256];
    struct resolved_node old_parent;
    struct resolved_node new_parent;
    memset(&new_parent, 0, sizeof(new_parent));
    int result = resolve_parent(old_dirfd, old_path, &old_parent,
                                old_name, sizeof(old_name), old_parent_path);
    if (result != 0) return result;
    result = resolve_parent(new_dirfd, new_path, &new_parent,
                            new_name, sizeof(new_name), new_parent_path);
    if (result == 0)
        result = sud_fuse_rename(old_parent.inode, old_name,
                                 new_parent.inode, new_name, flags);
    resolved_forget(&new_parent);
    resolved_forget(&old_parent);
    return result;
}

int sud_vfs_symlinkat(const char *target, int dirfd, const char *path)
{
    if (!target) return -EFAULT;
    char parent_path[PATH_MAX];
    char name[256];
    struct resolved_node parent;
    int result = resolve_parent(dirfd, path, &parent, name, sizeof(name),
                                parent_path);
    if (result != 0) return result;
    struct fuse_entry_out entry;
    result = sud_fuse_symlink(parent.inode, name, target, &entry);
    resolved_forget(&parent);
    if (result == 0 && entry.nodeid != FUSE_ROOT_ID)
        (void)sud_fuse_forget(entry.nodeid, 1);
    return result;
}

int sud_vfs_linkat(int old_dirfd, const char *old_path,
                   int new_dirfd, const char *new_path, int follow)
{
    char old_absolute[PATH_MAX];
    int result = absolute_at(old_dirfd, old_path, old_absolute,
                             sizeof(old_absolute));
    if (result != 0) return result;
    struct resolved_node source;
    result = resolve_absolute_full(old_absolute, &source, follow, 0);
    if (result != 0) return result;
    char new_parent_path[PATH_MAX];
    char new_name[256];
    struct resolved_node new_parent;
    memset(&new_parent, 0, sizeof(new_parent));
    result = resolve_parent(new_dirfd, new_path, &new_parent,
                            new_name, sizeof(new_name), new_parent_path);
    if (result == 0) {
        struct fuse_entry_out entry;
        result = sud_fuse_link(source.inode, new_parent.inode, new_name,
                               &entry);
        if (result == 0 && entry.nodeid != FUSE_ROOT_ID)
            (void)sud_fuse_forget(entry.nodeid, 1);
    }
    resolved_forget(&new_parent);
    resolved_forget(&source);
    return result;
}

int sud_vfs_close(int fd)
{
    fd_table_init();
    local_lock(&g_fd_lock);
    struct fd_entry *entry = fd_lookup_locked(fd);
    if (!entry) { local_unlock(&g_fd_lock); return -EBADF; }
    struct sud_remote_ofd *description = entry->description;
    entry->fd = -1;
    entry->description = 0;
    local_unlock(&g_fd_lock);
    int is_directory = (description->mode & S_IFMT) == S_IFDIR;
    int result = is_directory ? 0
        : sud_fuse_flush(description->inode, description->handle,
                         description->flags);
    if (__atomic_sub_fetch(&description->refs, 1u, __ATOMIC_ACQ_REL) == 0) {
        if (is_directory)
            (void)sud_fuse_releasedir(description->inode,
                                      description->handle,
                                      description->flags);
        else
            (void)sud_fuse_release(description->inode, description->handle,
                                   description->flags);
        if (description->lookup_count)
            (void)sud_fuse_forget(description->inode,
                                  description->lookup_count);
    }
    raw_syscall6(SYS_munmap, (long)description, SUD_OFD_MAP_SIZE, 0, 0, 0, 0);
    return result;
}

int sud_vfs_dup(int oldfd, int newfd)
{
    if (oldfd == newfd) return sud_vfs_owns_fd(oldfd) ? 0 : -EBADF;
    struct sud_remote_ofd *description = description_for(oldfd);
    if (!description) return -EBADF;
    struct sud_remote_ofd *mapping = map_description(newfd);
    if (!mapping) return -EBADF;
    __atomic_fetch_add(&mapping->refs, 1u, __ATOMIC_ACQ_REL);
    fd_table_init();
    local_lock(&g_fd_lock);
    struct fd_entry *existing = fd_lookup_locked(newfd);
    struct sud_remote_ofd *displaced = 0;
    if (existing) {
        displaced = existing->description;
        existing->description = mapping;
    } else if (fd_insert_locked(newfd, mapping) != 0) {
        local_unlock(&g_fd_lock);
        __atomic_fetch_sub(&mapping->refs, 1u, __ATOMIC_ACQ_REL);
        raw_syscall6(SYS_munmap, (long)mapping, SUD_OFD_MAP_SIZE, 0, 0, 0, 0);
        return -EMFILE;
    }
    local_unlock(&g_fd_lock);
    if (displaced) {
        int is_directory = (displaced->mode & S_IFMT) == S_IFDIR;
        if (!is_directory)
            (void)sud_fuse_flush(displaced->inode, displaced->handle,
                                 displaced->flags);
        if (__atomic_sub_fetch(&displaced->refs, 1u, __ATOMIC_ACQ_REL) == 0) {
            if (is_directory)
                (void)sud_fuse_releasedir(displaced->inode,
                                          displaced->handle,
                                          displaced->flags);
            else
                (void)sud_fuse_release(displaced->inode, displaced->handle,
                                       displaced->flags);
            if (displaced->lookup_count)
                (void)sud_fuse_forget(displaced->inode,
                                      displaced->lookup_count);
        }
        raw_syscall6(SYS_munmap, (long)displaced,
                     SUD_OFD_MAP_SIZE, 0, 0, 0, 0);
    }
    return 0;
}

void sud_vfs_fork_child(void)
{
    if (raw_getpid() != raw_gettid()) return;
    g_fd_lock = 0;
    g_cwd_lock = 0;
    if (!g_fd_initialized) return;
    for (unsigned int i = 0; i < SUD_FD_TABLE_SIZE; i++)
        if (g_fds[i].fd >= 0)
            __atomic_fetch_add(&g_fds[i].description->refs, 1u, __ATOMIC_ACQ_REL);
}

void sud_vfs_process_exit(void)
{
    if (!g_fd_initialized) return;
    for (unsigned int i = 0; i < SUD_FD_TABLE_SIZE; i++) {
        if (g_fds[i].fd < 0) continue;
        struct sud_remote_ofd *description = g_fds[i].description;
        int is_directory = (description->mode & S_IFMT) == S_IFDIR;
        if (!is_directory)
            (void)sud_fuse_flush(description->inode, description->handle,
                                 description->flags);
        if (__atomic_sub_fetch(&description->refs, 1u, __ATOMIC_ACQ_REL) == 0) {
            if (is_directory)
                (void)sud_fuse_releasedir(description->inode,
                                          description->handle,
                                          description->flags);
            else
                (void)sud_fuse_release(description->inode,
                                       description->handle,
                                       description->flags);
            if (description->lookup_count)
                (void)sud_fuse_forget(description->inode,
                                      description->lookup_count);
        }
        g_fds[i].fd = -1;
    }
}
