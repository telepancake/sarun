/*
 * sud/inramfs/super.c — Shared backing region, allocator, and
 * cross-process futex locks for the inramfs add-in.
 *
 * The backing region is a /dev/shm file mapped MAP_SHARED|MAP_FIXED
 * at a high fixed address by every sud loader.  All add-in state
 * (super, bitmaps, inode table, data blocks) lives inside the
 * region; pointer addresses are computed by adding region offsets
 * to sud_ir_base.  See internal.h for the layout.
 */

#include "sud/inramfs/inramfs.h"
#include "sud/inramfs/internal.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"

#ifndef MAP_NORESERVE
#define MAP_NORESERVE  0x4000
#endif
#ifndef EIO
#define EIO 5
#endif

/* ================================================================
 * Module state
 * ================================================================ */

volatile char *sud_ir_base;          /* base of the metadata mapping */
volatile char *sud_ir_data_base;     /* base of the small-file shm mapping
                                      * (NULL on 32-bit — never mapped) */

/* Mount config (parsed once from SUD_INRAMFS). */
static char   g_mount_path[PATH_MAX];
static size_t g_mount_len;
static size_t g_meta_size;           /* metadata region bytes (user-sized) */
static int    g_init_done;
static int    g_active;              /* 1 once attach succeeded */
static char   g_meta_shm_path[PATH_MAX];
static char   g_small_shm_path[PATH_MAX];
static char   g_shm_key[64];         /* key portion shared between shms;
                                      * used to compose per-file shm
                                      * paths in sud_ir_large_open. */
static int    g_small_fd = -1;       /* small-file shm fd; kept open
                                      * across the lifetime of the
                                      * addin for hole-punch and (on
                                      * 32-bit) pread/pwrite access. */

/* ================================================================
 * Public accessors
 * ================================================================ */

const char *sud_ir_mount_path(void) { return g_mount_len ? g_mount_path : 0; }
size_t      sud_ir_mount_len (void) { return g_mount_len; }
int         sud_inramfs_active(void) { return g_active; }
size_t      sud_ir_meta_size (void) { return g_meta_size; }
size_t      sud_ir_small_size(void)
{
    /* Returns the *mapped* size on 64-bit (the addin's munmap
     * interceptor uses this to detect munmap-of-our-region).  On
     * 32-bit the small shm is never mapped, so report 0 (no
     * interception possible / required). */
    if (!sud_ir_data_base) return 0;
    struct sud_ir_super *sb = sud_ir_sb();
    return sb ? (size_t)sb->small_shm_size : 0;
}

