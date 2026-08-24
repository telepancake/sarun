#!/usr/bin/env python3
"""Assemble Sarun's engine-only vendored dependencies.

The reusable assembler is owned by Bumba. This wrapper preserves Sarun's
historical entry point while selecting the engine project root.
"""

import os
import runpy


repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
tool = os.path.join(repo, "bumba", "tools", "vendor.py")
namespace = runpy.run_path(tool, run_name="bumba_vendor_tool")
project = os.path.join(repo, "engine")
namespace["PROJECT"] = project
namespace["VENDOR"] = os.path.join(project, "vendor")
namespace["PATCHES"] = os.path.join(project, "vendor-patches")
namespace["CACHE"] = os.path.join(project, ".vendor-cache")
raise SystemExit(namespace["main"]() or 0)
