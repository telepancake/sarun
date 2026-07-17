#!/usr/bin/env python3
"""Comparable live filesystem benchmarks for Sarun execution backends.

Timing occurs inside the box, so QEMU boot and runner startup are excluded.
Set SARUN_BENCH_ROUNDS to change the default three repetitions.
"""

import math
import os
import shutil
import socket
import statistics
import subprocess
import tempfile
import time
from importlib.machinery import SourceFileLoader
from pathlib import Path

from sarun_test_paths import ENGINE_BIN, HOST_ARCH, LIBTESTSARUN, REPO_ROOT
from test_backend_equiv_rs import backend_selector


BENCHMARK = r'''
set -eu
decimal() {
  value=$1
  while [ "${value#0}" != "$value" ]; do value=${value#0}; done
  [ -n "$value" ] || value=0
  printf '%s' "$value"
}
now() {
  read uptime ignored < /proc/uptime
  seconds=$(decimal "${uptime%.*}")
  hundredths=$(decimal "${uptime#*.}")
  printf '%s\n' "$((seconds * 100 + hundredths))"
}

start=$(now)
dd if=/dev/zero of=sequential bs=1048576 count=32 2>/dev/null
dd if=sequential of=/dev/null bs=1048576 2>/dev/null
end=$(now)
sequential_ns=$((end - start))

mkdir metadata
start=$(now)
i=0
while [ "$i" -lt 1000 ]; do
  printf x > "metadata/.tmp-$i"
  mv "metadata/.tmp-$i" "metadata/file-$i"
  test -s "metadata/file-$i"
  i=$((i + 1))
done
end=$(now)
metadata_ns=$((end - start))

mkdir parallel parallel-tmp
start=$(now)
i=0
while [ "$i" -lt 8 ]; do
  (
    j=0
    while [ "$j" -lt 100 ]; do
      printf 'worker-%s-%s' "$i" "$j" > "parallel-tmp/$i-$j"
      mv "parallel-tmp/$i-$j" "parallel/file-$i-$j"
      j=$((j + 1))
    done
  ) &
  i=$((i + 1))
done
wait
count=0
for file in parallel/file-*; do
  cat "$file" >/dev/null
  count=$((count + 1))
done
[ "$count" -eq 800 ]
end=$(now)
parallel_ns=$((end - start))

printf 'sequential=%s\nmetadata=%s\nparallel=%s\n' \
  "$sequential_ns" "$metadata_ns" "$parallel_ns" > benchmark-results
'''


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


def available_backends():
    result = ["fuse"]
    if HOST_ARCH == "x86_64" and (REPO_ROOT / "tv/sud64").exists():
        result.append("sud")
    configured = os.environ.get("SARUN_APPLIANCE_ROOT")
    if configured:
        root = Path(configured)
    else:
        root = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) \
            / "sarun/appliances/v1"
    appliance = root / HOST_ARCH / "kernel"
    qemu = root / f"host-{HOST_ARCH}" / f"qemu-system-{HOST_ARCH}"
    if appliance.exists() and qemu.exists():
        result.append("qemu")
    return result


def run_once(backend, iteration):
    root = Path(tempfile.mkdtemp(prefix=f"sarun-bench-{backend}-", dir="/var/tmp"))
    work = root / "lower"
    work.mkdir()
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
    env["SLOPBOX_NS"] = f"BENCH{backend.upper()}{iteration}"

    old = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    module = SourceFileLoader(f"bench_slopbox_{backend}_{iteration}",
                              str(LIBTESTSARUN)).load_module()
    module.ensure_dirs()
    engine = subprocess.Popen([str(ENGINE_BIN), "serve"], env=env,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not wait_socket(module.sock_path()):
            raise RuntimeError("engine socket never appeared")
        command = [
            str(ENGINE_BIN), "run", "--net", "off", *backend_selector(backend),
            f"BENCH-{backend}-{iteration}", "-C", str(work), "--",
            "sh", "-c", BENCHMARK,
        ]
        result = subprocess.run(command, env=env, capture_output=True, text=True,
                                timeout=300)
        if result.returncode != 0:
            raise RuntimeError(
                f"rc={result.returncode}: stdout={result.stdout[-500:]!r}; "
                f"stderr={result.stderr[-500:]!r}"
            )
        store = Path(env["XDG_STATE_HOME"]) / f"slopbox.{env['SLOPBOX_NS']}"
        sqlar = max(store.glob("*.sqlar"), key=lambda path: int(path.stem))
        rel = f"{str(work).lstrip('/')}/benchmark-results"
        payload = module.sqlar_content(sqlar, rel).decode()
        if (work / "benchmark-results").exists():
            raise RuntimeError("benchmark write escaped to the host")
        return {
            key: int(value) * 10.0
            for key, value in (line.split("=", 1) for line in payload.splitlines())
        }
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
        print(f"no {ENGINE_BIN}; build with `make engine`")
        return 2
    rounds = int(os.environ.get("SARUN_BENCH_ROUNDS", "3"))
    backends = available_backends()
    samples = {backend: [] for backend in backends}
    for iteration in range(rounds):
        for backend in backends:
            result = run_once(backend, iteration)
            samples[backend].append(result)
            print(f"{backend} round {iteration + 1}: {result}")

    metrics = ("sequential", "metadata", "parallel")
    medians = {
        backend: {
            metric: statistics.median(run[metric] for run in runs)
            for metric in metrics
        }
        for backend, runs in samples.items()
    }
    baseline = medians["fuse"]
    print("\n| backend | sequential ms | metadata ms | parallel ms | geo vs FUSE |")
    print("|---|---:|---:|---:|---:|")
    for backend in backends:
        ratios = [medians[backend][metric] / baseline[metric] for metric in metrics]
        geometric = math.prod(ratios) ** (1 / len(ratios))
        values = medians[backend]
        print(f"| {backend} | {values['sequential']:.1f} | "
              f"{values['metadata']:.1f} | {values['parallel']:.1f} | "
              f"{geometric:.2f}x |")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
