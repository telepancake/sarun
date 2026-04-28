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
    /* Fill stat using the same logic as sud_inramfs_op_stat (but
     * without re-walking).  fill_stat is private to vfs.c, so we
     * synthesize via op_stat-like rebuild: use the public stat with
     * a synthesised abs path won't work since we don't track paths.
     * So replicate the layout here. */
    extern void sud_inramfs_fill_stat(void *st, uint32_t idx,
                                      const struct sud_ir_inode *ino);
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
    if (!ino->u.reg.data_block_offset) {
        /* Empty file — nothing to map.  Return an anonymous read-only
         * mapping of the requested length so callers like
         * mmap()-then-fault behave sensibly. */
        long r = (long)raw_mmap(addr, length, prot,
                                flags | MAP_ANONYMOUS, -1, 0);
        if ((unsigned long)r >= (unsigned long)-4095) {
            *err = (int)-r;
            return MAP_FAILED;
        }
        return (void *)r;
    }
    if (offset < 0 || (uint64_t)offset + length > ino->u.reg.capacity_bytes) {
        *err = EINVAL;
        return MAP_FAILED;
    }
    /* The shm region was mapped at (sud_ir_base) starting from offset
     * 0 of the shm file.  Our data extent starts at byte
     * data_block_offset within the region.  We can simply hand back a
     * pointer into the existing mapping for MAP_SHARED reads/writes
     * — the caller's view of these bytes is exactly the inramfs
     * view.  This trivially gives them aligned file data without
     * requiring a second mmap call.
     *
     * Honour MAP_FIXED by validating that addr matches our pointer
     * (otherwise we'd silently give them a non-fixed location). */
    void *p = (char *)sud_ir_ptr(ino->u.reg.data_block_offset) + offset;
    if ((flags & MAP_FIXED) && addr && addr != p) {
        *err = EINVAL;
        return MAP_FAILED;
    }
    (void)prot;     /* the underlying mapping is RW; PROT_READ-only
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

/* statx is unimplemented in the initial cut — we return -ENOSYS so
 * callers that fall back to fstatat get the right answer. */
long sud_inramfs_op_statx(const char *abs_path, int follow,
                          unsigned int mask, void *statx_buf)
{
    (void)abs_path; (void)follow; (void)mask; (void)statx_buf;
    return -ENOSYS;
}

/* ================================================================
 * Pre-syscall dispatch
 * ================================================================ */

static int short_circuit(struct sud_syscall_ctx *ctx, long ret)
{
    ctx->ret = ret;
    return 1;
}

/* Resolve (dirfd, path) into an inramfs absolute path in the scratch
 * buffer.  Returns:
 *   0 on success — `*resolved_out` points into ctx->scratch
 *  -1 if path is not under the mount (caller falls through)
 *  -errno on a hard error that must be returned to the program */
static int resolve_into_scratch(struct sud_syscall_ctx *ctx,
                                int dirfd, const char *path,
                                const char **resolved_out)
{
    if (!ctx->scratch || ctx->scratch_size < PATH_MAX) return -1;
    int rc = sud_inramfs_resolve_at(dirfd, path,
                                    ctx->scratch, ctx->scratch_size);
    if (rc < 0) return rc;
    *resolved_out = ctx->scratch;
    return 0;
}

static int dispatch_path1(struct sud_syscall_ctx *ctx, int path_idx,
                          long (*op)(const char *))
{
    const char *abs;
    int r = resolve_into_scratch(ctx, AT_FDCWD,
                                 (const char *)ctx->args[path_idx], &abs);
    if (r < 0) return (r == -1) ? 0 : short_circuit(ctx, r);
    return short_circuit(ctx, op(abs));
}

static int inramfs_pre_syscall(struct sud_syscall_ctx *ctx)
{
    if (!sud_inramfs_active()) return 0;
    long nr = ctx->nr;

    /* ---- fd-based ops: hijack only if WE own the fd. -------------- */
#ifdef SYS_read
    if (nr == SYS_read && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx, sud_inramfs_op_read((int)ctx->args[0],
                                                     (void *)ctx->args[1],
                                                     (size_t)ctx->args[2]));
    }
#endif
#ifdef SYS_write
    if (nr == SYS_write && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx, sud_inramfs_op_write((int)ctx->args[0],
                                                      (const void *)ctx->args[1],
                                                      (size_t)ctx->args[2]));
    }
#endif
#ifdef SYS_pread64
    if (nr == SYS_pread64 && sud_inramfs_owns_fd((int)ctx->args[0])) {
#if defined(__x86_64__)
        off_t off = (off_t)ctx->args[3];
#else
        off_t off = (off_t)((uint64_t)(uint32_t)ctx->args[3]
                            | ((uint64_t)(uint32_t)ctx->args[4] << 32));
#endif
        return short_circuit(ctx, sud_inramfs_op_pread((int)ctx->args[0],
                                                      (void *)ctx->args[1],
                                                      (size_t)ctx->args[2],
                                                      off));
    }
