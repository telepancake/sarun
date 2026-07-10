#!/usr/bin/env python3
"""git_checkout against the RUST engine: the ONE official way to get a
commit's files into a box is checking them out into the box's CHANGES —
ordinary captured rows + pool blobs, exactly as if the box had written
them. The store streams the commit (union folded once at the byte level
— geometric delta compose + one overlay — then walked as bytes), so
memory stays bounded; nothing tree-sized stays resident in the engine.

The RAM-resident git RO-attachment (TipReadout) is REMOVED: a decoded
whole-tree View pinned for the attachment's lifetime is not a readout
over an on-disk image, it is a resident dataset. This test proves the
replacement semantics:

  • git_checkout reply carries sha+files; NO new box is created
  • the checkout lands as captured rows: bytes exact, exec mode kept
  • the box reads the files through the mount like any of its changes
  • the files are the box's OWN changes — overwriting them SUCCEEDS
    (the old attachment was EROFS; a checkout is not an attachment)
  • DEST nests the checkout; SUBPATH checks out a subtree only
  • a tag-at-tree ref checks out the tagged tree, pinned by tag sha
  • the git_attach verb is GONE from the wire (named error, no row)

Needs FUSE + bwrap + git + a built gitdepot (cargo builds it if absent).
Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_git_checkout_rs.py
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


def sqlar_dir():
    return Path(os.environ["XDG_STATE_HOME"]) / "slopbox.GCO"


def sqlar_set():
    return {p.name for p in sqlar_dir().glob("*.sqlar")}


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
        raise SystemExit("test_git_checkout_rs: engine or gitdepot binary "
                         "unavailable — run `make engine` / cargo build")
    tmp = Path(tempfile.mkdtemp(prefix="gco-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "GCO"
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
        # Second commit so the checked-out ref is provably the NEWEST tree.
        (repo / "README").write_text("readme v2\n")
        sh_git(repo, "add", "-A")
        sh_git(repo, "commit", "-q", "-m", "v2")
        # A tag at a TREE (the linux v2.6.11-tree shape): one more lane in
        # the union, checkout-able by name, pinned by the tag's own sha.
        sh_git(repo, "config", "tag.gpgsign", "false")
        sh_git(repo, "tag", "-a", "-m", "tree tag", "treetag", "main^{tree}")
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

        def sid_of(name):
            rep = m.sync_request(sock, type="ui", verb="resolve_box",
                                 args=[name])
            return int((rep or {}).get("r"))

        # A working box, then check main out under /gitsdk.
        r = subprocess.run([str(BIN), "run", "WORK", "--", "true"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, f"setup run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        sid = sid_of("WORK")
        sp = sqlar_dir() / f"{sid}.sqlar"
        before = sqlar_set()

        # The old attach path is GONE from the wire: a named error, no row.
        rep = m.sync_request(sock, type="ui", verb="git_attach",
                             args=[sid, str(store), "main", "gitsdk"])
        gr = (rep or {}).get("r", rep or {})
        check(gr.get("ok") is not True,
              f"git_attach verb is gone from the wire (got {rep!r})")

        rep = m.sync_request(sock, type="ui", verb="git_checkout",
                             args=[sid, str(store), "main", "gitsdk"])
        rr = (rep or {}).get("r", {})
        check(rr.get("ok") is True, f"git_checkout verb succeeds (got {rep!r})")
        sha = rr.get("sha") or ""
        check(len(sha) == 40, f"reply pins the full commit sha (got {sha!r})")
        check(rr.get("files") == 3, f"reply counts the files (got {rr!r})")
        check(sqlar_set() == before,
              "no NEW box — the checkout lands in the existing box")

        # The files ARE the box's captured changes: bytes + modes exact.
        check(row_bytes(m, sp, "gitsdk/README") == b"readme v2\n",
              "checkout wrote the NEWEST commit's bytes as a captured row")
        check(row_bytes(m, sp, "gitsdk/sdk/tool.txt") == b"SDK from git\n",
              "nested file captured with exact bytes")
        rmode = rows(sp).get("gitsdk/sdk/run.sh", (0, 0, None))[1]
        check(rmode & 0o111 != 0,
              f"exec bit survived the git mode conversion (mode {rmode:o})")

        # Served through the mount like any change of the box.
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--", "sh", "-c",
             "cat /gitsdk/README > /readme.txt; "
             "test -x /gitsdk/sdk/run.sh && echo exec > /xbit.txt"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"checked-out tree probe exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        check(row_bytes(m, sp, "readme.txt") == b"readme v2\n",
              "box reads the checked-out bytes through the mount")
        check(row_bytes(m, sp, "xbit.txt") == b"exec\n",
              "exec bit visible in the box")

        # The checkout is the box's OWN change — overwriting SUCCEEDS
        # (the removed attachment was EROFS; this is not an attachment).
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--",
             "sh", "-c", "echo mine > /gitsdk/README"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"overwriting a checked-out file succeeds (rc={r.returncode}: {r.stderr[:200]})")
        check(row_bytes(m, sp, "gitsdk/README") == b"mine\n",
              "the overwrite landed in the box's own row")

        # SUBPATH: a second box takes only sdk/, nested under /vendor.
        r = subprocess.run([str(BIN), "run", "PART", "--", "true"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, f"PART setup exits 0 (rc={r.returncode})")
        sid2 = sid_of("PART")
        sp2 = sqlar_dir() / f"{sid2}.sqlar"
        rep = m.sync_request(sock, type="ui", verb="git_checkout",
                             args=[sid2, str(store), "main", "vendor", "sdk"])
        pr = (rep or {}).get("r", {})
        check(pr.get("ok") is True and pr.get("files") == 2,
              f"subtree checkout serves only sdk/ (got {rep!r})")
        check(row_bytes(m, sp2, "vendor/tool.txt") == b"SDK from git\n",
              "subtree file lands relative to DEST")
        check("vendor/README" not in rows(sp2) and "README" not in rows(sp2),
              "nothing outside the subtree was written")

        # Tag-at-tree: pinned by the TAG object's sha, serves the tagged
        # tree (one more lane in the union).
        rep = m.sync_request(sock, type="ui", verb="git_checkout",
                             args=[sid2, str(store), "treetag", "tagged"])
        tr = (rep or {}).get("r", {})
        check(tr.get("ok") is True, f"tree-tag checkout succeeds (got {rep!r})")
        tag_sha = sh_git(repo, "rev-parse", "refs/tags/treetag").strip()
        check(tr.get("sha") == tag_sha,
              f"tree-tag pin is the TAG object's sha (got {tr.get('sha')!r})")
        check(row_bytes(m, sp2, "tagged/README") == b"readme v2\n",
              "tree-tag checkout serves the tagged tree's bytes")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_git_checkout_rs: all checks passed")


def test_git_checkout_rs():
    main()


if __name__ == "__main__":
    main()
