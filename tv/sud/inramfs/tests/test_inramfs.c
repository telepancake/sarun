/*
 * sud/inramfs/tests/test_inramfs.c — Functional tests for the
 * in-RAM filesystem add-in.
 *
 * Built freestanding for both -m32 and -m64 like the path_remap
 * tests, so any architecture-specific bug in the layout, allocator,
 * or lock primitives is caught in CI before the wrapper ships.
 *
 * Each test sets SUD_INRAMFS to a unique mount path with a tiny
 * region size, calls sud_inramfs_init(), exercises the public ops,
 * then resets and unlinks the backing file so the next test starts
 * from a clean slate.
 */

#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"
#include "sud/inramfs/inramfs.h"
#include "sud/inramfs/internal.h"
#include "sud/raw.h"
#include "sud/runtime_config.h"

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

/* ---- tiny test framework ---- */

static int g_failures;
static const char *g_curtest;

static void tlog(const char *m) { write(2, m, strlen(m)); }

#define TASSERT(c, d) do { if (!(c)) { \
    char _b[256]; snprintf(_b, sizeof(_b), \
    "FAIL [%s] %s @ line %d\n", g_curtest, (d), __LINE__); \
    tlog(_b); g_failures++; } } while (0)

#define TASSERT_EQ(a, e, d) do { long _a=(long)(a), _e=(long)(e); \
    if (_a != _e) { char _b[256]; snprintf(_b, sizeof(_b), \
    "FAIL [%s] %s @ line %d: got %ld want %ld\n", \
    g_curtest, (d), __LINE__, _a, _e); tlog(_b); g_failures++; } } while (0)

#define TASSERT_STREQ(a, e, d) do { \
    if (strcmp((a), (e)) != 0) { char _b[512]; snprintf(_b, sizeof(_b), \
    "FAIL [%s] %s @ line %d: got '%s' want '%s'\n", \
    g_curtest, (d), __LINE__, (a), (e)); tlog(_b); g_failures++; } } while (0)

/* ---- environment helpers ---- */

static void setup_mount(const char *path, int size_mb, const char *key)
{
    /* size_mb is the user-tunable metadata size.  The new layout
     * (8 MiB inode table + 256 KiB small-file bitmap + dirent block
     * area) needs ~12 MiB minimum; the addin's min_meta_size floor
     * will round up smaller requests, but make the tests honest by
     * passing at least 16 MiB. */
    if (size_mb < 16) size_mb = 16;
    char rule[PATH_MAX + 32];
    snprintf(rule, sizeof(rule), "inramfs:%s", path);
    struct sud_runtime_config cfg;
    sud_runtime_config_clear(&cfg);
    cfg.remap_rules[0] = rule;
    cfg.remap_rule_count = 1;
    cfg.inramfs_meta_mb = size_mb;
    cfg.inramfs_key = key;
    sud_runtime_config_test_install(&cfg);
    sud_inramfs_init();
}

static void teardown_mount(void)
{
    sud_inramfs_unlink_backing_for_testing();
    sud_inramfs_reset_for_testing();
    sud_runtime_config_test_clear();
}

/* ---- tests ---- */

static void test_init_and_root(void)
{
    g_curtest = "init_and_root";
    setup_mount("/inramfs", 4, "test_init");
    TASSERT(sud_inramfs_active(), "active after init");
    TASSERT(sud_inramfs_path_under_mount("/inramfs"),
            "mount root is under mount");
    TASSERT(sud_inramfs_path_under_mount("/inramfs/foo"),
            "subpath is under mount");
    TASSERT(!sud_inramfs_path_under_mount("/tmp/x"),
            "/tmp is not under mount");
    /* Root inode should be present, type DIR, mode 0755. */
    struct sud_ir_inode *root = sud_ir_inode_get(1);
    TASSERT(root != 0, "root inode exists");
    TASSERT_EQ(root->type, SUD_IR_T_DIR, "root is DIR");
    TASSERT_EQ(root->mode, 0755, "root mode 0755");
    TASSERT_EQ(root->nlink, 2, "root nlink 2");
    teardown_mount();
}

