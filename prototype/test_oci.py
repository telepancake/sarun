#!/usr/bin/env python3
"""Hermetic OCI tests for the Rust engine: load -> run -> build -> run-result.

NO registry, NO network: we synthesize a scratch oci-archive in-test whose
single-layer rootfs is just `/bin/sarun` — a copy of the static musl engine
binary, which therefore runs inside a closed box with nothing else present.
The image CMD is `/bin/sarun oci --help`, a deterministic, socket-free print, so
we can assert on its output to prove the box actually executed.

What it covers:
  * `oci load oci-archive:...`  -> at-rest layer-box stack (named base box)
  * `oci run <name>`            -> closed-rootfs box boots, runs the image CMD
  * `oci build`                 -> FROM the image, COPY a file, RUN (exec form),
                                   CMD; COPY landing is checked in the sqlar
  * `oci run <built>`           -> the built image runs its CMD

This exercises the closed/minimal-rootfs execution path end to end (fd-exec'd
inner, synthetic /proc,/dev,/sys,/tmp landing pads, host-visibility cwd=/).

    make test-oci         # or: prototype/test_oci.py
    (needs `make engine` first — uses the built static binary)

Self-safety: everything lives under an isolated XDG temp tree; the engine is
launched headless and SIGTERM'd in a finally; the tree is removed on exit.
"""
import os, sys, time, signal, socket, subprocess, tempfile, shutil
import json, hashlib, tarfile, gzip, io, sqlite3
from pathlib import Path

REPO = Path("/home/user/sarun")
ENGINE = REPO / "engine/target/x86_64-unknown-linux-musl/release/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


def env_for(tmp):
    e = dict(os.environ)
    e["XDG_STATE_HOME"] = str(tmp / "state")
    e["XDG_DATA_HOME"] = str(tmp / "data")
    e["XDG_RUNTIME_DIR"] = str(tmp / "run")
    e["XDG_CONFIG_HOME"] = str(tmp / "config")
    e.pop("SLOPBOX_NS", None)          # default app_dir = "slopbox"
    return e


def sock_path(e):  return Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock"
def state_dir(e):  return Path(e["XDG_STATE_HOME"]) / "slopbox"


def wait_socket(sock, timeout=40):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(str(sock)); return True
        except OSError:
            time.sleep(0.2)
    return False


def _sha(b):  return hashlib.sha256(b).hexdigest()


