/*
 * sud/path_remap/tests/test_overlay.c — Functional tests for the
 * sud/path_remap overlay layer.
 *
 * Built freestanding (-nostdlib) the same way as libc-fs's own tests:
 * the test driver links libc-fs's libc, raw, and overlay sources
 * directly so the same code paths run in test as in the live SUD
 * handler.
 *
 * Each test creates a fresh tmpdir tree (lower1, lower2, upper, and
 * an arbitrary "merged" mount point that doesn't actually exist on
 * disk), configures the overlay via the runtime-config test API
 * (sud_runtime_config_test_install), calls into the overlay
 * API, and verifies the result.
 *
 * Tests run for both -m32 and -m64 builds — the Makefile does both.
 */

#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"
#include "sud/path_remap/overlay.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

/* ---- Tiny test framework ------------------------------------------- */

static int g_failures;
static const char *g_curtest;

static void test_log(const char *msg)
{
    write(2, msg, strlen(msg));
}

#define TASSERT(cond, descr) do { \
    if (!(cond)) { \
        char _buf[256]; \
        snprintf(_buf, sizeof(_buf), "FAIL [%s] %s @ line %d\n", \
                 g_curtest, (descr), __LINE__); \
        test_log(_buf); \
        g_failures++; \
    } \
} while (0)

#define TASSERT_STREQ(actual, expected, descr) do { \
    if (strcmp((actual), (expected)) != 0) { \
        char _buf[512]; \
        snprintf(_buf, sizeof(_buf), \
                 "FAIL [%s] %s @ line %d: got '%s', want '%s'\n", \
                 g_curtest, (descr), __LINE__, (actual), (expected)); \
        test_log(_buf); \
        g_failures++; \
    } \
} while (0)

#define TASSERT_EQ(actual, expected, descr) do { \
    long _a = (long)(actual); \
    long _e = (long)(expected); \
    if (_a != _e) { \
        char _buf[256]; \
        snprintf(_buf, sizeof(_buf), \
                 "FAIL [%s] %s @ line %d: got %ld, want %ld\n", \
                 g_curtest, (descr), __LINE__, _a, _e); \
        test_log(_buf); \
        g_failures++; \
    } \
} while (0)

/* ---- Filesystem helpers using raw syscalls ------------------------- */

#ifndef S_IRUSR
#define S_IRUSR 0400
#define S_IWUSR 0200
#define S_IXUSR 0100
#endif

static int t_mkdir(const char *p, int mode)
{
#ifdef __NR_mkdirat
    return (int)raw_syscall6(__NR_mkdirat, AT_FDCWD, (long)p, mode, 0, 0, 0);
#else
    return (int)raw_syscall6(__NR_mkdir, (long)p, mode, 0, 0, 0, 0);
#endif
}

static int t_write_file(const char *p, const char *data)
{
    int fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)p,
                               O_WRONLY | O_CREAT | O_TRUNC, 0644, 0, 0);
    if (fd < 0) return fd;
    long n = strlen(data);
    long w = raw_write(fd, data, (size_t)n);
    raw_close(fd);
    return (int)((w == n) ? 0 : -1);
}

static int t_unlink(const char *p)
{
#ifdef __NR_unlinkat
    return (int)raw_syscall6(__NR_unlinkat, AT_FDCWD, (long)p, 0, 0, 0, 0);
#else
    return (int)raw_syscall6(__NR_unlink, (long)p, 0, 0, 0, 0, 0);
#endif
}

static int t_rmdir(const char *p)
{
#ifdef __NR_unlinkat
    return (int)raw_syscall6(__NR_unlinkat, AT_FDCWD, (long)p,
                             AT_REMOVEDIR, 0, 0, 0);
#else
    return (int)raw_syscall6(__NR_rmdir, (long)p, 0, 0, 0, 0, 0);
#endif
}

static int t_mknod_chr(const char *p)
{
#ifdef __NR_mknodat
    return (int)raw_syscall6(__NR_mknodat, AT_FDCWD, (long)p,
                             S_IFCHR | 0, 0, 0, 0);
#else
    return (int)raw_syscall6(__NR_mknod, (long)p, S_IFCHR | 0, 0, 0, 0, 0);
#endif
}

static int t_exists(const char *p)
{
    return raw_access(p, 0 /* F_OK */) == 0;
}

