#!/usr/bin/env python3
"""Extract the boot payloads from a RouterOS 7 NPK.

This intentionally has no third-party dependencies.  RouterOS 7 system NPKs
contain a SquashFS section and a zlib-compressed file archive.  Embedded
targets store a small ELF decompressor in boot/kernel; the actual Linux image
and initramfs are concatenated, page-aligned XZ streams in an ELF section.
"""

from __future__ import annotations

import argparse
import bz2
import json
import lzma
import os
from pathlib import Path
import re
import stat
import struct
import sys
import zlib

NPK_MAGIC = 0xBAD0F11E
XZ_MAGIC = b"\xfd7zXZ\x00"


class FormatError(RuntimeError):
    pass


def npk_sections(data: bytes):
    if len(data) < 8 or struct.unpack_from("<I", data)[0] != NPK_MAGIC:
        raise FormatError("not a RouterOS NPK (bad magic)")
    declared = struct.unpack_from("<I", data, 4)[0]
    if declared != len(data) - 8:
        raise FormatError(f"NPK size mismatch: header={declared}, file={len(data) - 8}")
    pos = 8
    while pos < len(data):
        if pos + 6 > len(data):
            raise FormatError(f"truncated section header at {pos:#x}")
        kind, size = struct.unpack_from("<HI", data, pos)
        start, end = pos + 6, pos + 6 + size
        if end > len(data):
            raise FormatError(f"section {kind} overruns the NPK")
        yield kind, data[start:end]
        pos = end


def archive_entries(blob: bytes):
    pos = 0
    while pos < len(blob):
        if pos + 30 > len(blob):
            raise FormatError(f"truncated archive entry at {pos:#x}")
        mode = struct.unpack_from("<H", blob, pos)[0]
        size = struct.unpack_from("<I", blob, pos + 24)[0]
        name_len = struct.unpack_from("<H", blob, pos + 28)[0]
        name_start = pos + 30
        data_start = name_start + name_len
        end = data_start + size
        if not name_len or end > len(blob):
            raise FormatError(f"invalid archive entry at {pos:#x}")
        try:
            name = blob[name_start:data_start].decode("utf-8")
        except UnicodeDecodeError as exc:
            raise FormatError(f"non-UTF-8 archive name at {pos:#x}") from exc
        yield mode, name, blob[data_start:end]
        pos = end


def elf_section(data: bytes, wanted: str) -> bytes | None:
    if data[:4] != b"\x7fELF" or data[4] not in (1, 2) or data[5] not in (1, 2):
        return None
    is_64 = data[4] == 2
    endian = "<" if data[5] == 1 else ">"
    if is_64:
        shoff = struct.unpack_from(endian + "Q", data, 40)[0]
        shentsize, shnum, shstrndx = struct.unpack_from(endian + "HHH", data, 58)
        shfmt = endian + "IIQQQQIIQQ"
    else:
        shoff = struct.unpack_from(endian + "I", data, 32)[0]
        shentsize, shnum, shstrndx = struct.unpack_from(endian + "HHH", data, 46)
        shfmt = endian + "IIIIIIIIII"
    if not shoff or not shnum or shstrndx >= shnum:
        return None
    need = shoff + shentsize * shnum
    if need > len(data) or struct.calcsize(shfmt) > shentsize:
        raise FormatError("invalid ELF section table")
    sections = [struct.unpack_from(shfmt, data, shoff + i * shentsize) for i in range(shnum)]
    names_section = sections[shstrndx]
    names = data[names_section[4] : names_section[4] + names_section[5]]
    for section in sections:
        name_at = section[0]
        name_end = names.find(b"\x00", name_at)
        if name_end < 0:
            continue
        if names[name_at:name_end].decode("ascii", "replace") == wanted:
            offset, size = section[4], section[5]
            if offset + size > len(data):
                raise FormatError(f"ELF section {wanted} overruns file")
            return data[offset : offset + size]
    return None


def elf_entry(data: bytes) -> int | None:
    """Return an ELF executable entry address without external tools."""
    if data[:4] != b"\x7fELF" or data[4] not in (1, 2) or data[5] not in (1, 2):
        return None
    endian = "<" if data[5] == 1 else ">"
    if data[4] == 1:
        return struct.unpack_from(endian + "I", data, 24)[0]
    return struct.unpack_from(endian + "Q", data, 24)[0]


def xz_streams(data: bytes):
    """Yield individually decompressed XZ streams, tolerating alignment fill."""
    pos = 0
    index = 0
    while True:
        start = data.find(XZ_MAGIC, pos)
        if start < 0:
            return
        decoder = lzma.LZMADecompressor(format=lzma.FORMAT_XZ)
        try:
            unpacked = decoder.decompress(data[start:])
        except lzma.LZMAError as exc:
            raise FormatError(f"bad XZ stream {index} at {start:#x}: {exc}") from exc
        if not decoder.eof:
            raise FormatError(f"truncated XZ stream {index} at {start:#x}")
        consumed = len(data[start:]) - len(decoder.unused_data)
        yield start, consumed, unpacked
        index += 1
        pos = start + consumed


