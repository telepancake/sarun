#!/usr/bin/env python3
"""Build a relocatable viros debugger bundle from one exact Linux Kbuild tree."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

BUNDLE_FORMAT = "viros-kernel-bundle-v1"
PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from probe.probe_tool import ElfObject, gnu_build_id  # noqa: E402
from probe.fixed_profile import FixedProfileError, scratch_gpas  # noqa: E402


PROBE_TOOL = PROJECT_ROOT / "probe" / "probe_tool.py"
SCRATCH_TOOL = PROJECT_ROOT / "probe" / "scratch" / "scratch_tool.py"
ARCH_TO_KBUILD = {
    "aarch64": "arm64",
    "arm": "arm",
    "mmips": "mips",
    "x86_64": "x86_64",
}
STANDARD_BOOT_IMAGES = {
    "aarch64": Path("arch/arm64/boot/Image"),
    "arm": Path("arch/arm/boot/zImage"),
    # The fixed MMIPS Malta profile boots the linked ELF directly.
    "mmips": Path("vmlinux"),
    "x86_64": Path("arch/x86/boot/bzImage"),
}


class BundleError(RuntimeError):
    """The requested inputs cannot form an exact, portable bundle."""


def default_vmlinux(kbuild_output: Path, supplied: Path | None) -> Path:
    """Use the only vmlinux which can belong to this Kbuild output."""

    return supplied if supplied is not None else kbuild_output / "vmlinux"


def default_boot_image(
    kbuild_output: Path, architecture: str, supplied: Path | None,
) -> Path:
    """Select the fixed profile's standard boot artifact, without searching."""

    if supplied is not None:
        return supplied
    candidate = kbuild_output / STANDARD_BOOT_IMAGES[architecture]
    if not candidate.is_file():
        relative = STANDARD_BOOT_IMAGES[architecture].as_posix()
        raise BundleError(
            f"standard {architecture} boot image is absent from the exact Kbuild "
            f"output ({relative}); build it or pass --boot-image"
        )
    return candidate


def _command_words(path: Path) -> list[str]:
    """Read command words from one Kbuild .cmd file.

    Only the generated ``cmd_* :=`` assignment is considered. Dependency and
    source rows can contain arbitrary paths which are not executed tools.
    """

    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []
    for line in lines:
        match = re.match(r"^cmd_[^ ]+\s*:?=\s*(.*)$", line)
        if match is None:
            continue
        try:
            return shlex.split(match.group(1), posix=True)
        except ValueError:
            return []
    return []


_TOOL_NAMES = {
    "make": re.compile(r"^(?:g?make)$"),
    "compiler": re.compile(
        r"^(?:(?:[A-Za-z0-9_.+]+-)*gcc(?:-[0-9][0-9.]*)?|clang(?:-[0-9][0-9.]*)?)$"
    ),
    "cross-ld": re.compile(
        r"^(?:(?:[A-Za-z0-9_.+]+-)*ld|ld\.lld)(?:-[0-9][0-9.]*)?$"
    ),
    "objcopy": re.compile(
        r"^(?:(?:[A-Za-z0-9_.+]+-)*objcopy|llvm-objcopy)(?:-[0-9][0-9.]*)?$"
    ),
}


def _recorded_tool_candidates(kbuild_output: Path, label: str) -> set[str]:
    """Return executable argv[0] values literally present in Kbuild commands."""

    if label == "compiler":
        records = [
            kbuild_output / "kernel/viros/.viros_scratch.o.cmd",
            kbuild_output / "kernel/viros/.viros_event.o.cmd",
        ]
    elif label == "cross-ld":
        records = [kbuild_output / ".vmlinux.cmd"]
    else:
        records = sorted(kbuild_output.rglob("*.cmd"))

    matcher = _TOOL_NAMES[label]
    candidates: set[str] = set()
    for record in records:
        words = _command_words(record)
        if label == "compiler" and words:
            # A compiler wrapper command (ccache, env, shell) is intentionally
            # not interpreted. Replaying it as CC would select the wrapper,
            # not the compiler. Other tools may occur in later commands of a
            # literal Kbuild compound recipe.
            words = words[:1]
        for word in words:
            candidate = Path(word)
            if not matcher.fullmatch(candidate.name):
                continue
            candidates.add(word)
    return candidates


