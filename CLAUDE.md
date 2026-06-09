# Working on sarun — read this first

**STOP assuming "deps aren't installed, nothing runs here."** They install
themselves. This repo runs fine in this container. Below is exactly how.

**This entire repo was written by Claude (you) — zero human changes.** So there
is no hidden human setup, no external environment you're missing. Everything
needed to run and test is in-repo plus `uv`. If something looks like it needs a
venv or a manual install, that's a stale artifact a past session left — fix it,
don't work around it. (Example already fixed: `test_e2e.py` once hardcoded a
`/home/user/venv` that never exists here.) Trust the established patterns; don't
re-derive "can it even run" every session — it can.

## What `sarun` is
A single-file app. The file `sarun` IS the executable — its shebang is
`#!/usr/bin/env -S uv run --script` with a PEP 723 `# /// script` dependency
block. **`uv` installs every dependency automatically on first run.** You do
not pip-install anything, you do not need a venv, you do not "set up the
environment." You just run it.

## Run the app
```
./sarun -h            # or:  uv run --script sarun -h
./sarun                # starts the UI/server
./sarun -- some cmd    # runs `some cmd` in a sandbox against a running UI
```
First run also **builds a patched pyfuse3** (see section 0 of `sarun`):
downloads a pinned sdist, applies an embedded patch, compiles it into
`~/.cache/sarun/…`. That takes ~25 s ONCE (needs network + a C toolchain:
gcc, pkg-config, libfuse3 dev headers — all present here), then it is cached
and every later run is ~0.4 s. Do not be surprised by the first-run pause; it
is not a hang.

## Run the tests
Each `test_*.py` is standalone (repo `check()/_fails` + `__main__` style) AND
pytest-compatible. The dependency list each file needs is in its module
docstring. The whole suite:
```
uv run --with pytest --with pytest-timeout --with "textual>=0.60" \
  --with "mitmproxy>=11" --with "wcmatch>=8.4" --with "pyfuse3>=3.2" \
  --with "trio>=0.22" --with "python-magic>=0.4" \
  pytest -q -p no:cacheprovider --ignore=test_e2e.py
```
Expected today: **150 passed, 1 skipped**. A single file:
```
uv run --with pytest --with "pyfuse3>=3.2" --with "trio>=0.22" \
  pytest -q -p no:cacheprovider test_outputs_capture.py
```
(Loading any test imports `sarun`, which triggers the section-0 pyfuse3
bootstrap, so the first test run also pays the ~25 s build, then caches.)

These are **real** tests: FUSE actually mounts, bwrap actually runs, the
network actually works in this sandbox. The patched-pyfuse3 assertion means a
test that somehow ran on stock pyfuse3 would fail loudly — by design.

## e2e tests (`test_e2e.py`)
End-to-end: launches the real headless UI + real `sarun -- cmd` boxes (basic,
nested, named, forced-userns, synthetic-DNS interception, stdout/stderr
capture). bwrap, FUSE mounts, and outbound network ALL work in this sandbox —
so these really run here. Has a uv shebang; run it directly:
```
./test_e2e.py            # or  uv run test_e2e.py
```
Expected: `E2E PASS` (one NOTE-skip: nested-inside-userns isn't exercisable as
root). The test process runs under uv (deps for its in-process SourceFileLoader)
and the box/UI subprocesses reuse that same interpreter via `sys.executable` —
there is NO hardcoded venv. Takes a few minutes (real boxes); the DNS suite
needs outbound network (it hits example.com through the proxy).

## Branch / workflow
Develop on the branch you were told to; commit with clear messages; push only
when asked (`git push -u origin <branch>`). One clean commit per logical change.

## The one true rule
Before claiming something can't run, **run it.** `./sarun -h` and the pytest
command above both work from a clean checkout here. If a step seems to need a
venv or a pip install, you are holding it wrong — it's a uv script.
