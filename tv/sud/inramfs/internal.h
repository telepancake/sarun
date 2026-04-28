/*
 * sud/inramfs/internal.h — Internal shared types for the inramfs
 * implementation files (super.c, vfs.c, addin.c).  Not part of the
 * public API; consumers outside sud/inramfs should not include it.
 *
 * Layout invariants (since these structures live in a shared mmap'd
 * region accessible from multiple processes):
 *
 *   - All offsets are 32-bit byte offsets relative to the base of the
 *     mapping, NOT raw pointers.  This keeps the shared region
 *     position-independent across attaching processes (we DO place
 *     it at a fixed address in every loader, but using offsets means
 *     a process that fails MAP_FIXED still produces correct compile-
 *     time-checked types).
 *   - All field sizes are explicit (uint32_t, uint64_t, ...).
 *   - Inode and block table sizes are configured at init time; once
 *     init writes the magic word, layout is frozen for the lifetime
 *     of the /dev/shm file.
 *
 * Concurrency:
 *   - sud_ir_super.lock is the *namespace* lock.  Acquire it for any
 *     operation that mutates the directory tree (mkdir/unlink/rename),
 *     allocates an inode, or allocates/frees data blocks.  Read-only
 *     traversal (lookup) does NOT take this lock — it relies on the
 *     append-only nature of inode allocation (an inode's existence
 *     bit, once set, is stable), and on per-dir mutation being done
 *     under sud_ir_super.lock so concurrent readers see a well-formed
 *     directory body.
 *   - Per-inode futex words guard file *content* (read/write/truncate).
 *     They do NOT guard directory bodies — those are guarded by the
 *     namespace lock.
 *   - All futex waits use FUTEX_WAIT (NOT _PRIVATE), since the futex
 *     word lives in shared memory accessed by multiple processes.
 */

#ifndef SUD_INRAMFS_INTERNAL_H
#define SUD_INRAMFS_INTERNAL_H

#include "libc-fs/libc.h"

/* ---- Region layout constants ---------------------------------- */

#define SUD_IR_MAGIC          0x494E5246u   /* "INRF" */
#define SUD_IR_VERSION        1
#define SUD_IR_BLOCK_SIZE     4096u
#define SUD_IR_NAME_MAX       255u
/* Cap on the bookkeeping section so the rest of the region is data. */
#define SUD_IR_MAX_INODES     (1u << 16)    /* 65536 */
/* Per-block bitmap stride (bytes per byte of bitmap). */
#define SUD_IR_BLOCK_GROUP    8u
/* Maximum directory nesting handled by sud_ir_walk; deeper paths
 * return -ENAMETOOLONG instead of overflowing the on-stack parent
 * tracking buffer.  64 is well above any realistic FS tree. */
#define SUD_IR_PARENT_STACK_MAX 64

/* Forward declaration. */
struct sud_ir_super;
struct sud_ir_inode;

/* ---- Inode types ---------------------------------------------- */

/* sud_ir_inode.type values.  Matches S_IFDIR/S_IFREG/S_IFLNK shifted
 * to a small enum so we can sanity-check it without bit-twiddling. */
enum {
    SUD_IR_T_FREE = 0,
    SUD_IR_T_REG  = 1,
    SUD_IR_T_DIR  = 2,
    SUD_IR_T_LNK  = 3,
};

/* ---- Inode ----------------------------------------------------- */

/* Fixed-size inode.  Lives in the inode table at offset
 * sizeof(super) + ino_index * sizeof(sud_ir_inode).  Field order is
 * tuned to keep the on-disk layout deterministic across compilers. */
