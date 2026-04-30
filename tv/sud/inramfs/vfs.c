/*
 * sud/inramfs/vfs.c — VFS operations for the inramfs add-in.
 *
 * Path walking, directory namespace ops, and file data ops.  All
 * operations take the namespace lock (super->lock) for namespace
 * mutations; per-inode locks for file content.  Lookup is read-only
 * and intentionally lock-free w.r.t. the namespace lock — which is
 * sound because:
 *   - dirents are never reordered in place; new entries are appended
 *     to the end of a dir block (or a freshly-allocated next block),
 *     and removed entries are replaced by the last entry under the
 *     namespace lock (compaction);
 *   - inode bitmap bits, once set, name a slot whose inode pointer
 *     is stable (the slot's contents may change generation on free/
 *     realloc, but lookup re-checks the dirent's ino_index);
 *   - readers tolerate a stale "found name X" / "didn't find name X"
 *     against a concurrent unlink/rename in the same way the kernel
 *     VFS does (the result is always *some* point-in-time view).
 *
 * Path walking does NOT cache; every lookup re-walks the tree.  For
 * the workloads inramfs targets (compiler intermediates, build
 * scratch space) the trees are shallow and walk overhead is
 * dominated by syscall handler overhead anyway.
 */

#include "sud/inramfs/inramfs.h"
#include "sud/inramfs/internal.h"
#include "sud/raw.h"

#ifndef EIO
#define EIO 5
#endif

/* ================================================================
 * Internal helpers
 * ================================================================ */

/* Compare a NUL-terminated string against a slice (name, len). */
static int name_eq(const char *a, const char *b, size_t b_len)
{
    for (size_t i = 0; i < b_len; i++) {
        if (a[i] == '\0' || a[i] != b[i]) return 0;
    }
    return a[b_len] == '\0';
}

/* d_type from inode type. */
static uint8_t d_type_from_ino(const struct sud_ir_inode *ino)
{
    switch (ino->type) {
        case SUD_IR_T_REG: return DT_REG;
        case SUD_IR_T_DIR: return DT_DIR;
        case SUD_IR_T_LNK: return DT_LNK;
    }
    return DT_UNKNOWN;
}

/* mode_t-style mode (full S_IF*+perm) from inode. */
static uint32_t full_mode(const struct sud_ir_inode *ino)
{
    uint32_t base = 0;
    switch (ino->type) {
        case SUD_IR_T_REG: base = S_IFREG; break;
        case SUD_IR_T_DIR: base = S_IFDIR; break;
        case SUD_IR_T_LNK: base = S_IFLNK; break;
    }
    return base | (ino->mode & 07777);
}

/* ================================================================
 * Directory namespace operations
 *
 * Each dir is a singly-linked chain of dirblocks.  Add: scan blocks
 * for a free slot, else allocate a new block at the head.  Remove:
 * find the dirent, swap it with the last dirent in the last block,
 * decrement counts, optionally free the now-empty last block.
 *
 * Caller MUST hold sb->lock for sud_ir_dir_link / sud_ir_dir_unlink.
 * sud_ir_dir_lookup is safe without the lock (best-effort point-in-
 * time view).
 * ================================================================ */

int sud_ir_dir_lookup(uint32_t dir_idx, const char *name, size_t name_len,
                      uint32_t *child_out)
{
    struct sud_ir_inode *dir = sud_ir_inode_get(dir_idx);
    if (!dir || dir->type != SUD_IR_T_DIR) return -ENOTDIR;
    if (name_len == 0) return -ENOENT;
    /* "." resolves to self, ".." needs the parent — but parent
     * resolution at this layer is the caller's job (we don't store a
     * parent pointer; ".." in a path is normalised in walk).  Here
     * we treat "." specially. */
    if (name_len == 1 && name[0] == '.') {
        *child_out = dir_idx;
        return 0;
    }

    uint32_t off = dir->u.dir.dirblock_head_offset;
    while (off) {
        struct sud_ir_dirblock *db =
            (struct sud_ir_dirblock *)sud_ir_ptr(off);
        uint32_t used = db->used;
        if (used > SUD_IR_DIRENTS_PER_BLOCK)
            used = SUD_IR_DIRENTS_PER_BLOCK;
        for (uint32_t i = 0; i < used; i++) {
            const struct sud_ir_dirent *de = &db->ents[i];
            if (de->ino_index == 0) continue;
            if (de->name_len != name_len) continue;
            if (memcmp(de->name, name, name_len) == 0
                && de->name[name_len] == '\0') {
                *child_out = de->ino_index;
                return 0;
            }
        }
        off = db->next_offset;
    }
    return -ENOENT;
}

int sud_ir_dir_link(uint32_t dir_idx, const char *name, size_t name_len,
                    uint32_t child_idx, uint8_t d_type)
{
    if (name_len == 0 || name_len > 63) return -ENAMETOOLONG;
    /* Reject "." and ".." — those are handled by walk, not stored. */
    if (name_len == 1 && name[0] == '.') return -EEXIST;
    if (name_len == 2 && name[0] == '.' && name[1] == '.') return -EEXIST;

    struct sud_ir_inode *dir = sud_ir_inode_get(dir_idx);
    if (!dir || dir->type != SUD_IR_T_DIR) return -ENOTDIR;

    /* First check this name isn't already there (callers may not
     * have done so).  This walks the chain; we then walk a second
     * time looking for an insertion slot.  Two passes make the
     * code simpler and dir blocks are bounded. */
    uint32_t exists;
    if (sud_ir_dir_lookup(dir_idx, name, name_len, &exists) == 0)
        return -EEXIST;

    uint32_t off = dir->u.dir.dirblock_head_offset;
    struct sud_ir_dirblock *target = 0;
    while (off) {
        struct sud_ir_dirblock *db =
            (struct sud_ir_dirblock *)sud_ir_ptr(off);
        if (db->used < SUD_IR_DIRENTS_PER_BLOCK) {
            target = db;
            break;
        }
        off = db->next_offset;
    }
    if (!target) {
        /* All blocks are full (or this is the first dirent ever).
         * Allocate a new dirblock and prepend it to the chain. */
        uint32_t new_off = sud_ir_block_alloc(1);
        if (!new_off) return -ENOSPC;
        struct sud_ir_dirblock *db =
            (struct sud_ir_dirblock *)sud_ir_ptr(new_off);
        memset(db, 0, sizeof(*db));
        db->next_offset = dir->u.dir.dirblock_head_offset;
        dir->u.dir.dirblock_head_offset = new_off;
        target = db;
    }

    struct sud_ir_dirent *de = &target->ents[target->used];
    de->ino_index = child_idx;
    de->d_type    = d_type;
    de->name_len  = (uint8_t)name_len;
    memcpy(de->name, name, name_len);
    de->name[name_len] = '\0';
    target->used++;
    dir->u.dir.dirent_count++;

    /* Update parent dir mtime/ctime. */
    uint64_t now = sud_ir_now_ns();
    dir->mtime_ns = now;
    dir->ctime_ns = now;

    /* If linking a directory, increment parent's nlink for its ".."
     * (matches POSIX: nlink == 2 + number of subdirectories). */
    if (d_type == DT_DIR) dir->nlink++;

    return 0;
}