/* Recursively delete a directory tree.  Used to reset between tests. */
static void t_rm_rf(const char *path)
{
    int fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)path,
                               O_RDONLY | O_DIRECTORY | O_NOFOLLOW,
                               0, 0, 0);
    if (fd < 0) {
        /* Maybe a file (or symlink) — try unlink. */
        t_unlink(path);
        return;
    }
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
            } *e = (void *)(buf + pos);
            pos += e->d_reclen;
            if (e->d_name[0] == '.' &&
                (e->d_name[1] == '\0' ||
                 (e->d_name[1] == '.' && e->d_name[2] == '\0')))
                continue;
            char child[PATH_MAX];
            size_t pl = strlen(path);
            size_t nl = strlen(e->d_name);
            if (pl + 1 + nl + 1 > sizeof(child)) continue;
            memcpy(child, path, pl);
            child[pl] = '/';
            memcpy(child + pl + 1, e->d_name, nl + 1);
            t_rm_rf(child);
        }
    }
    raw_close(fd);
    t_rmdir(path);
}

/* ---- Test fixture -------------------------------------------------- */

static char g_tmp[PATH_MAX];     /* /tmp/sud-overlay-test-<pid>      */
static char g_lower1[PATH_MAX];
static char g_lower2[PATH_MAX];
static char g_upper[PATH_MAX];
static char g_merged[PATH_MAX];

static void fixture_setup(void)
{
    long pid = raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
    snprintf(g_tmp,    sizeof(g_tmp),    "/tmp/sud-overlay-test-%ld", pid);
    snprintf(g_lower1, sizeof(g_lower1), "%s/lower1", g_tmp);
    snprintf(g_lower2, sizeof(g_lower2), "%s/lower2", g_tmp);
    snprintf(g_upper,  sizeof(g_upper),  "%s/upper",  g_tmp);
    snprintf(g_merged, sizeof(g_merged), "%s/merged", g_tmp);

    t_rm_rf(g_tmp);
    t_mkdir(g_tmp,    0755);
    t_mkdir(g_lower1, 0755);
    t_mkdir(g_lower2, 0755);
    t_mkdir(g_upper,  0755);
    /* g_merged is intentionally NOT created on disk — overlay rules
     * apply to a virtual mount point that need not exist. */
}

static void fixture_teardown(void)
{
    t_rm_rf(g_tmp);
    /* Clean up any synthetic merged dirs created for this pid by
     * sud_overlay_open_dir(). */
    char synth_pid[PATH_MAX];
    long pid = raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
    snprintf(synth_pid, sizeof(synth_pid),
             "/tmp/.sud-overlay/%ld", pid);
    t_rm_rf(synth_pid);
}

static void install_overlay(void)
{
    char spec[PATH_MAX * 4];
    snprintf(spec, sizeof(spec), "overlay:%s=%s+%s+%s",
             g_merged, g_upper, g_lower1, g_lower2);
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.remap_rules[0] = spec;
    cfg.remap_rule_count = 1;
    sud_runtime_config_test_install(&cfg);
    sud_overlay_reset_for_testing();
    sud_overlay_init();
}

static void install_readonly_overlay(void)
{
    char spec[PATH_MAX * 4];
    /* No upper — read-only overlay. */
    snprintf(spec, sizeof(spec), "overlay:%s=+%s+%s",
             g_merged, g_lower1, g_lower2);
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.remap_rules[0] = spec;
    cfg.remap_rule_count = 1;
    sud_runtime_config_test_install(&cfg);
    sud_overlay_reset_for_testing();
    sud_overlay_init();
}

static void install_simple_remap(void)
{
    char spec[PATH_MAX * 4];
    snprintf(spec, sizeof(spec), "remap:%s=%s", g_merged, g_lower1);
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.remap_rules[0] = spec;
    cfg.remap_rule_count = 1;
    sud_runtime_config_test_install(&cfg);
    sud_overlay_reset_for_testing();
    sud_overlay_init();
}

/* ---- Tests --------------------------------------------------------- */

static void test_simple_remap_compat(void)
{
    g_curtest = "simple_remap_compat";
    fixture_setup();
    install_simple_remap();

    char out[PATH_MAX], merged_path[PATH_MAX], want[PATH_MAX];
    snprintf(merged_path, sizeof(merged_path), "%s/anything", g_merged);
    int rc = sud_overlay_resolve(merged_path, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "simple read resolves");
    snprintf(want, sizeof(want), "%s/anything", g_lower1);
    TASSERT_STREQ(out, want, "simple resolved path");

    rc = sud_overlay_resolve(merged_path, 1, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "simple write also resolves");
    TASSERT_STREQ(out, want, "simple write path == read path");

    fixture_teardown();
}

