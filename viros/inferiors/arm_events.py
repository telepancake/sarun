"""Present native ARM userspace event frames to GDB's register protocol."""

from __future__ import annotations

from .kernel_events import (
    ARCH_AARCH64,
    ARCH_ARM,
    EVENT_REGS_COMPAT,
    EVENT_REGS_USER,
    EVENT_REGS_VALID,
    EVENT_KERNEL_DIE,
    EVENT_USER_SIGNAL,
    KernelEvent,
)
from .linux_oracle import RegisterRead
from .partial_registers import Aarch64PartialRegisterLayout


ARMV7_EVENT_REGISTERS = tuple(f"r{number}" for number in range(13)) + (
    "sp",
    "lr",
    "pc",
    "cpsr",
)
AARCH64_EVENT_REGISTERS = tuple(f"x{number}" for number in range(31)) + (
    "sp",
    "pc",
    "pstate",
)

_ARMV7_EVENT_INDEX = {
    name: index for index, name in enumerate(ARMV7_EVENT_REGISTERS)
}
_AARCH64_EVENT_INDEX = {
    **{name: index for index, name in enumerate(AARCH64_EVENT_REGISTERS)},
    "cpsr": AARCH64_EVENT_REGISTERS.index("pstate"),
}
_AARCH64_EVENT_INDEX.pop("pstate")


class ArmEventPresentationError(ValueError):
    """A kernel event does not match a supported native ARM frame."""


class Armv7PartialRegisterLayout(Aarch64PartialRegisterLayout):
    """Validated QEMU ARM target-description layout for native ARMv7."""

    _EXPECTED_ARCHITECTURES = frozenset({"arm"})
    _EXPLICIT_LITTLE_ARCHITECTURES = frozenset()
    _ARCHITECTURE_LABEL = "ARM"
    _REQUIRED_BITS = {name: 32 for name in ARMV7_EVENT_REGISTERS}
    _OPTIONAL_BITS = {}


def _validate_event(
    event: KernelEvent,
    layout: Aarch64PartialRegisterLayout,
    *,
    arch: int,
    pointer_bits: int,
    register_count: int,
    description: str,
) -> None:
    if not isinstance(event, KernelEvent):
        raise TypeError("event must be a KernelEvent")
    if not isinstance(layout, Aarch64PartialRegisterLayout):
        raise TypeError("layout must be a parsed target-description layout")
    if event.arch != arch or event.pointer_bits != pointer_bits:
        raise ArmEventPresentationError(
            f"event is not a native {description} frame"
        )
    if event.kind not in {EVENT_USER_SIGNAL, EVENT_KERNEL_DIE}:
        raise ArmEventPresentationError("event kind is unsupported")
    if not event.flags & EVENT_REGS_VALID:
        raise ArmEventPresentationError("event lacks a valid register frame")
    if event.kind == EVENT_USER_SIGNAL and not event.flags & EVENT_REGS_USER:
        raise ArmEventPresentationError("userspace event lacks a userspace frame")
    if event.kind == EVENT_KERNEL_DIE and event.flags & EVENT_REGS_USER:
        raise ArmEventPresentationError("kernel event is marked as userspace")
    if event.flags & EVENT_REGS_COMPAT:
        raise ArmEventPresentationError(
            f"compat frames are not supported by the native {description} layout"
        )
    if event.byte_order not in {"little", "big"}:
        raise ArmEventPresentationError("event has an invalid byte order")
    if event.byte_order != layout.byte_order:
        raise ArmEventPresentationError(
            "event byte order does not match the QEMU target description"
        )
    if len(event.registers) != register_count:
        raise ArmEventPresentationError(
            f"{description} event must contain exactly {register_count} registers"
        )
    if (
        isinstance(event.register_valid_mask, bool)
        or not isinstance(event.register_valid_mask, int)
        or not event.register_valid_mask
        or event.register_valid_mask >> register_count
    ):
        raise ArmEventPresentationError("event has an invalid register mask")


def _encode_event(
    event: KernelEvent,
    layout: Aarch64PartialRegisterLayout,
    indexes: dict[str, int],
) -> RegisterRead:
    fields: list[bytes] = []
    for register in layout.registers:
        index = indexes.get(register.name)
        if index is None or not event.register_available(index):
            fields.append(b"x" * (register.bitsize // 4))
            continue

        value = event.registers[index]
        if (
            isinstance(value, bool)
            or not isinstance(value, int)
            or value < 0
            or value >= 1 << register.bitsize
        ):
            raise ArmEventPresentationError(
                f"event value for {register.name!r} does not fit the described "
                f"{register.bitsize}-bit register"
            )
        raw = value.to_bytes(register.bitsize // 8, event.byte_order)
        fields.append(raw.hex().encode("ascii"))
    return RegisterRead(b"".join(fields))


def encode_armv7_event_registers(
    event: KernelEvent, layout: Armv7PartialRegisterLayout
) -> RegisterRead:
    """Encode one native 17-register ARMv7 frame; compat is unsupported."""

    if not isinstance(layout, Armv7PartialRegisterLayout):
        raise TypeError("ARMv7 event requires an Armv7PartialRegisterLayout")
    _validate_event(
        event,
        layout,
        arch=ARCH_ARM,
        pointer_bits=32,
        register_count=len(ARMV7_EVENT_REGISTERS),
        description="ARMv7",
    )
    return _encode_event(event, layout, _ARMV7_EVENT_INDEX)


def encode_aarch64_event_registers(
    event: KernelEvent, layout: Aarch64PartialRegisterLayout
) -> RegisterRead:
    """Encode one native 34-register AArch64 frame; AArch32 is unsupported."""

    if (
        not isinstance(layout, Aarch64PartialRegisterLayout)
        or layout._ARCHITECTURE_LABEL != "AArch64"
    ):
        raise TypeError("AArch64 event requires an AArch64 target layout")
    _validate_event(
        event,
        layout,
        arch=ARCH_AARCH64,
        pointer_bits=64,
        register_count=len(AARCH64_EVENT_REGISTERS),
        description="AArch64",
    )
    return _encode_event(event, layout, _AARCH64_EVENT_INDEX)
