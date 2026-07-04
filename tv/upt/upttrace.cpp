/*
 * upt/upttrace.cpp — userspace process tracer using ptrace.
 *
 * Standalone tracer binary. Produces the same trace stream as
 * mod/modtrace and sud/sudtrace, but works in any environment that
 * permits ptrace (no kernel module, no syscall-user-dispatch).
 *
 * Usage:
 *   upttrace [-o FILE[.zst]] [--no-env] -- command [args...]
 *
 * If -o is omitted the trace is written to stdout, e.g. for piping:
 *
 *   upttrace -- foo bar | tv --trace -
 *
 * Events emitted: CWD, EXEC, ARGV, ENV, AUXV, OPEN (real + inherited),
 * EXIT, STDOUT, STDERR.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cstdint>
#include <cerrno>
#include <climits>
#include <cctype>
#include <cstdarg>
#include <ctime>

extern "C" {
#include "wire/wire.h"
#include "trace/trace.h"
}

#include <string>
#include <vector>
#include <unordered_map>
#include <optional>
#include <utility>
#include <algorithm>
#include <string_view>
#include <memory>
#include <new>

#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
#include <dirent.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/ptrace.h>
#include <sys/user.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/prctl.h>
#include <linux/ptrace.h>
#include <elf.h>          /* AT_* constants, ElfW, NT_PRSTATUS */
#include <pthread.h>

#ifndef PR_SET_SYSCALL_USER_DISPATCH
#define PR_SET_SYSCALL_USER_DISPATCH 59
#endif
#ifndef PR_SYS_DISPATCH_OFF
#define PR_SYS_DISPATCH_OFF 0
#endif

/* C++ ptrace wrapper: glibc's C++ prototype wants __ptrace_request enum */
template <typename... Args>
static inline long xptrace(int req, Args... args) {
    return ptrace(static_cast<__ptrace_request>(req), args...);
}

/* ================================================================
 * Constants
 * ================================================================ */

static constexpr size_t WRITE_CAPTURE_MAX   = 4096;
static constexpr size_t ARGV_MAX_READ       = 32768;
static constexpr size_t ENV_MAX_READ        = 65536;
static constexpr size_t AUXV_MAX_READ       = 4096;
static constexpr size_t TRACE_OUT_RING_SIZE = (1 << 20);
static constexpr size_t TRACE_OUT_CHUNK_SIZE = (1 << 16);

/* ================================================================
 * Output stream
 * ================================================================ */

struct trace_output {
    FILE *stream = nullptr;
    bool owns_stream = false;
    bool error = false;
    bool closing = false;
    pid_t compressor_pid = 0;
    std::unique_ptr<char[]> ring;
    size_t ring_size = 0;
    size_t head = 0;
    size_t tail = 0;
    size_t used = 0;
    pthread_t writer = {};
    pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t can_read = PTHREAD_COND_INITIALIZER;
    pthread_cond_t can_write = PTHREAD_COND_INITIALIZER;
};

static struct trace_output g_out;
static struct stat g_creator_stdout_st; /* stat of the session creator's stdout */
static int g_creator_stdout_valid;
static int g_trace_exec_env = 1;

/* Global ev_state for wire format emission + mutex */
static ev_state g_ev_state{};
static pthread_mutex_t g_ev_lock = PTHREAD_MUTEX_INITIALIZER;

/* Forward declarations */
static int trace_output_enqueue(const char *buf, size_t len);

static bool path_has_suffix(const char *path, const char *suffix)
{
    if (!path || !suffix) return false;
    std::string_view p(path);
    std::string_view s(suffix);
    return p.ends_with(s);
}

/* ================================================================
 * Wire format event emission helper.
 *
 * Builds one wire event atom (header + blob) and writes to the output
 * ring. Mirroring proctrace.c's emit_one and sud/event.c's emit_event.
 * ================================================================ */

static void emit_one(int32_t type, uint64_t ts_ns,
                     pid_t pid, pid_t tgid, pid_t ppid,
                     pid_t nspid, pid_t nstgid,
                     const int64_t *extras, unsigned n_extras,
                     const void *blob, size_t blen)
{
    size_t buf_size = EV_HEADER_MAX + 2 * WIRE_PREFIX_MAX + blen + 16;
    std::vector<uint8_t> buf(buf_size);

    pthread_mutex_lock(&g_ev_lock);

    uint8_t hdr[EV_HEADER_MAX];
    Dst hd = wire_dst(hdr, sizeof hdr);
    /* upttrace is single-producer — stream_id 1. */
    ev_build_header(&g_ev_state, &hd, 1u, type, ts_ns,
                    pid, tgid, ppid, nspid, nstgid,
                    extras, n_extras);
    if (hd.p) {
        size_t hlen = (size_t)(hd.p - hdr);
        Dst od = wire_dst(buf.data(), buf_size);
        wire_put_pair(&od, wire_src(hdr, hlen), wire_src(blob, blen));
        if (od.p) {
            size_t total = (size_t)((uint8_t *)od.p - buf.data());
            pthread_mutex_unlock(&g_ev_lock);
            trace_output_enqueue(reinterpret_cast<const char*>(buf.data()), total);
            return;
        }
    }
    pthread_mutex_unlock(&g_ev_lock);
}

static int trace_output_write_plain(struct trace_output *out, const char *buf, size_t len)
{
    return fwrite(buf, 1, len, out->stream) == len ? 0 : -1;
}

static void *trace_output_writer_main(void *arg)
{
    struct trace_output *out = static_cast<struct trace_output *>(arg);
    std::unique_ptr<char[]> chunk(new(std::nothrow) char[TRACE_OUT_CHUNK_SIZE]);
    if (!chunk) {
        pthread_mutex_lock(&out->lock);
        out->error = true;
        pthread_cond_broadcast(&out->can_read);
        pthread_cond_broadcast(&out->can_write);
        pthread_mutex_unlock(&out->lock);
        return nullptr;
    }

    for (;;) {
        size_t n = 0;

        pthread_mutex_lock(&out->lock);
        while (out->used == 0 && !out->closing && !out->error)
            pthread_cond_wait(&out->can_read, &out->lock);
        if (out->error) {
            pthread_mutex_unlock(&out->lock);
            break;
        }
        if (out->used == 0 && out->closing) {
            pthread_mutex_unlock(&out->lock);
            break;
        }

        n = out->ring_size - out->tail;
        if (n > out->used) n = out->used;
        if (n > TRACE_OUT_CHUNK_SIZE) n = TRACE_OUT_CHUNK_SIZE;
        std::memcpy(chunk.get(), out->ring.get() + out->tail, n);
        out->tail = (out->tail + n) % out->ring_size;
        out->used -= n;
        pthread_cond_signal(&out->can_write);
        pthread_mutex_unlock(&out->lock);

        if (trace_output_write_plain(out, chunk.get(), n) != 0) {
            pthread_mutex_lock(&out->lock);
            out->error = true;
            pthread_cond_broadcast(&out->can_read);
            pthread_cond_broadcast(&out->can_write);
            pthread_mutex_unlock(&out->lock);
            break;
        }
    }

    if (!out->error)
        fflush(out->stream);
    return nullptr;
}

static int trace_output_enqueue(const char *buf, size_t len)
{
    struct trace_output *out = &g_out;

    if (!out->ring || len == 0) return 0;
    if (len > out->ring_size) return -1;

    pthread_mutex_lock(&out->lock);
    while (!out->error && (out->ring_size - out->used) < len)
        pthread_cond_wait(&out->can_write, &out->lock);
    if (out->error) {
        pthread_mutex_unlock(&out->lock);
        return -1;
    }

    size_t first = out->ring_size - out->head;
    if (first > len) first = len;
    std::memcpy(out->ring.get() + out->head, buf, first);
    if (len > first)
        std::memcpy(out->ring.get(), buf + first, len - first);
    out->head = (out->head + len) % out->ring_size;
    out->used += len;
    pthread_cond_signal(&out->can_read);
    pthread_mutex_unlock(&out->lock);
    return 0;
}

