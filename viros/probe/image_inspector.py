#!/usr/bin/env python3
"""Deterministically describe one selected firmware image.

This is an internal Sarun/ViroS boundary, not a path-discovery command.  The
caller supplies the bytes of the file the user selected, its captured-file
identity, and the finite set of captured artifacts reached through Sarun's
build records.  The inspector only relates content by size and SHA-256.  It
never searches a directory, consults the host environment, or interprets a
file name as evidence.

The result is deliberately JSON-shaped.  Constituent identities use the same
``path``/``size``/``sha256`` vocabulary as viros-kernel-bundle-v1 and
viros-image-bundle-v1, so the engine can materialize an internal bundle and
then pass it through the existing validators without weakening them.
"""

from __future__ import annotations

from dataclasses import dataclass
import gzip
import hashlib
import io
import struct
import tarfile
import zlib
from typing import Iterable, Mapping, Sequence


FORMAT = "viros-selected-image-derivation-v1"
MAX_IMAGE_BYTES = 4 * 1024 * 1024 * 1024
MAX_DECODED_BYTES = 1024 * 1024 * 1024
MAX_TAR_MEMBERS = 16_384
MAX_FDT_NODES = 65_536

_HEX = frozenset("0123456789abcdef")
_ARCHITECTURES = {
    "aarch64", "arm", "mmips", "mipsbe", "mips64", "powerpc",
    "tilegx", "x86", "x86_64",
}
_ROLES = {
    "device-tree", "disk", "firmware", "initramfs", "kernel",
    "kernel-boot", "rootfs", "vmlinux",
}


class ImageInspectionError(RuntimeError):
    """The selected bytes or supplied captured identities are inconsistent."""


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _safe_relative_path(value: str) -> str:
    if not isinstance(value, str):
        raise ImageInspectionError("captured path must be text")
    if not value or value.startswith("/") or "\\" in value or "\x00" in value:
        raise ImageInspectionError(f"captured path is not provider-relative: {value!r}")
    parts = value.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        raise ImageInspectionError(f"captured path is not normalized: {value!r}")
    return value


def _digest(value: str) -> str:
    if not isinstance(value, str):
        raise ImageInspectionError("captured SHA-256 must be text")
    value = value.lower()
    if len(value) != 64 or any(character not in _HEX for character in value):
        raise ImageInspectionError("captured SHA-256 must be 64 lowercase hex digits")
    return value


@dataclass(frozen=True)
class CapturedArtifact:
    """One immutable file identity supplied from Sarun's captured build graph."""

    box_id: int
    path: str
    size: int
    sha256: str
    record_id: str
    roles: tuple[str, ...] = ()
    architecture: str | None = None

    def __post_init__(self) -> None:
        if isinstance(self.box_id, bool) or not isinstance(self.box_id, int) or self.box_id < 0:
            raise ImageInspectionError("captured box_id must be a non-negative integer")
        object.__setattr__(self, "path", _safe_relative_path(self.path))
        if isinstance(self.size, bool) or not isinstance(self.size, int) or self.size < 0:
            raise ImageInspectionError("captured size must be a non-negative integer")
        object.__setattr__(self, "sha256", _digest(self.sha256))
        if not isinstance(self.record_id, str) or not self.record_id or "\x00" in self.record_id:
            raise ImageInspectionError("captured record_id must be non-empty")
        roles = tuple(sorted(set(self.roles)))
        if any(role not in _ROLES for role in roles):
            raise ImageInspectionError(f"unknown captured artifact role in {roles!r}")
        object.__setattr__(self, "roles", roles)
        if self.architecture is not None and self.architecture not in _ARCHITECTURES:
            raise ImageInspectionError(
                f"unknown captured architecture: {self.architecture!r}"
            )

    @classmethod
    def from_mapping(cls, value: Mapping[str, object]) -> "CapturedArtifact":
        allowed = {
            "box_id", "path", "size", "sha256", "record_id", "roles",
            "architecture",
        }
        extra = set(value) - allowed
        if extra:
            raise ImageInspectionError(f"unknown captured artifact fields: {sorted(extra)!r}")
        try:
            roles_value = value.get("roles", ())
            if isinstance(roles_value, (str, bytes)):
                raise TypeError
            return cls(
                box_id=value["box_id"],  # type: ignore[arg-type]
                path=value["path"],  # type: ignore[arg-type]
                size=value["size"],  # type: ignore[arg-type]
                sha256=value["sha256"],  # type: ignore[arg-type]
                record_id=value["record_id"],  # type: ignore[arg-type]
                roles=tuple(roles_value),  # type: ignore[arg-type]
                architecture=value.get("architecture"),  # type: ignore[arg-type]
            )
        except (KeyError, TypeError) as exc:
            raise ImageInspectionError("malformed captured artifact") from exc

    def descriptor(self) -> dict[str, object]:
        result: dict[str, object] = {
            "box_id": self.box_id,
            "path": self.path,
            "size": self.size,
            "sha256": self.sha256,
            "record_id": self.record_id,
            "roles": list(self.roles),
        }
        if self.architecture is not None:
            result["architecture"] = self.architecture
        return result


