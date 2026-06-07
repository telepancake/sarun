#!/usr/bin/env python3
"""End-to-end: start the real UI process, run a real `slopbox CMD` against it, and
verify (a) the box writes/modifies/deletes through the UI-served overlay, (b) the UI
consolidates on exit into patch/sqlar/tombstones + provenance, (c) the mount table
is clean afterwards, and (d) `slopbox CMD` with NO UI fails fast.

    /home/user/venv/bin/python test_e2e.py

Self-safety: isolated XDG temp tree; the UI is launched headless and killed in a
finally; the overlay is lazy-unmounted on the way out.
"""
import os, sys, time, signal, socket, subprocess, tempfile, shutil, sqlite3, json
import stat as stat_mod
from pathlib import Path
from importlib.machinery import SourceFileLoader

PYBIN = "/home/user/venv/bin/python"
SARUN = "/home/user/sarun/sarun"

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
            [PYBIN, SARUN, "-n", "--", "bash", "-c", script],
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
            check({"sqlar","provenance","process","env","flows"} <= tables,
                  "the one db carries all concept tables (sqlar/provenance/process/env/flows)")

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
      bind-fd'd child root (proving read-chaining through the child overlay).
    The child's bwrap is rooted via --bind-fd instead of --bind, which is the
    whole point of the mechanism tested here."""
    m = SourceFileLoader("slopbox", SARUN).load_module()
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
        nested_cmd = (
            f"XDG_STATE_HOME={e['XDG_STATE_HOME']!r} "
            f"XDG_RUNTIME_DIR={e['XDG_RUNTIME_DIR']!r} "
            f"{PYBIN} {SARUN} -- "
            "bash -c 'cat /parent_sentinel.txt > /child_proof.txt && "
            "echo nested-ok >> /child_proof.txt'"
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
    box launch + overlay capture must work through `bwrap --unshare-user`, and a
    NESTED box (a box that launches ./sarun inside it) must also register and
    capture — proving nesting works via the userns path.

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
            [PYBIN, SARUN, "-n", "--", "bash", "-c",
             "echo userns-capture > /userns_proof.txt"],
            env=e, capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"forced-userns: simple box run exited 0 "
              f"(got {r.returncode}: {r.stderr.strip()[-400:]})")
        check("UI connected" in r.stderr,
              "forced-userns: runner reports UI connected (userns path)")

        # Part 2: a NESTED box where the OUTER box runs through the userns path.
        # The outer box's `--unshare-user` namespace synthesizes CAP_SYS_ADMIN /
        # CAP_NET_ADMIN, so INSIDE the outer box those caps are *ambient* — the
        # inner ./sarun then takes the ordinary ambient path and nests normally.
        # This proves nesting works via the userns path: the outer userns is what
        # makes the inner box launchable. (We unset SLOPBOX_FORCE_USERNS for the
        # inner call so it uses the now-ambient caps rather than trying to create
        # a second userns inside the first — double-userns nesting is the later
        # follow-on, not v1.)
        nested_cmd = (
            "unset SLOPBOX_FORCE_USERNS; "
            f"XDG_STATE_HOME={e['XDG_STATE_HOME']!r} "
            f"XDG_RUNTIME_DIR={e['XDG_RUNTIME_DIR']!r} "
            f"{PYBIN} {SARUN} -- "
            "bash -c 'echo nested-userns-ok > /nested_userns_proof.txt'"
        )
        parent_script = (
            "set -e; "
            "echo parent-userns > /parent_userns_proof.txt; "
            + nested_cmd
        )
        r2 = subprocess.run(
            [PYBIN, SARUN, "--", "bash", "-c", parent_script],
            env=e, capture_output=True, text=True, timeout=120)
        check(r2.returncode == 0,
              f"forced-userns: nested box run exited 0 "
              f"(got {r2.returncode}: {r2.stderr.strip()[-400:]})")
        check(r2.stderr.count("overlay root:") >= 2,
              f"forced-userns: nested run shows >=2 overlay roots "
              f"(got {r2.stderr.count('overlay root:')})")

        # Poll for all three captured files across the produced sqlars.
        state = Path(e["XDG_STATE_HOME"]) / "slopbox"
        deadline = time.time() + 30
        wanted = {"userns_proof.txt", "parent_userns_proof.txt",
                  "nested_userns_proof.txt"}
        all_names: set = set()
        while time.time() < deadline:
            all_names = set()
            for sp in state.glob("*.sqlar"):
                try:
                    all_names |= {n for n, _md, _mt, _sz in m.sqlar_list(sp)}
                except Exception:
                    pass
            if wanted <= all_names:
                break
            time.sleep(0.3)
        check("userns_proof.txt" in all_names,
              "forced-userns: simple-box write captured (userns path)")
        check("parent_userns_proof.txt" in all_names,
              "forced-userns: parent box write captured")
        check("nested_userns_proof.txt" in all_names,
              "forced-userns: NESTED box write captured (nesting via userns works)")
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
    except Exception as ex:
        import traceback; traceback.print_exc(); _fails.append(str(ex))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("E2E PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


if __name__ == "__main__":
    sys.exit(main())
