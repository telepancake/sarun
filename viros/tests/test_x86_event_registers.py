from dataclasses import replace
import unittest

from inferiors.kernel_events import (
    ARCH_X86,
    EVENT_REGS_COMPAT,
    EVENT_REGS_USER,
    EVENT_REGS_VALID,
    EVENT_USER_SIGNAL,
    KernelEvent,
)
from inferiors.partial_registers import (
    PartialRegisterError,
    X86KernelEventRegisterLayout,
    X86_PT_REGS_REGISTERS,
)


X86_DESCRIBED_REGISTERS = (
    *((name, 64) for name in (
        "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp",
        "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rip",
    )),
    ("eflags", 32),
    ("cs", 32),
    ("ss", 32),
    ("ds", 32),
    ("es", 32),
    ("fs", 32),
    ("gs", 32),
    ("cr3", 64),
    ("orig_rax", 64),
)


def descriptions(registers=X86_DESCRIBED_REGISTERS):
    core = "".join(
        f'<reg name="{name}" bitsize="{bits}" regnum="{number}"/>'
        for number, (name, bits) in enumerate(registers)
    )
    return {
        "target.xml": (
            '<target xmlns:xi="http://www.w3.org/2001/XInclude">'
            "<architecture>i386:x86-64</architecture>"
            '<xi:include href="amd64-core.xml"/>'
            "</target>"
        ),
        "amd64-core.xml": f"<feature>{core}</feature>",
    }


