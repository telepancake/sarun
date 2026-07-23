from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest import mock

from callgate.architectures import AARCH64, ARMV7, MIPS32EL_MMIPS, X86_64
from callgate.rsp_target import RspHardwareBreakpoint
from inferiors.userspace_boundary import (
    UserspaceBoundaryError,
    UserspaceBoundaryPoint,
    UserspaceBoundaryRestorationError,
    reach_userspace_boundary,
    resolve_userspace_boundaries,
)


def symbol(name: str, value: int, *, shndx: int = 1) -> dict[str, int | str]:
    return {"name": name, "value": value, "shndx": shndx}


class FakeQemu:
    def __init__(self, packets=(), *, threads=("1", "2"), timeout=0.1):
        self.packets = list(packets)
        self.threads = tuple(threads)
        self.timeout = timeout
        self.current = self.threads[0]
        self.running = False
        self.calls = []
        self.receive_timeouts = []
        self.interrupts = 0
        self.resume_error: BaseException | None = None
        self.active = []
        self.fail_insert: set[int] = set()
        self.fail_remove: set[int] = set()

    def insert_breakpoint(self, kind, address, size):
        self.calls.append(("insert_breakpoint", kind, address, size))
        if self.running:
            raise AssertionError("breakpoint insert while QEMU is running")
        if address in self.fail_insert:
            raise RuntimeError(f"cannot insert {address:#x}")
        self.active.append((kind, address, size))

    def remove_breakpoint(self, kind, address, size):
        self.calls.append(("remove_breakpoint", kind, address, size))
        if self.running:
            raise AssertionError("breakpoint remove while QEMU is running")
        if address in self.fail_remove:
            raise RuntimeError(f"cannot remove {address:#x}")
        self.active.remove((kind, address, size))

    def resume(self):
        self.calls.append(("resume",))
        self.running = True
        if self.resume_error is not None:
            raise self.resume_error

    def receive_async_packet(self, timeout):
        self.calls.append(("receive", timeout))
        self.receive_timeouts.append(timeout)
        if not self.packets:
            raise AssertionError("no scripted QEMU packet")
        packet = self.packets.pop(0)
        if isinstance(packet, BaseException):
            raise packet
        if packet[:1] in {b"S", b"T"}:
            self.running = False
        return packet

    def forward_interrupt(self):
        self.calls.append(("interrupt",))
        self.interrupts += 1

    def current_thread(self):
        self.calls.append(("current_thread",))
        return self.current

    def thread_ids(self):
        self.calls.append(("thread_ids",))
        return self.threads


class FakeTarget:
    def __init__(self, architecture, qemu: FakeQemu, pcs):
        self.architecture = architecture
        self.client = qemu
        self.kernel_file = Path("/exact/vmlinux")
        self.pcs = tuple(pcs)
        self.active: list[RspHardwareBreakpoint] = []
        self.calls = []
        self.fail_insert: set[int] = set()
        self.fail_remove: set[int] = set()
        self._stop_synchronized = True

    def assert_stopped(self):
        self.calls.append(("assert_stopped",))
        if self.client.running:
            raise AssertionError("QEMU was not stopped")

    def cpu_ids(self):
        self.calls.append(("cpu_ids",))
        return tuple(range(len(self.client.threads)))

    def read_register(self, cpu, name):
        self.calls.append(("read_register", cpu, name))
        if self.client.running or not self._stop_synchronized:
            raise AssertionError("register read while QEMU is running")
        return self.pcs[cpu]

    def add_hardware_breakpoint(self, address):
        self.calls.append(("add", address))
        if self.client.running or not self._stop_synchronized:
            raise AssertionError("breakpoint insert while QEMU is running")
        if address in self.fail_insert:
            raise RuntimeError(f"cannot insert {address:#x}")
        token = RspHardwareBreakpoint(address, self.architecture.breakpoint_size)
        self.active.append(token)
        return token

    def remove_breakpoint(self, token):
        self.calls.append(("remove", token.address))
        if self.client.running or not self._stop_synchronized:
            raise AssertionError("breakpoint remove while QEMU is running")
        if token.address in self.fail_remove:
            raise RuntimeError(f"cannot remove {token.address:#x}")
        self.active.remove(token)
        token.removed = True


