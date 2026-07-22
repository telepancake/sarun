import struct
import unittest

from inferiors.kernel_events import (
    ARCH_ARM,
    ARCH_MIPS,
    ENDIAN_BIG,
    ENDIAN_LITTLE,
    EVENT_ABI_MAJOR,
    EVENT_ABI_MINOR,
    EVENT_ADDRESS_VALID,
    EVENT_HEADER_BYTES,
    EVENT_KERNEL_DIE,
    EVENT_MAGIC,
    EVENT_REGS_COMPAT,
    EVENT_REGS_USER,
    EVENT_REGS_VALID,
    EVENT_USER_SIGNAL,
    KernelEventDecodeError,
    decode_kernel_event,
)


HEADER_FORMAT = "I6HIIi5I8Q16s"


def event_record(
    *,
    endian="<",
    arch=ARCH_ARM,
    endian_code=ENDIAN_LITTLE,
    pointer_bits=32,
    signal=11,
    kind=EVENT_USER_SIGNAL,
    code=1,
    flags=EVENT_REGS_VALID | EVENT_REGS_USER | EVENT_ADDRESS_VALID,
    cpu=1,
    tgid=42,
    tid=43,
    sequence=7,
    task=0x81234000,
    mm=0x82345000,
    start_cookie=0x12345678,
    signal_struct=0x83456000,
    comm=b"test-process\0\0\0\0",
    address=0x1234,
    registers=(1, 2, 3),
    register_valid_mask=None,
):
    if register_valid_mask is None:
        register_valid_mask = (1 << len(registers)) - 1
    size = EVENT_HEADER_BYTES + len(registers) * 8
    header = struct.pack(
        endian + HEADER_FORMAT,
        EVENT_MAGIC,
        EVENT_ABI_MAJOR,
        EVENT_ABI_MINOR,
        arch,
        endian_code,
        pointer_bits,
        kind,
        size,
        signal,
        code,
        flags,
        len(registers),
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
        0,
        comm,
    )
    return header + struct.pack(endian + f"{len(registers)}Q", *registers)


class KernelEventTests(unittest.TestCase):
    def test_decodes_little_endian_arm_signal_and_compat_frame(self):
        record = event_record(flags=(
            EVENT_REGS_VALID | EVENT_REGS_USER | EVENT_REGS_COMPAT
        ), address=0)
        event = decode_kernel_event(
            record,
            byte_order="little",
            expected_arch=ARCH_ARM,
            expected_pointer_bits=32,
            expected_registers=3,
        )
        self.assertEqual((event.tgid, event.tid, event.signal), (42, 43, 11))
        self.assertEqual((event.cpu, event.comm), (1, "test-process"))
        self.assertEqual(event.registers, (1, 2, 3))
        self.assertTrue(event.register_available(2))
        self.assertTrue(event.compat)
        self.assertIsNone(event.address)

    def test_decodes_big_endian_mips_abort(self):
        record = event_record(
            endian=">",
            arch=ARCH_MIPS,
            endian_code=ENDIAN_BIG,
            signal=6,
            code=-6,
            registers=(0, 0x11223344, 0x88776655),
        )
        event = decode_kernel_event(
            record,
            byte_order="big",
            expected_arch=ARCH_MIPS,
            expected_pointer_bits=32,
            expected_registers=3,
        )
        self.assertEqual((event.signal, event.code), (6, -6))
        self.assertEqual(event.address, 0x1234)
        self.assertEqual(event.registers[-1], 0x88776655)

    def test_decodes_kernel_die_without_userspace_address_space(self):
        record = event_record(
            kind=EVENT_KERNEL_DIE,
            flags=EVENT_REGS_VALID | EVENT_ADDRESS_VALID,
            mm=0,
        )
        event = decode_kernel_event(
            record,
            byte_order="little",
            expected_arch=ARCH_ARM,
            expected_pointer_bits=32,
            expected_registers=3,
        )
        self.assertEqual(event.kind, EVENT_KERNEL_DIE)
        self.assertEqual(event.mm, 0)
        self.assertEqual(event.address, 0x1234)

    def test_rejects_user_flag_on_kernel_die(self):
        with self.assertRaises(KernelEventDecodeError):
            decode_kernel_event(
                event_record(kind=EVENT_KERNEL_DIE),
                byte_order="little",
                expected_arch=ARCH_ARM,
                expected_pointer_bits=32,
                expected_registers=3,
            )

    def test_rejects_metadata_size_identity_and_width_mismatches(self):
        cases = [
            (event_record(arch=ARCH_MIPS), {}),
            (event_record(pointer_bits=64), {}),
            (event_record(tgid=0), {}),
            (event_record(start_cookie=0), {}),
            (event_record(comm=b"bad\x01name\0\0\0\0\0\0\0\0"), {}),
            (event_record(registers=(1 << 32,)), {"expected_registers": 1}),
            (event_record(register_valid_mask=0), {}),
            (event_record(register_valid_mask=1 << 3), {}),
            (event_record()[:-1], {}),
            (event_record(flags=EVENT_REGS_VALID | EVENT_REGS_USER, address=1), {}),
        ]
        for record, options in cases:
            with self.subTest(options=options, size=len(record)):
                with self.assertRaises(KernelEventDecodeError):
                    decode_kernel_event(
                        record,
                        byte_order="little",
                        expected_arch=ARCH_ARM,
                        expected_pointer_bits=32,
                        expected_registers=options.get("expected_registers", 3),
                    )

    def test_rejects_big_endian_record_as_little_endian(self):
        record = event_record(
            endian=">", arch=ARCH_MIPS, endian_code=ENDIAN_BIG
        )
        with self.assertRaises(KernelEventDecodeError):
            decode_kernel_event(
                record,
                byte_order="little",
                expected_arch=ARCH_MIPS,
                expected_pointer_bits=32,
                expected_registers=3,
            )


if __name__ == "__main__":
    unittest.main()
