#!/bin/bash
# tests/sudtrace_test.sh — integration tests for sudtrace
#
# Tests that sudtrace correctly traces programs, including multithreaded
# ones, and produces a valid binary wire stream. Assertions are made
# against `tv dump`'s textual rendering of the wire file, so they are
# stable across binary-format revisions as long as the dump output
# stays human-readable.
set -eo pipefail

cd "$(dirname "$0")/.."
TV=./tv
SUDTRACE="$TV sud"
SUD32=./sud32
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# Render a wire file as text via `tv dump`. Returns the dump output on
# stdout. Wire files don't grep cleanly (zstd/binary framing), so all
# event-shape assertions go through this.
dump() { "$TV" dump "$1" 2>/dev/null; }

PASS=0 FAIL=0 TOTAL=0

run_test() {
    local name="$1"; shift
    TOTAL=$((TOTAL + 1))
    local ok=1
    for assertion in "$@"; do
        if ! eval "$assertion"; then ok=0; fi
    done
    if [ $ok -eq 1 ]; then
        PASS=$((PASS + 1))
        echo "  PASS  $name"
    else
        FAIL=$((FAIL + 1))
        echo "  FAIL  $name"
    fi
}

# ── Compile test programs ──────────────────────────────────────────────

cat > "$TMPDIR/hello.c" << 'EOF'
#include <stdio.h>
int main(void) {
    fprintf(stderr, "hello world\n");
    return 0;
}
EOF
gcc -o "$TMPDIR/hello" "$TMPDIR/hello.c"

cat > "$TMPDIR/threads.c" << 'EOF'
#include <stdio.h>
#include <pthread.h>
#include <unistd.h>
#include <fcntl.h>

void *worker(void *arg) {
    int id = *(int *)arg;
    fprintf(stderr, "thread %d\n", id);
    int fd = open("/dev/null", O_WRONLY);
    if (fd >= 0) { write(fd, "x", 1); close(fd); }
    return NULL;
}

int main(void) {
    pthread_t threads[4];
    int ids[4];
    for (int i = 0; i < 4; i++) {
        ids[i] = i;
        pthread_create(&threads[i], NULL, worker, &ids[i]);
    }
    for (int i = 0; i < 4; i++)
        pthread_join(threads[i], NULL);
    fprintf(stderr, "done\n");
    return 0;
}
EOF
gcc -o "$TMPDIR/threads" "$TMPDIR/threads.c" -lpthread

cat > "$TMPDIR/Makefile" << 'EOF'
all:
	/bin/echo build-ok
EOF

# Parallel Makefile: exercises SIGCHLD + jobserver pipe interaction.
# With -j4, GNU make blocks on the jobserver pipe between jobs and relies
# on SIGCHLD to interrupt the blocking read so it can reap finished
# children and release their job tokens.  Without the signal-mask fix,
# the SIGSYS handler blocks SIGCHLD → make deadlocks.
cat > "$TMPDIR/Makefile.parallel" << 'EOF'
.PHONY: all t1 t2 t3 t4 t5 t6 t7 t8
all: t1 t2 t3 t4 t5 t6 t7 t8
	@/bin/echo parallel-build-ok
t1 t2 t3 t4 t5 t6 t7 t8:
	@/bin/echo $@-done
EOF

cat > "$TMPDIR/shebang_args.sh" << 'EOF'
#!/bin/bash
printf 'argv0:%s\n' "$0"
printf 'arg1:%s\n' "$1"
EOF
chmod +x "$TMPDIR/shebang_args.sh"

cat > "$TMPDIR/static32.c" << 'EOF'
#include <unistd.h>
int main(void) {
    write(2, "hi32\n", 5);
    return 0;
}
EOF
HAVE_STATIC32=0
if gcc -m32 -static -o "$TMPDIR/static32" "$TMPDIR/static32.c" 2>/dev/null; then
    HAVE_STATIC32=1
else
    echo "  SKIP  static32 toolchain unavailable"
fi