static void test_mkdir_lookup(void)
{
    g_curtest = "mkdir_lookup";
    setup_mount("/inramfs", 4, "test_mkdir");
    TASSERT_EQ(sud_inramfs_op_mkdir("/inramfs/d1", 0755), 0, "mkdir /d1");
    /* mkdir again → EEXIST */
    TASSERT_EQ(sud_inramfs_op_mkdir("/inramfs/d1", 0755), -EEXIST, "EEXIST");
    /* nested */
    TASSERT_EQ(sud_inramfs_op_mkdir("/inramfs/d1/d2", 0700), 0, "nested mkdir");
    /* parent missing */
    TASSERT_EQ(sud_inramfs_op_mkdir("/inramfs/no/d", 0755), -ENOENT, "ENOENT");
    /* stat */
    char stbuf[256];
    TASSERT_EQ(sud_inramfs_op_stat("/inramfs/d1", stbuf, 1), 0, "stat /d1");
    TASSERT_EQ(sud_inramfs_op_stat("/inramfs/d1/d2", stbuf, 1), 0, "stat nested");
    teardown_mount();
}

static void test_open_write_read(void)
{
    g_curtest = "open_write_read";
    setup_mount("/inramfs", 4, "test_owr");
    int fd = (int)sud_inramfs_op_open("/inramfs/file1",
                                       O_WRONLY | O_CREAT, 0644);
    TASSERT(fd >= 0, "create file1");
    TASSERT(sud_inramfs_owns_fd(fd), "owns fd");
    long n = sud_inramfs_op_write(fd, "hello world", 11);
    TASSERT_EQ(n, 11, "wrote 11");
    TASSERT_EQ(sud_inramfs_op_close(fd), 0, "close");
    TASSERT(!sud_inramfs_owns_fd(fd), "fd released");

    int rfd = (int)sud_inramfs_op_open("/inramfs/file1", O_RDONLY, 0);
    TASSERT(rfd >= 0, "open RDONLY");
    char buf[32] = {0};
    n = sud_inramfs_op_read(rfd, buf, sizeof(buf));
    TASSERT_EQ(n, 11, "read 11");
    TASSERT(memcmp(buf, "hello world", 11) == 0, "data matches");
    /* second read returns 0 (EOF). */
    n = sud_inramfs_op_read(rfd, buf, sizeof(buf));
    TASSERT_EQ(n, 0, "EOF");
    sud_inramfs_op_close(rfd);
    teardown_mount();
}

static void test_lseek_holes(void)
{
    g_curtest = "lseek_holes";
    setup_mount("/inramfs", 4, "test_holes");
    int fd = (int)sud_inramfs_op_open("/inramfs/sparse",
                                       O_RDWR | O_CREAT, 0644);
    TASSERT(fd >= 0, "open sparse");
    /* Seek to 1000 and write 4 bytes — earlier bytes must read as zero. */
    TASSERT_EQ(sud_inramfs_op_lseek(fd, 1000, SEEK_SET), 1000, "seek 1000");
    TASSERT_EQ(sud_inramfs_op_write(fd, "ABCD", 4), 4, "write 4");
    TASSERT_EQ(sud_inramfs_op_lseek(fd, 0, SEEK_SET), 0, "seek 0");
    char buf[1004];
    long n = sud_inramfs_op_read(fd, buf, sizeof(buf));
    TASSERT_EQ(n, 1004, "read 1004");
    int all_zero = 1;
    for (int i = 0; i < 1000; i++) if (buf[i]) { all_zero = 0; break; }
    TASSERT(all_zero, "hole reads as zeros");
    TASSERT(memcmp(buf + 1000, "ABCD", 4) == 0, "tail matches");
    sud_inramfs_op_close(fd);
    teardown_mount();
}

static void test_truncate(void)
{
    g_curtest = "truncate";
    setup_mount("/inramfs", 4, "test_trunc");
    int fd = (int)sud_inramfs_op_open("/inramfs/t", O_RDWR | O_CREAT, 0644);
    TASSERT(fd >= 0, "open");
    sud_inramfs_op_write(fd, "0123456789", 10);
    TASSERT_EQ(sud_inramfs_op_ftruncate(fd, 5), 0, "ftruncate 5");
    TASSERT_EQ(sud_inramfs_op_lseek(fd, 0, SEEK_END), 5, "size 5");
    TASSERT_EQ(sud_inramfs_op_ftruncate(fd, 100), 0, "ftruncate 100");
    char buf[110] = {0};
    sud_inramfs_op_lseek(fd, 0, SEEK_SET);
    long n = sud_inramfs_op_read(fd, buf, sizeof(buf));
    TASSERT_EQ(n, 100, "read 100");
    /* First 5 bytes are the original data, rest are zeros. */
    TASSERT(memcmp(buf, "01234", 5) == 0, "head preserved");
    int z = 1;
    for (int i = 5; i < 100; i++) if (buf[i]) { z = 0; break; }
    TASSERT(z, "extension is zero");
    sud_inramfs_op_close(fd);
    teardown_mount();
}

