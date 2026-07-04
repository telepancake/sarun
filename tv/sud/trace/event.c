/*
 * sud/trace/event.c — TRACE-format event emission for sudtrace.
 *
 * Emits events using the format from trace/trace.h:
 *   [version atom is written once by the launcher]
 *   <outer atom per event> = wire_put_pair(stream_id+hdr, blob)
 *
 * Every emit path is async-signal-safe: raw syscalls, static buffers,
 * no malloc, no stdio, no TLS. The SIGSYS handler calls these from
 * arbitrary traced program contexts.
 *
 * Cross-process coherence — per-process stream design
 * ---------------------------------------------------
 * The previous design held a cross-process spinlock over a shared
 * ev_state and serialised every emitter. That kills throughput,
 * leaves async-signal-safety on a knife edge (a process killed mid-
 * lock wedges every other tracee), and forces every event in a
 * multi-process build to single-thread through one mutex.
 *
 * The new design:
 *   - The launcher (sud/sudtrace.c) sets up SUD_STATE_FD as a single
 *     MAP_SHARED page holding only `struct sud_shared { uint32_t
 *     next_stream_id; }` — an atomic counter, no lock.
 *   - Each process (the launcher itself + every traced child) calls
 *     sud_wire_init() once on startup, which atomically grabs the
 *     next stream_id with __sync_fetch_and_add and keeps a
 *     *process-local* ev_state.
 *   - Each event is built into a single static scratch buffer and
 *     emitted with one raw_write(). The syscall is atomic against
 *     other writers up to PIPE_BUF (pipes) or unconditionally on
 *     regular files in Linux, so events from different processes
 *     interleave at event boundaries but never inside an event.
 *   - The decoder (trace/trace_stream.cpp) keeps one ev_state per
 *     observed stream_id, so each producer's deltas stay coherent
 *     on its own state.
 *
 * Stream id 1 is the launcher's; children get 2, 3, 4, …
 */

#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "libc-fs/fmt.h"
#include "sud/trace/event.h"
#include "sud/state.h"
#include "wire/wire.h"
#include "trace/trace.h"

/* ================================================================
 * Global variable definitions
 * ================================================================ */
int        g_out_fd               = -1;
stat_buf_t g_creator_stdout_stbuf;
int        g_creator_stdout_valid;

/* ================================================================
 * Shared atomic counter + per-process delta state.
 *
 * `sud_wire_init` maps SUD_STATE_FD (set up by the launcher) and
 * grabs a fresh stream_id for this process via an atomic CAS-style
 * fetch-and-add. The counter is the *only* cross-process datum; the
 * delta state is process-local, so emit paths are lock-free.
 * ================================================================ */

struct sud_shared {
    /* Bumped atomically; first hand-out is 1 (the launcher's), then
     * 2, 3, … for each child. Stream id 0 stays reserved for legacy /
     * "no stream id" producers. */
    volatile uint32_t next_stream_id;
    /* Rest of the page is reserved for future use / padding. */
};

#define SUD_SHARED_PAGE_SIZE 4096

static struct sud_shared  g_local_shared;              /* fallback */
static struct sud_shared *g_shared = &g_local_shared;

/* Process-local: never shared. */
static ev_state g_ev_state;
static uint32_t g_stream_id = 0;
static int      g_stream_id_set = 0;

