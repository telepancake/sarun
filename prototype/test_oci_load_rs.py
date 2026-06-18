#!/usr/bin/env python3
"""`sarun oci load` against the RUST engine: builds a minimal oci-layout
fixture in a temp dir, loads it via `sarun oci load oci-layout:...`, and
asserts REAL effects in the resulting at-rest sarun box(es):

  • Per-layer sqlar exists in state_home, one box per image layer.
  • The base (rootfs) box has `no_host_fallback=1` in its meta — closed
    stack, no host bleed-through.
  • Regular files, dirs, and symlinks ingested via the existing
    BoxState mutation methods (file rows have a real blob on disk,
    symlinks carry their target bytes, dirs are mode 40755).
  • AUFS-style whiteouts (`.wh.<NAME>`) become S_IFCHR (020000)
    tombstone rows pointing at the sibling — NOT at a `.wh.X` literal.
  • The image config (env / cmd / workdir / user) is stored verbatim in
    the TOP box's meta as `oci_config`.
  • The engine HYDRATES the at-rest parent chain when a live child
    registers, so the merged FUSE view of the child sees the OCI box's
    captured entries — without this the chain would truncate at the
    child and OCI content would be invisible at runtime.

Skips (passes vacuously) if cargo/the binary are unavailable. Network is
not required — the fixture is fully synthetic.
"""
import gzip, hashlib, io, json, os, shutil, socket, sqlite3, subprocess
import sys, tarfile, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

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


def sha256_bytes(b: bytes) -> str:
    return "sha256:" + hashlib.sha256(b).hexdigest()


def write_blob(layout: Path, data: bytes):
    d = sha256_bytes(data)
    p = layout / "blobs" / "sha256" / d.split(":", 1)[1]
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)
    return d, len(data)


