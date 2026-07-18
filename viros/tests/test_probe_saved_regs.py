from __future__ import annotations

from dataclasses import replace
import hashlib
import json
from pathlib import Path
import struct
import tempfile
import unittest

from callgate.manifest import load_and_validate_manifest
from callgate.transaction import CallGateResult
from inferiors.linux_oracle import TaskId, TaskSnapshot
from probe.abi import (
    ABI_MINOR,
    ProbeCompatTaskError,
    ProbeDecodeError,
    ProbeInvalidRegistersError,
    ProbeStaleTaskError,
    ProbeTask,
    ProbeTaskRunningError,
    ProbeSnapshot,
    REGS_AARCH64_64,
    REGS_USER,
    REGS_VALID,
    SAVED_REGS_V1_SIZE,
    TASK_HAS_MM,
    TASK_ON_CPU,
    _build_saved_registers_request,
    decode_saved_registers_response,
)
from probe.register_reader import ProbeRegisterError, ProbeRegisterReader


ROOT = Path(__file__).resolve().parents[1]
TASK = 0xFFFF800012340000
MM = 0xFFFF800045670000
COOKIE = 0x123456789
HEADER = "<IHHHHHBBiIIIQQIIQ"


def task(**changes) -> ProbeTask:
    record = ProbeTask(
        TASK, TASK, TASK - 0x1000, MM, MM + 0x1000, COOKIE,
        1, 0, 42, 42, 1, 3, 0, 64, 0, "worker", (0,) * 10,
        TASK_HAS_MM,
    )
    return replace(record, **changes)


def response(*, status=0, flags=REGS_VALID | REGS_USER | REGS_AARCH64_64,
             record_task=TASK, mm=MM, cookie=COOKIE, sp=0x7FFFFFFFE000,
             pc=0x401234, pstate=0, minor=ABI_MINOR) -> bytes:
    count = 0 if status else 1
    written = 64 if status else 64 + SAVED_REGS_V1_SIZE
    header = struct.pack(
        HEADER, 0x56505253, 1, minor, 64, SAVED_REGS_V1_SIZE,
        1, 1, 64, status, 0, count, written, 0,
        record_task if status == 0 else 0, 12, 0, 0,
    )
    if status:
        return header
    values = tuple(range(31)) + (sp, pc, pstate)
    record = struct.pack(
        "<HHIQQQ31Q3Q", SAVED_REGS_V1_SIZE, 1, flags,
        record_task, mm, cookie, *values,
    )
    return header + record


def manifest(directory: Path, *, minor=ABI_MINOR):
    kernel = directory / "vmlinux"
    binary = directory / "probe.bin"
    kernel.write_bytes(b"exact saved-register test kernel")
    binary.write_bytes(bytes.fromhex("1f2003d500ca2ad4"))
    digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    request = struct.pack(
        "<IHHHHIQQIIQQQ", 0x56505251, 1, minor, 64, 1, 0,
        TASK, 0, 1, 0, 0, 0, 0,
    )
    document = {
        "format": "viros-callgate-v1", "architecture": "aarch64",
        "allow_transient_guest_modification": True,
        "kernel": {"vmlinux": kernel.name, "sha256": digest(kernel),
                   "build_id": "0123456789abcdef"},
        "regions": [
            {"name": "code", "role": "code",
             "virtual_address": "0xffff800080100000",
             "physical_address": "0x40100000", "size": 4096},
            {"name": "data", "role": "data",
             "virtual_address": "0xffff800082000000",
             "physical_address": "0x42000000", "size": 4096},
            {"name": "stack", "role": "stack",
             "virtual_address": "0xffff800082001000",
             "physical_address": "0x42001000", "size": 4096},
        ],
        "probe": {"binary": binary.name, "sha256": digest(binary),
                  "code_region": "code", "capabilities": [
                      "snapshot-v1", "translate-va-aarch64-v1",
                      "saved-regs-aarch64-v1"],
                  "entry_offset": 0, "completion_offset": 4},
        "mailbox": {"data_region": "data", "request_offset": 0,
                    "request_hex": request.hex(), "result_offset": 64,
                    "result_size": 512, "completion_magic_hex": "53525056"},
        "invocation": {"cpu": 0, "pstate": "0x3c5",
                       "stack_region": "stack",
                       "stack_pointer": "0xffff800082002000",
                       "timeout_seconds": 1},
    }
    path = directory / "callgate.json"
    path.write_text(json.dumps(document), encoding="utf-8")
    return load_and_validate_manifest(path)


class Factory:
    def __init__(self, result):
        self.result = result
        self.requests = []

    def __call__(self, target, transaction_manifest):
        factory = self

        class Transaction:
            def execute(self):
                factory.requests.append(transaction_manifest.request_bytes)
                return CallGateResult(factory.result, ("restored",))

        return Transaction()


