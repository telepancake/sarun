#!/usr/bin/env python3
"""Build an exact userspace symbol catalog from an in-memory newc image.

The caller supplies one initramfs byte string and a finite collection of
provenance-reached debugger ELF candidates.  This module does not extract the
archive, walk a directory, resolve a host path, inspect the environment, or
invoke another program.  GNU build ID is preferred; when either ELF lacks one,
an exact normalized PT_LOAD identity proves the association without paths.

Matched rows use the ``viros-image-bundle-v1`` executable vocabulary.  Valid
runtime ELFs with no unique matching DWARF file are listed in ``unmatched`` and
omitted from ``executables``; the live kernel view can therefore continue to
represent their processes as inferiors without attaching incorrect symbols.
"""

from __future__ import annotations

from dataclasses import dataclass
import gzip
import hashlib
import io
import stat
import struct
from typing import Mapping, Sequence

from probe.image_inspector import CapturedArtifact
from probe.elf_load_identity import (
    ElfLoadIdentity,
    ElfLoadIdentityError,
    elf_load_identity,
    same_loadable_content,
)


FORMAT = "viros-newc-userspace-catalog-v1"
MAX_ARCHIVE_BYTES = 1024 * 1024 * 1024
MAX_MEMBERS = 131_072
MAX_MEMBER_NAME_BYTES = 4096
MAX_CANDIDATES = 100_000
MAX_CANDIDATE_BYTES = 128 * 1024 * 1024 * 1024
_ELF_MAGIC = b"\x7fELF"
_SHT_NOTE = 7
_PT_NOTE = 4
_ET_EXEC = 2
_ET_DYN = 3


class NewcCatalogError(RuntimeError):
    """The archive, candidate identities, or symbol association is invalid."""


@dataclass(frozen=True)
class DebugElfCandidate:
    artifact: CapturedArtifact
    contents: bytes

    def __post_init__(self) -> None:
        if not isinstance(self.artifact, CapturedArtifact):
            raise NewcCatalogError("debug candidate descriptor has the wrong type")
        if not isinstance(self.contents, bytes):
            raise NewcCatalogError("debug candidate contents must be bytes")
        if len(self.contents) != self.artifact.size:
            raise NewcCatalogError(
                f"debug candidate size mismatch: box {self.artifact.box_id}:"
                f"{self.artifact.path}"
            )
        if hashlib.sha256(self.contents).hexdigest() != self.artifact.sha256:
            raise NewcCatalogError(
                f"debug candidate SHA-256 mismatch: box {self.artifact.box_id}:"
                f"{self.artifact.path}"
            )


@dataclass(frozen=True)
class UserspaceCatalog:
    archive_format: str
    executables: tuple[Mapping[str, object], ...]
    unmatched: tuple[str, ...]

    def descriptor(self) -> dict[str, object]:
        return {
            "format": FORMAT,
            "archive_format": self.archive_format,
            "executables": [dict(row) for row in self.executables],
            "unmatched": list(self.unmatched),
        }


def select_kernel_init(initramfs: bytes) -> str:
    """Select the conventional executable init from one exact archive.

    The order is part of the fixed initramfs profile.  No caller-provided path
    and no filesystem search participates in the decision.
    """

    _archive_format, members = _parse_newc(initramfs)
    by_path = {member.path: member for member in members}
    for path in ("/init", "/sbin/init"):
        member = by_path.get(path)
        if (
            member is not None
            and stat.S_ISREG(member.mode)
            and member.mode & 0o111
        ):
            return path
    raise NewcCatalogError(
        "initramfs contains neither executable /init nor executable /sbin/init"
    )


@dataclass(frozen=True)
class _Member:
    path: str
    mode: int
    contents: bytes


