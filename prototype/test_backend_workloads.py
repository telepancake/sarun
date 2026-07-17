#!/usr/bin/env python3
"""Strict real-tool workload matrix for every locally runnable backend.

Unlike the historical workload test, this gate never turns a missing tool or a
failed operation into a passing skip. It uses a caller-writable lower tree,
disables networking, compares backend observations, and hashes the lower tree
before and after each run to prove that no write escaped SarunFs.
"""

import hashlib
import os
import shutil
import socket
import stat
import subprocess
import tarfile
import tempfile
import time
from importlib.machinery import SourceFileLoader
from pathlib import Path

from sarun_test_paths import ENGINE_BIN, HOST_ARCH, LIBTESTSARUN, REPO_ROOT
from test_backend_equiv_rs import backend_selector


REQUIRED_TOOLS = (
    "autoconf", "cargo", "cc", "cmake", "git", "make", "ninja", "python3", "tar"
)
RESULTS = {
    "archive-result": b"ARCHIVE\n",
    "autoconf-result": b"backend-workload\n",
    "cargo-result": b"CARGO\n",
    "cmake-result": b"CMAKE\n",
    "git-result": b"2\n",
    "make-result": b"MAKE\n",
    "ninja-result": b"NINJA\n",
    "sqlite-result": b"100:4950\n",
}


WORKLOADS = {
    "git": r'''set -eu
mkdir git-repo
cd git-repo
git -c commit.gpgsign=false init -q
git -c commit.gpgsign=false config user.email workload@sarun.invalid
git -c commit.gpgsign=false config user.name sarun
printf one > tracked
git -c commit.gpgsign=false add tracked
git -c commit.gpgsign=false commit -qm one
printf two >> tracked
git -c commit.gpgsign=false add tracked
git -c commit.gpgsign=false commit -qm two
git -c commit.gpgsign=false rev-list --count HEAD > ../git-result
''',
    "sqlite": r'''set -eu
PYTHONDONTWRITEBYTECODE=1 python3 sqlite-workload.py
''',
    "make": r'''set -eu
make -C make-project -j4 --no-print-directory
./make-project/app > make-result
''',
    "ninja": r'''set -eu
ninja -C ninja-project -j4
./ninja-project/app > ninja-result
''',
    "autoconf": r'''set -eu
cd autoconf-project
autoconf
./configure >/dev/null
cat configured.txt > ../autoconf-result
''',
    "cargo": r'''set -eu
cd cargo-project
CARGO_NET_OFFLINE=true cargo build --offline --release --quiet
./target/release/backend-workload > ../cargo-result
''',
    "cmake": r'''set -eu
cmake -S cmake-project -B cmake-project/build -G Ninja >/dev/null
cmake --build cmake-project/build --parallel 4 >/dev/null
./cmake-project/build/app > cmake-result
''',
    "archive": r'''set -eu
mkdir extracted
tar -xf fixture.tar -C extracted
test -L extracted/link
test "$(cat extracted/link)" = ARCHIVE
cat extracted/payload > archive-result
''',
}


def available_backends():
    backends = ["fuse"]
    if HOST_ARCH == "x86_64" and (REPO_ROOT / "tv/sud64").exists():
        backends.append("sud")
    appliance = Path.home() / f".cache/sarun/appliances/v1/{HOST_ARCH}/kernel"
    qemu = Path.home() / (
        f".cache/sarun/appliances/v1/host-{HOST_ARCH}/qemu-system-{HOST_ARCH}"
    )
    if appliance.exists() and qemu.exists():
        backends.append("qemu")
    return backends


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


def write_c_project(directory, word, generator):
    directory.mkdir()
    for index in range(4):
        (directory / f"part{index}.c").write_text(
            f"int part{index}(void) {{ return {index}; }}\n"
        )
    (directory / "main.c").write_text(
        "#include <stdio.h>\n"
        + "".join(f"int part{index}(void);" for index in range(4))
        + f'\nint main(void) {{ puts("{word}"); return '
        + "+".join(f"part{index}()" for index in range(4))
        + " == 6 ? 0 : 1; }\n"
    )
    objects = " ".join(f"part{index}.o" for index in range(4)) + " main.o"
    if generator == "make":
        (directory / "Makefile").write_text(
            f"all: app\napp: {objects}\n\t$(CC) -o $@ {objects}\n"
            "%.o: %.c\n\t$(CC) -c -o $@ $<\n"
        )
    elif generator == "ninja":
        builds = "\n".join(
            f"build part{index}.o: cc part{index}.c" for index in range(4)
        )
        (directory / "build.ninja").write_text(
            "rule cc\n  command = cc -c -o $out $in\n"
            "rule link\n  command = cc -o $out $in\n"
            f"{builds}\nbuild main.o: cc main.c\n"
            f"build app: link {objects}\ndefault app\n"
        )


