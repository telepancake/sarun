from __future__ import annotations

import hashlib
import json
from pathlib import Path
import struct
import tempfile
import threading
import unittest

from inferiors.live_facade import (
    CurrentCpuProbeOracle,
    build_live_facade,
    load_target_descriptions,
)
from inferiors.linux_oracle import TaskId
from inferiors.partial_registers import (
    AARCH64_USER_REGISTERS,
    PartialRegisterError,
)
from inferiors.probe_oracle import ProbeOracle
from inferiors.qemu_rsp import QemuRspClient
from inferiors.rsp_proxy import RspFacade
from inferiors.rsp_transport import RspStream
from inferiors.rsp_server import qemu_cpu_stop_resolver
from probe.abi import ProbeSavedRegisters
from test_rsp_support import memory_duplex_pair


HEADER = "IHHHHHBBiIIIQQIIQ"
TASK = "HHIQQQQQQQQIIIIIHH16s10Q"
REQUEST = "IHHHHIQQIIQQQ"
ROOT = 0xFFFF800081234000


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _record(*, pid: int, cpu: int, current: bool) -> bytes:
    task = ROOT if pid == 1 else ROOT + pid * 0x1000
    flags = 1 | 2 | (4 if current else 0)
    return struct.pack(
        "<" + TASK,
        192, 1, flags, task, task, ROOT, ROOT + 0x500000,
        ROOT + 0x600000, pid, 0, 0, pid, pid, 0, cpu, 0, 64, 0,
        f"task{pid}".encode().ljust(16, b"\0"), *([0] * 10),
    )


def _response() -> bytes:
    records = _record(pid=1, cpu=0, current=False) + _record(
        pid=42, cpu=1, current=True
    )
    return struct.pack(
        "<" + HEADER,
        0x56505253, 1, 0, 64, 192, 1, 1, 64, 0, 0, 2,
        64 + len(records), 0, ROOT, 12, 0, 0,
    ) + records


def _manifest(directory: Path) -> Path:
    kernel = directory / "vmlinux"
    probe = directory / "probe.bin"
    kernel.write_bytes(b"exact live-facade test kernel")
    probe.write_bytes(bytes.fromhex("1f2003d500ca2ad4"))
    request = struct.pack(
        "<" + REQUEST,
        0x56505251, 1, 0, 64, 1, 0, ROOT, 0, 2, 0, 0, 0, 0,
    )
    document = {
        "format": "viros-callgate-v1",
        "architecture": "aarch64",
        "allow_transient_guest_modification": True,
        "kernel": {
            "vmlinux": kernel.name,
            "sha256": _sha256(kernel),
            "build_id": "0123456789abcdef",
        },
        "regions": [
            {"name": "code", "role": "code", "virtual_address": "0xffff800080100000",
             "physical_address": "0x40100000", "size": 4096},
            {"name": "data", "role": "data", "virtual_address": "0xffff800082000000",
             "physical_address": "0x42000000", "size": 4096},
            {"name": "stack", "role": "stack", "virtual_address": "0xffff800082001000",
             "physical_address": "0x42001000", "size": 4096},
        ],
        "probe": {"binary": probe.name, "sha256": _sha256(probe),
                  "code_region": "code", "entry_offset": 0, "completion_offset": 4},
        "mailbox": {"data_region": "data", "request_offset": 0,
                    "request_hex": request.hex(), "result_offset": 64,
                    "result_size": 64 + 2 * 192,
                    "completion_magic_hex": "53525056"},
        "invocation": {"cpu": 0, "pstate": "0x3c5", "stack_region": "stack",
                       "stack_pointer": "0xffff800082002000", "timeout_seconds": 1},
    }
    path = directory / "callgate.json"
    path.write_text(json.dumps(document), encoding="utf-8")
    return path