int sud_ir_dir_unlink(uint32_t dir_idx, const char *name, size_t name_len,
                      uint32_t *child_out)
{
    struct sud_ir_inode *dir = sud_ir_inode_get(dir_idx);
    if (!dir || dir->type != SUD_IR_T_DIR) return -ENOTDIR;

    /* Locate the entry. */
    uint32_t off = dir->u.dir.dirblock_head_offset;
    struct sud_ir_dirblock *found_db = 0;
    int found_idx = -1;
    while (off) {
        struct sud_ir_dirblock *db =
            (struct sud_ir_dirblock *)sud_ir_ptr(off);
        for (uint32_t i = 0; i < db->used; i++) {
            struct sud_ir_dirent *de = &db->ents[i];
            if (de->ino_index == 0) continue;
            if (de->name_len == name_len
                && memcmp(de->name, name, name_len) == 0) {
                found_db  = db;
                found_idx = (int)i;
                goto found;
            }
        }
        off = db->next_offset;
    }
    return -ENOENT;
found:
    *child_out = found_db->ents[found_idx].ino_index;
    uint8_t was_dir = (found_db->ents[found_idx].d_type == DT_DIR);

    /* Find the last dirent in the chain (last block, last used slot)
     * and swap it into the freed slot to keep the array dense. */
    uint32_t last_off = dir->u.dir.dirblock_head_offset;
    uint32_t last_prev = 0;
    while (1) {
        struct sud_ir_dirblock *db =
            (struct sud_ir_dirblock *)sud_ir_ptr(last_off);
        if (db->next_offset == 0) break;
        last_prev = last_off;
        last_off = db->next_offset;
    }
    struct sud_ir_dirblock *last_db =
        (struct sud_ir_dirblock *)sud_ir_ptr(last_off);
    if (last_db->used > 0) {
        struct sud_ir_dirent *src = &last_db->ents[last_db->used - 1];
        struct sud_ir_dirent *dst = &found_db->ents[found_idx];
        if (src != dst)
            *dst = *src;
        memset(src, 0, sizeof(*src));
        last_db->used--;
    }
    /* Free trailing empty block (but keep the head, even when empty,
     * to keep dirblock_head_offset consistent for an empty dir).  We
     * only reclaim when the now-empty block is NOT the head. */
    if (last_db->used == 0 && last_off != dir->u.dir.dirblock_head_offset) {
        struct sud_ir_dirblock *prev_db =
            (struct sud_ir_dirblock *)sud_ir_ptr(last_prev);
        prev_db->next_offset = 0;
        sud_ir_block_free(last_off, 1);
    }

    if (dir->u.dir.dirent_count) dir->u.dir.dirent_count--;
    if (was_dir && dir->nlink > 2) dir->nlink--;
    uint64_t now = sud_ir_now_ns();
    dir->mtime_ns = now;
    dir->ctime_ns = now;
    return 0;
}

int sud_ir_dir_is_empty(uint32_t dir_idx)
{
    struct sud_ir_inode *dir = sud_ir_inode_get(dir_idx);
    if (!dir || dir->type != SUD_IR_T_DIR) return 0;
    return dir->u.dir.dirent_count == 0;
}

/* ================================================================
 * Path walking
 *
 * Path is absolute and lies inside the mount.  We strip the mount
 * prefix and then walk components against the root inode (slot 1).
 * ================================================================ */

#define SUD_IR_MAX_SYMLINKS 16

static const char *strip_mount(const char *abs)
{
    const char *m = sud_ir_mount_path();
    size_t mlen = sud_ir_mount_len();
    if (!m) return 0;
    /* Trivial root mount ("/"): everything is under it. */
    if (mlen == 1 && m[0] == '/') return abs + 1;
    if (memcmp(abs, m, mlen) != 0) return 0;
    if (abs[mlen] == '\0') return abs + mlen;
    if (abs[mlen] != '/')   return 0;
    return abs + mlen + 1;
}

/* Walk one path component starting at `dir_idx` and return the
 * child idx.  `name`/`len` is the component slice.  Handles "."
 * and "..". */
static int walk_one(uint32_t cur, const char *name, size_t len,
                    uint32_t *out, uint32_t *parent_stack, int *sp)
{
    if (len == 0) { *out = cur; return 0; }
    if (len == 1 && name[0] == '.') { *out = cur; return 0; }
    if (len == 2 && name[0] == '.' && name[1] == '.') {
        if (*sp > 0) {
            (*sp)--;
            *out = parent_stack[*sp];
        } else {
            /* ".." at root resolves to root (Linux behaviour). */
            *out = cur;
        }
        return 0;
    }
    uint32_t child;
    int rc = sud_ir_dir_lookup(cur, name, len, &child);
    if (rc < 0) return rc;
    /* Bounds check: parent_stack is fixed-size in sud_ir_walk; refuse
     * paths that nest deeper than the stack rather than corrupting
     * memory.  64 levels is well above any realistic FS tree. */
    if (*sp >= SUD_IR_PARENT_STACK_MAX) return -ENAMETOOLONG;
    parent_stack[(*sp)++] = cur;
    *out = child;
    return 0;
}

