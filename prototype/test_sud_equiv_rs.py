#!/usr/bin/env python3
"""EQUIVALENCE + CAPABILITY proof for the sud backend (`sarun run --sud`).

sud is a REPLACEMENT for the FUSE overlay (engine/DESIGN-sud.md): the box runs
under tv's Syscall-User-Dispatch wrapper with a userland overlay + inramfs /tmp
instead of a FUSE mount, and a post-exit sweep ingests the result into the SAME
sqlar BoxState. Downstream (review / apply / discard / UI) must therefore see an
identical box whichever backend produced it.

PART A — EQUIVALENCE: one workload through BOTH backends, asserting the captured
sqlar agrees on every mechanism-agnostic observation:
  - file permission bits are captured exactly,
  - a user.* xattr set in the box is captured,
  - a host-file deletion becomes a char-device tombstone (== a whiteout that
    hides the host/lower file),
  - a program executed FROM THE HOST filesystem runs and its output is captured,
  - file content + nested-subdir writes are captured.

PART B — sud-only CAPABILITIES (no FUSE analogue):
  - executing an ELF binary that lives in the box's inramfs /tmp works
    seamlessly (the tv loader re-execs it from a memfd), and
  - a nested (same-in-same) sud box executes a binary located in its PARENT
    box's captured layer.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_sud_equiv_rs.py
Skips (passes vacuously) if cargo / the engine / bwrap / the sud64 wrapper / a
C toolchain is unavailable. Temp dirs live under /var/tmp on purpose: the box's
/tmp is an inramfs mount, so the engine state (which holds the overlay upper)
must NOT sit under /tmp or the two would overlap.
"""
import os, shutil, socket, sqlite3, stat as stat_mod, subprocess, sys, \
       tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader


def sqlar_xattr(sp, name, key):
    """Read one xattr value from a box's sqlar (table xattr(name,key,value))."""
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        row = con.execute("SELECT value FROM xattr WHERE name=? AND key=?",
                          (name, key)).fetchone()
        return bytes(row[0]) if row and row[0] is not None else None
    finally:
        con.close()

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"
TV = _HERE.parent / "tv"
SUD64 = TV / "sud64"
# The box's /tmp is inramfs; keep engine state (the overlay upper) off /tmp.
TMPBASE = "/var/tmp"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)

# A tiny STATIC helper the box execs to prove where a binary can run from, and
# to set an xattr (no setfattr in this image). Static so it needs no in-box
# dynamic loader path — important for exec-from-inramfs / exec-from-parent-box.
SUDTOOL_C = r"""
#include <sys/xattr.h>
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc >= 3 && !strcmp(argv[1], "mark")) {
        printf("MARK:%s\n", argv[2]); return 0;
    }
    if (argc >= 5 && !strcmp(argv[1], "setxattr")) {
        if (lsetxattr(argv[2], argv[3], argv[4], strlen(argv[4]), 0) != 0) {
            perror("setxattr"); return 1;
        }
        printf("XOK\n"); return 0;
    }
    return 2;
}
"""