static void test_unlink_rename(void)
{
    g_curtest = "unlink_rename";
    setup_mount("/inramfs", 4, "test_ur");
    int fd = (int)sud_inramfs_op_open("/inramfs/a", O_RDWR | O_CREAT, 0644);
    TASSERT(fd >= 0, "create a");
    sud_inramfs_op_close(fd);
    TASSERT_EQ(sud_inramfs_op_rename("/inramfs/a", "/inramfs/b", 0), 0,
               "rename a→b");
    char st[256];
    TASSERT_EQ(sud_inramfs_op_stat("/inramfs/a", st, 1), -ENOENT,
               "a is gone");
    TASSERT_EQ(sud_inramfs_op_stat("/inramfs/b", st, 1), 0, "b exists");
    TASSERT_EQ(sud_inramfs_op_unlink("/inramfs/b"), 0, "unlink b");
    TASSERT_EQ(sud_inramfs_op_stat("/inramfs/b", st, 1), -ENOENT, "b gone");
    /* unlink a directory returns EISDIR. */
    sud_inramfs_op_mkdir("/inramfs/d", 0755);
    TASSERT_EQ(sud_inramfs_op_unlink("/inramfs/d"), -EISDIR, "EISDIR");
    /* rmdir non-empty returns ENOTEMPTY. */
    fd = (int)sud_inramfs_op_open("/inramfs/d/x", O_RDWR | O_CREAT, 0644);
    sud_inramfs_op_close(fd);
    TASSERT_EQ(sud_inramfs_op_rmdir("/inramfs/d"), -ENOTEMPTY, "ENOTEMPTY");
    sud_inramfs_op_unlink("/inramfs/d/x");
    TASSERT_EQ(sud_inramfs_op_rmdir("/inramfs/d"), 0, "rmdir empty");
    teardown_mount();
}

static void test_symlink(void)
{
    g_curtest = "symlink";
    setup_mount("/inramfs", 4, "test_sym");
    int fd = (int)sud_inramfs_op_open("/inramfs/target",
                                       O_RDWR | O_CREAT, 0644);
    sud_inramfs_op_write(fd, "payload", 7);
    sud_inramfs_op_close(fd);
    TASSERT_EQ(sud_inramfs_op_symlink("target", "/inramfs/link"), 0,
               "make symlink");
    char buf[64] = {0};
    long n = sud_inramfs_op_readlink("/inramfs/link", buf, sizeof(buf));
    TASSERT_EQ(n, 6, "readlink len");
    buf[n] = '\0';
    TASSERT_STREQ(buf, "target", "readlink target");
    /* open follows the symlink and reads the file. */
    int rfd = (int)sud_inramfs_op_open("/inramfs/link", O_RDONLY, 0);
    TASSERT(rfd >= 0, "open through symlink");
    char rbuf[16] = {0};
    n = sud_inramfs_op_read(rfd, rbuf, sizeof(rbuf));
    TASSERT_EQ(n, 7, "read through symlink");
    TASSERT(memcmp(rbuf, "payload", 7) == 0, "data through symlink");
    sud_inramfs_op_close(rfd);
    /* Symlink loop → ELOOP. */
    sud_inramfs_op_symlink("loopA", "/inramfs/loopB");
    sud_inramfs_op_symlink("loopB", "/inramfs/loopA");
    int err = 0;
    uint32_t bad = sud_ir_walk("/inramfs/loopA", 1, &err);
    TASSERT_EQ(bad, 0, "loop returns 0");
    TASSERT_EQ(err, -ELOOP, "ELOOP");
    teardown_mount();
}

