/* trace/trace.h — tv's TRACE format, on top of the atom encoding in
 * wire/wire.h.
 *
 * Self-contained C99. Includes from
 *   - the kernel module (mod/proctrace.c),
 *   - freestanding sud helpers (sud32/sud64; no libc),
 *   - userspace tracers (upt/upttrace, C++),
 *   - the dump tool (tools/wiredump),
 *   - the trace ingest in tv (trace/trace_stream.cpp),
 *   - tests.
 *
 * Depends on wire/ only.
 *
 * ── Stream layout ──────────────────────────────────────────────────
 * The stream is a flat sequence of atoms. The first atom is
 *   wire_put_u64(TRACE_VERSION)
 * everything after is one outer atom per event.
 *
 * No packet boundaries. No record-end markers. No back-patching.
 * Producers never seek.
 *
 * ── Event layout ───────────────────────────────────────────────────
 * Each event is one outer atom. Its payload is two inner atoms:
 *
 *     outer_atom { hdr_atom || blob_atom }
 *
 * where:
 *
 *   hdr_atom  payload =
 *     wire_put_u64(stream_id)
 *     wire_put_i64(type   - prev.type)
 *     wire_put_i64(ts_ns  - prev.ts_ns)
 *     wire_put_i64(pid    - prev.pid)
 *     wire_put_i64(tgid   - prev.tgid)
 *     wire_put_i64(ppid   - prev.ppid)
 *     wire_put_i64(nspid  - prev.nspid)
 *     wire_put_i64(nstgid - prev.nstgid)
 *     [type-specific extras: wire_put_i64 each]
 *
 *   blob_atom payload = the (possibly empty) opaque payload bytes
 *
 * Extras are NOT delta-encoded: each is one wire_put_i64. They carry
 * the type-specific fixed fields:
 *   EV_EXIT : { status, code_or_signal, core_dumped, raw }       (4)
 *   EV_OPEN : { flags, fd, ino, dev_major, dev_minor, err, inh } (7)
 *   others  : (none)
 *
 * Each producer process keeps one ev_state per stream_id (in practice
 * one per process), updated step-for-step by the encoder. The decoder
 * keeps one ev_state per observed stream_id and applies the same
 * deltas. Stream id is whatever the producer chooses; single-producer
 * tracers use 1.
 *
 * Strings are bytes. The path / argv / env / stdout / stderr blobs
 * are whatever bytes the kernel handed us, NUL-included. There is no
 * encoding contract anywhere in this format.
 */

#ifndef TRACE_H
#define TRACE_H

#include "wire/wire.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Version atom written as the first thing in any stream. Every
 * consumer reads this first and refuses unknown versions. Bump on any
 * byte-level change. */
#define TRACE_VERSION 3u

/* Event class codes. Stable within one TRACE_VERSION. */
enum {
    EV_EXEC   = 0,  /* blob = exe path bytes              */
    EV_ARGV   = 1,  /* blob = NUL-separated argv bytes    */
    EV_ENV    = 2,  /* blob = NUL-separated envp bytes    */
    EV_AUXV   = 3,  /* blob = raw Elf*_auxv_t[] bytes     */
    EV_EXIT   = 4,  /* no blob; 4 i64 extras              */
    EV_OPEN   = 5,  /* blob = path bytes; 7 i64 extras    */
    EV_CWD    = 6,  /* blob = path bytes                  */
    EV_STDOUT = 7,  /* blob = stdout bytes                */
    EV_STDERR = 8,  /* blob = stderr bytes                */
};

/* exit.status values */
enum {
    EV_EXIT_EXITED   = 0,
    EV_EXIT_SIGNALED = 1,
};

/* Per-stream delta state. Initialise to all zeros. Encoder and
 * decoder keep one of these; values must match step-for-step. */
typedef struct {
    int64_t type;
    int64_t ts_ns;
    int64_t pid;
    int64_t tgid;
    int64_t ppid;
    int64_t nspid;
    int64_t nstgid;
} ev_state;

