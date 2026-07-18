"""Strict, endian-aware decoder for the viros frozen-probe ABI."""

from __future__ import annotations

from dataclasses import dataclass
import struct
from typing import Callable


REQUEST_MAGIC = 0x56505251
RESPONSE_MAGIC = 0x56505253
ABI_MAJOR = 1
ABI_MINOR = 2
RESPONSE_SIZE = 64
TASK_V1_SIZE = 192
TRANSLATION_V1_SIZE = 64
SAVED_REGS_V1_SIZE = 304
ENDIAN_LITTLE = 1
ENDIAN_BIG = 2
ARCH_AARCH64 = 1
ARCH_MIPS = 3
OP_SNAPSHOT = 1
OP_TRANSLATE_VA = 2
OP_SAVED_REGS = 3
STATUS_STALE_TASK = -5
STATUS_NOT_PRESENT = -6
STATUS_TASK_RUNNING = -7
STATUS_INVALID_REGS = -8
STATUS_COMPAT_TASK = -9
RESP_MORE = 1 << 0
TASK_HAS_MM = 1 << 0
TASK_GROUP_LEADER = 1 << 1
TASK_ON_CPU = 1 << 2
TASK_AUX_VALID = 1 << 3
XLATE_PRESENT = 1 << 0
XLATE_USER = 1 << 1
XLATE_WRITABLE = 1 << 2
XLATE_EXECUTABLE = 1 << 3
XLATE_BLOCK = 1 << 4
XLATE_SPECIAL = 1 << 5
XLATE_SAFE_READ = 1 << 6
REGS_VALID = 1 << 0
REGS_USER = 1 << 1
REGS_AARCH64_64 = 1 << 2
AUX_COUNT = 10

AUX_TAGS = (3, 4, 5, 6, 7, 9, 25, 33, 31, 23)


class ProbeDecodeError(ValueError):
    """Probe bytes do not satisfy the versioned snapshot ABI."""


class ProbeStatusError(ProbeDecodeError):
    def __init__(self, status: int):
        self.status = status
        super().__init__(f"probe returned status {status}")


class ProbeStaleTaskError(ProbeStatusError):
    """The task pointer, start cookie, or mm no longer matches the snapshot."""


class ProbeNotPresentError(ProbeStatusError):
    """The requested userspace virtual address is not presently mapped."""


class ProbeTaskRunningError(ProbeStatusError):
    """The task is current/on-CPU, so its saved exception frame is stale."""


class ProbeInvalidRegistersError(ProbeStatusError):
    """The task does not contain a well-formed saved EL0 register frame."""


class ProbeCompatTaskError(ProbeStatusError):
    """The saved-register ABI deliberately does not expose compat32 frames."""


@dataclass(frozen=True)
class SnapshotAbi:
    """Target metadata which fixes snapshot byte order and pointer width.

    The frozen ABI deliberately keeps pointer-bearing request and response
    fields at 64 bits on the wire.  A 32-bit target must therefore place a
    zero-extended pointer in each of those fields.
    """

    name: str
    arch: int
    byte_order: str
    pointer_bits: int

    def __post_init__(self) -> None:
        if not isinstance(self.name, str) or not self.name:
            raise ValueError("snapshot ABI name must be a nonempty string")
        if self.byte_order not in {"<", ">"}:
            raise ValueError("snapshot ABI byte order must be '<' or '>'")
        if self.pointer_bits not in {32, 64}:
            raise ValueError("snapshot ABI pointer width must be 32 or 64")
        if (
            not isinstance(self.arch, int)
            or isinstance(self.arch, bool)
            or self.arch <= 0
        ):
            raise ValueError("snapshot ABI architecture must be a positive integer")

    @property
    def endian_code(self) -> int:
        return ENDIAN_LITTLE if self.byte_order == "<" else ENDIAN_BIG

    @property
    def pointer_limit(self) -> int:
        return 1 << self.pointer_bits


AARCH64_SNAPSHOT_ABI = SnapshotAbi("aarch64", ARCH_AARCH64, "<", 64)
MIPS32EL_SNAPSHOT_ABI = SnapshotAbi("mips32el", ARCH_MIPS, "<", 32)
MIPS32BE_SNAPSHOT_ABI = SnapshotAbi("mips32be", ARCH_MIPS, ">", 32)


