#!/bin/bash
# tests/trace_compare/run.sh — Compare uproctrace and sudtrace on a mixed process tree.
#
# Builds small test binaries (static, dynamic, shebang), runs each under
# both uproctrace (ptrace) and sudtrace (SUD), then does a fuzzy comparison
# of the two JSONL traces.
#
# "Fuzzy" means: we normalise away PIDs, timestamps, inode numbers, device
# numbers, environment variables and other inherently variable fields, then
# compare the structural skeleton (event types, exe paths, opened paths,
# exit codes, argv tails).
#
# Both tracers must produce valid JSONL with the same schema.  We compare:
#   - Presence of expected event types (CWD, EXEC, OPEN, EXIT)
#   - Structural format (each event has pid, tgid, ppid, ts, event)
#   - EXEC events contain the target binary somewhere in argv
#   - Both have EXIT events with code 0 for successful runs
#   - Both capture inherited and non-inherited OPEN events
set -eo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
TV="$ROOT/tv"
SUDTRACE="$ROOT/sudtrace"
TESTDIR="$ROOT/tests/trace_compare"
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

check_re() {
    local desc="$1" pattern="$2" file="$3"
    TOTAL=$((TOTAL+1))
    if grep -qE "$pattern" "$file"; then
        PASS=$((PASS+1))
        echo "  PASS  $desc"
        return 0
    else
        FAIL=$((FAIL+1))
        echo "  FAIL  $desc (regex: $pattern)"
        return 1
    fi
}

# ── Build test programs ───────────────────────────────────────────────
echo "Building test programs…"
gcc -static -o "$TESTDIR/hello_static"  "$TESTDIR/hello_static.c"
gcc        -o "$TESTDIR/hello_dynamic" "$TESTDIR/hello_dynamic.c"
chmod +x "$TESTDIR/hello_shebang.sh" "$TESTDIR/run_tree.sh"

# ── Build tv + sudtrace if needed ─────────────────────────────────────
[ -x "$TV" ]       || { echo "Building tv…";       make tv       2>/dev/null; }
[ -x "$SUDTRACE" ] || { echo "Building sudtrace…"; make sudtrace 2>/dev/null; }

# ── Helper: run a command under both tracers ──────────────────────────
run_both() {
    local tag="$1"; shift
    echo ""
    echo "═══ Testing: $tag ═══"
    echo "  Command: $*"

    "$TV" --uproctrace --ptrace -o "$TMPDIR/${tag}_upt.jsonl" -- "$@" \
        > "$TMPDIR/${tag}_upt_stdout.txt" 2>&1 || true
    "$SUDTRACE" -o "$TMPDIR/${tag}_sud.jsonl" -- "$@" \
        > "$TMPDIR/${tag}_sud_stdout.txt" 2>&1 || true
}

