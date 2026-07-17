#!/usr/bin/env python3
"""Portable live equivalence workload for Sarun's filesystem transports.

The workload uses a caller-writable `/var/tmp` lower tree and disables
networking, so backend results are not confused with root or Tap availability.
On aarch64 it exercises live FUSE and the paired aarch64 QEMU appliance. Native
SUD is intentionally not claimed here because its x86 Syscall User Dispatch
wrapper cannot run on an aarch64 kernel.
"""

import os
import shutil
import socket
import sqlite3
import stat
import subprocess
import tempfile
import time
from importlib.machinery import SourceFileLoader
from pathlib import Path

from sarun_test_paths import ENGINE_BIN, HOST_ARCH, LIBTESTSARUN, REPO_ROOT


_fails = []


def check(condition, message):
    print(("  ok  " if condition else " FAIL ") + message)
    if not condition:
        _fails.append(message)


def wait_socket(path, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(1)
                client.connect(path)
                return True
        except OSError:
            time.sleep(0.1)
    return False


WORKLOAD = r'''
set -eu
printf CHANGED > lower.txt
chmod 741 lower.txt
mkdir -p nested
printf NESTED > nested/data
ln lower.txt hardlink
ln -s nested/data symlink
mv nested/data nested/moved
rm victim.txt

# The destination fd must retain the overwritten inode while the visible name
# retains the source inode.
printf SOURCE > source
printf DEST > destination
exec 8>>destination
mv source destination
printf OLD >&8
exec 8>&-
[ "$(cat destination)" = SOURCE ]

# A lazy lower fd survives unlink without changing or resurrecting the host
# file it originally projected.
exec 9<>lower-open.txt
rm lower-open.txt
printf PRIVATE >&9
exec 9>&-
[ ! -e lower-open.txt ]

printf x > sparse
truncate -s 1048577 sparse
cat > generated-tool <<'EOF'
#!/bin/sh
printf EXECUTED > executed.txt
EOF
chmod +x generated-tool
./generated-tool

# Multiple workers publish files by rename while sharing one directory. Every
# published name must be visible and readable after the barrier.
mkdir parallel parallel-tmp
i=0
while [ "$i" -lt 8 ]; do
  (
    j=0
    while [ "$j" -lt 8 ]; do
      printf 'worker-%s-%s' "$i" "$j" > "parallel-tmp/$i-$j"
      mv "parallel-tmp/$i-$j" "parallel/f$i-$j"
      j=$((j + 1))
    done
  ) &
  i=$((i + 1))
done
wait
count=0
for file in parallel/f*; do
  cat "$file" >/dev/null
  count=$((count + 1))
done
[ "$count" -eq 64 ]
printf '%s' "$count" > parallel-count
'''


def sqlar_observation(sqlar, module, work):
    prefix = str(work).lstrip("/") + "/"
    rows = {name: mode for name, mode, *_ in module.sqlar_list(sqlar)}

    def rel(name):
        return prefix + name

    with sqlite3.connect(f"file:{sqlar}?mode=ro", uri=True) as database:
        sparse = database.execute(
            "SELECT sz FROM sqlar WHERE name=?", (rel("sparse"),)
        ).fetchone()
    return {
        "lower": module.sqlar_content(sqlar, rel("lower.txt")),
        "lower_mode": stat.S_IMODE(rows.get(rel("lower.txt"), 0)),
        "hardlink": module.sqlar_content(sqlar, rel("hardlink")),
        "moved": module.sqlar_content(sqlar, rel("nested/moved")),
        "destination": module.sqlar_content(sqlar, rel("destination")),
        "executed": module.sqlar_content(sqlar, rel("executed.txt")),
        "parallel_count": module.sqlar_content(sqlar, rel("parallel-count")),
        "victim_tombstone": stat.S_ISCHR(rows.get(rel("victim.txt"), 0)),
        "sparse_size": sparse[0] if sparse else None,
        "source_absent": rel("source") not in rows,
        "lower_open_tombstone": stat.S_ISCHR(rows.get(rel("lower-open.txt"), 0)),
    }


def run_backend(backend):
    root = Path(tempfile.mkdtemp(prefix=f"sarun-equiv-{backend}-", dir="/var/tmp"))
    xdg = root / "xdg"
    work = root / "lower"
    work.mkdir(parents=True)
    (work / "lower.txt").write_bytes(b"ORIGINAL")
    (work / "lower-open.txt").write_bytes(b"LOWER-OPEN")
    (work / "victim.txt").write_bytes(b"VICTIM")

    env = dict(os.environ)
    for key, name in (
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
    ):
        directory = xdg / name
        directory.mkdir(parents=True)
        env[key] = str(directory)
    env["SLOPBOX_NS"] = f"EQUIV{backend.upper()}"

    old = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    module = SourceFileLoader(f"slopbox_{backend}", str(LIBTESTSARUN)).load_module()
    module.ensure_dirs()

    engine = subprocess.Popen(
        [str(ENGINE_BIN), "serve"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_socket(module.sock_path()):
            raise RuntimeError(f"{backend}: engine socket never appeared")
        command = [str(ENGINE_BIN), "run", "--net", "off"]
        if backend == "fuse":
            command.append("--fuse")
        elif backend == "sud":
            command.append("--sud")
        elif backend == "qemu":
            command.extend(["--qemu", HOST_ARCH])
        else:
            raise ValueError(backend)
        command.extend([
            f"EQUIV-{backend}", "-C", str(work), "--", "sh", "-c", WORKLOAD,
        ])
        result = subprocess.run(command, env=env, capture_output=True, text=True,
                                timeout=240)
        if result.returncode != 0:
            raise RuntimeError(
                f"{backend}: rc={result.returncode}; "
                f"stdout={result.stdout[-300:]!r}; stderr={result.stderr[-500:]!r}"
            )
        store = Path(env["XDG_STATE_HOME"]) / f"slopbox.{env['SLOPBOX_NS']}"
        sqlar = max(store.glob("*.sqlar"), key=lambda path: int(path.stem))
        observation = sqlar_observation(sqlar, module, work)
        observation["host_unchanged"] = (
            (work / "lower.txt").read_bytes() == b"ORIGINAL"
            and (work / "lower-open.txt").read_bytes() == b"LOWER-OPEN"
            and (work / "victim.txt").read_bytes() == b"VICTIM"
            and not (work / "executed.txt").exists()
        )
        return observation
    finally:
        engine.terminate()
        try:
            engine.wait(timeout=10)
        except subprocess.TimeoutExpired:
            engine.kill()
        os.environ.clear()
        os.environ.update(old)
        shutil.rmtree(root, ignore_errors=True)


def main():
    if not ENGINE_BIN.exists():
        print(f"backend-equiv: no {ENGINE_BIN} — SKIP")
        return 0
    backends = ["fuse"]
    if HOST_ARCH == "x86_64" and (REPO_ROOT / "tv/sud64").exists():
        backends.append("sud")
    elif HOST_ARCH != "x86_64":
        print("  (native SUD needs an x86_64 Syscall User Dispatch kernel)")
    appliance_root = Path.home() / ".cache/sarun/appliances/v1"
    appliance = appliance_root / HOST_ARCH / "kernel"
    qemu = appliance_root / f"host-{HOST_ARCH}" / f"qemu-system-{HOST_ARCH}"
    if appliance.exists() and qemu.exists():
        backends.append("qemu")
    else:
        print(f"  (paired {HOST_ARCH} QEMU appliance unavailable — QEMU leg skipped)")

    observations = {}
    for backend in backends:
        try:
            observations[backend] = run_backend(backend)
        except Exception as error:
            check(False, f"{backend}: workload completed ({error})")
            continue
        value = observations[backend]
        check(value["lower"] == b"CHANGED", f"{backend}: lower file copied up")
        check(value["lower_mode"] == 0o741, f"{backend}: mode captured")
        check(value["hardlink"] == b"CHANGED", f"{backend}: hardlink content")
        check(value["moved"] == b"NESTED", f"{backend}: nested rename")
        check(value["destination"] == b"SOURCE", f"{backend}: rename-over fd lifetime")
        check(value["executed"] == b"EXECUTED", f"{backend}: new executable ran")
        check(value["parallel_count"] == b"64",
              f"{backend}: concurrent publish/read barrier")
        check(value["victim_tombstone"], f"{backend}: lower deletion is a tombstone")
        check(value["lower_open_tombstone"], f"{backend}: open lower unlink is a tombstone")
        check(value["sparse_size"] == 1048577, f"{backend}: sparse length captured")
        check(value["source_absent"], f"{backend}: rename source absent")
        check(value["host_unchanged"], f"{backend}: no write escaped to host")

    if len(observations) > 1:
        reference = observations[backends[0]]
        for backend in backends[1:]:
            check(observations[backend] == reference,
                  f"equiv: {backend} observations equal {backends[0]}")

    print("\n" + ("BACKEND-EQUIV PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_backend_equiv_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    raise SystemExit(main())
