#!/usr/bin/env python3
"""git_attach (MIRRORS.md phase 4) against the RUST engine: a git ref
from a gitdepot mirror store attaches to a box as a READ-ONLY layer —
no checkout, no working tree. The verb resolves the ref's view from the
store, imports it as a fresh at-rest box, and appends it to the box's
RO attachments; from there DEPOT-DESIGN.md §8 semantics apply.

Real-effect assertions (never shape-only):
  • the imported box's sqlar holds the repo's files (modes converted)
  • read-through: `cat` of an attached repo file captures its bytes
  • EROFS: writing an attached repo key fails, leaving no captured row
  • the attachment shows in the session's parents (UI DAG visibility)

Needs FUSE + bwrap + git + a built gitdepot (cargo builds it if absent).
Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_git_attach_rs.py
"""
import os, shutil, socket, sqlite3, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"
GIMIR = _HERE.parent / "gimir"
GITDEPOT = GIMIR / "target/debug/gitdepot"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binaries() -> bool:
    if not BIN.exists():
        if shutil.which("cargo") is None:
            return False
        r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
                           capture_output=True, text=True)
        if r.returncode != 0 or not BIN.exists():
            return False
    if not GITDEPOT.exists():
        r = subprocess.run(["cargo", "build", "-p", "gitdepot"], cwd=GIMIR,
                           capture_output=True, text=True)
        if r.returncode != 0 or not GITDEPOT.exists():
            return False
    return True


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def sh_git(repo, *args):
    env = dict(os.environ,
               GIT_AUTHOR_NAME="T", GIT_AUTHOR_EMAIL="t@x",
               GIT_COMMITTER_NAME="T", GIT_COMMITTER_EMAIL="t@x",
               GIT_AUTHOR_DATE="2026-01-02T03:04:05Z",
               GIT_COMMITTER_DATE="2026-01-02T03:04:05Z")
    r = subprocess.run(["git", "-C", str(repo), *args],
                       capture_output=True, text=True, env=env)
    assert r.returncode == 0, f"git {args}: {r.stderr}"
    return r.stdout


def newest_sqlar():
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.GAT")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def rows(sp):
    with sqlite3.connect(f"file:{sp}?mode=ro", uri=True) as c:
        return {name: (rowid, mode, data) for rowid, name, mode, data in
                c.execute("SELECT rowid,name,mode,data FROM sqlar")}


def row_bytes(m, sp, name):
    r = rows(sp).get(name)
    if r is None:
        return None
    rowid, _mode, data = r
    if data is not None:
        return bytes(data)
    bp = m.blob_path(int(sp.stem), rowid)
    return bp.read_bytes() if bp.exists() else b""


def main():
    if not ensure_binaries():
        raise SystemExit("test_git_attach_rs: engine or gitdepot binary "
                         "unavailable — run `make engine` / cargo build")
    tmp = Path(tempfile.mkdtemp(prefix="gat-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "GAT"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # A small git repo → gitdepot store (both host-side inputs).
        repo = tmp / "repo"
        repo.mkdir()
        sh_git(repo, "init", "-q", "-b", "main")
        sh_git(repo, "config", "commit.gpgsign", "false")
        (repo / "sdk").mkdir()
        (repo / "sdk/tool.txt").write_text("SDK from git\n")
        (repo / "sdk/run.sh").write_text("#!/bin/sh\necho hi\n")
        os.chmod(repo / "sdk/run.sh", 0o755)
        (repo / "README").write_text("readme v1\n")
        sh_git(repo, "add", "-A")
        sh_git(repo, "commit", "-q", "-m", "v1")
        # Second commit so the attached ref is provably the NEWEST view.
        (repo / "README").write_text("readme v2\n")
        sh_git(repo, "add", "-A")
        sh_git(repo, "commit", "-q", "-m", "v2")
        store = tmp / "store"
        r = subprocess.run([str(GITDEPOT), "import", str(repo), str(store)],
                           capture_output=True, text=True)
        check(r.returncode == 0, f"gitdepot import (rc={r.returncode}: {r.stderr[:200]})")

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        sock = m.sock_path()

        # A working box, then attach main under /gitsdk.
        r = subprocess.run([str(BIN), "run", "WORK", "--", "true"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, f"setup run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        sp = newest_sqlar()
        sid = int(sp.stem)
        rep = m.sync_request(sock, type="ui", verb="git_attach",
                             args=[sid, str(store), "main", "gitsdk"])
        rr = (rep or {}).get("r", {})
        check(rr.get("ok") is True, f"git_attach verb succeeds (got {rep!r})")
        gid = rr.get("box")

        # The imported box holds the repo's files, right modes, no checkout.
        gsp = m.sqlar_path(str(gid))
        check(row_bytes(m, gsp, "gitsdk/README") == b"readme v2\n",
              "imported box serves the NEWEST commit's bytes")
        grows = rows(gsp)
        check("gitsdk/sdk/run.sh" in grows
              and (grows["gitsdk/sdk/run.sh"][1] & 0o111) != 0,
              "executable bit survived the git mode conversion")
        with sqlite3.connect(f"file:{gsp}?mode=ro", uri=True) as c:
            name = dict(c.execute("SELECT key,value FROM meta")).get("name")
        check(name and name.startswith("git:main@"),
              f"imported box named for the ref (got {name!r})")

        # Read-through into the working box's captured layer.
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--",
             "sh", "-c", "cat /gitsdk/sdk/tool.txt > /copied.txt"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"read of attached git file succeeds (rc={r.returncode}: {r.stderr[:200]})")
        check(row_bytes(m, sp, "copied.txt") == b"SDK from git\n",
              "attached bytes read through into a captured row")

        # EROFS on matched keys; no capture side effect.
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--",
             "sh", "-c", "echo overwrite > /gitsdk/README"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode != 0, "write to attached git key fails")
        check("gitsdk/README" not in rows(sp),
              "rejected write left NO captured row")

        # UI DAG visibility: the attachment is one of WORK's parents.
        rep = m.sync_request(sock, type="ui", verb="session_dicts", args=[])
        sessions = (rep or {}).get("r", [])
        mine = next((s for s in sessions if s.get("box_id") == sid), {})
        check(gid in (mine.get("parents") or []),
              f"attachment listed in session parents (got {mine.get('parents')!r})")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_git_attach_rs: all checks passed")


def test_git_attach_rs():
    main()


if __name__ == "__main__":
    main()
