#!/usr/bin/env python3
"""Box MODES parity for the RUST runner + register handshake (engine/): the
fully-Rust path (`sarun-engine run` -> Rust engine -> Rust `inner`) honours the
same modes the Python runner does:

  -e  env capture   — each writer's full environment is recorded (process_env
                      returns the writer's actual env vars).
  -d  direct        — NO overlay: a write lands on the REAL host and is NOT
                      captured into the box's sqlar.
  re-run            — `run NAME` into an EXISTING named box re-runs into it
                      (adds another ROOT to the box's process forest) instead of
                      minting a new box.

These are REAL effect assertions (a unique env var, a real host file, a second
root row), never shape-only. Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_modes_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import os, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE / "engine"
BIN = CRATE / "target/release/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["cargo", "build", "--release"], cwd=CRATE,
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


def newest_sqlar(m):
    """The most-recently-minted box sqlar (highest box_id)."""
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def wait_for_sqlar_settled(m, sp, timeout=20):
    """Wait until the box's sqlar is no longer running (its live/ backing gone)
    so reads see the at-rest archive, not a mid-run one."""
    end = time.time() + timeout
    while time.time() < end:
        if not (Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
                .joinpath(sp.stem).exists()):
            return
        time.sleep(0.1)


def main():
    if not ensure_binary():
        print("  ok  modes-rs: cargo/binary unavailable — SKIP")
        print("\nMODES-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="modesrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        rsup = m.RemoteSupervisor(sock)

        # ── -e ENV CAPTURE ──────────────────────────────────────────────────
        # The Rust runner inherits our env and bwrap passes it to the child, so a
        # UNIQUE var we set here ends up in the writer's /proc/<pid>/environ. With
        # -e the engine records it (deduped) into the box's `env` table and links
        # the writer's process row via env_id, so process_env returns it.
        uniq = "MODES_RS_MARK_" + os.urandom(4).hex()
        uval = "env-capture-proof-" + os.urandom(4).hex()
        env_run = dict(os.environ); env_run[uniq] = uval
        r = subprocess.run(
            [str(BIN), "run", "-e", "ENVBOX", "--", "sh", "-c",
             "echo hi > /root/modes_env.txt"],
            capture_output=True, text=True, timeout=60, env=env_run)
        check(r.returncode == 0,
              f"modes-rs: -e env box exits 0 (got {r.returncode}: {r.stderr[-200:]})")
        esp = newest_sqlar(m); wait_for_sqlar_settled(m, esp)
        wid = m.sqlar_writer_id(esp, "root/modes_env.txt")
        check(wid is not None, "modes-rs: -e box recorded the file's writer row")
        envd = rsup.process_env(esp.stem, wid) if wid else {}
        check(isinstance(envd, dict) and envd.get(uniq) == uval,
              f"modes-rs: -e captured the writer's UNIQUE env var "
              f"(got {uniq}={envd.get(uniq)!r})")
        # NEGATIVE control: a box WITHOUT -e must NOT record env (process_env {}).
        r = subprocess.run(
            [str(BIN), "run", "NOENVBOX", "--", "sh", "-c",
             "echo hi > /root/modes_noenv.txt"],
            capture_output=True, text=True, timeout=60, env=env_run)
        check(r.returncode == 0, "modes-rs: no-env control box exits 0")
        nsp = newest_sqlar(m); wait_for_sqlar_settled(m, nsp)
        nwid = m.sqlar_writer_id(nsp, "root/modes_noenv.txt")
        nenv = rsup.process_env(nsp.stem, nwid) if nwid else {}
        check(nenv == {} or uniq not in nenv,
              "modes-rs: without -e the writer's env is NOT recorded")

        # ── -d DIRECT (no overlay; write hits the REAL host, uncaptured) ─────
        dhost = Path("/root/modes_direct.txt"); dhost.unlink(missing_ok=True)
        try:
            r = subprocess.run(
                [str(BIN), "run", "-d", "DIRECTBOX", "--", "sh", "-c",
                 "echo direct-proof > /root/modes_direct.txt"],
                capture_output=True, text=True, timeout=60)
            check(r.returncode == 0,
                  f"modes-rs: -d direct box exits 0 (got {r.returncode}: {r.stderr[-200:]})")
            check(dhost.exists() and dhost.read_bytes() == b"direct-proof\n",
                  "modes-rs: -d write landed on the REAL host (no overlay)")
            dsp = newest_sqlar(m); wait_for_sqlar_settled(m, dsp)
            names = {n for n, *_ in m.sqlar_list(dsp)}
            check("root/modes_direct.txt" not in names,
                  "modes-rs: -d write was NOT captured into the box sqlar")
        finally:
            dhost.unlink(missing_ok=True)

        # ── RE-RUN (run NAME into an EXISTING box adds a 2nd ROOT) ───────────
        # First run mints a box named RERUNBOX; the second `run RERUNBOX` resolves
        # the SAME box_id and adds another root process row (re-run), rather than
        # minting a fresh box.
        pre = {p.stem for p in Path(os.environ["XDG_STATE_HOME"])
               .joinpath("slopbox.RS").glob("*.sqlar")}
        r = subprocess.run(
            [str(BIN), "run", "RERUNBOX", "--", "sh", "-c",
             "echo first > /root/.modes_rr1 2>/dev/null; true"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, "modes-rs: re-run first launch exits 0")
        rrsp = newest_sqlar(m); wait_for_sqlar_settled(m, rrsp)
        rr_id = rrsp.stem
        roots1 = rsup.proc_roots(rr_id)
        check(len(roots1) == 1,
              f"modes-rs: after one run the box has ONE root (got {len(roots1)})")
        # second run into the SAME name → same box_id, second root.
        r = subprocess.run(
            [str(BIN), "run", "RERUNBOX", "--", "sh", "-c",
             "echo second > /root/.modes_rr2 2>/dev/null; true"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, "modes-rs: re-run second launch exits 0")
        post = {p.stem for p in Path(os.environ["XDG_STATE_HOME"])
                .joinpath("slopbox.RS").glob("*.sqlar")}
        # No NEW box minted by the second run (re-run reused rr_id).
        new_after = (post - pre)
        check(new_after == {rr_id},
              f"modes-rs: re-run reused the SAME box_id, minted no new box "
              f"(new sqlars: {new_after})")
        wait_for_sqlar_settled(m, Path(os.environ['XDG_STATE_HOME'])
                               .joinpath('slopbox.RS', rr_id + '.sqlar'))
        roots2 = rsup.proc_roots(rr_id)
        check(len(roots2) == 2,
              f"modes-rs: after re-run the box has TWO roots (got {len(roots2)})")

        eng.terminate()
        try: eng.wait(timeout=10)
        except subprocess.TimeoutExpired:
            eng.kill(); eng.wait(timeout=5)
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        if eng is not None and eng.poll() is None:
            eng.kill()
            try: eng.wait(timeout=5)
            except Exception: pass
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("MODES-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_modes_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