uint32_t sud_ir_walk(const char *abs_path, int follow, int *err_out)
{
    if (!abs_path || abs_path[0] != '/') { *err_out = -EINVAL; return 0; }
    const char *rel = strip_mount(abs_path);
    if (!rel) { *err_out = -EINVAL; return 0; }

    /* The path may need re-walking if a symlink interposes; outer
     * loop bounds the symlink count. */
    char buf[PATH_MAX];
    size_t buf_used = strlen(rel);
    if (buf_used >= sizeof(buf)) { *err_out = -ENAMETOOLONG; return 0; }
    memcpy(buf, rel, buf_used + 1);
    int sym_left = SUD_IR_MAX_SYMLINKS;

    uint32_t parent_stack[SUD_IR_PARENT_STACK_MAX];
    int sp = 0;
    uint32_t cur = 1;        /* root */

restart:
    sp = 0;
    cur = 1;
    {
        const char *p = buf;
        while (*p) {
            while (*p == '/') p++;
            if (!*p) break;
            const char *start = p;
            while (*p && *p != '/') p++;
            size_t len = (size_t)(p - start);
            if (len > 63) { *err_out = -ENAMETOOLONG; return 0; }

            int last = (*p == '\0' || (*p == '/' && p[1] == '\0'));

            /* For non-last components, current must be a dir we can
             * descend into.  For symlinks anywhere except the
             * trailing position (or always if `follow`), follow. */
            uint32_t next;
            int rc = walk_one(cur, start, len, &next, parent_stack, &sp);
            if (rc < 0) { *err_out = rc; return 0; }

            struct sud_ir_inode *child = sud_ir_inode_get(next);
            if (!child) { *err_out = -ENOENT; return 0; }

            int do_follow = !last || follow;
            if (do_follow && child->type == SUD_IR_T_LNK) {
                if (--sym_left < 0) { *err_out = -ELOOP; return 0; }
                /* Substitute symlink target into the remaining path
                 * (target + remainder of buf after current p). */
                const char *tgt = (const char *)sud_ir_ptr(child->u.lnk.target_block_offset);
                size_t tlen = child->u.lnk.target_len;
                if (!tgt || tlen == 0) { *err_out = -ENOENT; return 0; }
                size_t rem = strlen(p);
                if (tgt[0] == '/') {
                    /* Absolute symlink: must point inside mount. */
                    const char *new_rel = strip_mount(tgt);
                    if (!new_rel) { *err_out = -EXDEV; return 0; }
                    size_t nlen = strlen(new_rel);
                    if (nlen + rem + 1 > sizeof(buf)) {
                        *err_out = -ENAMETOOLONG; return 0;
                    }
                    memmove(buf, new_rel, nlen);
                    memmove(buf + nlen, p, rem + 1);
                    buf_used = nlen + rem;
                } else {
                    /* Relative symlink: replace the just-consumed
                     * component with the symlink target. */
                    size_t prefix = (size_t)(start - buf);
                    if (prefix + tlen + rem + 1 > sizeof(buf)) {
                        *err_out = -ENAMETOOLONG; return 0;
                    }
                    memmove(buf + prefix + tlen, p, rem + 1);
                    memcpy (buf + prefix, tgt, tlen);
                    buf_used = prefix + tlen + rem;
                }
                goto restart;
            }
            cur = next;
        }
    }
    *err_out = 0;
    return cur;
}

int sud_ir_walk_parent(const char *abs_path,
                       uint32_t *parent_out, const char **base_out,
                       size_t *base_len_out)
{
    if (!abs_path || abs_path[0] != '/') return -EINVAL;

    /* Locate the last '/' that separates the basename. */
    size_t L = strlen(abs_path);
    if (L == 0) return -ENOENT;
    /* Strip trailing slashes (POSIX rename/unlink on "foo/" treats it
     * as "foo"; we follow that). */
    while (L > 1 && abs_path[L - 1] == '/') L--;
    size_t i = L;
    while (i > 0 && abs_path[i - 1] != '/') i--;
    /* abs_path[0..i] is parent (with trailing '/'); [i..L) is base. */
    if (i == 0) return -EINVAL;     /* no leading '/' — shouldn't happen */

    /* Build the parent path in a stack buffer, then walk it. */
    char parent[PATH_MAX];
    size_t plen = i;
    /* Strip the trailing '/' unless parent is "/" itself. */
    if (plen > 1) plen--;
    if (plen >= sizeof(parent)) return -ENAMETOOLONG;
    memcpy(parent, abs_path, plen);
    parent[plen] = '\0';

    /* Reject empty basename. */
    if (i == L) return -EISDIR;     /* path is just "/" */

    int err = 0;
    uint32_t pidx = sud_ir_walk(parent, 1 /*follow*/, &err);
    if (!pidx) return err;
    struct sud_ir_inode *p = sud_ir_inode_get(pidx);
    if (!p || p->type != SUD_IR_T_DIR) return -ENOTDIR;

    *parent_out   = pidx;
    *base_out     = abs_path + i;
    *base_len_out = L - i;
    return 0;
}

/* ================================================================
 * File data ops (use per-inode lock, not the namespace lock)
 *
 * Two-tier contiguous-extent storage:
 *
 *   SMALL tier — files of size <= SUD_IR_LARGE_THRESHOLD (128 KiB).
 *     One contiguous run of N blocks in the shared small-file shm,
 *     identified by (start_block, nblocks) in u.reg.u.small.  Block
 *     ids are 1-based; start_block == 0 means "no extent allocated"
 *     (empty file).  Allocator: first-fit bitmap kept in the
 *     metadata shm (see super.c::sud_ir_small_alloc).
 *
 *   LARGE tier — files exceeding the threshold OR files that the
 *     small allocator cannot fit (no contiguous run available).
 *     Each large inode owns its own /dev/shm tmpfs object,
 *     identified by (file_idx, file_gen) in u.reg.u.large.  Per-
 *     process kfd cache lives in super.c::sud_ir_large_open.
 *     The shm is grown via ftruncate as needed; there is no
 *     inramfs-imposed size cap.
 *
 * Promotion SMALL→LARGE happens in file_ensure_capacity when:
 *   (a) the post-grow size would exceed SUD_IR_LARGE_THRESHOLD, OR
 *   (b) the small extent must be enlarged (or initially allocated)
 *       and the small allocator has no contiguous run of the
 *       required length.
 *
 * Promotion is invisible to callers: read/write/truncate dispatch
 * by the inode's tag at every call.  An mmap holding a mapping
 * into a SMALL extent at promotion time is NOT addressed here —
 * the addin's op_mmap promotes BEFORE returning a mapping unless
 * it can prove the file is captive (see addin.c).
 *
 * Per-inode lock is held across read/write/truncate so the file's
 * tag and extent metadata are stable for the duration of one op.
 * The small-file allocator is NOT lock-free — alloc/free take
 * sb->lock briefly — but callers only enter that path on
 * grow/promote/free, not on the steady-state read/write fast path.
 * ================================================================ */

/* SMALL: copy `count` bytes between buf and the file's small extent
 * starting at intra-file offset `off`.  to_file selects direction.
 * Caller holds ino->lock and has verified the range fits within the
 * existing extent. */
static void small_xfer(struct sud_ir_inode *ino, void *buf, size_t count,
                       off_t off, int to_file)
{
    if (count == 0) return;
    uint32_t start = ino->u.reg.u.small.start_block;
    uint64_t shm_off = sud_ir_small_block_offset(start) + (uint64_t)off;
    if (sud_ir_data_base) {
        /* 64-bit fast path: zero-copy memcpy via the mapped shm. */
        char *p = (char *)sud_ir_data_base + shm_off;
        if (to_file) memcpy(p, buf, count);
        else         memcpy(buf, p, count);
    } else {
        /* 32-bit (or any path where the small shm is not mapped):
         * pread/pwrite onto the small-file shm fd. */
        if (to_file) sud_ir_small_pwrite(buf, count, shm_off);
        else         sud_ir_small_pread (buf, count, shm_off);
    }
}

