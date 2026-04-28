/*
 * sud/path_remap/overlay.c — Overlayfs-style path remapping core.
 *
 * See overlay.h for the public API and configuration syntax.
 *
 * This file is shared between two consumers:
 *
 *   1. sud32 / sud64 wrappers — built freestanding, called from the
 *      SIGSYS handler.  All filesystem operations therefore go through
 *      raw_syscall6() (libc-fs's libc wrappers also work but raw is
 *      preferred to avoid touching errno from signal context).
 *
 *   2. sud/path_remap/tests/test_overlay.c — built freestanding the
 *      same way (links libc-fs sources directly), exercises the same
 *      code paths from a normal main().
 */

#include "sud/path_remap/overlay.h"
#include "sud/raw.h"

/* ----------------------------------------------------------------
 *  Architecture-specific stat layout (we only need st_mode and
 *  st_rdev).  The kernel writes a different stat layout on i386
 *  (struct stat64 / fstatat64) than on x86_64 (struct stat /
 *  newfstatat), so we declare both and use the one matching the
 *  syscall raw_fstatat() actually issues.
 * ---------------------------------------------------------------- */
#if defined(__x86_64__)
struct sud_overlay_stat {
    unsigned long st_dev;        /*  0 */
    unsigned long st_ino;        /*  8 */
    unsigned long st_nlink;      /* 16 */
    unsigned int  st_mode;       /* 24 */
    unsigned int  st_uid;        /* 28 */
    unsigned int  st_gid;        /* 32 */
    int           __pad0;        /* 36 */
    unsigned long st_rdev;       /* 40 */
    long          st_size;       /* 48 */
    long          st_blksize;    /* 56 */
    long          st_blocks;     /* 64 */
    long          __rest[16];    /* padding */
};
#else
struct sud_overlay_stat {
    unsigned long long st_dev;       /*  0 */
    unsigned char  __pad0[4];        /*  8 */
    unsigned long  __st_ino;         /* 12 */
    unsigned int   st_mode;          /* 16 */
    unsigned int   st_nlink;         /* 20 */
    unsigned long  st_uid;           /* 24 */
    unsigned long  st_gid;           /* 28 */
    unsigned long long st_rdev;      /* 32 */
    unsigned char  __pad3[4];        /* 40 */
    long long      st_size;          /* 44 */
    unsigned long  st_blksize;       /* 52 */
    unsigned long long st_blocks;    /* 56 */
    unsigned long  __rest[16];       /* padding */
};
#endif

#ifndef S_IFMT
#define S_IFMT  0170000
#endif
#ifndef S_IFDIR
#define S_IFDIR 0040000
#endif
#ifndef S_IFCHR
#define S_IFCHR 0020000
#endif
#ifndef S_IFREG
#define S_IFREG 0100000
#endif
#ifndef S_IFLNK
#define S_IFLNK 0120000
#endif

#ifndef AT_SYMLINK_NOFOLLOW
#define AT_SYMLINK_NOFOLLOW 0x100
#endif

/* AT_REMOVEDIR (unlinkat flag) — distinguishes rmdir-mode unlinkat. */
#ifndef AT_REMOVEDIR
#define AT_REMOVEDIR 0x200
#endif

/* ----------------------------------------------------------------
 *  Raw helper syscalls we need but that aren't in sud/raw.h.
 *  Defined here as static inlines so the test driver gets the same
 *  helpers without pulling in extra headers.
 * ---------------------------------------------------------------- */
static inline long sud_ov_lstat(const char *path,
                                struct sud_overlay_stat *st)
{
#ifdef SYS_newfstatat
    return raw_syscall6(SYS_newfstatat, AT_FDCWD, (long)path, (long)st,
                        AT_SYMLINK_NOFOLLOW, 0, 0);
#else
    return raw_syscall6(__NR_fstatat64, AT_FDCWD, (long)path, (long)st,
                        AT_SYMLINK_NOFOLLOW, 0, 0);
#endif
}

static inline long sud_ov_mkdir(const char *path, int mode)
{
#ifdef __NR_mkdirat
    return raw_syscall6(__NR_mkdirat, AT_FDCWD, (long)path, mode, 0, 0, 0);
#else
    return raw_syscall6(__NR_mkdir, (long)path, mode, 0, 0, 0, 0);
#endif
}