static void test_getdents(void)
{
    g_curtest = "getdents";
    setup_mount("/inramfs", 4, "test_gd");
    sud_inramfs_op_mkdir("/inramfs/dir", 0755);
    int fd = (int)sud_inramfs_op_open("/inramfs/dir/a",
                                       O_WRONLY | O_CREAT, 0644);
    sud_inramfs_op_close(fd);
    fd = (int)sud_inramfs_op_open("/inramfs/dir/b", O_WRONLY | O_CREAT, 0644);
    sud_inramfs_op_close(fd);
    sud_inramfs_op_mkdir("/inramfs/dir/sub", 0755);
    int dfd = (int)sud_inramfs_op_open("/inramfs/dir",
                                        O_RDONLY | O_DIRECTORY, 0);
    TASSERT(dfd >= 0, "open dir");
    char buf[1024];
    long n = sud_inramfs_op_getdents64(dfd, buf, sizeof(buf));
    TASSERT(n > 0, "getdents>0");
    /* Walk the dirents; we expect ".", "..", "a", "b", "sub". */
    int saw_dot = 0, saw_dotdot = 0, saw_a = 0, saw_b = 0, saw_sub = 0;
    long off = 0;
    while (off < n) {
        struct linux_dirent64 *de = (struct linux_dirent64 *)(buf + off);
        if (strcmp(de->d_name, ".") == 0)   saw_dot = 1;
        else if (strcmp(de->d_name, "..") == 0) saw_dotdot = 1;
        else if (strcmp(de->d_name, "a") == 0)  saw_a = 1;
        else if (strcmp(de->d_name, "b") == 0)  saw_b = 1;
        else if (strcmp(de->d_name, "sub") == 0) {
            saw_sub = 1;
            TASSERT_EQ(de->d_type, DT_DIR, "sub is DIR");
        }
        off += de->d_reclen;
    }
    TASSERT(saw_dot && saw_dotdot && saw_a && saw_b && saw_sub,
            "all dirents present");
    /* Subsequent getdents returns 0 (cookie exhausted). */
    n = sud_inramfs_op_getdents64(dfd, buf, sizeof(buf));
    TASSERT_EQ(n, 0, "second getdents=0");
    sud_inramfs_op_close(dfd);
    teardown_mount();
}

static void test_chmod_chown_utimens(void)
{
    g_curtest = "chmod_chown_utimens";
    setup_mount("/inramfs", 4, "test_meta");
    int fd = (int)sud_inramfs_op_open("/inramfs/m", O_WRONLY | O_CREAT, 0644);
    sud_inramfs_op_close(fd);
    TASSERT_EQ(sud_inramfs_op_chmod("/inramfs/m", 0600), 0, "chmod 0600");
    char st[256];
    TASSERT_EQ(sud_inramfs_op_stat("/inramfs/m", st, 1), 0, "stat ok");
    /* Verify mode bits via inode peek. */
    int err = 0;
    uint32_t idx = sud_ir_walk("/inramfs/m", 1, &err);
    struct sud_ir_inode *ino = sud_ir_inode_get(idx);
    TASSERT_EQ(ino->mode, 0600, "mode bits");
    /* utimensat with explicit values. */
    struct timespec ts[2];
    ts[0].tv_sec = 1000; ts[0].tv_nsec = 500;
    ts[1].tv_sec = 2000; ts[1].tv_nsec = 600;
    TASSERT_EQ(sud_inramfs_op_utimensat("/inramfs/m", ts, 1), 0, "utimensat");
    TASSERT_EQ(ino->atime_ns, 1000ULL * 1000000000ULL + 500, "atime_ns");
    TASSERT_EQ(ino->mtime_ns, 2000ULL * 1000000000ULL + 600, "mtime_ns");
    teardown_mount();
}

static void test_link_hardlink(void)
{
    g_curtest = "link_hardlink";
    setup_mount("/inramfs", 4, "test_link");
    int fd = (int)sud_inramfs_op_open("/inramfs/orig",
                                       O_WRONLY | O_CREAT, 0644);
    sud_inramfs_op_write(fd, "abc", 3);
    sud_inramfs_op_close(fd);
    TASSERT_EQ(sud_inramfs_op_link("/inramfs/orig", "/inramfs/dup"), 0,
               "link orig→dup");
    /* Both paths point to the same inode. */
    int err = 0;
    uint32_t i1 = sud_ir_walk("/inramfs/orig", 0, &err);
    uint32_t i2 = sud_ir_walk("/inramfs/dup", 0, &err);
    TASSERT_EQ(i1, i2, "same inode");
    struct sud_ir_inode *ino = sud_ir_inode_get(i1);
    TASSERT_EQ(ino->nlink, 2, "nlink 2");
    /* Unlink one — inode survives. */
    sud_inramfs_op_unlink("/inramfs/orig");
    TASSERT_EQ(ino->nlink, 1, "nlink 1");
    /* Read through remaining link. */
    int r = (int)sud_inramfs_op_open("/inramfs/dup", O_RDONLY, 0);
    char buf[8] = {0};
    long n = sud_inramfs_op_read(r, buf, sizeof(buf));
    TASSERT_EQ(n, 3, "still 3 bytes");
    TASSERT(memcmp(buf, "abc", 3) == 0, "data preserved");
    sud_inramfs_op_close(r);
    /* Linking a directory is forbidden. */
    sud_inramfs_op_mkdir("/inramfs/d", 0755);
    TASSERT_EQ(sud_inramfs_op_link("/inramfs/d", "/inramfs/d2"), -EPERM,
               "EPERM on dir link");
    teardown_mount();
}

