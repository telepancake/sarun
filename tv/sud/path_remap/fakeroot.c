/*
 * sud/path_remap/fakeroot.c — fakeroot-style metadata override layer.
 *
 * See fakeroot.h for the public contract.  Brief recap of the moving
 * parts here:
 *
 *   1. Prefix table — a small fixed array of `fakeroot:<abs_prefix>`
 *      rules, walked in registration order by sud_fakeroot_match().
 *      Match semantics are the same as overlay.c's path_under():
 *      strncmp + path-component-boundary check.
 *
 *   2. Override table — a fixed-size open-addressed hash table keyed
 *      by (st_dev, st_ino) and storing recorded uid/gid/mode plus a
 *      bitmask of which fields are set.  The dispatcher records
 *      entries on chown/chmod and reads them back on stat to patch
 *      the kernel-written buffer.
 *
 *   3. Stat-buffer patchers — two architecture-specific layouts (the
 *      x86_64 newfstatat layout and the i386 fstatat64 layout).  We
 *      re-read dev/ino from the kernel-filled buffer rather than
 *      requiring the caller to pass them in: the kernel sets them
 *      atomically with the rest of the metadata, so they're the most
 *      reliable identifier we can use without re-statting the file.
 *
 * All filesystem operations happen via raw syscalls because this code
 * runs from inside the SIGSYS handler.
 */

#include "sud/path_remap/fakeroot.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"

/* ----------------------------------------------------------------
 *  Stat layouts.  Mirrors the two definitions in overlay.c — kept
 *  separate because each TU is built standalone and the layout is a
 *  TU-private detail.  Both have st_dev / st_ino / st_mode / st_uid /
 *  st_gid at the offsets the kernel writes for newfstatat /
 *  fstatat64 respectively.
 * ---------------------------------------------------------------- */
struct sud_fr_stat_x64 {
    unsigned long long st_dev;        /*  0 */
    unsigned long long st_ino;        /*  8 */
    unsigned long long st_nlink;      /* 16 */
    unsigned int       st_mode;       /* 24 */
    unsigned int       st_uid;        /* 28 */
    unsigned int       st_gid;        /* 32 */
    unsigned int       __pad0;        /* 36 */
    unsigned long long st_rdev;       /* 40 */
    long long          st_size;       /* 48 */
    long long          st_blksize;    /* 56 */
    long long          st_blocks;     /* 64 */
    long long          __rest[16];
};

struct sud_fr_stat64 {
    unsigned long long st_dev;        /*  0 */
    unsigned char      __pad0[4];     /*  8 */
    unsigned long      __st_ino;      /* 12 */
    unsigned int       st_mode;       /* 16 */
    unsigned int       st_nlink;      /* 20 */
    unsigned long      st_uid;        /* 24 */
    unsigned long      st_gid;        /* 28 */
    unsigned long long st_rdev;       /* 32 */
    unsigned char      __pad3[4];     /* 40 */
    long long          st_size;       /* 44 */
    unsigned long      st_blksize;    /* 52 */
    unsigned long long st_blocks;     /* 56 */
    unsigned long long st_ino;        /* 64 — 64-bit ino at the end of struct stat64 */
    unsigned long      __rest[16];
};

/* ----------------------------------------------------------------
 *  Prefix table
 * ---------------------------------------------------------------- */

#define SUD_FR_MAX_PREFIXES 16

struct sud_fr_prefix {
    const char *path;
    size_t      len;
};

static struct sud_fr_prefix g_prefixes[SUD_FR_MAX_PREFIXES];
static int                  g_prefix_count;
static int                  g_init_done;

static int fr_is_path_boundary(char c)
{
    return c == '\0' || c == '/';
}

static char *fr_dup_range(const char *start, size_t n)
{
    char *p = (char *)malloc(n + 1);
    if (!p) return 0;
    for (size_t i = 0; i < n; i++) p[i] = start[i];
    p[n] = '\0';
    return p;
}

/* Register one prefix.  Trailing slashes (except a bare "/") are
 * stripped so "/opt/build/" and "/opt/build" match identically. */