def infer_recorded_tool(kbuild_output: Path, label: str) -> str:
    """Infer one tool only when Kbuild recorded one usable absolute identity."""

    candidates = _recorded_tool_candidates(kbuild_output, label)
    usable: list[tuple[str, str]] = []
    for argument in sorted(candidates):
        candidate = Path(argument)
        if candidate.is_absolute():
            found = str(candidate) if candidate.is_file() else None
            selected = argument
        elif "/" in argument:
            relative = kbuild_output / candidate
            found = str(relative) if relative.is_file() else None
            # Kbuild runs generated commands from its output directory.
            selected = str(relative.absolute())
        else:
            # This is not an installation-path guess: it replays the exact
            # argv[0] recorded by Kbuild in the captured build box.
            found = shutil.which(argument)
            selected = argument
        if found is not None and os.access(found, os.X_OK):
            usable.append((argument, selected))
    if len(usable) == 1:
        return usable[0][1]
    option = f"--{label}"
    if len(usable) > 1:
        rendered = ", ".join(argument for argument, _ in usable)
        raise BundleError(
            f"{option} is ambiguous in the exact Kbuild command records: {rendered}; "
            f"pass {option} explicitly"
        )
    if candidates:
        rendered = ", ".join(sorted(candidates))
        raise BundleError(
            f"the exact Kbuild command records name {label} executable(s) which are "
            f"not available in this captured build box: {rendered}; pass {option} explicitly"
        )
    raise BundleError(
        f"the exact Kbuild command records do not identify a {label} "
        f"executable; pass {option} explicitly"
    )


