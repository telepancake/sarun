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

/* ================================================================
 * Module state
 * ================================================================ */

volatile char *sud_ir_base;          /* base of the shared mapping */

/* Mount config (parsed once from SUD_INRAMFS). */
static char   g_mount_path[PATH_MAX];
static size_t g_mount_len;
static size_t g_region_size;         /* bytes — set from SUD_INRAMFS or default */
static int    g_init_done;
static int    g_active;              /* 1 once attach succeeded */
static char   g_shm_path[PATH_MAX];

/* ================================================================
 * Public accessors
 * ================================================================ */

const char *sud_ir_mount_path(void) { return g_mount_len ? g_mount_path : 0; }
size_t      sud_ir_mount_len (void) { return g_mount_len; }
int         sud_inramfs_active(void) { return g_active; }

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
        if (ino->type == SUD_IR_T_REG && ino->u.reg.data_block_offset) {
            uint32_t nblocks = (ino->u.reg.capacity_bytes
                                + SUD_IR_BLOCK_SIZE - 1) / SUD_IR_BLOCK_SIZE;
            sud_ir_block_free(ino->u.reg.data_block_offset, nblocks);
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
 * Block allocator (caller holds sb->lock)
 *
 * Bitmap of 4 KiB blocks.  nblocks > 1 finds a contiguous run.  This
 * is a simple first-fit scan from a hint — fine for workloads where
 * the file count is dominated by small files; degrades for highly
 * fragmented allocations of large extents (documented future work
 * is a buddy or extent-tree allocator).
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

/* Compose the shm path: /dev/shm/sud-inramfs.<key>. */
static void compose_shm_path(const char *user_key,
                             const char *mount_path,
                             char *out, size_t out_sz)
{
    /* If the user provided SUD_INRAMFS_KEY use it verbatim, else hash
     * the mount path into a hex string so that two different mount
     * paths in the same UID never collide. */
    if (user_key && user_key[0]) {
        snprintf(out, out_sz, "/dev/shm/sud-inramfs.%s", user_key);
        return;
    }
    /* FNV-1a 64-bit hash. */
    uint64_t h = 0xcbf29ce484222325ull;
    for (const unsigned char *p = (const unsigned char *)mount_path; *p; p++) {
        h ^= *p;
        h *= 0x100000001b3ull;
    }
    snprintf(out, out_sz, "/dev/shm/sud-inramfs.%016lx",
             (unsigned long)h);
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

/* Initialise a freshly-created region.  Caller has the init latch.
 *
 * Layout:
 *   [0]                              super (struct sud_ir_super)
 *   [aligned-up]                     inode bitmap (SUD_IR_MAX_INODES bits)
 *   [aligned-up]                     inode table (SUD_IR_MAX_INODES * sizeof(inode))
 *   [aligned-up]                     block bitmap
 *   [aligned-up to BLOCK_SIZE]       data blocks
 */
static void init_region(struct sud_ir_super *sb, uint64_t region_size)
{
    /* Zero everything first.  This is the slow path (only the winner
     * of the init race runs it), and it ensures the bitmaps and
     * inode-table generation counters start at known values. */
    memset((void *)sb, 0, region_size);

    sb->version     = SUD_IR_VERSION;
    sb->region_size = region_size;

    uint32_t off = (uint32_t)((sizeof(*sb) + 63) & ~63u);

    /* Scale inode count to the region size: at most ~1/8 of the
     * region for the inode table.  This keeps small (test-sized)
     * regions usable while still providing tens of thousands of
     * inodes for default-sized regions. */
    uint32_t inode_count = SUD_IR_MAX_INODES;
    {
        uint64_t cap = (region_size / 8) / sizeof(struct sud_ir_inode);
        if (cap < 64) cap = 64;
        if (cap < inode_count) inode_count = (uint32_t)cap;
    }
    sb->inode_count       = inode_count;
    sb->inode_bitmap_off  = off;
    off += (inode_count + 7) / 8;
    off = (off + 63) & ~63u;

    sb->inode_table_off   = off;
    off += inode_count * (uint32_t)sizeof(struct sud_ir_inode);
    off = (off + 63) & ~63u;

    /* All remaining space is for the block bitmap + data blocks.  We
     * size the bitmap to cover the data region; iterate to find the
     * fixed point. */
    uint64_t remaining = region_size - off;
    /* Each (1 byte bitmap) covers 8 * 4096 = 32768 bytes of data.  Solve:
     *   bitmap_bytes + 8*4096*bitmap_bytes >= remaining
     *   bitmap_bytes >= remaining / (1 + 32768)
     */
    uint64_t bitmap_bytes = remaining / (1 + 8 * SUD_IR_BLOCK_SIZE);
    if (bitmap_bytes < 1) bitmap_bytes = 1;
    /* Round up bitmap to 64 B and align data area to BLOCK_SIZE. */
    bitmap_bytes = (bitmap_bytes + 63) & ~63ull;

    sb->block_bitmap_off = off;
    off += (uint32_t)bitmap_bytes;
    off = (off + SUD_IR_BLOCK_SIZE - 1) & ~(SUD_IR_BLOCK_SIZE - 1u);
    sb->block_data_off = off;

    sb->block_count = (uint32_t)((region_size - off) / SUD_IR_BLOCK_SIZE);
    /* Cap by what the bitmap can address. */
    if (sb->block_count > bitmap_bytes * 8)
        sb->block_count = (uint32_t)(bitmap_bytes * 8);

    sb->next_inode_hint = 2;     /* skip slot 0 (NULL) and 1 (root) */
    sb->next_block_hint = 0;

    /* Mark inode 0 (NULL) and inode 1 (root) as in-use, but we'll
     * initialise root's contents via the regular inode allocator. */
    uint8_t *bm = (uint8_t *)sud_ir_ptr(sb->inode_bitmap_off);
    bm_set(bm, 0);  /* sentinel — never returned as a real inode */
    /* Root: type = DIR, mode = 0755, uid = current uid, gid = current gid,
     * nlink = 2 (".", ".." from any future child). */
    struct sud_ir_inode *table =
        (struct sud_ir_inode *)sud_ir_ptr(sb->inode_table_off);
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

/* Open or create the /dev/shm backing file. */
static int open_or_create_shm(const char *path, uint64_t size, int *created)
{
    *created = 0;
    /* Try EXCL create first (we then own the ftruncate). */
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
    /* EEXIST: someone else got there first; just open. */
    fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)path,
                           O_RDWR | O_CLOEXEC, 0, 0, 0);
    return fd;
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

/* Parse SUD_INRAMFS=<path>[:<size_mb>]. */
static int parse_env(void)
{
    const char *e = getenv("SUD_INRAMFS");
    if (!e || !e[0]) return 0;
    /* Path. */
    const char *p = e;
    size_t plen = 0;
    while (p[plen] && p[plen] != ':') plen++;
    if (plen == 0 || plen >= sizeof(g_mount_path)) return 0;
    if (e[0] != '/') return 0;            /* must be absolute */
    /* Strip trailing slashes (but not the root). */
    while (plen > 1 && p[plen - 1] == '/') plen--;
    memcpy(g_mount_path, p, plen);
    g_mount_path[plen] = '\0';
    g_mount_len = plen;

    /* Size. */
    uint64_t size_mb = 256;
    if (p[plen] == ':') {
        const char *s = p + plen + 1;
        uint64_t v = 0;
        while (*s >= '0' && *s <= '9') {
            v = v * 10 + (uint64_t)(*s - '0');
            s++;
        }
        if (v) size_mb = v;
    }
    /* Cap at 16 GiB (16384 MiB) for sanity; cap at 1 GiB on i386 so
     * we don't blow the 32-bit address space. */
#if defined(__i386__)
    if (size_mb > 1024) size_mb = 1024;
#else
    if (size_mb > 16384) size_mb = 16384;
#endif
    g_region_size = (size_t)(size_mb * 1024ull * 1024ull);
    return 1;
}

void sud_inramfs_init(void)
{
    if (g_init_done) return;
    g_init_done = 1;

    if (!parse_env()) return;            /* no mount configured */

    compose_shm_path(getenv("SUD_INRAMFS_KEY"),
                     g_mount_path, g_shm_path, sizeof(g_shm_path));

    int created = 0;
    int fd = open_or_create_shm(g_shm_path, g_region_size, &created);
    if (fd < 0) return;                  /* can't attach — addin stays inactive */

    void *want = fixed_addr();
    long mflags = MAP_SHARED | MAP_FIXED;
    void *base = raw_mmap(want, g_region_size,
                          PROT_READ | PROT_WRITE,
                          (int)(MAP_SHARED | MAP_FIXED_NOREPLACE),
                          fd, 0);
    /* mmap returns the new mapping address on success or -errno
     * (in [-4095, -1]) on failure.  Cast to unsigned long so the
     * test works on i386 where a legitimate high address like
     * 0x80000000 looks negative when interpreted as signed long. */
    if ((unsigned long)base >= (unsigned long)-4095) {
        base = raw_mmap(want, g_region_size,
                        PROT_READ | PROT_WRITE,
                        (int)mflags, fd, 0);
    }
    raw_close(fd);
    if ((unsigned long)base >= (unsigned long)-4095) {
        return;
    }

    sud_ir_base = (volatile char *)base;
    struct sud_ir_super *sb = sud_ir_sb();

    if (created) {
        /* We created the region; we own init. */
        init_region(sb, g_region_size);
        sys_futex(&sb->init_state, FUTEX_WAKE, 0x7fffffff);
    } else {
        /* Race winner may still be initialising. */
        for (int spins = 0; spins < 1000; spins++) {
            if (__atomic_load_n(&sb->magic, __ATOMIC_ACQUIRE) == SUD_IR_MAGIC)
                break;
            __asm__ volatile("pause" ::: "memory");
        }
        if (__atomic_load_n(&sb->magic, __ATOMIC_ACQUIRE) != SUD_IR_MAGIC) {
            /* Try to claim init for ourselves. */
            uint32_t expected = 0;
            if (__atomic_compare_exchange_n(&sb->init_state, &expected, 1u,
                                            0, __ATOMIC_ACQ_REL,
                                            __ATOMIC_RELAXED)) {
                init_region(sb, g_region_size);
                sys_futex(&sb->init_state, FUTEX_WAKE, 0x7fffffff);
            } else {
                wait_for_init(&sb->init_state);
            }
        }
    }

    if (__atomic_load_n(&sb->magic, __ATOMIC_ACQUIRE) != SUD_IR_MAGIC) {
        /* Init failed; leave inactive. */
        return;
    }
    g_active = 1;
}

/* ================================================================
 * Test-only: tear down the in-process attachment.
 * ================================================================ */
void sud_inramfs_reset_for_testing(void)
{
    if (sud_ir_base && g_region_size) {
        munmap((void *)sud_ir_base, g_region_size);
    }
    sud_ir_base = 0;
    g_active = 0;
    g_init_done = 0;
    g_mount_len = 0;
    g_mount_path[0] = '\0';
    g_region_size = 0;
    g_shm_path[0] = '\0';
}

void sud_inramfs_unlink_backing_for_testing(void)
{
    if (g_shm_path[0])
        raw_unlinkat(AT_FDCWD, g_shm_path, 0);
}
