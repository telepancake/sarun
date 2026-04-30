/*
 * sud/inramfs/addin.c — SUD addin glue for the in-RAM filesystem.
 *
 * Acts as the syscall-dispatch front-end for the inramfs core
 * (super.c, vfs.c).  For every path-bearing syscall we:
 *   1. Ask path_remap to compute an absolute path from (dirfd, path)
 *      and check whether that path lies under the configured inramfs
 *      mount  (sud_pr_resolve_at_inramfs).
 *   2. If yes, run the in-process op, set ctx->ret to the result,
 *      and return 1 to short-circuit the kernel call.
 *   3. If no, return 0 so the next addin (path_remap) gets a turn.
 *
 * For fd-bearing syscalls (read/write/lseek/ftruncate/fstat/...) we
 * check sud_inramfs_owns_fd() — fds returned by inramfs are real
 * memfd fds that the kernel knows about (so close/dup/poll work
 * naturally), but read/write/seek/etc. against those fds must be
 * intercepted to read from inramfs's own data extents.
 *
 * After the Part-1 re-layering (PLAN.md), inramfs holds NO path /
 * cwd / dirfd state of its own.  All such state is owned by
 * sud/path_remap/path.{c,h}, which inramfs queries via the
 * sud_pr_* API.  In particular:
 *   - The inramfs mount prefix is owned by path_remap (parsed from
 *     --remap-rule inramfs:<path>).
 *   - The logical-CWD shadow (chdir/getcwd/fchdir bookkeeping) is
 *     owned by path_remap.  This file no longer hooks chdir, fchdir
 *     or getcwd.
 *   - The dirfd → absolute-path map is owned by path_remap.  This
 *     file registers an entry whenever sud_inramfs_op_open returns
 *     a fd for a directory inode.
 *
 * The fd table is process-local (file descriptors don't survive
 * exec across the wrapper boundary in the initial implementation —
 * documented limitation).
 */

#include "sud/inramfs/inramfs.h"
#include "sud/inramfs/internal.h"
#include "sud/path_remap/path.h"
#include "sud/addin.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"

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
    /* For directory fds: the absolute path used to open the fd is
     * registered with path_remap's dirfd table (sud_pr_dirfd_register)
     * in sud_inramfs_op_open and forgotten in sud_inramfs_op_close.
     * That single shared table is consulted by sud_pr_absolutise()
     * for any subsequent (dirfd, relpath) syscall, so this struct
     * carries no path itself. */
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
    /* Drop any path_remap dirfd entry; harmless no-op for non-dirs. */
    sud_pr_dirfd_forget(fd);
}

static int try_adopt_inherited_fd(int fd);   /* defined below */

int sud_inramfs_owns_fd(int fd)
{
    if (fdtab_lookup(fd)) return 1;
    if (try_adopt_inherited_fd(fd)) return 1;
    return 0;
}

/* Lazy adoption of inramfs memfds inherited across exec.
 *
 * Each open in this addin allocates a memfd named "sud-inramfs-<idx>"
 * and registers (kfd → inode_idx) in g_fdtab.  The memfd itself
 * survives exec when it's been dup'd onto a non-CLOEXEC fd (e.g.
 * shell redirect: `cmd > file` ⇒ fd 1 = inramfs memfd in cmd).
 * After exec, the memfd is still open in the kernel but our
 * process-local g_fdtab is empty — so reads/writes/copy_file_range
 * on that fd would skip the inramfs handlers, hit the kernel
 * directly, and operate on the empty memfd backing instead of the
 * real content in the data shm.
 *
 * Detect this by readlink-ing /proc/self/fd/N: a memfd's link
 * target is "/memfd:sud-inramfs-<idx> (deleted)".  When we recognise
 * the marker we fdtab_alloc the fd back into our table.
 *
 * Called lazily — the first time someone asks "does inramfs own
 * fd N?".  Result is cached in g_fdtab so subsequent lookups are
 * O(1) again.  Negative results are tracked in a small probed-fd
 * bitmap so we don't readlink unknown host fds repeatedly. */
#define SUD_IR_FD_PROBE_MAX 1024
static unsigned char g_fd_probed[SUD_IR_FD_PROBE_MAX / 8];

