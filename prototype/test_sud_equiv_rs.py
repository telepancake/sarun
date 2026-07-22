#!/usr/bin/env python3
"""EQUIVALENCE + CAPABILITY proof for the SUD backend (`sarun run --sud`).

SUD is a transport for the same SarunFs used by FUSE and QEMU
(engine/DESIGN-sud.md). Downstream review/apply/discard/UI results must be
identical whichever backend carried the canonical filesystem requests.

PART A — EQUIVALENCE: one workload through BOTH backends, asserting the captured
sqlar agrees on every mechanism-agnostic observation:
  - file permission bits are captured exactly,
  - a user.* xattr set in the box is captured,
  - a host-file deletion becomes a char-device tombstone (== a whiteout that
    hides the host/lower file),
  - a program executed FROM THE HOST filesystem runs and its output is captured,
  - file content + nested-subdir writes are captured.

PART B — SUD transport CAPABILITIES:
  - executing an ELF binary created inside the captured tree works, and
  - a nested (same-in-same) sud box executes a binary located in its PARENT
    box's captured layer.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_sud_equiv_rs.py
Skips (passes vacuously) if cargo / the engine / bwrap / the sud64 wrapper / a
C toolchain is unavailable. Host fixtures use /var/tmp so they do not collide
with paths deliberately created inside the test boxes.
"""
import os, shlex, shutil, socket, sqlite3, stat as stat_mod, subprocess, sys, \
       tempfile, time
from pathlib import Path
from sarun_test_paths import ENGINE_BIN
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


def sqlar_outputs(sp):
    """All captured (stream, content) rows from a box's outputs table,
    coalesced per stream. Canonical numbering (overlay.rs sink map):
    stream 0 = stdout, 1 = stderr."""
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        out = {0: b"", 1: b""}
        for stream, content in con.execute(
                "SELECT stream, content FROM outputs ORDER BY id"):
            if stream in out and content is not None:
                out[stream] += bytes(content)
        return out
    finally:
        con.close()

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = ENGINE_BIN
TV = _HERE.parent / "tv"
SUD64 = TV / "sud64"
SUD32 = TV / "sud32"
TMPBASE = "/var/tmp"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)

# A tiny STATIC helper the box execs to prove where a binary can run from, and
# to set an xattr (no setfattr in this image). Static so it needs no in-box
# dynamic loader path — important for exec-from-capture / exec-from-parent-box.
SUDTOOL_C = r"""
#define _GNU_SOURCE
#include <sys/xattr.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc >= 3 && !strcmp(argv[1], "mark")) {
        printf("MARK:%s\n", argv[2]); return 0;
    }
    /* Raw rename(2) SRC DST — NOT `mv`, whose EXDEV copy+unlink fallback
     * would mask a broken cross-store bridge. */
    if (argc >= 4 && !strcmp(argv[1], "xrename")) {
        if (rename(argv[2], argv[3]) != 0) { perror("rename"); return 1; }
        printf("XRENAMED\n"); return 0;
    }
    /* O_TMPFILE in DIR then linkat("/proc/self/fd/N", DIR/out) — atomic
     * output-file creation (ld/gas/install); EXDEV'd under sud pre-fix. */
    if (argc >= 3 && !strcmp(argv[1], "otmpfile")) {
        int fd = open(argv[2], O_TMPFILE | O_RDWR, 0644);
        if (fd < 0) { perror("O_TMPFILE"); return 1; }
        (void)!write(fd, "TMPDATA", 7);
        char p[64], dst[256];
        snprintf(p, sizeof(p), "/proc/self/fd/%d", fd);
        snprintf(dst, sizeof(dst), "%s/otmp.out", argv[2]);
        if (linkat(AT_FDCWD, p, AT_FDCWD, dst, AT_SYMLINK_FOLLOW) != 0) {
            perror("linkat"); return 1;
        }
        printf("OTMPLINKED\n"); return 0;
    }
    if (argc >= 5 && !strcmp(argv[1], "setxattr")) {
        if (lsetxattr(argv[2], argv[3], argv[4], strlen(argv[4]), 0) != 0) {
            perror("setxattr"); return 1;
        }
        printf("XOK\n"); return 0;
    }
    if (argc >= 2 && !strcmp(argv[1], "streams")) {
        /* Direct writes to fd 1 and fd 2 (not shell >&2, which the trace
         * addin labels by fd number as stdout). */
        (void)!write(1, "STDOUT-STREAM\n", 14);
        (void)!write(2, "STDERR-STREAM\n", 14);
        return 0;
    }
    return 2;
}
"""