def _build_layout(layout, exe, extra=None):
    """Build a synthetic single-layer oci-layout in dir `layout`. rootfs = bin/ +
    bin/sarun (the engine binary), plus an optional `/marker` file (`extra`,
    bytes) so callers can vary the image content → a distinct manifest digest.
    Returns the image's manifest digest ('sha256:…')."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as t:
        d = tarfile.TarInfo("bin"); d.type = tarfile.DIRTYPE; d.mode = 0o755
        t.addfile(d)
        f = tarfile.TarInfo("bin/sarun"); f.size = len(exe); f.mode = 0o755
        t.addfile(f, io.BytesIO(exe))
        if extra is not None:
            m = tarfile.TarInfo("marker"); m.size = len(extra); m.mode = 0o644
            t.addfile(m, io.BytesIO(extra))
    layer_raw = raw.getvalue()
    diff_id = "sha256:" + _sha(layer_raw)
    gz = gzip.compress(layer_raw)
    layer_digest = "sha256:" + _sha(gz)
    config = {
        "architecture": "amd64", "os": "linux",
        "config": {"Cmd": ["/bin/sarun", "oci", "--help"],
                   "Env": ["PATH=/bin"], "WorkingDir": "/"},
        "rootfs": {"type": "layers", "diff_ids": [diff_id]},
    }
    config_b = json.dumps(config).encode()
    config_digest = "sha256:" + _sha(config_b)
    manifest = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": config_digest, "size": len(config_b)},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": layer_digest, "size": len(gz)}],
    }
    manifest_b = json.dumps(manifest).encode()
    manifest_digest = "sha256:" + _sha(manifest_b)
    index = {
        "schemaVersion": 2,
        "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                       "digest": manifest_digest, "size": len(manifest_b),
                       "platform": {"architecture": "amd64", "os": "linux"}}],
    }
    (layout / "oci-layout").write_text(json.dumps({"imageLayoutVersion": "1.0.0"}))
    (layout / "index.json").write_bytes(json.dumps(index).encode())
    blobs = layout / "blobs" / "sha256"; blobs.mkdir(parents=True)
    (blobs / config_digest.split(":")[1]).write_bytes(config_b)
    (blobs / manifest_digest.split(":")[1]).write_bytes(manifest_b)
    (blobs / layer_digest.split(":")[1]).write_bytes(gz)
    return manifest_digest


def _tar_layout(layout, dest_tar):
    with tarfile.open(dest_tar, "w") as t:
        t.add(layout / "oci-layout", arcname="oci-layout")
        t.add(layout / "index.json", arcname="index.json")
        t.add(layout / "blobs", arcname="blobs")     # recursive


def build_archive(dest_tar, exe_path):
    """Write a synthetic single-layer oci-archive (a tar of an oci-layout)."""
    layout = Path(tempfile.mkdtemp(prefix="oci-layout-"))
    _build_layout(layout, Path(exe_path).read_bytes())
    _tar_layout(layout, dest_tar)
    shutil.rmtree(layout, ignore_errors=True)
    return dest_tar


def openssl_keypair(key_path, pub_path):
    """Generate an ECDSA P-256 keypair via the openssl CLI."""
    subprocess.run(["openssl", "ecparam", "-genkey", "-name", "prime256v1",
                    "-noout", "-out", str(key_path)], check=True, capture_output=True)
    subprocess.run(["openssl", "ec", "-in", str(key_path), "-pubout",
                    "-out", str(pub_path)], check=True, capture_output=True)


def build_signed_archive(dest_tar, exe_path, priv_key, extra):
    """Build an oci-archive whose image (content varied by `extra`) carries a
    cosign signature signed with `priv_key`. Returns the manifest digest."""
    layout = Path(tempfile.mkdtemp(prefix="oci-signed-"))
    try:
        md = _build_layout(layout, Path(exe_path).read_bytes(), extra)
        # cosign simple-signing payload naming this image's manifest digest.
        payload = json.dumps({
            "critical": {"identity": {"docker-reference": ""},
                         "image": {"docker-manifest-digest": md},
                         "type": "cosign container image signature"},
            "optional": None,
        }).encode()
        pf = layout / "_payload.json"; pf.write_bytes(payload)
        sig_der = subprocess.run(
            ["openssl", "dgst", "-sha256", "-sign", str(priv_key), str(pf)],
            check=True, capture_output=True).stdout
        pf.unlink()
        import base64 as _b64
        sig_b64 = _b64.b64encode(sig_der).decode()
        blobs = layout / "blobs" / "sha256"
        cfg = b"{}"; cfg_d = "sha256:" + _sha(cfg)
        pay_d = "sha256:" + _sha(payload)
        (blobs / cfg_d.split(":")[1]).write_bytes(cfg)
        (blobs / pay_d.split(":")[1]).write_bytes(payload)
        sigman = json.dumps({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": cfg_d, "size": len(cfg)},
            "layers": [{"mediaType": "application/vnd.dev.cosign.simplesigning.v1+json",
                        "digest": pay_d, "size": len(payload),
                        "annotations": {"dev.cosignproject.cosign/signature": sig_b64}}],
        }).encode()
        sigman_d = "sha256:" + _sha(sigman)
        (blobs / sigman_d.split(":")[1]).write_bytes(sigman)
        idx = json.loads((layout / "index.json").read_text())
        idx["manifests"].append({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": sigman_d, "size": len(sigman),
            "annotations": {"org.opencontainers.image.ref.name":
                            "sha256-" + md.split(":")[1] + ".sig"},
        })
        (layout / "index.json").write_bytes(json.dumps(idx).encode())
        _tar_layout(layout, dest_tar)
        return md
    finally:
        shutil.rmtree(layout, ignore_errors=True)


def build_tampered_layout(exe_path):
    """An oci-layout DIR like build_archive's, but the LAYER blob's content is
    corrupted while its filename stays the true manifest digest — so a
    digest-verifying loader must reject it. config/manifest stay valid (they're
    verified first; the loader must reach and fail on the layer)."""
    exe = Path(exe_path).read_bytes()
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as t:
        d = tarfile.TarInfo("bin"); d.type = tarfile.DIRTYPE; d.mode = 0o755; t.addfile(d)
        f = tarfile.TarInfo("bin/sarun"); f.size = len(exe); f.mode = 0o755
        t.addfile(f, io.BytesIO(exe))
    layer_raw = raw.getvalue()
    diff_id = "sha256:" + _sha(layer_raw)
    gz = gzip.compress(layer_raw)
    layer_digest = "sha256:" + _sha(gz)
    config = {"architecture": "amd64", "os": "linux",
              "config": {"Cmd": ["/bin/sarun", "oci", "--help"],
                         "Env": ["PATH=/bin"], "WorkingDir": "/"},
              "rootfs": {"type": "layers", "diff_ids": [diff_id]}}
    config_b = json.dumps(config).encode(); config_digest = "sha256:" + _sha(config_b)
    manifest = {"schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                           "digest": config_digest, "size": len(config_b)},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                            "digest": layer_digest, "size": len(gz)}]}
    manifest_b = json.dumps(manifest).encode(); manifest_digest = "sha256:" + _sha(manifest_b)
    index = {"schemaVersion": 2,
             "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                            "digest": manifest_digest, "size": len(manifest_b),
                            "platform": {"architecture": "amd64", "os": "linux"}}]}
    layout = Path(tempfile.mkdtemp(prefix="oci-tamper-"))
    (layout / "oci-layout").write_text(json.dumps({"imageLayoutVersion": "1.0.0"}))
    (layout / "index.json").write_bytes(json.dumps(index).encode())
    blobs = layout / "blobs" / "sha256"; blobs.mkdir(parents=True)
    (blobs / config_digest.split(":")[1]).write_bytes(config_b)
    (blobs / manifest_digest.split(":")[1]).write_bytes(manifest_b)
    # CORRUPT: filename is the true digest, content is not.
    (blobs / layer_digest.split(":")[1]).write_bytes(gz + b"corrupted")
    return layout


def box_names(sdir):
    out = set()
    for p in sdir.glob("*.sqlar"):
        try:
            c = sqlite3.connect(f"file:{p}?mode=ro", uri=True)
            row = c.execute("SELECT value FROM meta WHERE key='name'").fetchone()
            c.close()
            if row:
                out.add(row[0])
        except Exception:
            pass
    return out


def any_box_has_file(sdir, rel):
    for p in sdir.glob("*.sqlar"):
        try:
            c = sqlite3.connect(f"file:{p}?mode=ro", uri=True)
            row = c.execute("SELECT 1 FROM sqlar WHERE name=? LIMIT 1", (rel,)).fetchone()
            c.close()
            if row:
                return True
        except Exception:
            pass
    return False


def sarun(e, *args, timeout=180):
    return subprocess.run([str(ENGINE), *args], env=e,
                          capture_output=True, text=True, timeout=timeout)


def mnt_dir(e):
    return Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "mnt"


def meta_get(sqlar, key):
    """One meta value from a box's at-rest sqlar (None if absent/unreadable)."""
    try:
        c = sqlite3.connect(f"file:{sqlar}?mode=ro", uri=True)
        row = c.execute("SELECT value FROM meta WHERE key=?", (key,)).fetchone()
        c.close()
        return row[0] if row else None
    except Exception:
        return None