# ── Compile seccomp test programs ──────────────────────────────────────

# Test: program that installs a seccomp BPF filter via seccomp() syscall
# and then makes syscalls.  Without the fix, this crashes with SIGSYS
# because the filter blocks sudtrace's handler-internal syscalls.
cat > "$TMPDIR/seccomp_filter.c" << 'EOF'
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <linux/seccomp.h>
#include <linux/filter.h>
#include <linux/audit.h>
#include <stddef.h>

/* A very restrictive BPF filter that only allows:
 *   read, write, exit, exit_group, rt_sigreturn, brk, mmap, close, fstat
 * This would break sudtrace's handler which needs openat, clock_gettime, etc.
 */
static struct sock_filter filter[] = {
    /* Load syscall number */
    BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
    /* Allow basic syscalls */
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_read,  7, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_write, 6, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_exit,  5, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_exit_group, 4, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_rt_sigreturn, 3, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_brk,   2, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_close, 1, 0),
    /* Kill on anything else */
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL),
    /* Allow */
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
};

static struct sock_fprog prog = {
    .len = sizeof(filter) / sizeof(filter[0]),
    .filter = filter,
};

int main(void) {
    /* PR_SET_NO_NEW_PRIVS is required for non-root seccomp */
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0) {
        perror("PR_SET_NO_NEW_PRIVS");
        return 1;
    }

    /* Install seccomp filter via seccomp() syscall */
    long r = syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog);
    if (r < 0) {
        perror("seccomp(SET_MODE_FILTER)");
        return 1;
    }

    /* These should work — write is in the filter allowlist,
     * but under sudtrace, the handler also needs openat/clock_gettime/etc.
     * which would be blocked without the fix. */
    write(2, "seccomp-filter-ok\n", 18);
    return 0;
}
EOF
HAVE_SECCOMP_FILTER=0
if gcc -o "$TMPDIR/seccomp_filter" "$TMPDIR/seccomp_filter.c" 2>/dev/null; then
    HAVE_SECCOMP_FILTER=1
else
    echo "  SKIP  seccomp filter test: compilation failed"
fi

# Test: program that enters seccomp strict mode via prctl().
# In strict mode only read/write/_exit/sigreturn are allowed.
cat > "$TMPDIR/seccomp_strict.c" << 'EOF'
#include <stdio.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <linux/seccomp.h>

int main(void) {
    /* First do the output that needs non-strict syscalls */
    write(2, "before-strict\n", 14);

    /* Enter strict seccomp mode */
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT, 0, 0, 0) < 0) {
        write(2, "strict-failed\n", 14);
        _exit(1);
    }

    /* In strict mode, only read/write/_exit/sigreturn are allowed.
     * Under sudtrace without the fix, the handler's internal syscalls
     * (openat, clock_gettime) would be blocked by seccomp → crash. */
    write(2, "seccomp-strict-ok\n", 18);
    _exit(0);
}
EOF
HAVE_SECCOMP_STRICT=0
if gcc -o "$TMPDIR/seccomp_strict" "$TMPDIR/seccomp_strict.c" 2>/dev/null; then
    HAVE_SECCOMP_STRICT=1
else
    echo "  SKIP  seccomp strict test: compilation failed"
fi

# Test: program that installs seccomp via prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER)
cat > "$TMPDIR/seccomp_prctl.c" << 'EOF'
#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <linux/seccomp.h>
#include <linux/filter.h>
#include <linux/audit.h>
#include <stddef.h>

/* Filter that kills on most syscalls */
static struct sock_filter filter[] = {
    BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_read,  5, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_write, 4, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_exit,  3, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_exit_group, 2, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_rt_sigreturn, 1, 0),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
};

static struct sock_fprog prog = {
    .len = sizeof(filter) / sizeof(filter[0]),
    .filter = filter,
};

