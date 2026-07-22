"""Immutable architecture contracts for reversible call-gate execution.

Keeping register names, widths, call ABI, and invocation-state rules together
gives each target port one narrow seam without weakening manifest validation.
"""

from __future__ import annotations

from dataclasses import dataclass
from types import MappingProxyType
from typing import Callable, Mapping


OriginalStatePrecondition = Callable[[Mapping[str, int]], str | None]
ControlStateDeriver = Callable[[int | None, Mapping[str, int]], int]
EntryContextDeriver = Callable[
    [Mapping[str, int]], tuple[tuple[str, int], ...]
]


def _accept_original_state(registers: Mapping[str, int]) -> str | None:
    return None


def _unchanged_entry_context(
    registers: Mapping[str, int],
) -> tuple[tuple[str, int], ...]:
    return ()


def _manifest_control_state(
    manifest_value: int | None, registers: Mapping[str, int]
) -> int:
    if manifest_value is None:
        raise ValueError("the architecture requires manifest control state")
    return manifest_value


MIPS_STATUS_IE = 1 << 0
MIPS_STATUS_EXL = 1 << 1
MIPS_STATUS_ERL = 1 << 2
MIPS_STATUS_KSU = 3 << 3
MIPS_GATE_STATUS_CLEAR = MIPS_STATUS_KSU | MIPS_STATUS_IE | MIPS_STATUS_ERL


def _mips32_original_state(registers: Mapping[str, int]) -> str | None:
    if registers["r0"] != 0:
        return "MIPS r0 is not the architectural zero value"
    return None


def _mips32_gate_status(
    manifest_value: int | None, registers: Mapping[str, int]
) -> int:
    if manifest_value is not None:
        raise ValueError("MIPS gate status must not come from the manifest")
    return (registers["status"] & ~MIPS_GATE_STATUS_CLEAR) | MIPS_STATUS_EXL


ARM_CPSR_MODE = 0x1F
ARM_CPSR_SVC = 0x13
ARM_CPSR_T = 1 << 5
ARM_CPSR_F = 1 << 6
ARM_CPSR_I = 1 << 7
ARM_CPSR_A = 1 << 8
ARM_CPSR_E = 1 << 9
ARM_CPSR_IT_LOW = 0x3F << 10
ARM_CPSR_J = 1 << 24
ARM_CPSR_IT_HIGH = 0x3 << 25
ARM_CPSR_GATE_CLEAR = (
    ARM_CPSR_MODE
    | ARM_CPSR_T
    | ARM_CPSR_F
    | ARM_CPSR_I
    | ARM_CPSR_A
    | ARM_CPSR_E
    | ARM_CPSR_IT_LOW
    | ARM_CPSR_J
    | ARM_CPSR_IT_HIGH
)
ARM_CPSR_GATE_VALUE = ARM_CPSR_SVC | ARM_CPSR_F | ARM_CPSR_I | ARM_CPSR_A


def _armv7_original_state(registers: Mapping[str, int]) -> str | None:
    cpsr = registers["cpsr"]
    # SP and LR are banked by processor mode.  The generic transaction writes
    # LR before CPSR, so accepting another mode would place the completion
    # address in the wrong bank.  MikroTik's published non-Thumb2 ARM kernel
    # normally executes C code in little-endian ARM SVC state.
    if cpsr & ARM_CPSR_MODE != ARM_CPSR_SVC:
        return "ARM call gate requires a CPU already stopped in SVC mode"
    incompatible = (
        ARM_CPSR_T
        | ARM_CPSR_E
        | ARM_CPSR_IT_LOW
        | ARM_CPSR_J
        | ARM_CPSR_IT_HIGH
    )
    if cpsr & incompatible:
        return "ARM call gate requires little-endian ARM (not Thumb/Jazelle/IT) state"
    return None


