#!/usr/bin/env python3
"""Structural-diff conformance for the RUST engine (engine/src/review.rs +
control.rs): the struct_quick / struct_finish / struct_cancel verbs the Python
ChangeReview exposes, implemented in Rust, answered over the control socket so
a Python RemoteReview consuming structural_diff_quick / structural_diff_finish
works unmodified. A box is created via box_new, a small ELF (a copy of
/bin/true) is written into it, and the type line + sandboxed readelf dump are
asserted. Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_struct_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import os, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = str(Path(__file__).resolve().parent / "sarun")
CRATE = Path(__file__).resolve().parent.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

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


def main():
    if not ensure_binary():
        raise SystemExit("test_struct_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="structrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "STRUCT"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))

        # create a box; write a small ELF (copy of /bin/true) into it.
        rep = m.sync_request(sock, type="ui", verb="box_new", args=[])
        check(rep and rep.get("ok"), "struct-rs: box_new answers")
        bsid = rep["r"]["sid"]; root = Path(rep["r"]["root"])
        check(root.is_dir(), "struct-rs: box root appears under the mount")

        elf = Path("/bin/true").read_bytes()
        check(elf[:4] == b"\x7fELF", "struct-rs: /bin/true is an ELF (test premise)")
        (root / "root").mkdir(exist_ok=True)
        (root / "root/m_struct.bin").write_bytes(elf)
        rel = "root/m_struct.bin"

        # quick verb: type line + header + a job id (created file, no base).
        q = m.sync_request(sock, type="ui", verb="struct_quick",
                           args=[bsid, rel])["r"]
        lines = q["lines"]; job = q["job"]
        texts = [t for _, t in lines]
        check(any("ELF" in t for t in texts),
              "struct-rs: struct_quick type line mentions ELF")
        check(any("readelf" in t for t in texts),
              "struct-rs: struct_quick header names the readelf differ")
        check(job is not None,
              "struct-rs: struct_quick returns a job id for the heavy dump")

        # finish verb: runs the sandboxed readelf dump, returns the full lines.
        f = m.sync_request(sock, type="ui", verb="struct_finish", args=[job])["r"]
        flines = f["lines"]; ftexts = [t for _, t in flines]
        check(len(flines) >= len(lines),
              "struct-rs: struct_finish includes the quick head plus the dump")
        # created file (current only) -> the readelf dump is emitted as context.
        body = "\n".join(ftexts)
        check("ELF Header" in body or "Section Headers" in body
              or "Program Headers" in body or "<parser error" not in body,
              "struct-rs: readelf dump produced (or at least no parser error)")
        check("ELF Header" in body or "Section Headers" in body,
              "struct-rs: sandboxed readelf actually dumped the ELF structure")

        # the RemoteReview Python facade consumes the same verbs.
        rsup = m.RemoteSupervisor(sock)
        rlines, rjob = rsup.review.structural_diff_quick(bsid, rel)
        check(isinstance(rlines, list) and rlines and isinstance(rlines[0], tuple),
              "struct-rs: RemoteReview.structural_diff_quick returns (lines, job)")
        check(any("ELF" in t for _, t in rlines),
              "struct-rs: RemoteReview quick lines mention ELF")
        res = rsup.review.structural_diff_finish(rjob)
        check(isinstance(res, dict) and "lines" in res,
              "struct-rs: RemoteReview.structural_diff_finish returns dict(lines=...)")
        check(any("ELF Header" in t or "Section Headers" in t
                  for _, t in res["lines"]),
              "struct-rs: RemoteReview finish carries the readelf dump")

        # struct_cancel is a benign no-op ok.
        rsup.review.struct_cancel(99999)
        check(True, "struct-rs: struct_cancel on an unknown job is a no-op")

        # a non-recognized (plain text) file: quick returns no job.
        (root / "root/m_plain.txt").write_bytes(b"just text\n")
        q2 = m.sync_request(sock, type="ui", verb="struct_quick",
                            args=[bsid, "root/m_plain.txt"])["r"]
        check(q2["job"] is None,
              "struct-rs: unrecognized type yields no heavy job")
        check(any(t.startswith("type") for _, t in
                  [tuple(x) for x in q2["lines"]]),
              "struct-rs: unrecognized type still reports a type line")

        eng.terminate()
        try: eng.wait(timeout=10)
        except subprocess.TimeoutExpired:
            eng.kill(); eng.wait(timeout=5)
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        if eng is not None and eng.poll() is None:
            eng.kill()
            try: eng.wait(timeout=5)
            except Exception: pass
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("STRUCT-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_struct_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
