from dataclasses import replace
import unittest

from inferiors.arm_events import (
    AARCH64_EVENT_REGISTERS,
    ARMV7_EVENT_REGISTERS,
    ArmEventPresentationError,
    Armv7PartialRegisterLayout,
    encode_aarch64_event_registers,
    encode_armv7_event_registers,
)
from inferiors.kernel_events import (
    ARCH_AARCH64,
    ARCH_ARM,
    EVENT_REGS_COMPAT,
    EVENT_REGS_USER,
    EVENT_REGS_VALID,
    EVENT_USER_SIGNAL,
    KernelEvent,
)
from inferiors.partial_registers import (
    AARCH64_USER_REGISTERS,
    Aarch64PartialRegisterLayout,
    PartialRegisterError,
)


def target_descriptions(architecture, core, extra=b""):
    return {
        "target.xml": (
            b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
            + f"<architecture>{architecture}</architecture>".encode()
            + b'<xi:include href="core.xml"/>'
            + (b'<xi:include href="extra.xml"/>' if extra else b"")
            + b"</target>"
        ),
        "core.xml": core,
        **({"extra.xml": extra} if extra else {}),
    }


def arm_core_xml(*, omit=None, bad_bits=None):
    registers = []
    for number, name in enumerate(ARMV7_EVENT_REGISTERS):
        if name == omit:
            continue
        bits = bad_bits if name == "cpsr" and bad_bits is not None else 32
        regnum = 25 if name == "cpsr" else number
        registers.append(
            f'<reg name="{name}" bitsize="{bits}" regnum="{regnum}"/>'
        )
    return ("<feature>" + "".join(registers) + "</feature>").encode()


def aarch64_core_xml():
    registers = []
    for number, name in enumerate(AARCH64_USER_REGISTERS):
        bits = 32 if name == "cpsr" else 64
        registers.append(
            f'<reg name="{name}" bitsize="{bits}" regnum="{number}"/>'
        )
    return ("<feature>" + "".join(registers) + "</feature>").encode()