struct sud_ir_inode {
    /* Type discriminator (one of SUD_IR_T_*).  0 == free. */
    uint32_t type;
    /* Generation counter; incremented on every reallocation of this
     * slot.  Mostly defensive (NFS-style). */
    uint32_t generation;
    /* Standard unix inode attributes. */
    uint32_t mode;       /* permission bits + setuid/setgid/sticky */
    uint32_t uid;
    uint32_t gid;
    uint32_t nlink;      /* hard link count */
    uint64_t size;       /* logical file size */
    uint64_t atime_ns;   /* timestamps in nanoseconds since epoch */
    uint64_t mtime_ns;
    uint64_t ctime_ns;
    /* Per-inode futex word: 0 == unlocked, 1 == locked-no-waiters,
     * 2 == locked-with-waiters.  Used as a non-recursive mutex for
     * file-content ops. */
    uint32_t lock;
    /* Type-specific data.  The largest variant is the inline symlink
     * target buffer. */
    union {
        /* Regular file: backing data is a single contiguous extent.
         * data_block_offset == 0 means "no data extent yet".
         * capacity_bytes is the allocated size (page-multiple);
         * size <= capacity_bytes is the logical end-of-file. */
        struct {
            uint32_t data_block_offset; /* offset into the region */
            uint32_t capacity_bytes;
        } reg;
        /* Directory: dirents stored in a singly-linked chain of
         * dirent blocks (each block is SUD_IR_BLOCK_SIZE bytes,
         * holding a struct sud_ir_dirblock with up to ~62 entries).
         * dirblock_head_offset == 0 for an empty directory.  count
         * caches the total dirent count. */
        struct {
            uint32_t dirblock_head_offset;
            uint32_t dirent_count;
        } dir;
        /* Symlink: target stored in a data block so the inode itself
         * stays small.  target_block_offset == 0 means "empty link"
         * (uncommon); target_len excludes any NUL terminator. */
        struct {
            uint32_t target_block_offset;
            uint32_t target_len;
        } lnk;
    } u;
};

/* Sanity: keep struct stable.  Compilers must NOT add padding that
 * changes layout across builds — all fields are aligned. */

/* ---- Directory entry block ------------------------------------ */

#define SUD_IR_DIRENTS_PER_BLOCK 62

struct sud_ir_dirent {
    /* Inode index (1-based; 0 == empty slot).  We don't reuse slot
     * indices when a dirent is deleted — the dir block compaction
     * is done at remove time so iteration via getdents64 is dense. */
    uint32_t ino_index;
    /* Type hint for getdents64 (DT_REG/DT_DIR/DT_LNK). */
    uint8_t  d_type;
    uint8_t  name_len;          /* 0 means slot empty/last */
    uint8_t  _pad[2];
    char     name[64];          /* up to 63 bytes + NUL */
};

struct sud_ir_dirblock {
    uint32_t next_offset;       /* offset of next block, 0 if last */
    uint32_t used;              /* # of dirents in this block */
    uint32_t _pad[14];          /* align to 64 bytes */
    struct sud_ir_dirent ents[SUD_IR_DIRENTS_PER_BLOCK];
};

/* ---- Superblock ----------------------------------------------- */

struct sud_ir_super {
    uint32_t magic;             /* SUD_IR_MAGIC once initialised */
    uint32_t version;           /* SUD_IR_VERSION */
    uint64_t region_size;       /* total bytes mmap'd */
    /* Init latch: 0 == uninitialised, 1 == initialising, 2 == ready.
     * Loser of the init race futex_waits on this word. */
    uint32_t init_state;
    /* Namespace/allocator lock (cross-process futex). */
    uint32_t lock;
    /* Inode allocator: next inode slot to scan first; the bitmap
     * itself follows the super in the region.  Slot 1 is always
     * the root directory; slot 0 is reserved as "no inode". */
    uint32_t inode_count;       /* size of inode table */
    uint32_t inode_bitmap_off;  /* byte offset to the bitmap */
    uint32_t inode_table_off;   /* byte offset to the inode table */
    /* Block allocator: bitmap of 4 KiB blocks. */
    uint32_t block_count;
    uint32_t block_bitmap_off;
    uint32_t block_data_off;    /* offset of first data block */
    uint32_t next_inode_hint;   /* allocator search hint */
    uint32_t next_block_hint;
    /* Statistics (best-effort, not authoritative). */
    uint32_t inodes_in_use;
    uint32_t blocks_in_use;
};

/* ---- super.c: region access and locking ----------------------- */

/* The base of the shared mapping (NULL until sud_inramfs_init()
 * succeeds).  All offsets in the region are added to this. */
extern volatile char *sud_ir_base;