@dataclass(frozen=True)
class ProbeTask:
    task: int
    group_leader: int
    real_parent: int
    mm: int
    pgd_kernel_va: int
    start_cookie: int
    state: int
    task_flags: int
    pid: int
    tgid: int
    ppid: int
    cpu: int
    exit_state: int
    abi_bits: int
    auxv_valid: int
    comm: str
    auxv_values: tuple[int, ...]
    probe_flags: int

    @property
    def stable_cookie(self) -> int:
        """Opaque identity which survives snapshots but detects task reuse."""

        return (self.task << 64) | self.start_cookie


@dataclass(frozen=True)
class ProbePage:
    abi_minor: int
    arch: int
    byte_order: str
    pointer_bits: int
    page_shift: int
    flags: int
    next_cursor: int
    snapshot_root: int
    tasks: tuple[ProbeTask, ...]

    @property
    def more(self) -> bool:
        return bool(self.flags & RESP_MORE)


@dataclass(frozen=True)
class ProbeSnapshot:
    abi_minor: int
    arch: int
    byte_order: str
    pointer_bits: int
    page_shift: int
    snapshot_root: int
    tasks: tuple[ProbeTask, ...]


@dataclass(frozen=True)
class ProbeTranslation:
    task: int
    mm: int
    virtual_address: int
    physical_address: int
    contiguous_bytes: int
    mapping_bytes: int
    page_shift: int
    level: int
    flags: int


@dataclass(frozen=True)
class ProbeSavedRegisters:
    task: int
    mm: int
    start_cookie: int
    x: tuple[int, ...]
    sp: int
    pc: int
    pstate: int
    flags: int

    @property
    def valid(self) -> bool:
        return bool(self.flags & REGS_VALID)


_REQUEST_FORMAT = "IHHHHIQQIIQQQ"


def _pointer(value: int, field: str, abi: SnapshotAbi) -> int:
    if (isinstance(value, bool) or not isinstance(value, int)
            or not 0 <= value < abi.pointer_limit):
        raise ProbeDecodeError(
            f"{field} does not fit the {abi.pointer_bits}-bit {abi.name} pointer width"
        )
    return value


def build_snapshot_request(
    init_task: int,
    cursor_task: int,
    max_records: int,
    *,
    abi_minor: int = ABI_MINOR,
    snapshot_abi: SnapshotAbi = AARCH64_SNAPSHOT_ABI,
) -> bytes:
    """Pack one snapshot request for an explicitly selected target ABI."""

    if not isinstance(snapshot_abi, SnapshotAbi):
        raise ProbeDecodeError("snapshot request requires target ABI metadata")
    init_task = _pointer(init_task, "init_task", snapshot_abi)
    cursor_task = _pointer(cursor_task, "cursor_task", snapshot_abi)
    if not init_task:
        raise ProbeDecodeError("snapshot request has a zero init_task")
    if (isinstance(max_records, bool) or not isinstance(max_records, int)
            or not 0 < max_records < 1 << 32):
        raise ProbeDecodeError("snapshot max_records must be a nonzero uint32")
    if (isinstance(abi_minor, bool) or not isinstance(abi_minor, int)
            or not 0 <= abi_minor <= ABI_MINOR):
        raise ProbeDecodeError("snapshot request uses an unsupported ABI minor")
    return struct.pack(
        snapshot_abi.byte_order + _REQUEST_FORMAT,
        REQUEST_MAGIC, ABI_MAJOR, abi_minor, 64, OP_SNAPSHOT, 0,
        init_task, cursor_task, max_records, 0, 0, 0, 0,
    )


def _build_translation_request(
    task: ProbeTask, virtual_address: int, linear_map_offset: int,
    abi_minor: int = ABI_MINOR,
) -> bytes:
    """Build an internal ABI-v1.1/v1.2 request from already-bound values.

    The linear-map offset is security-sensitive and must be derived from
    QEMU's translation of this decoded task's ``mm->pgd``.  Keeping this
    helper private prevents the public memory API from accepting that value.
    """

    if not isinstance(task, ProbeTask):
        raise ProbeDecodeError("translation requires a decoded frozen-snapshot task")
    if not task.task or not task.mm:
        raise ProbeDecodeError("translation task has no stable user address space")
    userspace_limit = min(1 << task.abi_bits, 1 << 63)
    if (isinstance(virtual_address, bool) or not isinstance(virtual_address, int)
            or not 0 <= virtual_address < userspace_limit):
        raise ProbeDecodeError(
            f"virtual address does not fit the task's {task.abi_bits}-bit ABI")
    if (isinstance(linear_map_offset, bool) or not isinstance(linear_map_offset, int)
            or not 0 < linear_map_offset < 1 << 64 or linear_map_offset & 0xfff):
        raise ProbeDecodeError("linear-map offset must be a nonzero page-aligned uint64")
    if abi_minor not in (1, ABI_MINOR):
        raise ProbeDecodeError("translation requires probe ABI minor 1 or 2")
    return struct.pack(
        AARCH64_SNAPSHOT_ABI.byte_order + _REQUEST_FORMAT,
        REQUEST_MAGIC, ABI_MAJOR, abi_minor, 64,
        OP_TRANSLATE_VA, 0, task.task, task.mm, 0, 0,
        task.start_cookie, virtual_address, linear_map_offset,
    )


