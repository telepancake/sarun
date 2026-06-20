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


def build_archive(dest_tar, exe_path):
    """Write a synthetic single-layer oci-archive (a tar of an oci-layout) to
    `dest_tar`. rootfs = bin/ + bin/sarun (the static engine binary, 0755)."""
    exe = Path(exe_path).read_bytes()
    # 1. uncompressed rootfs layer tar
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as t:
        d = tarfile.TarInfo("bin"); d.type = tarfile.DIRTYPE; d.mode = 0o755
        t.addfile(d)
        f = tarfile.TarInfo("bin/sarun"); f.size = len(exe); f.mode = 0o755
        t.addfile(f, io.BytesIO(exe))
    layer_raw = raw.getvalue()
    diff_id = "sha256:" + _sha(layer_raw)
    gz = gzip.compress(layer_raw)
    layer_digest = "sha256:" + _sha(gz)
    # 2. image config
    config = {
        "architecture": "amd64", "os": "linux",
        "config": {"Cmd": ["/bin/sarun", "oci", "--help"],
                   "Env": ["PATH=/bin"], "WorkingDir": "/"},
        "rootfs": {"type": "layers", "diff_ids": [diff_id]},
    }
    config_b = json.dumps(config).encode()
    config_digest = "sha256:" + _sha(config_b)
    # 3. manifest
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
    # 4. index
    index = {
        "schemaVersion": 2,
        "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                       "digest": manifest_digest, "size": len(manifest_b),
                       "platform": {"architecture": "amd64", "os": "linux"}}],
    }
    # 5. assemble the oci-layout dir
    layout = Path(tempfile.mkdtemp(prefix="oci-layout-"))
    (layout / "oci-layout").write_text(json.dumps({"imageLayoutVersion": "1.0.0"}))
    (layout / "index.json").write_bytes(json.dumps(index).encode())
    blobs = layout / "blobs" / "sha256"; blobs.mkdir(parents=True)
    (blobs / config_digest.split(":")[1]).write_bytes(config_b)
    (blobs / manifest_digest.split(":")[1]).write_bytes(manifest_b)
    (blobs / layer_digest.split(":")[1]).write_bytes(gz)
    # 6. tar the layout
    with tarfile.open(dest_tar, "w") as t:
        t.add(layout / "oci-layout", arcname="oci-layout")
        t.add(layout / "index.json", arcname="index.json")
        t.add(layout / "blobs", arcname="blobs")     # recursive
    shutil.rmtree(layout, ignore_errors=True)
    return dest_tar


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