def _identity(data: bytes) -> dict[str, object]:
    return {"size": len(data), "sha256": _sha256(data)}


class _Catalog:
    def __init__(self, records: Iterable[CapturedArtifact]):
        self._records: list[CapturedArtifact] = []
        self._by_content: dict[tuple[int, str], list[CapturedArtifact]] = {}
        identities: set[tuple[int, str, str]] = set()
        for record in records:
            key = (record.box_id, record.path, record.record_id)
            if key in identities:
                raise ImageInspectionError(
                    f"duplicate captured artifact identity: box {record.box_id}:{record.path}"
                )
            identities.add(key)
            self._records.append(record)
            self._by_content.setdefault((record.size, record.sha256), []).append(record)
        self._records.sort(key=lambda row: (row.box_id, row.path, row.record_id))
        for matches in self._by_content.values():
            matches.sort(key=lambda row: (row.box_id, row.path, row.record_id))

    def matches(self, data: bytes) -> list[dict[str, object]]:
        rows = self._by_content.get((len(data), _sha256(data)), ())
        return [row.descriptor() for row in rows]

    def typed_matches(self, data: bytes) -> tuple[set[str], set[str]]:
        rows = self._by_content.get((len(data), _sha256(data)), ())
        roles = {role for row in rows for role in row.roles}
        architectures = {row.architecture for row in rows if row.architecture is not None}
        return roles, architectures  # type: ignore[return-value]

    def supporting(self, role: str, architecture: str | None) -> list[dict[str, object]]:
        return [
            row.descriptor()
            for row in self._records
            if role in row.roles
            and (architecture is None or row.architecture in {None, architecture})
        ]


def _component(
    role: str,
    media_type: str,
    data: bytes,
    locator: Mapping[str, object],
    catalog: _Catalog,
    *,
    architecture: str | None = None,
    metadata: Mapping[str, object] | None = None,
) -> dict[str, object]:
    if architecture is None:
        _roles, matched_architectures = catalog.typed_matches(data)
        if len(matched_architectures) == 1:
            architecture = next(iter(matched_architectures))
    result: dict[str, object] = {
        "role": role,
        "media_type": media_type,
        "identity": _identity(data),
        "artifact": {
            "path": f"derived/{role}-{_sha256(data)[:24]}.bin",
            "size": len(data),
            "sha256": _sha256(data),
        },
        "locator": dict(locator),
        "captured_matches": catalog.matches(data),
    }
    if architecture is not None:
        result["architecture"] = architecture
    if metadata:
        result["metadata"] = dict(metadata)
    return result


def _c_string(value: bytes) -> str:
    return value.split(b"\0", 1)[0].decode("utf-8", errors="strict")


def _tar_role(name: str) -> str | None:
    base = name.rsplit("/", 1)[-1]
    if base == "kernel" or base.startswith("kernel."):
        return "kernel"
    if base in {"root", "rootfs"} or base.startswith("rootfs."):
        return "rootfs"
    return None