#endif
#ifdef SYS_pwrite64
    if (nr == SYS_pwrite64 && sud_inramfs_owns_fd((int)ctx->args[0])) {
#if defined(__x86_64__)
        off_t off = (off_t)ctx->args[3];
#else
        off_t off = (off_t)((uint64_t)(uint32_t)ctx->args[3]
                            | ((uint64_t)(uint32_t)ctx->args[4] << 32));
#endif
        return short_circuit(ctx, sud_inramfs_op_pwrite((int)ctx->args[0],
                                                       (const void *)ctx->args[1],
                                                       (size_t)ctx->args[2],
                                                       off));
    }
#endif
#ifdef SYS_lseek
    if (nr == SYS_lseek && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx, sud_inramfs_op_lseek((int)ctx->args[0],
                                                      (off_t)ctx->args[1],
                                                      (int)ctx->args[2]));
    }
#endif
#ifdef SYS_close
    if (nr == SYS_close && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx, sud_inramfs_op_close((int)ctx->args[0]));
    }
#endif
#ifdef SYS_ftruncate
    if (nr == SYS_ftruncate && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx,
            sud_inramfs_op_ftruncate((int)ctx->args[0], (off_t)ctx->args[1]));
    }
#endif
#ifdef SYS_ftruncate64
    if (nr == SYS_ftruncate64 && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx,
            sud_inramfs_op_ftruncate((int)ctx->args[0], (off_t)ctx->args[1]));
    }
#endif
#ifdef SYS_fstat
    if (nr == SYS_fstat && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx,
            sud_inramfs_op_fstat((int)ctx->args[0], (void *)ctx->args[1]));
    }
#endif
#ifdef SYS_fstat64
    if (nr == SYS_fstat64 && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx,
            sud_inramfs_op_fstat((int)ctx->args[0], (void *)ctx->args[1]));
    }
#endif
#ifdef SYS_fchmod
    if (nr == SYS_fchmod && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx,
            sud_inramfs_op_fchmod((int)ctx->args[0], (int)ctx->args[1]));
    }
#endif
#ifdef SYS_fchown
    if (nr == SYS_fchown && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx,
            sud_inramfs_op_fchown((int)ctx->args[0], (int)ctx->args[1],
                                  (int)ctx->args[2]));
    }
#endif
#ifdef SYS_getdents64
    if (nr == SYS_getdents64 && sud_inramfs_owns_fd((int)ctx->args[0])) {
        return short_circuit(ctx,
            sud_inramfs_op_getdents64((int)ctx->args[0],
                                      (void *)ctx->args[1],
                                      (size_t)ctx->args[2]));
    }
#endif
#ifdef SYS_mmap
    if (nr == SYS_mmap && sud_inramfs_owns_fd((int)ctx->args[4])) {
        int err = 0;
        void *p = sud_inramfs_op_mmap((void *)ctx->args[0],
                                      (size_t)ctx->args[1],
                                      (int)ctx->args[2],
                                      (int)ctx->args[3],
                                      (int)ctx->args[4],
                                      (off_t)ctx->args[5],
                                      &err);
        if (p == MAP_FAILED) return short_circuit(ctx, -err);
        return short_circuit(ctx, (long)p);
    }
#endif
#ifdef SYS_mmap2
    if (nr == SYS_mmap2 && sud_inramfs_owns_fd((int)ctx->args[4])) {
        int err = 0;
        void *p = sud_inramfs_op_mmap((void *)ctx->args[0],
                                      (size_t)ctx->args[1],
                                      (int)ctx->args[2],
                                      (int)ctx->args[3],
                                      (int)ctx->args[4],
                                      (off_t)ctx->args[5] << MINI_MMAP2_SHIFT,
                                      &err);
        if (p == MAP_FAILED) return short_circuit(ctx, -err);
        return short_circuit(ctx, (long)p);
    }
#endif

    /* ---- Path-bearing syscalls: hijack only if path is under mount. */
#ifdef SYS_openat
    if (nr == SYS_openat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;          /* not under mount */
        if (r < 0)   return short_circuit(ctx, r);
        long fd = sud_inramfs_op_open(abs, (int)ctx->args[2], (int)ctx->args[3]);
        return short_circuit(ctx, fd);
    }
#endif
#ifdef SYS_open
    if (nr == SYS_open) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        long fd = sud_inramfs_op_open(abs, (int)ctx->args[1], (int)ctx->args[2]);
        return short_circuit(ctx, fd);
    }
#endif

    /* stat family */