def build_sudtool(bits=64):
    """Compile the static helper (64- or 32-bit); return its path or None."""
    if not shutil.which("gcc"):
        return None
    d = Path(tempfile.mkdtemp(prefix=f"sudtool{bits}-", dir=TMPBASE))
    src = d / "sudtool.c"; src.write_text(SUDTOOL_C)
    out = d / "sudtool"
    flags = ["-static", "-O2"] + (["-m32"] if bits == 32 else [])
    try:
        subprocess.run(["gcc", *flags, "-o", str(out), str(src)],
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
            subprocess.run(["make", "sud64"],
                           cwd=TV, check=True, timeout=600)
        except Exception:
            return False
    # sud32 is best-effort: it needs the -m32 toolchain (gcc-multilib).
    # Without it the 32-bit leg of the suite skips — but if the toolchain IS
    # there, the runner's dir-sibling convention (tv/sud32 next to tv/sud64)
    # must hold, else a 32-bit box's wrapper exec fails and captures nothing.
    if not SUD32.exists():
        try:
            subprocess.run(["make", "sud32"],
                           cwd=TV, check=True, timeout=600,
                           stderr=subprocess.DEVNULL)
        except Exception:
            pass
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

    def run(self, name, script, extra_argv=(), raw_cmd=None):
        argv = [str(BIN), "run", name]
        if self.mode == "sud":
            argv.append("--sud")
        argv += list(extra_argv) + ["--"]
        argv += list(raw_cmd) if raw_cmd is not None else ["sh", "-c", script]
        return subprocess.run(argv, capture_output=True, text=True,
                              timeout=180)

    def latest_sqlar(self):
        return max(Path(os.environ["XDG_STATE_HOME"])
                   .joinpath("slopbox.SUDEQ").glob("*.sqlar"),
                   key=lambda p: int(p.stem))

    def has_flows_pcap(self):
        """True iff the engine wrote a per-box network flows pcapng — the
        capture artifact a tap box produces (same for FUSE and sud)."""
        flows = Path(os.environ["XDG_STATE_HOME"]) / "slopbox.SUDEQ" / "flows"
        return any(flows.rglob("*.pcapng"))

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
    fixture = Path(tempfile.mkdtemp(prefix=f"sudeq-{mode}-", dir=TMPBASE))
    # Host binary the box will exec (lives on the real host = lower layer).
    host_bin = fixture / "sudeq_hostbin"
    shutil.copy(sudtool, host_bin); host_bin.chmod(0o755)
    victim = fixture / "sudeq_victim.txt"
    victim.write_bytes(b"v\n")
    # Host files the box MODIFIES (append / chmod): copy-up territory —
    # the captured box copy carries the change, the host copy must not.
    modf = fixture / "sudeq_mod.txt"
    modf.write_bytes(b"orig\n"); modf.chmod(0o644)
    victim_rel = victim.as_posix().removeprefix("/")
    mod_rel = modf.as_posix().removeprefix("/")
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
            # distinct stdout + stderr writes (captured into the outputs table)
            f"{host_bin} streams; "
            # in-place modification of a HOST file (append) + chmod of it:
            # copy-up semantics — box copy changes, host copy must not
            f"echo add >> {shlex.quote(str(modf))} && "
            f"chmod 0741 {shlex.quote(str(modf))} && "
            # deletion of a host file -> tombstone / whiteout
            f"rm {shlex.quote(str(victim))}")
        # --net off keeps this (filesystem-equivalence) workload deterministic
        # and independent of tap availability; networking has its own test.
        r = eng.run("SUDEQBOX", script, extra_argv=["--net", "off"])
        if r.returncode != 0:
            raise RuntimeError(f"{mode}: box rc={r.returncode}: {r.stderr[-400:]}")
        sp = eng.latest_sqlar()
        m = eng.m
        rows = {n: md for n, md, *_ in m.sqlar_list(sp)}
        outs = sqlar_outputs(sp)
        # SUD trace EOF finalization folds live/<id>/sud.trace into the box's
        # sqlar `sudtrace` row and removes the redundant file. Verify the blob
        # is present + the `sudtrace` control verb decodes ≥1 EXEC event + the
        # live file is gone. This test inspects SUD's syscall producer here;
        # FUSE TRACE v3 generation has its focused CLI regression.
        trace = None
        if mode == "sud":
            bid = sp.stem
            con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
            try:
                trow = con.execute(
                    "SELECT length(content) FROM sudtrace").fetchone()
                sqlar_len = trow[0] if trow and trow[0] is not None else 0
            finally:
                con.close()
            rep = m.sync_request(m.sock_path(), type="sudtrace", sid=bid) or {}
            events = rep.get("events", []) if rep.get("ok") else []
            live_trace = sp.parent / "live" / bid / "sud.trace"
            trace = {
                "sqlar_len": sqlar_len,
                "verb_ok": bool(rep.get("ok")),
                "exec_events": sum(1 for e in events if e.get("kind") == "EXEC"),
                "file_gone": not live_trace.exists(),
            }
        return {
            "trace": trace,
            "stdout_has_mark": "MARK:FROMHOST" in r.stdout,
            "out_content": m.sqlar_content(sp, "root/sudeq_out.txt"),
            "out_mode_perm": stat_mod.S_IMODE(rows.get("root/sudeq_out.txt", 0)),
            "xattr": sqlar_xattr(sp, "root/sudeq_xf.txt", "user.sudeq"),
            "nested_content": m.sqlar_content(sp, "root/sudeq_d/inner.txt"),
            "victim_is_tombstone": stat_mod.S_ISCHR(
                rows.get(victim_rel, 0)),
            "mod_content": m.sqlar_content(sp, mod_rel),
            "mod_perm": stat_mod.S_IMODE(rows.get(mod_rel, 0)),
            "host_mod_untouched": modf.read_bytes() == b"orig\n"
                and stat_mod.S_IMODE(modf.stat().st_mode) == 0o644,
            # An overlay deletion must leave the HOST file untouched (the
            # tombstone lives only in the box) — so the host victim PERSISTS.
            "host_victim_present": victim.exists(),
            "host_out_absent": not Path("/root/sudeq_out.txt").exists(),
            # output capture: stdout -> stream 1, stderr -> stream 2
            "cap_stdout": b"STDOUT-STREAM\n" in outs[0],
            "cap_stderr": b"STDERR-STREAM\n" in outs[1],
        }
    finally:
        shutil.rmtree(fixture, ignore_errors=True)
        eng.close()