static inline long sud_ov_mknod(const char *path, unsigned int mode,
                                unsigned int dev)
{
#ifdef __NR_mknodat
    return raw_syscall6(__NR_mknodat, AT_FDCWD, (long)path, mode, dev,
                        0, 0);
#else
    return raw_syscall6(__NR_mknod, (long)path, mode, dev, 0, 0, 0);
#endif
}

static inline long sud_ov_symlink(const char *target, const char *linkpath)
{
#ifdef __NR_symlinkat
    return raw_syscall6(__NR_symlinkat, (long)target, AT_FDCWD,
                        (long)linkpath, 0, 0, 0);
#else
    return raw_syscall6(__NR_symlink, (long)target, (long)linkpath,
                        0, 0, 0, 0);
#endif
}

static inline long sud_ov_unlink(const char *path)
{
#ifdef __NR_unlinkat
    return raw_syscall6(__NR_unlinkat, AT_FDCWD, (long)path, 0,
                        0, 0, 0);
#else
    return raw_syscall6(__NR_unlink, (long)path, 0, 0, 0, 0, 0);
#endif
}

static inline long sud_ov_getpid(void)
{
    return raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
}

/* ----------------------------------------------------------------
 *  Configuration storage.  Rules are owned by the addin for the
 *  lifetime of the process; never freed.
 * ---------------------------------------------------------------- */
#define SUD_OVERLAY_MAX_RULES   16
#define SUD_OVERLAY_MAX_LOWERS  16

struct sud_overlay_rule {
    /* Merged mount point ("/merged"). */
    const char *merged;
    size_t      merged_len;

    /* Upper (writable) layer; NULL or empty if read-only overlay. */
    const char *upper;
    size_t      upper_len;

    /* Lower layers in priority order. */
    const char *lowers[SUD_OVERLAY_MAX_LOWERS];
    size_t      lower_lens[SUD_OVERLAY_MAX_LOWERS];
    int         lower_count;

    /* When set, this rule was created from SUD_REMAP, not SUD_OVERLAY:
     * `upper` and the single `lowers[0]` are the same path, no whiteout
     * semantics are applied, and read/write resolve identically to the
     * legacy 1:1 remap behaviour. */
    int         simple;
};

static struct sud_overlay_rule g_rules[SUD_OVERLAY_MAX_RULES];
static int g_rule_count;
static int g_init_done;

/* ----------------------------------------------------------------
 *  Tiny utility helpers (kept local: libc-fs has strlen/memcpy/etc.
 *  but a few tasks below want bounded/explicit behaviour).
 * ---------------------------------------------------------------- */
static int is_path_boundary(char c)
{
    return c == '\0' || c == '/';
}

static char *dup_range(const char *start, size_t n)
{
    char *p = (char *)malloc(n + 1);
    if (!p) return 0;
    for (size_t i = 0; i < n; i++) p[i] = start[i];
    p[n] = '\0';
    return p;
}

/* Append `tail` to `head` into out[0..out_sz).  `head` is `head_len`
 * bytes (no NUL); `tail` is NUL-terminated.  Returns 0 on success,
 * -1 on overflow. */
static int compose(char *out, size_t out_sz,
                   const char *head, size_t head_len,
                   const char *tail)
{
    size_t tail_len = strlen(tail);
    if (head_len + tail_len + 1 > out_sz) return -1;
    memcpy(out, head, head_len);
    memcpy(out + head_len, tail, tail_len + 1);
    return 0;
}

/* Test whether `path` lies at or under `prefix` on a path-component
 * boundary.  Returns the relative tail (pointing into `path`, may be
 * "" or starting with '/') on match, or NULL otherwise. */
static const char *path_under(const char *path, const char *prefix,
                              size_t prefix_len)
{
    if (strncmp(path, prefix, prefix_len) != 0) return 0;
    if (!is_path_boundary(path[prefix_len])) return 0;
    return path + prefix_len;
}

/* ----------------------------------------------------------------
 *  Configuration parsing.
 * ---------------------------------------------------------------- */
