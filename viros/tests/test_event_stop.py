import struct
import unittest

from callgate.architectures import MIPS32EL_MMIPS
from inferiors.event_stop import KernelEventReadError, read_kernel_event_stop
from inferiors.kernel_events import (
    ARCH_MIPS,
    ENDIAN_LITTLE,
    EVENT_ABI_MAJOR,
    EVENT_ABI_MINOR,
    EVENT_HEADER_BYTES,
    EVENT_MAGIC,
    EVENT_REGS_USER,
    EVENT_REGS_VALID,
    EVENT_USER_SIGNAL,
)
from inferiors.linux_oracle import RegisterRead, TaskId


HEADER_FORMAT = "I6HIIi5I8Q16s"


def record(*, cpu=0, size_adjustment=0):
    registers = tuple(range(38))
    size = EVENT_HEADER_BYTES + len(registers) * 8 + size_adjustment
    header = struct.pack(
        "<" + HEADER_FORMAT,
        EVENT_MAGIC,
        EVENT_ABI_MAJOR,
        EVENT_ABI_MINOR,
        ARCH_MIPS,
        ENDIAN_LITTLE,
        32,
        EVENT_USER_SIGNAL,
        size,
        11,
        1,
        EVENT_REGS_VALID | EVENT_REGS_USER,
        len(registers),
        cpu,
        42,
        43,
        7,
        0x81234000,
        0x82345000,
        0x12345678,
        0x83456000,
        0,
        (1 << len(registers)) - 1,
        0,
        b"test-process\0\0\0\0",
    )
    return header + struct.pack("<38Q", *registers)


class FakeQemu:
    def __init__(self, memory):
        self.memory = memory
        self.reads = []

    def read_virtual_memory(self, thread, address, length, *, address_bits):
        self.reads.append((thread, address, length, address_bits))
        return self.memory[address : address + length]


class FakeTarget:
    def __init__(self, address):
        self.address = address
        self.reads = []

    def read_register(self, cpu, name):
        self.reads.append((cpu, name))
        return self.address


class EventStopTests(unittest.TestCase):
    def test_reads_header_and_tail_then_presents_the_owning_task(self):
        address = 0x100
        payload = record()
        qemu = FakeQemu(b"\0" * address + payload)
        target = FakeTarget(address)
        seen = []

        def encode(event):
            seen.append(event)
            return RegisterRead(b"0011xxxx")

        stop = read_kernel_event_stop(
            qemu=qemu,
            target=target,
            cpu_threads=("1",),
            cpu=0,
            architecture=MIPS32EL_MMIPS,
            event_arch=ARCH_MIPS,
            event_register_count=38,
            encode_registers=encode,
            map_signal=lambda signal: signal,
        )
        self.assertEqual(stop.identity, TaskId(42, 43))
        self.assertEqual(stop.gdb_signal, 11)
        self.assertEqual(stop.record_address, address)
        self.assertEqual(stop.registers.payload, b"0011xxxx")
        task = stop.task_snapshot()
        self.assertEqual(task.identity, TaskId(42, 43))
        self.assertEqual(task.current_cpu, 0)
        self.assertEqual(task.comm, "test-process")
        self.assertEqual(target.reads, [(0, "r4")])
        self.assertEqual(
            qemu.reads,
            [
                ("1", address, EVENT_HEADER_BYTES, 32),
                ("1", address + EVENT_HEADER_BYTES, len(payload) - EVENT_HEADER_BYTES, 32),
            ],
        )
        self.assertEqual(seen[0].comm, "test-process")

    def test_rejects_cpu_mismatch_and_invalid_advertised_size(self):
        for payload in (record(cpu=1), record(size_adjustment=8)):
            with self.subTest(payload=len(payload)):
                address = 0x80
                with self.assertRaises(KernelEventReadError):
                    read_kernel_event_stop(
                        qemu=FakeQemu(b"\0" * address + payload),
                        target=FakeTarget(address),
                        cpu_threads=("1",),
                        cpu=0,
                        architecture=MIPS32EL_MMIPS,
                        event_arch=ARCH_MIPS,
                        event_register_count=38,
                        encode_registers=lambda event: RegisterRead(b"00"),
                        map_signal=lambda signal: signal,
                    )


if __name__ == "__main__":
    unittest.main()
