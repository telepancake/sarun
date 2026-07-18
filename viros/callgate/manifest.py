"""Strict validation for kernel-bound AArch64 call-gate manifests."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
from typing import Any


FORMAT = "viros-callgate-v1"
ARCHITECTURE = "aarch64"
PAGE_SIZE = 4096
MAX_REGION_SIZE = 64 * 1024
UINT32_LIMIT = 1 << 32
UINT64_LIMIT = 1 << 64
_SHA256 = re.compile(r"^[0-9a-f]{64}$")
_VALIDATION_SEAL = object()


class ManifestError(ValueError):
    """The manifest is unsafe, incomplete, or does not match its files."""


def _integer(value: Any, field: str) -> int:
    if isinstance(value, bool):
        raise ManifestError(f"{field} must be an integer")
    if isinstance(value, int):
        result = value
    elif isinstance(value, str):
        try:
            result = int(value, 0)
        except ValueError as exc:
            raise ManifestError(f"{field} is not an integer: {value!r}") from exc
    else:
        raise ManifestError(f"{field} must be an integer or 0x-prefixed string")
    if result < 0:
        raise ManifestError(f"{field} must not be negative")
    return result


def _mapping(value: Any, field: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ManifestError(f"{field} must be an object")
    return value


def _string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise ManifestError(f"{field} must be a non-empty string")
    return value


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _checked_file(base: Path, value: Any, expected: Any, field: str) -> Path:
    text = _string(value, field)
    path = Path(text)
    if not path.is_absolute():
        path = base / path
    path = path.resolve()
    if not path.is_file():
        raise ManifestError(f"{field} does not name a regular file: {path}")
    wanted = _string(expected, field + "_sha256").lower()
    if not _SHA256.fullmatch(wanted):
        raise ManifestError(f"{field}_sha256 must contain 64 lowercase hex digits")
    actual = _sha256_file(path)
    if actual != wanted:
        raise ManifestError(
            f"{field} SHA-256 mismatch: expected {wanted}, got {actual}"
        )
    return path


@dataclass(frozen=True)
class Region:
    name: str
    role: str
    virtual_address: int
    physical_address: int
    size: int


@dataclass(frozen=True)
class ValidatedManifest:
    """A manifest that has passed file, layout, and invocation validation."""

    source: Path
    kernel_file: Path
    kernel_sha256: str
    kernel_build_id: str
    probe_file: Path
    probe_sha256: str
    probe_bytes: bytes
    probe_capabilities: tuple[str, ...]
    entry_address: int
    completion_address: int
    regions: tuple[Region, ...]
    code_region: str
    data_region: str
    stack_region: str
    cpu: int
    pstate: int
    stack_pointer: int
    request_address: int
    result_address: int
    request_offset: int
    request_bytes: bytes
    result_offset: int
    result_size: int
    completion_magic: bytes
    timeout_seconds: float
    _seal: object

    @property
    def is_validated(self) -> bool:
        return self._seal is _VALIDATION_SEAL

    def region(self, name: str) -> Region:
        for region in self.regions:
            if region.name == name:
                return region
        raise KeyError(name)


def _decode_hex(value: Any, field: str) -> bytes:
    if value in (None, ""):
        return b""
    text = _string(value, field)
    try:
        return bytes.fromhex(text)
    except ValueError as exc:
        raise ManifestError(f"{field} must be even-length hexadecimal") from exc


def _validate_regions(raw: Any) -> tuple[Region, ...]:
    if not isinstance(raw, list) or len(raw) < 3:
        raise ManifestError("regions must contain at least code, data, and stack")
    regions: list[Region] = []
    names: set[str] = set()
    roles: set[str] = set()
    for index, item in enumerate(raw):
        obj = _mapping(item, f"regions[{index}]")
        name = _string(obj.get("name"), f"regions[{index}].name")
        role = _string(obj.get("role"), f"regions[{index}].role")
        if role not in {"code", "data", "stack"}:
            raise ManifestError(f"regions[{index}].role is not code, data, or stack")
        if role in roles:
            raise ManifestError(f"duplicate region role: {role}")
        if name in names:
            raise ManifestError(f"duplicate region name: {name}")
        roles.add(role)
        names.add(name)
        virtual = _integer(obj.get("virtual_address"), f"regions[{index}].virtual_address")
        physical = _integer(obj.get("physical_address"), f"regions[{index}].physical_address")
        size = _integer(obj.get("size"), f"regions[{index}].size")
        if not size or size > MAX_REGION_SIZE or size % PAGE_SIZE:
            raise ManifestError(
                f"regions[{index}].size must be a nonzero page multiple no larger than {MAX_REGION_SIZE}"
            )
        if virtual % PAGE_SIZE or physical % PAGE_SIZE:
            raise ManifestError(f"regions[{index}] addresses must be page-aligned")
        if virtual >= UINT64_LIMIT or virtual + size > UINT64_LIMIT:
            raise ManifestError(
                f"regions[{index}] virtual address range does not fit in 64 bits"
            )
        if physical >= UINT64_LIMIT or physical + size > UINT64_LIMIT:
            raise ManifestError(
                f"regions[{index}] physical address range does not fit in 64 bits"
            )
        regions.append(Region(name, role, virtual, physical, size))

    if roles != {"code", "data", "stack"}:
        raise ManifestError("regions must contain exactly one code, data, and stack role")

    for address_kind in ("virtual_address", "physical_address"):
        ordered = sorted(regions, key=lambda region: getattr(region, address_kind))
        for left, right in zip(ordered, ordered[1:]):
            if getattr(left, address_kind) + left.size > getattr(right, address_kind):
                raise ManifestError(
                    f"regions {left.name} and {right.name} overlap in {address_kind} space"
                )
    return tuple(regions)


def load_and_validate_manifest(path: str | Path) -> ValidatedManifest:
    """Load a manifest and bind it to exact local kernel and probe files."""

    source = Path(path).resolve()
    try:
        raw = json.loads(source.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ManifestError(f"cannot read manifest {source}: {exc}") from exc
    root = _mapping(raw, "manifest")
    if root.get("format") != FORMAT:
        raise ManifestError(f"format must be {FORMAT!r}")
    if root.get("architecture") != ARCHITECTURE:
        raise ManifestError("only the aarch64 call gate is supported")
    if root.get("allow_transient_guest_modification") is not True:
        raise ManifestError("allow_transient_guest_modification must be true")

    kernel = _mapping(root.get("kernel"), "kernel")
    kernel_file = _checked_file(
        source.parent, kernel.get("vmlinux"), kernel.get("sha256"), "kernel.vmlinux"
    )
    kernel_sha256 = _string(kernel.get("sha256"), "kernel.sha256").lower()
    build_id = _string(kernel.get("build_id"), "kernel.build_id").lower()
    if not re.fullmatch(r"[0-9a-f]{8,128}", build_id):
        raise ManifestError("kernel.build_id must be 8-128 lowercase hex digits")

    regions = _validate_regions(root.get("regions"))
    by_name = {region.name: region for region in regions}
    probe = _mapping(root.get("probe"), "probe")
    probe_file = _checked_file(
        source.parent, probe.get("binary"), probe.get("sha256"), "probe.binary"
    )
    probe_sha256 = _string(probe.get("sha256"), "probe.sha256").lower()
    probe_bytes = probe_file.read_bytes()
    raw_capabilities = probe.get("capabilities", [])
    if not isinstance(raw_capabilities, list) or any(
        not isinstance(item, str) or not item for item in raw_capabilities
    ):
        raise ManifestError("probe.capabilities must be a list of non-empty strings")
    probe_capabilities = tuple(raw_capabilities)
    if len(set(probe_capabilities)) != len(probe_capabilities):
        raise ManifestError("probe.capabilities must not contain duplicates")
    known_capabilities = {
        "snapshot-v1",
        "translate-va-aarch64-v1",
        "saved-regs-aarch64-v1",
    }
    unknown_capabilities = set(probe_capabilities) - known_capabilities
    if unknown_capabilities:
        raise ManifestError(
            "probe.capabilities contains unsupported values: "
            + ", ".join(sorted(unknown_capabilities))
        )
    dependent_capabilities = {
        "translate-va-aarch64-v1",
        "saved-regs-aarch64-v1",
    }
    if dependent_capabilities.intersection(probe_capabilities) and (
        "snapshot-v1" not in probe_capabilities
    ):
        raise ManifestError("probe task operations require snapshot-v1")
    code_region_name = _string(probe.get("code_region"), "probe.code_region")
    if code_region_name not in by_name or by_name[code_region_name].role != "code":
        raise ManifestError("probe.code_region must name a code region")
    code_region = by_name[code_region_name]
    if len(probe_bytes) == 0 or len(probe_bytes) > code_region.size:
        raise ManifestError("probe binary must fit in its nonempty code region")
    entry_offset = _integer(probe.get("entry_offset"), "probe.entry_offset")
    completion_offset = _integer(
        probe.get("completion_offset"), "probe.completion_offset"
    )
    if entry_offset >= len(probe_bytes) or entry_offset % 4:
        raise ManifestError("probe.entry_offset must select an aligned instruction")
    if completion_offset >= len(probe_bytes) or completion_offset % 4:
        raise ManifestError("probe.completion_offset must select an aligned instruction")
    if entry_offset == completion_offset:
        raise ManifestError("probe entry and completion instructions must be distinct")

    mailbox = _mapping(root.get("mailbox"), "mailbox")
    data_name = _string(mailbox.get("data_region"), "mailbox.data_region")
    if data_name not in by_name or by_name[data_name].role != "data":
        raise ManifestError("mailbox.data_region must name a data region")
    data_region = by_name[data_name]
    request_offset = _integer(mailbox.get("request_offset", 0), "mailbox.request_offset")
    request_bytes = _decode_hex(mailbox.get("request_hex", ""), "mailbox.request_hex")
    result_offset = _integer(mailbox.get("result_offset"), "mailbox.result_offset")
    result_size = _integer(mailbox.get("result_size"), "mailbox.result_size")
    completion_magic = _decode_hex(
        mailbox.get("completion_magic_hex"), "mailbox.completion_magic_hex"
    )
    if request_offset + len(request_bytes) > data_region.size:
        raise ManifestError("mailbox request does not fit in the data region")
    if request_offset >= data_region.size:
        raise ManifestError("mailbox.request_offset must select the data region")
    if not result_size or result_offset + result_size > data_region.size:
        raise ManifestError("mailbox result must fit in the data region")
    request_end = request_offset + len(request_bytes)
    result_end = result_offset + result_size
    if request_bytes and request_offset < result_end and result_offset < request_end:
        raise ManifestError("mailbox request and result ranges must not overlap")
    if len(completion_magic) > result_size:
        raise ManifestError("completion magic is larger than the result")

    invocation = _mapping(root.get("invocation"), "invocation")
    cpu = _integer(invocation.get("cpu", 0), "invocation.cpu")
    pstate = _integer(invocation.get("pstate"), "invocation.pstate")
    if cpu >= UINT32_LIMIT:
        raise ManifestError("invocation.cpu must fit in 32 bits")
    if pstate >= UINT32_LIMIT:
        raise ManifestError("invocation.pstate must fit in 32 bits")
    if pstate & 0xF != 0x5 or pstate & 0x3C0 != 0x3C0:
        raise ManifestError("invocation.pstate must select EL1h with DAIF masked")
    stack_name = _string(invocation.get("stack_region"), "invocation.stack_region")
    if stack_name not in by_name or by_name[stack_name].role != "stack":
        raise ManifestError("invocation.stack_region must name a stack region")
    stack_region = by_name[stack_name]
    stack_pointer = _integer(invocation.get("stack_pointer"), "invocation.stack_pointer")
    if stack_pointer >= UINT64_LIMIT:
        raise ManifestError("invocation.stack_pointer must fit in 64 bits")
    if not (
        stack_region.virtual_address < stack_pointer <= stack_region.virtual_address + stack_region.size
    ) or stack_pointer % 16:
        raise ManifestError("invocation.stack_pointer must be aligned inside the stack region")
    request_address = data_region.virtual_address + request_offset
    result_address = data_region.virtual_address + result_offset
    # Early manifests carried a redundant single argument pointer.  Accept it
    # only when it states the exact request address derived above; otherwise it
    # would silently invoke the three-argument probe ABI incorrectly.
    if "argument_address" in invocation:
        argument_address = _integer(
            invocation.get("argument_address"), "invocation.argument_address"
        )
        if argument_address != request_address:
            raise ManifestError(
                "invocation.argument_address must equal data_region virtual address "
                "+ mailbox.request_offset"
            )
    timeout = invocation.get("timeout_seconds", 1.0)
    if not isinstance(timeout, (int, float)) or isinstance(timeout, bool) or not 0 < timeout <= 30:
        raise ManifestError("invocation.timeout_seconds must be in (0, 30]")

    return ValidatedManifest(
        source=source,
        kernel_file=kernel_file,
        kernel_sha256=kernel_sha256,
        kernel_build_id=build_id,
        probe_file=probe_file,
        probe_sha256=probe_sha256,
        probe_bytes=probe_bytes,
        probe_capabilities=probe_capabilities,
        entry_address=code_region.virtual_address + entry_offset,
        completion_address=code_region.virtual_address + completion_offset,
        regions=regions,
        code_region=code_region_name,
        data_region=data_name,
        stack_region=stack_name,
        cpu=cpu,
        pstate=pstate,
        stack_pointer=stack_pointer,
        request_address=request_address,
        result_address=result_address,
        request_offset=request_offset,
        request_bytes=request_bytes,
        result_offset=result_offset,
        result_size=result_size,
        completion_magic=completion_magic,
        timeout_seconds=float(timeout),
        _seal=_VALIDATION_SEAL,
    )