uint64_t sud_ir_now_ns(void)
{
    struct timespec ts = { 0, 0 };
    raw_clock_gettime(CLOCK_REALTIME, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* ================================================================
 * Cross-process futex lock
 *
 * Standard 3-state mutex: 0 unlocked, 1 locked-no-waiters, 2 locked-
 * with-waiters.  Word lives in MAP_SHARED memory, so wakeups must
 * use the non-private FUTEX_WAKE op (FUTEX_PRIVATE_FLAG would only
 * see waiters in our own process).
 * ================================================================ */

static long sys_futex(volatile uint32_t *uaddr, int op, uint32_t val)
{
#ifdef SYS_futex
    return raw_syscall6(SYS_futex, (long)uaddr, op, val, 0, 0, 0);
#else
    (void)uaddr; (void)op; (void)val;
    return -ENOSYS;
#endif
}

void sud_ir_lock(volatile uint32_t *word)
{
    /* Fast path: 0 → 1. */
    uint32_t expected = 0;
    if (__atomic_compare_exchange_n(word, &expected, 1u,
                                    0, __ATOMIC_ACQUIRE,
                                    __ATOMIC_RELAXED))
        return;
    /* Contended: spin a few times before going to the kernel. */
    for (int i = 0; i < 64; i++) {
        expected = 0;
        if (__atomic_compare_exchange_n(word, &expected, 1u,
                                        0, __ATOMIC_ACQUIRE,
                                        __ATOMIC_RELAXED))
            return;
        __asm__ volatile("pause" ::: "memory");
    }
    /* Slow path: ensure state is "locked-with-waiters", then sleep. */
    for (;;) {
        uint32_t cur = __atomic_exchange_n(word, 2u, __ATOMIC_ACQUIRE);
        if (cur == 0) return;       /* we got the lock */
        /* Sleep until value changes from 2; spurious wakeups handled
         * by the for-loop. */
        sys_futex(word, FUTEX_WAIT, 2u);
    }
}

void sud_ir_unlock(volatile uint32_t *word)
{
    uint32_t prev = __atomic_exchange_n(word, 0u, __ATOMIC_RELEASE);
    if (prev == 2u)
        sys_futex(word, FUTEX_WAKE, 1);
    /* prev == 1: no waiters, no wake needed. */
}

/* ================================================================
 * Bitmap helpers
 * ================================================================ */

static int  bm_test(const uint8_t *bm, uint32_t i)
{
    return (bm[i >> 3] >> (i & 7)) & 1;
}
static void bm_set(uint8_t *bm, uint32_t i)
{
    bm[i >> 3] |= (uint8_t)(1u << (i & 7));
}
static void bm_clear(uint8_t *bm, uint32_t i)
{
    bm[i >> 3] &= (uint8_t)~(1u << (i & 7));
}

/* ================================================================
 * Inode allocator (caller holds sb->lock)
 * ================================================================ */

struct sud_ir_inode *sud_ir_inode_get(uint32_t index)
{
    struct sud_ir_super *sb = sud_ir_sb();
    if (!sb || index == 0 || index >= sb->inode_count) return 0;
    uint8_t *bm = (uint8_t *)sud_ir_ptr(sb->inode_bitmap_off);
    if (!bm_test(bm, index)) return 0;
    struct sud_ir_inode *table =
        (struct sud_ir_inode *)sud_ir_ptr(sb->inode_table_off);
    return &table[index];
}

uint32_t sud_ir_inode_alloc(uint32_t type, uint32_t mode,
                            uint32_t uid, uint32_t gid)
{
    struct sud_ir_super *sb = sud_ir_sb();
    uint8_t *bm = (uint8_t *)sud_ir_ptr(sb->inode_bitmap_off);
    struct sud_ir_inode *table =
        (struct sud_ir_inode *)sud_ir_ptr(sb->inode_table_off);

    /* Linear scan from hint.  inode_count is bounded (≤ 2^16) so
     * this is fast even when full.  Skip slot 0 (reserved). */
    uint32_t start = sb->next_inode_hint;
    if (start < 1 || start >= sb->inode_count) start = 1;
    uint32_t i = start;
    do {
        if (!bm_test(bm, i)) {
            bm_set(bm, i);
            struct sud_ir_inode *ino = &table[i];
            uint32_t gen = ino->generation + 1;
            memset((void *)ino, 0, sizeof(*ino));
            ino->generation = gen;
            ino->type = type;
            ino->mode = mode;
            ino->uid  = uid;
            ino->gid  = gid;
            ino->nlink = 0;
            uint64_t now = sud_ir_now_ns();
            ino->atime_ns = now;
            ino->mtime_ns = now;
            ino->ctime_ns = now;
            sb->next_inode_hint = (i + 1 < sb->inode_count) ? i + 1 : 1;
            sb->inodes_in_use++;
            return i;
        }
        i++;
        if (i >= sb->inode_count) i = 1;
    } while (i != start);
    return 0;
}

void sud_ir_inode_free(uint32_t index)
{
    struct sud_ir_super *sb = sud_ir_sb();
    if (index == 0 || index >= sb->inode_count) return;
    uint8_t *bm = (uint8_t *)sud_ir_ptr(sb->inode_bitmap_off);
    if (!bm_test(bm, index)) return;
    struct sud_ir_inode *ino = sud_ir_inode_get(index);
    if (ino) {
        /* Reclaim type-specific data. */
        if (ino->type == SUD_IR_T_REG) {
            if (ino->u.reg.tag == SUD_IR_REG_LARGE) {
                /* Per-file shm: drop the cached fd and unlink the
                 * /dev/shm object so the kernel reclaims its pages
                 * on last close. */
                sud_ir_large_unlink(ino->u.reg.u.large.file_idx,
                                    ino->u.reg.u.large.file_gen);
            } else if (ino->u.reg.u.small.nblocks) {
                sud_ir_small_free(ino->u.reg.u.small.start_block,
                                  ino->u.reg.u.small.nblocks);
            }
            ino->u.reg.tag = SUD_IR_REG_SMALL;
            ino->u.reg.u.small.start_block = 0;
            ino->u.reg.u.small.nblocks     = 0;
        } else if (ino->type == SUD_IR_T_DIR && ino->u.dir.dirblock_head_offset) {
            uint32_t off = ino->u.dir.dirblock_head_offset;
            while (off) {
                struct sud_ir_dirblock *db = (struct sud_ir_dirblock *)sud_ir_ptr(off);
                uint32_t next = db->next_offset;
                sud_ir_block_free(off, 1);
                off = next;
            }
        } else if (ino->type == SUD_IR_T_LNK && ino->u.lnk.target_block_offset) {
            sud_ir_block_free(ino->u.lnk.target_block_offset, 1);
        }
        ino->type = SUD_IR_T_FREE;
    }
    bm_clear(bm, index);
    if (sb->inodes_in_use) sb->inodes_in_use--;
}

/* ================================================================
 * Metadata block allocator (caller holds sb->lock)
 *
 * Bitmap-allocated 4 KiB blocks living *inside the metadata region*.
 * Used for dirent blocks and symlink target blocks — anything that
 * needs to be addressable by region offset and read by both 32-bit
 * and 64-bit processes via the shared metadata mapping.  Regular-
 * file content does NOT live here; see sud_ir_fat_alloc/free for
 * that.
 *
 * Simple first-fit scan from a hint with two-pass wrap-around.  The
 * usage pattern (small directories, occasional symlinks) doesn't
 * stress this allocator enough to need anything fancier.
 * ================================================================ */

uint32_t sud_ir_block_alloc(uint32_t nblocks)
{
    if (nblocks == 0) return 0;
    struct sud_ir_super *sb = sud_ir_sb();
    uint8_t *bm = (uint8_t *)sud_ir_ptr(sb->block_bitmap_off);
    uint32_t total = sb->block_count;
    uint32_t start = sb->next_block_hint;
    if (start >= total) start = 0;

    /* Two-pass scan: from hint to end, then 0 to hint. */
    uint32_t passes[2][2] = { { start, total }, { 0, start } };
    for (int p = 0; p < 2; p++) {
        uint32_t i = passes[p][0];
        uint32_t end = passes[p][1];
        while (i + nblocks <= end) {
            /* Check run starting at i. */
            uint32_t j = 0;
            while (j < nblocks && !bm_test(bm, i + j)) j++;
            if (j == nblocks) {
                for (uint32_t k = 0; k < nblocks; k++)
                    bm_set(bm, i + k);
                sb->next_block_hint = (i + nblocks < total)
                                       ? (i + nblocks) : 0;
                sb->blocks_in_use += nblocks;
                uint32_t off = sb->block_data_off + i * SUD_IR_BLOCK_SIZE;
                /* Zero the freshly allocated extent so files have
                 * deterministic initial contents. */
                memset((void *)sud_ir_ptr(off), 0,
                       (size_t)nblocks * SUD_IR_BLOCK_SIZE);
                return off;
            }
            /* Skip over the busy block we just found. */
            i += j + 1;
        }
    }
    return 0;
}

void sud_ir_block_free(uint32_t off, uint32_t nblocks)
{
    if (off == 0 || nblocks == 0) return;
    struct sud_ir_super *sb = sud_ir_sb();
    if (off < sb->block_data_off) return;
    uint32_t i = (off - sb->block_data_off) / SUD_IR_BLOCK_SIZE;
    uint8_t *bm = (uint8_t *)sud_ir_ptr(sb->block_bitmap_off);
    for (uint32_t k = 0; k < nblocks && (i + k) < sb->block_count; k++) {
        if (bm_test(bm, i + k)) {
            bm_clear(bm, i + k);
            if (sb->blocks_in_use) sb->blocks_in_use--;
        }
    }
}

/* ================================================================
 * Small-file shm: contiguous-extent allocator
 *
 * The small-file shm is a separate /dev/shm object created sparse
 * with size SUD_IR_SMALL_SHM_SIZE (8 GiB on the default config).
 * Sparse means the kernel only allocates physical pages for blocks
 * that actually get written; the file size is just a virtual cap.
 * On 64-bit the shm is mapped MAP_SHARED|MAP_NORESERVE at a fixed
 * address so SMALL files are served by zero-copy memcpy.  On
 * 32-bit the shm is NOT mapped — there is no spare 32-bit address
 * space to reserve for it — and SMALL access goes through pread/
 * pwrite on g_small_fd via byte offsets computed the same way.
 *
 * Each SMALL regular file occupies exactly one contiguous run of
 * `nblocks` blocks (this is an inviolable invariant: file data is
 * never fragmented).  Allocation is first-fit over a bitmap kept
 * in the metadata shm.  When the bitmap is exhausted (no
 * contiguous run of the required length), allocation returns 0;
 * callers escalate by promoting the file to a per-file LARGE shm
 * (see vfs.c::file_promote).  This is NOT an error — it's the
 * normal escape valve that decouples small-file allocator capacity
 * from the maximum size of any individual file.
 *
 * Caller MUST hold sb->lock for alloc/free (the search-and-set is
 * not lock-free).  Promotion takes sb->lock briefly to free the
 * extent, but does the per-file shm work (open, ftruncate, copy)
 * outside the lock.
 * ================================================================ */

uint32_t sud_ir_small_alloc(uint32_t nblocks)
{
    if (nblocks == 0) return 0;
    struct sud_ir_super *sb = sud_ir_sb();
    uint8_t *bm = (uint8_t *)sud_ir_ptr(sb->small_bitmap_off);
    uint32_t total = sb->small_block_count;
    uint32_t start = sb->small_alloc_hint;
    if (start >= total) start = 0;

    /* Two-pass first-fit scan: hint→end, then 0→hint. */
    uint32_t passes[2][2] = { { start, total }, { 0, start } };
    for (int p = 0; p < 2; p++) {
        uint32_t i   = passes[p][0];
        uint32_t end = passes[p][1];
        while (i + nblocks <= end) {
            uint32_t j = 0;
            while (j < nblocks && !bm_test(bm, i + j)) j++;
            if (j == nblocks) {
                for (uint32_t k = 0; k < nblocks; k++)
                    bm_set(bm, i + k);
                sb->small_alloc_hint = (i + nblocks < total)
                                        ? (i + nblocks) : 0;
                sb->small_blocks_in_use += nblocks;
                /* Block ids are 1-based: caller stores
                 * start_block = i + 1 in the inode. */
                return i + 1;
            }
            i += j + 1;
        }
    }
    /* No contiguous run of `nblocks` available — caller promotes. */
    return 0;
}

void sud_ir_small_free(uint32_t start_block, uint32_t nblocks)
{
    if (start_block == 0 || nblocks == 0) return;
    struct sud_ir_super *sb = sud_ir_sb();
    uint8_t *bm = (uint8_t *)sud_ir_ptr(sb->small_bitmap_off);
    uint32_t i = start_block - 1;       /* convert to 0-based bitmap idx */
    if (i >= sb->small_block_count) return;

    uint32_t freed = 0;
    for (uint32_t k = 0; k < nblocks && (i + k) < sb->small_block_count; k++) {
        if (bm_test(bm, i + k)) {
            bm_clear(bm, i + k);
            freed++;
        }
    }
    if (sb->small_blocks_in_use >= freed) sb->small_blocks_in_use -= freed;
    else                                  sb->small_blocks_in_use = 0;

    /* Best-effort hole-punch: drop the freed range from the small
     * shm so resident memory tracks live data, not high-water-mark.
     * Failure (older kernels, tmpfs without punch-hole support) is
     * not fatal — pages stay around until shm unlink. */
    if (g_small_fd >= 0 && freed) {
#ifdef SYS_fallocate
        raw_syscall6(SYS_fallocate, g_small_fd,
                     FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                     (long)(uint64_t)((start_block - 1) * SUD_IR_BLOCK_SIZE),
                     (long)(uint64_t)((uint64_t)nblocks * SUD_IR_BLOCK_SIZE),
                     0, 0);
#endif
    }
}

/* ================================================================
 * 32-bit pread/pwrite over the small-file shm fd
 *
 * Used by SMALL data ops on i386 (where the small shm is never
 * mapped) and as a portable fallback on 64-bit (e.g. in promotion
 * when copying out before the per-file shm exists).  Loops to
 * handle short transfers — pread/pwrite on tmpfs almost never
 * shorts in practice but the contract demands it.
 * ================================================================ */

long sud_ir_small_pread(void *buf, size_t count, uint64_t off)
{
    if (g_small_fd < 0) return -EBADF;
    size_t done = 0;
    while (done < count) {
#ifdef SYS_pread64
        long n = raw_syscall6(SYS_pread64, g_small_fd,
                              (long)((char *)buf + done),
                              (long)(count - done),
                              (long)(uint64_t)(off + done), 0, 0);
#else
        long n = raw_syscall6(SYS_pread, g_small_fd,
                              (long)((char *)buf + done),
                              (long)(count - done),
                              (long)(uint64_t)(off + done), 0, 0);
#endif
        if (n < 0) {
            if (n == -EINTR) continue;
            return done ? (long)done : n;
        }
        if (n == 0) {
            /* Reading past EOF of the sparse small shm: zero-fill the
             * remainder so callers see deterministic content (matches
             * the mapped-shm fast path, where unwritten pages fault
             * to anonymous zero). */
            memset((char *)buf + done, 0, count - done);
            return (long)count;
        }
        done += (size_t)n;
    }
    return (long)done;
}

long sud_ir_small_pwrite(const void *buf, size_t count, uint64_t off)
{
    if (g_small_fd < 0) return -EBADF;
    size_t done = 0;
    while (done < count) {
#ifdef SYS_pwrite64
        long n = raw_syscall6(SYS_pwrite64, g_small_fd,
                              (long)((const char *)buf + done),
                              (long)(count - done),
                              (long)(uint64_t)(off + done), 0, 0);
#else
        long n = raw_syscall6(SYS_pwrite, g_small_fd,
                              (long)((const char *)buf + done),
                              (long)(count - done),
                              (long)(uint64_t)(off + done), 0, 0);
#endif
        if (n < 0) {
            if (n == -EINTR) continue;
            return done ? (long)done : n;
        }
        if (n == 0) return done ? (long)done : -EIO;
        done += (size_t)n;
    }
    return (long)done;
}

/* ================================================================
 * Per-file LARGE shm: per-process kfd cache
 *
 * Each LARGE inode owns its own /dev/shm tmpfs object named
 *   /dev/shm/sud-inramfs.<key>.f.<file_idx>.<file_gen>
 *
 * The shm is created at promotion time, ftruncated as the file
 * grows, and unlinked on inode-free.  Per-process we cache the
 * open fd in a small open-addressed array, keyed by
 * (file_idx, file_gen).  Lookup is O(N) over a bounded slot count,
 * which is fine: cache size is the number of LARGE files this
 * process has touched concurrently, expected to be small.
 *
 * The cache is process-local: the addin's mmap path opens the
 * per-file shm here on first use, then forwards mmap to the kfd.
 * Other processes opening the same LARGE inode get their own
 * cached kfd via the shared (file_idx, file_gen) key.
 * ================================================================ */

#define SUD_IR_LARGE_CACHE_SIZE 256
struct sud_ir_large_slot {
    int      kfd;
    uint32_t file_idx;
    uint32_t file_gen;
};
static struct sud_ir_large_slot g_large_cache[SUD_IR_LARGE_CACHE_SIZE];
static int                      g_large_cache_init;

static void large_cache_init(void)
{
    if (g_large_cache_init) return;
    for (int i = 0; i < SUD_IR_LARGE_CACHE_SIZE; i++)
        g_large_cache[i].kfd = -1;
    g_large_cache_init = 1;
}

static struct sud_ir_large_slot *large_cache_lookup(uint32_t idx, uint32_t gen)
{
    if (!g_large_cache_init) return 0;
    for (int i = 0; i < SUD_IR_LARGE_CACHE_SIZE; i++) {
        if (g_large_cache[i].kfd >= 0
            && g_large_cache[i].file_idx == idx
            && g_large_cache[i].file_gen == gen) {
            return &g_large_cache[i];
        }
    }
    return 0;
}

static struct sud_ir_large_slot *large_cache_insert(int kfd,
                                                    uint32_t idx,
                                                    uint32_t gen)
{
    large_cache_init();
    for (int i = 0; i < SUD_IR_LARGE_CACHE_SIZE; i++) {
        if (g_large_cache[i].kfd == -1) {
            g_large_cache[i].kfd      = kfd;
            g_large_cache[i].file_idx = idx;
            g_large_cache[i].file_gen = gen;
            return &g_large_cache[i];
        }
    }
    /* Cache full — evict the first slot.  The previous owner's
     * subsequent ops will reopen on demand.  No data loss because
     * the per-file shm is named in /dev/shm. */
    raw_close(g_large_cache[0].kfd);
    g_large_cache[0].kfd      = kfd;
    g_large_cache[0].file_idx = idx;
    g_large_cache[0].file_gen = gen;
    return &g_large_cache[0];
}

/* Compose /dev/shm/sud-inramfs.<key>.f.<idx>.<gen> into `out`. */
void sud_ir_large_path(uint32_t idx, uint32_t gen,
                       char *out, size_t out_sz)
{
    snprintf(out, out_sz, "/dev/shm/sud-inramfs.%s.f.%u.%u",
             g_shm_key, idx, gen);
}

int sud_ir_large_open(uint32_t file_idx, uint32_t file_gen)
{
    struct sud_ir_large_slot *s = large_cache_lookup(file_idx, file_gen);
    if (s) return s->kfd;

    char p[PATH_MAX];
    sud_ir_large_path(file_idx, file_gen, p, sizeof(p));
    /* O_CREAT so the first opener (across all processes) materialises
     * the shm.  Subsequent opens just attach.  No O_EXCL: races are
     * benign — both openers end up with a valid fd. */
    int fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)p,
                               O_RDWR | O_CREAT | O_CLOEXEC, 0600, 0, 0);
    if (fd < 0) return fd;
    large_cache_insert(fd, file_idx, file_gen);
    return fd;
}

