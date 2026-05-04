/*
 * sud/cmd-rewrite/tests/test_cmd_rewrite.c — Unit tests for the
 * cmd-rewrite addin's rule parser, matchers, and suppression list.
 *
 * Built freestanding (no libc) for 64-bit, mirroring the path_remap
 * and fake-exec test harnesses.  We exercise pure functions only —
 * the actual execve-rewriting addin path runs from inside the SIGSYS
 * handler and is covered end-to-end by sudtrace_test.sh.
 */

#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"
#include "sud/cmd-rewrite/rules.h"

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

#define TASSERT_STREQ(a, b, descr) do { \
    const char *_a = (a) ? (a) : "(null)"; \
    const char *_b = (b) ? (b) : "(null)"; \
    if (strcmp(_a, _b) != 0) { \
        char _buf[512]; \
        snprintf(_buf, sizeof(_buf), \
                 "FAIL [%s] %s @ line %d: got '%s', want '%s'\n", \
                 g_curtest, (descr), __LINE__, _a, _b); \
        test_log(_buf); \
        g_failures++; \
    } \
} while (0)

/* ---- Test helpers ------------------------------------------------ */

static char g_rule_buf[16][SUD_CMD_RULE_STR_MAX];
static int  g_rule_buf_used;

/* Install a runtime config with the given rule strings, then re-init
 * the rules table.  Each rule string is copied into our static arena
 * because parse_rule mutates the strings to NUL-terminate the
 * pattern fields. */
static void install_rules(const char *const *rule_strs, int n,
                          const char *const *suppress_strs, int sn)
{
    /* Reset rules-table init flag so a second install reparses. */
    extern void sud_cmd_rules_reset_for_test(void);
    sud_cmd_rules_reset_for_test();

    g_rule_buf_used = 0;
    sud_runtime_config_clear(&g_sud_runtime_config);
    g_sud_runtime_config_present = 1;

    for (int i = 0; i < n && g_rule_buf_used < 16; i++) {
        int j = 0;
        const char *s = rule_strs[i];
        while (s[j] && j + 1 < SUD_CMD_RULE_STR_MAX) {
            g_rule_buf[g_rule_buf_used][j] = s[j];
            j++;
        }
        g_rule_buf[g_rule_buf_used][j] = '\0';
        g_sud_runtime_config.cmd_rules[i] = g_rule_buf[g_rule_buf_used];
        g_rule_buf_used++;
    }
    g_sud_runtime_config.cmd_rule_count = n;

    for (int i = 0; i < sn && i < SUD_RC_MAX_SUPPRESS; i++)
        g_sud_runtime_config.suppressed[i] = suppress_strs[i];
    g_sud_runtime_config.suppress_count = sn;

    sud_cmd_rules_init();
}

/* ---- Pattern matchers ------------------------------------------- */

static void test_match_basename(void)
{
    g_curtest = "match_basename";
    TASSERT(sud_cmd_match_basename("/usr/bin/gcc", "gcc"), "abs path basename");
    TASSERT(sud_cmd_match_basename("gcc", "gcc"),         "bare basename");
    TASSERT(!sud_cmd_match_basename("/usr/bin/gcc-12", "gcc"), "version suffix");
    TASSERT(!sud_cmd_match_basename("/usr/bin/cc", "gcc"),     "different name");
    TASSERT(!sud_cmd_match_basename("", "gcc"),                "empty path");
}

static void test_match_path(void)
{
    g_curtest = "match_path";
    TASSERT(sud_cmd_match_path("/usr/bin/gcc", "/usr/bin/gcc"), "exact match");
    TASSERT(!sud_cmd_match_path("/usr/local/bin/gcc", "/usr/bin/gcc"),
            "different dir");
    TASSERT(!sud_cmd_match_path("/usr/bin/gcc", "gcc"), "non-abs want");
}

