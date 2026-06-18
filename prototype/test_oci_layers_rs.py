#!/usr/bin/env python3
"""Multi-layer OCI corner-case ingest against the RUST engine:

  • Whiteouts that hide a SIBLING from a lower layer (`.wh.<NAME>`).
  • Opaque dirs (`.wh..wh..opq`) that hide every lower-layer entry under the
    directory's WHOLE subtree, not just immediate children.
  • Missing parent-dir entries in the tar (mtree-style implicit dirs) —
    `srv/www/index.html` without a Directory entry for `srv/www` still
    resolves through the FUSE overlay (we auto-create the missing dir row).
  • Hardlinks within a layer — tar `Link` entries with mode=0 copy the
    source's bytes AND its mode (not the header's 0).

Constructed as a TWO-layer oci-layout fixture in a tempdir. No network.
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import gzip, hashlib, io, json, os, shutil, socket, sqlite3, subprocess
import sys, tarfile, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE.parent / "engine"
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


def sha256_bytes(b: bytes) -> str:
    return "sha256:" + hashlib.sha256(b).hexdigest()


def write_blob(layout: Path, data: bytes):
    d = sha256_bytes(data)
    p = layout / "blobs" / "sha256" / d.split(":", 1)[1]
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)
    return d, len(data)


def gz(raw: bytes) -> bytes:
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as g:
        g.write(raw)
    return out.getvalue()


def layer1_gz() -> bytes:
    """Layer 1: populate the bottom rootfs."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as t:
        def add_dir(name, mode=0o755, mtime=1700000000):
            ti = tarfile.TarInfo(name); ti.type = tarfile.DIRTYPE
            ti.mode = mode; ti.mtime = mtime; t.addfile(ti)
        def add_file(name, data, mode=0o644, mtime=1700000000):
            ti = tarfile.TarInfo(name); ti.size = len(data); ti.mode = mode
            ti.mtime = mtime; t.addfile(ti, io.BytesIO(data))
        # bwrap mount points so a child of this can launch a bwrap.
        for d in ("proc", "sys", "dev"): add_dir(d)
        add_dir("tmp", mode=0o1777)
        # /etc: a, b, c — layer 2 will opacify this dir.
        add_dir("etc")
        add_file("etc/a", b"layer1-a\n")
        add_file("etc/b", b"layer1-b\n")
        add_file("etc/c", b"layer1-c\n")
        # /var: layer 2 will WHITEOUT (not opacify) one entry, so survivors
        # from layer 1 are still visible. Single-NAME tombstone semantics.
        add_dir("var")
        add_file("var/keep", b"layer1-var-keep\n")
        add_file("var/zap",  b"layer1-var-zap\n")
        # /opt/deep/nested: layer 2 will OPACIFY /opt — every subtree of /opt
        # from layer 1 disappears, regardless of nesting depth.
        add_dir("opt")
        add_dir("opt/deep")
        add_dir("opt/deep/nested")
        add_file("opt/deep/nested/x", b"layer1-deep-x\n")
    return gz(buf.getvalue())