def _inspect_sysupgrade_tar(data: bytes, catalog: _Catalog) -> dict[str, object] | None:
    try:
        archive = tarfile.open(fileobj=io.BytesIO(data), mode="r:*")
    except (tarfile.TarError, OSError):
        return None
    components: list[dict[str, object]] = []
    seen: set[str] = set()
    has_control = False
    has_sysupgrade_directory = False
    try:
        members = archive.getmembers()
        if len(members) > MAX_TAR_MEMBERS:
            raise ImageInspectionError("sysupgrade archive has too many members")
        for member in members:
            name = member.name.removeprefix("./")
            if not name or name.startswith("/") or "\\" in name:
                raise ImageInspectionError(f"unsafe sysupgrade member name: {member.name!r}")
            if any(part in {"", ".", ".."} for part in name.split("/")):
                raise ImageInspectionError(f"unsafe sysupgrade member name: {member.name!r}")
            if name in seen:
                raise ImageInspectionError(f"duplicate sysupgrade member: {name}")
            seen.add(name)
            has_control |= name.rsplit("/", 1)[-1] == "CONTROL"
            has_sysupgrade_directory |= any(
                part.startswith("sysupgrade-") for part in name.split("/")[:-1]
            )
            role = _tar_role(name)
            if role is None:
                continue
            if not member.isfile():
                raise ImageInspectionError(f"sysupgrade constituent is not regular: {name}")
            if member.size > MAX_DECODED_BYTES:
                raise ImageInspectionError(f"sysupgrade constituent is too large: {name}")
            stream = archive.extractfile(member)
            if stream is None:
                raise ImageInspectionError(f"cannot read sysupgrade constituent: {name}")
            contents = stream.read(MAX_DECODED_BYTES + 1)
            if len(contents) != member.size:
                raise ImageInspectionError(f"truncated sysupgrade constituent: {name}")
            locator: dict[str, object] = {
                "kind": "container-member",
                "container": "tar",
                "member": name,
            }
            # Plain tar members can be copied directly by range.  For compressed
            # tar the member name remains the deterministic extraction contract.
            if data[257:262] == b"ustar":
                locator.update({"offset": member.offset_data, "length": member.size})
            components.append(_component(
                role,
                "application/vnd.openwrt.sysupgrade.kernel" if role == "kernel"
                else "application/vnd.openwrt.sysupgrade.rootfs",
                contents,
                locator,
                catalog,
            ))
    finally:
        archive.close()
    if not components or not (has_control or has_sysupgrade_directory):
        return None
    components.sort(key=lambda row: (str(row["role"]), str(row["locator"])))
    return {"kind": "openwrt-sysupgrade-tar", "components": components}


_UIMAGE_ARCH = {
    # The U-Boot MIPS architecture constants do not encode byte order.  A
    # matching captured artifact can supply mmips/mipsbe without guessing.
    2: "arm", 3: "x86", 6: "mips64", 7: "powerpc",
    22: "aarch64", 24: "x86_64",
}
_UIMAGE_TYPE_ROLE = {
    2: "kernel", 3: "initramfs", 5: "firmware", 7: "rootfs",
}
_UIMAGE_COMPRESSION = {0: "none", 1: "gzip", 2: "bzip2", 3: "lzma", 5: "lz4", 6: "zstd"}


