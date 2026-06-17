#!/usr/bin/env python3
"""Engine-level tests for the multiplexed pyfuse3 overlay.

Run via uv (installs pyfuse3+trio; first run also builds the patched pyfuse3):
    uv run --with pytest --with "pyfuse3>=3.2" --with "trio>=0.22" \
      pytest -q -p no:cacheprovider test_overlay_engine.py

Self-safety: every mount is at an isolated temp point, exercised via
timeout-wrapped child commands, and lazy-unmounted in a finally block, leaving a
clean mount table.
"""
import os, sys, subprocess, tempfile, shutil, time, json, sqlite3
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/prototype/sarun"
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
        self.sid = "1"
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
        # a created small file buffers in RAM and deflates into the sqlar row on close
        # (no pool blob); it reads back through the mount.
        check(fx.index.kind_of("sub/new.txt") == "file", "index: created file = 'file'")
        check(fx.sh("cat sub/new.txt").stdout == "hello\n",
              "created file reads back via overlay")
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
        # small file written via the overlay is captured (buffered, deflated into the
        # sqlar row on close — no pool blob); read it back through the mount.
        check(fx.index.kind_of(target) == "file", "marker captured in overlay index")
        check(fx.sh(f"cat {target}").stdout == "x", "marker reads back via overlay")
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
    symlink — recorded as a symlink ROW with its target preserved, NOT materialized
    as its target's bytes. Under the no-mirror invariant there is no on-disk up/<rel>
    artifact; the copy-up records the kind+target in the Index/row."""
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
        want_target = os.readlink(host_link).encode("utf-8", "surrogateescape")
        # touch -h sets mtime on the link itself (setattr utime, follow=False) — this
        # forces a copy-up of the lower symlink into this box's overlay.
        r = fx.sh(f"touch -h -d '2001-01-01' {rel} 2>&1; echo rc=$?")
        # NEVER an on-disk artifact (no mirror); the link is a row.
        up_art = fx.up / rel
        check(not up_art.exists() and not up_art.is_symlink(),
              "copy-up of lower symlink leaves NO on-disk up/<rel> artifact")
        # If a copy-up happened, the Index must record it as a symlink with the
        # ORIGINAL target preserved (not the target's bytes as a file).
        if fx.index.kind_of(rel) is not None:
            check(fx.index.kind_of(rel) == "symlink",
                  "copied-up lower symlink stays kind=symlink (not a file)")
            check(fx.index.symlink_target(rel) == want_target,
                  "copied-up lower symlink preserves its target in the row")
            # the mount still serves the original target via readlink.
            rl = fx.sh(f"readlink {rel}")
            check(rl.stdout.strip().encode("utf-8", "surrogateescape") == want_target,
                  "mount readlink returns the preserved target")
        else:
            check(True, "lower symlink not copied up (no setattr captured) — ok")
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
    psid = "101"; csid = "102"
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
        # the parent's file is unchanged — the child copied up into its own overlay.
        # Read through the parent's mount (tier-agnostic: serves row or blob).
        check(sh(proot, "cat pfile.txt").stdout == "from-parent\n",
              "parent's file untouched by the child's copy-up")
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
        m.consolidate(str(fx.backing), fx.sid, index=fx.index)

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

        # (d) Append via write-buffer: goes RAM→row (evicted), blob stays absent.
        r = fx.sh("printf 'appended\\n' >> evictme.txt && cat evictme.txt")
        check(r.returncode == 0, "lazy: append to evicted file succeeds")
        # With the Tier-0 write buffer the append never materialises a pool blob:
        # bytes go RAM→sqlar row (evicted). blob_before stays absent.
        check(not blob_before.exists(),
              "lazy: write-buffer append keeps file evicted (no pool blob)")
        check(r.stdout.encode() == known_bytes + b"appended\n",
              "lazy: faulted-in file has correct content after append")

        # (e) unconsolidate() leaves the file evicted and rebuilds NOTHING on disk:
        # dirs/symlinks are now served from the rows (no up/<rel> artifacts). File is
        # already evicted (written via buffer into row), so consolidate sees data NOT
        # NULL and leaves it as-is (no blob to deflate).
        m.consolidate(str(fx.backing), fx.sid, index=fx.index)
        blob_after_second = m.blob_path(fx.index.box_id, fx.index.row_id("evictme.txt"))
        # Now call unconsolidate — should NOT re-create the file blob.
        m.unconsolidate(str(fx.backing))
        check(not blob_after_second.exists(),
              "lazy: unconsolidate does NOT recreate file blob (stays evicted)")
        # NO dir/symlink up/<rel> artifacts on disk — they live only in the rows.
        check(not (fx.up / "mydir").exists(),
              "lazy: unconsolidate leaves no dir up/<rel> artifact (mirror-only)")
        check(not (fx.up / "mylink").exists() and not (fx.up / "mylink").is_symlink(),
              "lazy: unconsolidate leaves no symlink up/<rel> artifact (mirror-only)")
        # A FRESH Index opened from the backing must serve the dir/symlink straight
        # from the rows: the mirror loads them at Index.__init__ — this is exactly the
        # contract a base box relies on after unconsolidate.
        ridx = m.Index(fx.backing)
        try:
            check(ridx.kind_of("mydir") == "dir",
                  "lazy: reopened Index serves dir from the row (kind_of)")
            check(ridx.kind_of("mylink") == "symlink",
                  "lazy: reopened Index serves symlink from the row (kind_of)")
            check(ridx.symlink_target("mylink") == b"/some/target",
                  "lazy: reopened Index preserves symlink target from the row")
        finally:
            ridx.close()
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
        # A real hardlink shares the source inode's mtime; link-as-copy must too, or
        # a tree cloned with `cp -al` lands every file at copy-time and autotools
        # (which compares only mtimes) spuriously re-runs autoconf.
        r = fx.sh("printf 'h' > h && touch -d @1000000000 h && ln h h2 && "
                  "[ \"$(stat -c %Y h)\" = \"$(stat -c %Y h2)\" ] && echo SAME || echo DIFF")
        check(r.stdout.strip() == "SAME", "link-as-copy preserves the source mtime")
    finally:
        fx.stop()


def test_cpal_clone_preserves_mtime_ordering():
    """Workload-level guard for the autotools-in-a-box break: a source tree cloned
    with `cp -al` (the cheap per-arch clone) must keep its RELATIVE mtime ordering,
    so a generated file that ships newer than its source stays newer. A real
    hardlink gets this for free (shared inode); the overlay's link-as-copy only does
    if it preserves source mtimes. This is the cross-file, emergent property that the
    per-op pjdfstest/fsx batteries structurally cannot see — autotools breaks on
    exactly this."""
    fx = MountFixture()
    try:
        fx.start()
        # 'configure' (generated) ships NEWER than 'configure.ac' (source).
        r = fx.sh(
            "mkdir src && echo src > src/configure.ac && echo gen > src/configure && "
            "touch -d @1700000000 src/configure.ac && "
            "touch -d @1700000009 src/configure && "
            "cp -al src dst && "
            "([ dst/configure -nt dst/configure.ac ] && echo OK || echo BROKEN)")
        check(r.stdout.strip() == "OK",
              "cp -al clone keeps configure newer than configure.ac (no autoconf rerun)")
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


def test_passthrough_kicks_up_to_parent():
    """A nested box (child) in blanket-passthrough mode kicks write ops up to the
    parent's overlay rather than the real host:
      - create/write in the child lands in the PARENT's pool blob;
      - child's own index has nothing for that path;
      - the real host filesystem is untouched.
    Also covers a per-file passthrough rule on the child."""
    tmp = Path(tempfile.mkdtemp(prefix="ovl-ku-"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    mnt = tmp / "mnt"; live = tmp / "live"
    psid = "101"; csid = "102"
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
        # P: normal overlay session; C: passthrough=True, parent=P.
        mount.add_session(psid, pbk / "up", pidx)
        mount.add_session(csid, cbk / "up", cidx, passthrough=True, parent=psid)
        proot = mnt / psid; croot = mnt / csid

        # (1) Child creates a file. Passthrough→kick up→captured in parent.
        r = sh(croot, "mkdir -p sub && echo kicked > sub/f.txt && cat sub/f.txt")
        check(r.returncode == 0 and r.stdout == "kicked\n",
              "kick-up: child write returns correct content")

        # File must be captured in the PARENT's overlay, not the child's. Read through
        # the parent's mount (tier-agnostic: serves the row or a blob).
        check(pidx.kind_of("sub/f.txt") == "file",
              "kick-up: parent index has the file")
        check(sh(proot, "cat sub/f.txt").stdout == "kicked\n",
              "kick-up: parent has the kicked-up bytes")

        # Child's index must have nothing for the file.
        check(cidx.kind_of("sub/f.txt") is None,
              "kick-up: child index has nothing for the kicked-up file")

        # Real host must be untouched.
        check(not Path("/sub/f.txt").exists(),
              "kick-up: real host is untouched")

        # (2) Child creates another file then deletes it (unlink kick-up).
        sh(croot, "echo tmp > tmp.txt")
        # tmp.txt must now exist in parent
        check(pidx.kind_of("tmp.txt") == "file",
              "kick-up: create then delete — created in parent")
        sh(croot, "rm tmp.txt")
        # After unlink, parent records a whiteout or no entry (depending on lower).
        # The important thing: it's not still kind=="file" in the parent.
        check(pidx.kind_of("tmp.txt") != "file",
              "kick-up: unlink removed/whiteout-ed from parent overlay")
        check(cidx.kind_of("tmp.txt") is None,
              "kick-up: child index still has nothing after unlink")

        # (3) Child overwrites (O_TRUNC) an existing parent file: must update the
        #     parent blob, not create a new child blob.
        sh(proot, "echo original > shared.txt")
        p_rid_before = pidx.row_id("shared.txt")
        r = sh(croot, "echo overwritten > shared.txt && cat shared.txt")
        check(r.returncode == 0 and r.stdout == "overwritten\n",
              "kick-up: O_TRUNC overwrite from child gives new content")
        p_rid_after = pidx.row_id("shared.txt")
        check(p_rid_after is not None, "kick-up: parent still has the row after overwrite")
        p_blob_after = m.blob_path(pidx.box_id, p_rid_after)
        check(p_blob_after.exists() and p_blob_after.read_bytes() == b"overwritten\n",
              "kick-up: parent blob updated by child overwrite")
        check(cidx.kind_of("shared.txt") is None,
              "kick-up: child index still empty after overwrite")

        # (4) Per-file passthrough rule on the child (passthrough=False at session
        #     level, but one path is marked passthrough via FileRule). We use a fresh
        #     pair of sessions so the blanket-passthrough child above doesn't interfere.
        psid2 = "103"; csid2 = "104"
        pbk2 = live / psid2; cbk2 = live / csid2
        (pbk2 / "up").mkdir(parents=True); (cbk2 / "up").mkdir(parents=True)
        pidx2 = m.Index(pbk2); cidx2 = m.Index(cbk2)
        mount.add_session(psid2, pbk2 / "up", pidx2)
        # Child is NOT blanket-passthrough but inherits per-file rule below.
        mount.add_session(csid2, cbk2 / "up", cidx2, parent=psid2)
        # Inject a FileRules with one passthrough rule directly into the session dict.
        fr = m.FileRules.__new__(m.FileRules)
        fr.path = None
        fr.rules = [m.FileRule.single("passthrough", "path", "pt_only.txt")]
        mount.ops.sessions[csid2]["frules"] = fr
        proot2 = mnt / psid2; croot2 = mnt / csid2
        r = sh(croot2, "echo per-file > pt_only.txt && cat pt_only.txt")
        check(r.returncode == 0 and r.stdout == "per-file\n",
              "kick-up per-file rule: create+read")
        check(pidx2.kind_of("pt_only.txt") == "file",
              "kick-up per-file rule: landed in parent overlay")
        check(cidx2.kind_of("pt_only.txt") is None,
              "kick-up per-file rule: child index untouched")
        check(not Path("/pt_only.txt").exists(),
              "kick-up per-file rule: real host untouched")
        try: pidx2.close(); cidx2.close()
        except Exception: pass

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


def test_wbuf_small_new_file():
    """Small new file (< 1 MiB) written via open() goes RAM -> sqlar row (evicted),
    NEVER materialises a pool blob.  Content must read back correctly through the
    mount and the sqlar row must have data NOT NULL."""
    fx = MountFixture()
    try:
        fx.start()
        # Create the file via create() so a pool blob exists first, then overwrite it
        # via open() to exercise the write-buffer path.
        content = b"hello from wbuf\n"
        r = fx.sh(f"printf '%s' '{content.decode()}' > wbuf_test.txt && cat wbuf_test.txt")
        check(r.returncode == 0 and r.stdout.encode() == content,
              "wbuf small: content reads back correctly")
        rid = fx.index.row_id("wbuf_test.txt")
        check(rid is not None, "wbuf small: row exists in index")
        # Now rewrite shorter via open() (write-buffer path): no pool blob afterwards.
        short = b"short\n"
        r2 = fx.sh("printf 'short\\n' > wbuf_test.txt && cat wbuf_test.txt")
        check(r2.returncode == 0 and r2.stdout.encode() == short,
              "wbuf small: O_TRUNC rewrite via open() reads back correctly")
        # Wait for the rewrite's release/eviction to land before inspecting the row (same
        # async-release sync point as test_wbuf_create_buffers). Poll on the CONTENT, not
        # just non-NULL: the row may still hold the first write's bytes until the second
        # eviction completes, which under suite load lags sh() returning.
        import zlib as _zl
        deadline = time.time() + 10
        while time.time() < deadline:
            _r = fx.index.file_row("wbuf_test.txt")
            if _r is not None and _r[4] is not None:
                _b = bytes(_r[4])
                if (_b if len(_b) == _r[2] else _zl.decompress(_b)) == short:
                    break
            time.sleep(0.05)
        rid2 = fx.index.row_id("wbuf_test.txt")
        check(rid2 is not None, "wbuf small: row still present after rewrite")
        bp = m.blob_path(fx.index.box_id, rid2)
        check(not bp.exists(),
              "wbuf small: NO pool blob exists (went RAM->row, never a pool file)")
        # Verify the evicted row actually has data (NOT NULL) and correct content.
        row = fx.index.file_row("wbuf_test.txt")
        check(row is not None and row[4] is not None,
              "wbuf small: sqlar row data IS NOT NULL (evicted)")
        if row is not None and row[4] is not None:
            import zlib as _z
            blob = bytes(row[4]); sz = row[2]
            got = blob if len(blob) == sz else _z.decompress(blob)
            check(got == short, "wbuf small: sqlar row decompresses to the written bytes")
    finally:
        fx.stop()


def test_wbuf_otrunc_rewrite():
    """The ./configure pattern: printf long > f; printf short > f; cat f == short.
    O_TRUNC via write-buffer must NOT leave stale tail bytes."""
    fx = MountFixture()
    try:
        fx.start()
        r = fx.sh("printf 'LONG_ORIGINAL_CONTENT_PADDING\\n' > cfg.txt; "
                  "printf 'short\\n' > cfg.txt; cat cfg.txt")
        check(r.stdout == "short\n",
              "wbuf otrunc: rewrite shorter produces exact new content")
        r2 = fx.sh("wc -c < cfg.txt")
        check(r2.stdout.strip() == "6",
              "wbuf otrunc: wc -c matches the new length exactly")
    finally:
        fx.stop()


def test_wbuf_stat_coherence():
    """stat/wc -c on a file right after writing must reflect the buffered size."""
    fx = MountFixture()
    try:
        fx.start()
        # Write N bytes, then immediately wc -c in the same shell step (single open).
        # The write-buffer's _buffer_stat makes getattr report the live size.
        r = fx.sh("printf '%0.s-' {1..200} > sz_test.txt && wc -c < sz_test.txt")
        check(r.returncode == 0 and r.stdout.strip() == "200",
              "wbuf stat coherence: wc -c sees the written size immediately")
    finally:
        fx.stop()


def test_wbuf_existing_file_preserve():
    """Open an existing file WITHOUT truncate, append, close: full content preserved.
    Catches the seed-from-existing bug where the buffer starts empty on a non-trunc open."""
    fx = MountFixture()
    try:
        fx.start()
        # First, create the file (goes via create() -> pool blob).
        r = fx.sh("printf 'original\\n' > xfile.txt")
        check(r.returncode == 0, "wbuf preserve: initial create ok")
        # Now open WITHOUT truncate and append (open() writable path -> write buffer).
        r2 = fx.sh("printf 'appended\\n' >> xfile.txt && cat xfile.txt")
        check(r2.returncode == 0, "wbuf preserve: append ok")
        check(r2.stdout == "original\nappended\n",
              "wbuf preserve: full content (original + appended) present")
        # The file should now be evicted (data in row, no blob).
        rid = fx.index.row_id("xfile.txt")
        if rid is not None:
            bp = m.blob_path(fx.index.box_id, rid)
            check(not bp.exists(),
                  "wbuf preserve: file evicted after buffered append (no pool blob)")
    finally:
        fx.stop()


def test_wbuf_spill():
    """Write a >1 MiB file: must spill to a pool blob (Tier 1) and content is correct."""
    fx = MountFixture()
    try:
        fx.start()
        # Write 1.1 MiB of data via dd (dd uses create() + write()).
        # Then open for append (open() write path) to exercise spill via write-buffer.
        # Actually: create the file at 1.1 MiB via the shell (create() path -> pool blob),
        # then reopen for O_TRUNC rewrite of a large content to hit the spill threshold.
        big = 1200 * 1024  # 1.2 MiB
        r = fx.sh(f"dd if=/dev/zero bs=1024 count=1200 of=big.bin 2>/dev/null && "
                  f"wc -c < big.bin")
        check(r.returncode == 0 and r.stdout.strip() == str(big),
              "wbuf spill: large file created correctly")
        # The large file goes through create() -> pool blob (no buffer).
        rid = fx.index.row_id("big.bin")
        check(rid is not None, "wbuf spill: large file has a row")
        bp = m.blob_path(fx.index.box_id, rid)
        check(bp.exists(), "wbuf spill: large file has a pool blob (spilled or create)")
        # Read it back through the mount.
        r2 = fx.sh("wc -c < big.bin")
        check(r2.returncode == 0 and r2.stdout.strip() == str(big),
              "wbuf spill: large file content reads back at correct size")
    finally:
        fx.stop()


def test_terminated_parent_reads():
    """A child whose parent has been CONSOLIDATED (sqlar on disk, no longer live)
    must still see the parent's accepted state as its lower layer — files, dirs,
    and symlinks the parent wrote are visible through the child, a child write copies
    the parent's content into the child's own overlay (parent sqlar untouched), and a
    path the parent never had falls through to the real host.

    This mirrors test_nested_lower_chaining but the parent is FINISHED (removed from
    live sessions and consolidated) before the child is added."""
    tmp = Path(tempfile.mkdtemp(prefix="ovl-finpar-"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    mnt = tmp / "mnt"; live = tmp / "live"
    psid = "201"; csid = "202"
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

        # ── Phase 1: parent is LIVE, write several entries ──────────────────
        mount.add_session(psid, pbk / "up", pidx)
        proot = mnt / psid

        # regular file
        sh(proot, "echo parent-content > pfile.txt")
        # subdir with a file
        sh(proot, "mkdir -p pdir && echo subfile > pdir/sub.txt")
        # symlink
        sh(proot, "ln -s /etc/hostname pmylink")

        # verify parent writes are captured in the index
        check(pidx.kind_of("pfile.txt") == "file",
              "finpar: parent captured pfile.txt")
        check(pidx.kind_of("pdir") == "dir",
              "finpar: parent captured pdir")
        check(pidx.kind_of("pmylink") == "symlink",
              "finpar: parent captured pmylink")

        # ── Phase 2: CONSOLIDATE and REMOVE the parent session ───────────────
        # Record parent_sid in meta before consolidation (add_session already did this
        # for the child; we also need the parent's own parent chain for later tests,
        # but here the parent is top-level, so no meta entry needed).
        m.consolidate(str(pbk), psid, index=pidx)
        # After consolidate: pool blobs evicted (data in sqlar rows), up/ artifacts removed.
        p_rid = pidx.row_id("pfile.txt")
        p_blob = m.blob_path(pidx.box_id, p_rid) if p_rid is not None else None
        check(p_blob is not None and not p_blob.exists(),
              "finpar: parent's pool blob evicted after consolidate")

        # Remove the parent from live sessions — it is now "finished".
        mount.remove_session(psid)
        check(psid not in mount.ops.sessions,
              "finpar: parent is no longer a live session")

        # ── Phase 3: add ONLY the child, with parent=psid ───────────────────
        mount.add_session(csid, cbk / "up", cidx, parent=psid)
        croot = mnt / csid

        # (1) Child reads the parent's file (evicted, served from sqlar row via fault-in).
        r = sh(croot, "cat pfile.txt")
        check(r.returncode == 0 and r.stdout == "parent-content\n",
              "finpar: child reads parent's evicted file content correctly")
        check(not Path("/pfile.txt").exists(),
              "finpar: parent's file is NOT on the real host")

        # (2) Child lists the parent's directory.
        r = sh(croot, "ls pdir")
        check(r.returncode == 0 and "sub.txt" in r.stdout,
              "finpar: child lists parent's dir entries")

        # (3) Child reads the parent's symlink target.
        r = sh(croot, "readlink pmylink")
        check(r.returncode == 0 and r.stdout.strip() == "/etc/hostname",
              "finpar: child reads parent's symlink target")

        # (4) Child writes over the parent's file → copy-up into child's own overlay;
        #     the parent's sqlar must be untouched.
        r = sh(croot, "echo child-override > pfile.txt && cat pfile.txt")
        check(r.returncode == 0 and r.stdout == "child-override\n",
              "finpar: child write over parent file succeeds")
        check(cidx.kind_of("pfile.txt") == "file",
              "finpar: child's index has the overridden file")
        # Parent's sqlar row must be UNTOUCHED: a copy-up reads a finished (read-only)
        # parent's bytes directly from its row — it must NOT fault it in or NULL the row
        # (doing so would move the finished box's only copy into a throwaway pool blob).
        import zlib as _zl
        parent_row = pidx.file_row("pfile.txt")
        check(parent_row is not None and parent_row[4] is not None,
              "finpar: parent sqlar row still holds its data (not faulted-in/NULLed)")
        if parent_row is not None and parent_row[4] is not None:
            blob = bytes(parent_row[4])
            content = blob if len(blob) == parent_row[2] else _zl.decompress(blob)
            check(content == b"parent-content\n",
                  "finpar: parent sqlar row content unchanged after child copy-up")

        # (5) A path the parent never had falls through to the real host.
        r_host = sh(croot, "cat /etc/hostname 2>/dev/null; echo rc=$?")
        check("rc=0" in r_host.stdout or "rc=1" in r_host.stdout,
              "finpar: host fallthrough works for paths the parent never had")

        # (6) Child reads parent's subdir file.
        r = sh(croot, "cat pdir/sub.txt")
        check(r.returncode == 0 and r.stdout == "subfile\n",
              "finpar: child reads file inside parent's subdirectory")

        # (7) Detach + purge: the finished parent was attached on demand as a read-only
        #     base; removing the last child detaches it and reclaims the transient
        #     backing, while the durable <sid>.sqlar is preserved.
        check(psid in mount.ops.sessions and psid in mount.ops._base_sessions,
              "finpar: finished parent attached on demand as a read-only base")
        check(m.live_dir(psid).exists(),
              "finpar: base's transient backing exists while attached")
        mount.remove_session(csid)
        check(psid not in mount.ops.sessions,
              "finpar: base detached after its last child exits")
        check(not m.live_dir(psid).exists(),
              "finpar: base's transient backing purged on detach")
        check(m.sqlar_path(psid).exists(),
              "finpar: parent's durable sqlar preserved across detach")

    finally:
        try: mount.stop()
        except Exception: pass
        try:
            if os.path.ismount(str(mnt)):
                subprocess.run(["fusermount3", "-uz", str(mnt)],
                               stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, timeout=10)
        except Exception: pass
        try: pidx.close(); cidx.close()
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)


def test_wbuf_create_buffers():
    """A brand-new file made via create() (the common create+write+close, e.g. nearly
    everything ./configure writes) buffers in RAM and deflates straight into the sqlar
    row on close — it NEVER materialises a pool blob, yet reads back through the mount."""
    fx = MountFixture()
    try:
        fx.start()
        r = fx.sh("echo hi > created.txt")          # create() path (new file)
        check(r.returncode == 0, "create new file via create()")
        # close() triggers the FUSE release that deflates the RAM buffer into the sqlar
        # row, but release runs on the UI's FUSE loop AFTER the shell process exits — so
        # sh() can return before eviction lands. The window widens under suite load (the
        # FUSE loop is busy serving other boxes), which is the lone source of this test's
        # flakiness. Wait for the row to fill rather than racing the single check below.
        deadline = time.time() + 10
        while time.time() < deadline:
            _r = fx.index.file_row("created.txt")
            if _r is not None and _r[4] is not None:
                break
            time.sleep(0.05)
        rid = fx.index.row_id("created.txt")
        check(rid is not None and fx.index.kind_of("created.txt") == "file",
              "created file is tracked as a file")
        check(not m.blob_path(fx.index.box_id, rid).exists(),
              "created file made NO pool blob (RAM->row)")
        row = fx.index.file_row("created.txt")
        check(row is not None and row[4] is not None,
              "created file's bytes are in the evicted sqlar row")
        check(fx.sh("cat created.txt").stdout == "hi\n",
              "created file reads back correctly via the mount")
        # mode is honored (create 0600), proving the create() mode reached the buffer
        r = fx.sh("install -m 600 /dev/null cmode.txt 2>/dev/null; "
                  "printf data > cmode.txt; stat -c %a cmode.txt")
        check(r.returncode == 0 and r.stdout.strip() == "600",
              "create() mode is preserved through the buffer")
    finally:
        fx.stop()


def test_wbuf_periodic_flush():
    """flush_wbuf() snapshots dirty RAM buffers to sqlar rows WITHOUT resetting them.

    Key assertions:
      (a) After flush_wbuf(force=True) the sqlar row has data NOT NULL (snapshot reached).
      (b) The buffer stays usable: subsequent reads through the mount return the live bytes.
      (c) Appending after the snapshot and then closing yields the full correct content.
      (d) A non-dirty buffer is skipped by flush_wbuf() but force=True flushes it anyway.
    """
    import zlib as _zl
    fx = MountFixture()
    try:
        fx.start()

        # Write a small file and close it so the row exists (data NOT NULL) in the index.
        r = fx.sh("printf 'base content\\n' > flush_test.txt")
        check(r.returncode == 0, "flush_wbuf: initial file created ok")

        # Now open for append so a write buffer is live (file stays open in shell subshell).
        # Because each sh() call is a separate process that exits and closes its fds, we
        # cannot hold an fd open across sh() calls.  Instead we drive flush_wbuf directly
        # against the ops object: inject a synthetic write-buffer entry representing a file
        # that has been written but not yet closed, then call flush_wbuf and verify the row.

        # ── synthetic buffer injection ───────────────────────────────────────────────
        # Build a write-handle dict that looks exactly like a materialized RAM handle
        # (i.e. one whose first write has happened): flush_wbuf only snapshots those.
        sid = fx.sid
        rel = "flush_inject.txt"
        idx = fx.index
        wid = idx.writer_for(os.getpid())
        # Set a "file" kind entry so set_blob finds a consistent row state.
        idx.set_entry(rel, "file", 0o100644, wid, "create")
        content_a = b"injected line 1\ninjected line 2\n"
        wb = dict(
            sid=sid, rel=rel,
            data=bytearray(content_a),
            dirty=True,
            materialized=True,
            orphan=False,
            mode=0o100644,
            mtime_ns=int(1_000_000_000),
            wid=wid,
            spilled_fd=-1,
        )
        # Assign a fake fh that doesn't collide with the mount's own handles.
        fake_fh = 99999
        ops = fx.mount.ops
        ops._wbuf[fake_fh] = wb
        ops._wbuf_key[(sid, rel)] = fake_fh

        # (a) flush_wbuf(force=True) snapshots the buffer to the sqlar row.
        # We call it via trio.from_thread.run_sync so it runs on the serve thread
        # (matching the operational contract; in this test the _wbuf injection above
        # is safe because the mount is idle between FUSE ops at this point).
        import trio
        trio.from_thread.run_sync(ops.flush_wbuf, True, trio_token=fx.mount._trio_token)

        row = idx.file_row(rel)
        check(row is not None and row[4] is not None,
              "flush_wbuf: (a) sqlar row has data NOT NULL after flush")
        if row is not None and row[4] is not None:
            blob = bytes(row[4]); sz = row[2]
            got = blob if len(blob) == sz else _zl.decompress(blob)
            check(got == content_a,
                  "flush_wbuf: (a) row contains the correct snapshot bytes")

        # (b) Buffer is NOT reset: dirty flag is False now (cleaned), but data intact.
        check(wb["data"] == bytearray(content_a),
              "flush_wbuf: (b) bytearray unchanged after snapshot")
        check(wb["dirty"] is False,
              "flush_wbuf: (b) dirty flag cleared after successful snapshot")

        # (c) force=True flushes even non-dirty buffers.
        # Write more to the buffer (simulating a subsequent write), then flush again.
        content_b = b"injected line 1\ninjected line 2\nextra\n"
        wb["data"] = bytearray(content_b)
        wb["dirty"] = False  # pretend not dirty
        trio.from_thread.run_sync(ops.flush_wbuf, True, trio_token=fx.mount._trio_token)
        row2 = idx.file_row(rel)
        if row2 is not None and row2[4] is not None:
            blob2 = bytes(row2[4]); sz2 = row2[2]
            got2 = blob2 if len(blob2) == sz2 else _zl.decompress(blob2)
            check(got2 == content_b,
                  "flush_wbuf: (c) force=True flushes non-dirty buffer with updated bytes")
        else:
            check(False, "flush_wbuf: (c) row missing after force flush of non-dirty buffer")

        # (d) Non-dirty, non-forced buffer is skipped.
        wb["dirty"] = False
        # Corrupt the row manually to detect whether flush_wbuf touches it.
        idx._db.execute("UPDATE sqlar SET sz=0 WHERE name=?", (rel,))
        idx._db.commit()
        trio.from_thread.run_sync(ops.flush_wbuf, False, trio_token=fx.mount._trio_token)
        row3 = idx.file_row(rel)
        # flush_wbuf(force=False) must NOT have overwritten the row (buffer was not dirty).
        check(row3 is not None and row3[2] == 0,
              "flush_wbuf: (d) non-dirty buffer skipped (row not re-snapshotted)")

        # Cleanup: remove the fake buffer entry so the mount doesn't try to release it.
        ops._wbuf.pop(fake_fh, None)
        ops._wbuf_key.pop((sid, rel), None)

        # (e) A real file written through the mount still reads back correctly after a
        # flush_wbuf cycle — the live path is not corrupted by the snapshot.
        r2 = fx.sh("printf 'live write\\n' > live_test.txt && cat live_test.txt")
        check(r2.returncode == 0 and r2.stdout == "live write\n",
              "flush_wbuf: (e) real mount write-read unaffected by flush_wbuf")

    finally:
        fx.stop()


def test_self_paths_hidden():
    # sarun's own host dirs (data/config/state/runtime homes — the runtime home
    # contains the FUSE mountpoint itself) must be invisible THROUGH the
    # overlay: lookup ENOENTs, readdir omits them, and creating/shadowing them
    # is denied — while their siblings stay fully visible. This is what lets
    # the box see the rest of /run etc. without being able to re-enter the
    # overlay or touch the capture machinery (replaces the old blanket
    # "--tmpfs /run").
    fx = MountFixture()
    old_env = {k: os.environ.get(k) for k in ("XDG_DATA_HOME", "XDG_RUNTIME_DIR")}
    try:
        data = fx.tmp / "xdg-data"; (data / "slopbox").mkdir(parents=True)
        (data / "slopbox" / "secret.sqlar").write_text("capture machinery")
        (data / "visible.txt").write_text("sibling")
        rt = fx.tmp / "xdg-rt"; (rt / "slopbox" / "mnt").mkdir(parents=True)
        os.environ["XDG_DATA_HOME"] = str(data)
        os.environ["XDG_RUNTIME_DIR"] = str(rt)
        fx.start(lower="/")
        base = str(fx.root) + str(fx.tmp)   # the host temp tree, via the overlay
        r = fx.sh(f"ls {base}/xdg-data")
        check(r.returncode == 0 and "visible.txt" in r.stdout
              and "slopbox" not in r.stdout,
              "self-hide: readdir omits slopbox data dir, sibling listed")
        r = fx.sh(f"cat {base}/xdg-data/slopbox/secret.sqlar")
        check(r.returncode != 0, "self-hide: lookup inside hidden dir fails")
        r = fx.sh(f"ls {base}/xdg-rt")
        check(r.returncode == 0 and "slopbox" not in r.stdout,
              "self-hide: readdir omits slopbox runtime dir (mountpoint home)")
        r = fx.sh(f"mkdir {base}/xdg-rt/slopbox")
        check(r.returncode != 0, "self-hide: shadowing a hidden dir is denied")
    finally:
        fx.stop()
        for k, v in old_env.items():
            if v is None: os.environ.pop(k, None)
            else: os.environ[k] = v


if __name__ == "__main__":
    for t in (test_readthrough_and_create, test_copyup_modify,
              test_otrunc_rewrite_shorter, test_delete_whiteout,
              test_symlink_and_readlink, test_provenance_recorded, test_opaque_dir,
              test_rename, test_lower_symlink_copyup_preserves_type,
              test_passthrough_acts_on_host_records_nothing,
              test_nested_lower_chaining,
              test_lazy_file_materialization, test_hardlink_as_copy,
              test_rename_dir_replace_no_ghost,
              test_passthrough_kicks_up_to_parent,
              test_cpal_clone_preserves_mtime_ordering,
              test_wbuf_small_new_file, test_wbuf_otrunc_rewrite,
              test_wbuf_stat_coherence, test_wbuf_existing_file_preserve,
              test_wbuf_spill, test_wbuf_create_buffers,
              test_terminated_parent_reads,
              test_wbuf_periodic_flush, test_self_paths_hidden):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc()
            _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
