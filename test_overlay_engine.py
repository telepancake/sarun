#!/usr/bin/env python3
"""Engine-level tests for the multiplexed pyfuse3 overlay.

Run with the venv python (has pyfuse3+trio):
    /home/user/venv/bin/python test_overlay_engine.py

Self-safety: every mount is at an isolated temp point, exercised via
timeout-wrapped child commands, and lazy-unmounted in a finally block, leaving a
clean mount table.
"""
import os, sys, subprocess, tempfile, shutil, time, json, sqlite3
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/sarun"
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


class MountFixture:
    """Mount one session overlay at a temp point, run shell commands inside it via
    a timeout-wrapped child (operating through the FUSE path), then tear down."""
    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ovl-test-"))
        # The single db lives under state_home; keep it inside our temp tree so the
        # real ~/.local/state is untouched.
        os.environ["XDG_STATE_HOME"] = str(self.tmp / "state")
        self.mnt = self.tmp / "mnt"
        self.live = self.tmp / "live"
        self.sid = "20260604-000000_1"
        self.backing = self.live / self.sid
        self.up = self.backing / "up"
        self.up.mkdir(parents=True)
        self.mount = None
        self.index = None

    def start(self, lower=None, passthrough=False):
        self.index = m.Index(self.backing)
        self.mount = m.OverlayMount(self.mnt, lower=lower or "/")
        ok = self.mount.start()
        if not ok:
            raise RuntimeError(f"mount failed: {self.mount._start_error}")
        self.mount.add_session(self.sid, self.up, self.index,
                               passthrough=passthrough)
        self.root = self.mnt / self.sid

    def sh(self, script, timeout=15):
        """Run a bash script with cwd = the session overlay root."""
        return subprocess.run(["timeout", str(timeout), "bash", "-c", script],
                              cwd=str(self.root), capture_output=True, text=True)

    def stop(self):
        try:
            if self.mount: self.mount.stop()
        finally:
            try:
                if os.path.ismount(str(self.mnt)):
                    subprocess.run(["fusermount3", "-uz", str(self.mnt)],
                                   stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, timeout=10)
            except Exception: pass
            try:
                if self.index: self.index.close()
            except Exception: pass
            shutil.rmtree(self.tmp, ignore_errors=True)


def test_readthrough_and_create():
    fx = MountFixture()
    try:
        fx.start()
        # read-through to the host /etc/hostname
        r = fx.sh("cat /etc/hostname")
        host = Path("/etc/hostname").read_text() if Path("/etc/hostname").exists() else ""
        check(r.returncode == 0 and r.stdout == host,
              "read-through: cat /etc/hostname matches host")
        # create a new file
        r = fx.sh("mkdir -p sub && echo hello > sub/new.txt && cat sub/new.txt")
        check(r.returncode == 0 and r.stdout == "hello\n", "create file in new subdir")
        # file bytes are now in the pool, not up/<rel>
        blob = m.blob_path(fx.index.box_id, fx.index.row_id("sub/new.txt"))
        check(blob.read_bytes() == b"hello\n",
              "created file bytes landed in pool blob")
        check(fx.index.kind_of("sub/new.txt") == "file", "index: created file = 'file'")
        check(fx.index.kind_of("sub") == "dir", "index: parent dir tracked")
    finally:
        fx.stop()


def test_copyup_modify():
    fx = MountFixture()
    try:
        fx.start()
        # modify a host file -> copy-up, host untouched
        r = fx.sh("cp /etc/hostname hn && echo APPENDED >> hn && cat hn")
        check(r.returncode == 0, "copy host file then append")
        # the modify is on the *copy* hn (a created file); now modify-in-place a
        # read-through path by copying-up: append to a path that mirrors lower.
        os.makedirs(fx.up.parent, exist_ok=True)
        # write directly through overlay to a lower-backed path
        target = "etc_copy_test_marker"
        r2 = fx.sh(f"printf 'x' > {target}")
        check(r2.returncode == 0, "create marker via overlay")
        # file bytes are in the pool blob, not up/<rel>
        rid = fx.index.row_id(target)
        check(rid is not None and m.blob_path(fx.index.box_id, rid).exists(),
              "marker blob in pool")
    finally:
        fx.stop()