/* ================================================================
 * Timestamp
 * ================================================================ */

static uint64_t get_ts_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return static_cast<uint64_t>(ts.tv_sec) * 1000000000ULL 
         + static_cast<uint64_t>(ts.tv_nsec);
}

/* ================================================================
 * /proc helpers
 * ================================================================ */

/* Read a whole /proc file into a std::string */
static std::string read_proc_file(pid_t pid, const char *name, size_t max)
{
    char path[256];
    snprintf(path, sizeof(path), "/proc/%d/%s", static_cast<int>(pid), name);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return {};
    std::string buf(max, '\0');
    size_t total = 0;
    ssize_t n;
    while (total < max && (n = read(fd, buf.data() + total, max - total)) > 0)
        total += static_cast<size_t>(n);
    close(fd);
    if (total == 0) return {};
    buf.resize(total);
    return buf;
}

/* Read /proc/PID/exe symlink */
static std::string read_proc_exe(pid_t pid)
{
    char path[256];
    snprintf(path, sizeof(path), "/proc/%d/exe", static_cast<int>(pid));
    char buf[PATH_MAX];
    ssize_t n = readlink(path, buf, sizeof(buf) - 1);
    if (n <= 0) return {};
    buf[n] = '\0';
    std::string_view sv(buf, static_cast<size_t>(n));
    if (sv.ends_with(" (deleted)"))
        sv.remove_suffix(10);
    return std::string(sv);
}

/* Read /proc/PID/cwd symlink */
static std::string read_proc_cwd(pid_t pid)
{
    char path[256];
    snprintf(path, sizeof(path), "/proc/%d/cwd", static_cast<int>(pid));
    char buf[PATH_MAX];
    ssize_t n = readlink(path, buf, sizeof(buf) - 1);
    if (n <= 0) return {};
    buf[n] = '\0';
    return std::string(buf, static_cast<size_t>(n));
}

/* Get ppid from /proc/PID/stat */
static pid_t get_ppid(pid_t pid)
{
    char path[256], buf[512];
    snprintf(path, sizeof(path), "/proc/%d/stat", static_cast<int>(pid));
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) return 0;
    buf[n] = '\0';
    /* Format: pid (comm) state ppid ... */
    char *cp = std::strrchr(buf, ')');
    if (!cp) return 0;
    int ppid = 0;
    if (sscanf(cp + 2, "%*c %d", &ppid) != 1) return 0;
    return ppid;
}

/* Get tgid from /proc/PID/status */
static pid_t get_tgid(pid_t pid)
{
    char path[256], buf[2048];
    snprintf(path, sizeof(path), "/proc/%d/status", static_cast<int>(pid));
    int fd = open(path, O_RDONLY);
    if (fd < 0) return pid; /* fallback: assume tgid == pid */
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) return pid;
    buf[n] = '\0';
    const char *p = std::strstr(buf, "\nTgid:");
    if (!p) return pid;
    return std::atoi(p + 6);
}

/* Read a process's memory at a given address. */
static int ensure_proc_mem_fd(pid_t pid);

static ssize_t read_proc_mem(pid_t pid, unsigned long addr, void *buf, size_t len)
{
    int fd = ensure_proc_mem_fd(pid);
    if (fd < 0) return -1;
    return pread(fd, buf, len, static_cast<off_t>(addr));
}

/* Write to a process's memory at a given address. */
static ssize_t write_proc_mem(pid_t pid, unsigned long addr, const void *buf, size_t len)
{
    char path[256];
    snprintf(path, sizeof(path), "/proc/%d/mem", static_cast<int>(pid));
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -1;
    ssize_t n = pwrite(fd, buf, len, static_cast<off_t>(addr));
    close(fd);
    return n;
}

/* ================================================================
 * Tracked-pid set (std::vector)
 * ================================================================ */

static std::vector<pid_t> g_tracked;

static void pidset_add(pid_t pid)
{
    if (std::find(g_tracked.begin(), g_tracked.end(), pid) != g_tracked.end())
        return;
    g_tracked.push_back(pid);
}

static void pidset_remove(pid_t pid)
{
    auto it = std::find(g_tracked.begin(), g_tracked.end(), pid);
    if (it != g_tracked.end()) {
        *it = g_tracked.back();
        g_tracked.pop_back();
    }
}

static bool pidset_contains(pid_t pid)
{
    return std::find(g_tracked.begin(), g_tracked.end(), pid) != g_tracked.end();
}

/* ================================================================
 * Per-process state (for syscall entry/exit tracking)
 * ================================================================ */

struct proc_state {
    pid_t pid = 0;
    pid_t tgid = 0;           /* thread group leader — cached at first sight */
    pid_t ppid = 0;           /* cached parent pid */
    bool  is_thread = false;  /* true if pid != tgid (a non-leader thread) */
    int   mem_fd = -1;        /* cached /proc/pid/mem fd */
    bool  in_syscall = false; /* true if we're at syscall entry, false at exit */
    long  saved_syscall = 0;  /* syscall number at entry */
    /* saved args for specific syscalls */
    unsigned long arg0 = 0, arg1 = 0, arg2 = 0, arg3 = 0;
    /* ptrace emulation */
    bool  emu_neutralized = false; /* true if syscall was replaced with -1 for emulation */
    bool  emu_waiting = false;     /* true if blocked in emulated wait4 */
    unsigned long emu_wait_wstatus_addr = 0; /* wstatus pointer for emulated wait */
    long  emu_wait_pid = 0;    /* pid arg for emulated wait */
    long  emu_wait_options = 0;/* options arg for emulated wait */
    bool  emu_race_interrupt = false; /* true if we injected SIGCHLD to break real wait4 */

    ~proc_state() { if (mem_fd >= 0) ::close(mem_fd); }

    proc_state() = default;

    proc_state(proc_state&& o) noexcept
        : pid(o.pid), tgid(o.tgid), ppid(o.ppid), is_thread(o.is_thread),
          mem_fd(std::exchange(o.mem_fd, -1)), in_syscall(o.in_syscall),
          saved_syscall(o.saved_syscall),
          arg0(o.arg0), arg1(o.arg1), arg2(o.arg2), arg3(o.arg3),
          emu_neutralized(o.emu_neutralized), emu_waiting(o.emu_waiting),
          emu_wait_wstatus_addr(o.emu_wait_wstatus_addr),
          emu_wait_pid(o.emu_wait_pid), emu_wait_options(o.emu_wait_options),
          emu_race_interrupt(o.emu_race_interrupt)
    {}

    proc_state& operator=(proc_state&& o) noexcept {
        if (this != &o) {
            if (mem_fd >= 0) ::close(mem_fd);
            pid = o.pid; tgid = o.tgid; ppid = o.ppid;
            is_thread = o.is_thread;
            mem_fd = std::exchange(o.mem_fd, -1);
            in_syscall = o.in_syscall;
            saved_syscall = o.saved_syscall;
            arg0 = o.arg0; arg1 = o.arg1; arg2 = o.arg2; arg3 = o.arg3;
            emu_neutralized = o.emu_neutralized;
            emu_waiting = o.emu_waiting;
            emu_wait_wstatus_addr = o.emu_wait_wstatus_addr;
            emu_wait_pid = o.emu_wait_pid;
            emu_wait_options = o.emu_wait_options;
            emu_race_interrupt = o.emu_race_interrupt;
        }
        return *this;
    }

    proc_state(const proc_state&) = delete;
    proc_state& operator=(const proc_state&) = delete;
};

