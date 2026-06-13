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
            shutil.rmtree(fbk, ignore_errors=True)
            fhost = Path("/root/m3fid.txt"); fhost.unlink(missing_ok=True)
            try:
                m.sync_request(sock, type="ui", verb="review.apply",
                               args=[fid, ["root/m3fid.txt"]])
                import stat as _st3
                check(fhost.exists() and _st3.S_IMODE(fhost.stat().st_mode) == 0o751,
                      "engine-rs: apply restores the captured mode to the host")
            finally:
                fhost.unlink(missing_ok=True)
        finally:
            for p in (av, dv, drp): p.unlink(missing_ok=True)

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