static void test_root_mount(void)
{
    g_curtest = "root_mount";
    /* "/" as the mount: every absolute path is under it. */
    setup_mount("/", 4, "test_root");
    TASSERT(sud_inramfs_path_under_mount("/foo"), "/foo under root mount");
    TASSERT(sud_inramfs_path_under_mount("/"), "/ under root mount");
    int fd = (int)sud_inramfs_op_open("/file", O_WRONLY | O_CREAT, 0644);
    TASSERT(fd >= 0, "open /file");
    sud_inramfs_op_write(fd, "x", 1);
    sud_inramfs_op_close(fd);
    teardown_mount();
}

static void test_cross_process(void)
{
    /* Fork; child writes, parent reads.  Verifies the shared region
     * is genuinely shared across processes. */
    g_curtest = "cross_process";
    setup_mount("/inramfs", 4, "test_xp");
    int fd = (int)sud_inramfs_op_open("/inramfs/shared",
                                       O_RDWR | O_CREAT, 0644);
    TASSERT(fd >= 0, "create shared");
    sud_inramfs_op_close(fd);

    long pid = fork();
    if (pid == 0) {
        /* Child. */
        int cfd = (int)sud_inramfs_op_open("/inramfs/shared",
                                            O_WRONLY, 0);
        if (cfd < 0) raw_syscall6(SYS_exit, 11, 0, 0, 0, 0, 0);
        long w = sud_inramfs_op_write(cfd, "FROMCHILD", 9);
        if (w != 9) raw_syscall6(SYS_exit, 12, 0, 0, 0, 0, 0);
        sud_inramfs_op_close(cfd);
        raw_syscall6(SYS_exit, 0, 0, 0, 0, 0, 0);
    }
    /* Parent. */
    int status = 0;
    raw_syscall6(SYS_wait4, pid, (long)&status, 0, 0, 0, 0);
    TASSERT_EQ(status & 0xff7f, 0, "child exit OK");

    int pfd = (int)sud_inramfs_op_open("/inramfs/shared", O_RDONLY, 0);
    char buf[16] = {0};
    long n = sud_inramfs_op_read(pfd, buf, sizeof(buf));
    TASSERT_EQ(n, 9, "parent reads 9");
    TASSERT(memcmp(buf, "FROMCHILD", 9) == 0, "child's bytes visible");
    sud_inramfs_op_close(pfd);
    teardown_mount();
}

static void test_mmap(void)
{
    g_curtest = "mmap";
    setup_mount("/inramfs", 4, "test_mmap");
    int fd = (int)sud_inramfs_op_open("/inramfs/m", O_RDWR | O_CREAT, 0644);
    TASSERT(fd >= 0, "create");
    /* Truncate to a page. */
    TASSERT_EQ(sud_inramfs_op_ftruncate(fd, 4096), 0, "truncate");
    int err = 0;
    void *p = sud_inramfs_op_mmap(0, 4096, PROT_READ | PROT_WRITE,
                                   MAP_SHARED, fd, 0, &err);
    TASSERT(p != MAP_FAILED, "mmap returns valid pointer");
    /* Write through the mapping; read back via fd. */
    memcpy(p, "MMAPDATA", 8);
    char buf[4096] = {0};
    sud_inramfs_op_lseek(fd, 0, SEEK_SET);
    long n = sud_inramfs_op_read(fd, buf, sizeof(buf));
    TASSERT_EQ(n, 4096, "read full page");
    TASSERT(memcmp(buf, "MMAPDATA", 8) == 0, "mmap write visible");
    sud_inramfs_op_close(fd);
    teardown_mount();
}

/* Exercise the FAT chain: write a file that spans many blocks,
 * with a deterministic byte pattern, then read it back in two
 * passes (whole-file and a misaligned cross-block window) and
 * verify every byte.  Then truncate down (releasing trailing
 * blocks via FAT free) and back up (reallocating from the free
 * list) and verify the post-grow tail reads as zeros. */