static std::unordered_map<pid_t, proc_state> g_states;

static proc_state *get_state(pid_t pid)
{
    auto [it, inserted] = g_states.try_emplace(pid);
    if (inserted) {
        it->second.pid = pid;
        it->second.tgid = get_tgid(pid);
        it->second.ppid = get_ppid(pid);
        it->second.is_thread = (it->second.tgid != pid);
    }
    return &it->second;
}

static void free_state(pid_t pid)
{
    g_states.erase(pid);
}

static int ensure_proc_mem_fd(pid_t pid)
{
    proc_state *ps = get_state(pid);
    char path[256];
    if (!ps) return -1;
    if (ps->mem_fd >= 0) return ps->mem_fd;
    snprintf(path, sizeof(path), "/proc/%d/mem", static_cast<int>(pid));
    ps->mem_fd = open(path, O_RDONLY);
    return ps->mem_fd;
}

static void get_cached_ids(pid_t pid, pid_t *tgid, pid_t *ppid)
{
    proc_state *ps = get_state(pid);
    if (!ps) {
        *tgid = pid;
        *ppid = 0;
        return;
    }
    if (ps->tgid <= 0) ps->tgid = get_tgid(pid);
    if (ps->ppid <= 0) ps->ppid = get_ppid(pid);
    *tgid = ps->tgid > 0 ? ps->tgid : pid;
    *ppid = ps->ppid;
}

/* ================================================================
 * Read tracee string from /proc/pid/mem
 * ================================================================ */

static std::optional<std::string> read_tracee_string(pid_t pid, unsigned long addr, size_t max)
{
    if (!addr) return std::nullopt;
    std::string buf(max, '\0');
    ssize_t n = read_proc_mem(pid, addr, buf.data(), max);
    if (n <= 0) return std::nullopt;
    /* Find actual string length within the read data */
    size_t slen = strnlen(buf.data(), static_cast<size_t>(n));
    buf.resize(slen);
    return buf;
}

/* ================================================================
 * Event emission
 * ================================================================ */

static void emit_cwd_event(pid_t pid)
{
    pid_t tgid, ppid;
    get_cached_ids(pid, &tgid, &ppid);
    uint64_t ts_ns = get_ts_ns();

    std::string cwd = read_proc_cwd(pid);
    if (cwd.empty()) return;

    emit_one(EV_CWD, ts_ns, pid, tgid, ppid, pid, tgid,
             nullptr, 0, cwd.data(), cwd.size());
}

static void emit_exec_event(pid_t pid)
{
    pid_t tgid, ppid;
    get_cached_ids(pid, &tgid, &ppid);
    uint64_t ts_ns = get_ts_ns();

    /* 1. EV_EXEC — exe path */
    std::string exe = read_proc_exe(pid);
    emit_one(EV_EXEC, ts_ns, pid, tgid, ppid, pid, tgid,
             nullptr, 0, exe.data(), exe.size());

    /* 2. EV_ARGV — raw NUL-separated argv bytes from /proc/<pid>/cmdline */
    std::string argv_raw = read_proc_file(pid, "cmdline", ARGV_MAX_READ);
    emit_one(EV_ARGV, ts_ns, pid, tgid, ppid, pid, tgid,
             nullptr, 0, argv_raw.data(), argv_raw.size());

    /* 3. EV_ENV — raw NUL-separated environ bytes (only if g_trace_exec_env) */
    if (g_trace_exec_env) {
        std::string env_raw = read_proc_file(pid, "environ", ENV_MAX_READ);
        emit_one(EV_ENV, ts_ns, pid, tgid, ppid, pid, tgid,
                 nullptr, 0, env_raw.data(), env_raw.size());
    }

    /* 4. EV_AUXV — raw /proc/<pid>/auxv bytes */
    std::string auxv_raw = read_proc_file(pid, "auxv", AUXV_MAX_READ);
    emit_one(EV_AUXV, ts_ns, pid, tgid, ppid, pid, tgid,
             nullptr, 0, auxv_raw.data(), auxv_raw.size());
}

static void emit_inherited_open_for_fd(pid_t pid, pid_t tgid, pid_t ppid,
                                        uint64_t ts_ns,
                                        int fd_num)
{
    /* Read fd link and stat */
    char link_path[256], link_target[PATH_MAX];
    snprintf(link_path, sizeof(link_path), "/proc/%d/fd/%d", static_cast<int>(pid), fd_num);
    ssize_t n = readlink(link_path, link_target, sizeof(link_target) - 1);
    if (n <= 0) return;
    link_target[n] = '\0';

    struct stat st;
    if (fstatat(AT_FDCWD, link_path, &st, 0) < 0) {
        /* If we can't stat through /proc, use zeros */
        std::memset(&st, 0, sizeof(st));
    }

    /* Read fdinfo for flags */
    char fdinfo_path[256], fdinfo_buf[512];
    snprintf(fdinfo_path, sizeof(fdinfo_path), "/proc/%d/fdinfo/%d", static_cast<int>(pid), fd_num);
    int flags = O_RDONLY;
    int fi = open(fdinfo_path, O_RDONLY);
    if (fi >= 0) {
        ssize_t r = read(fi, fdinfo_buf, sizeof(fdinfo_buf) - 1);
        close(fi);
        if (r > 0) {
            fdinfo_buf[r] = '\0';
            const char *fp = std::strstr(fdinfo_buf, "flags:");
            if (fp) flags = static_cast<int>(strtol(fp + 6, nullptr, 8));
        }
    }

    /* EV_OPEN extras: {flags, fd, ino, dev_major, dev_minor, err, inherited} */
    int64_t extras[7] = {
        static_cast<int64_t>(flags),
        static_cast<int64_t>(fd_num),
        static_cast<int64_t>(st.st_ino),
        static_cast<int64_t>(major(st.st_dev)),
        static_cast<int64_t>(minor(st.st_dev)),
        0,  /* err */
        1   /* inherited */
    };

    emit_one(EV_OPEN, ts_ns, pid, tgid, ppid, pid, tgid,
             extras, 7, link_target, std::strlen(link_target));
}

static void emit_inherited_open_events(pid_t pid)
{
    pid_t tgid, ppid;
    get_cached_ids(pid, &tgid, &ppid);
    uint64_t ts_ns = get_ts_ns();

    char dir_path[256];
    snprintf(dir_path, sizeof(dir_path), "/proc/%d/fd", static_cast<int>(pid));
    DIR *d = opendir(dir_path);
    if (!d) return;

    struct dirent *ent;
    while ((ent = readdir(d)) != nullptr) {
        if (ent->d_name[0] == '.') continue;
        int fd_num = std::atoi(ent->d_name);
        emit_inherited_open_for_fd(pid, tgid, ppid, ts_ns, fd_num);
    }
    closedir(d);
}

static void emit_open_event(pid_t pid, const char *path, int flags,
                            long fd_or_err)
{
    pid_t tgid, ppid;
    get_cached_ids(pid, &tgid, &ppid);
    uint64_t ts_ns = get_ts_ns();

    /* If successful, get inode info from /proc/pid/fd/N */
    unsigned long ino_nr = 0;
    unsigned int dev_major = 0, dev_minor = 0;
    if (fd_or_err >= 0) {
        char fd_path[256];
        struct stat st;
        snprintf(fd_path, sizeof(fd_path), "/proc/%d/fd/%ld", static_cast<int>(pid), fd_or_err);
        if (fstatat(AT_FDCWD, fd_path, &st, 0) == 0) {
            ino_nr = st.st_ino;
            dev_major = major(st.st_dev);
            dev_minor = minor(st.st_dev);
        }
    }

    /* EV_OPEN extras: {flags, fd, ino, dev_major, dev_minor, err, inherited} */
    int64_t extras[7] = {
        static_cast<int64_t>(flags),
        fd_or_err >= 0 ? static_cast<int64_t>(fd_or_err) : -1,
        static_cast<int64_t>(ino_nr),
        static_cast<int64_t>(dev_major),
        static_cast<int64_t>(dev_minor),
        fd_or_err < 0 ? static_cast<int64_t>(fd_or_err) : 0,  /* err */
        0  /* not inherited */
    };

    const char *path_to_emit = path ? path : "";
    size_t path_len = path ? std::strlen(path) : 0;

    emit_one(EV_OPEN, ts_ns, pid, tgid, ppid, pid, tgid,
             extras, 7, path_to_emit, path_len);
}