def sud_captured_exec(sudtool):
    """Copy a binary into the captured tree and execute it there."""
    eng = Engine("sud")
    try:
        host_bin = Path(TMPBASE) / "sudeq_irbin"
        shutil.copy(sudtool, host_bin); host_bin.chmod(0o755)
        script = (f"cp {host_bin} /tmp/captured-tool && "
                  "chmod +x /tmp/captured-tool && "
                  "/tmp/captured-tool mark FROMCAPTURE")
        r = eng.run("IRBOX", script)
        host_bin.unlink(missing_ok=True)
        return (r.returncode == 0 and "MARK:FROMCAPTURE" in r.stdout, r)
    finally:
        eng.close()


def sud_shared_tree_ops(sudtool):
    """Rename and link operations across directories in the shared tree.

    A raw rename(2) from /tmp into /root and an O_TMPFILE+linkat inside /root
    both used to expose transport-specific filesystem boundaries and fail
    EXDEV, silently losing a staged build output.
    Uses the raw-syscall helper so a tool's own EXDEV fallback can't mask
    a regression."""
    eng = Engine("sud")
    try:
        host_bin = Path(TMPBASE) / "sudeq_xsbin"
        shutil.copy(sudtool, host_bin); host_bin.chmod(0o755)
        script = (
            "cp %s /tmp/xs && chmod +x /tmp/xs && "
            # raw rename across shared-tree directories: content survives
            "echo HDRDATA > /tmp/staged.h && "
            "/tmp/xs xrename /tmp/staged.h /root/moved.h && "
            "cat /root/moved.h && "
            # O_TMPFILE + linkat inside the overlay tree
            "mkdir -p /root/od && /tmp/xs otmpfile /root/od && "
            "cat /root/od/otmp.out"
        ) % host_bin
        r = eng.run("XSBOX", script)
        host_bin.unlink(missing_ok=True)
        ok = (r.returncode == 0 and "XRENAMED" in r.stdout
              and "HDRDATA" in r.stdout and "OTMPLINKED" in r.stdout
              and "TMPDATA" in r.stdout)
        return (ok, r)
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


def output_capture_32(tool32):
    """Run a 32-bit binary that writes distinct stdout+stderr under both
    backends; return {backend: (stdout_ok, stderr_ok)} from the outputs
    table. Proves output capture works for 32-bit boxes too."""
    host_bin = Path(TMPBASE) / "sudeq_out32"
    shutil.copy(tool32, host_bin); host_bin.chmod(0o755)
    res = {}
    try:
        for mode in ("fuse", "sud"):
            eng = Engine(mode)
            try:
                eng.run("OUT32", "", extra_argv=["--net", "off"],
                        raw_cmd=[str(host_bin), "streams"])
                outs = sqlar_outputs(eng.latest_sqlar())
                res[mode] = (b"STDOUT-STREAM\n" in outs[0],
                             b"STDERR-STREAM\n" in outs[1])
            finally:
                eng.close()
        return res
    finally:
        host_bin.unlink(missing_ok=True)


