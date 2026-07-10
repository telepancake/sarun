#!/usr/bin/env python3
"""Mirror serve path through the REAL CLI: bounded single-object
adapters (`sarun NAME attach wiki|ietf …` — ATTACH-CONVERGENCE.md) plus
the git CHECKOUT path (`sarun NAME checkout <store> <ref> [DEST]`) —
git attach was REMOVED by design (no RAM-resident datasets; a commit is
checked out into box changes instead), and this test pins the refusal.

Real-effect assertions (never shape-only):
  • `attach git` refuses loudly, pointing at checkout; no box appears
  • wiki/ietf attaches are bookkeeping-only: box count never changes
  • checkout streams the commit's bytes into WRITABLE box rows
  • each attached object's bytes are readable in the box (captured copy)
  • a write to an ATTACHED key is EROFS with NO captured row
  • all three attachments appear in the session dict's "attachments",
    named for their objects (git:…/main@sha8, wiki:…@rN, ietf:…@rev)

Needs FUSE + bwrap + git; builds gitdepot/wikimak/ietfmak via cargo if
absent. Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_mirror_attach_rs.py
"""
import http.server, os, shutil, socket, sqlite3, subprocess, sys
import tempfile, threading, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"
GIMIR = _HERE.parent / "gimir"
TOOLS = {name: GIMIR / f"target/debug/{name}"
         for name in ("gitdepot", "wikimak", "ietfmak")}

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binaries() -> bool:
    if not BIN.exists():
        r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
                           capture_output=True, text=True)
        if r.returncode != 0 or not BIN.exists():
            return False
    missing = [n for n, p in TOOLS.items() if not p.exists()]
    if missing:
        r = subprocess.run(
            ["cargo", "build", "-p", "gitdepot", "-p", "wikimak-wikipedia",
             "-p", "ietf-mirror"],
            cwd=GIMIR, capture_output=True, text=True)
        if r.returncode != 0:
            print(r.stderr[-2000:])
            return False
    return all(p.exists() for p in TOOLS.values())


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


class IetfHandler(http.server.BaseHTTPRequestHandler):
    DOCS = {
        # REAL all_id.txt shape: ONE line per draft, at its LATEST
        # revision; the mirror enumerates 00..01 from the head number.
        "/id/all_id.txt":
            "draft-test-mesh-01\t2026-02-01\tActive\n",
        "/archive/id/draft-test-mesh-00.txt": "mesh draft rev zero\n",
        "/archive/id/draft-test-mesh-01.txt": "mesh draft rev one\n",
    }
    def do_GET(self):
        body = self.DOCS.get(self.path)
        if body is None:
            self.send_response(404); self.end_headers(); return
        b = body.encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def log_message(self, *a):
        pass


def build_mirrors(tmp: Path):
    """git store, wikimak instance, ietf root — all host-side inputs."""
    # git
    repo = tmp / "repo"; repo.mkdir()
    sh_git(repo, "init", "-q", "-b", "main")
    sh_git(repo, "config", "commit.gpgsign", "false")
    (repo / "tool.txt").write_text("git tool v1\n")
    sh_git(repo, "add", "-A"); sh_git(repo, "commit", "-q", "-m", "v1")
    store = tmp / "gitstore"
    r = subprocess.run([str(TOOLS["gitdepot"]), "import", str(repo), str(store)],
                       capture_output=True, text=True)
    check(r.returncode == 0, f"gitdepot import (rc={r.returncode}: {r.stderr[:200]})")

    # wikipedia (fixture dump from the gimir test suite)
    dump = GIMIR / "wikimak/wikipedia/tests/data/export_three_pages.xml"
    wroot = tmp / "wiki"
    r = subprocess.run([str(TOOLS["wikimak"]), "import", str(dump), str(wroot)],
                       capture_output=True, text=True)
    check(r.returncode == 0, f"wikimak import (rc={r.returncode}: {r.stderr[:200]})")

    # ietf (update against a local stand-in host)
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), IetfHandler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    iroot = tmp / "ietf"
    env = dict(os.environ,
               IETFMAK_BASE_URL=f"http://127.0.0.1:{srv.server_port}")
    r = subprocess.run([str(TOOLS["ietfmak"]), "update", str(iroot)],
                       capture_output=True, text=True, env=env)
    srv.shutdown()
    check(r.returncode == 0, f"ietfmak update (rc={r.returncode}: {r.stderr[:300]})")
    return store, wroot, iroot