int sud_ir_large_ftruncate(uint32_t file_idx, uint32_t file_gen,
                           uint64_t size)
{
    int fd = sud_ir_large_open(file_idx, file_gen);
    if (fd < 0) return fd;
#ifdef SYS_ftruncate64
    long r = raw_syscall6(SYS_ftruncate64, fd, (long)size, 0, 0, 0, 0);
#else
    long r = raw_syscall6(SYS_ftruncate, fd, (long)size, 0, 0, 0, 0);
#endif
    return (r < 0) ? (int)r : 0;
}

void sud_ir_large_forget(uint32_t file_idx, uint32_t file_gen)
{
    struct sud_ir_large_slot *s = large_cache_lookup(file_idx, file_gen);
    if (s) {
        raw_close(s->kfd);
        s->kfd = -1;
    }
}

void sud_ir_large_unlink(uint32_t file_idx, uint32_t file_gen)
{
    sud_ir_large_forget(file_idx, file_gen);
    char p[PATH_MAX];
    sud_ir_large_path(file_idx, file_gen, p, sizeof(p));
    raw_unlinkat(AT_FDCWD, p, 0);   /* ignore ENOENT (loser of unlink race) */
}

/* ================================================================
 * Region attach / init
 *
 * Coordination:
 *   - First process to open the /dev/shm file creates it (O_CREAT|
 *     O_EXCL via openat) and ftruncates it to g_region_size.  Other
 *     processes lose the EXCL race and just open without create.
 *   - All processes mmap MAP_SHARED|MAP_FIXED at a fixed high
 *     address (so byte offsets are also valid as raw addresses; we
 *     use offsets for portability).
 *   - The first process to observe sb->magic == 0 takes the init
 *     latch (CAS init_state 0 → 1), populates the layout, and stores
 *     init_state = 2 + futex_wake.  Losers spin/futex_wait.
 *   - Per-rule prefix → key derivation makes the shm path stable
 *     across processes that share the same SUD_INRAMFS prefix.
 * ================================================================ */