static void parse_overlay_env(const char *env)
{
    while (*env && g_rule_count < SUD_OVERLAY_MAX_RULES) {
        /* Read merged path until '=' or ':' / end. */
        const char *merged = env;
        while (*env && *env != '=' && *env != ':') env++;
        if (*env != '=') {
            /* Malformed: skip up to next ':'. */
            while (*env && *env != ':') env++;
            if (*env == ':') env++;
            continue;
        }
        size_t merged_len = (size_t)(env - merged);
        env++;  /* skip '=' */

        struct sud_overlay_rule *r = &g_rules[g_rule_count];
        r->merged = 0; r->upper = 0; r->lower_count = 0; r->simple = 0;

        if (merged_len == 0) goto skip;
        r->merged = dup_range(merged, merged_len);
        r->merged_len = merged_len;
        if (!r->merged) goto skip;

        /* Upper (may be empty: "+lower1+lower2"). */
        const char *upper = env;
        while (*env && *env != '+' && *env != ':') env++;
        size_t upper_len = (size_t)(env - upper);
        if (upper_len > 0) {
            r->upper = dup_range(upper, upper_len);
            r->upper_len = upper_len;
            if (!r->upper) goto skip;
        }

        /* Lowers: each '+'-separated path. */
        while (*env == '+' && r->lower_count < SUD_OVERLAY_MAX_LOWERS) {
            env++;
            const char *low = env;
            while (*env && *env != '+' && *env != ':') env++;
            size_t low_len = (size_t)(env - low);
            if (low_len == 0) continue;
            char *p = dup_range(low, low_len);
            if (!p) goto skip;
            r->lowers[r->lower_count] = p;
            r->lower_lens[r->lower_count] = low_len;
            r->lower_count++;
        }

        /* A rule must have at least an upper or one lower to be usable. */
        if (r->upper || r->lower_count > 0) {
            g_rule_count++;
        }
skip:
        while (*env && *env != ':') env++;
        if (*env == ':') env++;
    }
}

static void parse_remap_env(const char *env)
{
    while (*env && g_rule_count < SUD_OVERLAY_MAX_RULES) {
        const char *src = env;
        while (*env && *env != '=' && *env != ':') env++;
        if (*env != '=') {
            while (*env && *env != ':') env++;
            if (*env == ':') env++;
            continue;
        }
        size_t src_len = (size_t)(env - src);
        env++;
        const char *dst = env;
        while (*env && *env != ':') env++;
        size_t dst_len = (size_t)(env - dst);
        if (src_len && dst_len) {
            struct sud_overlay_rule *r = &g_rules[g_rule_count];
            r->merged = dup_range(src, src_len);
            r->merged_len = src_len;
            r->upper = dup_range(dst, dst_len);
            r->upper_len = dst_len;
            r->lowers[0] = r->upper;
            r->lower_lens[0] = r->upper_len;
            r->lower_count = 1;
            r->simple = 1;
            if (r->merged && r->upper)
                g_rule_count++;
        }
        if (*env == ':') env++;
    }
}

void sud_overlay_init(void)
{
    if (g_init_done) return;
    g_init_done = 1;
    const char *ov = getenv("SUD_OVERLAY");
    if (ov && ov[0]) parse_overlay_env(ov);
    const char *rm = getenv("SUD_REMAP");
    if (rm && rm[0]) parse_remap_env(rm);
}

int sud_overlay_rule_count(void)
{
    return g_rule_count;
}

void sud_overlay_reset_for_testing(void)
{
    /* Free is not strictly necessary (process-lifetime) but tests run
     * reset between sub-tests; the leaked dup_range strings are tiny
     * and bounded by the number of rules tested. */
    for (int i = 0; i < g_rule_count; i++) {
        struct sud_overlay_rule *r = &g_rules[i];
        if (r->merged) free((void *)r->merged);
        if (r->upper) free((void *)r->upper);
        for (int j = 0; j < r->lower_count; j++) {
            if (r->simple) break;  /* lowers[0] aliases upper */
            if (r->lowers[j]) free((void *)r->lowers[j]);
        }
    }
    memset(g_rules, 0, sizeof(g_rules));
    g_rule_count = 0;
    g_init_done = 0;
}

/* ----------------------------------------------------------------
 *  Overlay lookup primitives.
 * ---------------------------------------------------------------- */

/* Find the rule whose merged prefix `path` lies under.  Returns the
 * rule and writes `*tail_out` to point at the trailing relative path
 * (may be "" or "/..."), or NULL if no match. */
static const struct sud_overlay_rule *
find_rule(const char *path, const char **tail_out)
{
    if (!path || path[0] != '/') return 0;
    for (int i = 0; i < g_rule_count; i++) {
        const struct sud_overlay_rule *r = &g_rules[i];
        const char *tail = path_under(path, r->merged, r->merged_len);
        if (tail) {
            *tail_out = tail;
            return r;
        }
    }
    return 0;
}

/* Stat `path` (no follow).  Returns kernel-style return code:
 *   0  — exists; *st filled
 *   <0 — -errno
 */