def newest_sqlar():
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.MAT")
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
        raise SystemExit("test_mirror_attach_rs: binaries unavailable")
    tmp = Path(tempfile.mkdtemp(prefix="mat-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "MAT"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        store, wroot, iroot = build_mirrors(tmp)

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        sock = m.sock_path()

        r = subprocess.run([str(BIN), "run", "WORK", "--", "true"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, f"setup run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        sp = newest_sqlar()
        sid = int(sp.stem)

        # git attach is GONE by design: the CLI refuses and points at
        # checkout. The bounded adapters (wiki/ietf) attach as before;
        # the git commit is CHECKED OUT into box changes.
        before = {p.name for p in Path(os.environ["XDG_STATE_HOME"])
                  .joinpath("slopbox.MAT").glob("*.sqlar")}
        r = subprocess.run([str(BIN), "WORK", "attach", "git", str(store),
                            "main", "gitsdk"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode != 0 and "checkout" in (r.stderr + r.stdout),
              f"attach git refuses, pointing at checkout "
              f"(rc={r.returncode}: {(r.stderr or r.stdout)[:200]})")
        for cli, want in ((["attach", "wiki", str(wroot), "1", "wiki"],
                           "attached wiki:wiki/Old Title@r"),
                          (["attach", "ietf", str(iroot), "draft-test-mesh",
                            "ietf"],
                           "attached ietf:draft-test-mesh@01")):
            r = subprocess.run([str(BIN), "WORK", *cli],
                               capture_output=True, text=True, timeout=60)
            check(r.returncode == 0 and want in r.stdout,
                  f"CLI {' '.join(cli[:2])} succeeds, prints name+rev "
                  f"(rc={r.returncode}: {(r.stderr or r.stdout)[:200]})")
        r = subprocess.run([str(BIN), "WORK", "checkout", str(store),
                            "main", "gitsdk"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"checkout streams the commit into box changes "
              f"(rc={r.returncode}: {(r.stderr or r.stdout)[:200]})")
        after = {p.name for p in Path(os.environ["XDG_STATE_HOME"])
                 .joinpath("slopbox.MAT").glob("*.sqlar")}
        check(after == before,
              "box count unchanged by attaches + checkout (no import)")

        # Read every attached object through the box; capture proves it.
        script = ("cat /gitsdk/tool.txt '/wiki/Old Title.txt' "
                  "/ietf/draft-test-mesh-01.txt > /gathered.txt")
        r = subprocess.run([str(BIN), "run", "WORK", "--", "sh", "-c", script],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"box reads all three mirrors (rc={r.returncode}: {r.stderr[:300]})")
        gathered = row_bytes(m, sp, "gathered.txt") or b""
        check(b"git tool v1" in gathered, "git bytes read through")
        check(b"mesh draft rev one" in gathered, "ietf HEAD bytes read through")
        check(len(gathered) > len(b"git tool v1\nmesh draft rev one\n"),
              "wiki page text read through (non-trivial bytes)")

        # Checked-out git rows are ORDINARY box changes: writable.
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--", "sh", "-c",
             "echo overwrite > /gitsdk/tool.txt"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"checked-out git file is writable (rc={r.returncode})")

        # EROFS on each ATTACHED kind's key; no capture side effect.
        for key in ("'/wiki/Old Title.txt'",
                    "/ietf/draft-test-mesh-00.txt"):
            r = subprocess.run(
                [str(BIN), "run", "WORK", "--", "sh", "-c",
                 f"echo overwrite > {key}"],
                capture_output=True, text=True, timeout=60)
            check(r.returncode != 0, f"write to {key} fails")
            check(key.strip("'").lstrip("/") not in rows(sp),
                  f"rejected write to {key} left NO captured row")

        # UI visibility: TWO rows in the session dict's "attachments"
        # (wiki + ietf — a checkout is box content, not an attachment);
        # parents stay box-only (empty here).
        rep = m.sync_request(sock, type="ui", verb="session_dicts", args=[])
        sessions = (rep or {}).get("r", [])
        mine = next((s for s in sessions if s.get("box_id") == sid), {})
        atts = mine.get("attachments") or []
        check(len(atts) == 2,
              f"two rows in session attachments (got {atts!r})")
        names = [a.get("name") for a in atts]
        kinds = [a.get("kind") for a in atts]
        for want in ("wiki:wiki/Old Title@r", "ietf:draft-test-mesh@01"):
            check(any(n and n.startswith(want) for n in names),
                  f"attachment named {want}… (names {names!r})")
        check(kinds == ["wiki", "ietf"]
              and all(a.get("rev") for a in atts),
              f"attachment rows carry kind + pinned rev (got {atts!r})")
        check(mine.get("parents") == [],
              f"no phantom parents for attachments (got {mine.get('parents')!r})")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_mirror_attach_rs: all checks passed")


def test_mirror_attach_rs():
    main()


if __name__ == "__main__":
    main()
