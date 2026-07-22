import unittest

from inferiors.gdb_signals import (
    GDB_SIGNAL_UNKNOWN,
    mips_linux_signal_to_gdb,
)
from inferiors.kernel_events import (
    ARCH_MIPS,
    EVENT_REGS_USER,
    EVENT_REGS_VALID,
    EVENT_USER_SIGNAL,
    KernelEvent,
)
from inferiors.mips_events import (
    MIPS_EVENT_REGISTER_COUNT,
    MIPS_LEGACY_G_REGISTER_COUNT,
    MipsEventPresentationError,
    encode_mips_event_registers,
)


def mips_event(*, byte_order="little", registers=None, valid_mask=None, **changes):
    if registers is None:
        registers = tuple([0] + [0x10000000 + index for index in range(1, 38)])
    if valid_mask is None:
        valid_mask = (1 << len(registers)) - 1
    values = dict(
        arch=ARCH_MIPS,
        byte_order=byte_order,
        pointer_bits=32,
        kind=EVENT_USER_SIGNAL,
        signal=11,
        code=1,
        flags=EVENT_REGS_VALID | EVENT_REGS_USER,
        cpu=0,
        tgid=42,
        tid=43,
        sequence=7,
        task=0x81234000,
        mm=0x82345000,
        start_cookie=0x12345678,
        signal_struct=0x83456000,
        comm="test-process",
        address=0x1234,
        registers=tuple(registers),
        register_valid_mask=valid_mask,
    )
    values.update(changes)
    return KernelEvent(**values)


def register_field(payload, number):
    start = number * 8
    return payload[start : start + 8]


class MipsSignalMappingTests(unittest.TestCase):
    def test_common_mips_signals_match_gdb_values(self):
        for signal in range(1, 16):
            with self.subTest(signal=signal):
                self.assertEqual(mips_linux_signal_to_gdb(signal), signal)

    def test_every_non_realtime_mips_remapping(self):
        remapped = {
            16: 30,
            17: 31,
            18: 20,
            19: 32,
            20: 28,
            21: 16,
            22: 23,
            23: 17,
            24: 18,
            25: 19,
            26: 21,
            27: 22,
            28: 26,
            29: 27,
            30: 24,
            31: 25,
        }
        for target_signal, gdb_signal in remapped.items():
            with self.subTest(target_signal=target_signal):
                self.assertEqual(
                    mips_linux_signal_to_gdb(target_signal), gdb_signal
                )

    def test_complete_mips_realtime_ranges_follow_gdbs_split_enum(self):
        expected = {32: 77}
        expected.update({signal: signal + 12 for signal in range(33, 64)})
        expected.update({signal: signal + 14 for signal in range(64, 128)})
        for target_signal, gdb_signal in expected.items():
            with self.subTest(target_signal=target_signal):
                self.assertEqual(
                    mips_linux_signal_to_gdb(target_signal), gdb_signal
                )

    def test_unknown_and_non_integer_signals_are_not_passed_through(self):
        self.assertEqual(mips_linux_signal_to_gdb(128), GDB_SIGNAL_UNKNOWN)
        self.assertEqual(mips_linux_signal_to_gdb(-1), GDB_SIGNAL_UNKNOWN)
        with self.assertRaises(TypeError):
            mips_linux_signal_to_gdb(True)


class MipsEventRegisterTests(unittest.TestCase):
    def test_little_endian_frame_uses_legacy_layout_and_honest_unknowns(self):
        registers = [0] + [index for index in range(1, 38)]
        registers[1] = 0x11223344
        registers[37] = 0x89ABCDEF
        valid = (1 << MIPS_EVENT_REGISTER_COUNT) - 1
        valid &= ~(1 << 26)
        valid &= ~(1 << 27)
        encoded = encode_mips_event_registers(mips_event(
            registers=registers, valid_mask=valid
        ))

        self.assertEqual(len(encoded.payload), MIPS_LEGACY_G_REGISTER_COUNT * 8)
        self.assertEqual(register_field(encoded.payload, 0), b"00000000")
        self.assertEqual(register_field(encoded.payload, 1), b"44332211")
        self.assertEqual(register_field(encoded.payload, 26), b"xxxxxxxx")
        self.assertEqual(register_field(encoded.payload, 27), b"xxxxxxxx")
        self.assertEqual(register_field(encoded.payload, 37), b"efcdab89")
        for number in range(38, 73):
            self.assertEqual(register_field(encoded.payload, number), b"xxxxxxxx")

    def test_big_endian_frame_preserves_target_byte_order(self):
        registers = [0] * MIPS_EVENT_REGISTER_COUNT
        registers[4] = 0x11223344
        registers[32] = 0x55667788
        registers[37] = 0x89ABCDEF
        encoded = encode_mips_event_registers(mips_event(
            byte_order="big", registers=registers
        ))

        self.assertEqual(register_field(encoded.payload, 4), b"11223344")
        self.assertEqual(register_field(encoded.payload, 32), b"55667788")
        self.assertEqual(register_field(encoded.payload, 37), b"89abcdef")

    def test_rejects_wrong_shape_width_mask_and_r0(self):
        cases = [
            mips_event(pointer_bits=64),
            mips_event(registers=(0,) * 37),
            mips_event(valid_mask=1 << 38),
            mips_event(registers=(1,) + (0,) * 37),
            mips_event(registers=(0, 1 << 32) + (0,) * 36),
        ]
        for event in cases:
            with self.subTest(event=event):
                with self.assertRaises(MipsEventPresentationError):
                    encode_mips_event_registers(event)


if __name__ == "__main__":
    unittest.main()