static long stat_one(const char *path, struct sud_overlay_stat *st)
{
    return sud_ov_lstat(path, st);
}

/* Is `path` a whiteout? (char-dev with rdev == 0). */
static int is_whiteout_st(const struct sud_overlay_stat *st)
{
    return ((st->st_mode & S_IFMT) == S_IFCHR) && (st->st_rdev == 0);
}

/* Is `path` an opaque dir? (overlayfs marker: trusted.overlay.opaque
 * xattr or a regular char-dev.0 named "...".) We don't model opaque
 * dirs since they require xattr support; lookup just walks all layers.
 * Documented limitation. */

/* Compose <layer><tail> into `out`.  Tail starts with '/' or is "". */
static int compose_layer(char *out, size_t out_sz,
                         const char *layer, size_t layer_len,
                         const char *tail)
{
    return compose(out, out_sz, layer, layer_len, tail);
}

/* Best-effort recursive mkdir of `dir` (each component 0755).
 * Stops on the first non-EEXIST error. */
static void mkdir_p(char *dir)
{
    /* Walk through components, temporarily NUL-terminating at each '/'
     * and mkdir'ing the prefix. */
    if (!dir || dir[0] != '/') return;
    char *p = dir + 1;
    for (;;) {
        while (*p && *p != '/') p++;
        char saved = *p;
        *p = '\0';
        long r = sud_ov_mkdir(dir, 0755);
        (void)r;  /* ignore EEXIST and other errors; final caller will
                   * see the real failure on the actual operation. */
        *p = saved;
        if (!*p) break;
        p++;
    }
}

/* Ensure parent directory of `path` exists in `upper`.  Returns 0 on
 * success or -errno on failure. */
static int ensure_parent_in_upper(const char *upper, size_t upper_len,
                                  const char *tail)
{
    /* Locate the last '/' in `tail`. */
    const char *last = 0;
    for (const char *p = tail; *p; p++)
        if (*p == '/') last = p;
    if (!last || last == tail) return 0;  /* no parent dir to create */
    size_t parent_tail_len = (size_t)(last - tail);
    char buf[PATH_MAX];
    if (upper_len + parent_tail_len + 1 > sizeof(buf)) return -ENAMETOOLONG;
    memcpy(buf, upper, upper_len);
    memcpy(buf + upper_len, tail, parent_tail_len);
    buf[upper_len + parent_tail_len] = '\0';
    mkdir_p(buf);
    return 0;
}

int sud_overlay_resolve(const char *path, int for_write,
                        char *out, size_t out_sz)
{
    if (!g_rule_count) return SUD_OVERLAY_PASSTHROUGH;
    if (!path || path[0] != '/') return SUD_OVERLAY_PASSTHROUGH;

    const char *tail;
    const struct sud_overlay_rule *r = find_rule(path, &tail);
    if (!r) return SUD_OVERLAY_PASSTHROUGH;

    /* Simple SUD_REMAP rule: pass through to the single mapping. */
    if (r->simple) {
        if (compose_layer(out, out_sz, r->upper, r->upper_len, tail) < 0)
            return SUD_OVERLAY_PASSTHROUGH;
        return SUD_OVERLAY_RESOLVED;
    }

    /* Check upper first (always needed: for whiteout detection on read
     * paths, and as the destination for writes). */
    char upath[PATH_MAX];
    int upper_state = 0;  /* 0=absent, 1=whiteout, 2=present */
    struct sud_overlay_stat st;
    if (r->upper) {
        if (compose_layer(upath, sizeof(upath),
                          r->upper, r->upper_len, tail) < 0)
            return SUD_OVERLAY_PASSTHROUGH;
        if (stat_one(upath, &st) == 0) {
            upper_state = is_whiteout_st(&st) ? 1 : 2;
        }
    }

    if (for_write) {
        if (!r->upper) return SUD_OVERLAY_READONLY;
        ensure_parent_in_upper(r->upper, r->upper_len, tail);
        memcpy(out, upath, strlen(upath) + 1);
        if (strlen(upath) + 1 > out_sz) return SUD_OVERLAY_PASSTHROUGH;
        return SUD_OVERLAY_RESOLVED;
    }

    /* Read path. */
    if (upper_state == 1) return SUD_OVERLAY_WHITEOUT;
    if (upper_state == 2) {
        if (strlen(upath) + 1 > out_sz) return SUD_OVERLAY_PASSTHROUGH;
        memcpy(out, upath, strlen(upath) + 1);
        return SUD_OVERLAY_RESOLVED;
    }
    /* Walk lowers. */
    for (int i = 0; i < r->lower_count; i++) {
        char buf[PATH_MAX];
        if (compose_layer(buf, sizeof(buf),
                          r->lowers[i], r->lower_lens[i], tail) < 0)
            continue;
        if (stat_one(buf, &st) == 0) {
            if (strlen(buf) + 1 > out_sz) return SUD_OVERLAY_PASSTHROUGH;
            memcpy(out, buf, strlen(buf) + 1);
            return SUD_OVERLAY_RESOLVED;
        }
    }
    /* Not found in any layer.  Return the upper path so the syscall
     * itself produces -ENOENT against a meaningful location.  If no
     * upper is configured, return the first lower. */
    if (r->upper) {
        if (strlen(upath) + 1 > out_sz) return SUD_OVERLAY_PASSTHROUGH;
        memcpy(out, upath, strlen(upath) + 1);
        return SUD_OVERLAY_RESOLVED;
    }
    if (r->lower_count > 0) {
        if (compose_layer(out, out_sz,
                          r->lowers[0], r->lower_lens[0], tail) < 0)
            return SUD_OVERLAY_PASSTHROUGH;
        return SUD_OVERLAY_RESOLVED;
    }
    return SUD_OVERLAY_PASSTHROUGH;
}

