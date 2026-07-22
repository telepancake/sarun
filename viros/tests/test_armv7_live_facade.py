from __future__ import annotations

import hashlib
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest import mock

from callgate.architectures import (
    ARMV7,
    ARM_CPSR_A,
    ARM_CPSR_E,
    ARM_CPSR_F,
    ARM_CPSR_I,
    ARM_CPSR_IT_HIGH,
    ARM_CPSR_IT_LOW,
    ARM_CPSR_J,
    ARM_CPSR_MODE,
    ARM_CPSR_SVC,
    ARM_CPSR_T,
    architecture_by_name,
)
from callgate.manifest import ManifestError, load_and_validate_manifest
from callgate.rsp_target import RspQemuTarget
from callgate.transaction import plan
from inferiors.live_facade import _make_runner, build_live_facade
from probe.abi import (
    ARCH_ARM,
    ARMV7LE_SNAPSHOT_ABI,
    RESPONSE_MAGIC,
    TASK_GROUP_LEADER,
    TASK_HAS_MM,
    TASK_ON_CPU,
    build_snapshot_request,
)


HEADER = "IHHHHHBBiIIIQQIIQ"
TASK = "HHIQQQQQQQQIIIIIHH16s10Q"
ARM_ROOT = 0x81234000


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _arm_core_xml() -> bytes:
    registers = [
        f'<reg name="r{number}" bitsize="32"/>' for number in range(13)
    ]
    registers.extend(
        (
            '<reg name="sp" bitsize="32" type="data_ptr"/>',
            '<reg name="lr" bitsize="32"/>',
            '<reg name="pc" bitsize="32" type="code_ptr"/>',
            # This is QEMU's historical arm-core.xml numbering: registers
            # 16..24 are the old FPA gap in the complete g packet.
            '<reg name="cpsr" bitsize="32" regnum="25"/>',
        )
    )
    return ("<feature>" + "".join(registers) + "</feature>").encode("ascii")


def _task_record(pid: int, *, current: bool, cpu: int) -> bytes:
    task = ARM_ROOT + (pid - 1) * 0x1000
    flags = TASK_HAS_MM | TASK_GROUP_LEADER
    if current:
        flags |= TASK_ON_CPU
    return struct.pack(
        "<" + TASK,
        192,
        1,
        flags,
        task,
        task,
        ARM_ROOT,
        0x82345000 + pid * 0x1000,
        0x83456000 + pid * 0x1000,
        0x1020304050607000 + pid,
        0,
        0,
        pid,
        pid,
        0,
        cpu,
        0,
        32,
        0,
        f"task{pid}".encode("ascii").ljust(16, b"\0"),
        *([0] * 10),
    )


def _arm_response() -> bytes:
    records = _task_record(1, current=False, cpu=0) + _task_record(
        42, current=True, cpu=1
    )
    return struct.pack(
        "<" + HEADER,
        RESPONSE_MAGIC,
        1,
        0,
        64,
        192,
        ARCH_ARM,
        1,
        32,
        0,
        0,
        2,
        64 + len(records),
        0,
        ARM_ROOT,
        12,
        0,
        0,
    ) + records


def _arm_manifest(
    directory: Path, *, pstate: int | None = None, capabilities=("snapshot-v1",)
) -> Path:
    kernel = directory / "vmlinux"
    probe = directory / "probe.bin"
    kernel.write_bytes(b"exact ARMv7 live-facade test kernel")
    # Two aligned ARM instructions; manifest validation binds the bytes but
    # does not execute this test fixture.
    probe.write_bytes(bytes.fromhex("0000a0e1000052e3"))
    request = build_snapshot_request(
        ARM_ROOT, 0, 2, abi_minor=0, snapshot_abi=ARMV7LE_SNAPSHOT_ABI
    )
    invocation: dict[str, object] = {
        "cpu": 0,
        "stack_region": "stack",
        "stack_pointer": "0x82002000",
        "timeout_seconds": 1,
    }
    if pstate is not None:
        invocation["pstate"] = pstate
    document = {
        "format": "viros-callgate-v1",
        "architecture": "arm",
        "allow_transient_guest_modification": True,
        "kernel": {
            "vmlinux": kernel.name,
            "sha256": _sha256(kernel),
            "build_id": "1234567890abcdef",
        },
        "regions": [
            {
                "name": "code",
                "role": "code",
                "virtual_address": "0x80100000",
                "physical_address": "0x00100000",
                "size": 4096,
            },
            {
                "name": "data",
                "role": "data",
                "virtual_address": "0x82000000",
                "physical_address": "0x02000000",
                "size": 4096,
            },
            {
                "name": "stack",
                "role": "stack",
                "virtual_address": "0x82001000",
                "physical_address": "0x02001000",
                "size": 4096,
            },
        ],
        "probe": {
            "binary": probe.name,
            "sha256": _sha256(probe),
            "code_region": "code",
            "entry_offset": 0,
            "completion_offset": 4,
            "capabilities": list(capabilities),
        },
        "mailbox": {
            "data_region": "data",
            "request_offset": 0,
            "request_hex": request.hex(),
            "result_offset": 64,
            "result_size": 64 + 2 * 192,
            "completion_magic_hex": struct.pack("<I", RESPONSE_MAGIC).hex(),
        },
        "invocation": invocation,
    }
    path = directory / "callgate.json"
    path.write_text(json.dumps(document), encoding="utf-8")
    return path