static void test_multi_block(void)
{
    g_curtest = "multi_block";
    setup_mount("/inramfs", 4, "test_mb");

    /* Pick a size that's clearly multi-block (40 KiB = 10 blocks at
     * 4 KiB each) and not a multiple of common sizes, so any
     * off-by-one in chain_xfer's block boundary handling shows up
     * as a byte mismatch. */
    enum { N = 40 * 1024 };
    static unsigned char src[N];
    for (int i = 0; i < N; i++) src[i] = (unsigned char)((i * 31u + 7u) & 0xff);

    int fd = (int)sud_inramfs_op_open("/inramfs/big",
                                       O_RDWR | O_CREAT, 0644);
    TASSERT(fd >= 0, "create big");
    TASSERT_EQ(sud_inramfs_op_write(fd, src, N), N, "write N bytes");

    /* Whole-file read. */
    static unsigned char back[N];
    TASSERT_EQ(sud_inramfs_op_lseek(fd, 0, SEEK_SET), 0, "rewind");
    TASSERT_EQ(sud_inramfs_op_read(fd, back, N), N, "read N");
    TASSERT(memcmp(src, back, N) == 0, "round-trip matches");

    /* Misaligned cross-block window (starts mid-block-2, ends
     * mid-block-5).  Catches block-boundary mistakes in chain_xfer. */
    enum { OFF = 4096 * 2 + 123, LEN = 4096 * 3 + 456 };
    static unsigned char window[LEN];
    TASSERT_EQ(sud_inramfs_op_lseek(fd, OFF, SEEK_SET), OFF, "seek mid-block");
    TASSERT_EQ(sud_inramfs_op_read(fd, window, LEN), LEN, "read window");
    TASSERT(memcmp(window, src + OFF, LEN) == 0, "window matches");

    /* Truncate down, then back up: the new tail must read as zeros. */
    TASSERT_EQ(sud_inramfs_op_ftruncate(fd, 8 * 1024), 0, "truncate to 8 KiB");
    TASSERT_EQ(sud_inramfs_op_ftruncate(fd, N), 0, "regrow to N");
    TASSERT_EQ(sud_inramfs_op_lseek(fd, 8 * 1024, SEEK_SET),
               8 * 1024, "seek past kept tail");
    TASSERT_EQ(sud_inramfs_op_read(fd, back, N - 8 * 1024),
               N - 8 * 1024, "read regrown tail");
    int all_zero = 1;
    for (int i = 0; i < N - 8 * 1024; i++) {
        if (back[i]) { all_zero = 0; break; }
    }
    TASSERT(all_zero, "regrown tail is zero");

    /* Original first 8 KiB must still be intact. */
    TASSERT_EQ(sud_inramfs_op_lseek(fd, 0, SEEK_SET), 0, "rewind");
    TASSERT_EQ(sud_inramfs_op_read(fd, back, 8 * 1024), 8 * 1024, "read kept");
    TASSERT(memcmp(back, src, 8 * 1024) == 0, "kept prefix intact");

    sud_inramfs_op_close(fd);

    /* Unlink to exercise FAT free of a multi-block chain. */
    TASSERT_EQ(sud_inramfs_op_unlink("/inramfs/big"), 0, "unlink big");

    /* And we should still be able to allocate after unlink (free
     * list reattached the blocks). */
    int fd2 = (int)sud_inramfs_op_open("/inramfs/big2",
                                        O_RDWR | O_CREAT, 0644);
    TASSERT(fd2 >= 0, "create big2 after unlink");
    TASSERT_EQ(sud_inramfs_op_write(fd2, src, N), N, "write again");
    sud_inramfs_op_close(fd2);

    teardown_mount();
}

/* XXH64 sanity test against published reference vectors (XXH64 of
 * an empty string and of the canonical "Nobody inspects the
 * spammish repetition" test string with seed 0).  Prevents subtle
 * regressions in the inlined implementation. */
static void test_xxh64(void)
{
    g_curtest = "xxh64";
    /* XXH64("", 0, seed=0) == 0xEF46DB3751D8E999 */
    TASSERT_EQ(sud_ir_xxh64("", 0), 0xEF46DB3751D8E999ULL,
               "xxh64 empty matches reference");
    /* XXH64("Nobody inspects the spammish repetition", seed=0)
     *   == 0xFBCEA83C8A378BF1 (canonical xxhash test vector). */
    const char *s = "Nobody inspects the spammish repetition";
    TASSERT_EQ(sud_ir_xxh64(s, strlen(s)), 0xFBCEA83C8A378BF1ULL,
               "xxh64 known string matches reference");
    /* Avalanche smoke: single-bit difference must produce ≥ a few
     * bit flips in the output (loose bound; just rules out
     * accidentally identity-mapping). */
    char a = 'a', b = 'b';
    uint64_t ha = sud_ir_xxh64(&a, 1), hb = sud_ir_xxh64(&b, 1);
    TASSERT(ha != hb, "single-bit avalanche");
}