def infer_cross_compile(
    compiler: Path, linker: Path, objcopy: Path,
) -> str:
    """Derive a GNU CROSS_COMPILE prefix and confirm its companion tools."""

    match = re.fullmatch(r"(?P<prefix>.*-)gcc(?:-[0-9][0-9.]*)?", compiler.name)
    if match is None:
        if re.fullmatch(r"gcc(?:-[0-9][0-9.]*)?", compiler.name):
            prefix_name = ""
        else:
            raise BundleError(
                "the exact compiler command does not encode CROSS_COMPILE; "
                "pass --cross-compile explicitly"
            )
    else:
        prefix_name = match.group("prefix")
    expected_ld = f"{prefix_name}ld"
    expected_objcopy = f"{prefix_name}objcopy"
    if linker.name != expected_ld or objcopy.name != expected_objcopy:
        raise BundleError(
            "the recorded compiler, linker, and objcopy do not establish one exact "
            "GNU CROSS_COMPILE prefix; pass --cross-compile explicitly"
        )
    if not prefix_name:
        return ""
    return str(compiler.parent / prefix_name)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def resolve_tool(argument: str, label: str) -> dict[str, object]:
    """Resolve and identify one explicitly selected executable."""

    if not argument or "\x00" in argument or "\n" in argument:
        raise BundleError(f"--{label} must name one explicit executable")
    found = shutil.which(argument)
    if found is None:
        raise BundleError(f"--{label} executable was not found: {argument}")
    # Preserve the selected argv[0]. Compiler SDKs commonly expose one
    # multi-call wrapper through triplet-named symlinks, and resolving that
    # symlink before execution changes which tool the wrapper selects.
    path = Path(found).absolute()
    resolved = path.resolve()
    if not resolved.is_file() or not os.access(path, os.X_OK):
        raise BundleError(f"--{label} is not an executable regular file: {path}")
    try:
        completed = subprocess.run(
            [str(path), "--version"], check=True, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, timeout=10,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise BundleError(f"cannot identify --{label} executable {path}: {exc}") from exc
    first_line = completed.stdout.splitlines()[0] if completed.stdout.splitlines() else ""
    if not first_line:
        raise BundleError(f"--{label} executable returned an empty version: {path}")
    return {
        "argument": argument,
        "path": str(path),
        "resolved_path": str(resolved),
        "sha256": sha256_file(resolved),
        "version": first_line,
    }


def validate_make_args(values: list[str]) -> list[str]:
    for value in values:
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=[^\x00\r\n]*", value):
            raise BundleError(
                "--make-arg must be one Kbuild variable assignment without newlines"
            )
    return list(values)


def effective_probe_make_args(requested: list[str], compiler: Path) -> list[str]:
    """Pin the helper compilation to the identified compiler executable."""

    return [*requested, f"CC={compiler}"]


def probe_build_command(
    *, python: str, kbuild_output: Path, output: Path, architecture: str,
    cross_compile: str, make: str, make_args: list[str], vmlinux: Path,
) -> list[str]:
    command = [
        python, str(PROBE_TOOL), "build",
        "--linux-dir", str(kbuild_output),
        "--output-dir", str(output),
        "--arch", architecture,
        "--cross-compile", cross_compile,
        "--make", make,
        "--vmlinux", str(vmlinux),
    ]
    for assignment in make_args:
        command.extend(("--make-arg", assignment))
    return command


def defined_symbol(vmlinux: Path, name: str) -> int:
    elf = ElfObject(vmlinux)
    values = {
        record["value"] for record in elf.symbol_records()
        if record["name"] == name and record["shndx"] != 0
    }
    if len(values) != 1:
        raise BundleError(f"matching vmlinux does not define exactly one {name}")
    return next(iter(values))


def copy_file(source: Path, destination: Path, label: str) -> None:
    if not source.is_file():
        raise BundleError(f"{label} is not a regular file: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination, follow_symlinks=True)


def artifact_rows(root: Path) -> list[dict[str, object]]:
    rows = []
    for path in sorted(root.rglob("*")):
        if path.is_file() and path.name != "bundle.json":
            rows.append({
                "path": path.relative_to(root).as_posix(),
                "size": path.stat().st_size,
                "sha256": sha256_file(path),
            })
    return rows


def write_bundle_manifest(
    root: Path, *, architecture: str, kbuild_output: Path,
    cross_compile: str, requested_make_args: list[str], probe_make_args: list[str],
    toolchain: dict[str, dict[str, object]],
    original_vmlinux: Path, original_boot_image: Path, runtime_offset: int,
) -> dict[str, object]:
    vmlinux = root / "vmlinux"
    boot_image = root / "kernel"
    document: dict[str, object] = {
        "format": BUNDLE_FORMAT,
        "architecture": architecture,
        "kbuild_arch": ARCH_TO_KBUILD[architecture],
        "kernel": {
            "vmlinux": "vmlinux",
            "vmlinux_sha256": sha256_file(vmlinux),
            "build_id": gnu_build_id(vmlinux, architecture),
            "boot_image": "kernel",
            "boot_image_sha256": sha256_file(boot_image),
            "runtime_offset": runtime_offset,
        },
        "inputs": {
            "kbuild_output": str(kbuild_output),
            "vmlinux": str(original_vmlinux),
            "boot_image": str(original_boot_image),
        },
        "toolchain": {
            "cross_compile": cross_compile,
            "requested_make_args": requested_make_args,
            "probe_make_args": probe_make_args,
            "tools": toolchain,
        },
        "entrypoints": {
            "callgate": "callgate.json",
            "gdb_loader": "vmlinux-gdb.py",
        },
        "artifacts": artifact_rows(root),
    }
    destination = root / "bundle.json"
    with destination.open("w", encoding="utf-8") as stream:
        json.dump(document, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    return document


def run_checked(command: list[str], log: Path) -> None:
    log.parent.mkdir(parents=True, exist_ok=True)
    try:
        with log.open("wb") as stream:
            subprocess.run(command, check=True, stdout=stream, stderr=subprocess.STDOUT)
    except (OSError, subprocess.CalledProcessError) as exc:
        rendered = " ".join(command)
        try:
            lines = log.read_text(encoding="utf-8", errors="replace").splitlines()
            tail = "\n".join(lines[-20:])
        except OSError:
            tail = ""
        detail = f"\nlast command output:\n{tail}" if tail else ""
        raise BundleError(f"bundle command failed: {rendered}: {exc}{detail}") from exc


def build_bundle(args: argparse.Namespace) -> Path:
    architecture = args.arch
    kbuild_output = args.kbuild_output.resolve()
    if not kbuild_output.is_dir():
        raise BundleError(f"Kbuild output directory does not exist: {kbuild_output}")
    original_vmlinux = default_vmlinux(kbuild_output, args.vmlinux).resolve()
    original_boot_image = default_boot_image(
        kbuild_output, architecture, args.boot_image,
    ).resolve()
    output = args.output_dir.resolve()
    requested_make_args = validate_make_args(args.make_arg)

    built_vmlinux = kbuild_output / "vmlinux"
    if not built_vmlinux.is_file() or not original_vmlinux.is_file():
        raise BundleError("both Kbuild output/vmlinux and --vmlinux must be regular files")
    if sha256_file(built_vmlinux) != sha256_file(original_vmlinux):
        raise BundleError("--vmlinux was not produced by the exact --kbuild-output tree")
    if not original_boot_image.is_file():
        raise BundleError(f"--boot-image is not a regular file: {original_boot_image}")
    try:
        original_boot_image.relative_to(kbuild_output)
    except ValueError as exc:
        raise BundleError(
            "--boot-image must be an artifact inside the exact --kbuild-output tree"
        ) from exc
    config = kbuild_output / ".config"
    loader = kbuild_output / "vmlinux-gdb.py"
    scripts = kbuild_output / "scripts" / "gdb"
    if not config.is_file():
        raise BundleError(f"exact Kbuild configuration is missing: {config}")
    if not loader.is_file() or not scripts.is_dir():
        raise BundleError(
            "matching Linux GDB Python helpers are missing; build scripts_gdb in the exact tree"
        )
    if output.exists():
        raise BundleError(f"output already exists; choose a new bundle directory: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)

    make_argument = args.make or infer_recorded_tool(kbuild_output, "make")
    compiler_argument = args.compiler or infer_recorded_tool(
        kbuild_output, "compiler",
    )
    linker_argument = args.cross_ld or infer_recorded_tool(
        kbuild_output, "cross-ld",
    )
    objcopy_argument = args.objcopy or infer_recorded_tool(
        kbuild_output, "objcopy",
    )
    tools = {
        "make": resolve_tool(make_argument, "make"),
        "compiler": resolve_tool(compiler_argument, "compiler"),
        "linker": resolve_tool(linker_argument, "cross-ld"),
        "objcopy": resolve_tool(objcopy_argument, "objcopy"),
    }
    cross_compile = args.cross_compile
    if cross_compile is None:
        cross_compile = infer_cross_compile(
            Path(str(tools["compiler"]["path"])),
            Path(str(tools["linker"]["path"])),
            Path(str(tools["objcopy"]["path"])),
        )
    # CC is made absolute and appended last so the helper object cannot silently
    # select a same-named compiler from the host PATH. LLVM= and the caller's
    # other exact Kbuild assignments still select the matching assembler/tool
    # family and retain their normal kernel-build meaning.
    probe_make_args = effective_probe_make_args(
        requested_make_args, Path(str(tools["compiler"]["path"])),
    )
    staging = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent))
    try:
        copy_file(original_vmlinux, staging / "vmlinux", "vmlinux")
        copy_file(original_boot_image, staging / "kernel", "boot image")
        copy_file(config, staging / "kernel.config", "kernel configuration")
        copy_file(loader, staging / "vmlinux-gdb.py", "Linux GDB loader")
        shutil.copytree(scripts, staging / "scripts" / "gdb", symlinks=False)

        run_checked([
            sys.executable, str(SCRATCH_TOOL), str(staging / "vmlinux"),
            "--runtime-offset", hex(args.runtime_offset),
            "--output", str(staging / "scratch.json"),
        ], staging / "logs" / "scratch.log")
        scratch = json.loads((staging / "scratch.json").read_text(encoding="utf-8"))
        if scratch.get("arch") != architecture:
            raise BundleError(
                f"vmlinux scratch architecture is {scratch.get('arch')!r}, expected {architecture!r}"
            )
        code_gva = scratch["regions"]["code"]["gva"]
        init_task = defined_symbol(staging / "vmlinux", "init_task") + args.runtime_offset
        supplied_gpas = {
            name: getattr(args, f"{name}_gpa")
            for name in ("code", "data", "stack")
        }
        if architecture == "mmips" or all(value is None for value in supplied_gpas.values()):
            try:
                region_gpas = scratch_gpas(
                    architecture,
                    staging / "vmlinux",
                    staging / "kernel",
                    scratch,
                )
            except (FixedProfileError, OSError) as exc:
                raise BundleError(f"cannot derive fixed-profile scratch mappings: {exc}") from exc
        else:
            region_gpas = {name: int(value) for name, value in supplied_gpas.items()}

        build_command = probe_build_command(
            python=sys.executable,
            kbuild_output=kbuild_output,
            output=staging / "probe-build",
            architecture=architecture,
            cross_compile=cross_compile,
            make=str(tools["make"]["path"]),
            make_args=probe_make_args,
            vmlinux=staging / "vmlinux",
        )
        run_checked(build_command, staging / "logs" / "probe-build.log")
        run_checked([
            sys.executable, str(PROBE_TOOL), "package",
            str(staging / "probe-build" / "probe.json"),
            "--load-address", hex(code_gva),
            "--output-dir", str(staging / "probe-package"),
            "--cross-ld", str(tools["linker"]["path"]),
            "--objcopy", str(tools["objcopy"]["path"]),
        ], staging / "logs" / "probe-package.log")
        callgate = [
            sys.executable, str(PROBE_TOOL), "callgate-manifest",
            str(staging / "probe-package" / "package.json"),
            "--vmlinux", str(staging / "vmlinux"),
            "--scratch-regions", str(staging / "scratch.json"),
            "--cpu", str(args.cpu),
            "--init-task", hex(init_task),
            "--timeout-seconds", str(args.timeout_seconds),
            "--output", str(staging / "callgate.json"),
        ]
        if architecture != "mmips":
            for name in ("code", "data", "stack"):
                callgate.extend((f"--{name}-gpa", hex(region_gpas[name])))
        if args.pstate is not None:
            callgate.extend(("--pstate", hex(args.pstate)))
        run_checked(callgate, staging / "logs" / "callgate.log")

        write_bundle_manifest(
            staging,
            architecture=architecture,
            kbuild_output=kbuild_output,
            cross_compile=cross_compile,
            requested_make_args=requested_make_args,
            probe_make_args=probe_make_args,
            toolchain=tools,
            original_vmlinux=original_vmlinux,
            original_boot_image=original_boot_image,
            runtime_offset=args.runtime_offset,
        )
        # All call-gate references are relative to this directory. Moving the
        # complete staging directory therefore preserves the validation which
        # callgate-manifest performed immediately before publication.
        os.replace(staging, output)
        return output
    except Exception:
        if staging.exists():
            shutil.rmtree(staging)
        raise


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--arch", required=True, choices=tuple(ARCH_TO_KBUILD))
    result.add_argument("--kbuild-output", type=Path, required=True)
    result.add_argument(
        "--vmlinux", type=Path,
        help="defaults to VMLINUX from the exact Kbuild output",
    )
    result.add_argument(
        "--boot-image", type=Path,
        help="defaults to the fixed architecture profile's standard Kbuild artifact",
    )
    result.add_argument("--output-dir", type=Path, required=True)
    result.add_argument(
        "--cross-compile",
        help=(
            "exact CROSS_COMPILE string (may be empty); omitted only when the "
            "recorded GNU tool names establish it"
        ),
    )
    result.add_argument(
        "--make", help="exact make executable; inferred from a literal Kbuild record",
    )
    result.add_argument(
        "--compiler",
        help="exact compiler executable; inferred from ViroS Kbuild object records",
    )
    result.add_argument(
        "--cross-ld", help="exact final linker; inferred from the vmlinux command record",
    )
    result.add_argument(
        "--objcopy", help="exact objcopy executable; inferred from Kbuild records",
    )
    result.add_argument(
        "--make-arg", action="append", default=[],
        help="repeat an exact Kbuild assignment such as LLVM=-21",
    )
    for name in ("code", "data", "stack"):
        result.add_argument(f"--{name}-gpa", type=lambda value: int(value, 0))
    result.add_argument("--runtime-offset", type=lambda value: int(value, 0), default=0)
    result.add_argument("--cpu", type=lambda value: int(value, 0), default=0)
    result.add_argument("--pstate", type=lambda value: int(value, 0))
    result.add_argument("--timeout-seconds", type=float, default=1.0)
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    if args.arch == "mmips":
        supplied = [name for name in ("code", "data", "stack")
                    if getattr(args, f"{name}_gpa") is not None]
        if supplied:
            raise BundleError(
                "MMIPS scratch GPAs are derived from KSEG0; omit explicit GPA options"
            )
    else:
        supplied = [name for name in ("code", "data", "stack")
                    if getattr(args, f"{name}_gpa") is not None]
        if supplied and len(supplied) != 3:
            raise BundleError(
                "fixed-profile scratch mappings are derived automatically; "
                "an explicit override must supply all three"
            )
    output = build_bundle(args)
    print(f"Built portable kernel debugger bundle: {output}")
    print(f"Call-gate manifest: {output / 'callgate.json'}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BundleError as exc:
        print(f"kernel-bundle: {exc}", file=sys.stderr)
        raise SystemExit(1)