static void emit_write_event(pid_t pid, const char *stream,
                             unsigned long buf_addr, size_t count)
{
    pid_t tgid, ppid;
    get_cached_ids(pid, &tgid, &ppid);
    uint64_t ts_ns = get_ts_ns();

    size_t to_read = count;
    if (to_read > WRITE_CAPTURE_MAX) to_read = WRITE_CAPTURE_MAX;

    std::string data(to_read, '\0');
    ssize_t n = read_proc_mem(pid, buf_addr, data.data(), to_read);
    if (n <= 0) return;
    to_read = static_cast<size_t>(n);
    data.resize(to_read);

    /* EV_STDOUT or EV_STDERR */
    int32_t ev = (stream[0] == 'S' && stream[3] == 'E') ? EV_STDERR : EV_STDOUT;

    emit_one(ev, ts_ns, pid, tgid, ppid, pid, tgid,
             nullptr, 0, data.data(), to_read);
}

static void emit_exit_event(pid_t pid, int status)
{
    pid_t tgid, ppid;
    get_cached_ids(pid, &tgid, &ppid);
    uint64_t ts_ns = get_ts_ns();

    /* EV_EXIT extras: {status_kind, code_or_sig, core_dumped, raw} */
    int64_t extras[4];
    if (WIFEXITED(status)) {
        extras[0] = EV_EXIT_EXITED;
        extras[1] = WEXITSTATUS(status);
        extras[2] = 0;
        extras[3] = status;
    } else if (WIFSIGNALED(status)) {
        extras[0] = EV_EXIT_SIGNALED;
        extras[1] = WTERMSIG(status);
#ifdef WCOREDUMP
        extras[2] = WCOREDUMP(status) ? 1 : 0;
#else
        extras[2] = 0;
#endif
        extras[3] = status;
    } else {
        extras[0] = EV_EXIT_EXITED;
        extras[1] = 0;
        extras[2] = 0;
        extras[3] = status;
    }

    emit_one(EV_EXIT, ts_ns, pid, tgid, ppid, pid, tgid,
             extras, 4, nullptr, 0);
}

/* ================================================================
 * Decide whether to capture a write on fd 1 as STDOUT
 * ================================================================ */

static int fd1_is_creator_stdout(pid_t pid)
{
    if (!g_creator_stdout_valid) return 0;
    char link_path[256];
    struct stat st;
    snprintf(link_path, sizeof(link_path), "/proc/%d/fd/1", static_cast<int>(pid));
    if (stat(link_path, &st) < 0) return 0;
    return (st.st_dev == g_creator_stdout_st.st_dev &&
            st.st_ino == g_creator_stdout_st.st_ino);
}

/* ================================================================
 * Syscall handling
 * ================================================================ */

/* Get syscall registers */
#if defined(__x86_64__)
static int get_syscall_info(pid_t pid, long *nr,
    unsigned long *a0, unsigned long *a1, unsigned long *a2,
    unsigned long *a3, long *ret)
{
    struct user_regs_struct regs;
    struct iovec iov = { .iov_base = &regs, .iov_len = sizeof(regs) };
    if (xptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov) < 0)
        return -1;
    *nr  = regs.orig_rax;
    *a0  = regs.rdi;
    *a1  = regs.rsi;
    *a2  = regs.rdx;
    *a3  = regs.r10;
    *ret = regs.rax;
    return 0;
}

static int set_syscall_nr(pid_t pid, long nr)
{
    struct user_regs_struct regs;
    struct iovec iov = { .iov_base = &regs, .iov_len = sizeof(regs) };
    if (xptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov) < 0)
        return -1;
    regs.orig_rax = nr;
    return xptrace(PTRACE_SETREGSET, pid, NT_PRSTATUS, &iov);
}

static int set_syscall_ret(pid_t pid, long ret)
{
    struct user_regs_struct regs;
    struct iovec iov = { .iov_base = &regs, .iov_len = sizeof(regs) };
    if (xptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov) < 0)
        return -1;
    regs.rax = ret;
    return xptrace(PTRACE_SETREGSET, pid, NT_PRSTATUS, &iov);
}

#elif defined(__aarch64__)
struct aarch64_user_regs {
    uint64_t regs[31];
    uint64_t sp;
    uint64_t pc;
    uint64_t pstate;
};
static int get_syscall_info(pid_t pid, long *nr,
    unsigned long *a0, unsigned long *a1, unsigned long *a2,
    unsigned long *a3, long *ret)
{
    struct aarch64_user_regs regs;
    struct iovec iov = { .iov_base = &regs, .iov_len = sizeof(regs) };
    if (xptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov) < 0)
        return -1;
    /* On arm64, the syscall number is in x8, return value in x0 */
    *nr  = regs.regs[8];
    *a0  = regs.regs[0];
    *a1  = regs.regs[1];
    *a2  = regs.regs[2];
    *a3  = regs.regs[3];
    *ret = regs.regs[0];
    return 0;
}

static int set_syscall_nr(pid_t pid, long nr)
{
    struct aarch64_user_regs regs;
    struct iovec iov = { .iov_base = &regs, .iov_len = sizeof(regs) };
    if (xptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov) < 0)
        return -1;
    regs.regs[8] = nr;
    return xptrace(PTRACE_SETREGSET, pid, NT_PRSTATUS, &iov);
}

static int set_syscall_ret(pid_t pid, long ret)
{
    struct aarch64_user_regs regs;
    struct iovec iov = { .iov_base = &regs, .iov_len = sizeof(regs) };
    if (xptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov) < 0)
        return -1;
    regs.regs[0] = ret;
    return xptrace(PTRACE_SETREGSET, pid, NT_PRSTATUS, &iov);
}

#else
/* Fallback — will compile but may not work on other arches */
static int get_syscall_info(pid_t pid, long *nr,
    unsigned long *a0, unsigned long *a1, unsigned long *a2,
    unsigned long *a3, long *ret)
{
    *nr = *a0 = *a1 = *a2 = *a3 = 0;
    *ret = 0;
    return -1;
}
static int set_syscall_nr(pid_t pid, long nr)
{
    (void)pid; (void)nr; return -1;
}
static int set_syscall_ret(pid_t pid, long ret)
{
    (void)pid; (void)ret; return -1;
}
#endif

/* ================================================================
 * Ptrace emulation — data structures
 * ================================================================ */

struct emu_tracee {
    pid_t pid = 0;            /* the sub-tracee */
    pid_t tracer_pid = 0;     /* the sub-tracer that thinks it owns this tracee */
    long  options = 0;        /* PTRACE_O_xxx set by the sub-tracer */
    bool  syscall_stop = false;   /* sub-tracer wants syscall-stops (PTRACE_SYSCALL) */
    bool  stopped = false;        /* sub-tracee held, awaiting sub-tracer resume */
    bool  stop_reported = false;  /* stop was delivered to sub-tracer via wait */
    int   wstatus = 0;        /* synthetic wait-status for sub-tracer */
    long  event_msg = 0;      /* GETEVENTMSG value (new child pid, etc.) */
    int   pending_sig = 0;    /* signal to deliver when sub-tracer resumes */
};

