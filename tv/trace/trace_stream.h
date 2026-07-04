/* trace/trace_stream.h — streaming decoder for tv's TRACE format.
 *
 * Producers (sud, mod, upt) emit binary trace events as defined in
 * trace/trace.h. tv consumes them through this adapter.
 *
 * The decoder is byte-streaming — feed() is safe with any fragment
 * size, including a single byte at a time from a non-blocking pipe.
 * Each decoded event is delivered to the sink as a TraceEvent (a POD
 * view into the decoder's internal buffer; valid for the duration of
 * the sink call).
 */
#pragma once

#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>

/* One decoded trace event. Pointers are valid only for the duration
 * of the sink call. The `type` is the EV_* code from trace/trace.h.
 * `extras`/`n_extras` are type-specific i64 fields (4 for EV_EXIT,
 * 7 for EV_OPEN, 0 otherwise). `blob`/`blen` is the variable-length
 * payload (may be empty / nullptr). */
struct TraceEvent {
    int32_t  type;
    uint64_t ts_ns;
    int32_t  pid, tgid, ppid, nspid, nstgid;
    uint32_t stream_id;
    const int64_t *extras;
    unsigned       n_extras;
    const char *blob;
    size_t      blen;
};

class TraceDecoder {
public:
    using Sink = std::function<void(const TraceEvent &)>;

    explicit TraceDecoder(Sink sink);
    ~TraceDecoder();
    TraceDecoder(const TraceDecoder &) = delete;
    TraceDecoder &operator=(const TraceDecoder &) = delete;

    /* Append `n` bytes of trace input. Decoded events are passed to
     * the sink as they complete. Bytes that don't form a whole event
     * are buffered until the next call. Returns false on a hard
     * format error (unsupported version, malformed atom). */
    bool feed(const void *data, size_t n);

    /* True if any byte has been fed since construction. */
    bool started() const;

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};