@dataclass(frozen=True)
class _ElfIdentity:
    elf_class: int
    machine: int
    endian: str
    elf_type: int
    build_id: str | None
    section_names: frozenset[str]
    load_identity: ElfLoadIdentity | None

    @property
    def architecture_key(self) -> tuple[int, int, str]:
        return self.elf_class, self.machine, self.endian

    @property
    def architecture(self) -> str | None:
        return {
            (32, 3, "<"): "x86",
            (64, 62, "<"): "x86_64",
            (32, 40, "<"): "arm",
            (32, 40, ">"): "arm",
            (64, 183, "<"): "aarch64",
            (32, 8, "<"): "mmips",
            (32, 8, ">"): "mipsbe",
            (64, 8, "<"): "mips64",
            (64, 8, ">"): "mips64",
            (32, 20, ">"): "powerpc",
            (64, 21, ">"): "powerpc",
            (64, 191, "<"): "tilegx",
            (64, 191, ">"): "tilegx",
        }.get(self.architecture_key)

    @property
    def has_dwarf(self) -> bool:
        return (
            bool(self.section_names & {".debug_info", ".zdebug_info"})
            and bool(self.section_names & {".debug_line", ".zdebug_line"})
        )


def _align4(value: int) -> int:
    return (value + 3) & ~3


def _hex_field(header: bytes, index: int) -> int:
    field = header[6 + index * 8 : 14 + index * 8]
    if len(field) != 8 or any(byte not in b"0123456789abcdefABCDEF" for byte in field):
        raise NewcCatalogError("newc header contains a non-hexadecimal field")
    try:
        return int(field, 16)
    except ValueError as exc:
        raise NewcCatalogError("newc header contains a non-hexadecimal field") from exc


def _guest_path(raw: bytes) -> str:
    if not raw or raw[-1:] != b"\0" or b"\0" in raw[:-1]:
        raise NewcCatalogError("newc member name is not singly NUL terminated")
    try:
        value = raw[:-1].decode("utf-8")
    except UnicodeDecodeError as exc:
        raise NewcCatalogError("newc member name is not UTF-8") from exc
    while value.startswith("./"):
        value = value[2:]
    if (
        not value
        or value.startswith("/")
        or "\\" in value
        or any(part in {"", ".", ".."} for part in value.split("/"))
    ):
        raise NewcCatalogError(f"unsafe newc member path: {value!r}")
    return "/" + value


def _decode_gzip(data: bytes) -> bytes:
    try:
        with gzip.GzipFile(fileobj=io.BytesIO(data)) as stream:
            decoded = stream.read(MAX_ARCHIVE_BYTES + 1)
            if len(decoded) > MAX_ARCHIVE_BYTES:
                raise NewcCatalogError("gzip initramfs expands beyond the archive bound")
            if stream.read(1):
                raise NewcCatalogError("gzip decoder retained unexpected data")
            return decoded
    except (OSError, EOFError) as exc:
        raise NewcCatalogError(f"invalid gzip initramfs: {exc}") from exc


def _parse_newc(data: bytes) -> tuple[str, tuple[_Member, ...]]:
    if not isinstance(data, bytes):
        raise NewcCatalogError("initramfs contents must be bytes")
    archive_format = "newc"
    if data.startswith(b"\x1f\x8b"):
        data = _decode_gzip(data)
        archive_format = "gzip-newc"
    if len(data) > MAX_ARCHIVE_BYTES:
        raise NewcCatalogError("initramfs exceeds the archive bound")

    cursor = 0
    members: list[_Member] = []
    paths: set[str] = set()
    trailer = False
    while cursor < len(data):
        if data[cursor:] == b"\0" * (len(data) - cursor):
            break
        if len(data) - cursor < 110:
            raise NewcCatalogError("truncated newc header")
        header = data[cursor : cursor + 110]
        magic = header[:6]
        if magic not in {b"070701", b"070702"}:
            raise NewcCatalogError("initramfs is not a newc archive")
        mode = _hex_field(header, 1)
        file_size = _hex_field(header, 6)
        name_size = _hex_field(header, 11)
        checksum = _hex_field(header, 12)
        if name_size == 0 or name_size > MAX_MEMBER_NAME_BYTES:
            raise NewcCatalogError("newc member name exceeds its bound")
        name_at = cursor + 110
        name_end = name_at + name_size
        contents_at = _align4(name_end)
        contents_end = contents_at + file_size
        next_cursor = _align4(contents_end)
        if name_end > len(data) or contents_end > len(data) or next_cursor > len(data):
            raise NewcCatalogError("newc member extends beyond the archive")
        name = data[name_at:name_end]
        contents = data[contents_at:contents_end]
        if magic == b"070702":
            if sum(contents) & 0xFFFFFFFF != checksum:
                raise NewcCatalogError("newc CRC member checksum does not match")
        elif checksum != 0:
            raise NewcCatalogError("newc member has a checksum in a non-CRC archive")
        if name == b"TRAILER!!!\0":
            if file_size != 0:
                raise NewcCatalogError("newc trailer carries file contents")
            trailer = True
            cursor = next_cursor
            break
        path = _guest_path(name)
        if path in paths:
            raise NewcCatalogError(f"duplicate newc member path: {path}")
        paths.add(path)
        members.append(_Member(path, mode, contents))
        if len(members) > MAX_MEMBERS:
            raise NewcCatalogError("newc archive has too many members")
        cursor = next_cursor

    if not trailer:
        raise NewcCatalogError("newc archive has no trailer")
    if any(data[cursor:]):
        raise NewcCatalogError("newc archive has nonzero data after its trailer")
    return archive_format, tuple(members)


