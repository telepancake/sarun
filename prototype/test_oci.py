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


def main():
    if not ENGINE.exists():
        print(f" FAIL  engine binary not built at {ENGINE} — run `make engine` first")
        sys.exit(1)
    tmp = Path(tempfile.mkdtemp(prefix="sarun-oci-test-"))
    e = env_for(tmp)
    arch = build_archive(str(tmp / "img.tar"), ENGINE)
    print(f"synthetic archive: {arch} ({os.path.getsize(arch)//1024} KiB)")

    proc = subprocess.Popen([str(ENGINE), "serve"], env=e,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        if not wait_socket(sock_path(e)):
            out = b""
            try: out = proc.stdout.read(4000) if proc.stdout else b""
            except Exception: pass
            print(" FAIL  engine socket never appeared:\n" + out.decode(errors="replace"))
            _fails.append("engine start")
            return

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
        check(any_box_has_file(state_dir(e), "marker.txt"),
              "COPY landed marker.txt in a build layer box")

        # ── run the built result ─────────────────────────────────────────────
        r = sarun(e, "oci", "run", "--net", "off", "BUILT")
        both = r.stdout + r.stderr
        check(r.returncode == 0, f"oci run BUILT exits 0 (stderr: {r.stderr.strip()[:300]})")
        check("oci load" in both,
              "oci run BUILT executed its CMD (RUN step kept the chain runnable)")
    finally:
        proc.send_signal(signal.SIGTERM)
        try: proc.wait(timeout=10)
        except Exception: proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        print(f"\nOCI FAIL ({len(_fails)} failed)")
        sys.exit(1)
    print("\nOCI PASS")
    sys.exit(0)


if __name__ == "__main__":
    main()