static void test_lower_precedence(void)
{
    g_curtest = "lower_precedence";
    fixture_setup();
    install_overlay();

    char p[PATH_MAX];
    snprintf(p, sizeof(p), "%s/fileA", g_lower1); t_write_file(p, "L1A");
    snprintf(p, sizeof(p), "%s/fileA", g_lower2); t_write_file(p, "L2A");
    snprintf(p, sizeof(p), "%s/fileB", g_lower2); t_write_file(p, "L2B");

    char out[PATH_MAX], m[PATH_MAX], want[PATH_MAX];
    snprintf(m, sizeof(m), "%s/fileA", g_merged);
    int rc = sud_overlay_resolve(m, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "fileA resolves");
    snprintf(want, sizeof(want), "%s/fileA", g_lower1);
    TASSERT_STREQ(out, want, "fileA resolves to lower1 (higher prio)");

    snprintf(m, sizeof(m), "%s/fileB", g_merged);
    rc = sud_overlay_resolve(m, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "fileB resolves");
    snprintf(want, sizeof(want), "%s/fileB", g_lower2);
    TASSERT_STREQ(out, want, "fileB resolves to lower2 only");

    fixture_teardown();
}

static void test_upper_overrides_lower(void)
{
    g_curtest = "upper_overrides_lower";
    fixture_setup();
    install_overlay();

    char p[PATH_MAX];
    snprintf(p, sizeof(p), "%s/fileA", g_lower1); t_write_file(p, "low");
    snprintf(p, sizeof(p), "%s/fileA", g_upper);  t_write_file(p, "up");

    char out[PATH_MAX], m[PATH_MAX], want[PATH_MAX];
    snprintf(m, sizeof(m), "%s/fileA", g_merged);
    int rc = sud_overlay_resolve(m, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "resolved");
    snprintf(want, sizeof(want), "%s/fileA", g_upper);
    TASSERT_STREQ(out, want, "upper takes priority");

    fixture_teardown();
}

static void test_write_redirects_to_upper_with_parents(void)
{
    g_curtest = "write_redirects_to_upper_with_parents";
    fixture_setup();
    install_overlay();

    char out[PATH_MAX], m[PATH_MAX], want[PATH_MAX];
    snprintf(m, sizeof(m), "%s/sub/dir/file", g_merged);

    int rc = sud_overlay_resolve(m, 1, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "write resolves");
    snprintf(want, sizeof(want), "%s/sub/dir/file", g_upper);
    TASSERT_STREQ(out, want, "write goes to upper");

    char chk[PATH_MAX];
    snprintf(chk, sizeof(chk), "%s/sub", g_upper);
    TASSERT(t_exists(chk), "upper/sub auto-created");
    snprintf(chk, sizeof(chk), "%s/sub/dir", g_upper);
    TASSERT(t_exists(chk), "upper/sub/dir auto-created");

    fixture_teardown();
}

static void test_whiteout_masks_lower(void)
{
    g_curtest = "whiteout_masks_lower";
    fixture_setup();
    install_overlay();

    char p[PATH_MAX];
    snprintf(p, sizeof(p), "%s/fileX", g_lower1); t_write_file(p, "secret");
    snprintf(p, sizeof(p), "%s/fileX", g_upper);
    TASSERT_EQ(t_mknod_chr(p), 0, "create whiteout");

    char out[PATH_MAX], m[PATH_MAX];
    snprintf(m, sizeof(m), "%s/fileX", g_merged);
    int rc = sud_overlay_resolve(m, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_WHITEOUT, "whiteout reported");

    fixture_teardown();
}

static void test_create_whiteout_after_unlink(void)
{
    g_curtest = "create_whiteout_after_unlink";
    fixture_setup();
    install_overlay();

    char p[PATH_MAX];
    snprintf(p, sizeof(p), "%s/fileY", g_lower1); t_write_file(p, "data");

    char m[PATH_MAX];
    snprintf(m, sizeof(m), "%s/fileY", g_merged);
    int rc = sud_overlay_create_whiteout(m);
    TASSERT_EQ(rc, 0, "whiteout creation succeeds");

    char out[PATH_MAX];
    int r2 = sud_overlay_resolve(m, 0, out, sizeof(out));
    TASSERT_EQ(r2, SUD_OVERLAY_WHITEOUT, "post-unlink lookup is whiteout");

    fixture_teardown();
}