/* Comfortable upper bound on the bytes ev_build_header writes:
 * 1 stream_id u64 (≤9) + 7 base scalars + up to 7 extras (largest
 * event is EV_OPEN), each at most 9 bytes. */
#define EV_HEADER_MAX 192u

/* Build one event's inline header into `*d`, delta-encoded against
 * `*st`, then commit the new values into `*st`.
 *
 * `extras`/`n_extras` are appended after the seven base scalars and
 * are NOT delta-encoded — each is one wire_put_i64.
 *
 * Caller then composes the event with
 *   wire_put_pair(out, hdr_src, blob_src)
 * where blob_src/blob_len is the optional payload bytes (path, argv,
 * stdout bytes, …) — passed through verbatim. */
static inline void ev_build_header(ev_state *st, Dst *d,
                                   uint32_t stream_id,
                                   int32_t type,
                                   uint64_t ts_ns,
                                   int32_t pid, int32_t tgid, int32_t ppid,
                                   int32_t nspid, int32_t nstgid,
                                   const int64_t *extras,
                                   unsigned n_extras) {
    wire_put_u64(d, (uint64_t)stream_id);

    int64_t n_type   = type;
    int64_t n_ts     = (int64_t)ts_ns;
    int64_t n_pid    = pid;
    int64_t n_tgid   = tgid;
    int64_t n_ppid   = ppid;
    int64_t n_nspid  = nspid;
    int64_t n_nstgid = nstgid;

    wire_put_i64(d, n_type   - st->type);
    wire_put_i64(d, n_ts     - st->ts_ns);
    wire_put_i64(d, n_pid    - st->pid);
    wire_put_i64(d, n_tgid   - st->tgid);
    wire_put_i64(d, n_ppid   - st->ppid);
    wire_put_i64(d, n_nspid  - st->nspid);
    wire_put_i64(d, n_nstgid - st->nstgid);
    for (unsigned i = 0; i < n_extras; i++) {
        wire_put_i64(d, extras[i]);
    }

    /* On error (Dst::p went NULL) we still commit the delta state —
     * the encoder will be torn down by the caller anyway. Committing
     * unconditionally keeps the encode path branch-free. */
    st->type   = n_type;
    st->ts_ns  = n_ts;
    st->pid    = n_pid;
    st->tgid   = n_tgid;
    st->ppid   = n_ppid;
    st->nspid  = n_nspid;
    st->nstgid = n_nstgid;
}

/* Decode the stream_id and seven base scalars from `*src`, commit
 * them into `*st`, and advance `*src` past those atoms. Caller
 * continues parsing extras from the same `Src`, then takes the
 * trailing bytes as the blob. */
static inline void ev_decode_header(ev_state *st, Src *src, WireErr *err,
                                    uint32_t *stream_id,
                                    int32_t *type, uint64_t *ts_ns,
                                    int32_t *pid, int32_t *tgid, int32_t *ppid,
                                    int32_t *nspid, int32_t *nstgid) {
    uint64_t sid = wire_get_u64(src, err);
    *stream_id = (uint32_t)sid;
    int64_t d;
    d = wire_get_i64(src, err); st->type   += d; *type   = (int32_t)st->type;
    d = wire_get_i64(src, err); st->ts_ns  += d; *ts_ns  = (uint64_t)st->ts_ns;
    d = wire_get_i64(src, err); st->pid    += d; *pid    = (int32_t)st->pid;
    d = wire_get_i64(src, err); st->tgid   += d; *tgid   = (int32_t)st->tgid;
    d = wire_get_i64(src, err); st->ppid   += d; *ppid   = (int32_t)st->ppid;
    d = wire_get_i64(src, err); st->nspid  += d; *nspid  = (int32_t)st->nspid;
    d = wire_get_i64(src, err); st->nstgid += d; *nstgid = (int32_t)st->nstgid;
}

#ifdef __cplusplus
}  /* extern "C" */
#endif

#endif /* TRACE_H */
