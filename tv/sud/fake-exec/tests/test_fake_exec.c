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

static char g_scratch[8192];

static void with_clean_config(void)
{
    sud_runtime_config_clear(&g_sud_runtime_config);
    g_sud_runtime_config_present = 1;
}

static int classify(const char *path, char *const *argv, char *const *envp,
                    struct fake_exec_decision *out)
{
    return sud_fake_exec_classify(path, argv, envp,
                                  g_scratch, sizeof(g_scratch), out);
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
    TASSERT(sud_fake_exec_lookup("/usr/bin/echo")  != 0, "echo canonical");
    TASSERT(sud_fake_exec_lookup("/bin/echo")      != 0, "echo /bin alias");
    TASSERT(sud_fake_exec_lookup("/usr/bin/printf") != 0, "printf canonical");
    TASSERT(sud_fake_exec_lookup("/bin/printf")    != 0, "printf /bin alias");
}

static void test_lookup_basename_only(void)
{
    g_curtest = "lookup_basename_only";
    /* Bare basename (no slash) matches via basename fallback. */
    TASSERT(sud_fake_exec_lookup("true")  != 0, "true bare");
    TASSERT(sud_fake_exec_lookup("false") != 0, "false bare");
    TASSERT(sud_fake_exec_lookup("echo")  != 0, "echo bare");
    TASSERT(sud_fake_exec_lookup("printf") != 0, "printf bare");
}

static void test_lookup_unknown_paths(void)
{
    g_curtest = "lookup_unknown_paths";
    TASSERT_EQ(sud_fake_exec_lookup("/usr/bin/cat"), 0, "cat not in MVP");
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
    int rc = classify("/usr/bin/true", argv, envp, &d);
    TASSERT_EQ(rc, 0, "classify rc");
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE, "track inline");
    TASSERT(d.builtin != 0, "builtin set");
    TASSERT_EQ(d.exit_status, 0, "exit 0");
    TASSERT_EQ(d.out_fd, -1, "no write");
}

static void test_classify_happy_path_false(void)
{
    g_curtest = "classify_happy_path_false";
    with_clean_config();
    char *argv[] = { (char *)"false", 0 };
    struct fake_exec_decision d;
    int rc = classify("/usr/bin/false", argv, 0, &d);
    TASSERT_EQ(rc, 0, "classify rc");
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE, "track inline");
    TASSERT_EQ(d.exit_status, 1, "exit 1");
    TASSERT_EQ(d.out_fd, -1, "no write");
}

static void test_classify_unknown_passthrough(void)
{
    g_curtest = "classify_unknown_passthrough";
    with_clean_config();
    char *argv[] = { (char *)"cat", (char *)"hi", 0 };
    struct fake_exec_decision d;
    int rc = classify("/usr/bin/cat", argv, 0, &d);
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
        classify("/usr/bin/true", argv, envp, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "LD_PRELOAD forces passthrough");
    }
    {
        char *envp[] = { (char *)"LD_LIBRARY_PATH=/lib/foo", 0 };
        struct fake_exec_decision d;
        classify("/usr/bin/true", argv, envp, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "LD_LIBRARY_PATH forces passthrough");
    }
    {
        char *envp[] = { (char *)"LD_AUDIT=foo.so", 0 };
        struct fake_exec_decision d;
        classify("/usr/bin/true", argv, envp, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "LD_AUDIT forces passthrough");
    }
    {
        /* Non-dangerous envvars should still classify as inline. */
        char *envp[] = { (char *)"LANG=C",
                         (char *)"PATH=/usr/bin:/bin", 0 };
        struct fake_exec_decision d;
        classify("/usr/bin/true", argv, envp, &d);
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
    classify("/usr/bin/true", argv, 0, &d);
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
        classify("/usr/bin/true", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "denied basename forces passthrough");
    }
    /* false is not denied, should still elide. */
    {
        char *argv[] = { (char *)"false", 0 };
        struct fake_exec_decision d;
        classify("/usr/bin/false", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE,
                   "non-denied builtin still elides");
    }
}

/* ---- Step B: echo / printf -------------------------------------- */