/* ================================================================
 * XXH64 — small inline 64-bit hash (Yann Collet, public-domain
 * primitives).  Used at process init to derive a stable shm-key from
 * the mount path; not on any data-path.  Inlined rather than pulling
 * in the bundled zstd xxhash.h (~7 KLOC, drags in stdint/string
 * headers that conflict with our freestanding build).  Values match
 * the reference XXH64 with seed 0; verified against test vectors in
 * test_inramfs.c::test_xxh64. */
#define SUD_IR_XXH_P1 0x9E3779B185EBCA87ULL
#define SUD_IR_XXH_P2 0xC2B2AE3D27D4EB4FULL
#define SUD_IR_XXH_P3 0x165667B19E3779F9ULL
#define SUD_IR_XXH_P4 0x85EBCA77C2B2AE63ULL
#define SUD_IR_XXH_P5 0x27D4EB2F165667C5ULL

static inline uint64_t sud_ir_rotl64(uint64_t x, int r)
{
    return (x << r) | (x >> (64 - r));
}

static inline uint64_t sud_ir_xxh_round(uint64_t acc, uint64_t input)
{
    acc += input * SUD_IR_XXH_P2;
    acc  = sud_ir_rotl64(acc, 31);
    acc *= SUD_IR_XXH_P1;
    return acc;
}

