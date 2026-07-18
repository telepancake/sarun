"""Immutable architecture contracts for reversible call-gate execution.

The first descriptor deliberately models only the already-proven AArch64
contract.  Keeping register names, widths, call ABI, and invocation-state
rules together gives later ports one narrow seam without weakening legacy
manifest validation.
"""

from __future__ import annotations

from dataclasses import dataclass
from types import MappingProxyType
from typing import Mapping


@dataclass(frozen=True)
class RegisterSpec:
    """One core register which must be snapshotted and restored."""

    name: str
    bits: int
    rsp_number: int | None = None


@dataclass(frozen=True)
class ArchitectureDescriptor:
    """The architecture-dependent part of the call-gate transaction."""

    name: str
    display_name: str
    address_bits: int
    page_size: int
    max_region_size: int
    instruction_alignment: int
    stack_alignment: int
    target_byte_order: str
    qemu_architecture_names: tuple[str, ...]
    breakpoint_size: int
    core_registers: tuple[RegisterSpec, ...]
    restore_order: tuple[str, ...]
    pc_register: str
    sp_register: str
    argument_registers: tuple[str, str, str]
    link_register: str
    control_register: str
    control_state_bits: int
    control_state_mask: int
    control_state_value: int
    control_state_description: str
    control_state_plan_description: str
    known_capabilities: frozenset[str]
    dependent_capabilities: frozenset[str]

    def __post_init__(self) -> None:
        names = self.register_names
        if (
            any(not isinstance(name, str) or not name for name in names)
            or len(names) != len(set(names))
        ):
            raise ValueError(
                "architecture core-register names must be nonempty and unique"
            )
        if any(
            isinstance(register.bits, bool)
            or not isinstance(register.bits, int)
            or register.bits <= 0
            or register.bits % 8
            for register in self.core_registers
        ):
            raise ValueError("architecture register widths must be positive whole bytes")
        fixed_numbers = tuple(
            register.rsp_number for register in self.core_registers
        )
        fixed = tuple(number for number in fixed_numbers if number is not None)
        if fixed and len(fixed) != len(fixed_numbers):
            raise ValueError(
                "architecture fixed RSP register map must cover every core register"
            )
        if any(
            isinstance(number, bool) or not isinstance(number, int) or number < 0
            for number in fixed
        ):
            raise ValueError(
                "architecture fixed RSP register numbers must be nonnegative integers"
            )
        if len(fixed) != len(set(fixed)):
            raise ValueError("architecture fixed RSP register numbers must be unique")
        if self.target_byte_order not in {"little", "big"}:
            raise ValueError("architecture target byte order must be little or big")
        if (
            not self.qemu_architecture_names
            or any(not isinstance(name, str) or not name for name in self.qemu_architecture_names)
            or len(self.qemu_architecture_names) != len(set(self.qemu_architecture_names))
        ):
            raise ValueError("architecture QEMU target names must be nonempty and unique")
        if (
            isinstance(self.breakpoint_size, bool)
            or not isinstance(self.breakpoint_size, int)
            or self.breakpoint_size <= 0
        ):
            raise ValueError("architecture breakpoint size must be positive")
        required = {
            self.pc_register,
            self.sp_register,
            self.link_register,
            self.control_register,
            *self.argument_registers,
        }
        if not required.issubset(names):
            raise ValueError("architecture call ABI names unknown core registers")
        if set(self.restore_order) != set(names) or len(self.restore_order) != len(names):
            raise ValueError("architecture restore order must contain every core register once")
        if self.restore_order[-1] != self.pc_register:
            raise ValueError("architecture restore order must restore PC last")

    @property
    def register_names(self) -> tuple[str, ...]:
        return tuple(register.name for register in self.core_registers)

    def register_bits(self, name: str) -> int:
        for register in self.core_registers:
            if register.name == name:
                return register.bits
        raise KeyError(name)

    @property
    def has_fixed_rsp_registers(self) -> bool:
        return bool(
            self.core_registers and self.core_registers[0].rsp_number is not None
        )

    def valid_control_state(self, value: int) -> bool:
        return (
            0 <= value < 1 << self.control_state_bits
            and value & self.control_state_mask == self.control_state_value
        )

    def entry_register_values(
        self,
        *,
        request_address: int,
        result_address: int,
        result_size: int,
        completion_address: int,
        control_state: int,
        stack_pointer: int,
        entry_address: int,
    ) -> tuple[tuple[str, int], ...]:
        """Return register writes in the proven entry order."""

        arguments = (request_address, result_address, result_size)
        return (
            *tuple(zip(self.argument_registers, arguments)),
            (self.link_register, completion_address),
            (self.control_register, control_state),
            (self.sp_register, stack_pointer),
            (self.pc_register, entry_address),
        )


