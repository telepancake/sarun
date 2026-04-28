/*
 * sud/path_remap/tests/test_dispatcher.c — Verifies the addin
 * dispatch order and the contract between sud_path_remap_addin and
 * sud_trace_addin.
 *
 * The real sud_addins_pre_syscall / sud_addins_post_syscall
 * dispatcher (sud/addin.c) iterates a static, compile-time addin
 * array.  This test links that dispatcher together with the real
 * sud_path_remap_addin and a *stub* sud_trace_addin defined locally,
 * so that the test can observe exactly what each addin saw of the
 * syscall context, and in what order.
 *
 * Each test case calls sud_addins_pre_syscall() against a real
 * sud_syscall_ctx initialized with a known SUD_OVERLAY rule, and
 * inspects the side-effects recorded by the trace stub.
 *
 * Built once with both addins (-DSUD_ADDIN_TRACE -DSUD_ADDIN_PATH_REMAP)
 * to verify the "both" case, and twice more (one addin each) to
 * verify either addin is usable on its own.  All three modes run
 * for both 32-bit and 64-bit.
 */

#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"
#include "sud/addin.h"
#include "sud/raw.h"

#if defined(SUD_ADDIN_PATH_REMAP)
#include "sud/path_remap/overlay.h"
#endif

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

#ifndef AT_REMOVEDIR
#define AT_REMOVEDIR 0x200
#endif

/* ---- Trace addin stub (only when SUD_ADDIN_TRACE is defined) ----- */
#ifdef SUD_ADDIN_TRACE

/* What the stub trace addin observed during pre/post.  The dispatcher
 * is single-threaded for any one ctx, so plain globals suffice. */
struct trace_observation {
    int    pre_called;
    int    post_called;
    int    pre_call_index;     /* monotonic counter of pre_syscall calls */
    int    post_call_index;
    long   pre_args[6];
    long   post_args[6];
    long   pre_nr;
    long   post_nr;
    long   post_ret;
};
static struct trace_observation g_trace;
static int g_pre_seq;
static int g_post_seq;

static void trace_stub_init(void)        {}
static void trace_stub_target_launch(const struct sud_tracee_launch *l) { (void)l; }
static void trace_stub_fork_child(void)  {}

static int trace_stub_pre_syscall(struct sud_syscall_ctx *ctx)
{
    g_trace.pre_called = 1;
    g_trace.pre_call_index = ++g_pre_seq;
    g_trace.pre_nr = ctx->nr;
    for (int i = 0; i < 6; i++) g_trace.pre_args[i] = ctx->args[i];
    return 0;  /* never short-circuit from the trace stub */
}

static void trace_stub_post_syscall(const struct sud_syscall_ctx *ctx)
{
    g_trace.post_called = 1;
    g_trace.post_call_index = ++g_post_seq;
    g_trace.post_nr = ctx->nr;
    for (int i = 0; i < 6; i++) g_trace.post_args[i] = ctx->args[i];
    g_trace.post_ret = ctx->ret;
}

const struct sud_addin sud_trace_addin = {
    "trace-stub",
    trace_stub_init,
    trace_stub_target_launch,
    trace_stub_fork_child,
    trace_stub_pre_syscall,
    trace_stub_post_syscall,
};

static void trace_observation_reset(void)
{
    memset(&g_trace, 0, sizeof(g_trace));
}

#endif /* SUD_ADDIN_TRACE */

/* ---- Tiny test framework ----------------------------------------- */

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

/* ---- Filesystem helpers (raw-syscall, libc-fs free) -------------- */

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
                             0020000 | 0, 0, 0, 0);
#else
    return (int)raw_syscall6(__NR_mknod, (long)p, 0020000 | 0, 0, 0, 0, 0);
#endif
}