def _bounded_slice(data: bytes, offset: int, size: int, label: str) -> bytes:
    if offset < 0 or size < 0 or offset > len(data) or size > len(data) - offset:
        raise NewcCatalogError(f"ELF {label} extends beyond the file")
    return data[offset : offset + size]


def _notes(data: bytes, endian: str) -> set[bytes]:
    identifiers: set[bytes] = set()
    cursor = 0
    while cursor < len(data):
        if data[cursor:] == b"\0" * (len(data) - cursor):
            break
        if len(data) - cursor < 12:
            raise NewcCatalogError("truncated ELF note")
        name_size, description_size, note_type = struct.unpack_from(
            endian + "III", data, cursor
        )
        cursor += 12
        name_end = cursor + name_size
        description_at = _align4(name_end)
        description_end = description_at + description_size
        next_cursor = _align4(description_end)
        if next_cursor > len(data):
            raise NewcCatalogError("ELF note extends beyond its container")
        owner = data[cursor:name_end].rstrip(b"\0")
        description = data[description_at:description_end]
        if owner == b"GNU" and note_type == 3:
            if not 4 <= len(description) <= 64:
                raise NewcCatalogError("GNU build ID has an unsupported size")
            identifiers.add(description)
        cursor = next_cursor
    return identifiers