def prepare_lower(work):
    (work / "sqlite-workload.py").write_text(
        "import sqlite3\n"
        "db = sqlite3.connect('workload.db')\n"
        "db.execute('pragma journal_mode=wal')\n"
        "db.execute('create table values_(value integer not null)')\n"
        "db.executemany('insert into values_ values (?)', ((i,) for i in range(100)))\n"
        "db.commit()\n"
        "count, total = db.execute('select count(*), sum(value) from values_').fetchone()\n"
        "db.execute('pragma wal_checkpoint(truncate)')\n"
        "db.close()\n"
        "open('sqlite-result', 'w').write(f'{count}:{total}\\n')\n"
    )

    cargo = work / "cargo-project"
    (cargo / "src").mkdir(parents=True)
    (cargo / "Cargo.toml").write_text(
        '[package]\nname = "backend-workload"\nversion = "0.1.0"\nedition = "2024"\n'
    )
    (cargo / "src/main.rs").write_text('fn main() { println!("CARGO"); }\n')

    write_c_project(work / "make-project", "MAKE", "make")
    write_c_project(work / "ninja-project", "NINJA", "ninja")

    autoconf = work / "autoconf-project"
    autoconf.mkdir()
    (autoconf / "configure.ac").write_text(
        "AC_INIT([backend-workload], [1])\n"
        "AC_CONFIG_FILES([configured.txt])\n"
        "AC_OUTPUT\n"
    )
    (autoconf / "configured.txt.in").write_text("@PACKAGE_NAME@\n")

    cmake = work / "cmake-project"
    cmake.mkdir()
    (cmake / "CMakeLists.txt").write_text(
        "cmake_minimum_required(VERSION 3.10)\n"
        "project(backend_workload C)\n"
        "add_executable(app main.c)\n"
    )
    (cmake / "main.c").write_text(
        '#include <stdio.h>\nint main(void) { puts("CMAKE"); return 0; }\n'
    )

    payload = work / "archive-payload"
    payload.write_bytes(b"ARCHIVE\n")
    os.chmod(payload, 0o640)
    link = work / "archive-link"
    link.symlink_to("payload")
    with tarfile.open(work / "fixture.tar", "w") as archive:
        archive.add(payload, arcname="payload")
        archive.add(link, arcname="link", recursive=False)
    payload.unlink()
    link.unlink()


def lower_digest(root):
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*"), key=lambda item: item.as_posix()):
        relative = path.relative_to(root).as_posix().encode()
        status = path.lstat()
        metadata = (
            stat.S_IMODE(status.st_mode), status.st_uid, status.st_gid,
            status.st_size, status.st_mtime_ns,
        )
        digest.update(relative + b"\0" + repr(metadata).encode() + b"\0")
        if path.is_symlink():
            digest.update(b"L" + os.readlink(path).encode())
        elif path.is_file():
            digest.update(b"F" + path.read_bytes())
        elif path.is_dir():
            digest.update(b"D")
        else:
            digest.update(b"?")
    return digest.digest()