def _inspect_uimage(data: bytes, catalog: _Catalog) -> dict[str, object] | None:
    if len(data) < 64 or data[:4] != b"'\x05\x19V":
        return None
    fields = struct.unpack(">7I4B32s", data[:64])
    magic, header_crc, timestamp, size, load, entry, payload_crc = fields[:7]
    os_id, arch_id, image_type, compression, name = fields[7:]
    if magic != 0x27051956 or size > len(data) - 64:
        raise ImageInspectionError("uImage payload size does not match the selected file")
    header = bytearray(data[:64])
    header[4:8] = b"\0" * 4
    if zlib.crc32(header) & 0xFFFFFFFF != header_crc:
        raise ImageInspectionError("uImage header CRC does not match")
    payload = data[64:64 + size]
    if zlib.crc32(payload) & 0xFFFFFFFF != payload_crc:
        raise ImageInspectionError("uImage payload CRC does not match")
    architecture = _UIMAGE_ARCH.get(arch_id)
    common = {
        "load": load, "entry": entry, "compression": _UIMAGE_COMPRESSION.get(compression, f"id-{compression}"),
        "name": _c_string(name), "os_id": os_id, "timestamp": timestamp,
    }
    components: list[dict[str, object]] = []
    if image_type == 4:  # IH_TYPE_MULTI
        lengths: list[int] = []
        cursor = 0
        while True:
            if cursor + 4 > len(payload):
                raise ImageInspectionError("truncated uImage multi-file length table")
            length = struct.unpack_from(">I", payload, cursor)[0]
            cursor += 4
            if length == 0:
                break
            lengths.append(length)
            if len(lengths) > 128:
                raise ImageInspectionError("uImage has too many multi-file constituents")
        for index, length in enumerate(lengths):
            if cursor + length > len(payload):
                raise ImageInspectionError("truncated uImage multi-file constituent")
            contents = payload[cursor:cursor + length]
            role = "kernel" if index == 0 else "initramfs" if index == 1 else "firmware"
            components.append(_component(
                role, "application/vnd.u-boot.multi-component", contents,
                {"kind": "selected-range", "offset": 64 + cursor, "length": length},
                catalog, architecture=architecture,
                metadata={**common, "index": index},
            ))
            cursor = (cursor + length + 3) & ~3
        if cursor not in {len(payload), (len(payload) + 3) & ~3}:
            raise ImageInspectionError("uImage multi-file layout has trailing bytes")
    else:
        role = _UIMAGE_TYPE_ROLE.get(image_type, "firmware")
        components.append(_component(
            role, "application/vnd.u-boot.image", payload,
            {"kind": "selected-range", "offset": 64, "length": len(payload)},
            catalog, architecture=architecture,
            metadata={**common, "image_type": image_type},
        ))
    trailing = data[64 + size:]
    if trailing:
        # OpenWrt commonly concatenates an aligned squashfs after a uImage.
        # Accept padding only when it leads to exactly one recognized rootfs.
        offsets = [
            offset for magic in (b"hsqs", b"sqsh", b"qshs", b"shsq")
            for offset in [trailing.find(magic)] if offset >= 0
        ]
        offsets = sorted(set(offsets))
        if len(offsets) == 1 and all(byte in {0, 0xFF} for byte in trailing[:offsets[0]]):
            offset = offsets[0]
            rootfs = trailing[offset:]
            components.append(_component(
                "rootfs", "application/vnd.squashfs", rootfs,
                {
                    "kind": "selected-range",
                    "offset": 64 + size + offset,
                    "length": len(rootfs),
                },
                catalog, architecture=architecture,
            ))
        elif any(byte not in {0, 0xFF} for byte in trailing):
            raise ImageInspectionError("uImage has unrecognized trailing firmware bytes")
    return {"kind": "uimage", "architecture": architecture, "components": components}


_FDT_MAGIC = 0xD00DFEED
_FDT_BEGIN_NODE = 1
_FDT_END_NODE = 2
_FDT_PROP = 3
_FDT_NOP = 4
_FDT_END = 9


def _be_integer(value: bytes, name: str) -> int:
    if len(value) not in {4, 8}:
        raise ImageInspectionError(f"FIT {name} is not a 32/64-bit integer")
    return int.from_bytes(value, "big")


def _fit_string(value: bytes, name: str) -> str:
    if not value.endswith(b"\0") or b"\0" in value[:-1]:
        raise ImageInspectionError(f"FIT {name} is not one string")
    try:
        return value[:-1].decode("ascii")
    except UnicodeDecodeError as exc:
        raise ImageInspectionError(f"FIT {name} is not ASCII") from exc


