#!/bin/bash
# tests/trace_compare/nested_uproctrace.sh — Test nested uproctrace under sudtrace.
#
# Runs:  sudtrace -o <file> -- tv --uproctrace -o <file> -- /bin/echo hello
#
# Validates:
#   - Both sudtrace and uproctrace produce valid JSONL
#   - sudtrace sees EXEC + EXIT for the tv process
#   - uproctrace sees CWD + EXEC + OPEN + EXIT + STDOUT for /bin/echo
#   - /bin/echo actually runs and produces output
set -eo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
TV="$ROOT/tv"
SUDTRACE="$ROOT/sudtrace"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

PASS=0
FAIL=0
TOTAL=0

check() {
    local desc="$1" pattern="$2" file="$3"
    TOTAL=$((TOTAL+1))
    if grep -qF "$pattern" "$file"; then
        PASS=$((PASS+1))
        echo "  PASS  $desc"
        return 0
    else
        FAIL=$((FAIL+1))
        echo "  FAIL  $desc (pattern: $pattern)"
        return 1
    fi
}

validate_schema() {
    local tag="$1" file="$2"
    local bad
    bad=$(jq -c 'select(.event == null or .ts == null or .pid == null)' "$file" | wc -l)
    TOTAL=$((TOTAL+1))
    if [ "$bad" -eq 0 ]; then
        PASS=$((PASS+1))
        echo "  PASS  $tag: all events have event/ts/pid"
    else
        FAIL=$((FAIL+1))
        echo "  FAIL  $tag: $bad events missing event/ts/pid"
    fi
}

# ── Build if needed ───────────────────────────────────────────────
[ -x "$TV" ]       || { echo "Building tv…";       make tv       2>/dev/null; }
[ -x "$SUDTRACE" ] || { echo "Building sudtrace…"; make sudtrace 2>/dev/null; }

# ═════════════════════════════════════════════════════════════════════
# Test: nested uproctrace under sudtrace
# ═════════════════════════════════════════════════════════════════════
echo ""
echo "═══ Nested tracing: sudtrace → uproctrace → echo ═══"

timeout 30 "$SUDTRACE" -o "$TMPDIR/sud.jsonl" \
    -- "$TV" --uproctrace --ptrace -o "$TMPDIR/upt.jsonl" \
    -- /bin/echo "hello from nested" \
    > "$TMPDIR/stdout.txt" 2>&1
RC=$?

echo "  exit code: $RC"
echo "  stdout: $(cat "$TMPDIR/stdout.txt")"

# ── sudtrace output checks ───────────────────────────────────────
echo ""
echo "=== sudtrace outer trace ==="
validate_schema "sudtrace" "$TMPDIR/sud.jsonl"
check "sud has EXEC"     '"event":"EXEC"' "$TMPDIR/sud.jsonl"
check "sud has EXIT"     '"event":"EXIT"' "$TMPDIR/sud.jsonl"
check "sud exit code 0"  '"code":0'       "$TMPDIR/sud.jsonl"
check "sud argv has tv"  'tv'             "$TMPDIR/sud.jsonl"

# ── uproctrace output checks ─────────────────────────────────────
echo ""
echo "=== uproctrace inner trace ==="

# uproctrace output should exist
TOTAL=$((TOTAL+1))
if [ -s "$TMPDIR/upt.jsonl" ]; then
    PASS=$((PASS+1))
    echo "  PASS  uproctrace output file exists and non-empty"
else
    FAIL=$((FAIL+1))
    echo "  FAIL  uproctrace output file missing or empty"
    echo ""
    echo "════════════════════════════════════════"
    echo "Results: $PASS passed, $FAIL failed out of $TOTAL"
    exit 1
fi

