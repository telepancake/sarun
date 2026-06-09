#!/usr/bin/env python3
"""Guard: sarun must run on its OWN patched pyfuse3 build, never stock.

sarun's top-of-file bootstrap (_ensure_patched_pyfuse3) builds a patched
pyfuse3-3.4.2 whose FUSE write() handler surfaces the writer's RequestContext
(uid/gid/pid) — stock pyfuse3 drops it. Loading sarun (here via SourceFileLoader,
exactly as the rest of the suite does) triggers that bootstrap and a hard
assertion. This test pins the contract: the ACTIVE pyfuse3's Operations.write
carries a `ctx` parameter and the app's own write handler requires it. Any
regression to stock pyfuse3 (or a dropped patch) fails the suite loudly instead
of silently running an unpatched library.

    uv run --with pyfuse3 --with trio test_patched_pyfuse3.py   # or: pytest
"""
import inspect
import sys
from importlib.machinery import SourceFileLoader
from pathlib import Path

SARUN = str(Path(__file__).resolve().parent / "sarun")
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []


def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


def test_active_pyfuse3_is_patched():
    # The bootstrap exposes the imported pyfuse3 at module scope.
    pyfuse3 = m.pyfuse3
    sig = inspect.signature(pyfuse3.Operations.write)
    check("ctx" in sig.parameters,
          "active pyfuse3 Operations.write has a `ctx` param (patched build)")
    # The active build comes from sarun's cache, not a stock site-packages.
    cache = m._pyfuse3_cache_dir()
    check(str(cache) in getattr(pyfuse3, "__file__", ""),
          f"active pyfuse3 loaded from sarun's patched cache ({cache})")


def test_app_write_handler_requires_ctx():
    Ops = m._build_overlay_ops()
    sig = inspect.signature(Ops.write)
    check("ctx" in sig.parameters,
          "app MultiplexOverlayFs.write requires a `ctx` parameter")
    check(sig.parameters["ctx"].default is inspect.Parameter.empty,
          "the `ctx` parameter is required (no default)")


if __name__ == "__main__":
    for t in (test_active_pyfuse3_is_patched, test_app_write_handler_requires_ctx):
        try:
            t()
        except Exception:
            import traceback
            traceback.print_exc()
            _fails.append(t.__name__)
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