def _inspect_fit(data: bytes, catalog: _Catalog) -> dict[str, object] | None:
    if len(data) < 40 or struct.unpack_from(">I", data)[0] != _FDT_MAGIC:
        return None
    header = struct.unpack_from(">10I", data)
    _, total_size, off_struct, off_strings, _off_reserve, version, last_version, _cpu, strings_size, struct_size = header
    if total_size < 40 or total_size > len(data):
        raise ImageInspectionError("FIT total size is outside the selected file")
    if version < 16 or last_version > version:
        raise ImageInspectionError("unsupported FIT/FDT version")
    if off_struct + struct_size > total_size or off_strings + strings_size > total_size:
        raise ImageInspectionError("FIT structure/string block is outside the FDT")
    strings = data[off_strings:off_strings + strings_size]
    cursor = off_struct
    end = off_struct + struct_size
    stack: list[str] = []
    properties: dict[str, dict[str, tuple[bytes, int]]] = {}
    nodes = 0
    saw_end = False
    while cursor + 4 <= end:
        token = struct.unpack_from(">I", data, cursor)[0]
        cursor += 4
        if token == _FDT_BEGIN_NODE:
            nul = data.find(b"\0", cursor, end)
            if nul < 0:
                raise ImageInspectionError("unterminated FIT node name")
            try:
                node = data[cursor:nul].decode("ascii")
            except UnicodeDecodeError as exc:
                raise ImageInspectionError("non-ASCII FIT node name") from exc
            if "/" in node:
                raise ImageInspectionError("FIT node name contains slash")
            stack.append(node)
            cursor = (nul + 4) & ~3
            nodes += 1
            if nodes > MAX_FDT_NODES:
                raise ImageInspectionError("FIT contains too many nodes")
        elif token == _FDT_END_NODE:
            if not stack:
                raise ImageInspectionError("unbalanced FIT end-node")
            stack.pop()
        elif token == _FDT_PROP:
            if cursor + 8 > end or not stack:
                raise ImageInspectionError("truncated FIT property")
            length, name_offset = struct.unpack_from(">2I", data, cursor)
            cursor += 8
            if cursor + length > end or name_offset >= len(strings):
                raise ImageInspectionError("FIT property is outside its block")
            name_end = strings.find(b"\0", name_offset)
            if name_end < 0:
                raise ImageInspectionError("unterminated FIT property name")
            try:
                prop_name = strings[name_offset:name_end].decode("ascii")
            except UnicodeDecodeError as exc:
                raise ImageInspectionError("non-ASCII FIT property name") from exc
            path = "/" + "/".join(part for part in stack if part)
            values = properties.setdefault(path, {})
            if prop_name in values:
                raise ImageInspectionError(f"duplicate FIT property {path}:{prop_name}")
            values[prop_name] = (data[cursor:cursor + length], cursor)
            cursor = (cursor + length + 3) & ~3
        elif token == _FDT_NOP:
            continue
        elif token == _FDT_END:
            saw_end = True
            break
        else:
            raise ImageInspectionError(f"unknown FIT structure token {token}")
    if not saw_end or stack:
        raise ImageInspectionError("unterminated FIT structure")

    configurations = properties.get("/configurations", {})
    config_nodes = sorted(path for path in properties if path.count("/") == 2 and path.startswith("/configurations/"))
    selected_names: set[str] = set()
    selected_config: str | None = None
    default = configurations.get("default")
    if default is not None:
        selected_config = _fit_string(default[0], "configurations/default")
    elif len(config_nodes) == 1:
        selected_config = config_nodes[0].rsplit("/", 1)[-1]
    if selected_config is not None:
        config = properties.get(f"/configurations/{selected_config}")
        if config is None:
            raise ImageInspectionError("FIT default configuration does not exist")
        for key in ("kernel", "ramdisk", "fdt", "firmware"):
            if key in config:
                selected_names.add(_fit_string(config[key][0], f"configuration/{key}"))

    image_nodes = sorted(path for path in properties if path.count("/") == 2 and path.startswith("/images/"))
    if not image_nodes:
        raise ImageInspectionError("FIT contains no /images children")
    if selected_names:
        available = {path.rsplit("/", 1)[-1] for path in image_nodes}
        missing = selected_names - available
        if missing:
            raise ImageInspectionError(f"FIT configuration names absent images: {sorted(missing)!r}")
        image_nodes = [path for path in image_nodes if path.rsplit("/", 1)[-1] in selected_names]
    elif len(image_nodes) > 1:
        raise ImageInspectionError("FIT has multiple images and no selected configuration")

    components: list[dict[str, object]] = []
    architectures: set[str] = set()
    role_for_type = {
        "kernel": "kernel", "ramdisk": "initramfs", "flat_dt": "device-tree",
        "filesystem": "rootfs", "firmware": "firmware",
    }
    for path in image_nodes:
        props = properties[path]
        name = path.rsplit("/", 1)[-1]
        image_type = _fit_string(props.get("type", (b"firmware\0", 0))[0], f"{name}/type")
        role = role_for_type.get(image_type, "firmware")
        arch = None
        if "arch" in props:
            raw_arch = _fit_string(props["arch"][0], f"{name}/arch")
            # FIT's `mips` value does not encode byte order.  Leave it unset
            # unless an exact captured artifact supplies mmips/mipsbe.
            arch = {"arm64": "aarch64", "powerpc": "powerpc"}.get(raw_arch, raw_arch)
            if arch in _ARCHITECTURES:
                architectures.add(arch)
            else:
                arch = None
        if "data" in props:
            contents, offset = props["data"]
        elif "data-position" in props:
            offset = _be_integer(props["data-position"][0], f"{name}/data-position")
            size = _be_integer(props.get("data-size", (b"", 0))[0], f"{name}/data-size")
            if offset + size > len(data):
                raise ImageInspectionError(f"FIT external data is outside file: {name}")
            contents = data[offset:offset + size]
        elif "data-offset" in props:
            relative = _be_integer(props["data-offset"][0], f"{name}/data-offset")
            size = _be_integer(props.get("data-size", (b"", 0))[0], f"{name}/data-size")
            offset = ((total_size + 3) & ~3) + relative
            if offset + size > len(data):
                raise ImageInspectionError(f"FIT external data is outside file: {name}")
            contents = data[offset:offset + size]
        else:
            raise ImageInspectionError(f"FIT image has no data: {name}")

        # Hash child nodes bind the stored image bytes.  Validate every hash
        # algorithm which ViroS can check without an external program.
        for hash_path, hash_props in properties.items():
            if not hash_path.startswith(path + "/") or hash_path.count("/") != 3:
                continue
            if "algo" not in hash_props or "value" not in hash_props:
                continue
            algo = _fit_string(hash_props["algo"][0], f"{name}/hash/algo")
            expected = hash_props["value"][0]
            if algo == "sha256":
                actual = hashlib.sha256(contents).digest()
            elif algo == "sha1":
                actual = hashlib.sha1(contents).digest()
            elif algo == "crc32":
                actual = struct.pack(">I", zlib.crc32(contents) & 0xFFFFFFFF)
            else:
                continue
            if actual != expected:
                raise ImageInspectionError(f"FIT {algo} does not match for image {name}")
        metadata: dict[str, object] = {"image": name, "type": image_type}
        for key in ("compression", "os"):
            if key in props:
                metadata[key] = _fit_string(props[key][0], f"{name}/{key}")
        for key in ("load", "entry"):
            if key in props:
                metadata[key] = _be_integer(props[key][0], f"{name}/{key}")
        components.append(_component(
            role, "application/vnd.u-boot.fit-component", contents,
            {"kind": "selected-range", "offset": offset, "length": len(contents)},
            catalog, architecture=arch, metadata=metadata,
        ))
    architecture = next(iter(architectures)) if len(architectures) == 1 else None
    result: dict[str, object] = {
        "kind": "fit", "components": components,
        "configuration": selected_config,
    }
    if architecture is not None:
        result["architecture"] = architecture
    return result


