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

volatile char *sud_ir_base;          /* base of the metadata mapping */
volatile char *sud_ir_data_base;     /* base of the data shm mapping */

/* Mount config (parsed once from SUD_INRAMFS). */
static char   g_mount_path[PATH_MAX];
static size_t g_mount_len;
static size_t g_meta_size;           /* metadata region bytes */
static size_t g_data_size;           /* data shm bytes */
static int    g_init_done;
static int    g_active;              /* 1 once attach succeeded */
static char   g_meta_shm_path[PATH_MAX];
static char   g_data_shm_path[PATH_MAX];
static int    g_data_fd = -1;        /* kept open for FALLOC_FL_PUNCH_HOLE */

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
        if (ino->type == SUD_IR_T_REG) {
            uint32_t b = ino->u.reg.head_block;
            while (b) {
                uint32_t next = sud_ir_fat()[b];
                sud_ir_fat_free(b);
                b = next;
            }
            ino->u.reg.head_block = 0;
            ino->u.reg.nblocks    = 0;
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
 * FAT allocator over the data shm — lock-free
 *
 * The data shm is a separate /dev/shm object holding only file
 * content, divided into fixed-size SUD_IR_BLOCK_SIZE blocks.
 * Allocation state lives entirely in the metadata region: a
 * uint32_t array of (fat_count + 1) entries (`fat[]`) plus a tagged
 * 64-bit free-list head in the super.  Index 0 is reserved (end-of-
 * chain sentinel); valid block ids are 1..fat_count.
 *
 * For an in-use block, fat[id] holds the next-block-id in the
 * file's chain (0 if last).  For a free block, fat[id] holds the
 * next free-block-id (Treiber stack).
 *
 * Concurrency: alloc/free are wait-free single-block operations
 * implemented as CAS pop/push on `sb->fat_free_head_tagged`.  The
 * tagged head packs an ABA counter into the upper 32 bits so a CAS
 * succeeds iff no other process modified the head between our load
 * and our store — without this, two processes interleaving as
 *   T1: load head=(A,c0); read fat[A]=B
 *   T2: pop A; pop B; push A   (head now (A,c2), but next ≠ B)
 *   T1: CAS head:(A,c0)→(B,c1)   *** would corrupt the free list
 * could install a stale next pointer.  With ABA tagging T1's CAS
 * sees (A,c2) ≠ (A,c0) and retries.
 *
 * Crucially this means callers (chain_append / chain_truncate) do
 * NOT take sb->lock for FAT operations.  Per-inode lock alone
 * protects each file's chain shape, so two writers to two different
 * files don't serialise on a global lock — exactly the property
 * a FAT-style allocator should give us.
 *
 * No headers/footers in the data blocks themselves: hole-punch is
 * page-aligned and never touches a neighbour file's data.
 * ================================================================ */

/* Pack/unpack helpers for the tagged head. */
static inline uint64_t fat_head_pack(uint32_t id, uint32_t tag)
{
    return ((uint64_t)tag << 32) | (uint64_t)id;
}
static inline uint32_t fat_head_id(uint64_t h)  { return (uint32_t)h; }
static inline uint32_t fat_head_tag(uint64_t h) { return (uint32_t)(h >> 32); }

uint32_t sud_ir_fat_alloc(void)
{
    struct sud_ir_super *sb = sud_ir_sb();
    uint32_t *fat = sud_ir_fat();
    /* Cast away `volatile` qualification on the head: the C atomics
     * builtins want a plain pointer; volatility is irrelevant when
     * every access goes through __atomic_*. */
    uint64_t *headp = (uint64_t *)&sb->fat_free_head_tagged;

    for (;;) {
        uint64_t old  = __atomic_load_n(headp, __ATOMIC_ACQUIRE);
        uint32_t id   = fat_head_id(old);
        if (id == 0) return 0;          /* data shm exhausted */
        /* fat[id] read may be stale (a concurrent push/pop could have
         * rewritten it).  That's OK: the CAS below only succeeds if
         * `old` is still the current head — i.e. no other thread
         * touched the free list since our load — so the next we
         * computed is the current free-list-next of id. */
        uint32_t next = __atomic_load_n(&fat[id], __ATOMIC_RELAXED);
        uint64_t neu  = fat_head_pack(next, fat_head_tag(old) + 1);
        if (__atomic_compare_exchange_n(headp, &old, neu,
                                        0, __ATOMIC_ACQ_REL,
                                        __ATOMIC_ACQUIRE)) {
            /* fat[id] is now the caller's to overwrite (chain link
             * pointer or 0 for end-of-chain).  We don't clear it
             * here — chain_append explicitly stores a successor or
             * 0 before the new run is published via fat[old_tail]. */
            __atomic_fetch_sub(&sb->fat_free_count, 1, __ATOMIC_RELAXED);
            return id;
        }
        /* CAS failed — retry. */
    }
}

void sud_ir_fat_free(uint32_t block_id)
{
    struct sud_ir_super *sb = sud_ir_sb();
    if (block_id == 0 || block_id > sb->fat_count) return;
    uint32_t *fat = sud_ir_fat();
    uint64_t *headp = (uint64_t *)&sb->fat_free_head_tagged;

    /* Best-effort hole-punch: drop the page from the data shm so
     * residency tracks live data, not high-water-mark.  Failure
     * (older kernels, tmpfs without punch-hole support) is not
     * fatal — the page just stays around until the shm is unlinked.
     * Done before the CAS publish: even if a concurrent allocator
     * pops `block_id` immediately after we publish, they will fault
     * a fresh zero page in on first access — semantically equivalent
     * to allocating a brand-new block. */
    if (g_data_fd >= 0) {
#ifdef SYS_fallocate
        raw_syscall6(SYS_fallocate, g_data_fd,
                     FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                     (long)(uint64_t)((block_id - 1) * SUD_IR_BLOCK_SIZE),
                     (long)SUD_IR_BLOCK_SIZE, 0, 0);
#endif
    }

    for (;;) {
        uint64_t old = __atomic_load_n(headp, __ATOMIC_ACQUIRE);
        /* Stage our next pointer.  Concurrent pops cannot read
         * fat[block_id] meaningfully until the CAS publishes the
         * push — they wouldn't see block_id as the head until then. */
        __atomic_store_n(&fat[block_id], fat_head_id(old), __ATOMIC_RELAXED);
        uint64_t neu = fat_head_pack(block_id, fat_head_tag(old) + 1);
        if (__atomic_compare_exchange_n(headp, &old, neu,
                                        0, __ATOMIC_ACQ_REL,
                                        __ATOMIC_ACQUIRE)) {
            __atomic_fetch_add(&sb->fat_free_count, 1, __ATOMIC_RELAXED);
            return;
        }
        /* CAS failed — retry; we'll re-stage fat[block_id] next loop. */
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

/* Compose the two shm paths:
 *   /dev/shm/sud-inramfs.<key>.meta     — metadata region
 *   /dev/shm/sud-inramfs.<key>.data     — small-files data store
 *
 * Splitting them lets us mmap each at its own fixed address (the
 * metadata region needs a stable layout; the data shm needs to be
 * hole-punchable without disturbing metadata pages). */
static void compose_shm_paths(const char *user_key,
                              const char *mount_path,
                              char *meta_out, size_t meta_sz,
                              char *data_out, size_t data_sz)
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
    snprintf(meta_out, meta_sz, "/dev/shm/sud-inramfs.%s.meta", key);
    snprintf(data_out, data_sz, "/dev/shm/sud-inramfs.%s.data", key);
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

/* Fixed mapping address for the **data** shm.  Like fixed_addr() but
 * for the small-files store: kept at its own high address so 32-bit
 * and 64-bit can both hand out raw pointers into a file's data via
 * sud_ir_data_block(id), with the address being deterministic across
 * processes that share the same mount. */
static void *fixed_data_addr(void)
{
#if defined(__x86_64__)
    return (void *)0x600000000000UL;
#else
    /* Below the metadata mapping (0x80000000) and above the wrapper
     * (0x20000000); leaves room for both even when the data shm is
     * sized at the i386 cap of 256 MiB. */
    return (void *)0x60000000UL;
#endif
}

/* Initialise a freshly-created **metadata** region.  Caller has the
 * init latch; meta_size is the metadata mapping length and
 * data_blocks is the number of data blocks the FAT must address.
 *
 * Layout (all aligned up to 64 B, then BLOCK_SIZE for the metadata
 * block area):
 *
 *   [0]                              super (struct sud_ir_super)
 *   [aligned]                        inode bitmap (SUD_IR_MAX_INODES bits)
 *   [aligned]                        inode table  (SUD_IR_MAX_INODES * sizeof(inode))
 *   [aligned]                        FAT[data_blocks + 1]  (uint32_t per data block)
 *   [aligned]                        metadata block bitmap
 *   [BLOCK_SIZE-aligned]             metadata blocks (dirents, symlink targets)
 */
static void init_meta_region(struct sud_ir_super *sb,
                             uint64_t meta_size, uint32_t data_blocks)
{
    memset((void *)sb, 0, meta_size);

    sb->version     = SUD_IR_VERSION;
    sb->region_size = meta_size;
    sb->data_shm_size = (uint64_t)data_blocks * SUD_IR_BLOCK_SIZE;

    uint32_t off = (uint32_t)((sizeof(*sb) + 63) & ~63u);

    sb->inode_count       = SUD_IR_MAX_INODES;
    sb->inode_bitmap_off  = off;
    off += (sb->inode_count + 7) / 8;
    off = (off + 63) & ~63u;

    sb->inode_table_off   = off;
    off += sb->inode_count * (uint32_t)sizeof(struct sud_ir_inode);
    off = (off + 63) & ~63u;

    /* FAT: one uint32_t per data block plus index 0 (sentinel). */
    sb->fat_off   = off;
    sb->fat_count = data_blocks;
    off += (data_blocks + 1) * (uint32_t)sizeof(uint32_t);
    off = (off + 63) & ~63u;

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

    /* Build the FAT free list: thread blocks 1..data_blocks together,
     * 1 → 2 → ... → data_blocks → 0.  Allocations pop from the head;
     * the LIFO order means freshly-freed blocks are reused first
     * (good for kernel page-cache locality). */
    uint32_t *fat = (uint32_t *)((char *)sb + sb->fat_off);
    fat[0] = 0;                  /* end-of-chain sentinel */
    for (uint32_t i = 1; i < data_blocks; i++) fat[i] = i + 1;
    if (data_blocks) fat[data_blocks] = 0;
    sb->fat_free_head_tagged = data_blocks ? fat_head_pack(1, 0) : 0;
    sb->fat_free_count       = data_blocks;

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

/* Parse SUD_INRAMFS=<path>[:<size_mb>].
 *
 * `size_mb` sizes the small-files **data** shm.  The metadata region
 * is sized separately based on inode/dirent capacity (see
 * choose_meta_size); it doesn't need to scale with file content. */
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

    /* Data-shm size. */
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
    /* Cap at 16 GiB on 64-bit; cap at 256 MiB on i386 so the data
     * shm fits comfortably in the 32-bit address space alongside the
     * metadata region, the wrapper, and the traced program. */
#if defined(__i386__)
    if (size_mb > 256) size_mb = 256;
#else
    if (size_mb > 16384) size_mb = 16384;
#endif
    g_data_size = (size_t)(size_mb * 1024ull * 1024ull);

    /* Metadata sizing: enough for the default inode/dirent budget
     * plus the FAT.  Computed lazily in choose_meta_size below. */
    return 1;
}

/* Choose the metadata region size.  Must hold:
 *   - super
 *   - inode bitmap (SUD_IR_MAX_INODES bits)
 *   - inode table  (SUD_IR_MAX_INODES * sizeof(inode))
 *   - metadata block bitmap + a few hundred 4 KiB metadata blocks
 *     (for dirents and symlinks)
 *   - FAT table sized to (data_size / BLOCK_SIZE + 1) uint32_ts
 *
 * The result is rounded up to a 4 KiB page. */
static size_t choose_meta_size(size_t data_size)
{
    size_t inode_table = SUD_IR_MAX_INODES * sizeof(struct sud_ir_inode);
    size_t inode_bitmap = (SUD_IR_MAX_INODES + 7) / 8;
    size_t fat_entries = data_size / SUD_IR_BLOCK_SIZE + 1;
    size_t fat_bytes   = fat_entries * sizeof(uint32_t);
    /* Reserve room for ~1024 metadata blocks (dirents + symlinks);
     * far more than any sane filesystem needs. */
    size_t meta_blocks = 1024;
    size_t meta_block_data = meta_blocks * SUD_IR_BLOCK_SIZE;
    size_t meta_block_bitmap = (meta_blocks + 7) / 8;

    size_t total = sizeof(struct sud_ir_super)
                 + inode_bitmap + inode_table
                 + fat_bytes
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
 * older kernels.  Returns the mapped address or NULL on failure. */
static void *map_at_fixed(void *want, size_t size, int fd)
{
    int mflags = MAP_SHARED | MAP_FIXED;
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

    g_meta_size = choose_meta_size(g_data_size);

    compose_shm_paths(getenv("SUD_INRAMFS_KEY"), g_mount_path,
                      g_meta_shm_path, sizeof(g_meta_shm_path),
                      g_data_shm_path, sizeof(g_data_shm_path));

    /* ---- Attach metadata shm. ---- */
    int meta_created = 0;
    int meta_fd = open_or_create_shm_at(g_meta_shm_path, g_meta_size,
                                        &meta_created);
    if (meta_fd < 0) return;
    void *meta_base = map_at_fixed(fixed_addr(), g_meta_size, meta_fd);
    raw_close(meta_fd);
    if (!meta_base) return;
    sud_ir_base = (volatile char *)meta_base;

    /* ---- Attach data shm. ----
     * Held open across the lifetime of the addin so sud_ir_fat_free
     * can punch holes in it. */
    int data_created = 0;
    g_data_fd = open_or_create_shm_at(g_data_shm_path, g_data_size,
                                      &data_created);
    if (g_data_fd < 0) {
        munmap((void *)sud_ir_base, g_meta_size);
        sud_ir_base = 0;
        return;
    }
    void *data_base = map_at_fixed(fixed_data_addr(), g_data_size, g_data_fd);
    if (!data_base) {
        raw_close(g_data_fd); g_data_fd = -1;
        munmap((void *)sud_ir_base, g_meta_size);
        sud_ir_base = 0;
        return;
    }
    sud_ir_data_base = (volatile char *)data_base;

    /* ---- Initialise the metadata region (one process wins). ---- */
    struct sud_ir_super *sb = sud_ir_sb();
    uint32_t data_blocks = (uint32_t)(g_data_size / SUD_IR_BLOCK_SIZE);
    if (meta_created) {
        init_meta_region(sb, g_meta_size, data_blocks);
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
                init_meta_region(sb, g_meta_size, data_blocks);
                sys_futex(&sb->init_state, FUTEX_WAKE, 0x7fffffff);
            } else {
                wait_for_init(&sb->init_state);
            }
        }
    }

    if (__atomic_load_n(&sb->magic, __ATOMIC_ACQUIRE) != SUD_IR_MAGIC) {
        /* Init failed somewhere; leave inactive but do not orphan
         * the data fd. */
        munmap((void *)sud_ir_data_base, g_data_size);
        munmap((void *)sud_ir_base,      g_meta_size);
        raw_close(g_data_fd); g_data_fd = -1;
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
    if (sud_ir_data_base && g_data_size) {
        munmap((void *)sud_ir_data_base, g_data_size);
    }
    if (g_data_fd >= 0) raw_close(g_data_fd);
    sud_ir_base      = 0;
    sud_ir_data_base = 0;
    g_data_fd        = -1;
    g_active         = 0;
    g_init_done      = 0;
    g_mount_len      = 0;
    g_mount_path[0]  = '\0';
    g_meta_size      = 0;
    g_data_size      = 0;
    g_meta_shm_path[0] = '\0';
    g_data_shm_path[0] = '\0';
}

void sud_inramfs_unlink_backing_for_testing(void)
{
    if (g_meta_shm_path[0])
        raw_unlinkat(AT_FDCWD, g_meta_shm_path, 0);
    if (g_data_shm_path[0])
        raw_unlinkat(AT_FDCWD, g_data_shm_path, 0);
}