class ArmRunner:
    def __call__(self, cursor: int) -> bytes:
        if cursor != 0:
            raise AssertionError(cursor)
        return _arm_response()


class ArmQemuClient:
    def __init__(self) -> None:
        self.closed = False
        self.current = "1"
        self.register_block = bytes(range(68))
        self.register_reads: list[str] = []
        self.single_register_reads: list[tuple[str, int]] = []
        self.inserted_breakpoints: list[tuple[int, int, int]] = []
        self.removed_breakpoints: list[tuple[int, int, int]] = []
        self.xml = {
            "target.xml": (
                b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
                b"<architecture>arm</architecture>"
                b'<xi:include href="arm-core.xml"/></target>'
            ),
            "arm-core.xml": _arm_core_xml(),
        }

    def request(self, payload: bytes) -> bytes:
        if payload == b"?":
            return b"T05thread:2;"
        raise AssertionError(payload)

    def read_xfer(self, object_name: str, annex: str) -> bytes:
        if object_name != "features":
            raise AssertionError(object_name)
        return self.xml[annex]

    def thread_ids(self) -> tuple[str, ...]:
        return ("1", "2")

    def current_thread(self) -> str:
        return self.current

    def select_thread(self, operation: str, thread: str) -> None:
        if operation != "g":
            raise AssertionError(operation)
        self.current = thread

    def read_register(self, number: int) -> bytes:
        self.single_register_reads.append((self.current, number))
        return (0x10000000 + number).to_bytes(4, "little")

    def read_register_block(self, thread: str) -> bytes:
        self.register_reads.append(thread)
        return self.register_block

    def close(self) -> None:
        self.closed = True

    def insert_breakpoint(self, kind: int, address: int, size: int) -> None:
        self.inserted_breakpoints.append((kind, address, size))

    def remove_breakpoint(self, kind: int, address: int, size: int) -> None:
        self.removed_breakpoints.append((kind, address, size))

    def resume(self) -> None:
        raise AssertionError("unexpected resume")

    def step(self, cpu: int) -> None:
        raise AssertionError("unexpected step")

    def step_thread(self, thread_id: str) -> None:
        raise AssertionError("unexpected step")