int main(void) {
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);

    /* Install via prctl instead of seccomp() syscall */
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog, 0, 0) < 0) {
        write(2, "prctl-seccomp-failed\n", 21);
        _exit(1);
    }

    write(2, "seccomp-prctl-ok\n", 17);
    _exit(0);
}
EOF
HAVE_SECCOMP_PRCTL=0
if gcc -o "$TMPDIR/seccomp_prctl" "$TMPDIR/seccomp_prctl.c" 2>/dev/null; then
    HAVE_SECCOMP_PRCTL=1
else
    echo "  SKIP  seccomp prctl test: compilation failed"
fi

# Test: program with seccomp that forks children (simulates complex build)
cat > "$TMPDIR/seccomp_build.c" << 'EOF'
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <linux/seccomp.h>
#include <linux/filter.h>
#include <linux/audit.h>
#include <stddef.h>
#include <string.h>

/* Restrictive filter — only allows a handful of syscalls */
static struct sock_filter filter[] = {
    BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_read, 8, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_write, 7, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_exit, 6, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_exit_group, 5, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_rt_sigreturn, 4, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone, 3, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_wait4, 2, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_close, 1, 0),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
};

static struct sock_fprog prog = {
    .len = sizeof(filter) / sizeof(filter[0]),
    .filter = filter,
};

int main(void) {
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);

    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog) < 0) {
        write(2, "seccomp-setup-failed\n", 21);
        _exit(1);
    }

    /* Fork a child — simulates build subprocess creation.
     * Both parent and child inherit seccomp filters, and both
     * should work under sudtrace despite the restrictive filter. */
    pid_t pid = fork();
    if (pid < 0) {
        write(2, "fork-failed\n", 12);
        _exit(1);
    }
    if (pid == 0) {
        write(2, "child-ok\n", 9);
        _exit(0);
    }
    int status;
    waitpid(pid, &status, 0);
    write(2, "seccomp-build-ok\n", 17);
    _exit(0);
}
EOF
HAVE_SECCOMP_BUILD=0
if gcc -o "$TMPDIR/seccomp_build" "$TMPDIR/seccomp_build.c" 2>/dev/null; then
    HAVE_SECCOMP_BUILD=1
else
    echo "  SKIP  seccomp build test: compilation failed"
fi

# ── Compile nested signal test programs ────────────────────────────────

# Test: trigger nested SIGSYS by having a SIGALRM handler make syscalls
# while the SIGSYS handler is active.  This is the root cause of "Bad
# system call" failures in complex builds (LTO, distrobox) where SIGCHLD
# from parallel child termination interrupts the SIGSYS handler.
cat > "$TMPDIR/nested_signal.c" << 'EOF'
#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>
#include <signal.h>
#include <sys/time.h>
#include <string.h>
#include <fcntl.h>

static volatile int alarm_count = 0;

static void alarm_handler(int sig)
{
    (void)sig;
    alarm_count++;
    /* This write() is from traced code (outside SUD allowed range).
     * If SIGSYS is blocked (auto-masked in the handler) and signals
     * aren't blocked, the kernel force_sig(SIGSYS) kills the process. */
    char buf[32];
    int n = snprintf(buf, sizeof(buf), "a%d\n", alarm_count);
    write(STDERR_FILENO, buf, n);
}

int main(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = alarm_handler;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);

    /* 1ms timer — high frequency to maximize chance of interrupting
     * the SIGSYS handler during a syscall. */
    struct itimerval it;
    it.it_interval.tv_sec = 0;
    it.it_interval.tv_usec = 1000;
    it.it_value.tv_sec = 0;
    it.it_value.tv_usec = 1000;
    setitimer(ITIMER_REAL, &it, NULL);

    int fd = open("/dev/null", O_WRONLY);
    for (int i = 0; i < 100000 && fd >= 0; i++)
        write(fd, "x", 1);

    memset(&it, 0, sizeof(it));
    setitimer(ITIMER_REAL, &it, NULL);
    if (fd >= 0) close(fd);

    char msg[64];
    int len = snprintf(msg, sizeof(msg),
                       "nested-signal-ok (alarms=%d)\n", alarm_count);
    write(STDERR_FILENO, msg, len);
    return 0;
}
EOF
HAVE_NESTED_SIGNAL=0
if gcc -O2 -o "$TMPDIR/nested_signal" "$TMPDIR/nested_signal.c" 2>/dev/null; then
    HAVE_NESTED_SIGNAL=1