static void test_classify_echo_simple(void)
{
    g_curtest = "classify_echo_simple";
    with_clean_config();
    char *argv[] = { (char *)"echo", (char *)"hello", (char *)"world", 0 };
    struct fake_exec_decision d;
    classify("/usr/bin/echo", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE, "echo elidable");
    TASSERT_EQ(d.exit_status, 0, "echo exit 0");
    TASSERT_EQ(d.out_fd, 1, "echo writes fd 1");
    TASSERT_EQ(d.out_len, 12, "len(\"hello world\\n\") == 12");
    TASSERT(d.out_buf && memcmp(d.out_buf, "hello world\n", 12) == 0,
            "echo bytes verbatim");
}

static void test_classify_echo_no_args(void)
{
    g_curtest = "classify_echo_no_args";
    with_clean_config();
    char *argv[] = { (char *)"echo", 0 };
    struct fake_exec_decision d;
    classify("/usr/bin/echo", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE, "echo no-args elidable");
    TASSERT_EQ(d.out_len, 1, "just a newline");
    TASSERT(d.out_buf && d.out_buf[0] == '\n', "newline byte");
}

static void test_classify_echo_dash_flag_passthrough(void)
{
    g_curtest = "classify_echo_dash_flag_passthrough";
    with_clean_config();
    char *argv[] = { (char *)"echo", (char *)"-n", (char *)"hi", 0 };
    struct fake_exec_decision d;
    classify("/usr/bin/echo", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
               "echo -n forces passthrough");
}

static void test_classify_printf_literal(void)
{
    g_curtest = "classify_printf_literal";
    with_clean_config();
    char *argv[] = { (char *)"printf", (char *)"hello", 0 };
    struct fake_exec_decision d;
    classify("/usr/bin/printf", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE, "printf elidable");
    TASSERT_EQ(d.out_len, 5, "5 bytes");
    TASSERT(d.out_buf && memcmp(d.out_buf, "hello", 5) == 0,
            "printf bytes verbatim");
}

static void test_classify_printf_percent_passthrough(void)
{
    g_curtest = "classify_printf_percent_passthrough";
    with_clean_config();
    {
        char *argv[] = { (char *)"printf", (char *)"%s", (char *)"x", 0 };
        struct fake_exec_decision d;
        classify("/usr/bin/printf", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "%-conversion forces passthrough");
    }
    {
        char *argv[] = { (char *)"printf", (char *)"hi\\n", 0 };
        struct fake_exec_decision d;
        classify("/usr/bin/printf", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "backslash escape forces passthrough");
    }
}

/* ---- Step C: /bin/sh -c "<trivial cmd>" ------------------------- */

static void test_sh_tokenise_basic(void)
{
    g_curtest = "sh_tokenise_basic";
    char tok[64];
    char *argv[8];
    int  argc = 0;
    int  ok;

    ok = sud_fake_exec_sh_tokenise("true", tok, sizeof(tok),
                                   argv, 8, &argc);
    TASSERT_EQ(ok, 1, "single token");
    TASSERT_EQ(argc, 1, "argc=1");
    TASSERT(strcmp(argv[0], "true") == 0, "argv[0]=true");

    ok = sud_fake_exec_sh_tokenise("/usr/bin/echo done",
                                   tok, sizeof(tok), argv, 8, &argc);
    TASSERT_EQ(ok, 1, "two tokens");
    TASSERT_EQ(argc, 2, "argc=2");
    TASSERT(strcmp(argv[0], "/usr/bin/echo") == 0, "argv[0]=path");
    TASSERT(strcmp(argv[1], "done") == 0, "argv[1]=done");
}

static void test_sh_tokenise_rejects_metachars(void)
{
    g_curtest = "sh_tokenise_rejects_metachars";
    char tok[64];
    char *argv[8];
    int  argc = 0;
    const char *cases[] = {
        "true | false",        /* pipe */
        "true; false",         /* sequence */
        "true && false",       /* and */
        "true > /dev/null",    /* redirect */
        "echo $HOME",          /* expansion */
        "echo `id`",           /* command substitution */
        "echo $(id)",          /* command substitution */
        "echo *",              /* glob */
        "echo a  b",           /* double space */
        " true",               /* leading space */
        "true ",               /* trailing space */
        "echo \"hi\"",         /* quotes */
        "echo 'hi'",           /* quotes */
        "",                    /* empty */
        0
    };
    for (int i = 0; cases[i]; i++) {
        int ok = sud_fake_exec_sh_tokenise(cases[i], tok, sizeof(tok),
                                           argv, 8, &argc);
        char buf[128];
        snprintf(buf, sizeof(buf), "rejects: %s", cases[i]);
        TASSERT_EQ(ok, 0, buf);
    }
}