class Armv7LiveFacadeTests(unittest.TestCase):
    def test_descriptor_matches_aapcs_and_qemu_arm_core(self):
        self.assertIs(architecture_by_name("arm"), ARMV7)
        self.assertEqual(
            ARMV7.register_names,
            tuple(f"r{number}" for number in range(13))
            + ("sp", "lr", "pc", "cpsr"),
        )
        self.assertTrue(all(spec.bits == 32 for spec in ARMV7.core_registers))
        self.assertFalse(ARMV7.has_fixed_rsp_registers)
        self.assertEqual(ARMV7.qemu_architecture_names, ("arm",))
        self.assertEqual(ARMV7.argument_registers, ("r0", "r1", "r2"))
        self.assertEqual((ARMV7.sp_register, ARMV7.link_register), ("sp", "lr"))
        self.assertEqual((ARMV7.stack_alignment, ARMV7.instruction_alignment), (8, 4))
        self.assertEqual(ARMV7.known_capabilities, frozenset({"snapshot-v1"}))

    def test_entry_state_is_derived_without_switching_banked_register_mode(self):
        original = {name: 0 for name in ARMV7.register_names}
        original["cpsr"] = 0xA8000000 | ARM_CPSR_SVC
        values = dict(
            ARMV7.entry_register_values(
                request_address=0x82000000,
                result_address=0x82000040,
                result_size=448,
                completion_address=0x80100004,
                control_state=None,
                original_registers=original,
                stack_pointer=0x82002000,
                entry_address=0x80100000,
            )
        )
        self.assertEqual(
            (values["r0"], values["r1"], values["r2"]),
            (0x82000000, 0x82000040, 448),
        )
        self.assertEqual(values["lr"], 0x80100004)
        self.assertEqual((values["sp"], values["pc"]), (0x82002000, 0x80100000))
        self.assertEqual(values["cpsr"] & ARM_CPSR_MODE, ARM_CPSR_SVC)
        self.assertEqual(
            values["cpsr"] & (ARM_CPSR_A | ARM_CPSR_I | ARM_CPSR_F),
            ARM_CPSR_A | ARM_CPSR_I | ARM_CPSR_F,
        )
        self.assertEqual(values["cpsr"] & 0xF8000000, 0xA8000000)

        incompatible = ARM_CPSR_T | ARM_CPSR_E | ARM_CPSR_J
        incompatible |= ARM_CPSR_IT_LOW | ARM_CPSR_IT_HIGH
        self.assertFalse(values["cpsr"] & incompatible)

        original["cpsr"] = 0x10  # User mode would switch the banked SP/LR.
        with self.assertRaisesRegex(ValueError, "SVC mode"):
            ARMV7.validate_original_state(original)

    def test_manifest_and_snapshot_runner_select_armv7_abi(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            manifest = load_and_validate_manifest(
                _arm_manifest(Path(directory_text))
            )
            self.assertIs(manifest.architecture, ARMV7)
            self.assertIsNone(manifest.pstate)
            self.assertEqual(manifest.probe_capabilities, ("snapshot-v1",))
            runner = _make_runner(object(), manifest)
            self.assertIs(runner.request.snapshot_abi, ARMV7LE_SNAPSHOT_ABI)
            transaction_plan = plan(manifest)
            self.assertIn(
                "snapshot 17 core registers on every vCPU", transaction_plan
            )
            self.assertTrue(
                any(
                    "r0=request" in step
                    and "r1=result" in step
                    and "r2=0x1c0" in step
                    and "lr=completion" in step
                    for step in transaction_plan
                )
            )

    def test_arm_manifest_rejects_control_state_and_unimplemented_capabilities(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            with self.assertRaisesRegex(ManifestError, "pstate must be omitted"):
                load_and_validate_manifest(_arm_manifest(directory, pstate=0x1D3))
            with self.assertRaisesRegex(ManifestError, "unsupported values"):
                load_and_validate_manifest(
                    _arm_manifest(
                        directory,
                        capabilities=("snapshot-v1", "translate-va-aarch64-v1"),
                    )
                )

    def test_qemu_xml_historical_cpsr_number_is_used(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            kernel = Path(directory_text) / "vmlinux"
            kernel.write_bytes(b"ARM target XML test kernel")
            client = ArmQemuClient()
            target = RspQemuTarget(client, kernel, "12345678", ARMV7)
            self.assertEqual(target.read_register(1, "pc"), 0x1000000F)
            self.assertEqual(target.read_register(1, "cpsr"), 0x10000019)
            self.assertIn(("2", 15), client.single_register_reads)
            self.assertIn(("2", 25), client.single_register_reads)
            self.assertEqual(client.current, "1")

    def test_live_facade_lists_tasks_and_forwards_only_current_cpu_registers(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            client = ArmQemuClient()

            def unavailable_reader(*args, **kwargs):
                raise AssertionError("snapshot-only ARM must not construct readers")

            live = build_live_facade(
                qemu_socket=str(directory / "qemu.sock"),
                gdb_socket=str(directory / "gdb.sock"),
                manifest_path=_arm_manifest(directory),
                client_factory=lambda path, timeout: client,
                runner_factory=lambda target, manifest: ArmRunner(),
                memory_reader_factory=unavailable_reader,
                register_reader_factory=unavailable_reader,
            )
            try:
                self.assertIs(live.target.architecture, ARMV7)
                self.assertEqual(live.facade.handle(b"qC"), b"QCp2a.2a")
                self.assertEqual(
                    live.facade.handle(b"g"),
                    client.register_block.hex().encode("ascii"),
                )
                self.assertEqual(client.register_reads, ["2"])
                self.assertEqual(live.facade.handle(b"m400000,4"), b"E14")
                self.assertEqual(live.facade.handle(b"Hgp1.1"), b"OK")
                self.assertEqual(live.facade.handle(b"g"), b"E14")
                self.assertEqual(
                    live.facade.handle(b"qXfer:features:read:arm-core.xml:0,400"),
                    b"l" + _arm_core_xml(),
                )
            finally:
                live.close()
            self.assertTrue(client.closed)

    def test_live_facade_installs_arm_event_boundary_with_arm_layout(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            client = ArmQemuClient()
            event_address = 0x80123400

            with mock.patch(
                "inferiors.live_facade._defined_kernel_symbol",
                return_value=event_address,
            ):
                live = build_live_facade(
                    qemu_socket=str(directory / "qemu.sock"),
                    gdb_socket=str(directory / "gdb.sock"),
                    manifest_path=_arm_manifest(directory),
                    client_factory=lambda path, timeout: client,
                    runner_factory=lambda target, manifest: ArmRunner(),
                    memory_reader_factory=lambda *args: None,
                    register_reader_factory=lambda *args: None,
                )
            try:
                self.assertIsNotNone(live.internal_breakpoints)
                self.assertEqual(
                    client.inserted_breakpoints,
                    [(1, event_address, ARMV7.breakpoint_size)],
                )
                # Parsing the event layout observes the real QEMU g-packet
                # boundary but does not enable saved-register calls.
                self.assertEqual(client.register_reads, ["1"])
                self.assertIsNone(live.oracle.partial_registers)
            finally:
                live.close()
            self.assertEqual(
                client.removed_breakpoints,
                [(1, event_address, ARMV7.breakpoint_size)],
            )
            self.assertTrue(client.closed)


if __name__ == "__main__":
    unittest.main()