def _armv7_gate_cpsr(
    manifest_value: int | None, registers: Mapping[str, int]
) -> int:
    if manifest_value is not None:
        raise ValueError("ARM gate CPSR must not come from the manifest")
    return (registers["cpsr"] & ~ARM_CPSR_GATE_CLEAR) | ARM_CPSR_GATE_VALUE


X86_EFLAGS_FIXED = 1 << 1
X86_EFLAGS_TF = 1 << 8
X86_EFLAGS_IF = 1 << 9
X86_EFLAGS_DF = 1 << 10
X86_EFLAGS_NT = 1 << 14
X86_EFLAGS_RF = 1 << 16
X86_EFLAGS_VM = 1 << 17
X86_EFLAGS_AC = 1 << 18
X86_GATE_EFLAGS_CLEAR = (
    X86_EFLAGS_TF
    | X86_EFLAGS_IF
    | X86_EFLAGS_DF
    | X86_EFLAGS_NT
    | X86_EFLAGS_RF
    | X86_EFLAGS_VM
    | X86_EFLAGS_AC
)


def _x86_64_original_state(registers: Mapping[str, int]) -> str | None:
    cpl = registers["cs"] & 3
    if cpl not in {0, 3} or registers["ss"] & 3 != cpl:
        return "x86-64 call gate requires matching kernel or userspace segments"
    rip = registers["rip"]
    if cpl == 0 and rip < 0xFFFF800000000000:
        return "x86-64 kernel stop lacks a canonical high-half PC"
    if cpl == 3 and rip > 0x00007FFFFFFFFFFF:
        return "x86-64 userspace stop lacks a canonical low-half PC"
    if not registers["eflags"] & X86_EFLAGS_FIXED:
        return "x86-64 EFLAGS lacks its architectural fixed bit"
    return None


def _x86_64_gate_eflags(
    manifest_value: int | None, registers: Mapping[str, int]
) -> int:
    if manifest_value is not None:
        raise ValueError("x86-64 gate EFLAGS must not come from the manifest")
    return (registers["eflags"] | X86_EFLAGS_FIXED) & ~X86_GATE_EFLAGS_CLEAR


def _x86_64_entry_context(
    registers: Mapping[str, int],
) -> tuple[tuple[str, int], ...]:
    if registers["cs"] & 3 == 0:
        return ()
    # A normal x86 syscall/interrupt entry executes SWAPGS before C code.
    # When GDB redirects a CPU stopped in userspace there is no architectural
    # entry stub, so reproduce that reversible part of the entry state.  The
    # per-CPU kernel base lives in KERNEL_GS_BASE while userspace is running.
    return (
        ("gs_base", registers["k_gs_base"]),
        ("k_gs_base", registers["gs_base"]),
    )