/* LARGE: pread/pwrite onto the per-file shm via the cached kfd. */
static long large_xfer(struct sud_ir_inode *ino, void *buf, size_t count,
                       off_t off, int to_file)
{
    int fd = sud_ir_large_open(ino->u.reg.u.large.file_idx,
                                ino->u.reg.u.large.file_gen);
    if (fd < 0) return fd;
    size_t done = 0;
    while (done < count) {
        long n;
        if (to_file) {
#ifdef SYS_pwrite64
            n = raw_syscall6(SYS_pwrite64, fd,
                             (long)((const char *)buf + done),
                             (long)(count - done),
                             (long)(uint64_t)((uint64_t)off + done), 0, 0);
#else
            n = raw_syscall6(SYS_pwrite, fd,
                             (long)((const char *)buf + done),
                             (long)(count - done),
                             (long)(uint64_t)((uint64_t)off + done), 0, 0);
#endif
        } else {
#ifdef SYS_pread64
            n = raw_syscall6(SYS_pread64, fd,
                             (long)((char *)buf + done),
                             (long)(count - done),
                             (long)(uint64_t)((uint64_t)off + done), 0, 0);
#else
            n = raw_syscall6(SYS_pread, fd,
                             (long)((char *)buf + done),
                             (long)(count - done),
                             (long)(uint64_t)((uint64_t)off + done), 0, 0);
#endif
        }
        if (n < 0) {
            if (n == -EINTR) continue;
            return done ? (long)done : n;
        }
        if (n == 0) {
            /* Read past EOF on the (sparse) per-file shm: zero-fill
             * the remainder for callers that asked for more bytes
             * than physical-pages back the range. */
            if (!to_file) memset((char *)buf + done, 0, count - done);
            else if (!done) return -EIO;
            return (long)(to_file ? done : count);
        }
        done += (size_t)n;
    }
    return (long)done;
}

/* Promote SMALL → LARGE: allocate per-file shm, ftruncate to current
 * size, copy bytes from the small extent to the per-file shm, free
 * the small extent, switch the inode tag.  Caller holds ino->lock. */
static int promote_locked(struct sud_ir_inode *ino)
{
    if (ino->u.reg.tag == SUD_IR_REG_LARGE) return 0;

    uint32_t idx_self;
    {
        /* Recover the inode index from the inode pointer (= its
         * offset into the inode table divided by sizeof(inode)).
         * We need this to compose the per-file shm path. */
        struct sud_ir_super *sb = sud_ir_sb();
        struct sud_ir_inode *table =
            (struct sud_ir_inode *)sud_ir_ptr(sb->inode_table_off);
        idx_self = (uint32_t)(ino - table);
    }
    uint32_t gen = ino->generation;

    /* Materialise the per-file shm at the file's current logical
     * size.  Tmpfs supports sparse files; ftruncate just sets the
     * size, no physical pages allocated until written. */
    int fd = sud_ir_large_open(idx_self, gen);
    if (fd < 0) return fd;
    int rc = sud_ir_large_ftruncate(idx_self, gen, ino->size);
    if (rc) return rc;

    /* Copy the small extent's live bytes into the per-file shm. */
    if (ino->u.reg.u.small.nblocks && ino->size) {
        char tmp[4096];
        uint64_t src_off = sud_ir_small_block_offset(
                               ino->u.reg.u.small.start_block);
        uint64_t remaining = ino->size;
        uint64_t dst = 0;
        while (remaining) {
            size_t chunk = remaining > sizeof(tmp) ? sizeof(tmp)
                                                   : (size_t)remaining;
            if (sud_ir_data_base) {
                memcpy(tmp, (const char *)sud_ir_data_base + src_off + dst,
                       chunk);
            } else {
                long got = sud_ir_small_pread(tmp, chunk, src_off + dst);
                if (got < 0) return (int)got;
                if ((size_t)got != chunk) {
                    /* Sparse hole — zero-fill the gap so the
                     * destination matches the SMALL contents. */
                    memset(tmp + got, 0, chunk - (size_t)got);
                }
            }
            /* pwrite chunk into per-file shm at offset `dst`. */
            size_t wrote = 0;
            while (wrote < chunk) {
#ifdef SYS_pwrite64
                long n = raw_syscall6(SYS_pwrite64, fd, (long)(tmp + wrote),
                                      (long)(chunk - wrote),
                                      (long)(uint64_t)(dst + wrote), 0, 0);
#else
                long n = raw_syscall6(SYS_pwrite, fd, (long)(tmp + wrote),
                                      (long)(chunk - wrote),
                                      (long)(uint64_t)(dst + wrote), 0, 0);
#endif
                if (n < 0) {
                    if (n == -EINTR) continue;
                    return (int)n;
                }
                if (n == 0) return -EIO;
                wrote += (size_t)n;
            }
            dst       += chunk;
            remaining -= chunk;
        }
    }

    /* Free the small extent under sb->lock, then switch tag. */
    if (ino->u.reg.u.small.nblocks) {
        struct sud_ir_super *sb = sud_ir_sb();
        sud_ir_lock(&sb->lock);
        sud_ir_small_free(ino->u.reg.u.small.start_block,
                          ino->u.reg.u.small.nblocks);
        sud_ir_unlock(&sb->lock);
    }
    ino->u.reg.tag = SUD_IR_REG_LARGE;
    ino->u.reg.u.large.file_idx = idx_self;
    ino->u.reg.u.large.file_gen = gen;
    return 0;
}

/* Public promotion entry: takes the per-inode lock around
 * promote_locked.  Used by addin.c::op_mmap to force a SMALL file
 * into the LARGE tier before returning a mapping that the caller
 * might keep alive across a future grow. */
int sud_ir_file_promote(struct sud_ir_inode *ino)
{
    if (ino->type != SUD_IR_T_REG) return -EINVAL;
    sud_ir_lock(&ino->lock);
    int rc = (ino->u.reg.tag == SUD_IR_REG_LARGE) ? 0 : promote_locked(ino);
    sud_ir_unlock(&ino->lock);
    return rc;
}

/* Ensure the SMALL extent has at least `need_blocks` contiguous
 * blocks; if the existing extent is too small, try to relocate to a
 * larger run.  Caller holds ino->lock.  Returns:
 *    0  on success (extent fits need_blocks)
 *   -ENOSPC if no run of size need_blocks exists in the small shm
 *           — caller must promote
 *   <0 errno on copy/IO failure
 *
 * Relocation strategy: allocate a new run of need_blocks, copy the
 * live bytes from the old extent into it, free the old extent.
 * Done under sb->lock + ino->lock (caller's lock); concurrent
 * readers of this file are blocked by ino->lock. */
