/*
 * sud/inramfs/addin.c — SUD addin glue for the in-RAM filesystem.
 *
 * Acts as the syscall-dispatch front-end for the inramfs core
 * (super.c, vfs.c).  For every path-bearing syscall we:
 *   1. Compute an absolute path from (dirfd, path).
 *   2. Check whether that path is under the configured inramfs mount.
 *   3. If yes, run the in-process op, set ctx->ret to the result,
 *      and return 1 to short-circuit the kernel call.
 *   4. If no, return 0 so the next addin (path_remap) gets a turn.
 *
 * For fd-bearing syscalls (read/write/lseek/ftruncate/fstat/...) we
 * check sud_inramfs_owns_fd() — fds returned by inramfs are real
 * memfd fds that the kernel knows about (so close/dup/poll work
 * naturally), but read/write/seek/etc. against those fds must be
 * intercepted to read from inramfs's own data extents.
 *
 * The fd table is process-local (file descriptors don't survive
 * exec across the wrapper boundary in the initial implementation —
 * documented limitation).
 */

#include "sud/inramfs/inramfs.h"
#include "sud/inramfs/internal.h"
#include "sud/addin.h"
#include "sud/raw.h"

/* ================================================================
 * Process-local fd table
 *
 * Every successful sud_inramfs_op_open allocates a real kernel fd
 * via memfd_create() so the fd number is unique within the process
 * and is recognised by close/dup/etc.  We map fd → inramfs handle
 * in a small open-addressed array.  Linear probing is fine: typical
 * concurrent open file counts under sud are O(10).
 * ================================================================ */

#define SUD_IR_FD_TABLE_SIZE 1024

struct sud_ir_open_file {
    int      kfd;            /* real kernel fd; -1 = empty slot */
    uint32_t inode_idx;
    uint32_t generation;     /* sanity cookie vs. sud_ir_inode.generation */
    uint64_t pos;            /* current file position */
    int      flags;          /* O_RDONLY / O_WRONLY / O_RDWR / O_APPEND */
    /* Directory-iteration cookie: byte offset into the dirblock chain
     * at which the next getdents64 call resumes. */
    uint32_t dir_cookie;
};

static struct sud_ir_open_file g_fdtab[SUD_IR_FD_TABLE_SIZE];
static int                    g_fdtab_init;

static void fdtab_init(void)
{
    if (g_fdtab_init) return;
    for (int i = 0; i < SUD_IR_FD_TABLE_SIZE; i++) g_fdtab[i].kfd = -1;
    g_fdtab_init = 1;
}

static struct sud_ir_open_file *fdtab_lookup(int fd)
{
    if (!g_fdtab_init || fd < 0) return 0;
    for (int i = 0; i < SUD_IR_FD_TABLE_SIZE; i++) {
        if (g_fdtab[i].kfd == fd) return &g_fdtab[i];
    }
    return 0;
}

static struct sud_ir_open_file *fdtab_alloc(int kfd, uint32_t inode_idx,
                                            int flags)
{
    fdtab_init();
    for (int i = 0; i < SUD_IR_FD_TABLE_SIZE; i++) {
        if (g_fdtab[i].kfd == -1) {
            g_fdtab[i].kfd       = kfd;
            g_fdtab[i].inode_idx = inode_idx;
            struct sud_ir_inode *ino = sud_ir_inode_get(inode_idx);
            g_fdtab[i].generation = ino ? ino->generation : 0;
            g_fdtab[i].pos        = 0;
            g_fdtab[i].flags      = flags;
            g_fdtab[i].dir_cookie = 0;
            return &g_fdtab[i];
        }
    }
    return 0;
}

static void fdtab_release(int fd)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (of) of->kfd = -1;
}

int sud_inramfs_owns_fd(int fd) { return fdtab_lookup(fd) != 0; }

/* Public: scrub any fdtab entry for `fd` (used when the kernel is
 * about to atomically replace it via dup2/dup3 from a NON-inramfs
 * source — in that case we don't hijack the syscall, but we still
 * need to forget our stale registration before the kernel does the
 * dup, otherwise subsequent read/write on `fd` will be misrouted
 * back into inramfs).  No effect if fd is not tracked. */
static void fdtab_forget(int fd)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (of) of->kfd = -1;
}

/* ================================================================
 * Path resolution helper (dirfd, relative path) → absolute path
 * ================================================================ */

/* Read /proc/self/cwd via raw syscall.  Used to absolute-ify
 * AT_FDCWD relative paths.  Returns 0 / -errno. */
static long read_cwd_abs(char *out, size_t out_sz)
{
    long n = raw_syscall6(SYS_readlinkat, AT_FDCWD,
                          (long)"/proc/self/cwd",
                          (long)out, (long)out_sz - 1, 0, 0);
    if (n < 0) return n;
    out[n] = '\0';
    return 0;
}

/* Compose abs from (dirfd, path).  Returns 0 / -errno.  When dirfd
 * names an inramfs directory fd, we resolve it back to the inode and
 * walk up to root via the directory tree (we don't store parent
 * pointers, so for now we only support AT_FDCWD and absolute paths;
 * inramfs dirfds are documented as not yet usable as base for
 * relative *at-syscalls).  This matches ramfs semantics for
 * common workloads (build tools use absolute paths). */
static int absolutise(int dirfd, const char *path, char *out, size_t out_sz)
{
    if (!path) return -EFAULT;
    if (path[0] == '/') {
        size_t n = strlen(path);
        if (n + 1 > out_sz) return -ENAMETOOLONG;
        memcpy(out, path, n + 1);
        return 0;
    }
    if (dirfd != AT_FDCWD) {
        struct sud_ir_open_file *of = fdtab_lookup(dirfd);
        if (of) {
            /* inramfs dirfd: not yet usable for relative paths in
             * this initial cut.  Tell the caller "not under mount"
             * so it falls through to the kernel (which will fail
             * sensibly with the kernel's view of the fd, since the
             * fd is a real memfd). */
            return -EXDEV;
        }
        /* Real kernel fd we don't own; let it pass through. */
        return -EXDEV;
    }
    /* AT_FDCWD: prepend cwd. */
    char cwd[PATH_MAX];
    long rc = read_cwd_abs(cwd, sizeof(cwd));
    if (rc < 0) return (int)rc;
    size_t cl = strlen(cwd);
    size_t pl = strlen(path);
    if (cl + 1 + pl + 1 > out_sz) return -ENAMETOOLONG;
    memcpy(out, cwd, cl);
    out[cl] = '/';
    memcpy(out + cl + 1, path, pl + 1);
    return 0;
}

/* Public: try to resolve (dirfd, path) into an inramfs-mount-relative
 * absolute path.  Returns 0 on success; -1 if path is NOT under the
 * mount; or -errno on a hard failure.  On success `out` is NUL-
 * terminated and starts with the mount prefix. */
int sud_inramfs_resolve_at(int dirfd, const char *path,
                           char *out, size_t out_sz)
{
    if (!sud_inramfs_active()) return -1;
    int rc = absolutise(dirfd, path, out, out_sz);
    if (rc < 0) return rc;
    if (!sud_inramfs_path_under_mount(out)) return -1;
    return 0;
}

