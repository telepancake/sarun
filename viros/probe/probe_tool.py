#!/usr/bin/env python3
"""Build and audit a viros frozen-kernel probe object."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tempfile


PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from callgate.manifest import ManifestError, load_and_validate_manifest


ET_REL = 1
ET_EXEC = 2
EM_MIPS = 8
EM_AARCH64 = 183
SHN_UNDEF = 0
SHT_SYMTAB = 2
SHT_DYNSYM = 11
SHT_RELA = 4
SHT_NOBITS = 8
SHT_REL = 9
SHT_NOTE = 7
SHF_WRITE = 0x1
SHF_ALLOC = 0x2
SHF_EXECINSTR = 0x4
SHF_MIPS_GPREL = 0x10000000

SHT_MIPS_REGINFO = 0x70000006
SHT_MIPS_ABIFLAGS = 0x7000002A

EF_MIPS_NOREORDER = 0x00000001
EF_MIPS_PIC = 0x00000002
EF_MIPS_CPIC = 0x00000004
EF_MIPS_ABI2 = 0x00000020
EF_MIPS_MICROMIPS = 0x02000000
EF_MIPS_ARCH_ASE_M16 = 0x04000000
EF_MIPS_ABI_MASK = 0x0000F000
EF_MIPS_ABI_O32 = 0x00001000
EF_MIPS_ARCH_MASK = 0xF0000000
EF_MIPS_ARCH_32R2 = 0x70000000
MMIPS_ALLOWED_E_FLAGS = EF_MIPS_NOREORDER | EF_MIPS_ABI_O32 | EF_MIPS_ARCH_32R2

AFL_REG_NONE = 0
AFL_REG_32 = 1
VAL_GNU_MIPS_ABI_FP_SOFT = 3
MIPS_AFL_FLAGS1_ODDSPREG = 0x1

# Relocations which obtain addresses through the ABI global pointer or GOT are
# incompatible with a flat image injected without the normal MIPS loader.
MIPS_GP_RELOCATIONS = frozenset({
    7,   # R_MIPS_GPREL16
    8,   # R_MIPS_LITERAL
    9,   # R_MIPS_GOT16
    11,  # R_MIPS_CALL16
    12,  # R_MIPS_GPREL32
    19, 20, 21, 22, 23,  # R_MIPS_GOT_DISP/PAGE/OFST/HI16/LO16
    30, 31,  # R_MIPS_CALL_HI16/LO16
    *range(38, 51),  # All MIPS TLS forms depend on loader/runtime state.
})
MIPS_COMPACT_ISA_RELOCATIONS = frozenset({
    *range(100, 114),  # R_MIPS16_*
    *range(133, 174),  # R_MICROMIPS_*
})

PROBE_PACKAGE_SCHEMA = "viros-probe-package-v1"
PROBE_BUILD_SCHEMA = "viros-probe-build-v1"
SCRATCH_REGIONS_SCHEMA = "viros-scratch-regions-v1"
PROBE_REQUEST_MAGIC = 0x56505251
PROBE_RESPONSE_MAGIC = 0x56505253
PROBE_ABI_MAJOR = 1
PROBE_ABI_MINOR = 2
PROBE_REQUEST_SIZE = 64
PROBE_RESPONSE_SIZE = 64
PROBE_TASK_SIZE = 192
PROBE_TRANSLATION_SIZE = 64
PROBE_SAVED_REGS_SIZE = 304
PROBE_OP_SNAPSHOT = 1
UINT64_LIMIT = 1 << 64


class AuditError(RuntimeError):
    pass


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _vmlinux_release_evidence(vmlinux: Path) -> set[str]:
    patterns = (
        re.compile(rb"Linux version (\d+\.\d+\.\d+(?:[-._+A-Za-z0-9]*))"),
        re.compile(rb"/linux-(\d+\.\d+\.\d+(?:[-._+A-Za-z0-9]*))"),
    )
    evidence: set[str] = set()
    tail = b""
    with vmlinux.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            searchable = tail + block
            for pattern in patterns:
                evidence.update(
                    value.decode("ascii") for value in pattern.findall(searchable)
                )
            # Both witnesses are short.  Retaining a larger boundary makes a
            # token split between read blocks deterministic without holding a
            # debug vmlinux (often hundreds of MiB) in host memory.
            tail = searchable[-512:]
    return evidence


def _kernel_release_provenance(linux_dir: Path, vmlinux: Path) -> dict:
    """Prove that Kbuild and vmlinux describe the same kernel release.

    A matching file hash alone is insufficient: an unrelated vmlinux can be
    copied or symlinked into a configured output tree.  Exact-headers code is
    safe only when the output tree which compiles it belongs to that kernel.
    Normal vmlinux images expose ``Linux version`` in allocatable data; debug
    packages whose allocatable sections are NOBITS still retain source paths
    in DWARF.  Require at least one of those independent release witnesses.
    """

    release_path = linux_dir / "include" / "config" / "kernel.release"
    try:
        kbuild_release = release_path.read_text(encoding="ascii").strip()
    except (OSError, UnicodeError) as exc:
        raise AuditError(
            "exact Kbuild directory has no readable include/config/kernel.release"
        ) from exc
    match = re.match(r"^(\d+\.\d+\.\d+)", kbuild_release)
    if not match:
        raise AuditError(f"invalid Kbuild kernel release {kbuild_release!r}")
    kbuild_version = match.group(1)
    evidence = _vmlinux_release_evidence(vmlinux)
    if not evidence:
        raise AuditError(
            "cannot establish the supplied vmlinux kernel release from its "
            "Linux banner or debug source paths"
        )
    evidence_versions = {
        re.match(r"^\d+\.\d+\.\d+", item).group(0) for item in evidence
    }
    if evidence_versions != {kbuild_version}:
        raise AuditError(
            f"Kbuild release {kbuild_release} does not match supplied vmlinux "
            f"release evidence {', '.join(sorted(evidence))}"
        )
    source_link = linux_dir / "source"
    source_tree = source_link.resolve() if source_link.exists() else linux_dir.resolve()
    return {
        "kbuild_release": kbuild_release,
        "vmlinux_release_evidence": sorted(evidence),
        "source_tree": str(source_tree),
    }


def _manifest_integer(value, field: str) -> int:
    if isinstance(value, bool):
        raise AuditError(f"{field} must be an integer")
    if isinstance(value, int):
        result = value
    elif isinstance(value, str):
        try:
            result = int(value, 0)
        except ValueError as exc:
            raise AuditError(f"{field} is not an integer: {value!r}") from exc
    else:
        raise AuditError(f"{field} must be an integer")
    if result < 0:
        raise AuditError(f"{field} must not be negative")
    return result


def _required_string(value, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise AuditError(f"{field} must be a non-empty string")
    return value


def _sha256_string(value, field: str) -> str:
    text = _required_string(value, field)
    if len(text) != 64 or text != text.lower() or any(
        character not in "0123456789abcdef" for character in text
    ):
        raise AuditError(f"{field} must contain 64 lowercase hex digits")
    return text


def _build_id_string(value, field: str) -> str:
    text = _required_string(value, field)
    if not 8 <= len(text) <= 128 or text != text.lower() or any(
        character not in "0123456789abcdef" for character in text
    ):
        raise AuditError(f"{field} must contain 8-128 lowercase hex digits")
    return text


def _package_file(package_path: Path, value, field: str) -> Path:
    path = Path(_required_string(value, field))
    if not path.is_absolute():
        path = package_path.parent / path
    path = path.resolve()
    if not path.is_file():
        raise AuditError(f"{field} does not name a regular file: {path}")
    return path


def _write_json_atomic(document: dict, output: Path) -> None:
    """Publish JSON atomically using a temporary beside the output file."""

    output = output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            "w", encoding="utf-8", dir=output.parent,
            prefix=f".{output.name}.", suffix=".tmp", delete=False,
        ) as stream:
            temporary_path = Path(stream.name)
            json.dump(document, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary_path, output)
        temporary_path = None
    finally:
        if temporary_path is not None:
            try:
                temporary_path.unlink()
            except FileNotFoundError:
                pass


class ElfObject:
    def __init__(self, path: Path):
        self.path = path
        self.data = path.read_bytes()
        if len(self.data) < 64 or self.data[:4] != b"\x7fELF":
            raise AuditError(f"{path}: not an ELF file")
        elf_class, encoding = self.data[4], self.data[5]
        if elf_class not in (1, 2) or encoding not in (1, 2):
            raise AuditError(f"{path}: unsupported ELF class or byte order")
        self.elf_class = 32 if elf_class == 1 else 64
        self.endian = "<" if encoding == 1 else ">"
        fmt = self.endian + ("HHIIIIIHHHHHH" if elf_class == 1 else "HHIQQQIHHHHHH")
        values = struct.unpack_from(fmt, self.data, 16)
        self.elf_type, self.machine, self.entry = values[0], values[1], values[3]
        self.flags = values[6]
        self.shoff = values[5]
        self.shentsize, self.shnum, self.shstrndx = values[10:13]
        self.sections = self._read_sections()

    def _read_sections(self):
        fmt = self.endian + ("IIIIIIIIII" if self.elf_class == 32 else "IIQQQQIIQQ")
        minimum = struct.calcsize(fmt)
        if self.shentsize < minimum or self.shnum == 0:
            raise AuditError(f"{self.path}: invalid section table")
        raw = []
        for index in range(self.shnum):
            offset = self.shoff + index * self.shentsize
            if offset + minimum > len(self.data):
                raise AuditError(f"{self.path}: truncated section table")
            raw.append(struct.unpack_from(fmt, self.data, offset))
        if self.shstrndx >= len(raw):
            raise AuditError(f"{self.path}: invalid section-name table")
        names = self._section_bytes(raw[self.shstrndx])
        sections = []
        for index, item in enumerate(raw):
            name_offset = item[0]
            end = names.find(b"\0", name_offset)
            name = names[name_offset:end if end >= 0 else None].decode(errors="replace")
            sections.append({
                "index": index, "name": name, "type": item[1],
                "flags": item[2], "addr": item[3], "offset": item[4], "size": item[5],
                "link": item[6], "info": item[7], "addralign": item[8],
                "entsize": item[9],
                "raw": item,
            })
        return sections

    def _section_bytes(self, section):
        offset, size = section[4], section[5]
        if offset + size > len(self.data):
            raise AuditError(f"{self.path}: truncated section contents")
        return self.data[offset:offset + size]

    def symbol_records(self):
        result = []
        fmt = self.endian + ("IIIBBH" if self.elf_class == 32 else "IBBHQQ")
        size = struct.calcsize(fmt)
        for section in self.sections:
            if section["type"] not in (SHT_SYMTAB, SHT_DYNSYM):
                continue
            if section["link"] >= len(self.sections):
                raise AuditError(f"{self.path}: invalid symbol string table")
            strings_section = self.sections[section["link"]]["raw"]
            strings = self._section_bytes(strings_section)
            entsize = section["entsize"] or size
            if entsize < size:
                raise AuditError(f"{self.path}: invalid symbol entry size")
            contents = self._section_bytes(section["raw"])
            for offset in range(0, len(contents) - size + 1, entsize):
                item = struct.unpack_from(fmt, contents, offset)
                name_offset = item[0]
                end = strings.find(b"\0", name_offset)
                name = strings[name_offset:end if end >= 0 else None].decode(errors="replace")
                if self.elf_class == 32:
                    value, symbol_size, info, other, shndx = (
                        item[1], item[2], item[3], item[4], item[5]
                    )
                else:
                    info, other, shndx, value, symbol_size = (
                        item[1], item[2], item[3], item[4], item[5]
                    )
                result.append({"name": name, "shndx": shndx, "value": value,
                               "size": symbol_size, "binding": info >> 4,
                               "type": info & 0xf, "other": other})
        return result

    def symbols(self):
        return [(item["name"], item["shndx"]) for item in self.symbol_records()]

    def relocation_records(self):
        """Return relocation types and symbol indexes from REL/RELA sections."""

        result = []
        if self.elf_class == 32:
            formats = {SHT_REL: self.endian + "II", SHT_RELA: self.endian + "IIi"}
        else:
            formats = {SHT_REL: self.endian + "QQ", SHT_RELA: self.endian + "QQq"}
        for section in self.sections:
            if section["type"] not in formats:
                continue
            fmt = formats[section["type"]]
            size = struct.calcsize(fmt)
            entsize = section["entsize"] or size
            if entsize < size or section["size"] % entsize:
                raise AuditError(f"{self.path}: malformed relocation section {section['name']}")
            contents = self._section_bytes(section["raw"])
            for offset in range(0, len(contents), entsize):
                item = struct.unpack_from(fmt, contents, offset)
                info = item[1]
                if self.elf_class == 32:
                    symbol_index, relocation_type = info >> 8, info & 0xff
                else:
                    symbol_index, relocation_type = info >> 32, info & 0xffffffff
                result.append({
                    "section": section["name"], "symbol_index": symbol_index,
                    "type": relocation_type,
                })
        return result


def gnu_build_id(path: Path, arch: str = "aarch64") -> str:
    """Return the GNU build ID embedded in an ELF image for ``arch``."""

    elf = ElfObject(path)
    machines = {"aarch64": EM_AARCH64, "mmips": EM_MIPS}
    if arch not in machines:
        raise AuditError(f"build-ID support is not implemented for {arch}")
    if elf.machine != machines[arch]:
        raise AuditError(
            f"{path}: expected {arch} machine {machines[arch]}, got {elf.machine}"
        )
    identifiers: set[bytes] = set()
    for section in elf.sections:
        if section["type"] != SHT_NOTE or not section["size"]:
            continue
        notes = elf._section_bytes(section["raw"])
        offset = 0
        while offset < len(notes):
            if notes[offset:] == b"\0" * (len(notes) - offset):
                break
            if len(notes) - offset < 12:
                raise AuditError(f"{path}: truncated ELF note in {section['name']}")
            name_size, description_size, note_type = struct.unpack_from(
                elf.endian + "III", notes, offset
            )
            offset += 12
            name_end = offset + name_size
            description_offset = (name_end + 3) & ~3
            description_end = description_offset + description_size
            next_offset = (description_end + 3) & ~3
            if next_offset > len(notes):
                raise AuditError(f"{path}: truncated ELF note in {section['name']}")
            owner = notes[offset:name_end].rstrip(b"\0")
            description = notes[description_offset:description_end]
            if owner == b"GNU" and note_type == 3:
                if not 4 <= len(description) <= 64:
                    raise AuditError(f"{path}: GNU build ID has an unsupported size")
                identifiers.add(description)
            offset = next_offset
    if not identifiers:
        raise AuditError(f"{path}: no GNU build ID was found")
    if len(identifiers) != 1:
        raise AuditError(f"{path}: multiple conflicting GNU build IDs were found")
    return next(iter(identifiers)).hex()


def load_probe_build(path: Path) -> tuple[dict, Path]:
    """Load an exact-Kbuild record and verify its compiled probe object."""

    build_path = path.resolve()
    try:
        document = json.loads(build_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise AuditError(f"cannot read probe build manifest {build_path}: {exc}") from exc
    if not isinstance(document, dict):
        raise AuditError("probe build manifest must be a JSON object")
    if document.get("schema") != PROBE_BUILD_SCHEMA:
        raise AuditError(f"probe build schema must be {PROBE_BUILD_SCHEMA!r}")
    if document.get("arch") not in ("aarch64", "mmips"):
        raise AuditError("probe build arch must be aarch64 or mmips")
    probe_object = _package_file(build_path, document.get("object"), "build.object")
    expected = _sha256_string(document.get("object_sha256"), "build.object_sha256")
    actual = _sha256_file(probe_object)
    if actual != expected:
        raise AuditError(
            f"build object SHA-256 mismatch: expected {expected}, got {actual}"
        )
    kernel = document.get("kernel")
    if not isinstance(kernel, dict):
        raise AuditError("build.kernel must be an object")
    _sha256_string(kernel.get("sha256"), "build.kernel.sha256")
    _build_id_string(kernel.get("build_id"), "build.kernel.build_id")
    return document, probe_object


def load_probe_package(path: Path) -> tuple[dict, Path]:
    """Load a sealed package and verify its exact flat probe binary."""

    package_path = path.resolve()
    try:
        document = json.loads(package_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise AuditError(f"cannot read probe package {package_path}: {exc}") from exc
    if not isinstance(document, dict):
        raise AuditError("probe package must be a JSON object")
    if document.get("schema") != PROBE_PACKAGE_SCHEMA:
        raise AuditError(f"probe package schema must be {PROBE_PACKAGE_SCHEMA!r}")
    arch = document.get("arch")
    if arch not in ("aarch64", "mmips"):
        raise AuditError("probe package arch must be aarch64 or mmips")
    minor = document.get("abi_minor")
    if document.get("abi_major") != PROBE_ABI_MAJOR or minor not in (0, 1, PROBE_ABI_MINOR):
        raise AuditError("probe package ABI is not a supported viros probe ABI 1.x")
    layout = document.get("abi_layout")
    legacy_layout = (
        isinstance(layout, dict)
        and layout.get("request_bytes") == PROBE_REQUEST_SIZE
        and layout.get("response_header_bytes") == PROBE_RESPONSE_SIZE
        and layout.get("task_record_bytes") == PROBE_TASK_SIZE
        and layout.get("target_byte_order") == "little"
        and layout.get("translation_record_bytes", PROBE_TRANSLATION_SIZE)
        == PROBE_TRANSLATION_SIZE
    )
    translation_layout = layout == {
        "version": 1,
        "request_v1_bytes": PROBE_REQUEST_SIZE,
        "response_v1_header_bytes": PROBE_RESPONSE_SIZE,
        "task_v1_bytes": PROBE_TASK_SIZE,
        "translation_v1_bytes": PROBE_TRANSLATION_SIZE,
        "target_byte_order": "little",
    }
    current_layout = layout == {
        "version": 1,
        "request_v1_bytes": PROBE_REQUEST_SIZE,
        "response_v1_header_bytes": PROBE_RESPONSE_SIZE,
        "task_v1_bytes": PROBE_TASK_SIZE,
        "translation_v1_bytes": PROBE_TRANSLATION_SIZE,
        "saved_regs_v1_bytes": PROBE_SAVED_REGS_SIZE,
        "target_byte_order": "little",
    }
    capabilities = document.get("capabilities")
    mmips_layout = layout == {
        "version": 1,
        "request_v1_bytes": PROBE_REQUEST_SIZE,
        "response_v1_header_bytes": PROBE_RESPONSE_SIZE,
        "task_v1_bytes": PROBE_TASK_SIZE,
        "target_byte_order": "little",
    }
    if arch == "aarch64":
        if ((minor == 0 and not legacy_layout)
                or (minor == 1 and (
                    not translation_layout or capabilities != [
                        "snapshot-v1", "translate-va-aarch64-v1"
                    ]))
                or (minor == PROBE_ABI_MINOR and (
                    not current_layout or capabilities != [
                        "snapshot-v1", "translate-va-aarch64-v1",
                        "saved-regs-aarch64-v1",
                    ]))):
            raise AuditError("probe package has an incompatible ABI layout")
    elif (
        minor != PROBE_ABI_MINOR
        or not mmips_layout
        or capabilities != ["snapshot-v1"]
    ):
        raise AuditError("mmips probe package must advertise only snapshot-v1")
    if arch == "mmips" and document.get("elf_abi") != {
        "class": 32, "byte_order": "little", "machine": "EM_MIPS",
        "isa": "mips32r2", "abi": "o32", "float": "soft",
        "pic": False, "mips16": False, "micromips": False,
    }:
        raise AuditError("mmips probe package has incompatible ELF ABI metadata")
    call_abi = document.get("call_abi")
    if arch == "aarch64":
        valid_call_abi = (
            isinstance(call_abi, dict)
            and call_abi.get("name") == "aapcs64"
            and call_abi.get("argument_registers") == ["x0", "x1", "x2"]
            and call_abi.get("link_register") == "x30"
            and call_abi.get("stack_alignment") == 16
        )
        error = "probe package has an incompatible AArch64 call ABI"
    else:
        valid_call_abi = (
            isinstance(call_abi, dict)
            and call_abi.get("name") == "o32-soft-float"
            and call_abi.get("argument_registers") == ["r4", "r5", "r6"]
            and call_abi.get("result_register") == "r2"
            and call_abi.get("link_register") == "r31"
            and call_abi.get("stack_alignment") == 8
        )
        error = "probe package has an incompatible MMIPS o32 call ABI"
    if not valid_call_abi:
        raise AuditError(error)

    binary = _package_file(package_path, document.get("binary"), "package.binary")
    expected = _sha256_string(document.get("binary_sha256"), "package.binary_sha256")
    actual = _sha256_file(binary)
    if actual != expected:
        raise AuditError(
            f"package binary SHA-256 mismatch: expected {expected}, got {actual}"
        )
    size = _manifest_integer(document.get("image_size"), "package.image_size")
    if binary.stat().st_size != size:
        raise AuditError(
            f"package image size is {size}, but its binary is {binary.stat().st_size} bytes"
        )
    load_address = _manifest_integer(document.get("load_address"), "package.load_address")
    image_start = _manifest_integer(document.get("image_start"), "package.image_start")
    image_end = _manifest_integer(document.get("image_end"), "package.image_end")
    entry_offset = _manifest_integer(document.get("entry_offset"), "package.entry_offset")
    completion_offset = _manifest_integer(
        document.get("completion_offset"), "package.completion_offset"
    )
    address_limit = UINT64_LIMIT if arch == "aarch64" else 1 << 32
    alignment = 16 if arch == "aarch64" else 4
    if load_address >= address_limit or load_address & (alignment - 1):
        raise AuditError(
            f"package.load_address must be an aligned "
            f"{64 if arch == 'aarch64' else 32}-bit {arch} address"
        )
    if image_start >= address_limit or image_end > address_limit:
        raise AuditError(
            f"probe package linked image bounds do not fit in "
            f"{64 if arch == 'aarch64' else 32} bits"
        )
    if image_start != load_address or image_end != image_start + size:
        raise AuditError("probe package linked image bounds are inconsistent")
    if any(offset >= size or offset & 3 for offset in (entry_offset, completion_offset)):
        raise AuditError("probe package entry and completion offsets must select instructions")
    kernel = document.get("kernel")
    if not isinstance(kernel, dict):
        raise AuditError("package.kernel must be an object")
    _sha256_string(kernel.get("sha256"), "package.kernel.sha256")
    _build_id_string(kernel.get("build_id"), "package.kernel.build_id")
    return document, binary


def _relative_file(path: Path, base: Path) -> str:
    return os.path.relpath(path.resolve(), base.resolve())


def _write_validated_callgate(document: dict, output: Path) -> None:
    """Validate through the runtime loader before atomically publishing."""

    output = output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            "w", encoding="utf-8", dir=output.parent,
            prefix=f".{output.name}.", suffix=".tmp", delete=False,
        ) as stream:
            temporary_path = Path(stream.name)
            json.dump(document, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        load_and_validate_manifest(temporary_path)
        os.replace(temporary_path, output)
        temporary_path = None
    except ManifestError as exc:
        raise AuditError(f"generated call-gate manifest is invalid: {exc}") from exc
    finally:
        if temporary_path is not None:
            try:
                temporary_path.unlink()
            except FileNotFoundError:
                pass


def _load_scratch_regions(
    path: Path, expected_vmlinux_sha256: str, expected_build_id: str,
    expected_arch: str,
) -> tuple[Path, dict[str, dict[str, int]]]:
    """Load symbol-derived scratch GVAs and bind them to the exact kernel."""

    scratch_path = path.resolve()
    try:
        document = json.loads(scratch_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise AuditError(
            f"cannot read scratch regions document {scratch_path}: {exc}"
        ) from exc
    if not isinstance(document, dict):
        raise AuditError("scratch regions document must be a JSON object")
    if document.get("schema") != SCRATCH_REGIONS_SCHEMA:
        raise AuditError(
            f"scratch regions schema must be {SCRATCH_REGIONS_SCHEMA!r}"
        )
    if document.get("arch") != expected_arch:
        raise AuditError(
            f"scratch regions document must describe {expected_arch}"
        )
    kernel = document.get("vmlinux")
    if not isinstance(kernel, dict):
        raise AuditError("scratch regions vmlinux must be an object")
    scratch_sha256 = _sha256_string(
        kernel.get("sha256"), "scratch.vmlinux.sha256"
    )
    scratch_build_id = _build_id_string(
        kernel.get("build_id"), "scratch.vmlinux.build_id"
    )
    if scratch_sha256 != expected_vmlinux_sha256:
        raise AuditError(
            "scratch regions vmlinux SHA-256 does not match --vmlinux and the "
            f"probe package: expected {expected_vmlinux_sha256}, got {scratch_sha256}"
        )
    if scratch_build_id != expected_build_id:
        raise AuditError(
            "scratch regions vmlinux build ID does not match --vmlinux and the "
            f"probe package: expected {expected_build_id}, got {scratch_build_id}"
        )

    raw_regions = document.get("regions")
    if not isinstance(raw_regions, dict):
        raise AuditError("scratch regions must be an object")
    regions = {}
    for name in ("code", "data", "stack"):
        raw = raw_regions.get(name)
        if not isinstance(raw, dict):
            raise AuditError(f"scratch region {name} must be an object")
        gva = _manifest_integer(raw.get("gva"), f"scratch.regions.{name}.gva")
        size = _manifest_integer(raw.get("size"), f"scratch.regions.{name}.size")
        address_bits = 64 if expected_arch == "aarch64" else 32
        address_limit = 1 << address_bits
        if gva >= address_limit or size <= 0 or gva + size > address_limit:
            raise AuditError(
                f"scratch region {name} has invalid {address_bits}-bit bounds"
            )
        region = {"gva": gva, "size": size}
        if expected_arch == "mmips":
            gpa = _manifest_integer(
                raw.get("gpa"), f"scratch.regions.{name}.gpa"
            )
            if not 0x80000000 <= gva or gva + size > 0xa0000000:
                raise AuditError(
                    f"scratch region {name} must remain wholly in MMIPS KSEG0"
                )
            if gpa != gva - 0x80000000:
                raise AuditError(
                    f"scratch region {name} GPA does not match its KSEG0 mapping"
                )
            region["gpa"] = gpa
        regions[name] = region
    return scratch_path, regions


def create_callgate_manifest(args):
    """Bridge a sealed probe package into the strict runtime manifest."""

    package_path = Path(args.package).resolve()
    package, binary = load_probe_package(package_path)
    arch = package["arch"]
    address_bits = 64 if arch == "aarch64" else 32
    if (
        not isinstance(args.init_task, int) or isinstance(args.init_task, bool)
        or args.init_task <= 0 or args.init_task >= 1 << address_bits
    ):
        raise AuditError(
            f"init_task must be a nonzero {address_bits}-bit address"
        )
    vmlinux = Path(args.vmlinux).resolve()
    if not vmlinux.is_file():
        raise AuditError(f"vmlinux does not name a regular file: {vmlinux}")
    build_id = gnu_build_id(vmlinux, arch)
    vmlinux_sha256 = _sha256_file(vmlinux)
    package_kernel = package["kernel"]
    if package_kernel["sha256"] != vmlinux_sha256:
        raise AuditError(
            "vmlinux SHA-256 does not match the kernel bound to the probe package: "
            f"expected {package_kernel['sha256']}, got {vmlinux_sha256}"
        )
    if package_kernel["build_id"] != build_id:
        raise AuditError(
            "vmlinux build ID does not match the kernel bound to the probe package: "
            f"expected {package_kernel['build_id']}, got {build_id}"
        )

    scratch_argument = getattr(args, "scratch_regions", None)
    region_arguments = {
        name: {
            field: getattr(args, f"{name}_{field}", None)
            for field in ("gva", "gpa", "size")
        }
        for name in ("code", "data", "stack")
    }
    scratch_path = None
    if scratch_argument is not None:
        ambiguous = [
            f"--{name}-{field}"
            for name, values in region_arguments.items()
            for field in ("gva", "size")
            if values[field] is not None
        ]
        if ambiguous:
            raise AuditError(
                "--scratch-regions cannot be mixed with explicit GVA/size inputs: "
                + ", ".join(ambiguous)
            )
        supplied_gpas = [
            f"--{name}-gpa" for name, values in region_arguments.items()
            if values["gpa"] is not None
        ]
        if arch == "mmips" and supplied_gpas:
            raise AuditError(
                "MMIPS --scratch-regions already contains exact KSEG0 mappings; "
                "do not supply " + ", ".join(supplied_gpas)
            )
        missing_gpas = [
            f"--{name}-gpa" for name, values in region_arguments.items()
            if values["gpa"] is None
        ]
        if arch == "aarch64" and missing_gpas:
            raise AuditError(
                "--scratch-regions requires all three physical mappings: "
                + ", ".join(missing_gpas)
            )
        scratch_path, discovered = _load_scratch_regions(
            Path(scratch_argument), vmlinux_sha256, build_id, arch,
        )
        for name in ("code", "data", "stack"):
            region_arguments[name]["gva"] = discovered[name]["gva"]
            region_arguments[name]["size"] = discovered[name]["size"]
            if arch == "mmips":
                region_arguments[name]["gpa"] = discovered[name]["gpa"]
    else:
        missing = [
            f"--{name}-{field}"
            for name, values in region_arguments.items()
            for field in ("gva", "gpa", "size")
            if values[field] is None
        ]
        if missing:
            raise AuditError(
                "explicit region mode requires every GVA, GPA, and size: "
                + ", ".join(missing)
            )

    output = Path(args.output).resolve()
    protected_inputs = {
        package_path: "probe package",
        binary: "probe binary",
        vmlinux: "vmlinux",
    }
    if scratch_path is not None:
        protected_inputs[scratch_path] = "scratch regions document"
    if output in protected_inputs:
        raise AuditError(
            f"output must not overwrite the input {protected_inputs[output]}: {output}"
        )

    code_gva = region_arguments["code"]["gva"]
    if code_gva != _manifest_integer(package["load_address"], "package.load_address"):
        raise AuditError(
            f"code GVA {code_gva:#x} does not match the probe package load address "
            f"{_manifest_integer(package['load_address'], 'package.load_address'):#x}"
        )
    result_offset = PROBE_REQUEST_SIZE
    result_size = region_arguments["data"]["size"] - result_offset
    if result_size < PROBE_RESPONSE_SIZE + PROBE_TASK_SIZE:
        raise AuditError(
            "data region must fit the 64-byte request, response header, and at least one task record"
        )
    max_records = (result_size - PROBE_RESPONSE_SIZE) // PROBE_TASK_SIZE
    request = struct.pack(
        "<IHHHHIQQIIQQQ",
        PROBE_REQUEST_MAGIC, PROBE_ABI_MAJOR, package["abi_minor"],
        PROBE_REQUEST_SIZE, PROBE_OP_SNAPSHOT, 0,
        args.init_task, 0, max_records, 0, 0, 0, 0,
    )
    stack_pointer = (
        region_arguments["stack"]["gva"] + region_arguments["stack"]["size"]
    )
    document = {
        "format": "viros-callgate-v1",
        "architecture": arch,
        "allow_transient_guest_modification": True,
        "kernel": {
            "vmlinux": _relative_file(vmlinux, output.parent),
            "sha256": vmlinux_sha256,
            "build_id": build_id,
        },
        "regions": [
            {
                "name": "code", "role": "code",
                "virtual_address": hex(region_arguments["code"]["gva"]),
                "physical_address": hex(region_arguments["code"]["gpa"]),
                "size": region_arguments["code"]["size"],
            },
            {
                "name": "data", "role": "data",
                "virtual_address": hex(region_arguments["data"]["gva"]),
                "physical_address": hex(region_arguments["data"]["gpa"]),
                "size": region_arguments["data"]["size"],
            },
            {
                "name": "stack", "role": "stack",
                "virtual_address": hex(region_arguments["stack"]["gva"]),
                "physical_address": hex(region_arguments["stack"]["gpa"]),
                "size": region_arguments["stack"]["size"],
            },
        ],
        "probe": {
            "binary": _relative_file(binary, output.parent),
            "sha256": package["binary_sha256"], "code_region": "code",
            "capabilities": package.get("capabilities", ["snapshot-v1"]),
            "entry_offset": _manifest_integer(package["entry_offset"], "package.entry_offset"),
            "completion_offset": _manifest_integer(
                package["completion_offset"], "package.completion_offset"
            ),
        },
        "mailbox": {
            "data_region": "data", "request_offset": 0,
            "request_hex": request.hex(), "result_offset": result_offset,
            "result_size": result_size,
            "completion_magic_hex": struct.pack("<I", PROBE_RESPONSE_MAGIC).hex(),
        },
        "invocation": {
            "cpu": args.cpu,
            "stack_region": "stack", "stack_pointer": hex(stack_pointer),
            "timeout_seconds": args.timeout_seconds,
        },
    }
    pstate = getattr(args, "pstate", None)
    if arch == "aarch64":
        document["invocation"]["pstate"] = hex(0x3c5 if pstate is None else pstate)
    elif pstate is not None:
        raise AuditError("--pstate is only valid for an aarch64 probe package")
    _write_validated_callgate(document, output)
    return document


def _audit_mmips_identity(elf: ElfObject, *, require_abi_flags: bool) -> None:
    if elf.elf_class != 32 or elf.endian != "<":
        raise AuditError(f"{elf.path}: mmips requires ELF32 little-endian")
    if elf.machine != EM_MIPS:
        raise AuditError(f"{elf.path}: expected mmips machine {EM_MIPS}, got {elf.machine}")
    if elf.flags & (EF_MIPS_PIC | EF_MIPS_CPIC):
        raise AuditError(f"{elf.path}: MMIPS probe must be non-PIC/non-CPIC")
    if elf.flags & (EF_MIPS_MICROMIPS | EF_MIPS_ARCH_ASE_M16):
        raise AuditError(f"{elf.path}: MIPS16 and microMIPS code are not supported")
    if elf.flags & EF_MIPS_ABI2 or elf.flags & EF_MIPS_ABI_MASK != EF_MIPS_ABI_O32:
        raise AuditError(f"{elf.path}: MMIPS probe must use the o32 ABI")
    if elf.flags & EF_MIPS_ARCH_MASK != EF_MIPS_ARCH_32R2:
        raise AuditError(f"{elf.path}: MMIPS probe must target MIPS32r2")
    unsupported = elf.flags & ~MMIPS_ALLOWED_E_FLAGS
    if unsupported:
        raise AuditError(f"{elf.path}: unsupported MIPS ELF flags {unsupported:#x}")

    abi_sections = [
        section for section in elf.sections
        if section["type"] == SHT_MIPS_ABIFLAGS or section["name"] == ".MIPS.abiflags"
    ]
    if not abi_sections:
        if require_abi_flags:
            raise AuditError(f"{elf.path}: missing .MIPS.abiflags soft-float proof")
        return
    if len(abi_sections) != 1:
        raise AuditError(f"{elf.path}: expected exactly one .MIPS.abiflags section")
    section = abi_sections[0]
    contents = elf._section_bytes(section["raw"])
    if section["type"] != SHT_MIPS_ABIFLAGS or len(contents) != 24:
        raise AuditError(f"{elf.path}: malformed .MIPS.abiflags section")
    (
        version, isa_level, isa_rev, gpr_size, cpr1_size, cpr2_size, fp_abi,
        isa_ext, ases, flags1, flags2,
    ) = struct.unpack(elf.endian + "H6B4I", contents)
    if version != 0 or isa_level != 32 or isa_rev != 2:
        raise AuditError(f"{elf.path}: .MIPS.abiflags does not describe MIPS32r2")
    if (
        gpr_size != AFL_REG_32
        or cpr1_size != AFL_REG_NONE
        or cpr2_size != AFL_REG_NONE
        or fp_abi != VAL_GNU_MIPS_ABI_FP_SOFT
    ):
        raise AuditError(f"{elf.path}: .MIPS.abiflags does not prove o32 soft-float")
    if isa_ext or ases or flags1 & ~MIPS_AFL_FLAGS1_ODDSPREG or flags2:
        raise AuditError(f"{elf.path}: .MIPS.abiflags requests unsupported ISA features")


def _audit_mmips_alloc_sections(elf: ElfObject, *, linked: bool) -> None:
    for section in elf.sections:
        if not section["size"] or not section["flags"] & SHF_ALLOC:
            continue
        name = section["name"]
        if section["flags"] & SHF_MIPS_GPREL:
            raise AuditError(f"{elf.path}: GP-relative alloc section {name}")
        if section["flags"] & SHF_EXECINSTR:
            if not name.startswith(".text"):
                raise AuditError(f"{elf.path}: unexpected executable alloc section {name}")
            if (
                section["addr"] & 3
                or section["offset"] & 3
                or section["size"] & 3
                or section["addralign"] < 4
            ):
                raise AuditError(f"{elf.path}: instruction section {name} is not 4-byte aligned")
            continue
        if name.startswith(".rodata"):
            continue
        if not linked and (
            section["type"] in (SHT_MIPS_ABIFLAGS, SHT_MIPS_REGINFO)
            or name in (".MIPS.abiflags", ".reginfo")
        ):
            continue
        raise AuditError(f"{elf.path}: unexpected MMIPS alloc section {name}")

    reginfo = [section for section in elf.sections
               if section["type"] == SHT_MIPS_REGINFO or section["name"] == ".reginfo"]
    for section in reginfo:
        contents = elf._section_bytes(section["raw"])
        if section["type"] != SHT_MIPS_REGINFO or len(contents) != 24:
            raise AuditError(f"{elf.path}: malformed .reginfo section")
        if struct.unpack(elf.endian + "IIIIII", contents)[-1] != 0:
            raise AuditError(f"{elf.path}: .reginfo contains a nonzero GP value")


def audit_object(path: Path, arch: str = "aarch64", max_alloc: int = 65536):
    elf = ElfObject(path)
    machines = {"aarch64": EM_AARCH64, "mmips": EM_MIPS}
    if arch not in machines:
        raise AuditError(f"audit support is not implemented for {arch}")
    if elf.elf_type != ET_REL:
        raise AuditError(f"{path}: expected ET_REL, got ELF type {elf.elf_type}")
    if elf.machine != machines[arch]:
        raise AuditError(f"{path}: expected {arch} machine {machines[arch]}, got {elf.machine}")
    if arch == "mmips":
        _audit_mmips_identity(elf, require_abi_flags=True)

    symbols = elf.symbols()
    undefined = sorted(name for name, shndx in symbols if name and shndx == SHN_UNDEF)
    if undefined:
        raise AuditError(f"{path}: undefined symbols: {', '.join(undefined)}")
    if arch == "mmips":
        mode_symbols = sorted({
            item["name"] for item in elf.symbol_records()
            if item["name"] and item["other"] & 0xf0
        })
        if mode_symbols:
            raise AuditError(
                f"{path}: PIC/MIPS16/microMIPS symbol modes: "
                + ", ".join(mode_symbols)
            )
        gp_symbols = sorted({
            name for name, _ in symbols
            if name in ("_gp", "_gp_disp", "__gnu_local_gp")
        })
        if gp_symbols:
            raise AuditError(f"{path}: GP-relative symbols: {', '.join(gp_symbols)}")
        gp_relocations = sorted({
            item["type"] for item in elf.relocation_records()
            if item["type"] in MIPS_GP_RELOCATIONS
        })
        if gp_relocations:
            raise AuditError(
                f"{path}: GP/GOT-relative MIPS relocations: "
                + ", ".join(str(item) for item in gp_relocations)
            )
        compact_relocations = sorted({
            item["type"] for item in elf.relocation_records()
            if item["type"] in MIPS_COMPACT_ISA_RELOCATIONS
        })
        if compact_relocations:
            raise AuditError(
                f"{path}: MIPS16/microMIPS relocations: "
                + ", ".join(str(item) for item in compact_relocations)
            )
    defined = {name for name, shndx in symbols if name and shndx != SHN_UNDEF}
    if "viros_probe_entry" not in defined:
        raise AuditError(f"{path}: viros_probe_entry is not defined")

    forbidden = (".modinfo", "__mcount", ".ftrace", ".asan", ".kasan", ".kcfi", ".llvm")
    bad_sections = [section["name"] for section in elf.sections
                    if any(token in section["name"] for token in forbidden)]
    if bad_sections:
        raise AuditError(f"{path}: instrumentation/module sections: {', '.join(bad_sections)}")
    alloc_size = sum(section["size"] for section in elf.sections
                     if section["flags"] & SHF_ALLOC)
    if alloc_size == 0:
        raise AuditError(f"{path}: object has no allocatable probe code")
    if alloc_size > max_alloc:
        raise AuditError(f"{path}: {alloc_size} allocatable bytes exceed {max_alloc}")
    writable = [section["name"] for section in elf.sections
                if section["size"] and section["flags"] & (SHF_ALLOC | SHF_WRITE)
                == (SHF_ALLOC | SHF_WRITE)]
    if writable:
        raise AuditError(f"{path}: stateful writable sections: {', '.join(writable)}")
    if arch == "mmips":
        _audit_mmips_alloc_sections(elf, linked=False)
    return {
        "path": str(path), "arch": arch, "elf_class": elf.elf_class,
        "byte_order": "little" if elf.endian == "<" else "big",
        "alloc_bytes": alloc_size,
        "sha256": hashlib.sha256(elf.data).hexdigest(),
        "entry_symbol": "viros_probe_entry",
    }


def linker_script(load_address: int, arch: str = "aarch64") -> str:
    if arch == "mmips":
        if load_address < 0 or load_address > 0xffffffff:
            raise AuditError("load address does not fit a 32-bit MMIPS address")
        if load_address & 3:
            raise AuditError("MMIPS probe load address must be 4-byte aligned")
        return f"""/* Generated by probe_tool.py; do not edit. */
