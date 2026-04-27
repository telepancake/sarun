/* wire/wire.h — the only file that knows tv's atom encoding format.
 *
 * Self-contained C99. No libc beyond memcpy. Includes correctly from
 *   - the kernel module (mod/proctrace.c),
 *   - freestanding sud helpers (sud32/sud64; no libc),
 *   - C++ consumers in tv,
 *   - dump/test tools.
 *
 * Format is intentionally fluid. Nothing outside this header knows the
 * raw bytes; everything goes through these functions.
 *
 * ── Atoms ──────────────────────────────────────────────────────────
 *   b in 0x00..0xBF : 1-byte atom, payload is the single byte b.
 *                                                       (1 byte total)
 *   b in 0xC0..0xF7 : inline atom, len = b - 0xC0 (0..55).
 *                     payload follows.            (1 + len bytes total)
 *   b in 0xF8..0xFF : long atom, lensz = b - 0xF8 (0..7).
 *                     lensz LE bytes of length follow,
 *                     then `length` bytes of payload.
 *                                          (1 + lensz + len bytes total)
 *
 * Round-trips arbitrary byte sequences: every byte value is a legal
 * single-byte atom, every length up to 2^56-1 is a legal long atom.
 *
 * Worst-case prefix is 1 + 7 = 8 bytes (long form, lensz=7); see
 * WIRE_PREFIX_MAX below.
 *
 * ── Cursor types ───────────────────────────────────────────────────
 * All public functions take `Src *` and/or `Dst *`. Functions
 * short-circuit on a NULL `p`, so a long chain of put/get calls can
 * be written without per-call error checks; one check at the end is
 * enough. On error every function sets the cursor's `p` to NULL.
 *
 * Decoders accept an optional `WireErr *err` (may be NULL) to
 * distinguish between the kinds of failure (notably WIRE_ERR_TRUNC vs
 * WIRE_ERR_FORMAT, which a streaming consumer needs).
 *
 * Note: `wire_get` returns an `Src` view *into* the input buffer — no
 * data is copied. The format was crafted so that the encoded payload
 * of every atom is a contiguous byte range that can be consumed
 * directly.
 */

#ifndef WIRE_H
#define WIRE_H

#ifdef __KERNEL__
#  include <linux/types.h>
#  include <linux/string.h>
#else
#  include <stdint.h>
#  include <string.h>
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* Worst-case bytes for one atom's framing prefix (1 tag + 7 LE
 * length bytes). Useful for stack-buffer sizing. */
#define WIRE_PREFIX_MAX 8u

typedef enum {
    WIRE_OK     = 0,
    WIRE_ERR_TRUNC  = 1,  /* decoder: input ended mid-atom */
    WIRE_ERR_FORMAT = 2,  /* decoder: malformed atom (e.g. lensz=7) */
    WIRE_ERR_SPACE  = 3,  /* encoder: output buffer too small */
} WireErr;

typedef struct { const uint8_t *p; const uint8_t *end; } Src;
typedef struct {       uint8_t *p;       uint8_t *end; } Dst;

/* ────────────────────────── helpers ───────────────────────────── */

static inline Src wire_src(const void *p, uint64_t n) {
    Src s; s.p = (const uint8_t *)p; s.end = (const uint8_t *)p + n; return s;
}
static inline Dst wire_dst(void *p, uint64_t n) {
    Dst d; d.p = (uint8_t *)p; d.end = (uint8_t *)p + n; return d;
}
static inline uint64_t wire_src_len(Src s) {
    return (s.p && s.end >= s.p) ? (uint64_t)(s.end - s.p) : 0;
}

/* Bytes that wire_put_blob would write encoding `s`. */
static inline uint64_t wire_encoded_len(Src s) {
    uint64_t n = wire_src_len(s);
    /* self-byte */
    if (n == 1u && s.p[0] < 0xC0u) { return 1u; }
    /* inline */
    if (n <= 0x37u) { return 1u + n; }
    /* long */
    uint8_t lensz = 0; uint64_t tmp = n;
    while (tmp) { lensz++; tmp >>= 8; }
    return 1u + (uint64_t)lensz + n;
}

/* Internal: write a long-form prefix for payload of length `len`.
 * Caller has already ensured space and excluded the inline case. */
static inline int wire__put_long_prefix(Dst *d, uint64_t len) {
    uint8_t lenbuf[8]; uint8_t lensz = 0; uint64_t tmp = len;
    while (tmp) { lenbuf[lensz++] = (uint8_t)(tmp & 0xFFu); tmp >>= 8; }
    if (lensz > 7u) { d->p = NULL; return -1; }
    if ((uint64_t)(d->end - d->p) < 1u + (uint64_t)lensz) { d->p = NULL; return -1; }
    *d->p++ = (uint8_t)(0xF8u + lensz);
    for (uint8_t i = 0; i < lensz; i++) { *d->p++ = lenbuf[i]; }
    return 0;
}

/* ─────────────────────────── encoders ─────────────────────────── */