/* ----------------------------------------------------------------
 *  *at-syscall path resolution.
 * ---------------------------------------------------------------- */

/* Per-fd tracking: which dirfds returned by sud_overlay_open_dir map
 * back to which merged path (so that openat(dirfd, "name") can be
 * re-resolved against the full overlay).  Bounded; entries are
 * recycled LRU-style. */
#define SUD_OVERLAY_FD_MAP_SIZE 64
struct sud_fd_map_entry {
    int  fd;            /* -1 = empty */
    char merged_path[PATH_MAX];
};
static struct sud_fd_map_entry g_fd_map[SUD_OVERLAY_FD_MAP_SIZE];
static int g_fd_map_init;

static void fd_map_init(void)
{
    if (g_fd_map_init) return;
    for (int i = 0; i < SUD_OVERLAY_FD_MAP_SIZE; i++)
        g_fd_map[i].fd = -1;
    g_fd_map_init = 1;
}

static void fd_map_remember(int fd, const char *merged_path)
{
    fd_map_init();
    /* Find an empty slot, or replace an existing entry for the same fd
     * (kernel may reuse fd numbers after close). */
    int slot = -1;
    for (int i = 0; i < SUD_OVERLAY_FD_MAP_SIZE; i++) {
        if (g_fd_map[i].fd == fd || g_fd_map[i].fd == -1) {
            slot = i;
            break;
        }
    }
    if (slot < 0) slot = fd % SUD_OVERLAY_FD_MAP_SIZE;
    g_fd_map[slot].fd = fd;
    size_t n = strlen(merged_path);
    if (n >= sizeof(g_fd_map[slot].merged_path))
        n = sizeof(g_fd_map[slot].merged_path) - 1;
    memcpy(g_fd_map[slot].merged_path, merged_path, n);
    g_fd_map[slot].merged_path[n] = '\0';
}

static const char *fd_map_lookup(int fd)
{
    if (!g_fd_map_init || fd < 0) return 0;
    for (int i = 0; i < SUD_OVERLAY_FD_MAP_SIZE; i++) {
        if (g_fd_map[i].fd == fd)
            return g_fd_map[i].merged_path;
    }
    return 0;
}

/* Read /proc/self/cwd.  Returns 0 on success or -errno. */
static long read_cwd(char *out, size_t out_sz)
{
    long n = raw_syscall6(SYS_readlinkat, AT_FDCWD,
                          (long)"/proc/self/cwd",
                          (long)out, out_sz - 1, 0, 0);
    if (n < 0) return n;
    out[n] = '\0';
    return 0;
}