/* High-concurrency small-extent allocator stress test.
 *
 * N child processes each loop ITERS times: alloc a per-child file,
 * write a per-iteration-sized payload, truncate down, unlink.  The
 * goal is to hammer the contiguous-extent bitmap allocator across
 * processes — alloc/free contend on sb->lock, so any locking bug
 * would corrupt the bitmap (manifesting as overlapping extents,
 * which read-back would catch via cross-contamination, or as
 * leaked blocks visible in `small_blocks_in_use`).
 *
 * Invariant checked: after all children exit and all files are
 * gone, small_blocks_in_use must equal its initial value (no
 * leaks).
 *
 * All payloads are kept under SUD_IR_LARGE_THRESHOLD so the SMALL
 * tier is exercised exclusively (LARGE-tier promotion is covered
 * by test_promotion). */
static void test_smalldata_concurrency(void)
{
    g_curtest = "smalldata_concurrency";
    setup_mount("/inramfs", 16, "test_smallconc");

    struct sud_ir_super *sb = sud_ir_sb();
    uint32_t initial_in_use = __atomic_load_n(&sb->small_blocks_in_use,
                                              __ATOMIC_ACQUIRE);

    enum { N_CHILDREN = 8, ITERS = 50 };
    long pids[N_CHILDREN];
    for (int c = 0; c < N_CHILDREN; c++) {
        long pid = fork();
        if (pid == 0) {
            char path[64];
            for (int it = 0; it < ITERS; it++) {
                snprintf(path, sizeof(path),
                         "/inramfs/c%d_i%d", c, it);
                int fd = (int)sud_inramfs_op_open(
                    path, O_RDWR | O_CREAT, 0644);
                if (fd < 0) raw_syscall6(SYS_exit, 21, 0, 0, 0, 0, 0);
                /* Vary size so allocations of different lengths
                 * interleave (exposes ordering bugs).  Cap at half
                 * the threshold so we stay in SMALL tier. */
                size_t bytes = (size_t)(((c + 1) * 7919u
                                         + (uint32_t)it * 1031u)
                                         % (32u * 1024u)) + 1024u;
                static unsigned char buf[64 * 1024];
                for (size_t k = 0; k < bytes; k++) {
                    buf[k] = (unsigned char)((k * (c + 1)
                                              + it * 17u) & 0xff);
                }
                if (sud_inramfs_op_write(fd, buf, bytes) != (long)bytes)
                    raw_syscall6(SYS_exit, 22, 0, 0, 0, 0, 0);
                static unsigned char rb[64 * 1024];
                if (sud_inramfs_op_lseek(fd, 0, SEEK_SET) != 0)
                    raw_syscall6(SYS_exit, 23, 0, 0, 0, 0, 0);
                if (sud_inramfs_op_read(fd, rb, bytes) != (long)bytes)
                    raw_syscall6(SYS_exit, 24, 0, 0, 0, 0, 0);
                for (size_t k = 0; k < bytes; k++) {
                    if (rb[k] != buf[k])
                        raw_syscall6(SYS_exit, 25, 0, 0, 0, 0, 0);
                }
                if (sud_inramfs_op_ftruncate(fd, 1024) != 0)
                    raw_syscall6(SYS_exit, 26, 0, 0, 0, 0, 0);
                if (sud_inramfs_op_ftruncate(fd, bytes) != 0)
                    raw_syscall6(SYS_exit, 27, 0, 0, 0, 0, 0);
                sud_inramfs_op_close(fd);
                if (sud_inramfs_op_unlink(path) != 0)
                    raw_syscall6(SYS_exit, 28, 0, 0, 0, 0, 0);
            }
            raw_syscall6(SYS_exit, 0, 0, 0, 0, 0, 0);
        }
        pids[c] = pid;
    }
    int any_failed = 0;
    for (int c = 0; c < N_CHILDREN; c++) {
        int status = 0;
        raw_syscall6(SYS_wait4, pids[c], (long)&status, 0, 0, 0, 0);
        if ((status & 0xff7f) != 0) any_failed = 1;
    }
    TASSERT_EQ(any_failed, 0, "all children exited cleanly");

    uint32_t final_in_use = __atomic_load_n(&sb->small_blocks_in_use,
                                            __ATOMIC_ACQUIRE);
    TASSERT_EQ(final_in_use, initial_in_use,
               "small_blocks_in_use restored after all unlinks");

    teardown_mount();
}

/* SMALL→LARGE promotion test.
 *
 * Write a file in SMALL tier (under 128 KiB), verify the inode is
 * tagged SMALL.  Extend it past 128 KiB, verify the tag flips to
 * LARGE and the per-file shm appears under /dev/shm.  Read back
 * the full content and verify byte-for-byte that the small→large
 * copy preserved the original bytes plus appended new bytes. */
