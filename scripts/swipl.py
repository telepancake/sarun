#!/usr/bin/env python3
"""Build the pinned, core-only SWI-Prolog archive for sarun.

Sources and intermediate build trees live in XDG_CACHE_HOME. The stable output
layout consumed by engine/build.rs is written below ignored engine/target.
"""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile

SWIPL_VERSION = "9.2.9"
SWIPL_COMMIT = "e3b19512e69a544f05b1bffbd14f3a0b519ad04d"
SWIPL_URL = f"https://github.com/SWI-Prolog/swipl-devel/archive/{SWIPL_COMMIT}.tar.gz"
SWIPL_SHA256 = "281e59fff098094bec8dc0831bd360c35d6360aaf12eebfc7b6be74f31d74d72"

ZLIB_VERSION = "1.3.1"
ZLIB_COMMIT = "51b7f2abdade71cd9bb0e7a373ef2610ec6f9daf"
ZLIB_URL = f"https://github.com/madler/zlib/archive/{ZLIB_COMMIT}.tar.gz"
ZLIB_SHA256 = "d9e270d46252734aa49770fbc544125391617956266f220bd63216c834f3a522"

SUPPORTED_TARGETS = {
    "x86_64-linux-musl": "x86_64",
    "aarch64-linux-musl": "aarch64",
}
HOST_TARGETS = {
    "x86_64": "x86_64-linux-musl",
    "aarch64": "aarch64-linux-musl",
    "arm64": "aarch64-linux-musl",
}
SOURCE_DATE_EPOCH = "1734688000"  # SWI-Prolog V9.2.9 commit time
PIPELINE_VERSION = "5"


def run(*args: str, env: dict[str, str] | None = None) -> None:
    print("+", " ".join(args), flush=True)
    subprocess.run(args, check=True, env=env)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def tool_version(*args: str) -> str:
    output = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE).stdout
    return output.splitlines()[0].strip()


def metadata(repo: Path, host: str, target: str, zig: str) -> dict[str, str]:
    grammar = repo / "engine" / "pl" / "action_grammar.pl"
    grammar_engine = repo / "engine" / "pl" / "grammar_engine.pl"
    grammar_ir = repo / "engine" / "pl" / "grammar_ir.pl"
    catalog = repo / "engine" / "pl" / "action_catalog.pl"
    context_relation = repo / "engine" / "pl" / "context_relation.pl"
    transport_catalog = repo / "engine" / "pl" / "transport_catalog.pl"
    if not grammar.is_file():
        raise RuntimeError(f"missing application grammar: {grammar}")
    if not grammar_engine.is_file():
        raise RuntimeError(f"missing grammar engine: {grammar_engine}")
    if not grammar_ir.is_file():
        raise RuntimeError(f"missing grammar IR: {grammar_ir}")
    if not catalog.is_file():
        raise RuntimeError(f"missing action catalog: {catalog}")
    if not context_relation.is_file():
        raise RuntimeError(f"missing context relation: {context_relation}")
    if not transport_catalog.is_file():
        raise RuntimeError(f"missing transport catalog: {transport_catalog}")
    return {
        "pipeline": PIPELINE_VERSION,
        "target": target,
        "host": host,
        "source_date_epoch": SOURCE_DATE_EPOCH,
        "swipl_version": SWIPL_VERSION,
        "swipl_commit": SWIPL_COMMIT,
        "swipl_source_sha256": SWIPL_SHA256,
        "zlib_version": ZLIB_VERSION,
        "zlib_commit": ZLIB_COMMIT,
        "zlib_source_sha256": ZLIB_SHA256,
        "action_grammar_sha256": sha256(grammar),
        "grammar_engine_sha256": sha256(grammar_engine),
        "grammar_ir_sha256": sha256(grammar_ir),
        "action_catalog_sha256": sha256(catalog),
        "context_relation_sha256": sha256(context_relation),
        "transport_catalog_sha256": sha256(transport_catalog),
        "swipl_license_sha256": sha256(repo / "LICENSES" / "SWI-Prolog.txt"),
        "zlib_license_sha256": sha256(repo / "LICENSES" / "zlib.txt"),
        "cmake": tool_version("cmake", "--version"),
        "ninja": tool_version("ninja", "--version"),
        "zig": tool_version(zig, "version"),
        "cc": tool_version("cc", "--version"),
        "use_signals": "OFF",
        "no_bignum_patch": "guard-ar-alloc-buffer",
    }


