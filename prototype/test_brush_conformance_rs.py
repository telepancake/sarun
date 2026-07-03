#!/usr/bin/env python3
"""bash-conformance corpus for the embedded brush shell, through a REAL box.

Each probe is a small shell script exercising one bash behavior family
(parameter expansion, arrays, quoting/IFS, heredocs, [[ ]]/(( )), traps,
set -euo pipefail, redirections, builtins, brace/glob expansion). Every
probe runs twice:

  host:  bash probe.sh            (the reference)
  box:   sarun run -b -- driver   (each probe via `bash probe.sh` — brush
                                   in BASH mode; the box shadows bash too)

stdout+stderr+exit of each probe are diffed. A divergence not listed in
XFAIL fails the suite — the same hard-gate rule as the kati corpus, so
brush/bash conformance can only move forward. XFAIL entries are visible
debt with a reason string, not silent skips.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_brush_conformance_rs.py
Skips (passes vacuously) if the engine binary is unavailable.
"""
import os, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
BIN = _HERE.parent / "engine/target/x86_64-unknown-linux-musl/release/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)

# name -> script. Deterministic only: no $RANDOM, no PIDs, no timing.
PROBES = {
    # ── parameter expansion ────────────────────────────────────────────────
    "pe_defaults": '''u=; s=set
echo "[${u:-d}] [${s:-d}] [${u-d}] [${nou-d}] [${nou:-d}]"
echo "[${u:+alt}] [${s:+alt}]"
v=; : ${v:=assigned}; echo "[$v]"''',
    "pe_trim": '''p=aa.bb.cc
echo "[${p%.*}] [${p%%.*}] [${p#*.}] [${p##*.}]"
f=/x/y/z.tar.gz
echo "[${f##*/}] [${f%/*}]"''',
    "pe_subst": '''v="aa bb aa"
echo "[${v/aa/X}] [${v//aa/X}] [${v/#aa/X}] [${v/%aa/X}]"
echo "[${v/ /_}]"''',
    "pe_len_slice": '''v=abcdefgh
echo "[${#v}] [${v:2:3}] [${v:5}] [${v: -3}] [${v:1:-2}]"''',
    "pe_case_mod": '''v="mIxEd Case"
echo "[${v^^}] [${v,,}] [${v^}] [${v,}]"''',
    "pe_indirect": '''real=hello; name=real
echo "[${!name}]"''',
    "lineno": '''echo "L1=$LINENO"
echo "L2=$LINENO"
as_lineno_1=$LINENO as_lineno_1a=$as_lineno_1
as_lineno_2=$LINENO as_lineno_2a=$as_lineno_2
eval "test \\"x$as_lineno_1$as_lineno_1a\\" != \\"x$as_lineno_2$as_lineno_2a\\"" && test "x`expr $as_lineno_1 + 1`" = "x$as_lineno_2" && echo LINENO-works''',
    "pe_unset_err": '''(set -u; echo "${undefined_var}") 2>/dev/null; echo "rc=$?"''',
    # ── arrays ─────────────────────────────────────────────────────────────
    "arr_basic": '''a=(one two three)
echo "[${a[0]}] [${a[2]}] [${#a[@]}] [${a[@]}]"
a+=(four); echo "[${#a[@]}] [${a[3]}]"''',
    "arr_quote_split": '''a=("x y" z)
for w in "${a[@]}"; do echo "w=[$w]"; done
for w in ${a[@]}; do echo "u=[$w]"; done''',
    "arr_slice_keys": '''a=(p q r s)
echo "[${a[@]:1:2}] [${!a[@]}]"
unset 'a[1]'; echo "[${!a[@]}] [${#a[@]}]"''',
    "arr_assoc": '''declare -A m
m[foo]=1; m[bar]=2
echo "[${m[foo]}] [${m[bar]}] [${#m[@]}]"''',
    # ── quoting / IFS / word splitting ─────────────────────────────────────
    "ifs_split": '''v="a:b:c"
IFS=: read -r x y z <<< "$v"
echo "[$x][$y][$z]"
IFS=:; set -- $v; echo "n=$# one=$1"; unset IFS''',
    "at_star": '''set -- "a b" c
echo "at:"; for w in "$@"; do echo "[$w]"; done
echo "star:"; for w in "$*"; do echo "[$w]"; done''',
    "dollar_quotes": '''printf '%s\\n' $'tab\\there' $'nl\\nline'"end"''',
    # ── heredocs ───────────────────────────────────────────────────────────
    "heredoc_expand": '''v=world
cat <<EOF
hello $v $(echo sub)
EOF
cat <<'EOF'
literal $v $(echo sub)
EOF''',
    "heredoc_dash": '''cat <<-EOF
	indented
	lines
	EOF
echo done''',
    "herestring": '''tr a-z A-Z <<< "lower case"''',
    # ── [[ ]] / (( )) / case ───────────────────────────────────────────────
    "cond_dbracket": '''v=hello.txt
[[ $v == *.txt ]] && echo glob-yes
[[ $v =~ ^h.*xt$ ]] && echo "re-yes [${BASH_REMATCH[0]}]"
[[ -n $v && $v != x ]] && echo and-yes''',
    "arith": '''x=5
echo "[$((x*2+1))] [$((x>3?1:0))] [$((0x10)) $((010))]"
(( x += 2 )); echo "[$x]"
for ((i=0;i<3;i++)); do printf '%d' "$i"; done; echo''',
    "case_fall": '''v=b
case $v in
  a) echo A;;
  b) echo B;&
  c) echo C;;&
  *) echo star;;
esac''',
    # ── functions / locals / return ────────────────────────────────────────
    "func_local": '''g=global
f() { local g=inner; echo "in=[$g]"; return 3; }
f; echo "rc=$? out=[$g]"''',
    "func_args": '''f() { echo "n=$# 1=[$1] all=[$*]"; shift; echo "after=[$1]"; }
f p q r''',
    # ── traps / exit ───────────────────────────────────────────────────────
    "trap_exit": '''trap 'echo trapped' EXIT
echo body''',
    "trap_exit_subshell_reset": '''trap 'echo parent-trap' EXIT
( echo in-sub )
( exit 5 ); echo "rc=$?"
echo body-done''',
    "bg_subshell_pid": '''( sleep 0.1 ) &
p=$!
test -n "$p" && echo "bg-pid-set"
wait $p
echo "waited=$?"''',
    "trap_exit_rc": '''(trap 'echo t' EXIT; exit 7); echo "rc=$?"''',
    # ── set -e / -u / pipefail ─────────────────────────────────────────────
    "errexit_basic": '''(set -e; false; echo not-reached); echo "rc=$?"
(set -e; false || true; echo reached); echo "rc=$?"''',
    "errexit_func": '''f() { false; echo in-f; }
(set -e; f; echo after); echo "rc=$?"
(set -e; if f; then :; fi; echo cond-ok); echo "rc=$?"''',
    "pipefail": '''(set -o pipefail; false | true); echo "rc=$?"
(false | true); echo "rc=$?"
false | true; echo "ps=[${PIPESTATUS[0]} ${PIPESTATUS[1]}]"''',
    # ── redirections ───────────────────────────────────────────────────────
    "redir_order": '''f() { echo out; echo err >&2; }
f 2>&1 | sort
f > /dev/null 2>&1; echo "silent=$?"''',
    "redir_amp": '''{ echo o; echo e >&2; } &> both.txt
sort both.txt; rm -f both.txt''',
    "exec_fd": '''exec 3> fd3.txt
echo to-three >&3
exec 3>&-
cat fd3.txt; rm -f fd3.txt''',
    # ── builtins ───────────────────────────────────────────────────────────
    "printf_fmt": '''printf '%05d|%-6s|%x|%%\\n' 42 ab 255
printf '%s,' a b c; echo
printf -v var 'got:%d' 9; echo "$var"''',
    "read_ifs": '''printf 'k=v\\n' | while IFS== read -r k v; do echo "[$k][$v]"; done''',
    "getopts_basic": '''set -- -a -b val rest
while getopts ab: o; do echo "o=$o a=${OPTARG-}"; done
shift $((OPTIND-1)); echo "rest=[$1]"''',
    "eval_basic": '''cmd='echo evaled $x'; x=1
eval "$cmd"''',
    # ── expansions ─────────────────────────────────────────────────────────
    "brace": '''echo {1..5} {a,b}x x{,y}''',
    "glob_basic": '''mkdir -p gd && touch gd/a.c gd/b.c gd/c.h
echo gd/*.c
echo gd/*.zz
rm -rf gd''',
    "cmdsub_nest": '''echo "[$(echo $(echo inner))]"
echo "[`echo back`]"''',
    "subshell_state": '''v=outer
(v=inner; echo "in=[$v]")
echo "out=[$v]"
{ v=grouped; }; echo "grp=[$v]"''',
}