@dataclass(frozen=True)
class RegisterSpec:
    """One core register which must be snapshotted and restored."""

    name: str
    bits: int
    rsp_number: int | None = None
    restoration_audit_mask: int | None = None


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
    link_register: str | None
    control_register: str
    control_state_bits: int
    control_state_mask: int
    control_state_value: int
    control_state_description: str
    control_state_plan_description: str
    manifest_control_state: bool
    original_state_precondition: OriginalStatePrecondition
    control_state_deriver: ControlStateDeriver
    entry_mode_registers: tuple[tuple[str, int], ...]
    entry_context_deriver: EntryContextDeriver
    entry_address_registers: tuple[str, ...]
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
        if any(
            register.restoration_audit_mask is not None
            and (
                isinstance(register.restoration_audit_mask, bool)
                or not isinstance(register.restoration_audit_mask, int)
                or register.restoration_audit_mask <= 0
                or register.restoration_audit_mask >= 1 << register.bits
            )
            for register in self.core_registers
        ):
            raise ValueError(
                "architecture restoration audit masks must be positive partial "
                "register-width bitmasks"
            )
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
            self.control_register,
            *self.argument_registers,
        }
        if self.link_register is not None:
            required.add(self.link_register)
        if not required.issubset(names):
            raise ValueError("architecture call ABI names unknown core registers")
        if set(self.restore_order) != set(names) or len(self.restore_order) != len(names):
            raise ValueError("architecture restore order must contain every core register once")
        if self.restore_order[-1] != self.pc_register:
            raise ValueError("architecture restore order must restore PC last")
        if (
            len(self.entry_address_registers) != len(set(self.entry_address_registers))
            or not set(self.entry_address_registers).issubset(names)
        ):
            raise ValueError(
                "architecture entry-address registers must be unique core registers"
            )
        reserved_entry_registers = {
            self.pc_register,
            self.sp_register,
            self.control_register,
            *self.argument_registers,
        }
        if self.link_register is not None:
            reserved_entry_registers.add(self.link_register)
        if reserved_entry_registers.intersection(self.entry_address_registers):
            raise ValueError(
                "architecture entry-address registers must not overlap call ABI registers"
            )
        mode_names = tuple(register for register, _ in self.entry_mode_registers)
        if (
            len(mode_names) != len(set(mode_names))
            or not set(mode_names).issubset(names)
            or set(mode_names).intersection(
                reserved_entry_registers | set(self.entry_address_registers)
            )
        ):
            raise ValueError(
                "architecture entry-mode registers must be unique, independent "
                "core registers"
            )
        for register, value in self.entry_mode_registers:
            if (
                isinstance(value, bool)
                or not isinstance(value, int)
                or not 0 <= value < 1 << self.register_bits(register)
            ):
                raise ValueError(
                    f"entry-mode value for {register} does not fit its width"
                )

    @property
    def register_names(self) -> tuple[str, ...]:
        return tuple(register.name for register in self.core_registers)

    def register_bits(self, name: str) -> int:
        for register in self.core_registers:
            if register.name == name:
                return register.bits
        raise KeyError(name)

    def restoration_audit_mask(self, name: str) -> int:
        """Return the bits whose original value can be restored and verified."""

        for register in self.core_registers:
            if register.name == name:
                if register.restoration_audit_mask is not None:
                    return register.restoration_audit_mask
                return (1 << register.bits) - 1
        raise KeyError(name)

    @property
    def has_fixed_rsp_registers(self) -> bool:
        return bool(
            self.core_registers and self.core_registers[0].rsp_number is not None
        )

    def valid_control_state(self, value: int) -> bool:
        if not self.manifest_control_state:
            return False
        return (
            0 <= value < 1 << self.control_state_bits
            and value & self.control_state_mask == self.control_state_value
        )

    def validate_original_state(self, registers: Mapping[str, int]) -> None:
        """Reject an incomplete or unsafe selected-CPU snapshot."""

        for register in self.core_registers:
            value = registers.get(register.name)
            if (
                isinstance(value, bool)
                or not isinstance(value, int)
                or not 0 <= value < 1 << register.bits
            ):
                raise ValueError(
                    f"original {register.name} does not fit its {register.bits}-bit width"
                )
        reason = self.original_state_precondition(registers)
        if reason is not None:
            raise ValueError(reason)

    def entry_register_values(
        self,
        *,
        request_address: int,
        result_address: int,
        result_size: int,
        completion_address: int,
        control_state: int | None,
        original_registers: Mapping[str, int],
        stack_pointer: int,
        entry_address: int,
    ) -> tuple[tuple[str, int], ...]:
        """Return register writes in the proven entry order."""

        self.validate_original_state(original_registers)
        derived_control_state = self.control_state_deriver(
            control_state, original_registers
        )
        if not 0 <= derived_control_state < 1 << self.control_state_bits:
            raise ValueError(
                "derived control state does not fit its architectural width"
            )
        if (
            self.manifest_control_state
            and not self.valid_control_state(derived_control_state)
        ):
            raise ValueError(
                "manifest control state does not satisfy the architecture policy"
            )
        arguments = (request_address, result_address, result_size)
        link = (
            ((self.link_register, completion_address),)
            if self.link_register is not None
            else ()
        )
        entry_context = self.entry_context_deriver(original_registers)
        context_names = tuple(register for register, _ in entry_context)
        fixed_names = {register for register, _ in self.entry_mode_registers}
        if (
            len(context_names) != len(set(context_names))
            or not set(context_names).issubset(self.register_names)
            or set(context_names).intersection(fixed_names)
        ):
            raise ValueError(
                "derived entry-context registers must be unique, known, and "
                "independent of fixed entry-mode registers"
            )
        values = (
            # Mode comes first: on x86 this changes QEMU's GPR write width from
            # IA32 to 64-bit before installing high kernel arguments.
            *self.entry_mode_registers,
            *entry_context,
            *tuple(zip(self.argument_registers, arguments)),
            *tuple(
                (register, entry_address)
                for register in self.entry_address_registers
            ),
            *link,
            (self.control_register, derived_control_state),
            (self.sp_register, stack_pointer),
            (self.pc_register, entry_address),
        )
        for register, value in values:
            if (
                isinstance(value, bool)
                or not isinstance(value, int)
                or not 0 <= value < 1 << self.register_bits(register)
            ):
                raise ValueError(
                    f"entry value for {register} does not fit its architectural width"
                )
        return values


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
    manifest_control_state=True,
    original_state_precondition=_accept_original_state,
    control_state_deriver=_manifest_control_state,
    entry_mode_registers=(),
    entry_context_deriver=_unchanged_entry_context,
    entry_address_registers=(),
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


