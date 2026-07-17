#!/usr/bin/env python3
"""Build a real Linux kernel under the FUSE and QEMU execution backends.

This is deliberately larger than a syscall fixture.  Each backend configures and
builds Linux from a clean output directory with ``make -j10``, then the test reads
the result back from the box archive.  The lower directory and kernel source must
remain unchanged on the host.

The target compiler is wrapped only to count simultaneous, real clang processes;
the wrapper always invokes /usr/bin/clang-21 with the original arguments.

Run from the repository root:

    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" \
      --with "wcmatch>=8.4" --with "python-magic>=0.4" \
      python prototype/test_kernel_build_backends.py --keep
"""

import argparse
import hashlib
import json
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
from test_backend_equiv_rs import backend_selector


DEFAULT_SOURCE = Path.home() / ".cache/sarun/appliances/src/linux-6.18.38"
COMMON_CONFIG = REPO_ROOT / "engine/appliance/kernel-common.config"
ARCH_CONFIG = REPO_ROOT / "engine/appliance/kernel-aarch64.config"

CLANG_PROBE = r'''#!/bin/sh
set -u
state=${SARUN_CLANG_PROBE_STATE:?}
exec 9>"$state/lock"
flock 9
active=$(cat "$state/active")
active=$((active + 1))
printf '%s\n' "$active" > "$state/active"
maximum=$(cat "$state/maximum")
if [ "$active" -gt "$maximum" ]; then
    printf '%s\n' "$active" > "$state/maximum"
fi
flock -u 9

/usr/bin/clang-21 "$@"
status=$?

flock 9
active=$(cat "$state/active")
printf '%s\n' "$((active - 1))" > "$state/active"
flock -u 9
exit "$status"
'''


def wait_socket(path, timeout=30):
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


def tree_metadata_digest(root):
    """Hash mutation-relevant metadata without reading the 1.7 GiB source."""
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*"), key=lambda item: item.as_posix()):
        status = path.lstat()
        relative = path.relative_to(root).as_posix().encode()
        metadata = (
            stat.S_IFMT(status.st_mode), stat.S_IMODE(status.st_mode),
            status.st_size, status.st_mtime_ns,
        )
        digest.update(relative + b"\0" + repr(metadata).encode() + b"\0")
        if path.is_symlink():
            digest.update(os.readlink(path).encode() + b"\0")
    return digest.hexdigest()


def sqlar_counts(path):
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        tables = {
            row[0] for row in connection.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )
        }
        counts = {}
        for table in ("sqlar", "provenance", "process", "outputs", "meta"):
            counts[table] = (
                connection.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
                if table in tables else 0
            )
        meta = dict(connection.execute("SELECT key,value FROM meta")) if "meta" in tables else {}
        artifact_writers = connection.execute(
            "SELECT name,writer,last_writer FROM sqlar "
            "WHERE name LIKE '%/arch/arm64/boot/Image' OR name LIKE '%/vmlinux'"
        ).fetchall()
        return counts, meta, artifact_writers
    finally:
        connection.close()


def captured_output(module, sqlar):
    chunks = []
    for row in module.outputs_list(sqlar):
        detail = module.outputs_get(sqlar, row["id"])
        if detail and detail.get("content") is not None:
            chunks.append(bytes(detail["content"]))
    return b"".join(chunks)


def shell_workload(source, work, jobs):
    out = work / "out"
    probe = work / "clang-probe"
    return f'''set -eu
export KBUILD_BUILD_TIMESTAMP='2026-07-17 00:00:00 UTC'
export KBUILD_BUILD_USER=sarun
export KBUILD_BUILD_HOST=backend-validation
export KCONFIG_NOTIMESTAMP=1
export SOURCE_DATE_EPOCH=1784246400
export SARUN_CLANG_PROBE_STATE={out}/.clang-probe
make -C {source} O={out} ARCH=arm64 LLVM=-21 tinyconfig
{source}/scripts/kconfig/merge_config.sh -m -O {out} \
    {out}/.config {COMMON_CONFIG} {ARCH_CONFIG}
make -C {source} O={out} ARCH=arm64 LLVM=-21 olddefconfig
mkdir -p "$SARUN_CLANG_PROBE_STATE"
printf '0\n' > "$SARUN_CLANG_PROBE_STATE/active"
printf '0\n' > "$SARUN_CLANG_PROBE_STATE/maximum"
started=$(date +%s)
make -C {source} O={out} ARCH=arm64 LLVM=-21 CC={probe} -j{jobs} Image
ended=$(date +%s)
test -s {out}/arch/arm64/boot/Image
test -s {out}/vmlinux
objects=$(find {out} -type f -name '*.o' | wc -l)
maximum=$(cat "$SARUN_CLANG_PROBE_STATE/maximum")
printf 'SARUN_KERNEL_BUILD_DONE jobs={jobs} max_clang=%s objects=%s seconds=%s\n' \
    "$maximum" "$objects" "$((ended - started))"
printf 'jobs={jobs}\nmax_clang=%s\nobjects=%s\nseconds=%s\n' \
    "$maximum" "$objects" "$((ended - started))" > {out}/sarun-build-summary
sha256sum {out}/arch/arm64/boot/Image {out}/vmlinux
'''


