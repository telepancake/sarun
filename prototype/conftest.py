"""pytest glue for this repo's test style.

The test files use a non-raising `check(cond, msg)` helper that appends failures
to a module-level `_fails` list; each file's `__main__` block then does
`sys.exit(1 if _fails else 0)`. That makes them self-enforcing when run
standalone (`python test_X.py`) — but under pytest, `check()` never raises, so a
function with failing checks still returns normally and is reported as PASSED.
The suite then gives false green (a real failing assertion in
test_terminated_parent_reads sat hidden behind "23 passed").

This wrapper closes that gap: for any test module that exposes a `_fails` list,
it snapshots the list around each test's call phase and, if `check()` recorded
new failures during it (and the test didn't already raise), turns the test into
a clean FAILED with those messages. Standalone runs don't load conftest, so
their existing `_fails`/`sys.exit` path is unaffected; modules that don't use the
pattern are ignored.
"""
import os
import subprocess
from pathlib import Path
from sarun_test_paths import ENGINE_BIN

import pytest


# ── Process-env hygiene ──────────────────────────────────────────────────────
# Most `test_*_rs.py` modules point the box at a per-test temp tree by assigning
# `os.environ["XDG_STATE_HOME"] = …` (and XDG_RUNTIME_DIR / XDG_CONFIG_HOME /
# XDG_DATA_HOME / SLOPBOX_NS) directly, and only ever `pop("SLOPBOX_NS")` in
# their teardown — the XDG vars are never restored. That mutation is
# PROCESS-GLOBAL, so it leaks into every later test in the same worker. The
# nastiest symptom: a later test that shells out to `make engine` inherits a
# stale temp `XDG_DATA_HOME`, so `uv tool`/cargo-zigbuild resolve to the temp
# dir and recompile the whole crate graph COLD instead of reusing engine/target
# — and under `pytest -n auto` several workers do it at once, turning a
# minutes-long suite into a build-bound one.
#
# Fix it once, centrally: snapshot os.environ around every test and restore it
# afterward, so each test starts from the real environment no matter what ran
# before. (Standalone `python test_X.py` runs don't load conftest, so their own
# behavior is unchanged.)
@pytest.fixture(autouse=True)
def _restore_os_environ():
    saved = dict(os.environ)
    try:
        yield
    finally:
        os.environ.clear()
        os.environ.update(saved)


# ── Build the engine binary exactly once, serialized ─────────────────────────
# Each `*_rs` test's own `ensure_binary()` calls `make engine` on demand when
# the static musl binary is missing. Under `pytest -n auto` multiple workers hit
# that simultaneously and stampede `cargo zigbuild` into the one shared
# engine/target. Pre-build it once here, before any test body runs, behind a
# cross-process file lock: the first worker to grab the lock builds (with the
# real environment, since this runs before any test mutates it), the rest block
# then find the finished binary and skip. A pre-existing binary is a no-op.
_REPO_ROOT = Path(__file__).resolve().parent.parent
_ENGINE_BIN = ENGINE_BIN


@pytest.fixture(scope="session", autouse=True)
def _engine_binary_built():
    import fcntl
    if _ENGINE_BIN.exists():
        return
    target = _REPO_ROOT / "engine" / "target"
    target.mkdir(parents=True, exist_ok=True)
    with open(target / ".sarun-build.lock", "w") as lockf:
        fcntl.flock(lockf, fcntl.LOCK_EX)
        try:
            if not _ENGINE_BIN.exists():
                subprocess.run(["make", "engine"], cwd=_REPO_ROOT, check=False)
        finally:
            fcntl.flock(lockf, fcntl.LOCK_UN)


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_call(item):
    fails = getattr(getattr(item, "module", None), "_fails", None)
    before = len(fails) if isinstance(fails, list) else None
    outcome = yield
    if before is None or outcome.excinfo is not None:
        return  # not our pattern, or the test already raised — leave it be
    new = fails[before:]
    if new:
        outcome.force_exception(pytest.fail.Exception(
            "check() recorded %d failure(s):\n  - %s"
            % (len(new), "\n  - ".join(map(str, new))),
            pytrace=False))