static int fd_was_probed(int fd)
{
    if (fd < 0 || fd >= SUD_IR_FD_PROBE_MAX) return 1;   /* don't probe */
    return (g_fd_probed[fd >> 3] >> (fd & 7)) & 1u;
}
static void fd_mark_probed(int fd)
{
    if (fd < 0 || fd >= SUD_IR_FD_PROBE_MAX) return;
    g_fd_probed[fd >> 3] |= (unsigned char)(1u << (fd & 7));
}

static int try_adopt_inherited_fd(int fd)
{
    if (fd < 0) return 0;
    if (fd_was_probed(fd)) return 0;
    fd_mark_probed(fd);

    /* Build "/proc/self/fd/<fd>". */
    char p[64];
    int n = snprintf(p, sizeof(p), "/proc/self/fd/%d", fd);
    if (n <= 0 || n >= (int)sizeof(p)) return 0;

    char buf[128];
    long r = raw_syscall6(SYS_readlinkat, AT_FDCWD, (long)p,
                          (long)buf, (long)(sizeof(buf) - 1), 0, 0);
    if (r <= 0) return 0;
    buf[r] = '\0';

    /* memfd link looks like "/memfd:sud-inramfs-<idx> (deleted)".
     * Older kernels: "/memfd:sud-inramfs-<idx>". */
    static const char marker[] = "/memfd:sud-inramfs-";
    const size_t mlen = sizeof(marker) - 1;
    if ((size_t)r < mlen || memcmp(buf, marker, mlen) != 0) return 0;

    /* Parse the inode index. */
    const char *q = buf + mlen;
    uint32_t idx = 0;
    int any = 0;
    while (*q >= '0' && *q <= '9') {
        idx = idx * 10u + (uint32_t)(*q - '0');
        q++; any = 1;
    }
    if (!any || idx == 0) return 0;
    /* Validate against the inode table — guard against bogus links
     * (e.g. a user crafting a memfd with the same name).  An invalid
     * idx would be silently absorbed and could cause later UB. */
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return 0;

    /* Adopt: register fd in fdtab.  flags=O_RDWR is the safe
     * superset; we don't actually enforce read/write distinction
     * locally on inramfs fds (the underlying memfd's own perms
     * gate the kernel-side access for the tools that talk to the
     * kfd directly, e.g. fstat, dup, fcntl). */
    fdtab_alloc(fd, idx, O_RDWR);
    return 1;
}

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
    sud_pr_dirfd_forget(fd);
}

/* ================================================================
 * Path resolution lives in sud/path_remap/path.{c,h} now.
 *
 * The CWD shadow, dirfd→logical-path table, absolutise() and the
 * inramfs mount-prefix knowledge that used to live here have all
 * moved to sud/path_remap/path.c (see PLAN.md Part 1).  Callers that
 * need to resolve a (dirfd, path) into an absolute path under the
 * inramfs mount, or test whether a path is under the mount, call
 * sud_pr_resolve_at_inramfs() / sud_pr_inramfs_path_under_mount()
 * directly.  No shim is provided here.
 * ================================================================ */

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

/* Open a known-existing inode and return a fresh kfd.  Performs the
 * type/access checks and O_TRUNC, allocates a memfd for the kfd,
 * registers the fd in the local table, and (for directories)
 * publishes the abs_path to the shared dirfd registry so subsequent
 * *at(dirfd, relpath) syscalls resolve back into inramfs.  abs_path
 * may be NULL when the caller doesn't have one (no dirfd
 * registration is performed in that case). */
long sud_inramfs_op_open_inode(uint32_t inode_idx, int flags,
                               const char *abs_path)
{
    struct sud_ir_inode *ino = sud_ir_inode_get(inode_idx);
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

    int kfd = alloc_kfd(inode_idx);
    if (kfd < 0) return kfd;
    struct sud_ir_open_file *of = fdtab_alloc(kfd, inode_idx, flags);
    if (!of) {
        raw_close(kfd);
        return -EMFILE;
    }
    /* For directory fds, register the absolute path with path_remap's
     * shared dirfd table so subsequent *at(dirfd, relpath) syscalls
     * (in any addin) resolve correctly via this fd.  Non-dir opens
     * skip the registration — they're not valid base dirs anyway and
     * a dirfd lookup on them would surface -EXDEV → kernel pass-
     * through, which the kernel turns into a sensible -ENOTDIR. */
    if (ino->type == SUD_IR_T_DIR && abs_path) {
        sud_pr_dirfd_register(kfd, abs_path);
    }
    /* O_APPEND is honoured at write time via flags. */
    return kfd;
}