def run_backend(backend, source, jobs, keep):
    root = Path(tempfile.mkdtemp(prefix=f"sarun-kernel-{backend}-", dir="/var/tmp"))
    work = root / "lower"
    work.mkdir()
    probe = work / "clang-probe"
    probe.write_text(CLANG_PROBE)
    probe.chmod(0o755)
    lower_before = tree_metadata_digest(work)
    source_before = tree_metadata_digest(source)

    env = dict(os.environ)
    for key, name in (
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
    ):
        directory = root / name
        directory.mkdir()
        env[key] = str(directory)
    env["SLOPBOX_NS"] = f"KERNEL{backend.upper()}"

    old_env = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    module = SourceFileLoader(
        f"kernel_build_slopbox_{backend}", str(LIBTESTSARUN)
    ).load_module()
    module.ensure_dirs()
    engine_log = (root / "engine.log").open("wb")
    engine = subprocess.Popen(
        [str(ENGINE_BIN), "serve"], env=env,
        stdout=engine_log, stderr=subprocess.STDOUT,
    )
    failed = True
    try:
        if not wait_socket(module.sock_path()):
            raise RuntimeError(f"{backend}: engine socket never appeared")
        command = [
            str(ENGINE_BIN), "run", "--net", "off", *backend_selector(backend),
            f"KERNEL-{backend}", "-C", str(work), "--", "sh", "-c",
            shell_workload(source, work, jobs),
        ]
        started = time.monotonic()
        result = subprocess.run(
            command, env=env, capture_output=True, timeout=7200
        )
        elapsed = time.monotonic() - started

        store = Path(env["XDG_STATE_HOME"]) / f"slopbox.{env['SLOPBOX_NS']}"
        archives = sorted(store.glob("*.sqlar"), key=lambda path: int(path.stem))
        if not archives:
            raise RuntimeError(
                f"{backend}: no box archive; rc={result.returncode}; "
                f"stderr={result.stderr[-2000:]!r}"
            )
        sqlar = archives[-1]
        rows = module.sqlar_list(sqlar)
        names = {row[0] for row in rows}
        prefix = str(work).lstrip("/") + "/out/"
        image_name = prefix + "arch/arm64/boot/Image"
        vmlinux_name = prefix + "vmlinux"
        summary_name = prefix + "sarun-build-summary"
        image = module.sqlar_content(sqlar, image_name)
        vmlinux = module.sqlar_content(sqlar, vmlinux_name)
        summary = module.sqlar_content(sqlar, summary_name)
        output = captured_output(module, sqlar)
        processes = module.process_list(sqlar)
        clang_processes = [
            row for row in processes
            if "clang-21" in Path(row[4]).name
            or any("clang-21" in Path(str(arg)).name for arg in row[5])
        ]
        object_rows = sum(name.startswith(prefix) and name.endswith(".o") for name in names)
        counts, meta, artifact_writers = sqlar_counts(sqlar)

        observation = {
            "backend": backend,
            "command_returncode": result.returncode,
            "wall_seconds": round(elapsed, 3),
            "sqlar": str(sqlar),
            "box_name": meta.get("name"),
            "archive_counts": counts,
            "archive_object_rows": object_rows,
            "process_rows": len(processes),
            "clang_process_rows": len(clang_processes),
            "output_bytes": len(output),
            "completion_marker_captured": b"SARUN_KERNEL_BUILD_DONE" in output,
            "summary": summary.decode(errors="replace") if summary else None,
            "image_sha256": hashlib.sha256(image).hexdigest() if image else None,
            "vmlinux_sha256": hashlib.sha256(vmlinux).hexdigest() if vmlinux else None,
            "image_bytes": len(image) if image else None,
            "vmlinux_bytes": len(vmlinux) if vmlinux else None,
            "artifact_writers": artifact_writers,
            "host_lower_unchanged": tree_metadata_digest(work) == lower_before,
            "host_output_absent": not (work / "out").exists(),
            "host_source_unchanged": tree_metadata_digest(source) == source_before,
            "stdout_tail": result.stdout[-4000:].decode(errors="replace"),
            "stderr_tail": result.stderr[-4000:].decode(errors="replace"),
        }
        (root / "report.json").write_text(json.dumps(observation, indent=2) + "\n")

        errors = []
        if result.returncode != 0:
            errors.append(f"runner returned {result.returncode}")
        if image is None or vmlinux is None:
            errors.append("kernel artifacts are absent from the box archive")
        if summary is None or b"jobs=" + str(jobs).encode() not in summary:
            errors.append("build summary is absent or has the wrong job count")
        if not observation["completion_marker_captured"]:
            errors.append("recorded outputs lack the completion marker")
        if object_rows < 500:
            errors.append(f"only {object_rows} object files were captured")
        if not processes or not clang_processes:
            errors.append("process trace lacks compiler processes")
        if not artifact_writers:
            errors.append("artifact writer provenance is absent")
        if not observation["host_lower_unchanged"] or not observation["host_output_absent"]:
            errors.append("a build write escaped into the host lower tree")
        if not observation["host_source_unchanged"]:
            errors.append("the host kernel source tree changed")
        if summary:
            fields = dict(
                line.split("=", 1) for line in summary.decode().splitlines() if "=" in line
            )
            if int(fields.get("max_clang", "0")) < 2:
                errors.append("fewer than two real clang processes overlapped")

        if errors:
            raise RuntimeError(f"{backend}: " + "; ".join(errors) + f"; report={root / 'report.json'}")
        failed = False
        return observation, root
    finally:
        engine.terminate()
        try:
            engine.wait(timeout=10)
        except subprocess.TimeoutExpired:
            engine.kill()
            engine.wait(timeout=5)
        engine_log.close()
        os.environ.clear()
        os.environ.update(old_env)
        if not keep and not failed:
            shutil.rmtree(root, ignore_errors=True)


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--backend", action="append", choices=("fuse", "qemu"))
    parser.add_argument("--jobs", type=int, default=10)
    parser.add_argument("--source", type=Path, default=DEFAULT_SOURCE)
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args(argv)
    backends = args.backend or ["fuse", "qemu"]

    required = (ENGINE_BIN, args.source / "Makefile", COMMON_CONFIG, ARCH_CONFIG)
    missing_paths = [str(path) for path in required if not path.exists()]
    missing_tools = [
        tool for tool in ("clang-21", "flock", "make") if shutil.which(tool) is None
    ]
    if missing_paths or missing_tools:
        print("missing paths: " + ", ".join(missing_paths))
        print("missing tools: " + ", ".join(missing_tools))
        return 2
    if HOST_ARCH != "aarch64":
        print(f"this fixture currently builds the paired arm64 kernel, not {HOST_ARCH}")
        return 2

    observations = []
    for backend in backends:
        print(f"[{backend}] Linux kernel make -j{args.jobs}", flush=True)
        try:
            observation, root = run_backend(backend, args.source, args.jobs, args.keep)
        except Exception as error:
            print(f"FAIL: {error}")
            return 1
        observations.append(observation)
        summary = observation["summary"].strip().replace("\n", ", ")
        print(
            f"PASS: {summary}; {observation['process_rows']} process rows; "
            f"{observation['output_bytes']} output bytes; state={root}",
            flush=True,
        )

    if len(observations) > 1:
        first, *rest = observations
        for observation in rest:
            if observation["image_sha256"] != first["image_sha256"]:
                print(
                    f"FAIL: Image differs between {first['backend']} and "
                    f"{observation['backend']}"
                )
                return 1
            if observation["vmlinux_sha256"] != first["vmlinux_sha256"]:
                print(
                    f"FAIL: vmlinux differs between {first['backend']} and "
                    f"{observation['backend']}"
                )
                return 1
        print("PASS: FUSE and QEMU produced byte-identical kernel artifacts")
    return 0


def test_kernel_build_backends():
    assert main([]) == 0


if __name__ == "__main__":
    raise SystemExit(main())
