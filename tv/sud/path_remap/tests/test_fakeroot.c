/*
 * sud/path_remap/tests/test_fakeroot.c — Unit tests for the fakeroot
 * metadata-override layer (sud/path_remap/fakeroot.{c,h}).
 *
 * The dispatcher integration (chown/chmod/getuid short-circuits, stat
 * post-processing) lives in sud/path_remap/addin.c and is exercised
 * by the dispatcher tests; this file pins down the layer's own
 * primitives in isolation:
 *
 *   - prefix matching (component boundary, trailing slashes)
 *   - active() vs reset_for_testing()
 *   - record_chown(uid=-1, gid=-1) preserving the other field
 *   - lookup() flag bitmask
 *   - patch_kernel_stat / patch_kernel_stat64 in-place patching,
 *     with mode-bit type preservation
 *
 * Built freestanding the same way as test_overlay.c so it runs under
 * both -m32 and -m64 with the real fakeroot.c implementation linked
 * directly.
 */

#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"
#include "sud/path_remap/fakeroot.h"
#include "sud/runtime_config.h"

/* This TU is linked alongside test_overlay.c into one binary; the
 * sigreturn-restorer stubs and the actual main() live there.  We
 * export run_fakeroot_tests() so the shared driver can call us. */

/* ---- Tiny test framework (mirrors test_overlay.c) ---------------- */

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

/* ---- Local copies of the kernel stat layouts --------------------- */
struct fr_stat_x64 {
    unsigned long long st_dev;
    unsigned long long st_ino;
    unsigned long long st_nlink;
    unsigned int       st_mode;
    unsigned int       st_uid;
    unsigned int       st_gid;
    unsigned int       __pad0;
    unsigned long long st_rdev;
    long long          st_size;
    long long          st_blksize;
    long long          st_blocks;
    long long          __rest[16];
};

struct fr_stat64 {
    unsigned long long st_dev;
    unsigned char      __pad0[4];
    unsigned long      __st_ino;
    unsigned int       st_mode;
    unsigned int       st_nlink;
    unsigned long      st_uid;
    unsigned long      st_gid;
    unsigned long long st_rdev;
    unsigned char      __pad3[4];
    long long          st_size;
    unsigned long      st_blksize;
    unsigned long long st_blocks;
    unsigned long long st_ino;
    unsigned long      __rest[16];
};

/* ---- Helpers ----------------------------------------------------- */

static void install_rules(const char **rules, int n)
{
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    for (int i = 0; i < n; i++) cfg.remap_rules[i] = rules[i];
    cfg.remap_rule_count = n;
    sud_runtime_config_test_install(&cfg);
    sud_fakeroot_reset_for_testing();
    sud_fakeroot_init();
}

/* ---- Tests ------------------------------------------------------- */

static void test_no_rules_inactive(void)
{
    g_curtest = "no_rules_inactive";
    install_rules(0, 0);
    TASSERT_EQ(sud_fakeroot_active(), 0, "active=0 with no rules");
    TASSERT_EQ(sud_fakeroot_match("/anything"), 0, "match=0 with no rules");
}

static void test_prefix_match_boundary(void)
{
    g_curtest = "prefix_match_boundary";
    const char *r[] = { "fakeroot:/build" };
    install_rules(r, 1);

    TASSERT_EQ(sud_fakeroot_active(), 1, "active=1");
    TASSERT_EQ(sud_fakeroot_match("/build"),       1, "exact prefix");
    TASSERT_EQ(sud_fakeroot_match("/build/foo"),   1, "deeper path");
    TASSERT_EQ(sud_fakeroot_match("/build2"),      0, "non-boundary");
    TASSERT_EQ(sud_fakeroot_match("/buildfoo/x"),  0, "non-boundary 2");
    TASSERT_EQ(sud_fakeroot_match("/elsewhere"),   0, "unrelated");
    TASSERT_EQ(sud_fakeroot_match(0),              0, "NULL path");
    TASSERT_EQ(sud_fakeroot_match("relative"),     0, "relative path");
}

