"""Architecture-neutral paths shared by the real-engine test suite."""

import os
import platform
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent

_MACHINE_ALIASES = {
    "amd64": "x86_64",
    "arm64": "aarch64",
}
HOST_ARCH = _MACHINE_ALIASES.get(platform.machine().lower(), platform.machine().lower())
ENGINE_TARGET = os.environ.get(
    "SARUN_ENGINE_TARGET",
    f"{HOST_ARCH}-unknown-linux-musl",
)
ENGINE_BIN = REPO_ROOT / "engine" / "target" / ENGINE_TARGET / "release" / "sarun"
LIBTESTSARUN = REPO_ROOT / "prototype" / "libtestsarun.py"