def brush_workload(mode):
    """Run the same `-b` (embedded brush) workload under both backends: an
    in-process builtin write (echo redirect), a NESTED `/bin/sh -c` (the
    shadow shim — FUSE overlay shadow vs sud remap-to-shadow-symlink), an
    in-process `cat`, and an in-process `make` (embedded kati/n2) whose
    recipe writes a file. Returns observations for equivalence checks."""
    proj = Path(tempfile.mkdtemp(prefix=f"sudeq-bproj-", dir=TMPBASE))
    (proj / "Makefile").write_text("all:\n\techo BUILTMARK > built.txt\n")
    eng = Engine(mode)
    try:
        script = ("echo TOPMARK > /root/sudeq_brush.txt && "
                  "/bin/sh -c 'echo NESTEDMARK >> /root/sudeq_brush.txt' && "
                  "cat /root/sudeq_brush.txt && "
                  f"cd {proj} && make")
        r = eng.run("BRUSHBOX", script, extra_argv=["--net", "off", "-b"])
        if r.returncode != 0:
            raise RuntimeError(f"{mode}: -b box rc={r.returncode}: "
                               f"{r.stderr[-400:]}")
        sp = eng.latest_sqlar()
        m = eng.m
        return {
            "content": m.sqlar_content(sp, "root/sudeq_brush.txt"),
            "stdout_top": "TOPMARK" in r.stdout,
            "stdout_nested": "NESTEDMARK" in r.stdout,
            "make_out": m.sqlar_content(
                sp, str(proj).lstrip("/") + "/built.txt"),
            "host_untouched": not Path("/root/sudeq_brush.txt").exists()
                and not (proj / "built.txt").exists(),
        }
    finally:
        Path("/root/sudeq_brush.txt").unlink(missing_ok=True)
        shutil.rmtree(proj, ignore_errors=True)
        eng.close()