def build_identity(metadata: dict[str, str]) -> str:
    """Identify compiled dependencies, excluding repackaged application data."""
    resource_keys = {
        "action_grammar_sha256",
        "grammar_engine_sha256",
        "grammar_ir_sha256",
        "action_catalog_sha256",
        "context_relation_sha256",
        "transport_catalog_sha256",
        "swipl_license_sha256",
        "zlib_license_sha256",
    }
    payload = "".join(
        f"{key}={value}\n"
        for key, value in sorted(metadata.items())
        if key not in resource_keys
    )
    return hashlib.sha256(payload.encode()).hexdigest()[:16]


def read_build_info(path: Path) -> dict[str, str]:
    try:
        entries = [line.split("=", 1) for line in path.read_text().splitlines()]
        if any(len(entry) != 2 for entry in entries):
            return {}
        return dict(entries)
    except OSError:
        return {}


def valid_output(output: Path, expected: dict[str, str]) -> bool:
    info = read_build_info(output / "BUILD-INFO")
    if any(info.get(key) != value for key, value in expected.items()):
        return False
    artifacts = (
        "boot.prc",
        "sarun.prc",
        "lib/libswipl.a",
        "lib/libz.a",
        "include/SWI-Prolog.h",
        "include/SWI-Stream.h",
        "include/zlib.h",
        "include/zconf.h",
        "LICENSES/SWI-Prolog.txt",
        "LICENSES/zlib.txt",
    )
    return all(
        (output / name).is_file()
        and info.get(f"artifact.{name}.sha256") == sha256(output / name)
        for name in artifacts
    )


def download(url: str, expected: str, path: Path) -> None:
    if path.exists() and sha256(path) == expected:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        request = urllib.request.Request(
            url, headers={"User-Agent": "sarun-swipl-build/1"}
        )
        with urllib.request.urlopen(request) as response:
            shutil.copyfileobj(response, stream)
    actual = sha256(temporary)
    if actual != expected:
        temporary.unlink()
        raise RuntimeError(
            f"SHA256 mismatch for {url}: expected {expected}, got {actual}"
        )
    temporary.replace(path)


def extract(archive: Path, destination: Path, expected_dir: str) -> Path:
    source = destination / expected_dir
    marker = source / ".sarun-source"
    identity = f"{sha256(archive)}\n"
    if marker.exists() and marker.read_text() == identity:
        return source

    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=destination) as temporary:
        temporary_path = Path(temporary)
        with tarfile.open(archive, "r:gz") as bundle:
            bundle.extractall(temporary_path, filter="data")
        extracted = temporary_path / expected_dir
        if not extracted.is_dir():
            raise RuntimeError(f"archive {archive} did not contain {expected_dir}")
        (extracted / ".sarun-source").write_text(identity)
        if source.exists():
            shutil.rmtree(source)
        shutil.move(str(extracted), source)
    return source


def validate_notices(repo: Path, swipl_source: Path, zlib_source: Path) -> None:
    notices = (
        (repo / "LICENSES" / "SWI-Prolog.txt", swipl_source / "LICENSE"),
        (repo / "LICENSES" / "zlib.txt", zlib_source / "LICENSE"),
    )
    for tracked, upstream in notices:
        if tracked.read_bytes() != upstream.read_bytes():
            raise RuntimeError(
                f"license notice mismatch: {tracked} ({sha256(tracked)}) != "
                f"{upstream} ({sha256(upstream)})"
            )