static void test_readonly_overlay(void)
{
    g_curtest = "readonly_overlay";
    fixture_setup();
    install_readonly_overlay();

    char p[PATH_MAX];
    snprintf(p, sizeof(p), "%s/fileZ", g_lower2); t_write_file(p, "z");

    char m[PATH_MAX], out[PATH_MAX], want[PATH_MAX];
    snprintf(m, sizeof(m), "%s/fileZ", g_merged);
    int rc = sud_overlay_resolve(m, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "read works");
    snprintf(want, sizeof(want), "%s/fileZ", g_lower2);
    TASSERT_STREQ(out, want, "read resolves to lower2");

    rc = sud_overlay_resolve(m, 1, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_READONLY, "write reports readonly");

    fixture_teardown();
}

static void test_passthrough_for_unrelated_paths(void)
{
    g_curtest = "passthrough_for_unrelated_paths";
    fixture_setup();
    install_overlay();

    char out[PATH_MAX];
    int rc = sud_overlay_resolve("/etc/passwd", 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_PASSTHROUGH, "unrelated path passthrough");

    /* Boundary: a path that starts with the merged prefix as a string
     * but isn't on a path-component boundary must NOT match. */
    char m_almost[PATH_MAX];
    snprintf(m_almost, sizeof(m_almost), "%s_other/x", g_merged);
    rc = sud_overlay_resolve(m_almost, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_PASSTHROUGH, "non-boundary prefix passthrough");

    fixture_teardown();
}

static int collect_sorted_dirents(int fd, char *out, size_t out_sz)
{
    char names[64][256];
    int  count = 0;
    char buf[4096];
    for (;;) {
        long n = raw_getdents64(fd, buf, sizeof(buf));
        if (n <= 0) break;
        long pos = 0;
        while (pos < n && count < 64) {
            struct {
                uint64_t d_ino;
                int64_t  d_off;
                unsigned short d_reclen;
                unsigned char  d_type;
                char     d_name[];
            } *e = (void *)(buf + pos);
            pos += e->d_reclen;
            if (e->d_name[0] == '.' &&
                (e->d_name[1] == '\0' ||
                 (e->d_name[1] == '.' && e->d_name[2] == '\0')))
                continue;
            size_t l = strlen(e->d_name);
            if (l >= sizeof(names[0])) l = sizeof(names[0]) - 1;
            memcpy(names[count], e->d_name, l);
            names[count][l] = '\0';
            count++;
        }
    }
    /* Insertion sort. */
    for (int i = 1; i < count; i++) {
        char tmp[256];
        memcpy(tmp, names[i], sizeof(tmp));
        int j = i;
        while (j > 0 && strcmp(names[j-1], tmp) > 0) {
            memcpy(names[j], names[j-1], sizeof(names[j]));
            j--;
        }
        memcpy(names[j], tmp, sizeof(names[j]));
    }
    out[0] = '\0';
    size_t off = 0;
    for (int i = 0; i < count; i++) {
        size_t l = strlen(names[i]);
        if (off + l + 2 >= out_sz) break;
        if (i > 0) out[off++] = '|';
        memcpy(out + off, names[i], l);
        off += l;
        out[off] = '\0';
    }
    return count;
}

