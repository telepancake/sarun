#!/usr/bin/env python3
"""m2 conformance: the RUST engine (engine/) serves the control protocol well
enough that the PYTHON clients work against it unmodified — RemoteSupervisor
verbs, the subscribe event feed, the single-instance guard, namespaced paths,
clean SIGTERM teardown. The box on disk is created by the Python module (the
sqlar format is read by rusqlite on the other side). Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_engine_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import json, os, socket, stat as stat_mod, subprocess, sys, tempfile, shutil, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/sarun"
CRATE = Path("/home/user/sarun/engine")
BIN = CRATE / "target/release/sarun-engine"

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


def main():
    if not ensure_binary():
        print("  ok  engine-rs: cargo/binary unavailable — SKIP")
        print("\nENGINE-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="engrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # A finished box on disk, written by the PYTHON engine's storage code.
        sid = "9001"
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())
        idx.set_entry("rs.txt", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id("rs.txt"))
        bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(b"rust!\n")
        m.consolidate(str(backing), sid, index=idx)
        idx.close()
        m.sqlar_meta_set(m.sqlar_path(sid), "name", "RSBOX")
        shutil.rmtree(backing, ignore_errors=True)   # at rest: sqlar only

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        check("slopbox.RS" in sock,
              "engine-rs: socket lives at the NAMESPACED path")
        if not wait_socket(sock):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        check(m.ui_is_running(sock), "engine-rs: ui_is_running sees the engine")

        r = subprocess.run([str(BIN), "serve"], capture_output=True,
                           text=True, timeout=15)
        check(r.returncode == 4 and "already running" in r.stderr,
              "engine-rs: second instance refused with the same exit code")

        rsup = m.RemoteSupervisor(sock)
        sd = rsup.session_dicts()
        check(any(d.get("session_id") == sid and d.get("name") == "RSBOX"
                  for d in sd),
              "engine-rs: session_dicts reads the Python-written sqlar")
        check(sid in rsup.sessions, "engine-rs: .sessions facade works")
        check(rsup.display_path(sid) == "RSBOX",
              "engine-rs: display_path resolves the NAME label")
        procs = rsup.processes(sid)
        check(procs and any(p[0] == wid for p in procs),
              "engine-rs: processes(sid) reads the process table")
        st = rsup.review._state()
        check(st["consolidating"] == [] and st["consolidated"] == [],
              "engine-rs: review_state answers (no folds — by design, D4)")
        check(rsup.review._live(sid) is False, "engine-rs: review_live answers")
        try:
            rsup._rpc("definitely_not_a_verb")
            check(False, "engine-rs: unknown verb must raise")
        except m.RemoteError as e:
            check("unknown verb" in str(e), "engine-rs: unknown verb refused")
        rep = m.sync_request(sock, type="register", session_id="X", cmd=["true"])
        check(rep is not None and rep.get("ok") is False,
              "engine-rs: register refused politely (m2: no boxes yet)")
        rep = m.sync_request(sock, type="nonsense")
        check(rep is not None and rep.get("ok") is False
              and "unknown control type" in (rep.get("error") or ""),
              "engine-rs: unknown control type gets an explicit error")

        # subscribe feed: ack first, then a broadcast triggered by `ping`.
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sc:
            sc.settimeout(10); sc.connect(sock)
            sc.sendall(b'{"type":"subscribe"}\n')
            f = sc.makefile("rb")
            ack = json.loads(f.readline())
            check(ack.get("ok") is True, "engine-rs: subscribe acked")
            check(rsup._rpc("ping") == "pong", "engine-rs: ping verb answers")
            ev = json.loads(f.readline())
            check(ev.get("type") == "pong",
                  "engine-rs: broadcast event arrives on the subscribed conn")

        eng.terminate()
        try: eng.wait(timeout=10)
        except subprocess.TimeoutExpired:
            eng.kill(); eng.wait(timeout=5)
        check(eng.returncode == 0, "engine-rs: SIGTERM exits 0")
        check(not Path(sock).exists(),
              "engine-rs: socket removed on shutdown")
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        if eng is not None and eng.poll() is None:
            eng.kill()
            try: eng.wait(timeout=5)
            except Exception: pass
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("ENGINE-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_engine_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