/* ================================================================
 * Public open: walks the path, allocates a real fd via memfd_create,
 * registers the fd in the local table.
 * ================================================================ */

static int alloc_kfd(uint32_t inode_idx)
{
    /* Cookie name encodes inode for debug-friendliness. */
    char name[32];
    snprintf(name, sizeof(name), "sud-inramfs-%u", inode_idx);
#ifdef SYS_memfd_create
    long fd = raw_syscall6(SYS_memfd_create, (long)name,
                           MFD_CLOEXEC, 0, 0, 0, 0);
    if (fd < 0) return (int)fd;
    return (int)fd;
#else
    (void)inode_idx;
    return -ENOSYS;
#endif
}

long sud_inramfs_op_open(const char *abs_path, int flags, int mode)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path,
                               (flags & O_NOFOLLOW) ? 0 : 1, &err);

    if (!idx && err != -ENOENT) return err;
    if (!idx && !(flags & O_CREAT)) return -ENOENT;

    /* O_EXCL semantics: if the file already exists, fail. */
    if (idx && (flags & O_CREAT) && (flags & O_EXCL)) return -EEXIST;

    if (!idx) {
        /* Create regular file using the namespace lock + helpers. */
        uint32_t pidx;
        const char *base;
        size_t blen;
        int rc = sud_ir_walk_parent(abs_path, &pidx, &base, &blen);
        if (rc) return rc;
        if (blen == 0 || blen > 63) return -ENAMETOOLONG;
        struct sud_ir_super *sb = sud_ir_sb();
        sud_ir_lock(&sb->lock);
        uint32_t exists;
        if (sud_ir_dir_lookup(pidx, base, blen, &exists) == 0) {
            /* Race: another opener won.  Use the existing inode. */
            idx = exists;
        } else {
            uint32_t uid = 0, gid = 0;
#ifdef SYS_getuid
            uid = (uint32_t)raw_syscall6(SYS_getuid, 0, 0, 0, 0, 0, 0);
#endif
#ifdef SYS_getgid
            gid = (uint32_t)raw_syscall6(SYS_getgid, 0, 0, 0, 0, 0, 0);
#endif
            uint32_t ni = sud_ir_inode_alloc(SUD_IR_T_REG,
                                             (uint32_t)(mode & 07777),
                                             uid, gid);
            if (!ni) { sud_ir_unlock(&sb->lock); return -ENOSPC; }
            struct sud_ir_inode *new_ino = sud_ir_inode_get(ni);
            new_ino->nlink = 1;
            int lrc = sud_ir_dir_link(pidx, base, blen, ni, DT_REG);
            if (lrc) {
                sud_ir_inode_free(ni);
                sud_ir_unlock(&sb->lock);
                return lrc;
            }
            idx = ni;
        }
        sud_ir_unlock(&sb->lock);
    }

    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;

    /* O_DIRECTORY enforcement. */
    if ((flags & O_DIRECTORY) && ino->type != SUD_IR_T_DIR) return -ENOTDIR;
    /* Default: refuse to open a directory for writing. */
    if (ino->type == SUD_IR_T_DIR) {
        if ((flags & O_ACCMODE) != O_RDONLY) return -EISDIR;
    }

    /* O_TRUNC on regular files. */
    if ((flags & O_TRUNC) && ino->type == SUD_IR_T_REG
        && (flags & O_ACCMODE) != O_RDONLY) {
        sud_ir_file_truncate(ino, 0);
    }

    int kfd = alloc_kfd(idx);
    if (kfd < 0) return kfd;
    struct sud_ir_open_file *of = fdtab_alloc(kfd, idx, flags);
    if (!of) {
        raw_close(kfd);
        return -EMFILE;
    }
    /* O_APPEND is honoured at write time via flags. */
    return kfd;
}

long sud_inramfs_op_close(int fd)
{
    if (!fdtab_lookup(fd)) return -EBADF;
    fdtab_release(fd);
    raw_close(fd);
    return 0;
}

long sud_inramfs_op_read(int fd, void *buf, size_t count)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    if ((of->flags & O_ACCMODE) == O_WRONLY) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    if (ino->type != SUD_IR_T_REG) return -EISDIR;
    long n = sud_ir_file_read(ino, buf, count, (off_t)of->pos);
    if (n > 0) of->pos += (uint64_t)n;
    return n;
}

long sud_inramfs_op_write(int fd, const void *buf, size_t count)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    if ((of->flags & O_ACCMODE) == O_RDONLY) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    if (ino->type != SUD_IR_T_REG) return -EBADF;
    off_t off;
    if (of->flags & O_APPEND) off = (off_t)ino->size;
    else                      off = (off_t)of->pos;
    long n = sud_ir_file_write(ino, buf, count, off);
    if (n > 0) of->pos = (uint64_t)off + (uint64_t)n;
    return n;
}

long sud_inramfs_op_pread(int fd, void *buf, size_t count, off_t off)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    if ((of->flags & O_ACCMODE) == O_WRONLY) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    return sud_ir_file_read(ino, buf, count, off);
}

long sud_inramfs_op_pwrite(int fd, const void *buf, size_t count, off_t off)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    if ((of->flags & O_ACCMODE) == O_RDONLY) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    return sud_ir_file_write(ino, buf, count, off);
}

long sud_inramfs_op_lseek(int fd, off_t off, int whence)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    int64_t newp;
    switch (whence) {
        case SEEK_SET: newp = off; break;
        case SEEK_CUR: newp = (int64_t)of->pos + off; break;
        case SEEK_END: newp = (int64_t)ino->size + off; break;
        default: return -EINVAL;
    }
    if (newp < 0) return -EINVAL;
    of->pos = (uint64_t)newp;
    return newp;
}

long sud_inramfs_op_ftruncate(int fd, off_t length)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    if ((of->flags & O_ACCMODE) == O_RDONLY) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    if (ino->type != SUD_IR_T_REG) return -EINVAL;
    return sud_ir_file_truncate(ino, length);
}

long sud_inramfs_op_fstat(int fd, void *st_buf)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    sud_inramfs_fill_stat(st_buf, of->inode_idx, ino);
    return 0;
}

long sud_inramfs_op_fchmod(int fd, int mode)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    ino->mode = (uint32_t)(mode & 07777);
    ino->ctime_ns = sud_ir_now_ns();
    return 0;
}

long sud_inramfs_op_fchown(int fd, int uid, int gid)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    if (uid != -1) ino->uid = (uint32_t)uid;
    if (gid != -1) ino->gid = (uint32_t)gid;
    ino->ctime_ns = sud_ir_now_ns();
    return 0;
}