/* Convert a region offset to a usable pointer (returns NULL for 0). */
static inline void *sud_ir_ptr(uint32_t off)
{
    if (off == 0) return 0;
    return (void *)((char *)sud_ir_base + off);
}

/* Convert a pointer back to a region offset (assumes the pointer
 * lies within the mapping; caller must ensure this). */
static inline uint32_t sud_ir_off(const void *p)
{
    if (!p) return 0;
    return (uint32_t)((const char *)p - (const char *)sud_ir_base);
}

/* Pointer to the live superblock at the base of the region. */
static inline struct sud_ir_super *sud_ir_sb(void)
{
    return (struct sud_ir_super *)sud_ir_base;
}

/* Cross-process futex lock/unlock on a uint32_t word in the shared
 * region.  Implemented in super.c. */
void sud_ir_lock(volatile uint32_t *word);
void sud_ir_unlock(volatile uint32_t *word);

/* Allocate / free a block (4 KiB, page-aligned).  Caller MUST hold
 * sb->lock.  Returns block offset or 0 on out-of-space. */
uint32_t sud_ir_block_alloc(uint32_t nblocks);
void     sud_ir_block_free(uint32_t off, uint32_t nblocks);

/* Allocate / free an inode index.  Caller MUST hold sb->lock.
 * Returns inode index (1-based) or 0 on out-of-space. */
uint32_t sud_ir_inode_alloc(uint32_t type, uint32_t mode,
                            uint32_t uid, uint32_t gid);
void     sud_ir_inode_free(uint32_t index);

/* Look up an inode by 1-based index.  Returns NULL for 0/out-of-
 * range/free indices. */
struct sud_ir_inode *sud_ir_inode_get(uint32_t index);

/* Mount-prefix accessors (set by sud_inramfs_init from SUD_INRAMFS). */
const char *sud_ir_mount_path(void);
size_t      sud_ir_mount_len(void);

/* Current monotonic-ish wall-clock in ns — used to set i_*time. */
uint64_t sud_ir_now_ns(void);

/* ---- vfs.c: filesystem operations ----------------------------- */

/* Walk a path (must be absolute and inside the mount) and return the
 * 1-based inode index of the named entry.  If `follow` is non-zero,
 * trailing symlinks are resolved (with depth-limit ELOOP detection).
 * Returns 0 on failure with *err_out set to -errno. */
uint32_t sud_ir_walk(const char *abs_path, int follow, int *err_out);

/* Walk to the parent dir of `abs_path`, returning that dir's inode
 * index in *parent_out and a pointer to the final basename in
 * *base_out (points into abs_path; not NUL-terminated past the end
 * of the original path).  Returns -errno on failure or 0 on success. */
int sud_ir_walk_parent(const char *abs_path,
                       uint32_t *parent_out, const char **base_out,
                       size_t *base_len_out);

/* Directory namespace ops.  All take the parent dir's inode index
 * and a basename slice (name, name_len).  Caller must hold sb->lock. */
int      sud_ir_dir_lookup(uint32_t dir_idx, const char *name,
                           size_t name_len, uint32_t *child_out);
int      sud_ir_dir_link  (uint32_t dir_idx, const char *name,
                           size_t name_len, uint32_t child_idx,
                           uint8_t d_type);
int      sud_ir_dir_unlink(uint32_t dir_idx, const char *name,
                           size_t name_len, uint32_t *child_out);
int      sud_ir_dir_is_empty(uint32_t dir_idx);

/* File data ops.  Caller does NOT need to hold sb->lock; these take
 * the per-inode lock internally. */
long sud_ir_file_read (struct sud_ir_inode *ino, void *buf,
                       size_t count, off_t off);
long sud_ir_file_write(struct sud_ir_inode *ino, const void *buf,
                       size_t count, off_t off);
long sud_ir_file_truncate(struct sud_ir_inode *ino, off_t length);

/* Fill a `struct stat` (kernel ABI for the running arch) for the
 * given inode.  Implemented in addin.c so the layout is colocated
 * with the per-arch ABI choices in op_fstat/op_stat. */
void sud_inramfs_fill_stat(void *st_buf, uint32_t idx,
                           const struct sud_ir_inode *ino);

#endif /* SUD_INRAMFS_INTERNAL_H */
