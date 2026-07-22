"""Decode exact-kernel debugger stop records.

The matching debug kernel can publish a small, native-endian record when
Linux is about to deliver a selected signal to a userspace task.  QEMU stops
at the observer's no-op boundary and this module turns the bounded record into
architecture-neutral data for the multiprocess RSP facade.
"""

from __future__ import annotations

from dataclasses import dataclass
import struct


EVENT_MAGIC = 0x56455231  # "VER1"
EVENT_ABI_MAJOR = 1
EVENT_ABI_MINOR = 0
EVENT_HEADER_BYTES = 128
EVENT_MAX_REGISTERS = 64
EVENT_MAX_BYTES = EVENT_HEADER_BYTES + EVENT_MAX_REGISTERS * 8

ENDIAN_LITTLE = 1
ENDIAN_BIG = 2

ARCH_AARCH64 = 1
ARCH_ARM = 2
ARCH_MIPS = 3
ARCH_X86 = 4

EVENT_USER_SIGNAL = 1
EVENT_KERNEL_DIE = 2

EVENT_REGS_VALID = 1 << 0
EVENT_REGS_USER = 1 << 1
EVENT_REGS_COMPAT = 1 << 2
EVENT_ADDRESS_VALID = 1 << 3
EVENT_KNOWN_FLAGS = (
    EVENT_REGS_VALID | EVENT_REGS_USER | EVENT_REGS_COMPAT | EVENT_ADDRESS_VALID
)

_HEADER_FORMAT = "I6HIIi5I8Q16s"


class KernelEventDecodeError(ValueError):
    """A debugger stop record does not satisfy the frozen event ABI."""


@dataclass(frozen=True)
class KernelEvent:
    arch: int
    byte_order: str
    pointer_bits: int
    kind: int
    signal: int
    code: int
    flags: int
    cpu: int
    tgid: int
    tid: int
    sequence: int
    task: int
    mm: int
    start_cookie: int
    signal_struct: int
    comm: str
    address: int | None
    registers: tuple[int, ...]
    register_valid_mask: int

    @property
    def compat(self) -> bool:
        return bool(self.flags & EVENT_REGS_COMPAT)

    def register_available(self, index: int) -> bool:
        if not 0 <= index < len(self.registers):
            raise IndexError(index)
        return bool(self.register_valid_mask & (1 << index))


def _byte_order_prefix(byte_order: str) -> str:
    if byte_order == "little":
        return "<"
    if byte_order == "big":
        return ">"
    raise KernelEventDecodeError("event byte order must be little or big")