static int small_extent_grow(struct sud_ir_inode *ino, uint32_t need_blocks)
{
    if (ino->u.reg.u.small.nblocks >= need_blocks) return 0;
    struct sud_ir_super *sb = sud_ir_sb();

    sud_ir_lock(&sb->lock);
    uint32_t new_start = sud_ir_small_alloc(need_blocks);
    sud_ir_unlock(&sb->lock);
    if (new_start == 0) return -ENOSPC;

    /* Copy live bytes from old to new extent (if any). */
    if (ino->u.reg.u.small.start_block && ino->size) {
        uint64_t src = sud_ir_small_block_offset(
                            ino->u.reg.u.small.start_block);
        uint64_t dst = sud_ir_small_block_offset(new_start);
        if (sud_ir_data_base) {
            memcpy((char *)sud_ir_data_base + dst,
                   (const char *)sud_ir_data_base + src,
                   (size_t)ino->size);
        } else {
            char buf[4096];
            uint64_t left = ino->size, off = 0;
            while (left) {
                size_t chunk = left > sizeof(buf) ? sizeof(buf) : (size_t)left;
                long n = sud_ir_small_pread(buf, chunk, src + off);
                if (n < 0) {
                    sud_ir_lock(&sb->lock);
                    sud_ir_small_free(new_start, need_blocks);
                    sud_ir_unlock(&sb->lock);
                    return (int)n;
                }
                if ((size_t)n < chunk) memset(buf + n, 0, chunk - (size_t)n);
                long w = sud_ir_small_pwrite(buf, chunk, dst + off);
                if (w < 0) {
                    sud_ir_lock(&sb->lock);
                    sud_ir_small_free(new_start, need_blocks);
                    sud_ir_unlock(&sb->lock);
                    return (int)w;
                }
                off  += chunk;
                left -= chunk;
            }
        }
    }
    /* Swap in the new extent and free the old. */
    uint32_t old_start = ino->u.reg.u.small.start_block;
    uint32_t old_nb    = ino->u.reg.u.small.nblocks;
    ino->u.reg.u.small.start_block = new_start;
    ino->u.reg.u.small.nblocks     = need_blocks;
    if (old_nb) {
        sud_ir_lock(&sb->lock);
        sud_ir_small_free(old_start, old_nb);
        sud_ir_unlock(&sb->lock);
    }
    return 0;
}

/* Ensure the file can address `need_bytes` (logical EOF or write
 * extent end).  Allocates / extends / promotes as needed.  Caller
 * holds ino->lock.  Never SHRINKS — a write whose end-offset is
 * below the current size must not destroy data past it. */
static int file_ensure_capacity(struct sud_ir_inode *ino, uint64_t need_bytes)
{
    if (need_bytes > 0xffffffffffffull) return -EFBIG;

    /* No growth required. */
    if (need_bytes <= ino->size) return 0;

    /* Already LARGE: just grow the per-file shm. */
    if (ino->u.reg.tag == SUD_IR_REG_LARGE) {
        return sud_ir_large_ftruncate(ino->u.reg.u.large.file_idx,
                                      ino->u.reg.u.large.file_gen,
                                      need_bytes);
    }

    /* SMALL → must promote if past threshold. */
    if (need_bytes > (uint64_t)SUD_IR_LARGE_THRESHOLD) {
        int rc = promote_locked(ino);
        if (rc) return rc;
        return sud_ir_large_ftruncate(ino->u.reg.u.large.file_idx,
                                      ino->u.reg.u.large.file_gen,
                                      need_bytes);
    }

    /* SMALL stays SMALL: grow the contiguous extent if needed.
     * If the small allocator can't satisfy a contiguous run of the
     * required length, escalate to LARGE — the small shm being
     * fragmented is a normal escape path, not an error. */
    uint32_t need_blocks = (uint32_t)((need_bytes + SUD_IR_BLOCK_SIZE - 1)
                                       / SUD_IR_BLOCK_SIZE);
    if (need_blocks == 0) return 0;
    int rc = small_extent_grow(ino, need_blocks);
    if (rc == -ENOSPC) {
        rc = promote_locked(ino);
        if (rc) return rc;
        return sud_ir_large_ftruncate(ino->u.reg.u.large.file_idx,
                                      ino->u.reg.u.large.file_gen,
                                      need_bytes);
    }
    return rc;
}

long sud_ir_file_read(struct sud_ir_inode *ino, void *buf,
                      size_t count, off_t off)
{
    if (off < 0) return -EINVAL;
    if (ino->type != SUD_IR_T_REG) return -EINVAL;
    sud_ir_lock(&ino->lock);
    if ((uint64_t)off >= ino->size) { sud_ir_unlock(&ino->lock); return 0; }
    uint64_t avail = ino->size - (uint64_t)off;
    if ((uint64_t)count > avail) count = (size_t)avail;
    if (count) {
        if (ino->u.reg.tag == SUD_IR_REG_LARGE) {
            long n = large_xfer(ino, buf, count, off, 0);
            if (n < 0) { sud_ir_unlock(&ino->lock); return n; }
            count = (size_t)n;
        } else {
            small_xfer(ino, buf, count, off, 0);
        }
    }
    ino->atime_ns = sud_ir_now_ns();
    sud_ir_unlock(&ino->lock);
    return (long)count;
}

long sud_ir_file_write(struct sud_ir_inode *ino, const void *buf,
                       size_t count, off_t off)
{
    if (off < 0) return -EINVAL;
    if (ino->type != SUD_IR_T_REG) return -EINVAL;
    if (count == 0) return 0;
    sud_ir_lock(&ino->lock);
    uint64_t end = (uint64_t)off + (uint64_t)count;
    int rc = file_ensure_capacity(ino, end);
    if (rc) { sud_ir_unlock(&ino->lock); return rc; }
    if (ino->u.reg.tag == SUD_IR_REG_LARGE) {
        long n = large_xfer(ino, (void *)buf, count, off, 1);
        if (n < 0) { sud_ir_unlock(&ino->lock); return n; }
        count = (size_t)n;
    } else {
        small_xfer(ino, (void *)buf, count, off, 1);
    }
    if (end > ino->size) ino->size = end;
    uint64_t now = sud_ir_now_ns();
    ino->mtime_ns = now;
    ino->ctime_ns = now;
    sud_ir_unlock(&ino->lock);
    return (long)count;
}

