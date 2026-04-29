#!/bin/bash
# tests/inramfs_sqlite_e2e.sh — ultimate end-to-end test for the
# inramfs add-in.
#
# Runs an entire sqlite-from-source workflow under ./sudtrace with
# SUD_INRAMFS+SUD_REMAP routing all I/O into the in-RAM filesystem:
#
#   1.  git clone (small) sqlite amalgamation repo into /ir/work
#   2.  compile sqlite3 (gcc shell.c sqlite3.c -o sqlite3)
#   3.  invocation #1: CREATE TABLE + bulk INSERT into /ir/db.sqlite
#   4.  invocation #2 (separate process): SELECT aggregates, validate
#       row counts, hashed sum, and an indexed range query
#
# Asserts:
#   - every step exits 0 (no crashes, no hangs)
#   - the trace shows no host-disk write outside the working dir
#     (we don't enforce this strictly here; the read-only-on-host
#     property is what SUD_REMAP / SUD_INRAMFS together provide)
#   - after the launcher exits, /dev/shm/sud-inramfs.* contains no
#     leftover backing files (the launcher owns the shm lifetime)
#
# Run by `make inramfs-test-sqlite` (also invoked by full e2e).
#
# The clone target is the github "azadkuh/sqlite-amalgamation"
# mirror, which is a ~13 MiB shallow checkout containing just the
# amalgamated sqlite3.c + shell.c + headers — no autoconf, no tcl
# dependency, no MSVC stuff.  If the network is unreachable the
# test prints SKIP and exits 0 so it doesn't fail in air-gapped CI.

set -eo pipefail

cd "$(dirname "$0")/.."

SUD64=./sud64
SUDTRACE=./sudtrace
MOUNT=/ir_sqlite

for f in "$SUD64" "$SUDTRACE"; do
    if [ ! -x "$f" ]; then
        echo "inramfs-sqlite-e2e: missing $f — build first" >&2
        exit 1
    fi
done

# Network preflight: don't fail in air-gapped envs.
if ! curl -sSf --max-time 5 -o /dev/null -I \
        https://github.com/azadkuh/sqlite-amalgamation; then
    echo "inramfs-sqlite-e2e: network unreachable — SKIP"
    exit 0
fi

# Pre-clean any leftover shm so the post-run check is meaningful.
rm -f /dev/shm/sud-inramfs.* 2>/dev/null || true

# Capture stdout/stderr; we'll print everything on failure for
# debuggability, and only the final summary on success.
LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

fail() {
    echo "inramfs-sqlite-e2e: FAIL — $*" >&2
    echo "--- launcher log ---" >&2
    cat "$LOG" >&2 || true
    echo "--- /dev/shm leftover ---" >&2
    ls /dev/shm/sud-inramfs.* 2>&1 >&2 || true
    rm -f /dev/shm/sud-inramfs.* 2>/dev/null || true
    exit 1
}

# Mount size: 13 MiB clone + build artefacts (sqlite3.c is 9 MiB
# and the .o is bigger) + a 10k-row db.  64 MiB is plenty.
#
# The script runs INSIDE one sudtrace invocation so that all four
# phases (clone, build, run-1, run-2) share the same inramfs.  The
# launcher mints the SUD_INRAMFS_KEY itself and unlinks both shm
# files on exit, so after the script we can grep /dev/shm to
# confirm no orphans were left behind.
SUD_INRAMFS="${MOUNT}:64" \
    "$SUDTRACE" -o /dev/null -- /bin/bash -c '
set -e
MOUNT="'"$MOUNT"'"

echo "[1/4] git clone --depth=1 sqlite-amalgamation"
mkdir -p "$MOUNT/work"
cd "$MOUNT/work"
git clone --depth=1 -q https://github.com/azadkuh/sqlite-amalgamation src
[ -f src/sqlite3.c ] || { echo "clone missing sqlite3.c"; exit 2; }
[ -f src/shell.c   ] || { echo "clone missing shell.c";   exit 2; }

echo "[2/4] compile sqlite3"
cd src
# -O0 so compile is fast; -DSQLITE_OMIT_LOAD_EXTENSION drops dlopen
# (would need -ldl).  -DHAVE_USLEEP is what real builds set.
gcc -O0 -DSQLITE_THREADSAFE=0 \
        -DSQLITE_OMIT_LOAD_EXTENSION \
        -DHAVE_USLEEP \
        shell.c sqlite3.c -lm -lpthread -o sqlite3
[ -x ./sqlite3 ] || { echo "build did not produce sqlite3"; exit 2; }
./sqlite3 -version

echo "[3/4] invocation #1: create + populate /ir/db.sqlite"
DB="$MOUNT/db.sqlite"
rm -f "$DB"
./sqlite3 "$DB" <<SQL
CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT, v INTEGER);
CREATE INDEX i_k ON t(k);
BEGIN;
WITH RECURSIVE c(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM c WHERE i<10000)
INSERT INTO t(id,k,v) SELECT i, printf("k%05d", i%997), i*7 FROM c;
COMMIT;
SQL
[ -s "$DB" ] || { echo "db not created"; exit 2; }

echo "[4/4] invocation #2: validate via SELECTs"
N=$(./sqlite3 "$DB" "SELECT COUNT(*) FROM t;")
S=$(./sqlite3 "$DB" "SELECT SUM(v)   FROM t;")
K=$(./sqlite3 "$DB" "SELECT COUNT(*) FROM t WHERE k = \"k00042\";")

# Expected: 10000 rows, sum = 7*(1+2+...+10000) = 7*50005000 = 350035000
# Indexed lookup: rows with i%997==42 in [1..10000] →
#   i ∈ {42, 1039, 2036, 3033, 4030, 5027, 6024, 7021, 8018, 9015}
#   = 10 rows.
[ "$N" = "10000"      ]   || { echo "row count wrong: $N";   exit 3; }
[ "$S" = "350035000"  ]   || { echo "sum wrong: $S";         exit 3; }
[ "$K" = "10"         ]   || { echo "indexed count wrong: $K"; exit 3; }

echo "PASS rows=$N sum=$S indexed=$K"
' >"$LOG" 2>&1 || fail "launcher returned non-zero exit"

# Confirm the workload's PASS line.
if ! tail -n 5 "$LOG" | grep -q "^PASS rows=10000"; then
    fail "PASS line missing from workload output"
fi

# Confirm the launcher cleaned up its /dev/shm files.
if ls /dev/shm/sud-inramfs.* >/dev/null 2>&1; then
    fail "shm files left in /dev/shm after launcher exit"
fi

echo "inramfs-sqlite-e2e: PASS"
echo "  $(grep -E '^\[|^PASS' "$LOG" | sed 's/^/    /')"
