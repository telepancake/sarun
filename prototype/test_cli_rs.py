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
import os, shutil, signal, socket, sqlite3, stat as stat_mod, subprocess, sys, tempfile, time
from pathlib import Path
from sarun_test_paths import ENGINE_BIN
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = ENGINE_BIN

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
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
        raise SystemExit("test_cli_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="clirs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    host_tmp = None
    old_sigterm = signal.getsignal(signal.SIGTERM)

    def terminate(signum, _frame):
        # Turn a normal harness termination into stack unwinding so the
        # temporary host fixture and engine process are cleaned in `finally`.
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, terminate)
    try:
        # FUSE-backed boxes deliberately replace /tmp with a private tmpfs, so
        # fixtures which must remain visible inside the box belong in /var/tmp.
        host_tmp = Path(tempfile.mkdtemp(prefix="clirs-host-", dir="/var/tmp"))
        aphost = host_tmp / "cli_apply_proof.txt"
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

        # The ordinary shell contract does not require -C: a relative command
        # is resolved from the caller's inherited cwd after the top-level FUSE
        # runner joins the broker's user+mount namespaces.  This specifically
        # guards the namespace transition from making getcwd() fail and the
        # runner silently substituting `/`.
        # Keep this outside the XDG fixture root: Sarun deliberately hides its
        # own state/config/runtime trees from boxes.
        relative = host_tmp / "relative-cwd"
        relative.mkdir()
        (relative / "values.mk").write_text("VALUE := from-provider\n")
        (relative / "Makefile").write_text(
            "include values.mk\n"
            "all:\n"
            "\t@printf 'direct-kati=<%s>\\n' '$(VALUE)'\n"
        )
        script = relative / "probe.sh"
        script.write_text(
            "#!/bin/sh\n"
            "make -s -f Makefile\n"
            "printf 'relative-script-ok argv1=<%s> argv2=<%s> cwd=<%s>\\n' "
            '"$1" "$2" "$PWD"\n'
        )
        script.chmod(0o755)
        r = subprocess.run(
            [str(BIN), "run", "--fuse", "-b", "--", "./probe.sh",
             "first argument", "second"],
            cwd=relative, capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              "cli-rs: inherited cwd resolves executable relative script "
              f"(got {r.returncode}: {r.stderr[-300:]})")
        check(
            "relative-script-ok argv1=<first argument> argv2=<second> "
            f"cwd=<{relative}>" in r.stdout,
            "cli-rs: relative script preserves cwd and argv "
            f"(stdout={r.stdout.strip()!r})")
        check("direct-kati=<from-provider>" in r.stdout,
              "cli-rs: embedded Kati reads Makefile and include through "
              "the FUSE box provider")

        # FUSE emits the same compact TRACE v3 stream as SUD. The first run
        # above owns box 1 in this isolated namespace; this relative run is 2.
        trace_box = "2"
        trace_db = m.sqlar_path(trace_box)
        with sqlite3.connect(f"file:{trace_db}?mode=ro", uri=True) as con:
            row = con.execute("SELECT length(content) FROM sudtrace").fetchone()
            access = con.execute(
                "SELECT process_id,path,flags FROM file_access "
                "WHERE path LIKE '%/probe.sh' OR path='probe.sh' "
                "OR path LIKE '%/Makefile' OR path='Makefile' "
                "OR path LIKE '%/values.mk' OR path='values.mk'"
            ).fetchall()
        trace_len = row[0] if row and row[0] is not None else 0
        trace_reply = m.sync_request(m.sock_path(), type="sudtrace", sid=trace_box) or {}
        trace_events = trace_reply.get("events", []) if trace_reply.get("ok") else []
        check(trace_len > 0 and trace_reply.get("ok"),
              f"cli-rs: FUSE run stores a decodable TRACE v3 stream "
              f"(bytes={trace_len}, reply={trace_reply.get('error')!r})")
        check(any(event.get("kind") == "OPEN"
                  and event.get("text", "").endswith("/probe.sh")
                  for event in trace_events),
              "cli-rs: FUSE TRACE attributes the relative script open")
        check(any(process_id > 0 and flags & 3 == 0
                  for process_id, _path, flags in access),
              f"cli-rs: FUSE read-open is indexed by process and path "
              f"(rows={access!r})")
        check(all(any(process_id > 0 and path.endswith(name) and flags & 3 == 0
                      for process_id, path, flags in access)
                  for name in ("/Makefile", "/values.mk")),
              "cli-rs: embedded Kati reads are attributed to the normal "
              f"FUSE trace (rows={access!r})")
        check(not (m.live_home() / trace_box / "sud.trace").exists(),
              "cli-rs: completed FUSE run removes its live TRACE spool")

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
        build_box(
            m, "9701", "CLIAPPLY", aphost.as_posix().removeprefix("/"),
            b"applied-cli\n")
        ra = subprocess.run([str(BIN), "CLIAPPLY", "apply"],
                            capture_output=True, text=True)
        check(ra.returncode == 0, "cli-rs: NAME apply exits 0")
        check(aphost.exists() and aphost.read_bytes() == b"applied-cli\n",
              "cli-rs: NAME apply wrote the change to the real host")
        check(not m.sqlar_path("9701").exists(),
              "cli-rs: NAME apply reaped the emptied box")

        # NAME discard → drops the change, host untouched, box reaped
        dishost = host_tmp / "cli_discard_proof.txt"
        build_box(
            m, "9702", "CLIDISCARD", dishost.as_posix().removeprefix("/"),
            b"nope\n")
        rd = subprocess.run([str(BIN), "CLIDISCARD", "discard"],
                            capture_output=True, text=True)
        check(rd.returncode == 0, "cli-rs: NAME discard exits 0")
        check(not dishost.exists(),
              "cli-rs: NAME discard did NOT touch the host")
        check(not m.sqlar_path("9702").exists(),
              "cli-rs: NAME discard reaped the box")
    finally:
        if host_tmp is not None:
            shutil.rmtree(host_tmp, ignore_errors=True)
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        signal.signal(signal.SIGTERM, old_sigterm)
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("CLI-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_cli_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
