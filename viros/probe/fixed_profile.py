"""Derive reserved kernel-page mappings for ViroS fixed QEMU profiles."""

from __future__ import annotations

from pathlib import Path
import struct

from probe.probe_tool import ElfObject, EM_AARCH64, EM_ARM, EM_MIPS, EM_X86_64


class FixedProfileError(RuntimeError):
    pass


_IDENTITY = {
    "aarch64": (EM_AARCH64, 64, "<"),
    "arm": (EM_ARM, 32, "<"),
    "mmips": (EM_MIPS, 32, "<"),
    "x86_64": (EM_X86_64, 64, "<"),
}


def _regions(document: dict, architecture: str) -> tuple[int, dict[str, tuple[int, int]]]:
    if document.get("arch") != architecture:
        raise FixedProfileError("scratch architecture does not match the fixed profile")
    page_size = document.get("page_size")
    raw = document.get("regions")
    if not isinstance(page_size, int) or page_size <= 0 or not isinstance(raw, dict):
        raise FixedProfileError("scratch description has invalid page geometry")
    result = {}
    for name in ("code", "data", "stack"):
        item = raw.get(name)
        if not isinstance(item, dict):
            raise FixedProfileError(f"scratch description has no {name} region")
        address, size = item.get("gva"), item.get("size")
        if (
            not isinstance(address, int)
            or not isinstance(size, int)
            or address < 0
            or size != page_size
            or address % page_size
        ):
            raise FixedProfileError(f"scratch {name} region is not one aligned page")
        result[name] = (address, size)
    return page_size, result


def _symbol(elf: ElfObject, name: str) -> int:
    values = {
        row["value"]
        for row in elf.symbol_records()
        if row["name"] == name and row["shndx"] != 0
    }
    if len(values) != 1:
        raise FixedProfileError(f"vmlinux does not define exactly one {name}")
    return next(iter(values))


def _x86_loads(elf: ElfObject) -> list[tuple[int, int, int]]:
    if len(elf.data) < 64:
        raise FixedProfileError("x86-64 vmlinux has a truncated ELF header")
    phoff = struct.unpack_from("<Q", elf.data, 32)[0]
    phentsize, phnum = struct.unpack_from("<HH", elf.data, 54)
    item_format = "<IIQQQQQQ"
    item_size = struct.calcsize(item_format)
    if phentsize < item_size or not phnum or phoff + phentsize * phnum > len(elf.data):
        raise FixedProfileError("x86-64 vmlinux has an invalid program-header table")
    loads = []
    for index in range(phnum):
        item = struct.unpack_from(item_format, elf.data, phoff + index * phentsize)
        if item[0] == 1 and item[6] > 0:
            loads.append((item[3], item[4], item[6]))
    return loads


def _aarch64_text_gpa(boot_image: Path) -> int:
    try:
        header = boot_image.read_bytes()[:64]
    except OSError as exc:
        raise FixedProfileError(f"cannot read AArch64 Image header: {exc}") from exc
    if len(header) < 64 or header[56:60] != b"ARM\x64":
        raise FixedProfileError("fixed AArch64 profile requires an uncompressed Linux Image")
    text_offset = struct.unpack_from("<Q", header, 8)[0]
    if text_offset >= 0x20000000 or text_offset & 0xFFF:
        raise FixedProfileError("AArch64 Image has an invalid text offset")
    return 0x40000000 + text_offset


def scratch_gpas(
    architecture: str,
    vmlinux: Path,
    boot_image: Path,
    scratch_document: dict,
) -> dict[str, int]:
    """Return exact GPAs implied by one fixed machine and linked kernel."""

    if architecture not in _IDENTITY:
        raise FixedProfileError(f"no fixed QEMU profile for {architecture}")
    page_size, regions = _regions(scratch_document, architecture)
    elf = ElfObject(vmlinux)
    expected = _IDENTITY[architecture]
    if (elf.machine, elf.elf_class, elf.endian) != expected:
        raise FixedProfileError("vmlinux ELF identity does not match the fixed profile")

    result: dict[str, int] = {}
    if architecture == "mmips":
        for name, (gva, _size) in regions.items():
            if not 0x80000000 <= gva < 0xA0000000:
                raise FixedProfileError(f"MMIPS scratch {name} is outside KSEG0")
            result[name] = gva - 0x80000000
    elif architecture == "x86_64":
        loads = _x86_loads(elf)
        for name, (gva, size) in regions.items():
            matches = [
                physical + gva - virtual
                for virtual, physical, memory_size in loads
                if virtual <= gva and gva + size <= virtual + memory_size
            ]
            if len(matches) != 1:
                raise FixedProfileError(
                    f"x86-64 scratch {name} is not covered by exactly one PT_LOAD"
                )
            result[name] = matches[0]
    else:
        text_gva = _symbol(elf, "_text")
        text_gpa = 0x40008000 if architecture == "arm" else _aarch64_text_gpa(boot_image)
        image_offset = text_gva - text_gpa
        for name, (gva, size) in regions.items():
            gpa = gva - image_offset
            if gpa < 0x40000000 or gpa + size > 0x60000000:
                raise FixedProfileError(f"{architecture} scratch {name} is outside guest RAM")
            result[name] = gpa

    for name, value in result.items():
        if value < 0 or value % page_size:
            raise FixedProfileError(f"scratch {name} GPA is not page aligned")
    if len(set(result.values())) != 3:
        raise FixedProfileError("scratch regions do not map to distinct physical pages")
    return result
