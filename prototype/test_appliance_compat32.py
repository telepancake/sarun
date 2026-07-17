#!/usr/bin/env python3
"""Boot every available appliance and execute its native ELF32 ABI probe."""

import os
import shutil
import socket
import subprocess
import tempfile
import time
from pathlib import Path

from sarun_test_paths import ENGINE_BIN, HOST_ARCH, REPO_ROOT


ARCHITECTURES = {
    "aarch64": ("armv7-linux-musleabihf", "compat32-arm.S"),
    "x86_64": ("i386-linux-musl", "compat32-x86.S"),
}


def wait_socket(path, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(os.fspath(path))
                return True
        except OSError:
            time.sleep(0.1)
    return False


def appliance_root():
    configured = os.environ.get("SARUN_APPLIANCE_ROOT")
    if configured:
        return Path(configured)
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
    return cache / "sarun/appliances/v1"


def compiler():
    for name in ("clang-21", "clang"):
        found = shutil.which(name)
        if found:
            return found
    raise RuntimeError("ELF32 appliance probes require clang-21 (or clang)")


def build_probe(architecture, output):
    target, source = ARCHITECTURES[architecture]
    command = [
        compiler(), f"--target={target}", "-nostdlib", "-static",
        "-fuse-ld=lld", "-Wl,--build-id=none",
    ]
    if architecture == "x86_64":
        command.append("-Wl,-m,elf_i386")
    command.extend([
        "-o", str(output),
        str(REPO_ROOT / "engine/appliance" / source),
    ])
    subprocess.run(command, check=True)
    output.chmod(0o755)


def available(architecture):
    root = appliance_root()
    return all(path.is_file() for path in (
        root / architecture / "kernel",
        root / architecture / "init",
        root / f"host-{HOST_ARCH}" / f"qemu-system-{architecture}",
    ))


def host_supports_compat32(architecture):
    if architecture == "x86_64":
        return True
    try:
        result = subprocess.run(
            ["lscpu"], env={**os.environ, "LC_ALL": "C"},
            capture_output=True, text=True, check=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return False
    return any(line.startswith("CPU op-mode(s):") and "32-bit" in line
               for line in result.stdout.splitlines())


def run_architecture(architecture):
    root = Path(tempfile.mkdtemp(prefix=f"sarun-compat32-{architecture}-",
                                dir="/var/tmp"))
    work = root / "lower"
    work.mkdir()
    probe = work / "compat32-probe"
    build_probe(architecture, probe)
    original = probe.read_bytes()
    env = dict(os.environ)
    for key, name in (
        ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
    ):
        directory = root / name
        directory.mkdir()
        env[key] = str(directory)
    env["SLOPBOX_NS"] = f"COMPAT32{architecture.upper().replace('_', '')}"
    socket_path = root / "run" / f"slopbox.{env['SLOPBOX_NS']}" / "ui.sock"
    engine = subprocess.Popen(
        [str(ENGINE_BIN), "serve"], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
    )
    try:
        if not wait_socket(socket_path):
            raise RuntimeError("engine socket never appeared")
        result = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", "--qemu", architecture,
             f"COMPAT32-{architecture}", "-C", str(work), "--", "./compat32-probe"],
            env=env, capture_output=True, text=True, timeout=240,
        )
        expected_accelerator = (
            "kvm" if architecture == HOST_ARCH
            and os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
        )
        if expected_accelerator == "kvm" and not host_supports_compat32(architecture):
            expected_accelerator = "tcg"
        if (architecture == HOST_ARCH
                and os.environ.get("SARUN_REQUIRE_KVM") == "1"
                and expected_accelerator != "kvm"):
            raise RuntimeError(
                f"KVM required for {architecture}, but its complete 64/32-bit "
                "process ABI is unavailable"
            )
        marker = f"qemu {architecture} accelerator {expected_accelerator}"
        # The appliance console carries kernel boot/shutdown messages as well
        # as process stdio.  The probe's complete line and exit status are the
        # ABI contract.
        if result.returncode != 32 or "compat32-ok" not in result.stdout.splitlines():
            raise RuntimeError(
                f"ELF32 process failed: rc={result.returncode}; "
                f"stdout={result.stdout!r}; stderr={result.stderr[-800:]!r}"
            )
        if marker not in result.stderr:
            raise RuntimeError(
                f"expected {marker!r}; stderr={result.stderr[-800:]!r}"
            )
        if probe.read_bytes() != original:
            raise RuntimeError("guest changed its lower ELF32 executable")
        print(f"  ok  qemu {architecture}: ELF32 process, exit 32, "
              f"{expected_accelerator}")
    finally:
        engine.terminate()
        try:
            engine.wait(timeout=10)
        except subprocess.TimeoutExpired:
            engine.kill()
            engine.wait(timeout=5)
        shutil.rmtree(root, ignore_errors=True)


def main():
    if not ENGINE_BIN.is_file():
        raise RuntimeError(f"missing engine {ENGINE_BIN}; run `make engine`")
    tested = 0
    for architecture in ARCHITECTURES:
        if available(architecture):
            run_architecture(architecture)
            tested += 1
        else:
            print(f"  (qemu {architecture} appliance unavailable — skipped)")
    if not tested:
        raise RuntimeError("no appliance pair is available; run `make appliances`")
    print("APPLIANCE-COMPAT32 PASS")
    return 0


def test_appliance_compat32():
    assert main() == 0


if __name__ == "__main__":
    raise SystemExit(main())