static void test_merged_directory_listing(void)
{
    g_curtest = "merged_directory_listing";
    fixture_setup();
    install_overlay();

    /* Layout:
     *   lower1/{a, common}
     *   lower2/{b, common, hidden}
     *   upper /{c, hidden=whiteout}
     * Expected merged: a, b, c, common (4 entries; "hidden" hidden).
     */
    char p[PATH_MAX];
    snprintf(p, sizeof(p), "%s/a",      g_lower1); t_write_file(p, "a");
    snprintf(p, sizeof(p), "%s/common", g_lower1); t_write_file(p, "L1");
    snprintf(p, sizeof(p), "%s/b",      g_lower2); t_write_file(p, "b");
    snprintf(p, sizeof(p), "%s/common", g_lower2); t_write_file(p, "L2");
    snprintf(p, sizeof(p), "%s/hidden", g_lower2); t_write_file(p, "x");
    snprintf(p, sizeof(p), "%s/c",      g_upper);  t_write_file(p, "c");
    snprintf(p, sizeof(p), "%s/hidden", g_upper);
    TASSERT_EQ(t_mknod_chr(p), 0, "whiteout");

    int fd = sud_overlay_open_dir(g_merged, O_RDONLY | O_DIRECTORY, 0);
    TASSERT(fd >= 0, "open_dir succeeds");
    if (fd >= 0) {
        char names[1024];
        int n = collect_sorted_dirents(fd, names, sizeof(names));
        TASSERT_EQ(n, 4, "exactly 4 merged entries");
        TASSERT_STREQ(names, "a|b|c|common", "merged sorted listing");
        raw_close(fd);
    }

    fixture_teardown();
}

static void test_resolve_at_with_dirfd(void)
{
    g_curtest = "resolve_at_with_dirfd";
    fixture_setup();
    install_overlay();

    char p[PATH_MAX];
    snprintf(p, sizeof(p), "%s/x", g_lower1); t_write_file(p, "lx");

    int fd = sud_overlay_open_dir(g_merged, O_RDONLY | O_DIRECTORY, 0);
    TASSERT(fd >= 0, "open merged dir");
    if (fd >= 0) {
        char out[PATH_MAX], want[PATH_MAX];
        int rc = sud_overlay_resolve_at(fd, "x", 0, out, sizeof(out));
        TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "resolve_at via dirfd");
        snprintf(want, sizeof(want), "%s/x", g_lower1);
        TASSERT_STREQ(out, want, "resolve_at picks lower1");
        raw_close(fd);
    }

    fixture_teardown();
}

static void test_multi_rule_parsing(void)
{
    g_curtest = "multi_rule_parsing";
    fixture_setup();

    /* Use /tmp paths for the second rule too, since rules with non-
     * existent paths are still parsed and counted; we only assert the
     * count and that resolution finds the right rule. */
    char spec1[PATH_MAX * 4], spec2[PATH_MAX * 4];
    snprintf(spec1, sizeof(spec1),
             "overlay:%s=%s+%s+%s",
             g_merged, g_upper, g_lower1, g_lower2);
    snprintf(spec2, sizeof(spec2), "overlay:/aux=%s", g_lower1);
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.remap_rules[0] = spec1;
    cfg.remap_rules[1] = spec2;
    cfg.remap_rule_count = 2;
    sud_runtime_config_test_install(&cfg);
    sud_overlay_reset_for_testing();
    sud_overlay_init();
    TASSERT_EQ(sud_overlay_rule_count(), 2, "two rules parsed");

    char out[PATH_MAX];
    int rc = sud_overlay_resolve("/aux/foo", 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "second rule applies");

    fixture_teardown();
}

/* PLAN.md line 49 — passthrough rule kind.  Three properties:
 *
 *   1. A bare passthrough rule matches the prefix and reports
 *      PASSTHROUGH (no rewrite, regardless of for_write).
 *   2. A passthrough rule listed BEFORE a wider overlay rule wins
 *      for paths inside its sub-prefix — this is the "carve-out"
 *      idiom users will reach for in practice.
 *   3. Passthrough is honoured by the *at-syscall resolver too, and
 *      by the directory-open special case (no synthetic merged dir
 *      is built — the kernel sees the raw open). */