static void test_match_glob(void)
{
    g_curtest = "match_glob";
    TASSERT(sud_cmd_match_glob("/usr/bin/gcc",      "gcc"),   "exact glob");
    TASSERT(sud_cmd_match_glob("/usr/bin/gcc-12",   "gcc-*"), "trailing *");
    TASSERT(sud_cmd_match_glob("/usr/bin/x86_64-linux-gnu-gcc", "*-gcc"),
            "leading * + suffix");
    TASSERT(sud_cmd_match_glob("/opt/v/arm-foo-gcc-9.4",
                               "*-gcc-[0-9]*"),
            "char class");
    TASSERT(!sud_cmd_match_glob("/opt/v/arm-foo-gcc",
                                "*-gcc-[0-9]*"),
            "char class needs match");
    TASSERT(sud_cmd_match_glob("/usr/bin/g++",  "g??"),       "?");
    TASSERT(!sud_cmd_match_glob("/usr/bin/cc",  "gcc*"),       "no-match prefix");
    TASSERT(sud_cmd_match_glob("/x/foo.bar",    "foo.*"),      "literal dot");
    TASSERT(sud_cmd_match_glob("/x/abc",        "[abc][abc][abc]"),
            "multi class");
    TASSERT(!sud_cmd_match_glob("/x/abc",       "[!abc]*"),    "negated class");
    TASSERT(sud_cmd_match_glob("/x/dabc",       "[!abc]*"),    "neg-class hit");
}

/* ---- Rule parser ------------------------------------------------- */

static void test_parse_compiler_wrap(void)
{
    g_curtest = "parse_compiler_wrap";
    const char *rules[] = {
        "compiler-wrap:basename:gcc:/usr/bin/ccache",
    };
    install_rules(rules, 1, 0, 0);
    int n;
    const struct sud_cmd_rule *t = sud_cmd_rules_table(&n);
    TASSERT_EQ(n, 1, "one rule");
    TASSERT_EQ(t[0].kind, SUD_CMD_KIND_COMPILER_WRAP, "kind");
    TASSERT_EQ(t[0].match, SUD_CMD_MATCH_BASENAME, "match");
    TASSERT_STREQ(t[0].pattern, "gcc", "pattern");
    TASSERT_STREQ(t[0].tool, "/usr/bin/ccache", "tool");
    TASSERT_STREQ(t[0].name, "compiler-wrap:basename:gcc", "implicit name");
}

static void test_parse_exec_strip(void)
{
    g_curtest = "parse_exec_strip";
    const char *rules[] = {
        "exec-strip:basename:sudo",
        "exec-strip:basename:env",
        "exec-strip:basename:foo:abc:xy:1",
    };
    install_rules(rules, 3, 0, 0);
    int n;
    const struct sud_cmd_rule *t = sud_cmd_rules_table(&n);
    TASSERT_EQ(n, 3, "three rules");
    TASSERT_EQ(t[0].kind, SUD_CMD_KIND_EXEC_STRIP, "rule0 kind");
    TASSERT_EQ(t[0].strip_default, 1, "rule0 uses default");
    /* sudo's default singletons begins with A. */
    TASSERT(t[0].strip.singletons[0] == 'A', "sudo singletons");

    TASSERT_EQ(t[1].strip_default, 1, "rule1 uses default");
    TASSERT(t[1].strip.singletons[0] == 'i', "env singletons");

    TASSERT_EQ(t[2].strip_default, 0, "rule2 custom");
    TASSERT_STREQ(t[2].strip.singletons, "abc", "custom singletons");
    TASSERT_STREQ(t[2].strip.arg_takers, "xy",  "custom arg-takers");
    TASSERT_EQ(t[2].strip.accept_ddash, 1, "custom accept ddash");
}

static void test_parse_exec_as(void)
{
    g_curtest = "parse_exec_as";
    const char *rules[] = {
        "exec-as:basename:make:0",
        "exec-as:basename:install:0:0",
        "exec-as:glob:*-make:1000:1000",
    };
    install_rules(rules, 3, 0, 0);
    int n;
    const struct sud_cmd_rule *t = sud_cmd_rules_table(&n);
    TASSERT_EQ(n, 3, "three rules");
    TASSERT_EQ(t[0].as_uid, 0, "uid 0");
    TASSERT_EQ(t[0].as_gid, -1, "gid unset");
    TASSERT_EQ(t[1].as_uid, 0, "uid 0 (full)");
    TASSERT_EQ(t[1].as_gid, 0, "gid 0");
    TASSERT_EQ(t[2].as_uid, 1000, "uid 1000");
    TASSERT_EQ(t[2].as_gid, 1000, "gid 1000");
    TASSERT_EQ(t[2].match, SUD_CMD_MATCH_GLOB, "glob match");
    TASSERT_STREQ(t[2].pattern, "*-make", "glob pattern");
}

