/*
 * sud/fake-exec/tests/test_fake_exec.c — Unit tests for the
 * fake-exec addin's classifier and builtin registry.
 *
 * Built freestanding (no libc) for both 32-bit and 64-bit, mirroring
 * the path_remap and inramfs test harnesses.  We exercise pure
 * functions only — the actual SYS_exit-emitting path in addin.c is
 * not test-callable because it is supposed to terminate the calling
 * task.  E2E coverage of that path lives in tests/sudtrace_test.sh.
 */

#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"
#include "sud/fake-exec/fake_exec.h"
#include "sud/fake-exec/builtins.h"

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

/* ---- Tiny test framework (mirrors test_dispatcher.c) ------------- */

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

/* ---- Test helpers ------------------------------------------------ */

static void with_clean_config(void)
{
    sud_runtime_config_clear(&g_sud_runtime_config);
    g_sud_runtime_config_present = 1;
}

/* ---- Tests ------------------------------------------------------- */

static void test_lookup_canonical_paths(void)
{
    g_curtest = "lookup_canonical_paths";
    TASSERT(sud_fake_exec_lookup("/usr/bin/true")  != 0, "true canonical");
    TASSERT(sud_fake_exec_lookup("/usr/bin/false") != 0, "false canonical");
    TASSERT(sud_fake_exec_lookup("/bin/true")      != 0, "true /bin alias");
    TASSERT(sud_fake_exec_lookup("/bin/false")     != 0, "false /bin alias");
    TASSERT(sud_fake_exec_lookup("/usr/bin/:")     != 0, "colon");
}

static void test_lookup_basename_only(void)
{
    g_curtest = "lookup_basename_only";
    /* Bare basename (no slash) matches via basename fallback. */
    TASSERT(sud_fake_exec_lookup("true")  != 0, "true bare");
    TASSERT(sud_fake_exec_lookup("false") != 0, "false bare");
}

static void test_lookup_unknown_paths(void)
{
    g_curtest = "lookup_unknown_paths";
    TASSERT_EQ(sud_fake_exec_lookup("/usr/bin/echo"),    0, "echo not in MVP");
    TASSERT_EQ(sud_fake_exec_lookup("/usr/local/bin/true"), 0,
               "wrong-prefix true rejected (canonical-only)");
    TASSERT_EQ(sud_fake_exec_lookup(""),  0, "empty path");
    TASSERT_EQ(sud_fake_exec_lookup(0),   0, "null path");
}

static void test_builtin_exit_status(void)
{
    g_curtest = "builtin_exit_status";
    const struct sud_fake_exec_builtin *bt = sud_fake_exec_lookup("/usr/bin/true");
    const struct sud_fake_exec_builtin *bf = sud_fake_exec_lookup("/usr/bin/false");
    TASSERT(bt && bt->run_inline, "true has run_inline");
    TASSERT(bf && bf->run_inline, "false has run_inline");
    TASSERT_EQ(bt->run_inline(0, 0), 0, "true exits 0");
    TASSERT_EQ(bf->run_inline(0, 0), 1, "false exits 1");
    TASSERT(bt->flags & FAKE_EXEC_VFORK_SAFE, "true is vfork-safe");
    TASSERT(bf->flags & FAKE_EXEC_VFORK_SAFE, "false is vfork-safe");
}

static void test_classify_happy_path_true(void)
{
    g_curtest = "classify_happy_path_true";
    with_clean_config();
    char *argv[] = { (char *)"true", 0 };
    char *envp[] = { (char *)"PATH=/usr/bin", 0 };
    struct fake_exec_decision d;
    int rc = sud_fake_exec_classify("/usr/bin/true", argv, envp, &d);
    TASSERT_EQ(rc, 0, "classify rc");
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE, "track inline");
    TASSERT(d.builtin != 0, "builtin set");
    TASSERT_EQ(d.exit_status, 0, "exit 0");
}

static void test_classify_happy_path_false(void)
{
    g_curtest = "classify_happy_path_false";
    with_clean_config();
    char *argv[] = { (char *)"false", 0 };
    struct fake_exec_decision d;
    int rc = sud_fake_exec_classify("/usr/bin/false", argv, 0, &d);
    TASSERT_EQ(rc, 0, "classify rc");
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE, "track inline");
    TASSERT_EQ(d.exit_status, 1, "exit 1");
}

static void test_classify_unknown_passthrough(void)
{
    g_curtest = "classify_unknown_passthrough";
    with_clean_config();
    char *argv[] = { (char *)"echo", (char *)"hi", 0 };
    struct fake_exec_decision d;
    int rc = sud_fake_exec_classify("/usr/bin/echo", argv, 0, &d);
    TASSERT_EQ(rc, 0, "classify rc");
    TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH, "passthrough on unknown");
    TASSERT_EQ(d.builtin, 0, "no builtin");
}