validate_schema "uproctrace" "$TMPDIR/upt.jsonl"
check "upt has CWD"          '"event":"CWD"'    "$TMPDIR/upt.jsonl"
check "upt has EXEC"         '"event":"EXEC"'   "$TMPDIR/upt.jsonl"
check "upt has EXIT"         '"event":"EXIT"'   "$TMPDIR/upt.jsonl"
check "upt has OPEN"         '"event":"OPEN"'   "$TMPDIR/upt.jsonl"
check "upt has STDOUT"       '"event":"STDOUT"' "$TMPDIR/upt.jsonl"
check "upt exit code 0"      '"code":0'         "$TMPDIR/upt.jsonl"
check "upt argv has echo"    'echo'             "$TMPDIR/upt.jsonl"
check "upt captures output"  'hello from nested' "$TMPDIR/upt.jsonl"

# ── echo actually ran ────────────────────────────────────────────
TOTAL=$((TOTAL+1))
if grep -q "hello from nested" "$TMPDIR/stdout.txt"; then
    PASS=$((PASS+1))
    echo "  PASS  echo output visible on stdout"
else
    FAIL=$((FAIL+1))
    echo "  FAIL  echo output not on stdout"
fi

# ── Event count summary ──────────────────────────────────────────
echo ""
echo "=== Event counts ==="
echo "sudtrace:"
jq -r '.event' "$TMPDIR/sud.jsonl" | sort | uniq -c | sed 's/^/  /'
echo "uproctrace:"
jq -r '.event' "$TMPDIR/upt.jsonl" | sort | uniq -c | sed 's/^/  /'

# ═════════════════════════════════════════════════════════════════════
# Test: nested uproctrace with a more complex process tree
# ═════════════════════════════════════════════════════════════════════
echo ""
echo "═══ Nested tracing: sudtrace → uproctrace → sh -c (fork+exec) ═══"

timeout 30 "$SUDTRACE" -o "$TMPDIR/sud2.jsonl" \
    -- "$TV" --uproctrace --ptrace -o "$TMPDIR/upt2.jsonl" \
    -- /bin/sh -c 'echo "child1"; /bin/echo "child2"' \
    > "$TMPDIR/stdout2.txt" 2>&1
RC2=$?

echo "  exit code: $RC2"

echo ""
echo "=== sudtrace outer trace (tree) ==="
validate_schema "sudtrace" "$TMPDIR/sud2.jsonl"
check "sud2 has EXIT"    '"code":0' "$TMPDIR/sud2.jsonl"

echo ""
echo "=== uproctrace inner trace (tree) ==="
TOTAL=$((TOTAL+1))
if [ -s "$TMPDIR/upt2.jsonl" ]; then
    PASS=$((PASS+1))
    echo "  PASS  uproctrace output exists for tree test"
else
    FAIL=$((FAIL+1))
    echo "  FAIL  uproctrace output missing for tree test"
fi

validate_schema "uproctrace" "$TMPDIR/upt2.jsonl"
check "upt2 has EXEC"    '"event":"EXEC"' "$TMPDIR/upt2.jsonl"
check "upt2 has EXIT"    '"event":"EXIT"' "$TMPDIR/upt2.jsonl"

# Count EXEC events (should be ≥2: /bin/sh + /bin/echo)
EXEC_COUNT=$(jq -r 'select(.event=="EXEC")' "$TMPDIR/upt2.jsonl" | wc -l)
TOTAL=$((TOTAL+1))
if [ "$EXEC_COUNT" -ge 2 ]; then
    PASS=$((PASS+1))
    echo "  PASS  uproctrace saw $EXEC_COUNT EXEC events (expected ≥2)"
else
    FAIL=$((FAIL+1))
    echo "  FAIL  uproctrace saw $EXEC_COUNT EXEC events (expected ≥2)"
fi

# Check that both children produced output
TOTAL=$((TOTAL+1))
if grep -q "child1" "$TMPDIR/stdout2.txt" && grep -q "child2" "$TMPDIR/stdout2.txt"; then
    PASS=$((PASS+1))
    echo "  PASS  both children produced output"
else
    FAIL=$((FAIL+1))
    echo "  FAIL  missing child output"
fi

# ── Summary ──────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════"
echo "Results: $PASS passed, $FAIL failed out of $TOTAL"
if [ "$FAIL" -gt 0 ]; then
    echo "SOME CHECKS FAILED"
    exit 1
fi
echo "ALL CHECKS PASSED"
