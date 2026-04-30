/*
 * sud/path_remap/path.c — Path-layer state owned by path_remap.
 *
 * Holds the logical CWD shadow, dirfd→logical-path table,
 * absolutise(), and the inramfs mount-prefix knowledge.  See path.h.
 *
 * All filesystem operations go through raw_syscall6() because the
 * code runs from inside the SIGSYS handler.
 */

#include "sud/path_remap/path.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"

/* ================================================================
 * Logical CWD shadow
 *
 * The kernel only knows about real filesystem paths; a kernel
 * chdir(/inramfs/...) returns ENOENT.  We shadow the user-visible
 * CWD here.  When chdir lands on an inramfs path we stash it and
 * point the kernel CWD at "/" so /proc/self/cwd is at least
 * resolvable.  Subsequent AT_FDCWD-relative path resolution in
 * sud_pr_absolutise consults this shadow before falling back to
 * /proc/self/cwd, and getcwd(2) returns the shadow verbatim.
 *
 * Inheritance across exec: g_sud_runtime_config.cwd tracks the
 * shadow in lock-step, and sud/elf.c::build_exec_argv re-emits
 * "--cwd <abs>" on every child wrapper.  Children parse the flag
 * in their wrapper.c::main and our seed-from-runtime-config picks
 * it up on first access here.
 * ================================================================ */

static char g_logical_cwd[PATH_MAX];
static int  g_cwd_seeded;

void sud_pr_cwd_seed_from_runtime_config(void)
{
    if (g_cwd_seeded) return;
    g_cwd_seeded = 1;

    if (!g_sud_runtime_config_present) return;
    const char *v = g_sud_runtime_config.cwd;
    if (!v || v[0] != '/') return;
    size_t vl = strlen(v);
    if (vl >= sizeof(g_logical_cwd)) return;
    memcpy(g_logical_cwd, v, vl + 1);
}

void sud_pr_cwd_set(const char *abs_path)
{
    if (!abs_path || !abs_path[0]) {
        g_logical_cwd[0] = '\0';
    } else {
        size_t l = strlen(abs_path);
        if (l >= sizeof(g_logical_cwd)) {
            g_logical_cwd[0] = '\0';
        } else {
            memcpy(g_logical_cwd, abs_path, l + 1);
        }
    }
    /* Keep the runtime-config slot in lock-step so sud/elf.c::
     * build_exec_argv re-emits the right --cwd to children. */
    if (g_sud_runtime_config_present) {
        sud_runtime_config_set_cwd(&g_sud_runtime_config,
                                   g_logical_cwd[0] ? g_logical_cwd : 0);
    }
    g_cwd_seeded = 1;   /* explicit set wins; suppress later seed */
}

const char *sud_pr_cwd_get(void)
{
    sud_pr_cwd_seed_from_runtime_config();
    return g_logical_cwd[0] ? g_logical_cwd : 0;
}

long sud_pr_read_kernel_cwd(char *out, size_t out_sz)
{
    if (out_sz == 0) return -EINVAL;
    long n = raw_syscall6(SYS_readlinkat, AT_FDCWD,
                          (long)"/proc/self/cwd",
                          (long)out, (long)out_sz - 1, 0, 0);
    if (n < 0) return n;
    out[n] = '\0';
    return 0;
}

void sud_pr_cwd_reset_for_testing(void)
{
    g_logical_cwd[0] = '\0';
    g_cwd_seeded = 0;
}

/* ================================================================
 * dirfd → logical-path table
 *
 * Open-addressed array; linear probing.  Typical usage in build
 * workloads keeps a handful of dir fds open at any time, so a small
 * fixed-size table is fine.
 * ================================================================ */

#define SUD_PR_DIRFD_TABLE_SIZE 256
#define SUD_PR_DIRFD_PATH_MAX   512

struct sud_pr_dirfd_slot {
    int  fd;                                 /* -1 = free */
    char path[SUD_PR_DIRFD_PATH_MAX];        /* absolute, NUL-terminated */
};

static struct sud_pr_dirfd_slot g_dirfd_tab[SUD_PR_DIRFD_TABLE_SIZE];
static int                      g_dirfd_init;

