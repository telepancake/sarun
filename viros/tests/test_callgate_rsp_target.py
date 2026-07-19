from __future__ import annotations

import hashlib
from dataclasses import replace
from pathlib import Path
import tempfile
import unittest

from callgate.architectures import (
    AARCH64,
    ARCHITECTURES,
    MIPS32EL_MMIPS,
    RegisterSpec,
    architecture_by_name,
)
from callgate.rsp_target import RspQemuTarget, RspTargetError
from callgate.transaction import AARCH64_REGISTERS


def core_xml(omit: str | None = None) -> bytes:
    registers = []
    for name in AARCH64_REGISTERS:
        if name == omit:
            continue
        bits = 32 if name == "cpsr" else 64
        registers.append(f'<reg name="{name}" bitsize="{bits}"/>')
    return ("<feature>" + "".join(registers) + "</feature>").encode()


class FakeQemuClient:
    timeout = 0.25

    def __init__(self):
        self.threads = ("1", "2")
        self.current = "1"
        self.continue_thread = "1"
        self.monitor_cpu = 0
        self.calls = []
        self.interrupts = 0
        self.physical = bytearray(0x3000)
        self.breakpoint = None
        self.stop_packets = [b"T05thread:2;"]
        self.xml = {
            "target.xml": (
                b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
                b"<architecture>aarch64</architecture>"
                b'<xi:include href="aarch64-core.xml"/></target>'
            ),
            "aarch64-core.xml": core_xml(),
        }
        self.registers = {}
        for thread_index, thread in enumerate(self.threads):
            for number, name in enumerate(AARCH64_REGISTERS):
                width = 4 if name == "cpsr" else 8
                value = thread_index * 0x1000 + number
                self.registers[(thread, number)] = value.to_bytes(width, "little")

    def request(self, payload):
        self.calls.append(("request", payload))
        if payload == b"?":
            return b"T05thread:1;"
        raise AssertionError(payload)

    def thread_ids(self):
        self.calls.append(("thread_ids",))
        return self.threads

    def current_thread(self):
        self.calls.append(("current_thread",))
        return self.current

    def select_thread(self, operation, thread):
        self.calls.append(("select", operation, thread))
        if operation == "g":
            self.current = thread
        else:
            self.continue_thread = thread

    def monitor_command(self, command):
        self.calls.append(("monitor", command))
        if command == "info cpus":
            return f"* CPU #{self.monitor_cpu}: thread_id=1\n"
        if command.startswith("cpu "):
            self.monitor_cpu = int(command.split()[1])
            return ""
        if command.startswith("gva2gpa "):
            address = int(command.split()[1], 0)
            return f"gpa: {address - 0xffff000000000000 + self.monitor_cpu * 0x1000:#x}\n"
        raise AssertionError(command)

    def read_physical(self, address, size):
        self.calls.append(("read_physical", address, size))
        return bytes(self.physical[address : address + size])

    def write_physical(self, address, data):
        self.calls.append(("write_physical", address, bytes(data)))
        self.physical[address : address + len(data)] = data

    def read_xfer(self, object_name, annex):
        self.calls.append(("xfer", object_name, annex))
        return self.xml[annex]

    def read_register(self, number):
        self.calls.append(("read_register", self.current, number))
        return self.registers[(self.current, number)]

    def write_register(self, number, value):
        self.calls.append(("write_register", self.current, number, bytes(value)))
        self.registers[(self.current, number)] = bytes(value)

    def insert_breakpoint(self, kind, address, size):
        self.calls.append(("insert_breakpoint", kind, address, size))
        self.breakpoint = (kind, address, size)

    def remove_breakpoint(self, kind, address, size):
        self.calls.append(("remove_breakpoint", kind, address, size))
        if self.breakpoint != (kind, address, size):
            raise AssertionError("breakpoint ownership mismatch")
        self.breakpoint = None

    def resume_thread(self, thread):
        self.calls.append(("resume_thread", thread, self.continue_thread))

    def require_vcont_action(self, action):
        self.calls.append(("require_vcont", action))

    def receive_async_packet(self, timeout):
        self.calls.append(("receive", timeout))
        if not self.stop_packets:
            raise AssertionError("no scripted stop")
        packet = self.stop_packets.pop(0)
        if isinstance(packet, BaseException):
            raise packet
        return packet

    def forward_interrupt(self):
        self.calls.append(("interrupt",))
        self.interrupts += 1


class RspQemuTargetTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.kernel = Path(self.temporary.name) / "vmlinux"
        self.kernel.write_bytes(b"exact test kernel")
        self.build_id = "12345678abcdef00"
        self.client = FakeQemuClient()
        self.target = RspQemuTarget(self.client, self.kernel, self.build_id)

    def tearDown(self):
        self.temporary.cleanup()

    def test_stopped_cpu_enumeration_and_exact_kernel_identity(self):
        self.target.assert_stopped()
        self.assertEqual(self.target.cpu_ids(), (0, 1))
        digest = hashlib.sha256(self.kernel.read_bytes()).hexdigest()
        self.target.verify_kernel(str(self.kernel), digest, self.build_id)
        with self.assertRaisesRegex(RspTargetError, "build ID"):
            self.target.verify_kernel(str(self.kernel), digest, "deadbeef")

    def test_mapping_uses_qrcmd_cpu_and_restores_hmp_selection(self):
        virtual = 0xFFFF000000001000
        self.target.verify_mapping(1, virtual, 0x2000)
        monitors = [call[1] for call in self.client.calls if call[0] == "monitor"]
        self.assertEqual(
            monitors,
            ["info cpus", "cpu 1", f"gva2gpa {virtual:#x}", "cpu 0"],
        )
        self.assertEqual(self.client.monitor_cpu, 0)

    def test_translate_virtual_returns_gpa_and_restores_hmp_selection(self):
        virtual = 0xFFFF000000001000
        self.client.monitor_cpu = 1
        self.assertEqual(self.target.translate_virtual(0, virtual), 0x1000)
        self.assertEqual(self.client.monitor_cpu, 1)

    def test_register_description_include_and_thread_selection_are_strict(self):
        original = self.client.current
        value = self.target.read_register(1, "pc")
        self.assertEqual(value, 0x1000 + AARCH64_REGISTERS.index("pc"))
        self.assertEqual(self.client.current, original)
        self.target.write_register(1, "x3", 0x8877665544332211)
        self.assertEqual(
            self.client.registers[("2", 3)],
            bytes.fromhex("1122334455667788"),
        )
        self.assertEqual(self.client.current, original)
        self.assertIn(("xfer", "features", "target.xml"), self.client.calls)
        self.assertIn(("xfer", "features", "aarch64-core.xml"), self.client.calls)

    def test_constructor_default_and_explicit_descriptor_are_compatible(self):
        self.assertIs(self.target.architecture, AARCH64)
        explicit = RspQemuTarget(
            self.client, self.kernel, self.build_id, AARCH64
        )
        self.assertIs(explicit.architecture, AARCH64)

    def test_target_name_and_byte_order_come_from_descriptor(self):
        descriptor = replace(
            AARCH64,
            display_name="Synthetic",
            target_byte_order="big",
            qemu_architecture_names=("synthetic",),
        )
        self.client.xml["target.xml"] = self.client.xml["target.xml"].replace(
            b"<architecture>aarch64</architecture>",
            b"<architecture>synthetic</architecture>",
        )
        target = RspQemuTarget(
            self.client, self.kernel, self.build_id, descriptor
        )
        pc_number = AARCH64_REGISTERS.index("pc")
        self.client.registers[("1", pc_number)] = (0x1020304050607080).to_bytes(
            8, "big"
        )
        self.assertEqual(target.read_register(0, "pc"), 0x1020304050607080)
        target.write_register(0, "x3", 0x1122334455667788)
        self.assertEqual(
            self.client.registers[("1", 3)], bytes.fromhex("1122334455667788")
        )

    def test_breakpoint_size_comes_from_descriptor(self):
        descriptor = replace(AARCH64, breakpoint_size=8)
        target = RspQemuTarget(
            self.client, self.kernel, self.build_id, descriptor
        )
        token = target.add_hardware_breakpoint(0xFFFF000000123000)
        self.assertEqual(token.size, 8)
        self.assertEqual(
            self.client.breakpoint, (1, 0xFFFF000000123000, 8)
        )
        target.remove_breakpoint(token)

    def test_register_description_rejects_missing_core_register(self):
        self.client.xml["aarch64-core.xml"] = core_xml(omit="pc")
        with self.assertRaisesRegex(RspTargetError, "lacks registers: pc"):
            self.target.read_register(0, "pc")

    def test_register_description_rejects_wrong_target_name_and_width(self):
        self.client.xml["target.xml"] = self.client.xml["target.xml"].replace(
            b"<architecture>aarch64</architecture>",
            b"<architecture>wrong</architecture>",
        )
        with self.assertRaisesRegex(RspTargetError, "expected AArch64"):
            self.target.read_register(0, "pc")

        self.target._register_map = None
        self.client.xml["target.xml"] = self.client.xml["target.xml"].replace(
            b"<architecture>wrong</architecture>",
            b"<architecture>aarch64</architecture>",
        )
        self.client.xml["aarch64-core.xml"] = core_xml().replace(
            b'<reg name="pc" bitsize="64"/>',
            b'<reg name="pc" bitsize="32"/>',
        )
        with self.assertRaisesRegex(
            RspTargetError, "describes pc as 32 bits, expected 64"
        ):
            self.target.read_register(0, "pc")

    def test_register_description_resolves_nested_relative_includes(self):
        self.client.xml["target.xml"] = (
            b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
            b"<architecture>aarch64</architecture>"
            b'<xi:include href="features/wrapper.xml"/></target>'
        )
        self.client.xml["features/wrapper.xml"] = (
            b'<feature xmlns:xi="http://www.w3.org/2001/XInclude">'
            b'<xi:include href="../aarch64-core.xml"/></feature>'
        )
        self.target.read_register(0, "pc")
        self.assertIn(("xfer", "features", "features/wrapper.xml"), self.client.calls)
        self.assertIn(("xfer", "features", "aarch64-core.xml"), self.client.calls)

    def test_register_description_rejects_include_escape(self):
        self.client.xml["target.xml"] = (
            b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
            b"<architecture>aarch64</architecture>"
            b'<xi:include href="../outside.xml"/></target>'
        )
        with self.assertRaisesRegex(RspTargetError, "unsafe.*include"):
            self.target.read_register(0, "pc")

    def test_physical_memory_and_breakpoint_delegate_with_ownership_token(self):
        self.target.write_physical(0x10, b"probe")
        self.assertEqual(self.target.read_physical(0x10, 5), b"probe")
        token = self.target.add_hardware_breakpoint(0xFFFF000000123000)
        self.target.remove_breakpoint(token)
        self.target.remove_breakpoint(token)
        removals = [call for call in self.client.calls if call[0] == "remove_breakpoint"]
        self.assertEqual(len(removals), 1)

    def test_resume_selects_one_cpu_checks_pc_and_restores_hg_selection(self):
        pc_number = AARCH64_REGISTERS.index("pc")
        address = 0xFFFF000000123000
        self.client.registers[("2", pc_number)] = address.to_bytes(8, "little")
        self.target.run_cpu_until(1, address, 2.0)
        self.assertIn(("resume_thread", "2", "1"), self.client.calls)
        self.assertEqual(self.client.current, "1")
        self.assertEqual(self.client.continue_thread, "1")
        self.assertFalse(any(call[:2] == ("select", "c") for call in self.client.calls))

    def test_resume_preserves_independent_hc_selection(self):
        pc_number = AARCH64_REGISTERS.index("pc")
        address = 0xFFFF000000123000
        self.client.continue_thread = "2"
        self.client.registers[("1", pc_number)] = address.to_bytes(8, "little")
        self.target.run_cpu_until(0, address, 2.0)
        self.assertIn(("resume_thread", "1", "2"), self.client.calls)
        self.assertEqual(self.client.current, "1")
        self.assertEqual(self.client.continue_thread, "2")
        self.assertFalse(any(call[:2] == ("select", "c") for call in self.client.calls))

    def test_resume_timeout_interrupts_waits_for_stop_and_restores_selection(self):
        self.client.stop_packets = [TimeoutError("late"), b"T02thread:2;"]
        with self.assertRaisesRegex(RspTargetError, "exceeded"):
            self.target.run_cpu_until(1, 0x1234, 0.01)
        self.assertEqual(self.client.interrupts, 1)
        self.assertEqual(self.client.current, "1")
        self.assertEqual(self.client.continue_thread, "1")

    def test_failed_interrupt_poisoning_prevents_unsynchronized_cleanup_packets(self):
        self.client.stop_packets = [TimeoutError("late"), TimeoutError("no stop")]
        with self.assertRaisesRegex(RspTargetError, "stop synchronization failed.*no stop"):
            self.target.run_cpu_until(1, 0x1234, 0.01)
        call_count = len(self.client.calls)
        with self.assertRaisesRegex(RspTargetError, "stop state is unknown"):
            self.target.write_register(1, "pc", 0x1234)
        self.assertEqual(len(self.client.calls), call_count)
        self.assertTrue(self.target.supports_bounded_resume)

    def test_keyboard_interrupt_is_synchronized_before_it_propagates(self):
        self.client.stop_packets = [KeyboardInterrupt(), b"T02thread:2;"]
        with self.assertRaises(KeyboardInterrupt):
            self.target.run_cpu_until(1, 0x1234, 1.0)
        self.assertEqual(self.client.interrupts, 1)
        self.assertEqual(self.client.current, "1")
        # A synchronized stop leaves the target safe for the outer
        # transaction's register/page restoration.
        self.target.write_register(1, "pc", 0x1234)


class MipsFixedRspTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.kernel = Path(self.temporary.name) / "vmlinux"
        self.kernel.write_bytes(b"experimental MMIPS backend fixture")
        self.client = FakeQemuClient()
        self.client.xml = {}
        self.client.registers = {
            (thread, number): (thread_index * 0x1000 + number).to_bytes(
                4, "little"
            )
            for thread_index, thread in enumerate(self.client.threads)
            for number in range(38)
        }
        self.target = RspQemuTarget(
            self.client,
            self.kernel,
            "12345678abcdef00",
            MIPS32EL_MMIPS,
        )

    def tearDown(self):
        self.temporary.cleanup()

    def test_descriptor_is_manifest_registered_for_mmips(self):
        self.assertIs(ARCHITECTURES["mmips"], MIPS32EL_MMIPS)
        self.assertIs(architecture_by_name("mmips"), MIPS32EL_MMIPS)
        self.assertEqual(len(MIPS32EL_MMIPS.core_registers), 38)
        self.assertTrue(all(
            register.bits == 32
            for register in MIPS32EL_MMIPS.core_registers
        ))
        self.assertEqual(
            tuple(register.rsp_number for register in MIPS32EL_MMIPS.core_registers),
            tuple(range(38)),
        )
        self.assertFalse(any(
            register.name.startswith("f")
            for register in MIPS32EL_MMIPS.core_registers
        ))

    def test_fixed_map_reads_and_writes_little_endian_p_packets_without_xml(self):
        self.assertEqual(self.target.read_register(1, "status"), 0x1020)
        self.assertIn(("read_register", "2", 32), self.client.calls)
        self.target.write_register(1, "r4", 0x88776655)
        self.assertEqual(self.client.registers[("2", 4)], b"\x55\x66\x77\x88")
        self.assertIn(
            ("write_register", "2", 4, b"\x55\x66\x77\x88"),
            self.client.calls,
        )
        self.assertFalse(any(call[0] == "xfer" for call in self.client.calls))

    def test_fixed_map_names_widths_and_regnums_are_exact(self):
        expected_names = tuple(
            [f"r{number}" for number in range(32)]
            + ["status", "lo", "hi", "badvaddr", "cause", "pc"]
        )
        self.assertEqual(MIPS32EL_MMIPS.register_names, expected_names)
        for number, name in enumerate(expected_names):
            register = self.target._register(name)
            self.assertEqual((register.number, register.bits), (number, 32))
        calls = len(self.client.calls)
        with self.assertRaisesRegex(
            RspTargetError, "unsupported MIPS32 little-endian MMIPS core register: f0"
        ):
            self.target.read_register(0, "f0")
        self.assertEqual(len(self.client.calls), calls)

    def test_descriptor_validates_fixed_register_maps(self):
        partial = (
            replace(AARCH64.core_registers[0], rsp_number=0),
            *AARCH64.core_registers[1:],
        )
        with self.assertRaisesRegex(ValueError, "cover every core register"):
            replace(AARCH64, core_registers=partial)

        duplicate = list(MIPS32EL_MMIPS.core_registers)
        duplicate[1] = RegisterSpec("r1", 32, 0)
        with self.assertRaisesRegex(ValueError, "numbers must be unique"):
            replace(MIPS32EL_MMIPS, core_registers=tuple(duplicate))

        negative = list(MIPS32EL_MMIPS.core_registers)
        negative[1] = RegisterSpec("r1", 32, -1)
        with self.assertRaisesRegex(ValueError, "nonnegative integers"):
            replace(MIPS32EL_MMIPS, core_registers=tuple(negative))

    def test_breakpoint_size_and_completion_pc_use_mips_descriptor(self):
        address = 0x80123000
        token = self.target.add_hardware_breakpoint(address)
        self.assertEqual(token.size, 4)
        self.assertEqual(self.client.breakpoint, (1, address, 4))
        self.target.remove_breakpoint(token)

        self.client.registers[("2", 37)] = address.to_bytes(4, "little")
        self.target.run_cpu_until(1, address, 2.0)
        self.assertIn(("read_register", "2", 37), self.client.calls)
        self.assertIn(("resume_thread", "2", "1"), self.client.calls)


if __name__ == "__main__":
    unittest.main()
