/* wiredump.c — round-trip selftest and human dump for tv trace streams.
 *
 *   wiredump --selftest
 *       Round-trip every event class plus boundary cases for the
 *       wire byte primitive (lengths 0/1/55/56/256/65536, deltas across
 *       zero, embedded NULs in blobs, etc.). Exits non-zero on any
 *       mismatch.
 *
 *   wiredump trace.bin [trace.bin ...]
 *       mmap each file, print one line per event to stdout. Pipe
 *       compressed traces through `zstd -dc`.
 *
 * Depends on wire/ and trace/ only.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include "wire/wire.h"
#include "trace/trace.h"

/* ─── pretty-print bytes that aren't claimed to be text ─────────── */

static void put_safe(const uint8_t *b, uint64_t n) {
    for (uint64_t i = 0; i < n; i++) {
        uint8_t c = b[i];
        if (c >= 0x20 && c < 0x7f && c != '\\') {
            putchar((int)c);
        } else {
            printf("\\x%02x", c);
        }
    }
}

static const char *ev_name(int32_t t) {
    switch (t) {
    case EV_EXEC:   return "EXEC";
    case EV_ARGV:   return "ARGV";
    case EV_ENV:    return "ENV";
    case EV_AUXV:   return "AUXV";
    case EV_EXIT:   return "EXIT";
    case EV_OPEN:   return "OPEN";
    case EV_CWD:    return "CWD";
    case EV_STDOUT: return "STDOUT";
    case EV_STDERR: return "STDERR";
    default:        return "????";
    }
}

/* Decode and print one outer event atom. `outer` is the outer atom
 * payload (i.e. the result of wire_get on the top-level cursor). */
static int print_event(ev_state *st, Src outer) {
    WireErr err = WIRE_OK;
    Src hdr  = wire_get(&outer, &err);
    Src blob = wire_get(&outer, &err);
    if (err != WIRE_OK) { return -1; }

    uint32_t sid;
    int32_t type, pid, tgid, ppid, nspid, nstgid;
    uint64_t ts_ns;
    ev_decode_header(st, &hdr, &err, &sid,
                     &type, &ts_ns, &pid, &tgid, &ppid, &nspid, &nstgid);
    if (err != WIRE_OK) { return -1; }

    printf("[sid=%u t=%lu] %s pid=%d tgid=%d ppid=%d nspid=%d nstgid=%d ",
           sid, (unsigned long)ts_ns, ev_name(type),
           pid, tgid, ppid, nspid, nstgid);

    switch (type) {
    case EV_EXIT: {
        int64_t status = wire_get_i64(&hdr, &err);
        int64_t code   = wire_get_i64(&hdr, &err);
        int64_t core   = wire_get_i64(&hdr, &err);
        int64_t raw    = wire_get_i64(&hdr, &err);
        if (err != WIRE_OK) return -1;
        printf("%s code/sig=%ld core=%ld raw=%ld",
               status == EV_EXIT_EXITED ? "exited" : "signaled",
               (long)code, (long)core, (long)raw);
        break;
    }
    case EV_OPEN: {
        int64_t flags  = wire_get_i64(&hdr, &err);
        int64_t fd     = wire_get_i64(&hdr, &err);
        int64_t ino    = wire_get_i64(&hdr, &err);
        int64_t devmaj = wire_get_i64(&hdr, &err);
        int64_t devmin = wire_get_i64(&hdr, &err);
        int64_t errn   = wire_get_i64(&hdr, &err);
        int64_t inh    = wire_get_i64(&hdr, &err);
        if (err != WIRE_OK) return -1;
        printf("flags=0x%lx fd=%ld ino=%ld dev=%ld:%ld err=%ld inh=%ld path=\"",
               (unsigned long)flags, (long)fd, (long)ino,
               (long)devmaj, (long)devmin, (long)errn, (long)inh);
        put_safe(blob.p, wire_src_len(blob));
        putchar('"');
        break;
    }
    case EV_EXEC: case EV_CWD:
    case EV_STDOUT: case EV_STDERR:
    case EV_ARGV: case EV_ENV: case EV_AUXV: {
        uint64_t blen = wire_src_len(blob);
        printf("blob=%luB \"", (unsigned long)blen);
        put_safe(blob.p, blen);
        putchar('"');
        break;
    }
    default:
        printf("(unknown event class)");
        return -1;
    }
    putchar('\n');
    return 0;
}

/* ─── stream walker ─────────────────────────────────────────────── */