def _elf_architecture(data: bytes) -> str | None:
    if len(data) < 20 or data[:4] != b"\x7fELF" or data[4] not in {1, 2}:
        return None
    if data[5] == 1:
        byteorder = "little"
    elif data[5] == 2:
        byteorder = "big"
    else:
        return None
    machine = int.from_bytes(data[18:20], byteorder)
    if machine == 8:
        return "mmips" if byteorder == "little" else "mipsbe"
    return {3: "x86", 20: "powerpc", 21: "powerpc", 40: "arm", 62: "x86_64", 183: "aarch64", 191: "tilegx"}.get(machine)


def _gzip_decompress(data: bytes) -> bytes:
    try:
        stream = gzip.GzipFile(fileobj=io.BytesIO(data))
        decoded = stream.read(MAX_DECODED_BYTES + 1)
        if len(decoded) > MAX_DECODED_BYTES:
            raise ImageInspectionError("gzip image expands beyond the ViroS limit")
        if stream.read(1):
            raise ImageInspectionError("gzip decoder left unexpected data")
        return decoded
    except (OSError, EOFError, zlib.error) as exc:
        raise ImageInspectionError(f"invalid gzip image: {exc}") from exc


def _raw_kind(data: bytes, catalog: _Catalog) -> tuple[str, str, str | None]:
    architecture = _elf_architecture(data)
    if architecture is not None:
        return "elf-kernel", "kernel", architecture
    if len(data) >= 0x23A and data[0x202:0x206] == b"HdrS":
        xloadflags = int.from_bytes(data[0x236:0x238], "little")
        return "linux-bzimage", "kernel", "x86_64" if xloadflags & 1 else "x86"
    if len(data) >= 0x40 and data[0x38:0x3C] == b"ARM\x64":
        return "linux-arm64-image", "kernel", "aarch64"
    if data.startswith((b"070701", b"070702")):
        return "cpio-newc", "initramfs", None
    if data[:4] in {b"hsqs", b"sqsh", b"qshs", b"shsq"}:
        return "squashfs", "rootfs", None
    if len(data) >= 0x43A and data[0x438:0x43A] == b"\x53\xef":
        return "ext-filesystem", "disk", None
    if len(data) >= 520 and data[512:520] == b"EFI PART":
        return "gpt-disk", "disk", None
    if len(data) >= 512 and data[510:512] == b"\x55\xaa":
        return "mbr-disk", "disk", None
    roles, architectures = catalog.typed_matches(data)
    boot_roles = roles & {"disk", "firmware", "initramfs", "kernel", "kernel-boot", "rootfs", "vmlinux"}
    role = next(iter(boot_roles)) if len(boot_roles) == 1 else "firmware"
    architecture = next(iter(architectures)) if len(architectures) == 1 else None
    return "captured-opaque" if boot_roles else "opaque", role, architecture


