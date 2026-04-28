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
    char val[PATH_MAX + 32];
    snprintf(val, sizeof(val), "%s:%d", path, size_mb);
    setenv("SUD_INRAMFS", val, 1);
    if (key) setenv("SUD_INRAMFS_KEY", key, 1);
    sud_inramfs_init();
}

static void teardown_mount(void)
{
    sud_inramfs_unlink_backing_for_testing();
    sud_inramfs_reset_for_testing();
    unsetenv("SUD_INRAMFS");
    unsetenv("SUD_INRAMFS_KEY");
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

    if (g_failures) {
        char b[64];
        snprintf(b, sizeof(b), "inramfs test: %d FAILURES\n", g_failures);
        tlog(b);
        return 1;
    }
    tlog("inramfs test: all tests passed\n");
    return 0;
}