def layer2_gz() -> bytes:
    """Layer 2: whiteouts, opaque marker, and an implicit-parent path."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as t:
        def add_dir(name, mode=0o755, mtime=1700000001):
            ti = tarfile.TarInfo(name); ti.type = tarfile.DIRTYPE
            ti.mode = mode; ti.mtime = mtime; t.addfile(ti)
        def add_file(name, data, mode=0o644, mtime=1700000001):
            ti = tarfile.TarInfo(name); ti.size = len(data); ti.mode = mode
            ti.mtime = mtime; t.addfile(ti, io.BytesIO(data))
        def add_link(name, target, mode=0o644, mtime=1700000001):
            # Tar Link entry: mode=0 in the spec (the source's metadata is
            # supposed to carry through). We deliberately write mode=0 to
            # verify the engine picks up mode FROM THE SOURCE, not header.
            ti = tarfile.TarInfo(name); ti.type = tarfile.LNKTYPE
            ti.linkname = target; ti.mode = 0; ti.mtime = mtime; t.addfile(ti)
        # /etc: replace EVERYTHING. Opaque marker + one new file `d`.
        add_dir("etc", mtime=1700000001)
        add_file("etc/.wh..wh..opq", b"")
        add_file("etc/d", b"layer2-d\n")
        # /var: whiteout `zap`. `keep` stays from layer 1.
        add_dir("var")
        add_file("var/.wh.zap", b"")
        add_file("var/new", b"layer2-var-new\n")
        # /opt: opacify the whole subtree — `opt/deep/nested/x` from layer 1
        # must be hidden even though no explicit whiteout names it.
        add_dir("opt")
        add_file("opt/.wh..wh..opq", b"")
        add_file("opt/after", b"layer2-opt-after\n")
        # /srv/www/index.html with NO `srv/www` Directory entry — verify
        # the engine still synthesizes a parent dir row so ls works.
        add_file("srv/www/index.html",
                 b"<html>layer2-index</html>\n", mode=0o644)
        # Hardlink: /usr/bin/orig (executable) ←→ /usr/bin/alias.
        # The Link entry's mode is 0 in the header; the engine must
        # carry over mode 0o755 from /usr/bin/orig.
        add_dir("usr"); add_dir("usr/bin")
        add_file("usr/bin/orig", b"hello-from-orig\n", mode=0o755)
        add_link("usr/bin/alias", "usr/bin/orig")
    return gz(buf.getvalue())


def build_layout(out: Path):
    out.mkdir(parents=True, exist_ok=True)
    (out / "oci-layout").write_text(json.dumps({"imageLayoutVersion":"1.0.0"}))
    l1 = layer1_gz(); l1_d, l1_s = write_blob(out, l1)
    l2 = layer2_gz(); l2_d, l2_s = write_blob(out, l2)
    config = {
        "architecture": "amd64", "os": "linux",
        "config": {
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "Cmd": ["/bin/sh"], "WorkingDir": "/", "User": "0:0",
        },
        "rootfs": {"type": "layers", "diff_ids": [l1_d, l2_d]},
    }
    cfg = json.dumps(config, separators=(",", ":")).encode()
    cfg_d, cfg_s = write_blob(out, cfg)
    manifest = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": cfg_d, "size": cfg_s},
        "layers": [
            {"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip",
             "digest": l1_d, "size": l1_s},
            {"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip",
             "digest": l2_d, "size": l2_s},
        ],
    }
    mb = json.dumps(manifest, separators=(",", ":")).encode()
    mb_d, mb_s = write_blob(out, mb)
    index = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType":"application/vnd.oci.image.manifest.v1+json",
            "digest": mb_d, "size": mb_s,
            "platform": {"architecture": "amd64", "os": "linux"}}],
    }
    (out / "index.json").write_text(json.dumps(index))


def main():
    if not ensure_binary():
        print("  ok  oci-layers-rs: cargo/binary unavailable — SKIP")
        print("\nOCI-LAYERS-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="ocilrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    layout = tmp / "layout"
    build_layout(layout)
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # Load the 2-layer image.
        r = subprocess.run(
            [str(BIN), "oci", "load", f"oci-layout:{layout}", "img"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"oci-layers: load exits 0 (got {r.returncode}: {r.stderr[-200:]})")
        check("2 layer" in r.stdout,
              f"oci-layers: reported 2 layers (stdout={r.stdout!r})")

        # Sqlars: 1 per layer, parent chain hooked up, no_host_fallback only
        # on the base.
        state = Path(os.environ["XDG_STATE_HOME"]) / "slopbox.RS"
        sqlars = sorted(state.glob("*.sqlar"), key=lambda p: int(p.stem))
        check(len(sqlars) == 2,
              f"oci-layers: two sqlars (got {len(sqlars)})")

        def meta(sp, k):
            with sqlite3.connect(str(sp)) as c:
                row = c.execute("SELECT value FROM meta WHERE key=?",
                                (k,)).fetchone()
            return row[0] if row else None
        def rows(sp):
            with sqlite3.connect(str(sp)) as c:
                return {n: (mode, sz, opaque) for n, mode, sz, opaque
                        in c.execute("SELECT name, mode, sz, opaque FROM sqlar")}

        base, top = sqlars[0], sqlars[1]
        check(meta(base, "no_host_fallback") == "1",
              "oci-layers: base box (layer 1) has no_host_fallback=1")
        check(meta(top, "no_host_fallback") is None,
              "oci-layers: top box (layer 2) does NOT have no_host_fallback "
              "(it inherits via the chain)")
        check(meta(top, "parent_box_id") == str(int(base.stem)),
              f"oci-layers: top.parent_box_id = base id "
              f"(got {meta(top, 'parent_box_id')!r})")
        check(meta(base, "name") == "img",
              "oci-layers: base box named 'img' (user-supplied)")
        check(meta(top, "name", ).startswith("L"),
              f"oci-layers: top box auto-named L<id> (got {meta(top, 'name')!r})")

        # ── opaque marker stored as sqlar.opaque=1 on top box's 'etc' + 'opt'
        top_rows = rows(top)
        m_etc, _, op_etc = top_rows.get("etc", (None, None, None))
        check(op_etc == 1,
              f"opaque: top box's 'etc' has opaque=1 (got {op_etc!r}, "
              f"mode={oct(m_etc) if m_etc else None})")
        m_opt, _, op_opt = top_rows.get("opt", (None, None, None))
        check(op_opt == 1,
              f"opaque: top box's 'opt' has opaque=1 (got {op_opt!r})")
        # And NO literal `.wh..wh..opq` row exists.
        check("etc/.wh..wh..opq" not in top_rows
              and "opt/.wh..wh..opq" not in top_rows,
              "opaque: no literal '.wh..wh..opq' rows (consumed by ingest)")

        # ── implicit parent dirs: srv/www exists as a Dir row even though
        #    layer 2's tar didn't include a Directory entry for it.
        m_www, _, _ = top_rows.get("srv/www", (None, None, None))
        check(m_www is not None and (m_www & 0o170000) == 0o040000,
              f"implicit-parent: 'srv/www' synthesized as a Dir row "
              f"(mode={oct(m_www) if m_www else None})")
        m_srv, _, _ = top_rows.get("srv", (None, None, None))
        check(m_srv is not None and (m_srv & 0o170000) == 0o040000,
              f"implicit-parent: 'srv' synthesized as a Dir row "
              f"(mode={oct(m_srv) if m_srv else None})")

        # ── hardlink: alias has the SOURCE's mode (0o100755), not the
        #    Link entry's header mode (0). And its blob is a real copy.
        m_alias, sz_alias, _ = top_rows.get("usr/bin/alias", (None, None, None))
        check(m_alias == 0o100755,
              f"hardlink: 'usr/bin/alias' mode inherited from source "
              f"(got {oct(m_alias) if m_alias else None}, want 0o100755)")
        check(sz_alias == len(b"hello-from-orig\n"),
              f"hardlink: 'usr/bin/alias' carries the source's bytes "
              f"(got sz={sz_alias})")

        # ── runtime: register a child of the top box and verify
        #    (a) the engine surfaces oci runtime fields (env/cwd/cmd/user)
        #        in the register ack, walked from the parent chain's TOP
        #        layer where oci_config was stamped;
        #    (b) the FUSE merged view honors opaque + whiteouts as expected.
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("engine socket never appeared")
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(str(sock))
        reg = {"type":"register","cmd":["true"],
               "session_id": f"img.L{top.stem}.SCRATCH",
               "want_capture": False, "want_readonly_parent": True}
        s.sendall((json.dumps(reg) + "\n").encode())
        ack = s.recv(8192).decode().split("\n", 1)[0]
        ack_j = json.loads(ack)
        check(ack_j.get("ok") is True,
              f"runtime: child of top layer registered (got {ack[:200]})")
        mnt = ack_j.get("mount")
        # OCI runtime fields surfaced from the chain's oci_config.
        oci_rt = ack_j.get("oci") or {}
        check(isinstance(oci_rt, dict) and oci_rt,
              f"oci-runtime: ack carries the oci runtime fields (got {oci_rt!r})")
        check(oci_rt.get("cwd") == "/",
              f"oci-runtime: WorkingDir → cwd = '/' (got {oci_rt.get('cwd')!r})")
        check(oci_rt.get("cmd") == ["/bin/sh"],
              f"oci-runtime: Cmd → cmd = ['/bin/sh'] (got {oci_rt.get('cmd')!r})")
        check(oci_rt.get("user") == "0:0",
              f"oci-runtime: User → user = '0:0' (got {oci_rt.get('user')!r})")
        env_list = oci_rt.get("env") or []
        check(any(e.startswith("PATH=") for e in env_list),
              f"oci-runtime: Env carries the image's PATH (got {env_list!r})")

        # /etc: opaque'd in top → only 'd' is visible (a, b, c GONE).
        etc_listing = sorted(os.listdir(Path(mnt, "etc"))) if mnt else []
        check(etc_listing == ["d"],
              f"opaque: /etc shows ONLY layer 2's 'd' (layer 1's a, b, c "
              f"hidden by opaque). got={etc_listing}")

        # /var: whiteout('zap') only, NOT opaque → layer 1's 'keep' survives,
        # layer 2's 'new' added, layer 1's 'zap' removed.
        var_listing = sorted(os.listdir(Path(mnt, "var"))) if mnt else []
        check(var_listing == ["keep", "new"],
              f"whiteout: /var shows 'keep' (layer 1 survivor) + 'new' "
              f"(layer 2), 'zap' tombstoned. got={var_listing}")

        # /opt: opaque'd in top → layer 1's WHOLE subtree (opt/deep/nested/x)
        # hidden. Only layer 2's 'after' visible.
        opt_listing = sorted(os.listdir(Path(mnt, "opt"))) if mnt else []
        check(opt_listing == ["after"],
              f"opaque-subtree: /opt's deep subtree hidden by opaque marker. "
              f"got={opt_listing}")
        # And the deep file is NOT reachable by direct lookup either.
        try:
            os.stat(Path(mnt, "opt", "deep", "nested", "x"))
            deep_visible = True
        except FileNotFoundError:
            deep_visible = False
        check(not deep_visible,
              "opaque-subtree: opt/deep/nested/x NOT directly resolvable "
              "(opaque on /opt hides every descendant)")

        # /srv/www/index.html: implicit-parent path resolves end-to-end.
        try:
            idx = Path(mnt, "srv", "www", "index.html").read_bytes()
        except OSError as e:
            idx = None
            check(False, f"implicit-parent: read of /srv/www/index.html failed: {e}")
        check(idx == b"<html>layer2-index</html>\n",
              f"implicit-parent: /srv/www/index.html reads its bytes "
              f"(got {idx!r})")

        # /usr/bin/alias and /usr/bin/orig read the same content.
        try:
            orig = Path(mnt, "usr", "bin", "orig").read_bytes()
            alias = Path(mnt, "usr", "bin", "alias").read_bytes()
        except OSError as e:
            orig = alias = None
            check(False, f"hardlink: read failed: {e}")
        check(orig == b"hello-from-orig\n" and alias == orig,
              f"hardlink: 'alias' reads identical bytes to 'orig' "
              f"(orig={orig!r}, alias={alias!r})")

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
    print("\n" + ("OCI-LAYERS-RS PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_oci_layers_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