def _inspect_raw(data: bytes, catalog: _Catalog, *, depth: int = 0) -> dict[str, object]:
    if data.startswith(b"\x1f\x8b"):
        if depth >= 2:
            raise ImageInspectionError("too many nested gzip image layers")
        decoded = _gzip_decompress(data)
        nested = _inspect_raw(decoded, catalog, depth=depth + 1)
        components = nested["components"]
        assert isinstance(components, list)
        for component in components:
            assert isinstance(component, dict)
            component["locator"] = {
                "kind": "decoded-selected",
                "codec": "gzip",
                "decoded_identity": _identity(decoded),
                "inner": component["locator"],
            }
        result = dict(nested)
        result["kind"] = f"gzip-{nested['kind']}"
        return result
    kind, role, architecture = _raw_kind(data, catalog)
    component = _component(
        role, {
            "elf-kernel": "application/x-elf",
            "cpio-newc": "application/x-cpio",
            "squashfs": "application/vnd.squashfs",
            "ext-filesystem": "application/vnd.linux.ext",
        }.get(kind, "application/octet-stream"),
        data,
        {"kind": "selected-range", "offset": 0, "length": len(data)},
        catalog,
        architecture=architecture,
    )
    result: dict[str, object] = {"kind": kind, "components": [component]}
    if architecture is not None:
        result["architecture"] = architecture
    return result


def _profile(architecture: str | None) -> str | None:
    return {
        "aarch64": "virt-initramfs-aarch64-v1",
        "arm": "virt-initramfs-arm-v1",
        "mmips": "malta-initramfs-mipsel-v1",
        "x86_64": "microvm-initramfs-x86_64-v1",
    }.get(architecture)


def inspect_selected_image(
    data: bytes,
    selected: CapturedArtifact,
    captured_artifacts: Sequence[CapturedArtifact] = (),
) -> dict[str, object]:
    """Return a canonical derivation for one explicitly selected captured file."""

    if not isinstance(data, bytes):
        raise ImageInspectionError("selected image contents must be bytes")
    if len(data) > MAX_IMAGE_BYTES:
        raise ImageInspectionError("selected image exceeds the ViroS inspection limit")
    actual = _identity(data)
    if selected.size != actual["size"] or selected.sha256 != actual["sha256"]:
        raise ImageInspectionError("selected captured identity does not match its bytes")
    supplied = list(captured_artifacts)
    selected_key = (selected.box_id, selected.path, selected.record_id)
    matching_selected = [
        row for row in supplied
        if (row.box_id, row.path, row.record_id) == selected_key
    ]
    if matching_selected and any(row != selected for row in matching_selected):
        raise ImageInspectionError("selected identity conflicts with its captured catalog row")
    if not matching_selected:
        supplied.append(selected)
    catalog = _Catalog(supplied)

    layout = _inspect_sysupgrade_tar(data, catalog)
    if layout is None:
        layout = _inspect_uimage(data, catalog)
    if layout is None:
        layout = _inspect_fit(data, catalog)
    if layout is None:
        layout = _inspect_raw(data, catalog)

    components = layout["components"]
    assert isinstance(components, list)
    architectures = {
        str(component["architecture"])
        for component in components
        if isinstance(component, Mapping) and component.get("architecture") is not None
    }
    if not architectures and layout.get("architecture") is not None:
        architectures.add(str(layout["architecture"]))
    architecture = next(iter(architectures)) if len(architectures) == 1 else None
    profile = _profile(architecture)
    roles = {str(component["role"]) for component in components if isinstance(component, Mapping)}
    boot_input = bool(roles & {"disk", "firmware"}) or (
        "kernel" in roles and bool(roles & {"initramfs", "rootfs"})
    )
    materialized = [component["artifact"] for component in components]
    supporting = {
        role: catalog.supporting(role, architecture)
        for role in ("vmlinux", "kernel-boot")
        if catalog.supporting(role, architecture)
    }
    result: dict[str, object] = {
        "format": FORMAT,
        "selected": selected.descriptor(),
        "layout": layout["kind"],
        "components": components,
        "bundle_inputs": {
            "materialized_artifacts": materialized,
            "supporting_artifacts": supporting,
        },
        "compatibility": {
            "artifact_identity": "size-sha256",
            "kernel_bundle_format": "viros-kernel-bundle-v1",
            "image_bundle_format": "viros-image-bundle-v1",
            "boot_input_complete": boot_input and profile is not None,
        },
    }
    if architecture is not None:
        result["architecture"] = architecture
    if profile is not None:
        result["profile"] = profile
    for key in ("configuration",):
        if key in layout:
            result[key] = layout[key]
    return result