def box_id_by_name(sdir, name):
    for p in sdir.glob("*.sqlar"):
        if meta_get(p, "name") == name:
            return int(p.stem)
    return None


def children_of(sdir, parent_id):
    """Immediate child box ids — those whose sqlar meta parent_box_id == parent."""
    kids = []
    for p in sdir.glob("*.sqlar"):
        if meta_get(p, "parent_box_id") == str(parent_id):
            kids.append(int(p.stem))
    return kids


def ctl(e, msg, timeout=30):
    """Send one control message over the UI socket and return its JSON reply."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(str(sock_path(e)))
    s.sendall((json.dumps(msg) + "\n").encode())
    buf = b""
    while b"\n" not in buf:
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
    s.close()
    return json.loads(buf.split(b"\n", 1)[0] or b"{}")


def start_engine(e):
    proc = subprocess.Popen([str(ENGINE), "serve"], env=e,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if not wait_socket(sock_path(e)):
        out = proc.stdout.read(4000) if proc.stdout else b""
        raise RuntimeError("engine socket never appeared:\n"
                           + out.decode(errors="replace"))
    return proc


def stop_engine(proc):
    proc.send_signal(signal.SIGTERM)
    try: proc.wait(timeout=10)
    except Exception: proc.kill()


def main():
    if not ENGINE.exists():
        print(f" FAIL  engine binary not built at {ENGINE} — run `make engine` first")
        sys.exit(1)
    tmp = Path(tempfile.mkdtemp(prefix="sarun-oci-test-"))
    e = env_for(tmp)
    arch = build_archive(str(tmp / "img.tar"), ENGINE)
    print(f"synthetic archive: {arch} ({os.path.getsize(arch)//1024} KiB)")

    try:
        proc = start_engine(e)
    except RuntimeError as ex:
        print(" FAIL  " + str(ex)); _fails.append("engine start")
        shutil.rmtree(tmp, ignore_errors=True); sys.exit(1)
    try:
        # ── load ─────────────────────────────────────────────────────────────
        r = sarun(e, "oci", "load", f"oci-archive:{arch}", "SYN")
        check(r.returncode == 0, f"oci load exits 0 (stderr: {r.stderr.strip()[:200]})")
        check("loaded image" in r.stdout, "oci load reports a loaded image")
        check(len(list(state_dir(e).glob("*.sqlar"))) >= 1, "oci load created box sqlar(s)")
        check("SYN" in box_names(state_dir(e)), "base box is named SYN")

        # ── run ──────────────────────────────────────────────────────────────
        r = sarun(e, "oci", "run", "--net", "off", "SYN")
        both = r.stdout + r.stderr
        check(r.returncode == 0, f"oci run SYN exits 0 (stderr: {r.stderr.strip()[:300]})")
        check("oci load" in both and "usage" in both.lower(),
              "oci run SYN executed the image CMD (/bin/sarun oci --help)")

        # ── build ────────────────────────────────────────────────────────────
        ctx = tmp / "ctx"; ctx.mkdir()
        (ctx / "marker.txt").write_text("hello-oci-build\n")
        (ctx / "Dockerfile").write_text(
            "FROM SYN\n"
            "COPY marker.txt /marker.txt\n"
            'RUN ["/bin/sarun", "oci", "--help"]\n'   # exec form: no /bin/sh needed
            'CMD ["/bin/sarun", "oci", "--help"]\n')
        r = sarun(e, "oci", "build", "--net", "off", "-t", "BUILT", str(ctx))
        check(r.returncode == 0, f"oci build exits 0 (stderr: {r.stderr.strip()[:400]})")
        check("built image 'BUILT'" in r.stdout, "oci build reports built image BUILT")
        try: built_top = int(r.stdout.split("top box ")[-1].split()[0])
        except Exception: built_top = 0
        check(any_box_has_file(state_dir(e), "marker.txt"),
              "COPY landed marker.txt in a build layer box")

        # ── run the built result ─────────────────────────────────────────────
        r = sarun(e, "oci", "run", "--net", "off", "BUILT")
        both = r.stdout + r.stderr
        check(r.returncode == 0, f"oci run BUILT exits 0 (stderr: {r.stderr.strip()[:300]})")
        check("oci load" in both,
              "oci run BUILT executed its CMD (RUN step kept the chain runnable)")

        # ── build #2: COPY --from, glob, ADD-tar auto-extract, config carry ───
        # Exercises the v1 build-instruction gaps that were correctness holes:
        #   * multi-stage `COPY --from=<stage>` (cross-stage merged-view read)
        #   * glob sources (`COPY *.txt`)
        #   * ADD of a local .tar.gz AUTO-EXTRACTS (not copied as a blob)
        #   * STOPSIGNAL + HEALTHCHECK carried into the image config JSON
        ctx2 = tmp / "ctx2"; ctx2.mkdir()
        (ctx2 / "a.txt").write_text("aaa\n")
        (ctx2 / "b.txt").write_text("bbb\n")
        # a gzip'd tar with one entry `inside.txt` — ADD must extract it.
        braw = io.BytesIO()
        with tarfile.open(fileobj=braw, mode="w") as t:
            data = b"unpacked-content\n"
            ti = tarfile.TarInfo("inside.txt"); ti.size = len(data); ti.mode = 0o644
            t.addfile(ti, io.BytesIO(data))
        (ctx2 / "bundle.tar.gz").write_bytes(gzip.compress(braw.getvalue()))
        # xz and bzip2 tarballs (Docker auto-extracts these too). Written via
        # tarfile's own w:xz / w:bz2 so the magic bytes are authentic.
        def _make_tar(path, mode, member, content):
            with tarfile.open(path, mode=mode) as t:
                ti = tarfile.TarInfo(member); ti.size = len(content); ti.mode = 0o644
                t.addfile(ti, io.BytesIO(content))
        _make_tar(ctx2 / "bundle.tar.xz",  "w:xz",  "inxz.txt", b"xz-content\n")
        _make_tar(ctx2 / "bundle.tar.bz2", "w:bz2", "inbz.txt", b"bz2-content\n")
        (ctx2 / "Dockerfile").write_text(
            "FROM SYN AS stage1\n"
            "COPY a.txt /from_stage/a.txt\n"
            "FROM SYN\n"
            "COPY *.txt /globbed/\n"                       # glob
            "ADD bundle.tar.gz /unpacked/\n"              # gzip tar auto-extract
            "ADD bundle.tar.xz /unpacked_xz/\n"          # xz tar auto-extract
            "ADD bundle.tar.bz2 /unpacked_bz2/\n"        # bzip2 tar auto-extract
            "COPY --from=stage1 /from_stage/a.txt /copied_a.txt\n"  # cross-stage
            "STOPSIGNAL SIGQUIT\n"
            "HEALTHCHECK --interval=5s --retries=2 CMD /bin/true\n"
            'CMD ["/bin/sarun", "oci", "--help"]\n')
        r = sarun(e, "oci", "build", "--net", "off", "-t", "BUILT2", str(ctx2))
        check(r.returncode == 0, f"build2 exits 0 (stderr: {r.stderr.strip()[:500]})")
        try: built2_top = int(r.stdout.split("top box ")[-1].split()[0])
        except Exception: built2_top = 0
        check(any_box_has_file(state_dir(e), "globbed/a.txt")
              and any_box_has_file(state_dir(e), "globbed/b.txt"),
              "glob COPY *.txt landed a.txt AND b.txt")
        check(any_box_has_file(state_dir(e), "unpacked/inside.txt"),
              "ADD bundle.tar.gz AUTO-EXTRACTED (unpacked/inside.txt present, "
              "not a copied bundle.tar.gz blob)")
        check(not any_box_has_file(state_dir(e), "unpacked/bundle.tar.gz"),
              "ADD did NOT copy the tarball itself (it was extracted)")
        check(any_box_has_file(state_dir(e), "unpacked_xz/inxz.txt")
              and not any_box_has_file(state_dir(e), "unpacked_xz/bundle.tar.xz"),
              "ADD bundle.tar.xz AUTO-EXTRACTED (unpacked_xz/inxz.txt present, "
              "tarball not copied)")
        check(any_box_has_file(state_dir(e), "unpacked_bz2/inbz.txt")
              and not any_box_has_file(state_dir(e), "unpacked_bz2/bundle.tar.bz2"),
              "ADD bundle.tar.bz2 AUTO-EXTRACTED (unpacked_bz2/inbz.txt present, "
              "tarball not copied)")
        check(any_box_has_file(state_dir(e), "copied_a.txt"),
              "COPY --from=stage1 read the file from the other stage's view")
        cfg_raw = meta_get(state_dir(e) / f"{built2_top}.sqlar", "oci_config")
        cfg = json.loads(cfg_raw).get("config", {}) if cfg_raw else {}
        check(cfg.get("StopSignal") == "SIGQUIT",
              f"STOPSIGNAL carried into config (got {cfg.get('StopSignal')!r})")
        hc = cfg.get("Healthcheck") or {}
        # Shell-form `CMD /bin/true` → Test ["CMD-SHELL", …] (Docker semantics);
        # exec form `CMD ["…"]` would be ["CMD", …]. Durations → ns ints.
        check(hc.get("Test") == ["CMD-SHELL", "/bin/true"]
              and hc.get("Interval") == 5_000_000_000 and hc.get("Retries") == 2,
              f"HEALTHCHECK carried into config with ns interval (got {hc!r})")

        # ── image cache: oci run by the SAME reference reuses the loaded stack ─
        # SYN was loaded from this archive, so its oci_reference == the archive
        # ref. Running that ref again must reuse the loaded layer boxes — only a
        # fresh container box is added, no re-pull / new layer stack.
        before = len(list(state_dir(e).glob("*.sqlar")))
        r = sarun(e, "oci", "run", "--net", "off", f"oci-archive:{arch}")
        after = len(list(state_dir(e).glob("*.sqlar")))
        check(r.returncode == 0, f"oci run by archive ref exits 0 (stderr: {r.stderr.strip()[:200]})")
        check("reusing already-loaded image" in r.stderr,
              "oci run by ref reused the loaded stack (no re-pull)")
        check(after - before == 1,
              f"reuse added only the container box, not a new layer stack (+{after - before})")

        # ── image cache v2: coalesce by MANIFEST DIGEST across ref strings ────
        # The archive is a tar of an oci-layout. Extracting it yields the SAME
        # blobs/manifest → the SAME manifest digest as SYN. Running it by a
        # DIFFERENT reference string (oci-layout:<dir>) must coalesce onto SYN's
        # already-loaded stack (v2 key), not pull a second copy.
        lay = tmp / "layout"; lay.mkdir()
        with tarfile.open(arch) as t:
            t.extractall(lay)
        before2 = len(list(state_dir(e).glob("*.sqlar")))
        r = sarun(e, "oci", "run", "--net", "off", f"oci-layout:{lay}")
        after2 = len(list(state_dir(e).glob("*.sqlar")))
        check(r.returncode == 0, f"oci run by layout ref exits 0 (stderr: {r.stderr.strip()[:200]})")
        check("manifest" in r.stderr and "reusing already-loaded image" in r.stderr,
              f"oci run by a DIFFERENT ref string coalesced on the manifest digest "
              f"(stderr: {r.stderr.strip()[:200]})")
        check(after2 - before2 == 1,
              f"digest coalesce added only the container box, no new layer stack "
              f"(+{after2 - before2})")

        # ── in-box `oci build` runs HOST-SIDE in the engine ───────────────────
        # An in-box build must create its layer boxes (FROM/COPY/RUN/top) in the
        # engine's state, not through the box's own FUSE (which would make them
        # ephemeral). The CLI ships the context to the engine, which builds in a
        # host-side worker. Context must live on a host path the box can see —
        # NOT under /tmp (a box masks /tmp with a private tmpfs) — so stage it
        # under the repo dir, which the box sees via the host lower layer.
        ibctx = Path(tempfile.mkdtemp(prefix=".inbox-ctx-", dir=str(REPO)))
        try:
            (ibctx / "marker.txt").write_text("in-box-built\n")
            (ibctx / "Dockerfile").write_text(
                "FROM SYN\n"
                "COPY marker.txt /ib_marker.txt\n"
                'RUN ["/bin/sarun", "oci", "--help"]\n'   # exec form, no shell
                'CMD ["/bin/sarun", "oci", "--help"]\n')
            before_ib = len(list(state_dir(e).glob("*.sqlar")))
            # `sarun run BOX -- /proc/self/exe oci build ...`: the box command IS
            # the engine binary, so the in-box `oci build` exercises the in-box
            # code path (SARUN_BROKER set → ships to the engine).
            r = sarun(e, "run", "--net", "off", "TBUILD", "--",
                      "/proc/self/exe", "oci", "build", "--net", "off",
                      "-t", "INBOXBUILT", str(ibctx))
            both = r.stdout + r.stderr
            check(r.returncode == 0,
                  f"in-box oci build exits 0 (stderr: {r.stderr.strip()[-300:]})")
            check("built image 'INBOXBUILT'" in both,
                  "in-box build reported the built image")
            check("INBOXBUILT" in box_names(state_dir(e)),
                  "in-box-built top box persists in HOST state (engine created "
                  "it, not the box's ephemeral overlay)")
            check(any_box_has_file(state_dir(e), "ib_marker.txt"),
                  "in-box COPY landed ib_marker.txt in a HOST build layer")
            after_ib = len(list(state_dir(e).glob("*.sqlar")))
            check(after_ib - before_ib >= 3,
                  f"in-box build added host layer boxes (FROM/COPY/RUN/top) "
                  f"(+{after_ib - before_ib})")
            r2 = sarun(e, "oci", "run", "--net", "off", "INBOXBUILT")
            check(r2.returncode == 0 and "oci load" in (r2.stdout + r2.stderr),
                  "the in-box-built image runs on the host")
        finally:
            shutil.rmtree(ibctx, ignore_errors=True)

        # ── key-based cosign signature verification ───────────────────────────
        # A trust policy (cosign.toml) covering a reference makes a valid cosign
        # signature REQUIRED (fail closed). We sign synthetic archives with
        # openssl and check: a correctly-signed image loads and is reported
        # verified; an image signed with the WRONG key is rejected and creates no
        # box; with no policy, verification is skipped. Distinct image content
        # (the `extra` marker) gives each archive its own manifest digest so the
        # image cache doesn't mask a re-verification.
        keyA = tmp / "a.key"; pubA = tmp / "a.pub"; openssl_keypair(keyA, pubA)
        keyB = tmp / "b.key"; pubB = tmp / "b.pub"; openssl_keypair(keyB, pubB)
        cfg_dir = Path(e["XDG_CONFIG_HOME"]) / "slopbox"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        cosign_toml = cfg_dir / "cosign.toml"
        cosign_toml.write_text(
            f'[[verify]]\nmatch = "oci-archive:"\nkey_file = "{pubA}"\n')
        good = tmp / "good.tar"; build_signed_archive(good, ENGINE, keyA, b"good-content")
        bad = tmp / "bad.tar";  build_signed_archive(bad, ENGINE, keyB, b"bad-content")
        r = sarun(e, "oci", "load", f"oci-archive:{good}", "SIGNGOOD")
        check(r.returncode == 0 and "cosign signature verified" in r.stderr,
              f"cosign: correctly-signed image loads + reported verified "
              f"(rc={r.returncode}, stderr={r.stderr.strip()[-200:]})")
        r = sarun(e, "oci", "load", f"oci-archive:{bad}", "SIGNBAD")
        check(r.returncode != 0
              and "cosign verification failed" in (r.stdout + r.stderr),
              f"cosign: WRONG-key signature is rejected "
              f"(rc={r.returncode}, stderr={r.stderr.strip()[-200:]})")
        check("SIGNBAD" not in box_names(state_dir(e)),
              "cosign: a rejected image created no box (fail closed)")
        cosign_toml.unlink()   # no policy → no verification
        r = sarun(e, "oci", "load", f"oci-archive:{bad}", "SIGNBAD2")
        check(r.returncode == 0,
              f"cosign: with no policy the same image loads unverified "
              f"(rc={r.returncode})")

        # ── oci save: round-trip an image stack to an oci-archive ─────────────
        # Export the multi-layer BUILT image, re-load the archive, and run it —
        # proving the export is a valid, faithful, re-loadable OCI image (layer
        # chain + config carried forward through the inverse of load's ingest).
        saved = tmp / "saved.tar"
        r = sarun(e, "oci", "save", "BUILT", "-o", str(saved))
        check(r.returncode == 0 and "saved box 'BUILT'" in r.stdout,
              f"oci save BUILT → oci-archive (rc={r.returncode}, "
              f"out={r.stdout.strip()[-120:]})")
        check(saved.exists()
              and tarfile.open(saved).getnames()[:2] == ["oci-layout", "index.json"],
              "saved archive is an oci-layout tar")
        r = sarun(e, "oci", "load", f"oci-archive:{saved}", "BUILTSAVED")
        check(r.returncode == 0,
              f"re-load the saved archive (rc={r.returncode}, "
              f"stderr={r.stderr.strip()[-150:]})")
        r = sarun(e, "oci", "run", "--net", "off", "BUILTSAVED")
        check(r.returncode == 0 and "oci load" in (r.stdout + r.stderr),
              "the re-loaded saved image runs its CMD (faithful round-trip)")
        # A non-image (or absent) box can't be saved.
        r = sarun(e, "oci", "save", "NOSUCHBOX")
        check(r.returncode != 0, "oci save of an unknown box fails")

        # ── dissolve carries the no_host closure DOWN to the children ─────────
        # SYN is a no_host image base: its rootfs is CLOSED (absent paths ENOENT,
        # never the host's fs). Every container/build box sits on top of it. The
        # real delete path is `dissolve` — it finalizes SYN's own changes, copies
        # SYN's content DOWN into each immediate child that inherited it, frees
        # SYN, and re-parents those children onto SYN's own parent. SYN was loaded
        # --no-parent, so the grandparent is None (children go top-level). The new
        # thing under test: the closure (no_host_fallback) must ALSO copy down, or
        # a child re-parented to top-level silently re-opens to the host fs.
        syn_id = box_id_by_name(state_dir(e), "SYN")
        check(syn_id is not None, "found the SYN base box")
        check(meta_get(state_dir(e) / f"{syn_id}.sqlar", "no_host_fallback") == "1",
              "SYN base is closed (no_host_fallback=1) before dissolve")
        kids = children_of(state_dir(e), syn_id)
        check(len(kids) >= 1,
              f"SYN has immediate children to re-parent (got {len(kids)})")
        # Dissolve via the real control verb (not a manual sqlar unlink).
        resp = ctl(e, {"type": "ui", "verb": "dissolve", "args": [syn_id]})
        check(resp.get("ok") is True, f"dissolve SYN ok (resp: {resp})")
        check(not (state_dir(e) / f"{syn_id}.sqlar").exists(),
              "dissolve freed SYN's sqlar")
        for k in kids:
            kp = state_dir(e) / f"{k}.sqlar"
            check(meta_get(kp, "no_host_fallback") == "1",
                  f"child {k} inherited the no_host closure from dissolved SYN")
            check(meta_get(kp, "parent_box_id") in (None, ""),
                  f"child {k} re-parented onto SYN's (absent) parent → top-level")

        # ── digest verification: a corrupted layer blob is rejected ───────────
        # read_blob_by_digest hashes each oci-archive/oci-layout blob and bails
        # if it doesn't match the digest its descriptor claims — so a corrupted
        # or swapped blob can't silently become a box.
        bad = build_tampered_layout(ENGINE)
        before_bad = set(box_names(state_dir(e)))
        r = sarun(e, "oci", "load", f"oci-layout:{bad}", "TAMPER")
        out = (r.stderr + r.stdout).lower()
        check(r.returncode != 0,
              f"oci load of a tampered layout FAILS (rc={r.returncode})")
        check("digest mismatch" in out,
              f"load rejects the corrupted blob with a digest-mismatch error "
              f"(stderr: {r.stderr.strip()[:200]})")
        check(set(box_names(state_dir(e))) == before_bad,
              "tampered image created no box")
        shutil.rmtree(bad, ignore_errors=True)
        # End to end: restart, re-run BUILT. SYN's content (incl. /bin/sarun) was
        # copied down, so the box STILL boots and runs its CMD — and stays closed.
        stop_engine(proc)
        proc = start_engine(e)   # reassigned so `finally` stops the new engine
        r = sarun(e, "oci", "run", "--net", "off", "BUILT", timeout=120)
        both = r.stdout + r.stderr
        check(r.returncode == 0 and "oci load" in both,
              f"BUILT still boots+runs its CMD after its base was dissolved "
              f"(rc={r.returncode}, out={both.strip()[:200]})")
    finally:
        stop_engine(proc)
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        print(f"\nOCI FAIL ({len(_fails)} failed)")
        sys.exit(1)
    print("\nOCI PASS")
    sys.exit(0)


if __name__ == "__main__":
    main()