#define DO_STAT_AT(SYSNR, follow_default) \
    if (nr == SYSNR) { \
        const char *abs; \
        int dirfd = (int)ctx->args[0]; \
        const char *p = (const char *)ctx->args[1]; \
        int flags = (int)ctx->args[3]; \
        int follow = (flags & AT_SYMLINK_NOFOLLOW) ? 0 : (follow_default); \
        int r = resolve_into_scratch(ctx, dirfd, p, &abs); \
        if (r == -1) return 0; \
        if (r < 0)   return short_circuit(ctx, r); \
        return short_circuit(ctx, \
            sud_inramfs_op_stat(abs, (void *)ctx->args[2], follow)); \
    }
#ifdef SYS_newfstatat
    DO_STAT_AT(SYS_newfstatat, 1)
#endif
#ifdef SYS_fstatat64
    DO_STAT_AT(SYS_fstatat64, 1)
#endif

#ifdef SYS_stat
    if (nr == SYS_stat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_stat(abs, (void *)ctx->args[1], 1));
    }
#endif
#ifdef SYS_lstat
    if (nr == SYS_lstat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_stat(abs, (void *)ctx->args[1], 0));
    }
#endif
#ifdef SYS_stat64
    if (nr == SYS_stat64) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_stat(abs, (void *)ctx->args[1], 1));
    }
#endif
#ifdef SYS_lstat64
    if (nr == SYS_lstat64) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_stat(abs, (void *)ctx->args[1], 0));
    }
#endif

#ifdef SYS_access
    if (nr == SYS_access) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_access(abs, (int)ctx->args[1]));
    }
#endif
#ifdef SYS_faccessat
    if (nr == SYS_faccessat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_access(abs, (int)ctx->args[2]));
    }
#endif
#ifdef SYS_faccessat2
    if (nr == SYS_faccessat2) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_access(abs, (int)ctx->args[2]));
    }
#endif

#ifdef SYS_mkdir
    if (nr == SYS_mkdir) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_mkdir(abs, (int)ctx->args[1]));
    }
#endif
#ifdef SYS_mkdirat
    if (nr == SYS_mkdirat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_mkdir(abs, (int)ctx->args[2]));
    }
#endif

#ifdef SYS_rmdir
    if (nr == SYS_rmdir) return dispatch_path1(ctx, 0, sud_inramfs_op_rmdir);
#endif
#ifdef SYS_unlink
    if (nr == SYS_unlink) return dispatch_path1(ctx, 0, sud_inramfs_op_unlink);
#endif
#ifdef SYS_unlinkat
    if (nr == SYS_unlinkat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        long rc = (ctx->args[2] & AT_REMOVEDIR)
                  ? sud_inramfs_op_rmdir(abs)
                  : sud_inramfs_op_unlink(abs);
        return short_circuit(ctx, rc);
    }
#endif

#ifdef SYS_symlink
    if (nr == SYS_symlink) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_symlink((const char *)ctx->args[0], abs));
    }
#endif
#ifdef SYS_symlinkat
    if (nr == SYS_symlinkat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[1],
                                     (const char *)ctx->args[2], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_symlink((const char *)ctx->args[0], abs));
    }
#endif
#ifdef SYS_readlink
    if (nr == SYS_readlink) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_readlink(abs, (char *)ctx->args[1],
                                    (size_t)ctx->args[2]));
    }
#endif
#ifdef SYS_readlinkat
    if (nr == SYS_readlinkat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_readlink(abs, (char *)ctx->args[2],
                                    (size_t)ctx->args[3]));
    }
#endif

#ifdef SYS_chmod
    if (nr == SYS_chmod) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_chmod(abs, (int)ctx->args[1]));
    }
#endif
#ifdef SYS_fchmodat
    if (nr == SYS_fchmodat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_chmod(abs, (int)ctx->args[2]));
    }
#endif
#ifdef SYS_chown
    if (nr == SYS_chown) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_chown(abs, (int)ctx->args[1], (int)ctx->args[2], 1));
    }
#endif
#ifdef SYS_lchown
    if (nr == SYS_lchown) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_chown(abs, (int)ctx->args[1], (int)ctx->args[2], 0));
    }
#endif
#ifdef SYS_fchownat
    if (nr == SYS_fchownat) {
        const char *abs;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        int follow = ((int)ctx->args[4] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
        return short_circuit(ctx,
            sud_inramfs_op_chown(abs, (int)ctx->args[2], (int)ctx->args[3],
                                 follow));
    }
#endif

#ifdef SYS_truncate
    if (nr == SYS_truncate) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_truncate(abs, (off_t)ctx->args[1]));
    }
#endif
#ifdef SYS_truncate64
    if (nr == SYS_truncate64) {
        const char *abs;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        return short_circuit(ctx,
            sud_inramfs_op_truncate(abs, (off_t)ctx->args[1]));
    }
#endif

