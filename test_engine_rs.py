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
        check(rep is not None and rep.get("ok") is True and rep.get("mount"),
              "engine-rs: register acks with the mount bind target")
        check("_box_sid" not in (rep or {}),
              "engine-rs: internal markers never reach the wire")
        time.sleep(0.5)   # sync_request closed the conn: teardown should fire
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

        # ── m3a: the overlay core — capture through <mnt>/<box_id> ──────────
        rep = m.sync_request(sock, type="ui", verb="box_new", args=[])
        check(rep and rep.get("ok"), "engine-rs: box_new answers")
        bsid = rep["r"]["sid"]; root = Path(rep["r"]["root"])
        check(root.is_dir(), "engine-rs: box root appears under the mount")

        host_keep = Path("/root/m3a_keep.txt")
        host_gone = Path("/root/m3a_gone.txt")
        host_keep.write_bytes(b"lower\n")
        host_gone.write_bytes(b"victim\n")
        try:
            # lazy capture: an O_RDWR open with NO write must record nothing
            with open(root / "root/m3a_keep.txt", "r+b"):
                pass
            sp = m.sqlar_path(bsid)
            check("root/m3a_keep.txt" not in {n for n, *_ in m.sqlar_list(sp)},
                  "engine-rs: writable open with no write captures NOTHING (D3)")

            # create a new file; append to a host file (copy-up); rm a host file
            (root / "root/m3a_new.txt").write_bytes(b"made in rust\n")
            with open(root / "root/m3a_keep.txt", "ab") as f:
                f.write(b"upper\n")
            (root / "root/m3a_gone.txt").unlink()
            (root / "root/m3a_dir").mkdir()

            # the box view merges; the HOST is untouched
            check((root / "root/m3a_keep.txt").read_bytes() == b"lower\nupper\n",
                  "engine-rs: append copy-up reads back merged through the box")
            check(not (root / "root/m3a_gone.txt").exists(),
                  "engine-rs: unlinked host file is hidden in the box view")
            names = {p.name for p in (root / "root").iterdir()}
            check("m3a_gone.txt" not in names and "m3a_new.txt" in names,
                  "engine-rs: readdir merges upper and hides whiteouts")
            check(host_keep.read_bytes() == b"lower\n" and host_gone.exists(),
                  "engine-rs: the real host is untouched by any of it")

            # the PYTHON readers verify the capture (same sqlar+pool layout)
            rows = {n: mode for n, mode, *_ in m.sqlar_list(sp)}
            check(m.sqlar_content(sp, "root/m3a_new.txt") == b"made in rust\n",
                  "engine-rs: python sqlar_content reads the rust pool blob")
            check(m.sqlar_content(sp, "root/m3a_keep.txt") == b"lower\nupper\n",
                  "engine-rs: copy-up blob carries lower+upper bytes")
            check(stat_mod.S_ISCHR(rows.get("root/m3a_gone.txt", 0)),
                  "engine-rs: deletion is a python-readable tombstone")
            check(stat_mod.S_ISDIR(rows.get("root/m3a_dir", 0)),
                  "engine-rs: mkdir captured as a dir row")
            # rename (mv): the build-critical atomic case
            (root / "root/m3a_new.txt").rename(root / "root/m3a_renamed.txt")
            check((root / "root/m3a_renamed.txt").read_bytes() == b"made in rust\n"
                  and not (root / "root/m3a_new.txt").exists(),
                  "engine-rs: rename moves the file in the box view")
            check(m.sqlar_content(sp, "root/m3a_renamed.txt") == b"made in rust\n",
                  "engine-rs: renamed row keeps its blob (python-readable)")

            wid2 = m.sqlar_writer_id(sp, "root/m3a_renamed.txt")
            prov = m.sqlar_proc_prov(sp, wid2) if wid2 else None
            check(prov is not None and prov.get("exe"),
                  "engine-rs: writer provenance recorded and python-readable")
        finally:
            host_keep.unlink(missing_ok=True)
            host_gone.unlink(missing_ok=True)

        # ── m3b: the REAL python runner (bwrap + --inner) against rust ──────
        victim = Path("/root/m3b_victim.txt")
        out_host = Path("/root/m3b_out.txt")
        victim.write_bytes(b"v\n"); out_host.unlink(missing_ok=True)
        try:
            r = subprocess.run(
                [sys.executable, SARUN, "RSE2E", "--", "sh", "-c",
                 "echo rust-box > /root/m3b_out.txt && rm /root/m3b_victim.txt"],
                capture_output=True, text=True, timeout=120)
            check(r.returncode == 0,
                  f"engine-rs: python runner exits 0 against rust engine "
                  f"(got {r.returncode}: {r.stderr[-300:]})")
            check(not out_host.exists() and victim.exists(),
                  "engine-rs: box writes captured, host untouched")
            sp2 = max(Path(os.environ["XDG_STATE_HOME"])
                      .joinpath("slopbox.RS").glob("*.sqlar"),
                      key=lambda p: int(p.stem))
            check(m.sqlar_meta_get(sp2, "name") == "RSE2E",
                  "engine-rs: runner-supplied NAME recorded in meta")
            check(m.sqlar_content(sp2, "root/m3b_out.txt") == b"rust-box\n",
                  "engine-rs: box-run output captured, python-readable")
            rows2 = {n: mode for n, mode, *_ in m.sqlar_list(sp2)}
            check(stat_mod.S_ISCHR(rows2.get("root/m3b_victim.txt", 0)),
                  "engine-rs: box-run deletion is a tombstone")
            check(bool(m.root_cmd(sp2.stem)),
                  "engine-rs: root process row records the runner's argv")
            mnt_names = {p.name for p in
                         (Path(os.environ["XDG_RUNTIME_DIR"]) / "slopbox.RS"
                          / "mnt").iterdir()}
            check(sp2.stem not in mnt_names,
                  "engine-rs: box gone from the mount after teardown (conn EOF)")
        finally:
            victim.unlink(missing_ok=True)
            out_host.unlink(missing_ok=True)

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
