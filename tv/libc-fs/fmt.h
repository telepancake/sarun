/*
 * libc-fs/fmt.h — TLS-free integer and string formatting for signal handlers.
 *
 * snprintf uses glibc internals that access %fs (stack canary,
 * locale data, errno).  These helpers format numbers directly
 * into a caller-supplied buffer without any TLS access.
 *
 * All functions return a pointer past the last character written
 * (the position of the NUL terminator), allowing easy chaining:
 *
 *     char *p = buf;
 *     p = fmt_str(p, "/proc/");
 *     p = fmt_int(p, pid);
 *
 * The caller is responsible for ensuring the buffer is large enough.
 *
 * Included in exactly one translation unit (the freestanding sud binary).
 */

#ifndef LIBC_FS_FMT_H
#define LIBC_FS_FMT_H

#include "libc-fs/libc.h"

/* ================================================================
 * Parsing helpers
 *
 * strtol / atoi use glibc locale internals that trigger SIGSEGV
 * inside the SIGSYS handler (NULL locale struct pointer).  These
 * parse digits directly — no locale lookup needed.
 * ================================================================ */

/* Parse a decimal integer from a string.  Skips leading whitespace,
 * handles optional sign. */
static inline int parse_int(const char *s)
{
    if (!s) return 0;
    while (*s == ' ' || *s == '\t' || *s == '\n') s++;
    int neg = 0;
    if (*s == '-') { neg = 1; s++; }
    else if (*s == '+') { s++; }
    int val = 0;
    while (*s >= '0' && *s <= '9')
        val = val * 10 + (*s++ - '0');
    return neg ? -val : val;
}

/* Parse an octal long from a string.  Skips leading whitespace. */
static inline long parse_long_octal(const char *s)
{
    if (!s) return 0;
    while (*s == ' ' || *s == '\t') s++;
    long val = 0;
    while (*s >= '0' && *s <= '7')
        val = val * 8 + (*s++ - '0');
    return val;
}

/* ================================================================
 * Formatting helpers
 *
 * Each function writes into buf and returns a pointer to the NUL
 * terminator (i.e. one past the last character written).
 * ================================================================ */

/* Format a signed int.  Returns pointer past last char written. */
static inline char *fmt_int(char *buf, int val)
{
    char tmp[24];
    int neg = 0, pos = 0;
    unsigned int uv;
    if (val < 0) { neg = 1; uv = (unsigned int)(-(val + 1)) + 1u; }
    else uv = (unsigned int)val;
    do { tmp[pos++] = '0' + (uv % 10); uv /= 10; } while (uv);
    if (neg) tmp[pos++] = '-';
    for (int i = 0; i < pos; i++) buf[i] = tmp[pos - 1 - i];
    buf[pos] = '\0';
    return buf + pos;
}

/* Format a signed long.  Returns pointer past last char written. */
static inline char *fmt_long(char *buf, long val)
{
    char tmp[24];
    int neg = 0, pos = 0;
    unsigned long uv;
    if (val < 0) { neg = 1; uv = (unsigned long)(-(val + 1)) + 1UL; }
    else uv = (unsigned long)val;
    do { tmp[pos++] = '0' + (uv % 10); uv /= 10; } while (uv);
    if (neg) tmp[pos++] = '-';
    for (int i = 0; i < pos; i++) buf[i] = tmp[pos - 1 - i];
    buf[pos] = '\0';
    return buf + pos;
}

/* Format an unsigned long.  Returns pointer past last char written. */
static inline char *fmt_ulong(char *buf, unsigned long val)
{
    char tmp[24];
    int pos = 0;
    do { tmp[pos++] = '0' + (val % 10); val /= 10; } while (val);
    for (int i = 0; i < pos; i++) buf[i] = tmp[pos - 1 - i];
    buf[pos] = '\0';
    return buf + pos;
}

/* Format a size_t as unsigned.  Returns pointer past last char written. */
static inline char *fmt_size(char *buf, size_t val)
{
    return fmt_ulong(buf, (unsigned long)val);
}

/* Format an unsigned long as lowercase hex with exactly 'digits' hex digits
 * (pad with leading zeros).  'digits' must be <= 16.  Returns pointer past
 * last char written.  No "0x" prefix is emitted. */
static inline char *fmt_hex_ul(char *buf, unsigned long val, int digits)
{
    static const char hextab[] = "0123456789abcdef";
    for (int i = digits - 1; i >= 0; i--) {
        buf[i] = hextab[val & 0xf];
        val >>= 4;
    }
    buf[digits] = '\0';
    return buf + digits;
}

/* Copy a string into buf.  Returns pointer past last char written. */
static inline char *fmt_str(char *buf, const char *s)
{
    while (*s) *buf++ = *s++;
    *buf = '\0';
    return buf;
}

/* Write a single character.  Returns pointer past last char written. */
static inline char *fmt_ch(char *buf, char c)
{
    buf[0] = c;
    buf[1] = '\0';
    return buf + 1;
}

/* Format "/proc/<pid>/<name>" into buf. Returns pointer past last char. */
static inline char *fmt_proc_path(char *buf, pid_t pid, const char *name)
{
    char *p = buf;
    p = fmt_str(p, "/proc/");
    p = fmt_int(p, (int)pid);
    p = fmt_ch(p, '/');
    p = fmt_str(p, name);
    return p;
}

#endif /* LIBC_FS_FMT_H */