static inline uint64_t sud_ir_xxh_merge(uint64_t acc, uint64_t val)
{
    val = sud_ir_xxh_round(0, val);
    acc ^= val;
    return acc * SUD_IR_XXH_P1 + SUD_IR_XXH_P4;
}

/* XXH64(data, len, seed=0). */
uint64_t sud_ir_xxh64(const void *data, size_t len)
{
    const uint8_t *p   = (const uint8_t *)data;
    const uint8_t *end = p + len;
    uint64_t h64;

    if (len >= 32) {
        const uint8_t *limit = end - 32;
        /* Reference XXH64 seed-derived initial accumulators (seed=0).
         * `0 - P1` is the spec's two's-complement negation of P1 in
         * unsigned uint64_t arithmetic — well-defined and matches
         * the upstream reference implementation. */
        uint64_t v1 = SUD_IR_XXH_P1 + SUD_IR_XXH_P2;
        uint64_t v2 = SUD_IR_XXH_P2;
        uint64_t v3 = 0;
        uint64_t v4 = 0 - SUD_IR_XXH_P1;
        do {
            uint64_t k1, k2, k3, k4;
            memcpy(&k1, p,      8);
            memcpy(&k2, p + 8,  8);
            memcpy(&k3, p + 16, 8);
            memcpy(&k4, p + 24, 8);
            v1 = sud_ir_xxh_round(v1, k1);
            v2 = sud_ir_xxh_round(v2, k2);
            v3 = sud_ir_xxh_round(v3, k3);
            v4 = sud_ir_xxh_round(v4, k4);
            p += 32;
        } while (p <= limit);
        h64 = sud_ir_rotl64(v1, 1) + sud_ir_rotl64(v2, 7)
            + sud_ir_rotl64(v3, 12) + sud_ir_rotl64(v4, 18);
        h64 = sud_ir_xxh_merge(h64, v1);
        h64 = sud_ir_xxh_merge(h64, v2);
        h64 = sud_ir_xxh_merge(h64, v3);
        h64 = sud_ir_xxh_merge(h64, v4);
    } else {
        h64 = SUD_IR_XXH_P5;
    }
    h64 += (uint64_t)len;
    while (p + 8 <= end) {
        uint64_t k1;
        memcpy(&k1, p, 8);
        k1  = sud_ir_xxh_round(0, k1);
        h64 ^= k1;
        h64  = sud_ir_rotl64(h64, 27) * SUD_IR_XXH_P1 + SUD_IR_XXH_P4;
        p += 8;
    }
    if (p + 4 <= end) {
        uint32_t v;
        memcpy(&v, p, 4);
        h64 ^= (uint64_t)v * SUD_IR_XXH_P1;
        h64  = sud_ir_rotl64(h64, 23) * SUD_IR_XXH_P2 + SUD_IR_XXH_P3;
        p += 4;
    }
    while (p < end) {
        h64 ^= (uint64_t)(*p) * SUD_IR_XXH_P5;
        h64  = sud_ir_rotl64(h64, 11) * SUD_IR_XXH_P1;
        p++;
    }
    h64 ^= h64 >> 33;
    h64 *= SUD_IR_XXH_P2;
    h64 ^= h64 >> 29;
    h64 *= SUD_IR_XXH_P3;
    h64 ^= h64 >> 32;
    return h64;
}

/* Compose the two shm paths plus the shared key portion:
 *   /dev/shm/sud-inramfs.<key>.meta      — metadata region
 *   /dev/shm/sud-inramfs.<key>.smalldata — small-file shm
 *
 * The key is also stashed in `key_out` so per-LARGE-file shm paths
 * (composed lazily by sud_ir_large_open) share the same prefix. */
static void compose_shm_paths(const char *user_key,
                              const char *mount_path,
                              char *meta_out,  size_t meta_sz,
                              char *small_out, size_t small_sz,
                              char *key_out,   size_t key_sz)
{
    /* 64 bytes: holds a 16-hex-char XXH64 key (17 with NUL) or a
     * caller-supplied SUD_INRAMFS_KEY (snprintf truncates safely). */
    char key[64];
    if (user_key && user_key[0]) {
        snprintf(key, sizeof(key), "%s", user_key);
    } else {
        size_t n = 0;
        while (mount_path[n]) n++;
        uint64_t h = sud_ir_xxh64(mount_path, n);
        snprintf(key, sizeof(key), "%016llx", (unsigned long long)h);
    }
    snprintf(meta_out,  meta_sz,  "/dev/shm/sud-inramfs.%s.meta",      key);
    snprintf(small_out, small_sz, "/dev/shm/sud-inramfs.%s.smalldata", key);
    snprintf(key_out,   key_sz,   "%s", key);
}

/* Choose the fixed mapping address.  sud32 lives at 0x20000000 and
 * sud64 at 0x40000000 (see Makefile -Ttext-segment); we pick a
 * region well above either to avoid colliding with the wrapper, the
 * traced program's brk/heap, or the loader's mmap area. */
static void *fixed_addr(void)
{
#if defined(__x86_64__)
    /* Pick a high address well above ld.so's typical mmap region
     * (~0x7f...) and the wrapper's text (sud64 lives at 0x40000000),
     * while staying inside the canonical user-space half.  Every sud
     * loader maps the shared region here so byte offsets within the
     * region are stable across processes. */
    return (void *)0x500000000000UL;
#else
    /* On i386, sud32 is at 0x20000000, the kernel takes >= 0xc0000000.
     * Place us at 0x80000000 (mid user space). */
    return (void *)0x80000000UL;
#endif
}