static void test_trailing_slash_stripped(void)
{
    g_curtest = "trailing_slash_stripped";
    const char *r[] = { "fakeroot:/opt/stage/" };
    install_rules(r, 1);
    TASSERT_EQ(sud_fakeroot_match("/opt/stage"),      1, "matches stripped");
    TASSERT_EQ(sud_fakeroot_match("/opt/stage/bin"),  1, "matches deeper");
    TASSERT_EQ(sud_fakeroot_match("/opt/staged"),     0, "non-boundary");
}

static void test_root_prefix(void)
{
    g_curtest = "root_prefix";
    const char *r[] = { "fakeroot:/" };
    install_rules(r, 1);
    TASSERT_EQ(sud_fakeroot_match("/"),         1, "root matches itself");
    TASSERT_EQ(sud_fakeroot_match("/etc"),      1, "root matches /etc");
    TASSERT_EQ(sud_fakeroot_match("/var/log"),  1, "root matches deep");
}

static void test_record_and_lookup(void)
{
    g_curtest = "record_and_lookup";
    const char *r[] = { "fakeroot:/x" };
    install_rules(r, 1);

    /* Initially no override. */
    unsigned int u = 99, g = 99, m = 0;
    unsigned f = sud_fakeroot_lookup(1, 42, &u, &g, &m);
    TASSERT_EQ(f, 0, "no override before record");

    /* Record uid only. */
    sud_fakeroot_record_chown(1, 42, 1000, -1);
    f = sud_fakeroot_lookup(1, 42, &u, &g, &m);
    TASSERT_EQ(f, SUD_FAKEROOT_HAS_UID, "uid set, gid unset");
    TASSERT_EQ(u, 1000, "uid value");

    /* Record gid only — uid override must be preserved. */
    sud_fakeroot_record_chown(1, 42, -1, 2000);
    f = sud_fakeroot_lookup(1, 42, &u, &g, &m);
    TASSERT_EQ(f, SUD_FAKEROOT_HAS_UID | SUD_FAKEROOT_HAS_GID,
               "uid+gid both set");
    TASSERT_EQ(u, 1000, "uid preserved");
    TASSERT_EQ(g, 2000, "gid value");

    /* Record mode. */
    sud_fakeroot_record_chmod(1, 42, 0755);
    f = sud_fakeroot_lookup(1, 42, &u, &g, &m);
    TASSERT_EQ(f, SUD_FAKEROOT_HAS_UID | SUD_FAKEROOT_HAS_GID
                  | SUD_FAKEROOT_HAS_MODE, "all three set");
    TASSERT_EQ(m, 0755, "mode value (07777 mask)");

    /* Distinct (dev, ino) is independent. */
    f = sud_fakeroot_lookup(2, 42, &u, &g, &m);
    TASSERT_EQ(f, 0, "different dev = no override");
    f = sud_fakeroot_lookup(1, 43, &u, &g, &m);
    TASSERT_EQ(f, 0, "different ino = no override");
}

static void test_chmod_strips_type_bits(void)
{
    g_curtest = "chmod_strips_type_bits";
    const char *r[] = { "fakeroot:/" };
    install_rules(r, 1);
    /* Caller passes S_IFREG | 0644; we should store only 0644. */
    sud_fakeroot_record_chmod(7, 7, S_IFREG | 0644);
    unsigned int m = 0;
    unsigned f = sud_fakeroot_lookup(7, 7, 0, 0, &m);
    TASSERT_EQ(f, SUD_FAKEROOT_HAS_MODE, "mode override present");
    TASSERT_EQ(m, 0644, "type bits stripped from stored mode");
}

