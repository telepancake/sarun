/* tests.cpp - tv self-tests.
 *
 * Run with `tv --test`. Strictly black-box: build a trace byte stream
 * in-memory, feed it through the same TraceDecoder + TvDb path the
 * real ingest uses, then assert that SQL queries against the resulting
 * database return what we expect.
 */

#include "trace/trace_stream.h"
#include "tv_db.h"

extern "C" {
#include "trace/trace.h"
}

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace {

int g_fail = 0;
int g_pass = 0;

#define EXPECT(cond) do { \
    if (cond) { g_pass++; } \
    else { g_fail++; std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); } \
} while (0)

#define EXPECT_EQ(a, b) do { \
    auto _a = (a); auto _b = (b); \
    if (_a == _b) { g_pass++; } \
    else { g_fail++; std::fprintf(stderr, "FAIL %s:%d: %s == %s (left=%lld right=%lld)\n", \
        __FILE__, __LINE__, #a, #b, (long long)_a, (long long)_b); } \
} while (0)

#define EXPECT_STR(a, b) do { \
    std::string _a = (a); std::string _b = (b); \
    if (_a == _b) { g_pass++; } \
    else { g_fail++; std::fprintf(stderr, "FAIL %s:%d: %s == %s (left=\"%s\" right=\"%s\")\n", \
        __FILE__, __LINE__, #a, #b, _a.c_str(), _b.c_str()); } \
} while (0)

/* -- tiny trace-stream builder -------------------------------------- */

struct TraceBuilder {
    std::vector<uint8_t> buf;
    ev_state st{};
    uint32_t stream_id = 1;
    bool wrote_version = false;

    void version() {
        if (wrote_version) return;
        uint8_t pre[16]; Dst d = wire_dst(pre, sizeof pre);
        wire_put_u64(&d, TRACE_VERSION);
        if (!d.p) std::abort();
        buf.insert(buf.end(), pre, d.p);
        wrote_version = true;
    }

    void event(int32_t type, uint64_t ts_ns, int32_t pid, int32_t tgid,
               int32_t ppid, const int64_t *extras, unsigned n_extras,
               const void *blob, size_t blen) {
        version();
        uint8_t hdrbuf[EV_HEADER_MAX];
        Dst hd = wire_dst(hdrbuf, sizeof hdrbuf);
        ev_build_header(&st, &hd, stream_id, type, ts_ns,
                        pid, tgid, ppid,
                        pid /*nspid*/, tgid /*nstgid*/,
                        extras, n_extras);
        if (!hd.p) { std::fprintf(stderr, "ev_build_header failed\n"); std::abort(); }
        size_t hlen = (size_t)(hd.p - hdrbuf);

        size_t cap_extra = 2 * WIRE_PREFIX_MAX + hlen + blen + 16;
        size_t cur = buf.size();
        buf.resize(cur + cap_extra);
        Dst od = wire_dst(buf.data() + cur, cap_extra);
        wire_put_pair(&od,
                      wire_src(hdrbuf, hlen),
                      wire_src(blob, blen));
        if (!od.p) { std::fprintf(stderr, "wire_put_pair failed\n"); std::abort(); }
        buf.resize((size_t)(od.p - buf.data()));
    }
};

/* -- fixture: a tiny synthetic trace -------------------------------- */

void build_fixture(TraceBuilder &w) {
    /* Process 100: make all */
    w.event(EV_EXEC, 1000, /*pid*/100, /*tgid*/100, /*ppid*/1, nullptr, 0,
            "/usr/bin/make", 13);
    w.event(EV_ARGV, 1000, 100, 100, 1, nullptr, 0, "make\0all\0", 9);
    w.event(EV_CWD,  1000, 100, 100, 1, nullptr, 0, "/home/user", 10);
    int64_t open_ex[7] = {0 /*flags*/, 3, 0, 0, 0, 0, 0};
    w.event(EV_OPEN, 1100, 100, 100, 1, open_ex, 7, "/etc/passwd", 11);
    int64_t open_ex2[7] = {0101 /*O_WRONLY|O_CREAT*/, 4, 0, 0, 0, 0, 0};
    w.event(EV_OPEN, 1200, 100, 100, 1, open_ex2, 7, "/tmp/out", 8);
    int64_t ex[4] = {EV_EXIT_EXITED, 0, 0, 0};
    w.event(EV_EXIT, 2000, 100, 100, 1, ex, 4, nullptr, 0);

    /* Process 200: failing child of 100 */
    w.event(EV_EXEC, 1500, 200, 200, 100, nullptr, 0, "/bin/false", 10);
    w.event(EV_ARGV, 1500, 200, 200, 100, nullptr, 0, "false\0", 6);
    int64_t ex2[4] = {EV_EXIT_EXITED, 1, 0, 256};
    w.event(EV_EXIT, 1600, 200, 200, 100, ex2, 4, nullptr, 0);
}

/* -- tests ----------------------------------------------------------- */

void test_trace_decoder_parses_all_events() {
    TraceBuilder w;
    build_fixture(w);

    int counts[16] = {0};
    TraceDecoder dec([&](const TraceEvent &ev) {
        if (ev.type >= 0 && ev.type < 16) counts[ev.type]++;
    });
    EXPECT(dec.feed(w.buf.data(), w.buf.size()));
    EXPECT_EQ(counts[EV_EXEC], 2);
    EXPECT_EQ(counts[EV_ARGV], 2);
    EXPECT_EQ(counts[EV_CWD],  1);
    EXPECT_EQ(counts[EV_OPEN], 2);
    EXPECT_EQ(counts[EV_EXIT], 2);
}

void test_trace_decoder_byte_at_a_time() {
    TraceBuilder w;
    build_fixture(w);
    int total = 0;
    TraceDecoder dec([&](const TraceEvent &) { total++; });
    for (size_t i = 0; i < w.buf.size(); i++) {
        EXPECT(dec.feed(&w.buf[i], 1));
    }
    /* 2 EXEC + 2 ARGV + 1 CWD + 2 OPEN + 2 EXIT = 9 */
    EXPECT_EQ(total, 9);
}

void test_trace_decoder_rejects_wrong_version() {
    /* Hand-craft a stream with a bad version. */
    uint8_t bad[2];
    Dst d = wire_dst(bad, sizeof bad);
    wire_put_u64(&d, 99); /* bad version */
    int evs = 0;
    TraceDecoder dec([&](const TraceEvent &){ evs++; });
    EXPECT(!dec.feed(bad, (size_t)(d.p - bad)));
    EXPECT_EQ(evs, 0);
}

/* Two producers (stream_id 1 and stream_id 2) emit interleaved events
 * into the same output. The decoder must keep one ev_state per
 * stream_id; if it accidentally shares state, the deltas desync and
 * the second stream's pid/tgid/ts come out wrong. */
void test_trace_decoder_multi_stream() {
    std::vector<uint8_t> buf;
    {
        uint8_t v[2]; Dst d = wire_dst(v, sizeof v);
        wire_put_u64(&d, TRACE_VERSION);
        if (!d.p) std::abort();
        buf.insert(buf.end(), v, d.p);
    }
    auto emit = [&](uint32_t sid, ev_state *st, int32_t type, uint64_t ts,
                    int32_t pid, int32_t tgid, int32_t ppid,
                    const int64_t *extras, unsigned n_extras,
                    const void *blob, size_t blen) {
        uint8_t hdr[EV_HEADER_MAX];
        Dst hd = wire_dst(hdr, sizeof hdr);
        ev_build_header(st, &hd, sid, type, ts,
                        pid, tgid, ppid, pid, tgid,
                        extras, n_extras);
        if (!hd.p) std::abort();
        size_t hlen = (size_t)(hd.p - hdr);

        std::vector<uint8_t> ev(hlen + blen + 2 * WIRE_PREFIX_MAX + 16);
        Dst d = wire_dst(ev.data(), ev.size());
        wire_put_pair(&d, wire_src(hdr, hlen), wire_src(blob, blen));
        if (!d.p) std::abort();
        buf.insert(buf.end(), ev.data(), d.p);
    };

    ev_state st1{}, st2{};
    emit(1, &st1, EV_EXEC, 1000, 100, 100, 1, nullptr, 0, "/bin/sh", 7);
    emit(2, &st2, EV_EXEC, 2000, 500, 500, 1, nullptr, 0, "/bin/cat", 8);
    emit(1, &st1, EV_EXEC, 1100, 101, 101, 100, nullptr, 0, "/bin/ls", 7);
    emit(2, &st2, EV_EXEC, 2100, 501, 501, 500, nullptr, 0, "/bin/awk", 8);
    int64_t ex[4] = {EV_EXIT_EXITED, 0, 0, 0};
    emit(1, &st1, EV_EXIT, 1200, 100, 100, 1,   ex, 4, nullptr, 0);
    emit(2, &st2, EV_EXIT, 2200, 500, 500, 1,   ex, 4, nullptr, 0);

    struct Got { uint32_t sid; int32_t pid; int32_t type; uint64_t ts; };
    std::vector<Got> got;
    TraceDecoder dec([&](const TraceEvent &ev) {
        got.push_back({ev.stream_id, ev.pid, ev.type, ev.ts_ns});
    });
    EXPECT(dec.feed(buf.data(), buf.size()));
    EXPECT_EQ((size_t)6, got.size());
    if (got.size() == 6) {
        EXPECT_EQ((int)got[0].sid, 1); EXPECT_EQ((int)got[0].pid, 100);
        EXPECT_EQ((int)got[1].sid, 2); EXPECT_EQ((int)got[1].pid, 500);
        EXPECT_EQ((int)got[2].sid, 1); EXPECT_EQ((int)got[2].pid, 101);
        EXPECT_EQ((int)got[3].sid, 2); EXPECT_EQ((int)got[3].pid, 501);
        EXPECT_EQ((int)got[4].sid, 1); EXPECT_EQ((int)got[4].pid, 100);
        EXPECT_EQ((int)got[5].sid, 2); EXPECT_EQ((int)got[5].pid, 500);
        EXPECT_EQ((long long)got[0].ts, 1000LL);
        EXPECT_EQ((long long)got[1].ts, 2000LL);
        EXPECT_EQ((long long)got[5].ts, 2200LL);
    }
}

void test_tvdb_ingest_and_query() {
    std::string err;
    auto db = TvDb::open_memory(&err);
    EXPECT(db != nullptr);
    if (!db) { std::fprintf(stderr, "open_memory: %s\n", err.c_str()); return; }

    TraceBuilder w;
    build_fixture(w);
    TraceDecoder dec([&](const TraceEvent &ev) {
        std::string e; (void)db->append(ev, &e);
    });
    EXPECT(dec.feed(w.buf.data(), w.buf.size()));
    EXPECT(db->flush(&err));

    auto exec_n = db->query_int64("SELECT COUNT(*) FROM exec", &err);
    EXPECT_EQ((int64_t)2, exec_n.empty() ? -1 : exec_n[0]);
    auto open_n = db->query_int64("SELECT COUNT(*) FROM open_", &err);
    EXPECT_EQ((int64_t)2, open_n.empty() ? -1 : open_n[0]);
    auto exit_n = db->query_int64("SELECT COUNT(*) FROM exit_", &err);
    EXPECT_EQ((int64_t)2, exit_n.empty() ? -1 : exit_n[0]);
    auto argv_n = db->query_int64("SELECT COUNT(*) FROM argv", &err);
    EXPECT_EQ((int64_t)3, argv_n.empty() ? -1 : argv_n[0]);

    auto exes = db->query_strings(
        "SELECT exe FROM exec WHERE tgid = 100", &err);
    EXPECT_EQ((size_t)1, exes.size());
    if (!exes.empty()) EXPECT_STR(exes[0][0], "/usr/bin/make");

    auto codes = db->query_int64(
        "SELECT code_or_sig FROM exit_ WHERE tgid = 200", &err);
    EXPECT_EQ((int64_t)1, codes.empty() ? -1 : codes[0]);

    auto wopens = db->query_strings(
        "SELECT path FROM open_ WHERE tgid = 100 AND (flags % 4) != 0", &err);
    EXPECT_EQ((size_t)1, wopens.size());
    if (!wopens.empty()) EXPECT_STR(wopens[0][0], "/tmp/out");

    EXPECT_EQ((int64_t)10, db->total_event_count());
}

void test_tvdb_persists_across_open() {
    std::string err;
    char path[64]; std::snprintf(path, sizeof path, "/tmp/tv_test_%d.tvdb",
                                 (int)::getpid());
    ::unlink(path);
    {
        auto db = TvDb::open_file(path, &err);
        EXPECT(db != nullptr);
        if (!db) return;
        TraceBuilder w; build_fixture(w);
        TraceDecoder dec([&](const TraceEvent &ev){
            std::string e; (void)db->append(ev, &e);
        });
        dec.feed(w.buf.data(), w.buf.size());
        db->flush(&err);
    }
    {
        auto db = TvDb::open_file(path, &err);
        EXPECT(db != nullptr);
        if (!db) { ::unlink(path); return; }
        EXPECT_EQ((int64_t)10, db->total_event_count());
    }
    ::unlink(path);
}

void test_data_source_mode1_query() {
    std::string err;
    auto db = TvDb::open_memory(&err);
    if (!db) { std::fprintf(stderr, "open_memory: %s\n", err.c_str()); g_fail++; return; }
    TraceBuilder w; build_fixture(w);
    TraceDecoder dec([&](const TraceEvent &ev){ std::string e; (void)db->append(ev, &e); });
    dec.feed(w.buf.data(), w.buf.size());
    db->flush(&err);

    auto rows = db->query_strings(
        "SELECT p.tgid FROM exec p GROUP BY p.tgid ORDER BY MIN(p.ts_ns)", &err);
    EXPECT_EQ((size_t)2, rows.size());
    if (rows.size() == 2) {
        EXPECT_STR(rows[0][0], "100");
        EXPECT_STR(rows[1][0], "200");
    }
}

void test_proc_index_built_and_persisted() {
    std::string err;
    char path[64]; std::snprintf(path, sizeof path, "/tmp/tv_idx_%d.tvdb",
                                 (int)::getpid());
    ::unlink(path);
    {
        auto db = TvDb::open_file(path, &err);
        if (!db) { ::unlink(path); g_fail++; return; }
        TraceBuilder w; build_fixture(w);
        TraceDecoder dec([&](const TraceEvent &ev){ std::string e; (void)db->append(ev, &e); });
        dec.feed(w.buf.data(), w.buf.size());
        db->flush(&err);
        EXPECT(db->ensure_proc_index(&err));
        if (!err.empty()) std::fprintf(stderr, "ensure_proc_index: %s\n", err.c_str());

        auto rows = db->query_strings(
            "SELECT tgid, ppid, CAST(exe AS VARCHAR), exit_code "
            "FROM tv_idx_proc ORDER BY tgid", &err);
        EXPECT_EQ((size_t)2, rows.size());
        if (rows.size() == 2) {
            EXPECT_STR(rows[0][0], "100");
            EXPECT_STR(rows[0][1], "1");
            EXPECT_STR(rows[0][2], "/usr/bin/make");
            EXPECT_STR(rows[1][0], "200");
            EXPECT_STR(rows[1][1], "100");
            EXPECT_STR(rows[1][2], "/bin/false");
            EXPECT_STR(rows[1][3], "1");
        }

        EXPECT(db->ensure_proc_index(&err));
        auto meta = db->query_strings(
            "SELECT value FROM tv_meta WHERE key='idx_proc'", &err);
        EXPECT_EQ((size_t)1, meta.size());
    }
    {
        auto db = TvDb::open_file(path, &err);
        if (!db) { ::unlink(path); g_fail++; return; }
        auto rows = db->query_int64("SELECT COUNT(*) FROM tv_idx_proc", &err);
        EXPECT_EQ((int64_t)2, rows.empty() ? -1 : rows[0]);
    }
    ::unlink(path);
}

void test_path_index_summary() {
    std::string err;
    auto db = TvDb::open_memory(&err);
    if (!db) { g_fail++; return; }
    TraceBuilder w; build_fixture(w);
    TraceDecoder dec([&](const TraceEvent &ev){ std::string e; (void)db->append(ev, &e); });
    dec.feed(w.buf.data(), w.buf.size());
    db->flush(&err);
    EXPECT(db->ensure_path_index(&err));
    if (!err.empty()) std::fprintf(stderr, "ensure_path_index: %s\n", err.c_str());

    auto rows = db->query_strings(
        "SELECT path, opens, reads, writes "
        "FROM tv_idx_path ORDER BY path", &err);
    EXPECT_EQ((size_t)2, rows.size());
    if (rows.size() == 2) {
        EXPECT_STR(rows[0][0], "/etc/passwd");
        EXPECT_STR(rows[0][1], "1");
        EXPECT_STR(rows[0][2], "1");
        EXPECT_STR(rows[0][3], "0");
        EXPECT_STR(rows[1][0], "/tmp/out");
        EXPECT_STR(rows[1][3], "1");
    }
}

void test_edge_index_dep_closure() {
    std::string err;
    auto db = TvDb::open_memory(&err);
    if (!db) { g_fail++; return; }
    TraceBuilder w; build_fixture(w);
    TraceDecoder dec([&](const TraceEvent &ev){ std::string e; (void)db->append(ev, &e); });
    dec.feed(w.buf.data(), w.buf.size());
    db->flush(&err);
    EXPECT(db->ensure_edge_index(&err));

    auto rows = db->query_strings(
        "WITH RECURSIVE closure(path, depth) AS ("
        "  SELECT '/tmp/out', 0"
        "  UNION "
        "  SELECT e2.path, c.depth + 1 "
        "  FROM closure c "
        "  JOIN tv_idx_edge e1 ON e1.path = c.path AND e1.mode = 1 "
        "  JOIN tv_idx_edge e2 ON e2.tgid = e1.tgid AND e2.mode = 0 "
        "                    AND e2.path <> c.path "
        "  WHERE c.depth < 8 "
        ") SELECT DISTINCT path FROM closure ORDER BY path", &err);
    EXPECT(rows.size() >= 2);
    bool saw_passwd = false;
    for (auto &r : rows) if (r[0] == "/etc/passwd") saw_passwd = true;
    EXPECT(saw_passwd);
}

} /* namespace */

int run_tests() {
    test_trace_decoder_parses_all_events();
    test_trace_decoder_byte_at_a_time();
    test_trace_decoder_rejects_wrong_version();
    test_trace_decoder_multi_stream();
    test_tvdb_ingest_and_query();
    test_tvdb_persists_across_open();
    test_data_source_mode1_query();
    test_proc_index_built_and_persisted();
    test_path_index_summary();
    test_edge_index_dep_closure();

    std::fprintf(stderr, "tv tests: %d passed, %d failed\n", g_pass, g_fail);
    return g_fail == 0 ? 0 : 1;
}
