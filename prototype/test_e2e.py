#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "textual>=0.60", "wcmatch>=8.4",
#   "pyfuse3>=3.2", "trio>=0.22", "python-magic>=0.4",
# ]
# ///
"""End-to-end: start the real UI process, run a real `slopbox CMD` against it, and
verify (a) the box writes/modifies/deletes through the UI-served overlay, (b) the UI
consolidates on exit into patch/sqlar/tombstones + provenance, (c) the mount table
is clean afterwards, and (d) `slopbox CMD` with NO UI fails fast.

    ./test_e2e.py            # uv installs deps and provisions the box interpreter
    uv run test_e2e.py       # same thing, explicit

Needs real bwrap (works in this sandbox). The test process runs under uv (so it
has sarun's deps for the in-process SourceFileLoader), and the UI/box subprocesses
reuse that same deps-equipped interpreter via sys.executable — there is NO
hardcoded venv. First run also builds the patched pyfuse3 (section 0 of sarun),
~25 s once, then cached.

Self-safety: isolated XDG temp tree; the UI is launched headless and killed in a
finally; the overlay is lazy-unmounted on the way out.
"""
import os, sys, time, signal, socket, subprocess, tempfile, shutil, sqlite3, json
import stat as stat_mod
from pathlib import Path
from importlib.machinery import SourceFileLoader

# The interpreter the box/UI subprocesses run under: the very one running this test,
# which uv (the shebang above) has already equipped with sarun's deps. No hardcoded venv.
PYBIN = sys.executable
SARUN = "/home/user/sarun/prototype/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def env_for(tmp):
    e = dict(os.environ)
    e["XDG_STATE_HOME"] = str(tmp/"state")
    e["XDG_RUNTIME_DIR"] = str(tmp/"run")
    e["XDG_CONFIG_HOME"] = str(tmp/"config")
    e["XDG_DATA_HOME"] = str(tmp/"data")
    # headless textual so the UI process runs without a real terminal
    e["TEXTUAL"] = ""
    e.setdefault("TERM", "dumb")
    return e


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.2)
    return False


def test_no_ui_fails_fast(tmp):
    e = env_for(tmp)
    r = subprocess.run([PYBIN, SARUN, "--", "true"], env=e,
                       capture_output=True, text=True, timeout=30)
    check(r.returncode != 0, "slopbox CMD with no UI exits non-zero")
    check("UI is not running" in r.stderr or "not running" in r.stderr.lower(),
          "no-UI run prints a clear 'UI not running' message")


def test_dash_dash_required(tmp):
    e = env_for(tmp)
    r = subprocess.run([PYBIN, SARUN, "ls"], env=e,
                       capture_output=True, text=True, timeout=30)
    check(r.returncode != 0, "slopbox CMD without `--` exits non-zero")
    check("--" in r.stderr and "command" in r.stderr.lower(),
          "missing `--` prints a clear error suggesting `slopbox -- ls`")