/* Fixed mapping address for the small-file shm.  64-bit only (32-bit
 * does not map this shm).  We pick a high address well above the
 * metadata mapping; the small shm is reserved as a single huge
 * MAP_NORESERVE region so its full virtual size (8 GiB by default)
 * is reserved up front but only physical pages for blocks that get
 * touched are actually allocated. */
static void *fixed_data_addr(void)
{
#if defined(__x86_64__)
    return (void *)0x600000000000UL;
#else
    /* Unused on 32-bit. */
    return (void *)0;
#endif
}

/* Initialise a freshly-created **metadata** region.  Caller has the
 * init latch.  meta_size is the user-chosen metadata mapping length.
 *
 * Layout (all aligned up to 64 B, then BLOCK_SIZE for the metadata
 * block area):
 *
 *   [0]                              super (struct sud_ir_super)
 *   [aligned]                        inode bitmap (SUD_IR_MAX_INODES bits)
 *   [aligned]                        inode table  (SUD_IR_MAX_INODES * sizeof(inode))
 *   [aligned]                        small-file allocator bitmap
 *                                    (SUD_IR_SMALL_BLOCKS bits)
 *   [aligned]                        metadata block bitmap
 *   [BLOCK_SIZE-aligned]             metadata blocks (dirents, symlink targets)
 */
static void init_meta_region(struct sud_ir_super *sb, uint64_t meta_size)
{
    memset((void *)sb, 0, meta_size);

    sb->version       = SUD_IR_VERSION;
    sb->region_size   = meta_size;
    sb->small_shm_size = SUD_IR_SMALL_SHM_SIZE;

    uint32_t off = (uint32_t)((sizeof(*sb) + 63) & ~63u);

    sb->inode_count       = SUD_IR_MAX_INODES;
    sb->inode_bitmap_off  = off;
    off += (sb->inode_count + 7) / 8;
    off = (off + 63) & ~63u;

    sb->inode_table_off   = off;
    off += sb->inode_count * (uint32_t)sizeof(struct sud_ir_inode);
    off = (off + 63) & ~63u;

    /* Small-file allocator bitmap: one bit per BLOCK_SIZE block in
     * the small-file shm.  Sized to cover SUD_IR_SMALL_BLOCKS so
     * the worst case (every inode holds a max-sized small file) is
     * representable.  When the bitmap is exhausted, new file growth
     * promotes to a per-file LARGE shm. */
    sb->small_block_count = SUD_IR_SMALL_BLOCKS;
    sb->small_bitmap_off  = off;
    uint32_t small_bitmap_bytes = (sb->small_block_count + 7) / 8;
    small_bitmap_bytes = (small_bitmap_bytes + 63) & ~63u;
    off += small_bitmap_bytes;
    off = (off + 63) & ~63u;
    sb->small_alloc_hint    = 0;
    sb->small_blocks_in_use = 0;

    /* Metadata block area: everything that's left in the metadata
     * region, used for dirent blocks and symlink targets. */
    uint64_t remaining = meta_size - off;
    uint64_t bitmap_bytes = remaining / (1 + 8 * SUD_IR_BLOCK_SIZE);
    if (bitmap_bytes < 1) bitmap_bytes = 1;
    bitmap_bytes = (bitmap_bytes + 63) & ~63ull;

    sb->block_bitmap_off = off;
    off += (uint32_t)bitmap_bytes;
    off = (off + SUD_IR_BLOCK_SIZE - 1) & ~(SUD_IR_BLOCK_SIZE - 1u);
    sb->block_data_off = off;
    sb->block_count = (uint32_t)((meta_size - off) / SUD_IR_BLOCK_SIZE);
    if (sb->block_count > bitmap_bytes * 8)
        sb->block_count = (uint32_t)(bitmap_bytes * 8);

    sb->next_inode_hint = 2;     /* skip slot 0 (NULL) and 1 (root) */
    sb->next_block_hint = 0;

    /* Reserve inode 0 (NULL sentinel) and initialise inode 1 (root). */
    uint8_t *bm = (uint8_t *)((char *)sb + sb->inode_bitmap_off);
    bm_set(bm, 0);
    struct sud_ir_inode *table =
        (struct sud_ir_inode *)((char *)sb + sb->inode_table_off);
    bm_set(bm, 1);
    struct sud_ir_inode *root = &table[1];
    memset((void *)root, 0, sizeof(*root));
    root->type  = SUD_IR_T_DIR;
    root->mode  = 0755;
#ifdef SYS_getuid
    root->uid = (uint32_t)raw_syscall6(SYS_getuid, 0, 0, 0, 0, 0, 0);
#elif defined(SYS_getuid32)
    root->uid = (uint32_t)raw_syscall6(SYS_getuid32, 0, 0, 0, 0, 0, 0);
#endif
#ifdef SYS_getgid
    root->gid = (uint32_t)raw_syscall6(SYS_getgid, 0, 0, 0, 0, 0, 0);
#elif defined(SYS_getgid32)
    root->gid = (uint32_t)raw_syscall6(SYS_getgid32, 0, 0, 0, 0, 0, 0);
#endif
    root->nlink = 2;
    uint64_t now = sud_ir_now_ns();
    root->atime_ns = root->mtime_ns = root->ctime_ns = now;
    sb->inodes_in_use = 1;

    /* Magic is the last write — readers that see magic != 0 know the
     * region layout is fully populated. */
    __atomic_store_n(&sb->magic, SUD_IR_MAGIC, __ATOMIC_RELEASE);
}

/* Wait for the init latch to settle.  Used by losers of the init race. */
static void wait_for_init(volatile uint32_t *init_state)
{
    for (int i = 0; i < 100000; i++) {
        if (__atomic_load_n(init_state, __ATOMIC_ACQUIRE) >= 2) return;
        sys_futex(init_state, FUTEX_WAIT, 1);
        if (__atomic_load_n(init_state, __ATOMIC_ACQUIRE) >= 2) return;
    }
    /* If we're stuck for >100k spins something is very wrong; fall
     * through and let the caller see a still-zero magic.  The addin
     * disables itself in that case. */
}