def hvfs_entries(data: bytes):
    """Yield files from the Tilera hypervisor's embedded HvFs image."""
    if len(data) < 0x14 or data[:4] != b"HvFs":
        raise FormatError("bad Tilera HvFs header")
    count = struct.unpack_from("<I", data, 4)[0]
    if count > 1024 or 0x14 + count * 16 > len(data):
        raise FormatError(f"invalid Tilera HvFs entry count: {count}")
    for index in range(count):
        name_at, data_at, size, flags = struct.unpack_from("<IIII", data, 0x14 + index * 16)
        name_at += 4  # offsets are relative to the bytes after the HvFs magic
        name_end = data.find(b"\x00", name_at)
        payload_at = data_at + 4  # per-file metadata word
        if name_at >= len(data) or name_end < name_at or payload_at + size > len(data):
            raise FormatError(f"Tilera HvFs entry {index} overruns the filesystem")
        try:
            name = data[name_at:name_end].decode("ascii")
        except UnicodeDecodeError as exc:
            raise FormatError(f"non-ASCII Tilera HvFs name at entry {index}") from exc
        if not name or "/" in name or name in (".", ".."):
            raise FormatError(f"unsafe Tilera HvFs name: {name!r}")
        yield name, data[payload_at:payload_at + size], flags


def cpio_init(data: bytes) -> tuple[bytes, int] | None:
    """Return /init and its mode from a newc initramfs."""
    pos = 0
    while pos + 110 <= len(data):
        magic = data[pos:pos + 6]
        if magic not in (b"070701", b"070702"):
            return None
        try:
            fields = [int(data[pos + 6 + i * 8:pos + 14 + i * 8], 16) for i in range(13)]
        except ValueError as exc:
            raise FormatError(f"invalid newc header at {pos:#x}") from exc
        mode, size, name_size = fields[1], fields[6], fields[11]
        name_at = pos + 110
        name_end = name_at + name_size
        if not name_size or name_end > len(data) or data[name_end - 1] != 0:
            raise FormatError(f"invalid newc name at {pos:#x}")
        name = data[name_at:name_end - 1].decode("utf-8", "replace")
        payload_at = (name_end + 3) & ~3
        payload_end = payload_at + size
        if payload_end > len(data):
            raise FormatError(f"newc payload {name!r} overruns initramfs")
        if name in ("init", "/init"):
            return data[payload_at:payload_end], mode
        if name == "TRAILER!!!":
            return None
        pos = (payload_end + 3) & ~3
    return None


def classify_ppc(kernel: bytes) -> str:
    match = re.search(rb"Linux version ([^ \x00]+)", kernel)
    version = match.group(1).decode("ascii", "replace") if match else ""
    if version.endswith("-smp"):
        return "ppc-e500-smp"
    if version.endswith("-e500"):
        return "ppc-e500"
    if version.endswith("-440"):
        return "ppc-440"
    return "ppc-83xx"


def make_elf32(raw: bytes, machine: int, endian: str, vaddr: int, entry: int, flags: int) -> bytes:
    """Make the minimal ELF container QEMU's direct kernel loader expects."""
    byteorder = 1 if endian == "<" else 2
    ident = b"\x7fELF" + bytes((1, byteorder, 1, 0)) + bytes(8)
    ehdr = ident + struct.pack(
        endian + "HHIIIIIHHHHHH",
        2, machine, 1, entry, 52, 0, flags, 52, 32, 1, 0, 0, 0,
    )
    offset = 0x1000
    phdr = struct.pack(
        endian + "IIIIIIII", 1, offset, vaddr, vaddr, len(raw), len(raw), 7, 0x1000
    )
    return ehdr + phdr + bytes(offset - len(ehdr) - len(phdr)) + raw


def write_file(path: Path, data: bytes, mode: int = 0o644):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    path.chmod(mode)