ENTRY(viros_probe_entry)
SECTIONS
{{
  . = 0x{load_address:x};
  __viros_image_start = .;
  .text : ALIGN(4) {{ *(.text .text.*) }}
  .rodata : ALIGN(4) {{ *(.rodata .rodata.*) }}
  .data : ALIGN(4) {{ *(.data .data.*) }}
  .bss (NOLOAD) : ALIGN(4) {{ *(.bss .bss.* COMMON) }}
  __viros_image_end = .;
  ASSERT(SIZEOF(.data) == 0, "viros probe must not contain writable data")
  ASSERT(SIZEOF(.bss) == 0, "viros probe must not contain bss")
  /DISCARD/ : {{ *(.MIPS.abiflags) *(.reginfo) *(.pdr) *(.mdebug.*) *(.gnu.attributes) *(.comment) *(.note*) *(.eh_frame*) *(.debug*) *(.discard.*) }}
}}
"""
    if arch != "aarch64":
        raise AuditError(f"link support is not implemented for {arch}")
    if load_address < 0 or load_address > 0xffffffffffffffff:
        raise AuditError("load address does not fit an AArch64 address")
    if load_address & 0xf:
        raise AuditError("AArch64 probe load address must be 16-byte aligned")
    return f"""/* Generated by probe_tool.py; do not edit. */
