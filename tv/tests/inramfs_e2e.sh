#!/bin/bash
# tests/inramfs_e2e.sh — end-to-end test for the inramfs add-in.
#
# Runs ./sudtrace --inramfs <MOUNT> over a tiny shell workload,
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
SHM_GLOB="/dev/shm/sud-inramfs.${KEY}*"

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
trap 'rm -rf "$TMPDIR"; rm -f $SHM_GLOB' EXIT

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

    rm -f $SHM_GLOB

    local strace_log="$TMPDIR/strace.$name"
    local stdout_log="$TMPDIR/stdout.$name"
    local trace_out="$TMPDIR/trace.$name"

    # %file traces all syscalls that take a pathname argument.
    # We intentionally run strace OUTSIDE the SUD wrapper, so the
    # only file syscalls strace sees are those that actually
    # reached the kernel.  Any /inramfs_e2e/* in the captured
    # output is therefore a leak.
    if ! strace -f -e trace=%file -o "$strace_log" \
         "$SUDTRACE" --inramfs "$MOUNT:8" --inramfs-key "$KEY" \
         -o "$trace_out" -- /bin/sh -c "$cmd" \
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
# process.  Now uses `mkdir -p` (which chdir's into each component
# via chdir(2)) — chdir into inramfs is supported in the addin via
# a logical-CWD layer that propagates across exec via the --cwd flag
# re-emitted on every child wrapper invocation.
run_e2e "mkdir-and-nested" "deep:hello" \
    "mkdir -p $MOUNT/a/b/c &&
     echo hello > $MOUNT/a/b/c/f &&
     printf 'deep:'; cat $MOUNT/a/b/c/f"

# T4: chdir + getcwd + relative paths + cross-exec inheritance.
# After `cd $MOUNT/d`, `pwd`, `ls`, and `cat foo` (relative path)
# must all see the inramfs view, both in the parent shell (chdir
# tracked locally) and in exec'd children (logical CWD inherited
# via the --cwd flag re-emitted on every child wrapper invocation).
run_e2e "chdir-and-relative" "/inramfs_e2e/d
hello
hello" \
    "mkdir $MOUNT/d && cd $MOUNT/d && echo hello > foo &&
     pwd && cat foo && /bin/sh -c 'cat foo'"

# T5: cat-redirect via inherited fd.  Bash opens dst for write
# (inramfs memfd), dup2's onto fd 1, exec's cat.  cat's process
# starts with fd 1 inherited but not in the addin's fdtab; modern
# coreutils cat uses copy_file_range() which the addin must steer
# back to read/write, and the inherited fd 1 must be lazily adopted
# so writes land in the inramfs file rather than its empty memfd.
run_e2e "cat-redirect-inherited-fd" "HELLOWORLDHELLOWORLD" \
    "printf HELLOWORLD > $MOUNT/a && cat $MOUNT/a > $MOUNT/b &&
     cat $MOUNT/a; cat $MOUNT/b"

# T6: ftw-style traversal via dup'd dir fd.  `rm -rf` opens a
# directory, then fdopendir's it (which dups the fd internally),
# then unlinkat's via the *dup*.  Without dir_path inheritance on
# dup, the unlinkat would fail with EXDEV.
run_e2e "rm-rf-deep" "ok" \
    "mkdir -p $MOUNT/r/sub/sub2 &&
     echo a > $MOUNT/r/x && echo b > $MOUNT/r/sub/y &&
     echo c > $MOUNT/r/sub/sub2/z &&
     rm -rf $MOUNT/r &&
     [ ! -e $MOUNT/r ] && echo ok"

echo "inramfs-e2e: $PASS/$TOTAL passed, $FAIL failed"
[ $FAIL -eq 0 ]
