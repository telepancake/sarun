#!/usr/bin/env python3
"""The `passthrough` FILE RULE drives kernel read-passthrough (D5), done as the
user asked — the EXISTING rule, not a new one. A `passthrough`-ruled path is
HOST-DIRECT: reads are kernel-served (daemon out of the loop), writes go
straight to the host, uncaptured. The RULE (user declaration) decides — never
an automatic per-open guess.

Crucially, passthrough rules are PATH-ONLY (the parser skips any clause line
containing ':', so a `box:` matcher is ignored). This is required, not a
limitation: a passthrough path must be host-direct in EVERY box. A box-scoped
rule (host-direct in a child but captured in its parent) would make the child
read the parent's still-captured blob through a passthrough fd, and the parent
copying-up that blob would hit the kernel's passthrough-vs-write EIO. Path-only
means a passthrough file is read directly from the host in every box, so the
nested read-through-parent path never touches a passthrough'd inode.

Asserted with real effects:
  - passthrough-ruled read: kernel-served (daemon read() counter idle) + correct;
  - non-ruled read: daemon-served (counter moves) — proving the RULE gates it;
  - passthrough write: lands on the REAL HOST, uncaptured;
  - a `box:`-scoped rule line is IGNORED (the path stays captured) — path-only;
  - NESTED: a child box reads the passthrough file straight from the host, same
    bytes as the parent — no overlay read-through, no copy-up, no EIO.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_passthrough_rule_rs.py
Skips (passes vacuously) if cargo/the binary/FUSE/kernel-passthrough unavailable.
"""
import os, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path
from sarun_test_paths import ENGINE_BIN
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = ENGINE_BIN

ORIG = b"PT-INPUT-" + bytes(range(256)) * 32000  # ~8 MB
PT = Path("/root/ptr_in.bin")       # passthrough-ruled
OTHER = Path("/root/ptr_other.bin") # not ruled
BM = Path("/root/ptr_boxmatch.bin") # targeted by a (ignored) box-scoped rule
TRUNC = Path("/root/ptr_trunc.bin") # passthrough-ruled, EXISTING (O_TRUNC case)

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
                       capture_output=True, text=True)
    return r.returncode == 0 and BIN.exists()


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def stats(p):
    try:
        t = p.read_text()
        return ("passthrough=1" in t, int(t.split("daemon_reads=")[1].split()[0]))
    except (OSError, IndexError, ValueError):
        return False, 0