def test_otrunc_rewrite_shorter():
    # Regression: O_TRUNC on the overlay (copy-up) open path must truncate. With FUSE
    # atomic_o_trunc the kernel passes O_TRUNC to open() and sends no separate
    # setattr(size=0), so a `>`-rewrite with SHORTER content must not leave the old
    # tail behind. This is the config.status `subs-N.sed` busyloop bug.
    fx = MountFixture()
    try:
        fx.start()
        # (a) an upper (created) file rewritten shorter: exact new bytes, no stale tail
        r = fx.sh("printf 'LOOOOOOOOONG original content\\n' > a; printf 'short\\n' > a; cat a")
        check(r.stdout == "short\n",
              "upper O_TRUNC rewrite-shorter truncates (no stale tail)")
        r = fx.sh("wc -c < a")
        check(r.stdout.strip() == "6", "upper O_TRUNC rewrite leaves exactly the new length")
        # (b) a lower-backed file rewritten shorter (copy-up + O_TRUNC together)
        r = fx.sh("printf 'tiny\\n' > etc/hostname; "
                  "printf 'len=%s' \"$(wc -c < etc/hostname)\"")
        check(r.stdout == "len=5", "lower copy-up + O_TRUNC truncates to new length")
        # (c) truncate to empty
        r = fx.sh("printf 'data\\n' > e; : > e; wc -c < e")
        check(r.stdout.strip() == "0", "O_TRUNC to empty yields a 0-byte file")
    finally:
        fx.stop()


def test_delete_whiteout():
    fx = MountFixture()
    try:
        fx.start()
        # create then delete -> upper-only, no whiteout needed
        fx.sh("echo a > f1 && rm f1")
        check(fx.index.kind_of("f1") is None, "create+delete upper-only leaves no entry")
        # delete a host file THROUGH the overlay (relative path = overlay path; an
        # absolute path would hit the host directly and is NOT what we test).
        r = fx.sh("rm etc/hostname; echo rc=$?")
        check("rc=0" in r.stdout, "rm of overlay etc/hostname succeeds")
        check("whiteout" == fx.index.kind_of("etc/hostname"),
              "deleting host /etc/hostname (via overlay) records a whiteout")
        # and it's masked in the merged view
        r2 = fx.sh("cat etc/hostname 2>&1; echo rc=$?")
        check("No such file" in r2.stdout or "rc=1" in r2.stdout,
              "whiteout masks the lower file")
        # host file itself is untouched
        check(Path("/etc/hostname").exists(), "host /etc/hostname still present")
    finally:
        fx.stop()


def test_symlink_and_readlink():
    fx = MountFixture()
    try:
        fx.start()
        r = fx.sh("ln -s /target/path mylink && readlink mylink")
        check(r.returncode == 0 and r.stdout.strip() == "/target/path",
              "symlink create + readlink")
        check(fx.index.kind_of("mylink") == "symlink", "index: symlink kind")
    finally:
        fx.stop()


def test_provenance_recorded():
    fx = MountFixture()
    try:
        fx.start()
        fx.sh("echo data > provtest.txt")
        prov = fx.index.writer_provenance("provtest.txt")
        check(prov is not None, "provenance recorded for a write")
        if prov:
            check(prov.get("pid", 0) > 0, "provenance has a pid")
            check(isinstance(prov.get("argv"), list), "provenance has argv list")
    finally:
        fx.stop()


def test_opaque_dir():
    fx = MountFixture()
    try:
        fx.start()
        # rm -rf a lower dir then recreate it -> opaque
        # use /usr/share-style? safer: pick a small lower dir we won't disturb on host.
        # /etc/skel commonly exists; otherwise create our own scenario via a known dir.
        lower_dir = None
        for cand in ("/etc/profile.d", "/etc/skel"):
            p = Path(cand)
            if p.is_dir() and any(p.iterdir()): lower_dir = cand; break
        if lower_dir is None:
            check(True, "opaque: no non-empty lower dir candidate (skipped)")
            return
        rel = lower_dir.lstrip("/")    # relative = overlay path
        r = fx.sh(f"rm -rf {rel} && mkdir {rel} && echo new > {rel}/only.txt")
        check(r.returncode == 0, f"rm -rf + recreate {rel}")
        check(fx.index.is_opaque(rel), "recreated dir over lower is opaque")
        # listing shows only the new file, not the old lower children
        r2 = fx.sh(f"ls {rel}")
        check(r2.stdout.strip() == "only.txt", "opaque dir hides lower children")
    finally:
        fx.stop()


