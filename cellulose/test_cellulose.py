"""End-to-end tests for cellulose against the local fixture page.

Runs the real browser over CDP and checks the rendered grid: exact cell
placement of positioned text, justification flattening, CJK double-width,
and that the synthetic font stays small. Needs a Chromium (see
cellulose.BROWSER_CANDIDATES); no network required (file:// fixture).

Deps: websocket-client, fonttools, pillow
Run:  uv run --with websocket-client,fonttools,pillow python3 test_cellulose.py
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

failures = []


def check(cond, msg):
    status = "ok" if cond else "FAIL"
    print(f"{status}: {msg}")
    if not cond:
        failures.append(msg)


def _fails():
    return failures


def render_fixture():
    out = subprocess.run(
        [
            sys.executable,
            os.path.join(HERE, "cellulose.py"),
            "--dump-text",
            "--size",
            "80x30",
            "file://" + os.path.join(HERE, "fixture.html"),
        ],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if out.returncode != 0:
        sys.exit(f"render failed:\n{out.stderr}")
    lines = out.stdout.split("\n")
    return lines + [""] * (30 - len(lines))


def test_grid_rendering():
    lines = render_fixture()

    # fixture positions #red at left:160px top:128px -> col 20, row 8
    check(lines[8][20:27] == "REDTEXT", f"REDTEXT at row 8 col 20: {lines[8]!r}")

    # justified paragraph flattened: words at clean single-space columns
    check(lines[0].startswith("alpha beta gamma delta"), f"line 0: {lines[0]!r}")
    check("  " not in lines[0].strip(), "no double gaps from justification")

    # CJK double width: line at top:288px -> row 18, chars occupy 2 cells
    cjk = lines[18]
    check(cjk.startswith("mix "), f"cjk line: {cjk!r}")
    check("漢字" in cjk, "CJK characters present")
    # text dumps drop wide-continuation cells: "漢字 " is 3 string chars
    check(cjk.find("wide") == cjk.find("漢") + 3, "CJK text run intact")


def test_font_size():
    from cellfont import build_font_bytes, char_cells

    data = build_font_bytes()
    check(len(data) < 100_000, f"font stays small: {len(data)} bytes")
    check(char_cells("a") == 1, "narrow char = 1 cell")
    check(char_cells("漢") == 2, "CJK char = 2 cells")
    check(char_cells("\U0001f600") == 2, "emoji = 2 cells")


if __name__ == "__main__":
    test_font_size()
    test_grid_rendering()
    if failures:
        sys.exit(f"{len(failures)} failure(s)")
    print("all checks passed")