long sud_ir_file_truncate(struct sud_ir_inode *ino, off_t length)
{
    if (length < 0) return -EINVAL;
    if (ino->type != SUD_IR_T_REG) return -EINVAL;
    sud_ir_lock(&ino->lock);
    uint64_t newsz = (uint64_t)length;
    int rc = 0;
    if (newsz > ino->size) {
        rc = file_ensure_capacity(ino, newsz);
        if (rc) { sud_ir_unlock(&ino->lock); return rc; }
        /* Hole between old size and new size reads as zeros — small
         * shm pages were zero-filled by the kernel on first fault;
         * per-file shm pages likewise; explicit zeroing not needed. */
    } else if (newsz < ino->size) {
        if (ino->u.reg.tag == SUD_IR_REG_LARGE) {
            rc = sud_ir_large_ftruncate(ino->u.reg.u.large.file_idx,
                                        ino->u.reg.u.large.file_gen,
                                        newsz);
            if (rc) { sud_ir_unlock(&ino->lock); return rc; }
        } else {
            /* SMALL shrink: keep the existing extent (may be larger
             * than strictly needed); zero the freed tail so a later
             * truncate-up exposes zeros, not stale data. */
            if (ino->u.reg.u.small.nblocks && ino->size > newsz) {
                uint64_t base = sud_ir_small_block_offset(
                                   ino->u.reg.u.small.start_block);
                uint64_t off  = base + newsz;
                uint64_t len  = ino->size - newsz;
                if (sud_ir_data_base) {
                    memset((char *)sud_ir_data_base + off, 0, (size_t)len);
                } else {
                    /* Best-effort zero on 32-bit: write zeros via fd. */
                    static const char zero[4096];
                    while (len) {
                        size_t chunk = len > sizeof(zero) ? sizeof(zero)
                                                          : (size_t)len;
                        sud_ir_small_pwrite(zero, chunk, off);
                        off += chunk;
                        len -= chunk;
                    }
                }
            }
            /* Shrink the small extent if it's significantly larger
             * than needed — release blocks back to the bitmap.
             * Threshold: drop trailing blocks that are entirely
             * past `newsz`. */
            uint32_t need_blocks = (uint32_t)((newsz + SUD_IR_BLOCK_SIZE - 1)
                                              / SUD_IR_BLOCK_SIZE);
            if (need_blocks < ino->u.reg.u.small.nblocks) {
                struct sud_ir_super *sb = sud_ir_sb();
                uint32_t free_from = ino->u.reg.u.small.start_block
                                    + need_blocks;
                uint32_t free_n    = ino->u.reg.u.small.nblocks - need_blocks;
                sud_ir_lock(&sb->lock);
                sud_ir_small_free(free_from, free_n);
                sud_ir_unlock(&sb->lock);
                ino->u.reg.u.small.nblocks = need_blocks;
                if (need_blocks == 0) ino->u.reg.u.small.start_block = 0;
            }
        }
    }
    ino->size = newsz;
    uint64_t now = sud_ir_now_ns();
    ino->mtime_ns = now;
    ino->ctime_ns = now;
    sud_ir_unlock(&ino->lock);
    return 0;
}

/* ================================================================
 * Public op: stat (writes a kernel-ABI struct stat into st_buf).
 * ================================================================ */

/* We construct the stat layout manually so callers can build either
 * 32-bit or 64-bit struct stat without including kernel headers. */
#if defined(__x86_64__)
struct sud_ir_kstat {
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
struct sud_ir_kstat {                /* matches stat64 on i386 */
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

static void fill_stat(struct sud_ir_kstat *st, uint32_t idx,
                      const struct sud_ir_inode *ino)
{
    memset(st, 0, sizeof(*st));
#if defined(__x86_64__)
    st->st_dev   = 0;
    st->st_ino   = idx;
    st->st_nlink = ino->nlink;
    st->st_mode  = full_mode(ino);
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
    st->st_dev   = 0;
    st->__st_ino = idx;
    st->st_ino   = idx;
    st->st_nlink = ino->nlink;
    st->st_mode  = full_mode(ino);
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

long sud_inramfs_op_stat(const char *abs_path, void *st_buf, int follow)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, follow, &err);
    if (!idx) return err;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;
    fill_stat((struct sud_ir_kstat *)st_buf, idx, ino);
    return 0;
}

/* ================================================================
 * Public op: namespace mutators
 * ================================================================ */

static long create_at(const char *abs_path, uint32_t type, uint32_t mode,
                      const char *symlink_target, uint32_t *out_idx)
{
    uint32_t pidx;
    const char *base;
    size_t blen;
    int rc = sud_ir_walk_parent(abs_path, &pidx, &base, &blen);
    if (rc) return rc;
    if (blen == 0 || blen > 63) return -ENAMETOOLONG;
    /* Refuse special component names. */
    if (blen == 1 && base[0] == '.') return -EEXIST;
    if (blen == 2 && base[0] == '.' && base[1] == '.') return -EEXIST;

    struct sud_ir_super *sb = sud_ir_sb();
    sud_ir_lock(&sb->lock);
    uint32_t exists;
    if (sud_ir_dir_lookup(pidx, base, blen, &exists) == 0) {
        sud_ir_unlock(&sb->lock);
        return -EEXIST;
    }
    /* Allocate inode. */
    uint32_t uid = 0, gid = 0;
#ifdef SYS_getuid
    uid = (uint32_t)raw_syscall6(SYS_getuid, 0, 0, 0, 0, 0, 0);
#endif
#ifdef SYS_getgid
    gid = (uint32_t)raw_syscall6(SYS_getgid, 0, 0, 0, 0, 0, 0);
#endif
    uint32_t new_idx = sud_ir_inode_alloc(type, mode & 07777, uid, gid);
    if (!new_idx) { sud_ir_unlock(&sb->lock); return -ENOSPC; }
    struct sud_ir_inode *new_ino = sud_ir_inode_get(new_idx);
    if (type == SUD_IR_T_DIR) {
        new_ino->nlink = 2;     /* "." and the link from parent */
    } else {
        new_ino->nlink = 1;
    }
    if (type == SUD_IR_T_LNK && symlink_target) {
        size_t tlen = strlen(symlink_target);
        if (tlen >= PATH_MAX) {
            sud_ir_inode_free(new_idx);
            sud_ir_unlock(&sb->lock);
            return -ENAMETOOLONG;
        }
        /* Allocate a single block to hold the target string.  Symlink
         * targets are bounded by PATH_MAX which fits in one 4 KiB
         * block, so we never need a multi-block extent here. */
        uint32_t tgt_off = sud_ir_block_alloc(1);
        if (!tgt_off) {
            sud_ir_inode_free(new_idx);
            sud_ir_unlock(&sb->lock);
            return -ENOSPC;
        }
        memcpy(sud_ir_ptr(tgt_off), symlink_target, tlen + 1);
        new_ino->u.lnk.target_block_offset = tgt_off;
        new_ino->u.lnk.target_len = (uint32_t)tlen;
        new_ino->size = tlen;
    }
    uint8_t dt = (type == SUD_IR_T_DIR) ? DT_DIR
                : (type == SUD_IR_T_LNK) ? DT_LNK
                : DT_REG;
    rc = sud_ir_dir_link(pidx, base, blen, new_idx, dt);
    if (rc) {
        sud_ir_inode_free(new_idx);
        sud_ir_unlock(&sb->lock);
        return rc;
    }
    if (out_idx) *out_idx = new_idx;
    sud_ir_unlock(&sb->lock);
    return 0;
}

/* Returns 1 if abs_path is exactly the mount root (after stripping
 * any trailing slashes).  Used by op_mkdir/op_rmdir/op_unlink to
 * give the right errno when an operation targets the mount root
 * itself — its parent is "/", which is outside the mount, so a
 * naive walk_parent would surface -EINVAL where Linux returns
 * -EEXIST/-EBUSY/-EISDIR. */
static int is_mount_root(const char *abs_path)
{
    if (!abs_path) return 0;
    const char *m = sud_ir_mount_path();
    size_t mlen = sud_ir_mount_len();
    if (!m) return 0;
    size_t L = strlen(abs_path);
    while (L > 1 && abs_path[L - 1] == '/') L--;
    return L == mlen && memcmp(abs_path, m, mlen) == 0;
}

long sud_inramfs_op_mkdir(const char *abs_path, int mode)
{
    if (is_mount_root(abs_path)) return -EEXIST;
    return create_at(abs_path, SUD_IR_T_DIR, (uint32_t)(mode & 07777), 0, 0);
}

long sud_inramfs_op_symlink(const char *target, const char *abs_linkpath)
{
    if (!target) return -EINVAL;
    return create_at(abs_linkpath, SUD_IR_T_LNK, 0777, target, 0);
}

long sud_inramfs_op_readlink(const char *abs_path, char *buf, size_t bufsz)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, 0 /*don't follow last*/, &err);
    if (!idx) return err;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;
    if (ino->type != SUD_IR_T_LNK) return -EINVAL;
    const char *tgt = (const char *)sud_ir_ptr(ino->u.lnk.target_block_offset);
    size_t tlen = ino->u.lnk.target_len;
    if (!tgt) return 0;
    if (tlen > bufsz) tlen = bufsz;
    memcpy(buf, tgt, tlen);
    return (long)tlen;
}