/* Encode `s` into `*d`. */
static inline void wire_put_blob(Dst *d, Src s) {
    if (!d->p) return;
    uint64_t n = wire_src_len(s);
    /* Self-byte form: 1-byte payload whose only byte is < 0xC0. */
    if (n == 1u && s.p[0] < 0xC0u) {
        if (d->p >= d->end) { d->p = NULL; return; }
        *d->p++ = s.p[0];
        return;
    }
    /* Inline form: 1 + n bytes, n in 0..55. */
    if (n <= 0x37u) {
        if ((uint64_t)(d->end - d->p) < 1u + n) { d->p = NULL; return; }
        *d->p++ = (uint8_t)(0xC0u + n);
        if (n) { memcpy(d->p, s.p, (size_t)n); d->p += n; }
        return;
    }
    /* Long form: 1 + lensz + n bytes. */
    if (wire__put_long_prefix(d, n) < 0) return;
    if ((uint64_t)(d->end - d->p) < n) { d->p = NULL; return; }
    memcpy(d->p, s.p, (size_t)n);
    d->p += n;
}

/* Encode v as the minimal-LE-bytes blob. */
static inline void wire_put_u64(Dst *d, uint64_t v) {
    if (!d->p) return;
    uint8_t buf[8]; uint8_t n = 0;
    while (v) { buf[n++] = (uint8_t)(v & 0xFFu); v >>= 8; }
    if (n == 1u && buf[0] < 0xC0u) {
        if (d->p >= d->end) { d->p = NULL; return; }
        *d->p++ = buf[0];
        return;
    }
    if ((uint64_t)(d->end - d->p) < 1u + (uint64_t)n) { d->p = NULL; return; }
    *d->p++ = (uint8_t)(0xC0u + n);
    for (uint8_t i = 0; i < n; i++) { *d->p++ = buf[i]; }
}

static inline void wire_put_i64(Dst *d, int64_t v) {
    uint64_t u = ((uint64_t)v << 1) ^ (uint64_t)(v >> 63);
    wire_put_u64(d, u);
}

/* Single outer atom whose payload is the concatenation of
 * wire_put_blob(srcs[i]) for i in [0, n).
 *
 * Computes the total payload length up front via wire_encoded_len so
 * the framing prefix is correct in one pass — no buffering, no
 * back-patching. */
static inline void wire_put_many(Dst *d, const Src *srcs, unsigned n) {
    if (!d->p) return;
    uint64_t total = 0;
    for (unsigned i = 0; i < n; i++) { total += wire_encoded_len(srcs[i]); }
    /* Inline form for total ≤ 55 bytes. */
    if (total <= 0x37u) {
        if ((uint64_t)(d->end - d->p) < 1u + total) { d->p = NULL; return; }
        *d->p++ = (uint8_t)(0xC0u + total);
    } else {
        if (wire__put_long_prefix(d, total) < 0) return;
    }
    for (unsigned i = 0; i < n; i++) { wire_put_blob(d, srcs[i]); }
}

static inline void wire_put_pair(Dst *d, Src a, Src b) {
    Src s[2]; s[0] = a; s[1] = b;
    wire_put_many(d, s, 2);
}

/* ────────────────────────── decoders ──────────────────────────── */

static inline void wire__set_err(WireErr *err, WireErr e) {
    if (err && *err == WIRE_OK) { *err = e; }
}

/* Read one atom from `*src`. Returns an `Src` view of the payload
 * (no copy) and advances `src->p` past the atom. On error: sets
 * `src->p` to NULL, optionally sets `*err`, returns an empty view. */
static inline Src wire_get(Src *src, WireErr *err) {
    Src out; out.p = NULL; out.end = NULL;
    if (!src->p) return out;
    const uint8_t *q = src->p;
    if (q >= src->end) { src->p = NULL; wire__set_err(err, WIRE_ERR_TRUNC); return out; }
    uint8_t b = q[0];
    if (b < 0xC0u) {
        out.p = q; out.end = q + 1;
        src->p = q + 1;
        return out;
    }
    if (b < 0xF8u) {
        uint64_t len = (uint64_t)(b - 0xC0u);
        if ((uint64_t)(src->end - q) < 1u + len) {
            src->p = NULL; wire__set_err(err, WIRE_ERR_TRUNC); return out;
        }
        out.p = q + 1; out.end = q + 1 + len;
        src->p = q + 1u + len;
        return out;
    }
    uint8_t lensz = (uint8_t)(b - 0xF8u);
    if ((uint64_t)(src->end - q) < 1u + (uint64_t)lensz) {
        src->p = NULL; wire__set_err(err, WIRE_ERR_TRUNC); return out;
    }
    uint64_t len = 0;
    for (uint8_t i = 0; i < lensz; i++) {
        len |= (uint64_t)q[1 + i] << (8u * i);
    }
    if ((uint64_t)(src->end - q) < 1u + (uint64_t)lensz + len) {
        src->p = NULL; wire__set_err(err, WIRE_ERR_TRUNC); return out;
    }
    out.p = q + 1u + lensz; out.end = out.p + len;
    src->p = q + 1u + lensz + len;
    return out;
}

/* Decode an atom as u64 (LE-extend the payload bytes to 64 bits). */
static inline uint64_t wire_get_u64(Src *src, WireErr *err) {
    Src a = wire_get(src, err);
    if (!a.p) return 0;
    uint64_t n = wire_src_len(a);
    if (n > 8u) { src->p = NULL; wire__set_err(err, WIRE_ERR_FORMAT); return 0; }
    uint64_t v = 0;
    for (uint64_t i = 0; i < n; i++) { v |= (uint64_t)a.p[i] << (8u * i); }
    return v;
}

static inline int64_t wire_get_i64(Src *src, WireErr *err) {
    uint64_t u = wire_get_u64(src, err);
    int64_t lsb = (int64_t)(u & 1u);
    return (int64_t)(u >> 1) ^ -lsb;
}

#ifdef __cplusplus
}  /* extern "C" */
#endif

#endif /* WIRE_H */
