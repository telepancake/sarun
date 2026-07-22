import struct
import unittest

from inferiors.linux_oracle import RegisterRead, Snapshot, TaskId, TaskSnapshot
from inferiors.rsp_proxy import FacadeState, RspFacade


class FakeOracle:
    def __init__(self):
        self.tasks = (
            TaskSnapshot(TaskId(1, 1), 0x101, "init", "/sbin/init", b"AUX1", "sleeping"),
            TaskSnapshot(TaskId(42, 42), 0x202, "zebra", "/usr/sbin/zebra", b"AUX42", "running", 0),
            TaskSnapshot(TaskId(42, 43), 0x203, "zebra-w", "/usr/sbin/zebra", b"AUX43", "sleeping"),
        )
        self.memory = {
            (1, 0x400000): b"\x7fELF-init",
            (42, 0x400000): b"\x7fELF-zebr",
        }

    def snapshot(self):
        return Snapshot(1, self.tasks)

    def read_memory(self, task, address, length):
        return self.memory[(task.identity.tgid, address)][:length]

    def write_memory(self, task, address, data):
        self.memory[(task.identity.tgid, address)] = data

    def read_registers(self, task):
        return struct.pack("<II", task.identity.tgid, task.identity.tid)

    def write_registers(self, task, data):
        raise NotImplementedError


class FakeQemu:
    def __init__(self):
        self.inserted = []
        self.removed = []
        self.resumes = 0
        self.steps = []

    def insert_breakpoint(self, kind, address, size):
        self.inserted.append((kind, address, size))

    def remove_breakpoint(self, kind, address, size):
        self.removed.append((kind, address, size))

    def resume(self):
        self.resumes += 1

    def step(self, cpu):
        self.steps.append(cpu)


class FakeInternalContinue:
    def __init__(self, handled):
        self.handled = handled
        self.calls = 0

    def begin_continue(self):
        self.calls += 1
        return self.handled


class RspFacadeTests(unittest.TestCase):
    def setUp(self):
        self.oracle = FakeOracle()
        self.qemu = FakeQemu()
        self.facade = RspFacade(self.oracle, self.qemu, b"<target/>")

    def read_xfer(self, obj, annex="", chunk=11):
        data = bytearray()
        offset = 0
        while True:
            reply = self.facade.handle(
                f"qXfer:{obj}:read:{annex}:{offset:x},{chunk:x}".encode()
            )
            data.extend(reply[1:])
            if reply[:1] == b"l":
                return bytes(data)
            self.assertEqual(reply[:1], b"m")
            offset += len(reply) - 1

    def test_supported_and_thread_enumeration_use_multiprocess_ids(self):
        supported = self.facade.handle(b"qSupported:multiprocess+")
        self.assertIn(b"multiprocess+", supported)
        xml = self.read_xfer("threads")
        self.assertIn(b'id="p1.1"', xml)
        self.assertIn(b'id="p2a.2a"', xml)
        self.assertIn(b'id="p2a.2b"', xml)
        self.assertIn(b'name="zebra"', xml)
        self.assertEqual(self.facade.handle(b"qfThreadInfo"), b"mp1.1,p2a.2a,p2a.2b")

    def test_process_selection_controls_auxv_exec_memory_and_registers(self):
        self.assertEqual(self.facade.handle(b"Hgp2a.-1"), b"OK")
        self.assertEqual(self.read_xfer("auxv"), b"AUX42")
        self.assertEqual(self.read_xfer("exec-file", "2a"), b"/usr/sbin/zebra")
        self.assertEqual(self.facade.handle(b"m400000,8"), b"7f454c462d7a6562")
        self.assertEqual(self.facade.handle(b"g"), struct.pack("<II", 42, 42).hex().encode())
        self.assertEqual(self.facade.handle(b"qC"), b"QCp2a.2a")

    def test_breakpoints_are_global_downstream_but_owned_by_process(self):
        self.assertEqual(self.facade.handle(b"z1,401000,4"), b"OK")
        self.assertEqual(self.qemu.removed, [])
        self.assertEqual(self.facade.handle(b"Hgp1.1"), b"OK")
        self.assertEqual(self.facade.handle(b"Z1,401000,4"), b"OK")
        self.assertEqual(self.facade.handle(b"Hgp2a.2a"), b"OK")
        self.assertEqual(self.facade.handle(b"Z1,401000,4"), b"OK")
        self.assertEqual(self.qemu.inserted, [(1, 0x401000, 4)])

        self.assertEqual(self.facade.handle(b"Hgp1.1"), b"OK")
        self.assertEqual(self.facade.handle(b"z1,401000,4"), b"OK")
        self.assertEqual(self.qemu.removed, [])
        self.assertFalse(self.facade.owns_breakpoint(1, 0x401000))
        self.assertTrue(self.facade.owns_breakpoint(42, 0x401000))

        self.assertIsNone(self.facade.on_stop(TaskId(1, 1), address=0x401000))
        self.assertEqual(self.qemu.resumes, 1)
        stop = self.facade.on_stop(TaskId(42, 42), address=0x401000)
        self.assertEqual(stop, b"T05thread:p2a.2a;")
        self.assertEqual(self.facade.state, FacadeState.STOPPED)

        self.assertEqual(self.facade.handle(b"z1,401000,4"), b"OK")
        self.assertEqual(self.qemu.removed, [(1, 0x401000, 4)])

    def test_step_is_limited_to_a_current_task(self):
        self.assertIsNone(self.facade.handle(b"vCont;s:p2a.2a"))
        self.assertEqual(self.qemu.steps, [0])
        self.facade.state = FacadeState.STOPPED
        self.facade.handle(b"Hcp1.1")
        self.assertEqual(self.facade.handle(b"vCont;s:p1.1"), b"E01")

    def test_signal_stop_uses_event_registers_until_resume(self):
        event_registers = RegisterRead(b"11223344xxxxxxxx")
        stop = self.facade.on_stop(
            TaskId(42, 42), signal=11, registers=event_registers
        )
        self.assertEqual(stop, b"T0bthread:p2a.2a;")
        self.assertEqual(self.facade.handle(b"?"), b"T0bthread:p2a.2a;")
        self.assertEqual(self.facade.handle(b"g"), event_registers.payload)

        self.assertIsNone(self.facade.handle(b"c"))
        self.facade.state = FacadeState.STOPPED
        self.assertEqual(
            self.facade.handle(b"g"),
            struct.pack("<II", 42, 42).hex().encode(),
        )

    def test_internal_continue_step_replaces_ordinary_resume(self):
        controller = FakeInternalContinue(True)
        facade = RspFacade(
            self.oracle,
            self.qemu,
            b"<target/>",
            internal_continue=controller,
        )
        self.assertIsNone(facade.handle(b"c"))
        self.assertEqual(controller.calls, 1)
        self.assertEqual(self.qemu.resumes, 0)
        self.assertEqual(facade.state, FacadeState.RUNNING)

    def test_signal_continue_acknowledges_only_the_reported_linux_event(self):
        self.facade.on_stop(TaskId(42, 42), signal=11)
        self.assertEqual(self.facade.handle(b"C06"), b"E01")
        self.assertEqual(self.qemu.resumes, 0)
        self.assertIsNone(self.facade.handle(b"vCont;C0b:p2a.2a"))
        self.assertEqual(self.facade.continue_thread, TaskId(42, 42))
        self.assertEqual(self.qemu.resumes, 1)


if __name__ == "__main__":
    unittest.main()