def patch_swipl(source: Path) -> None:
    """Fix 9.2.9 core-only arithmetic; upstream assumes a bignum allocator."""
    path = source / "src" / "pl-vmi.c"
    old = "  __PL_ar_ctx.alloc_buf = __PL_ar_buf;"
    new = "#if O_MY_GMP_ALLOC\n  __PL_ar_ctx.alloc_buf = __PL_ar_buf;\n#endif"
    text = path.read_text()
    if new in text:
        return
    if text.count(old) != 1:
        raise RuntimeError(f"cannot apply no-bignum SWI patch to {path}")
    path.write_text(text.replace(old, new))


def configure(
    source: Path, build: Path, options: list[str], env: dict[str, str]
) -> None:
    build.mkdir(parents=True, exist_ok=True)
    run(
        "cmake",
        "-S",
        str(source),
        "-B",
        str(build),
        "-G",
        "Ninja",
        "-DCMAKE_BUILD_TYPE=Release",
        *options,
        env=env,
    )


def build_zlib(
    source: Path,
    build: Path,
    install: Path,
    env: dict[str, str],
    cross_options: list[str],
) -> None:
    configure(
        source,
        build,
        [
            f"-DCMAKE_INSTALL_PREFIX={install}",
            "-DBUILD_SHARED_LIBS=OFF",
            "-DZLIB_BUILD_EXAMPLES=OFF",
            *cross_options,
        ],
        env,
    )
    run("cmake", "--build", str(build), "--target", "zlibstatic", "--parallel", env=env)
    (install / "lib").mkdir(parents=True, exist_ok=True)
    (install / "include").mkdir(parents=True, exist_ok=True)
    shutil.copyfile(build / "libz.a", install / "lib" / "libz.a")
    shutil.copyfile(source / "zlib.h", install / "include" / "zlib.h")
    shutil.copyfile(build / "zconf.h", install / "include" / "zconf.h")


def swipl_options(zlib: Path) -> list[str]:
    return [
        "-DSWIPL_PACKAGES=OFF",
        "-DBUILD_PDF_DOCUMENTATION=OFF",
        "-DINSTALL_DOCUMENTATION=OFF",
        "-DBUILD_TESTING=OFF",
        "-DINSTALL_TESTS=OFF",
        "-DBUILD_SWIPL_LD=OFF",
        "-DSWIPL_SHARED_LIB=OFF",
        "-DSTATIC_EXTENSIONS=ON",
        "-DUSE_GMP=OFF",
        "-DUSE_LIBBF=OFF",
        "-DCMAKE_DISABLE_FIND_PACKAGE_Curses=TRUE",
        "-DUSE_TCMALLOC=OFF",
        # sarun owns process signal handlers. The embedded runtime is confined
        # to one worker, so compiling out SWI's signal machinery is safer than
        # relying only on the runtime --no-signals option.
        "-DUSE_SIGNALS=OFF",
        "-DMULTI_THREADED=ON",
        f"-DZLIB_ROOT={zlib}",
        f"-DZLIB_INCLUDE_DIR={zlib / 'include'}",
        f"-DZLIB_LIBRARY={zlib / 'lib' / 'libz.a'}",
        f"-DZLIB_LIBRARY_RELEASE={zlib / 'lib' / 'libz.a'}",
        "-DZLIB_USE_STATIC_LIBS=ON",
    ]


def create_app_resource(
    boot: Path, output: Path, repo: Path, swipl_source: Path
) -> None:
    shutil.copyfile(boot, output)
    entries = {
        "app/action_grammar.pl": repo / "engine" / "pl" / "action_grammar.pl",
        "app/grammar_engine.pl": repo / "engine" / "pl" / "grammar_engine.pl",
        "app/grammar_ir.pl": repo / "engine" / "pl" / "grammar_ir.pl",
        "app/action_catalog.pl": repo / "engine" / "pl" / "action_catalog.pl",
        "app/context_relation.pl": repo / "engine" / "pl" / "context_relation.pl",
        "app/transport_catalog.pl": repo / "engine" / "pl" / "transport_catalog.pl",
        "library/lists.pl": swipl_source / "library" / "lists.pl",
        "library/pairs.pl": swipl_source / "library" / "pairs.pl",
    }
    with zipfile.ZipFile(output, "a", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, source in entries.items():
            info = zipfile.ZipInfo(name, date_time=(2024, 12, 20, 9, 46, 40))
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, source.read_bytes())


