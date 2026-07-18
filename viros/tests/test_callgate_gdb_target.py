from __future__ import annotations

from pathlib import Path
import tempfile
import unittest

from callgate.gdb_target import (
    GdbQemuTarget,
    GdbTargetError,
    PhysicalModeRestorationError,
)
from callgate.transaction import AARCH64_REGISTERS


class MemoryView:
    def __init__(self, data):
        self.data = bytes(data)

    def tobytes(self):
        return self.data


class FakeThread:
    def __init__(self, owner, number):
        self.owner = owner
        self.num = number
        self.global_num = number
        self.running = False
        self.valid = True

    def switch(self):
        self.owner.selected = self

    def is_running(self):
        return self.running

    def is_valid(self):
        return self.valid


class FakeInferior:
    def __init__(self, owner):
        self.owner = owner
        self.data = bytearray(range(256)) * 64
        self.base = 0x40000000
        self.fail_read = False
        self.fail_write = False

    def threads(self):
        # Deliberately reverse order to test the stable CPU mapping.
        return tuple(reversed(self.owner.threads))

    def read_memory(self, address, size):
        if self.fail_read:
            raise RuntimeError("memory read failed")
        if not self.owner.physical_mode:
            raise RuntimeError("not in physical mode")
        offset = address - self.base
        return MemoryView(self.data[offset : offset + size])

    def write_memory(self, address, data, size):
        if self.fail_write:
            raise RuntimeError("memory write failed")
        if not self.owner.physical_mode:
            raise RuntimeError("not in physical mode")
        offset = address - self.base
        self.data[offset : offset + size] = data


class FakeBreakpoint:
    def __init__(self, owner, expression):
        self.owner = owner
        self.address = int(expression[1:], 0)
        self.valid = True
        self.delete_count = 0

    def is_valid(self):
        return self.valid

    def delete(self):
        self.delete_count += 1
        self.valid = False


class FakeProgspace:
    def __init__(self, filename):
        self.filename = filename


class FakeGdb:
    BP_HARDWARE_BREAKPOINT = 7

    def __init__(self, kernel):
        self.threads = [FakeThread(self, 1), FakeThread(self, 2)]
        self.selected = self.threads[0]
        self.inferior = FakeInferior(self)
        self.progspace = FakeProgspace(str(kernel))
        self.physical_mode = False
        self.fail_mode_restore = False
        self.scheduler_locking = "off"
        self.monitor_cpu = 0
        self.commands = []
        self.breakpoints = []
        self.sync_stop_address = None
        self.gpas = {
            (1, 0xFFFF000000100000): 0x40100000,
            (2, 0xFFFF000000100000): 0x50100000,
        }
        self.registers = {
            (thread.global_num, name): thread.global_num * 0x1000 + index
            for thread in self.threads
            for index, name in enumerate(AARCH64_REGISTERS)
        }

    def selected_inferior(self):
        return self.inferior

    def selected_thread(self):
        return self.selected

    def current_progspace(self):
        return self.progspace

    def parameter(self, name):
        assert name == "scheduler-locking"
        return self.scheduler_locking

    def parse_and_eval(self, expression):
        return self.registers[(self.selected.global_num, expression[1:])]

    def Breakpoint(self, expression, **kwargs):
        breakpoint = FakeBreakpoint(self, expression)
        self.breakpoints.append(breakpoint)
        return breakpoint

    def execute(self, command, from_tty=False, to_string=False):
        self.commands.append(command)
        if command == "maintenance packet qqemu.PhyMemMode":
            return f'sending query\nreceived: "{int(self.physical_mode)}"\n'
        if command.startswith("maintenance packet Qqemu.PhyMemMode:"):
            value = command.rsplit(":", 1)[1]
            if self.fail_mode_restore and value == "0" and self.physical_mode:
                return 'received: "E22"\n'
            self.physical_mode = value == "1"
            return 'received: "OK"\n'
        if command.startswith("monitor gva2gpa "):
            virtual = int(command.rsplit(" ", 1)[1], 0)
            key = (self.monitor_cpu + 1, virtual)
            if key not in self.gpas:
                return "Unmapped\n"
            return f"gpa: {self.gpas[key]:#x}\n"
        if command == "monitor info cpus":
            return f"* CPU #{self.monitor_cpu}: thread_id=1\n"
        if command.startswith("monitor cpu "):
            self.monitor_cpu = int(command.rsplit(" ", 1)[1], 0)
            return ""
        if command.startswith("set $"):
            left, right = command[5:].split(" = ", 1)
            self.registers[(self.selected.global_num, left)] = int(right, 0)
            return ""
        if command.startswith("set scheduler-locking "):
            self.scheduler_locking = command.rsplit(" ", 1)[1]
            return ""
        if command == "continue":
            self.selected.running = True
            if self.sync_stop_address is None:
                raise RuntimeError("synchronous continue has no configured stop")
            self.registers[(self.selected.global_num, "pc")] = self.sync_stop_address
            self.selected.running = False
            return ""
        if command == "interrupt":
            self.selected.running = False
            return ""
        raise AssertionError(f"unexpected GDB command: {command}")


class GdbTargetTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.kernel = Path(self.temporary.name) / "vmlinux"
        self.kernel.write_bytes(b"test kernel")
        self.gdb = FakeGdb(self.kernel)
        self.target = GdbQemuTarget(self.gdb)

    def tearDown(self):
        self.temporary.cleanup()

    def test_cpu_selection_is_stable_and_mapping_restores_thread(self):
        original = self.gdb.selected
        self.gdb.monitor_cpu = 1
        self.target.verify_mapping(1, 0xFFFF000000100000, 0x50100000)
        self.assertIs(self.gdb.selected, original)
        self.assertEqual(self.gdb.monitor_cpu, 1)
        self.assertEqual(self.target.cpu_ids(), (0, 1))
        self.assertIn("monitor cpu 1", self.gdb.commands)

    def test_translate_virtual_returns_gpa_and_restores_hmp_cpu(self):
        self.gdb.monitor_cpu = 1
        self.assertEqual(
            self.target.translate_virtual(0, 0xFFFF000000100000),
            0x40100000,
        )
        self.assertEqual(self.gdb.monitor_cpu, 1)

    def test_mapping_mismatch_is_rejected(self):
        with self.assertRaisesRegex(GdbTargetError, "mapping mismatch"):
            self.target.verify_mapping(0, 0xFFFF000000100000, 0xDEADBEEF)

    def test_physical_read_enters_and_restores_mode(self):
        result = self.target.read_physical(0x40000010, 4)
        self.assertEqual(result, bytes(range(16, 20)))
        self.assertFalse(self.gdb.physical_mode)
        self.assertEqual(
            [command for command in self.gdb.commands if "PhyMemMode" in command],
            [
                "maintenance packet qqemu.PhyMemMode",
                "maintenance packet Qqemu.PhyMemMode:1",
                "maintenance packet Qqemu.PhyMemMode:0",
            ],
        )

    def test_physical_write_restores_mode_after_operation_failure(self):
        self.gdb.inferior.fail_write = True
        with self.assertRaisesRegex(GdbTargetError, "memory write failed"):
            self.target.write_physical(0x40000000, b"abcd")
        self.assertFalse(self.gdb.physical_mode)

    def test_physical_mode_restore_failure_preserves_primary(self):
        self.gdb.inferior.fail_read = True
        self.gdb.fail_mode_restore = True
        with self.assertRaises(PhysicalModeRestorationError) as caught:
            self.target.read_physical(0x40000000, 4)
        self.assertIn("memory read failed", str(caught.exception.primary))
        self.assertIn("restore QEMU memory mode", str(caught.exception))

    def test_register_access_uses_selected_cpu_and_restores_thread(self):
        original = self.gdb.selected
        before = self.target.read_register(1, "x7")
        self.target.write_register(1, "x7", 0xCAFE)
        self.assertNotEqual(before, 0xCAFE)
        self.assertEqual(self.target.read_register(1, "x7"), 0xCAFE)
        self.assertIs(self.gdb.selected, original)

    def test_negative_gdb_register_value_is_normalized_to_uint64(self):
        self.gdb.registers[(1, "x8")] = -1
        self.assertEqual(self.target.read_register(0, "x8"), (1 << 64) - 1)

    def test_hardware_breakpoint_lifetime_is_owned_and_idempotent(self):
        token = self.target.add_hardware_breakpoint(0xFFFF000000100004)
        self.target.remove_breakpoint(token)
        self.target.remove_breakpoint(token)
        self.assertEqual(token.breakpoint.delete_count, 1)

    def test_synchronous_resume_stops_at_expected_breakpoint_and_restores_settings(self):
        completion = 0xFFFF000000100004
        other_before = dict(
            (key, value) for key, value in self.gdb.registers.items() if key[0] == 2
        )

        self.gdb.sync_stop_address = completion
        self.target.run_cpu_until(0, completion, 1.0)
        self.assertEqual(self.gdb.scheduler_locking, "off")
        self.assertFalse(any(thread.running for thread in self.gdb.threads))
        self.assertEqual(
            dict((key, value) for key, value in self.gdb.registers.items() if key[0] == 2),
            other_before,
        )
        self.assertIn("continue", self.gdb.commands)
        self.assertNotIn("continue&", self.gdb.commands)
        self.assertNotIn("interrupt", self.gdb.commands)

    def test_unexpected_stop_address_is_rejected(self):
        self.gdb.sync_stop_address = 0xFFFF000000100008
        with self.assertRaisesRegex(GdbTargetError, "stopped at"):
            self.target.run_cpu_until(0, 0xFFFF000000100004, 1.0)
        self.assertEqual(self.gdb.scheduler_locking, "off")


if __name__ == "__main__":
    unittest.main()