def run_backend(backend):
    root = Path(tempfile.mkdtemp(prefix=f"sarun-workloads-{backend}-", dir="/var/tmp"))
    work = root / "lower"
    work.mkdir()
    prepare_lower(work)
    before = lower_digest(work)
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
    env["SLOPBOX_NS"] = f"WORKLOAD{backend.upper()}"

    old = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    module = SourceFileLoader(f"workload_slopbox_{backend}", str(LIBTESTSARUN)).load_module()
    module.ensure_dirs()
    engine = subprocess.Popen(
        [str(ENGINE_BIN), "serve"], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_socket(module.sock_path()):
            raise RuntimeError("engine socket never appeared")
        store = Path(env["XDG_STATE_HOME"]) / f"slopbox.{env['SLOPBOX_NS']}"
        prefix = str(work).lstrip("/") + "/"
        observation = {}
        stage_rows = {}
        for stage, script in WORKLOADS.items():
            command = [
                str(ENGINE_BIN), "run", "--net", "off", *backend_selector(backend),
                f"WORKLOAD-{backend}-{stage}", "-C", str(work), "--",
                "sh", "-c", script,
            ]
            result = subprocess.run(
                command, env=env, capture_output=True, text=True, timeout=1200
            )
            if result.returncode != 0:
                raise RuntimeError(
                    f"{stage}: rc={result.returncode}; "
                    f"stdout={result.stdout[-1000:]!r}; "
                    f"stderr={result.stderr[-1500:]!r}"
                )
            sqlar = max(store.glob("*.sqlar"), key=lambda path: int(path.stem))
            rows = {name: mode for name, mode, *_ in module.sqlar_list(sqlar)}
            stage_rows[stage] = rows
            result_name = f"{stage}-result"
            observation[result_name] = module.sqlar_content(
                sqlar, prefix + result_name
            )
        observation["make_executable"] = bool(
            stage_rows["make"].get(prefix + "make-project/app", 0) & 0o111
        )
        observation["ninja_executable"] = bool(
            stage_rows["ninja"].get(prefix + "ninja-project/app", 0) & 0o111
        )
        observation["cargo_executable"] = bool(
            stage_rows["cargo"].get(
                prefix + "cargo-project/target/release/backend-workload", 0
            ) & 0o111
        )
        observation["cmake_executable"] = bool(
            stage_rows["cmake"].get(prefix + "cmake-project/build/app", 0) & 0o111
        )
        observation["git_objects"] = sum(
            name.startswith(prefix + "git-repo/.git/objects/")
            for name in stage_rows["git"]
        )
        observation["sqlite_database"] = (
            prefix + "workload.db" in stage_rows["sqlite"]
        )
        observation["host_unchanged"] = lower_digest(work) == before
        return observation
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


def main():
    if not ENGINE_BIN.exists():
        print(f"backend-workloads: no {ENGINE_BIN}; build with `make engine`")
        return 2
    missing = [tool for tool in REQUIRED_TOOLS if shutil.which(tool) is None]
    if missing:
        print("backend-workloads: missing required tools: " + ", ".join(missing))
        return 2

    failures = []
    observations = {}
    backends = available_backends()
    for backend in backends:
        print(f"\n[{backend}] real developer workloads")
        try:
            observation = run_backend(backend)
        except Exception as error:
            print(f" FAIL {backend}: {error}")
            failures.append(f"{backend}: {error}")
            continue
        observations[backend] = observation
        for name, expected in RESULTS.items():
            okay = observation[name] == expected
            print(("  ok  " if okay else " FAIL ") + f"{backend}: {name}")
            if not okay:
                failures.append(f"{backend}: {name}")
        for name in ("make_executable", "ninja_executable", "cargo_executable",
                     "cmake_executable", "sqlite_database", "host_unchanged"):
            okay = bool(observation[name])
            print(("  ok  " if okay else " FAIL ") + f"{backend}: {name}")
            if not okay:
                failures.append(f"{backend}: {name}")
        enough_objects = observation["git_objects"] >= 4
        print(("  ok  " if enough_objects else " FAIL ")
              + f"{backend}: git object database captured")
        if not enough_objects:
            failures.append(f"{backend}: git objects")

    if len(observations) > 1:
        baseline_name = next(iter(observations))
        baseline = observations[baseline_name]
        for backend, observation in list(observations.items())[1:]:
            equal = observation == baseline
            print(("  ok  " if equal else " FAIL ")
                  + f"equiv: {backend} observations equal {baseline_name}")
            if not equal:
                failures.append(f"equiv: {backend}")

    print("\n" + ("BACKEND-WORKLOADS PASS" if not failures
                  else f"{len(failures)} FAILURE(S)"))
    return 1 if failures else 0


def test_backend_workloads():
    assert main() == 0


if __name__ == "__main__":
    raise SystemExit(main())