/* Populate g_mount_path / g_mount_len / g_meta_size from the runtime
 * config (preferred) or, if no config is present, from the legacy
 * SUD_INRAMFS env var (transitional, used only by tests that have
 * not yet migrated).  Returns 1 if a mount was configured, 0 if not.
 *
 * The mount path is sourced from a "--remap-rule inramfs:<path>"
 * entry — once Part 1 of the layered split lands, only path_remap
 * will read this rule, but for now inramfs continues to consult it
 * directly so it knows which prefix it owns.  The metadata size
 * comes from --inramfs-meta-mb. */
static int parse_env(void)
{
    const char *path = NULL;
    size_t      plen = 0;
    uint64_t    size_mb = 16;       /* default */

    if (g_sud_runtime_config_present) {
        for (int i = 0; i < g_sud_runtime_config.remap_rule_count; i++) {
            const char *r = g_sud_runtime_config.remap_rules[i];
            if (!r) continue;
            /* Only the first inramfs rule is honoured.  Match
             * "inramfs:" prefix, then take the rest as the path
             * (which may itself contain a trailing :<size> from a
             * legacy translation — strip it). */
            const char tag[] = "inramfs:";
            const size_t tlen = sizeof(tag) - 1;
            int match = 1;
            for (size_t k = 0; k < tlen; k++)
                if (r[k] != tag[k]) { match = 0; break; }
            if (!match) continue;
            const char *p = r + tlen;
            const char *end = p;
            while (*end && *end != ':') end++;
            path = p;
            plen = (size_t)(end - p);
            break;
        }
        if (g_sud_runtime_config.inramfs_meta_mb > 0)
            size_mb = (uint64_t)g_sud_runtime_config.inramfs_meta_mb;
    }
    if (!path) {
        /* Transitional fallback: legacy env var.  Removed once all
         * tests have switched to populating g_sud_runtime_config. */
        const char *e = getenv("SUD_INRAMFS");
        if (!e || !e[0]) return 0;
        path = e;
        plen = 0;
        while (path[plen] && path[plen] != ':') plen++;
        if (path[plen] == ':') {
            const char *s = path + plen + 1;
            uint64_t v = 0;
            while (*s >= '0' && *s <= '9') {
                v = v * 10 + (uint64_t)(*s - '0');
                s++;
            }
            if (v) size_mb = v;
        }
    }
    if (plen == 0 || plen >= sizeof(g_mount_path)) return 0;
    if (path[0] != '/') return 0;            /* must be absolute */
    /* Strip trailing slashes (but not the root). */
    while (plen > 1 && path[plen - 1] == '/') plen--;
    memcpy(g_mount_path, path, plen);
    g_mount_path[plen] = '\0';
    g_mount_len = plen;

    g_meta_size = (size_t)(size_mb * 1024ull * 1024ull);
    return 1;
}

/* Compute the minimum viable metadata region size: enough for the
 * inode table, the small-file allocator bitmap, the metadata block
 * bitmap, and at least a few hundred metadata blocks for dirents
 * and symlinks.  If the user-requested size is below this floor
 * we round it up. */
static size_t min_meta_size(void)
{
    size_t inode_table  = SUD_IR_MAX_INODES * sizeof(struct sud_ir_inode);
    size_t inode_bitmap = (SUD_IR_MAX_INODES + 7) / 8;
    size_t small_bitmap = (SUD_IR_SMALL_BLOCKS + 7) / 8;
    /* Reserve ~1024 metadata blocks for dirents + symlinks. */
    size_t meta_blocks       = 1024;
    size_t meta_block_data   = meta_blocks * SUD_IR_BLOCK_SIZE;
    size_t meta_block_bitmap = (meta_blocks + 7) / 8;

    size_t total = sizeof(struct sud_ir_super)
                 + inode_bitmap + inode_table
                 + small_bitmap
                 + meta_block_bitmap + meta_block_data
                 + 16 * SUD_IR_BLOCK_SIZE;          /* alignment slack */
    return (total + SUD_IR_BLOCK_SIZE - 1) & ~(size_t)(SUD_IR_BLOCK_SIZE - 1);
}

/* Open or create a /dev/shm backing file at `path` and ftruncate it
 * to `size`.  Returns an open fd or a negative -errno; sets
 * *created to 1 if we won the create race. */
static int open_or_create_shm_at(const char *path, uint64_t size, int *created)
{
    *created = 0;
    int fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)path,
                               O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC,
                               0600, 0, 0);
    if (fd >= 0) {
        *created = 1;
#ifdef SYS_ftruncate
        long r = raw_syscall6(SYS_ftruncate, fd, (long)size, 0, 0, 0, 0);
#else
        long r = raw_syscall6(SYS_ftruncate64, fd, (long)size, 0, 0, 0, 0);
#endif
        if (r < 0) { raw_close(fd); return (int)r; }
        return fd;
    }
    /* Lost the EXCL race; just open. */
    return (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)path,
                             O_RDWR | O_CLOEXEC, 0, 0, 0);
}

/* mmap MAP_SHARED at exactly `want`.  Tries MAP_FIXED_NOREPLACE first
 * so two add-ins arguing over the same address fail loudly rather
 * than silently clobbering each other; falls back to MAP_FIXED on
 * older kernels.  `extra_flags` allows the caller to add MAP_NORESERVE
 * for the small-file shm so a multi-GiB sparse reservation does not
 * fail the kernel's overcommit check.  Returns the mapped address or
 * NULL on failure. */
static void *map_at_fixed(void *want, size_t size, int fd, int extra_flags)
{
    int mflags = MAP_SHARED | MAP_FIXED | extra_flags;
    void *p = raw_mmap(want, size, PROT_READ | PROT_WRITE,
                       mflags | MAP_FIXED_NOREPLACE, fd, 0);
    if ((unsigned long)p >= (unsigned long)-4095) {
        p = raw_mmap(want, size, PROT_READ | PROT_WRITE, mflags, fd, 0);
    }
    if ((unsigned long)p >= (unsigned long)-4095) return 0;
    return p;
}