class ProbeSavedRegistersTests(unittest.TestCase):
    def test_request_is_bound_to_frozen_task_identity(self):
        fields = struct.unpack("<IHHHHIQQIIQQQ", _build_saved_registers_request(task()))
        self.assertEqual(fields[:6], (0x56505251, 1, 2, 64, 3, 0))
        self.assertEqual(fields[6:13], (TASK, MM, 0, 0, COOKIE, 0, 0))

    def test_request_rejects_on_cpu_compat_and_unstable_tasks(self):
        with self.assertRaisesRegex(ProbeDecodeError, "on-CPU"):
            _build_saved_registers_request(task(probe_flags=TASK_HAS_MM | TASK_ON_CPU))
        with self.assertRaisesRegex(ProbeDecodeError, "compat32"):
            _build_saved_registers_request(task(abi_bits=32))
        with self.assertRaisesRegex(ProbeDecodeError, "stable"):
            _build_saved_registers_request(task(mm=0))

        request = _build_saved_registers_request(task(start_cookie=0))
        self.assertEqual(struct.unpack("<IHHHHIQQIIQQQ", request)[10], 0)

    def test_decodes_all_aarch64_user_registers_with_explicit_validity(self):
        decoded = decode_saved_registers_response(response())
        self.assertTrue(decoded.valid)
        self.assertEqual(decoded.x, tuple(range(31)))
        self.assertEqual((decoded.sp, decoded.pc, decoded.pstate),
                         (0x7FFFFFFFE000, 0x401234, 0))
        self.assertEqual((decoded.task, decoded.mm, decoded.start_cookie),
                         (TASK, MM, COOKIE))

    def test_decodes_explicit_rejection_statuses(self):
        for status, error in (
            (-5, ProbeStaleTaskError),
            (-7, ProbeTaskRunningError),
            (-8, ProbeInvalidRegistersError),
            (-9, ProbeCompatTaskError),
        ):
            with self.subTest(status=status), self.assertRaises(error):
                decode_saved_registers_response(response(status=status))

    def test_rejects_invalid_identity_flags_mode_and_older_abi(self):
        with self.assertRaisesRegex(ProbeDecodeError, "validity flags"):
            decode_saved_registers_response(response(flags=REGS_VALID))
        with self.assertRaisesRegex(ProbeDecodeError, "task identity"):
            decode_saved_registers_response(response(mm=0))
        with self.assertRaisesRegex(ProbeDecodeError, "EL0t"):
            decode_saved_registers_response(response(pstate=5))
        with self.assertRaisesRegex(ProbeDecodeError, "EL0t"):
            decode_saved_registers_response(response(pstate=1 << 32))
        with self.assertRaisesRegex(ProbeDecodeError, "incompatible magic or ABI"):
            decode_saved_registers_response(response(minor=1))

    def test_kernel_source_contains_noncurrent_and_frame_validations(self):
        source = (ROOT / "probe/kernel/viros_probe.c").read_text()
        for witness in (
            "task == current", "task->on_cpu", "PF_KTHREAD",
            "task_pt_regs(task)", "user_mode(regs)", "PSR_MODE_EL0t",
            "VIROS_PROBE_COMPAT_TASK", "VIROS_PROBE_REGS_VALID",
        ):
            self.assertIn(witness, source)

    def test_reader_binds_snapshot_executes_and_audits_one_identity(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            factory = Factory(response())
            reader = ProbeRegisterReader(
                object(), manifest(Path(directory_text)), factory)
            snapshot = ProbeSnapshot(
                ABI_MINOR, 1, "<", 64, 12, TASK, (task(),))
            reader.bind_snapshot(snapshot)
            selected = TaskSnapshot(
                TaskId(42, 42), task().stable_cookie, "worker", "", b"")
            saved = reader.read_registers(selected)
            request = struct.unpack("<IHHHHIQQIIQQQ", factory.requests[0])
            self.assertEqual(request[4], 3)
            self.assertEqual(request[6:11], (TASK, MM, 0, 0, COOKIE))
            self.assertEqual((saved.pc, saved.sp), (0x401234, 0x7FFFFFFFE000))
            self.assertEqual(reader.audits, (("restored",),))

            rebound = ProbeSnapshot(
                ABI_MINOR, 1, "<", 64, 12, TASK,
                (task(start_cookie=COOKIE + 1),),
            )
            reader.bind_snapshot(rebound)
            with self.assertRaisesRegex(ProbeRegisterError, "bound frozen"):
                reader.read_registers(selected)

    def test_reader_rejects_legacy_request_or_unadvertised_capability(self):
        with tempfile.TemporaryDirectory(dir=".") as directory_text:
            directory = Path(directory_text)
            legacy = manifest(directory, minor=1)
            with self.assertRaisesRegex(ProbeRegisterError, "ABI-v1.2"):
                ProbeRegisterReader(object(), legacy, Factory(response()))
            document = json.loads(legacy.source.read_text(encoding="utf-8"))
            document["probe"]["capabilities"].remove(
                "saved-regs-aarch64-v1")
            legacy.source.write_text(json.dumps(document), encoding="utf-8")
            unadvertised = load_and_validate_manifest(legacy.source)
            with self.assertRaisesRegex(ProbeRegisterError, "does not advertise"):
                ProbeRegisterReader(
                    object(), unadvertised, Factory(response()))


if __name__ == "__main__":
    unittest.main()