def kernel_event(arch, pointer_bits, registers, *, byte_order="little", **changes):
    values = dict(
        arch=arch,
        byte_order=byte_order,
        pointer_bits=pointer_bits,
        kind=EVENT_USER_SIGNAL,
        signal=11,
        code=1,
        flags=EVENT_REGS_VALID | EVENT_REGS_USER,
        cpu=0,
        tgid=40,
        tid=41,
        sequence=1,
        task=0x1000,
        mm=0x2000,
        start_cookie=0x3000,
        signal_struct=0x4000,
        comm="routing-daemon",
        address=0x5000,
        registers=tuple(registers),
        register_valid_mask=(1 << len(registers)) - 1,
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


class Armv7EventRegisterTests(unittest.TestCase):
    def layout(self, *, byte_order="little", core=None, observed_bytes=None):
        extra = (
            b'<feature><reg name="d0" bitsize="64" regnum="26"/>'
            b'<reg name="fpscr" bitsize="32"/></feature>'
        )
        return Armv7PartialRegisterLayout.from_target_descriptions(
            target_descriptions("arm", core or arm_core_xml(), extra),
            byte_order=byte_order,
            observed_g_bytes=(
                17 * 4 + 8 + 4 if observed_bytes is None else observed_bytes
            ),
        )

    def test_native_frame_follows_sparse_qemu_xml_and_marks_fp_unavailable(self):
        registers = list(range(17))
        registers[0] = 0x11223344
        registers[15] = 0x89ABCDEF
        layout = self.layout()
        encoded = encode_armv7_event_registers(
            kernel_event(ARCH_ARM, 32, registers), layout
        )

        self.assertEqual(register_chunk(layout, encoded.payload, "r0"), b"44332211")
        self.assertEqual(register_chunk(layout, encoded.payload, "pc"), b"efcdab89")
        self.assertEqual(register_chunk(layout, encoded.payload, "cpsr"), b"10000000")
        self.assertEqual(register_chunk(layout, encoded.payload, "d0"), b"x" * 16)
        self.assertEqual(register_chunk(layout, encoded.payload, "fpscr"), b"x" * 8)
        self.assertEqual(len(encoded.payload), (17 * 4 + 8 + 4) * 2)

    def test_big_endian_and_validity_mask_are_preserved(self):
        registers = [0] * 17
        registers[0] = 0x11223344
        registers[15] = 0x55667788
        valid = ((1 << 17) - 1) & ~(1 << 15)
        layout = self.layout(byte_order="big")
        encoded = encode_armv7_event_registers(
            kernel_event(
                ARCH_ARM, 32, registers, byte_order="big",
                register_valid_mask=valid,
            ),
            layout,
        )

        self.assertEqual(register_chunk(layout, encoded.payload, "r0"), b"11223344")
        self.assertEqual(register_chunk(layout, encoded.payload, "pc"), b"x" * 8)

    def test_rejects_compat_wrong_metadata_shape_mask_and_width(self):
        good = kernel_event(ARCH_ARM, 32, [0] * 17)
        cases = (
            replace(good, flags=good.flags | EVENT_REGS_COMPAT),
            replace(good, arch=ARCH_AARCH64),
            replace(good, pointer_bits=64),
            replace(good, kind=99),
            replace(good, flags=0),
            replace(good, byte_order="big"),
            replace(good, registers=(0,) * 16, register_valid_mask=1),
            replace(good, register_valid_mask=1 << 17),
            replace(
                good,
                registers=((1 << 32),) + (0,) * 16,
                register_valid_mask=(1 << 17) - 1,
            ),
        )
        for event in cases:
            with self.subTest(event=event):
                with self.assertRaises(ArmEventPresentationError):
                    encode_armv7_event_registers(event, self.layout())

    def test_arm_xml_must_describe_the_native_core_exactly(self):
        with self.assertRaisesRegex(PartialRegisterError, "lacks required.*cpsr"):
            self.layout(
                core=arm_core_xml(omit="cpsr"), observed_bytes=16 * 4 + 8 + 4
            )
        with self.assertRaisesRegex(PartialRegisterError, "cpsr.*64 bits"):
            self.layout(core=arm_core_xml(bad_bits=64))


class Aarch64EventRegisterTests(unittest.TestCase):
    def layout(self, *, byte_order="little"):
        extra = (
            b'<feature><reg name="v0" bitsize="128" regnum="34"/>'
            b'<reg name="fpsr" bitsize="32"/></feature>'
        )
        return Aarch64PartialRegisterLayout.from_target_descriptions(
            target_descriptions("aarch64", aarch64_core_xml(), extra),
            byte_order=byte_order,
            observed_g_bytes=31 * 8 + 8 + 8 + 4 + 16 + 4,
        )

    def test_native_frame_maps_pstate_to_cpsr_and_marks_fp_unavailable(self):
        registers = [0x1000 + number for number in range(34)]
        registers[0] = 0x0102030405060708
        registers[31] = 0x1112131415161718
        registers[32] = 0x2122232425262728
        registers[33] = 0xA1B2C3D4
        layout = self.layout()
        encoded = encode_aarch64_event_registers(
            kernel_event(ARCH_AARCH64, 64, registers), layout
        )

        self.assertEqual(
            register_chunk(layout, encoded.payload, "x0"), b"0807060504030201"
        )
        self.assertEqual(
            register_chunk(layout, encoded.payload, "sp"), b"1817161514131211"
        )
        self.assertEqual(
            register_chunk(layout, encoded.payload, "pc"), b"2827262524232221"
        )
        self.assertEqual(
            register_chunk(layout, encoded.payload, "cpsr"), b"d4c3b2a1"
        )
        self.assertEqual(register_chunk(layout, encoded.payload, "v0"), b"x" * 32)
        self.assertEqual(register_chunk(layout, encoded.payload, "fpsr"), b"x" * 8)

    def test_big_endian_and_partial_validity_are_explicit(self):
        registers = [0] * 34
        registers[0] = 0x0102030405060708
        valid = ((1 << 34) - 1) & ~1
        layout = self.layout(byte_order="big")
        encoded = encode_aarch64_event_registers(
            kernel_event(
                ARCH_AARCH64, 64, registers, byte_order="big",
                register_valid_mask=valid,
            ),
            layout,
        )

        self.assertEqual(register_chunk(layout, encoded.payload, "x0"), b"x" * 16)
        self.assertEqual(register_chunk(layout, encoded.payload, "x1"), b"0" * 16)

    def test_rejects_aarch32_and_incompatible_native_frames(self):
        good = kernel_event(ARCH_AARCH64, 64, [0] * 34)
        cases = (
            replace(good, flags=good.flags | EVENT_REGS_COMPAT),
            replace(good, pointer_bits=32),
            replace(good, arch=ARCH_ARM),
            replace(good, registers=(0,) * 33, register_valid_mask=1),
            replace(good, register_valid_mask=0),
            replace(good, registers=(0,) * 33 + (1 << 32,)),
        )
        for event in cases:
            with self.subTest(event=event):
                with self.assertRaises(ArmEventPresentationError):
                    encode_aarch64_event_registers(event, self.layout())

    def test_layout_classes_cannot_be_mixed(self):
        arm_layout = Armv7EventRegisterTests().layout()
        aarch_layout = self.layout()
        with self.assertRaises(TypeError):
            encode_aarch64_event_registers(
                kernel_event(ARCH_AARCH64, 64, [0] * 34), arm_layout
            )
        with self.assertRaises(TypeError):
            encode_armv7_event_registers(
                kernel_event(ARCH_ARM, 32, [0] * 17), aarch_layout
            )


if __name__ == "__main__":
    unittest.main()