def boot_artifacts(arch: str, wrapper: bytes, output: Path):
    if arch == "tile":
        filesystem = elf_section(wrapper, "fs")
        if filesystem is None:
            raise FormatError("TILE hypervisor ELF has no HvFs section")
        target = output / arch
        write_file(target / "hypervisor.elf", wrapper, 0o755)
        names = []
        for name, payload, _flags in hvfs_entries(filesystem):
            names.append(name)
            write_file(target / "hvfs" / name, payload)
            if name == "vmlinux":
                if not payload.startswith(b"BZh"):
                    raise FormatError("TILE HvFs vmlinux is not bzip2 data")
                try:
                    write_file(target / "vmlinux", bz2.decompress(payload), 0o755)
                except OSError as exc:
                    raise FormatError(f"cannot decompress TILE vmlinux: {exc}") from exc
            elif name == "initramfs.cpio.gz":
                write_file(target / "initramfs.xz", payload)
                try:
                    initramfs = lzma.decompress(payload, format=lzma.FORMAT_XZ)
                except lzma.LZMAError as exc:
                    raise FormatError(f"cannot decompress TILE initramfs: {exc}") from exc
                write_file(target / "initramfs.cpio", initramfs)
                found = cpio_init(initramfs)
                if found:
                    write_file(target / "init", found[0], found[1] & 0o777)
                    entry = elf_entry(found[0])
                    if entry is not None:
                        write_file(target / "init.entry", f"0x{entry:x}\n".encode())
        if "vmlinux" not in names or "initramfs.cpio.gz" not in names:
            raise FormatError("TILE HvFs lacks vmlinux or initramfs")
        return {"variant": "tile", "hvfs_entries": names}

    compressed = elf_section(wrapper, "initrd")
    if compressed is None:
        # TILE embeds a hypervisor filesystem in `fs`, not a Linux initrd.
        return None
    streams = list(xz_streams(compressed))
    if not streams:
        raise FormatError("boot/kernel has an initrd section but no XZ stream")
    linux = streams[0][2]
    initramfs = streams[1][2] if len(streams) > 1 else b""
    variant = classify_ppc(linux) if arch == "ppc" else arch
    target = output / variant
    write_file(target / "kernel.raw", linux)
    write_file(target / "kernel.wrapper.elf", wrapper, 0o755)
    if initramfs:
        write_file(target / "initramfs.cpio", initramfs)
        found = cpio_init(initramfs)
        if found:
            write_file(target / "init", found[0], found[1] & 0o777)
            entry = elf_entry(found[0])
            if entry is not None:
                write_file(target / "init.entry", f"0x{entry:x}\n".encode())

    if arch == "ppc":
        # The decompressed PPC image is linked relocatably and begins at zero.
        elf = make_elf32(linux, machine=20, endian=">", vaddr=0, entry=0, flags=0)
        write_file(target / "kernel.qemu.elf", elf, 0o755)
    elif arch in ("mipsbe", "smips"):
        elf = make_elf32(
            linux, machine=8, endian=">", vaddr=0x80011000,
            entry=0x80011000, flags=0x70001005,
        )
        write_file(target / "kernel.qemu.elf", elf, 0o755)
    elif arch == "mmips":
        elf = make_elf32(
            linux, machine=8, endian="<", vaddr=0x80011000,
            entry=0x80011000, flags=0x70001005,
        )
        write_file(target / "kernel.qemu.elf", elf, 0o755)
    return {
        "variant": variant,
        "linux_bytes": len(linux),
        "initramfs_bytes": len(initramfs),
        "xz_streams": len(streams),
    }


def extract(npk: Path, arch: str, output: Path):
    data = npk.read_bytes()
    squashfs = None
    archive = None
    for kind, body in npk_sections(data):
        if kind == 21:
            squashfs = body
        elif kind == 4:
            try:
                archive = zlib.decompress(body)
            except zlib.error as exc:
                raise FormatError(f"cannot decompress NPK file archive: {exc}") from exc
    if squashfs is None or archive is None:
        raise FormatError("NPK lacks a SquashFS or file-archive section")

    output.mkdir(parents=True, exist_ok=True)
    write_file(output / arch / "rootfs.squashfs", squashfs)
    entries = []
    kernel_index = 0
    boot = []
    for mode, name, payload in archive_entries(archive):
        entries.append({"name": name, "mode": oct(mode), "bytes": len(payload)})
        if name == "boot/kernel":
            result = boot_artifacts(arch, payload, output)
            if result:
                boot.append(result)
            else:
                write_file(output / arch / f"kernel-{kernel_index}.elf", payload, 0o755)
            kernel_index += 1
        elif name == "boot/initrd.rgz":
            write_file(output / arch / "initrd.rgz", payload)
        elif name == "boot/EFI/BOOT/BOOTX64.EFI":
            write_file(output / arch / "BOOTX64.EFI", payload, 0o755)

    metadata = {
        "source": str(npk),
        "architecture": arch,
        "squashfs_bytes": len(squashfs),
        "archive_entries": entries,
        "boot_artifacts": boot,
    }
    write_file(output / arch / "metadata.json", (json.dumps(metadata, indent=2) + "\n").encode())
    return metadata


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("npk", type=Path)
    parser.add_argument("architecture", choices=("x86", "arm", "arm64", "mipsbe", "mmips", "smips", "ppc", "tile"))
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        metadata = extract(args.npk, args.architecture, args.output)
    except (OSError, FormatError) as exc:
        print(f"npk_extract.py: {exc}", file=sys.stderr)
        return 1
    print(json.dumps(metadata["boot_artifacts"], indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