else
    echo "  SKIP  nested signal test: compilation failed"
fi

# ── Compile LTO test programs ─────────────────────────────────────────

# Test: multi-file C++ build with -flto=auto, simulating the exact
# scenario reported in the bug (LTO linker spawns parallel lto1 workers
# that signal SIGCHLD to the parent while the parent is in the SIGSYS
# handler).
cat > "$TMPDIR/lto_main.cpp" << 'EOF'
#include <cstdio>
extern int lto_helper(int x);
int main() {
    int r = lto_helper(21);
    fprintf(stderr, "lto-result=%d\n", r);
    return 0;
}
EOF
cat > "$TMPDIR/lto_helper.cpp" << 'EOF'
int lto_helper(int x) { return x * 2; }
EOF
HAVE_LTO=0
if g++ -std=c++17 -O2 -flto=auto -o "$TMPDIR/lto_test" \
       "$TMPDIR/lto_main.cpp" "$TMPDIR/lto_helper.cpp" 2>/dev/null; then
    HAVE_LTO=1
else
    echo "  SKIP  LTO test: g++ -flto=auto unavailable"
fi

# ── Compile identity-masking test programs ─────────────────────────────

# Test: program reads /proc/self/exe and checks it doesn't leak sudtrace path
cat > "$TMPDIR/check_proc_exe.c" << 'EOF'
#include <stdio.h>
#include <unistd.h>
#include <string.h>
int main(void) {
    char buf[4096];
    ssize_t n = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
    if (n <= 0) {
        fprintf(stderr, "FAIL: readlink(/proc/self/exe) failed\n");
        return 1;
    }
    buf[n] = '\0';
    fprintf(stderr, "proc_self_exe=%s\n", buf);
    if (strstr(buf, "sud64") || strstr(buf, "sud32") ||
        strstr(buf, "sudtrace")) {
        fprintf(stderr, "FAIL: /proc/self/exe leaks sudtrace path\n");
        return 1;
    }
    fprintf(stderr, "proc-exe-ok\n");
    return 0;
}
EOF
HAVE_CHECK_PROC_EXE=0
if gcc -o "$TMPDIR/check_proc_exe" "$TMPDIR/check_proc_exe.c" 2>/dev/null; then
    HAVE_CHECK_PROC_EXE=1
else
    echo "  SKIP  check_proc_exe test: compilation failed"
fi

# Test: program reads /proc/<pid>/exe (using getpid) and checks identity
cat > "$TMPDIR/check_pid_exe.c" << 'EOF'
#include <stdio.h>
#include <unistd.h>
#include <string.h>
int main(void) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/exe", (int)getpid());
    char buf[4096];
    ssize_t n = readlink(path, buf, sizeof(buf) - 1);
    if (n <= 0) {
        fprintf(stderr, "FAIL: readlink(%s) failed\n", path);
        return 1;
    }
    buf[n] = '\0';
    fprintf(stderr, "proc_pid_exe=%s\n", buf);
    if (strstr(buf, "sud64") || strstr(buf, "sud32") ||
        strstr(buf, "sudtrace")) {
        fprintf(stderr, "FAIL: /proc/<pid>/exe leaks sudtrace path\n");
        return 1;
    }
    fprintf(stderr, "pid-exe-ok\n");
    return 0;
}
EOF
HAVE_CHECK_PID_EXE=0
if gcc -o "$TMPDIR/check_pid_exe" "$TMPDIR/check_pid_exe.c" 2>/dev/null; then
    HAVE_CHECK_PID_EXE=1
else
    echo "  SKIP  check_pid_exe test: compilation failed"
fi

