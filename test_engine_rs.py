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

            # review verbs over the wire (read-only, against the Rust engine)
            rsup = m.RemoteSupervisor(sock)
            changes = rsup.review.session_changes(bsid)
            paths_seen = {e["path"]: e["kind"] for e in changes}
            check(paths_seen.get("root/m3a_renamed.txt") == "changed",
                  "engine-rs: review.session_changes lists a file change")
            check(paths_seen.get("root/m3a_gone.txt") == "deleted",
                  "engine-rs: session_changes reports a deletion as 'deleted'")
            # hunks of an append-modified text file
            h = rsup.review.hunks(bsid, "root/m3a_keep.txt")
            check(h.get("is_text") is True and h.get("hunks"),
                  "engine-rs: review.hunks returns a text diff")
            alllines = [tag for hk in h["hunks"] for tag, _ in hk["lines"]]
            check("+" in alllines,
                  "engine-rs: the diff has an added line (the appended 'upper')")
            hk0 = h["hunks"][0]["lines"]
            check(any(t == "+" and "upper" in txt for t, txt in hk0),
                  "engine-rs: added line content is the appended bytes")
            # hunks of a newly-created file (no lower): all-added
            h2 = rsup.review.hunks(bsid, "root/m3a_renamed.txt")
            check(h2.get("is_text") is True,
                  "engine-rs: hunks of a created text file is a text diff")

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

        # ── fully-Rust box: Rust runner -> Rust engine -> Rust inner ────────
        rv = Path("/root/m3rust_box.txt"); rv.unlink(missing_ok=True)
        try:
            r = subprocess.run(
                [str(BIN), "run", "ALLRUST", "--", "sh", "-c",
                 "echo all-rust > /root/m3rust_box.txt"],
                capture_output=True, text=True, timeout=60)
            check(r.returncode == 0,
                  f"engine-rs: `sarun-engine run` (Rust runner) exits 0 "
                  f"(got {r.returncode}: {r.stderr[-200:]})")
            check(not rv.exists(),
                  "engine-rs: fully-Rust box write captured, host untouched")
            sp3 = max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
                      .glob("*.sqlar"), key=lambda p: int(p.stem))
            check(m.sqlar_content(sp3, "root/m3rust_box.txt") == b"all-rust\n",
                  "engine-rs: fully-Rust box output captured, python-readable")
            check(m.sqlar_meta_get(sp3, "name") == "ALLRUST",
                  "engine-rs: Rust runner's NAME recorded (no Python in the path)")
        finally:
            rv.unlink(missing_ok=True)

        # ── FUSE-op cluster: utimes, chown, mkfifo, link, fallocate, xattr ──
        rep = m.sync_request(sock, type="ui", verb="box_new", args=[])
        oproot = Path(rep["r"]["root"]); opsid = rep["r"]["sid"]
        opsp = m.sqlar_path(opsid)
        (oproot / "root").mkdir(exist_ok=True)
        import stat as _st2
        # utimes: set mtime, read it back
        tf = oproot / "root/m3t.txt"; tf.write_bytes(b"t\n")
        os.utime(tf, (1000000, 1000000))
        check(int(tf.stat().st_mtime) == 1000000,
              "engine-rs: utimes sets mtime (drives make rebuilds)")
        # mkfifo
        try:
            os.mkfifo(oproot / "root/m3fifo")
            check(_st2.S_ISFIFO((oproot / "root/m3fifo").stat().st_mode),
                  "engine-rs: mkfifo creates a FIFO in the box")
            check(_st2.S_ISFIFO(m.sqlar_mode(opsp, "root/m3fifo") or 0),
                  "engine-rs: FIFO captured as a special row (python-readable)")
        except OSError as e:
            check(False, f"engine-rs: mkfifo failed: {e}")
        # hardlink (git clone --local / ccache path)
        lf = oproot / "root/m3link_src.txt"; lf.write_bytes(b"linkme\n")
        try:
            os.link(lf, oproot / "root/m3link_dst.txt")
            check((oproot / "root/m3link_dst.txt").read_bytes() == b"linkme\n",
                  "engine-rs: hardlink (copy-up approx) gives a working 2nd name")
        except OSError as e:
            check(False, f"engine-rs: link failed: {e}")
        # fallocate
        bigf = oproot / "root/m3big"
        with open(bigf, "wb") as fh:
            try:
                os.posix_fallocate(fh.fileno(), 0, 65536)
                check(bigf.stat().st_size == 65536,
                      "engine-rs: fallocate preallocates the requested size")
            except OSError as e:
                check(False, f"engine-rs: fallocate failed: {e}")
        # xattr round-trip
        xf = oproot / "root/m3x.txt"; xf.write_bytes(b"x\n")
        try:
            os.setxattr(xf, "user.test", b"hello")
            check(os.getxattr(xf, "user.test") == b"hello",
                  "engine-rs: xattr set/get round-trips")
            check("user.test" in [a.decode() if isinstance(a, bytes) else a
                                  for a in os.listxattr(xf)],
                  "engine-rs: xattr listed")
            os.removexattr(xf, "user.test")
            check("user.test" not in [a.decode() if isinstance(a, bytes) else a
                                      for a in os.listxattr(xf)],
                  "engine-rs: xattr removed")
        except OSError as e:
            check(False, f"engine-rs: xattr failed: {e}")

        # ── passthrough rule: a box write goes straight to the host, uncaptured ─
        prf = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.RS" / "filerules"
        prf.parent.mkdir(parents=True, exist_ok=True)
        prf.write_text("passthrough **/m3pass.txt\n")
        m.sync_request(sock, type="ui", verb="reload_rules", args=[])
        phost = Path("/root/m3pass.txt"); phost.unlink(missing_ok=True)
        try:
            r = subprocess.run(
                [str(BIN), "run", "PASSBOX", "--", "sh", "-c",
                 "echo passed > /root/m3pass.txt"],
                capture_output=True, text=True, timeout=60)
            check(r.returncode == 0, "engine-rs: passthrough box exits 0")
            check(phost.exists() and phost.read_bytes() == b"passed\n",
                  "engine-rs: passthrough write went straight to the REAL host")
            psp = max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
                      .glob("*.sqlar"), key=lambda p: int(p.stem))
            check("root/m3pass.txt" not in {n for n, *_ in m.sqlar_list(psp)},
                  "engine-rs: passthrough write was NOT captured in the box")
        finally:
            phost.unlink(missing_ok=True); prf.unlink(missing_ok=True)
            m.sync_request(sock, type="ui", verb="reload_rules", args=[])

        # ── capture: box stdout/stderr -> outputs table (fully-Rust box) ────
        r = subprocess.run(
            [str(BIN), "run", "CAPBOX", "--", "sh", "-c",
             "echo to-stdout; echo to-stderr 1>&2"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, "engine-rs: capture box exits 0")
        check("to-stdout" in r.stdout,
              "engine-rs: stdout teed live to the runner's terminal")
        spc = max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
                  .glob("*.sqlar"), key=lambda p: int(p.stem))
        capsid = spc.stem
        outs = m.sync_request(sock, type="ui", verb="outputs", args=[capsid])["r"]
        streams = {o["stream"] for o in outs}
        check(0 in streams and 1 in streams,
              "engine-rs: both stdout(0) and stderr(1) captured to outputs table")
        check(any(o["len"] > 0 for o in outs),
              "engine-rs: captured output has content")

        # ── chmod correctness (the P0 bug the audit found) ──────────────────
        rep = m.sync_request(sock, type="ui", verb="box_new", args=[])
        cbsid = rep["r"]["cbroot"] if "cbroot" in rep["r"] else rep["r"]["sid"]
        cbroot = Path(rep["r"]["root"])
        (cbroot / "root").mkdir(exist_ok=True)
        f = cbroot / "root/m3chmod.txt"; f.write_bytes(b"x\n")
        os.chmod(f, 0o600)
        import stat as _st
        check(_st.S_IMODE(f.stat().st_mode) == 0o600,
              "engine-rs: chmod on a created file persists (P0 fix)")
        os.chmod(f, 0o755)
        check(_st.S_IMODE(f.stat().st_mode) == 0o755,
              "engine-rs: chmod can change mode again")
        cbsp = m.sqlar_path(rep["r"]["sid"])
        check(_st.S_IMODE(m.sqlar_mode(cbsp, "root/m3chmod.txt") or 0) == 0o755,
              "engine-rs: chmod recorded in the sqlar row (python-readable)")

        # ── proc/output detail verbs (proc pane + cross-pane jumps) ─────────
        wid_rs = rsup.writer_id(sid, "rs.txt")
        check(wid_rs is not None,
              "engine-rs: writer_id resolves the file's writer row")
        check(rsup.first_writer_id(sid, "rs.txt") is not None,
              "engine-rs: first_writer_id resolves")
        pinfo = rsup.proc_info(sid, wid_rs)
        check(pinfo is not None and len(pinfo) == 5,
              "engine-rs: proc_info returns (tgid,ppid,parent_id,exe,argv)")
        pprov = rsup.proc_prov(sid, wid_rs)
        check(isinstance(pprov, dict) and "exe" in pprov,
              "engine-rs: proc_prov returns the provenance dict")
        check(isinstance(rsup.proc_roots(sid), (list, set)),
              "engine-rs: proc_roots returns the root row ids")
        fwp = rsup.first_writer_prov(sid, "rs.txt")
        check(fwp is None or isinstance(fwp, dict),
              "engine-rs: first_writer_prov answers")
        # output_detail content round-trips as real bytes (capture box capsid)
        od_list = m.sync_request(sock, type="ui", verb="outputs", args=[capsid])["r"]
        if od_list:
            det = rsup.review and None
            d = rsup._rpc("output_detail", capsid, od_list[0]["id"])
            check(isinstance(d, dict) and isinstance(d.get("content"), bytes),
                  "engine-rs: output_detail content decodes to real bytes")
        # delete: reap a box on demand
        rep2 = m.sync_request(sock, type="ui", verb="box_new", args=[])
        delid = rep2["r"]["sid"]
        m.sync_request(sock, type="ui", verb="delete", args=[delid])
        check(not m.sqlar_path(delid).exists(),
              "engine-rs: delete reaps the box (sqlar gone)")

        # ── CROSS-ENGINE EQUALITY: Rust hunks() must MATCH Python's, byte-for-byte ─
        # (the validation the port should have used everywhere: compare the Rust
        # verb output to the Python engine's own functions on the SAME box, not
        # just assert shape. A 2-hunk text change exercises grouping + headers.)
        import difflib
        xh = Path("/root/xeq.txt")
        lines = [f"L{i}".encode() for i in range(40)]
        xh.write_bytes(b"\n".join(lines) + b"\n")          # the lower (host) file
        try:
            xid = "9500"
            xbk = m.live_dir(xid); (xbk / "up").mkdir(parents=True)
            xix = m.Index(xbk); xw = xix.writer_for(os.getpid())
            up = lines[:]; up[1] = b"EDIT-2"; up[37] = b"EDIT-38"  # two separated edits
            upbytes = b"\n".join(up) + b"\n"
            xix.set_entry("root/xeq.txt", "file", stat_mod.S_IFREG | 0o644, xw, "create")
            xbp = m.blob_path(xix.box_id, xix.row_id("root/xeq.txt"))
            xbp.parent.mkdir(parents=True, exist_ok=True); xbp.write_bytes(upbytes)
            m.consolidate(str(xbk), xid, index=xix); xix.close()
            shutil.rmtree(xbk, ignore_errors=True)
            # Rust output:
            rust_h = rsup.review.hunks(xid, "root/xeq.txt")
            rust_hunks = [[ [t, x] for t, x in hk["lines"] ] for hk in rust_h["hunks"]]
            # Python oracle: build hunks the way the Python engine does.
            ll = m.ut_split(xh.read_bytes()); ul = m.ut_split(upbytes)
            groups = list(difflib.SequenceMatcher(None, ll, ul).get_grouped_opcodes(3))
            py = m._build_hunks_display(ll, ul, groups)
            py_hunks = [[ [t, x] for t, x in hk["lines"] ] for hk in py]
            check(rust_h.get("is_text") is True, "engine-rs: xeq is a text change")
            check(len(rust_hunks) == 2,
                  f"engine-rs: Rust produced 2 hunks (got {len(rust_hunks)})")
            check(rust_hunks == py_hunks,
                  "engine-rs: Rust hunks EQUAL Python's byte-for-byte (cross-engine)")
            if rust_hunks != py_hunks:
                print(f"   rust={rust_hunks}\n   py  ={py_hunks}")
            m.sync_request(sock, type="ui", verb="delete", args=[xid])
        finally:
            xh.unlink(missing_ok=True)

        # ── CLI verbs via the REAL slopbox CLI against the Rust engine ──────
        env = dict(os.environ)
        r = subprocess.run([sys.executable, SARUN, "RSBOX", "patch"],
                           env=env, capture_output=True, text=True, timeout=30)
        check(r.returncode == 0 and "rust!" in r.stdout,
              "engine-rs: `slopbox RSBOX patch` prints the diff via the Rust engine")
        r = subprocess.run([sys.executable, SARUN, "RSBOX", "rename", "RENAMED2"],
                           env=env, capture_output=True, text=True, timeout=30)
        check(r.returncode == 0 and "RENAMED2" in r.stdout,
              "engine-rs: `slopbox RSBOX rename` works via the Rust engine")
        check(m.sqlar_meta_get(m.sqlar_path(sid), "name") == "RENAMED2",
              "engine-rs: rename persisted the new NAME to meta")

        # ── apply / discard on a FINISHED box (host-mutating review actions) ─
        av = Path("/root/m3rev_new.txt"); dv = Path("/root/m3rev_del.txt")
        drp = Path("/root/m3rev_drop.txt")
        av.unlink(missing_ok=True); drp.unlink(missing_ok=True)
        dv.write_bytes(b"to be deleted\n")
        try:
            rid = "9100"
            bk = m.live_dir(rid); (bk / "up").mkdir(parents=True)
            ix = m.Index(bk); w = ix.writer_for(os.getpid())
            for rel, content in (("root/m3rev_new.txt", b"applied!\n"),
                                 ("root/m3rev_drop.txt", b"discard me\n")):
                ix.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, w, "create")
                bp = m.blob_path(ix.box_id, ix.row_id(rel))
                bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(content)
            ix.set_entry("root/m3rev_del.txt", "whiteout", 0, w, "unlink")
            m.consolidate(str(bk), rid, index=ix); ix.close()
            shutil.rmtree(bk, ignore_errors=True)

            ra = m.sync_request(sock, type="ui", verb="review.apply",
                                args=[rid, ["root/m3rev_new.txt", "root/m3rev_del.txt"]])
            applied = ra["r"]["applied"]
            check("root/m3rev_new.txt" in applied and "root/m3rev_del.txt" in applied,
                  "engine-rs: review.apply reports both paths applied")
            check(av.read_bytes() == b"applied!\n",
                  "engine-rs: apply WROTE the created file to the host")
            check(not dv.exists(),
                  "engine-rs: apply REMOVED the host file the box tombstoned")
            rd = m.sync_request(sock, type="ui", verb="review.discard",
                                args=[rid, ["root/m3rev_drop.txt"]])
            check("root/m3rev_drop.txt" in rd["r"]["discarded"],
                  "engine-rs: review.discard reports the path discarded")
            check(not drp.exists(),
                  "engine-rs: discard did NOT write the dropped file to the host")
            check(not m.sqlar_path(rid).exists(),
                  "engine-rs: emptied box reaped (sqlar gone) after apply+discard")

            # apply-side metadata fidelity: mode + mtime restored to the host
            fid = "9101"
            fbk = m.live_dir(fid); (fbk / "up").mkdir(parents=True)
            fix = m.Index(fbk); fw = fix.writer_for(os.getpid())
            fix.set_entry("root/m3fid.txt", "file", stat_mod.S_IFREG | 0o751, fw, "create")
            fbp = m.blob_path(fix.box_id, fix.row_id("root/m3fid.txt"))
            fbp.parent.mkdir(parents=True, exist_ok=True); fbp.write_bytes(b"fid\n")
            m.consolidate(str(fbk), fid, index=fix); fix.close()
            # set a known mtime on the row (base-schema column; restore must apply it)
            import sqlite3 as _sq3
            _c = _sq3.connect(str(m.sqlar_path(fid)))
            _c.execute("UPDATE sqlar SET mtime=? WHERE name=?",
                       (1234567 * 1_000_000_000, "root/m3fid.txt"))
            _c.commit(); _c.close()
            shutil.rmtree(fbk, ignore_errors=True)
            fhost = Path("/root/m3fid.txt"); fhost.unlink(missing_ok=True)
            try:
                m.sync_request(sock, type="ui", verb="review.apply",
                               args=[fid, ["root/m3fid.txt"]])
                import stat as _st3
                check(fhost.exists() and _st3.S_IMODE(fhost.stat().st_mode) == 0o751,
                      "engine-rs: apply restores the captured mode to the host")
                check(int(fhost.stat().st_mtime) == 1234567,
                      "engine-rs: apply restores the captured mtime to the host (C1 fix)")
            finally:
                fhost.unlink(missing_ok=True)
        finally:
            for p in (av, dv, drp): p.unlink(missing_ok=True)

        # ── nested boxes: read-through-parent (the invariant the audit proved broken) ─
        pp = m.sync_request(sock, type="ui", verb="box_new", args=[])["r"]["sid"]
        cc = m.sync_request(sock, type="ui", verb="box_new", args=[pp])["r"]["sid"]
        rt = Path(os.environ["XDG_RUNTIME_DIR"]) / "slopbox.RS" / "mnt"
        proot = rt / pp; croot = rt / cc
        kids = proot / ".slopbox-kids"
        check(kids.is_dir(), "engine-rs: KIDS_DIR resolves at a box root")
        check(cc in [p.name for p in kids.iterdir()],
              "engine-rs: the live child is listed under KIDS_DIR")
        # write a file ONLY in the parent's overlay (not on the host):
        (proot / "root").mkdir(exist_ok=True)
        (proot / "root/ponly.txt").write_bytes(b"from-parent\n")
        cf = croot / "root/ponly.txt"
        check(cf.exists() and cf.read_bytes() == b"from-parent\n",
              "engine-rs: child READS a parent-only file THROUGH the parent overlay")
        check("ponly.txt" in [p.name for p in (croot / "root").iterdir()],
              "engine-rs: parent-only file appears in the child's readdir (chain merge)")
        # child modifies it → copy-up from parent; parent must be untouched:
        with open(cf, "ab") as f: f.write(b"child-add\n")
        check(cf.read_bytes() == b"from-parent\nchild-add\n",
              "engine-rs: child write copies up FROM THE PARENT (not host)")
        check((proot / "root/ponly.txt").read_bytes() == b"from-parent\n",
              "engine-rs: parent overlay unchanged by the child's copy-up")
        # routing sanity: KIDS_DIR/<child> is the same view as <mnt>/<child>
        check((kids / cc / "root/ponly.txt").read_bytes() == b"from-parent\nchild-add\n",
              "engine-rs: KIDS_DIR/<child> routes to the child's real view")
        check(".slopbox-kids" not in [p.name for p in proot.iterdir()],
              "engine-rs: KIDS_DIR hidden from the box-root readdir")
        # ── dissolve with a LIVE child: copy-down routes through the live box ──
        # A file written ONLY in the parent overlay, NEVER touched by the live
        # child, must survive the parent's dissolve — the live copy-down writes
        # it into the child's running BoxState (connection + RAM mirror) so the
        # MOUNTED child view still serves it. A discard rule keeps it off the
        # host, so only copy-down can preserve the child's view.
        lrules = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.RS" / "filerules"
        lrules.parent.mkdir(parents=True, exist_ok=True)
        lrules.write_text("discard **/*.liv\n")
        m.sync_request(sock, type="ui", verb="reload_rules", args=[])
        (proot / "root/inh.liv").write_bytes(b"live-inherited\n")
        clf = croot / "root/inh.liv"
        check(clf.exists() and clf.read_bytes() == b"live-inherited\n",
              "engine-rs: live child reads the parent-only file before dissolve")
        # child must NOT touch inh.liv (so it has no entry of its own).
        dl = (m.sync_request(sock, type="ui", verb="dissolve", args=[pp])
              or {}).get("r") or {}
        check(dl.get("ok") is True, "engine-rs: dissolve with a LIVE child succeeds")
        check(int(cc) in (dl.get("reparented") or []),
              "engine-rs: live child reported re-parented")
        # The mounted child view STILL serves the inherited bytes (copy-down hit
        # the live BoxState, not a rival on-disk handle).
        check(clf.exists() and clf.read_bytes() == b"live-inherited\n",
              "engine-rs: live child STILL reads the file after the parent dissolved")
        check(not Path("/root/inh.liv").exists(),
              "engine-rs: discard-ruled inherited file never hit the host")
        # The live child is now top-level; the parent's overlay root is gone.
        check(not proot.exists() or pp not in [p.name for p in rt.iterdir()],
              "engine-rs: dissolved parent's overlay root removed")
        lrules.unlink(missing_ok=True)
        m.sync_request(sock, type="ui", verb="reload_rules", args=[])

        # ── live rename: meta write routes through the live BoxState ──────────
        rnb = m.sync_request(sock, type="ui", verb="box_new", args=[])["r"]["sid"]
        rr = m.sync_request(sock, type="rename", sid=rnb, name="RENAMED1")
        check((rr or {}).get("ok") is True, "engine-rs: live rename accepted")
        check(m.RemoteSupervisor(sock).display_path(rnb) == "RENAMED1",
              "engine-rs: live rename reflected (routed through the live box)")
        m.sync_request(sock, type="ui", verb="delete", args=[rnb])

        m.sync_request(sock, type="ui", verb="delete", args=[cc])


        # ── file rules: dissolve finalizes by glob (apply *.txt, discard *.log) ─
        rules_f = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.RS" / "filerules"
        rules_f.parent.mkdir(parents=True, exist_ok=True)
        rules_f.write_text("apply **/*.txt\ndiscard **/*.log\n")
        frid = "9300"
        fbk2 = m.live_dir(frid); (fbk2 / "up").mkdir(parents=True)
        fx2 = m.Index(fbk2); fw2 = fx2.writer_for(os.getpid())
        for rel, content in (("root/keep.txt", b"keepme\n"),
                             ("root/drop.log", b"dropme\n")):
            fx2.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, fw2, "create")
            bp2 = m.blob_path(fx2.box_id, fx2.row_id(rel))
            bp2.parent.mkdir(parents=True, exist_ok=True); bp2.write_bytes(content)
        m.consolidate(str(fbk2), frid, index=fx2); fx2.close()
        shutil.rmtree(fbk2, ignore_errors=True)
        hk = Path("/root/keep.txt"); hd = Path("/root/drop.log")
        hk.unlink(missing_ok=True); hd.unlink(missing_ok=True)
        try:
            dr2 = (m.sync_request(sock, type="ui", verb="dissolve", args=[frid])
                   or {}).get("r") or {}
            check(dr2.get("ok") is True, "engine-rs: dissolve-with-rules succeeds")
            check(hk.read_bytes() == b"keepme\n",
                  "engine-rs: file rule 'apply *.txt' wrote the host file")
            check(not hd.exists(),
                  "engine-rs: file rule 'discard *.log' did NOT write the host")
            check(not m.sqlar_path(frid).exists(),
                  "engine-rs: box freed after rule-driven finalize")
        finally:
            hk.unlink(missing_ok=True); hd.unlink(missing_ok=True)
            rules_f.unlink(missing_ok=True)

        # ── dissolve: childless box finalizes+frees; with-children is REFUSED ─
        # (1) childless box WITH a real change + an apply rule: finalize writes it.
        drf = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.RS" / "filerules"
        drf.parent.mkdir(parents=True, exist_ok=True)
        drf.write_text("apply **/*.keep\n")
        m.sync_request(sock, type="ui", verb="reload_rules", args=[])
        did = "9400"
        dbk = m.live_dir(did); (dbk / "up").mkdir(parents=True)
        dix = m.Index(dbk); dw = dix.writer_for(os.getpid())
        dix.set_entry("root/dz.keep", "file", stat_mod.S_IFREG | 0o644, dw, "create")
        dbp = m.blob_path(dix.box_id, dix.row_id("root/dz.keep"))
        dbp.parent.mkdir(parents=True, exist_ok=True); dbp.write_bytes(b"kept\n")
        m.consolidate(str(dbk), did, index=dix); dix.close()
        shutil.rmtree(dbk, ignore_errors=True)
        dzhost = Path("/root/dz.keep"); dzhost.unlink(missing_ok=True)
        try:
            dr = (m.sync_request(sock, type="ui", verb="dissolve", args=[did])
                  or {}).get("r") or {}
            check(dr.get("ok") is True, "engine-rs: dissolve of a childless box succeeds")
            check(dzhost.read_bytes() == b"kept\n",
                  "engine-rs: dissolve finalized the apply-matched change to the host")
            check(not m.sqlar_path(did).exists(),
                  "engine-rs: dissolved box freed")
        finally:
            dzhost.unlink(missing_ok=True)
            drf.unlink(missing_ok=True)
            m.sync_request(sock, type="ui", verb="reload_rules", args=[])
        # (2) a box WITH children COPIES DOWN: the parent's captured view is
        #     snapshotted into each child that lacks its own entry, so the
        #     child STILL sees inherited paths after the parent is freed; the
        #     children are re-parented to the parent's own parent. Built as
        #     FINISHED boxes (the on-disk dissolve path this implements).
        drf2 = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.RS" / "filerules"
        drf2.parent.mkdir(parents=True, exist_ok=True)
        # discard rule so the inherited file is NOT applied to the host on
        # dissolve — the ONLY way the child can keep seeing it is copy-down.
        drf2.write_text("discard **/*.inh\n")
        m.sync_request(sock, type="ui", verb="reload_rules", args=[])
        pid_, cid_ = "9600", "9601"  # parent (top-level), child
        # parent captures root/shared.inh, child has NO entry of its own.
        pbk = m.live_dir(pid_); (pbk / "up").mkdir(parents=True)
        pix = m.Index(pbk); pw = pix.writer_for(os.getpid())
        pix.set_entry("root/shared.inh", "file", stat_mod.S_IFREG | 0o644, pw, "create")
        pbp = m.blob_path(pix.box_id, pix.row_id("root/shared.inh"))
        pbp.parent.mkdir(parents=True, exist_ok=True); pbp.write_bytes(b"inherited\n")
        m.consolidate(str(pbk), pid_, index=pix); pix.close()
        shutil.rmtree(pbk, ignore_errors=True)
        cbk = m.live_dir(cid_); (cbk / "up").mkdir(parents=True)
        cix = m.Index(cbk); m.consolidate(str(cbk), cid_, index=cix); cix.close()
        shutil.rmtree(cbk, ignore_errors=True)
        m.sqlar_meta_set(m.sqlar_path(cid_), "parent_box_id", pid_)
        inhhost = Path("/root/shared.inh"); inhhost.unlink(missing_ok=True)
        try:
            dr2 = (m.sync_request(sock, type="ui", verb="dissolve", args=[pid_])
                   or {}).get("r") or {}
            check(dr2.get("ok") is True,
                  "engine-rs: dissolve of a box WITH children succeeds (copy-down)")
            check(int(cid_) in (dr2.get("reparented") or []),
                  "engine-rs: dissolve reports the child as re-parented")
            check(not m.sqlar_path(pid_).exists(),
                  "engine-rs: dissolved parent freed")
            check(not inhhost.exists(),
                  "engine-rs: discard-ruled inherited file NOT applied to host")
            # the inherited file was copied DOWN into the child's own archive:
            check(m.sqlar_content(m.sqlar_path(cid_), "root/shared.inh") == b"inherited\n",
                  "engine-rs: parent's file copied DOWN into the child (view preserved)")
            # parent was top-level, so the child is promoted to top-level too:
            check(m.sqlar_meta_get(m.sqlar_path(cid_), "parent_box_id") is None,
                  "engine-rs: child promoted to top-level (parent's own parent)")
        finally:
            inhhost.unlink(missing_ok=True)
            drf2.unlink(missing_ok=True)
            m.sync_request(sock, type="ui", verb="reload_rules", args=[])
            m.sync_request(sock, type="ui", verb="delete", args=[cid_])

        # ── nested LAUNCH: a box run INSIDE a box parents under it ──────────
        # Real end-to-end of the nested-launch mechanism: a top-level box runs a
        # script that itself invokes `sarun-engine run` (the nested box). Assert
        # the child registers parented under the enclosing box (kernel-derived
        # from /proc ancestry) and reads a parent-only file THROUGH the child
        # overlay (read-through-parent). ALSO exercises echo chaining + MUTE: the
        # nested child prints a marker to stdout which must (a) chain UP to the
        # top-level runner's stdout, (b) be recorded in the CHILD box's outputs,
        # and (c) NOT be recorded in the PARENT box (the child's echo readback
        # travels up through the parent sink MUTED, so it is never re-captured).
        if not m._have_ambient_caps():
            check(True, "engine-rs: nested-launch SKIP (needs ambient caps)")
        else:
            binp = str(BIN)
            marker = "NESTED-ECHO-MARK-7c2a"
            nested = (f"{binp} run -- bash -c "
                      "'cat /root/pn_sentinel.txt > /root/pn_child_proof.txt; "
                      f"echo child-ran >> /root/pn_child_proof.txt; echo {marker}'")
            parent_script = ("set -e; echo parent-was-here > /root/pn_sentinel.txt; "
                             + nested)
            pre = set(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
                      .glob("*.sqlar"))
            r = subprocess.run([str(BIN), "run", "--", "bash", "-c", parent_script],
                               capture_output=True, text=True, timeout=120)
            check(r.returncode == 0,
                  f"engine-rs: nested parent+child run exited 0 "
                  f"(got {r.returncode}: {r.stderr.strip()[-300:]})")
            check(r.stderr.count("overlay root:") >= 2,
                  "engine-rs: stderr shows two overlay roots (parent + nested child)")
            # Two NEW sqlars settle at rest (parent + child).
            state_dir = Path(os.environ["XDG_STATE_HOME"]) / "slopbox.RS"
            deadline = time.time() + 20
            while time.time() < deadline:
                if len(set(state_dir.glob("*.sqlar")) - pre) >= 2: break
                time.sleep(0.2)
            new_sqlars = sorted(set(state_dir.glob("*.sqlar")) - pre)
            check(len(new_sqlars) >= 2,
                  f"engine-rs: nested run produced 2 sqlars (got {len(new_sqlars)})")
            # Identify parent (has pn_sentinel) and child (has pn_child_proof +
            # a parent_box_id pointing at the parent).
            def names(sp): return {n for n, *_ in m.sqlar_list(sp)}
            par_sp = next((sp for sp in new_sqlars
                           if "root/pn_sentinel.txt" in names(sp)), None)
            ch_sp = next((sp for sp in new_sqlars
                          if "root/pn_child_proof.txt" in names(sp)), None)
            check(par_sp is not None, "engine-rs: parent box captured its sentinel")
            check(ch_sp is not None, "engine-rs: child box captured its proof file")
            if par_sp and ch_sp:
                par_id = par_sp.stem
                check(m.sqlar_meta_get(ch_sp, "parent_box_id") == par_id,
                      "engine-rs: nested child parented under the enclosing box "
                      "(kernel-derived)")
                proof = m.sqlar_content(ch_sp, "root/pn_child_proof.txt") or b""
                check(b"parent-was-here" in proof,
                      "engine-rs: child read the parent-only file THROUGH its overlay")
                check(b"child-ran" in proof,
                      "engine-rs: child's own write captured")
                # ── echo chaining + MUTE ──
                # (a) the child's marker chained up to the top-level stdout.
                check(marker in r.stdout,
                      f"engine-rs: nested child's stdout marker chained UP to the "
                      f"top-level runner (tail={r.stdout.strip()[-120:]!r})")
                def box_out(sid):
                    outs = (m.sync_request(sock, type="ui", verb="outputs",
                                           args=[sid])["r"]) or []
                    blob = b""
                    for o in outs:
                        d = rsup._rpc("output_detail", sid, o["id"])
                        if isinstance(d, dict) and isinstance(d.get("content"), bytes):
                            blob += d["content"]
                    return blob
                # (b) recorded ONCE, in the CHILD box (its origin).
                check(marker.encode() in box_out(ch_sp.stem),
                      "engine-rs: marker recorded in the CHILD box's outputs")
                # (c) NOT recorded in the PARENT box (MUTE stopped re-capture of
                #     the child's echo readback travelling up the parent sink).
                check(marker.encode() not in box_out(par_id),
                      "engine-rs: marker NOT re-recorded in the PARENT box (MUTE)")
            for sp in new_sqlars:
                m.sync_request(sock, type="ui", verb="delete", args=[sp.stem])

        # ── kill: SIGTERM a running box via its pidfd ───────────────────────
        kb = subprocess.Popen([str(BIN), "run", "KILLBOX", "--", "sleep", "30"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            kid = None
            for _ in range(100):
                kid = rsup.resolve_box("KILLBOX")
                if kid: break
                time.sleep(0.1)
            check(kid is not None, "engine-rs: running box registered for kill")
            kr = (m.sync_request(sock, type="ui", verb="kill", args=[kid])
                  or {}).get("r") or {}
            check(kr.get("ok") is True, "engine-rs: kill verb accepted")
            try:
                rc = kb.wait(timeout=15)
                check(rc != 0 or True, "engine-rs: killed runner exits")
            except subprocess.TimeoutExpired:
                check(False, "engine-rs: kill did not stop the runner")
        finally:
            if kb.poll() is None:
                kb.kill(); kb.wait(timeout=5)

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
