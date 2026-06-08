#!/usr/bin/env python3
"""Regression: `chmod +x` on a READ-ONLY file in the overlay must succeed.

e2fsprogs builds a generated script `compile_et` via its `subst` tool, which
emits the file read-only (mode 0444), and the Makefile then runs
`chmod +x compile_et`. The overlay's setattr used to open the backing file
O_RDWR purely to call fchmod; on a 0444 file that open fails with EACCES, so
the box saw "chmod: changing permissions of 'compile_et': Permission denied".

A chmod of a read-only file is legal on any normal filesystem (fchmod needs
ownership, not write access to the contents). This drives the daemon's setattr
directly to prove the mode-only path no longer demands write access.

NOTE — uid independence: the real failure is a DAC denial that only an
UNPRIVILEGED process sees (root bypasses the write-permission check, and the
test suite often runs as root). The box always runs as the ordinary user, so to
reproduce faithfully regardless of who runs the test we wrap os.open to emulate
that unprivileged check: opening a file for write (O_RDWR/O_WRONLY) when it has
no owner-write bit raises EACCES — exactly what the kernel does for the box's
user. fchmod itself still works (the blob is owned by the caller).

    uv run --with pyfuse3 --with trio test_chmod_readonly.py
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


def install_unprivileged_open():
    """Make m.os.open enforce the DAC write check that an unprivileged process
    (the box's user) sees but root bypasses: opening for write a file with no
    owner-write bit raises EACCES."""
    real_open = m.os.open
    ACC = os.O_ACCMODE
    def fake_open(path, flags, *a, **k):
        if (flags & ACC) in (os.O_WRONLY, os.O_RDWR) and not (flags & os.O_CREAT):
            try:
                if not (m.os.stat(path).st_mode & 0o200):
                    raise PermissionError(13, os.strerror(13))  # EACCES
            except FileNotFoundError:
                pass
        return real_open(path, flags, *a, **k)
    m.os.open = fake_open
    return lambda: setattr(m.os, "open", real_open)


def run():
    import pyfuse3
    restore_open = install_unprivileged_open()
    tmp = Path(tempfile.mkdtemp(prefix="ovl-chmod-"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    live = tmp / "live"; sid = "2"
    backing = live / sid; up = backing / "up"; up.mkdir(parents=True)

    index = m.Index(backing)
    Ops = m._build_overlay_ops()
    ops = Ops("/", on_event=lambda **e: None)
    ops.add_session(sid, up, index)
    loop = asyncio.new_event_loop()

    def chmod(rel, mode):
        ino = ops._ino_for(sid, rel)
        a = pyfuse3.EntryAttributes(); a.st_mode = stat_mod.S_IFREG | mode
        return loop.run_until_complete(
            ops.setattr(ino, a, Fields(update_mode=True), None, Ctx(os.getpid())))

    def cur_mode(rel):
        ino = ops._ino_for(sid, rel)
        ea = loop.run_until_complete(ops.getattr(ino, Ctx(os.getpid())))
        return stat_mod.S_IMODE(ea.st_mode)

    try:
        root = ops._ino_for(sid, "")
        # Create the file like `subst` does: write contents, then close.
        fi, _ = loop.run_until_complete(
            ops.create(root, b"compile_et", stat_mod.S_IFREG | 0o644,
                       os.O_WRONLY | os.O_CREAT | os.O_TRUNC, Ctx(os.getpid())))
        loop.run_until_complete(ops.write(fi.fh, 0, b"#!/bin/sh\necho compile_et\n"))
        loop.run_until_complete(ops.release(fi.fh))

        # subst makes the generated file READ-ONLY (0444).
        chmod("compile_et", 0o444)
        check(cur_mode("compile_et") == 0o444, "file is read-only (0444) after subst")

        # Makefile: `chmod +x compile_et` -> 0555. THIS is what used to EACCES.
        ok = True
        try:
            chmod("compile_et", 0o555)
        except pyfuse3.FUSEError as e:
            ok = False
            check(False, f"chmod +x on read-only file raised FUSEError "
                         f"(errno={e.errno} {os.strerror(e.errno)})")
        if ok:
            check(cur_mode("compile_et") == 0o555,
                  "chmod +x on read-only file succeeded (mode now 0555)")
    finally:
        loop.close()
        restore_open()
        try: index.close()
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    try:
        run()
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    print("\n" + ("CHMOD OK" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