# Known divergences: name -> reason. VISIBLE debt, not silent skips —
# an xpass (fixed entry still listed) fails the suite so this can't rot.
XFAIL = {
    # $! is empty after backgrounding a SUBSHELL — `( cmd ) &` doesn't
    # register a job pid. autoconf guards its uses, but bash sets it.
    "bg_subshell_pid": "$! empty after ( cmd ) & — background subshell job",
}


def main():
    if not BIN.exists():
        print("test_brush_conformance_rs: no engine binary (skip)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="brushconf-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
        (tmp / sub).mkdir(parents=True, exist_ok=True)
    os.environ["SLOPBOX_NS"] = "BC"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()

    work = Path("/root/brushconf_work")
    shutil.rmtree(work, ignore_errors=True)
    (work / "probes").mkdir(parents=True)
    names = sorted(PROBES)
    for n in names:
        (work / "probes" / f"{n}.sh").write_text(PROBES[n] + "\n")
    # host reference: bash (brush's compat target)
    ref = {}
    for n in names:
        r = subprocess.run(["bash", f"probes/{n}.sh"], cwd=work,
                           capture_output=True, text=True, timeout=30)
        ref[n] = f"{r.stdout}{r.stderr}rc={r.returncode}\n"
    # driver runs every probe through brush (`sh` in a -b box), one box total
    driver = ["mkdir -p out"]
    for n in names:
        driver.append(f"bash probes/{n}.sh > out/{n} 2>&1 < /dev/null; echo rc=$? >> out/{n}")
    (work / "driver.sh").write_text("\n".join(driver) + "\n")

    eng = subprocess.Popen([str(BIN), "serve"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    try:
        if not wait_socket_path(m.sock_path()):
            check(False, "engine socket appeared")
            return 1
        r = subprocess.run(
            [str(BIN), "run", "-b", "BRUSHCONF", "-C", str(work), "--",
             "sh", "driver.sh"],
            capture_output=True, text=True, timeout=300)
        check(r.returncode == 0,
              f"driver box exits 0 (got {r.returncode}: {r.stderr[-400:]})")
        sp = max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.BC")
                 .glob("*.sqlar"), key=lambda p: int(p.stem))
        n_fail = n_xfail = n_xpass = 0
        diverging = []
        for n in names:
            rel = str((work / "out" / n).resolve()).lstrip("/")
            got = (m.sqlar_content(sp, rel) or b"").decode(errors="replace")
            same = got == ref[n]
            if n in XFAIL:
                if same:
                    n_xpass += 1
                    check(False, f"{n}: XPASS — drop its XFAIL entry")
                else:
                    n_xfail += 1
            elif same:
                pass
            else:
                n_fail += 1
                diverging.append(n)
                print(f" FAIL {n}\n  --- bash ---\n{ref[n]}  --- brush ---\n{got}")
        check(n_fail == 0,
              f"brush matches bash on all non-XFAIL probes "
              f"({len(names)-n_fail-n_xfail}/{len(names)} pass, "
              f"xfail={n_xfail}, diverging={diverging})")
    finally:
        eng.terminate()
        try: eng.wait(timeout=10)
        except Exception: eng.kill()
        shutil.rmtree(work, ignore_errors=True)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("BRUSH-CONFORMANCE PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def wait_socket_path(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def test_brush_conformance_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