long sud_inramfs_op_unlink(const char *abs_path)
{
    if (is_mount_root(abs_path)) return -EISDIR;
    uint32_t pidx;
    const char *base;
    size_t blen;
    int rc = sud_ir_walk_parent(abs_path, &pidx, &base, &blen);
    if (rc) return rc;
    struct sud_ir_super *sb = sud_ir_sb();
    sud_ir_lock(&sb->lock);
    uint32_t cidx;
    rc = sud_ir_dir_lookup(pidx, base, blen, &cidx);
    if (rc) { sud_ir_unlock(&sb->lock); return rc; }
    struct sud_ir_inode *child = sud_ir_inode_get(cidx);
    if (!child) { sud_ir_unlock(&sb->lock); return -ENOENT; }
    if (child->type == SUD_IR_T_DIR) {
        sud_ir_unlock(&sb->lock);
        return -EISDIR;
    }
    rc = sud_ir_dir_unlink(pidx, base, blen, &cidx);
    if (rc) { sud_ir_unlock(&sb->lock); return rc; }
    if (child->nlink) child->nlink--;
    if (child->nlink == 0) sud_ir_inode_free(cidx);
    sud_ir_unlock(&sb->lock);
    return 0;
}

long sud_inramfs_op_rmdir(const char *abs_path)
{
    if (is_mount_root(abs_path)) return -EBUSY;
    uint32_t pidx;
    const char *base;
    size_t blen;
    int rc = sud_ir_walk_parent(abs_path, &pidx, &base, &blen);
    if (rc) return rc;
    /* Refuse "." and "..". */
    if ((blen == 1 && base[0] == '.')
        || (blen == 2 && base[0] == '.' && base[1] == '.'))
        return -EINVAL;
    struct sud_ir_super *sb = sud_ir_sb();
    sud_ir_lock(&sb->lock);
    uint32_t cidx;
    rc = sud_ir_dir_lookup(pidx, base, blen, &cidx);
    if (rc) { sud_ir_unlock(&sb->lock); return rc; }
    struct sud_ir_inode *child = sud_ir_inode_get(cidx);
    if (!child || child->type != SUD_IR_T_DIR) {
        sud_ir_unlock(&sb->lock); return -ENOTDIR;
    }
    if (!sud_ir_dir_is_empty(cidx)) {
        sud_ir_unlock(&sb->lock); return -ENOTEMPTY;
    }
    rc = sud_ir_dir_unlink(pidx, base, blen, &cidx);
    if (rc) { sud_ir_unlock(&sb->lock); return rc; }
    sud_ir_inode_free(cidx);
    sud_ir_unlock(&sb->lock);
    return 0;
}

long sud_inramfs_op_link(const char *abs_oldpath, const char *abs_newpath)
{
    int err = 0;
    uint32_t src = sud_ir_walk(abs_oldpath, 0, &err);
    if (!src) return err;
    struct sud_ir_inode *src_ino = sud_ir_inode_get(src);
    if (!src_ino) return -ENOENT;
    if (src_ino->type == SUD_IR_T_DIR) return -EPERM;

    uint32_t pidx;
    const char *base;
    size_t blen;
    int rc = sud_ir_walk_parent(abs_newpath, &pidx, &base, &blen);
    if (rc) return rc;
    struct sud_ir_super *sb = sud_ir_sb();
    sud_ir_lock(&sb->lock);
    uint32_t exists;
    if (sud_ir_dir_lookup(pidx, base, blen, &exists) == 0) {
        sud_ir_unlock(&sb->lock);
        return -EEXIST;
    }
    uint8_t dt = (src_ino->type == SUD_IR_T_LNK) ? DT_LNK : DT_REG;
    rc = sud_ir_dir_link(pidx, base, blen, src, dt);
    if (rc) { sud_ir_unlock(&sb->lock); return rc; }
    src_ino->nlink++;
    src_ino->ctime_ns = sud_ir_now_ns();
    sud_ir_unlock(&sb->lock);
    return 0;
}