static void dirfd_init(void)
{
    if (g_dirfd_init) return;
    for (int i = 0; i < SUD_PR_DIRFD_TABLE_SIZE; i++)
        g_dirfd_tab[i].fd = -1;
    g_dirfd_init = 1;
}

void sud_pr_dirfd_register(int fd, const char *abs_path)
{
    if (fd < 0 || !abs_path || !abs_path[0]) return;
    size_t l = strlen(abs_path);
    if (l >= SUD_PR_DIRFD_PATH_MAX) return;
    dirfd_init();
    /* Replace existing entry if present. */
    int free_slot = -1;
    for (int i = 0; i < SUD_PR_DIRFD_TABLE_SIZE; i++) {
        if (g_dirfd_tab[i].fd == fd) {
            memcpy(g_dirfd_tab[i].path, abs_path, l + 1);
            return;
        }
        if (g_dirfd_tab[i].fd == -1 && free_slot < 0) free_slot = i;
    }
    if (free_slot < 0) return;        /* table full: silently drop */
    g_dirfd_tab[free_slot].fd = fd;
    memcpy(g_dirfd_tab[free_slot].path, abs_path, l + 1);
}

const char *sud_pr_dirfd_lookup(int fd)
{
    if (!g_dirfd_init || fd < 0) return 0;
    for (int i = 0; i < SUD_PR_DIRFD_TABLE_SIZE; i++) {
        if (g_dirfd_tab[i].fd == fd)
            return g_dirfd_tab[i].path[0] ? g_dirfd_tab[i].path : 0;
    }
    return 0;
}

void sud_pr_dirfd_forget(int fd)
{
    if (!g_dirfd_init || fd < 0) return;
    for (int i = 0; i < SUD_PR_DIRFD_TABLE_SIZE; i++) {
        if (g_dirfd_tab[i].fd == fd) {
            g_dirfd_tab[i].fd = -1;
            g_dirfd_tab[i].path[0] = '\0';
            return;
        }
    }
}

void sud_pr_dirfd_reset_for_testing(void)
{
    for (int i = 0; i < SUD_PR_DIRFD_TABLE_SIZE; i++) {
        g_dirfd_tab[i].fd = -1;
        g_dirfd_tab[i].path[0] = '\0';
    }
    g_dirfd_init = 1;
}

/* ================================================================
 * (dirfd, path) → absolute path
 * ================================================================ */

int sud_pr_absolutise(int dirfd, const char *path,
                      char *out, size_t out_sz)
{
    if (!path) return -EFAULT;
    if (path[0] == '/') {
        size_t n = strlen(path);
        if (n + 1 > out_sz) return -ENAMETOOLONG;
        memcpy(out, path, n + 1);
        return 0;
    }
    if (dirfd != AT_FDCWD) {
        const char *base = sud_pr_dirfd_lookup(dirfd);
        if (!base) {
            /* Unknown dirfd — let caller fall through to the kernel,
             * which will use its own dirfd table.  -EXDEV is the
             * sentinel value the dispatchers expect. */
            return -EXDEV;
        }
        size_t bl = strlen(base);
        size_t pl = strlen(path);
        if (bl + 1 + pl + 1 > out_sz) return -ENAMETOOLONG;
        memcpy(out, base, bl);
        out[bl] = '/';
        memcpy(out + bl + 1, path, pl + 1);
        return 0;
    }
    /* AT_FDCWD: prepend the logical CWD if active, else /proc/self/cwd.
     * The logical CWD wins so that, after a chdir into inramfs, a
     * relative open("foo") correctly resolves under the inramfs
     * mount instead of under "/" (where we parked the kernel CWD). */
    char cwd[PATH_MAX];
    size_t cl;
    const char *lcwd = sud_pr_cwd_get();
    if (lcwd) {
        cl = strlen(lcwd);
        if (cl >= sizeof(cwd)) return -ENAMETOOLONG;
        memcpy(cwd, lcwd, cl + 1);
    } else {
        long rc = sud_pr_read_kernel_cwd(cwd, sizeof(cwd));
        if (rc < 0) return (int)rc;
        cl = strlen(cwd);
    }
    size_t pl = strlen(path);
    if (cl + 1 + pl + 1 > out_sz) return -ENAMETOOLONG;
    memcpy(out, cwd, cl);
    out[cl] = '/';
    memcpy(out + cl + 1, path, pl + 1);
    return 0;
}