/* Build the absolute "merged" path that `(dirfd, path)` refers to. */
static int resolve_at_to_abs(int dirfd, const char *path,
                             char *out, size_t out_sz)
{
    if (!path) return -1;
    if (path[0] == '/') {
        size_t n = strlen(path);
        if (n + 1 > out_sz) return -1;
        memcpy(out, path, n + 1);
        return 0;
    }
    /* Relative.  Locate the base directory. */
    if (dirfd == AT_FDCWD) {
        char cwd[PATH_MAX];
        if (read_cwd(cwd, sizeof(cwd)) != 0) return -1;
        size_t cwd_len = strlen(cwd);
        size_t plen = strlen(path);
        /* cwd + "/" + path */
        if (cwd_len + 1 + plen + 1 > out_sz) return -1;
        memcpy(out, cwd, cwd_len);
        out[cwd_len] = '/';
        memcpy(out + cwd_len + 1, path, plen + 1);
        return 0;
    }
    const char *merged = fd_map_lookup(dirfd);
    if (!merged) return -1;  /* unknown dirfd — caller passes through */
    size_t mlen = strlen(merged);
    size_t plen = strlen(path);
    if (mlen + 1 + plen + 1 > out_sz) return -1;
    memcpy(out, merged, mlen);
    out[mlen] = '/';
    memcpy(out + mlen + 1, path, plen + 1);
    return 0;
}

int sud_overlay_resolve_at(int dirfd, const char *path, int for_write,
                           char *out, size_t out_sz)
{
    if (!g_rule_count) return SUD_OVERLAY_PASSTHROUGH;
    if (!path) return SUD_OVERLAY_PASSTHROUGH;
    char abs[PATH_MAX];
    if (resolve_at_to_abs(dirfd, path, abs, sizeof(abs)) != 0)
        return SUD_OVERLAY_PASSTHROUGH;
    return sud_overlay_resolve(abs, for_write, out, out_sz);
}

/* ----------------------------------------------------------------
 *  Whiteout creation.
 * ---------------------------------------------------------------- */

int sud_overlay_create_whiteout(const char *path)
{
    if (!path || path[0] != '/') return -EINVAL;
    const char *tail;
    const struct sud_overlay_rule *r = find_rule(path, &tail);
    if (!r) return 0;            /* not under overlay — nothing to do */
    if (r->simple) return 0;     /* simple remap has no whiteouts */
    if (!r->upper) return -EROFS;

    /* If the entry doesn't exist in any lower, no whiteout is needed. */
    int needed = 0;
    struct sud_overlay_stat st;
    for (int i = 0; i < r->lower_count; i++) {
        char buf[PATH_MAX];
        if (compose_layer(buf, sizeof(buf),
                          r->lowers[i], r->lower_lens[i], tail) < 0)
            continue;
        if (stat_one(buf, &st) == 0) { needed = 1; break; }
    }
    if (!needed) return 0;

    char upath[PATH_MAX];
    if (compose_layer(upath, sizeof(upath),
                      r->upper, r->upper_len, tail) < 0)
        return -ENAMETOOLONG;
    ensure_parent_in_upper(r->upper, r->upper_len, tail);

    /* If something already exists at the upper location, remove it
     * first.  This is the case after a write-then-delete cycle: the
     * upper has the modified file, which we now replace with the
     * whiteout marker. */
    if (stat_one(upath, &st) == 0)
        sud_ov_unlink(upath);

    long rc = sud_ov_mknod(upath, S_IFCHR | 0, 0);
    if (rc < 0) return (int)rc;
    return 0;
}

/* ----------------------------------------------------------------
 *  Synthetic merged directory.
 * ---------------------------------------------------------------- */

static int g_synth_seq;

/* Recursively unlink contents of `dir` (depth 1 only — synthetic dirs
 * are flat, just symlinks).  Best-effort. */
static void scrub_dir(const char *dir)
{
    int fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)dir,
                               O_RDONLY | O_DIRECTORY, 0, 0, 0);
    if (fd < 0) return;
    char buf[4096];
    for (;;) {
        long n = raw_getdents64(fd, buf, sizeof(buf));
        if (n <= 0) break;
        long pos = 0;
        while (pos < n) {
            struct {
                uint64_t d_ino;
                int64_t  d_off;
                unsigned short d_reclen;
                unsigned char  d_type;
                char     d_name[];
            } *ent = (void *)(buf + pos);
            pos += ent->d_reclen;
            if (ent->d_name[0] == '.' &&
                (ent->d_name[1] == '\0' ||
                 (ent->d_name[1] == '.' && ent->d_name[2] == '\0')))
                continue;
            char child[PATH_MAX];
            size_t dlen = strlen(dir);
            size_t nlen = strlen(ent->d_name);
            if (dlen + 1 + nlen + 1 > sizeof(child)) continue;
            memcpy(child, dir, dlen);
            child[dlen] = '/';
            memcpy(child + dlen + 1, ent->d_name, nlen + 1);
            raw_syscall6(__NR_unlinkat, AT_FDCWD, (long)child, 0, 0, 0, 0);
        }
    }
    raw_close(fd);
}

