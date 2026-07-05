#!/usr/bin/env python3
"""External (Ext-row) RO attachment END-TO-END probe against the RUST
engine (attach-implementation-plan commits 3-5): a git mirror store is
attached to a box as a `RoAttachment::Ext` bookkeeping row — NO import,
NO new at-rest box — and served straight from the store through the
overlay's ChainLink::Ext arm + the depot cache.

Injection path: the generic `ro_attach` verb, which accepts object rows
{kind,store,ref,rev,prefix,name} alongside historical int box ids.

Real-effect assertions (never shape-only):
  • the attached file is readable through the mount at the prefix
  • getattr size is right WITHOUT decoding (stat through the mount)
  • EROFS on overwrite of an attached key; no captured row
  • LAZINESS: no new NN.sqlar box file appears in state_home
  • repeat read works (second open = cache pool hit for Bytes blobs)
  • the Bytes blob landed in state_home/cache/blob (the §7 pool)

Needs FUSE + bwrap + git + a built gitdepot (cargo builds it if absent).
Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_ext_attach_probe_rs.py
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


def state_dir():
    return Path(os.environ["XDG_STATE_HOME"]) / "slopbox.EXT"


def sqlar_set():
    return {p.name for p in state_dir().glob("*.sqlar")}


def newest_sqlar():
    return max(state_dir().glob("*.sqlar"), key=lambda p: int(p.stem))


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
        raise SystemExit("test_ext_attach_probe_rs: engine or gitdepot "
                         "binary unavailable — run `make engine` / cargo build")
    tmp = Path(tempfile.mkdtemp(prefix="ext-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "EXT"
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
        (repo / "sdk/tool.txt").write_text("SDK via ext row\n")
        (repo / "README").write_text("readme ext\n")
        sh_git(repo, "add", "-A")
        sh_git(repo, "commit", "-q", "-m", "v1")
        sha = sh_git(repo, "rev-parse", "HEAD").strip()
        store = tmp / "store"
        r = subprocess.run([str(GITDEPOT), "import", str(repo), str(store)],
                           capture_output=True, text=True)
        check(r.returncode == 0,
              f"gitdepot import (rc={r.returncode}: {r.stderr[:200]})")

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        sock = m.sock_path()

        # A working box, then inject the Ext row via the generic verb.
        r = subprocess.run([str(BIN), "run", "WORK", "--", "true"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"setup run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        sp = newest_sqlar()
        sid = int(sp.stem)
        before = sqlar_set()
        ext = {"kind": "git", "store": str(store), "ref": "main",
               "rev": sha, "prefix": "extsdk",
               "name": f"git:repo/main@{sha[:8]}"}
        rep = m.sync_request(sock, type="ui", verb="ro_attach",
                             args=[sid, ext])
        check((rep or {}).get("r", {}).get("ok") is True,
              f"ro_attach accepts an Ext object row (got {rep!r})")

        # Laziness tell #1: attaching imported NOTHING — no new box sqlar.
        check(sqlar_set() == before,
              "no new NN.sqlar appeared on attach (reference, not import)")

        # Read + getattr through the mount at the prefix.
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--", "sh", "-c",
             "cat /extsdk/sdk/tool.txt > /copied.txt; "
             "stat -c %s /extsdk/sdk/tool.txt > /size.txt; "
             "ls /extsdk > /listing.txt"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"read of ext-attached file succeeds (rc={r.returncode}: {r.stderr[:200]})")
        check(row_bytes(m, sp, "copied.txt") == b"SDK via ext row\n",
              "attached bytes read through into a captured row")
        check(row_bytes(m, sp, "size.txt") == b"16\n",
              f"getattr size matches the entry "
              f"(got {row_bytes(m, sp, 'size.txt')!r})")
        listing = (row_bytes(m, sp, "listing.txt") or b"").decode()
        check("README" in listing and "sdk" in listing,
              f"merged listing shows attachment names (got {listing!r})")

        # The Bytes blob landed in the §7 cache pool.
        pool = list((state_dir() / "cache" / "blob").rglob("*"))
        check(any(p.is_file() for p in pool),
              f"cache pool holds the served blob ({len(pool)} entries)")

        # Repeat read: second open dedupes onto the same pool path.
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--", "sh", "-c",
             "cat /extsdk/sdk/tool.txt > /copied2.txt"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0 and
              row_bytes(m, sp, "copied2.txt") == b"SDK via ext row\n",
              "second read (cache hit) serves the same bytes")

        # EROFS on matched keys; no capture side effect.
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--", "sh", "-c",
             "echo overwrite > /extsdk/README"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode != 0, "write to ext-attached key fails (EROFS)")
        check("extsdk/README" not in rows(sp),
              "rejected write left NO captured row")

        # Laziness tell #2: still no imported box after all the reads.
        check(sqlar_set() == before,
              "still no new NN.sqlar after serving reads (lazy end to end)")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_ext_attach_probe_rs: all checks passed")


def test_ext_attach_probe_rs():
    main()


if __name__ == "__main__":
    main()