static std::unordered_map<pid_t, emu_tracee> g_emu_tracees;

static emu_tracee *find_emu_tracee(pid_t pid)
{
    auto it = g_emu_tracees.find(pid);
    if (it != g_emu_tracees.end()) return &it->second;
    return nullptr;
}

static emu_tracee *find_emu_tracee_for(pid_t tracer, pid_t tracee)
{
    auto it = g_emu_tracees.find(tracee);
    if (it != g_emu_tracees.end() && it->second.tracer_pid == tracer)
        return &it->second;
    return nullptr;
}

static bool is_emu_tracer(pid_t pid)
{
    for (auto &[k, t] : g_emu_tracees)
        if (t.tracer_pid == pid) return true;
    return false;
}

static emu_tracee *add_emu_tracee(pid_t tracee_pid, pid_t tracer_pid)
{
    auto *existing = find_emu_tracee(tracee_pid);
    if (existing) { existing->tracer_pid = tracer_pid; return existing; }
    auto [it, ok] = g_emu_tracees.try_emplace(tracee_pid);
    it->second.pid = tracee_pid;
    it->second.tracer_pid = tracer_pid;
    return &it->second;
}

static void remove_emu_tracee(pid_t pid)
{
    g_emu_tracees.erase(pid);
}

/* Remove all sub-tracees belonging to a given sub-tracer. */
static void remove_emu_tracees_for(pid_t tracer_pid)
{
    std::erase_if(g_emu_tracees, [tracer_pid](const auto &pair) {
        return pair.second.tracer_pid == tracer_pid;
    });
}

/* Find any un-reported stopped sub-tracee for a given sub-tracer. */
static emu_tracee *find_stopped_for(pid_t tracer, long wait_pid)
{
    for (auto &[k, t] : g_emu_tracees) {
        if (t.tracer_pid != tracer) continue;
        if (!t.stopped || t.stop_reported) continue;
        if (wait_pid == -1 || wait_pid == t.pid ||
            (wait_pid == 0 /* any in same pgid – approximate */))
            return &t;
    }
    return nullptr;
}

/* ================================================================
 * Ptrace emulation — deliver stop to a waiting sub-tracer
 * ================================================================ */

static void try_deliver_to_tracer(pid_t tracer_pid)
{
    proc_state *tps = get_state(tracer_pid);
    if (!tps->emu_waiting) {
#ifdef SYS_wait4
        if (tps->in_syscall && !tps->emu_neutralized &&
            tps->saved_syscall == SYS_wait4) {
            tps->emu_race_interrupt = true;
            syscall(SYS_tgkill, tracer_pid, tracer_pid, SIGCHLD);
        }
#endif
        return;
    }

    emu_tracee *et = find_stopped_for(tracer_pid, tps->emu_wait_pid);
    if (!et) return;

    /* Write wstatus to the sub-tracer's address space */
    if (tps->emu_wait_wstatus_addr) {
        int wst = et->wstatus;
        write_proc_mem(tracer_pid, tps->emu_wait_wstatus_addr, &wst, sizeof(wst));
    }

    /* Set the return value to the stopped sub-tracee's pid */
    set_syscall_ret(tracer_pid, et->pid);

    et->stop_reported = true;
    tps->emu_waiting = false;

    /* Resume the sub-tracer */
    xptrace(PTRACE_SYSCALL, tracer_pid, NULL, 0);
}

/* ================================================================
 * Ptrace emulation — handle emulated xptrace() syscall
 * ================================================================ */

static long emu_handle_ptrace(pid_t caller, unsigned long request,
                              unsigned long pid_arg, unsigned long addr,
                              unsigned long data)
{
    pid_t target = static_cast<pid_t>(pid_arg);

    switch (request) {

    /* ---- PTRACE_TRACEME ---- */
    case PTRACE_TRACEME: {
        pid_t ppid = get_ppid(caller);
        if (ppid <= 0) return -ESRCH;
        add_emu_tracee(caller, ppid);
        return 0;
    }

    /* ---- PTRACE_SETOPTIONS ---- */
    case PTRACE_SETOPTIONS: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et) return -ESRCH;
        et->options = static_cast<long>(data);
        return 0;
    }

    /* ---- PTRACE_SYSCALL / PTRACE_CONT ---- */
    case PTRACE_SYSCALL:
    case PTRACE_CONT: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et) return -ESRCH;
        et->syscall_stop = (request == PTRACE_SYSCALL);
        int sig = static_cast<int>(data);  /* signal to deliver */
        et->stopped = false;
        et->stop_reported = false;
        xptrace(PTRACE_SYSCALL, target, NULL, (void *)(long)sig);
        return 0;
    }

    /* ---- PTRACE_DETACH ---- */
    case PTRACE_DETACH: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et) return -ESRCH;
        int sig = static_cast<int>(data);
        et->stopped = false;
        et->stop_reported = false;
        remove_emu_tracee(target);
        /* Resume normally */
        xptrace(PTRACE_SYSCALL, target, NULL, (void *)(long)sig);
        return 0;
    }

    /* ---- PTRACE_GETEVENTMSG ---- */
    case PTRACE_GETEVENTMSG: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et) return -ESRCH;
        unsigned long msg = static_cast<unsigned long>(et->event_msg);
        if (data)
            write_proc_mem(caller, data, &msg, sizeof(msg));
        return 0;
    }

    /* ---- PTRACE_GETREGS ---- */
    case PTRACE_GETREGS: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et || !et->stopped) return -ESRCH;
#if defined(__x86_64__)
        struct user_regs_struct regs;
        struct iovec iov = { .iov_base = &regs, .iov_len = sizeof(regs) };
        if (xptrace(PTRACE_GETREGSET, target, NT_PRSTATUS, &iov) < 0)
            return -EIO;
        if (data)
            write_proc_mem(caller, data, &regs, sizeof(regs));
#else
        /* On non-x86_64, GETREGS may not be available; fall back. */
        return -EIO;
#endif
        return 0;
    }

    /* ---- PTRACE_SETREGS ---- */
    case PTRACE_SETREGS: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et || !et->stopped) return -ESRCH;
#if defined(__x86_64__)
        struct user_regs_struct regs;
        if (!data) return -EIO;
        if (read_proc_mem(caller, data, &regs, sizeof(regs)) < static_cast<ssize_t>(sizeof(regs)))
            return -EIO;
        struct iovec iov = { .iov_base = &regs, .iov_len = sizeof(regs) };
        if (xptrace(PTRACE_SETREGSET, target, NT_PRSTATUS, &iov) < 0)
            return -EIO;
#else
        return -EIO;