def _build_saved_registers_request(task: ProbeTask) -> bytes:
    """Build an ABI-v1.2 request bound to one frozen snapshot record."""

    if not isinstance(task, ProbeTask):
        raise ProbeDecodeError(
            "saved registers require a decoded frozen-snapshot task")
    if not task.task or not task.mm:
        raise ProbeDecodeError(
            "saved-register task has no stable user address space identity")
    if task.abi_bits != 64:
        raise ProbeDecodeError("compat32 saved registers are not supported")
    if task.probe_flags & TASK_ON_CPU:
        raise ProbeDecodeError("an on-CPU task has no authoritative saved frame")
    return struct.pack(
        AARCH64_SNAPSHOT_ABI.byte_order + _REQUEST_FORMAT,
        REQUEST_MAGIC, ABI_MAJOR, ABI_MINOR, 64,
        OP_SAVED_REGS, 0, task.task, task.mm, 0, 0,
        task.start_cookie, 0, 0,
    )


_HEADER_FORMAT = "IHHHHHBBiIIIQQIIQ"
_TASK_FORMAT = "HHIQQQQQQQQIIIIIHH16s10Q"
_TRANSLATION_FORMAT = "HHIQQQQQQIHH"
_SAVED_REGS_FORMAT = "HHIQQQ31Q3Q"


def _unpack(fmt: str, byte_order: str, data: bytes, offset: int = 0):
    try:
        return struct.unpack_from(byte_order + fmt, data, offset)
    except struct.error as exc:
        raise ProbeDecodeError("truncated probe response") from exc


def _status_error(status: int) -> ProbeStatusError:
    if status == STATUS_STALE_TASK:
        return ProbeStaleTaskError(status)
    if status == STATUS_NOT_PRESENT:
        return ProbeNotPresentError(status)
    if status == STATUS_TASK_RUNNING:
        return ProbeTaskRunningError(status)
    if status == STATUS_INVALID_REGS:
        return ProbeInvalidRegistersError(status)
    if status == STATUS_COMPAT_TASK:
        return ProbeCompatTaskError(status)
    return ProbeStatusError(status)


