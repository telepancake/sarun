#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "pyfuse3>=3.2",
#   "trio>=0.22",
#   "python-magic>=0.4",
#   "wcmatch>=8.4",
# ]
# ///
"""
overlay_bench — isolate the cost of sarun's FUSE overlay on a configure-style
workload.

It loads sarun's own OverlayMount/Index machinery (no UI, no proxy, no bwrap
from sarun itself), mounts the one multiplexed overlay, registers a session,
then runs a command under bwrap whose root is bound to that session's overlay
view — exactly as the real runner does. The "native" mode runs the same command
under the same bwrap isolation but binds the real host fs read-only instead, so
the ONLY difference between the two numbers is the FUSE overlay in the I/O path.

  overlay_bench.py overlay  --proj DIR --runs N
  overlay_bench.py native   --proj DIR --runs N

The workload is `./configure` in DIR (created by gen_project.sh). DIR must live
on a path that is NOT shadowed by bwrap's --proc/--dev/--tmpfs (so: not under
/tmp, /run, /proc, /dev). The host toolchain (/usr, /bin, /lib) is reached
through the overlay in overlay-mode, so every exec of sh/gcc/cc1/ld and every
header read crosses FUSE — which is the point.
"""
import importlib.machinery
import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def load_sarun():
    here = Path(__file__).resolve().parent
    # SARUN_PATH lets a caller point at a specific revision of the script (e.g. a
    # pre-change baseline checked out to a temp file) for before/after comparison.
    path = Path(os.environ.get("SARUN_PATH") or (here.parent / "sarun"))
    loader = importlib.machinery.SourceFileLoader("sarunmod", str(path))
    spec = importlib.util.spec_from_loader("sarunmod", loader)
    mod = importlib.util.module_from_spec(spec)
    # Register before exec: @dataclass introspection looks the module up in
    # sys.modules by __module__ name.
    sys.modules["sarunmod"] = mod
    loader.exec_module(mod)
    return mod


def make_bwrap(root_bind, chdir, cmd, overlay):
    """bwrap argv. overlay=True binds `root_bind` (the FUSE session root) as /;
    overlay=False ro-binds the real / and rw-binds only the build dir."""
    args = ["bwrap"]
    if overlay:
        args += ["--bind", root_bind, "/"]
    else:
        args += ["--ro-bind", "/", "/", "--bind", chdir, chdir]
    args += [
        "--proc", "/proc",
        "--dev", "/dev",
        "--tmpfs", "/tmp",
        "--tmpfs", "/run",
        "--unshare-pid", "--unshare-ipc", "--unshare-uts",
        "--die-with-parent",
        "--cap-drop", "ALL",
        "--chdir", chdir,
        "--", *cmd,
    ]
    return args


