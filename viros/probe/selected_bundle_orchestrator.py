#!/usr/bin/env python3
"""Plan internal debugger bundles from exact selected boot payloads.

This is deliberately not a command-line publisher.  Sarun supplies one typed
combined-image or kernel/initramfs request, an exact attachment-backed
:class:`ArtifactSource`, and a new output location.  The finite provenance
catalog is the only set of files this module may inspect for debugger and
tooling resources.

The current kernel bundle builder needs a filesystem-shaped Kbuild tree and
runnable tool paths.  Captured regular-file identities do not encode either a
tree mount or executable invocations.  Consequently this module does not turn
provider-relative names into host paths.  It produces the complete userspace
derivation plus a typed kernel build plan, including explicit requirements for
the adapter and for any absent Kbuild evidence.  Once a catalog-backed Kbuild
executor exists, that plan is sufficient to feed ``kernel_bundle.build_bundle``
and then write the two existing bundle formats without adding selectors.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from enum import Enum
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import tempfile
from typing import Callable, Mapping, Sequence

from probe.image_inspector import CapturedArtifact
from probe import image_bundle, kernel_bundle
from probe.kernel_bundle import BUNDLE_FORMAT, STANDARD_BOOT_IMAGES
from probe.newc_userspace import (
    DebugElfCandidate,
    NewcCatalogError,
    UserspaceCatalog,
    catalog_newc_userspace,
    select_kernel_init,
)
from probe.provider_derivation import (
    ArtifactSource,
    ProviderDerivationError,
    SelectedImageRequest,
    SelectedKernelInitramfsRequest,
    derive_selected_image,
    derive_selected_kernel_initramfs,
    read_verified_artifact,
)


PLAN_FORMAT = "viros-selected-bundle-plan-v1"
RESULT_FORMAT = "viros-selected-bundle-orchestration-v1"
IMAGE_BUNDLE_FORMAT = "viros-image-bundle-v1"


class FixedBootProfile(Enum):
    """Closed set of Sarun/QEMU machine profiles accepted by this executor."""

    X86_64 = ("x86_64", "microvm-initramfs-x86_64-v1")
    AARCH64 = ("aarch64", "virt-initramfs-aarch64-v1")
    ARM = ("arm", "virt-initramfs-arm-v1")
    MMIPS = ("mmips", "malta-initramfs-mipsel-v1")

    @property
    def architecture(self) -> str:
        return self.value[0]

    @property
    def profile(self) -> str:
        return self.value[1]

    @classmethod
    def for_architecture(cls, architecture: str) -> "FixedBootProfile":
        for row in cls:
            if row.architecture == architecture:
                return row
        raise SelectedBundleError(
            f"no selected-bundle fixed profile for architecture {architecture!r}"
        )


_SUPPORTED_ARCHITECTURES = frozenset(row.architecture for row in FixedBootProfile)
_KERNEL_ELF_IDENTITIES = {
    "aarch64": (2, 1, 183),
    "arm": (1, 1, 40),
    "mmips": (1, 1, 8),
    "x86_64": (2, 1, 62),
}
_BOOT_COMMANDS = {
    "aarch64": "arch/arm64/boot/.Image.cmd",
    "arm": "arch/arm/boot/.zImage.cmd",
    "mmips": ".vmlinux.cmd",
    "x86_64": "arch/x86/boot/.bzImage.cmd",
}


class SelectedBundleError(RuntimeError):
    """The selected artifact or finite provenance catalog is inconsistent."""


_TOOL_LABELS = frozenset(("make", "compiler", "cross-ld", "objcopy"))
SelectedBootRequest = SelectedImageRequest | SelectedKernelInitramfsRequest


@dataclass(frozen=True)
class CatalogExecutable:
    """Exact provenance binding for one argv[0] executable identity.

    CapturedArtifact describes regular-file contents but intentionally has no
    Unix mode.  This extra bit of execution metadata is therefore carried only
    for the four Kbuild tools, not added to every captured file.
    """

    label: str
    argv0: str
    artifact: CapturedArtifact
    mode: int = 0o700

    def __post_init__(self) -> None:
        if self.label not in _TOOL_LABELS:
            raise SelectedBundleError(f"unknown catalog executable label: {self.label!r}")
        if (
            not self.argv0
            or "\x00" in self.argv0
            or "\n" in self.argv0
            or Path(self.argv0).name != Path(self.artifact.path).name
        ):
            raise SelectedBundleError(
                f"catalog executable {self.label} does not preserve recorded argv[0] identity"
            )
        if self.argv0.startswith("/") and self.artifact.path != self.argv0[1:]:
            raise SelectedBundleError(
                f"catalog executable {self.label} does not preserve its absolute argv[0] path"
            )
        if (
            isinstance(self.mode, bool)
            or not isinstance(self.mode, int)
            or self.mode & ~0o777
            or not self.mode & 0o111
        ):
            raise SelectedBundleError("catalog executable mode must contain an execute bit")


@dataclass(frozen=True)
class SelectedBundleExecutionRequest:
    selected_boot: SelectedBootRequest
    executables: tuple[CatalogExecutable, ...]
    fixed_profile: FixedBootProfile | None = None

    def __post_init__(self) -> None:
        if not isinstance(
            self.selected_boot, (SelectedImageRequest, SelectedKernelInitramfsRequest)
        ):
            raise SelectedBundleError("execution request has no typed boot selection")
        if not isinstance(self.executables, tuple) or any(
            not isinstance(row, CatalogExecutable) for row in self.executables
        ):
            raise SelectedBundleError("execution request executables have the wrong type")
        if self.fixed_profile is not None and not isinstance(
            self.fixed_profile, FixedBootProfile
        ):
            raise SelectedBundleError("execution request has an unsupported fixed profile")
        labels = [row.label for row in self.executables]
        if len(labels) != len(set(labels)):
            raise SelectedBundleError("execution request repeats a tool label")


@dataclass(frozen=True)
class MissingRequirement:
    code: str
    detail: str
    expected: tuple[str, ...] = ()

    def descriptor(self) -> dict[str, object]:
        result: dict[str, object] = {"code": self.code, "detail": self.detail}
        if self.expected:
            result["expected"] = list(self.expected)
        return result


@dataclass(frozen=True)
class SelectedBundlePlan:
    fixed_profile: FixedBootProfile
    selected_derivation: Mapping[str, object]
    initramfs: Mapping[str, object]
    kernel_init: str
    userspace: UserspaceCatalog
    kernel_inputs: Mapping[str, object]
    recorded_tools: Mapping[str, tuple[str, ...]]
    missing_requirements: tuple[MissingRequirement, ...]

    @property
    def ready(self) -> bool:
        return not self.missing_requirements

    def descriptor(self) -> dict[str, object]:
        return {
            "format": PLAN_FORMAT,
            "architecture": self.fixed_profile.architecture,
            "profile": self.fixed_profile.profile,
            "ready": self.ready,
            "selected_derivation": dict(self.selected_derivation),
            "initramfs": dict(self.initramfs),
            "kernel_init": self.kernel_init,
            "userspace": self.userspace.descriptor(),
            "kernel_bundle": {
                "format": BUNDLE_FORMAT,
                "output": "kernel-bundle",
                "inputs": dict(self.kernel_inputs),
                "recorded_tools": {
                    label: list(values)
                    for label, values in sorted(self.recorded_tools.items())
                },
            },
            "image_bundle": {
                "format": IMAGE_BUNDLE_FORMAT,
                "output": "image-bundle",
                "kernel_bundle": "kernel-bundle/bundle.json",
                "initramfs": "selected-image/" + str(self.initramfs["path"]),
                "init": self.kernel_init,
            },
            "missing_requirements": [
                requirement.descriptor()
                for requirement in self.missing_requirements
            ],
        }


@dataclass(frozen=True)
class SelectedBundleResult:
    output_root: Path
    plan: SelectedBundlePlan

    def descriptor(self) -> dict[str, object]:
        return {
            "format": RESULT_FORMAT,
            "output": str(self.output_root),
            "plan": self.plan.descriptor(),
        }


def _artifact_key(row: CapturedArtifact) -> tuple[int, str, str]:
    return row.box_id, row.path, row.record_id


def _catalog(request: SelectedBootRequest) -> tuple[CapturedArtifact, ...]:
    return request.captured_artifacts


def _selected_payloads(request: SelectedBootRequest) -> tuple[CapturedArtifact, ...]:
    if isinstance(request, SelectedKernelInitramfsRequest):
        return request.kernel, request.initramfs
    return (request.selected,)


def _resolve_fixed_profile(
    request: SelectedBootRequest,
    supplied: FixedBootProfile | None,
) -> FixedBootProfile:
    """Select one profile and reject every contradictory tagged artifact."""

    tagged = {
        row.architecture
        for row in (*_selected_payloads(request), *_catalog(request))
        if row.architecture is not None
    }
    unsupported = sorted(tagged - _SUPPORTED_ARCHITECTURES)
    if unsupported:
        raise SelectedBundleError(
            "selected provenance names unsupported architecture(s): "
            + ", ".join(unsupported)
        )
    if supplied is None:
        if len(tagged) != 1:
            raise SelectedBundleError(
                "selected provenance does not identify exactly one fixed architecture; "
                "supply a FixedBootProfile explicitly"
            )
        supplied = FixedBootProfile.for_architecture(next(iter(tagged)))
    mismatches = sorted(tagged - {supplied.architecture})
    if mismatches:
        raise SelectedBundleError(
            f"selected provenance architecture(s) {', '.join(mismatches)} are "
            f"incompatible with fixed profile {supplied.profile}"
        )
    return supplied


def _unique_content(
    rows: Sequence[CapturedArtifact], label: str
) -> tuple[CapturedArtifact | None, MissingRequirement | None]:
    if not rows:
        return None, MissingRequirement(
            f"missing-{label}", f"provenance contains no exact {label} artifact"
        )
    by_content: dict[tuple[int, str], list[CapturedArtifact]] = {}
    for row in rows:
        by_content.setdefault((row.size, row.sha256), []).append(row)
    if len(by_content) != 1:
        return None, MissingRequirement(
            f"ambiguous-{label}",
            f"provenance contains multiple different {label} contents",
            tuple(
                f"box {row.box_id}:{row.path}@{row.sha256}"
                for row in sorted(rows, key=_artifact_key)
            ),
        )
    copies = next(iter(by_content.values()))
    return min(copies, key=_artifact_key), None


def _row_descriptor(row: CapturedArtifact | None) -> dict[str, object] | None:
    return None if row is None else row.descriptor()


def _under(root: str, relative: str) -> str:
    return f"{root}/{relative}" if root else relative


def _catalog_location(
    rows: Mapping[tuple[int, str], CapturedArtifact], box_id: int, path: str
) -> CapturedArtifact | None:
    return rows.get((box_id, path))


_TOOL_PATTERNS = {
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


def _command_words(contents: bytes) -> list[str]:
    text = contents.decode("utf-8", errors="replace")
    for line in text.splitlines():
        match = re.match(r"^cmd_[^ ]+\s*:?=\s*(.*)$", line)
        if match is None:
            continue
        try:
            return shlex.split(match.group(1), posix=True)
        except ValueError:
            return []
    return []


def _recorded_tools(command_records: Sequence[tuple[str, bytes]]) -> dict[str, tuple[str, ...]]:
    result: dict[str, tuple[str, ...]] = {}
    for label, matcher in _TOOL_PATTERNS.items():
        candidates: set[str] = set()
        for path, contents in command_records:
            if label == "compiler" and not path.endswith(
                ("kernel/viros/.viros_event.o.cmd", "kernel/viros/.viros_scratch.o.cmd")
            ):
                continue
            words = _command_words(contents)
            if label == "compiler":
                words = words[:1]
            for word in words:
                if matcher.fullmatch(Path(word).name):
                    candidates.add(word)
        result[label] = tuple(sorted(candidates))
    return result


def _validate_kernel_inputs(
    fixed_profile: FixedBootProfile, vmlinux: bytes, boot: bytes
) -> None:
    architecture = fixed_profile.architecture
    elf_class, elf_data, machine = _KERNEL_ELF_IDENTITIES[architecture]
    if (
        len(vmlinux) < 20
        or vmlinux[:4] != b"\x7fELF"
        or vmlinux[4] != elf_class
        or vmlinux[5] != elf_data
        or vmlinux[6] != 1
        or int.from_bytes(vmlinux[18:20], "little") != machine
    ):
        width = 64 if elf_class == 2 else 32
        raise SelectedBundleError(
            f"captured {architecture} vmlinux role does not identify a "
            f"little-endian ELF{width} machine {machine} image"
        )

    if architecture == "x86_64":
        valid_boot = (
            len(boot) >= 0x238
            and boot[0x202:0x206] == b"HdrS"
            and bool(int.from_bytes(boot[0x236:0x238], "little") & 1)
        )
        detail = "64-bit Linux bzImage"
    elif architecture == "aarch64":
        valid_boot = len(boot) >= 64 and boot[56:60] == b"ARM\x64"
        detail = "uncompressed AArch64 Linux Image"
    elif architecture == "arm":
        valid_boot = (
            len(boot) >= 0x28
            and int.from_bytes(boot[0x24:0x28], "little") == 0x016F2818
        )
        detail = "ARM Linux zImage"
    else:
        valid_boot = (
            len(boot) >= 20
            and boot[:7] == b"\x7fELF\x01\x01\x01"
            and int.from_bytes(boot[18:20], "little") == 8
        )
        detail = "little-endian ELF32 MIPS kernel"
    if not valid_boot:
        raise SelectedBundleError(
            f"captured {architecture} kernel-boot role does not identify a {detail}"
        )


def _kernel_plan(
    request: SelectedBootRequest,
    source: ArtifactSource,
    fixed_profile: FixedBootProfile,
    *,
    selected_boot_image: CapturedArtifact | None = None,
) -> tuple[dict[str, object], dict[str, tuple[str, ...]], list[MissingRequirement]]:
    rows = _catalog(request)
    architecture = fixed_profile.architecture
    missing: list[MissingRequirement] = []
    vmlinux, issue = _unique_content(
        [row for row in rows if "vmlinux" in row.roles and row.architecture == architecture],
        f"{architecture}-vmlinux",
    )
    if issue is not None:
        missing.append(issue)

    if selected_boot_image is None:
        boot, issue = _unique_content(
            [
                row
                for row in rows
                if "kernel-boot" in row.roles and row.architecture == architecture
            ],
            f"{architecture}-kernel-boot",
        )
        if issue is not None:
            missing.append(issue)
    else:
        boot = selected_boot_image
        if boot.architecture not in {None, architecture}:
            raise SelectedBundleError(
                f"selected kernel architecture is incompatible with {fixed_profile.profile}"
            )
    if vmlinux is not None and boot is not None:
        _validate_kernel_inputs(
            fixed_profile,
            read_verified_artifact(source, vmlinux),
            read_verified_artifact(source, boot),
        )

    root: str | None = None
    box_id: int | None = None
    if vmlinux is not None:
        if vmlinux.path == "vmlinux":
            root = ""
        elif vmlinux.path.endswith("/vmlinux"):
            root = vmlinux.path[: -len("/vmlinux")]
        else:
            missing.append(MissingRequirement(
                "unrooted-kbuild-output",
                "the exact vmlinux path does not identify a Kbuild output root",
                (vmlinux.path,),
            ))
        box_id = vmlinux.box_id

    by_location = {(row.box_id, row.path): row for row in rows}
    required: dict[str, CapturedArtifact | None] = {}
    command_records: list[tuple[str, bytes]] = []
    if root is not None and box_id is not None:
        expected_boot = _under(root, STANDARD_BOOT_IMAGES[architecture].as_posix())
        if (
            selected_boot_image is None
            and boot is not None
            and (boot.box_id != box_id or boot.path != expected_boot)
        ):
            missing.append(MissingRequirement(
                "kernel-boot-outside-kbuild-output",
                f"the fixed {architecture} boot image is not the standard artifact in the vmlinux Kbuild output",
                (f"box {box_id}:{expected_boot}",),
            ))
        required_paths = {
            "config": ".config",
            "gdb_loader": "vmlinux-gdb.py",
            "scratch_command": "kernel/viros/.viros_scratch.o.cmd",
            "event_command": "kernel/viros/.viros_event.o.cmd",
            "vmlinux_command": ".vmlinux.cmd",
            "boot_command": _BOOT_COMMANDS[architecture],
        }
        for label, relative in required_paths.items():
            expected = _under(root, relative)
            row = _catalog_location(by_location, box_id, expected)
            required[label] = row
            if row is None:
                missing.append(MissingRequirement(
                    f"missing-kbuild-{label.replace('_', '-')}",
                    f"the exact Kbuild provenance does not contain {relative}",
                    (f"box {box_id}:{expected}",),
                ))
            elif label.endswith("command"):
                command_records.append((relative, read_verified_artifact(source, row)))
        scripts = sorted(
            (
                row for row in rows
                if row.box_id == box_id
                and row.path.startswith(_under(root, "scripts/gdb/") if root else "scripts/gdb/")
            ),
            key=_artifact_key,
        )
        if not scripts:
            missing.append(MissingRequirement(
                "missing-kbuild-gdb-scripts",
                "the exact Kbuild provenance contains no scripts/gdb helper files",
                (f"box {box_id}:{_under(root, 'scripts/gdb/')}*",),
            ))
    else:
        required = {}
        scripts = []

    tools = _recorded_tools(command_records)
    for label, values in tools.items():
        if len(values) != 1:
            missing.append(MissingRequirement(
                f"missing-recorded-{label}",
                f"Kbuild command evidence must identify exactly one {label} argv[0]",
                values,
            ))

    # This is the one capability the existing path-based builder cannot infer
    # from CapturedArtifact.  Keeping it explicit prevents a provider-relative
    # path from silently becoming a host installation path.
    missing.append(MissingRequirement(
        "catalog-backed-kbuild-executor",
        "kernel_bundle.build_bundle requires a mounted Kbuild tree and runnable recorded tools; the selected-image catalog currently supplies immutable file identities only",
    ))

    inputs: dict[str, object] = {
        "kbuild_output": None if root is None or box_id is None else {
            "box_id": box_id,
            "path": root,
        },
        "vmlinux": _row_descriptor(vmlinux),
        "boot_image": _row_descriptor(boot),
        "required_artifacts": {
            label: _row_descriptor(row) for label, row in sorted(required.items())
        },
        "gdb_scripts": [row.descriptor() for row in scripts],
    }
    return inputs, tools, missing


def _debug_candidates(
    request: SelectedBootRequest, source: ArtifactSource
) -> tuple[DebugElfCandidate, ...]:
    candidates: list[DebugElfCandidate] = []
    selected_locations = {
        (row.box_id, row.path) for row in _selected_payloads(request)
    }
    for row in sorted(_catalog(request), key=_artifact_key):
        if (row.box_id, row.path) in selected_locations or row.size < 20:
            continue
        contents = read_verified_artifact(source, row)
        if not contents.startswith(b"\x7fELF"):
            continue
        candidates.append(DebugElfCandidate(row, contents))
    return tuple(candidates)


def _materialized_initramfs(
    result: Mapping[str, object], selected_root: Path
) -> tuple[dict[str, object], bytes]:
    rows = result.get("materialized_components")
    if not isinstance(rows, list):
        raise SelectedBundleError("selected image derivation returned no components")
    initramfs = [
        row for row in rows
        if isinstance(row, Mapping) and row.get("role") == "initramfs"
    ]
    if len(initramfs) != 1:
        raise SelectedBundleError(
            "selected image must materialize exactly one initramfs component"
        )
    row = dict(initramfs[0])
    path = row.get("path")
    if not isinstance(path, str):
        raise SelectedBundleError("materialized initramfs has no relative path")
    try:
        contents = selected_root.joinpath(*path.split("/")).read_bytes()
    except OSError as exc:
        raise SelectedBundleError(f"cannot read materialized initramfs: {exc}") from exc
    return row, contents


def orchestrate_selected_initramfs(
    request: SelectedBootRequest,
    source: ArtifactSource,
    output_root: Path,
    *,
    fixed_profile: FixedBootProfile | None = None,
) -> SelectedBundleResult:
    """Derive a fixed-profile combined-image or kernel/initramfs plan."""

    output_root = Path(output_root)
    if output_root.exists():
        raise SelectedBundleError("selected bundle orchestration output already exists")
    if not output_root.parent.is_dir():
        raise SelectedBundleError("selected bundle orchestration parent is unavailable")
    staging = Path(tempfile.mkdtemp(prefix=f".{output_root.name}.", dir=output_root.parent))
    complete = False
    try:
        selected_root = staging / "selected-image"
        if isinstance(request, SelectedKernelInitramfsRequest):
            derivation_result = derive_selected_kernel_initramfs(
                request, source, selected_root
            )
        else:
            derivation_result = derive_selected_image(request, source, selected_root)
        derivation = derivation_result.descriptor()
        selected_document = derivation.get("derivation")
        if not isinstance(selected_document, Mapping):
            raise SelectedBundleError("selected image has no derivation document")
        if selected_document.get("layout") not in {
            "cpio-newc",
            "gzip-cpio-newc",
            "selected-kernel-initramfs",
        }:
            raise SelectedBundleError(
                "the initial selected-image vertical accepts only cpio newc or cpio.gz"
            )
        initramfs, initramfs_bytes = _materialized_initramfs(derivation, selected_root)
        try:
            kernel_init = select_kernel_init(initramfs_bytes)
            selected_profile = _resolve_fixed_profile(request, fixed_profile)
            userspace = catalog_newc_userspace(
                initramfs_bytes, _debug_candidates(request, source)
            )
        except NewcCatalogError as exc:
            raise SelectedBundleError(str(exc)) from exc
        kernel_inputs, tools, missing = _kernel_plan(
            request,
            source,
            selected_profile,
            selected_boot_image=(
                request.kernel
                if isinstance(request, SelectedKernelInitramfsRequest)
                else None
            ),
        )
        plan = SelectedBundlePlan(
            fixed_profile=selected_profile,
            selected_derivation=selected_document,
            initramfs=initramfs,
            kernel_init=kernel_init,
            userspace=userspace,
            kernel_inputs=kernel_inputs,
            recorded_tools=tools,
            missing_requirements=tuple(missing),
        )
        with (staging / "bundle-plan.json").open("x", encoding="utf-8") as stream:
            json.dump(plan.descriptor(), stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(staging, output_root)
        complete = True
        return SelectedBundleResult(output_root, plan)
    except (ProviderDerivationError, OSError) as exc:
        raise SelectedBundleError(str(exc)) from exc
    finally:
        if not complete:
            shutil.rmtree(staging, ignore_errors=True)


def _private_artifact_path(root: Path, row: CapturedArtifact) -> Path:
    return root / "boxes" / str(row.box_id) / Path(*row.path.split("/"))


def _materialize_catalog(
    request: SelectedBootRequest, source: ArtifactSource, root: Path
) -> dict[tuple[int, str], Path]:
    locations: dict[tuple[int, str], CapturedArtifact] = {}
    for row in (*_selected_payloads(request), *_catalog(request)):
        key = (row.box_id, row.path)
        previous = locations.get(key)
        if previous is not None and (
            previous.size != row.size or previous.sha256 != row.sha256
        ):
            raise SelectedBundleError(
                f"catalog location has conflicting contents: box {row.box_id}:{row.path}"
            )
        locations[key] = row
    result: dict[tuple[int, str], Path] = {}
    for key, row in sorted(locations.items()):
        destination = _private_artifact_path(root, row)
        destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        contents = read_verified_artifact(source, row)
        with destination.open("xb") as stream:
            stream.write(contents)
        destination.chmod(0o600)
        result[key] = destination
    return result


def _elf_has_interpreter(contents: bytes) -> bool:
    if len(contents) < 20 or contents[:4] != b"\x7fELF":
        raise SelectedBundleError(
            "catalog executable loader metadata is missing: tool is not a native ELF file"
        )
    elf_class = contents[4]
    endian = {1: "little", 2: "big"}.get(contents[5])
    if elf_class not in {1, 2} or endian is None:
        raise SelectedBundleError("catalog executable has an unsupported ELF identity")
    if elf_class == 1:
        if len(contents) < 52:
            raise SelectedBundleError("catalog executable has a truncated ELF header")
        phoff = int.from_bytes(contents[28:32], endian)
        phentsize = int.from_bytes(contents[42:44], endian)
        phnum = int.from_bytes(contents[44:46], endian)
    else:
        if len(contents) < 64:
            raise SelectedBundleError("catalog executable has a truncated ELF header")
        phoff = int.from_bytes(contents[32:40], endian)
        phentsize = int.from_bytes(contents[54:56], endian)
        phnum = int.from_bytes(contents[56:58], endian)
    if phnum and phentsize < 4:
        raise SelectedBundleError("catalog executable has invalid program headers")
    if phoff > len(contents) or phnum > (len(contents) - phoff) // max(phentsize, 1):
        raise SelectedBundleError("catalog executable program headers leave the file")
    return any(
        int.from_bytes(
            contents[phoff + index * phentsize : phoff + index * phentsize + 4],
            endian,
        ) == 3
        for index in range(phnum)
    )


def _bind_executables(
    execution: SelectedBundleExecutionRequest,
    plan: SelectedBundlePlan,
    source: ArtifactSource,
    materialized: Mapping[tuple[int, str], Path],
) -> dict[str, Path]:
    catalog = {
        (row.box_id, row.path, row.record_id): row
        for row in _catalog(execution.selected_boot)
    }
    bindings = {row.label: row for row in execution.executables}
    if set(bindings) != _TOOL_LABELS:
        absent = sorted(_TOOL_LABELS - set(bindings))
        raise SelectedBundleError(
            "catalog executable metadata is missing for: " + ", ".join(absent)
        )
    result: dict[str, Path] = {}
    for label in sorted(_TOOL_LABELS):
        binding = bindings[label]
        recorded = plan.recorded_tools.get(label, ())
        if recorded != (binding.argv0,):
            raise SelectedBundleError(
                f"catalog executable {label} does not match the exact Kbuild argv[0]"
            )
        key = (
            binding.artifact.box_id,
            binding.artifact.path,
            binding.artifact.record_id,
        )
        if catalog.get(key) != binding.artifact:
            raise SelectedBundleError(
                f"catalog executable {label} is outside selected-image provenance"
            )
        contents = read_verified_artifact(source, binding.artifact)
        if _elf_has_interpreter(contents):
            raise SelectedBundleError(
                f"catalog executable loader metadata is missing for {label}: "
                "the captured ELF names a dynamic interpreter but its private execution "
                "closure is not represented"
            )
        path = materialized[(binding.artifact.box_id, binding.artifact.path)]
        path.chmod(binding.mode)
        result[label] = path
    return result


def _kernel_arguments(
    plan: SelectedBundlePlan,
    materialized: Mapping[tuple[int, str], Path],
    tools: Mapping[str, Path],
    output: Path,
) -> argparse.Namespace:
    root = plan.kernel_inputs.get("kbuild_output")
    vmlinux = plan.kernel_inputs.get("vmlinux")
    boot = plan.kernel_inputs.get("boot_image")
    if not all(isinstance(row, Mapping) for row in (root, vmlinux, boot)):
        raise SelectedBundleError("kernel build plan has incomplete exact inputs")

    def captured_path(row: Mapping[str, object]) -> Path:
        box_id = row.get("box_id")
        path = row.get("path")
        if isinstance(box_id, bool) or not isinstance(box_id, int) or not isinstance(path, str):
            raise SelectedBundleError("kernel build plan contains a malformed artifact")
        try:
            return materialized[(box_id, path)]
        except KeyError as exc:
            raise SelectedBundleError(
                f"kernel build input is absent from private catalog: box {box_id}:{path}"
            ) from exc

    root_box = root.get("box_id")  # type: ignore[union-attr]
    root_path = root.get("path")  # type: ignore[union-attr]
    if isinstance(root_box, bool) or not isinstance(root_box, int) or not isinstance(root_path, str):
        raise SelectedBundleError("kernel build plan has a malformed Kbuild root")
    private_box = next(
        path.parents[len(Path(*artifact_path.split("/")).parts) - 1]
        for (box, artifact_path), path in materialized.items()
        if box == root_box
    )
    kbuild_output = private_box / Path(*root_path.split("/")) if root_path else private_box
    boot_image = captured_path(boot)  # type: ignore[arg-type]
    if plan.selected_derivation.get("layout") == "selected-kernel-initramfs":
        selected_dir = kbuild_output / ".viros-selected"
        selected_dir.mkdir(mode=0o700)
        selected_boot = selected_dir / "kernel"
        shutil.copyfile(boot_image, selected_boot)
        selected_boot.chmod(0o600)
        boot_image = selected_boot
    return argparse.Namespace(
        arch=plan.fixed_profile.architecture,
        kbuild_output=kbuild_output,
        vmlinux=captured_path(vmlinux),  # type: ignore[arg-type]
        boot_image=boot_image,
        output_dir=output,
        cross_compile=None,
        make=str(tools["make"]),
        compiler=str(tools["compiler"]),
        cross_ld=str(tools["cross-ld"]),
        objcopy=str(tools["objcopy"]),
        make_arg=[],
        code_gpa=None,
        data_gpa=None,
        stack_gpa=None,
        runtime_offset=0,
        cpu=0,
        pstate=None,
        timeout_seconds=1.0,
    )


def _write_internal_image_bundle(
    root: Path,
    kernel_root: Path,
    selected_root: Path,
    plan: SelectedBundlePlan,
    request: SelectedBootRequest,
    source: ArtifactSource,
) -> Path:
    if root.exists():
        raise SelectedBundleError("internal image bundle output already exists")
    root.mkdir(mode=0o700)
    shutil.copytree(kernel_root, root / "kernel", symlinks=False)
    initramfs_path = selected_root / Path(*str(plan.initramfs["path"]).split("/"))
    shutil.copy2(initramfs_path, root / "rootfs.cpio")
    (root / "symbols").mkdir()
    by_identity = {
        (row.box_id, row.path, row.record_id): row
        for row in _catalog(request)
    }
    executable_rows: list[dict[str, object]] = []
    copied: dict[str, str] = {}
    for source_row in plan.userspace.executables:
        row = dict(source_row)
        key = (row.pop("debug_box_id"), row["debug_elf"], row.pop("debug_record_id"))
        artifact = by_identity.get(key)  # type: ignore[arg-type]
        if artifact is None:
            raise SelectedBundleError("userspace debugger ELF left exact provenance")
        executable_identity = str(row["build_id"])
        destination = f"symbols/{executable_identity}.elf"
        contents = read_verified_artifact(source, artifact)
        prior = copied.get(executable_identity)
        if prior is None:
            with (root / destination).open("xb") as stream:
                stream.write(contents)
            copied[executable_identity] = artifact.sha256
        elif prior != artifact.sha256:
            raise SelectedBundleError(
                f"userspace executable identity {executable_identity} identifies "
                "different debugger contents"
            )
        row["debug_elf"] = destination
        executable_rows.append(row)
    executable_rows.sort(key=lambda row: str(row["guest_path"]))
    document = {
        "format": IMAGE_BUNDLE_FORMAT,
        "architecture": plan.fixed_profile.architecture,
        "boot": {
            "profile": plan.fixed_profile.profile,
            "kernel_bundle": "kernel/bundle.json",
            "initramfs": "rootfs.cpio",
            "init": plan.kernel_init,
        },
        "userspace": {"executables": executable_rows},
        "artifacts": image_bundle.artifact_rows(root),
    }
    with (root / "image.json").open("x", encoding="utf-8") as stream:
        json.dump(document, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    return root


def execute_selected_initramfs(
    execution: SelectedBundleExecutionRequest,
    source: ArtifactSource,
    output_root: Path,
    *,
    _kernel_builder: Callable[[argparse.Namespace], Path] = kernel_bundle.build_bundle,
) -> SelectedBundleResult:
    """Build both internal bundle formats from exact captured provenance.

    ``_kernel_builder`` is a test seam, not a selector: production callers use
    the existing kernel bundle implementation unconditionally.
    """

    output_root = Path(output_root)
    if output_root.exists() or not output_root.parent.is_dir():
        raise SelectedBundleError("selected bundle execution output is unavailable")
    transaction = Path(tempfile.mkdtemp(prefix=f".{output_root.name}.", dir=output_root.parent))
    work = transaction / "result"
    complete = False
    try:
        derived = orchestrate_selected_initramfs(
            execution.selected_boot,
            source,
            work,
            fixed_profile=execution.fixed_profile,
        )
        remaining = [
            row for row in derived.plan.missing_requirements
            if row.code != "catalog-backed-kbuild-executor"
        ]
        if remaining:
            raise SelectedBundleError(
                "kernel build plan is incomplete: "
                + ", ".join(row.code for row in remaining)
            )
        private = transaction / "private-catalog"
        materialized = _materialize_catalog(execution.selected_boot, source, private)
        tools = _bind_executables(execution, derived.plan, source, materialized)
        arguments = _kernel_arguments(
            derived.plan, materialized, tools, work / "kernel-bundle"
        )
        built = _kernel_builder(arguments)
        if built.resolve() != (work / "kernel-bundle").resolve():
            raise SelectedBundleError("kernel bundle builder returned an unexpected output")
        if not (built / "bundle.json").is_file():
            raise SelectedBundleError("kernel bundle builder emitted no bundle.json")
        _write_internal_image_bundle(
            work / "image-bundle",
            built,
            work / "selected-image",
            derived.plan,
            execution.selected_boot,
            source,
        )
        completed_plan = SelectedBundlePlan(
            fixed_profile=derived.plan.fixed_profile,
            selected_derivation=derived.plan.selected_derivation,
            initramfs=derived.plan.initramfs,
            kernel_init=derived.plan.kernel_init,
            userspace=derived.plan.userspace,
            kernel_inputs=derived.plan.kernel_inputs,
            recorded_tools=derived.plan.recorded_tools,
            missing_requirements=(),
        )
        plan_path = work / "bundle-plan.json"
        plan_path.write_text(
            json.dumps(completed_plan.descriptor(), indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        os.replace(work, output_root)
        complete = True
        return SelectedBundleResult(output_root, completed_plan)
    except (kernel_bundle.BundleError, image_bundle.ImageBundleError, OSError) as exc:
        raise SelectedBundleError(str(exc)) from exc
    finally:
        shutil.rmtree(transaction, ignore_errors=True)