class FakeClient:
    def __init__(self):
        self.closed = False
        self.register_reads = []
        self.breakpoints = []
        self.resumes = 0
        self.register_block = bytes.fromhex("01020304")
        self.xml = {
            "target.xml": (
                b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
                b"<architecture>aarch64</architecture>"
                b'<xi:include href="core.xml"/></target>'
            ),
            "core.xml": b'<feature><reg name="x0" bitsize="64"/></feature>',
        }

    def request(self, payload):
        if payload == b"?":
            return b"T05thread:2;"
        raise AssertionError(payload)

    def read_xfer(self, object_name, annex):
        self.assert_equal(object_name, "features")
        return self.xml[annex]

    @staticmethod
    def assert_equal(left, right):
        if left != right:
            raise AssertionError((left, right))

    def thread_ids(self):
        return ("1", "2")

    def read_register_block(self, thread):
        self.register_reads.append(thread)
        return self.register_block

    def close(self):
        self.closed = True

    # QemuBackend methods are not reached by this stopped packet test.
    def insert_breakpoint(self, kind, address, size):
        self.breakpoints.append((kind, address, size))

    def remove_breakpoint(self, kind, address, size):
        self.breakpoints.remove((kind, address, size))

    def resume(self):
        self.resumes += 1

    def step(self, cpu):
        raise AssertionError("unexpected step")


class FrozenRunner:
    def __call__(self, cursor):
        if cursor != 0:
            raise AssertionError(cursor)
        return _response()


class MappingRunner:
    def __call__(self, cursor):
        if cursor != 0:
            raise AssertionError(cursor)
        response = bytearray(_response())
        response[6:8] = (1).to_bytes(2, "little")
        return bytes(response)


class FakeProgramMemoryReader:
    def __init__(self):
        self.bound = ()
        self.reads = []

    def bind_snapshot(self, snapshot):
        self.bound = tuple(task.stable_cookie for task in snapshot.tasks)

    def read_memory(self, task, address, length):
        self.reads.append((task.identity, task.task_cookie, address, length))
        if task.task_cookie not in self.bound:
            raise OSError("stale task")
        return bytes((address + offset) & 0xff for offset in range(length))


class FakeSavedRegisterReader:
    def __init__(self):
        self.bound = ()
        self.reads = []

    def bind_snapshot(self, snapshot):
        self.bound = tuple(task.stable_cookie for task in snapshot.tasks)

    def read_registers(self, task):
        self.reads.append((task.identity, task.task_cookie))
        if task.task_cookie not in self.bound:
            raise OSError("stale task")
        return ProbeSavedRegisters(
            task=task.task_cookie >> 64,
            mm=ROOT + 0x500000,
            start_cookie=task.task_cookie & ((1 << 64) - 1),
            x=tuple(range(31)),
            sp=0x1020304050607080,
            pc=0x405060,
            pstate=0,
            flags=7,
        )


def _full_core_xml(extra: bytes = b"") -> bytes:
    registers = []
    for regnum, name in enumerate(AARCH64_USER_REGISTERS):
        bits = 32 if name == "cpsr" else 64
        registers.append(
            f'<reg name="{name}" bitsize="{bits}" regnum="{regnum}"/>'
        )
    return ("<feature>" + "".join(registers)).encode() + extra + b"</feature>"


class FakePcTarget:
    def __init__(self, pc):
        self.pc = pc
        self.reads = []

    def read_register(self, cpu, name):
        self.reads.append((cpu, name))
        return self.pc