long sud_inramfs_op_futimens(int fd, const struct timespec ts[2])
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) return -EBADF;
    uint64_t now = sud_ir_now_ns();
    if (!ts) {
        ino->atime_ns = now;
        ino->mtime_ns = now;
    } else {
        if (ts[0].tv_nsec == UTIME_NOW)        ino->atime_ns = now;
        else if (ts[0].tv_nsec != UTIME_OMIT)  ino->atime_ns =
            (uint64_t)ts[0].tv_sec * 1000000000ull + (uint64_t)ts[0].tv_nsec;
        if (ts[1].tv_nsec == UTIME_NOW)        ino->mtime_ns = now;
        else if (ts[1].tv_nsec != UTIME_OMIT)  ino->mtime_ns =
            (uint64_t)ts[1].tv_sec * 1000000000ull + (uint64_t)ts[1].tv_nsec;
    }
    ino->ctime_ns = now;
    return 0;
}

/* ================================================================
 * getdents64 — serialise dirents from the dir inode's dirblock chain.
 * dir_cookie tracks "global" dirent index across blocks so partial
 * reads resume correctly.
 * ================================================================ */

long sud_inramfs_op_getdents64(int fd, void *buf, size_t count)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    struct sud_ir_inode *dir = sud_ir_inode_get(of->inode_idx);
    if (!dir) return -EBADF;
    if (dir->type != SUD_IR_T_DIR) return -ENOTDIR;

    char *out = (char *)buf;
    size_t written = 0;
    uint32_t emitted = 0;
    uint32_t want_skip = of->dir_cookie;

    /* Inject "." and ".." as dirents 0 and 1. */
    static const struct {
        const char *name;
        uint32_t    ino_offset_token;     /* 0=self, 1=self again (no parent ptr) */
    } dotdot[2] = { { ".", 0 }, { "..", 1 } };
    for (int i = 0; i < 2; i++) {
        if (emitted++ < want_skip) continue;
        size_t nlen = (i == 0) ? 1 : 2;
        size_t reclen = (sizeof(struct linux_dirent64) + nlen + 1 + 7) & ~7u;
        if (written + reclen > count) {
            if (written == 0) return -EINVAL;
            return (long)written;
        }
        struct linux_dirent64 *de = (struct linux_dirent64 *)(out + written);
        de->d_ino    = of->inode_idx;
        de->d_off    = (int64_t)(emitted);
        de->d_reclen = (unsigned short)reclen;
        de->d_type   = DT_DIR;
        memcpy(de->d_name, dotdot[i].name, nlen);
        de->d_name[nlen] = '\0';
        written += reclen;
        of->dir_cookie = emitted;
    }

    uint32_t off = dir->u.dir.dirblock_head_offset;
    while (off) {
        struct sud_ir_dirblock *db =
            (struct sud_ir_dirblock *)sud_ir_ptr(off);
        for (uint32_t i = 0; i < db->used; i++) {
            const struct sud_ir_dirent *src = &db->ents[i];
            if (src->ino_index == 0) continue;
            if (emitted++ < want_skip) continue;
            size_t nlen = src->name_len;
            size_t reclen = (sizeof(struct linux_dirent64) + nlen + 1 + 7) & ~7u;
            if (written + reclen > count) {
                if (written == 0) return -EINVAL;
                return (long)written;
            }
            struct linux_dirent64 *de = (struct linux_dirent64 *)(out + written);
            de->d_ino    = src->ino_index;
            de->d_off    = (int64_t)emitted;
            de->d_reclen = (unsigned short)reclen;
            de->d_type   = src->d_type;
            memcpy(de->d_name, src->name, nlen);
            de->d_name[nlen] = '\0';
            written += reclen;
            of->dir_cookie = emitted;
        }
        off = db->next_offset;
    }
    return (long)written;
}

/* ================================================================
 * mmap — map the file's data extent (or a slice of it) into the
 * caller's address space.  Strategy:
 *   - if the file's data lives in the shared region's data area
 *     (which is always the case here — single contiguous extent),
 *     re-mmap the underlying memfd-backed shm region at the
 *     extent's slot, MAP_SHARED.
 *   - the underlying shm fd is reopened by name from /dev/shm so
 *     we don't have to keep it pinned across processes.
 * ================================================================ */

void *sud_inramfs_op_mmap(void *addr, size_t length, int prot, int flags,
                          int fd, off_t offset, int *err)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) { *err = EBADF; return MAP_FAILED; }
    struct sud_ir_inode *ino = sud_ir_inode_get(of->inode_idx);
    if (!ino) { *err = EBADF; return MAP_FAILED; }
    if (ino->type != SUD_IR_T_REG) { *err = ENODEV; return MAP_FAILED; }
    if (offset < 0) { *err = EINVAL; return MAP_FAILED; }

    /* Empty file: hand back an anonymous mapping of the requested
     * length so mmap()-then-fault programs behave sensibly. */
    if (ino->u.reg.head_block == 0) {
        long r = (long)raw_mmap(addr, length, prot,
                                flags | MAP_ANONYMOUS, -1, 0);
        if ((unsigned long)r >= (unsigned long)-4095) {
            *err = (int)-r;
            return MAP_FAILED;
        }
        return (void *)r;
    }

    /* Single-block fast path: when the requested range falls inside
     * one FAT block we can hand back a direct pointer into the data
     * shm, giving full MAP_SHARED semantics with no copy.  Multi-
     * block ranges need page-table coalescing of non-contiguous
     * blocks, which is the per-file shm work in M3b — refuse for
     * now rather than silently lose MAP_SHARED writeback. */
    uint64_t end = (uint64_t)offset + (uint64_t)length;
    uint32_t block_ix = (uint32_t)(offset / SUD_IR_BLOCK_SIZE);
    uint32_t in_block = (uint32_t)(offset % SUD_IR_BLOCK_SIZE);
    if (end > (uint64_t)(block_ix + 1) * SUD_IR_BLOCK_SIZE) {
        *err = ENOTSUP;
        return MAP_FAILED;
    }
    if (block_ix >= ino->u.reg.nblocks) { *err = EINVAL; return MAP_FAILED; }

    uint32_t *fat = sud_ir_fat();
    uint32_t b = ino->u.reg.head_block;
    for (uint32_t i = 0; i < block_ix; i++) b = fat[b];
    void *p = (char *)sud_ir_data_block(b) + in_block;
    if ((flags & MAP_FIXED) && addr && addr != p) {
        *err = EINVAL;
        return MAP_FAILED;
    }
    (void)prot;     /* underlying mapping is RW; PROT_READ-only
                     * callers see no harm. */
    return p;
}

/* ================================================================
 * fill_stat re-export (vfs.c keeps the static; addin.c needs it for
 * fstat without re-walking).  We define a thin trampoline here.
 * ================================================================ */

/* Helper that mirrors fill_stat in vfs.c.  We re-derive the layout
 * here to avoid leaking the kstat union into a header. */