#endif
        return 0;
    }

    /* ---- PTRACE_GETREGSET ---- */
    case PTRACE_GETREGSET: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et || !et->stopped) return -ESRCH;
        if (!data) return -EIO;
        /* Read the iovec from the caller's address space */
        struct iovec caller_iov;
        if (read_proc_mem(caller, data, &caller_iov, sizeof(caller_iov))
                < static_cast<ssize_t>(sizeof(caller_iov)))
            return -EIO;
        /* Allocate a local buffer, do the real ptrace, write back */
        size_t bufsz = caller_iov.iov_len;
        if (bufsz > 4096) bufsz = 4096;
        void *buf = malloc(bufsz);
        if (!buf) return -ENOMEM;
        struct iovec local_iov = { .iov_base = buf, .iov_len = bufsz };
        if (xptrace(PTRACE_GETREGSET, target, addr, &local_iov) < 0) {
            free(buf);
            return -EIO;
        }
        write_proc_mem(caller, reinterpret_cast<unsigned long>(caller_iov.iov_base), buf, local_iov.iov_len);
        /* Update iov_len in caller's iov to reflect actual size */
        caller_iov.iov_len = local_iov.iov_len;
        write_proc_mem(caller, data, &caller_iov, sizeof(caller_iov));
        free(buf);
        return 0;
    }

    /* ---- PTRACE_SETREGSET ---- */
    case PTRACE_SETREGSET: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et || !et->stopped) return -ESRCH;
        if (!data) return -EIO;
        struct iovec caller_iov;
        if (read_proc_mem(caller, data, &caller_iov, sizeof(caller_iov))
                < static_cast<ssize_t>(sizeof(caller_iov)))
            return -EIO;
        size_t bufsz = caller_iov.iov_len;
        if (bufsz > 4096) bufsz = 4096;
        void *buf = malloc(bufsz);
        if (!buf) return -ENOMEM;
        if (read_proc_mem(caller, reinterpret_cast<unsigned long>(caller_iov.iov_base), buf, bufsz)
                < static_cast<ssize_t>(bufsz)) {
            free(buf);
            return -EIO;
        }
        struct iovec local_iov = { .iov_base = buf, .iov_len = bufsz };
        int rc = xptrace(PTRACE_SETREGSET, target, addr, &local_iov) < 0 ? -EIO : 0;
        free(buf);
        return rc;
    }

    /* ---- PTRACE_PEEKDATA / PTRACE_PEEKTEXT ---- */
    case PTRACE_PEEKDATA:
    case PTRACE_PEEKTEXT: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et) return -ESRCH;
        errno = 0;
        long val = xptrace(PTRACE_PEEKDATA, target, (void *)addr, NULL);
        if (errno) return -errno;
        return val;
    }

    /* ---- PTRACE_POKEDATA / PTRACE_POKETEXT ---- */
    case PTRACE_POKEDATA:
    case PTRACE_POKETEXT: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et) return -ESRCH;
        if (xptrace(PTRACE_POKEDATA, target, (void *)addr, (void *)data) < 0)
            return -errno;
        return 0;
    }

    /* ---- PTRACE_ATTACH ---- */
    case PTRACE_ATTACH: {
        /* Only allow attaching to processes we're already tracing */
        if (!pidset_contains(target)) return -EPERM;
        add_emu_tracee(target, caller);
        /* The target will receive a SIGSTOP; we'll queue it. */
        kill(target, SIGSTOP);
        return 0;
    }

    /* ---- PTRACE_KILL ---- */
#ifdef PTRACE_KILL
    case PTRACE_KILL: {
        emu_tracee *et = find_emu_tracee_for(caller, target);
        if (!et) return -ESRCH;
        kill(target, SIGKILL);
        remove_emu_tracee(target);
        return 0;
    }
#endif

    default:
        /* Unsupported request — return EINVAL so the caller can cope */
        return -EINVAL;
    }
}

/* ================================================================
 * Ptrace emulation — handle emulated wait4() syscall
 * ================================================================ */

static int emu_handle_wait4(pid_t caller, proc_state *ps)
{
    long wpid_arg = static_cast<long>(static_cast<int>(ps->arg0)); /* sign-extend pid_t */
    unsigned long wstatus_addr = ps->arg1;
    int options = static_cast<int>(ps->arg2);

    /* Check for an already-stopped sub-tracee */
    emu_tracee *et = find_stopped_for(caller, wpid_arg);
    if (et) {
        if (wstatus_addr) {
            int wst = et->wstatus;
            write_proc_mem(caller, wstatus_addr, &wst, sizeof(wst));
        }
        set_syscall_ret(caller, et->pid);
        et->stop_reported = true;
        return 0; /* resume caller */
    }

    if (options & WNOHANG) {
        set_syscall_ret(caller, 0);
        return 0;
    }

    /* No pending stop and blocking — hold the sub-tracer */
    ps->emu_waiting = true;
    ps->emu_wait_wstatus_addr = wstatus_addr;
    ps->emu_wait_pid = wpid_arg;
    ps->emu_wait_options = options;
    return 1; /* hold caller */
}

/* ================================================================
 * Ptrace emulation — queue a stop for a sub-tracee
 * ================================================================ */

static void emu_queue_stop(emu_tracee *et, int wstatus)
{
    et->stopped = true;
    et->stop_reported = false;
    et->wstatus = wstatus;
    try_deliver_to_tracer(et->tracer_pid);
}

static void handle_syscall_entry(pid_t pid, proc_state *ps)
{
    long nr, ret;
    unsigned long a0, a1, a2, a3;
    if (get_syscall_info(pid, &nr, &a0, &a1, &a2, &a3, &ret) < 0)
        return;

    ps->saved_syscall = nr;
    ps->arg0 = a0;
    ps->arg1 = a1;
    ps->arg2 = a2;
    ps->arg3 = a3;
    ps->emu_neutralized = false;

    /* ---- Intercept xptrace() syscall ---- */
    if (nr == SYS_ptrace) {
        set_syscall_nr(pid, -1);   /* neutralise → kernel returns -ENOSYS */
        ps->emu_neutralized = true;
        return;
    }

    /* ---- Intercept wait4() from sub-tracers ---- */
#ifdef SYS_wait4
    if (nr == SYS_wait4 && is_emu_tracer(pid)) {
        set_syscall_nr(pid, -1);
        ps->emu_neutralized = true;
        return;
    }
#endif
#ifdef SYS_waitid
    if (nr == SYS_waitid && is_emu_tracer(pid)) {
        set_syscall_nr(pid, -1);
        ps->emu_neutralized = true;
        return;
    }
#endif
}

/*
 * handle_syscall_exit — returns 1 if the pid should be held (not resumed).
 */
static int handle_syscall_exit(pid_t pid, proc_state *ps)
{
    long nr, ret_unused;
    unsigned long a0_unused, a1_unused, a2_unused, a3_unused;
    if (get_syscall_info(pid, &nr, &a0_unused, &a1_unused, &a2_unused, &a3_unused, &ret_unused) < 0)
        return 0;

    /* ---- Handle emulated (neutralised) syscalls ---- */
    if (ps->emu_neutralized) {
        if (ps->saved_syscall == SYS_ptrace) {
            long result = emu_handle_ptrace(pid, ps->arg0, ps->arg1,
                                            ps->arg2, ps->arg3);
            set_syscall_ret(pid, result);
            return 0;
        }
#ifdef SYS_wait4
        if (ps->saved_syscall == SYS_wait4) {
            return emu_handle_wait4(pid, ps);
        }
#endif
#ifdef SYS_waitid
        if (ps->saved_syscall == SYS_waitid) {
            /* waitid has different arg layout but approximate with wait4 logic */
            return emu_handle_wait4(pid, ps);
        }
#endif
        return 0;
    }

    /* Race-condition path */
#ifdef SYS_wait4
    if (ps->saved_syscall == SYS_wait4 && is_emu_tracer(pid)) {
        return emu_handle_wait4(pid, ps);
    }
#endif

    long syscall_nr = ps->saved_syscall;
    long ret_val = ret_unused; /* rax on x86_64, x0 on aarch64 */

    /* ---- execve / execveat ---- */
    if (syscall_nr == SYS_execve
#ifdef SYS_execveat
        || syscall_nr == SYS_execveat
#endif
       ) {
        if (ret_val == 0) {
            ps->tgid = pid;
            ps->is_thread = false;
            emit_cwd_event(pid);
            emit_exec_event(pid);
            emit_inherited_open_events(pid);
        }
        return 0;
    }

    /* ---- openat / open ---- */
#ifdef SYS_openat
    if (syscall_nr == SYS_openat) {
        /* a0 = dirfd, a1 = pathname, a2 = flags */
        auto path = read_tracee_string(pid, ps->arg1, PATH_MAX);
        emit_open_event(pid, path ? path->c_str() : nullptr, static_cast<int>(ps->arg2), ret_val);
        return 0;
    }
#endif
#ifdef SYS_open
    if (syscall_nr == SYS_open) {
        /* a0 = pathname, a1 = flags */
        auto path = read_tracee_string(pid, ps->arg0, PATH_MAX);
        emit_open_event(pid, path ? path->c_str() : nullptr, static_cast<int>(ps->arg1), ret_val);
        return 0;
    }
#endif

    /* ---- chdir / fchdir ---- */
    if (syscall_nr == SYS_chdir || syscall_nr == SYS_fchdir) {
        if (ret_val == 0)
            emit_cwd_event(pid);
        return 0;
    }

    /* ---- write ---- */
    if (syscall_nr == SYS_write) {
        unsigned int fd = static_cast<unsigned int>(ps->arg0);
        if (ret_val <= 0) return 0;
        if (fd == 2) {
            emit_write_event(pid, "STDERR", ps->arg1, static_cast<size_t>(ret_val));
        } else if (fd == 1 && fd1_is_creator_stdout(pid)) {
            emit_write_event(pid, "STDOUT", ps->arg1, static_cast<size_t>(ret_val));
        }
        return 0;
    }

    /* ---- writev (for STDERR/STDOUT that goes through writev) ---- */
    if (syscall_nr == SYS_writev) {
        unsigned int fd = static_cast<unsigned int>(ps->arg0);
        if (ret_val <= 0) return 0;
        if (fd != 1 && fd != 2) return 0;
        if (fd == 1 && !fd1_is_creator_stdout(pid)) return 0;
        const char *stream = (fd == 2) ? "STDERR" : "STDOUT";
        (void)stream;
        return 0;
    }

    return 0;
}