static void test_classify_sh_dash_c_true(void)
{
    g_curtest = "classify_sh_dash_c_true";
    with_clean_config();
    char *argv[] = { (char *)"sh", (char *)"-c", (char *)"true", 0 };
    struct fake_exec_decision d;
    classify("/bin/sh", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE,
               "sh -c true elidable");
    TASSERT_EQ(d.exit_status, 0, "exit 0");
    TASSERT_EQ(d.out_fd, -1, "no write");
}

static void test_classify_sh_dash_c_false(void)
{
    g_curtest = "classify_sh_dash_c_false";
    with_clean_config();
    char *argv[] = { (char *)"sh", (char *)"-c",
                     (char *)"/usr/bin/false", 0 };
    struct fake_exec_decision d;
    classify("/bin/sh", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE,
               "sh -c /usr/bin/false elidable");
    TASSERT_EQ(d.exit_status, 1, "exit 1");
}

static void test_classify_sh_dash_c_echo(void)
{
    g_curtest = "classify_sh_dash_c_echo";
    with_clean_config();
    char *argv[] = { (char *)"sh", (char *)"-c",
                     (char *)"/usr/bin/echo done", 0 };
    struct fake_exec_decision d;
    classify("/bin/sh", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE,
               "sh -c echo elidable");
    TASSERT_EQ(d.out_fd, 1, "writes fd 1");
    TASSERT_EQ(d.out_len, 5, "len(\"done\\n\")");
    TASSERT(d.out_buf && memcmp(d.out_buf, "done\n", 5) == 0,
            "echo bytes from sh -c");
}

static void test_classify_sh_dash_c_unknown_passthrough(void)
{
    g_curtest = "classify_sh_dash_c_unknown_passthrough";
    with_clean_config();
    {
        char *argv[] = { (char *)"sh", (char *)"-c",
                         (char *)"/usr/bin/grep foo", 0 };
        struct fake_exec_decision d;
        classify("/bin/sh", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "unknown inner cmd → passthrough");
    }
    {
        /* Trailing positional parameter rejects (not the "single
         * trivial command" shape). */
        char *argv[] = { (char *)"sh", (char *)"-c",
                         (char *)"true", (char *)"$0", 0 };
        struct fake_exec_decision d;
        classify("/bin/sh", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "extra positional arg → passthrough");
    }
    {
        /* Pipe in inner command rejects. */
        char *argv[] = { (char *)"sh", (char *)"-c",
                         (char *)"true | false", 0 };
        struct fake_exec_decision d;
        classify("/bin/sh", argv, 0, &d);
        TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
                   "metachars in inner cmd → passthrough");
    }
}

static void test_classify_sh_bash_alias(void)
{
    g_curtest = "classify_sh_bash_alias";
    with_clean_config();
    char *argv[] = { (char *)"bash", (char *)"-c", (char *)"true", 0 };
    struct fake_exec_decision d;
    classify("/bin/bash", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_INLINE_VFORK_SAFE,
               "bash -c true elidable too");
}

static void test_classify_sh_dash_c_respects_deny(void)
{
    g_curtest = "classify_sh_dash_c_respects_deny";
    with_clean_config();
    g_sud_runtime_config.fake_exec_deny[0] = "true";
    g_sud_runtime_config.fake_exec_deny_count = 1;
    char *argv[] = { (char *)"sh", (char *)"-c", (char *)"true", 0 };
    struct fake_exec_decision d;
    classify("/bin/sh", argv, 0, &d);
    TASSERT_EQ(d.track, FAKE_EXEC_PASSTHROUGH,
               "deny list applies to sh -c inner cmd too");
}

/* ---- Runtime-config wiring -------------------------------------- */

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

    test_classify_echo_simple();
    test_classify_echo_no_args();
    test_classify_echo_dash_flag_passthrough();
    test_classify_printf_literal();
    test_classify_printf_percent_passthrough();

    test_sh_tokenise_basic();
    test_sh_tokenise_rejects_metachars();
    test_classify_sh_dash_c_true();
    test_classify_sh_dash_c_false();
    test_classify_sh_dash_c_echo();
    test_classify_sh_dash_c_unknown_passthrough();
    test_classify_sh_bash_alias();
    test_classify_sh_dash_c_respects_deny();

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