_AARCH64_NAMES = tuple([f"x{number}" for number in range(31)] + ["sp", "pc", "cpsr"])

AARCH64 = ArchitectureDescriptor(
    name="aarch64",
    display_name="AArch64",
    address_bits=64,
    page_size=4096,
    max_region_size=64 * 1024,
    instruction_alignment=4,
    stack_alignment=16,
    target_byte_order="little",
    qemu_architecture_names=("aarch64", "aarch64:little"),
    breakpoint_size=4,
    core_registers=tuple(
        RegisterSpec(name, 32 if name == "cpsr" else 64)
        for name in _AARCH64_NAMES
    ),
    restore_order=tuple([f"x{number}" for number in range(31)] + ["sp", "cpsr", "pc"]),
    pc_register="pc",
    sp_register="sp",
    argument_registers=("x0", "x1", "x2"),
    link_register="x30",
    control_register="cpsr",
    control_state_bits=32,
    control_state_mask=0x3CF,
    control_state_value=0x3C5,
    control_state_description="EL1h with DAIF masked",
    control_state_plan_description="EL1h",
    known_capabilities=frozenset({
        "snapshot-v1",
        "translate-va-aarch64-v1",
        "saved-regs-aarch64-v1",
    }),
    dependent_capabilities=frozenset({
        "translate-va-aarch64-v1",
        "saved-regs-aarch64-v1",
    }),
)


_MMIPS_REGISTER_NAMES = tuple(
    [f"r{number}" for number in range(32)]
    + ["status", "lo", "hi", "badvaddr", "cause", "pc"]
)

# Backend-only description of the legacy MMIPS QEMU stub's fixed MIPS32
# register numbering.  CP1 is deliberately absent: a soft-float probe cannot
# clobber it, and no-FPU targets reject those optional p packets.  This constant
# is intentionally absent from ARCHITECTURES: no manifest, probe, or call-gate
# support is implied by it.
MIPS32EL_MMIPS = ArchitectureDescriptor(
    name="mips32el-mmips-experimental",
    display_name="MIPS32 little-endian MMIPS",
    address_bits=32,
    page_size=4096,
    max_region_size=64 * 1024,
    instruction_alignment=4,
    stack_alignment=8,
    target_byte_order="little",
    qemu_architecture_names=("mips",),
    breakpoint_size=4,
    core_registers=tuple(
        RegisterSpec(name, 32, number)
        for number, name in enumerate(_MMIPS_REGISTER_NAMES)
    ),
    restore_order=_MMIPS_REGISTER_NAMES,
    pc_register="pc",
    sp_register="r29",
    argument_registers=("r4", "r5", "r6"),
    link_register="r31",
    control_register="status",
    control_state_bits=32,
    control_state_mask=0,
    control_state_value=0,
    control_state_description="descriptor-only preserved kernel status",
    control_state_plan_description="preserved kernel status",
    known_capabilities=frozenset(),
    dependent_capabilities=frozenset(),
)


ARCHITECTURES: Mapping[str, ArchitectureDescriptor] = MappingProxyType({
    AARCH64.name: AARCH64,
})


def architecture_by_name(name: object) -> ArchitectureDescriptor:
    """Return a supported descriptor without accepting aliases implicitly."""

    try:
        return ARCHITECTURES[name]  # type: ignore[index]
    except (KeyError, TypeError) as exc:
        raise LookupError(name) from exc