/* Decide the temp root for synthetic dirs.  We try $TMPDIR, then
 * /tmp.  The chosen path is cached. */
static const char *synth_tmp_root(void)
{
    static char root[PATH_MAX];
    static int  cached;
    if (cached) return root[0] ? root : 0;
    cached = 1;
    const char *t = getenv("TMPDIR");
    if (!t || !t[0]) t = "/tmp";
    size_t tl = strlen(t);
    if (tl + 1 > sizeof(root)) return 0;
    memcpy(root, t, tl + 1);
    return root;
}

/* Build the per-process synthetic-dir parent: /tmp/.sud-overlay/<pid>.
 * Created (best-effort) on every call so callers tolerate external
 * cleanup of /tmp between calls. */
static const char *synth_pid_dir(void)
{
    static char dir[PATH_MAX];
    static int  cached;
    if (!cached) {
        const char *root = synth_tmp_root();
        if (!root) return 0;
        long pid = sud_ov_getpid();
        int n = snprintf(dir, sizeof(dir), "%s/.sud-overlay/%ld",
                         root, pid);
        if (n <= 0 || (size_t)n >= sizeof(dir)) { dir[0] = 0; return 0; }
        cached = 1;
    }
    if (!dir[0]) return 0;
    /* Ensure the dir (and its parent) exist on every call.  mkdir is
     * cheap and EEXIST-tolerant; this matters when the caller has
     * cleaned /tmp between invocations (e.g. our test driver). */
    char parent[PATH_MAX];
    /* Locate the last '/' to derive the parent path. */
    int last = -1;
    for (int i = 0; dir[i]; i++)
        if (dir[i] == '/') last = i;
    if (last > 0) {
        memcpy(parent, dir, (size_t)last);
        parent[last] = '\0';
        sud_ov_mkdir(parent, 0755);
    }
    sud_ov_mkdir(dir, 0755);
    return dir;
}

/* Read a directory's entries into a flat name list.  `seen` is updated
 * for de-duplication; entries already in `seen` (or marked as a
 * whiteout in `whiteouts`) are skipped.  When `is_upper` is set, char-
 * dev-zero entries are added to `whiteouts` instead of being symlinked.
 *
 * For each new entry we create a symlink in `synth` that points to the
 * absolute path of the source entry. */
struct name_set {
    /* Linear array of NUL-terminated names; bounded by the per-call
     * buffer size.  Modest cap: O(N) lookup is fine for typical N. */
    char  *buf;
    size_t cap;
    size_t used;
    int    count;
};

static int name_set_has(const struct name_set *s, const char *name)
{
    const char *p = s->buf;
    const char *end = s->buf + s->used;
    while (p < end) {
        if (strcmp(p, name) == 0) return 1;
        p += strlen(p) + 1;
    }
    return 0;
}

static int name_set_add(struct name_set *s, const char *name)
{
    size_t n = strlen(name) + 1;
    if (s->used + n > s->cap) return -1;
    memcpy(s->buf + s->used, name, n);
    s->used += n;
    s->count++;
    return 0;
}