def time_run(args, label):
    # Wipe configure's caches/outputs so each run is from scratch.
    t0 = time.monotonic()
    p = subprocess.run(args, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    dt = time.monotonic() - t0
    if p.returncode != 0:
        sys.stderr.write(f"[{label}] FAILED rc={p.returncode}\n")
        sys.stderr.write(p.stderr.decode(errors="replace")[-2000:] + "\n")
    return dt, p.returncode


def reset_proj(proj):
    for n in ("config.log", "config.status", "config.h", "Makefile",
              "autom4te.cache"):
        t = Path(proj) / n
        if t.is_dir():
            shutil.rmtree(t, ignore_errors=True)
        else:
            t.unlink(missing_ok=True)


def setup_overlay(sarun):
    """Mount the real overlay under a throwaway XDG root, register one session.
    Returns (tmproot, mount, box_root, sid)."""
    tmproot = tempfile.mkdtemp(prefix="ovbench-")
    os.environ["XDG_STATE_HOME"] = os.path.join(tmproot, "state")
    os.environ["XDG_RUNTIME_DIR"] = os.path.join(tmproot, "run")
    os.makedirs(os.environ["XDG_STATE_HOME"], exist_ok=True)
    os.makedirs(os.environ["XDG_RUNTIME_DIR"], exist_ok=True)
    mp = sarun.mnt_point()
    mount = sarun.OverlayMount(mp, lower="/")
    if not mount.start():
        sys.stderr.write(f"mount failed: {mount._start_error}\n")
        sys.exit(1)
    print(f"overlay mounted at {mp}")
    sid = str(sarun.mint_box_id())
    backing = sarun.live_dir(sid)
    up = backing / "up"
    up.mkdir(parents=True, exist_ok=True)
    index = sarun.Index(backing)
    mount.add_session(sid, up, index)
    box_root = str(mp / sid)
    print(f"session {sid} -> {box_root}")
    return tmproot, mount, box_root, sid


def run_coherence(box_root):
    """Prove keep_cache=True does NOT serve stale data. Two mutation classes:

      (A) write THROUGH the overlay (a box copy-up) — the kernel invalidates its
          read cache as a side effect of the write-through, so this is coherent
          even without our help; included as a sanity floor.
      (B) change the underlying LOWER host file directly, behind the overlay's back
          — NO write crosses the overlay inode, so the kernel's page cache would
          stay stale under keep_cache=True until attrs are refreshed. This is the
          window our _autocache closes: every open re-lstats the lower file and
          invalidates the inode when (size, mtime) moved. THIS is the case that
          actually exercises the fix; a STALE here = a real regression.

    Each case seeds, caches via an overlay read (keep_cache=True), mutates, then
    re-reads through a fresh open and asserts the new bytes are seen."""
    host_dir = tempfile.mkdtemp(prefix="coh-host-", dir="/root")
    rel = host_dir.lstrip("/")
    ov_dir = os.path.join(box_root, rel)
    fails = 0

    def ov(name):
        return os.path.join(ov_dir, name)

    def host(name):
        return os.path.join(host_dir, name)

    def read_ov(name):
        with open(ov(name), "rb") as f:
            return f.read()

    def write_ov(name, data):                 # (A) through the overlay (copy-up)
        with open(ov(name), "wb") as f:
            f.write(data)

    def write_host(name, data):               # (B) behind the overlay's back
        with open(host(name), "wb") as f:
            f.write(data)

    def check(label, got, want):
        nonlocal fails
        ok = got == want
        if not ok:
            fails += 1
        print(f"  [{'ok' if ok else 'STALE'}] {label}: "
              f"got {got!r:.40} want {want!r:.40}")

    # ── (A) write-through overlay: same-size / grow / shrink ─────────────────
    write_host("same", b"AAAAAAAA")
    check("A seed read", read_ov("same"), b"AAAAAAAA")
    time.sleep(0.01); write_ov("same", b"BBBBBBBB")
    check("A same-size overwrite", read_ov("same"), b"BBBBBBBB")

    write_host("grow", b"short")
    check("A seed read", read_ov("grow"), b"short")
    time.sleep(0.01); write_ov("grow", b"a much longer body than before")
    check("A grow overwrite", read_ov("grow"), b"a much longer body than before")

    # ── (B) external lower change behind the overlay (the real keep_cache risk) ─
    # Same size: only mtime moves — a size-only check would miss this.
    write_host("ext_same", b"VERSION1")
    check("B seed read", read_ov("ext_same"), b"VERSION1")
    time.sleep(0.01); write_host("ext_same", b"VERSION2")
    check("B external same-size", read_ov("ext_same"), b"VERSION2")

    # Tight loop, all sub-attr-timeout (no sleeps): each external rewrite must be
    # seen on the very next open despite the 1.0s attr/entry cache window.
    for i in range(6):
        body = f"EXT-REV-{i:03d}".encode()
        write_host("ext_loop", body)
        check(f"B external loop #{i}", read_ov("ext_loop"), body)

    shutil.rmtree(host_dir, ignore_errors=True)
    if fails:
        print(f"COHERENCE: {fails} STALE read(s) — FAIL")
        sys.exit(1)
    print("COHERENCE: all reads fresh — PASS")


def run_exec(sarun, box_root, rel, exec_cmd):
    """Run an arbitrary command with its cwd inside the live overlay, so the tool
    drives the FUSE ops directly (no bwrap). `rel` is a path relative to the box
    root (the merged view of /); it is created as a copy-up dir. Used to point a
    third-party suite (e.g. pjdfstest's `prove`) at the overlay."""
    workdir = os.path.join(box_root, rel.lstrip("/"))
    os.makedirs(workdir, exist_ok=True)
    print(f"exec cwd={workdir}\n  $ {' '.join(exec_cmd)}")
    p = subprocess.run(exec_cmd, cwd=workdir)
    print(f"exec rc={p.returncode}")
    sys.exit(p.returncode)


def main():
    mode = sys.argv[1]
    proj = None
    runs = 3
    rel = "root/ovbench-work"
    exec_cmd = None
    a = sys.argv[2:]
    while a:
        if a[0] == "--proj":
            proj = a[1]; a = a[2:]
        elif a[0] == "--runs":
            runs = int(a[1]); a = a[2:]
        elif a[0] == "--rel":
            rel = a[1]; a = a[2:]
        elif a[0] == "--":
            exec_cmd = a[1:]; break
        else:
            a = a[1:]

    if mode == "exec":
        sarun = load_sarun()
        tmproot, mount, box_root, sid = setup_overlay(sarun)
        try:
            run_exec(sarun, box_root, rel, exec_cmd)
        finally:
            try: mount.stop()
            except Exception: pass
            shutil.rmtree(tmproot, ignore_errors=True)
        return

    assert proj, "need --proj DIR"
    proj = str(Path(proj).resolve())
    cmd = ["./configure"]

    if mode == "native":
        times = []
        for i in range(runs):
            reset_proj(proj)
            dt, rc = time_run(make_bwrap(None, proj, cmd, overlay=False),
                              f"native#{i}")
            print(f"native  run {i}: {dt:7.3f}s  rc={rc}")
            times.append(dt)
        best = min(times)
        print(f"native  best: {best:7.3f}s  (of {runs})")
        return

    # overlay / coherence modes: stand up the real FUSE overlay.
    sarun = load_sarun()
    tmproot, mount, box_root, sid = setup_overlay(sarun)
    try:
        if mode == "coherence":
            run_coherence(box_root)
            return
        times = []
        for i in range(runs):
            reset_proj(proj)
            dt, rc = time_run(make_bwrap(box_root, proj, cmd, overlay=True),
                              f"overlay#{i}")
            print(f"overlay run {i}: {dt:7.3f}s  rc={rc}")
            times.append(dt)
        best = min(times)
        print(f"overlay best: {best:7.3f}s  (of {runs})")
    finally:
        try:
            mount.stop()
        except Exception:
            pass
        shutil.rmtree(tmproot, ignore_errors=True)


if __name__ == "__main__":
    main()
