#!/usr/bin/env python3
"""Drive the REAL sarun UI in a pty — the Vars (variable provenance) pane.

Not a render smoke test: a real engine, a real `run -b --vars` box, real
keystrokes into the interactive UI, and assertions against the emulated
terminal screen (pyte). Covers the flows that shipped broken when only the
data layer was tested:
  - 'v' opens Vars and auto-prompts for a query
  - results show the FULL site (make dir + Makefile:NN), the assignment as
    written, and its dereferenced variables
  - Enter focuses the detail items; j/↓ moves over them; Enter on a
    dereference re-queries that name (the chain walk)
  - Backspace restores the PREVIOUS query (not just the pane)
  - a single-word query matches values by substring
  - cross-nav ('p') from a Vars row doesn't crash and lands on Pipes

Run:
    uv run --with "pyte>=0.8" python test_ui_vars_rs.py
Skips (passes vacuously) if the engine binary is unavailable.
"""
import os, pty, re, select, shutil, subprocess, sys, tempfile, time
from pathlib import Path
from sarun_test_paths import ENGINE_BIN

_HERE = Path(__file__).resolve().parent
BIN = ENGINE_BIN

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def main():
    if not BIN.exists():
        print("test_ui_vars_rs: no engine binary — run `make engine` (skip)")
        return 0
    import pyte
    tmp = Path(tempfile.mkdtemp(prefix="uivars-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
        (tmp / sub).mkdir(parents=True, exist_ok=True)
    os.environ["SLOPBOX_NS"] = "UIV"
    work = Path("/root/uivars_work")
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True, exist_ok=True)
    (work / "Makefile").write_text(
        "ORIG_VAR := aa\n"
        "ORIG_VAR += bb\n"
        "DERIVED := pre-$(ORIG_VAR)-$(OTHER)\n"
        "OTHER := zz\n"
        "LOSER ?= default-val\n"
        "all:\n\t@echo done > out.txt\n")
    eng = subprocess.Popen([str(BIN), "serve"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    ui_pid = None
    try:
        sock = Path(os.environ["XDG_RUNTIME_DIR"]) / "slopbox.UIV/ui.sock"
        for _ in range(100):
            if sock.exists(): break
            time.sleep(0.1)
        else:
            check(False, "engine socket appeared")
            return 1
        r = subprocess.run(
            [str(BIN), "run", "-b", "--vars", "UIVARS", "-C", str(work),
             "--", "make", "LOSER=cmdline-won"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"--vars box exits 0 (got {r.returncode}: {r.stderr[-400:]})")

        ui_pid, fd = pty.fork()
        if ui_pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.execv(str(BIN), [str(BIN), "--sock", str(sock)])
        import fcntl, struct, termios
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
        vt = pyte.Screen(160, 40)
        stream = pyte.ByteStream(vt)

        def drain(t):
            end = time.time() + t
            buf = b""
            while time.time() < end:
                rd, _, _ = select.select([fd], [], [], 0.1)
                if rd:
                    try: buf += os.read(fd, 65536)
                    except OSError: break
            return buf

        stream.feed(drain(1.5))
        def send(keys, wait=0.8):
            os.write(fd, keys.encode())
            stream.feed(drain(wait))
        def screen():
            return "\n".join(vt.display)

        send("v", 1.0)
        check("variable query" in screen(),
              "'v' opens Vars with the query prompt")
        send("DERIVED"); send("\r", 1.0)
        txt = screen()
        check("DERIVED" in txt and "uivars_work/Makefile:3" in txt,
              "detail shows the FULL site (dir-joined Makefile:3)")
        check("Flags: s - simple" in txt and "x - exported" in txt,
              "the flag legend at the top is complete (wraps, not truncated)")
        check(any(re.search(r"[│║┌]\s*s\s+DERIVED", l) for l in vt.display),
              "flags are a narrow left column (Mikrotik style): 's DERIVED …'")
        check("pre-$(ORIG_VAR)-$(OTHER)" in txt,
              "detail shows the assignment as written (unexpanded rhs)")
        check("→ ORIG_VAR" in txt and "→ OTHER" in txt,
              "detail lists both dereferenced variables")
        # the value body is painted (background) so its exact extent —
        # trailing whitespace included — is visible
        val_bg = ""
        for row in range(40):
            line = "".join(vt.buffer[row][c].data for c in range(160))
            col = line.find("pre-aa bb-", 72)   # right pane only
            if col >= 0:
                val_bg = vt.buffer[row][col].bg
                break
        check(val_bg not in ("", "default", "00cdcd"),  # not selection-cyan
              f"value body is painted with its own background (bg={val_bg!r})")
        send("\r", 0.8)          # focus detail items (first = ORIG_VAR)
        send("j", 0.5)           # second item = OTHER
        send("\r", 1.0)          # follow it
        txt = screen()
        check("name-or-value~[OTHER]" in txt and "Makefile:4" in txt,
              "Enter on a dereference re-queries that name (chain walk)")
        send("\x7f", 1.0)        # Backspace
        check("name-or-value~[DERIVED]" in screen(),
              "Backspace restores the PREVIOUS query")
        send("/", 0.5)
        send("\x7f" * 30, 0.3)   # the prompt seeds the current query — clear
        send("aa"); send("\r", 1.0)
        txt = screen()
        check("ORIG_VAR" in txt and "Makefile:1" in txt,
              "a single word matches VALUES by substring ('aa' finds ORIG_VAR)")
        send("/", 0.5)
        send("\x7f" * 30, 0.3)
        send("LOSER"); send("\r", 1.0)
        txt = screen()
        check("q" in txt and "did NOT assign" in txt
              and "cmdline-won" in txt,
              "a ?= beaten by a command-line value carries the explicit NOTE")
        send("p", 1.2)
        check("Pipes" in screen(), "'p' cross-nav from Vars lands on Pipes")
        send("q", 0.5)
    finally:
        if ui_pid:
            try: os.kill(ui_pid, 15)
            except ProcessLookupError: pass
        eng.terminate()
        try: eng.wait(timeout=10)
        except Exception: eng.kill()
        shutil.rmtree(work, ignore_errors=True)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("UI-VARS-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_ui_vars_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