#if defined(__x86_64__)
struct sud_ir_kstat_pub {
    unsigned long st_dev;
    unsigned long st_ino;
    unsigned long st_nlink;
    unsigned int  st_mode;
    unsigned int  st_uid;
    unsigned int  st_gid;
    int           __pad0;
    unsigned long st_rdev;
    long          st_size;
    long          st_blksize;
    long          st_blocks;
    long          st_atime; long st_atime_nsec;
    long          st_mtime; long st_mtime_nsec;
    long          st_ctime; long st_ctime_nsec;
    long          __unused[3];
};
#else
struct sud_ir_kstat_pub {
    unsigned long long st_dev;
    unsigned char  __pad0[4];
    unsigned long  __st_ino;
    unsigned int   st_mode;
    unsigned int   st_nlink;
    unsigned long  st_uid;
    unsigned long  st_gid;
    unsigned long long st_rdev;
    unsigned char  __pad3[4];
    long long      st_size;
    unsigned long  st_blksize;
    unsigned long long st_blocks;
    unsigned long  st_atime; unsigned long st_atime_nsec;
    unsigned long  st_mtime; unsigned long st_mtime_nsec;
    unsigned long  st_ctime; unsigned long st_ctime_nsec;
    unsigned long long st_ino;
};
#endif

void sud_inramfs_fill_stat(void *st_buf, uint32_t idx,
                           const struct sud_ir_inode *ino)
{
    struct sud_ir_kstat_pub *st = (struct sud_ir_kstat_pub *)st_buf;
    memset(st, 0, sizeof(*st));
    uint32_t mode = (ino->type == SUD_IR_T_DIR ? S_IFDIR
                  : ino->type == SUD_IR_T_LNK ? S_IFLNK
                  : S_IFREG) | (ino->mode & 07777);
#if defined(__x86_64__)
    st->st_ino   = idx;
    st->st_nlink = ino->nlink;
    st->st_mode  = mode;
    st->st_uid   = ino->uid;
    st->st_gid   = ino->gid;
    st->st_size  = (long)ino->size;
    st->st_blksize = SUD_IR_BLOCK_SIZE;
    st->st_blocks  = (long)((ino->size + 511) / 512);
    st->st_atime = (long)(ino->atime_ns / 1000000000ull);
    st->st_atime_nsec = (long)(ino->atime_ns % 1000000000ull);
    st->st_mtime = (long)(ino->mtime_ns / 1000000000ull);
    st->st_mtime_nsec = (long)(ino->mtime_ns % 1000000000ull);
    st->st_ctime = (long)(ino->ctime_ns / 1000000000ull);
    st->st_ctime_nsec = (long)(ino->ctime_ns % 1000000000ull);
#else
    st->__st_ino = idx;
    st->st_ino   = idx;
    st->st_nlink = ino->nlink;
    st->st_mode  = mode;
    st->st_uid   = ino->uid;
    st->st_gid   = ino->gid;
    st->st_size  = (long long)ino->size;
    st->st_blksize = SUD_IR_BLOCK_SIZE;
    st->st_blocks  = (unsigned long long)((ino->size + 511) / 512);
    st->st_atime = (unsigned long)(ino->atime_ns / 1000000000ull);
    st->st_atime_nsec = (unsigned long)(ino->atime_ns % 1000000000ull);
    st->st_mtime = (unsigned long)(ino->mtime_ns / 1000000000ull);
    st->st_mtime_nsec = (unsigned long)(ino->mtime_ns % 1000000000ull);
    st->st_ctime = (unsigned long)(ino->ctime_ns / 1000000000ull);
    st->st_ctime_nsec = (unsigned long)(ino->ctime_ns % 1000000000ull);
#endif
}

/* statx adapter: walk the path, fill a `struct statx` for the
 * basic-stats fields.  Glibc/coreutils gate on STATX_BASIC_STATS,
 * so we always populate those bits regardless of the caller's mask
 * (kernel statx is allowed to over-fill).  Newer fields (btime,
 * mnt_id, dio align) are zeroed. */
long sud_inramfs_op_statx_fill(const char *abs_path, int follow,
                               unsigned int mask, void *statx_buf)
{
    (void)mask;
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, follow ? 1 : 0, &err);
    if (!idx) return err ? err : -ENOENT;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;

    struct statx *stx = (struct statx *)statx_buf;
    memset(stx, 0, sizeof(*stx));

    stx->stx_mask    = STATX_BASIC_STATS;
    stx->stx_blksize = SUD_IR_BLOCK_SIZE;
    stx->stx_nlink   = ino->nlink;
    stx->stx_uid     = ino->uid;
    stx->stx_gid     = ino->gid;
    uint32_t mode_bits = (ino->type == SUD_IR_T_DIR ? S_IFDIR
                       :  ino->type == SUD_IR_T_LNK ? S_IFLNK
                       :                              S_IFREG)
                       | (ino->mode & 07777);
    stx->stx_mode    = (uint16_t)mode_bits;
    stx->stx_ino     = idx;
    stx->stx_size    = ino->size;
    stx->stx_blocks  = (ino->size + 511) / 512;
    stx->stx_atime.tv_sec  = (int64_t)(ino->atime_ns / 1000000000ull);
    stx->stx_atime.tv_nsec = (uint32_t)(ino->atime_ns % 1000000000ull);
    stx->stx_mtime.tv_sec  = (int64_t)(ino->mtime_ns / 1000000000ull);
    stx->stx_mtime.tv_nsec = (uint32_t)(ino->mtime_ns % 1000000000ull);
    stx->stx_ctime.tv_sec  = (int64_t)(ino->ctime_ns / 1000000000ull);
    stx->stx_ctime.tv_nsec = (uint32_t)(ino->ctime_ns % 1000000000ull);
    /* No btime (creation time tracking) yet; mask bit stays clear. */
    return 0;
}

/* ================================================================
 * fd duplication.
 *
 * We delegate the actual fd-number allocation to the kernel via the
 * raw dup/dup3 syscalls — the kernel knows which numbers are free,
 * what FD_CLOEXEC needs to be set on the new fd, and how to close
 * an existing entry at `newfd` for dup2/dup3.  After the kernel
 * call returns the new fd number, we register it in our fd table so
 * subsequent read/write/lseek/close on the new fd dispatch back into
 * inramfs.
 *
 * Caveat: in Linux, dup2/dup3 produces two fds that share the same
 * "open file description" (and therefore f_pos).  Our fd table
 * stores a per-fd `pos`, so two duplicates each track their own
 * position.  This is wrong only for code that reads/writes through
 * one duplicate while another reads f_pos — uncommon in build
 * workloads.  The shared inode means file *contents* are correctly
 * shared, which is the property bash's redirect logic actually
 * relies on.
 * ================================================================ */

static long fdtab_register_dup(struct sud_ir_open_file *src, int newkfd)
{
    /* Allocate a new fdtab slot for newkfd.  We can't reuse fdtab_alloc
     * directly because it overwrites pos/flags; we want to inherit. */
    fdtab_init();
    /* Defensive: if newkfd already has an entry (e.g. dup2 onto an
     * inramfs fd), the kernel already closed it on our behalf — drop
     * the stale entry. */
    {
        struct sud_ir_open_file *stale = fdtab_lookup(newkfd);
        if (stale) stale->kfd = -1;
    }
    for (int i = 0; i < SUD_IR_FD_TABLE_SIZE; i++) {
        if (g_fdtab[i].kfd == -1) {
            g_fdtab[i].kfd        = newkfd;
            g_fdtab[i].inode_idx  = src->inode_idx;
            g_fdtab[i].generation = src->generation;
            g_fdtab[i].pos        = src->pos;
            g_fdtab[i].flags      = src->flags;
            g_fdtab[i].dir_cookie = src->dir_cookie;
            return newkfd;
        }
    }
    return -EMFILE;
}