def publish(
    repo: Path,
    swipl_source: Path,
    target_build: Path,
    target_zlib: Path,
    output: Path,
    build_metadata: dict[str, str],
) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent))
    try:
        (temporary / "lib").mkdir()
        (temporary / "include").mkdir()
        (temporary / "LICENSES").mkdir()
        shutil.copyfile(
            target_build / "src" / "libswipl.a", temporary / "lib" / "libswipl.a"
        )
        shutil.copyfile(target_zlib / "lib" / "libz.a", temporary / "lib" / "libz.a")
        boot = target_build / "home" / "boot.prc"
        shutil.copyfile(boot, temporary / "boot.prc")
        create_app_resource(boot, temporary / "sarun.prc", repo, swipl_source)
        shutil.copyfile(
            swipl_source / "src" / "SWI-Prolog.h",
            temporary / "include" / "SWI-Prolog.h",
        )
        shutil.copyfile(
            swipl_source / "src" / "os" / "SWI-Stream.h",
            temporary / "include" / "SWI-Stream.h",
        )
        shutil.copyfile(
            target_zlib / "include" / "zlib.h", temporary / "include" / "zlib.h"
        )
        shutil.copyfile(
            target_zlib / "include" / "zconf.h", temporary / "include" / "zconf.h"
        )
        shutil.copyfile(
            repo / "LICENSES" / "SWI-Prolog.txt",
            temporary / "LICENSES" / "SWI-Prolog.txt",
        )
        shutil.copyfile(
            repo / "LICENSES" / "zlib.txt", temporary / "LICENSES" / "zlib.txt"
        )

        artifact_metadata = {}
        for path in sorted(item for item in temporary.rglob("*") if item.is_file()):
            name = path.relative_to(temporary).as_posix()
            artifact_metadata[f"artifact.{name}.sha256"] = sha256(path)
        info = {**build_metadata, **artifact_metadata}
        (temporary / "BUILD-INFO").write_text(
            "".join(f"{key}={value}\n" for key, value in sorted(info.items()))
        )
        if valid_output(output, build_metadata):
            shutil.rmtree(temporary)
            return
        if output.exists():
            shutil.rmtree(output)
        temporary.replace(output)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise


def main() -> int:
    repo = Path(__file__).resolve().parent.parent
    cache_home = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
    default_cache = cache_home / "sarun" / "swipl" / SWIPL_VERSION
    host = platform.machine().lower()
    default_target = HOST_TARGETS.get(host)
    if default_target is None:
        raise RuntimeError(f"unsupported build host architecture: {host}")

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--target",
        choices=sorted(SUPPORTED_TARGETS),
        default=os.environ.get("SARUN_SWIPL_TARGET", default_target),
    )
    parser.add_argument(
        "--cache",
        type=Path,
        default=Path(os.environ.get("SARUN_SWIPL_CACHE", default_cache)),
    )
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    target = args.target

    for tool in ("cmake", "ninja", "cc"):
        if not shutil.which(tool):
            raise RuntimeError(f"required tool not found: {tool}")
    zig = os.environ.get("SWIPL_ZIG") or shutil.which("python-zig")
    if not zig:
        raise RuntimeError("python-zig not found; install cargo-zigbuild with ziglang")

    cache = args.cache.resolve()
    downloads = cache / "downloads"
    sources = cache / "sources"
    output = (
        args.output
        or repo / "engine" / "target" / "swipl" / SWIPL_VERSION / target
    ).resolve()
    swipl_archive = downloads / f"swipl-{SWIPL_VERSION}-{SWIPL_COMMIT}.tar.gz"
    zlib_archive = downloads / f"zlib-{ZLIB_VERSION}-{ZLIB_COMMIT}.tar.gz"
    download(SWIPL_URL, SWIPL_SHA256, swipl_archive)
    download(ZLIB_URL, ZLIB_SHA256, zlib_archive)
    swipl_source = extract(swipl_archive, sources, f"swipl-devel-{SWIPL_COMMIT}")
    zlib_source = extract(zlib_archive, sources, f"zlib-{ZLIB_COMMIT}")
    validate_notices(repo, swipl_source, zlib_source)

    build_metadata = metadata(repo, host, target, zig)
    if valid_output(output, build_metadata):
        print(f"SWI-Prolog output unchanged and validated: {output}")
        return 0
    identity = build_identity(build_metadata)
    work = cache / f"pipeline-{PIPELINE_VERSION}-{identity}" / host
    patch_swipl(swipl_source)

    env = os.environ.copy()
    env.update({"SOURCE_DATE_EPOCH": SOURCE_DATE_EPOCH, "TZ": "UTC", "LC_ALL": "C"})
    prefix_map = f"-ffile-prefix-map={cache}=/usr/src/sarun-swipl -fdebug-prefix-map={cache}=/usr/src/sarun-swipl"

    native_zlib = work / "native-zlib-install"
    build_zlib(zlib_source, work / "native-zlib-build", native_zlib, env, [])
    native_swipl_build = work / "native-swipl-build"
    configure(
        swipl_source,
        native_swipl_build,
        [
            f"-DCMAKE_CXX_COMPILER={zig}",
            "-DCMAKE_CXX_COMPILER_ARG1=c++",
            f"-DCMAKE_CXX_FLAGS=-target {host}-linux-gnu -Wno-error=date-time -g0 {prefix_map}",
            *swipl_options(native_zlib),
            f"-DCMAKE_C_FLAGS=-Wno-error=date-time -g0 {prefix_map}",
        ],
        env,
    )
    run(
        "cmake",
        "--build",
        str(native_swipl_build),
        "--target",
        "core",
        "--parallel",
        env=env,
    )

    cross = [
        "-DCMAKE_SYSTEM_NAME=Linux",
        f"-DCMAKE_SYSTEM_PROCESSOR={SUPPORTED_TARGETS[target]}",
        "-DCMAKE_CROSSCOMPILING=ON",
        f"-DCMAKE_C_COMPILER={zig}",
        "-DCMAKE_C_COMPILER_ARG1=cc",
        f"-DCMAKE_CXX_COMPILER={zig}",
        "-DCMAKE_CXX_COMPILER_ARG1=c++",
        f"-DCMAKE_C_FLAGS=-target {target} -Wno-error=date-time -g0 {prefix_map}",
        f"-DCMAKE_CXX_FLAGS=-target {target} -Wno-error=date-time -g0 {prefix_map}",
        "-DCMAKE_EXE_LINKER_FLAGS=-static",
        "-DRUN_RESULT=0",
        "-DRUN_RESULT__TRYRUN_OUTPUT=0",
        "-DHAVE_WEAK_ATTRIBUTE_EXITCODE=0",
        "-DHAVE_WEAK_ATTRIBUTE_EXITCODE__TRYRUN_OUTPUT=0",
        "-DQSORT_R_GNU=1",
        "-DLLROUND_OK=1",
        "-DMODF_OK=1",
    ]
    target_zlib = work / "target-zlib-install"
    build_zlib(zlib_source, work / "target-zlib-build", target_zlib, env, cross)
    target_swipl_build = work / "target-swipl-build"
    configure(
        swipl_source,
        target_swipl_build,
        [
            *swipl_options(target_zlib),
            *cross,
            "-DCMAKE_HOST_CC=cc",
            f"-DSWIPL_NATIVE_FRIEND={native_swipl_build}",
        ],
        env,
    )
    run(
        "cmake",
        "--build",
        str(target_swipl_build),
        "--target",
        "swipl",
        "bootfile",
        "--parallel",
        env=env,
    )
    publish(
        repo,
        swipl_source,
        target_swipl_build,
        target_zlib,
        output,
        build_metadata,
    )
    print(f"SWI-Prolog output: {output}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"swipl build failed: {error}", file=sys.stderr)
        raise SystemExit(1)