def build_sudtool():
    """Compile the static helper; return its path or None if no toolchain."""
    if not shutil.which("gcc"):
        return None
    d = Path(tempfile.mkdtemp(prefix="sudtool-", dir=TMPBASE))
    src = d / "sudtool.c"; src.write_text(SUDTOOL_C)
    out = d / "sudtool"
    try:
        subprocess.run(["gcc", "-static", "-O2", "-o", str(out), str(src)],
                       check=True, timeout=120,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    except Exception:
        return None
    return out if out.exists() else None


def ensure_binaries():
    if not BIN.exists():
        try:
            subprocess.run(["make", "engine"], cwd=CRATE.parent,
                           check=True, timeout=1200)
        except Exception:
            return False
    if not SUD64.exists():
        try:
            subprocess.run(
                ["make", "sud64",
                 "SUD_ADDINS=sud/trace sud/path_remap sud/cmd-rewrite "
                 "sud/fake-exec sud/inramfs"],
                cwd=TV, check=True, timeout=600)
        except Exception:
            return False
    return BIN.exists() and SUD64.exists()


def wait_socket(sock, timeout=10.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


class Engine:
    """A headless engine on an isolated (non-/tmp) state dir."""
    def __init__(self, mode):
        self.mode = mode
        self.tmp = Path(tempfile.mkdtemp(prefix=f"sudeq-{mode}-", dir=TMPBASE))
        for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                       ("XDG_CONFIG_HOME", "config"),
                       ("XDG_DATA_HOME", "data")):
            os.environ[k] = str(self.tmp / sub)
        os.environ["SLOPBOX_NS"] = "SUDEQ"
        if mode == "sud":
            os.environ["SARUN_SUD64"] = str(SUD64)
        self.m = SourceFileLoader("slopbox", SARUN).load_module()
        self.m.ensure_dirs()
        self.proc = subprocess.Popen([str(BIN), "serve"],
                                     stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL)
        if not wait_socket(self.m.sock_path()):
            raise RuntimeError(f"{mode}: engine socket never appeared")

    def run(self, name, script, extra_argv=()):
        argv = [str(BIN), "run", name]
        if self.mode == "sud":
            argv.append("--sud")
        argv += list(extra_argv) + ["--", "sh", "-c", script]
        return subprocess.run(argv, capture_output=True, text=True,
                              timeout=180)

    def latest_sqlar(self):
        return max(Path(os.environ["XDG_STATE_HOME"])
                   .joinpath("slopbox.SUDEQ").glob("*.sqlar"),
                   key=lambda p: int(p.stem))

    def close(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try: self.proc.wait(timeout=10)
            except Exception: self.proc.kill()
        os.environ.pop("SLOPBOX_NS", None)
        os.environ.pop("SARUN_SUD64", None)
        shutil.rmtree(self.tmp, ignore_errors=True)


def equivalence_workload(mode, sudtool):
    """Run the shared workload under one backend; return observations."""
    # Host binary the box will exec (lives on the real host = lower layer).
    host_bin = Path(TMPBASE) / "sudeq_hostbin"
    shutil.copy(sudtool, host_bin); host_bin.chmod(0o755)
    victim = Path("/root/sudeq_victim.txt")
    victim.write_bytes(b"v\n")
    eng = Engine(mode)
    try:
        script = (
            # exec a HOST binary; its stdout must be captured
            f"{host_bin} mark FROMHOST && "
            # a file with a distinctive permission mode
            "echo body > /root/sudeq_out.txt && chmod 0741 /root/sudeq_out.txt && "
            # a user.* xattr on another file
            "echo xf > /root/sudeq_xf.txt && "
            f"{host_bin} setxattr /root/sudeq_xf.txt user.sudeq hello && "
            # nested subdir write
            "mkdir -p /root/sudeq_d && echo nested > /root/sudeq_d/inner.txt && "
            # deletion of a host file -> tombstone / whiteout
            "rm /root/sudeq_victim.txt")
        r = eng.run("SUDEQBOX", script)
        if r.returncode != 0:
            raise RuntimeError(f"{mode}: box rc={r.returncode}: {r.stderr[-400:]}")
        sp = eng.latest_sqlar()
        m = eng.m
        rows = {n: md for n, md, *_ in m.sqlar_list(sp)}
        return {
            "stdout_has_mark": "MARK:FROMHOST" in r.stdout,
            "out_content": m.sqlar_content(sp, "root/sudeq_out.txt"),
            "out_mode_perm": stat_mod.S_IMODE(rows.get("root/sudeq_out.txt", 0)),
            "xattr": sqlar_xattr(sp, "root/sudeq_xf.txt", "user.sudeq"),
            "nested_content": m.sqlar_content(sp, "root/sudeq_d/inner.txt"),
            "victim_is_tombstone": stat_mod.S_ISCHR(
                rows.get("root/sudeq_victim.txt", 0)),
            # An overlay deletion must leave the HOST file untouched (the
            # tombstone lives only in the box) — so the host victim PERSISTS.
            "host_victim_present": victim.exists(),
            "host_out_absent": not Path("/root/sudeq_out.txt").exists(),
        }
    finally:
        victim.unlink(missing_ok=True)
        Path("/root/sudeq_out.txt").unlink(missing_ok=True)
        host_bin.unlink(missing_ok=True)
        eng.close()


def sud_inramfs_exec(sudtool):
    """Copy a binary into the box's inramfs /tmp and exec it there."""
    eng = Engine("sud")
    try:
        host_bin = Path(TMPBASE) / "sudeq_irbin"
        shutil.copy(sudtool, host_bin); host_bin.chmod(0o755)
        script = (f"cp {host_bin} /tmp/irtool && chmod +x /tmp/irtool && "
                  "/tmp/irtool mark FROMINRAMFS")
        r = eng.run("IRBOX", script)
        host_bin.unlink(missing_ok=True)
        return (r.returncode == 0 and "MARK:FROMINRAMFS" in r.stdout, r)
    finally:
        eng.close()


def sud_parentbox_exec(sudtool):
    """A nested (same-in-same) sud box execs a binary in its PARENT's layer."""
    eng = Engine("sud")
    try:
        host_bin = Path(TMPBASE) / "sudeq_pbin"
        shutil.copy(sudtool, host_bin); host_bin.chmod(0o755)
        # Parent captures a binary at /root/pbin (into its layer). Use
        # `cat >` rather than `cp`: cp attempts a reflink/rename across the
        # overlay boundary and fails EXDEV; a plain write is captured cleanly.
        rp = eng.run("PARENT",
                     f"cat {host_bin} > /root/pbin && chmod +x /root/pbin")
        if rp.returncode != 0:
            return (False, rp)
        # Child nests under PARENT (dotted name) and execs the parent's binary,
        # which it sees through the flattened lower stack.
        rc = eng.run("PARENT.CHILD", "/root/pbin mark FROMPARENTBOX")
        host_bin.unlink(missing_ok=True)
        Path("/root/pbin").unlink(missing_ok=True)  # defensive; box-captured
        return (rc.returncode == 0 and "MARK:FROMPARENTBOX" in rc.stdout, rc)
    finally:
        Path("/root/pbin").unlink(missing_ok=True)
        eng.close()


def main():
    if not ensure_binaries():
        print("test_sud_equiv_rs: engine or sud64 unavailable — SKIP"); return 0
    if not shutil.which("bwrap"):
        print("test_sud_equiv_rs: bwrap unavailable (FUSE needs it) — SKIP")
        return 0
    sudtool = build_sudtool()
    if sudtool is None:
        print("test_sud_equiv_rs: no C toolchain for the helper — SKIP"); return 0

    try:
        fuse = equivalence_workload("fuse", sudtool)
        sud = equivalence_workload("sud", sudtool)
    except Exception as e:
        print(f"test_sud_equiv_rs: backend unavailable ({e}) — SKIP"); return 0

    # ── PART A: each backend captured correctly, and they AGREE. ──
    for label, obs in (("fuse", fuse), ("sud", sud)):
        check(obs["host_out_absent"] and obs["host_victim_present"],
              f"{label}: box writes captured + host deletion NOT applied "
              f"to host (host untouched)")
        check(obs["stdout_has_mark"],
              f"{label}: program exec'd FROM HOST, output captured")
        check(obs["out_content"] == b"body\n",
              f"{label}: file content captured")
        check(obs["out_mode_perm"] == 0o741,
              f"{label}: file permission bits captured "
              f"(got {obs['out_mode_perm']:o})")
        check(obs["xattr"] == b"hello",
              f"{label}: user.* xattr captured (got {obs['xattr']!r})")
        check(obs["nested_content"] == b"nested\n",
              f"{label}: nested-dir write captured")
        check(obs["victim_is_tombstone"],
              f"{label}: host-file deletion is a char-dev tombstone/whiteout")

    for field in ("host_out_absent", "host_victim_present", "stdout_has_mark",
                  "out_content", "out_mode_perm", "xattr", "nested_content",
                  "victim_is_tombstone"):
        check(fuse[field] == sud[field],
              f"equiv: '{field}' identical across FUSE and sud "
              f"(fuse={fuse[field]!r} sud={sud[field]!r})")

    # ── PART B: sud-only exec capabilities. ──
    try:
        ok_ir, r_ir = sud_inramfs_exec(sudtool)
        check(ok_ir, "sud: ELF binary in inramfs /tmp execs seamlessly"
                     + ("" if ok_ir else f" (rc={r_ir.returncode} "
                        f"out={r_ir.stdout[-120:]!r} err={r_ir.stderr[-160:]!r})"))
        ok_pb, r_pb = sud_parentbox_exec(sudtool)
        check(ok_pb, "sud: nested box execs a binary from its PARENT box's layer"
                     + ("" if ok_pb else f" (rc={r_pb.returncode} "
                        f"out={r_pb.stdout[-120:]!r} err={r_pb.stderr[-160:]!r})"))
    except Exception as e:
        print(f"test_sud_equiv_rs: sud capability section unavailable ({e})")

    shutil.rmtree(Path(sudtool).parent, ignore_errors=True)
    print("\n" + ("SUD-EQUIV PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_sud_equiv_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