long sud_inramfs_op_dup(int oldfd)
{
    struct sud_ir_open_file *of = fdtab_lookup(oldfd);
    if (!of) return -EBADF;
#ifdef SYS_dup
    long newkfd = raw_syscall6(SYS_dup, oldfd, 0, 0, 0, 0, 0);
#else
    /* No SYS_dup on this arch (e.g. aarch64 only has dup3); use
     * dup3 with newfd allocated by us via F_DUPFD trick — but for
     * the platforms we ship on (x86_64 / i386) SYS_dup exists. */
    long newkfd = -ENOSYS;
#endif
    if (newkfd < 0) return newkfd;
    long rc = fdtab_register_dup(of, (int)newkfd);
    if (rc < 0) raw_close((int)newkfd);
    return rc;
}

long sud_inramfs_op_dup3(int oldfd, int newfd, int flags)
{
    struct sud_ir_open_file *of = fdtab_lookup(oldfd);
    if (!of) return -EBADF;
    if (oldfd == newfd) {
        /* Linux's dup2 returns oldfd; dup3 returns -EINVAL for this.
         * Caller distinguishes via flags-validation outside. */
        return newfd;
    }
#ifdef SYS_dup3
    long newkfd = raw_syscall6(SYS_dup3, oldfd, newfd, flags, 0, 0, 0);
#else
    long newkfd = raw_syscall6(SYS_dup2, oldfd, newfd, 0, 0, 0, 0);
    (void)flags;
#endif
    if (newkfd < 0) return newkfd;
    long rc = fdtab_register_dup(of, (int)newkfd);
    if (rc < 0) raw_close((int)newkfd);
    return rc;
}

long sud_inramfs_op_fcntl_dupfd(int oldfd, int minfd, int cloexec)
{
    struct sud_ir_open_file *of = fdtab_lookup(oldfd);
    if (!of) return -EBADF;
#if defined(SYS_fcntl)
    long cmd = cloexec ? F_DUPFD_CLOEXEC : F_DUPFD;
    long newkfd = raw_syscall6(SYS_fcntl, oldfd, cmd, minfd, 0, 0, 0);
#elif defined(SYS_fcntl64)
    long cmd = cloexec ? F_DUPFD_CLOEXEC : F_DUPFD;
    long newkfd = raw_syscall6(SYS_fcntl64, oldfd, cmd, minfd, 0, 0, 0);
#else
    (void)minfd; (void)cloexec;
    long newkfd = -ENOSYS;
#endif
    if (newkfd < 0) return newkfd;
    long rc = fdtab_register_dup(of, (int)newkfd);
    if (rc < 0) raw_close((int)newkfd);
    return rc;
}

long sud_inramfs_op_fcntl_getfl(int fd)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    /* Return access-mode | O_APPEND | O_NONBLOCK that we recorded at
     * open time (plus any later F_SETFL).  Other status bits are not
     * meaningful for in-RAM files. */
    return (long)(of->flags & (O_ACCMODE | O_APPEND | O_NONBLOCK));
}

long sud_inramfs_op_fcntl_setfl(int fd, int flags)
{
    struct sud_ir_open_file *of = fdtab_lookup(fd);
    if (!of) return -EBADF;
    /* fcntl(F_SETFL) only updates O_APPEND/O_NONBLOCK/O_DIRECT/O_ASYNC
     * — access mode and creation flags are immutable.  We only model
     * the first two. */
    int keep = of->flags & ~(O_APPEND | O_NONBLOCK);
    int set  = flags & (O_APPEND | O_NONBLOCK);
    of->flags = keep | set;
    return 0;
}

/* ================================================================
 * Pre-syscall dispatch
 *
 * Each path-bearing or fd-bearing syscall is handled by a single
 * row in the dispatch table.  A row binds a syscall number to:
 *
 *   - a "match" predicate (does inramfs claim this call?), and
 *   - a "do" function that performs the in-process op and writes
 *     the result into ctx->ret.
 *
 * The match step uses argument indices stored in the row to find
 * either the fd or the (dirfd, path) pair to test against the
 * mount.  The do step receives those same arguments pre-resolved.
 *
 * Splitting match from do keeps the table dense (most rows are one
 * line) and lets non-matching syscalls fall through to the next
 * addin without paying for path resolution.
 * ================================================================ */

static int short_circuit(struct sud_syscall_ctx *ctx, long ret)
{
    ctx->ret = ret;
    return 1;
}

/* Resolve (dirfd, path) into ctx->scratch as a NUL-terminated path
 * known to be under the inramfs mount.  Returns 0 / -1 / -errno;
 * on -1 the caller falls through to the next addin. */
static int resolve_path(struct sud_syscall_ctx *ctx,
                        int dirfd, const char *path,
                        const char **abs_out)
{
    if (!ctx->scratch || ctx->scratch_size < PATH_MAX) return -1;
    int rc = sud_inramfs_resolve_at(dirfd, path,
                                    ctx->scratch, ctx->scratch_size);
    if (rc < 0) return rc;
    *abs_out = ctx->scratch;
    return 0;
}

/* Resolve two paths.  Used by rename/link.  src_dirfd/src_path
 * yields `*src_out`, copied into the small `src_save` buffer so
 * the second resolve can reuse ctx->scratch for the destination.
 * Returns 0 / -1 / -errno using the same convention as resolve_path. */
static int resolve_two_paths(struct sud_syscall_ctx *ctx,
                             int src_dirfd, const char *src_path,
                             int dst_dirfd, const char *dst_path,
                             char *src_save, size_t src_save_sz,
                             const char **src_out, const char **dst_out)
{
    const char *first;
    int r = resolve_path(ctx, src_dirfd, src_path, &first);
    if (r < 0) return r;
    size_t l = strlen(first);
    if (l + 1 > src_save_sz) return -ENAMETOOLONG;
    memcpy(src_save, first, l + 1);
    *src_out = src_save;
    r = sud_inramfs_resolve_at(dst_dirfd, dst_path,
                               ctx->scratch, ctx->scratch_size);
    /* If the destination is on the host FS, this is a cross-FS
     * link/rename and we must refuse it (the source is on inramfs
     * and the destination is not). */
    if (r < 0) return -EXDEV;
    *dst_out = ctx->scratch;
    return 0;
}

/* off_t pieces in pread64/pwrite64 are ABI-split on 32-bit.  On
 * x86_64 the offset is one register; on i386 it's two. */
static off_t pread_offset(const long *args)
{
#if defined(__x86_64__)
    return (off_t)args[3];
#else
    return (off_t)((uint64_t)(uint32_t)args[3]
                 | ((uint64_t)(uint32_t)args[4] << 32));
#endif
}

/* ---- handler types --------------------------------------------- */

/* All handlers receive the syscall ctx (for ret + raw args) plus a
 * pre-extracted artefact (resolved path, or fd value).  Returning
 * the handler's value is what gets short-circuited. */