ENTRY(viros_probe_entry)
SECTIONS
{{
  . = 0x{load_address:x};
  __viros_image_start = .;
  .text : ALIGN(16) {{ *(.text .text.*) }}
  .rodata : ALIGN(16) {{ *(.rodata .rodata.*) }}
  .data : ALIGN(16) {{ *(.data .data.*) }}
  .bss (NOLOAD) : ALIGN(16) {{ *(.bss .bss.* COMMON) }}
  __viros_image_end = .;
  ASSERT(SIZEOF(.data) == 0, "viros probe must not contain writable data")
  ASSERT(SIZEOF(.bss) == 0, "viros probe must not contain bss")
  /DISCARD/ : {{ *(.comment) *(.note*) *(.eh_frame*) *(.debug*) *(.discard.*) }}
}}
"""


def audit_linked_image(
    path: Path, load_address: int, max_alloc: int = 65536,
    arch: str = "aarch64",
):
    elf = ElfObject(path)
    if elf.elf_type != ET_EXEC:
        raise AuditError(f"{path}: expected linked ET_EXEC, got ELF type {elf.elf_type}")
    machines = {"aarch64": EM_AARCH64, "mmips": EM_MIPS}
    if arch not in machines:
        raise AuditError(f"linked-image audit support is not implemented for {arch}")
    if elf.machine != machines[arch]:
        raise AuditError(f"{path}: expected {arch} machine {machines[arch]}, got {elf.machine}")
    if arch == "mmips":
        _audit_mmips_identity(elf, require_abi_flags=False)
    symbols = elf.symbol_records()
    undefined = sorted(item["name"] for item in symbols
                       if item["name"] and item["shndx"] == SHN_UNDEF)
    if undefined:
        raise AuditError(f"{path}: linked image has undefined symbols: {', '.join(undefined)}")
    by_name = {item["name"]: item for item in symbols if item["name"]}
    missing = [name for name in ("viros_probe_entry", "viros_probe_complete")
               if name not in by_name or by_name[name]["shndx"] == SHN_UNDEF]
    if missing:
        raise AuditError(f"{path}: missing package symbols: {', '.join(missing)}")
    remaining_relocations = []
    for section in elf.sections:
        if section["type"] in (SHT_REL, SHT_RELA) and section["size"]:
            remaining_relocations.append(section["name"])
    if remaining_relocations:
        raise AuditError(
            f"{path}: remaining relocations in absolute image: "
            f"{', '.join(remaining_relocations)}")
    alloc = [section for section in elf.sections
             if section["flags"] & SHF_ALLOC and section["size"]]
    if not alloc:
        raise AuditError(f"{path}: linked image has no allocatable code")
    nobits = [section["name"] for section in alloc if section["type"] == SHT_NOBITS]
    writable = [section["name"] for section in alloc if section["flags"] & SHF_WRITE]
    if nobits or writable:
        names = sorted(set(nobits + writable))
        raise AuditError(f"{path}: linked image is not a stateless flat blob: {', '.join(names)}")
    if arch == "mmips":
        _audit_mmips_alloc_sections(elf, linked=True)
    image_start = min(section["addr"] for section in alloc)
    image_end = max(section["addr"] + section["size"] for section in alloc)
    if image_start != load_address:
        raise AuditError(f"{path}: linked image starts at {image_start:#x}, expected {load_address:#x}")
    if image_end - image_start > max_alloc:
        raise AuditError(f"{path}: linked image exceeds {max_alloc} bytes")
    addresses = {name: by_name[name]["value"]
                 for name in ("viros_probe_entry", "viros_probe_complete")}
    for name, value in addresses.items():
        if not image_start <= value < image_end:
            raise AuditError(f"{path}: {name} at {value:#x} is outside the flat image")
        if arch == "mmips" and value & 3:
            raise AuditError(f"{path}: {name} does not select a 4-byte instruction")
    if arch == "mmips" and elf.entry != addresses["viros_probe_entry"]:
        raise AuditError(f"{path}: ELF entry does not match viros_probe_entry")
    return {
        "image_start": image_start, "image_end": image_end,
        "image_size": image_end - image_start,
        "entry_address": addresses["viros_probe_entry"],
        "entry_offset": addresses["viros_probe_entry"] - image_start,
        "completion_address": addresses["viros_probe_complete"],
        "completion_offset": addresses["viros_probe_complete"] - image_start,
    }


def package_object(args):
    build_manifest, source = load_probe_build(Path(args.build_manifest))
    arch = build_manifest["arch"]
    source_audit = audit_object(source, arch, args.max_alloc)
    output = Path(args.output_dir).resolve()
    output.mkdir(parents=True, exist_ok=True)
    script_path = output / "viros_probe.lds"
    elf_path = output / "viros_probe.elf"
    binary_path = output / "viros_probe.bin"
    script_path.write_text(linker_script(args.load_address, arch))
    linker_command = [args.cross_ld]
    if arch == "mmips":
        linker_command.extend(["-m", "elf32ltsmip"])
    linker_command.extend([
        "-nostdlib", "-static", "--build-id=none", "-T", str(script_path),
        "-o", str(elf_path), str(source),
    ])
    subprocess.run(linker_command, check=True)
    linked = audit_linked_image(
        elf_path, args.load_address, args.max_alloc, arch=arch,
    )
    subprocess.run([args.objcopy, "-O", "binary", str(elf_path), str(binary_path)],
                   check=True)
    binary = binary_path.read_bytes()
    if len(binary) != linked["image_size"]:
        raise AuditError(
            f"flat binary is {len(binary)} bytes; linked alloc image is {linked['image_size']} bytes"
        )
    if arch == "aarch64":
        abi_layout = {
            "version": 1,
            "request_v1_bytes": 64, "response_v1_header_bytes": 64,
            "task_v1_bytes": 192, "translation_v1_bytes": 64,
            "saved_regs_v1_bytes": 304,
            "target_byte_order": "little",
        }
        capabilities = [
            "snapshot-v1", "translate-va-aarch64-v1",
            "saved-regs-aarch64-v1",
        ]
        call_abi = {
            "name": "aapcs64", "argument_registers": ["x0", "x1", "x2"],
            "result_register": "x0", "link_register": "x30",
            "stack_alignment": 16, "completion_trap": "brk-0x5650",
        }
        architecture_metadata = {
            "pgd_address_kind": "kernel-virtual-address",
        }
    else:
        abi_layout = {
            "version": 1,
            "request_v1_bytes": 64, "response_v1_header_bytes": 64,
            "task_v1_bytes": 192,
            "target_byte_order": "little",
        }
        capabilities = ["snapshot-v1"]
        call_abi = {
            "name": "o32-soft-float",
            "argument_registers": ["r4", "r5", "r6"],
            "result_register": "r2", "link_register": "r31",
            "stack_alignment": 8, "completion_trap": "break-0x5650",
        }
        architecture_metadata = {
            "elf_abi": {
                "class": 32, "byte_order": "little", "machine": "EM_MIPS",
                "isa": "mips32r2", "abi": "o32", "float": "soft",
                "pic": False, "mips16": False, "micromips": False,
            },
        }
    manifest = {
        "schema": PROBE_PACKAGE_SCHEMA, "arch": arch,
        "abi_major": 1, "abi_minor": PROBE_ABI_MINOR,
        "abi_layout": abi_layout,
        "capabilities": capabilities,
        "call_abi": call_abi,
        "load_address": args.load_address,
        **linked,
        **architecture_metadata,
        "object_sha256": source_audit["sha256"],
        "linked_elf_sha256": hashlib.sha256(elf_path.read_bytes()).hexdigest(),
        "binary_sha256": hashlib.sha256(binary).hexdigest(),
        "linked_elf": elf_path.name, "binary": binary_path.name,
        "linker_script": script_path.name,
        "cross_ld": args.cross_ld, "objcopy": args.objcopy,
        "kernel": {
            "sha256": build_manifest["kernel"]["sha256"],
            "build_id": build_manifest["kernel"]["build_id"],
            **{
                key: build_manifest["kernel"][key]
                for key in (
                    "kbuild_release", "vmlinux_release_evidence", "source_tree"
                )
                if key in build_manifest["kernel"]
            },
        },
    }
    manifest_path = output / "package.json"
    _write_json_atomic(manifest, manifest_path)
    return manifest


def build_object(args):
    root = Path(__file__).resolve().parent
    output = Path(args.output_dir).resolve()
    linux_dir = Path(args.linux_dir).resolve()
    vmlinux = Path(args.vmlinux).resolve()
    built_vmlinux = linux_dir / "vmlinux"
    if not built_vmlinux.is_file():
        raise AuditError(
            "exact Kbuild directory has no vmlinux; use the configured kernel "
            "output directory which produced the supplied symbol file"
        )
    if not vmlinux.is_file():
        raise AuditError(f"vmlinux does not name a regular file: {vmlinux}")
    built_sha256 = _sha256_file(built_vmlinux)
    supplied_sha256 = _sha256_file(vmlinux)
    if built_sha256 != supplied_sha256:
        raise AuditError(
            "--vmlinux was not produced by --linux-dir: exact Kbuild vmlinux "
            f"SHA-256 is {built_sha256}, supplied file is {supplied_sha256}"
        )
    provenance = _kernel_release_provenance(linux_dir, vmlinux)
    source = output / "src"
    kernel_source = source / "kernel"
    kernel_source.mkdir(parents=True, exist_ok=True)
    (source / "include").mkdir(exist_ok=True)
    shutil.copy2(root / "kernel" / "Kbuild", kernel_source / "Kbuild")
    shutil.copy2(root / "kernel" / "viros_probe.c", kernel_source / "viros_probe.c")
    shutil.copy2(root / "include" / "viros_probe_abi.h", source / "include" / "viros_probe_abi.h")

    env = os.environ.copy()
    env["ARCH"] = {"aarch64": "arm64", "mmips": "mips"}[args.arch]
    env["CROSS_COMPILE"] = args.cross_compile
    command = [args.make, "-C", str(linux_dir),
               f"M={kernel_source}", "viros_probe.o"]
    subprocess.run(command, check=True, env=env)
    object_path = kernel_source / "viros_probe.o"
    result = audit_object(object_path, args.arch, args.max_alloc)
    result["linux_dir"] = str(linux_dir)
    result["cross_compile"] = args.cross_compile
    result["schema"] = PROBE_BUILD_SCHEMA
    result["object"] = _relative_file(object_path, output)
    result["object_sha256"] = result.pop("sha256")
    result["kernel"] = {
        "vmlinux": _relative_file(vmlinux, output),
        "sha256": supplied_sha256,
        "build_id": gnu_build_id(vmlinux, args.arch),
        **provenance,
    }
    manifest = output / "probe.json"
    _write_json_atomic(result, manifest)
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    audit = subparsers.add_parser("audit", help="validate a compiled ET_REL probe")
    audit.add_argument("object", type=Path)
    audit.add_argument("--arch", choices=("aarch64", "mmips"), default="aarch64")
    audit.add_argument("--max-alloc", type=int, default=65536)
    build = subparsers.add_parser("build", help="compile through an exact kernel Kbuild")
    build.add_argument("--linux-dir", required=True)
    build.add_argument("--output-dir", required=True)
    build.add_argument("--arch", choices=("aarch64", "mmips"), default="aarch64")
    build.add_argument("--cross-compile", required=True)
    build.add_argument("--vmlinux", required=True)
    build.add_argument("--make", default="make")
    build.add_argument("--max-alloc", type=int, default=65536)
    package = subparsers.add_parser(
        "package", help="absolute-link an audited probe into a flat image")
    package.add_argument(
        "build_manifest", type=Path,
        help="probe.json emitted by the exact-Kbuild build stage",
    )
    package.add_argument("--load-address", type=lambda value: int(value, 0), required=True)
    package.add_argument("--output-dir", required=True)
    package.add_argument("--cross-ld", required=True)
    package.add_argument("--objcopy", required=True)
    package.add_argument("--max-alloc", type=int, default=65536)
    callgate = subparsers.add_parser(
        "callgate-manifest",
        help="bind a sealed probe package and scratch mappings for viros-probe-run",
    )
    callgate.add_argument("package", type=Path, help="sealed probe package.json")
    callgate.add_argument("--vmlinux", type=Path, required=True)
    callgate.add_argument("--output", type=Path, required=True)
    callgate.add_argument(
        "--scratch-regions", type=Path,
        help=(
            "scratch.json from scratch_tool.py; supplies all region GVAs and sizes "
            "and, for MMIPS KSEG0, exact GPAs; AArch64 still requires three "
            "runtime GPA arguments"
        ),
    )
    for region in ("code", "data", "stack"):
        callgate.add_argument(
            f"--{region}-gva", type=lambda value: int(value, 0),
            help=f"guest virtual base of the {region} region (explicit mode)",
        )
        callgate.add_argument(
            f"--{region}-gpa", type=lambda value: int(value, 0),
            help=f"guest physical base of the {region} scratch region",
        )
        callgate.add_argument(
            f"--{region}-size", type=lambda value: int(value, 0),
            help=f"size in bytes of the {region} region (explicit mode)",
        )
    callgate.add_argument("--cpu", type=lambda value: int(value, 0), required=True)
    callgate.add_argument(
        "--init-task", type=lambda value: int(value, 0), required=True,
        help="kernel virtual address of init_task from the exact vmlinux",
    )
    callgate.add_argument(
        "--pstate", type=lambda value: int(value, 0), default=None,
        help="AArch64 EL1h PSTATE used during the probe call (default: 0x3c5)",
    )
    callgate.add_argument("--timeout-seconds", type=float, default=1.0)
    args = parser.parse_args(argv)
    try:
        if args.command == "audit":
            result = audit_object(args.object, args.arch, args.max_alloc)
        elif args.command == "build":
            result = build_object(args)
        elif args.command == "package":
            result = package_object(args)
        else:
            result = create_callgate_manifest(args)
    except (AuditError, OSError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"probe_tool: {exc}\n")
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