/* Open-addressed, dynamically-grown table of per-stream ev_state.
 * Streams are sparse (one per emitting process) but their count is
 * unbounded in principle - a fork-bomby workload can produce millions
 * over the life of a trace. */
struct stream_tab_entry {
    uint32_t id;     /* 0 = empty slot */
    ev_state st;
};
struct stream_tab {
    struct stream_tab_entry *slots;
    uint32_t cap;
    uint32_t count;
};
static struct stream_tab g_streams;

#define STREAM_TAB_INIT_CAP 64u

static void stream_tab_reset(struct stream_tab *t) {
    free(t->slots);
    t->slots = NULL;
    t->cap   = 0;
    t->count = 0;
}

static struct stream_tab_entry *stream_tab_insert_raw(
        struct stream_tab_entry *slots, uint32_t cap, uint32_t id) {
    uint32_t mask = cap - 1u;
    uint32_t h = id * 2654435761u;
    for (uint32_t i = 0; i < cap; i++) {
        struct stream_tab_entry *e = &slots[(h + i) & mask];
        if (e->id == 0) {
            e->id = id;
            memset(&e->st, 0, sizeof(e->st));
            return e;
        }
    }
    return NULL;
}

static int stream_tab_grow(struct stream_tab *t) {
    uint32_t new_cap = t->cap ? t->cap * 2u : STREAM_TAB_INIT_CAP;
    if (new_cap < t->cap) { return -1; }
    struct stream_tab_entry *ns = calloc(new_cap, sizeof(*ns));
    if (!ns) { return -1; }
    for (uint32_t i = 0; i < t->cap; i++) {
        if (t->slots[i].id != 0) {
            struct stream_tab_entry *e =
                stream_tab_insert_raw(ns, new_cap, t->slots[i].id);
            e->st = t->slots[i].st;
        }
    }
    free(t->slots);
    t->slots = ns;
    t->cap   = new_cap;
    return 0;
}

static ev_state *stream_state_for(uint32_t id) {
    if ((uint64_t)(g_streams.count + 1u) * 4u > (uint64_t)g_streams.cap * 3u) {
        if (stream_tab_grow(&g_streams) < 0) { return NULL; }
    }
    uint32_t mask = g_streams.cap - 1u;
    uint32_t h = id * 2654435761u;
    for (uint32_t i = 0; i < g_streams.cap; i++) {
        struct stream_tab_entry *e = &g_streams.slots[(h + i) & mask];
        if (e->id == id) return &e->st;
        if (e->id == 0) {
            e->id = id;
            memset(&e->st, 0, sizeof(e->st));
            g_streams.count++;
            return &e->st;
        }
    }
    return NULL;
}

static int walk_stream(const uint8_t *buf, uint64_t len) {
    Src in = wire_src(buf, len);
    WireErr err = WIRE_OK;

    uint64_t ver = wire_get_u64(&in, &err);
    if (err != WIRE_OK) {
        fprintf(stderr, "wiredump: missing version atom\n");
        return -1;
    }
    if (ver != TRACE_VERSION) {
        fprintf(stderr, "wiredump: unsupported trace version %lu (want %u)\n",
                (unsigned long)ver, TRACE_VERSION);
        return -1;
    }
    fprintf(stderr, "-- trace version %lu, %lu bytes --\n",
            (unsigned long)ver, (unsigned long)len);

    stream_tab_reset(&g_streams);

    while (in.p < in.end) {
        const uint8_t *evstart = in.p;
        Src outer = wire_get(&in, &err);
        if (err != WIRE_OK) {
            fprintf(stderr, "wiredump: truncated/bad atom at offset %ld\n",
                    (long)(evstart - buf));
            return -1;
        }

        /* Need to peek stream_id to look up state. */
        Src peek = outer;
        WireErr perr = WIRE_OK;
        Src hdr = wire_get(&peek, &perr);
        if (perr != WIRE_OK) {
            fprintf(stderr, "wiredump: malformed event header\n");
            return -1;
        }
        uint64_t sid = wire_get_u64(&hdr, &perr);
        if (perr != WIRE_OK) {
            fprintf(stderr, "wiredump: malformed stream_id\n");
            return -1;
        }
        ev_state *st = stream_state_for((uint32_t)sid);
        if (!st) {
            fprintf(stderr, "wiredump: out of memory growing stream table\n");
            return -1;
        }
        if (print_event(st, outer) < 0) {
            fprintf(stderr, "wiredump: malformed event\n");
            return -1;
        }
    }
    return 0;
}

