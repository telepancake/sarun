#!/usr/bin/env python3
"""End-to-end: start the real UI process, run a real `slopbox CMD` against it, and
verify (a) the box writes/modifies/deletes through the UI-served overlay, (b) the UI
consolidates on exit into patch/sqlar/tombstones + provenance, (c) the mount table
is clean afterwards, and (d) `slopbox CMD` with NO UI fails fast.

    /home/user/venv/bin/python test_e2e.py

Self-safety: isolated XDG temp tree; the UI is launched headless and killed in a
finally; the overlay is lazy-unmounted on the way out.
"""
import os, sys, time, signal, socket, subprocess, tempfile, shutil, sqlite3, json
import stat as stat_mod
from pathlib import Path
from importlib.machinery import SourceFileLoader

PYBIN = "/home/user/venv/bin/python"
SARUN = "/home/user/sarun/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def env_for(tmp):
    e = dict(os.environ)
    e["XDG_STATE_HOME"] = str(tmp/"state")
    e["XDG_RUNTIME_DIR"] = str(tmp/"run")
    e["XDG_CONFIG_HOME"] = str(tmp/"config")
    e["XDG_DATA_HOME"] = str(tmp/"data")
    # headless textual so the UI process runs without a real terminal
    e["TEXTUAL"] = ""
    e.setdefault("TERM", "dumb")
    return e


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.2)
    return False


def test_no_ui_fails_fast(tmp):
    e = env_for(tmp)
    r = subprocess.run([PYBIN, SARUN, "true"], env=e,
                       capture_output=True, text=True, timeout=30)
    check(r.returncode != 0, "slopbox CMD with no UI exits non-zero")
    check("UI is not running" in r.stderr or "not running" in r.stderr.lower(),
          "no-UI run prints a clear 'UI not running' message")


