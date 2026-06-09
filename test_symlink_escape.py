#!/usr/bin/env python3
"""Adversarial test: prove the DAEMON never follows a sandbox-planted symlink onto
its host target (O_NOFOLLOW / follow_symlinks=False on every host-path
open/stat/truncate/chmod/chown/utime).

Why drive the daemon directly: in production bwrap binds <mnt>/<sid> as `/`, so an
*absolute* symlink target resolves INSIDE the sandbox overlay, and the kernel
follows symlinks against the sandbox root — the host is unreachable to the child.
The residual risk is the *daemon itself* dereferencing a symlink while servicing
copy-up or setattr.

NOTE — no-mirror invariant: a box-created symlink no longer materializes an on-disk
up/<rel> artifact; its target lives ENTIRELY in the sqlar row and is served from
the in-RAM Index mirror. So copy-up/readlink/setattr on an overlay symlink never
touch a disk symlink at all (there is none): readlink returns the row target, and
setattr updates the row, not an artifact. This test stays valid as the guard for
the host-path surface: we still plant a symlink whose target is a host file and
invoke the daemon's write-side handlers on that path, asserting the host target is
untouched (the handlers must never dereference the link onto the host).

    /home/user/venv/bin/python test_symlink_escape.py
"""
import os, sys, asyncio, tempfile, shutil, stat as stat_mod
from pathlib import Path
from importlib.machinery import SourceFileLoader

m = SourceFileLoader("slopbox", "/home/user/sarun/sarun").load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


class Ctx:
    def __init__(self, pid): self.pid = pid; self.uid = os.getuid(); self.gid = os.getgid()
    umask = 0o022


class Fields:
    """Mimic pyfuse3 SetattrFields."""
    def __init__(self, **kw):
        for f in ("update_size","update_mode","update_uid","update_gid",
                  "update_mtime","update_atime"):
            setattr(self, f, kw.get(f, False))


def run():
    import pyfuse3
    tmp = Path(tempfile.mkdtemp(prefix="ovl-esc-"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")    # keep the single db local
    live = tmp / "live"; sid = "2"     # box_id is an integer identity (post-refactor)
    backing = live / sid; up = backing / "up"; up.mkdir(parents=True)
    target = tmp / "HOST_SECRET"; target.write_text("ORIGINAL\n")
    orig = target.read_text()

    index = m.Index(backing)
    Ops = m._build_overlay_ops()
    ops = Ops("/", on_event=lambda **e: None)
    ops.add_session(sid, up, index)

    # Plant a symlink via the symlink() handler exactly as a sandboxed program would:
    # name "evil" -> the host target (absolute). Under the no-mirror invariant this
    # records a sqlar ROW (target in the row), NOT an on-disk artifact — assert both:
    # the link is served from the mirror, and nothing was materialized in up/.
    sroot_ino = ops._ino_for(sid, "")
    loop = asyncio.new_event_loop()
    try:
        loop.run_until_complete(
            ops.symlink(sroot_ino, b"evil", os.fsencode(str(target)), Ctx(os.getpid())))
        check(index.kind_of("evil") == "symlink" and
              index.symlink_target("evil") == os.fsencode(str(target)),
              "planted symlink lives in the mirror row (no on-disk artifact)")
        check(not (up / "evil").is_symlink() and not (up / "evil").exists(),
              "symlink op materialized NO up/<rel> artifact")
        evil_ino = ops._ino_for(sid, "evil")

        # 1) copy-up via a WRITE open of the symlink path: must NOT write the host.
        try:
            fi = loop.run_until_complete(ops.open(evil_ino, os.O_RDWR, Ctx(os.getpid())))
            # if it returned a fh, write through it
            loop.run_until_complete(ops.write(fi.fh, 0, b"PWNED", Ctx(os.getpid())))
            loop.run_until_complete(ops.release(fi.fh))
        except pyfuse3.FUSEError as e:
            pass   # ELOOP/EACCES is the correct fail-closed outcome
        check(target.read_text() == orig,
              f"write-open of symlink path did NOT touch host target "
              f"({target.read_text()!r})")

        # 2) setattr truncate through the symlink inode.
        try:
            a = pyfuse3.EntryAttributes(); a.st_size = 0
            loop.run_until_complete(
                ops.setattr(evil_ino, a, Fields(update_size=True), None, Ctx(os.getpid())))
        except pyfuse3.FUSEError:
            pass
        check(target.read_text() == orig and target.stat().st_size == len(orig),
              "setattr truncate did NOT touch host target")

        # 3) setattr chmod through the symlink inode.
        before_mode = target.stat().st_mode
        try:
            a = pyfuse3.EntryAttributes(); a.st_mode = stat_mod.S_IFREG | 0o600
            loop.run_until_complete(
                ops.setattr(evil_ino, a, Fields(update_mode=True), None, Ctx(os.getpid())))
        except pyfuse3.FUSEError:
            pass
        check(target.stat().st_mode == before_mode,
              "setattr chmod did NOT change host target mode")

        # 4) setattr utime through the symlink inode.
        before_mtime = target.stat().st_mtime
        try:
            a = pyfuse3.EntryAttributes(); a.st_mtime_ns = 0; a.st_atime_ns = 0
            loop.run_until_complete(
                ops.setattr(evil_ino, a, Fields(update_mtime=True, update_atime=True),
                            None, Ctx(os.getpid())))
        except pyfuse3.FUSEError:
            pass
        check(abs(target.stat().st_mtime - before_mtime) < 1,
              "setattr utime did NOT change host target mtime")
    finally:
        loop.close()
        try: index.close()
        except Exception: pass
        final = target.read_text()
        check(final == orig, f"FINAL: host target intact ({final!r})")
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    try:
        run()
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    print("\n" + ("ESCAPE CLOSED" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