# Test: program reads AT_EXECFN from auxv and checks it doesn't leak
cat > "$TMPDIR/check_execfn.c" << 'EOF'
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
int main(void) {
    const char *execfn = (const char *)getauxval(AT_EXECFN);
    if (!execfn || !execfn[0]) {
        fprintf(stderr, "SKIP: AT_EXECFN not available\n");
        return 0;
    }
    fprintf(stderr, "at_execfn=%s\n", execfn);
    if (strstr(execfn, "sud64") || strstr(execfn, "sud32") ||
        strstr(execfn, "sudtrace")) {
        fprintf(stderr, "FAIL: AT_EXECFN leaks sudtrace path\n");
        return 1;
    }
    fprintf(stderr, "execfn-ok\n");
    return 0;
}
EOF
HAVE_CHECK_EXECFN=0
if gcc -o "$TMPDIR/check_execfn" "$TMPDIR/check_execfn.c" 2>/dev/null; then
    HAVE_CHECK_EXECFN=1
else
    echo "  SKIP  check_execfn test: compilation failed"
fi

# Test: program reads /proc/self/comm and checks it doesn't leak
cat > "$TMPDIR/check_comm.c" << 'EOF'
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
int main(void) {
    char buf[256] = {0};
    int fd = open("/proc/self/comm", O_RDONLY);
    if (fd < 0) {
        fprintf(stderr, "FAIL: cannot open /proc/self/comm\n");
        return 1;
    }
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) {
        fprintf(stderr, "FAIL: cannot read /proc/self/comm\n");
        return 1;
    }
    buf[n] = '\0';
    /* Strip trailing newline */
    char *nl = strchr(buf, '\n');
    if (nl) *nl = '\0';
    fprintf(stderr, "comm=%s\n", buf);
    if (strstr(buf, "sud64") || strstr(buf, "sud32")) {
        fprintf(stderr, "FAIL: /proc/self/comm leaks sudtrace name\n");
        return 1;
    }
    fprintf(stderr, "comm-ok\n");
    return 0;
}
EOF
HAVE_CHECK_COMM=0
if gcc -o "$TMPDIR/check_comm" "$TMPDIR/check_comm.c" 2>/dev/null; then
    HAVE_CHECK_COMM=1
else
    echo "  SKIP  check_comm test: compilation failed"
fi

# ── Test: basic single-threaded tracing ────────────────────────────────

if [ -x "$TV" ]; then

OUT=$($SUDTRACE -o "$TMPDIR/hello.wire" -- "$TMPDIR/hello" 2>&1)
run_test "basic: hello world traced" \
    'dump "$TMPDIR/hello.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/hello.wire" | grep -q " EXIT "' \
    'dump "$TMPDIR/hello.wire" | grep -q "exited code/sig="' \
    '! dump "$TMPDIR/hello.wire" | grep -q "signaled code/sig="'

run_test "basic: stderr captured" \
    'dump "$TMPDIR/hello.wire" | grep -q " STDERR "' \
    'dump "$TMPDIR/hello.wire" | grep -q "hello world"'

OUT=$($SUDTRACE -o "$TMPDIR/shebang_args.wire" -- "$TMPDIR/shebang_args.sh" v045 2>&1)
run_test "basic: shebang script keeps interpreter argv aligned" \
    'printf "%s\n" "$OUT" | grep -F -q -- "argv0:$TMPDIR/shebang_args.sh"' \
    'printf "%s\n" "$OUT" | grep -F -q -- "arg1:v045"' \
    'dump "$TMPDIR/shebang_args.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/shebang_args.wire" | grep -q "exited code/sig=0 "' \
    '! dump "$TMPDIR/shebang_args.wire" | grep -q "signaled code/sig="'

# ── Test: multithreaded tracing ────────────────────────────────────────