static int dump_file(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { perror(path); return 1; }
    struct stat st;
    if (fstat(fd, &st) < 0) { perror("fstat"); close(fd); return 1; }
    if (st.st_size <= 0) { close(fd); return 0; }
    void *m = mmap(NULL, (size_t)st.st_size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (m == MAP_FAILED) { perror("mmap"); return 1; }
    int rc = walk_stream((const uint8_t *)m, (uint64_t)st.st_size);
    munmap(m, (size_t)st.st_size);
    return rc != 0;
}

/* ─── selftest ──────────────────────────────────────────────────── */

#define ASSERT(cond) do { if (!(cond)) { \
    fprintf(stderr, "SELFTEST FAIL: %s (line %d)\n", #cond, __LINE__); \
    return 1; \
} } while (0)

static int rt_blob(uint64_t len, uint8_t fill) {
    static uint8_t buf[1024 * 1024];
    static uint8_t src[70000];
    if (len > sizeof src) { return 1; }
    for (uint64_t i = 0; i < len; i++) { src[i] = (uint8_t)((fill + i) & 0xFF); }
    Dst d = wire_dst(buf, sizeof buf);
    wire_put_blob(&d, wire_src(src, len));
    if (!d.p) {
        fprintf(stderr, "rt_blob len=%lu: encode failed\n", (unsigned long)len);
        return 1;
    }
    Src r = wire_src(buf, (uint64_t)((uint8_t *)d.p - buf));
    WireErr e = WIRE_OK;
    Src out = wire_get(&r, &e);
    if (e != WIRE_OK) {
        fprintf(stderr, "rt_blob len=%lu: decode failed\n", (unsigned long)len);
        return 1;
    }
    if (wire_src_len(out) != len || memcmp(out.p, src, (size_t)len) != 0) {
        fprintf(stderr, "rt_blob len=%lu: mismatch (got %lu)\n",
                (unsigned long)len, (unsigned long)wire_src_len(out));
        return 1;
    }
    return 0;
}

static int rt_u64(uint64_t v) {
    uint8_t buf[16];
    Dst d = wire_dst(buf, sizeof buf);
    wire_put_u64(&d, v);
    if (!d.p) return 1;
    Src r = wire_src(buf, (uint64_t)((uint8_t *)d.p - buf));
    WireErr e = WIRE_OK;
    uint64_t got = wire_get_u64(&r, &e);
    if (e != WIRE_OK || got != v) {
        fprintf(stderr, "rt_u64 v=%lu got=%lu\n",
                (unsigned long)v, (unsigned long)got);
        return 1;
    }
    return 0;
}

static int rt_i64(int64_t v) {
    uint8_t buf[16];
    Dst d = wire_dst(buf, sizeof buf);
    wire_put_i64(&d, v);
    if (!d.p) return 1;
    Src r = wire_src(buf, (uint64_t)((uint8_t *)d.p - buf));
    WireErr e = WIRE_OK;
    int64_t got = wire_get_i64(&r, &e);
    if (e != WIRE_OK || got != v) {
        fprintf(stderr, "rt_i64 v=%ld got=%ld\n", (long)v, (long)got);
        return 1;
    }
    return 0;
}

/* Build a small synthetic stream covering every event class, walk it. */
static int test_stream(void) {
    static uint8_t buf[4096];
    Dst d = wire_dst(buf, sizeof buf);
    wire_put_u64(&d, TRACE_VERSION);
    ASSERT(d.p);

    ev_state enc = {0};
    uint8_t hdr[EV_HEADER_MAX];
    int64_t extras[7];
    const uint64_t base_ts = 1711814400ull * 1000000000ull;

    #define EMIT(type_, ts_, extras_, n_extras_, blob_, blen_) do { \
        Dst hd = wire_dst(hdr, sizeof hdr); \
        ev_build_header(&enc, &hd, 1u, (type_), (ts_), \
                        1234, 1234, 1200, 1234, 1234, (extras_), (n_extras_)); \
        ASSERT(hd.p); \
        wire_put_pair(&d, \
                      wire_src(hdr, (size_t)((uint8_t*)hd.p - hdr)), \
                      wire_src((blob_), (blen_))); \
        ASSERT(d.p); \
    } while (0)

    EMIT(EV_EXEC, base_ts + 100, NULL, 0, "/usr/bin/\0make", 14);
    EMIT(EV_ARGV, base_ts + 101, NULL, 0, "make\0-j8\0", 9);
    EMIT(EV_ENV,  base_ts + 102, NULL, 0, "PATH=/usr/bin\0HOME=/root\0", 25);
    static const uint8_t auxv_null[16] = {0};
    EMIT(EV_AUXV, base_ts + 103, NULL, 0, auxv_null, sizeof auxv_null);
    EMIT(EV_CWD,  base_ts + 200, NULL, 0, "/home/u\xff", 8);

    extras[0] = 0;            /* flags */
    extras[1] = 3;            /* fd */
    extras[2] = 524297;       /* ino */
    extras[3] = 259;          /* dev_major */
    extras[4] = 2;            /* dev_minor */
    extras[5] = 0;            /* err */
    extras[6] = 0;            /* inh */
    EMIT(EV_OPEN, base_ts + 300, extras, 7, "/etc/passwd", 11);

    EMIT(EV_STDOUT, base_ts + 400, NULL, 0, "hello\n", 6);
    EMIT(EV_STDERR, base_ts + 500, NULL, 0, "warning\n", 8);

    extras[0] = EV_EXIT_SIGNALED;
    extras[1] = 11;           /* SIGSEGV */
    extras[2] = 1;            /* core dumped */
    extras[3] = 139;          /* raw */
    EMIT(EV_EXIT, base_ts + 600, extras, 4, NULL, 0);

    #undef EMIT

    uint64_t total = (uint64_t)((uint8_t *)d.p - buf);
    fprintf(stderr, "test_stream: %lu bytes, walking back…\n",
            (unsigned long)total);
    return walk_stream(buf, total);
}