def decode_kernel_event(
    data: bytes,
    *,
    byte_order: str,
    expected_arch: int,
    expected_pointer_bits: int,
    expected_registers: int | None = None,
) -> KernelEvent:
    """Validate and decode one complete native-endian debugger event record."""

    if not isinstance(data, bytes):
        raise TypeError("event record must be bytes")
    if expected_arch not in {ARCH_AARCH64, ARCH_ARM, ARCH_MIPS, ARCH_X86}:
        raise KernelEventDecodeError("unsupported expected event architecture")
    if expected_pointer_bits not in {32, 64}:
        raise KernelEventDecodeError("expected pointer width must be 32 or 64")
    if (
        expected_registers is not None
        and (
            isinstance(expected_registers, bool)
            or not isinstance(expected_registers, int)
            or not 0 < expected_registers <= EVENT_MAX_REGISTERS
        )
    ):
        raise KernelEventDecodeError("invalid expected event register count")
    if not EVENT_HEADER_BYTES <= len(data) <= EVENT_MAX_BYTES:
        raise KernelEventDecodeError("event record has an invalid size")

    prefix = _byte_order_prefix(byte_order)
    header_size = struct.calcsize(prefix + _HEADER_FORMAT)
    if header_size != EVENT_HEADER_BYTES:
        raise AssertionError("event header format no longer matches its ABI size")
    (
        magic,
        major,
        minor,
        arch,
        endian,
        pointer_bits,
        kind,
        record_size,
        signal,
        code,
        flags,
        register_count,
        cpu,
        tgid,
        tid,
        sequence,
        task,
        mm,
        start_cookie,
        signal_struct,
        address,
        register_valid_mask,
        reserved0,
        comm_raw,
    ) = struct.unpack_from(prefix + _HEADER_FORMAT, data)

    expected_endian = ENDIAN_LITTLE if byte_order == "little" else ENDIAN_BIG
    if magic != EVENT_MAGIC or major != EVENT_ABI_MAJOR or minor != EVENT_ABI_MINOR:
        raise KernelEventDecodeError("event record has incompatible magic or ABI")
    if arch != expected_arch or endian != expected_endian:
        raise KernelEventDecodeError("event record has incompatible target metadata")
    if pointer_bits != expected_pointer_bits:
        raise KernelEventDecodeError("event record has incompatible pointer width")
    if kind not in {EVENT_USER_SIGNAL, EVENT_KERNEL_DIE}:
        raise KernelEventDecodeError("event record has an unsupported event kind")
    if not 1 <= signal <= 64:
        raise KernelEventDecodeError("event record has an invalid signal number")
    if flags & ~EVENT_KNOWN_FLAGS:
        raise KernelEventDecodeError("event record has unknown flags")
    if not flags & EVENT_REGS_VALID:
        raise KernelEventDecodeError("event record lacks a valid register frame")
    if kind == EVENT_USER_SIGNAL and not flags & EVENT_REGS_USER:
        raise KernelEventDecodeError("userspace event lacks a userspace frame")
    if kind == EVENT_KERNEL_DIE and flags & (
        EVENT_REGS_USER | EVENT_REGS_COMPAT
    ):
        raise KernelEventDecodeError("kernel event has userspace-only flags")
    if not 0 < register_count <= EVENT_MAX_REGISTERS:
        raise KernelEventDecodeError("event record has an invalid register count")
    if expected_registers is not None and register_count != expected_registers:
        raise KernelEventDecodeError("event record has the wrong register count")
    if record_size != EVENT_HEADER_BYTES + register_count * 8 or len(data) != record_size:
        raise KernelEventDecodeError("event record size does not match its registers")
    if not tgid or not tid or not sequence or not task or not start_cookie or reserved0:
        raise KernelEventDecodeError("event record has invalid identity or reserved fields")
    if not register_valid_mask or register_valid_mask >> register_count:
        raise KernelEventDecodeError("event record has an invalid register-valid mask")

    pointer_limit = 1 << pointer_bits
    if (
        (kind == EVENT_USER_SIGNAL and not mm)
        or any(value >= pointer_limit for value in (task, mm, signal_struct))
    ):
        raise KernelEventDecodeError("event pointer exceeds the target width")
    comm_bytes = comm_raw.split(b"\0", 1)[0]
    if (
        not comm_bytes
        or any(byte < 0x20 or byte >= 0x7F for byte in comm_bytes)
        or any(comm_raw[len(comm_bytes) + 1 :])
    ):
        raise KernelEventDecodeError("event record has an invalid task name")
    comm = comm_bytes.decode("ascii")
    if flags & EVENT_ADDRESS_VALID:
        if address >= pointer_limit:
            raise KernelEventDecodeError("event address exceeds the target width")
        decoded_address: int | None = address
    else:
        if address:
            raise KernelEventDecodeError("event without an address has a nonzero value")
        decoded_address = None

    registers = struct.unpack_from(
        prefix + f"{register_count}Q", data, EVENT_HEADER_BYTES
    )
    if pointer_bits == 32 and any(value >= 1 << 32 for value in registers):
        raise KernelEventDecodeError("event register exceeds the 32-bit target width")

    return KernelEvent(
        arch=arch,
        byte_order=byte_order,
        pointer_bits=pointer_bits,
        kind=kind,
        signal=signal,
        code=code,
        flags=flags,
        cpu=cpu,
        tgid=tgid,
        tid=tid,
        sequence=sequence,
        task=task,
        mm=mm,
        start_cookie=start_cookie,
        signal_struct=signal_struct,
        comm=comm,
        address=decoded_address,
        registers=tuple(registers),
        register_valid_mask=register_valid_mask,
    )