static void fr_register_prefix(const char *spec, size_t len)
{
    if (g_prefix_count >= SUD_FR_MAX_PREFIXES) return;
    while (len > 1 && spec[len - 1] == '/') len--;
    if (len == 0 || spec[0] != '/') return;
    char *p = fr_dup_range(spec, len);
    if (!p) return;
    g_prefixes[g_prefix_count].path = p;
    g_prefixes[g_prefix_count].len  = len;
    g_prefix_count++;
}

void sud_fakeroot_init(void)
{
    if (g_init_done) return;
    g_init_done = 1;

    if (!g_sud_runtime_config_present) return;
    for (int i = 0; i < g_sud_runtime_config.remap_rule_count; i++) {
        const char *r = g_sud_runtime_config.remap_rules[i];
        if (!r || !r[0]) continue;
        const char *colon = r;
        while (*colon && *colon != ':') colon++;
        if (*colon != ':') continue;
        size_t klen = (size_t)(colon - r);
        if (klen != 8) continue;
        if (r[0]!='f' || r[1]!='a' || r[2]!='k' || r[3]!='e' ||
            r[4]!='r' || r[5]!='o' || r[6]!='o' || r[7]!='t')
            continue;
        const char *spec = colon + 1;
        size_t slen = 0;
        while (spec[slen]) slen++;
        fr_register_prefix(spec, slen);
    }
}

int sud_fakeroot_active(void)
{
    return g_prefix_count > 0;
}

int sud_fakeroot_match(const char *abs_path)
{
    if (!abs_path || abs_path[0] != '/') return 0;
    for (int i = 0; i < g_prefix_count; i++) {
        const struct sud_fr_prefix *p = &g_prefixes[i];
        /* Bare "/" prefix matches every absolute path. */
        if (p->len == 1 && p->path[0] == '/') return 1;
        if (strncmp(abs_path, p->path, p->len) != 0) continue;
        if (fr_is_path_boundary(abs_path[p->len])) return 1;
    }
    return 0;
}

/* ----------------------------------------------------------------
 *  Override table
 *
 *  Open-addressed hash, linear probing.  Capacity is fixed at compile
 *  time (4096 slots ≈ a few thousand chowned files).  Real fakeroot
 *  workloads tend to run in the low thousands per package; if we
 *  overflow we silently drop the new entry and the program sees the
 *  on-disk uid/gid for that file (documented degradation).
 * ---------------------------------------------------------------- */

#define SUD_FR_TABLE_SIZE 4096

struct sud_fr_entry {
    unsigned long long dev;
    unsigned long long ino;
    unsigned int       uid;
    unsigned int       gid;
    unsigned int       mode;       /* permission bits only (mask 07777) */
    unsigned int       flags;      /* SUD_FAKEROOT_HAS_* */
    unsigned int       used;       /* 0 = empty slot */
};

static struct sud_fr_entry g_overrides[SUD_FR_TABLE_SIZE];

static unsigned long fr_hash(unsigned long long dev, unsigned long long ino)
{
    /* xor-shift mix; sufficient for the small table size. */
    unsigned long long h = ino * 1099511628211ULL;
    h ^= dev * 14695981039346656037ULL;
    h ^= h >> 33;
    h *= 1099511628211ULL;
    h ^= h >> 33;
    return (unsigned long)h & (SUD_FR_TABLE_SIZE - 1);
}

/* Locate the slot for (dev, ino).  If `create` is non-zero and no
 * existing slot is found, allocate one (initialised used=1, flags=0,
 * uid/gid/mode unset).  Returns NULL if the table is full and no
 * existing entry matches. */
static struct sud_fr_entry *fr_slot(unsigned long long dev,
                                     unsigned long long ino,
                                     int create)
{
    unsigned long h = fr_hash(dev, ino);
    for (unsigned i = 0; i < SUD_FR_TABLE_SIZE; i++) {
        unsigned long idx = (h + i) & (SUD_FR_TABLE_SIZE - 1);
        struct sud_fr_entry *e = &g_overrides[idx];
        if (e->used && e->dev == dev && e->ino == ino) return e;
        if (!e->used) {
            if (!create) return 0;
            e->used  = 1;
            e->dev   = dev;
            e->ino   = ino;
            e->flags = 0;
            return e;
        }
    }
    return 0;  /* table full; caller silently drops the override */
}