static void test_parse_malformed(void)
{
    g_curtest = "parse_malformed";
    const char *rules[] = {
        "garbage",
        "compiler-wrap:basename:gcc",                  /* missing tool */
        "compiler-wrap:zzz:gcc:/usr/bin/ccache",       /* bad match-kind */
        "exec-as:basename:make",                       /* missing uid */
        "exec-as:basename:make:abc",                   /* non-numeric uid */
        /* And one valid rule mixed in: parser must skip the bad ones
         * but accept this. */
        "compiler-wrap:basename:gcc:/usr/bin/ccache",
    };
    install_rules(rules, 6, 0, 0);
    int n;
    const struct sud_cmd_rule *t = sud_cmd_rules_table(&n);
    TASSERT_EQ(n, 1, "one valid rule from six");
    TASSERT_STREQ(t[0].pattern, "gcc", "kept the good one");
}

/* ---- Suppression ------------------------------------------------- */

static void test_suppression_user_seeded(void)
{
    g_curtest = "suppression_user_seeded";
    const char *rules[] = {
        "compiler-wrap:basename:gcc:/usr/bin/ccache",
    };
    const char *supps[] = { "compiler-wrap:basename:gcc" };
    install_rules(rules, 1, supps, 1);
    TASSERT(sud_cmd_rule_is_suppressed("compiler-wrap:basename:gcc"),
            "user-seeded suppression hit");
    TASSERT(!sud_cmd_rule_is_suppressed("compiler-wrap:basename:cc"),
            "different name not suppressed");
}

static void test_suppression_auto_appended(void)
{
    g_curtest = "suppression_auto_appended";
    const char *rules[] = {
        "compiler-wrap:basename:gcc:/usr/bin/ccache",
    };
    install_rules(rules, 1, 0, 0);
    TASSERT(!sud_cmd_rule_is_suppressed("compiler-wrap:basename:gcc"),
            "initially not suppressed");
    sud_cmd_rule_add_suppression("compiler-wrap:basename:gcc");
    TASSERT(sud_cmd_rule_is_suppressed("compiler-wrap:basename:gcc"),
            "now suppressed");
    /* Idempotent — second add doesn't grow the list. */
    int prev = g_sud_runtime_config.suppress_count;
    sud_cmd_rule_add_suppression("compiler-wrap:basename:gcc");
    TASSERT_EQ(g_sud_runtime_config.suppress_count, prev,
               "second add idempotent");
}

/* ---- Default flag-skip specs ------------------------------------ */

static void test_default_strip_specs(void)
{
    g_curtest = "default_strip_specs";
    const struct sud_cmd_strip_spec *s;

    s = sud_cmd_strip_default_for("sudo");
    TASSERT(s != 0, "sudo spec exists");
    TASSERT(s->accept_ddash, "sudo accepts --");

    s = sud_cmd_strip_default_for("fakeroot-ng");
    TASSERT(s != 0, "fakeroot-ng spec exists");

    s = sud_cmd_strip_default_for("env");
    TASSERT(s != 0, "env spec exists");

    s = sud_cmd_strip_default_for("nonexistent");
    TASSERT(s == 0, "unknown returns NULL");
}

/* ---- runtime_config integration ---------------------------------- */

