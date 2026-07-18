"""Honest partial AArch64 register replies for GDB's remote protocol.

GDB permits a remote stub to return literal ``x`` digits for a register that
exists but is unavailable.  This module uses that representation to combine a
captured userspace core-register set with the complete register layout QEMU
advertised in its recursively fetched target descriptions.  In particular,
optional floating-point and system registers are never represented by made-up
zeroes.

Target-description XML does not carry target byte order.  The caller therefore
has to supply it explicitly; this also keeps integer serialization independent
of the host running the facade.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import PurePosixPath
import re
from typing import Mapping
import xml.etree.ElementTree as ET


class PartialRegisterError(ValueError):
    """A target description or supplied partial register set is unusable."""


AARCH64_USER_REGISTERS = tuple(f"x{number}" for number in range(31)) + (
    "sp",
    "pc",
    "cpsr",
)

_EXPECTED_BITS = {
    name: 32 if name == "cpsr" else 64 for name in AARCH64_USER_REGISTERS
}
_DECIMAL = re.compile(r"[0-9]+")
_HEX = re.compile(r"0[xX][0-9a-fA-F]+")
_XI_NAMESPACE = "http://www.w3.org/2001/XInclude"


@dataclass(frozen=True)
class RegisterDescription:
    """One described remote register in GDB ``g``-packet order."""

    name: str
    regnum: int
    bitsize: int


@dataclass(frozen=True)
class Aarch64PartialRegisterLayout:
    """Validated AArch64 target-description layout and partial encoder."""

    registers: tuple[RegisterDescription, ...]
    byte_order: str

    @classmethod
    def from_target_descriptions(
        cls,
        descriptions: Mapping[bytes | str, bytes | str],
        *,
        byte_order: str,
    ) -> "Aarch64PartialRegisterLayout":
        """Parse ``target.xml`` and its includes in target-description order.

        ``descriptions`` is normally the already-fetched mapping returned by
        the live facade's recursive QEMU target-description loader.  Annex
        names may be bytes or ASCII strings.  Register numbers omitted in XML
        follow GDB's global previous-register rule across feature includes.
        """

        if byte_order not in {"little", "big"}:
            raise PartialRegisterError("byte_order must be 'little' or 'big'")

        documents: dict[str, bytes] = {}
        for raw_name, raw_document in descriptions.items():
            try:
                name = (
                    raw_name.decode("ascii")
                    if isinstance(raw_name, bytes)
                    else raw_name
                )
            except UnicodeDecodeError as exc:
                raise PartialRegisterError(
                    "target-description annex name is not ASCII"
                ) from exc
            if not isinstance(name, str) or not name:
                raise PartialRegisterError("target-description annex name is invalid")
            if name in documents:
                raise PartialRegisterError(
                    f"ambiguous duplicate target-description annex {name!r}"
                )
            if isinstance(raw_document, str):
                document = raw_document.encode("utf-8")
            elif isinstance(raw_document, bytes):
                document = raw_document
            else:
                raise PartialRegisterError(
                    f"target-description annex {name!r} is not bytes or text"
                )
            documents[name] = document

        if "target.xml" not in documents:
            raise PartialRegisterError("target description lacks target.xml")

        registers: list[RegisterDescription] = []
        names: set[str] = set()
        numbers: set[int] = set()
        active: list[str] = []
        next_regnum = 0
        architecture: str | None = None

        def parse_document(annex: str) -> ET.Element:
            try:
                document = documents[annex]
            except KeyError as exc:
                raise PartialRegisterError(
                    f"target-description include {annex!r} was not fetched"
                ) from exc
            try:
                return ET.fromstring(document)
            except ET.ParseError as first:
                # QEMU's generated target.xml relies on the gdb-target.dtd to
                # declare the conventional xi prefix.  ElementTree does not
                # load that external DTD, so reproduce only that fixed binding
                # when the otherwise-normal QEMU document uses an unbound xi.
                if b"xi:" not in document or b"xmlns:xi" in document:
                    raise PartialRegisterError(
                        f"malformed target-description annex {annex!r}: {first}"
                    ) from first
                root_match = re.search(rb"<(target|feature)(?=[\s>])", document)
                if root_match is None:
                    raise PartialRegisterError(
                        f"malformed target-description annex {annex!r}: {first}"
                    ) from first
                insertion = root_match.end()
                with_namespace = (
                    document[:insertion]
                    + f' xmlns:xi="{_XI_NAMESPACE}"'.encode("ascii")
                    + document[insertion:]
                )
                try:
                    return ET.fromstring(with_namespace)
                except ET.ParseError as exc:
                    raise PartialRegisterError(
                        f"malformed target-description annex {annex!r}: {exc}"
                    ) from exc

        def include_name(parent: str, href: str) -> str:
            if not href or any(character in href for character in (":", "?", "#")):
                raise PartialRegisterError(
                    f"unsafe target-description include href {href!r}"
                )
            path = PurePosixPath(href)
            if path.is_absolute():
                raise PartialRegisterError(
                    f"unsafe target-description include href {href!r}"
                )
            combined = PurePosixPath(parent).parent.joinpath(path)
            parts: list[str] = []
            for part in combined.parts:
                if part in {"", "."}:
                    continue
                if part == "..":
                    if not parts:
                        raise PartialRegisterError(
                            f"unsafe target-description include href {href!r}"
                        )
                    parts.pop()
                else:
                    parts.append(part)
            if not parts:
                raise PartialRegisterError(
                    f"unsafe target-description include href {href!r}"
                )
            return "/".join(parts)

        def parse_nonnegative_integer(text: str, field: str, name: str) -> int:
            if _DECIMAL.fullmatch(text):
                value = int(text, 10)
            elif _HEX.fullmatch(text):
                value = int(text, 16)
            else:
                raise PartialRegisterError(
                    f"register {name!r} has invalid {field} {text!r}"
                )
            return value

        def visit(annex: str) -> None:
            nonlocal next_regnum, architecture
            if annex in active:
                chain = " -> ".join((*active, annex))
                raise PartialRegisterError(
                    f"cyclic target-description include: {chain}"
                )
            active.append(annex)
            try:
                root = parse_document(annex)

                def walk(element: ET.Element) -> None:
                    nonlocal next_regnum, architecture
                    local = element.tag.rsplit("}", 1)[-1]
                    if local == "architecture":
                        value = (element.text or "").strip()
                        if not value:
                            raise PartialRegisterError(
                                "target description has an empty architecture"
                            )
                        if architecture is not None:
                            raise PartialRegisterError(
                                "target description has ambiguous architecture declarations"
                            )
                        architecture = value
                    elif local == "include":
                        href = element.attrib.get("href")
                        if href is None:
                            raise PartialRegisterError(
                                "target-description include lacks href"
                            )
                        parse = element.attrib.get("parse", "xml")
                        if parse != "xml":
                            raise PartialRegisterError(
                                "target-description include is not XML"
                            )
                        allowed = {"href", "parse"}
                        if any(
                            key.rsplit("}", 1)[-1] not in allowed
                            for key in element.attrib
                        ):
                            raise PartialRegisterError(
                                "target-description include has unsupported attributes"
                            )
                        visit(include_name(annex, href))
                        return
                    elif local == "reg":
                        name = element.attrib.get("name")
                        bits_text = element.attrib.get("bitsize")
                        if not name or bits_text is None:
                            raise PartialRegisterError(
                                "target-description register is incomplete"
                            )
                        bitsize = parse_nonnegative_integer(
                            bits_text, "bitsize", name
                        )
                        if bitsize == 0 or bitsize % 8:
                            raise PartialRegisterError(
                                f"register {name!r} has non-byte-sized bitsize {bitsize}"
                            )
                        regnum_text = element.attrib.get("regnum")
                        regnum = (
                            next_regnum
                            if regnum_text is None
                            else parse_nonnegative_integer(
                                regnum_text, "regnum", name
                            )
                        )
                        next_regnum = regnum + 1
                        if name in names:
                            raise PartialRegisterError(
                                f"duplicate target register name {name!r}"
                            )
                        if regnum in numbers:
                            raise PartialRegisterError(
                                f"ambiguous duplicate target register number {regnum}"
                            )
                        names.add(name)
                        numbers.add(regnum)
                        registers.append(
                            RegisterDescription(name, regnum, bitsize)
                        )
                    for child in element:
                        walk(child)

                walk(root)
            finally:
                active.pop()

        visit("target.xml")
        if architecture not in {"aarch64", "aarch64:little"}:
            raise PartialRegisterError(
                f"target architecture is {architecture!r}, expected AArch64"
            )
        if architecture == "aarch64:little" and byte_order != "little":
            raise PartialRegisterError(
                "target architecture explicitly says little-endian"
            )

        by_name = {register.name: register for register in registers}
        missing = [name for name in AARCH64_USER_REGISTERS if name not in by_name]
        if missing:
            raise PartialRegisterError(
                "target description lacks required AArch64 registers: "
                + ", ".join(missing)
            )
        for name, expected in _EXPECTED_BITS.items():
            actual = by_name[name].bitsize
            if actual != expected:
                raise PartialRegisterError(
                    f"target describes {name!r} as {actual} bits, expected {expected}"
                )

        return cls(tuple(sorted(registers, key=lambda item: item.regnum)), byte_order)

    def encode_g_packet(self, values: Mapping[str, int]) -> bytes:
        """Return an unframed ASCII reply payload for an RSP ``g`` request.

        Every required AArch64 userspace core register must be supplied as an
        unsigned integer.  All other registers in the target description are
        encoded as one literal ``x`` per nybble, GDB's specified marker for a
        known register whose value cannot be accessed.
        """

        supplied = set(values)
        required = set(AARCH64_USER_REGISTERS)
        missing = required - supplied
        extra = supplied - required
        if missing:
            raise PartialRegisterError(
                "partial register values lack: " + ", ".join(sorted(missing))
            )
        if extra:
            raise PartialRegisterError(
                "partial register values contain unknown names: "
                + ", ".join(sorted(extra))
            )

        chunks: list[bytes] = []
        for register in self.registers:
            if register.name not in required:
                chunks.append(b"x" * (register.bitsize // 4))
                continue
            value = values[register.name]
            if (
                not isinstance(value, int)
                or isinstance(value, bool)
                or value < 0
                or value >= 1 << register.bitsize
            ):
                raise PartialRegisterError(
                    f"invalid {register.bitsize}-bit value for {register.name!r}: "
                    f"{value!r}"
                )
            raw = value.to_bytes(register.bitsize // 8, self.byte_order)
            chunks.append(raw.hex().encode("ascii"))
        return b"".join(chunks)
