#!/usr/bin/env python3
"""Portable live equivalence workload for Sarun's filesystem transports.

The workload uses a caller-writable `/var/tmp` lower tree and disables
networking, so backend results are not confused with root or Tap availability.
On aarch64 it exercises live FUSE and the paired aarch64 QEMU appliance. Native
SUD is intentionally not claimed here because its x86 Syscall User Dispatch
wrapper cannot run on an aarch64 kernel.
"""

import os
import http.server
import signal
import shutil
import socket
import sqlite3
import stat
import subprocess
import tempfile
import threading
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

BRUSH_WORKLOAD = r'''
set -eu
mkdir brush-build
cd brush-build
printf 'all: kati-out\nkati-out:\n\tprintf KATI > kati-out\n' > Makefile
make -j2
[ "$(cat kati-out)" = KATI ]
printf 'rule emit\n  command = printf NINJA > $out\nbuild ninja-out: emit\n' > build.ninja
ninja
[ "$(cat ninja-out)" = NINJA ]
printf BRUSH > brush-result
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


def backend_selector(backend):
    if backend == "fuse":
        return ["--fuse"]
    if backend == "sud":
        return ["--sud"]
    if backend == "qemu":
        return ["--qemu", HOST_ARCH]
    raise ValueError(backend)


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
        command = [str(ENGINE_BIN), "run", "--net", "off", *backend_selector(backend)]
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
        brush_command = [
            str(ENGINE_BIN), "run", "--net", "off", *backend_selector(backend),
            "-b", f"EQUIV-{backend}-brush", "-C", str(work), "--",
            "sh", "-c", BRUSH_WORKLOAD,
        ]
        brush_result = subprocess.run(
            brush_command, env=env, capture_output=True, text=True, timeout=240
        )
        if brush_result.returncode != 0:
            raise RuntimeError(
                f"{backend} brush: rc={brush_result.returncode}; "
                f"stdout={brush_result.stdout[-300:]!r}; "
                f"stderr={brush_result.stderr[-500:]!r}"
            )
        brush_sqlar = max(store.glob("*.sqlar"), key=lambda path: int(path.stem))
        brush_prefix = str(work).lstrip("/") + "/brush-build/"
        observation["brush_result"] = module.sqlar_content(
            brush_sqlar, brush_prefix + "brush-result"
        )
        observation["kati_result"] = module.sqlar_content(
            brush_sqlar, brush_prefix + "kati-out"
        )
        observation["ninja_result"] = module.sqlar_content(
            brush_sqlar, brush_prefix + "ninja-out"
        )
        observation["brush_host_unchanged"] = not (work / "brush-build").exists()
        abort_command = [
            str(ENGINE_BIN), "run", "--net", "off", *backend_selector(backend),
            f"EQUIV-{backend}-abort", "-C", str(work), "--", "sh", "-c",
            "printf READY > forced-ready; sleep 60",
        ]
        abort = subprocess.Popen(
            abort_command,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        time.sleep(1)
        was_running = abort.poll() is None
        try:
            os.killpg(abort.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            abort.wait(timeout=15)
            time.sleep(0.2)
            try:
                os.killpg(abort.pid, 0)
                group_gone = False
            except ProcessLookupError:
                group_gone = True
        except subprocess.TimeoutExpired:
            group_gone = False
        observation["forced_shutdown"] = was_running and group_gone
        if not group_gone:
            try:
                os.killpg(abort.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                abort.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        probe = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", *backend_selector(backend),
             f"EQUIV-{backend}-after-abort", "--", "sh", "-c", "exit 0"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=120,
        )
        observation["forced_shutdown"] = (
            observation["forced_shutdown"] and probe.returncode == 0
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


def run_cross_arch_brush(architecture):
    root = Path(tempfile.mkdtemp(
        prefix=f"sarun-equiv-qemu-{architecture}-brush-", dir="/var/tmp"
    ))
    work = root / "lower"
    work.mkdir()
    env = dict(os.environ)
    for key, name in (
        ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
    ):
        directory = root / name
        directory.mkdir()
        env[key] = str(directory)
    env["SLOPBOX_NS"] = f"EQUIVQEMU{architecture.upper().replace('_', '')}"
    old = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    module = SourceFileLoader(
        f"slopbox_qemu_{architecture}_brush", str(LIBTESTSARUN)
    ).load_module()
    module.ensure_dirs()
    engine = subprocess.Popen(
        [str(ENGINE_BIN), "serve"], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_socket(module.sock_path()):
            raise RuntimeError("engine socket never appeared")
        result = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", "--qemu", architecture,
             "-b", f"EQUIV-qemu-{architecture}-brush", "-C", str(work), "--",
             "sh", "-c", BRUSH_WORKLOAD],
            env=env, capture_output=True, text=True, timeout=240,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"rc={result.returncode}; stdout={result.stdout[-300:]!r}; "
                f"stderr={result.stderr[-500:]!r}"
            )
        store = Path(env["XDG_STATE_HOME"]) / f"slopbox.{env['SLOPBOX_NS']}"
        sqlar = max(store.glob("*.sqlar"), key=lambda path: int(path.stem))
        prefix = str(work).lstrip("/") + "/brush-build/"
        brush_result = module.sqlar_content(sqlar, prefix + "brush-result")
        kati_result = module.sqlar_content(sqlar, prefix + "kati-out")
        ninja_result = module.sqlar_content(sqlar, prefix + "ninja-out")
        nonzero = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", "--qemu", architecture,
             "-b", f"LIFE-{architecture}-NONZERO", "-C", str(work), "--",
             "sh", "-c", "exit 37"],
            env=env, capture_output=True, timeout=240,
        )
        missing = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", "--qemu", architecture,
             f"LIFE-{architecture}-MISSING", "-C", str(work), "--",
             "/definitely/missing-sarun-command"],
            env=env, capture_output=True, timeout=240,
        )
        rerun_name = f"LIFE-{architecture}-RERUN"
        rerun_first = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", "--qemu", architecture,
             "-b", rerun_name, "-C", str(work), "--", "sh", "-c",
             "printf first > cross-rerun-first"],
            env=env, capture_output=True, timeout=240,
        )
        rerun_second = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", "--qemu", architecture,
             "-b", rerun_name, "-C", str(work), "--", "sh", "-c",
             "test \"$(cat cross-rerun-first)\" = first; printf second > cross-rerun-second"],
            env=env, capture_output=True, timeout=240,
        )
        rerun_archives = [
            archive for archive in store.glob("*.sqlar")
            if module.sqlar_meta_get(archive, "name") == rerun_name
        ]
        rerun_archive = rerun_archives[0] if len(rerun_archives) == 1 else None
        work_prefix = str(work).lstrip("/") + "/"
        return {
            "brush": brush_result,
            "kati": kati_result,
            "ninja": ninja_result,
            "nonzero": nonzero.returncode,
            "missing": missing.returncode,
            "rerun_exits": (rerun_first.returncode, rerun_second.returncode),
            "rerun_unique": len(rerun_archives) == 1,
            "rerun_first": None if rerun_archive is None else module.sqlar_content(
                rerun_archive, work_prefix + "cross-rerun-first"
            ),
            "rerun_second": None if rerun_archive is None else module.sqlar_content(
                rerun_archive, work_prefix + "cross-rerun-second"
            ),
            "host_unchanged": not any(work.iterdir()),
        }
    finally:
        engine.terminate()
        try:
            engine.wait(timeout=10)
        except subprocess.TimeoutExpired:
            engine.kill()
            engine.wait(timeout=5)
        os.environ.clear()
        os.environ.update(old)
        shutil.rmtree(root, ignore_errors=True)


def run_nested_qemu():
    """A FUSE child launches QEMU through its authenticated broker channel."""
    root = Path(tempfile.mkdtemp(prefix="sarun-equiv-nested-qemu-", dir="/var/tmp"))
    work = root / "lower"
    work.mkdir()
    env = dict(os.environ)
    for key, name in (
        ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
    ):
        directory = root / name
        directory.mkdir()
        env[key] = str(directory)
    env["SLOPBOX_NS"] = "EQUIVNESTEDQEMU"
    old = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    module = SourceFileLoader(
        "slopbox_nested_qemu", str(LIBTESTSARUN)
    ).load_module()
    module.ensure_dirs()
    engine = subprocess.Popen(
        [str(ENGINE_BIN), "serve"], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_socket(module.sock_path()):
            raise RuntimeError("engine socket never appeared")
        nested = (
            '"$SARUN_EXE" run --net off --qemu '
            f'{HOST_ARCH} CHILD -C "$PWD" -- sh -c '
            "'printf NESTED-QEMU > nested-result; printf nested-qemu-ok'"
        )
        result = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", "--fuse", "OUTER",
             "-C", str(work), "--", "sh", "-c", nested],
            env=env, capture_output=True, timeout=240,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"rc={result.returncode}; stdout={result.stdout[-300:]!r}; "
                f"stderr={result.stderr[-500:]!r}"
            )
        store = Path(env["XDG_STATE_HOME"]) / f"slopbox.{env['SLOPBOX_NS']}"
        archives = sorted(store.glob("*.sqlar"), key=lambda path: int(path.stem))
        if len(archives) != 2:
            raise RuntimeError(f"expected parent and child archives, got {archives}")
        parent, child = archives
        child_result = module.sqlar_content(
            child, str(work / "nested-result").lstrip("/")
        )
        return {
            "exit_output": b"nested-qemu-ok" in result.stdout,
            "parent_name": module.sqlar_meta_get(parent, "name"),
            "child_name": module.sqlar_meta_get(child, "name"),
            "child_parent": module.sqlar_meta_get(child, "parent_box_id"),
            "parent_id": parent.stem,
            "child_result": child_result,
            "host_unchanged": not (work / "nested-result").exists(),
        }
    finally:
        engine.terminate()
        try:
            engine.wait(timeout=10)
        except subprocess.TimeoutExpired:
            engine.kill()
            engine.wait(timeout=5)
        os.environ.clear()
        os.environ.update(old)
        shutil.rmtree(root, ignore_errors=True)


def run_qemu_inside_qemu():
    """Guest requests launch flat sibling QEMUs through their host runner."""
    root = Path(tempfile.mkdtemp(prefix="sarun-equiv-qemu-flat-nested-", dir="/var/tmp"))
    work = root / "lower"
    work.mkdir()
    env = dict(os.environ)
    for key, name in (
        ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
    ):
        directory = root / name
        directory.mkdir()
        env[key] = str(directory)
    env["SLOPBOX_NS"] = "EQUIVQEMUFLATNESTED"
    old = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    module = SourceFileLoader(
        "slopbox_qemu_flat_nested", str(LIBTESTSARUN)
    ).load_module()
    module.ensure_dirs()
    engine = subprocess.Popen(
        [str(ENGINE_BIN), "serve"], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_socket(module.sock_path()):
            raise RuntimeError("engine socket never appeared")
        nested = (
            "printf 'NESTED-INPUT\\n' | "
            '"$SARUN_EXE" run --net off --qemu '
            f'{HOST_ARCH} INPUT -C "$PWD" -- sh -c '
            "'IFS= read -r value; test \"$value\" = NESTED-INPUT; "
            "printf %s \"$value\" > flat-nested-input; printf flat-input-ok'; "
            '"$SARUN_EXE" run --net off --qemu '
            f'{HOST_ARCH} SIGNAL -C "$PWD" -- sh -c '
            "'printf flat-signal-ready; exec sleep 60' & nested=$!; "
            'sleep 5; kill -TERM "$nested"; wait "$nested"; code=$?; '
            "printf 'flat-signal-%s' \"$code\"; test \"$code\" -eq 143; "
            '"$SARUN_EXE" run --net off --qemu '
            f'{HOST_ARCH} ORPHAN -C "$PWD" -- sh -c '
            "'exec sleep 60' & sleep 5; printf flat-outer-exit"
        )
        result = subprocess.run(
            [str(ENGINE_BIN), "run", "--net", "off", "--qemu", HOST_ARCH,
             "OUTER", "-C", str(work), "--", "sh", "-c", nested],
            env=env, capture_output=True, timeout=480,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"rc={result.returncode}; stdout={result.stdout[-600:]!r}; "
                f"stderr={result.stderr[-800:]!r}"
            )
        store = Path(env["XDG_STATE_HOME"]) / f"slopbox.{env['SLOPBOX_NS']}"
        archives = sorted(store.glob("*.sqlar"), key=lambda path: int(path.stem))
        if len(archives) != 4:
            raise RuntimeError(
                f"expected outer and three inner archives, got {archives}"
            )
        by_name = {
            module.sqlar_meta_get(archive, "name"): archive
            for archive in archives
        }
        outer = by_name["OUTER"]
        input_box = by_name["INPUT"]
        signal_box = by_name["SIGNAL"]
        orphan_box = by_name["ORPHAN"]
        return {
            "input_output": b"flat-input-ok" in result.stdout,
            "signal_output": b"flat-signal-ready" in result.stdout,
            "signal_result": b"flat-signal-143" in result.stdout,
            "outer_exit": b"flat-outer-exit" in result.stdout,
            "outer_name": module.sqlar_meta_get(outer, "name"),
            "input_name": module.sqlar_meta_get(input_box, "name"),
            "signal_name": module.sqlar_meta_get(signal_box, "name"),
            "input_parent": module.sqlar_meta_get(input_box, "parent_box_id"),
            "signal_parent": module.sqlar_meta_get(signal_box, "parent_box_id"),
            "orphan_parent": module.sqlar_meta_get(orphan_box, "parent_box_id"),
            "outer_id": outer.stem,
            "input_result": module.sqlar_content(
                input_box, str(work / "flat-nested-input").lstrip("/")
            ),
            "host_unchanged": not (work / "flat-nested-input").exists(),
        }
    finally:
        engine.terminate()
        try:
            engine.wait(timeout=10)
        except subprocess.TimeoutExpired:
            engine.kill()
            engine.wait(timeout=5)
        os.environ.clear()
        os.environ.update(old)
        shutil.rmtree(root, ignore_errors=True)


def run_qemu_lifecycle():
    root = Path(tempfile.mkdtemp(prefix="sarun-equiv-qemu-life-", dir="/var/tmp"))
    work = root / "lower"
    work.mkdir()
    env = dict(os.environ)
    for key, name in (
        ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
    ):
        directory = root / name
        directory.mkdir()
        env[key] = str(directory)
    env["SLOPBOX_NS"] = "EQUIVQEMULIFE"
    env["APPLIANCE_MARK"] = "descriptor-control"
    old = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    module = SourceFileLoader(
        "slopbox_qemu_lifecycle", str(LIBTESTSARUN)
    ).load_module()
    module.ensure_dirs()
    class LocalHandler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            body = b"QEMU-HOST-NETWORK"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    local_server = http.server.ThreadingHTTPServer(("0.0.0.0", 0), LocalHandler)
    local_server_thread = threading.Thread(target=local_server.serve_forever, daemon=True)
    local_server_thread.start()
    local_port = local_server.server_address[1]
    engine = subprocess.Popen(
        [str(ENGINE_BIN), "serve"], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )

    def run_case(name, command, net="off"):
        return subprocess.run(
            [str(ENGINE_BIN), "run", "--net", net, "--qemu", HOST_ARCH,
             name, "-C", str(work), "--", *command],
            env=env, capture_output=True, timeout=240,
        )

    try:
        if not wait_socket(module.sock_path()):
            raise RuntimeError("engine socket never appeared")
        nonzero = run_case("LIFE-NONZERO", ["sh", "-c", "exit 37"])
        signalled = run_case("LIFE-SIGNAL", ["sh", "-c", "kill -TERM $$"])
        missing = run_case("LIFE-MISSING", ["/definitely/missing-sarun-command"])
        environment = run_case(
            "LIFE-ENV",
            ["sh", "-c", "printf '%s:%s' \"$APPLIANCE_MARK\" \"$PWD\" > appliance-env"],
        )
        network_off = run_case(
            "LIFE-NET-OFF", ["sh", "-c", "test ! -e /sys/class/net/eth0"], "off"
        )
        network_host = run_case(
            "LIFE-NET-HOST",
            ["sh", "-c", "test -e /sys/class/net/eth0; "
             f"curl -fsS --max-time 10 http://10.0.2.2:{local_port}/ > host-http"],
            "host",
        )
        network_tap = run_case(
            "LIFE-NET-TAP",
            ["sh", "-c", "test -e /sys/class/net/eth0; getent hosts lifecycle.invalid > tap-dns"],
            "tap",
        )
        rerun_first = run_case(
            "LIFE-RERUN", ["sh", "-c", "printf first > rerun-first"]
        )
        rerun_second = run_case(
            "LIFE-RERUN",
            ["sh", "-c", "test \"$(cat rerun-first)\" = first; printf second > rerun-second"],
        )
        store = Path(env["XDG_STATE_HOME"]) / f"slopbox.{env['SLOPBOX_NS']}"
        archives = list(store.glob("*.sqlar"))
        named = {
            module.sqlar_meta_get(archive, "name"): archive for archive in archives
        }
        rerun_archives = [
            archive for archive in archives
            if module.sqlar_meta_get(archive, "name") == "LIFE-RERUN"
        ]
        rerun_archive = rerun_archives[0] if len(rerun_archives) == 1 else None
        prefix = str(work).lstrip("/") + "/"
        return {
            "nonzero": nonzero.returncode,
            "signalled": signalled.returncode,
            "missing": missing.returncode,
            "environment_exit": environment.returncode,
            "environment": module.sqlar_content(
                named["LIFE-ENV"], prefix + "appliance-env"
            ),
            "expected_environment": f"descriptor-control:{work}".encode(),
            "network": (
                network_off.returncode,
                network_host.returncode,
                network_tap.returncode,
            ),
            "tap_dns": module.sqlar_content(
                named["LIFE-NET-TAP"], prefix + "tap-dns"
            ),
            "host_http": module.sqlar_content(
                named["LIFE-NET-HOST"], prefix + "host-http"
            ),
            "rerun_exits": (rerun_first.returncode, rerun_second.returncode),
            "rerun_unique": len(rerun_archives) == 1,
            "rerun_first": None if rerun_archive is None else module.sqlar_content(
                rerun_archive, prefix + "rerun-first"
            ),
            "rerun_second": None if rerun_archive is None else module.sqlar_content(
                rerun_archive, prefix + "rerun-second"
            ),
            "host_unchanged": not any(work.iterdir()),
        }
    finally:
        local_server.shutdown()
        local_server.server_close()
        local_server_thread.join(timeout=5)
        engine.terminate()
        try:
            engine.wait(timeout=10)
        except subprocess.TimeoutExpired:
            engine.kill()
            engine.wait(timeout=5)
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
        check(value["brush_result"] == b"BRUSH",
              f"{backend}: parser-driven brush command ran")
        check(value["kati_result"] == b"KATI",
              f"{backend}: projected make ran embedded Kati")
        check(value["ninja_result"] == b"NINJA",
              f"{backend}: projected ninja ran embedded n2")
        check(value["brush_host_unchanged"],
              f"{backend}: brush/build writes did not escape to host")
        check(value["forced_shutdown"],
              f"{backend}: forced box termination is reaped and transport reusable")

    if len(observations) > 1:
        reference = observations[backends[0]]
        for backend in backends[1:]:
            check(observations[backend] == reference,
                  f"equiv: {backend} observations equal {backends[0]}")

    if "qemu" in backends:
        try:
            nested = run_nested_qemu()
        except Exception as error:
            check(False, f"nested qemu: FUSE parent launched appliance ({error})")
        else:
            check(nested["exit_output"],
                  "nested qemu: appliance exit/output returned through parent")
            check(nested["parent_name"] == "OUTER" and nested["child_name"] == "CHILD",
                  "nested qemu: relative names retained")
            check(nested["child_parent"] == nested["parent_id"],
                  "nested qemu: broker-authenticated parent recorded")
            check(nested["child_result"] == b"NESTED-QEMU",
                  "nested qemu: child write captured in child archive")
            check(nested["host_unchanged"],
                  "nested qemu: child write did not escape to host")
        try:
            flat_nested = run_qemu_inside_qemu()
        except Exception as error:
            check(False, f"qemu inside qemu: flat sibling launch completed ({error})")
        else:
            check(flat_nested["input_output"],
                  "qemu inside qemu: stdin and output crossed operation stream")
            check(flat_nested["signal_output"] and flat_nested["signal_result"],
                  "qemu inside qemu: caller signal returned exact status 143")
            check(flat_nested["outer_exit"],
                  "qemu inside qemu: outer result waited for flat child teardown")
            check(flat_nested["outer_name"] == "OUTER"
                  and flat_nested["input_name"] == "INPUT"
                  and flat_nested["signal_name"] == "SIGNAL",
                  "qemu inside qemu: relative names retained")
            check(flat_nested["input_parent"] == flat_nested["outer_id"]
                  and flat_nested["signal_parent"] == flat_nested["outer_id"]
                  and flat_nested["orphan_parent"] == flat_nested["outer_id"],
                  "qemu inside qemu: authenticated logical parent recorded")
            check(flat_nested["input_result"] == b"NESTED-INPUT",
                  "qemu inside qemu: relayed input reached child capture")
            check(flat_nested["host_unchanged"],
                  "qemu inside qemu: sibling write did not escape to host")
        try:
            lifecycle = run_qemu_lifecycle()
        except Exception as error:
            check(False, f"qemu lifecycle: suite completed ({error})")
        else:
            check(lifecycle["nonzero"] == 37,
                  "qemu lifecycle: exact nonzero exit returned")
            check(lifecycle["signalled"] == 128 + signal.SIGTERM,
                  "qemu lifecycle: child signal returned as shell status")
            check(lifecycle["missing"] == 127,
                  "qemu lifecycle: exec failure returned as versioned status 127")
            check(lifecycle["environment_exit"] == 0
                  and lifecycle["environment"] == lifecycle["expected_environment"],
                  "qemu lifecycle: environment and cwd crossed binary control")
            check(lifecycle["network"] == (0, 0, 0)
                  and lifecycle["host_http"] == b"QEMU-HOST-NETWORK"
                  and b"lifecycle.invalid" in lifecycle["tap_dns"],
                  "qemu lifecycle: off/host/tap network paths work")
            check(lifecycle["rerun_exits"] == (0, 0)
                  and lifecycle["rerun_unique"]
                  and lifecycle["rerun_first"] == b"first"
                  and lifecycle["rerun_second"] == b"second",
                  "qemu lifecycle: same-name rerun reuses captured state")
            check(lifecycle["host_unchanged"],
                  "qemu lifecycle: no captured write escaped to host")

    other_architecture = "x86_64" if HOST_ARCH == "aarch64" else "aarch64"
    other_kernel = appliance_root / other_architecture / "kernel"
    other_init = appliance_root / other_architecture / "init"
    other_qemu = appliance_root / f"host-{HOST_ARCH}" / (
        f"qemu-system-{other_architecture}"
    )
    if other_kernel.exists() and other_init.exists() and other_qemu.exists():
        try:
            cross = run_cross_arch_brush(other_architecture)
        except Exception as error:
            check(False, f"qemu {other_architecture}: cross-architecture brush ({error})")
        else:
            check(cross["brush"] == b"BRUSH",
                  f"qemu {other_architecture}: parser-driven brush command ran")
            check(cross["kati"] == b"KATI",
                  f"qemu {other_architecture}: target-architecture Kati ran")
            check(cross["ninja"] == b"NINJA",
                  f"qemu {other_architecture}: target-architecture n2 ran")
            check(cross["nonzero"] == 37 and cross["missing"] == 127,
                  f"qemu {other_architecture}: lifecycle exit statuses are exact")
            check(cross["rerun_exits"] == (0, 0)
                  and cross["rerun_unique"]
                  and cross["rerun_first"] == b"first"
                  and cross["rerun_second"] == b"second",
                  f"qemu {other_architecture}: immediate stateful rerun works")
            check(cross["host_unchanged"],
                  f"qemu {other_architecture}: no cross-architecture host escape")
    else:
        print(f"  (paired {other_architecture} appliance unavailable — cross-arch brush skipped)")

    print("\n" + ("BACKEND-EQUIV PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_backend_equiv_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    raise SystemExit(main())