# Run 3 times to detect race conditions — the clone3 child/parent
# synchronization and signal handler re-entrancy are timing-sensitive.
for attempt in 1 2 3; do
    OUT=$($SUDTRACE -o "$TMPDIR/threads_${attempt}.wire" -- "$TMPDIR/threads" 2>&1)
    run_test "threads attempt $attempt: no crash" \
        '! dump "$TMPDIR/threads_${attempt}.wire" | grep -q "signaled code/sig="' \
        'dump "$TMPDIR/threads_${attempt}.wire" | grep -q "exited code/sig="'
    run_test "threads attempt $attempt: stderr captured" \
        'dump "$TMPDIR/threads_${attempt}.wire" | grep -q "done"'
done

OUT=$($SUDTRACE -o "$TMPDIR/make.wire" -- make -f "$TMPDIR/Makefile" 2>&1)
run_test "make: external command traced without SIGSYS crash" \
    'printf "%s\n" "$OUT" | grep -q "build-ok"' \
    'dump "$TMPDIR/make.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/make.wire" | grep -q "exited code/sig="' \
    '! dump "$TMPDIR/make.wire" | grep -q "signaled code/sig="'

# ── Test: parallel make (SIGCHLD + jobserver pipe interaction) ─────────

OUT=$(timeout 30 $SUDTRACE -o "$TMPDIR/pmake.wire" -- \
    make -j4 -f "$TMPDIR/Makefile.parallel" 2>&1)
run_test "make -j4: parallel build completes (SIGCHLD not blocked)" \
    'printf "%s\n" "$OUT" | grep -q "parallel-build-ok"' \
    'dump "$TMPDIR/pmake.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/pmake.wire" | grep -q "exited code/sig="' \
    '! dump "$TMPDIR/pmake.wire" | grep -q "signaled code/sig="'

# ── Test: seccomp filter via seccomp() syscall ────────────────────────