_ARMV7_NAMES = tuple(
    [f"r{number}" for number in range(13)] + ["sp", "lr", "pc", "cpsr"]
)

# QEMU's arm-core.xml assigns r0..pc their historical numbers 0..15 and CPSR
# number 25 (with the old FPA gap in between).  Leave rsp_number unset so the
# target adapter must validate and use QEMU's advertised XML mapping.
ARMV7 = ArchitectureDescriptor(
    name="arm",
    display_name="ARMv7 little-endian",
    address_bits=32,
    page_size=4096,
    max_region_size=64 * 1024,
    instruction_alignment=4,
    stack_alignment=8,
    target_byte_order="little",
    qemu_architecture_names=("arm",),
    breakpoint_size=4,
    core_registers=tuple(RegisterSpec(name, 32) for name in _ARMV7_NAMES),
    restore_order=tuple(
        [f"r{number}" for number in range(13)]
        + ["lr", "sp", "cpsr", "pc"]
    ),
    pc_register="pc",
    sp_register="sp",
    argument_registers=("r0", "r1", "r2"),
    link_register="lr",
    control_register="cpsr",
    control_state_bits=32,
    control_state_mask=0,
    control_state_value=0,
    control_state_description="CPSR derived from the stopped CPU",
    control_state_plan_description="derived ARM SVC CPSR with AIF masked",
    manifest_control_state=False,
    original_state_precondition=_armv7_original_state,
    control_state_deriver=_armv7_gate_cpsr,
    entry_mode_registers=(),
    entry_context_deriver=_unchanged_entry_context,
    entry_address_registers=(),
    known_capabilities=frozenset({"snapshot-v1"}),
    dependent_capabilities=frozenset(),
)


_X86_64_NAMES = (
    "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp",
    "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15",
    "rip", "eflags", "cs", "ss", "gs_base", "k_gs_base",
)

