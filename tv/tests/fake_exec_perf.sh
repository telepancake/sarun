#!/bin/bash
# tests/fake_exec_perf.sh — Perf regression gate for the fake-exec addin.
#
# Builds a small fixture that runs many trivial helper invocations
# under sudtrace, measures wall-clock with --no-fake-exec vs the
# default (fake-exec on), and asserts the on/off ratio stays below
# a checked-in threshold.  See PLAN.md Part 3 Step G.
#
# Skipped automatically when timing-fragile knobs aren't reproducible
# (no /usr/bin/time and we can't fall back to bash $SECONDS, no
# /usr/bin/true, etc.).
set -eo pipefail

cd "$(dirname "$0")/.."
SUDTRACE_BIN=./sudtrace
SUD64_BIN=./sud64
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

ITERS=${FAKE_EXEC_PERF_ITERS:-500}
RATIO_THRESHOLD=${FAKE_EXEC_PERF_RATIO:-95}   # on/off, percentage; <100 = win

# Sanity: required binaries.
for tool in "$SUDTRACE_BIN" "$SUD64_BIN" /usr/bin/true /usr/bin/false /bin/sh; do
    if [ ! -x "$tool" ]; then
        echo "SKIP: $tool not available"
        exit 0
    fi
done

# fixture-true.sh: a sh loop that runs /usr/bin/true many times.
# Each iteration exercises the direct-execve elision path.
cat > "$TMPDIR/fixture-true.sh" << EOF
#!/bin/sh
i=0
while [ \$i -lt $ITERS ]; do
    /usr/bin/true
    i=\$((i + 1))
done
EOF
chmod +x "$TMPDIR/fixture-true.sh"

# fixture-sh-c.sh: each line a /bin/sh -c "true" invocation, exercising
# the Step C inner-recursion elision path on top of an outer fork+exec.
cat > "$TMPDIR/fixture-sh-c.sh" << EOF
#!/bin/sh
i=0
while [ \$i -lt $ITERS ]; do
    /bin/sh -c true
    i=\$((i + 1))
done
EOF
chmod +x "$TMPDIR/fixture-sh-c.sh"

# Median of three nanosecond timings.
median3_ns() {
    local a="$1" b="$2" c="$3"
    # Sort three ints, return the middle.
    printf '%s\n' "$a" "$b" "$c" | sort -n | sed -n '2p'
}

time_one_ns() {
    local out="$1"; shift
    local t0 t1
    t0=$(date +%s%N)
    "$@" > "$out" 2>&1
    t1=$(date +%s%N)
    echo $((t1 - t0))
}

run_fixture() {
    local fixture="$1"
    local label="$2"

    local off_a off_b off_c
    off_a=$(time_one_ns "$TMPDIR/${label}_off_a.wire" \
        "$SUDTRACE_BIN" -o "$TMPDIR/${label}_off_a.wire" --no-fake-exec \
        -- "$fixture")
    off_b=$(time_one_ns "$TMPDIR/${label}_off_b.wire" \
        "$SUDTRACE_BIN" -o "$TMPDIR/${label}_off_b.wire" --no-fake-exec \
        -- "$fixture")
    off_c=$(time_one_ns "$TMPDIR/${label}_off_c.wire" \
        "$SUDTRACE_BIN" -o "$TMPDIR/${label}_off_c.wire" --no-fake-exec \
        -- "$fixture")
    local off_med
    off_med=$(median3_ns "$off_a" "$off_b" "$off_c")

    local on_a on_b on_c
    on_a=$(time_one_ns "$TMPDIR/${label}_on_a.wire" \
        "$SUDTRACE_BIN" -o "$TMPDIR/${label}_on_a.wire" \
        -- "$fixture")
    on_b=$(time_one_ns "$TMPDIR/${label}_on_b.wire" \
        "$SUDTRACE_BIN" -o "$TMPDIR/${label}_on_b.wire" \
        -- "$fixture")
    on_c=$(time_one_ns "$TMPDIR/${label}_on_c.wire" \
        "$SUDTRACE_BIN" -o "$TMPDIR/${label}_on_c.wire" \
        -- "$fixture")
    local on_med
    on_med=$(median3_ns "$on_a" "$on_b" "$on_c")

    # Integer percent ratio = (on * 100) / off.  Lower is better.
    local pct=$(( on_med * 100 / off_med ))

    printf '  %-12s  off=%9d ns  on=%9d ns  ratio=%3d%%  (threshold <%d%%)\n' \
        "$label" "$off_med" "$on_med" "$pct" "$RATIO_THRESHOLD"

    if [ "$pct" -ge "$RATIO_THRESHOLD" ]; then
        echo "FAIL: fake-exec on/$label not under $RATIO_THRESHOLD%"
        return 1
    fi
    return 0
}

echo "fake_exec_perf.sh: ITERS=$ITERS, threshold=<$RATIO_THRESHOLD%"
fail=0
run_fixture "$TMPDIR/fixture-true.sh"  "true_loop"   || fail=1
run_fixture "$TMPDIR/fixture-sh-c.sh"  "sh_c_true"   || fail=1

if [ "$fail" -ne 0 ]; then
    echo "fake_exec_perf.sh: FAIL"
    exit 1
fi
echo "fake_exec_perf.sh: OK"