static void t_rm_rf(const char *path)
{
    int fd = (int)raw_syscall6(SYS_openat, AT_FDCWD, (long)path,
                               O_RDONLY | O_DIRECTORY | O_NOFOLLOW,
                               0, 0, 0);
    if (fd < 0) { t_unlink(path); return; }
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

/* ---- Test fixture ------------------------------------------------ */

static char g_tmp[PATH_MAX];
static char g_lower[PATH_MAX];
static char g_upper[PATH_MAX];
static char g_merged[PATH_MAX];

static void fixture_setup(void)
{
    long pid = raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
    snprintf(g_tmp,    sizeof(g_tmp),    "/tmp/sud-dispatch-test-%ld", pid);
    snprintf(g_lower,  sizeof(g_lower),  "%s/lower",  g_tmp);
    snprintf(g_upper,  sizeof(g_upper),  "%s/upper",  g_tmp);
    snprintf(g_merged, sizeof(g_merged), "%s/merged", g_tmp);
    t_rm_rf(g_tmp);
    t_mkdir(g_tmp,   0755);
    t_mkdir(g_lower, 0755);
    t_mkdir(g_upper, 0755);
}

static void fixture_teardown(void)
{
    t_rm_rf(g_tmp);
    long pid = raw_syscall6(SYS_getpid, 0, 0, 0, 0, 0, 0);
    char synth[PATH_MAX];
    snprintf(synth, sizeof(synth), "/tmp/.sud-overlay/%ld", pid);
    t_rm_rf(synth);
}

#ifdef SUD_ADDIN_PATH_REMAP
static void install_overlay(void)
{
    char env[PATH_MAX * 4];
    snprintf(env, sizeof(env), "%s=%s+%s", g_merged, g_upper, g_lower);
    setenv("SUD_OVERLAY", env, 1);
    unsetenv("SUD_REMAP");
    sud_overlay_reset_for_testing();
    sud_overlay_init();
}

static void install_readonly_overlay(void)
{
    char env[PATH_MAX * 4];
    snprintf(env, sizeof(env), "%s=+%s", g_merged, g_lower);
    setenv("SUD_OVERLAY", env, 1);
    unsetenv("SUD_REMAP");
    sud_overlay_reset_for_testing();
    sud_overlay_init();
}

static void install_no_overlay(void)
{
    unsetenv("SUD_OVERLAY");
    unsetenv("SUD_REMAP");
    sud_overlay_reset_for_testing();
    sud_overlay_init();
}
#endif

/* Run the wrapper_init hooks for every linked addin.  This is what
 * sud's startup does. */
static void dispatcher_init(void)
{
    sud_addins_wrapper_init();
}

/* Build a (mostly) zero-initialized syscall ctx with a scratch buffer
 * for path_remap to write into.  Caller fills in nr/args. */
static char g_scratch[PATH_MAX];

static struct sud_syscall_ctx make_ctx(void)
{
    struct sud_syscall_ctx ctx;
    memset(&ctx, 0, sizeof(ctx));
    ctx.scratch = g_scratch;
    ctx.scratch_size = sizeof(g_scratch);
    g_scratch[0] = '\0';
    return ctx;
}

/* ---- Tests ------------------------------------------------------- */

#if defined(SUD_ADDIN_TRACE) && defined(SUD_ADDIN_PATH_REMAP)
/* "Both addins" mode: we expect path_remap to run before trace, and
 * trace's pre/post hooks to see the rewritten path. */

static void test_both_trace_sees_remapped_openat(void)
{
    g_curtest = "both/trace_sees_remapped_openat";
    fixture_setup();
    install_overlay();

    /* Layout: lower has /merged/foo */
    char p[PATH_MAX]; snprintf(p, sizeof(p), "%s/foo", g_lower);
    t_write_file(p, "data");

    char merged_path[PATH_MAX];
    snprintf(merged_path, sizeof(merged_path), "%s/foo", g_merged);

    trace_observation_reset();
    struct sud_syscall_ctx ctx = make_ctx();
    ctx.nr      = SYS_openat;
    ctx.args[0] = AT_FDCWD;
    ctx.args[1] = (long)merged_path;
    ctx.args[2] = O_RDONLY;
    ctx.args[3] = 0;

    int sc = sud_addins_pre_syscall(&ctx);
    TASSERT_EQ(sc, 0, "non-O_DIRECTORY openat should not short-circuit");
    TASSERT(g_trace.pre_called, "trace pre_syscall ran");

    /* The path arg trace observed must be the rewritten lower path. */
    char want[PATH_MAX];
    snprintf(want, sizeof(want), "%s/foo", g_lower);
    TASSERT_STREQ((const char *)g_trace.pre_args[1], want,
                  "trace pre saw remapped path");

    /* Simulate kernel ret then run post. */
    ctx.ret = 42;
    sud_addins_post_syscall(&ctx);
    TASSERT(g_trace.post_called, "trace post_syscall ran");
    TASSERT_STREQ((const char *)g_trace.post_args[1], want,
                  "trace post saw remapped path");

    fixture_teardown();
}

static void test_both_pre_called_in_correct_order(void)
{
    /* path_remap must run before trace.  We can verify this by setting
     * up a whiteout so that path_remap short-circuits with -ENOENT;
     * if path_remap ran AFTER trace, trace would have run first and
     * its pre_call_index would be 1.  With path_remap first, trace's
     * pre_call_index stays 0 (never invoked). */
    g_curtest = "both/short_circuit_skips_trace";
    fixture_setup();
    install_overlay();

    /* lower has fileX, upper has whiteout for fileX. */
    char p[PATH_MAX];
    snprintf(p, sizeof(p), "%s/fileX", g_lower); t_write_file(p, "secret");
    snprintf(p, sizeof(p), "%s/fileX", g_upper);
    TASSERT_EQ(t_mknod_chr(p), 0, "create whiteout");

    char merged_path[PATH_MAX];
    snprintf(merged_path, sizeof(merged_path), "%s/fileX", g_merged);

    trace_observation_reset();
    struct sud_syscall_ctx ctx = make_ctx();
    ctx.nr      = SYS_openat;
    ctx.args[0] = AT_FDCWD;
    ctx.args[1] = (long)merged_path;
    ctx.args[2] = O_RDONLY;

    int sc = sud_addins_pre_syscall(&ctx);
    TASSERT_EQ(sc, 1, "whiteout short-circuits the dispatcher");
    TASSERT_EQ(ctx.ret, -ENOENT, "whiteout returns -ENOENT");
    TASSERT_EQ(g_trace.pre_called, 0,
               "trace pre_syscall NOT invoked after path_remap short-circuit");

    fixture_teardown();
}

static void test_both_readonly_overlay_blocks_writes_before_trace(void)
{
    g_curtest = "both/readonly_blocks_before_trace";
    fixture_setup();
    install_readonly_overlay();

    char merged_path[PATH_MAX];
    snprintf(merged_path, sizeof(merged_path), "%s/new", g_merged);

    trace_observation_reset();
    struct sud_syscall_ctx ctx = make_ctx();
    ctx.nr      = SYS_openat;
    ctx.args[0] = AT_FDCWD;
    ctx.args[1] = (long)merged_path;
    ctx.args[2] = O_WRONLY | O_CREAT;
    ctx.args[3] = 0644;

    int sc = sud_addins_pre_syscall(&ctx);
    TASSERT_EQ(sc, 1, "read-only overlay short-circuits writes");
    TASSERT(ctx.ret < 0, "EROFS or similar returned");
    TASSERT_EQ(g_trace.pre_called, 0,
               "trace pre NOT invoked after readonly short-circuit");

    fixture_teardown();
}

static void test_both_no_rules_means_path_unchanged(void)
{
    /* With no SUD_OVERLAY/SUD_REMAP env, path_remap is a no-op and
     * trace simply sees the original args. */
    g_curtest = "both/no_rules_passthrough";
    fixture_setup();
    install_no_overlay();

    const char *path = "/etc/passwd";
    trace_observation_reset();
    struct sud_syscall_ctx ctx = make_ctx();
    ctx.nr      = SYS_openat;
    ctx.args[0] = AT_FDCWD;
    ctx.args[1] = (long)path;
    ctx.args[2] = O_RDONLY;

    int sc = sud_addins_pre_syscall(&ctx);
    TASSERT_EQ(sc, 0, "no rules: dispatcher passes through");
    TASSERT(g_trace.pre_called, "trace pre still ran");
    TASSERT_STREQ((const char *)g_trace.pre_args[1], path,
                  "trace pre saw original path (unchanged)");

    fixture_teardown();
}

static void test_both_unrelated_path_passthrough(void)
{
    g_curtest = "both/unrelated_path_passthrough";
    fixture_setup();
    install_overlay();

    /* /etc/passwd is not under the overlay rule. */
    const char *path = "/etc/passwd";
    trace_observation_reset();
    struct sud_syscall_ctx ctx = make_ctx();
    ctx.nr      = SYS_openat;
    ctx.args[0] = AT_FDCWD;
    ctx.args[1] = (long)path;
    ctx.args[2] = O_RDONLY;

    int sc = sud_addins_pre_syscall(&ctx);
    TASSERT_EQ(sc, 0, "unrelated path: dispatcher passes through");
    TASSERT(g_trace.pre_called, "trace pre still ran");
    TASSERT_STREQ((const char *)g_trace.pre_args[1], path,
                  "trace pre saw unmodified path");

    fixture_teardown();
}

#endif /* SUD_ADDIN_TRACE && SUD_ADDIN_PATH_REMAP */

#if defined(SUD_ADDIN_PATH_REMAP) && !defined(SUD_ADDIN_TRACE)
/* "path_remap only" mode: dispatcher must run path_remap and rewrite
 * the path.  Trace symbol is NOT linked. */

static void test_only_pathremap_rewrites_path(void)
{
    g_curtest = "only_pathremap/rewrites_path";
    fixture_setup();
    install_overlay();

    char p[PATH_MAX]; snprintf(p, sizeof(p), "%s/foo", g_lower);
    t_write_file(p, "x");

    char merged_path[PATH_MAX];
    snprintf(merged_path, sizeof(merged_path), "%s/foo", g_merged);

    struct sud_syscall_ctx ctx = make_ctx();
    ctx.nr      = SYS_openat;
    ctx.args[0] = AT_FDCWD;
    ctx.args[1] = (long)merged_path;
    ctx.args[2] = O_RDONLY;

    int sc = sud_addins_pre_syscall(&ctx);
    TASSERT_EQ(sc, 0, "no short-circuit on plain read");

    char want[PATH_MAX];
    snprintf(want, sizeof(want), "%s/foo", g_lower);
    TASSERT_STREQ((const char *)ctx.args[1], want,
                  "ctx.args[1] rewritten by path_remap");

    fixture_teardown();
}

#endif /* SUD_ADDIN_PATH_REMAP only */

#if defined(SUD_ADDIN_TRACE) && !defined(SUD_ADDIN_PATH_REMAP)
/* "trace only" mode: dispatcher must run trace and leave args alone.
 * path_remap symbol is NOT linked. */

static void test_only_trace_leaves_path_alone(void)
{
    g_curtest = "only_trace/leaves_path_alone";
    fixture_setup();

    const char *path = "/some/arbitrary/path";
    trace_observation_reset();
    struct sud_syscall_ctx ctx = make_ctx();
    ctx.nr      = SYS_openat;
    ctx.args[0] = AT_FDCWD;
    ctx.args[1] = (long)path;
    ctx.args[2] = O_RDONLY;

    int sc = sud_addins_pre_syscall(&ctx);
    TASSERT_EQ(sc, 0, "no short-circuit");
    TASSERT(g_trace.pre_called, "trace pre invoked");
    TASSERT_STREQ((const char *)g_trace.pre_args[1], path,
                  "trace pre saw original path (no path_remap linked)");
    TASSERT_STREQ((const char *)ctx.args[1], path,
                  "ctx.args[1] unchanged after dispatcher");

    fixture_teardown();
}

#endif /* SUD_ADDIN_TRACE only */

/* ---- Driver ------------------------------------------------------ */

int main(int argc, char **argv)
{
    (void)argc; (void)argv;

    dispatcher_init();

#if defined(SUD_ADDIN_TRACE) && defined(SUD_ADDIN_PATH_REMAP)
    test_both_trace_sees_remapped_openat();
    test_both_pre_called_in_correct_order();
    test_both_readonly_overlay_blocks_writes_before_trace();
    test_both_no_rules_means_path_unchanged();
    test_both_unrelated_path_passthrough();
    const char *mode = "BOTH";
#elif defined(SUD_ADDIN_PATH_REMAP)
    test_only_pathremap_rewrites_path();
    const char *mode = "PATH_REMAP_ONLY";
#elif defined(SUD_ADDIN_TRACE)
    test_only_trace_leaves_path_alone();
    const char *mode = "TRACE_ONLY";
#else
    const char *mode = "NONE";
#endif

    if (g_failures) {
        char buf[128];
        snprintf(buf, sizeof(buf),
                 "dispatcher test (%s): %d failure(s)\n", mode, g_failures);
        test_log(buf);
        return 1;
    }
    char buf[128];
    snprintf(buf, sizeof(buf),
             "dispatcher test (%s): all interaction tests passed\n", mode);
    test_log(buf);
    return 0;
}