static void merge_layer(const char *layer, size_t layer_len,
                        const char *tail,
                        const char *synth, size_t synth_len,
                        struct name_set *seen,
                        struct name_set *whiteouts,
                        int is_upper)
{
    char dirpath[PATH_MAX];
    if (compose_layer(dirpath, sizeof(dirpath),
                      layer, layer_len, tail) < 0)
        return;
    int fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)dirpath,
                               O_RDONLY | O_DIRECTORY, 0, 0, 0);
    if (fd < 0) return;
    char dbuf[4096];
    for (;;) {
        long n = raw_getdents64(fd, dbuf, sizeof(dbuf));
        if (n <= 0) break;
        long pos = 0;
        while (pos < n) {
            struct {
                uint64_t d_ino;
                int64_t  d_off;
                unsigned short d_reclen;
                unsigned char  d_type;
                char     d_name[];
            } *ent = (void *)(dbuf + pos);
            pos += ent->d_reclen;
            const char *name = ent->d_name;
            if (name[0] == '.' &&
                (name[1] == '\0' ||
                 (name[1] == '.' && name[2] == '\0')))
                continue;

            /* Build target path inside this layer. */
            size_t dlen = strlen(dirpath);
            size_t nlen = strlen(name);
            char tgt[PATH_MAX];
            if (dlen + 1 + nlen + 1 > sizeof(tgt)) continue;
            memcpy(tgt, dirpath, dlen);
            tgt[dlen] = '/';
            memcpy(tgt + dlen + 1, name, nlen + 1);

            /* Whiteout in upper? Record and skip. */
            if (is_upper) {
                struct sud_overlay_stat st;
                if (sud_ov_lstat(tgt, &st) == 0 && is_whiteout_st(&st)) {
                    name_set_add(whiteouts, name);
                    continue;
                }
            } else {
                /* Lower entry hidden by a whiteout? */
                if (name_set_has(whiteouts, name)) continue;
            }
            /* Higher-priority layer already provided this name? */
            if (name_set_has(seen, name)) continue;
            if (name_set_add(seen, name) != 0) {
                raw_close(fd);
                return;
            }
            /* Create symlink in synth dir. */
            char link[PATH_MAX];
            if (synth_len + 1 + nlen + 1 > sizeof(link)) continue;
            memcpy(link, synth, synth_len);
            link[synth_len] = '/';
            memcpy(link + synth_len + 1, name, nlen + 1);
            sud_ov_symlink(tgt, link);
        }
    }
    raw_close(fd);
}

int sud_overlay_open_dir(const char *path, int flags, int mode)
{
    if (!g_rule_count) return SUD_OVERLAY_NO_DIR;
    if (!path || path[0] != '/') return SUD_OVERLAY_NO_DIR;
    const char *tail;
    const struct sud_overlay_rule *r = find_rule(path, &tail);
    if (!r) return SUD_OVERLAY_NO_DIR;
    if (r->simple) {
        /* Simple remap: just open the mapped path directly. */
        char buf[PATH_MAX];
        if (compose_layer(buf, sizeof(buf),
                          r->upper, r->upper_len, tail) < 0)
            return -ENAMETOOLONG;
        long fd = raw_syscall6(SYS_openat, AT_FDCWD, (long)buf,
                               flags, mode, 0, 0);
        return (int)fd;
    }

    /* Build synth dir: /tmp/.sud-overlay/<pid>/<seq>/ */
    const char *parent = synth_pid_dir();
    if (!parent) return -ENOMEM;
    int seq = ++g_synth_seq;
    char synth[PATH_MAX];
    int sn = snprintf(synth, sizeof(synth), "%s/%d", parent, seq);
    if (sn <= 0 || (size_t)sn >= sizeof(synth)) return -ENAMETOOLONG;
    /* In case a stale dir from a previous run lingers, scrub then
     * recreate.  Mkdir failure is fatal. */
    scrub_dir(synth);
    raw_syscall6(__NR_unlinkat, AT_FDCWD, (long)synth, AT_REMOVEDIR, 0, 0, 0);
    long mr = sud_ov_mkdir(synth, 0755);
    if (mr < 0 && mr != -EEXIST) return (int)mr;
    size_t synth_len = (size_t)sn;

    /* Buffers for de-dup and whiteout tracking. 64KiB each handles
     * O(thousands) of entries. */
    static char seen_buf[65536];
    static char white_buf[16384];
    struct name_set seen     = { seen_buf,  sizeof(seen_buf),  0, 0 };
    struct name_set whiteouts= { white_buf, sizeof(white_buf), 0, 0 };

    if (r->upper)
        merge_layer(r->upper, r->upper_len, tail,
                    synth, synth_len, &seen, &whiteouts, 1);
    for (int i = 0; i < r->lower_count; i++)
        merge_layer(r->lowers[i], r->lower_lens[i], tail,
                    synth, synth_len, &seen, &whiteouts, 0);

    long fd = raw_syscall6(SYS_openat, AT_FDCWD, (long)synth,
                           flags | O_DIRECTORY, mode, 0, 0);
    if (fd < 0) return (int)fd;
    fd_map_remember((int)fd, path);
    return (int)fd;
}

int sud_overlay_open_dir_at(int dirfd, const char *path, int flags, int mode)
{
    if (!g_rule_count) return SUD_OVERLAY_NO_DIR;
    if (!path) return SUD_OVERLAY_NO_DIR;
    char abs[PATH_MAX];
    if (resolve_at_to_abs(dirfd, path, abs, sizeof(abs)) != 0)
        return SUD_OVERLAY_NO_DIR;
    return sud_overlay_open_dir(abs, flags, mode);
}