typedef long (*ir_fd_handler)(struct sud_syscall_ctx *ctx, int fd);
typedef long (*ir_path_handler)(struct sud_syscall_ctx *ctx,
                                const char *abs);
typedef long (*ir_two_path_handler)(struct sud_syscall_ctx *ctx,
                                    const char *src, const char *dst);

/* ---- per-syscall handlers (one tiny function per syscall) ----- */

static long h_read   (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_read (fd, (void *)c->args[1], (size_t)c->args[2]); }
static long h_write  (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_write(fd, (const void *)c->args[1], (size_t)c->args[2]); }
static long h_pread  (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_pread (fd, (void *)c->args[1], (size_t)c->args[2],
                               pread_offset(c->args)); }
static long h_pwrite (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_pwrite(fd, (const void *)c->args[1],
                               (size_t)c->args[2], pread_offset(c->args)); }
static long h_lseek  (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_lseek(fd, (off_t)c->args[1], (int)c->args[2]); }
static long h_close  (struct sud_syscall_ctx *c, int fd)
{ (void)c; return sud_inramfs_op_close(fd); }
static long h_ftrunc (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_ftruncate(fd, (off_t)c->args[1]); }
static long h_fstat  (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_fstat(fd, (void *)c->args[1]); }
static long h_fchmod (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_fchmod(fd, (int)c->args[1]); }
static long h_fchown (struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_fchown(fd, (int)c->args[1], (int)c->args[2]); }
static long h_getdents64(struct sud_syscall_ctx *c, int fd)
{ return sud_inramfs_op_getdents64(fd, (void *)c->args[1], (size_t)c->args[2]); }
static long h_fsync_noop(struct sud_syscall_ctx *c, int fd)
{ (void)c; (void)fd; return 0; }

/* mmap differs: uses err out-param and returns a pointer.  Wrap into
 * the long-return shape via the standard Linux mmap kernel ABI:
 * success yields the address, failure yields -errno. */
static long h_mmap(struct sud_syscall_ctx *c, int fd)
{
    int err = 0;
    void *p = sud_inramfs_op_mmap((void *)c->args[0], (size_t)c->args[1],
                                  (int)c->args[2], (int)c->args[3],
                                  fd, (off_t)c->args[5], &err);
    return (p == MAP_FAILED) ? -err : (long)p;
}
#ifdef SYS_mmap2
static long h_mmap2(struct sud_syscall_ctx *c, int fd)
{
    int err = 0;
    void *p = sud_inramfs_op_mmap((void *)c->args[0], (size_t)c->args[1],
                                  (int)c->args[2], (int)c->args[3],
                                  fd, (off_t)c->args[5] << MINI_MMAP2_SHIFT,
                                  &err);
    return (p == MAP_FAILED) ? -err : (long)p;
}
#endif

/* Path handlers.  The dispatch table tells us where in args[] the
 * path lives and which dirfd to use; by the time we're called the
 * path has already been resolved and verified to be under mount.
 * We just need to pull any remaining mode/flags from ctx->args. */

static long h_open_creat(struct sud_syscall_ctx *c, const char *abs)
{
    /* open(path, flags, mode) — flags @[1], mode @[2]. */
    return sud_inramfs_op_open(abs, (int)c->args[1], (int)c->args[2]);
}
static long h_openat_creat(struct sud_syscall_ctx *c, const char *abs)
{
    /* openat(dirfd, path, flags, mode) — flags @[2], mode @[3]. */
    return sud_inramfs_op_open(abs, (int)c->args[2], (int)c->args[3]);
}

