"""Present normalized Linux MIPS event registers to legacy GDB clients."""

from __future__ import annotations

from .kernel_events import ARCH_MIPS, KernelEvent
from .linux_oracle import RegisterRead


MIPS_EVENT_REGISTER_COUNT = 38
MIPS_LEGACY_G_REGISTER_COUNT = 73
MIPS_REGISTER_BYTES = 4


class MipsEventPresentationError(ValueError):
    """A decoded event cannot be represented by the legacy MIPS GDB ABI."""


def encode_mips_event_registers(event: KernelEvent) -> RegisterRead:
    """Encode a normalized MIPS32 frame as a legacy 73-register ``g`` reply.

    The event's first 38 entries are r0..r31, status, lo, hi, badvaddr,
    cause, and pc.  The remaining legacy FPU, control, and restart registers
    were not captured by the kernel observer and are therefore emitted as
    unavailable ``x`` digits rather than fabricated zeroes.
    """

    if not isinstance(event, KernelEvent):
        raise TypeError("MIPS event must be a decoded KernelEvent")
    if event.arch != ARCH_MIPS or event.pointer_bits != 32:
        raise MipsEventPresentationError("event is not a 32-bit MIPS frame")
    if event.byte_order not in {"little", "big"}:
        raise MipsEventPresentationError("event has an invalid byte order")
    if len(event.registers) != MIPS_EVENT_REGISTER_COUNT:
        raise MipsEventPresentationError(
            "MIPS event must contain exactly 38 normalized registers"
        )
    if (
        isinstance(event.register_valid_mask, bool)
        or not isinstance(event.register_valid_mask, int)
        or event.register_valid_mask < 0
        or event.register_valid_mask >> MIPS_EVENT_REGISTER_COUNT
    ):
        raise MipsEventPresentationError("MIPS event has an invalid validity mask")

    fields: list[bytes] = []
    unavailable = b"x" * (MIPS_REGISTER_BYTES * 2)
    for index, value in enumerate(event.registers):
        if not event.register_available(index):
            fields.append(unavailable)
            continue
        if (
            isinstance(value, bool)
            or not isinstance(value, int)
            or not 0 <= value < 1 << 32
        ):
            raise MipsEventPresentationError(
                f"MIPS event register {index} does not fit 32 bits"
            )
        if index == 0 and value != 0:
            raise MipsEventPresentationError("MIPS r0 is not the architectural zero")
        encoded = value.to_bytes(MIPS_REGISTER_BYTES, event.byte_order)
        fields.append(encoded.hex().encode())

    fields.extend(
        unavailable
        for _ in range(MIPS_LEGACY_G_REGISTER_COUNT - MIPS_EVENT_REGISTER_COUNT)
    )
    return RegisterRead(b"".join(fields))