def run_with_ui(tmp):
    m = SourceFileLoader("slopbox", SARUN).load_module()
    e = env_for(tmp)
    sock = str(Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock")
    mnt = Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "mnt"
    # launch the UI headless. Run() needs a screen; use textual's headless driver by
    # importing and calling run with headless. Simplest: drive run_ui in a subprocess
    # with a pty-less headless app via the App.run(headless=...) — not exposed on the
    # CLI, so we spawn a tiny harness that runs the app headless.
    harness = tmp / "ui_harness.py"
    harness.write_text(
        "import os\n"
        "from importlib.machinery import SourceFileLoader\n"
        f"m = SourceFileLoader('slopbox', {SARUN!r}).load_module()\n"
        "m.ensure_dirs()\n"
        "app = m._make_ui_app()()\n"
        "app.run(headless=True)\n")
    ui = subprocess.Popen([PYBIN, str(harness)], env=e,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        if not wait_socket(sock, 30):
            out = b""
            try: out = ui.stdout.read(4000) if ui.stdout else b""
            except Exception: pass
            raise RuntimeError(f"UI socket never appeared. UI output:\n{out.decode(errors='replace')}")

        # pick a victim host file we can safely 'delete' inside the overlay
        # (only the overlay view changes; the host file is never touched)
        victim = "etc/hostname"
        host_victim = Path("/") / victim
        host_before = host_victim.read_bytes() if host_victim.exists() else None

        script = (
            "set -e; "
            "echo hello-overlay > /root_newfile.txt; "      # created text
            # binary: deterministic NUL-containing bytes so it is unambiguously
            # non-text (random bytes occasionally lack a NUL and would fold as text).
            "mkdir -p /newdir && printf 'BIN\\x00\\x01\\x02\\xff\\xfe' > /newdir/blob.bin; "
            "ln -s /target /newlink; "                      # symlink
            f"rm -f /{victim}; "                            # delete a host file (overlay)
            "true")
        r = subprocess.run(
            [PYBIN, SARUN, "--", "bash", "-c", script],
            env=e, capture_output=True, text=True, timeout=120)
        check(r.returncode == 0, f"slopbox run exited 0 (got {r.returncode}: {r.stderr.strip()[-300:]})")
        check("UI connected" in r.stderr, "runner reports the UI connected")

        # host victim untouched
        host_after = host_victim.read_bytes() if host_victim.exists() else None
        check(host_after == host_before and host_after is not None,
              "host file the box 'deleted' is untouched on the real host")

        # the UI consolidates on unregister into the ONE sqlar (text+binary+symlink+
        # tombstone all in it); poll for it rather than racing a fixed sleep.
        state = Path(e["XDG_STATE_HOME"]) / "slopbox"
        deadline = time.time() + 30
        while time.time() < deadline:
            if list(state.glob("*.sqlar")):
                break
            time.sleep(0.3)
        patches = list(state.glob("*.patch.xz"))
        sqlars = list(state.glob("*.sqlar"))
        check(len(patches) == 0, f"no patch.xz at rest (got {len(patches)})")
        check(len(sqlars) == 1, f"exactly one db file produced (got {len(sqlars)})")

        if sqlars:
            sp = sqlars[0]
            names = {n for n, _md, _mt, _sz in m.sqlar_list(sp)}
            check("root_newfile.txt" in names and
                  m.sqlar_content(sp, "root_newfile.txt") == b"hello-overlay\n",
                  "created text file captured (content filled) in the one db")
            check("newdir/blob.bin" in names, "binary file captured in the one db")
            check("newlink" in names, "symlink captured in the one db")
            check(victim in names and stat_mod.S_ISCHR(m.sqlar_mode(sp, victim) or 0),
                  "host-file deletion captured as a tombstone in the one db")
            # provenance present, in the SAME db
            conn = sqlite3.connect(str(sp))
            try:
                prov = conn.execute(
                    "SELECT path,pid,exe,argv FROM provenance").fetchall()
                tables = {r[0] for r in conn.execute(
                    "SELECT name FROM sqlite_master WHERE type='table'")}
            except sqlite3.Error:
                prov = []; tables = set()
            finally:
                conn.close()
            check(len(prov) >= 1, "provenance table populated in the one db")
            if prov:
                check(all(int(p[1]) > 0 for p in prov), "every provenance row has a pid")
            check({"sqlar","provenance","process","env"} <= tables,
                  "the one db carries all concept tables (sqlar/provenance/process/env)")

        # backing live/<sid> is gone (consolidated + cleaned). Poll: the unregister
        # handler removes the backing right AFTER writing the stores, so the stores
        # can momentarily exist before the dir is gone.
        live = Path(e["XDG_STATE_HOME"]) / "slopbox" / "live"
        deadline = time.time() + 15
        leftover = ["x"]
        while time.time() < deadline:
            leftover = [d for d in (live.iterdir() if live.exists() else [])
                        if (d/"up").is_dir()]
            if not leftover: break
            time.sleep(0.3)
        check(not leftover, "live/ is empty at rest after teardown")

        # the mount is still up (UI alive) but the synthetic root is empty again
        ls = subprocess.run(["timeout","10","ls",str(mnt)], capture_output=True, text=True)
        check(ls.returncode == 0 and ls.stdout.strip() == "",
              "overlay synthetic root empty again after the box exits")

        # single-instance: a second `slopbox` (UI) must refuse while one is running.
        r2 = subprocess.run([PYBIN, SARUN], env=e, capture_output=True, text=True,
                            timeout=20)
        check(r2.returncode != 0 and "already running" in r2.stderr.lower(),
              "a second UI instance refuses to start")

        # on-demand patch over the control socket: the finished box stayed selected,
        # so `slopbox patch` returns its patch (the created text file shows up).
        rp = subprocess.run([PYBIN, SARUN, "patch"], env=e, capture_output=True,
                            timeout=20)
        check(rp.returncode == 0, "slopbox patch exits 0 against the running UI")
        check(b"root_newfile.txt" in rp.stdout and b"hello-overlay" in rp.stdout,
              "slopbox patch prints the selected box's unified patch to stdout")
    finally:
        # shut the UI down
        try:
            ui.send_signal(signal.SIGINT)
            ui.wait(timeout=10)
        except Exception:
            try: ui.kill(); ui.wait(timeout=5)
            except Exception: pass
        # the mount must be gone now
        time.sleep(0.5)
        still = os.path.ismount(str(mnt))
        if still:
            try:
                subprocess.run(["fusermount3","-uz",str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=10)
            except Exception: pass
        check(not still, "overlay unmounted after the UI exits (clean mount table)")


def run_nested_e2e(tmp):
    """Launch the UI + a parent box; from inside the parent box run ./sarun -- cmd
    (the nested invocation) and assert:
    - A CHILD session registers with parent set (proving the nested-launch path).
    - The nested command can read a file written by the parent box through the
      child root (proving read-chaining through the child overlay).
    The child's bwrap is rooted by binding the parent-exposed synthetic path
    /<KIDS_DIR>/<child> as its root — the nested-launch mechanism tested here.
    (This case runs in ambient-caps mode; the cap-less userns variant is asserted
    by run_forced_userns_e2e.)"""
    m = SourceFileLoader("slopbox", SARUN).load_module()
    if not m._have_ambient_caps():
        print("  SKIP nested-e2e: this ambient-caps variant needs CAP_SYS_ADMIN/"
              "CAP_NET_ADMIN; the cap-less userns variant runs in run_forced_userns_e2e.")
        return
    e = env_for(tmp)
    sock = str(Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock")
    mnt = Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "mnt"

    harness = tmp / "ui_harness2.py"
    harness.write_text(
        "import os\n"
        "from importlib.machinery import SourceFileLoader\n"
        f"m = SourceFileLoader('slopbox', {SARUN!r}).load_module()\n"
        "m.ensure_dirs()\n"
        "app = m._make_ui_app()()\n"
        "app.run(headless=True)\n")
    ui = subprocess.Popen([PYBIN, str(harness)], env=e,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        if not wait_socket(sock, 30):
            out = b""
            try: out = ui.stdout.read(4000) if ui.stdout else b""
            except Exception: pass
            raise RuntimeError(f"Nested-e2e: UI socket never appeared. UI output:\n"
                               f"{out.decode(errors='replace')}")

        # The parent box script:
        #  1. Writes a sentinel file via the overlay (proves parent overlay is live).
        #  2. Launches a child box via `python SARUN -- cmd` (the NESTED LAUNCH).
        #     Inside the child, we read the sentinel — if the child overlay correctly
        #     chains reads through the parent, the sentinel is visible there too.
        #  3. The child writes its own file so we can confirm the child ran.
        #
        # We pass the XDG env into the nested invocation explicitly so the nested
        # runner finds the same UI socket as the parent runner.
        # The nested child prints a UNIQUE marker to stdout (captured); we assert that
        # marker (a) chains all the way up to the TOP-LEVEL runner's stdout, (b) is
        # recorded in the CHILD box's outputs (recorded once at the origin), and (c) is
        # NOT in the PARENT box's outputs (the mute signal stops the parent re-recording
        # the echoed child bytes when they travel up through the parent's sinks).
        marker = "NESTED-ECHO-MARKER-4b9f1e"
        nested_cmd = (
            f"XDG_STATE_HOME={e['XDG_STATE_HOME']!r} "
            f"XDG_RUNTIME_DIR={e['XDG_RUNTIME_DIR']!r} "
            f"{PYBIN} {SARUN} -- "
            "bash -c 'cat /parent_sentinel.txt > /child_proof.txt && "
            f"echo nested-ok >> /child_proof.txt && echo {marker}'"
        )
        parent_script = (
            "set -e; "
            "echo parent-was-here > /parent_sentinel.txt; "
            + nested_cmd
        )
        r = subprocess.run(
            [PYBIN, SARUN, "--", "bash", "-c", parent_script],
            env=e, capture_output=True, text=True, timeout=120)
        stderr = r.stderr
        check(r.returncode == 0,
              f"nested-e2e: parent+nested box run exited 0 "
              f"(got {r.returncode}: {stderr.strip()[-400:]})")
        check("UI connected" in stderr,
              "nested-e2e: parent runner reports UI connected")
        # (a) the nested child's marker chained up through the parent box to the
        #     top-level runner's OWN stdout (ECHO frames at every level).
        check(marker in r.stdout,
              f"nested-e2e: nested child's marker reached the top-level runner's stdout "
              f"(echo chains up; stdout tail={r.stdout.strip()[-200:]!r})")

        # The UI's sqlar output will include TWO sessions (parent + child) after both
        # finish. Poll for two *.sqlar files.
        state = Path(e["XDG_STATE_HOME"]) / "slopbox"
        deadline = time.time() + 30
        while time.time() < deadline:
            if len(list(state.glob("*.sqlar"))) >= 2:
                break
            time.sleep(0.3)
        sqlars = list(state.glob("*.sqlar"))
        check(len(sqlars) >= 2,
              f"nested-e2e: at least 2 sqlar files produced (got {len(sqlars)})")

        # One sqlar must contain parent_sentinel.txt; another must contain child_proof.txt.
        all_names: set = set()
        for sp in sqlars:
            try:
                all_names |= {n for n, _md, _mt, _sz in m.sqlar_list(sp)}
            except Exception:
                pass
        check("parent_sentinel.txt" in all_names,
              "nested-e2e: parent sentinel captured in a sqlar")
        check("child_proof.txt" in all_names,
              "nested-e2e: child proof file captured in a sqlar (child ran)")

        # Look for the child_proof content — it should start with "parent-was-here"
        # proving the child read through the parent overlay.
        child_content = b""
        for sp in sqlars:
            try:
                c = m.sqlar_content(sp, "child_proof.txt")
                if c:
                    child_content = c; break
            except Exception:
                pass
        check(b"parent-was-here" in child_content,
              f"nested-e2e: child_proof contains parent sentinel content "
              f"(got {child_content!r})")
        check(b"nested-ok" in child_content,
              f"nested-e2e: child_proof contains child's own line "
              f"(got {child_content!r})")

        # (b)+(c) MUTE correctness. Identify THIS run's parent/child sqlars by content
        # (the state dir may also hold leftover boxes from earlier suites in the same
        # tmp): the CHILD wrote child_proof.txt; the PARENT wrote parent_sentinel.txt.
        def _has_file(sp, name):
            try:
                return name in {n for n, _md, _mt, _sz in m.sqlar_list(sp)}
            except Exception:
                return False
        def _outputs_blob(sp):
            rows = m.outputs_list(sp)
            return b"\n".join(bytes((m.outputs_get(sp, r["id"]) or {}).get("content")
                                    or b"") for r in rows)
        child_sp = next((sp for sp in sqlars if _has_file(sp, "child_proof.txt")), None)
        parent_sp = next((sp for sp in sqlars
                          if _has_file(sp, "parent_sentinel.txt")
                          and not _has_file(sp, "child_proof.txt")), None)
        check(child_sp is not None and parent_sp is not None,
              "nested-e2e: identified both the child (child_proof.txt) and parent "
              "(parent_sentinel.txt) sqlar of THIS run")
        if child_sp is not None:
            child_out = _outputs_blob(child_sp)
            check(marker.encode() in child_out,
                  f"nested-e2e: (b) the marker IS recorded in the CHILD box's outputs "
                  f"(recorded once at origin)")
        if parent_sp is not None:
            parent_out = _outputs_blob(parent_sp)
            check(marker.encode() not in parent_out,
                  f"nested-e2e: (c) the marker is NOT in the PARENT box's outputs "
                  f"(mute stops the parent re-recording the echoed child bytes)")
            # The parent's outputs DO still hold the nested runner's own cmd_run
            # diagnostics ("slopbox: N ...") — a different pid, recorded normally.
            check(b"slopbox:" in parent_out,
                  f"nested-e2e: (c') the parent's outputs still hold the nested runner's "
                  f"own diagnostics (a different, un-muted pid)")

        # Verify stderr mentions TWO slopbox registrations (parent + child).
        check(stderr.count("overlay root:") >= 2,
              f"nested-e2e: stderr shows >=2 overlay roots "
              f"(got {stderr.count('overlay root:')})")

    finally:
        try:
            ui.send_signal(signal.SIGINT)
            ui.wait(timeout=10)
        except Exception:
            try: ui.kill(); ui.wait(timeout=5)
            except Exception: pass
        time.sleep(0.5)
        still = os.path.ismount(str(mnt))
        if still:
            try:
                subprocess.run(["fusermount3", "-uz", str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=10)
            except Exception: pass
        check(not still, "nested-e2e: overlay unmounted after UI exits")


def run_named_box_e2e(tmp):
    """Launch the UI + a named box (MYBOX) via `slopbox MYBOX -- cmd`.

    Verifies (box_id-identity model):
    - The box's sqlar is <box_id>.sqlar; the NAME 'MYBOX' is a meta label.
    - A dotted child MYBOX.CHILD (parent = MYBOX finished box) registers with the
      correct parent pointer (parent_box_id = MYBOX's box_id).
    """
    m = SourceFileLoader("slopbox", SARUN).load_module()
    if not m._have_ambient_caps():
        print("  SKIP named-e2e: dotted child boxes nest, which needs ambient caps; "
              "the unprivileged userns path hosts single boxes only (see TODO).")
        return
    e = env_for(tmp)
    sock = str(Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock")
    mnt = Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "mnt"
    state = Path(e["XDG_STATE_HOME"]) / "slopbox"

    harness = tmp / "ui_harness3.py"
    harness.write_text(
        "import os\n"
        "from importlib.machinery import SourceFileLoader\n"
        f"m = SourceFileLoader('slopbox', {SARUN!r}).load_module()\n"
        "m.ensure_dirs()\n"
        "app = m._make_ui_app()()\n"
        "app.run(headless=True)\n")
    ui = subprocess.Popen([PYBIN, str(harness)], env=e,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        if not wait_socket(sock, 30):
            out = b""
            try: out = ui.stdout.read(4000) if ui.stdout else b""
            except Exception: pass
            raise RuntimeError(f"Named-e2e: UI socket never appeared. UI output:\n"
                               f"{out.decode(errors='replace')}")

        # Run a named box MYBOX.
        r = subprocess.run(
            [PYBIN, SARUN, "MYBOX", "--", "bash", "-c",
             "echo named-box-ok > /named_proof.txt"],
            env=e, capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"named-e2e: MYBOX run exited 0 (got {r.returncode}: {r.stderr.strip()[-200:]})")
        check("UI connected" in r.stderr, "named-e2e: MYBOX runner reports UI connected")

        def find_by_name(nm):
            """The <box_id>.sqlar whose 'name' meta == nm, else None (box_id identity)."""
            for p in state.glob("*.sqlar"):
                if m.sqlar_meta_get(p, "name") == nm:
                    return p
            return None

        # Poll for the MYBOX box (located by NAME meta, not by filename).
        deadline = time.time() + 30
        while time.time() < deadline:
            if find_by_name("MYBOX") is not None:
                break
            time.sleep(0.3)
        sp = find_by_name("MYBOX")
        check(sp is not None, "named-e2e: a <box_id>.sqlar with name='MYBOX' produced")

        if sp is not None:
            names = {n for n, _md, _mt, _sz in m.sqlar_list(sp)}
            check("named_proof.txt" in names,
                  "named-e2e: MYBOX sqlar contains named_proof.txt")
            check(m.BOX_ID_RE.match(sp.stem) is not None,
                  f"named-e2e: MYBOX sqlar is named by box_id (got {sp.stem!r})")
            mybox_id = sp.stem

        # Run MYBOX.CHILD (dotted display path; parent resolves to MYBOX by name).
        r2 = subprocess.run(
            [PYBIN, SARUN, "MYBOX.CHILD", "--", "bash", "-c",
             "echo child-ok > /child_named_proof.txt"],
            env=e, capture_output=True, text=True, timeout=60)
        check(r2.returncode == 0,
              f"named-e2e: MYBOX.CHILD run exited 0 (got {r2.returncode}: {r2.stderr.strip()[-200:]})")

        deadline = time.time() + 30
        while time.time() < deadline:
            if find_by_name("CHILD") is not None:
                break
            time.sleep(0.3)
        sp_c = find_by_name("CHILD")
        check(sp_c is not None, "named-e2e: a <box_id>.sqlar with name='CHILD' produced")

        if sp_c is not None:
            names_c = {n for n, _md, _mt, _sz in m.sqlar_list(sp_c)}
            check("child_named_proof.txt" in names_c,
                  "named-e2e: MYBOX.CHILD sqlar contains child_named_proof.txt")
            parent_box_id = m.sqlar_meta_get(sp_c, "parent_box_id")
            check(parent_box_id == mybox_id,
                  f"named-e2e: CHILD parent_box_id is MYBOX's box_id "
                  f"(got {parent_box_id!r}, want {mybox_id!r})")

    finally:
        try:
            ui.send_signal(signal.SIGINT)
            ui.wait(timeout=10)
        except Exception:
            try: ui.kill(); ui.wait(timeout=5)
            except Exception: pass
        time.sleep(0.5)
        still = os.path.ismount(str(mnt))
        if still:
            try:
                subprocess.run(["fusermount3", "-uz", str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=10)
            except Exception: pass
        check(not still, "named-e2e: overlay unmounted after UI exits")


def run_forced_userns_e2e(tmp):
    """Exercise the unprivileged-user-namespace runner path (SLOPBOX_FORCE_USERNS=1):
    a single box launch + overlay capture must work through `bwrap --unshare-user`
    with no ambient caps. Hosting a NESTED box from inside a userns box is a KNOWN
    LIMITATION (the cap-less box can't graft the child overlay) and is NOT asserted
    here — see the TODO section in sarun.

    Skipped (not failed) if _userns_runner_works() is False in this environment."""
    m = SourceFileLoader("slopbox", SARUN).load_module()
    if not m._userns_runner_works():
        print("  SKIP forced-userns e2e: unprivileged user namespaces unavailable here")
        return
    e = env_for(tmp)
    e["SLOPBOX_FORCE_USERNS"] = "1"
    sock = str(Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock")
    mnt = Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "mnt"

    harness = tmp / "ui_harness_userns.py"
    harness.write_text(
        "import os\n"
        "from importlib.machinery import SourceFileLoader\n"
        f"m = SourceFileLoader('slopbox', {SARUN!r}).load_module()\n"
        "m.ensure_dirs()\n"
        "app = m._make_ui_app()()\n"
        "app.run(headless=True)\n")
    ui = subprocess.Popen([PYBIN, str(harness)], env=e,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        if not wait_socket(sock, 30):
            out = b""
            try: out = ui.stdout.read(4000) if ui.stdout else b""
            except Exception: pass
            raise RuntimeError("forced-userns: UI socket never appeared. UI output:\n"
                               f"{out.decode(errors='replace')}")

        # Part 1: a simple box write captured through the --unshare-user path.
        r = subprocess.run(
            [PYBIN, SARUN, "--", "bash", "-c",
             "echo userns-capture > /userns_proof.txt"],
            env=e, capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"forced-userns: simple box run exited 0 "
              f"(got {r.returncode}: {r.stderr.strip()[-400:]})")
        check("UI connected" in r.stderr,
              "forced-userns: runner reports UI connected (userns path)")

        # Part 2: confirm the simple box's write was captured through the userns path.
        state = Path(e["XDG_STATE_HOME"]) / "slopbox"
        deadline = time.time() + 30
        all_names: set = set()
        while time.time() < deadline:
            all_names = set()
            for sp in state.glob("*.sqlar"):
                try:
                    all_names |= {n for n, _md, _mt, _sz in m.sqlar_list(sp)}
                except Exception:
                    pass
            if "userns_proof.txt" in all_names:
                break
            time.sleep(0.3)
        check("userns_proof.txt" in all_names,
              "forced-userns: simple-box write captured (userns path)")

        # Part 3: a NESTED box under the userns path. A cap-less userns box hosts a
        # child by binding the parent-exposed synthetic /<KIDS_DIR>/<child> as the
        # child's root, inside the child's OWN --unshare-user namespace (caps for
        # free) — no ambient privilege, no move_mount. Assert the child runs, reads
        # the parent's overlay (read-chaining), and its write is captured.
        #
        # This requires creating a userns FROM INSIDE a cap-dropped userns. That is
        # the normal case for a real unprivileged user, but it FAILS for uid 0:
        # writing the inner userns's uid_map needs CAP_SETUID in the parent userns,
        # and the single-uid self-map exception does not apply to a cap-dropped root.
        # So when this suite runs AS ROOT (forcing the userns path artificially), the
        # nested case can't be exercised here — probe for it and NOTE-skip if absent.
        # (Validated manually as a real non-root user; see commit message / TODO.)
        probe = subprocess.run(
            ["bwrap", "--unshare-user", "--cap-drop", "ALL", "--dev-bind", "/", "/",
             "--", "bwrap", "--unshare-user", "--dev-bind", "/", "/", "--",
             "true"],
            capture_output=True, timeout=30)
        if probe.returncode != 0:
            print("  NOTE forced-userns: nested-in-userns not exercisable in this "
                  "context (creating a userns inside a cap-dropped userns failed — "
                  "expected as root; works for a real unprivileged user). Skipping "
                  "the nested assertions.")
            return
        nested_cmd = (
            f"XDG_STATE_HOME={e['XDG_STATE_HOME']!r} "
            f"XDG_RUNTIME_DIR={e['XDG_RUNTIME_DIR']!r} "
            f"SLOPBOX_FORCE_USERNS=1 "
            f"{PYBIN} {SARUN} -- "
            "bash -c 'cat /userns_parent.txt > /userns_child.txt && "
            "echo userns-nested-ok >> /userns_child.txt'"
        )
        parent_script = ("set -e; echo userns-parent-here > /userns_parent.txt; "
                         + nested_cmd)
        rn = subprocess.run(
            [PYBIN, SARUN, "--", "bash", "-c", parent_script],
            env=e, capture_output=True, text=True, timeout=120)
        check(rn.returncode == 0,
              f"forced-userns: nested (parent+child) run exited 0 "
              f"(got {rn.returncode}: {rn.stderr.strip()[-500:]})")
        check(rn.stderr.count("overlay root:") >= 2,
              f"forced-userns: stderr shows >=2 overlay roots (parent+child nested) "
              f"(got {rn.stderr.count('overlay root:')})")
        # Poll for the child's captured proof across all sqlars.
        child = b""
        deadline = time.time() + 30
        while time.time() < deadline:
            for sp in state.glob("*.sqlar"):
                try:
                    c = m.sqlar_content(sp, "userns_child.txt")
                    if c: child = c; break
                except Exception: pass
            if child: break
            time.sleep(0.3)
        check(b"userns-parent-here" in child,
              f"forced-userns: nested child read parent overlay (read-chaining) "
              f"(got {child!r})")
        check(b"userns-nested-ok" in child,
              "forced-userns: nested child's own write captured (cap-less nested host)")
    finally:
        try:
            ui.send_signal(signal.SIGINT)
            ui.wait(timeout=10)
        except Exception:
            try: ui.kill(); ui.wait(timeout=5)
            except Exception: pass
        time.sleep(0.5)
        still = os.path.ismount(str(mnt))
        if still:
            try:
                subprocess.run(["fusermount3", "-uz", str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=10)
            except Exception: pass
        check(not still, "forced-userns: overlay unmounted after UI exits")


def _launch_ui(tmp, e):
    """Start the headless UI; return (proc, sock, mnt) once its control socket is up."""
    sock = str(Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock")
    mnt = Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "mnt"
    harness = tmp / "ui_harness.py"
    harness.write_text(
        "from importlib.machinery import SourceFileLoader\n"
        f"m = SourceFileLoader('slopbox', {SARUN!r}).load_module()\n"
        "m.ensure_dirs()\n"
        "app = m._make_ui_app()()\n"
        "app.run(headless=True)\n")
    ui = subprocess.Popen([PYBIN, str(harness)], env=e,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if not wait_socket(sock, 30):
        out = b""
        try: out = ui.stdout.read(4000) if ui.stdout else b""
        except Exception: pass
        raise RuntimeError(f"UI socket never appeared. UI output:\n{out.decode(errors='replace')}")
    return ui, sock, mnt


def _kill_ui(ui, mnt):
    try:
        ui.send_signal(signal.SIGINT); ui.wait(timeout=10)
    except Exception:
        try: ui.kill(); ui.wait(timeout=5)
        except Exception: pass
    time.sleep(0.5)
    if os.path.ismount(str(mnt)):
        try: subprocess.run(["fusermount3", "-uz", str(mnt)],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
        except Exception: pass


def _box_sqlar(e, timeout=20):
    """Poll for the one consolidated <box_id>.sqlar the UI writes after the box exits."""
    state = Path(e["XDG_STATE_HOME"]) / "slopbox"
    deadline = time.time() + timeout
    while time.time() < deadline:
        sqlars = list(state.glob("*.sqlar"))
        if sqlars: return sqlars[0]
        time.sleep(0.3)
    return None


def run_capture_e2e(tmp):
    """Default stdout/stderr capture. With no -t the child's stdout/stderr are redirected
    through FUSE capture-sink files: every write is recorded in the `outputs` table —
    per-writer, attributed by the patched pyfuse3's write-pid — AND echoed live back to
    the runner's real terminal. Verify (a) the runner SEES the output live (echo works),
    (b) each write lands in `outputs` on the right stream, and (c) writes from DIFFERENT
    processes are attributed to DIFFERENT process rows (the point of the per-write pid)."""
    import sqlite3
    m = SourceFileLoader("slopbox", SARUN).load_module()
    e = env_for(tmp)
    ui, sock, mnt = _launch_ui(tmp, e)
    try:
        mo, me = "OUT-MARKER-7f3a", "ERR-MARKER-9c2b"
        # outer bash writes mo to stdout (builtin → outer pid); a SEPARATE child bash
        # writes me to stderr (its own pid) → two distinct writers.
        script = f"echo {mo}; bash -c 'echo {me} 1>&2'; true"
        r = subprocess.run([PYBIN, SARUN, "--", "bash", "-c", script],   # no -t → capture
                           env=e, capture_output=True, text=True, timeout=90)
        check(r.returncode == 0,
              f"capture-e2e: box run exited 0 (got {r.returncode}: {r.stderr.strip()[-200:]})")
        # (a) live echo: the box output is replayed onto the runner's own stdout/stderr
        check(mo in r.stdout, "capture-e2e: child stdout is echoed live to the runner")
        check(me in r.stderr or me in r.stdout,
              "capture-e2e: child stderr is echoed live to the runner")
        # (b)+(c) the outputs table
        sp = _box_sqlar(e)
        rows, procs = [], []
        if sp:
            c = sqlite3.connect(str(sp))
            try:
                rows = c.execute("SELECT stream, process_id, content FROM outputs").fetchall()
                procs = c.execute("SELECT id, exe FROM process").fetchall()
            except sqlite3.Error: rows = []
            finally: c.close()
        def _has(marker, stream):
            return any(s == stream and marker.encode() in bytes(content or b"")
                       for s, _pid, content in rows)
        check(_has(mo, 0), "capture-e2e: stdout write recorded in outputs on stream=0")
        check(_has(me, 1), "capture-e2e: stderr write recorded in outputs on stream=1")
        pids = {pid for _s, pid, _c in rows}
        check(len(pids) >= 2,
              f"capture-e2e: writes from different processes get different process_id "
              f"(got {sorted(pids)})")
        check(pids and pids <= {p[0] for p in procs},
              "capture-e2e: every outputs.process_id resolves to a process row (provenance)")
    finally:
        _kill_ui(ui, mnt)


def run_engine_e2e(tmp):
    """Headless engine mode (`slopbox engine`): no UI process at all. A box runs
    against it, its writes are captured, patch/discard work over the socket, a
    second instance is refused, and SIGTERM tears down cleanly (socket gone,
    overlay unmounted)."""
    e = env_for(tmp)
    sock = str(Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock")
    mnt = Path(e["XDG_RUNTIME_DIR"]) / "slopbox" / "mnt"
    eng = subprocess.Popen([PYBIN, SARUN, "engine"], env=e,
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        if not wait_socket(sock, 30):
            out = b""
            try: out = eng.stdout.read(4000) if eng.stdout else b""
            except Exception: pass
            raise RuntimeError("engine socket never appeared. Output:\n"
                               + out.decode(errors="replace"))

        r = subprocess.run([PYBIN, SARUN, "engine"], env=e,
                           capture_output=True, text=True, timeout=30)
        check(r.returncode != 0 and "already running" in r.stderr,
              "engine-e2e: a second engine instance is refused")

        r = subprocess.run(
            [PYBIN, SARUN, "ENGBOX", "--",
             "sh", "-c", "echo engine-proof > /root/e2e_engine_proof.txt"],
            env=e, capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"engine-e2e: box run against headless engine exited 0 "
              f"(got {r.returncode}: {r.stderr[-300:]})")
        check(not Path("/root/e2e_engine_proof.txt").exists(),
              "engine-e2e: host untouched (write captured in the overlay)")

        r = subprocess.run([PYBIN, SARUN, "ENGBOX", "patch"], env=e,
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0 and "engine-proof" in r.stdout,
              "engine-e2e: `patch` over the socket prints the captured change")

        r = subprocess.run([PYBIN, SARUN, "ENGBOX", "discard"], env=e,
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0 and "removed" in r.stdout,
              "engine-e2e: `discard` consumes the box")

        eng.send_signal(signal.SIGTERM)
        try: eng.wait(timeout=30)
        except subprocess.TimeoutExpired:
            eng.kill(); eng.wait(timeout=10)
        check(eng.returncode == 0, "engine-e2e: SIGTERM exits 0")
        check(not Path(sock).exists() or not wait_socket(sock, 1),
              "engine-e2e: control socket gone after shutdown")
        with open("/proc/mounts") as f:
            check(str(mnt) not in f.read(),
                  "engine-e2e: overlay unmounted after engine exit")
    finally:
        if eng.poll() is None:
            eng.kill()
            try: eng.wait(timeout=10)
            except Exception: pass


def main():
    tmp = Path(tempfile.mkdtemp(prefix="e2e-"))
    try:
        print("== no-UI fail-fast ==")
        test_no_ui_fails_fast(tmp)
        print("\n== `--` required ==")
        test_dash_dash_required(tmp)
        print("\n== end-to-end with the UI ==")
        run_with_ui(tmp)
        print("\n== nested-box e2e (LAUNCH mechanism) ==")
        run_nested_e2e(tmp)
        print("\n== named-box e2e (dotted scoped names) ==")
        run_named_box_e2e(tmp)
        print("\n== forced-userns e2e (unprivileged --unshare-user runner) ==")
        run_forced_userns_e2e(Path(tempfile.mkdtemp(prefix="e2e-userns-")))
        print("\n== stdout/stderr capture e2e ==")
        run_capture_e2e(Path(tempfile.mkdtemp(prefix="e2e-cap-")))
        print("\n== headless engine e2e (`slopbox engine`, no UI) ==")
        run_engine_e2e(Path(tempfile.mkdtemp(prefix="e2e-eng-")))
    except Exception as ex:
        import traceback; traceback.print_exc(); _fails.append(str(ex))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("E2E PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


if __name__ == "__main__":
    sys.exit(main())