/* ================================================================
 * Main tracer loop
 * ================================================================ */

static void usage(const char *prog)
{
    std::fprintf(stderr,
            "Usage: %s [-o FILE[.zst]] [--no-env] -- command [args...]\n",
            prog);
    std::exit(1);
}

static int trace_output_uses_zstd(const char *outfile)
{
    return outfile && path_has_suffix(outfile, ".zst");
}

static int open_trace_output(const char *outfile)
{
    int want_zstd = trace_output_uses_zstd(outfile);

    /* Field-by-field reset (cannot memset because of unique_ptr member) */
    g_out.stream = nullptr;
    g_out.owns_stream = false;
    g_out.error = false;
    g_out.closing = false;
    g_out.compressor_pid = 0;
    g_out.ring.reset();
    g_out.ring_size = TRACE_OUT_RING_SIZE;
    g_out.head = 0;
    g_out.tail = 0;
    g_out.used = 0;
    g_out.writer = {};
    g_out.lock = PTHREAD_MUTEX_INITIALIZER;
    g_out.can_read = PTHREAD_COND_INITIALIZER;
    g_out.can_write = PTHREAD_COND_INITIALIZER;

    g_out.ring = std::make_unique<char[]>(g_out.ring_size);
    if (!g_out.ring) {
        perror("malloc");
        return -1;
    }
    pthread_mutex_init(&g_out.lock, nullptr);
    pthread_cond_init(&g_out.can_read, nullptr);
    pthread_cond_init(&g_out.can_write, nullptr);

    if (outfile && want_zstd) {
        int pipefd[2];
        if (pipe(pipefd) < 0) {
            perror("pipe");
            g_out.ring.reset();
            return -1;
        }
        g_out.compressor_pid = fork();
        if (g_out.compressor_pid < 0) {
            perror("fork");
            close(pipefd[0]);
            close(pipefd[1]);
            g_out.ring.reset();
            return -1;
        }
        if (g_out.compressor_pid == 0) {
            if (dup2(pipefd[0], STDIN_FILENO) < 0) _exit(127);
            close(pipefd[0]);
            close(pipefd[1]);
            execlp("zstd", "zstd", "-q", "-T0", "-f", "-o", outfile, static_cast<char *>(nullptr));
            _exit(127);
        }
        close(pipefd[0]);
        g_out.stream = fdopen(pipefd[1], "wb");
        if (!g_out.stream) {
            perror("fdopen");
            close(pipefd[1]);
            waitpid(g_out.compressor_pid, nullptr, 0);
            g_out.ring.reset();
            return -1;
        }
        g_out.owns_stream = true;
    } else if (outfile) {
        g_out.stream = std::fopen(outfile, "wb");
        if (!g_out.stream) {
            perror("fopen");
            g_out.ring.reset();
            return -1;
        }
        g_out.owns_stream = true;
    } else {
        g_out.stream = stdout;
        g_out.owns_stream = false;
    }
    setvbuf(g_out.stream, nullptr, _IOFBF, TRACE_OUT_RING_SIZE);
    if (pthread_create(&g_out.writer, nullptr, trace_output_writer_main, &g_out) != 0) {
        perror("pthread_create");
        if (g_out.owns_stream) std::fclose(g_out.stream);
        if (g_out.compressor_pid > 0) waitpid(g_out.compressor_pid, nullptr, 0);
        g_out.ring.reset();
        return -1;
    }

    /* Emit TRACE_VERSION as the first thing in the stream */
    uint8_t version_buf[16];
    Dst vd = wire_dst(version_buf, sizeof(version_buf));
    wire_put_u64(&vd, TRACE_VERSION);
    if (vd.p) {
        trace_output_enqueue(reinterpret_cast<const char*>(version_buf),
                           static_cast<size_t>((uint8_t *)vd.p - version_buf));
    }

    return 0;
}

