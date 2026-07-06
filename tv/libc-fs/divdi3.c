/* 64-bit division intrinsics for the 32-bit freestanding build.
 *
 * The sud wrappers link with -nostdlib and no compiler runtime (zig cc
 * cannot attach its compiler_rt to a -nostdlib link), so the libcalls
 * i386 codegen emits for 64-bit '/' and '%' must come from here. The
 * 64-bit build never references them (x86_64 divides inline); the
 * guard keeps this TU empty there.
 *
 * Shift-subtract restoring division, one bit per step. All shifts are
 * by constant amounts so codegen never re-enters a libcall.
 * Division by zero returns q=0, r=n (freestanding: no trap to raise).
 */
#if defined(__i386__)

unsigned long long __udivmoddi4(unsigned long long n, unsigned long long d,
                                unsigned long long *rem)
{
    if (d == 0) {
        if (rem)
            *rem = n;
        return 0;
    }
    unsigned long long q = 0, r = 0;
    for (int i = 0; i < 64; i++) {
        r = (r << 1) | (n >> 63);
        n <<= 1;
        q <<= 1;
        if (r >= d) {
            r -= d;
            q |= 1;
        }
    }
    if (rem)
        *rem = r;
    return q;
}

unsigned long long __udivdi3(unsigned long long n, unsigned long long d)
{
    return __udivmoddi4(n, d, 0);
}

unsigned long long __umoddi3(unsigned long long n, unsigned long long d)
{
    unsigned long long r;
    __udivmoddi4(n, d, &r);
    return r;
}

long long __divdi3(long long n, long long d)
{
    int neg = (n < 0) != (d < 0);
    unsigned long long un = n < 0 ? 0ULL - (unsigned long long)n : (unsigned long long)n;
    unsigned long long ud = d < 0 ? 0ULL - (unsigned long long)d : (unsigned long long)d;
    unsigned long long q = __udivmoddi4(un, ud, 0);
    return neg ? -(long long)q : (long long)q;
}

long long __moddi3(long long n, long long d)
{
    unsigned long long un = n < 0 ? 0ULL - (unsigned long long)n : (unsigned long long)n;
    unsigned long long ud = d < 0 ? 0ULL - (unsigned long long)d : (unsigned long long)d;
    unsigned long long r;
    __udivmoddi4(un, ud, &r);
    return n < 0 ? -(long long)r : (long long)r;
}

#endif /* __i386__ */