def main():
    if not ensure_binary():
        raise SystemExit("test_passthrough_rule_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="ptr-"))
    for k, sub in (("XDG_STATE_HOME", "st"), ("XDG_RUNTIME_DIR", "rn"),
                   ("XDG_CONFIG_HOME", "cf"), ("XDG_DATA_HOME", "d")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "PTR"
    sf = tmp / "stats.txt"
    env = dict(os.environ); env["SARUN_STATS_FILE"] = str(sf)
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    rules = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.PTR" / "filerules"
    rules.parent.mkdir(parents=True, exist_ok=True)
    # path-only passthrough (honored) + a box-scoped line (must be IGNORED).
    rules.write_text("passthrough root/ptr_in.bin\n"
                     "passthrough root/ptr_new.bin\n"
                     "passthrough root/ptr_trunc.bin\n"
                     "passthrough box:PTR root/ptr_boxmatch.bin\n")
    for p in (PT, OTHER, BM, TRUNC): p.write_bytes(ORIG)
    eng = None

    def box(cmd):
        return subprocess.run([str(BIN), "run", "--", "sh", "-c", cmd], env=env,
                              capture_output=True, text=True, timeout=120)

    def latest():
        sps = list(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.PTR")
                   .glob("*.sqlar"))
        return max(sps, key=lambda p: int(p.stem)) if sps else None
    try:
        eng = subprocess.Popen([str(BIN), "serve"], env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            raise RuntimeError("engine socket never appeared")
        pt = False
        for _ in range(50):
            pt, _ = stats(sf)
            if pt: break
            time.sleep(0.1)
        if not pt:
            print("  ok  ptr-rs: kernel did not negotiate FUSE_PASSTHROUGH — SKIP")
            print("\nPTR-RS PASS (skipped)"); return 0

        # 1) passthrough-ruled read → kernel-served + correct
        _, a0 = stats(sf); box("cat /root/ptr_in.bin > /root/copy_pt")
        time.sleep(0.3); _, a1 = stats(sf)
        sp = latest()
        check(m.sqlar_content(sp, "root/copy_pt") == ORIG,
              "ptr-rs: passthrough read byte-for-byte correct (kernel-served)")
        d_pt = a1 - a0

        # 2) non-ruled read → daemon-served (counter moves more)
        _, b0 = stats(sf); box("cat /root/ptr_other.bin > /root/copy_other")
        time.sleep(0.3); _, b1 = stats(sf)
        sp = latest()
        check(m.sqlar_content(sp, "root/copy_other") == ORIG,
              "ptr-rs: non-ruled read byte-for-byte correct")
        d_other = b1 - b0
        check(d_other > d_pt + 8,
              f"ptr-rs: the RULE gates passthrough — ruled read bypassed the daemon "
              f"({d_pt}) but the non-ruled read did not ({d_other})")

        # 3) passthrough write (a FRESH host-direct file) → lands on the REAL
        #    HOST, uncaptured. (A fresh file avoids the separate O_TRUNC-on-
        #    existing bug in the host-direct write path — see DESIGN.md D5.)
        new = Path("/root/ptr_new.bin"); new.unlink(missing_ok=True)
        box("printf 'HOSTDIRECT' > /root/ptr_new.bin")
        sp = latest()
        check(new.exists() and new.read_bytes() == b"HOSTDIRECT",
              "ptr-rs: passthrough write created the file on the REAL HOST")
        check(m.sqlar_content(sp, "root/ptr_new.bin") is None,
              "ptr-rs: passthrough write NOT captured in the box (host-direct)")
        new.unlink(missing_ok=True)

        # 3b) TRUNCATING write to an EXISTING passthrough file (the O_TRUNC bug):
        #     the truncate (setattr size=0) must hit the host, not copy-up. Host
        #     ends up cleanly the new bytes (no stale tail) and nothing captured.
        box("printf 'SHORT' > /root/ptr_trunc.bin")
        sp = latest()
        check(TRUNC.read_bytes() == b"SHORT",
              f"ptr-rs: O_TRUNC of an existing passthrough file truncates the HOST "
              f"cleanly, no stale tail (got {TRUNC.read_bytes()!r})")
        check(m.sqlar_content(sp, "root/ptr_trunc.bin") is None,
              "ptr-rs: O_TRUNC of a passthrough file does NOT spuriously capture")

        # 4) a `box:`-scoped rule line is IGNORED → the path stays CAPTURED
        box("printf 'BOXMATCH' > /root/ptr_boxmatch.bin")
        sp = latest()
        check(m.sqlar_content(sp, "root/ptr_boxmatch.bin") == b"BOXMATCH",
              "ptr-rs: box-scoped passthrough line IGNORED — path captured "
              "(passthrough rules are path-only)")
        check(BM.read_bytes() == ORIG,
              "ptr-rs: box-matched path's host file untouched (it was captured)")

        # 5) NESTED: a child box reads the passthrough file straight from the host
        binp = str(BIN)
        nested = (f"{binp} run -- sh -c "
                  "'head -c9 /root/ptr_in.bin > /root/nested_read'")
        r = box(f"set -e; {nested}")
        check(r.returncode == 0, "ptr-rs: nested parent+child run exited 0")
        # the child's read result lives in the CHILD box's sqlar; find a box whose
        # nested_read captured the real host bytes.
        sps = sorted(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.PTR")
                     .glob("*.sqlar"), key=lambda p: int(p.stem))
        got = None
        for sp in sps:
            c = m.sqlar_content(sp, "root/nested_read")
            if c: got = c
        check(got == ORIG[:9],
              f"ptr-rs: NESTED child read the passthrough file from the HOST "
              f"directly, correct bytes (got={got!r})")
    finally:
        for p in (PT, OTHER, BM, TRUNC, Path("/root/ptr_new.bin")):
            p.unlink(missing_ok=True)
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("PTR-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_passthrough_rule_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