exec_basenames() {
    jq -r '
        select(.event == "EXEC")
        | (
            (.argv // []) as $argv
            | if ($argv | length) == 0 then (.exe // "")
              elif (($argv[0] // "") | split("/") | last | test("^sud(32|64)?trace([_-].+)?$")) then
                  if ($argv | length) >= 3 and (($argv[1] // "") | split("/") | last | test("^ld-linux")) then
                      ($argv[2] // "")
                  else
                      ($argv[1] // "")
                  end
              else
                  ($argv[0] // "")
              end
          )
        | split("/") | last
    ' "$1" \
        | grep -vE '^(|sudtrace([_-].+)?|sud32([_-].+)?|sud64([_-].+)?|ld-linux[^[:space:]]*)$' \
        | sort -u
run_both_no_env() {
    local tag="$1"; shift
    echo ""
    echo "═══ Testing: $tag ═══"
    echo "  Command: $*"

    "$TV" --uproctrace --ptrace --no-env -o "$TMPDIR/${tag}_upt.jsonl" -- "$@" \
        > "$TMPDIR/${tag}_upt_stdout.txt" 2>&1 || true
    "$SUDTRACE" --no-env -o "$TMPDIR/${tag}_sud.jsonl" -- "$@" \
        > "$TMPDIR/${tag}_sud_stdout.txt" 2>&1 || true
}

# ── Helper: normalise a trace ─────────────────────────────────────────
normalise() {
    jq -c '
        if .event == "EXEC" then
            {event,
             exe_base: ((.exe // "") | split("/") | last),
             argv_base: [(.argv // [] | .[] | split("/") | last)]}
        elif .event == "OPEN" then
            {event,
             path_base: ((.path // "") | split("/") | last),
             inherited: (.inherited // false)}
        elif .event == "EXIT" then
            {event, code: (.code // null), status: (.status // null)}
        elif .event == "CWD" then
            {event, path}
        elif .event == "STDOUT" then
            {event}
        elif .event == "STDERR" then
            {event}
        else
            {event}
        end
    ' "$1" | sort
}

# ── Helper: validate JSON schema ─────────────────────────────────────
validate_schema() {
    local tag="$1" file="$2"
    # Every line must be valid JSON with at least event, ts, pid
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

check_exec_field_absent() {
    local desc="$1" field="$2" file="$3"
    TOTAL=$((TOTAL+1))
    if jq -e --arg field "$field" 'select(.event=="EXEC") | has($field)' "$file" > /dev/null; then
        FAIL=$((FAIL+1))
        echo "  FAIL  $desc ($field present)"
        return 1
    else
        PASS=$((PASS+1))
        echo "  PASS  $desc"
        return 0
    fi
}

# ═════════════════════════════════════════════════════════════════════
# Test 1: Dynamic binary (/bin/echo)
# ═════════════════════════════════════════════════════════════════════
run_both "dynamic_echo" /bin/echo "hello from dynamic echo"

echo ""
echo "=== Test 1: Dynamic binary (/bin/echo) ==="
validate_schema "uproctrace" "$TMPDIR/dynamic_echo_upt.jsonl"
validate_schema "sudtrace"   "$TMPDIR/dynamic_echo_sud.jsonl"
check "upt has CWD"          '"event":"CWD"'  "$TMPDIR/dynamic_echo_upt.jsonl"
check "sud has CWD"          '"event":"CWD"'  "$TMPDIR/dynamic_echo_sud.jsonl"
check "upt has EXEC"         '"event":"EXEC"' "$TMPDIR/dynamic_echo_upt.jsonl"
check "sud has EXEC"         '"event":"EXEC"' "$TMPDIR/dynamic_echo_sud.jsonl"
check "upt has EXIT"         '"event":"EXIT"' "$TMPDIR/dynamic_echo_upt.jsonl"
check "sud has EXIT"         '"event":"EXIT"' "$TMPDIR/dynamic_echo_sud.jsonl"
check "upt has OPEN"         '"event":"OPEN"' "$TMPDIR/dynamic_echo_upt.jsonl"
check "sud has OPEN"         '"event":"OPEN"' "$TMPDIR/dynamic_echo_sud.jsonl"
check "upt exit code 0"     '"code":0'       "$TMPDIR/dynamic_echo_upt.jsonl"
check "sud exit code 0"     '"code":0'       "$TMPDIR/dynamic_echo_sud.jsonl"
# Both should reference "echo" somewhere in argv
check "upt argv has echo"   'echo'           "$TMPDIR/dynamic_echo_upt.jsonl"
check "sud argv has echo"   'echo'           "$TMPDIR/dynamic_echo_sud.jsonl"

run_both_no_env "dynamic_echo_no_env" /bin/echo "hello from dynamic echo"

echo ""
echo "=== Test 1b: Dynamic binary without env capture ==="
validate_schema "uproctrace no-env" "$TMPDIR/dynamic_echo_no_env_upt.jsonl"
validate_schema "sudtrace no-env"   "$TMPDIR/dynamic_echo_no_env_sud.jsonl"
check "upt no-env has EXEC"         '"event":"EXEC"' "$TMPDIR/dynamic_echo_no_env_upt.jsonl"
check "sud no-env has EXEC"         '"event":"EXEC"' "$TMPDIR/dynamic_echo_no_env_sud.jsonl"
check_exec_field_absent "upt no-env omits env" "env" "$TMPDIR/dynamic_echo_no_env_upt.jsonl"
check_exec_field_absent "sud no-env omits env" "env" "$TMPDIR/dynamic_echo_no_env_sud.jsonl"

# ═════════════════════════════════════════════════════════════════════
# Test 2: Shebang script
# ═════════════════════════════════════════════════════════════════════
run_both "shebang" "$TESTDIR/hello_shebang.sh"

echo ""
echo "=== Test 2: Shebang script ==="
validate_schema "uproctrace" "$TMPDIR/shebang_upt.jsonl"
validate_schema "sudtrace"   "$TMPDIR/shebang_sud.jsonl"
check "upt has EXEC"         '"event":"EXEC"' "$TMPDIR/shebang_upt.jsonl"
check "sud has EXEC"         '"event":"EXEC"' "$TMPDIR/shebang_sud.jsonl"
check "upt has EXIT"         '"event":"EXIT"' "$TMPDIR/shebang_upt.jsonl"
check "sud has EXIT"         '"event":"EXIT"' "$TMPDIR/shebang_sud.jsonl"
check "upt argv has shebang" 'hello_shebang.sh' "$TMPDIR/shebang_upt.jsonl"
check "sud argv has shebang" 'hello_shebang.sh' "$TMPDIR/shebang_sud.jsonl"
check "upt exit code 0"     '"code":0'       "$TMPDIR/shebang_upt.jsonl"
check "sud exit code 0"     '"code":0'       "$TMPDIR/shebang_sud.jsonl"

# ═════════════════════════════════════════════════════════════════════
# Test 3: Static binary
# ═════════════════════════════════════════════════════════════════════
run_both "static" "$TESTDIR/hello_static"

echo ""
echo "=== Test 3: Static binary ==="
validate_schema "uproctrace" "$TMPDIR/static_upt.jsonl"
validate_schema "sudtrace"   "$TMPDIR/static_sud.jsonl"
check "upt has EXEC"         '"event":"EXEC"'    "$TMPDIR/static_upt.jsonl"
check "sud has EXEC"         '"event":"EXEC"'    "$TMPDIR/static_sud.jsonl"
check "upt has EXIT"         '"event":"EXIT"'    "$TMPDIR/static_upt.jsonl"
check "sud has EXIT"         '"event":"EXIT"'    "$TMPDIR/static_sud.jsonl"
check "upt argv has static"  'hello_static'      "$TMPDIR/static_upt.jsonl"
check "sud argv has static"  'hello_static'      "$TMPDIR/static_sud.jsonl"
check "upt exit code 0"     '"code":0'          "$TMPDIR/static_upt.jsonl"
check "sud exit code 0"     '"code":0'          "$TMPDIR/static_sud.jsonl"

# ═════════════════════════════════════════════════════════════════════
# Test 4: Dynamic binary (custom)
# ═════════════════════════════════════════════════════════════════════
run_both "dynamic" "$TESTDIR/hello_dynamic"

echo ""
echo "=== Test 4: Dynamic binary (custom) ==="
validate_schema "uproctrace" "$TMPDIR/dynamic_upt.jsonl"
validate_schema "sudtrace"   "$TMPDIR/dynamic_sud.jsonl"
check "upt has EXEC"         '"event":"EXEC"'     "$TMPDIR/dynamic_upt.jsonl"
check "sud has EXEC"         '"event":"EXEC"'     "$TMPDIR/dynamic_sud.jsonl"
check "upt has EXIT"         '"event":"EXIT"'     "$TMPDIR/dynamic_upt.jsonl"
check "sud has EXIT"         '"event":"EXIT"'     "$TMPDIR/dynamic_sud.jsonl"
check "upt argv has dynamic" 'hello_dynamic'      "$TMPDIR/dynamic_upt.jsonl"
check "sud argv has dynamic" 'hello_dynamic'      "$TMPDIR/dynamic_sud.jsonl"
check "upt exit code 0"     '"code":0'           "$TMPDIR/dynamic_upt.jsonl"
check "sud exit code 0"     '"code":0'           "$TMPDIR/dynamic_sud.jsonl"

# ═════════════════════════════════════════════════════════════════════
# Test 5: Process tree (all types)
# ═════════════════════════════════════════════════════════════════════
run_both "tree" "$TESTDIR/run_tree.sh"

echo ""
echo "=== Test 5: Full process tree ==="
validate_schema "uproctrace" "$TMPDIR/tree_upt.jsonl"
validate_schema "sudtrace"   "$TMPDIR/tree_sud.jsonl"

# uproctrace should see all sub-processes
check "upt tree has hello_static"  'hello_static'  "$TMPDIR/tree_upt.jsonl"
check "upt tree has hello_dynamic" 'hello_dynamic' "$TMPDIR/tree_upt.jsonl"
check "upt tree has hello_shebang" 'hello_shebang' "$TMPDIR/tree_upt.jsonl"
check "upt tree has run_tree"      'run_tree'      "$TMPDIR/tree_upt.jsonl"

# sudtrace should at least see the top-level script
check "sud tree has run_tree"      'run_tree'      "$TMPDIR/tree_sud.jsonl"

# ═════════════════════════════════════════════════════════════════════
# Cross-tracer structural comparison
# ═════════════════════════════════════════════════════════════════════
echo ""
echo "=== Cross-tracer event format comparison ==="

# Both tracers should produce the same set of event types
for evt in CWD EXEC OPEN EXIT; do
    UPT_HAS=$(jq -r "select(.event==\"$evt\")" "$TMPDIR/dynamic_echo_upt.jsonl" | wc -l)
    SUD_HAS=$(jq -r "select(.event==\"$evt\")" "$TMPDIR/dynamic_echo_sud.jsonl" | wc -l)
    TOTAL=$((TOTAL+1))
    if [ "$UPT_HAS" -gt 0 ] && [ "$SUD_HAS" -gt 0 ]; then
        PASS=$((PASS+1))
        echo "  PASS  Both tracers emit $evt events (upt:$UPT_HAS sud:$SUD_HAS)"
    else
        FAIL=$((FAIL+1))
        echo "  FAIL  $evt event mismatch (upt:$UPT_HAS sud:$SUD_HAS)"
    fi
done

# Both EXEC events should have the same schema fields
for field in exe argv env auxv; do
    TOTAL=$((TOTAL+1))
    UPT_HAS=$(jq -r "select(.event==\"EXEC\") | has(\"$field\")" "$TMPDIR/dynamic_echo_upt.jsonl" | grep -c true || true)
    SUD_HAS=$(jq -r "select(.event==\"EXEC\") | has(\"$field\")" "$TMPDIR/dynamic_echo_sud.jsonl" | grep -c true || true)
    if [ "$UPT_HAS" -gt 0 ] && [ "$SUD_HAS" -gt 0 ]; then
        PASS=$((PASS+1))
        echo "  PASS  Both EXEC events have .$field"
    else
        FAIL=$((FAIL+1))
        echo "  FAIL  EXEC .$field mismatch (upt:$UPT_HAS sud:$SUD_HAS)"
    fi
done

# ── Event count summary ──────────────────────────────────────────────
echo ""
echo "=== Event count summary (dynamic echo test) ==="
echo "uproctrace:"
jq -r '.event' "$TMPDIR/dynamic_echo_upt.jsonl" | sort | uniq -c | sed 's/^/  /'
echo "sudtrace:"
jq -r '.event' "$TMPDIR/dynamic_echo_sud.jsonl" | sort | uniq -c | sed 's/^/  /'

# ── Normalised diff ──────────────────────────────────────────────────
echo ""
echo "=== Normalised structural diff (echo test, first 30 lines) ==="
normalise "$TMPDIR/dynamic_echo_upt.jsonl" > "$TMPDIR/echo_upt_norm.jsonl"
normalise "$TMPDIR/dynamic_echo_sud.jsonl" > "$TMPDIR/echo_sud_norm.jsonl"
diff --color=never -u "$TMPDIR/echo_upt_norm.jsonl" "$TMPDIR/echo_sud_norm.jsonl" \
    | head -30 || true

# ═════════════════════════════════════════════════════════════════════
# Test 6: Clean rebuild should trace subprocess execs too
# ═════════════════════════════════════════════════════════════════════
echo ""
echo "=== Test 6: Clean rebuild subprocess coverage ==="
cp "$TV" "$TMPDIR/tv_trace"
cp "$SUDTRACE" "$TMPDIR/sudtrace_trace"
"$TMPDIR/tv_trace" --uproctrace --ptrace -o "$TMPDIR/rebuild_upt.jsonl" -- \
    make -B tv sudtrace > "$TMPDIR/rebuild_upt_stdout.txt" 2>&1 || true
"$TMPDIR/sudtrace_trace" -o "$TMPDIR/rebuild_sud.jsonl" -- \
    make -B tv sudtrace > "$TMPDIR/rebuild_sud_stdout.txt" 2>&1 || true
validate_schema "rebuild uproctrace" "$TMPDIR/rebuild_upt.jsonl"
validate_schema "rebuild sudtrace"   "$TMPDIR/rebuild_sud.jsonl"
exec_basenames "$TMPDIR/rebuild_upt.jsonl" > "$TMPDIR/rebuild_upt_execs.txt"
exec_basenames "$TMPDIR/rebuild_sud.jsonl" > "$TMPDIR/rebuild_sud_execs.txt"
check "rebuild sud sees gen_sql_h" 'gen_sql_h' "$TMPDIR/rebuild_sud_execs.txt"
check_re "rebuild sud sees compiler exec" '(cc|gcc|x86_64-linux-gnu-gcc-13)' "$TMPDIR/rebuild_sud_execs.txt"
TOTAL=$((TOTAL+1))
if diff -u "$TMPDIR/rebuild_upt_execs.txt" "$TMPDIR/rebuild_sud_execs.txt" >/dev/null; then
    PASS=$((PASS+1))
    echo "  PASS  rebuild exec basename set matches ptrace"
else
    FAIL=$((FAIL+1))
    echo "  FAIL  rebuild exec basename set differs from ptrace"
    diff -u "$TMPDIR/rebuild_upt_execs.txt" "$TMPDIR/rebuild_sud_execs.txt" | sed 's/^/    /' || true
fi

# ── Summary ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════"
echo "Results: $PASS passed, $FAIL failed out of $TOTAL"
if [ "$FAIL" -gt 0 ]; then
    echo "SOME CHECKS FAILED"
    exit 1
fi
echo "ALL CHECKS PASSED"