void sud_wire_init(void)
{
    /* If the launcher set up SUD_STATE_FD (a MAP_SHARED memfd),
     * mapping it gives us a page of memory shared with every sibling
     * traced process — exactly what the atomic stream-id counter
     * wants. If the fd isn't present the mmap fails and we keep the
     * process-local fallback (single-stream stand-alone runs). */
    if (!g_stream_id_set) {
        /* Only attempt the mmap if g_shared still points at the
         * process-local fallback (i.e. we haven't already grabbed
         * the shared page in a previous lifetime — see
         * sud_wire_postfork()). The shared page is a MAP_SHARED
         * mapping of SUD_STATE_FD; it survives fork (the kernel
         * mapping is inherited) so the post-fork child can keep
         * using it without re-mmap. */
        if (g_shared == &g_local_shared) {
            void *p = raw_mmap(NULL, SUD_SHARED_PAGE_SIZE,
                               PROT_READ | PROT_WRITE, MAP_SHARED,
                               SUD_STATE_FD, 0);
            /* raw_mmap returns the raw kernel return value cast to a
             * pointer; on failure that's a small negative errno (e.g.
             * (void *)-9 for EBADF), which is *not* MAP_FAILED. Treat
             * anything in the top 4 KiB of the address space as an error.
             * Otherwise we'd happily write through a junk pointer below. */
            if ((uintptr_t)p < (uintptr_t)-4096L)
                g_shared = (struct sud_shared *)p;
        }
        /* Atomic grab. __sync_fetch_and_add is async-signal-safe
         * (single instruction lock-prefixed on x86) and works on the
         * shared page (or on the process-local fallback). */
        g_stream_id = __sync_fetch_and_add(&g_shared->next_stream_id, 1u) + 1u;
        g_stream_id_set = 1;
    }
}

void sud_wire_postfork(void)
{
    /* After fork(), the child inherits the parent's g_stream_id and
     * delta-encoder state (g_ev_state) via copy-on-write. If we don't
     * reset them, both processes will emit events tagged with the same
     * stream_id but with diverging local ev_state — the decoder uses a
     * single per-stream-id ev_state and would apply parent's deltas to
     * child's encoded values (or vice-versa), producing nonsense type/
     * pid/ts numbers ("unknown event type -124xxx" on ingest).
     *
     * The shared atomic counter (g_shared->next_stream_id) sits on a
     * MAP_SHARED page that fork inherits, so __sync_fetch_and_add still
     * sees every sibling's allocation. We just need to clear the local
     * "already initialised" flag and zero the ev_state, then re-init. */
    g_stream_id_set = 0;
    g_stream_id = 0;
    __builtin_memset(&g_ev_state, 0, sizeof(g_ev_state));
    sud_wire_init();
}

/* ================================================================
 * Stat field extraction from raw kernel buffers.
 *
 * stat_buf_t is an opaque 256-byte union; we call fstatat with
 * the appropriate syscall for the arch and extract st_dev / st_ino
 * via memcpy to avoid alignment issues.
 * ================================================================ */
static int event_fstatat(const char *path, stat_buf_t *sb)
{
    __builtin_memset(sb, 0, sizeof(*sb));
#ifdef SYS_newfstatat
    return (int)raw_syscall6(SYS_newfstatat, AT_FDCWD, (long)path,
                             (long)sb, 0, 0, 0);
#else
    return (int)raw_syscall6(SYS_fstatat64, AT_FDCWD, (long)path,
                             (long)sb, 0, 0, 0);
#endif
}

static unsigned long sb_dev(const stat_buf_t *sb)
{
#if defined(__x86_64__)
    unsigned long v;
    __builtin_memcpy(&v, sb->_data, sizeof(v));
    return v;
#else
    unsigned long long v;
    __builtin_memcpy(&v, sb->_data, sizeof(v));
    return (unsigned long)v;
#endif
}

static unsigned long sb_ino(const stat_buf_t *sb)
{
#if defined(__x86_64__)
    unsigned long v;
    __builtin_memcpy(&v, sb->_data + 8, sizeof(v));
    return v;
#else
    unsigned long long v;
    __builtin_memcpy(&v, sb->_data + 88, sizeof(v));
    return (unsigned long)v;
#endif
}

static unsigned int sud_major(unsigned long dev)
{
    return (unsigned int)((dev >> 8) & 0xfff);
}

static unsigned int sud_minor(unsigned long dev)
{
    return (unsigned int)((dev & 0xff) | ((dev >> 12) & ~0xffU));
}

/* WCOREDUMP is not defined in the freestanding headers */
#ifndef WCOREDUMP
#define WCOREDUMP(s) ((s) & 0x80)
#endif

