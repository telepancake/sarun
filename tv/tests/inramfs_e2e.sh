#!/bin/bash
# tests/inramfs_e2e.sh — end-to-end test for the inramfs add-in.
#
# Runs ./sudtrace with SUD_INRAMFS=<MOUNT> over a tiny shell workload,
# under strace -f -e trace=%file, and asserts:
#
#   1. The workload itself succeeded (expected stdout).
#   2. Zero kernel file syscalls reference any path under <MOUNT>.
#      The only places the mount path is allowed to appear in the
#      strace output are inside execve argv arrays (the kernel sees
#      argv as opaque strings, not as paths to resolve), and inside
#      the readlink(2) reply for "/proc/self/exe" (which strace
#      decodes by resolving the symlink — never our problem). Any
#      other appearance means a syscall escaped the inramfs handler
#      and reached the kernel — i.e. the in-RAM filesystem leaked.
#
# The harness is run by `make inramfs-test` after the unit tests.

set -eo pipefail

cd "$(dirname "$0")/.."

SUD64=./sud64
SUDTRACE=./sudtrace
MOUNT=/inramfs_e2e
KEY="e2e_$$_$(date +%s)"
SHM_FILE="/dev/shm/sud-inramfs.${KEY}"

# Sanity: the binaries we need are present.
for f in "$SUD64" "$SUDTRACE"; do
    if [ ! -x "$f" ]; then
        echo "inramfs e2e: missing $f — build first with: make sud64 sudtrace" >&2
        exit 1
    fi
done

# Sanity: strace exists and the kernel supports SUD.
if ! command -v strace >/dev/null 2>&1; then
    echo "inramfs e2e: strace not installed — skipping" >&2
    exit 0
fi

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"; rm -f "$SHM_FILE"' EXIT

PASS=0 FAIL=0 TOTAL=0

# ---------------------------------------------------------------- #
# Run a workload under sudtrace + strace and verify both:
#   * the workload prints the expected text on stdout
#   * no kernel %file syscall references the inramfs mount prefix
#
# Args:
#   $1 = test name
#   $2 = expected stdout (verbatim)
#   $3 = shell command to run inside sudtrace
# ---------------------------------------------------------------- #
run_e2e() {
    local name="$1" want="$2" cmd="$3"
    TOTAL=$((TOTAL + 1))

    rm -f "$SHM_FILE"

    local strace_log="$TMPDIR/strace.$name"
    local stdout_log="$TMPDIR/stdout.$name"
    local trace_out="$TMPDIR/trace.$name"

    # %file traces all syscalls that take a pathname argument.
    # We intentionally run strace OUTSIDE the SUD wrapper, so the
    # only file syscalls strace sees are those that actually
    # reached the kernel.  Any /inramfs_e2e/* in the captured
    # output is therefore a leak.
    if ! SUD_INRAMFS="$MOUNT:8" SUD_INRAMFS_KEY="$KEY" \
         strace -f -e trace=%file -o "$strace_log" \
         "$SUDTRACE" -o "$trace_out" -- /bin/sh -c "$cmd" \
         > "$stdout_log" 2>"$TMPDIR/stderr.$name"; then
        echo "FAIL [$name] sudtrace exit non-zero"
        echo "  stderr:" && sed 's/^/    /' "$TMPDIR/stderr.$name"
        FAIL=$((FAIL + 1))
        return
    fi

    local got
    got=$(cat "$stdout_log")
    if [ "$got" != "$want" ]; then
        echo "FAIL [$name] stdout mismatch"
        echo "  want: $(printf '%s' "$want" | od -c | head -2)"
        echo "  got:  $(printf '%s' "$got"  | od -c | head -2)"
        FAIL=$((FAIL + 1))
        return
    fi

    # Filter strace output to only lines that could leak the prefix.
    # Allowed appearances of "$MOUNT" in strace -f -e trace=%file:
    #   * inside execve(...)        — argv strings are not kernel
    #                                 path resolutions.
    #   * inside the *return value* of readlink/readlinkat/getcwd
    #                               — those are kernel→user replies,
    #                                 not user→kernel queries.
    # Anything else is a leak.
    local leaks
    leaks=$(awk -v mnt="$MOUNT" '
        # Strip leading "PID  " column for matching.
        {
            line = $0
            sub(/^[0-9]+ +/, "", line)
        }
        # Skip execve lines — argv is opaque to the kernel.
        line ~ /^execve\(/ { next }
        # Skip lines that only contain mnt inside the syscall return
        # string.  strace prints  open(...) = N<...>  with file paths
        # in angle brackets for stat/open returns; that is a strace
        # decoration, not a kernel path argument.  We strip
        # everything after the final " = " before matching.
        {
            sub(/ = .*/, "", line)
        }
        index(line, mnt) > 0 { print }
    ' "$strace_log")

    if [ -n "$leaks" ]; then
        echo "FAIL [$name] kernel saw inramfs path:"
        echo "$leaks" | sed 's/^/    /' | head -10
        FAIL=$((FAIL + 1))
        return
    fi

    echo "PASS [$name]"
    PASS=$((PASS + 1))
}

# ---------- workloads ---------- #

# T1: shell redirection.  Bash opens /inramfs_e2e/x for write,
# echo writes "hi", then cat reads it back.  Requires:
#   - openat hijack
#   - dup/dup2 hijack (bash dup2's the fd onto stdout for redirect)
#   - write hijack on the duped fd
#   - cross-process visibility (cat is an exec'd child)
run_e2e "redir-and-cat" "hi" \
    "echo hi > $MOUNT/x && cat $MOUNT/x"

# T2: in-process write+read via shell built-ins.
#   exec 5>FILE / echo X >&5 / read <&5  exercises dup2 onto an
#   arbitrary fd and write through the duped fd, all in one process.
run_e2e "exec-redir-builtins" "got: BB" \
    "exec 5>$MOUNT/y; echo BB >&5; exec 5<&-;
     exec 6<$MOUNT/y; read -r line <&6; exec 6<&-;
     printf 'got: %s' \"\$line\""

# T3: nested directories + cross-process write/read.  Verifies path
# walking through user-created directories survives to a child
# process.  We mkdir each component explicitly rather than using
# `mkdir -p` because coreutils' -p switches CWD into each component
# via chdir(2), and chdir on inramfs paths is intentionally a
# no-pass-through (the host kernel can't see the mount).  Adding
# chdir support is queued for a follow-up milestone.
run_e2e "mkdir-and-nested" "deep:hello" \
    "mkdir $MOUNT/a && mkdir $MOUNT/a/b && mkdir $MOUNT/a/b/c &&
     echo hello > $MOUNT/a/b/c/f &&
     printf 'deep:'; cat $MOUNT/a/b/c/f"

echo "inramfs-e2e: $PASS/$TOTAL passed, $FAIL failed"
[ $FAIL -eq 0 ]