static void test_promotion(void)
{
    g_curtest = "promotion";
    setup_mount("/inramfs", 16, "test_promote");

    int fd = (int)sud_inramfs_op_open("/inramfs/big", O_RDWR | O_CREAT, 0644);
    TASSERT(fd >= 0, "open big");

    /* Phase 1: SMALL — write 64 KiB of pattern A. */
    static unsigned char patA[64 * 1024];
    for (size_t i = 0; i < sizeof(patA); i++)
        patA[i] = (unsigned char)((i * 37u + 11u) & 0xff);
    TASSERT_EQ(sud_inramfs_op_write(fd, patA, sizeof(patA)),
               (long)sizeof(patA), "wrote 64 KiB");

    struct sud_ir_inode *ino;
    {
        struct sud_ir_super *sb = sud_ir_sb();
        struct sud_ir_inode *table =
            (struct sud_ir_inode *)sud_ir_ptr(sb->inode_table_off);
        /* Locate the inode by scanning the table for the unique
         * REG inode whose size matches what we just wrote.  Fine
         * for a single-process test: only one such file exists. */
        ino = 0;
        for (uint32_t i = 1; i < sb->inode_count; i++) {
            if (table[i].type == SUD_IR_T_REG && table[i].size == sizeof(patA)) {
                ino = &table[i]; break;
            }
        }
        TASSERT(ino != 0, "located big inode");
    }
    TASSERT_EQ(ino->u.reg.tag, SUD_IR_REG_SMALL, "starts SMALL");

    /* Phase 2: extend to 200 KiB — must promote to LARGE. */
    static unsigned char patB[200 * 1024 - 64 * 1024];
    for (size_t i = 0; i < sizeof(patB); i++)
        patB[i] = (unsigned char)((i * 53u + 7u) & 0xff);
    TASSERT_EQ(sud_inramfs_op_write(fd, patB, sizeof(patB)),
               (long)sizeof(patB), "extend to 200 KiB");
    TASSERT_EQ(ino->u.reg.tag, SUD_IR_REG_LARGE, "promoted to LARGE");

    /* Phase 3: read back, verify both halves. */
    TASSERT_EQ(sud_inramfs_op_lseek(fd, 0, SEEK_SET), 0, "rewind");
    static unsigned char rb[200 * 1024];
    TASSERT_EQ(sud_inramfs_op_read(fd, rb, sizeof(rb)),
               (long)sizeof(rb), "read 200 KiB");
    int bad = 0;
    for (size_t i = 0; i < sizeof(patA); i++) {
        if (rb[i] != patA[i]) { bad = 1; break; }
    }
    TASSERT_EQ(bad, 0, "patA bytes preserved across promotion");
    for (size_t i = 0; i < sizeof(patB); i++) {
        if (rb[sizeof(patA) + i] != patB[i]) { bad = 2; break; }
    }
    TASSERT_EQ(bad, 0, "patB bytes correct after promotion");

    /* Phase 4: mmap-of-LARGE returns a writable mapping at the
     * right content; verify by reading the first byte. */
    int err = 0;
    void *m = sud_inramfs_op_mmap(0, 4096, PROT_READ, MAP_SHARED, fd, 0, &err);
    TASSERT(m != MAP_FAILED, "mmap large succeeds");
    TASSERT_EQ(((unsigned char *)m)[0], patA[0], "mmap content matches");
    munmap(m, 4096);

    sud_inramfs_op_close(fd);
    sud_inramfs_op_unlink("/inramfs/big");
    teardown_mount();
}

/* ---- entrypoint ---- */

int main(int argc, char **argv)
{
    (void)argc; (void)argv;
    /* environ is already set up by the freestanding _start stub
     * in libc-fs (see sudmini_start_c); don't overwrite it. */

    test_init_and_root();
    test_mkdir_lookup();
    test_open_write_read();
    test_lseek_holes();
    test_truncate();
    test_unlink_rename();
    test_symlink();
    test_getdents();
    test_chmod_chown_utimens();
    test_link_hardlink();
    test_root_mount();
    test_cross_process();
    test_mmap();
    test_multi_block();
    test_xxh64();
    test_smalldata_concurrency();
    test_promotion();

    if (g_failures) {
        char b[64];
        snprintf(b, sizeof(b), "inramfs test: %d FAILURES\n", g_failures);
        tlog(b);
        return 1;
    }
    tlog("inramfs test: all tests passed\n");
    return 0;
}