static int close_trace_output(const char *outfile)
{
    int rc = 0;
    if (!g_out.stream) return 0;

    pthread_mutex_lock(&g_out.lock);
    g_out.closing = true;
    pthread_cond_broadcast(&g_out.can_read);
    pthread_mutex_unlock(&g_out.lock);
    pthread_join(g_out.writer, nullptr);

    if (g_out.error) {
        std::fprintf(stderr, "upttrace: trace output failed\n");
        rc = -1;
    }
    if (outfile && g_out.owns_stream && std::fclose(g_out.stream) != 0)
        rc = -1;
    else if (!outfile)
        std::fflush(g_out.stream);
    if (g_out.compressor_pid > 0) {
        int status;
        if (waitpid(g_out.compressor_pid, &status, 0) < 0
            || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
            rc = -1;
    }
    pthread_cond_destroy(&g_out.can_read);
    pthread_cond_destroy(&g_out.can_write);
    pthread_mutex_destroy(&g_out.lock);

    /* Field-by-field reset */
    g_out.ring.reset();
    g_out.stream = nullptr;
    g_out.owns_stream = false;
    g_out.error = false;
    g_out.closing = false;
    g_out.compressor_pid = 0;
    g_out.ring_size = 0;
    g_out.head = 0;
    g_out.tail = 0;
    g_out.used = 0;
    g_out.writer = {};

    return rc;
}


static int run_ptrace_trace(char **cmd, const char *outfile)
{
    if (open_trace_output(outfile) < 0)
        return 1;

    /* Record the creator's stdout inode for STDOUT filtering */
    g_creator_stdout_valid = (fstat(STDOUT_FILENO, &g_creator_stdout_st) == 0);

    /* Fork the child */
    pid_t child = fork();
    if (child < 0) { perror("fork"); std::exit(1); }

    if (child == 0) {
        /* Child: request tracing, then exec */
        if (xptrace(PTRACE_TRACEME, 0, NULL, NULL) < 0) {
            perror("xptrace(TRACEME)");
            _exit(127);
        }
        raise(SIGSTOP); /* Wait for parent to set options */
        execvp(cmd[0], cmd);
        perror("execvp");
        _exit(127);
    }

    /* Parent: wait for child's initial SIGSTOP */
    int status;
    waitpid(child, &status, 0);
    if (!WIFSTOPPED(status)) {
        std::fprintf(stderr, "upttrace: child did not stop\n");
        std::exit(1);
    }

    /* Set ptrace options to track forks and execs */
    long opts = PTRACE_O_TRACESYSGOOD   /* set bit 7 in signal for syscall stops */
              | PTRACE_O_TRACEFORK
              | PTRACE_O_TRACEVFORK
              | PTRACE_O_TRACECLONE
              | PTRACE_O_TRACEEXEC;
    if (xptrace(PTRACE_SETOPTIONS, child, NULL, opts) < 0) {
        perror("xptrace(SETOPTIONS)");
        std::exit(1);
    }

    /* Track the child */
    pidset_add(child);

    /* Resume child (it will immediately hit execve) */
    xptrace(PTRACE_SYSCALL, child, NULL, 0);

    /* Main event loop */
    while (!g_tracked.empty()) {
        int wstatus;
        pid_t wpid = waitpid(-1, &wstatus, __WALL);
        if (wpid < 0) {
            if (errno == EINTR) continue;
            if (errno == ECHILD) break;
            break;
        }

        if (WIFEXITED(wstatus) || WIFSIGNALED(wstatus)) {
            /* Process/thread exited */
            if (pidset_contains(wpid)) {
                proc_state *eps = get_state(wpid);
                if (!eps->is_thread)
                    emit_exit_event(wpid, wstatus);
                pidset_remove(wpid);
                free_state(wpid);
            }
            /* If this was a sub-tracee, queue the exit for its sub-tracer */
            emu_tracee *et = find_emu_tracee(wpid);
            if (et) {
                emu_queue_stop(et, wstatus);
            }
            /* If this was a sub-tracer, release all its sub-tracees */
            remove_emu_tracees_for(wpid);
            continue;
        }

        if (!WIFSTOPPED(wstatus)) continue;

        int sig = WSTOPSIG(wstatus);
        int event = static_cast<unsigned>(wstatus) >> 16;

        /* Handle ptrace events (fork/vfork/clone/exec) */
        if (event == PTRACE_EVENT_FORK ||
            event == PTRACE_EVENT_VFORK ||
            event == PTRACE_EVENT_CLONE) {
            unsigned long new_pid;
            xptrace(PTRACE_GETEVENTMSG, wpid, NULL, &new_pid);
            if (new_pid > 0) {
                pidset_add(static_cast<pid_t>(new_pid));
            }

            emu_tracee *et = find_emu_tracee(wpid);
            if (et) {
                int want = 0;
                if (event == PTRACE_EVENT_FORK && (et->options & PTRACE_O_TRACEFORK)) want = 1;
                if (event == PTRACE_EVENT_VFORK && (et->options & PTRACE_O_TRACEVFORK)) want = 1;
                if (event == PTRACE_EVENT_CLONE && (et->options & PTRACE_O_TRACECLONE)) want = 1;

                if (want && new_pid > 0) {
                    emu_tracee *net = add_emu_tracee(static_cast<pid_t>(new_pid), et->tracer_pid);
                    if (net) {
                        net->options = et->options;
                        net->syscall_stop = et->syscall_stop;
                    }
                }

                if (want) {
                    et->event_msg = static_cast<long>(new_pid);
                    int emu_wstatus = (SIGTRAP << 8) | (event << 16) | 0x7f;
                    emu_queue_stop(et, emu_wstatus);
                    continue;
                }
            }

            xptrace(PTRACE_SYSCALL, wpid, NULL, 0);
            continue;
        }

        if (event == PTRACE_EVENT_EXEC) {
            emu_tracee *et = find_emu_tracee(wpid);
            if (et && (et->options & PTRACE_O_TRACEEXEC)) {
                et->event_msg = static_cast<long>(wpid);
                int emu_wstatus = (SIGTRAP << 8) | (PTRACE_EVENT_EXEC << 16) | 0x7f;
                emu_queue_stop(et, emu_wstatus);
                continue;  /* hold sub-tracee */
            }
            xptrace(PTRACE_SYSCALL, wpid, NULL, 0);
            continue;
        }

        /* Syscall stop (bit 7 set in signal from PTRACE_O_TRACESYSGOOD) */
        if (sig == (SIGTRAP | 0x80)) {
            proc_state *ps = get_state(wpid);
            int hold = 0;
            if (!ps->in_syscall) {
                /* Syscall entry */
                ps->in_syscall = true;
                handle_syscall_entry(wpid, ps);
            } else {
                /* Syscall exit */
                ps->in_syscall = false;
                hold = handle_syscall_exit(wpid, ps);
            }

            if (!hold) {
                emu_tracee *et = find_emu_tracee(wpid);
                if (et && et->syscall_stop) {
                    int ss = (et->options & PTRACE_O_TRACESYSGOOD)
                             ? (SIGTRAP | 0x80) : SIGTRAP;
                    int emu_wstatus = (ss << 8) | 0x7f;
                    emu_queue_stop(et, emu_wstatus);
                    hold = 1;
                }
            }

            if (!hold)
                xptrace(PTRACE_SYSCALL, wpid, NULL, 0);
            continue;
        }

        /* PTRACE_EVENT_STOP for newly traced processes */
        if (sig == SIGSTOP && event == 0 && pidset_contains(wpid)) {
            emu_tracee *et = find_emu_tracee(wpid);
            if (et) {
                int emu_wstatus = (SIGSTOP << 8) | 0x7f;
                emu_queue_stop(et, emu_wstatus);
                continue;  /* hold sub-tracee */
            }
            xptrace(PTRACE_SYSCALL, wpid, NULL, 0);
            continue;
        }

        /* Group stop / PTRACE_INTERRUPT stop */
        if (event == PTRACE_EVENT_STOP) {
            if (sig == SIGTRAP) {
                xptrace(PTRACE_SYSCALL, wpid, NULL, 0);
            } else {
                xptrace(PTRACE_LISTEN, wpid, NULL, 0);
            }
            continue;
        }

        /* Signal delivery */
        {
            proc_state *sigps = get_state(wpid);
            if (sig == SIGCHLD && sigps->emu_race_interrupt) {
                sigps->emu_race_interrupt = false;
                xptrace(PTRACE_SYSCALL, wpid, NULL, 0);
                continue;
            }

            emu_tracee *et = find_emu_tracee(wpid);
            if (et) {
                et->pending_sig = sig;
                int emu_wstatus = (sig << 8) | 0x7f;
                emu_queue_stop(et, emu_wstatus);
                continue;  /* hold sub-tracee until sub-tracer resumes */
            }
        }

        /* Deliver the signal to the tracee */
        xptrace(PTRACE_SYSCALL, wpid, NULL, (void *)(long)sig);
    }

    if (close_trace_output(outfile) != 0)
        return 1;

    /* Clean up */
    g_tracked.clear();
    g_states.clear();
    g_emu_tracees.clear();

    return 0;
}


int main(int argc, char **argv)
{
    const char *outfile = nullptr;
    int cmd_start = -1;

    for (int i = 1; i < argc; i++) {
        std::string_view arg(argv[i]);
        if (arg == "--") {
            cmd_start = i + 1;
            break;
        }
        if (arg == "-o" && i + 1 < argc) {
            outfile = argv[++i];
        } else if (arg == "--no-env") {
            g_trace_exec_env = 0;
        } else if (arg == "-h" || arg == "--help") {
            usage(argv[0]);
        } else {
            cmd_start = i;
            break;
        }
    }

    if (cmd_start < 0 || cmd_start >= argc)
        usage(argv[0]);

    return run_ptrace_trace(argv + cmd_start, outfile);
}
