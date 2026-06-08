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
import pytest


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