void sud_inramfs_init(void)
{
    if (g_init_done) return;
    g_init_done = 1;

    if (!parse_env()) return;            /* no mount configured */

    /* Round the user's request up to the floor — it must be at
     * least big enough to hold the inode table + small-file bitmap
     * + a few hundred dirent blocks, otherwise the FS is unusable. */
    size_t floor = min_meta_size();
    if (g_meta_size < floor) g_meta_size = floor;

    /* Source the inramfs key from the runtime config (preferred); on
     * absence (test harness without a populated config) fall back to
     * the legacy SUD_INRAMFS_KEY env var.  The compose_shm_paths
     * helper accepts NULL meaning "no key suffix". */
    const char *key = NULL;
    if (g_sud_runtime_config_present)
        key = g_sud_runtime_config.inramfs_key;
    if (!key) key = getenv("SUD_INRAMFS_KEY");
    compose_shm_paths(key, g_mount_path,
                      g_meta_shm_path,  sizeof(g_meta_shm_path),
                      g_small_shm_path, sizeof(g_small_shm_path),
                      g_shm_key,        sizeof(g_shm_key));

    /* ---- Attach metadata shm. ---- */
    int meta_created = 0;
    int meta_fd = open_or_create_shm_at(g_meta_shm_path, g_meta_size,
                                        &meta_created);
    if (meta_fd < 0) return;
    void *meta_base = map_at_fixed(fixed_addr(), g_meta_size, meta_fd, 0);
    raw_close(meta_fd);
    if (!meta_base) return;
    sud_ir_base = (volatile char *)meta_base;

    /* ---- Attach small-file shm. ----
     * Created sparse at SUD_IR_SMALL_SHM_SIZE so the kernel only
     * allocates physical pages for blocks that actually get
     * written.  Held open across the lifetime of the addin so
     * sud_ir_small_free can punch holes in it (and so 32-bit
     * processes can pread/pwrite). */
    int small_created = 0;
    g_small_fd = open_or_create_shm_at(g_small_shm_path,
                                       SUD_IR_SMALL_SHM_SIZE,
                                       &small_created);
    if (g_small_fd < 0) {
        munmap((void *)sud_ir_base, g_meta_size);
        sud_ir_base = 0;
        return;
    }
#if defined(__x86_64__)
    /* 64-bit: map the small shm as one huge MAP_NORESERVE
     * reservation so SMALL files are served by zero-copy memcpy.
     * MAP_NORESERVE is essential — without it, the kernel would
     * try to commit 8 GiB of swap up front and fail. */
    void *small_base = map_at_fixed(fixed_data_addr(),
                                    (size_t)SUD_IR_SMALL_SHM_SIZE,
                                    g_small_fd, MAP_NORESERVE);
    if (!small_base) {
        raw_close(g_small_fd); g_small_fd = -1;
        munmap((void *)sud_ir_base, g_meta_size);
        sud_ir_base = 0;
        return;
    }
    sud_ir_data_base = (volatile char *)small_base;
#else
    /* 32-bit: the small shm is NOT mapped.  There is no spare
     * 32-bit address space for a multi-GiB reservation; SMALL
     * access goes through sud_ir_small_pread/pwrite on g_small_fd. */
    sud_ir_data_base = 0;
#endif

    /* ---- Initialise the metadata region (one process wins). ---- */
    struct sud_ir_super *sb = sud_ir_sb();
    if (meta_created) {
        init_meta_region(sb, g_meta_size);
        sys_futex(&sb->init_state, FUTEX_WAKE, 0x7fffffff);
    } else {
        for (int spins = 0; spins < 1000; spins++) {
            if (__atomic_load_n(&sb->magic, __ATOMIC_ACQUIRE) == SUD_IR_MAGIC)
                break;
            __asm__ volatile("pause" ::: "memory");
        }
        if (__atomic_load_n(&sb->magic, __ATOMIC_ACQUIRE) != SUD_IR_MAGIC) {
            uint32_t expected = 0;
            if (__atomic_compare_exchange_n(&sb->init_state, &expected, 1u,
                                            0, __ATOMIC_ACQ_REL,
                                            __ATOMIC_RELAXED)) {
                init_meta_region(sb, g_meta_size);
                sys_futex(&sb->init_state, FUTEX_WAKE, 0x7fffffff);
            } else {
                wait_for_init(&sb->init_state);
            }
        }
    }

    if (__atomic_load_n(&sb->magic, __ATOMIC_ACQUIRE) != SUD_IR_MAGIC) {
        /* Init failed somewhere; leave inactive but do not orphan
         * the small-file fd. */
        if (sud_ir_data_base)
            munmap((void *)sud_ir_data_base, (size_t)SUD_IR_SMALL_SHM_SIZE);
        munmap((void *)sud_ir_base,      g_meta_size);
        raw_close(g_small_fd); g_small_fd = -1;
        sud_ir_data_base = 0;
        sud_ir_base      = 0;
        return;
    }

    g_active = 1;
}

/* ================================================================
 * Test-only: tear down the in-process attachment.
 * ================================================================ */
void sud_inramfs_reset_for_testing(void)
{
    if (sud_ir_base && g_meta_size) {
        munmap((void *)sud_ir_base, g_meta_size);
    }
    if (sud_ir_data_base) {
        munmap((void *)sud_ir_data_base, (size_t)SUD_IR_SMALL_SHM_SIZE);
    }
    if (g_small_fd >= 0) raw_close(g_small_fd);
    /* Drop all per-large-file kfds. */
    if (g_large_cache_init) {
        for (int i = 0; i < SUD_IR_LARGE_CACHE_SIZE; i++) {
            if (g_large_cache[i].kfd >= 0) {
                raw_close(g_large_cache[i].kfd);
                g_large_cache[i].kfd = -1;
            }
        }
        g_large_cache_init = 0;
    }
    sud_ir_base       = 0;
    sud_ir_data_base  = 0;
    g_small_fd        = -1;
    g_active          = 0;
    g_init_done       = 0;
    g_mount_len       = 0;
    g_mount_path[0]   = '\0';
    g_meta_size       = 0;
    g_meta_shm_path[0]  = '\0';
    g_small_shm_path[0] = '\0';
    g_shm_key[0]        = '\0';
}

void sud_inramfs_unlink_backing_for_testing(void)
{
    if (g_meta_shm_path[0])
        raw_unlinkat(AT_FDCWD, g_meta_shm_path, 0);
    if (g_small_shm_path[0])
        raw_unlinkat(AT_FDCWD, g_small_shm_path, 0);
    /* Per-LARGE-file shms: the launcher's unlink path doesn't enumerate
     * them (no opendir in raw libc), so this test-only helper relies
     * on each test cleanly unlinking files via the addin (which calls
     * sud_ir_large_unlink on inode free).  Tests that crash mid-run
     * may leak per-file shms; CI cleanup is expected to wipe
     * /dev/shm/sud-inramfs.* between runs. */
}