/* ================================================================
 * inramfs mount-prefix
 *
 * The inramfs mount point used to live in sud/inramfs/super.c as
 * g_mount_path / g_mount_len, parsed from SUD_INRAMFS.  After the
 * Part-1 re-layering the mount point is owned here: it is one entry
 * in the runtime-config remap-rule list (kind "inramfs:<abs_path>")
 * and inramfs's vfs.c queries this layer for the prefix when it
 * needs to strip it from an absolute path before walking the inode
 * tree.
 * ================================================================ */

static char   g_inramfs_mount[PATH_MAX];
static size_t g_inramfs_mount_len;

void sud_pr_inramfs_mount_set(const char *abs_path)
{
    if (!abs_path || !abs_path[0]) {
        g_inramfs_mount[0] = '\0';
        g_inramfs_mount_len = 0;
        return;
    }
    if (abs_path[0] != '/') return;
    size_t l = strlen(abs_path);
    /* Strip trailing slashes (except the root "/" itself). */
    while (l > 1 && abs_path[l - 1] == '/') l--;
    if (l == 0 || l >= sizeof(g_inramfs_mount)) {
        g_inramfs_mount[0] = '\0';
        g_inramfs_mount_len = 0;
        return;
    }
    memcpy(g_inramfs_mount, abs_path, l);
    g_inramfs_mount[l] = '\0';
    g_inramfs_mount_len = l;
}

const char *sud_pr_inramfs_mount_path(void)
{
    return g_inramfs_mount_len ? g_inramfs_mount : 0;
}

size_t sud_pr_inramfs_mount_len(void)
{
    return g_inramfs_mount_len;
}

int sud_pr_inramfs_path_under_mount(const char *abs_path)
{
    if (!abs_path || abs_path[0] != '/') return 0;
    if (!g_inramfs_mount_len) return 0;
    /* Trivial root mount ("/"): every absolute path matches. */
    if (g_inramfs_mount_len == 1 && g_inramfs_mount[0] == '/') return 1;
    /* memcmp avoids strncmp, keeping the freestanding-build deps
     * minimal (the code is the same set of helpers that sudtrace
     * and the wrapper share). */
    size_t i = 0;
    while (i < g_inramfs_mount_len && abs_path[i] == g_inramfs_mount[i])
        i++;
    if (i != g_inramfs_mount_len) return 0;
    char c = abs_path[g_inramfs_mount_len];
    return c == '\0' || c == '/';
}

void sud_pr_inramfs_init_from_runtime_config(void)
{
    if (!g_sud_runtime_config_present) return;
    /* Walk the remap-rule list looking for an "inramfs:<abs_path>"
     * entry.  The first wins — multiple inramfs mounts are not
     * supported in this iteration. */
    for (int i = 0; i < g_sud_runtime_config.remap_rule_count; i++) {
        const char *r = g_sud_runtime_config.remap_rules[i];
        if (!r) continue;
        static const char pfx[] = "inramfs:";
        size_t plen = sizeof(pfx) - 1;
        size_t ri = 0;
        while (ri < plen && r[ri] == pfx[ri]) ri++;
        if (ri != plen) continue;
        sud_pr_inramfs_mount_set(r + plen);
        return;
    }
}

int sud_pr_resolve_at_inramfs(int dirfd, const char *path,
                              char *out, size_t out_sz)
{
    if (!g_inramfs_mount_len) return -1;
    int rc = sud_pr_absolutise(dirfd, path, out, out_sz);
    if (rc < 0) return rc;
    if (!sud_pr_inramfs_path_under_mount(out)) return -1;
    return 0;
}

/* ================================================================
 * Lifecycle
 * ================================================================ */

void sud_pr_path_init(void)
{
    dirfd_init();
    sud_pr_cwd_seed_from_runtime_config();
    sud_pr_inramfs_init_from_runtime_config();
}