static void test_runtime_config_emit_cmd_rules(void)
{
    g_curtest = "runtime_config_emit_cmd_rules";
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.cmd_rules[0]   = "compiler-wrap:basename:gcc:/usr/bin/ccache";
    cfg.cmd_rules[1]   = "exec-strip:basename:sudo";
    cfg.cmd_rule_count = 2;
    cfg.suppressed[0]  = "compiler-wrap:basename:cc1";
    cfg.suppress_count = 1;
    cfg.pretend_uid    = 0;
    cfg.pretend_gid    = 1000;

    const char *out[32];
    char scratch[128];
    int n = sud_runtime_config_emit(&cfg, out, 32, scratch, sizeof(scratch));
    TASSERT(n >= 8, "emitted enough args");

    int saw_cr_gcc = 0, saw_cr_sudo = 0, saw_supp = 0;
    int saw_pu = 0, saw_pg = 0;
    for (int i = 0; i + 1 < n; i++) {
        if (strcmp(out[i], "--cmd-rule") == 0 &&
            strcmp(out[i+1], "compiler-wrap:basename:gcc:/usr/bin/ccache") == 0)
            saw_cr_gcc = 1;
        if (strcmp(out[i], "--cmd-rule") == 0 &&
            strcmp(out[i+1], "exec-strip:basename:sudo") == 0)
            saw_cr_sudo = 1;
        if (strcmp(out[i], "--suppress-rule") == 0 &&
            strcmp(out[i+1], "compiler-wrap:basename:cc1") == 0)
            saw_supp = 1;
        if (strcmp(out[i], "--pretend-uid") == 0 &&
            strcmp(out[i+1], "0") == 0)
            saw_pu = 1;
        if (strcmp(out[i], "--pretend-gid") == 0 &&
            strcmp(out[i+1], "1000") == 0)
            saw_pg = 1;
    }
    TASSERT(saw_cr_gcc, "compiler-wrap rule emitted");
    TASSERT(saw_cr_sudo, "exec-strip rule emitted");
    TASSERT(saw_supp, "suppression emitted");
    TASSERT(saw_pu, "pretend-uid 0 emitted");
    TASSERT(saw_pg, "pretend-gid 1000 emitted");
}

static void test_runtime_config_parse_cmd_rules(void)
{
    g_curtest = "runtime_config_parse_cmd_rules";
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    char *argv[] = {
        (char *)"sud64",
        (char *)"--cmd-rule", (char *)"compiler-wrap:basename:gcc:/usr/bin/ccache",
        (char *)"--suppress-rule", (char *)"compiler-wrap:basename:cc1",
        (char *)"--pretend-uid", (char *)"0",
        (char *)"--pretend-gid", (char *)"1000",
        (char *)"/usr/bin/gcc", (char *)"-c", (char *)"foo.c",
        0
    };
    int argi = 1;
    int rc = sud_runtime_config_parse(12, argv, &argi, &cfg);
    TASSERT_EQ(rc, 0, "parse rc");
    TASSERT_EQ(cfg.cmd_rule_count, 1, "one cmd rule");
    TASSERT_STREQ(cfg.cmd_rules[0],
                  "compiler-wrap:basename:gcc:/usr/bin/ccache", "rule");
    TASSERT_EQ(cfg.suppress_count, 1, "one suppression");
    TASSERT_STREQ(cfg.suppressed[0],
                  "compiler-wrap:basename:cc1", "supp");
    TASSERT_EQ(cfg.pretend_uid, 0, "pretend uid");
    TASSERT_EQ(cfg.pretend_gid, 1000, "pretend gid");
    TASSERT_EQ(argi, 9, "stops at /usr/bin/gcc");
}

/* ---- Entrypoint --------------------------------------------------- */

int main(int argc, char **argv)
{
    (void)argc; (void)argv;

    test_match_basename();
    test_match_path();
    test_match_glob();
    test_parse_compiler_wrap();
    test_parse_exec_strip();
    test_parse_exec_as();
    test_parse_malformed();
    test_suppression_user_seeded();
    test_suppression_auto_appended();
    test_default_strip_specs();
    test_runtime_config_emit_cmd_rules();
    test_runtime_config_parse_cmd_rules();

    if (g_failures == 0) {
        const char *ok = "cmd-rewrite tests OK\n";
        write(1, ok, strlen(ok));
        return 0;
    }
    char buf[64];
    snprintf(buf, sizeof(buf), "cmd-rewrite tests FAILED (%d)\n", g_failures);
    test_log(buf);
    return 1;
}