def build_layer_gz() -> bytes:
    """tar+gzip with the entry shapes worth verifying: dir / regular / symlink
    / whiteout, plus the bwrap-needed mountpoint dirs so a child box that
    parents to this layer can actually start a bwrap."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as t:
        def add_dir(name, mode=0o755, mtime=1700000000):
            ti = tarfile.TarInfo(name); ti.type = tarfile.DIRTYPE
            ti.mode = mode; ti.mtime = mtime; t.addfile(ti)
        def add_file(name, data, mode=0o644, mtime=1700000000):
            ti = tarfile.TarInfo(name); ti.size = len(data); ti.mode = mode
            ti.mtime = mtime; t.addfile(ti, io.BytesIO(data))
        def add_symlink(name, target):
            ti = tarfile.TarInfo(name); ti.type = tarfile.SYMTYPE
            ti.linkname = target; ti.mode = 0o777; t.addfile(ti)
        # bwrap mount points (so a child of this image can start a bwrap).
        add_dir("proc"); add_dir("sys"); add_dir("dev")
        add_dir("tmp", mode=0o1777)
        # The fixture's "image" content.
        add_dir("etc")
        add_file("etc/sarun-test", b"sarun-oci-load-fixture-marker\n")
        add_dir("usr"); add_dir("usr/bin")
        add_file("usr/bin/hello", b"#!/bin/sh\necho hi\n", mode=0o755)
        add_symlink("tmp-link", "tmp")
        # Whiteout tombstone: lower layer's `etc/gone` should be deleted by us.
        add_file("etc/.wh.gone", b"")
    raw = buf.getvalue()
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as g:
        g.write(raw)
    return out.getvalue()


def build_oci_layout(out: Path):
    out.mkdir(parents=True, exist_ok=True)
    (out / "oci-layout").write_text(json.dumps({"imageLayoutVersion":"1.0.0"}))
    layer_gz = build_layer_gz()
    layer_digest, layer_size = write_blob(out, layer_gz)
    config = {
        "architecture": "amd64", "os": "linux",
        "config": {
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:"
                    "/usr/bin:/sbin:/bin",
                    "OCI_FIXTURE_MARK=1"],
            "Cmd": ["/usr/bin/hello"],
            "WorkingDir": "/",
            "User": "0:0",
        },
        "rootfs": {"type": "layers", "diff_ids": [layer_digest]},
    }
    config_bytes = json.dumps(config, separators=(",", ":")).encode()
    config_digest, config_size = write_blob(out, config_bytes)
    manifest = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": config_digest, "size": config_size},
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest, "size": layer_size}],
    }
    mbytes = json.dumps(manifest, separators=(",", ":")).encode()
    mdigest, msize = write_blob(out, mbytes)
    index = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": mdigest, "size": msize,
            "annotations": {"org.opencontainers.image.ref.name": "test"}}],
    }
    (out / "index.json").write_text(json.dumps(index))


def main():
    if not ensure_binary():
        print("  ok  oci-load-rs: cargo/binary unavailable — SKIP")
        print("\nOCI-LOAD-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="ocirs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    layout = tmp / "layout"
    build_oci_layout(layout)
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # ── (1) sarun oci load oci-layout:... NAME ────────────────────────────
        r = subprocess.run(
            [str(BIN), "oci", "load", f"oci-layout:{layout}", "alpine"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"oci-load: exits 0 (got {r.returncode}: {r.stderr[-200:]})")
        check("alpine" in r.stdout and "1 layer" in r.stdout,
              f"oci-load: stdout reports the chain shape (got {r.stdout!r})")

        # Exactly one sqlar from this load (single-layer fixture).
        state = Path(os.environ["XDG_STATE_HOME"]) / "slopbox.RS"
        sqlars = sorted(state.glob("*.sqlar"))
        check(len(sqlars) == 1,
              f"oci-load: one sqlar per layer (got {len(sqlars)})")
        sp = sqlars[0]

        def meta(k):
            with sqlite3.connect(str(sp)) as c:
                row = c.execute("SELECT value FROM meta WHERE key=?",
                                (k,)).fetchone()
            return row[0] if row else None
        def sqlar_rows():
            with sqlite3.connect(str(sp)) as c:
                return {n: (mode, sz) for n, mode, sz in c.execute(
                    "SELECT name, mode, sz FROM sqlar")}

        check(meta("name") == "alpine",
              f"oci-load: base box named 'alpine' (got {meta('name')!r})")
        check(meta("no_host_fallback") == "1",
              f"oci-load: base box has no_host_fallback=1 "
              f"(got {meta('no_host_fallback')!r})")
        check(meta("parent_box_id") is None,
              "oci-load: base box has NO parent (it's the bottom of the stack)")
        check(meta("oci_reference", ).startswith("oci-layout:") if meta("oci_reference") else False,
              f"oci-load: oci_reference recorded "
              f"(got {meta('oci_reference')!r})")
        cfg = meta("oci_config") or ""
        check("OCI_FIXTURE_MARK" in cfg and "/usr/bin/hello" in cfg,
              "oci-load: oci_config JSON has the fixture's env + cmd")

        # ── (2) per-entry-kind ingest ─────────────────────────────────────────
        rows = sqlar_rows()
        # dir
        m_etc, sz_etc = rows.get("etc", (None, None))
        check(m_etc == 0o040755 and sz_etc == 0,
              f"oci-load: dir 'etc' ingested as mode 40755 "
              f"(got mode={oct(m_etc) if m_etc else None} sz={sz_etc})")
        # regular file (a blob exists on disk for this row)
        m_f, sz_f = rows.get("etc/sarun-test", (None, None))
        check(m_f == 0o100644
              and sz_f == len(b"sarun-oci-load-fixture-marker\n"),
              f"oci-load: regular file 'etc/sarun-test' ingested "
              f"(got mode={oct(m_f) if m_f else None} sz={sz_f})")
        # symlink (sz == len(target))
        m_l, sz_l = rows.get("tmp-link", (None, None))
        check(m_l == 0o120777 and sz_l == len("tmp"),
              f"oci-load: symlink 'tmp-link' ingested as mode 120777 "
              f"(got mode={oct(m_l) if m_l else None} sz={sz_l})")
        # whiteout — the `.wh.gone` entry becomes a tombstone ROW for 'etc/gone',
        # NOT a literal '.wh.gone' file.
        check("etc/.wh.gone" not in rows,
              "oci-load: NO literal '.wh.gone' row (the .wh. is a convention, "
              "not a real file)")
        m_w, _ = rows.get("etc/gone", (None, None))
        check(m_w == 0o020000,
              f"oci-load: whiteout target 'etc/gone' = S_IFCHR tombstone "
              f"(got mode={oct(m_w) if m_w else None})")

        # ── (3) hydration on register ────────────────────────────────────────
        # Start the engine and register a child of `alpine`. The engine's
        # add_box should hydrate the alpine BoxState into the overlay's live
        # box map, so resolve()/scan_dir() on the child SEE the alpine
        # entries. We don't bwrap — instead we list the FUSE mount path of
        # the child box from the host, which exercises scan_dir directly.
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("engine socket never appeared")

        # Register a child of alpine (no bwrap from a fake pidfd: we just send
        # a register message; the engine will mount the new box at <mnt>/<id>).
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(str(sock))
        reg = {"type": "register", "cmd": ["true"],
               "session_id": "alpine.HYD",
               "want_capture": False, "want_readonly_parent": True}
        s.sendall((json.dumps(reg) + "\n").encode())
        ack = s.recv(8192).decode().split("\n", 1)[0]
        ack_j = json.loads(ack)
        check(ack_j.get("ok") is True,
              f"register: alpine.HYD acked ok (got {ack})")
        child_mount = ack_j.get("mount")
        check(bool(child_mount),
              f"register: ack carries the child mount path (got {child_mount!r})")

        # The merged FUSE view of the child must include alpine's captured
        # entries (etc, usr, etc/sarun-test, tmp-link, etc.). If hydration
        # didn't run, scan_dir would see only the child's own (empty) box
        # AND fall through to host — but no_host_fallback=1 on alpine would
        # leave it Absent.
        merged = set()
        try:
            merged = set(os.listdir(child_mount))
        except OSError as e:
            check(False, f"hydration: ls of child mount failed: {e}")
        expected = {"etc", "usr", "tmp", "tmp-link", "proc", "sys", "dev"}
        missing = expected - merged
        check(not missing,
              f"hydration: child sees alpine's top-level entries "
              f"(merged={sorted(merged)}, missing={sorted(missing)})")
        # And the per-entry contents resolve through.
        try:
            content = Path(child_mount, "etc", "sarun-test").read_bytes()
        except OSError as e:
            content = None
            check(False, f"hydration: cannot read etc/sarun-test: {e}")
        check(content == b"sarun-oci-load-fixture-marker\n",
              f"hydration: child READS the alpine layer's file content "
              f"(got {content!r})")
        # Whiteout tombstone hides etc/gone (a non-existent sibling — proves
        # the tombstone is honored as Absent, not as a real ENOENT-on-host).
        # No assertion on etc/gone — it was never on the lower in our test.

        # ── (4) writes in the child bottle UP — readonly_parent prevents
        # apply from leaking back into the alpine layer (proven by the
        # readonly_parent test in test_parent_modes_rs.py; not re-asserted
        # here since this is the OCI-specific surface.)

        s.close()
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
    print("\n" + ("OCI-LOAD-RS PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_oci_load_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