def run_with_ui(tmp):
    m = SourceFileLoader("slopbox", SARUN).load_module()
    e = env_for(tmp)
    sock = str(Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock")
    mnt = Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "mnt"
    # launch the UI headless. Run() needs a screen; use textual's headless driver by
    # importing and calling run with headless. Simplest: drive run_ui in a subprocess
    # with a pty-less headless app via the App.run(headless=...) — not exposed on the
    # CLI, so we spawn a tiny harness that runs the app headless.
    harness = tmp / "ui_harness.py"
    harness.write_text(
        "import os\n"
        "from importlib.machinery import SourceFileLoader\n"
        f"m = SourceFileLoader('slopbox', {SARUN!r}).load_module()\n"
        "m.ensure_dirs()\n"
        "app = m._make_ui_app()()\n"
        "app.run(headless=True)\n")
    ui = subprocess.Popen([PYBIN, str(harness)], env=e,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        if not wait_socket(sock, 30):
            out = b""
            try: out = ui.stdout.read(4000) if ui.stdout else b""
            except Exception: pass
            raise RuntimeError(f"UI socket never appeared. UI output:\n{out.decode(errors='replace')}")

        # pick a victim host file we can safely 'delete' inside the overlay
        # (only the overlay view changes; the host file is never touched)
        victim = "etc/hostname"
        host_victim = Path("/") / victim
        host_before = host_victim.read_bytes() if host_victim.exists() else None

        script = (
            "set -e; "
            "echo hello-overlay > /root_newfile.txt; "      # created text
            # binary: deterministic NUL-containing bytes so it is unambiguously
            # non-text (random bytes occasionally lack a NUL and would fold as text).
            "mkdir -p /newdir && printf 'BIN\\x00\\x01\\x02\\xff\\xfe' > /newdir/blob.bin; "
            "ln -s /target /newlink; "                      # symlink
            f"rm -f /{victim}; "                            # delete a host file (overlay)
            "true")
        r = subprocess.run(
            [PYBIN, SARUN, "-n", "bash", "-c", script],
            env=e, capture_output=True, text=True, timeout=120)
        check(r.returncode == 0, f"slopbox run exited 0 (got {r.returncode}: {r.stderr.strip()[-300:]})")
        check("UI connected" in r.stderr, "runner reports the UI connected")

        # host victim untouched
        host_after = host_victim.read_bytes() if host_victim.exists() else None
        check(host_after == host_before and host_after is not None,
              "host file the box 'deleted' is untouched on the real host")

        # the UI consolidates on unregister; poll for the stores rather than racing
        # a fixed sleep (consolidation can lag on a loaded machine).
        state = Path(e["XDG_STATE_HOME"]) / "slopbox"
        deadline = time.time() + 30
        while time.time() < deadline:
            if list(state.glob("*.patch.xz")) and list(state.glob("*.sqlar")):
                break
            time.sleep(0.3)
        patches = list(state.glob("*.patch.xz"))
        sqlars = list(state.glob("*.sqlar"))
        check(len(patches) == 1, f"exactly one patch.xz produced (got {len(patches)})")
        check(len(sqlars) == 1, f"exactly one sqlar produced (got {len(sqlars)})")

        if patches:
            files = m.parse_patch(m.read_patch_file(patches[0]))
            check("root_newfile.txt" in files, "created text file folded into the patch")
        if sqlars:
            sp = sqlars[0]
            names = {n for n, _md, _mt, _sz in m.sqlar_list(sp)}
            check("newdir/blob.bin" in names, "binary file captured in the sqlar")
            check("newlink" in names, "symlink captured in the sqlar")
            check(victim in names and stat_mod.S_ISCHR(m.sqlar_mode(sp, victim) or 0),
                  "host-file deletion captured as a tombstone (from index.db)")
            # provenance present
            conn = sqlite3.connect(str(sp))
            try:
                prov = conn.execute(
                    "SELECT path,pid,exe,argv FROM provenance").fetchall()
            except sqlite3.Error:
                prov = []
            finally:
                conn.close()
            check(len(prov) >= 1, "provenance table populated in the sqlar")
            if prov:
                check(all(int(p[1]) > 0 for p in prov), "every provenance row has a pid")

        # backing live/<sid> is gone (consolidated + cleaned). Poll: the unregister
        # handler removes the backing right AFTER writing the stores, so the stores
        # can momentarily exist before the dir is gone.
        live = Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "live"
        deadline = time.time() + 15
        leftover = ["x"]
        while time.time() < deadline:
            leftover = [d for d in (live.iterdir() if live.exists() else [])
                        if (d/"up").is_dir()]
            if not leftover: break
            time.sleep(0.3)
        check(not leftover, "live/ is empty at rest after teardown")

        # the mount is still up (UI alive) but the synthetic root is empty again
        ls = subprocess.run(["timeout","10","ls",str(mnt)], capture_output=True, text=True)
        check(ls.returncode == 0 and ls.stdout.strip() == "",
              "overlay synthetic root empty again after the box exits")
    finally:
        # shut the UI down
        try:
            ui.send_signal(signal.SIGINT)
            ui.wait(timeout=10)
        except Exception:
            try: ui.kill(); ui.wait(timeout=5)
            except Exception: pass
        # the mount must be gone now
        time.sleep(0.5)
        still = os.path.ismount(str(mnt))
        if still:
            try:
                subprocess.run(["fusermount3","-uz",str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=10)
            except Exception: pass
        check(not still, "overlay unmounted after the UI exits (clean mount table)")


def main():
    tmp = Path(tempfile.mkdtemp(prefix="e2e-"))
    try:
        print("== no-UI fail-fast ==")
        test_no_ui_fails_fast(tmp)
        print("\n== end-to-end with the UI ==")
        run_with_ui(tmp)
    except Exception as ex:
        import traceback; traceback.print_exc(); _fails.append(str(ex))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("E2E PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


if __name__ == "__main__":
    sys.exit(main())