static long h_stat_follow  (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_stat(abs, (void *)c->args[1], 1); }
static long h_stat_nofollow(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_stat(abs, (void *)c->args[1], 0); }
static long h_fstatat(struct sud_syscall_ctx *c, const char *abs)
{
    int follow = ((int)c->args[3] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
    return sud_inramfs_op_stat(abs, (void *)c->args[2], follow);
}

static long h_statx(struct sud_syscall_ctx *c, const char *abs)
{
    int follow = ((int)c->args[2] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
    return sud_inramfs_op_statx_fill(abs, follow,
                                     (unsigned int)c->args[3],
                                     (void *)c->args[4]);
}

static long h_access_a1(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_access(abs, (int)c->args[1]); }
static long h_access_a2(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_access(abs, (int)c->args[2]); }

static long h_mkdir_a1 (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_mkdir(abs, (int)c->args[1]); }
static long h_mkdir_a2 (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_mkdir(abs, (int)c->args[2]); }
static long h_rmdir    (struct sud_syscall_ctx *c, const char *abs)
{ (void)c; return sud_inramfs_op_rmdir(abs); }
static long h_unlink   (struct sud_syscall_ctx *c, const char *abs)
{ (void)c; return sud_inramfs_op_unlink(abs); }
static long h_unlinkat (struct sud_syscall_ctx *c, const char *abs)
{
    return ((int)c->args[2] & AT_REMOVEDIR)
        ? sud_inramfs_op_rmdir(abs)
        : sud_inramfs_op_unlink(abs);
}

static long h_symlink_a0(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_symlink((const char *)c->args[0], abs); }

static long h_readlink_a1(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_readlink(abs, (char *)c->args[1], (size_t)c->args[2]); }
static long h_readlink_a2(struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_readlink(abs, (char *)c->args[2], (size_t)c->args[3]); }

static long h_chmod_a1 (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_chmod(abs, (int)c->args[1]); }
static long h_chmod_a2 (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_chmod(abs, (int)c->args[2]); }
static long h_chown    (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_chown(abs, (int)c->args[1], (int)c->args[2], 1); }
static long h_lchown   (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_chown(abs, (int)c->args[1], (int)c->args[2], 0); }
static long h_fchownat (struct sud_syscall_ctx *c, const char *abs)
{
    int follow = ((int)c->args[4] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
    return sud_inramfs_op_chown(abs, (int)c->args[2], (int)c->args[3], follow);
}

static long h_truncate (struct sud_syscall_ctx *c, const char *abs)
{ return sud_inramfs_op_truncate(abs, (off_t)c->args[1]); }

static long h_utimensat(struct sud_syscall_ctx *c, const char *abs)
{
    int follow = ((int)c->args[3] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
    return sud_inramfs_op_utimensat(abs,
        (const struct timespec *)c->args[2], follow);
}

/* Two-path handlers. */
static long h_rename(struct sud_syscall_ctx *c, const char *src, const char *dst)
{ (void)c; return sud_inramfs_op_rename(src, dst, 0); }
static long h_renameat2(struct sud_syscall_ctx *c, const char *src, const char *dst)
{ return sud_inramfs_op_rename(src, dst, (unsigned int)c->args[4]); }
static long h_link(struct sud_syscall_ctx *c, const char *src, const char *dst)
{ (void)c; return sud_inramfs_op_link(src, dst); }

/* ---- dispatch table -------------------------------------------- */

/* Encoding:
 *   .nr           — syscall number we match.  Rows are skipped at
 *                   build time when SYS_xxx is undefined for the
 *                   running arch.
 *   .fd_idx       — args[] index of the fd to test for inramfs
 *                   ownership.  -1 means "this is a path syscall".
 *   .dirfd_idx    — args[] index of the dirfd to combine with the
 *                   path.  -1 means "use AT_FDCWD".  Only meaningful
 *                   when path_idx >= 0.
 *   .path_idx     — args[] index of the pathname.  -1 means "this is
 *                   an fd syscall".
 *   .fd_h/path_h  — handler function.  Exactly one of fd_h/path_h
 *                   is set per row.
 */
struct ir_dispatch_row {
    long             nr;
    signed char      fd_idx;
    signed char      dirfd_idx;
    signed char      path_idx;
    ir_fd_handler    fd_h;
    ir_path_handler  path_h;
};

#define ROW_FD(SYSNR, FDIDX, H)  \
    { SYSNR, (FDIDX), -1, -1, (H), 0 }
#define ROW_PATH(SYSNR, DIRFDIDX, PATHIDX, H) \
    { SYSNR, -1, (DIRFDIDX), (PATHIDX), 0, (H) }

static const struct ir_dispatch_row ir_dispatch[] = {
    /* fd ops */
#ifdef SYS_read
    ROW_FD(SYS_read,        0, h_read),
#endif
#ifdef SYS_write
    ROW_FD(SYS_write,       0, h_write),
#endif
#ifdef SYS_pread64
    ROW_FD(SYS_pread64,     0, h_pread),
#endif
#ifdef SYS_pwrite64
    ROW_FD(SYS_pwrite64,    0, h_pwrite),
#endif
#ifdef SYS_lseek
    ROW_FD(SYS_lseek,       0, h_lseek),
#endif
#ifdef SYS_close
    ROW_FD(SYS_close,       0, h_close),
#endif
#ifdef SYS_ftruncate
    ROW_FD(SYS_ftruncate,   0, h_ftrunc),
#endif
#ifdef SYS_ftruncate64
    ROW_FD(SYS_ftruncate64, 0, h_ftrunc),
#endif
#ifdef SYS_fstat
    ROW_FD(SYS_fstat,       0, h_fstat),
#endif
#ifdef SYS_fstat64
    ROW_FD(SYS_fstat64,     0, h_fstat),
#endif
#ifdef SYS_fchmod
    ROW_FD(SYS_fchmod,      0, h_fchmod),
#endif
#ifdef SYS_fchown
    ROW_FD(SYS_fchown,      0, h_fchown),
#endif
#ifdef SYS_getdents64
    ROW_FD(SYS_getdents64,  0, h_getdents64),
#endif
#ifdef SYS_mmap
    ROW_FD(SYS_mmap,        4, h_mmap),
#endif
#ifdef SYS_mmap2
    ROW_FD(SYS_mmap2,       4, h_mmap2),
#endif
#ifdef SYS_fsync
    ROW_FD(SYS_fsync,        0, h_fsync_noop),
#endif
#ifdef SYS_fdatasync
    ROW_FD(SYS_fdatasync,    0, h_fsync_noop),
#endif
#ifdef SYS_sync_file_range
    ROW_FD(SYS_sync_file_range, 0, h_fsync_noop),
#endif

    /* open / openat — DIRFDIDX of -1 means AT_FDCWD; openat is +1. */
#ifdef SYS_open
    ROW_PATH(SYS_open,    -1, 0, h_open_creat),
#endif
#ifdef SYS_openat
    ROW_PATH(SYS_openat,   0, 1, h_openat_creat),
#endif

    /* stat family */
#ifdef SYS_stat
    ROW_PATH(SYS_stat,    -1, 0, h_stat_follow),
#endif
#ifdef SYS_lstat
    ROW_PATH(SYS_lstat,   -1, 0, h_stat_nofollow),
#endif
#ifdef SYS_stat64
    ROW_PATH(SYS_stat64,  -1, 0, h_stat_follow),
#endif
#ifdef SYS_lstat64
    ROW_PATH(SYS_lstat64, -1, 0, h_stat_nofollow),
#endif
#ifdef SYS_newfstatat
    ROW_PATH(SYS_newfstatat, 0, 1, h_fstatat),
#endif
#ifdef SYS_fstatat64
    ROW_PATH(SYS_fstatat64,  0, 1, h_fstatat),
#endif
#ifdef SYS_statx
    ROW_PATH(SYS_statx,      0, 1, h_statx),
#endif

    /* access family */
#ifdef SYS_access
    ROW_PATH(SYS_access,     -1, 0, h_access_a1),
#endif
#ifdef SYS_faccessat
    ROW_PATH(SYS_faccessat,   0, 1, h_access_a2),
#endif
#ifdef SYS_faccessat2
    ROW_PATH(SYS_faccessat2,  0, 1, h_access_a2),
#endif

    /* directory ops */
#ifdef SYS_mkdir
    ROW_PATH(SYS_mkdir,    -1, 0, h_mkdir_a1),
#endif
#ifdef SYS_mkdirat
    ROW_PATH(SYS_mkdirat,   0, 1, h_mkdir_a2),
#endif
#ifdef SYS_rmdir
    ROW_PATH(SYS_rmdir,    -1, 0, h_rmdir),
#endif
#ifdef SYS_unlink
    ROW_PATH(SYS_unlink,   -1, 0, h_unlink),
#endif
#ifdef SYS_unlinkat
    ROW_PATH(SYS_unlinkat,  0, 1, h_unlinkat),
#endif

    /* symlink/readlink — symlink path is the *new* name (args[1] for
     * symlink, args[2] for symlinkat).  The target text is opaque to
     * the path-resolution machinery; the handler reads it from args[0]. */
#ifdef SYS_symlink
    ROW_PATH(SYS_symlink,   -1, 1, h_symlink_a0),
#endif
#ifdef SYS_symlinkat
    ROW_PATH(SYS_symlinkat,  1, 2, h_symlink_a0),
#endif
#ifdef SYS_readlink
    ROW_PATH(SYS_readlink,  -1, 0, h_readlink_a1),
#endif
#ifdef SYS_readlinkat
    ROW_PATH(SYS_readlinkat, 0, 1, h_readlink_a2),
#endif

    /* chmod / chown */
#ifdef SYS_chmod
    ROW_PATH(SYS_chmod,    -1, 0, h_chmod_a1),
#endif
#ifdef SYS_fchmodat
    ROW_PATH(SYS_fchmodat,  0, 1, h_chmod_a2),
#endif
#ifdef SYS_chown
    ROW_PATH(SYS_chown,    -1, 0, h_chown),
#endif
#ifdef SYS_lchown
    ROW_PATH(SYS_lchown,   -1, 0, h_lchown),
#endif
#ifdef SYS_fchownat
    ROW_PATH(SYS_fchownat,  0, 1, h_fchownat),
#endif

    /* truncate / utimensat */
#ifdef SYS_truncate
    ROW_PATH(SYS_truncate,   -1, 0, h_truncate),
#endif
#ifdef SYS_truncate64
    ROW_PATH(SYS_truncate64, -1, 0, h_truncate),
#endif
};

#define IR_DISPATCH_LEN ((int)(sizeof(ir_dispatch)/sizeof(ir_dispatch[0])))

static int dispatch_table(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;
    for (int i = 0; i < IR_DISPATCH_LEN; i++) {
        const struct ir_dispatch_row *row = &ir_dispatch[i];
        if (row->nr != nr) continue;

        if (row->fd_idx >= 0) {
            int fd = (int)ctx->args[row->fd_idx];
            if (!sud_inramfs_owns_fd(fd)) return 0;
            return short_circuit(ctx, row->fd_h(ctx, fd));
        }
        /* path row */
        int dirfd = (row->dirfd_idx < 0) ? AT_FDCWD
                                         : (int)ctx->args[row->dirfd_idx];
        const char *abs;
        int r = resolve_path(ctx, dirfd,
                             (const char *)ctx->args[row->path_idx], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, row->path_h(ctx, abs));
    }
    return 0;
}

/* Two-path syscalls (rename / link family).  Pulled out of the main
 * table because they need a second resolve and the cross-FS error
 * shape is uniform. */
static int dispatch_two_path(struct sud_syscall_ctx *ctx,
                             int src_dirfd_idx, int src_path_idx,
                             int dst_dirfd_idx, int dst_path_idx,
                             ir_two_path_handler h)
{
    char src_save[PATH_MAX];
    int src_dirfd = (src_dirfd_idx < 0) ? AT_FDCWD
                                        : (int)ctx->args[src_dirfd_idx];
    int dst_dirfd = (dst_dirfd_idx < 0) ? AT_FDCWD
                                        : (int)ctx->args[dst_dirfd_idx];
    const char *src, *dst;
    int r = resolve_two_paths(ctx,
                              src_dirfd, (const char *)ctx->args[src_path_idx],
                              dst_dirfd, (const char *)ctx->args[dst_path_idx],
                              src_save, sizeof(src_save), &src, &dst);
    if (r == -1) return 0;
    if (r < 0)   return short_circuit(ctx, r);
    return short_circuit(ctx, h(ctx, src, dst));
}

/* dup family.  We hijack only when the SOURCE fd is inramfs-owned;
 * if the source is a host fd but the DESTINATION is one of ours, we
 * scrub our stale fdtab entry before letting the kernel atomically
 * replace the fd, otherwise subsequent read/write on the destination
 * would be misrouted back here. */
static int dispatch_dup_to(struct sud_syscall_ctx *ctx,
                           int oldfd, int newfd, int flags)
{
    if (sud_inramfs_owns_fd(oldfd))
        return short_circuit(ctx, sud_inramfs_op_dup3(oldfd, newfd, flags));
    if (sud_inramfs_owns_fd(newfd)) fdtab_forget(newfd);
    return 0;
}

/* fcntl: only F_DUPFD/F_DUPFD_CLOEXEC and F_GETFL/F_SETFL are
 * inramfs-relevant.  F_GETFD/F_SETFD (FD_CLOEXEC) deliberately fall
 * through to the kernel — cloexec lives on the underlying memfd and
 * the kernel handles it correctly. */
static int dispatch_fcntl(struct sud_syscall_ctx *ctx)
{
    int fd = (int)ctx->args[0];
    if (!sud_inramfs_owns_fd(fd)) return 0;
    long cmd = ctx->args[1];
    long arg = ctx->args[2];
    switch (cmd) {
    case F_DUPFD:
        return short_circuit(ctx,
            sud_inramfs_op_fcntl_dupfd(fd, (int)arg, 0));
    case F_DUPFD_CLOEXEC:
        return short_circuit(ctx,
            sud_inramfs_op_fcntl_dupfd(fd, (int)arg, 1));
    case F_GETFL:
        return short_circuit(ctx, sud_inramfs_op_fcntl_getfl(fd));
    case F_SETFL:
        return short_circuit(ctx, sud_inramfs_op_fcntl_setfl(fd, (int)arg));
    default:
        /* F_GETFD/F_SETFD/locks/etc — pass through to kernel. */
        return 0;
    }
}

static int inramfs_pre_syscall(struct sud_syscall_ctx *ctx)
{
    if (!sud_inramfs_active()) return 0;
    long nr = ctx->nr;

    /* dup family: special destination-scrub semantics. */
#ifdef SYS_dup
    if (nr == SYS_dup) {
        int fd = (int)ctx->args[0];
        if (!sud_inramfs_owns_fd(fd)) return 0;
        return short_circuit(ctx, sud_inramfs_op_dup(fd));
    }
#endif
#ifdef SYS_dup2
    if (nr == SYS_dup2)
        return dispatch_dup_to(ctx, (int)ctx->args[0], (int)ctx->args[1], 0);
#endif
#ifdef SYS_dup3
    if (nr == SYS_dup3)
        return dispatch_dup_to(ctx, (int)ctx->args[0], (int)ctx->args[1],
                               (int)ctx->args[2]);
#endif

    /* fcntl subcommand demux. */
#ifdef SYS_fcntl
    if (nr == SYS_fcntl)   return dispatch_fcntl(ctx);
#endif
#ifdef SYS_fcntl64
    if (nr == SYS_fcntl64) return dispatch_fcntl(ctx);
#endif

    /* Two-path ops (rename/link family). */
#ifdef SYS_rename
    if (nr == SYS_rename)
        return dispatch_two_path(ctx, -1, 0, -1, 1, h_rename);
#endif
#ifdef SYS_renameat
    if (nr == SYS_renameat)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_rename);
#endif
#ifdef SYS_renameat2
    if (nr == SYS_renameat2)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_renameat2);
#endif
#ifdef SYS_link
    if (nr == SYS_link)
        return dispatch_two_path(ctx, -1, 0, -1, 1, h_link);
#endif
#ifdef SYS_linkat
    if (nr == SYS_linkat)
        return dispatch_two_path(ctx,  0, 1,  2, 3, h_link);
#endif

    /* utimensat: a NULL path means "operate on dirfd as if it were
     * an open fd" (futimens semantics), which we don't yet support
     * — fall through.  Otherwise dispatch via the normal path lane. */
#ifdef SYS_utimensat
    if (nr == SYS_utimensat) {
        const char *p = (const char *)ctx->args[1];
        if (!p) return 0;
        const char *abs;
        int r = resolve_path(ctx, (int)ctx->args[0], p, &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx, h_utimensat(ctx, abs));
    }
#endif

    /* Everything else (most fd + path syscalls) is in the table. */
    return dispatch_table(ctx);
}

/* ---- addin lifecycle hooks --------------------------------------- */

static void inramfs_init_hook(void)
{
    sud_inramfs_init();
}

static void inramfs_target_launch(const struct sud_tracee_launch *l)
{
    (void)l;
}

static void inramfs_fork_child(void)
{
    /* fd table is a per-process map.  Child inherits memfds via fork
     * just as it would inherit any other fd; the table contents are
     * still valid for the child.  No reset needed. */
}

const struct sud_addin sud_inramfs_addin = {
    "inramfs",
    inramfs_init_hook,
    inramfs_target_launch,
    inramfs_fork_child,
    inramfs_pre_syscall,
    0   /* no post_syscall hook */
};