def decode_response(
    data: bytes, *, expected_abi: SnapshotAbi | None = None
) -> ProbePage:
    if not isinstance(data, bytes):
        raise ProbeDecodeError("probe response must be bytes")
    if len(data) < RESPONSE_SIZE:
        raise ProbeDecodeError("truncated probe response header")
    endian_code = data[14]
    if endian_code == ENDIAN_LITTLE:
        byte_order = "<"
    elif endian_code == ENDIAN_BIG:
        byte_order = ">"
    else:
        raise ProbeDecodeError(f"invalid probe byte order {endian_code}")
    fields = _unpack(_HEADER_FORMAT, byte_order, data)
    (magic, major, minor, header_size, record_size, arch, encoded_endian,
     pointer_bits, status, flags, record_count, bytes_written, next_cursor,
     snapshot_root, page_shift, reserved0, reserved1) = fields
    if magic != RESPONSE_MAGIC:
        raise ProbeDecodeError(f"bad probe response magic {magic:#x}")
    if major != ABI_MAJOR:
        raise ProbeDecodeError(f"unsupported probe ABI major {major}")
    if minor > ABI_MINOR:
        raise ProbeDecodeError(f"unsupported probe ABI minor {minor}")
    if encoded_endian != endian_code:
        raise ProbeDecodeError("inconsistent response byte order")
    if header_size < RESPONSE_SIZE or header_size > len(data):
        raise ProbeDecodeError(f"invalid response header size {header_size}")
    if record_size < TASK_V1_SIZE:
        raise ProbeDecodeError(f"task record size {record_size} is too small")
    if pointer_bits not in (32, 64):
        raise ProbeDecodeError(f"invalid pointer width {pointer_bits}")
    if expected_abi is None:
        # Preserve the original AArch64-only default.  Additional target
        # decoders must opt in with the exact expected metadata.
        if arch != ARCH_AARCH64:
            raise ProbeDecodeError(f"unsupported probe architecture {arch}")
    elif not isinstance(expected_abi, SnapshotAbi):
        raise ProbeDecodeError("response decoder requires target ABI metadata")
    elif (arch, byte_order, pointer_bits) != (
        expected_abi.arch, expected_abi.byte_order, expected_abi.pointer_bits
    ):
        raise ProbeDecodeError(
            "probe response target metadata does not match expected "
            f"{expected_abi.name} ABI"
        )
    if not 10 <= page_shift <= 24:
        raise ProbeDecodeError(f"invalid page shift {page_shift}")
    if flags & ~RESP_MORE:
        raise ProbeDecodeError(f"unknown response flags {flags:#x}")
    if reserved0 or reserved1:
        raise ProbeDecodeError("nonzero reserved response fields")
    if status:
        raise _status_error(status)
    expected = header_size + record_count * record_size
    if bytes_written != expected or bytes_written > len(data):
        raise ProbeDecodeError(
            f"inconsistent response length: header says {bytes_written}, expected {expected}, "
            f"buffer has {len(data)}")
    if bool(flags & RESP_MORE) != bool(next_cursor):
        raise ProbeDecodeError("pagination flag and next cursor disagree")
    if not snapshot_root:
        raise ProbeDecodeError("snapshot root is zero")
    if expected_abi is not None and expected_abi.pointer_bits == 32:
        _pointer(snapshot_root, "snapshot_root", expected_abi)
        _pointer(next_cursor, "next_cursor", expected_abi)

    tasks = []
    known_task_flags = TASK_HAS_MM | TASK_GROUP_LEADER | TASK_ON_CPU | TASK_AUX_VALID
    for index in range(record_count):
        offset = header_size + index * record_size
        values = _unpack(_TASK_FORMAT, byte_order, data, offset)
        (own_size, version, probe_flags, task, leader, parent, mm, pgd,
         start_cookie, state, task_flags, pid, tgid, ppid, cpu, exit_state,
         abi_bits, auxv_valid, comm_raw, *auxv_values) = values
        if own_size != TASK_V1_SIZE or version != 1:
            raise ProbeDecodeError(
                f"task record {index} has unsupported size/version {own_size}/{version}")
        if probe_flags & ~known_task_flags:
            raise ProbeDecodeError(f"task record {index} has unknown flags {probe_flags:#x}")
        if not task or not leader:
            raise ProbeDecodeError(f"task record {index} has a zero identity pointer")
        if expected_abi is not None and expected_abi.pointer_bits == 32:
            for field, value in (
                ("task", task), ("group_leader", leader),
                ("real_parent", parent), ("mm", mm), ("pgd", pgd),
            ):
                _pointer(value, f"task[{index}].{field}", expected_abi)
        if abi_bits not in (32, 64):
            raise ProbeDecodeError(f"task record {index} has invalid ABI width {abi_bits}")
        if expected_abi is not None and abi_bits > expected_abi.pointer_bits:
            raise ProbeDecodeError(
                f"task record {index} ABI width exceeds the expected target pointer width"
            )
        if auxv_valid & ~((1 << AUX_COUNT) - 1):
            raise ProbeDecodeError(f"task record {index} has invalid auxv mask")
        if bool(auxv_valid) != bool(probe_flags & TASK_AUX_VALID):
            raise ProbeDecodeError(f"task record {index} has inconsistent auxv flags")
        if bool(mm) != bool(probe_flags & TASK_HAS_MM):
            raise ProbeDecodeError(f"task record {index} has inconsistent mm flag")
        comm = comm_raw.split(b"\0", 1)[0].decode("utf-8", errors="replace")
        tasks.append(ProbeTask(
            task, leader, parent, mm, pgd, start_cookie, state, task_flags,
            pid, tgid, ppid, cpu, exit_state, abi_bits, auxv_valid, comm,
            tuple(auxv_values), probe_flags))
    return ProbePage(
        minor, arch, byte_order, pointer_bits, page_shift, flags, next_cursor,
        snapshot_root, tuple(tasks))