class SymbolResolutionTests(unittest.TestCase):
    def resolve(self, architecture, records):
        elf = SimpleNamespace(symbol_records=lambda: records)
        with mock.patch(
            "inferiors.userspace_boundary.ElfObject", return_value=elf
        ) as constructor:
            result = resolve_userspace_boundaries("/exact/vmlinux", architecture)
        constructor.assert_called_once_with(Path("/exact/vmlinux"))
        return result

    def test_fixed_arm_and_mips_profiles_use_only_the_exact_required_name(self):
        cases = (
            (AARCH64, "ret_to_user", 0xFFFF800080001000),
            (ARMV7, "ret_to_user", 0xC0001000),
            (MIPS32EL_MMIPS, "start_thread", 0x80101000),
        )
        for architecture, name, address in cases:
            with self.subTest(architecture=architecture.name):
                records = [
                    symbol(name + ".isra.0", address - 4),
                    symbol(name, address, shndx=0),
                    symbol(name, address),
                    symbol(name, address),
                ]
                self.assertEqual(
                    self.resolve(architecture, records),
                    (UserspaceBoundaryPoint(address, (name,)),),
                )

    def test_x86_installs_each_available_native_and_compat_boundary(self):
        native = 0xFFFFFFFF81001000
        compat = 0xFFFFFFFF81002000
        self.assertEqual(
            self.resolve(
                X86_64,
                [
                    symbol("start_thread", native),
                    symbol("compat_start_thread", compat),
                ],
            ),
            (
                UserspaceBoundaryPoint(native, ("start_thread",)),
                UserspaceBoundaryPoint(compat, ("compat_start_thread",)),
            ),
        )
        self.assertEqual(
            self.resolve(X86_64, [symbol("compat_start_thread", compat)]),
            (UserspaceBoundaryPoint(compat, ("compat_start_thread",)),),
        )

    def test_x86_aliases_at_one_address_create_one_owned_breakpoint(self):
        address = 0xFFFFFFFF81001000
        self.assertEqual(
            self.resolve(
                X86_64,
                [
                    symbol("start_thread", address),
                    symbol("compat_start_thread", address),
                ],
            ),
            (
                UserspaceBoundaryPoint(
                    address, ("start_thread", "compat_start_thread")
                ),
            ),
        )

    def test_missing_conflicting_and_unaligned_definitions_fail_closed(self):
        with self.assertRaisesRegex(UserspaceBoundaryError, "lacks required"):
            self.resolve(AARCH64, [symbol("ret_to_user.local", 0x1000)])
        with self.assertRaisesRegex(UserspaceBoundaryError, "conflicting"):
            self.resolve(
                MIPS32EL_MMIPS,
                [
                    symbol("start_thread", 0x80100000),
                    symbol("start_thread", 0x80101000),
                ],
            )
        with self.assertRaisesRegex(UserspaceBoundaryError, "unaligned"):
            self.resolve(ARMV7, [symbol("ret_to_user", 0xC0001002)])


