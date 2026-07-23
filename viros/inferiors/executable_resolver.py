"""Resolve a stopped Linux process to one finite Sarun executable catalog.

The resolver never treats a guest pathname as identity.  It reads the ELF
metadata retained in the process address space, prefers a GNU build ID, and
uses ``AT_EXECFN`` only to distinguish catalog rows which already have the
same verified identity.  A program without a build ID can use the established
PT_LOAD fingerprint only when every contributing byte is demonstrably retained
unchanged in a non-writable ET_EXEC mapping.

All reads and table sizes are bounded.  A missing mapping or malformed ELF is
an ordinary unresolved result, not a debugger-session failure.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import struct
from typing import Callable, Iterable, Protocol

from callgate.architectures import ArchitectureDescriptor
from callgate.transaction import CallGateError, RestorationError
from probe.abi import ProbeDecodeError
from probe.elf_load_identity import DOMAIN

from .linux_oracle import TaskSnapshot
from .qemu_rsp import RspRemoteError


AT_NULL = 0
AT_PHDR = 3
AT_PHENT = 4
AT_PHNUM = 5
AT_EXECFN = 31

PT_LOAD = 1
PT_NOTE = 4
PT_PHDR = 6
PF_X = 1
PF_W = 2
ET_EXEC = 2
ET_DYN = 3

NT_GNU_BUILD_ID = 3

MAX_AUXV_BYTES = 4096
MAX_PROGRAM_HEADERS = 128
MAX_PROGRAM_HEADER_BYTES = 64 * 1024
MAX_NOTE_BYTES = 1024 * 1024
MAX_TOTAL_NOTE_BYTES = 2 * 1024 * 1024
MAX_TOTAL_LOAD_BYTES = 32 * 1024 * 1024
MAX_EXECFN_BYTES = 4096
_READ_CHUNK = 64 * 1024


class CatalogExecutable(Protocol):
    guest_path: bytes
    build_id: bytes
    elf_class: int
    elf_machine: int


class ExecutableCatalog(Protocol):
    profile: int
    executables: tuple[CatalogExecutable, ...]


MemoryRead = Callable[[TaskSnapshot, int, int], bytes]


class ExecutableResolutionError(ValueError):
    """The stopped process does not expose a safely usable ELF identity."""


@dataclass(frozen=True)
class _ProgramHeader:
    kind: int
    offset: int
    virtual_address: int
    physical_address: int
    file_size: int
    memory_size: int
    flags: int
    alignment: int


@dataclass(frozen=True)
class _MemoryElf:
    elf_class: int
    endian: str
    endian_code: int
    machine: int
    elf_type: int
    load_bias: int
    header_address: int
    headers: tuple[_ProgramHeader, ...]


@dataclass(frozen=True)
class _CatalogIdentity:
    guest_path: bytes
    identity: bytes


_ARCHITECTURE_IDENTITY = {
    "aarch64": (1, 64, 183),
    "x86_64": (2, 64, 62),
    "arm": (3, 32, 40),
    "mmips": (4, 32, 8),
}


def _checked_add(left: int, right: int, limit: int, label: str) -> int:
    if left < 0 or right < 0 or left > limit or right > limit - left:
        raise ExecutableResolutionError(f"{label} is outside the task address space")
    return left + right


def _read_exact(
    read_memory: MemoryRead,
    task: TaskSnapshot,
    address: int,
    length: int,
    *,
    maximum: int,
) -> bytes:
    if length < 0 or length > maximum:
        raise ExecutableResolutionError("process-memory read exceeds its bound")
    try:
        data = read_memory(task, address, length)
    except RestorationError:
        raise
    except (
        CallGateError,
        LookupError,
        NotImplementedError,
        OSError,
        ProbeDecodeError,
        RspRemoteError,
    ) as exc:
        raise ExecutableResolutionError("process memory is unavailable") from exc
    if not isinstance(data, bytes) or len(data) != length:
        raise ExecutableResolutionError("process-memory read was incomplete")
    return data


def _auxiliary_vector(
    task: TaskSnapshot, elf_class: int, endian: str
) -> dict[int, int]:
    data = task.auxv
    if not isinstance(data, bytes) or len(data) > MAX_AUXV_BYTES:
        raise ExecutableResolutionError("auxiliary vector exceeds its bound")
    word = 8 if elf_class == 64 else 4
    pair_size = word * 2
    if not data or len(data) % pair_size:
        raise ExecutableResolutionError("auxiliary vector has an invalid size")
    code = "Q" if word == 8 else "I"
    values: dict[int, int] = {}
    terminated = False
    pairs = tuple(struct.iter_unpack(endian + code * 2, data))
    for index, (tag, value) in enumerate(pairs):
        if tag == AT_NULL:
            if value != 0 or index != len(pairs) - 1:
                raise ExecutableResolutionError("AT_NULL is not canonical")
            terminated = True
            break
        if tag in values:
            raise ExecutableResolutionError("auxiliary vector repeats a tag")
        values[tag] = value
    if not terminated:
        raise ExecutableResolutionError("auxiliary vector is not terminated")
    return values


def _program_headers(
    data: bytes,
    *,
    elf_class: int,
    endian: str,
    address_limit: int,
) -> tuple[_ProgramHeader, ...]:
    item_format = endian + ("IIQQQQQQ" if elf_class == 64 else "IIIIIIII")
    item_size = struct.calcsize(item_format)
    if not data or len(data) % item_size:
        raise ExecutableResolutionError("program-header table has an invalid size")
    rows = []
    for offset in range(0, len(data), item_size):
        values = struct.unpack_from(item_format, data, offset)
        if elf_class == 64:
            (
                kind,
                flags,
                file_offset,
                virtual,
                physical,
                file_size,
                memory_size,
                alignment,
            ) = values
        else:
            (
                kind,
                file_offset,
                virtual,
                physical,
                file_size,
                memory_size,
                flags,
                alignment,
            ) = values
        if file_size > memory_size:
            raise ExecutableResolutionError("PT_LOAD file size exceeds memory size")
        if alignment not in {0, 1} and alignment & (alignment - 1):
            raise ExecutableResolutionError("program-header alignment is invalid")
        _checked_add(file_offset, file_size, address_limit, "ELF file range")
        _checked_add(virtual, memory_size, address_limit, "ELF virtual range")
        rows.append(
            _ProgramHeader(
                kind,
                file_offset,
                virtual,
                physical,
                file_size,
                memory_size,
                flags,
                alignment,
            )
        )
    return tuple(rows)


def _header_fields(
    data: bytes, *, elf_class: int, endian: str, machine: int
) -> tuple[int, int, int, int, int]:
    header_size = 64 if elf_class == 64 else 52
    class_code = 2 if elf_class == 64 else 1
    endian_code = 1 if endian == "<" else 2
    if (
        len(data) != header_size
        or data[:7] != b"\x7fELF" + bytes((class_code, endian_code, 1))
    ):
        raise ExecutableResolutionError("mapped ELF header has incompatible identity")
    elf_type, found_machine = struct.unpack_from(endian + "HH", data, 16)
    if found_machine != machine or elf_type not in {ET_EXEC, ET_DYN}:
        raise ExecutableResolutionError(
            "mapped ELF architecture or type is incompatible"
        )
    if struct.unpack_from(endian + "I", data, 20)[0] != 1:
        raise ExecutableResolutionError("mapped ELF version is unsupported")
    if elf_class == 64:
        phoff = struct.unpack_from(endian + "Q", data, 32)[0]
        ehsize, phentsize, phnum = struct.unpack_from(endian + "HHH", data, 52)
    else:
        phoff = struct.unpack_from(endian + "I", data, 28)[0]
        ehsize, phentsize, phnum = struct.unpack_from(endian + "HHH", data, 40)
    if ehsize != header_size:
        raise ExecutableResolutionError("mapped ELF header size is noncanonical")
    return elf_type, phoff, phentsize, phnum, endian_code


def _load_bias_and_header(
    *,
    task: TaskSnapshot,
    read_memory: MemoryRead,
    at_phdr: int,
    table_size: int,
    headers: tuple[_ProgramHeader, ...],
    elf_class: int,
    endian: str,
    machine: int,
    address_limit: int,
) -> tuple[int, int, int, int]:
    header_size = 64 if elf_class == 64 else 52
    phdr_biases = {
        at_phdr - row.virtual_address
        for row in headers
        if row.kind == PT_PHDR
        and row.file_size >= table_size
        and at_phdr >= row.virtual_address
    }

    candidates: list[tuple[int, int]] = []
    for bias in phdr_biases:
        for load in headers:
            if load.kind != PT_LOAD or load.offset != 0 or load.file_size < header_size:
                continue
            try:
                address = _checked_add(
                    bias, load.virtual_address, address_limit, "mapped ELF header"
                )
            except ExecutableResolutionError:
                continue
            candidates.append((bias, address))

    # Normal Linux binaries place the program-header table immediately after
    # the ELF header.  This bounded candidate supports binaries without a
    # PT_PHDR row; all relationships are subsequently verified from the ELF.
    if at_phdr >= header_size:
        standard_header = at_phdr - header_size
        for load in headers:
            if (
                load.kind != PT_LOAD
                or load.offset != 0
                or load.file_size < header_size + table_size
            ):
                continue
            if standard_header >= load.virtual_address:
                candidates.append(
                    (standard_header - load.virtual_address, standard_header)
                )

    valid: set[tuple[int, int, int, int]] = set()
    for bias, address in candidates:
        if bias < 0:
            continue
        try:
            raw = _read_exact(
                read_memory, task, address, header_size, maximum=header_size
            )
            elf_type, phoff, phentsize, phnum, endian_code = _header_fields(
                raw, elf_class=elf_class, endian=endian, machine=machine
            )
            if (
                phentsize * phnum != table_size
                or _checked_add(address, phoff, address_limit, "program-header address")
                != at_phdr
            ):
                continue
            # The in-memory table must be retained by a PT_LOAD with the exact
            # file-offset/virtual-address relationship.
            phend = phoff + table_size
            retained = any(
                load.kind == PT_LOAD
                and load.offset <= phoff
                and phend <= load.offset + load.file_size
                and at_phdr
                == bias + load.virtual_address + (phoff - load.offset)
                for load in headers
            )
            if retained:
                valid.add((bias, address, elf_type, endian_code))
        except ExecutableResolutionError:
            continue
    if len(valid) != 1:
        raise ExecutableResolutionError(
            "mapped ELF load bias is unavailable or ambiguous"
        )
    return next(iter(valid))


def _mapped_elf(
    task: TaskSnapshot,
    read_memory: MemoryRead,
    *,
    elf_class: int,
    endian: str,
    machine: int,
) -> tuple[_MemoryElf, dict[int, int]]:
    auxiliary = _auxiliary_vector(task, elf_class, endian)
    try:
        at_phdr = auxiliary[AT_PHDR]
        phentsize = auxiliary[AT_PHENT]
        phnum = auxiliary[AT_PHNUM]
    except KeyError as exc:
        raise ExecutableResolutionError("auxiliary vector lacks ELF metadata") from exc
    expected_phentsize = 56 if elf_class == 64 else 32
    if phentsize != expected_phentsize or not 0 < phnum <= MAX_PROGRAM_HEADERS:
        raise ExecutableResolutionError("auxiliary program-header geometry is invalid")
    table_size = phentsize * phnum
    if table_size > MAX_PROGRAM_HEADER_BYTES:
        raise ExecutableResolutionError("program-header table exceeds its bound")
    address_limit = (1 << elf_class) - 1
    _checked_add(at_phdr, table_size, address_limit, "program-header table")
    raw_headers = _read_exact(
        read_memory,
        task,
        at_phdr,
        table_size,
        maximum=MAX_PROGRAM_HEADER_BYTES,
    )
    headers = _program_headers(
        raw_headers,
        elf_class=elf_class,
        endian=endian,
        address_limit=address_limit,
    )
    if not any(row.kind == PT_LOAD for row in headers):
        raise ExecutableResolutionError("mapped ELF has no PT_LOAD")
    bias, header_address, elf_type, endian_code = _load_bias_and_header(
        task=task,
        read_memory=read_memory,
        at_phdr=at_phdr,
        table_size=table_size,
        headers=headers,
        elf_class=elf_class,
        endian=endian,
        machine=machine,
        address_limit=address_limit,
    )
    return (
        _MemoryElf(
            elf_class,
            endian,
            endian_code,
            machine,
            elf_type,
            bias,
            header_address,
            headers,
        ),
        auxiliary,
    )


def _file_range_is_retained(
    row: _ProgramHeader, loads: tuple[_ProgramHeader, ...]
) -> bool:
    end = row.offset + row.file_size
    return any(
        load.kind == PT_LOAD
        and load.offset <= row.offset
        and end <= load.offset + load.file_size
        and row.virtual_address
        == load.virtual_address + (row.offset - load.offset)
        for load in loads
    )


def _gnu_build_id(
    image: _MemoryElf, task: TaskSnapshot, read_memory: MemoryRead
) -> bytes | None:
    notes = [row for row in image.headers if row.kind == PT_NOTE and row.file_size]
    if sum(row.file_size for row in notes) > MAX_TOTAL_NOTE_BYTES:
        raise ExecutableResolutionError("ELF notes exceed their aggregate bound")
    identifiers: set[bytes] = set()
    loads = tuple(row for row in image.headers if row.kind == PT_LOAD)
    address_limit = (1 << image.elf_class) - 1
    for note in notes:
        if note.file_size > MAX_NOTE_BYTES or not _file_range_is_retained(note, loads):
            raise ExecutableResolutionError("ELF note is not safely retained in memory")
        address = _checked_add(
            image.load_bias,
            note.virtual_address,
            address_limit,
            "mapped ELF note",
        )
        _checked_add(
            address,
            note.file_size,
            address_limit,
            "mapped ELF note range",
        )
        data = _read_exact(
            read_memory,
            task,
            address,
            note.file_size,
            maximum=MAX_NOTE_BYTES,
        )
        at = 0
        while at < len(data):
            if len(data) - at < 12:
                if any(data[at:]):
                    raise ExecutableResolutionError("ELF note header is truncated")
                break
            namesz, descsz, note_type = struct.unpack_from(
                image.endian + "III", data, at
            )
            if namesz == 0 and descsz == 0 and note_type == 0:
                if any(data[at:]):
                    raise ExecutableResolutionError(
                        "ELF note padding is malformed"
                    )
                break
            at += 12
            name_end = _checked_add(at, namesz, len(data), "ELF note name")
            name = data[at:name_end].rstrip(b"\0")
            at = (name_end + 3) & ~3
            desc_end = _checked_add(at, descsz, len(data), "ELF note description")
            description = data[at:desc_end]
            at = (desc_end + 3) & ~3
            if at > len(data):
                raise ExecutableResolutionError("ELF note padding is truncated")
            if name == b"GNU" and note_type == NT_GNU_BUILD_ID:
                if not 4 <= len(description) <= 64:
                    raise ExecutableResolutionError("GNU build ID has an invalid size")
                identifiers.add(description)
    if len(identifiers) > 1:
        raise ExecutableResolutionError("mapped ELF has conflicting GNU build IDs")
    return next(iter(identifiers)) if identifiers else None


def _read_load_contents(
    image: _MemoryElf,
    row: _ProgramHeader,
    task: TaskSnapshot,
    read_memory: MemoryRead,
) -> bytes:
    address_limit = (1 << image.elf_class) - 1
    address = _checked_add(
        image.load_bias, row.virtual_address, address_limit, "mapped PT_LOAD"
    )
    _checked_add(
        address,
        row.file_size,
        address_limit,
        "mapped PT_LOAD range",
    )
    result = bytearray()
    remaining = row.file_size
    while remaining:
        length = min(remaining, _READ_CHUNK)
        result.extend(
            _read_exact(
                read_memory,
                task,
                address + len(result),
                length,
                maximum=_READ_CHUNK,
            )
        )
        remaining -= length
    section_fields = (
        ((40, 8), (58, 2), (60, 2), (62, 2))
        if image.elf_class == 64
        else ((32, 4), (46, 2), (48, 2), (50, 2))
    )
    for field_offset, field_size in section_fields:
        if (
            row.offset < field_offset + field_size
            and field_offset < row.offset + row.file_size
        ):
            start = max(field_offset, row.offset) - row.offset
            end = (
                min(field_offset + field_size, row.offset + row.file_size)
                - row.offset
            )
            result[start:end] = b"\0" * (end - start)
    return bytes(result)


def _exact_load_identity(
    image: _MemoryElf, task: TaskSnapshot, read_memory: MemoryRead
) -> bytes | None:
    loads = tuple(
        sorted(
            (row for row in image.headers if row.kind == PT_LOAD),
            key=lambda row: (
                row.virtual_address,
                row.file_size,
                row.memory_size,
                row.flags,
                row.alignment,
                row.offset,
            ),
        )
    )
    # A dynamic loader may relocate ET_DYN text, and any PF_W segment may have
    # changed since exec.  Neither can prove the file-content fingerprint from
    # a stopped process, so the fallback is deliberately unavailable there.
    if image.elf_type != ET_EXEC or any(row.flags & PF_W for row in loads):
        return None
    if not any(row.file_size and row.flags & PF_X for row in loads):
        return None
    if sum(row.file_size for row in loads) > MAX_TOTAL_LOAD_BYTES:
        raise ExecutableResolutionError("PT_LOAD contents exceed their aggregate bound")

    class_code = 2 if image.elf_class == 64 else 1
    digest = hashlib.sha256()
    digest.update(DOMAIN)
    digest.update(
        struct.pack(
            ">BBHHI",
            class_code,
            image.endian_code,
            image.machine,
            image.elf_type,
            len(loads),
        )
    )
    for row in loads:
        contents = _read_load_contents(image, row, task, read_memory)
        digest.update(
            struct.pack(
                ">QQQIQ",
                row.virtual_address,
                row.file_size,
                row.memory_size,
                row.flags,
                row.alignment,
            )
        )
        digest.update(contents)
    return digest.hexdigest().encode("ascii")


def _read_execfn(
    task: TaskSnapshot,
    read_memory: MemoryRead,
    address: int | None,
    *,
    address_limit: int,
) -> bytes | None:
    if address is None or not 0 < address <= address_limit:
        return None
    result = bytearray()
    while len(result) < MAX_EXECFN_BYTES:
        remaining_page = 4096 - (address & 4095)
        length = min(256, remaining_page, MAX_EXECFN_BYTES - len(result))
        try:
            chunk = _read_exact(
                read_memory, task, address, length, maximum=256
            )
        except ExecutableResolutionError:
            return None
        terminator = chunk.find(b"\0")
        if terminator >= 0:
            result.extend(chunk[:terminator])
            return bytes(result)
        result.extend(chunk)
        address += length
        if address > address_limit:
            return None
    return None


class LiveExecutableResolver:
    """Map memory identity to exactly one catalog guest executable path."""

    def __init__(
        self,
        architecture: ArchitectureDescriptor,
        executables: Iterable[CatalogExecutable],
        *,
        profile: int | None = None,
    ) -> None:
        try:
            expected_profile, elf_class, machine = _ARCHITECTURE_IDENTITY[
                architecture.name
            ]
        except KeyError as exc:
            raise ValueError(
                f"no executable resolver for architecture {architecture.name!r}"
            ) from exc
        if architecture.target_byte_order != "little":
            raise ValueError(
                "live executable resolver currently requires little-endian ELF"
            )
        if profile is not None and profile != expected_profile:
            raise ValueError(
                "image catalog profile does not match the live architecture"
            )
        rows = []
        for row in executables:
            if row.elf_class != elf_class or row.elf_machine != machine:
                raise ValueError(
                    "catalog executable architecture does not match the live image"
                )
            if (
                not isinstance(row.guest_path, bytes)
                or not row.guest_path.startswith(b"/")
                or b"\0" in row.guest_path
                or any(
                    part in {b"", b".", b".."}
                    for part in row.guest_path[1:].split(b"/")
                )
                or not isinstance(row.build_id, bytes)
            ):
                raise ValueError("catalog executable identity is invalid")
            try:
                identity = (
                    bytes.fromhex(row.build_id.decode("ascii"))
                    .hex()
                    .encode("ascii")
                )
                row.guest_path.decode("utf-8")
            except (UnicodeDecodeError, ValueError) as exc:
                raise ValueError("catalog executable identity is invalid") from exc
            if identity != row.build_id or not 4 <= len(identity) // 2 <= 64:
                raise ValueError("catalog executable identity is noncanonical")
            rows.append(_CatalogIdentity(row.guest_path, identity))
        self.architecture = architecture
        self.elf_class = elf_class
        self.machine = machine
        self.endian = "<"
        self.rows = tuple(rows)

    @classmethod
    def from_catalog(
        cls, architecture: ArchitectureDescriptor, catalog: ExecutableCatalog
    ) -> "LiveExecutableResolver":
        try:
            profile = catalog.profile
            executables = catalog.executables
        except AttributeError as exc:
            raise ValueError("image catalog has no executable records") from exc
        return cls(architecture, executables, profile=profile)

    def _matched_path(
        self,
        identity: bytes,
        *,
        task: TaskSnapshot,
        read_memory: MemoryRead,
        execfn_address: int | None,
    ) -> str:
        paths = {
            row.guest_path for row in self.rows if row.identity == identity
        }
        if len(paths) == 1:
            return next(iter(paths)).decode("utf-8")
        if len(paths) > 1:
            execfn = _read_execfn(
                task,
                read_memory,
                execfn_address,
                address_limit=(1 << self.elf_class) - 1,
            )
            if execfn in paths:
                return execfn.decode("utf-8")
        return ""

    def resolve(self, task: TaskSnapshot, read_memory: MemoryRead) -> str:
        """Return one verified guest path, or an empty unresolved result."""

        if not self.rows:
            return ""
        try:
            image, auxiliary = _mapped_elf(
                task,
                read_memory,
                elf_class=self.elf_class,
                endian=self.endian,
                machine=self.machine,
            )
            build_id = _gnu_build_id(image, task, read_memory)
            if build_id is not None:
                path = self._matched_path(
                    build_id.hex().encode("ascii"),
                    task=task,
                    read_memory=read_memory,
                    execfn_address=auxiliary.get(AT_EXECFN),
                )
                if path:
                    return path
                # Catalog construction can relate a runtime carrying a GNU
                # note to a debug ELF which does not.  In that case the wire
                # identity is the common PT_LOAD fingerprint; build ID was
                # still attempted first and the fallback retains its stricter
                # unchanged-content requirements below.
            load_identity = _exact_load_identity(image, task, read_memory)
            if load_identity is None:
                return ""
            return self._matched_path(
                load_identity,
                task=task,
                read_memory=read_memory,
                execfn_address=auxiliary.get(AT_EXECFN),
            )
        except ExecutableResolutionError:
            return ""