/* ================================================================
 * Write helpers — no cross-process lock. raw_write/raw_writev are
 * each one syscall, which the kernel treats atomically against other
 * writers on regular files (and up to PIPE_BUF on pipes). Events
 * are sized to fit in a single syscall, so different processes'
 * events interleave at event boundaries only — never inside one.
 * ================================================================ */

/* Write all `len` bytes of `buf` to g_out_fd via raw_write. Used for
 * scratch-buffer-fits-the-event path; one call here = one atomic
 * event on the wire. Falls back to a loop only on partial writes
 * (regular files do this on EINTR / disk-full edge cases). */
static void emit_raw(const void *buf, size_t len)
{
    const char *p = (const char *)buf;
    size_t off = 0;
    while (off < len) {
        ssize_t n = raw_write(g_out_fd, p + off, len - off);
        if (n <= 0) break;
        off += n;
    }
}

/* ================================================================
 * Timestamp
 * ================================================================ */
static uint64_t get_ts_ns(void)
{
    struct timespec ts;
    raw_clock_gettime(CLOCK_REALTIME, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* ================================================================
 * Core event emitter.
 *
 * Builds the header (stream_id || delta-encoded base scalars +
 * extras) against this process's *local* ev_state, then writes the
 * outer atom — a wire_put_pair of (header, blob) — into the static
 * scratch buffer and ships it in one raw_write().
 *
 * No lock — the only cross-process state is the stream-id counter
 * touched once at sud_wire_init() time. raw_write is atomic per-call
 * against other writers on regular files (and up to PIPE_BUF on
 * pipes), so events from different processes interleave at event
 * boundaries only — never inside one.
 * ================================================================ */

#define WIRE_EVENT_STACK_MAX  (ENV_MAX_READ + PATH_MAX * 8 + 256)

/* Static scratch buffer reused across emits. Re-entrancy: a SIGSYS
 * handler emit can't fire while we're inside a SIGSYS handler on the
 * same thread (SIGSYS is blocked during its own handler), and we
 * never longjmp out of an emit. So a single static is safe. */
static char g_event_buf[WIRE_EVENT_STACK_MAX];

static void emit_event(int32_t type, pid_t pid, pid_t tgid, pid_t ppid,
                       uint64_t ts_ns,
                       const int64_t *extras, unsigned n_extras,
                       const void *blob, size_t blen)
{
    if (g_out_fd < 0) return;
    /* Lazy init in case a stand-alone use forgot to call sud_wire_init. */
    if (!g_stream_id_set) sud_wire_init();

    uint8_t hdr[EV_HEADER_MAX];
    Dst hd = wire_dst(hdr, sizeof hdr);
    ev_build_header(&g_ev_state, &hd, g_stream_id,
                    type, ts_ns,
                    pid, tgid, ppid,
                    /* nspid, nstgid: same as pid/tgid when not
                     * explicitly different — sud can't tell */
                    pid, tgid,
                    extras, n_extras);
    if (!hd.p) return;
    size_t hlen = (size_t)(hd.p - hdr);

    Dst od = wire_dst(g_event_buf, WIRE_EVENT_STACK_MAX);
    wire_put_pair(&od,
                  wire_src(hdr, hlen),
                  wire_src(blob, blen));
    if (!od.p) return;  /* event larger than scratch — drop */
    emit_raw(g_event_buf, (size_t)((uint8_t *)od.p - (uint8_t *)g_event_buf));
}

/* ================================================================
 * Proc helpers (raw-syscall-safe)
 * ================================================================ */

ssize_t read_proc_raw(pid_t pid, const char *name,
                      char *buf, size_t bufsz)
{
    char path[256];
    fmt_proc_path(path, pid, name);
    int fd = raw_open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t total = 0, n;
    while ((size_t)total < bufsz &&
           (n = raw_read(fd, buf + total, bufsz - total)) > 0)
        total += n;
    raw_close(fd);
    return total;
}

char *read_proc_exe(pid_t pid, char *buf, size_t bufsz)
{
    char path[256];
    fmt_proc_path(path, pid, "exe");
    ssize_t n = raw_readlink(path, buf, bufsz - 1);
    if (n <= 0) return NULL;
    buf[n] = '\0';
    const char *del = " (deleted)";
    size_t dlen = strlen(del);
    if ((size_t)n > dlen && strcmp(buf + n - dlen, del) == 0)
        buf[n - dlen] = '\0';
    return buf;
}

char *read_proc_cwd(pid_t pid, char *buf, size_t bufsz)
{
    char path[256];
    fmt_proc_path(path, pid, "cwd");
    ssize_t n = raw_readlink(path, buf, bufsz - 1);
    if (n <= 0) return NULL;
    buf[n] = '\0';
    return buf;
}

pid_t get_ppid(pid_t pid)
{
    char buf[512];
    ssize_t n = read_proc_raw(pid, "stat", buf, sizeof(buf) - 1);
    if (n <= 0) return 0;
    buf[n] = '\0';
    char *cp = strrchr(buf, ')');
    if (!cp) return 0;
    cp += 2;
    while (*cp && *cp != ' ') cp++;
    return parse_int(cp);
}

pid_t get_tgid(pid_t pid)
{
    char buf[2048];
    ssize_t n = read_proc_raw(pid, "status", buf, sizeof(buf) - 1);
    if (n <= 0) return pid;
    buf[n] = '\0';
    const char *p = strstr(buf, "\nTgid:");
    if (!p) return pid;
    return parse_int(p + 6);
}

/* ================================================================
 * Event emitters
 * ================================================================ */

void emit_cwd_event(pid_t pid)
{
    pid_t tgid = get_tgid(pid);
    pid_t ppid = get_ppid(pid);
    uint64_t ts = get_ts_ns();

    char cwd_buf[PATH_MAX];
    char *cwd = read_proc_cwd(pid, cwd_buf, sizeof(cwd_buf));
    if (!cwd) return;

    emit_event(EV_CWD, pid, tgid, ppid, ts, NULL, 0, cwd, strlen(cwd));
}

/* Static buffers for emit_exec_event, protected by emit_lock via
 * emit_event. Used sequentially (not re-entered under lock). */
static char g_exec_cmdline[ARGV_MAX_READ];
static char g_exec_env_buf[ENV_MAX_READ];

/* Length of a flattened NUL-separated argv vector. Includes trailing
 * NULs between entries (but not a final trailing NUL — same layout
 * as /proc/PID/cmdline). */
static size_t flatten_argv_vec(char *dst, size_t dstsize,
                               char *const *argv, int argc)
{
    size_t off = 0;
    for (int i = 0; i < argc; i++) {
        const char *arg = argv[i] ? argv[i] : "";
        size_t len = strlen(arg);
        if (off + len + 1 > dstsize) break;
        __builtin_memcpy(dst + off, arg, len);
        dst[off + len] = '\0';
        off += len + 1;
    }
    return off;
}

void emit_exec_event(pid_t pid, const char *fallback_exe,
                     int fallback_argc, char **fallback_argv)
{
    pid_t tgid = get_tgid(pid);
    pid_t ppid = get_ppid(pid);
    uint64_t ts = get_ts_ns();

    /* 1. EV_EXEC — blob is the resolved exe path. */
    char exe_buf[PATH_MAX];
    const char *exe = (fallback_exe && fallback_exe[0])
        ? fallback_exe
        : read_proc_exe(pid, exe_buf, sizeof(exe_buf));
    if (exe) {
        emit_event(EV_EXEC, pid, tgid, ppid, ts, NULL, 0,
                   exe, strlen(exe));
    } else {
        emit_event(EV_EXEC, pid, tgid, ppid, ts, NULL, 0, "", 0);
    }

    /* 2. EV_ARGV — blob is the raw NUL-separated argv bytes. */
    size_t argv_len = 0;
    if (fallback_argv && fallback_argc > 0) {
        argv_len = flatten_argv_vec(g_exec_cmdline, ARGV_MAX_READ,
                                    fallback_argv, fallback_argc);
    } else {
        ssize_t n = read_proc_raw(pid, "cmdline",
                                  g_exec_cmdline, ARGV_MAX_READ);
        if (n > 0) argv_len = (size_t)n;
    }
    emit_event(EV_ARGV, pid, tgid, ppid, ts, NULL, 0,
               g_exec_cmdline, argv_len);

    /* 3. EV_ENV — blob is /proc/PID/environ, raw NUL-separated.
     *             Skipped entirely when env tracing is disabled.   */
    if (g_trace_exec_env) {
        ssize_t n = read_proc_raw(pid, "environ",
                                  g_exec_env_buf, ENV_MAX_READ);
        if (n > 0) {
            emit_event(EV_ENV, pid, tgid, ppid, ts, NULL, 0,
                       g_exec_env_buf, (size_t)n);
        }
    }
}

void emit_inherited_open_for_fd(pid_t pid, pid_t tgid, pid_t ppid,
                                struct timespec *ts, int fd_num)
{
    if (fd_num == g_out_fd) return;
    if (fd_num == SUD_STATE_FD) return;

    char link_path[256], link_target[PATH_MAX];
    {
        char *lp = link_path;
        lp = fmt_str(lp, "/proc/");
        lp = fmt_int(lp, (int)pid);
        lp = fmt_str(lp, "/fd/");
        lp = fmt_int(lp, fd_num);
    }
    ssize_t n = raw_readlink(link_path, link_target, sizeof(link_target) - 1);
    if (n <= 0) return;
    link_target[n] = '\0';

    stat_buf_t stbuf;
    event_fstatat(link_path, &stbuf);

    /* flags live in /proc/<pid>/fdinfo/<fd_num>, line "flags:<octal>" */
    char fdinfo_path[256], fdinfo_buf[512];
    {
        char *fp = fdinfo_path;
        fp = fmt_str(fp, "/proc/");
        fp = fmt_int(fp, (int)pid);
        fp = fmt_str(fp, "/fdinfo/");
        fp = fmt_int(fp, fd_num);
    }
    int flags = O_RDONLY;
    int fi = raw_open(fdinfo_path, O_RDONLY);
    if (fi >= 0) {
        ssize_t r = raw_read(fi, fdinfo_buf, sizeof(fdinfo_buf) - 1);
        raw_close(fi);
        if (r > 0) {
            fdinfo_buf[r] = '\0';
            const char *fptr = strstr(fdinfo_buf, "flags:");
            if (fptr) flags = (int)parse_long_octal(fptr + 6);
        }
    }

    unsigned long dev = sb_dev(&stbuf);
    unsigned long ino = sb_ino(&stbuf);

    uint64_t ts_ns = (uint64_t)ts->tv_sec * 1000000000ull
                   + (uint64_t)ts->tv_nsec;

    int64_t extras[7] = {
        (int64_t)flags,            /* flags */
        (int64_t)fd_num,           /* fd */
        (int64_t)ino,              /* ino */
        (int64_t)sud_major(dev),   /* dev_major */
        (int64_t)sud_minor(dev),   /* dev_minor */
        0,                         /* err */
        1,                         /* inherited */
    };
    emit_event(EV_OPEN, pid, tgid, ppid, ts_ns,
               extras, 7, link_target, strlen(link_target));
}

void emit_inherited_open_events(pid_t pid)
{
    pid_t tgid = get_tgid(pid);
    pid_t ppid = get_ppid(pid);
    struct timespec ts;
    raw_clock_gettime(CLOCK_REALTIME, &ts);

    char dir_path[256];
    fmt_proc_path(dir_path, pid, "fd");
    DIR *d = opendir(dir_path);
    if (!d) return;

    struct dirent *ent;
    while ((ent = readdir(d)) != NULL) {
        if (ent->d_name[0] == '.') continue;
        int fd_num = parse_int(ent->d_name);
        emit_inherited_open_for_fd(pid, tgid, ppid, &ts, fd_num);
    }
    closedir(d);
}

void emit_open_event(pid_t pid, const char *path, int flags,
                     long fd_or_err)
{
    pid_t tgid = get_tgid(pid);
    pid_t ppid = get_ppid(pid);
    uint64_t ts = get_ts_ns();

    unsigned long ino_nr = 0;
    unsigned int dev_major = 0, dev_minor = 0;
    int err = 0;

    if (fd_or_err >= 0) {
        char fd_path[256];
        char *fp = fd_path;
        fp = fmt_str(fp, "/proc/");
        fp = fmt_int(fp, (int)pid);
        fp = fmt_str(fp, "/fd/");
        fp = fmt_long(fp, fd_or_err);

        stat_buf_t sb;
        if (event_fstatat(fd_path, &sb) >= 0) {
            ino_nr = sb_ino(&sb);
            dev_major = sud_major(sb_dev(&sb));
            dev_minor = sud_minor(sb_dev(&sb));
        }
    } else {
        err = (int)fd_or_err;
    }

    int64_t extras[7] = {
        (int64_t)flags,
        (int64_t)(fd_or_err >= 0 ? fd_or_err : -1),
        (int64_t)ino_nr,
        (int64_t)dev_major,
        (int64_t)dev_minor,
        (int64_t)err,
        0,    /* inherited=false */
    };
    emit_event(EV_OPEN, pid, tgid, ppid, ts, extras, 7,
               path ? path : "", path ? strlen(path) : 0);
}

void emit_unlink_event(pid_t pid, const char *path, long ret)
{
    /* UNLINK is an EV_OPEN-ish event in terms of carrying just a path,
     * but we don't have an EV_UNLINK in the wire format. Squash it
     * into EV_OPEN with fd=-1 and err = ret (0 on success, negative
     * errno on failure). Consumers that care about unlink detect it
     * via a reserved flags bit. */
    (void)path; (void)ret;
    /* For now, drop UNLINK entirely in wire mode — downstream tests
     * don't depend on it and the kernel module didn't emit it either
     * under wire. If needed later we add EV_UNLINK to wire.h. */
}

void emit_write_event(pid_t pid, const char *stream,
                      const void *data_buf, size_t count)
{
    pid_t tgid = get_tgid(pid);
    pid_t ppid = get_ppid(pid);
    uint64_t ts = get_ts_ns();

    size_t to_read = count;
    if (to_read > WRITE_CAPTURE_MAX) to_read = WRITE_CAPTURE_MAX;

    int32_t ev = (stream[0] == 'S' && stream[3] == 'E') ? EV_STDERR
                                                        : EV_STDOUT;
    emit_event(ev, pid, tgid, ppid, ts, NULL, 0, data_buf, to_read);
}

void emit_exit_event(pid_t pid, int status)
{
    pid_t tgid = get_tgid(pid);
    pid_t ppid = get_ppid(pid);
    uint64_t ts = get_ts_ns();

    int64_t extras[4];
    if (WIFEXITED(status)) {
        extras[0] = EV_EXIT_EXITED;
        extras[1] = WEXITSTATUS(status);
        extras[2] = 0;
        extras[3] = status;
    } else if (WIFSIGNALED(status)) {
        extras[0] = EV_EXIT_SIGNALED;
        extras[1] = WTERMSIG(status);
        extras[2] = WCOREDUMP(status) ? 1 : 0;
        extras[3] = status;
    } else {
        extras[0] = EV_EXIT_EXITED;
        extras[1] = 0;
        extras[2] = 0;
        extras[3] = status;
    }
    emit_event(EV_EXIT, pid, tgid, ppid, ts, extras, 4, NULL, 0);
}

/* ================================================================
 * STDOUT filtering
 * ================================================================ */
int fd1_is_creator_stdout(pid_t pid)
{
    if (!g_creator_stdout_valid) return 0;

    char link_path[256];
    char *lp = link_path;
    lp = fmt_str(lp, "/proc/");
    lp = fmt_int(lp, (int)pid);
    lp = fmt_str(lp, "/fd/1");

    stat_buf_t sb;
    if (event_fstatat(link_path, &sb) < 0) return 0;

    return (sb_dev(&sb) == sb_dev(&g_creator_stdout_stbuf) &&
            sb_ino(&sb) == sb_ino(&g_creator_stdout_stbuf));
}