class BoundaryAdvancementTests(unittest.TestCase):
    native = UserspaceBoundaryPoint(
        0xFFFFFFFF81001000, ("start_thread",)
    )
    compat = UserspaceBoundaryPoint(
        0xFFFFFFFF81002000, ("compat_start_thread",)
    )
    points = (native, compat)

    def advance(self, target, timeout=0.25, points=None):
        points = self.points if points is None else points
        with mock.patch(
            "inferiors.userspace_boundary.resolve_userspace_boundaries",
            return_value=points,
        ) as resolver:
            result = reach_userspace_boundary(target, timeout_seconds=timeout)
        resolver.assert_called_once_with(target.kernel_file, target.architecture)
        return result

    def test_console_packets_are_consumed_and_the_hit_boundary_is_verified(self):
        qemu = FakeQemu([b"O626f6f740a", b"O", b"T05thread:2;"])
        target = FakeTarget(X86_64, qemu, (0, self.compat.address))

        result = self.advance(target)

        self.assertEqual(result.point, self.compat)
        self.assertEqual((result.cpu, result.thread_id), (1, "2"))
        self.assertEqual(result.stop_reply, b"T05thread:2;")
        self.assertFalse(qemu.running)
        self.assertTrue(target._stop_synchronized)
        self.assertEqual(target.active, [])
        self.assertEqual(qemu.active, [])
        self.assertEqual(
            [call for call in qemu.calls if call[0] == "remove_breakpoint"],
            [
                ("remove_breakpoint", 0, self.compat.address, 1),
                ("remove_breakpoint", 0, self.native.address, 1),
            ],
        )
        self.assertTrue(all(0 < value <= 0.25 for value in qemu.receive_timeouts))

    def test_plain_stop_uses_qc_thread_and_still_checks_that_cpus_pc(self):
        qemu = FakeQemu([b"S05"])
        qemu.current = "2"
        target = FakeTarget(X86_64, qemu, (0, self.native.address))

        result = self.advance(target)

        self.assertEqual((result.cpu, result.thread_id), (1, "2"))
        self.assertIn(("current_thread",), qemu.calls)

    def test_non_x86_profiles_use_target_owned_hardware_breakpoints(self):
        point = UserspaceBoundaryPoint(0xFFFF800080001000, ("ret_to_user",))
        qemu = FakeQemu([b"T05thread:1;"])
        target = FakeTarget(AARCH64, qemu, (point.address, 0))

        result = self.advance(target, points=(point,))

        self.assertEqual(result.point, point)
        self.assertIn(("add", point.address), target.calls)
        self.assertIn(("remove", point.address), target.calls)
        self.assertFalse(
            any(call[0] == "insert_breakpoint" for call in qemu.calls)
        )
        self.assertEqual(target.active, [])
        self.assertTrue(target._stop_synchronized)

    def test_timeout_interrupts_to_a_bounded_stop_before_removing_breakpoints(self):
        qemu = FakeQemu(
            [TimeoutError("startup late"), b"O73746f7070696e670a", b"T02thread:1;"],
            timeout=0.05,
        )
        target = FakeTarget(X86_64, qemu, (0, 0))

        with self.assertRaisesRegex(
            UserspaceBoundaryError, "within 0.25s.*left stopped"
        ):
            self.advance(target)

        self.assertEqual(qemu.interrupts, 1)
        self.assertFalse(qemu.running)
        self.assertTrue(target._stop_synchronized)
        self.assertEqual(target.active, [])
        self.assertEqual(qemu.active, [])
        self.assertLessEqual(qemu.receive_timeouts[-1], 0.05)
        interrupt_index = qemu.calls.index(("interrupt",))
        first_remove = next(
            index
            for index, call in enumerate(qemu.calls)
            if call[0] == "remove_breakpoint"
        )
        self.assertGreater(first_remove, interrupt_index)

    def test_resume_transport_failure_is_also_resynchronized_and_cleaned(self):
        qemu = FakeQemu([b"T02thread:1;"])
        qemu.resume_error = ConnectionError("resume acknowledgement lost")
        target = FakeTarget(X86_64, qemu, (0, 0))

        with self.assertRaisesRegex(ConnectionError, "acknowledgement lost"):
            self.advance(target)

        self.assertEqual(qemu.interrupts, 1)
        self.assertFalse(qemu.running)
        self.assertTrue(target._stop_synchronized)
        self.assertEqual(target.active, [])
        self.assertEqual(qemu.active, [])

    def test_failed_restop_marks_target_unsynchronized_and_avoids_rsp_cleanup(self):
        qemu = FakeQemu([TimeoutError("late"), TimeoutError("no stop")])
        target = FakeTarget(X86_64, qemu, (0, 0))

        with self.assertRaises(UserspaceBoundaryRestorationError) as caught:
            self.advance(target)

        self.assertIsInstance(caught.exception.primary, TimeoutError)
        self.assertFalse(target._stop_synchronized)
        self.assertTrue(qemu.running)
        self.assertEqual(qemu.interrupts, 1)
        self.assertFalse(
            any(call[0] == "remove_breakpoint" for call in qemu.calls)
        )
        self.assertEqual(len(qemu.active), 2)

    def test_unexpected_stop_pc_fails_but_leaves_no_temporary_breakpoints(self):
        qemu = FakeQemu([b"T05thread:1;"])
        target = FakeTarget(X86_64, qemu, (0xFFFFFFFF81234567, 0))

        with self.assertRaisesRegex(UserspaceBoundaryError, "not an owned"):
            self.advance(target)

        self.assertFalse(qemu.running)
        self.assertEqual(target.active, [])
        self.assertEqual(qemu.active, [])

    def test_non_trap_stop_at_boundary_is_not_claimed(self):
        qemu = FakeQemu([b"T02thread:1;"])
        target = FakeTarget(X86_64, qemu, (self.native.address, 0))

        with self.assertRaisesRegex(UserspaceBoundaryError, "not an owned.*trap"):
            self.advance(target)

        self.assertFalse(qemu.running)
        self.assertEqual(target.active, [])
        self.assertEqual(qemu.active, [])

    def test_insert_failure_removes_every_breakpoint_already_installed(self):
        qemu = FakeQemu()
        target = FakeTarget(X86_64, qemu, (0, 0))
        qemu.fail_insert.add(self.compat.address)

        with self.assertRaisesRegex(RuntimeError, "cannot insert"):
            self.advance(target)

        self.assertNotIn(("resume",), qemu.calls)
        self.assertEqual(target.active, [])
        self.assertEqual(qemu.active, [])
        self.assertIn(
            ("remove_breakpoint", 0, self.native.address, 1), qemu.calls
        )

    def test_cleanup_attempts_every_removal_and_reports_all_failures(self):
        qemu = FakeQemu([b"T05thread:1;"])
        target = FakeTarget(X86_64, qemu, (self.native.address, 0))
        qemu.fail_remove = {self.native.address, self.compat.address}

        with self.assertRaises(UserspaceBoundaryRestorationError) as caught:
            self.advance(target)

        self.assertIsNone(caught.exception.primary)
        self.assertEqual(len(caught.exception.cleanup_errors), 2)
        self.assertEqual(
            [call for call in qemu.calls if call[0] == "remove_breakpoint"],
            [
                ("remove_breakpoint", 0, self.compat.address, 1),
                ("remove_breakpoint", 0, self.native.address, 1),
            ],
        )
        self.assertFalse(qemu.running)
        self.assertTrue(target._stop_synchronized)

    def test_interruption_is_resynchronized_then_propagated(self):
        qemu = FakeQemu([KeyboardInterrupt(), b"T02thread:1;"])
        target = FakeTarget(X86_64, qemu, (0, 0))

        with self.assertRaises(KeyboardInterrupt):
            self.advance(target)

        self.assertEqual(qemu.interrupts, 1)
        self.assertFalse(qemu.running)
        self.assertTrue(target._stop_synchronized)
        self.assertEqual(target.active, [])
        self.assertEqual(qemu.active, [])

    def test_invalid_timeout_does_not_touch_target(self):
        qemu = FakeQemu()
        target = FakeTarget(X86_64, qemu, (0, 0))
        for value in (0, -1, True, float("nan"), float("inf")):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    reach_userspace_boundary(target, timeout_seconds=value)
        self.assertEqual(target.calls, [])


if __name__ == "__main__":
    unittest.main()