if [ "$HAVE_SECCOMP_FILTER" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/seccomp_filter.wire" -- "$TMPDIR/seccomp_filter" 2>&1)
run_test "seccomp: filter via seccomp() syscall doesn't crash" \
    'printf "%s\n" "$OUT" | grep -q "seccomp-filter-ok"' \
    'dump "$TMPDIR/seccomp_filter.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/seccomp_filter.wire" | grep -q "exited code/sig="' \
    '! dump "$TMPDIR/seccomp_filter.wire" | grep -q "signaled code/sig="'
fi

# ── Test: seccomp strict mode via prctl() ─────────────────────────────

if [ "$HAVE_SECCOMP_STRICT" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/seccomp_strict.wire" -- "$TMPDIR/seccomp_strict" 2>&1)
run_test "seccomp: strict mode via prctl doesn't crash" \
    'printf "%s\n" "$OUT" | grep -q "seccomp-strict-ok"' \
    'dump "$TMPDIR/seccomp_strict.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/seccomp_strict.wire" | grep -q "exited code/sig="' \
    '! dump "$TMPDIR/seccomp_strict.wire" | grep -q "signaled code/sig="'
fi

# ── Test: seccomp filter via prctl(PR_SET_SECCOMP) ────────────────────

if [ "$HAVE_SECCOMP_PRCTL" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/seccomp_prctl.wire" -- "$TMPDIR/seccomp_prctl" 2>&1)
run_test "seccomp: filter via prctl(PR_SET_SECCOMP) doesn't crash" \
    'printf "%s\n" "$OUT" | grep -q "seccomp-prctl-ok"' \
    'dump "$TMPDIR/seccomp_prctl.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/seccomp_prctl.wire" | grep -q "exited code/sig="' \
    '! dump "$TMPDIR/seccomp_prctl.wire" | grep -q "signaled code/sig="'
fi

# ── Test: seccomp with fork (simulates complex build) ─────────────────

if [ "$HAVE_SECCOMP_BUILD" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/seccomp_build.wire" -- "$TMPDIR/seccomp_build" 2>&1)
run_test "seccomp: filter + fork (complex build simulation) works" \
    'printf "%s\n" "$OUT" | grep -q "seccomp-build-ok"' \
    'printf "%s\n" "$OUT" | grep -q "child-ok"' \
    'dump "$TMPDIR/seccomp_build.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/seccomp_build.wire" | grep -q "exited code/sig="' \
    '! dump "$TMPDIR/seccomp_build.wire" | grep -q "signaled code/sig="'
fi

# ── Test: nested signal (root cause of LTO/distrobox crashes) ─────────

if [ "$HAVE_NESTED_SIGNAL" -eq 1 ]; then
# Run 3 times — the crash is timing-dependent (signal must interrupt
# the SIGSYS handler while it's executing a syscall).
for attempt in 1 2 3; do
    OUT=$($SUDTRACE -o "$TMPDIR/nested_signal_${attempt}.wire" -- "$TMPDIR/nested_signal" 2>&1)
    run_test "nested signal attempt $attempt: no Bad system call" \
        'printf "%s\n" "$OUT" | grep -q "nested-signal-ok"' \
        'printf "%s\n" "$OUT" | grep -q "alarms="' \
        '! printf "%s\n" "$OUT" | grep -q "alarms=0)"' \
        'dump "$TMPDIR/nested_signal_${attempt}.wire" | grep -q " EXEC "' \
        'dump "$TMPDIR/nested_signal_${attempt}.wire" | grep -q "exited code/sig="' \
        '! dump "$TMPDIR/nested_signal_${attempt}.wire" | grep -q "signaled code/sig="'
done
fi

# ── Test: LTO build under sudtrace ────────────────────────────────────

if [ "$HAVE_LTO" -eq 1 ]; then
# Build the LTO test program UNDER sudtrace — this exercises the exact
# codepath that triggers SIGCHLD during the SIGSYS handler (parallel
# lto1/cc1plus workers terminate and signal the parent).
OUT=$($SUDTRACE -o "$TMPDIR/lto_build.wire" -- \
    g++ -std=c++17 -O2 -flto=auto -o "$TMPDIR/lto_build_out" \
        "$TMPDIR/lto_main.cpp" "$TMPDIR/lto_helper.cpp" 2>&1)
run_test "LTO build: g++ -flto=auto under sudtrace doesn't crash" \
    'test -x "$TMPDIR/lto_build_out"' \
    'dump "$TMPDIR/lto_build.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/lto_build.wire" | grep -q "exited code/sig="' \
    '! dump "$TMPDIR/lto_build.wire" | grep -q "signaled code/sig="'
fi

if [ "$HAVE_STATIC32" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/static32.wire" -- "$TMPDIR/static32" 2>&1)
run_test "static32: traced through sud32 wrapper" \
    'dump "$TMPDIR/static32.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/static32.wire" | grep -q "exited code/sig="' \
    'dump "$TMPDIR/static32.wire" | grep -q " STDERR "' \
    'dump "$TMPDIR/static32.wire" | grep -q "hi32"' \
    '! dump "$TMPDIR/static32.wire" | grep -q "signaled code/sig="'

if [ -x "$TV" ]; then
OUT=$("$TV" --uproctrace --sud -o "$TMPDIR/up_static32.wire" -- "$TMPDIR/static32" 2>&1)
run_test "uproctrace --sud: static32 uses matching sud launcher" \
    'dump "$TMPDIR/up_static32.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/up_static32.wire" | grep -q "exited code/sig="' \
    'dump "$TMPDIR/up_static32.wire" | grep -q " STDERR "' \
    'dump "$TMPDIR/up_static32.wire" | grep -q "hi32"' \
    '! dump "$TMPDIR/up_static32.wire" | grep -q "signaled code/sig="'
fi
fi

if [ -x "$SUD32" ]; then
OUT=$("$SUD32" -o "$TMPDIR/sud32_hello.wire" -- "$TMPDIR/hello" 2>&1)
run_test "sud32: mixed-arch launch selects traceable wrapper" \
    'dump "$TMPDIR/sud32_hello.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/sud32_hello.wire" | grep -q "exited code/sig="' \
    'dump "$TMPDIR/sud32_hello.wire" | grep -q " STDERR "' \
    'dump "$TMPDIR/sud32_hello.wire" | grep -q "hello world"' \
    '! dump "$TMPDIR/sud32_hello.wire" | grep -q "signaled code/sig="'
fi

# ── Test: identity masking — /proc/self/exe ───────────────────────────

if [ "$HAVE_CHECK_PROC_EXE" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/proc_exe.wire" -- "$TMPDIR/check_proc_exe" 2>&1)
run_test "identity: /proc/self/exe doesn't leak sudtrace" \
    'printf "%s\n" "$OUT" | grep -q "proc-exe-ok"' \
    '! printf "%s\n" "$OUT" | grep -q "FAIL"' \
    'dump "$TMPDIR/proc_exe.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/proc_exe.wire" | grep -q "exited code/sig="'
fi

# ── Test: identity masking — /proc/<pid>/exe ──────────────────────────

if [ "$HAVE_CHECK_PID_EXE" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/pid_exe.wire" -- "$TMPDIR/check_pid_exe" 2>&1)
run_test "identity: /proc/<pid>/exe doesn't leak sudtrace" \
    'printf "%s\n" "$OUT" | grep -q "pid-exe-ok"' \
    '! printf "%s\n" "$OUT" | grep -q "FAIL"' \
    'dump "$TMPDIR/pid_exe.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/pid_exe.wire" | grep -q "exited code/sig="'
fi

# ── Test: identity masking — AT_EXECFN ────────────────────────────────

if [ "$HAVE_CHECK_EXECFN" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/execfn.wire" -- "$TMPDIR/check_execfn" 2>&1)
run_test "identity: AT_EXECFN doesn't leak sudtrace" \
    'printf "%s\n" "$OUT" | grep -q "execfn-ok"' \
    '! printf "%s\n" "$OUT" | grep -q "FAIL"' \
    'dump "$TMPDIR/execfn.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/execfn.wire" | grep -q "exited code/sig="'
fi

# ── Test: identity masking — /proc/self/comm ──────────────────────────

if [ "$HAVE_CHECK_COMM" -eq 1 ]; then
OUT=$($SUDTRACE -o "$TMPDIR/comm.wire" -- "$TMPDIR/check_comm" 2>&1)
run_test "identity: /proc/self/comm doesn't leak sudtrace" \
    'printf "%s\n" "$OUT" | grep -q "comm-ok"' \
    '! printf "%s\n" "$OUT" | grep -q "FAIL"' \
    'dump "$TMPDIR/comm.wire" | grep -q " EXEC "' \
    'dump "$TMPDIR/comm.wire" | grep -q "exited code/sig="'
fi

# ── UNLINK event test ──────────────────────────────────────────────────

cat > "$TMPDIR/unlink_test.c" << 'EOF'
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
int main(void) {
    int fd = open("/tmp/sudtrace_unlink_test_$$", O_WRONLY|O_CREAT|O_TRUNC, 0644);
    if (fd >= 0) {
        write(fd, "hello", 5);
        close(fd);
        unlink("/tmp/sudtrace_unlink_test_$$");
    }
    return 0;
}
EOF
if cc -static -o "$TMPDIR/unlink_test" "$TMPDIR/unlink_test.c" 2>/dev/null; then
    OUT=$($SUDTRACE -o "$TMPDIR/unlink.wire" -- "$TMPDIR/unlink_test" 2>&1 || true)
    run_test "unlink: UNLINK event emitted" \
        'dump "$TMPDIR/unlink.wire" | grep -q " UNLINK "'
fi

else
    echo "  SKIP  sudtrace not built (run 'make sudtrace' first)"
fi

# ═══════════════════════════════════════════════════════════════════════
# Summary
# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "═══════════════════════════════════════"
echo "  $PASS passed, $FAIL failed (of $TOTAL)"
echo "═══════════════════════════════════════"
[ "$FAIL" -eq 0 ]