def described_bytes(registers=X86_DESCRIBED_REGISTERS):
    return sum(bits // 8 for _, bits in registers)


def event(*, compat=False, registers=None, valid_mask=None, **changes):
    if registers is None:
        registers = tuple(0x1000 + index for index in range(21))
    if valid_mask is None:
        valid_mask = (1 << len(registers)) - 1
    values = dict(
        arch=ARCH_X86,
        byte_order="little",
        pointer_bits=64,
        kind=EVENT_USER_SIGNAL,
        signal=11,
        code=1,
        flags=(EVENT_REGS_VALID | EVENT_REGS_USER)
        | (EVENT_REGS_COMPAT if compat else 0),
        cpu=0,
        tgid=41,
        tid=42,
        sequence=1,
        task=0xFFFF800000001000,
        mm=0xFFFF800000002000,
        start_cookie=0x1234,
        signal_struct=0xFFFF800000003000,
        comm="quagga",
        address=0x400123,
        registers=tuple(registers),
        register_valid_mask=valid_mask,
    )
    values.update(changes)
    return KernelEvent(**values)


def register_chunk(layout, payload, name):
    offset = 0
    for register in layout.registers:
        length = register.bitsize // 4
        if register.name == name:
            return payload[offset:offset + length]
        offset += length
    raise KeyError(name)


class X86KernelEventRegisterLayoutTests(unittest.TestCase):
    def layout(self, registers=X86_DESCRIBED_REGISTERS):
        return X86KernelEventRegisterLayout.from_target_descriptions(
            descriptions(registers),
            byte_order="little",
            observed_g_bytes=described_bytes(registers),
        )

    def test_native_pt_regs_map_into_qemu_packet_order(self):
        layout = self.layout()
        stopped = event()
        payload = layout.encode_kernel_event(stopped)
        index = {name: number for number, name in enumerate(X86_PT_REGS_REGISTERS)}

        for name in (
            "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp",
            "r8", "r15", "rip", "eflags", "cs", "ss", "orig_rax",
        ):
            bits = next(reg.bitsize for reg in layout.registers if reg.name == name)
            expected = stopped.registers[index[name]].to_bytes(
                bits // 8, "little"
            ).hex().encode()
            self.assertEqual(register_chunk(layout, payload, name), expected)

        for name in ("ds", "es", "fs", "gs", "cr3"):
            chunk = register_chunk(layout, payload, name)
            self.assertEqual(chunk, b"x" * len(chunk))
        self.assertEqual(len(payload), described_bytes() * 2)

    def test_ia32_uses_low_halves_and_withholds_x86_64_only_registers(self):
        registers = [0xA5A5A5A500000000 | index for index in range(21)]
        registers[X86_PT_REGS_REGISTERS.index("rax")] = 0xDEADBEEF12345678
        registers[X86_PT_REGS_REGISTERS.index("rip")] = 0x9999999987654321
        stopped = event(compat=True, registers=registers)
        layout = self.layout()
        payload = layout.encode_kernel_event(stopped)

        self.assertEqual(
            register_chunk(layout, payload, "rax"), b"7856341200000000"
        )
        self.assertEqual(
            register_chunk(layout, payload, "rip"), b"2143658700000000"
        )
        for name in ("r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"):
            self.assertEqual(register_chunk(layout, payload, name), b"x" * 16)
        # Segment selectors and flags retain their described 32-bit width.
        self.assertEqual(register_chunk(layout, payload, "cs"), b"11000000")
        self.assertEqual(register_chunk(layout, payload, "eflags"), b"12000000")

    def test_event_valid_mask_becomes_per_register_unavailable_markers(self):
        rax = X86_PT_REGS_REGISTERS.index("rax")
        rip = X86_PT_REGS_REGISTERS.index("rip")
        valid = ((1 << 21) - 1) & ~(1 << rax) & ~(1 << rip)
        payload = self.layout().encode_kernel_event(event(valid_mask=valid))

        self.assertEqual(register_chunk(self.layout(), payload, "rax"), b"x" * 16)
        self.assertEqual(register_chunk(self.layout(), payload, "rip"), b"x" * 16)
        self.assertNotIn(b"x", register_chunk(self.layout(), payload, "rbx"))

    def test_orig_rax_is_optional_in_qemu_observed_prefix(self):
        without_orig = tuple(
            register for register in X86_DESCRIBED_REGISTERS
            if register[0] != "orig_rax"
        )
        layout = self.layout(without_orig)
        payload = layout.encode_kernel_event(event())
        self.assertEqual(len(payload), described_bytes(without_orig) * 2)

    def test_rejects_incompatible_event_metadata_and_values(self):
        layout = self.layout()
        invalid = (
            replace(event(), arch=1),
            replace(event(), byte_order="big"),
            replace(event(), pointer_bits=32),
            replace(event(), kind=99),
            replace(event(), flags=0),
            replace(event(), registers=(0,) * 20, register_valid_mask=1),
            replace(event(), register_valid_mask=1 << 21),
            replace(
                event(),
                registers=((1 << 64),) + (0,) * 20,
                register_valid_mask=(1 << 21) - 1,
            ),
        )
        for stopped in invalid:
            with self.subTest(stopped=stopped):
                with self.assertRaises(PartialRegisterError):
                    layout.encode_kernel_event(stopped)

    def test_rejects_wrong_architecture_missing_core_and_wrong_width(self):
        target = descriptions()
        target["target.xml"] = target["target.xml"].replace(
            "i386:x86-64", "aarch64"
        )
        with self.assertRaisesRegex(PartialRegisterError, "expected x86-64"):
            X86KernelEventRegisterLayout.from_target_descriptions(
                target, byte_order="little", observed_g_bytes=described_bytes()
            )

        missing = tuple(reg for reg in X86_DESCRIBED_REGISTERS if reg[0] != "rip")
        with self.assertRaisesRegex(PartialRegisterError, "lacks required.*rip"):
            self.layout(missing)

        wrong_eflags = tuple(
            (name, 64 if name == "eflags" else bits)
            for name, bits in X86_DESCRIBED_REGISTERS
        )
        with self.assertRaisesRegex(PartialRegisterError, "eflags.*64 bits"):
            self.layout(wrong_eflags)

        wrong_orig = tuple(
            (name, 32 if name == "orig_rax" else bits)
            for name, bits in X86_DESCRIBED_REGISTERS
        )
        with self.assertRaisesRegex(PartialRegisterError, "orig_rax.*32 bits"):
            self.layout(wrong_orig)

        with self.assertRaisesRegex(PartialRegisterError, "little-endian"):
            X86KernelEventRegisterLayout.from_target_descriptions(
                descriptions(), byte_order="big",
                observed_g_bytes=described_bytes(),
            )


if __name__ == "__main__":
    unittest.main()
