"""Exact identity for the bytes an ELF loader maps into a process.

GNU build IDs remain the preferred executable identity.  This module supplies
the deterministic fallback for linked ELFs without one.  It deliberately uses
program headers rather than sections because a deployed stripped executable
may have no section-header table at all.

The only normalized bytes are the ELF header fields which locate and describe
the section-header table.  Stripping debug-only sections changes those fields
even though it does not change the load image.  All other PT_LOAD bytes are
hashed and, for association, compared directly.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import struct


DOMAIN = b"viros-elf-pt-load-v1\0"
PT_LOAD = 1
ET_EXEC = 2
ET_DYN = 3
_ELF_MAGIC = b"\x7fELF"


class ElfLoadIdentityError(ValueError):
    pass


@dataclass(frozen=True)
class LoadSegment:
    offset: int
    virtual_address: int
    physical_address: int
    file_size: int
    memory_size: int
    flags: int
    alignment: int
    content_sha256: bytes

    @property
    def layout(self) -> tuple[int, int, int, int, int]:
        return (
            self.virtual_address,
            self.file_size,
            self.memory_size,
            self.flags,
            self.alignment,
        )


@dataclass(frozen=True)
class ElfLoadIdentity:
    elf_class: int
    endian: str
    machine: int
    elf_type: int
    segments: tuple[LoadSegment, ...]
    fingerprint: str

    @property
    def architecture_key(self) -> tuple[int, int, str]:
        return self.elf_class, self.machine, self.endian

    @property
    def filter_key(self) -> tuple[object, ...]:
        return (
            self.elf_class,
            self.machine,
            self.endian,
            self.elf_type,
            tuple((segment.layout, segment.content_sha256) for segment in self.segments),
        )


def _bounded(data: bytes, offset: int, size: int, label: str) -> memoryview:
    if offset < 0 or size < 0 or offset > len(data) or size > len(data) - offset:
        raise ElfLoadIdentityError(f"ELF {label} extends beyond the file")
    return memoryview(data)[offset : offset + size]


def _normalized_segment(
    data: bytes,
    offset: int,
    size: int,
    section_fields: tuple[tuple[int, int], ...],
) -> bytes | memoryview:
    contents = _bounded(data, offset, size, "load segment")
    overlaps = [
        (field_offset, field_size)
        for field_offset, field_size in section_fields
        if field_offset < offset + size and offset < field_offset + field_size
    ]
    if not overlaps:
        return contents
    normalized = bytearray(contents)
    for field_offset, field_size in overlaps:
        start = max(field_offset, offset) - offset
        end = min(field_offset + field_size, offset + size) - offset
        normalized[start:end] = b"\0" * (end - start)
    return bytes(normalized)


def elf_load_identity(data: bytes) -> ElfLoadIdentity:
    """Return the canonical linked-ELF PT_LOAD identity for ``data``."""

    if not isinstance(data, bytes):
        raise ElfLoadIdentityError("ELF contents must be bytes")
    if len(data) < 20 or data[:4] != _ELF_MAGIC:
        raise ElfLoadIdentityError("not an ELF file")
    class_code = data[4]
    endian_code = data[5]
    elf_class = {1: 32, 2: 64}.get(class_code)
    endian = {1: "<", 2: ">"}.get(endian_code)
    if elf_class is None or endian is None:
        raise ElfLoadIdentityError("unsupported ELF class or byte order")
    header_size = 52 if elf_class == 32 else 64
    if len(data) < header_size:
        raise ElfLoadIdentityError("truncated ELF header")
    if data[6] != 1 or struct.unpack_from(endian + "I", data, 20)[0] != 1:
        raise ElfLoadIdentityError("unsupported ELF version")
    elf_type, machine = struct.unpack_from(endian + "HH", data, 16)
    if elf_type not in {ET_EXEC, ET_DYN}:
        raise ElfLoadIdentityError("ELF is not ET_EXEC or ET_DYN")
    if elf_class == 32:
        phoff = struct.unpack_from(endian + "I", data, 28)[0]
        phentsize, phnum = struct.unpack_from(endian + "HH", data, 42)
        ph_format = endian + "IIIIIIII"
        section_fields = ((32, 4), (46, 2), (48, 2), (50, 2))
    else:
        phoff = struct.unpack_from(endian + "Q", data, 32)[0]
        phentsize, phnum = struct.unpack_from(endian + "HH", data, 54)
        ph_format = endian + "IIQQQQQQ"
        section_fields = ((40, 8), (58, 2), (60, 2), (62, 2))
    ph_minimum = struct.calcsize(ph_format)
    if phnum == 0 or phnum == 0xFFFF:
        raise ElfLoadIdentityError("ELF has no directly encoded program headers")
    if phentsize < ph_minimum:
        raise ElfLoadIdentityError("invalid ELF program-header size")
    if phoff > len(data) or phnum > (len(data) - phoff) // phentsize:
        raise ElfLoadIdentityError("ELF program-header table extends beyond the file")

    rows: list[
        tuple[tuple[int, int, int, int, int, int, int], bytes | memoryview]
    ] = []
    for index in range(phnum):
        values = struct.unpack_from(ph_format, data, phoff + index * phentsize)
        if values[0] != PT_LOAD:
            continue
        if elf_class == 32:
            offset, virtual, physical, file_size, memory_size, flags, alignment = values[1:]
        else:
            flags = values[1]
            offset, virtual, physical, file_size, memory_size, alignment = values[2:]
        if file_size > memory_size:
            raise ElfLoadIdentityError("ELF load segment file size exceeds memory size")
        if alignment not in {0, 1} and alignment & (alignment - 1):
            raise ElfLoadIdentityError("ELF load segment alignment is not a power of two")
        segment_contents = _normalized_segment(
            data, offset, file_size, section_fields
        )
        rows.append(
            (
                (offset, virtual, physical, file_size, memory_size, flags, alignment),
                segment_contents,
            )
        )
    if not rows:
        raise ElfLoadIdentityError("ELF has no PT_LOAD segments")
    if not any(row[0][3] and row[0][5] & 1 for row in rows):
        raise ElfLoadIdentityError("ELF has no nonempty executable PT_LOAD segment")
    rows.sort(
        key=lambda item: (
            item[0][1],
            item[0][3],
            item[0][4],
            item[0][5],
            item[0][6],
            item[0][0],
        )
    )

    digest = hashlib.sha256()
    digest.update(DOMAIN)
    digest.update(struct.pack(">BBHHI", class_code, endian_code, machine, elf_type, len(rows)))
    segments: list[LoadSegment] = []
    for row, contents in rows:
        offset, virtual, physical, file_size, memory_size, flags, alignment = row
        digest.update(
            struct.pack(">QQQIQ", virtual, file_size, memory_size, flags, alignment)
        )
        digest.update(contents)
        segments.append(
            LoadSegment(
                offset=offset,
                virtual_address=virtual,
                physical_address=physical,
                file_size=file_size,
                memory_size=memory_size,
                flags=flags,
                alignment=alignment,
                content_sha256=hashlib.sha256(contents).digest(),
            )
        )
    return ElfLoadIdentity(
        elf_class=elf_class,
        endian=endian,
        machine=machine,
        elf_type=elf_type,
        segments=tuple(segments),
        fingerprint=digest.hexdigest(),
    )


def same_loadable_content(
    left_data: bytes,
    left: ElfLoadIdentity,
    right_data: bytes,
    right: ElfLoadIdentity,
) -> bool:
    """Prove equality after the cheap canonical layout/hash filter."""

    if left.filter_key != right.filter_key:
        return False
    left_fields = (
        ((32, 4), (46, 2), (48, 2), (50, 2))
        if left.elf_class == 32
        else ((40, 8), (58, 2), (60, 2), (62, 2))
    )
    right_fields = (
        ((32, 4), (46, 2), (48, 2), (50, 2))
        if right.elf_class == 32
        else ((40, 8), (58, 2), (60, 2), (62, 2))
    )
    return all(
        _normalized_segment(left_data, left_row.offset, left_row.file_size, left_fields)
        == _normalized_segment(right_data, right_row.offset, right_row.file_size, right_fields)
        for left_row, right_row in zip(left.segments, right.segments, strict=True)
    )