static void test_passthrough_rule(void)
{
    g_curtest = "passthrough_rule";
    fixture_setup();

    /* (1) Bare passthrough: just the rule, no overlay/remap. */
    char spec[PATH_MAX * 2];
    snprintf(spec, sizeof(spec), "passthrough:%s", g_merged);
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.remap_rules[0] = spec;
    cfg.remap_rule_count = 1;
    sud_runtime_config_test_install(&cfg);
    sud_overlay_reset_for_testing();
    sud_overlay_init();
    TASSERT_EQ(sud_overlay_rule_count(), 1, "passthrough rule parsed");

    char path[PATH_MAX], out[PATH_MAX];
    snprintf(path, sizeof(path), "%s/anything", g_merged);
    int rc = sud_overlay_resolve(path, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_PASSTHROUGH, "read passes through");
    rc = sud_overlay_resolve(path, 1, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_PASSTHROUGH, "write passes through");

    /* Boundary: must match on a path-component boundary (no
     * "/mergedfoo" matching "/merged"). */
    char neighbour[PATH_MAX];
    snprintf(neighbour, sizeof(neighbour), "%s_neighbour/x", g_merged);
    rc = sud_overlay_resolve(neighbour, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_PASSTHROUGH, "non-boundary not matched");

    /* (2) Carve-out: passthrough rule listed first wins for its
     * sub-prefix even though a wider overlay rule covers the parent.
     * Use an inner subdir of the existing merged tree as the carve. */
    char carve[PATH_MAX], carve_path[PATH_MAX];
    char overlay_spec[PATH_MAX * 4], passthrough_spec[PATH_MAX * 2];
    snprintf(carve, sizeof(carve), "%s/dev", g_merged);
    snprintf(carve_path, sizeof(carve_path), "%s/null", carve);
    snprintf(passthrough_spec, sizeof(passthrough_spec),
             "passthrough:%s", carve);
    snprintf(overlay_spec, sizeof(overlay_spec),
             "overlay:%s=%s+%s+%s",
             g_merged, g_upper, g_lower1, g_lower2);
    sud_runtime_config_clear(&cfg);
    /* Order matters: passthrough must come before the overlay rule it
     * carves out of, since find_rule() returns the first match. */
    cfg.remap_rules[0] = passthrough_spec;
    cfg.remap_rules[1] = overlay_spec;
    cfg.remap_rule_count = 2;
    sud_runtime_config_test_install(&cfg);
    sud_overlay_reset_for_testing();
    sud_overlay_init();
    TASSERT_EQ(sud_overlay_rule_count(), 2, "carve-out: 2 rules");

    rc = sud_overlay_resolve(carve_path, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_PASSTHROUGH, "carve-out wins for sub-prefix");

    /* The wider overlay rule still applies to siblings of the carve. */
    char sibling[PATH_MAX];
    snprintf(sibling, sizeof(sibling), "%s/etc", g_merged);
    /* Make sure the upper exists so the resolve path picks an upper
     * answer (test fixture creates upper as a real dir). */
    rc = sud_overlay_resolve(sibling, 1, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_RESOLVED, "overlay still owns siblings");

    /* (3) The *at-form delegates to sud_overlay_resolve; spot-check
     * that passthrough propagates through it for an absolute path. */
    rc = sud_overlay_resolve_at(AT_FDCWD, carve_path, 0, out, sizeof(out));
    TASSERT_EQ(rc, SUD_OVERLAY_PASSTHROUGH, "_at honours passthrough");

    /* Directory open on a passthrough prefix must report NO_DIR so
     * the caller falls back to a raw kernel openat (no synthetic
     * merged dir is materialised). */
    int dfd = sud_overlay_open_dir(carve, O_RDONLY | O_DIRECTORY, 0);
    TASSERT_EQ(dfd, SUD_OVERLAY_NO_DIR, "open_dir on passthrough = NO_DIR");

    /* create_whiteout on a passthrough prefix is a no-op (returns 0)
     * — passthrough has no upper layer to mark. */
    int wo = sud_overlay_create_whiteout(carve_path);
    TASSERT_EQ(wo, 0, "create_whiteout on passthrough is a no-op");

    fixture_teardown();
}

/* ---- Driver -------------------------------------------------------- */

/* Exposed by sud/path_remap/tests/test_fakeroot.c — linked into the
 * same binary so a single test invocation covers the full path_remap
 * surface. */
extern int run_fakeroot_tests(void);

int main(int argc, char **argv)
{
    (void)argc; (void)argv;

    test_simple_remap_compat();
    test_lower_precedence();
    test_upper_overrides_lower();
    test_write_redirects_to_upper_with_parents();
    test_whiteout_masks_lower();
    test_create_whiteout_after_unlink();
    test_readonly_overlay();
    test_passthrough_for_unrelated_paths();
    test_merged_directory_listing();
    test_resolve_at_with_dirfd();
    test_multi_rule_parsing();
    test_passthrough_rule();

    int fr_fail = run_fakeroot_tests();

    if (g_failures || fr_fail) {
        char buf[64];
        snprintf(buf, sizeof(buf),
                 "overlay test: %d failure(s)\n", g_failures + fr_fail);
        test_log(buf);
        return 1;
    }
    test_log("overlay test: all overlay tests passed\n");
    return 0;
}