void sud_fakeroot_record_chown(unsigned long long dev,
                                unsigned long long ino,
                                int uid, int gid)
{
    struct sud_fr_entry *e = fr_slot(dev, ino, 1);
    if (!e) return;
    if (uid != -1) {
        e->uid    = (unsigned int)uid;
        e->flags |= SUD_FAKEROOT_HAS_UID;
    }
    if (gid != -1) {
        e->gid    = (unsigned int)gid;
        e->flags |= SUD_FAKEROOT_HAS_GID;
    }
}

void sud_fakeroot_record_chmod(unsigned long long dev,
                                unsigned long long ino,
                                unsigned int mode)
{
    struct sud_fr_entry *e = fr_slot(dev, ino, 1);
    if (!e) return;
    e->mode   = mode & 07777u;
    e->flags |= SUD_FAKEROOT_HAS_MODE;
}

unsigned sud_fakeroot_lookup(unsigned long long dev,
                              unsigned long long ino,
                              unsigned int *uid_out,
                              unsigned int *gid_out,
                              unsigned int *mode_out)
{
    struct sud_fr_entry *e = fr_slot(dev, ino, 0);
    if (!e || !e->used) return 0;
    if ((e->flags & SUD_FAKEROOT_HAS_UID) && uid_out)  *uid_out  = e->uid;
    if ((e->flags & SUD_FAKEROOT_HAS_GID) && gid_out)  *gid_out  = e->gid;
    if ((e->flags & SUD_FAKEROOT_HAS_MODE) && mode_out) *mode_out = e->mode;
    return e->flags;
}

/* ----------------------------------------------------------------
 *  Stat-buffer patchers
 *
 *  The kernel has just written its native stat layout into the user
 *  buffer.  We re-read (st_dev, st_ino) out of the buffer, look up
 *  the override, and overwrite uid/gid/mode in place.  Mode patching
 *  preserves the file-type bits (S_IFMT) the kernel set; only the
 *  permission bits (07777) come from the override.
 * ---------------------------------------------------------------- */

void sud_fakeroot_patch_kernel_stat(void *buf)
{
    if (!buf || g_prefix_count == 0) return;
    struct sud_fr_stat_x64 *st = (struct sud_fr_stat_x64 *)buf;
    unsigned int uid = st->st_uid;
    unsigned int gid = st->st_gid;
    unsigned int mode_bits = st->st_mode & 07777u;
    unsigned f = sud_fakeroot_lookup(st->st_dev, st->st_ino,
                                      &uid, &gid, &mode_bits);
    if (!f) return;
    if (f & SUD_FAKEROOT_HAS_UID)  st->st_uid = uid;
    if (f & SUD_FAKEROOT_HAS_GID)  st->st_gid = gid;
    if (f & SUD_FAKEROOT_HAS_MODE) {
        unsigned int type = st->st_mode & ~07777u;
        st->st_mode = type | (mode_bits & 07777u);
    }
}

void sud_fakeroot_patch_kernel_stat64(void *buf)
{
    if (!buf || g_prefix_count == 0) return;
    struct sud_fr_stat64 *st = (struct sud_fr_stat64 *)buf;
    unsigned int uid = (unsigned int)st->st_uid;
    unsigned int gid = (unsigned int)st->st_gid;
    unsigned int mode_bits = st->st_mode & 07777u;
    unsigned f = sud_fakeroot_lookup(st->st_dev, st->st_ino,
                                      &uid, &gid, &mode_bits);
    if (!f) return;
    if (f & SUD_FAKEROOT_HAS_UID)  st->st_uid = uid;
    if (f & SUD_FAKEROOT_HAS_GID)  st->st_gid = gid;
    if (f & SUD_FAKEROOT_HAS_MODE) {
        unsigned int type = st->st_mode & ~07777u;
        st->st_mode = type | (mode_bits & 07777u);
    }
}

/* ----------------------------------------------------------------
 *  Test harness reset
 * ---------------------------------------------------------------- */

void sud_fakeroot_reset_for_testing(void)
{
    for (int i = 0; i < g_prefix_count; i++) {
        if (g_prefixes[i].path) free((void *)g_prefixes[i].path);
        g_prefixes[i].path = 0;
        g_prefixes[i].len  = 0;
    }
    g_prefix_count = 0;
    g_init_done = 0;
    memset(g_overrides, 0, sizeof(g_overrides));
}