def decode_translation_response(data: bytes) -> ProbeTranslation:
    """Decode one AArch64 VA translation response with strict ABI checks."""

    if not isinstance(data, bytes):
        raise ProbeDecodeError("probe response must be bytes")
    if len(data) < RESPONSE_SIZE:
        raise ProbeDecodeError("truncated probe response header")
    endian_code = data[14]
    if endian_code == ENDIAN_LITTLE:
        byte_order = "<"
    elif endian_code == ENDIAN_BIG:
        byte_order = ">"
    else:
        raise ProbeDecodeError(f"invalid probe byte order {endian_code}")
    fields = _unpack(_HEADER_FORMAT, byte_order, data)
    (magic, major, minor, header_size, record_size, arch, encoded_endian,
     pointer_bits, status, response_flags, record_count, bytes_written,
     next_cursor, snapshot_root, header_page_shift, reserved0, reserved1) = fields
    if magic != RESPONSE_MAGIC or major != ABI_MAJOR or minor not in (1, ABI_MINOR):
        raise ProbeDecodeError("translation response has incompatible magic or ABI")
    if encoded_endian != endian_code or byte_order != "<":
        raise ProbeDecodeError("translation response is not little-endian AArch64")
    if (header_size != RESPONSE_SIZE or record_size != TRANSLATION_V1_SIZE
            or arch != ARCH_AARCH64 or pointer_bits != 64):
        raise ProbeDecodeError("translation response has incompatible target/layout metadata")
    if not 10 <= header_page_shift <= 24:
        raise ProbeDecodeError(f"invalid page shift {header_page_shift}")
    if response_flags or next_cursor or reserved0 or reserved1:
        raise ProbeDecodeError("translation response has nonzero reserved/pagination fields")
    if status:
        if record_count or bytes_written != RESPONSE_SIZE:
            raise ProbeDecodeError("failed translation response contains a record")
        raise _status_error(status)
    if (record_count != 1 or bytes_written != RESPONSE_SIZE + TRANSLATION_V1_SIZE
            or bytes_written > len(data) or not snapshot_root):
        raise ProbeDecodeError("translation response does not contain exactly one record")
    values = _unpack(_TRANSLATION_FORMAT, byte_order, data, RESPONSE_SIZE)
    (own_size, version, flags, task, mm, virtual_address, physical_address,
     contiguous_bytes, mapping_bytes, page_shift, level, record_reserved) = values
    known_flags = (XLATE_PRESENT | XLATE_USER | XLATE_WRITABLE |
                   XLATE_EXECUTABLE | XLATE_BLOCK | XLATE_SPECIAL |
                   XLATE_SAFE_READ)
    if own_size != TRANSLATION_V1_SIZE or version != 1 or flags & ~known_flags:
        raise ProbeDecodeError("translation record has incompatible size/version/flags")
    if not (flags & XLATE_PRESENT) or not task or not mm or task != snapshot_root:
        raise ProbeDecodeError("translation record has invalid identity/presence metadata")
    if flags & XLATE_SAFE_READ and (
            not flags & XLATE_USER or flags & XLATE_SPECIAL):
        raise ProbeDecodeError("translation record has an unsafe SAFE_READ claim")
    if (record_reserved or not 10 <= page_shift <= 63
            or mapping_bytes != 1 << page_shift or page_shift < header_page_shift
            or level not in (2, 3, 4)):
        raise ProbeDecodeError("translation record has invalid mapping geometry")
    offset = virtual_address & (mapping_bytes - 1)
    if contiguous_bytes != mapping_bytes - offset or not contiguous_bytes:
        raise ProbeDecodeError("translation record has inconsistent contiguous span")
    if physical_address >= 1 << 64 or physical_address + contiguous_bytes > 1 << 64:
        raise ProbeDecodeError("translation physical span overflows 64 bits")
    return ProbeTranslation(
        task, mm, virtual_address, physical_address, contiguous_bytes,
        mapping_bytes, page_shift, level, flags,
    )