# The helper begins in a retained x86-64 wrapper.  That wrapper performs the
# normal SysV call (which supplies the on-stack return address) and falls
# through to its internal completion symbol, so no architectural link register
# is invented here.
X86_64 = ArchitectureDescriptor(
    name="x86_64",
    display_name="x86-64",
    address_bits=64,
    page_size=4096,
    max_region_size=64 * 1024,
    instruction_alignment=1,
    stack_alignment=16,
    target_byte_order="little",
    qemu_architecture_names=("i386:x86-64",),
    breakpoint_size=1,
    core_registers=tuple(
        RegisterSpec(name, 32 if name in {"eflags", "cs", "ss"} else 64)
        for name in _X86_64_NAMES
    ),
    restore_order=tuple(
        name for name in _X86_64_NAMES
        if name not in {"rsp", "eflags", "cs", "ss", "rip"}
    ) + ("rsp", "ss", "cs", "eflags", "rip"),
    pc_register="rip",
    sp_register="rsp",
    argument_registers=("rdi", "rsi", "rdx"),
    link_register=None,
    control_register="eflags",
    control_state_bits=32,
    control_state_mask=0,
    control_state_value=0,
    control_state_description="EFLAGS derived from the stopped kernel CPU",
    control_state_plan_description="derived kernel EFLAGS with asynchronous state masked",
    manifest_control_state=False,
    original_state_precondition=_x86_64_original_state,
    control_state_deriver=_x86_64_gate_eflags,
    # Linux x86-64's stable GDT selectors.  QEMU's GDB register path loads the
    # associated descriptor cache, allowing a stopped IA32/native userspace
    # CPU to enter the retained 64-bit helper and later restore its exact mode.
    entry_mode_registers=(("ss", 0x18), ("cs", 0x10)),
    entry_context_deriver=_x86_64_entry_context,
    entry_address_registers=(),
    known_capabilities=frozenset({"snapshot-v1"}),
    dependent_capabilities=frozenset(),
)


_MMIPS_REGISTER_NAMES = tuple(
    [f"r{number}" for number in range(32)]
    + ["status", "lo", "hi", "badvaddr", "cause", "pc"]
)

# CP0 Cause is not a general storage register.  QEMU's architectural write
# path (cpu_mips_store_cause) accepts IV, WP, the two software-interrupt bits,
# and DC on the MIPS32r2 CPU used by the MMIPS machine.  BD, TI, CE, hardware
# interrupt-pending, and ExcCode are read-only or live exception state and may
# legitimately change while the selected CPU executes the helper.  Audit every
# writable field without claiming that those live fields can be restored by a
# GDB register write.
MIPS_CAUSE_RESTORATION_AUDIT_MASK = 0x08C00300

# Description of the legacy MMIPS QEMU stub's fixed MIPS32 register numbering
# and o32 helper-call policy.  CP1 is deliberately absent: a soft-float helper
# cannot clobber it, and no-FPU targets reject those optional p packets.
MIPS32EL_MMIPS = ArchitectureDescriptor(
    name="mmips",
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
        RegisterSpec(
            name,
            32,
            number,
            restoration_audit_mask=(
                MIPS_CAUSE_RESTORATION_AUDIT_MASK if name == "cause" else None
            ),
        )
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
    control_state_description="status derived from the stopped CPU",
    control_state_plan_description="derived kernel EXL status",
    manifest_control_state=False,
    original_state_precondition=_mips32_original_state,
    control_state_deriver=_mips32_gate_status,
    entry_mode_registers=(),
    entry_context_deriver=_unchanged_entry_context,
    entry_address_registers=("r25",),
    known_capabilities=frozenset({"snapshot-v1"}),
    dependent_capabilities=frozenset(),
)


ARCHITECTURES: Mapping[str, ArchitectureDescriptor] = MappingProxyType({
    AARCH64.name: AARCH64,
    ARMV7.name: ARMV7,
    X86_64.name: X86_64,
    MIPS32EL_MMIPS.name: MIPS32EL_MMIPS,
})


def architecture_by_name(name: object) -> ArchitectureDescriptor:
    """Return a supported descriptor without accepting aliases implicitly."""

    try:
        return ARCHITECTURES[name]  # type: ignore[index]
    except (KeyError, TypeError) as exc:
        raise LookupError(name) from exc
