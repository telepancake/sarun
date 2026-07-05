#!/usr/bin/env python3
"""ATTACH-CONVERGENCE proofs against the RUST engine (attach-
implementation-plan commit 7): the §8 independence invariant and the
laziness guarantee of external (Ext-row) RO attachments.

§8 byte-identical invariant: the same deterministic workload runs in a
box WITHOUT an attachment and in a box WITH a git attachment, writing
BESIDE the attachment (never touching matched keys). The two boxes'
captured sqlar tables must be byte-identical (name/mode/sz/content,
ordered by name; rowids and mtimes excluded — allocation order and
clocks are not semantics). Plus: an EROFS write to a matched key leaves
no captured row.

Laziness: attaching a ~200-file git store is bookkeeping only — the
attach RPC returns fast, creates no NN.sqlar and no depot-cache pool
entries; reading ONE file materializes a small number of pool blobs
(never ~200) and still imports nothing.

Needs FUSE + bwrap + git + a built gitdepot (cargo builds it if
absent). Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_attach_convergence_rs.py
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
    return Path(os.environ["XDG_STATE_HOME"]) / "slopbox.ACV"


def sqlar_set():
    return {p.name for p in state_dir().glob("*.sqlar")}


def pool_files():
    root = state_dir() / "cache" / "blob"
    if not root.exists():
        return set()
    return {p for p in root.rglob("*") if p.is_file()}


def captured(m, sp):
    """(name, mode, sz, content) per captured row, ordered by name —
    rowids and mtimes excluded (allocation order / clocks, not
    semantics). Content follows external blobs via blob_path."""
    out = []
    with sqlite3.connect(f"file:{sp}?mode=ro", uri=True) as c:
        for rowid, name, mode, sz, data in c.execute(
                "SELECT rowid,name,mode,sz,data FROM sqlar ORDER BY name"):
            if data is None:
                bp = m.blob_path(int(sp.stem), rowid)
                data = bp.read_bytes() if bp.exists() else b""
            out.append((name, mode, sz, bytes(data)))
    return out


def run_box(name, script):
    return subprocess.run([str(BIN), "run", name, "--", "sh", "-c", script],
                          capture_output=True, text=True, timeout=60)


def main():
    if not ensure_binaries():
        raise SystemExit("test_attach_convergence_rs: engine or gitdepot "
                         "binary unavailable — run `make engine` / cargo build")
    tmp = Path(tempfile.mkdtemp(prefix="acv-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "ACV"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # Small store for the §8 proof; big (~200 files) for laziness.
        repo = tmp / "repo"
        repo.mkdir()
        sh_git(repo, "init", "-q", "-b", "main")
        sh_git(repo, "config", "commit.gpgsign", "false")
        (repo / "tool.txt").write_text("attached tool\n")
        sh_git(repo, "add", "-A")
        sh_git(repo, "commit", "-q", "-m", "v1")
        store = tmp / "store"
        r = subprocess.run([str(GITDEPOT), "import", str(repo), str(store)],
                           capture_output=True, text=True)
        check(r.returncode == 0,
              f"gitdepot import small (rc={r.returncode}: {r.stderr[:200]})")

        big = tmp / "bigrepo"
        big.mkdir()
        sh_git(big, "init", "-q", "-b", "main")
        sh_git(big, "config", "commit.gpgsign", "false")
        for i in range(200):
            (big / f"f_{i:03d}.txt").write_text(f"file {i}\n" * 4)
        sh_git(big, "add", "-A")
        sh_git(big, "commit", "-q", "-m", "big")
        bigstore = tmp / "bigstore"
        r = subprocess.run([str(GITDEPOT), "import", str(big), str(bigstore)],
                           capture_output=True, text=True)
        check(r.returncode == 0,
              f"gitdepot import big (rc={r.returncode}: {r.stderr[:200]})")
        shutil.rmtree(big, ignore_errors=True)  # disk is tight

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        sock = m.sock_path()

        def sid_of(name):
            rep = m.sync_request(sock, type="ui", verb="resolve_box",
                                 args=[name])
            return int((rep or {}).get("r"))

        # Two boxes; ATTD gets the attachment, PLAIN stays bare.
        for name in ("PLAIN", "ATTD"):
            r = subprocess.run([str(BIN), "run", name, "--", "true"],
                               capture_output=True, text=True, timeout=60)
            check(r.returncode == 0,
                  f"setup run {name} exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        plain_sid, attd_sid = sid_of("PLAIN"), sid_of("ATTD")
        rep = m.sync_request(sock, type="ui", verb="git_attach",
                             args=[attd_sid, str(store), "main", "sdk"])
        rr = (rep or {}).get("r", {})
        check(rr.get("ok") is True and "box" not in rr,
              f"git_attach is bookkeeping-only (got {rep!r})")

        # §8: same deterministic workload, writes BESIDE the attachment.
        workload = ("printf 'hi\\n' > /out.txt; mkdir -p /d; "
                    "printf 'nested\\n' > /d/f.txt; chmod 640 /d/f.txt")
        for name in ("PLAIN", "ATTD"):
            r = run_box(name, workload)
            check(r.returncode == 0,
                  f"workload in {name} exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        plain_rows = captured(m, m.sqlar_path(str(plain_sid)))
        attd_rows = captured(m, m.sqlar_path(str(attd_sid)))
        check(plain_rows == attd_rows,
              "captured layers byte-identical with/without attachment "
              f"(plain {len(plain_rows)} rows vs attd {len(attd_rows)})")
        check(any(n == "out.txt" and c == b"hi\n"
                  for n, _m, _s, c in plain_rows),
              "workload really captured its writes (out.txt row present)")

        # EROFS on a matched key leaves NO captured row.
        r = run_box("ATTD", "echo overwrite > /sdk/tool.txt")
        check(r.returncode != 0, "write to attached key fails (EROFS)")
        check(all(n != "sdk/tool.txt"
                  for n, _m, _s, _c in captured(m, m.sqlar_path(str(attd_sid)))),
              "rejected write left NO captured row")

        # Laziness: attach the ~200-file store — instant bookkeeping.
        boxes_before = sqlar_set()
        pool_before = pool_files()
        t0 = time.monotonic()
        rep = m.sync_request(sock, type="ui", verb="git_attach",
                             args=[attd_sid, str(bigstore), "main", "big"])
        dt = time.monotonic() - t0
        check((rep or {}).get("r", {}).get("ok") is True,
              f"big git_attach succeeds (got {rep!r})")
        check(dt < 5.0, f"attach RPC is metadata-fast ({dt:.2f}s)")
        check(sqlar_set() == boxes_before,
              "big attach created no NN.sqlar (no import)")
        check(pool_files() == pool_before,
              "big attach created no cache pool entries (no decode)")

        # Read ONE file: a small number of pool blobs, never ~200.
        r = run_box("ATTD", "cat /big/f_007.txt > /one.txt")
        check(r.returncode == 0 and
              (dict(((n, c) for n, _m, _s, c in
                     captured(m, m.sqlar_path(str(attd_sid)))))
               .get("one.txt") == b"file 7\n" * 4),
              "one attached file reads through correctly")
        new_pool = len(pool_files() - pool_before)
        check(1 <= new_pool <= 5,
              f"one read materialized ~one pool blob, not 200 (got {new_pool})")
        check(sqlar_set() == boxes_before,
              "still no NN.sqlar after serving the read (lazy end to end)")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_attach_convergence_rs: all checks passed")


def test_attach_convergence_rs():
    main()


if __name__ == "__main__":
    main()
