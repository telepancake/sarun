#!/usr/bin/env python3
"""CLI-convenience + -C parity for the RUST engine binary (engine/):

  -C DIR          the box's working directory is DIR (the child's `pwd` is DIR,
                  echoed back to the runner's stdout via the live mux).
  sarun-engine NAME            select the box (exit 0 / 1)
  sarun-engine NAME patch      print the box's unified diff to stdout
  sarun-engine NAME apply      write the box's changes to the host, reap it
  sarun-engine NAME discard    drop the box's changes, reap it
  sarun-engine NAME rename NEW  rename the box (persisted to meta)

REAL effect assertions (a host file actually appears/doesn't, the diff text
carries the real path, the meta NAME changes), never shape-only. Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_cli_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import os, shutil, socket, stat as stat_mod, subprocess, sys, tempfile, time
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


def build_box(m, sid, name, rel, content):
    """A finished box (sqlar at rest) with one captured file change + a NAME."""
    bk = m.live_dir(sid); (bk / "up").mkdir(parents=True)
    ix = m.Index(bk); w = ix.writer_for(os.getpid())
    ix.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, w, "create")
    bp = m.blob_path(ix.box_id, ix.row_id(rel))
    bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(content)
    m.consolidate(str(bk), sid, index=ix); ix.close()
    m.sqlar_meta_set(m.sqlar_path(sid), "name", name)
    shutil.rmtree(bk, ignore_errors=True)


def main():
    if not ensure_binary():
        print("  ok  cli-rs: cargo/binary unavailable — SKIP")
        print("\nCLI-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="clirs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    aphost = Path("/root/cli_apply_proof.txt"); aphost.unlink(missing_ok=True)
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("rust engine socket never appeared")

        # ── -C chdir: the child runs IN the given directory ─────────────────
        # `pwd` prints the cwd; the live echo mux chains it back to our stdout.
        r = subprocess.run([str(BIN), "run", "-C", "/tmp", "--", "pwd"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, "cli-rs: -C run exits 0")
        check("/tmp" in r.stdout,
              f"cli-rs: -C set the box working dir (pwd echoed {r.stdout.strip()!r})")

        # ── CLI ops on a named box ──────────────────────────────────────────
        build_box(m, "9700", "CLIPATCH", "root/clip.txt", b"hello-cli\n")
        # bare NAME → select
        rs = subprocess.run([str(BIN), "CLIPATCH"], capture_output=True, text=True)
        check(rs.returncode == 0, "cli-rs: bare NAME selects the box (exit 0)")
        rs = subprocess.run([str(BIN), "NOSUCHBOX"], capture_output=True, text=True)
        check(rs.returncode != 0, "cli-rs: bare unknown NAME fails")
        # NAME patch → unified diff carrying the real path
        rp = subprocess.run([str(BIN), "CLIPATCH", "patch"],
                            capture_output=True, text=True)
        check(rp.returncode == 0 and "root/clip.txt" in rp.stdout,
              f"cli-rs: NAME patch prints the diff (head={rp.stdout[:60]!r})")
        # NAME rename NEW → persisted to meta
        rr = subprocess.run([str(BIN), "CLIPATCH", "rename", "CLIP2"],
                            capture_output=True, text=True)
        check(rr.returncode == 0
              and m.sqlar_meta_get(m.sqlar_path("9700"), "name") == "CLIP2",
              "cli-rs: NAME rename persists the new name to meta")

        # NAME apply → writes the change to the host, reaps the box
        build_box(m, "9701", "CLIAPPLY", "root/cli_apply_proof.txt", b"applied-cli\n")
        ra = subprocess.run([str(BIN), "CLIAPPLY", "apply"],
                            capture_output=True, text=True)
        check(ra.returncode == 0, "cli-rs: NAME apply exits 0")
        check(aphost.exists() and aphost.read_bytes() == b"applied-cli\n",
              "cli-rs: NAME apply wrote the change to the real host")
        check(not m.sqlar_path("9701").exists(),
              "cli-rs: NAME apply reaped the emptied box")

        # NAME discard → drops the change, host untouched, box reaped
        dishost = Path("/root/cli_discard_proof.txt"); dishost.unlink(missing_ok=True)
        build_box(m, "9702", "CLIDISCARD", "root/cli_discard_proof.txt", b"nope\n")
        rd = subprocess.run([str(BIN), "CLIDISCARD", "discard"],
                            capture_output=True, text=True)
        check(rd.returncode == 0, "cli-rs: NAME discard exits 0")
        check(not dishost.exists(),
              "cli-rs: NAME discard did NOT touch the host")
        check(not m.sqlar_path("9702").exists(),
              "cli-rs: NAME discard reaped the box")
    finally:
        aphost.unlink(missing_ok=True)
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("CLI-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_cli_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