/* Create a regular file under the given parent dir and open it.
 * Mirrors the O_CREAT half of open(2): if the entry already exists
 * (race with another opener) we use it (subject to O_EXCL).
 * abs_path is for dirfd registration only (NULL skips it). */
long sud_inramfs_op_create_open_inode(uint32_t parent_idx,
                                      const char *name, size_t name_len,
                                      int flags, int mode,
                                      const char *abs_path)
{
    if (name_len == 0 || name_len > 63) return -ENAMETOOLONG;
    uint32_t idx = 0;
    struct sud_ir_super *sb = sud_ir_sb();
    sud_ir_lock(&sb->lock);
    uint32_t exists;
    if (sud_ir_dir_lookup(parent_idx, name, name_len, &exists) == 0) {
        /* Race: another opener won.  O_EXCL means caller required
         * creation, so refuse. */
        if (flags & O_EXCL) {
            sud_ir_unlock(&sb->lock);
            return -EEXIST;
        }
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
        int lrc = sud_ir_dir_link(parent_idx, name, name_len, ni, DT_REG);
        if (lrc) {
            sud_ir_inode_free(ni);
            sud_ir_unlock(&sb->lock);
            return lrc;
        }
        idx = ni;
    }
    sud_ir_unlock(&sb->lock);
    return sud_inramfs_op_open_inode(idx, flags, abs_path);
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
        /* Create regular file via the parent + basename helper. */
        uint32_t pidx;
        const char *base;
        size_t blen;
        int rc = sud_ir_walk_parent(abs_path, &pidx, &base, &blen);
        if (rc) return rc;
        return sud_inramfs_op_create_open_inode(pidx, base, blen,
                                                flags, mode, abs_path);
    }

    return sud_inramfs_op_open_inode(idx, flags, abs_path);
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
 * mmap — map the file's data into the caller's address space.
 *
 * Two-tier dispatch:
 *
 *   LARGE  → forward mmap to the per-file shm's kfd at `offset`.
 *            Any aligned (offset, length) is supported with full
 *            MAP_SHARED semantics; the per-file shm is the actual
 *            backing object.
 *
 *   SMALL  → promote the file to LARGE first, then take the LARGE
 *            path above.  This guarantees that a future grow of
 *            the file (which would relocate the small extent) does
 *            not silently invalidate the caller's mapping — the
 *            per-file shm grows in place via ftruncate, no mapping
 *            move is ever required.  Cost: a one-time copy of the
 *            file's <=128 KiB body into the per-file shm.  This is
 *            cheap and avoids the cross-process map-rewrite
 *            machinery that a captive-mmap fast path would need.
 *
 * Failure modes are real-OS errors (out of physical RAM, fd-table
 * exhaustion, etc.) — we never return synthetic errors like
 * "couldn't pin a small file" because such an error would arise
 * only from us being too lazy to relocate properly.
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

    /* Empty file with no extent: hand back an anonymous mapping of
     * the requested length so mmap()-then-fault programs behave
     * sensibly. */
    if (ino->size == 0
        && ino->u.reg.tag == SUD_IR_REG_SMALL
        && ino->u.reg.u.small.start_block == 0) {
        long r = (long)raw_mmap(addr, length, prot,
                                flags | MAP_ANONYMOUS, -1, 0);
        if ((unsigned long)r >= (unsigned long)-4095) {
            *err = (int)-r;
            return MAP_FAILED;
        }
        return (void *)r;
    }

    /* Promote SMALL → LARGE so the caller's mapping is backed by a
     * file that can grow in place via ftruncate without ever
     * needing to relocate. */
    if (ino->u.reg.tag == SUD_IR_REG_SMALL) {
        int rc = sud_ir_file_promote(ino);
        if (rc) { *err = -rc; return MAP_FAILED; }
    }

    /* LARGE: forward mmap to the per-file shm's kfd at `offset`. */
    int kfd = sud_ir_large_open(ino->u.reg.u.large.file_idx,
                                ino->u.reg.u.large.file_gen);
    if (kfd < 0) { *err = -kfd; return MAP_FAILED; }
    long r = (long)raw_mmap(addr, length, prot, flags, kfd, offset);
    if ((unsigned long)r >= (unsigned long)-4095) {
        *err = (int)-r;
        return MAP_FAILED;
    }
    return (void *)r;
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
long sud_inramfs_op_statx_fill_inode(uint32_t inode_idx,
                                     unsigned int mask, void *statx_buf)
{
    (void)mask;
    struct sud_ir_inode *ino = sud_ir_inode_get(inode_idx);
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
    stx->stx_ino     = inode_idx;
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

long sud_inramfs_op_statx_fill(const char *abs_path, int follow,
                               unsigned int mask, void *statx_buf)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, follow ? 1 : 0, &err);
    if (!idx) return err ? err : -ENOENT;
    return sud_inramfs_op_statx_fill_inode(idx, mask, statx_buf);
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
        sud_pr_dirfd_forget(newkfd);
    }
    for (int i = 0; i < SUD_IR_FD_TABLE_SIZE; i++) {
        if (g_fdtab[i].kfd == -1) {
            g_fdtab[i].kfd        = newkfd;
            g_fdtab[i].inode_idx  = src->inode_idx;
            g_fdtab[i].generation = src->generation;
            g_fdtab[i].pos        = src->pos;
            g_fdtab[i].flags      = src->flags;
            g_fdtab[i].dir_cookie = src->dir_cookie;
            /* Inherit the abs-path used to open the source dirfd, if
             * any.  Without this, dup'd directory fds (e.g. those
             * minted by fdopendir, which is what `rm -rf` and many
             * ftw-style traversals use) would lose their path and
             * cause subsequent unlinkat/openat-relative ops on the
             * dup to fail with EXDEV from sud_pr_absolutise().  The
             * dirfd table is owned by path_remap; mirror the entry
             * there. */
            const char *src_path = sud_pr_dirfd_lookup(src->kfd);
            if (src_path) sud_pr_dirfd_register(newkfd, src_path);
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
 * Pre-syscall dispatch — fd-bearing syscalls only.
 *
 * After PLAN.md Part 1, all PATH-bearing dispatch
 * (open/stat/mkdir/unlink/symlink/readlink/chmod/chown/utimensat/
 * truncate/rename/link/...) lives in sud/path_remap/inramfs_glue.c
 * and is driven by path_remap before this addin is consulted.
 *
 * What remains here:
 *
 *   - The fd-bearing syscall table (read/write/lseek/ftruncate/
 *     fstat/fchmod/fchown/getdents64/mmap/fsync/close), each row
 *     gated on sud_inramfs_owns_fd(fd).
 *   - dup family (dup/dup2/dup3) — special destination-scrub
 *     semantics that path_remap can't express.
 *   - fcntl subcommand demux (F_DUPFD, F_DUPFD_CLOEXEC, F_GETFL, F_SETFL).
 *   - munmap of our shared shm regions (must be a no-op so the
 *     addin keeps working after the traced program "unmaps" them).
 *   - copy_file_range/sendfile/splice refusal (force userland
 *     fallback on inramfs fds; their backing memfds are empty).
 * ================================================================ */

static int short_circuit(struct sud_syscall_ctx *ctx, long ret)
{
    ctx->ret = ret;
    return 1;
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

/* ---- fd handlers — one per syscall ----------------------------- */

typedef long (*ir_fd_handler)(struct sud_syscall_ctx *ctx, int fd);

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

/* ---- fd dispatch table ----------------------------------------- */

struct ir_fd_row {
    long           nr;
    signed char    fd_idx;
    ir_fd_handler  fd_h;
};

#define ROW_FD(SYSNR, FDIDX, H)  { SYSNR, (FDIDX), (H) }

static const struct ir_fd_row ir_fd_dispatch[] = {
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
};

#define IR_FD_DISPATCH_LEN \
    ((int)(sizeof(ir_fd_dispatch)/sizeof(ir_fd_dispatch[0])))

static int dispatch_fd_table(struct sud_syscall_ctx *ctx)
{
    long nr = ctx->nr;
    for (int i = 0; i < IR_FD_DISPATCH_LEN; i++) {
        const struct ir_fd_row *row = &ir_fd_dispatch[i];
        if (row->nr != nr) continue;
        int fd = (int)ctx->args[row->fd_idx];
        if (!sud_inramfs_owns_fd(fd)) return 0;
        return short_circuit(ctx, row->fd_h(ctx, fd));
    }
    return 0;
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

/* (execve path-rewriting was tried here and removed: under sud the
 * kernel only ever directly execs sud32/sud64.  An execve("/ir/foo",
 * argv, envp) issued by a traced program is rewritten by handler.c's
 * build_exec_argv into execve("<self_exe>", ["sud{32,64}", "/ir/foo",
 * ...], envp), and sud{32,64} then reads "/ir/foo" via syscalls that
 * the inramfs addin intercepts.  The previous /proc/self/fd/N memfd
 * trick bypassed sud entirely and destroyed exec tracing — see
 * sud/elf.c for the fix that lets build_exec_argv classify inramfs
 * binaries correctly.) */

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

    /* munmap interception.
     *
     * The metadata mapping AND (on 64-bit) the small-file shm
     * mapping are shared backing regions used by the addin itself.
     * If the traced program munmaps a range inside one of them, the
     * kernel obediently tears it out of this process's address
     * space — but the addin still relies on those mappings being
     * present for every subsequent inramfs operation.
     *
     * Treat any munmap range that lies wholly inside one of our
     * shared regions as a no-op: the user thinks they unmapped
     * their copy, our backing region stays intact.  Per-file LARGE
     * shm mappings are NOT intercepted here — those are real
     * per-file mappings the user got from op_mmap and is free to
     * tear down.  Partial overlap with one of our regions falls
     * through to the kernel; that boundary case is rare.
     */
#ifdef SYS_munmap
    if (nr == SYS_munmap) {
        unsigned long a = (unsigned long)ctx->args[0];
        unsigned long l = (unsigned long)ctx->args[1];
        unsigned long meta_b = (unsigned long)sud_ir_base;
        unsigned long data_b = (unsigned long)sud_ir_data_base;
        unsigned long meta_sz = (unsigned long)sud_ir_meta_size();
        unsigned long data_sz = (unsigned long)sud_ir_small_size();
        if ((meta_b && a >= meta_b && a + l <= meta_b + meta_sz) ||
            (data_b && a >= data_b && a + l <= data_b + data_sz)) {
            return short_circuit(ctx, 0);
        }
    }
#endif

    /* Zero-copy fast paths (copy_file_range / sendfile / splice).
     *
     * The kernel implements these in terms of the underlying file's
     * page cache.  Our inramfs files are backed by *empty* memfds —
     * the real content lives in the inramfs data shm and is only
     * surfaced via the addin's read/write/pread/pwrite.  If we
     * forwarded copy_file_range to the kernel, it would dutifully
     * copy 0 bytes from one empty memfd to another and report
     * success.  This silently corrupts workloads like coreutils
     * `cat` (which uses copy_file_range as its primary path).
     *
     * Force the userland fallback by returning -EXDEV when any
     * involved fd is inramfs-owned.  -EXDEV is the documented
     * "different filesystems / not supported" errno that GNU
     * coreutils, busybox, and similar tools handle by retrying via
     * read/write.  Wholly host-fd calls fall through to the kernel
     * unchanged.
     */
#ifdef SYS_copy_file_range
    if (nr == SYS_copy_file_range) {
        int in_fd  = (int)ctx->args[0];
        int out_fd = (int)ctx->args[2];
        if (sud_inramfs_owns_fd(in_fd) || sud_inramfs_owns_fd(out_fd)) {
            return short_circuit(ctx, -ENOSYS);
        }
    }
#endif
#ifdef SYS_sendfile
    if (nr == SYS_sendfile) {
        int out_fd = (int)ctx->args[0];
        int in_fd  = (int)ctx->args[1];
        if (sud_inramfs_owns_fd(in_fd) || sud_inramfs_owns_fd(out_fd))
            return short_circuit(ctx, -EINVAL);
    }
#endif
#ifdef SYS_sendfile64
    if (nr == SYS_sendfile64) {
        int out_fd = (int)ctx->args[0];
        int in_fd  = (int)ctx->args[1];
        if (sud_inramfs_owns_fd(in_fd) || sud_inramfs_owns_fd(out_fd))
            return short_circuit(ctx, -EINVAL);
    }
#endif
#ifdef SYS_splice
    if (nr == SYS_splice) {
        int in_fd  = (int)ctx->args[0];
        int out_fd = (int)ctx->args[2];
        if (sud_inramfs_owns_fd(in_fd) || sud_inramfs_owns_fd(out_fd))
            return short_circuit(ctx, -EINVAL);
    }
#endif

    /* Path-bearing dispatch (open/stat/mkdir/unlink/rename/...) is
     * driven by path_remap before this hook runs (see
     * sud/path_remap/inramfs_glue.c).  Everything left is a
     * fd-bearing op handled by the table below. */
    return dispatch_fd_table(ctx);
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