def _elf_identity(data: bytes) -> _ElfIdentity:
    if len(data) < 20 or data[:4] != _ELF_MAGIC:
        raise NewcCatalogError("not an ELF file")
    elf_class = {1: 32, 2: 64}.get(data[4])
    endian = {1: "<", 2: ">"}.get(data[5])
    if elf_class is None or endian is None:
        raise NewcCatalogError("unsupported ELF class or byte order")
    minimum = 52 if elf_class == 32 else 64
    if len(data) < minimum:
        raise NewcCatalogError("truncated ELF header")
    if data[6] != 1 or struct.unpack_from(endian + "I", data, 20)[0] != 1:
        raise NewcCatalogError("unsupported ELF version")
    elf_type, machine = struct.unpack_from(endian + "HH", data, 16)
    if elf_class == 32:
        phoff, shoff = struct.unpack_from(endian + "II", data, 28)
        phentsize, phnum, shentsize, shnum, shstrndx = struct.unpack_from(
            endian + "HHHHH", data, 42
        )
        ph_format = endian + "IIIIIIII"
        sh_format = endian + "IIIIIIIIII"
    else:
        phoff, shoff = struct.unpack_from(endian + "QQ", data, 32)
        phentsize, phnum, shentsize, shnum, shstrndx = struct.unpack_from(
            endian + "HHHHH", data, 54
        )
        ph_format = endian + "IIQQQQQQ"
        sh_format = endian + "IIQQQQIIQQ"

    identifiers: set[bytes] = set()
    ph_minimum = struct.calcsize(ph_format)
    if phnum and phentsize < ph_minimum:
        raise NewcCatalogError("invalid ELF program-header size")
    for index in range(phnum):
        header = _bounded_slice(data, phoff + index * phentsize, ph_minimum, "program header")
        values = struct.unpack(ph_format, header)
        if values[0] != _PT_NOTE:
            continue
        if elf_class == 32:
            offset, size = values[1], values[4]
        else:
            offset, size = values[2], values[5]
        identifiers.update(_notes(_bounded_slice(data, offset, size, "note segment"), endian))

    section_names: set[str] = set()
    sh_minimum = struct.calcsize(sh_format)
    sections: list[tuple[int, ...]] = []
    if shnum:
        if shentsize < sh_minimum:
            raise NewcCatalogError("invalid ELF section-header size")
        for index in range(shnum):
            header = _bounded_slice(
                data, shoff + index * shentsize, sh_minimum, "section header"
            )
            sections.append(struct.unpack(sh_format, header))
        if shstrndx >= len(sections):
            raise NewcCatalogError("invalid ELF section-name table")
        names_row = sections[shstrndx]
        names = _bounded_slice(data, names_row[4], names_row[5], "section-name table")
        for row in sections:
            name_offset = row[0]
            if name_offset >= len(names):
                raise NewcCatalogError("ELF section name is outside its string table")
            end = names.find(b"\0", name_offset)
            if end < 0:
                raise NewcCatalogError("ELF section name is not terminated")
            try:
                name = names[name_offset:end].decode("ascii")
            except UnicodeDecodeError as exc:
                raise NewcCatalogError("ELF section name is not ASCII") from exc
            section_names.add(name)
            if row[1] == _SHT_NOTE and row[5]:
                identifiers.update(
                    _notes(_bounded_slice(data, row[4], row[5], "note section"), endian)
                )
    if len(identifiers) != 1:
        if identifiers:
            raise NewcCatalogError("ELF has conflicting GNU build IDs")
    load_identity = None
    if elf_type in {_ET_EXEC, _ET_DYN}:
        try:
            load_identity = elf_load_identity(data)
        except ElfLoadIdentityError:
            # A valid GNU build ID remains the preferred exact association.
            # Without it this linked ELF simply cannot use the fallback.
            pass
    return _ElfIdentity(
        elf_class=elf_class,
        machine=machine,
        endian=endian,
        elf_type=elf_type,
        build_id=(next(iter(identifiers)).hex() if identifiers else None),
        section_names=frozenset(section_names),
        load_identity=load_identity,
    )


