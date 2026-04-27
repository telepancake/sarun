/*
 * sud/trace/event.h — Wire-format event emission for sudtrace.
 *
 * Emits events in the trace format defined by trace/trace.h.
 * All emit_* functions are async-signal-safe: raw syscalls only,
 * static buffers, no cross-process locking. Each process owns its
 * own ev_state and stream_id (handed out from a shared atomic
 * counter at sud_wire_init time), so emits are lock-free.
 */

#ifndef SUD_EVENT_H
#define SUD_EVENT_H

#include "libc-fs/libc.h"
#include "sud/state.h"

/* ================================================================
 * Constants
 * ================================================================ */
#define WRITE_CAPTURE_MAX    4096
#define ARGV_MAX_READ        32768
#define ENV_MAX_READ         65536

/* Reserve two high FDs so children are unlikely to clobber them.
 *   SUD_OUTPUT_FD : the wire output file.
 *   SUD_STATE_FD  : a MAP_SHARED anonymous page holding the
 *                   atomic stream-id counter. No lock anywhere. */
#define SUD_OUTPUT_FD        1023
#define SUD_STATE_FD         1022
#define SUDTRACE_OUTFILE_ENV "SUDTRACE_OUTFILE"

/* ================================================================
 * Extern globals
 * ================================================================ */
extern int        g_out_fd;
extern stat_buf_t g_creator_stdout_stbuf;
extern int        g_creator_stdout_valid;

/* ================================================================
 * Wire setup — must be called once per process after g_out_fd is set.
 *
 * sud_wire_init():
 *   Map SUD_STATE_FD as MAP_SHARED so this process can read the
 *   shared atomic stream-id counter, then atomically allocate this
 *   process's own stream_id. ev_state is process-local and zeroed.
 *   Falls back to a process-local counter if the FD isn't set up
 *   (stand-alone wrapper runs).
 * ================================================================ */
void sud_wire_init(void);

/* Reset and re-init the wire stream after fork.
 *
 * Must be called from the child process after a fork (or non-CLONE_VM
 * clone) so that the child gets its own stream_id from the shared
 * counter and starts the per-stream delta encoder fresh. Without this,
 * parent and child both emit events with the same stream_id and the
 * decoder produces garbage deltas. */
void sud_wire_postfork(void);

/* ================================================================
 * Proc helpers
 * ================================================================ */
ssize_t read_proc_raw(pid_t pid, const char *name, char *buf, size_t bufsz);
char   *read_proc_exe(pid_t pid, char *buf, size_t bufsz);
char   *read_proc_cwd(pid_t pid, char *buf, size_t bufsz);
pid_t   get_ppid(pid_t pid);
pid_t   get_tgid(pid_t pid);

/* ================================================================
 * Event emission (public API — callers unchanged from the JSONL era)
 * ================================================================ */
void emit_cwd_event(pid_t pid);
void emit_exec_event(pid_t pid, const char *fallback_exe,
                     int fallback_argc, char **fallback_argv);
void emit_inherited_open_for_fd(pid_t pid, pid_t tgid, pid_t ppid,
                                struct timespec *ts, int fd_num);
void emit_inherited_open_events(pid_t pid);
void emit_open_event(pid_t pid, const char *path, int flags, long fd_or_err);
void emit_unlink_event(pid_t pid, const char *path, long ret);
void emit_write_event(pid_t pid, const char *stream,
                      const void *data_buf, size_t count);
void emit_exit_event(pid_t pid, int status);

/* ================================================================
 * STDOUT filtering
 * ================================================================ */
int fd1_is_creator_stdout(pid_t pid);

#endif /* SUD_EVENT_H */