#ifdef SYS_utimensat
    if (nr == SYS_utimensat) {
        const char *abs;
        int dirfd = (int)ctx->args[0];
        const char *p = (const char *)ctx->args[1];
        if (!p) return 0;             /* fd-targeted utimensat — TODO */
        int r = resolve_into_scratch(ctx, dirfd, p, &abs);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        int follow = ((int)ctx->args[3] & AT_SYMLINK_NOFOLLOW) ? 0 : 1;
        return short_circuit(ctx,
            sud_inramfs_op_utimensat(abs,
                                     (const struct timespec *)ctx->args[2],
                                     follow));
    }
#endif

#ifdef SYS_rename
    if (nr == SYS_rename) {
        char abs2[PATH_MAX];
        const char *abs1;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs1);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        /* Save abs1 since the second resolve will reuse the scratch.
         * Need a stable copy — copy out into abs2's tail half. */
        size_t l1 = strlen(abs1);
        if (l1 + 1 > sizeof(abs2)) return short_circuit(ctx, -ENAMETOOLONG);
        memcpy(abs2, abs1, l1 + 1);
        const char *abs1_stable = abs2;
        const char *abs2p;
        r = sud_inramfs_resolve_at(AT_FDCWD,
                                   (const char *)ctx->args[1],
                                   ctx->scratch, ctx->scratch_size);
        if (r < 0) {
            /* dst not under mount — cross-fs rename, refuse. */
            return short_circuit(ctx, -EXDEV);
        }
        abs2p = ctx->scratch;
        return short_circuit(ctx,
            sud_inramfs_op_rename(abs1_stable, abs2p, 0));
    }
#endif
#ifdef SYS_renameat
    if (nr == SYS_renameat) {
        char saved[PATH_MAX];
        const char *abs1;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs1);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        size_t l1 = strlen(abs1);
        if (l1 + 1 > sizeof(saved)) return short_circuit(ctx, -ENAMETOOLONG);
        memcpy(saved, abs1, l1 + 1);
        r = sud_inramfs_resolve_at((int)ctx->args[2],
                                   (const char *)ctx->args[3],
                                   ctx->scratch, ctx->scratch_size);
        if (r < 0) return short_circuit(ctx, -EXDEV);
        return short_circuit(ctx,
            sud_inramfs_op_rename(saved, ctx->scratch, 0));
    }
#endif
#ifdef SYS_renameat2
    if (nr == SYS_renameat2) {
        char saved[PATH_MAX];
        const char *abs1;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs1);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        size_t l1 = strlen(abs1);
        if (l1 + 1 > sizeof(saved)) return short_circuit(ctx, -ENAMETOOLONG);
        memcpy(saved, abs1, l1 + 1);
        r = sud_inramfs_resolve_at((int)ctx->args[2],
                                   (const char *)ctx->args[3],
                                   ctx->scratch, ctx->scratch_size);
        if (r < 0) return short_circuit(ctx, -EXDEV);
        return short_circuit(ctx,
            sud_inramfs_op_rename(saved, ctx->scratch,
                                  (unsigned int)ctx->args[4]));
    }
#endif
#ifdef SYS_link
    if (nr == SYS_link) {
        char saved[PATH_MAX];
        const char *abs1;
        int r = resolve_into_scratch(ctx, AT_FDCWD,
                                     (const char *)ctx->args[0], &abs1);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        size_t l1 = strlen(abs1);
        if (l1 + 1 > sizeof(saved)) return short_circuit(ctx, -ENAMETOOLONG);
        memcpy(saved, abs1, l1 + 1);
        r = sud_inramfs_resolve_at(AT_FDCWD,
                                   (const char *)ctx->args[1],
                                   ctx->scratch, ctx->scratch_size);
        if (r < 0) return short_circuit(ctx, -EXDEV);
        return short_circuit(ctx,
            sud_inramfs_op_link(saved, ctx->scratch));
    }
#endif
#ifdef SYS_linkat
    if (nr == SYS_linkat) {
        char saved[PATH_MAX];
        const char *abs1;
        int r = resolve_into_scratch(ctx, (int)ctx->args[0],
                                     (const char *)ctx->args[1], &abs1);
        if (r == -1) return 0;
        if (r < 0)   return short_circuit(ctx, r);
        size_t l1 = strlen(abs1);
        if (l1 + 1 > sizeof(saved)) return short_circuit(ctx, -ENAMETOOLONG);
        memcpy(saved, abs1, l1 + 1);
        r = sud_inramfs_resolve_at((int)ctx->args[2],
                                   (const char *)ctx->args[3],
                                   ctx->scratch, ctx->scratch_size);
        if (r < 0) return short_circuit(ctx, -EXDEV);
        return short_circuit(ctx,
            sud_inramfs_op_link(saved, ctx->scratch));
    }
#endif
    return 0;
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