static void test_patch_kernel_stat(void)
{
    g_curtest = "patch_kernel_stat";
    const char *r[] = { "fakeroot:/" };
    install_rules(r, 1);

    sud_fakeroot_record_chown(0xdead, 0xbeef, 1234, 5678);
    sud_fakeroot_record_chmod(0xdead, 0xbeef, 0750);

    struct fr_stat_x64 st;
    memset(&st, 0, sizeof(st));
    st.st_dev  = 0xdead;
    st.st_ino  = 0xbeef;
    st.st_mode = S_IFREG | 0644;     /* kernel-reported */
    st.st_uid  = 1000;
    st.st_gid  = 1000;

    sud_fakeroot_patch_kernel_stat(&st);
    TASSERT_EQ(st.st_uid, 1234, "uid patched");
    TASSERT_EQ(st.st_gid, 5678, "gid patched");
    TASSERT_EQ(st.st_mode, (unsigned int)(S_IFREG | 0750),
               "mode permission bits patched, type preserved");

    /* Untouched dev/ino = no patch. */
    struct fr_stat_x64 st2;
    memset(&st2, 0, sizeof(st2));
    st2.st_dev  = 1; st2.st_ino = 1;
    st2.st_mode = S_IFREG | 0644;
    st2.st_uid  = 999; st2.st_gid = 999;
    sud_fakeroot_patch_kernel_stat(&st2);
    TASSERT_EQ(st2.st_uid, 999, "uid untouched");
    TASSERT_EQ(st2.st_gid, 999, "gid untouched");
    TASSERT_EQ(st2.st_mode, (unsigned int)(S_IFREG | 0644),
               "mode untouched");
}

static void test_patch_kernel_stat64(void)
{
    g_curtest = "patch_kernel_stat64";
    const char *r[] = { "fakeroot:/" };
    install_rules(r, 1);

    sud_fakeroot_record_chown(11, 22, 7, 8);

    struct fr_stat64 st;
    memset(&st, 0, sizeof(st));
    st.st_dev  = 11;
    st.st_ino  = 22;
    st.st_mode = S_IFDIR | 0755;
    st.st_uid  = 1000;
    st.st_gid  = 1000;

    sud_fakeroot_patch_kernel_stat64(&st);
    TASSERT_EQ((long)st.st_uid, 7, "stat64 uid patched");
    TASSERT_EQ((long)st.st_gid, 8, "stat64 gid patched");
    TASSERT_EQ(st.st_mode, (unsigned int)(S_IFDIR | 0755),
               "stat64 mode unchanged (no chmod recorded)");
}

static void test_reset(void)
{
    g_curtest = "reset";
    const char *r[] = { "fakeroot:/x" };
    install_rules(r, 1);
    sud_fakeroot_record_chown(1, 1, 0, 0);
    TASSERT_EQ(sud_fakeroot_active(), 1, "active before reset");

    sud_fakeroot_reset_for_testing();
    TASSERT_EQ(sud_fakeroot_active(), 0, "inactive after reset");
    unsigned int u = 0, g = 0, m = 0;
    TASSERT_EQ(sud_fakeroot_lookup(1, 1, &u, &g, &m), 0,
               "override gone after reset");
}

static void test_multiple_prefixes(void)
{
    g_curtest = "multiple_prefixes";
    const char *r[] = { "fakeroot:/a", "fakeroot:/b" };
    install_rules(r, 2);
    TASSERT_EQ(sud_fakeroot_match("/a/x"), 1, "first prefix");
    TASSERT_EQ(sud_fakeroot_match("/b/y"), 1, "second prefix");
    TASSERT_EQ(sud_fakeroot_match("/c/z"), 0, "neither prefix");
}

/* ---- Entry point exposed to the shared driver -------------------- */

int run_fakeroot_tests(void)
{
    int saved = g_failures;
    test_no_rules_inactive();
    test_prefix_match_boundary();
    test_trailing_slash_stripped();
    test_root_prefix();
    test_record_and_lookup();
    test_chmod_strips_type_bits();
    test_patch_kernel_stat();
    test_patch_kernel_stat64();
    test_reset();
    test_multiple_prefixes();
    int new_failures = g_failures - saved;
    if (new_failures == 0)
        test_log("fakeroot test: all fakeroot tests passed\n");
    return new_failures;
}