long sud_inramfs_op_rename(const char *abs_oldpath,
                           const char *abs_newpath, unsigned int flags)
{
    /* Only support flags == 0 in the initial cut (no RENAME_EXCHANGE,
     * no RENAME_NOREPLACE).  Flag-zero matches plain rename(2). */
    if (flags) return -EINVAL;

    uint32_t old_par, new_par;
    const char *old_base, *new_base;
    size_t old_blen, new_blen;
    int rc;
    rc = sud_ir_walk_parent(abs_oldpath, &old_par, &old_base, &old_blen);
    if (rc) return rc;
    rc = sud_ir_walk_parent(abs_newpath, &new_par, &new_base, &new_blen);
    if (rc) return rc;

    struct sud_ir_super *sb = sud_ir_sb();
    sud_ir_lock(&sb->lock);

    uint32_t src;
    rc = sud_ir_dir_lookup(old_par, old_base, old_blen, &src);
    if (rc) { sud_ir_unlock(&sb->lock); return rc; }
    struct sud_ir_inode *src_ino = sud_ir_inode_get(src);
    if (!src_ino) { sud_ir_unlock(&sb->lock); return -ENOENT; }

    /* If a destination exists, it must be replaceable.  Directory
     * onto directory only if dst is empty; non-dir onto non-dir
     * always; dir onto non-dir or vice versa is ENOTDIR/EISDIR. */
    uint32_t dst;
    int dst_existed = (sud_ir_dir_lookup(new_par, new_base, new_blen, &dst) == 0);
    if (dst_existed) {
        struct sud_ir_inode *dst_ino = sud_ir_inode_get(dst);
        if (dst == src) { sud_ir_unlock(&sb->lock); return 0; }
        if (!dst_ino) { sud_ir_unlock(&sb->lock); return -ENOENT; }
        if (src_ino->type == SUD_IR_T_DIR && dst_ino->type != SUD_IR_T_DIR) {
            sud_ir_unlock(&sb->lock); return -ENOTDIR;
        }
        if (src_ino->type != SUD_IR_T_DIR && dst_ino->type == SUD_IR_T_DIR) {
            sud_ir_unlock(&sb->lock); return -EISDIR;
        }
        if (dst_ino->type == SUD_IR_T_DIR && !sud_ir_dir_is_empty(dst)) {
            sud_ir_unlock(&sb->lock); return -ENOTEMPTY;
        }
        /* Unlink dst. */
        uint32_t tmp;
        rc = sud_ir_dir_unlink(new_par, new_base, new_blen, &tmp);
        if (rc) { sud_ir_unlock(&sb->lock); return rc; }
        if (dst_ino->nlink) dst_ino->nlink--;
        if (dst_ino->type == SUD_IR_T_DIR) {
            /* Directory unlink takes 2 references off (.. removed). */
            dst_ino->nlink = 0;
        }
        if (dst_ino->nlink == 0) sud_ir_inode_free(dst);
    }

    /* Unlink src from old_par, link into new_par. */
    uint32_t tmp;
    rc = sud_ir_dir_unlink(old_par, old_base, old_blen, &tmp);
    if (rc) { sud_ir_unlock(&sb->lock); return rc; }
    uint8_t dt = (src_ino->type == SUD_IR_T_DIR) ? DT_DIR
               : (src_ino->type == SUD_IR_T_LNK) ? DT_LNK : DT_REG;
    rc = sud_ir_dir_link(new_par, new_base, new_blen, src, dt);
    if (rc) {
        /* Best-effort restore. */
        sud_ir_dir_link(old_par, old_base, old_blen, src, dt);
        sud_ir_unlock(&sb->lock);
        return rc;
    }
    src_ino->ctime_ns = sud_ir_now_ns();
    sud_ir_unlock(&sb->lock);
    return 0;
}

long sud_inramfs_op_truncate(const char *abs_path, off_t length)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, 1, &err);
    if (!idx) return err;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;
    if (ino->type == SUD_IR_T_DIR) return -EISDIR;
    if (ino->type != SUD_IR_T_REG) return -EINVAL;
    return sud_ir_file_truncate(ino, length);
}

long sud_inramfs_op_chmod(const char *abs_path, int mode)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, 1, &err);
    if (!idx) return err;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;
    ino->mode = (uint32_t)(mode & 07777);
    ino->ctime_ns = sud_ir_now_ns();
    return 0;
}

long sud_inramfs_op_chown(const char *abs_path, int uid, int gid, int follow)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, follow, &err);
    if (!idx) return err;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;
    if (uid != -1) ino->uid = (uint32_t)uid;
    if (gid != -1) ino->gid = (uint32_t)gid;
    ino->ctime_ns = sud_ir_now_ns();
    return 0;
}

long sud_inramfs_op_utimensat(const char *abs_path,
                              const struct timespec ts[2], int follow)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, follow, &err);
    if (!idx) return err;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;
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

long sud_inramfs_op_access(const char *abs_path, int mode)
{
    (void)mode;     /* we don't enforce permissions in initial cut */
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, 1, &err);
    if (!idx) return err;
    return 0;
}

/* chdir(2) validation: confirm `abs_path` exists in the inramfs and
 * is a directory.  Caller (the addin) handles the bookkeeping —
 * stashing the path as the logical CWD for relative-path resolution
 * — so we just resolve and check. */
long sud_inramfs_op_chdir(const char *abs_path)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, 1, &err);
    if (!idx) return err;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;
    if (ino->type != SUD_IR_T_DIR) return -ENOTDIR;
    return 0;
}

/* Return a real kernel fd backing the file at `abs_path`.  Used by
 * the ELF loader (sud/loader.c) and any other consumer that needs to
 * hand the kernel a real fd it can pread/mmap from for an inramfs
 * file.  SMALL files don't have an individual kernel fd (they live as
 * runs in the shared smalldata shm), so promote them to LARGE first.
 * The caller owns the returned fd (open it fresh — sud_ir_large_open
 * caches its fd internally and would conflict on close). */
long sud_inramfs_op_get_kfd(const char *abs_path)
{
    int err = 0;
    uint32_t idx = sud_ir_walk(abs_path, 1, &err);
    if (!idx) return err;
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    if (!ino) return -ENOENT;
    if (ino->type != SUD_IR_T_REG) return -EISDIR;

    /* Promote SMALL → LARGE so a per-file kernel fd exists. */
    if (ino->u.reg.tag == SUD_IR_REG_SMALL) {
        int rc = sud_ir_file_promote(ino);
        if (rc) return rc;
    }
    char p[PATH_MAX];
    sud_ir_large_path(ino->u.reg.u.large.file_idx,
                      ino->u.reg.u.large.file_gen,
                      p, sizeof(p));
    long fd = raw_syscall6(SYS_openat, AT_FDCWD, (long)p,
                           O_RDONLY | O_CLOEXEC, 0, 0, 0);
    return fd;
}