static void test_classify_dangerous_envp(void)
{
    g_curtest = "classify_dangerous_envp";
    with_clean_config();
    char *argv[] = { (char *)"true", 0 };
    {
        char *envp[] = { (char *)"LD_PRELOAD=/tmp/x.so", 0 };
        struct fake_exec_decision d;
        sud_fake_exec_classify("/usr/bin/true", argv, envp, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "LD_PRELOAD forces passthrough");
    }
    {
        char *envp[] = { (char *)"LD_LIBRARY_PATH=/lib/foo", 0 };
        struct fake_exec_decision d;
        sud_fake_exec_classify("/usr/bin/true", argv, envp, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "LD_LIBRARY_PATH forces passthrough");
    }
    {
        char *envp[] = { (char *)"LD_AUDIT=foo.so", 0 };
        struct fake_exec_decision d;
        sud_fake_exec_classify("/usr/bin/true", argv, envp, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "LD_AUDIT forces passthrough");
    }
    {
        /* Non-dangerous envvars should still classify as inline. */
        char *envp[] = { (char *)"LANG=C",
                         (char *)"PATH=/usr/bin:/bin", 0 };
        struct fake_exec_decision d;
        sud_fake_exec_classify("/usr/bin/true", argv, envp, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE,
                   "benign envp keeps inline");
    }
}

static void test_classify_off_disables_addin(void)
{
    g_curtest = "classify_off_disables_addin";
    with_clean_config();
    g_sud_runtime_config.fake_exec_off = 1;
    char *argv[] = { (char *)"true", 0 };
    struct fake_exec_decision d;
    sud_fake_exec_classify("/usr/bin/true", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
               "--fake-exec off forces passthrough");
}

static void test_classify_deny_list(void)
{
    g_curtest = "classify_deny_list";
    with_clean_config();
    g_sud_runtime_config.fake_exec_deny[0] = "true";
    g_sud_runtime_config.fake_exec_deny_count = 1;
    {
        char *argv[] = { (char *)"true", 0 };
        struct fake_exec_decision d;
        sud_fake_exec_classify("/usr/bin/true", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "denied basename forces passthrough");
    }
    /* false is not denied, should still elide. */
    {
        char *argv[] = { (char *)"false", 0 };
        struct fake_exec_decision d;
        sud_fake_exec_classify("/usr/bin/false", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE,
                   "non-denied builtin still elides");
    }
}

static void test_runtime_config_emit_includes_fake_exec(void)
{
    g_curtest = "runtime_config_emit_includes_fake_exec";
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.fake_exec_off = 1;
    cfg.fake_exec_deny[0] = "echo";
    cfg.fake_exec_deny[1] = "printf";
    cfg.fake_exec_deny_count = 2;
    const char *out[16];
    char scratch[64];
    int n = sud_runtime_config_emit(&cfg, out, 16, scratch, sizeof(scratch));
    TASSERT(n >= 6, "emitted at least 6 args");
    /* Find "--fake-exec" + "off" pair. */
    int saw_off = 0, saw_deny_echo = 0, saw_deny_printf = 0;
    for (int i = 0; i + 1 < n; i++) {
        if (strcmp(out[i], "--fake-exec") == 0 && strcmp(out[i+1], "off") == 0)
            saw_off = 1;
        if (strcmp(out[i], "--fake-exec-deny") == 0 &&
            strcmp(out[i+1], "echo") == 0) saw_deny_echo = 1;
        if (strcmp(out[i], "--fake-exec-deny") == 0 &&
            strcmp(out[i+1], "printf") == 0) saw_deny_printf = 1;
    }
    TASSERT(saw_off, "--fake-exec off emitted");
    TASSERT(saw_deny_echo, "--fake-exec-deny echo emitted");
    TASSERT(saw_deny_printf, "--fake-exec-deny printf emitted");
}

static void test_runtime_config_parse_fake_exec(void)
{
    g_curtest = "runtime_config_parse_fake_exec";
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    char *argv[] = {
        (char *)"sud64",
        (char *)"--fake-exec", (char *)"off",
        (char *)"--fake-exec-deny", (char *)"true",
        (char *)"--fake-exec-deny", (char *)"false",
        (char *)"/bin/echo", (char *)"hi",
        0
    };
    int argc = 9;
    int argi = 1;
    int rc = sud_runtime_config_parse(argc, argv, &argi, &cfg);
    TASSERT_EQ(rc, 0, "parse rc");
    TASSERT_EQ(cfg.fake_exec_off, 1, "off set");
    TASSERT_EQ(cfg.fake_exec_deny_count, 2, "deny count");
    TASSERT(strcmp(cfg.fake_exec_deny[0], "true") == 0, "deny[0] true");
    TASSERT(strcmp(cfg.fake_exec_deny[1], "false") == 0, "deny[1] false");
    TASSERT_EQ(argi, 7, "stops at /bin/echo");
}

/* ---- Entrypoint --------------------------------------------------- */

int main(int argc, char **argv)
{
    (void)argc; (void)argv;

    test_lookup_canonical_paths();
    test_lookup_basename_only();
    test_lookup_unknown_paths();
    test_builtin_exit_status();
    test_classify_happy_path_true();
    test_classify_happy_path_false();
    test_classify_unknown_passthrough();
    test_classify_dangerous_envp();
    test_classify_off_disables_addin();
    test_classify_deny_list();
    test_runtime_config_emit_includes_fake_exec();
    test_runtime_config_parse_fake_exec();

    if (g_failures == 0) {
        const char *ok = "fake-exec tests OK\n";
        write(1, ok, strlen(ok));
        return 0;
    }
    char buf[64];
    snprintf(buf, sizeof(buf), "fake-exec tests FAILED (%d)\n", g_failures);
    test_log(buf);
    return 1;
}