class LiveFacadeTests(unittest.TestCase):
    def test_recursive_descriptions_are_preserved_for_upstream_gdb(self):
        client = FakeClient()
        descriptions = load_target_descriptions(client)
        self.assertEqual(set(descriptions), {b"target.xml", b"core.xml"})

    def test_current_cpu_registers_only_and_process_memory_errors(self):
        client = FakeClient()
        oracle = CurrentCpuProbeOracle(
            ProbeOracle(FrozenRunner()), client, ("1", "2")
        )
        tasks = oracle.snapshot().tasks
        sleeping, current = tasks
        with self.assertRaisesRegex(OSError, "sleeping-task"):
            oracle.read_registers(sleeping)
        self.assertEqual(oracle.read_registers(current).payload, b"01020304")
        self.assertEqual(client.register_reads, ["2"])
        with self.assertRaisesRegex(OSError, "virtual memory"):
            oracle.read_memory(current, 0x400000, 4)

    def test_interrupt_stop_is_reported_without_breakpoint_filtering(self):
        client = FakeClient()
        oracle = CurrentCpuProbeOracle(
            ProbeOracle(FrozenRunner()), client, ("1", "2")
        )
        target = FakePcTarget(0x401000)
        facade = RspFacade(
            oracle, client, client.xml["target.xml"],
            target_descriptions={
                name.encode(): document for name, document in client.xml.items()
            },
        )
        # Even with a breakpoint at the current PC, SIGINT is a user stop and
        # must reach GDB rather than entering false-process-hit filtering.
        self.assertEqual(facade.handle(b"Z0,401000,4"), b"OK")

        identity, signal, address = qemu_cpu_stop_resolver(
            client, target, ("1", "2")
        )(b"T02thread:2;", facade)
        self.assertEqual(identity, TaskId(42, 42))
        self.assertEqual((signal, address), (2, None))
        self.assertEqual(target.reads, [])
        self.assertEqual(
            facade.on_stop(identity, signal, address, refresh=False),
            b"T02thread:p2a.2a;",
        )
        self.assertEqual(client.resumes, 0)

    def test_owned_breakpoint_hit_uses_stopped_cpu_pc(self):
        client = FakeClient()
        oracle = CurrentCpuProbeOracle(
            ProbeOracle(FrozenRunner()), client, ("1", "2")
        )
        target = FakePcTarget(0x401000)
        facade = RspFacade(
            oracle, client, client.xml["target.xml"],
            target_descriptions={
                name.encode(): document for name, document in client.xml.items()
            },
        )
        self.assertEqual(facade.handle(b"Z0,401000,4"), b"OK")

        identity, signal, address = qemu_cpu_stop_resolver(
            client, target, ("1", "2")
        )(b"T05thread:2;", facade)
        self.assertEqual((identity, signal, address), (TaskId(42, 42), 5, 0x401000))
        self.assertEqual(target.reads, [(1, "pc")])
        self.assertEqual(
            facade.on_stop(identity, signal, address, refresh=False),
            b"T05thread:p2a.2a;",
        )
        self.assertEqual(client.resumes, 0)

    def test_same_va_unowned_process_hit_is_auto_resumed(self):
        client = FakeClient()
        oracle = CurrentCpuProbeOracle(
            ProbeOracle(FrozenRunner()), client, ("1", "2")
        )
        target = FakePcTarget(0x401000)
        facade = RspFacade(
            oracle, client, client.xml["target.xml"],
            target_descriptions={
                name.encode(): document for name, document in client.xml.items()
            },
        )
        # PID 1 owns the breakpoint, but PID 42 is current on stopped CPU 1.
        self.assertEqual(facade.handle(b"Hgp1.1"), b"OK")
        self.assertEqual(facade.handle(b"Z0,401000,4"), b"OK")

        identity, signal, address = qemu_cpu_stop_resolver(
            client, target, ("1", "2")
        )(b"T05thread:2;", facade)
        self.assertEqual((identity, signal, address), (TaskId(42, 42), 5, 0x401000))
        self.assertIsNone(facade.on_stop(identity, signal, address, refresh=False))
        self.assertEqual(client.resumes, 1)

    def test_builder_validates_and_fake_gdb_packets_use_honest_boundary(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            manifest = _manifest(directory)
            client = FakeClient()

            live = build_live_facade(
                qemu_socket=str(directory / "qemu.sock"),
                gdb_socket=str(directory / "gdb.sock"),
                manifest_path=manifest,
                client_factory=lambda path, timeout: client,
                runner_factory=lambda target, sealed: FrozenRunner(),
            )
            try:
                # The current vCPU task is the initial GDB inferior even though
                # PID 1 sorts first and is sleeping.
                self.assertEqual(live.facade.handle(b"qC"), b"QCp2a.2a")
                self.assertEqual(live.facade.handle(b"Hg0"), b"OK")
                self.assertEqual(live.facade.handle(b"qC"), b"QCp2a.2a")
                self.assertEqual(live.facade.handle(b"g"), b"01020304")
                self.assertEqual(live.facade.handle(b"m400000,4"), b"E14")
                self.assertEqual(live.facade.handle(b"Hgp1.1"), b"OK")
                self.assertEqual(live.facade.handle(b"g"), b"E14")
                self.assertEqual(live.facade.handle(b"p0"), b"E14")
                self.assertEqual(live.facade.handle(b"M400000,1:00"), b"E14")
                self.assertTrue(
                    live.facade.handle(b"qSupported").startswith(b"PacketSize=")
                )
                self.assertEqual(
                    live.facade.handle(b"qXfer:features:read:core.xml:0,100"),
                    b'l<feature><reg name="x0" bitsize="64"/></feature>',
                )
            finally:
                live.close()
            self.assertTrue(client.closed)

    def test_advertised_mapping_capability_reads_selected_program(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            manifest = _manifest(directory)
            document = json.loads(manifest.read_text(encoding="utf-8"))
            document["probe"]["capabilities"] = [
                "snapshot-v1", "translate-va-aarch64-v1"
            ]
            request = bytearray.fromhex(document["mailbox"]["request_hex"])
            request[6:8] = (1).to_bytes(2, "little")
            document["mailbox"]["request_hex"] = request.hex()
            manifest.write_text(json.dumps(document), encoding="utf-8")
            memory = FakeProgramMemoryReader()

            live = build_live_facade(
                qemu_socket=str(directory / "qemu.sock"),
                gdb_socket=str(directory / "gdb.sock"),
                manifest_path=manifest,
                client_factory=lambda path, timeout: FakeClient(),
                runner_factory=lambda target, sealed: MappingRunner(),
                memory_reader_factory=lambda target, sealed: memory,
            )
            try:
                self.assertEqual(live.facade.handle(b"Hgp2a.2a"), b"OK")
                self.assertEqual(
                    live.facade.handle(b"m400ffe,6"), b"feff00010203"
                )
                self.assertEqual(memory.reads[-1][0], TaskId(42, 42))
                self.assertEqual(memory.reads[-1][2:], (0x400ffe, 6))
            finally:
                live.close()

    def test_advertised_saved_registers_make_sleeping_task_readable(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            manifest = _manifest(directory)
            document = json.loads(manifest.read_text(encoding="utf-8"))
            document["probe"]["capabilities"] = [
                "snapshot-v1", "translate-va-aarch64-v1",
                "saved-regs-aarch64-v1",
            ]
            request = bytearray.fromhex(document["mailbox"]["request_hex"])
            request[6:8] = (2).to_bytes(2, "little")
            document["mailbox"]["request_hex"] = request.hex()
            manifest.write_text(json.dumps(document), encoding="utf-8")
            client = FakeClient()
            client.xml["core.xml"] = _full_core_xml(
                b'<reg name="system_test" bitsize="64" regnum="34"/>')
            client.register_block = bytes(range(256)) + bytes(range(12))
            saved = FakeSavedRegisterReader()

            class Abi12Runner(FrozenRunner):
                def __call__(self, cursor):
                    response = bytearray(super().__call__(cursor))
                    response[6:8] = (2).to_bytes(2, "little")
                    return bytes(response)

            live = build_live_facade(
                qemu_socket=str(directory / "qemu.sock"),
                gdb_socket=str(directory / "gdb.sock"),
                manifest_path=manifest,
                client_factory=lambda path, timeout: client,
                runner_factory=lambda target, sealed: Abi12Runner(),
                memory_reader_factory=lambda target, sealed: FakeProgramMemoryReader(),
                register_reader_factory=lambda target, sealed: saved,
            )
            try:
                # A current task remains a byte-for-byte forwarding of QEMU's
                # raw core block, independent of the sleeping-task encoder.
                self.assertEqual(
                    live.facade.handle(b"g"),
                    client.register_block.hex().encode("ascii"),
                )
                self.assertEqual(live.facade.handle(b"Hgp1.1"), b"OK")
                reply = live.facade.handle(b"g")
                values = {f"x{index}": index for index in range(31)}
                values.update(sp=0x1020304050607080, pc=0x405060, cpsr=0)
                expected = b"".join(
                    values[name]
                    .to_bytes(4 if name == "cpsr" else 8, "little")
                    .hex().encode()
                    for name in AARCH64_USER_REGISTERS
                )
                self.assertEqual(reply, expected)
                self.assertEqual(saved.reads[-1][0], TaskId(1, 1))
                # CPU 0 supplied the observed size during construction; CPU 1
                # supplied the unchanged current-task block.  The sleeping
                # read itself used only the probe.
                self.assertEqual(client.register_reads, ["1", "2"])
            finally:
                live.close()

    def test_saved_register_builder_rejects_nonprefix_qemu_g_size(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            manifest = _manifest(directory)
            document = json.loads(manifest.read_text(encoding="utf-8"))
            document["probe"]["capabilities"] = [
                "snapshot-v1", "saved-regs-aarch64-v1",
            ]
            request = bytearray.fromhex(document["mailbox"]["request_hex"])
            request[6:8] = (2).to_bytes(2, "little")
            document["mailbox"]["request_hex"] = request.hex()
            manifest.write_text(json.dumps(document), encoding="utf-8")
            client = FakeClient()
            client.xml["core.xml"] = _full_core_xml()
            client.register_block = bytes(31 * 8 + 8 + 8 + 3)

            with self.assertRaisesRegex(PartialRegisterError, "ends inside"):
                build_live_facade(
                    qemu_socket=str(directory / "qemu.sock"),
                    gdb_socket=str(directory / "gdb.sock"),
                    manifest_path=manifest,
                    client_factory=lambda path, timeout: client,
                    runner_factory=lambda target, sealed: FrozenRunner(),
                    register_reader_factory=lambda target, sealed: (
                        FakeSavedRegisterReader()),
                )
            self.assertTrue(client.closed)
            self.assertEqual(client.register_reads, ["1"])

    def test_unadvertised_legacy_manifest_never_constructs_memory_reader(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            manifest = _manifest(directory)

            def forbidden(target, sealed):
                raise AssertionError("legacy listing-only manifest enabled memory")

            live = build_live_facade(
                qemu_socket=str(directory / "qemu.sock"),
                gdb_socket=str(directory / "gdb.sock"),
                manifest_path=manifest,
                client_factory=lambda path, timeout: FakeClient(),
                runner_factory=lambda target, sealed: FrozenRunner(),
                memory_reader_factory=forbidden,
            )
            try:
                self.assertEqual(live.facade.handle(b"m400000,4"), b"E14")
            finally:
                live.close()

    def test_fake_qemu_transport_to_fake_gdb_packet_round_trip(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            manifest = _manifest(directory)
            qemu_client_end, qemu_stub_end = memory_duplex_pair()
            qemu_stub = RspStream(qemu_stub_end)
            remote_errors = []

            target_xml = (
                b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
                b"<architecture>aarch64</architecture>"
                b'<xi:include href="core.xml"/></target>'
            )
            core_xml = b'<feature><reg name="x0" bitsize="64"/></feature>'

            def qemu_remote():
                script = (
                    (b"?", b"T05thread:2;"),
                    (b"qXfer:features:read:target.xml:0,800", b"l" + target_xml),
                    (b"qXfer:features:read:core.xml:0,800", b"l" + core_xml),
                    (b"qfThreadInfo", b"m1,2"),
                    (b"qsThreadInfo", b"l"),
                )
                try:
                    for expected, reply in script:
                        self.assertEqual(qemu_stub.receive_packet(2), expected)
                        qemu_stub.send_packet(reply, 2)
                except BaseException as exc:
                    remote_errors.append(exc)

            remote_thread = threading.Thread(target=qemu_remote)
            remote_thread.start()
            live = build_live_facade(
                qemu_socket=str(directory / "qemu.sock"),
                gdb_socket=str(directory / "gdb.sock"),
                manifest_path=manifest,
                client_factory=lambda path, timeout: QemuRspClient(
                    qemu_client_end, timeout
                ),
                runner_factory=lambda target, sealed: FrozenRunner(),
            )
            remote_thread.join(3)
            self.assertFalse(remote_thread.is_alive())
            if remote_errors:
                raise remote_errors[0]

            upstream_server, upstream_gdb = memory_duplex_pair()
            server_thread = threading.Thread(
                target=live.server.serve_connection, args=(upstream_server,)
            )
            server_thread.start()
            gdb = RspStream(upstream_gdb)
            try:
                gdb.send_packet(b"qC", 2)
                self.assertEqual(gdb.receive_packet(2), b"QCp2a.2a")
            finally:
                gdb.close()
                server_thread.join(3)
                live.close()
                qemu_stub.close()
            self.assertFalse(server_thread.is_alive())


if __name__ == "__main__":
    unittest.main()