def decode_saved_registers_response(data: bytes) -> ProbeSavedRegisters:
    """Decode one identity-bound, non-current AArch64 EL0 saved frame."""

    if not isinstance(data, bytes):
        raise ProbeDecodeError("probe response must be bytes")
    if len(data) < RESPONSE_SIZE:
        raise ProbeDecodeError("truncated probe response header")
    endian_code = data[14]
    if endian_code != ENDIAN_LITTLE:
        raise ProbeDecodeError("saved-register response is not little-endian")
    fields = _unpack(_HEADER_FORMAT, "<", data)
    (magic, major, minor, header_size, record_size, arch, encoded_endian,
     pointer_bits, status, response_flags, record_count, bytes_written,
     next_cursor, snapshot_root, page_shift, reserved0, reserved1) = fields
    if (magic != RESPONSE_MAGIC or major != ABI_MAJOR or minor != ABI_MINOR
            or encoded_endian != endian_code):
        raise ProbeDecodeError("saved-register response has incompatible magic or ABI")
    if (header_size != RESPONSE_SIZE or record_size != SAVED_REGS_V1_SIZE
            or arch != ARCH_AARCH64 or pointer_bits != 64):
        raise ProbeDecodeError(
            "saved-register response has incompatible target/layout metadata")
    if not 10 <= page_shift <= 24:
        raise ProbeDecodeError(f"invalid page shift {page_shift}")
    if response_flags or next_cursor or reserved0 or reserved1:
        raise ProbeDecodeError(
            "saved-register response has nonzero reserved/pagination fields")
    if status:
        if record_count or bytes_written != RESPONSE_SIZE:
            raise ProbeDecodeError("failed saved-register response contains a record")
        raise _status_error(status)
    if (record_count != 1
            or bytes_written != RESPONSE_SIZE + SAVED_REGS_V1_SIZE
            or bytes_written > len(data) or not snapshot_root):
        raise ProbeDecodeError(
            "saved-register response does not contain exactly one record")
    values = _unpack(_SAVED_REGS_FORMAT, "<", data, RESPONSE_SIZE)
    own_size, version, flags, task, mm, start_cookie, *registers = values
    expected_flags = REGS_VALID | REGS_USER | REGS_AARCH64_64
    if own_size != SAVED_REGS_V1_SIZE or version != 1 or flags != expected_flags:
        raise ProbeDecodeError(
            "saved-register record has incompatible size/version/validity flags")
    if not task or not mm or task != snapshot_root:
        raise ProbeDecodeError("saved-register record has invalid task identity")
    x = tuple(registers[:31])
    sp, pc, pstate = registers[31:]
    if (len(x) != 31 or sp >= 1 << 63 or pc >= 1 << 63
            or pstate >= 1 << 32 or pstate & 0xf):
        raise ProbeDecodeError("saved-register record is not a valid AArch64 EL0t frame")
    return ProbeSavedRegisters(
        task, mm, start_cookie, x, sp, pc, pstate, flags)


def decode_paginated(
    fetch: Callable[[int], bytes],
    max_pages: int = 4096,
    *,
    expected_abi: SnapshotAbi | None = None,
) -> ProbeSnapshot:
    """Fetch and validate all pages from one frozen probe snapshot."""

    cursor = 0
    seen_cursors = {0}
    seen_tasks: set[int] = set()
    pages: list[ProbePage] = []
    tasks: list[ProbeTask] = []
    for _ in range(max_pages):
        page = decode_response(fetch(cursor), expected_abi=expected_abi)
        if pages:
            first = pages[0]
            identity = (page.abi_minor, page.arch, page.byte_order,
                        page.pointer_bits, page.page_shift, page.snapshot_root)
            wanted = (first.abi_minor, first.arch, first.byte_order,
                      first.pointer_bits, first.page_shift, first.snapshot_root)
            if identity != wanted:
                raise ProbeDecodeError("probe page metadata changed during pagination")
            if not page.tasks or page.tasks[0].task != cursor:
                raise ProbeDecodeError("probe page did not begin at its requested cursor")
        elif page.tasks and page.tasks[0].task != page.snapshot_root:
            raise ProbeDecodeError("first probe page does not begin at snapshot root")
        for task in page.tasks:
            if task.task in seen_tasks:
                raise ProbeDecodeError(f"duplicate task pointer {task.task:#x} in snapshot")
            seen_tasks.add(task.task)
            tasks.append(task)
        pages.append(page)
        if not page.more:
            first = pages[0]
            return ProbeSnapshot(
                first.abi_minor, first.arch, first.byte_order,
                first.pointer_bits, first.page_shift, first.snapshot_root,
                tuple(tasks))
        cursor = page.next_cursor
        if cursor in seen_cursors:
            raise ProbeDecodeError(f"pagination cursor cycle at {cursor:#x}")
        seen_cursors.add(cursor)
    raise ProbeDecodeError(f"probe snapshot exceeded {max_pages} pages")
