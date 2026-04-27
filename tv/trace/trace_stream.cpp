/* trace/trace_stream.cpp — implementation of TraceDecoder. */

#include "trace/trace_stream.h"
#include "trace/trace.h"

#include <cstring>
#include <unordered_map>
#include <vector>

struct TraceDecoder::Impl {
    Sink sink;
    std::vector<uint8_t> buf;            /* unconsumed bytes carried across feeds */
    std::unordered_map<uint32_t, ev_state> states;
    bool got_version = false;
    bool error = false;
    bool any_input = false;

    explicit Impl(Sink s) : sink(std::move(s)) {}

    /* Try to consume one whole atom from `buf` starting at offset
     * `pos`. Returns the number of bytes consumed, 0 if not enough
     * data, or (size_t)-1 on hard error. */
    size_t consume_one(size_t pos) {
        if (error) return (size_t)-1;
        const uint8_t *base = buf.data() + pos;
        size_t avail = buf.size() - pos;
        if (avail == 0) return 0;

        Src in = wire_src(base, avail);
        WireErr err = WIRE_OK;

        if (!got_version) {
            uint64_t v = wire_get_u64(&in, &err);
            if (err == WIRE_ERR_TRUNC) return 0;
            if (err != WIRE_OK || v != TRACE_VERSION) {
                error = true; return (size_t)-1;
            }
            got_version = true;
            return (size_t)(in.p - base);
        }

        Src outer = wire_get(&in, &err);
        if (err == WIRE_ERR_TRUNC) return 0;
        if (err != WIRE_OK) { error = true; return (size_t)-1; }

        /* Two inner atoms: header, blob. */
        WireErr ierr = WIRE_OK;
        Src hdr  = wire_get(&outer, &ierr);
        Src blob = wire_get(&outer, &ierr);
        if (ierr != WIRE_OK) { error = true; return (size_t)-1; }

        uint32_t stream_id;
        int32_t type, pid, tgid, ppid, nspid, nstgid;
        uint64_t ts_ns;

        /* Header begins with stream_id. Peek (then rewind) so we
         * know which ev_state to look up before applying deltas. */
        WireErr derr = WIRE_OK;
        Src hdr_for_peek = hdr;
        uint64_t sid64 = wire_get_u64(&hdr_for_peek, &derr);
        if (derr != WIRE_OK) { error = true; return (size_t)-1; }
        stream_id = (uint32_t)sid64;

        ev_state &st = states[stream_id];
        ev_state snapshot = st;
        ev_decode_header(&st, &hdr, &derr, &stream_id,
                         &type, &ts_ns, &pid, &tgid, &ppid, &nspid, &nstgid);
        if (derr != WIRE_OK) { st = snapshot; error = true; return (size_t)-1; }

        int64_t extras[7] = {0};
        unsigned n_extras = 0;
        switch (type) {
            case EV_EXIT: n_extras = 4; break;
            case EV_OPEN: n_extras = 7; break;
            default:      n_extras = 0; break;
        }
        for (unsigned i = 0; i < n_extras; i++) {
            extras[i] = wire_get_i64(&hdr, &derr);
        }
        if (derr != WIRE_OK) { st = snapshot; error = true; return (size_t)-1; }

        TraceEvent ev{};
        ev.type      = type;
        ev.ts_ns     = ts_ns;
        ev.pid       = pid;
        ev.tgid      = tgid;
        ev.ppid      = ppid;
        ev.nspid     = nspid;
        ev.nstgid    = nstgid;
        ev.stream_id = stream_id;
        ev.extras    = extras;
        ev.n_extras  = n_extras;
        ev.blob      = (const char *)blob.p;
        ev.blen      = (size_t)wire_src_len(blob);
        if (sink) sink(ev);
        return (size_t)(in.p - base);
    }
};

TraceDecoder::TraceDecoder(Sink sink) : impl_(std::make_unique<Impl>(std::move(sink))) {}
TraceDecoder::~TraceDecoder() = default;

bool TraceDecoder::feed(const void *data, size_t n) {
    if (n == 0) return !impl_->error;
    impl_->any_input = true;
    impl_->buf.insert(impl_->buf.end(),
                      (const uint8_t *)data, (const uint8_t *)data + n);
    /* Walk the buffer with a cursor; erase consumed bytes only once
     * at the end. Linear in bytes added even when called byte-at-a-
     * time across many events. */
    size_t pos = 0;
    while (pos < impl_->buf.size()) {
        if (impl_->error) return false;
        size_t k = impl_->consume_one(pos);
        if (k == (size_t)-1) return false;
        if (k == 0) break;
        pos += k;
    }
    if (pos > 0) {
        impl_->buf.erase(impl_->buf.begin(),
                         impl_->buf.begin() + (ptrdiff_t)pos);
    }
    return !impl_->error;
}

bool TraceDecoder::started() const {
    return impl_->any_input;
}