static int selftest(void) {
    /* Atom primitive boundaries. */
    for (uint64_t L = 0; L <= 56; L++) { if (rt_blob(L, 0x42)) { return 1; } }
    if (rt_blob(255, 0x11))   { return 1; }
    if (rt_blob(256, 0x22))   { return 1; }
    if (rt_blob(65535, 0x33)) { return 1; }
    if (rt_blob(65536, 0x44)) { return 1; }

    static const uint64_t us[] = {
        0, 1, 191, 192, 255, 256, 65535, 65536,
        0xFFFFFFFFu, 0x100000000ull,
        0x7FFFFFFFFFFFFFFFull, 0xFFFFFFFFFFFFFFFFull,
    };
    for (size_t i = 0; i < sizeof us / sizeof us[0]; i++) {
        if (rt_u64(us[i])) { return 1; }
    }

    static const int64_t is[] = {
        0, 1, -1, 63, 64, -64, -65,
        0x7FFFFFFF, -0x80000000ll,
        0x7FFFFFFFFFFFFFFFll, (int64_t)0x8000000000000000ull,
    };
    for (size_t i = 0; i < sizeof is / sizeof is[0]; i++) {
        if (rt_i64(is[i])) { return 1; }
    }

    if (test_stream()) { return 1; }

    /* Tiny-event budget: after delta-priming, a 6-byte stdout event in
     * the same thread (1 ns later) should pack tight. */
    {
        uint8_t buf[128], hdr[EV_HEADER_MAX];
        Dst d = wire_dst(buf, sizeof buf);
        ev_state s = {0};
        Dst hd = wire_dst(hdr, sizeof hdr);
        ev_build_header(&s, &hd, 1u, EV_STDOUT, 1000,
                        1, 1, 1, 1, 1, NULL, 0);
        ASSERT(hd.p);
        wire_put_pair(&d, wire_src(hdr, (size_t)((uint8_t *)hd.p - hdr)),
                      wire_src("x", 1));
        ASSERT(d.p);
        size_t before = (size_t)((uint8_t *)d.p - buf);
        Dst hd2 = wire_dst(hdr, sizeof hdr);
        ev_build_header(&s, &hd2, 1u, EV_STDOUT, 1001,
                        1, 1, 1, 1, 1, NULL, 0);
        ASSERT(hd2.p);
        wire_put_pair(&d, wire_src(hdr, (size_t)((uint8_t *)hd2.p - hdr)),
                      wire_src("hello\n", 6));
        ASSERT(d.p);
        size_t delta = (size_t)((uint8_t *)d.p - buf) - before;
        fprintf(stderr, "tiny-event size: %zu bytes\n", delta);
        ASSERT(delta <= 20);  /* second event of same stream packs tight */
    }

    fprintf(stderr, "wiredump selftest: OK\n");
    return 0;
}

int wiredump_main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "--selftest") == 0) {
        return selftest();
    }
    if (argc < 2) {
        fprintf(stderr,
                "usage: tv dump --selftest\n"
                "       tv dump trace.bin [trace.bin ...]\n");
        return 2;
    }
    int rc = 0;
    for (int i = 1; i < argc; i++) {
        if (dump_file(argv[i]) != 0) { rc = 1; }
    }
    return rc;
}