def inspect_selected_image_mapping(
    data: bytes,
    selected: Mapping[str, object],
    captured_artifacts: Sequence[Mapping[str, object]] = (),
) -> dict[str, object]:
    """Strict mapping adapter for typed wire/database callers."""

    return inspect_selected_image(
        data,
        CapturedArtifact.from_mapping(selected),
        tuple(CapturedArtifact.from_mapping(row) for row in captured_artifacts),
    )


def _materialize_locator(selected_data: bytes, locator: Mapping[str, object]) -> bytes:
    kind = locator.get("kind")
    if kind == "selected-range":
        offset = locator.get("offset")
        length = locator.get("length")
        if (
            isinstance(offset, bool) or not isinstance(offset, int) or offset < 0
            or isinstance(length, bool) or not isinstance(length, int) or length < 0
            or offset + length > len(selected_data)
        ):
            raise ImageInspectionError("derived selected range is outside the image")
        return selected_data[offset:offset + length]
    if kind == "container-member" and locator.get("container") == "tar":
        member_name = locator.get("member")
        if not isinstance(member_name, str):
            raise ImageInspectionError("derived tar member has no typed name")
        # An uncompressed tar carries an exact direct range, avoiding a second
        # parser pass.  Compressed tar uses the already constrained member
        # contract and remains independent of any host tar executable.
        if "offset" in locator or "length" in locator:
            return _materialize_locator(selected_data, {
                "kind": "selected-range",
                "offset": locator.get("offset"),
                "length": locator.get("length"),
            })
        try:
            with tarfile.open(fileobj=io.BytesIO(selected_data), mode="r:*") as archive:
                rows = [row for row in archive.getmembers() if row.name.removeprefix("./") == member_name]
                if len(rows) != 1 or not rows[0].isfile():
                    raise ImageInspectionError(
                        f"derived tar member is not one regular file: {member_name}"
                    )
                stream = archive.extractfile(rows[0])
                if stream is None:
                    raise ImageInspectionError(f"cannot materialize tar member: {member_name}")
                contents = stream.read(MAX_DECODED_BYTES + 1)
                if len(contents) != rows[0].size or len(contents) > MAX_DECODED_BYTES:
                    raise ImageInspectionError(f"invalid derived tar member size: {member_name}")
                return contents
        except (tarfile.TarError, OSError) as exc:
            raise ImageInspectionError(f"cannot materialize derived tar member: {exc}") from exc
    if kind == "decoded-selected" and locator.get("codec") == "gzip":
        inner = locator.get("inner")
        if not isinstance(inner, Mapping):
            raise ImageInspectionError("derived gzip locator has no inner locator")
        return _materialize_locator(_gzip_decompress(selected_data), inner)
    raise ImageInspectionError(f"unknown derived component locator: {kind!r}")


def materialize_component(
    selected_data: bytes, component: Mapping[str, object],
) -> bytes:
    """Materialize and revalidate one component returned by the inspector."""

    locator = component.get("locator")
    artifact_row = component.get("artifact")
    if not isinstance(locator, Mapping) or not isinstance(artifact_row, Mapping):
        raise ImageInspectionError("component has no typed locator/artifact descriptor")
    contents = _materialize_locator(selected_data, locator)
    size = artifact_row.get("size")
    digest = artifact_row.get("sha256")
    if size != len(contents) or digest != _sha256(contents):
        raise ImageInspectionError("materialized component identity does not match derivation")
    return contents