def test_rename():
    fx = MountFixture()
    try:
        fx.start()
        r = fx.sh("echo one > a.txt && mv a.txt b.txt && cat b.txt")
        check(r.returncode == 0 and r.stdout == "one\n", "rename created file")
        check(fx.index.kind_of("b.txt") == "file", "index: renamed dest tracked")
        check(fx.index.kind_of("a.txt") is None, "index: renamed src dropped")
        # rename a directory tree
        r2 = fx.sh("mkdir d1 && echo x > d1/inner.txt && mv d1 d2 && cat d2/inner.txt")
        check(r2.returncode == 0 and r2.stdout == "x\n", "rename dir tree")
        check(fx.index.kind_of("d2/inner.txt") == "file", "index: reparented child")
    finally:
        fx.stop()


def test_lower_symlink_copyup_preserves_type():
    """A lower (host) symlink that gets copied up (e.g. setattr on it) must remain a
    symlink in the upper, NOT be materialized as its target's bytes."""
    fx = MountFixture()
    try:
        fx.start()
        # find a host symlink to exercise (many exist under /usr/lib, /etc/...)
        host_link = None
        for cand in ("/etc/mtab", "/etc/os-release"):
            if Path(cand).is_symlink(): host_link = cand; break
        if host_link is None:
            # fall back: create one on the lower? can't write host. scan /usr/bin.
            import glob as _g
            for p in _g.glob("/usr/bin/*")[:2000]:
                if os.path.islink(p): host_link = p; break
        if host_link is None:
            check(True, "lower-symlink: no host symlink found (skipped)"); return
        rel = host_link.lstrip("/")
        # touch -h sets mtime on the link itself (setattr utime, follow=False)
        r = fx.sh(f"touch -h -d '2001-01-01' {rel} 2>&1; echo rc=$?")
        # the upper artifact (if materialized) must be a symlink, not a file
        up_art = fx.up / rel
        if up_art.exists() or up_art.is_symlink():
            check(up_art.is_symlink(),
                  "copied-up lower symlink stays a symlink (not target bytes)")
        else:
            check(True, "lower symlink not materialized (no copy-up needed) — ok")
    finally:
        fx.stop()


def test_passthrough_acts_on_host_records_nothing():
    """With -d (blanket passthrough) the box's writes land on the REAL host fs (a
    temp lower we own) and NOTHING is recorded in the overlay upper / the sqlar."""
    fx = MountFixture()
    host_lower = Path(tempfile.mkdtemp(prefix="ovl-pt-host-"))
    try:
        fx.start(lower=str(host_lower), passthrough=True)
        r = fx.sh("mkdir -p d && echo hi > d/f.txt && cat d/f.txt && rm d/f.txt; "
                  "echo rc=$?")
        check("hi" in r.stdout and "rc=0" in r.stdout, "passthrough create/read/delete")
        # the bytes went to the REAL host lower, NOT the overlay upper.
        check(not (fx.up / "d").exists(), "nothing recorded in the overlay upper")
        check(fx.index.kind_of("d") is None and fx.index.kind_of("d/f.txt") is None,
              "nothing recorded in the single sqlar for passthrough paths")
        # create one that survives, prove it's a real host file under the lower.
        r2 = fx.sh("echo survive > kept.txt; echo rc=$?")
        check("rc=0" in r2.stdout, "passthrough create succeeds")
        check((host_lower / "kept.txt").read_text() == "survive\n",
              "passthrough write landed on the real host lower")
        check(not (fx.up / "kept.txt").exists(),
              "passthrough write left no overlay artifact")
    finally:
        fx.stop()
        shutil.rmtree(host_lower, ignore_errors=True)