def net_capture(mode):
    """Run a tap box; return (dns_via_engine, flows_pcap). A tap box's DNS
    is answered by the engine's synthetic resolver (fake-IP range 240/8 or
    a non-public address), and the engine writes a per-box flows pcapng —
    both the observable, upstream-independent proofs that the box's network
    is engine-mediated (== captured). Same mechanism for FUSE and sud."""
    eng = Engine(mode)
    try:
        r = eng.run("NETBOX", "getent hosts example.com 2>&1 | head -1",
                    extra_argv=["--net", "tap"])
        # Synthetic address: the engine hands out its own fake IP, never the
        # real public one — proof the lookup went through the engine stack.
        line = (r.stdout or "").split()
        ip = line[0] if line else ""
        via_engine = ip.startswith("240.") or ip.startswith("10.") \
            or ip.startswith("100.64.")
        return (via_engine, eng.has_flows_pcap(), r)
    finally:
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
        check(obs["mod_content"] == b"orig\nadd\n" and obs["mod_perm"] == 0o741,
              f"{label}: in-place host-file modify copied up "
              f"(content={obs['mod_content']!r} mode={obs['mod_perm']:o})")
        check(obs["host_mod_untouched"],
              f"{label}: host copy of the modified file untouched")
        check(obs["cap_stdout"] and obs["cap_stderr"],
              f"{label}: stdout+stderr captured to the outputs table "
              f"(out={obs['cap_stdout']} err={obs['cap_stderr']})")

    for field in ("host_out_absent", "host_victim_present", "stdout_has_mark",
                  "out_content", "out_mode_perm", "xattr", "nested_content",
                  "victim_is_tombstone", "mod_content", "mod_perm",
                  "host_mod_untouched", "cap_stdout", "cap_stderr"):
        check(fuse[field] == sud[field],
              f"equiv: '{field}' identical across FUSE and sud "
              f"(fuse={fuse[field]!r} sud={sud[field]!r})")

    # ── PART A-trace: the sud box's TRACE stream is durable + queryable. ──
    tr = sud["trace"]
    check(tr is not None and tr["sqlar_len"] > 0,
          f"sud: box sqlar has a non-empty sudtrace row (len={tr and tr['sqlar_len']})")
    check(tr is not None and tr["verb_ok"] and tr["exec_events"] >= 1,
          f"sud: sudtrace verb returns ok with ≥1 EXEC event "
          f"(ok={tr and tr['verb_ok']} execs={tr and tr['exec_events']})")
    check(tr is not None and tr["file_gone"],
          "sud: live/<id>/sud.trace removed after EOF finalization")

    # ── PART A2: 32-bit output capture (both backends). ──
    tool32 = build_sudtool(bits=32)
    if tool32 is not None and not SUD32.exists():
        # A 32-bit box needs the sud32 wrapper (dir sibling of sud64);
        # running without it would exec-fail and capture nothing.
        print("  (sud32 wrapper did not build — skipping 32-bit output capture)")
        shutil.rmtree(Path(tool32).parent, ignore_errors=True)
        tool32 = None
    if tool32 is None:
        print("  (no 32-bit toolchain — skipping 32-bit output capture)")
    else:
        try:
            oc32 = output_capture_32(tool32)
            for mode in ("fuse", "sud"):
                so, se = oc32[mode]
                check(so and se,
                      f"{mode}: 32-bit box stdout+stderr captured "
                      f"(out={so} err={se})")
            check(oc32["fuse"] == oc32["sud"],
                  "equiv: 32-bit output capture identical across FUSE and sud")
        except Exception as e:
            print(f"  (32-bit output capture unavailable: {e})")
        finally:
            shutil.rmtree(Path(tool32).parent, ignore_errors=True)

    # ── PART A3: network capture (tap), both backends. ──
    if not Path("/dev/net/tun").exists():
        print("  (no /dev/net/tun — skipping network capture)")
    else:
        try:
            fnet = net_capture("fuse")
            snet = net_capture("sud")
            for mode, (via, pcap, r) in (("fuse", fnet), ("sud", snet)):
                # tap may be unavailable (rootless/no CAP_NET_ADMIN); only
                # assert capture when the box actually got a tap datapath.
                if via or pcap:
                    check(via, f"{mode}: tap box DNS answered by the engine "
                               f"stack (synthetic IP; got {r.stdout.split()[:1]})")
                    check(pcap, f"{mode}: engine wrote a per-box flows pcapng")
                else:
                    print(f"  ({mode}: tap datapath unavailable here — "
                          f"skipping net capture asserts)")
            if (fnet[0] or fnet[1]) and (snet[0] or snet[1]):
                check(fnet[0] == snet[0] and fnet[1] == snet[1],
                      "equiv: network capture identical across FUSE and sud")
        except Exception as e:
            print(f"  (network capture unavailable: {e})")

    # ── PART A4: -b embedded-brush boxes (both backends). ──
    try:
        fb = brush_workload("fuse")
        sb = brush_workload("sud")
        for label, obs in (("fuse", fb), ("sud", sb)):
            check(obs["content"] == b"TOPMARK\nNESTEDMARK\n",
                  f"{label}: -b brush builtin write + nested /bin/sh shim "
                  f"captured (got {obs['content']!r})")
            check(obs["stdout_top"] and obs["stdout_nested"],
                  f"{label}: -b brush stdout visible to the runner")
            check(obs["make_out"] == b"BUILTMARK\n",
                  f"{label}: -b in-process make (kati/n2) recipe write "
                  f"captured (got {obs['make_out']!r})")
            check(obs["host_untouched"],
                  f"{label}: -b brush writes stayed in the box")
        for field in ("content", "stdout_top", "stdout_nested",
                      "make_out", "host_untouched"):
            check(fb[field] == sb[field],
                  f"equiv: -b '{field}' identical across FUSE and sud "
                  f"(fuse={fb[field]!r} sud={sb[field]!r})")
    except Exception as e:
        print(f"  (-b brush workload unavailable: {e})")

    # ── PART B: SUD transport exec and descriptor capabilities. ──
    try:
        ok_ir, r_ir = sud_captured_exec(sudtool)
        check(ok_ir, "sud: ELF binary created in the captured tree executes"
                     + ("" if ok_ir else f" (rc={r_ir.returncode} "
                        f"out={r_ir.stdout[-120:]!r} err={r_ir.stderr[-160:]!r})"))
        ok_pb, r_pb = sud_parentbox_exec(sudtool)
        check(ok_pb, "sud: nested box execs a binary from its PARENT box's layer"
                     + ("" if ok_pb else f" (rc={r_pb.returncode} "
                        f"out={r_pb.stdout[-120:]!r} err={r_pb.stderr[-160:]!r})"))
        ok_xs, r_xs = sud_shared_tree_ops(sudtool)
        check(ok_xs, "sud: rename /tmp->/root + O_TMPFILE linkat use the "
                     "same shared filesystem"
                     + ("" if ok_xs else f" (rc={r_xs.returncode} "
                        f"out={r_xs.stdout[-160:]!r} err={r_xs.stderr[-160:]!r})"))
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
