#!/usr/bin/env python3
"""Discover viros reserved scratch regions in an exact supported vmlinux."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


PROJECT_ROOT = Path(__file__).resolve().parents[2]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from probe.probe_tool import (  # noqa: E402
    AuditError,
    ElfObject,
    EM_AARCH64,
    EM_ARM,
    EM_MIPS,
    EM_X86_64,
    _sha256_file,
    _write_json_atomic,
    gnu_build_id,
)


SCRATCH_SCHEMA = "viros-scratch-regions-v1"
SHF_WRITE = 0x1
SHF_ALLOC = 0x2
SHF_EXECINSTR = 0x4
SUPPORTED_PAGE_SIZES = (4096, 16384, 65536)
UINT64_LIMIT = 1 << 64
UINT32_LIMIT = 1 << 32
MIPS_KSEG0_START = 0x80000000
MIPS_KSEG0_END = 0xA0000000
SYMBOLS = {
    "code": ("__viros_scratch_code_start", "__viros_scratch_code_end"),
    "data": ("__viros_scratch_data_start", "__viros_scratch_data_end"),
    "stack": ("__viros_scratch_stack_start", "__viros_scratch_stack_end"),
}


class ScratchError(RuntimeError):
    pass


def _defined_symbol(elf: ElfObject, name: str) -> dict:
    matches = [
        record for record in elf.symbol_records()
        if record["name"] == name and record["shndx"] != 0
    ]
    identities = {(record["value"], record["shndx"]) for record in matches}
    if not identities:
        raise ScratchError(f"{elf.path}: missing defined symbol {name}")
    if len(identities) != 1:
        raise ScratchError(f"{elf.path}: conflicting definitions of symbol {name}")
    value, section_index = next(iter(identities))
    if section_index >= len(elf.sections):
        raise ScratchError(f"{elf.path}: symbol {name} has an invalid section index")
    return {"value": value, "section": section_index}


def _check_section(
    elf: ElfObject, name: str, start: dict, end: dict, required: int, forbidden: int,
) -> None:
    if start["section"] != end["section"]:
        raise ScratchError(f"{name} scratch bounds are not in one ELF section")
    section = elf.sections[start["section"]]
    section_end = section["addr"] + section["size"]
    if not (section["addr"] <= start["value"] < end["value"] <= section_end):
        raise ScratchError(f"{name} scratch bounds escape ELF section {section['name']}")
    if section["flags"] & required != required or section["flags"] & forbidden:
        raise ScratchError(
            f"{name} scratch section {section['name']} has unsafe ELF flags "
            f"0x{section['flags']:x}"
        )


def discover_regions(vmlinux: Path, runtime_offset: int = 0) -> dict:
    vmlinux = vmlinux.resolve()
    if runtime_offset < 0 or runtime_offset >= UINT64_LIMIT:
        raise ScratchError("runtime offset must be an unsigned 64-bit integer")
    elf = ElfObject(vmlinux)
    if elf.machine == EM_AARCH64:
        if elf.elf_class != 64 or elf.endian != "<":
            raise ScratchError(
                f"{vmlinux}: aarch64 scratch requires ELF64 little-endian"
            )
        arch = "aarch64"
        address_limit = UINT64_LIMIT
        supported_page_sizes = SUPPORTED_PAGE_SIZES
    elif elf.machine == EM_ARM:
        if elf.elf_class != 32 or elf.endian != "<":
            raise ScratchError(
                f"{vmlinux}: arm scratch requires ELF32 little-endian"
            )
        arch = "arm"
        address_limit = UINT32_LIMIT
        supported_page_sizes = (4096,)
    elif elf.machine == EM_MIPS:
        if elf.elf_class != 32 or elf.endian != "<":
            raise ScratchError(
                f"{vmlinux}: mmips scratch requires ELF32 little-endian"
            )
        arch = "mmips"
        address_limit = UINT32_LIMIT
        supported_page_sizes = SUPPORTED_PAGE_SIZES
    elif elf.machine == EM_X86_64:
        if elf.elf_class != 64 or elf.endian != "<":
            raise ScratchError(
                f"{vmlinux}: x86_64 scratch requires ELF64 little-endian"
            )
        arch = "x86_64"
        address_limit = UINT64_LIMIT
        supported_page_sizes = (4096,)
    else:
        raise ScratchError(
            f"{vmlinux}: expected x86-64 ({EM_X86_64}), ARM ({EM_ARM}), "
            f"AArch64 ({EM_AARCH64}), or MIPS ({EM_MIPS}) machine, "
            f"got {elf.machine}"
        )

    raw_regions = {}
    for name, (start_name, end_name) in SYMBOLS.items():
        start = _defined_symbol(elf, start_name)
        end = _defined_symbol(elf, end_name)
        size = end["value"] - start["value"]
        if size <= 0:
            raise ScratchError(f"{name} scratch region has invalid bounds")
        raw_regions[name] = (start, end, size, start_name, end_name)

    sizes = {region[2] for region in raw_regions.values()}
    if len(sizes) != 1 or next(iter(sizes)) not in supported_page_sizes:
        raise ScratchError(
            f"scratch regions must each occupy one supported {arch} page"
        )
    page_size = next(iter(sizes))
    if runtime_offset % page_size:
        raise ScratchError("runtime offset must be page aligned")

    ranges = []
    regions = {}
    for name, (start, end, size, start_name, end_name) in raw_regions.items():
        if start["value"] % page_size:
            raise ScratchError(f"{name} scratch region is not page aligned")
        runtime_gva = start["value"] + runtime_offset
        if runtime_gva >= address_limit or runtime_gva + size > address_limit:
            raise ScratchError(
                f"{name} runtime GVA exceeds {address_limit.bit_length() - 1}-bit "
                "address space"
            )
        if arch == "mmips" and not (
            MIPS_KSEG0_START <= start["value"]
            and end["value"] <= MIPS_KSEG0_END
            and MIPS_KSEG0_START <= runtime_gva
            and runtime_gva + size <= MIPS_KSEG0_END
        ):
            raise ScratchError(
                f"{name} scratch region must remain wholly in MIPS KSEG0"
            )
        ranges.append((start["value"], end["value"], name))
        region = {
            "gva": runtime_gva,
            "link_gva": start["value"],
            "size": size,
            "start_symbol": start_name,
            "end_symbol": end_name,
        }
        if arch == "mmips":
            region["gpa"] = runtime_gva - MIPS_KSEG0_START
        regions[name] = region

    ranges.sort()
    for previous, current in zip(ranges, ranges[1:]):
        if current[0] < previous[1]:
            raise ScratchError(
                f"scratch regions {previous[2]} and {current[2]} overlap"
            )

    code = raw_regions["code"]
    _check_section(
        elf, "code", code[0], code[1], SHF_ALLOC | SHF_EXECINSTR, SHF_WRITE,
    )
    for name in ("data", "stack"):
        region = raw_regions[name]
        _check_section(
            elf, name, region[0], region[1], SHF_ALLOC | SHF_WRITE, SHF_EXECINSTR,
        )

    return {
        "schema": SCRATCH_SCHEMA,
        "arch": arch,
        "page_size": page_size,
        "runtime_offset": runtime_offset,
        "vmlinux": {
            "path": str(vmlinux),
            "sha256": _sha256_file(vmlinux),
            "build_id": gnu_build_id(vmlinux, arch),
        },
        "regions": regions,
    }


def main(argv=None) -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("vmlinux", type=Path)
    parser.add_argument(
        "--runtime-offset", type=lambda value: int(value, 0), default=0,
        help="known kernel runtime relocation added to link-time symbol GVAs",
    )
    parser.add_argument(
        "--output", type=Path,
        help="also atomically write the JSON document to this file",
    )
    args = parser.parse_args(argv)
    try:
        result = discover_regions(args.vmlinux, args.runtime_offset)
        if args.output:
            if args.output.resolve() == args.vmlinux.resolve():
                raise ScratchError("output must not overwrite vmlinux")
            _write_json_atomic(result, args.output)
    except (AuditError, ScratchError, OSError) as exc:
        parser.exit(1, f"scratch_tool: {exc}\n")
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