def test_nested_lower_chaining():
    """A box launched inside another box (parent= set) reads the layer beneath its own
    upper from the PARENT box's merged overlay view, not the real host: parent-created
    files show through, a parent whiteout hides a host file, and a child write copies up
    from the parent's upper into the child's own upper while the parent stays untouched."""
    tmp = Path(tempfile.mkdtemp(prefix="ovl-nest-"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    mnt = tmp / "mnt"; live = tmp / "live"
    psid = "20260604-000000_1"; csid = "20260604-000000_2"
    pbk = live / psid; cbk = live / csid
    (pbk / "up").mkdir(parents=True); (cbk / "up").mkdir(parents=True)
    pidx = m.Index(pbk); cidx = m.Index(cbk)
    mount = m.OverlayMount(mnt, lower="/")

    def sh(root, script, timeout=15):
        return subprocess.run(["timeout", str(timeout), "bash", "-c", script],
                              cwd=str(root), capture_output=True, text=True)
    try:
        if not mount.start():
            raise RuntimeError(f"mount failed: {mount._start_error}")
        mount.add_session(psid, pbk / "up", pidx)
        mount.add_session(csid, cbk / "up", cidx, parent=psid)
        proot = mnt / psid; croot = mnt / csid
        # (1) parent creates a file; the child reads it through the lower chain — and it
        #     never touches the real host.
        sh(proot, "echo from-parent > pfile.txt")
        r = sh(croot, "cat pfile.txt")
        check(r.returncode == 0 and r.stdout == "from-parent\n",
              "child reads a parent-created file through the lower chain")
        check(not Path("/pfile.txt").exists(), "parent-created file is NOT on the host")
        # (2) child appends -> copy-up from the parent's upper into the child's own
        #     upper; the parent's upper is untouched.
        r = sh(croot, "echo from-child >> pfile.txt && cat pfile.txt")
        check(r.stdout == "from-parent\nfrom-child\n",
              "child copies up the parent file and appends to its own copy")
        check(cidx.kind_of("pfile.txt") == "file", "child upper captured the file")
        # parent's pool blob must be untouched (child wrote to its own blob)
        p_rid = pidx.row_id("pfile.txt")
        p_blob = m.blob_path(pidx.box_id, p_rid) if p_rid is not None else None
        check(p_blob is not None and p_blob.read_bytes() == b"from-parent\n",
              "parent's pool blob is untouched by the child's write")
        # (3) a parent whiteout of a host file hides it from the child too.
        if Path("/etc/hostname").exists():
            sh(proot, "rm etc/hostname")
            r = sh(croot, "cat etc/hostname 2>&1; echo rc=$?")
            check("rc=1" in r.stdout or "No such file" in r.stdout,
                  "parent whiteout of a host file hides it from the child")
            check(Path("/etc/hostname").exists(), "host /etc/hostname still present")
        # (4) child readdir merges parent-created entries.
        sh(proot, "mkdir pdir && echo a > pdir/x")
        r = sh(croot, "ls pdir")
        check(r.stdout.strip() == "x", "child lists a parent-created dir's contents")
    finally:
        try:
            mount.stop()
        finally:
            try:
                if os.path.ismount(str(mnt)):
                    subprocess.run(["fusermount3", "-uz", str(mnt)],
                                   stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, timeout=10)
            except Exception: pass
            try: pidx.close(); cidx.close()
            except Exception: pass
            shutil.rmtree(tmp, ignore_errors=True)


def test_lazy_file_materialization():
    """Evicted files (data in row, blob absent) are readable without faulting to
    disk, and fault in only on write. unconsolidate() leaves files evicted."""
    fx = MountFixture()
    try:
        fx.start()
        known_bytes = b"lazy content line\n"
        # Create a regular file and a dir and a symlink through the mount.
        r = fx.sh("printf 'lazy content line\\n' > evictme.txt && "
                  "mkdir mydir && "
                  "ln -s /some/target mylink")
        check(r.returncode == 0, "lazy: created file, dir, symlink")

        # Consolidate with index= kept open so the live mount stays valid.
        blob_before = m.blob_path(fx.index.box_id, fx.index.row_id("evictme.txt"))
        m.consolidate(str(fx.backing), fx.sid, ["x"], index=fx.index)

        # (a) Blob must be gone (evicted).
        check(not blob_before.exists(), "lazy: pool blob evicted after consolidate")

        # (b) stat through the mount reports the right size (from row, no blob).
        r = fx.sh("wc -c < evictme.txt")
        check(r.returncode == 0 and r.stdout.strip() == str(len(known_bytes)),
              "lazy: stat of evicted file reports correct size")

        # (c) cat returns exact bytes served from row; blob remains absent.
        r = fx.sh("cat evictme.txt")
        check(r.returncode == 0 and r.stdout.encode() == known_bytes,
              "lazy: read of evicted file returns correct bytes")
        check(not blob_before.exists(),
              "lazy: blob still absent after read-only open (no materialization)")

        # (d) Append faults in: blob must appear with correct content.
        r = fx.sh("printf 'appended\\n' >> evictme.txt && cat evictme.txt")
        check(r.returncode == 0, "lazy: append to evicted file succeeds")
        check(blob_before.exists(), "lazy: blob materialized (faulted in) after write")
        check(r.stdout.encode() == known_bytes + b"appended\n",
              "lazy: faulted-in file has correct content after append")

        # (e) unconsolidate() rebuilds dir/symlink artifacts but leaves file evicted.
        # First consolidate again so everything is stored (blob re-created by fault-in
        # above, so consolidate will deflate it back into the row).
        m.consolidate(str(fx.backing), fx.sid, ["x"], index=fx.index)
        blob_after_second = m.blob_path(fx.index.box_id, fx.index.row_id("evictme.txt"))
        # Now call unconsolidate — should NOT re-create the file blob.
        m.unconsolidate(str(fx.backing), fx.sid)
        check(not blob_after_second.exists(),
              "lazy: unconsolidate does NOT recreate file blob (stays evicted)")
        # Dir and symlink up/<rel> artifacts must be rebuilt.
        check((fx.up / "mydir").is_dir(),
              "lazy: unconsolidate rebuilds dir up/<rel> artifact")
        check((fx.up / "mylink").is_symlink(),
              "lazy: unconsolidate rebuilds symlink up/<rel> artifact")
    finally:
        fx.stop()


def test_hardlink_as_copy():
    """link() of a regular file is a copy across overlay layers: both names hold the
    content, and the dest is an independent file whose bytes are in the pool (this
    regressed when file bytes moved out of up/<rel> into the pool)."""
    fx = MountFixture()
    try:
        fx.start()
        r = fx.sh("printf 'orig\\n' > f && ln f g && cat g")
        check(r.returncode == 0 and r.stdout == "orig\n",
              "hardlink dest reads the source content")
        check(fx.index.kind_of("g") == "file", "index: hardlink dest is a file")
        blob = m.blob_path(fx.index.box_id, fx.index.row_id("g"))
        check(blob.exists() and blob.read_bytes() == b"orig\n",
              "hardlink dest bytes are in the pool")
        # link-as-copy: the two names are independent (overlay can't true-hardlink).
        r = fx.sh("printf 'changed\\n' > f; cat g")
        check(r.stdout == "orig\n", "dest is independent of a later source rewrite")
    finally:
        fx.stop()


def test_rename_dir_replace_no_ghost():
    """Renaming a dir OVER a populated overlay dir must not leave the destination's
    prior children behind as ghost rows / orphaned pool blobs (reparent only renames
    the source subtree; the destination subtree must be pruned first)."""
    fx = MountFixture()
    try:
        fx.start()
        # dest dir with its own overlay file (row + pool blob), source dir with another
        r = fx.sh("mkdir d2 && printf 'old\\n' > d2/old.txt && "
                  "mkdir d1 && printf 'new\\n' > d1/new.txt && "
                  "mv -T d1 d2 && cat d2/new.txt; echo rc=$?")
        check("new" in r.stdout and "rc=0" in r.stdout, "mv -T dir over populated dir")
        old_rid = fx.index.row_id("d2/old.txt")
        check(fx.index.kind_of("d2/old.txt") is None,
              "destination's prior child is gone (no ghost row)")
        check(old_rid is None, "ghost row id cleared")
        # the merged listing shows only the source's child
        r = fx.sh("ls d2")
        check(r.stdout.strip() == "new.txt", "renamed dir shows only the source content")
        check(fx.index.kind_of("d2/new.txt") == "file", "source child reparented")
    finally:
        fx.stop()


if __name__ == "__main__":
    for t in (test_readthrough_and_create, test_copyup_modify,
              test_otrunc_rewrite_shorter, test_delete_whiteout,
              test_symlink_and_readlink, test_provenance_recorded, test_opaque_dir,
              test_rename, test_lower_symlink_copyup_preserves_type,
              test_passthrough_acts_on_host_records_nothing,
              test_nested_lower_chaining,
              test_lazy_file_materialization, test_hardlink_as_copy,
              test_rename_dir_replace_no_ghost):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc()
            _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