def catalog_newc_userspace(
    initramfs: bytes,
    candidates: Sequence[DebugElfCandidate],
) -> UserspaceCatalog:
    """Return deterministic executable rows for one exact newc initramfs."""

    if not isinstance(candidates, Sequence) or isinstance(candidates, (str, bytes)):
        raise NewcCatalogError("debug candidates must be a finite sequence")
    if len(candidates) > MAX_CANDIDATES:
        raise NewcCatalogError("debug candidate catalog is too large")
    total_candidate_bytes = 0
    validated_candidates: list[DebugElfCandidate] = []
    locations: set[tuple[int, str]] = set()
    for candidate in candidates:
        if not isinstance(candidate, DebugElfCandidate):
            raise NewcCatalogError("debug candidate has the wrong type")
        location = (candidate.artifact.box_id, candidate.artifact.path)
        if location in locations:
            raise NewcCatalogError(
                f"duplicate debug candidate: box {location[0]}:{location[1]}"
            )
        locations.add(location)
        total_candidate_bytes += len(candidate.contents)
        if total_candidate_bytes > MAX_CANDIDATE_BYTES:
            raise NewcCatalogError("debug candidate bytes exceed their aggregate bound")
        validated_candidates.append(candidate)

    archive_format, members = _parse_newc(initramfs)
    runtime: list[tuple[_Member, _ElfIdentity]] = []
    for member in members:
        if not stat.S_ISREG(member.mode) or not member.contents.startswith(_ELF_MAGIC):
            continue
        try:
            identity = _elf_identity(member.contents)
        except NewcCatalogError:
            # A malformed, relocatable, or build-ID-less ELF remains valid
            # guest content but cannot receive an exact symbol association.
            continue
        if identity.elf_type in {_ET_EXEC, _ET_DYN}:
            runtime.append((member, identity))

    wanted = {
        (identity.build_id, identity.architecture_key)
        for _member, identity in runtime
        if identity.build_id is not None
    }
    by_build_id: dict[
        tuple[str, tuple[int, int, str]],
        dict[str, list[tuple[DebugElfCandidate, _ElfIdentity]]],
    ] = {}
    fallback_candidates: list[tuple[DebugElfCandidate, _ElfIdentity]] = []
    for candidate in validated_candidates:
        try:
            identity = _elf_identity(candidate.contents)
        except NewcCatalogError:
            continue
        if (
            candidate.artifact.architecture is not None
            and candidate.artifact.architecture != identity.architecture
        ):
            raise NewcCatalogError(
                f"debug candidate architecture mismatch: box "
                f"{candidate.artifact.box_id}:{candidate.artifact.path}"
            )
        if identity.elf_type not in {_ET_EXEC, _ET_DYN} or not identity.has_dwarf:
            continue
        fallback_candidates.append((candidate, identity))
        if identity.build_id is None:
            continue
        key = identity.build_id, identity.architecture_key
        if (
            key not in wanted
        ):
            continue
        by_build_id.setdefault(key, {}).setdefault(candidate.artifact.sha256, []).append(
            (candidate, identity)
        )

    executables: list[Mapping[str, object]] = []
    unmatched: list[str] = []
    for member, identity in sorted(runtime, key=lambda row: row[0].path):
        contents: dict[str, list[tuple[DebugElfCandidate, _ElfIdentity]]] = {}
        association_id = identity.build_id
        association_kind = "gnu-build-id"
        if identity.build_id is not None:
            contents = by_build_id.get(
                (identity.build_id, identity.architecture_key), {}
            )
        if not contents and identity.load_identity is not None:
            association_kind = "pt-load-sha256"
            for candidate, candidate_identity in fallback_candidates:
                # Different nonempty build IDs are affirmative evidence that
                # these are not the same link result.  The fallback exists
                # only because at least one side has no GNU note.
                if (
                    identity.build_id is not None
                    and candidate_identity.build_id is not None
                ):
                    continue
                if (
                    candidate_identity.architecture_key != identity.architecture_key
                    or candidate_identity.elf_type != identity.elf_type
                    or candidate_identity.load_identity is None
                    or candidate_identity.load_identity.filter_key
                    != identity.load_identity.filter_key
                    or not same_loadable_content(
                        member.contents,
                        identity.load_identity,
                        candidate.contents,
                        candidate_identity.load_identity,
                    )
                ):
                    continue
                contents.setdefault(candidate.artifact.sha256, []).append(
                    (candidate, candidate_identity)
                )
        if not contents:
            unmatched.append(member.path)
            continue
        if len(contents) != 1:
            paths = sorted(
                candidate.artifact.path
                for copies in contents.values()
                for candidate, _candidate_identity in copies
            )
            label = (
                f"build ID {identity.build_id}"
                if association_kind == "gnu-build-id"
                else f"PT_LOAD identity {identity.load_identity.fingerprint}"
            )
            raise NewcCatalogError(
                f"{label} has ambiguous DWARF ELF contents: " + ", ".join(paths)
            )
        copies = next(iter(contents.values()))
        candidate, candidate_identity = min(
            copies,
            key=lambda row: (
                row[0].artifact.box_id,
                row[0].artifact.path,
                row[0].artifact.record_id,
            ),
        )
        if association_kind == "pt-load-sha256":
            association_id = (
                candidate_identity.build_id
                or candidate_identity.load_identity.fingerprint
            )
        if association_id is None:
            raise NewcCatalogError("matched ELF has no stable association identity")
        executables.append({
            "guest_path": member.path,
            "build_id": association_id,
            "runtime_sha256": hashlib.sha256(member.contents).hexdigest(),
            "runtime_size": len(member.contents),
            "debug_elf": candidate.artifact.path,
            "debug_box_id": candidate.artifact.box_id,
            "debug_record_id": candidate.artifact.record_id,
            "debug_sha256": candidate.artifact.sha256,
            "debug_size": candidate.artifact.size,
            "elf_class": identity.elf_class,
            "elf_machine": identity.machine,
            "source_view": "provider-root",
        })
    return UserspaceCatalog(
        archive_format=archive_format,
        executables=tuple(executables),
        unmatched=tuple(unmatched),
    )
