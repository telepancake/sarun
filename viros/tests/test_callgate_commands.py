from __future__ import annotations

import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from callgate.manifest import ManifestError
from callgate.transaction import CallGateResult
from gdb_probe import (
    ProbeProcessResult,
    format_live_result,
    format_process_snapshot,
    run_live_probe,
    run_probe_snapshot,
    validated_plan,
)
from probe.abi import ProbeSnapshot, ProbeTask, TASK_ON_CPU


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def command_manifest(directory: Path) -> Path:
    kernel = directory / "vmlinux"
    probe = directory / "probe.bin"
    kernel.write_bytes(b"kernel")
    probe.write_bytes(bytes.fromhex("1f2003d51f2003d5"))
    manifest = {
        "format": "viros-callgate-v1",
        "architecture": "aarch64",
        "allow_transient_guest_modification": True,
        "kernel": {
            "vmlinux": "vmlinux",
            "sha256": digest(kernel),
            "build_id": "12345678",
        },
        "regions": [
            {"name": "code", "role": "code", "virtual_address": "0xffff000000100000", "physical_address": "0x40100000", "size": 4096},
            {"name": "data", "role": "data", "virtual_address": "0xffff000000200000", "physical_address": "0x40200000", "size": 4096},
            {"name": "stack", "role": "stack", "virtual_address": "0xffff000000201000", "physical_address": "0x40201000", "size": 4096},
        ],
        "probe": {
            "binary": "probe.bin",
            "sha256": digest(probe),
            "code_region": "code",
            "entry_offset": 0,
            "completion_offset": 4,
        },
        "mailbox": {
            "data_region": "data",
            "result_offset": 0,
            "result_size": 4,
            "completion_magic_hex": "5649524f",
        },
        "invocation": {
            "cpu": 0,
            "pstate": "0x3c5",
            "stack_region": "stack",
            "stack_pointer": "0xffff000000202000",
            "argument_address": "0xffff000000200000",
            "timeout_seconds": 1,
        },
    }
    path = directory / "manifest.json"
    path.write_text(json.dumps(manifest), encoding="utf-8")
    return path


class CommandGuardTests(unittest.TestCase):
    def test_plan_is_available_without_gdb_or_target_access(self):
        with tempfile.TemporaryDirectory() as temporary:
            operations = validated_plan(str(command_manifest(Path(temporary))))
            self.assertTrue(any("snapshot" in operation for operation in operations))
            self.assertTrue(any("restore" in operation for operation in operations))

    @mock.patch("gdb_probe.CallGateTransaction")
    @mock.patch("gdb_probe.GdbQemuTarget")
    def test_valid_manifest_constructs_backend_then_executes_transaction(
        self, target_type, transaction_type
    ):
        with tempfile.TemporaryDirectory() as temporary:
            path = command_manifest(Path(temporary))
            gdb_module = object()
            target = target_type.return_value
            expected = CallGateResult(b"VIRO", ("region code: restored",))
            transaction_type.return_value.execute.return_value = expected

            actual = run_live_probe(str(path), gdb_module)

            self.assertIs(actual, expected)
            target_type.assert_called_once_with(gdb_module)
            transaction_type.assert_called_once()
            self.assertIs(transaction_type.call_args.args[0], target)
            self.assertTrue(transaction_type.call_args.args[1].is_validated)
            transaction_type.return_value.execute.assert_called_once_with()

    @mock.patch("gdb_probe.GdbQemuTarget")
    def test_invalid_manifest_fails_before_target_construction(self, target_type):
        with tempfile.TemporaryDirectory() as temporary:
            path = command_manifest(Path(temporary))
            document = json.loads(path.read_text(encoding="utf-8"))
            document["probe"]["sha256"] = "0" * 64
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaises(ManifestError):
                run_live_probe(str(path), object())
            target_type.assert_not_called()

    def test_result_format_preserves_binary_response_and_audit(self):
        lines = format_live_result(
            CallGateResult(
                result=b"\x00VIRO\xff",
                audit=("region code: restored", "68 register values: restored"),
            )
        )
        self.assertEqual(lines[0], "Probe response (6 bytes): 005649524fff")
        self.assertEqual(lines[1], "Restoration audit:")
        self.assertEqual(lines[2:], (
            "  region code: restored",
            "  68 register values: restored",
        ))

    @mock.patch("gdb_probe.ProbeSnapshotRunner")
    @mock.patch("gdb_probe.GdbQemuTarget")
    def test_process_snapshot_validates_before_backend_and_runs_shared_runner(
        self, target_type, runner_type
    ):
        with tempfile.TemporaryDirectory() as temporary:
            path = command_manifest(Path(temporary))
            gdb_module = object()
            snapshot = ProbeSnapshot(0, 1, "<", 64, 12, 0x1000, ())
            runner_type.return_value.snapshot.return_value = snapshot
            runner_type.return_value.audits = (("region code: restored",),)

            actual = run_probe_snapshot(str(path), gdb_module)

            self.assertEqual(
                actual,
                ProbeProcessResult(snapshot, (("region code: restored",),)),
            )
            target_type.assert_called_once_with(gdb_module)
            runner_type.assert_called_once()
            self.assertIs(runner_type.call_args.args[0], target_type.return_value)
            self.assertTrue(runner_type.call_args.args[1].is_validated)
            runner_type.return_value.snapshot.assert_called_once_with()

    @mock.patch("gdb_probe.ProbeSnapshotRunner")
    @mock.patch("gdb_probe.GdbQemuTarget")
    def test_invalid_process_manifest_fails_before_target_or_runner(
        self, target_type, runner_type
    ):
        with tempfile.TemporaryDirectory() as temporary:
            path = command_manifest(Path(temporary))
            document = json.loads(path.read_text(encoding="utf-8"))
            document["kernel"]["sha256"] = "0" * 64
            path.write_text(json.dumps(document), encoding="utf-8")

            with self.assertRaises(ManifestError):
                run_probe_snapshot(str(path), object())

            target_type.assert_not_called()
            runner_type.assert_not_called()

    def test_process_snapshot_format_includes_threads_and_page_audits(self):
        running = ProbeTask(
            task=0x1000, group_leader=0x1000, real_parent=0,
            mm=0x2000, pgd_kernel_va=0x3000, start_cookie=1,
            state=0, task_flags=0, pid=7, tgid=7, ppid=1, cpu=2,
            exit_state=0, abi_bits=64, auxv_valid=0, comm="quagga",
            auxv_values=(0,) * 10, probe_flags=TASK_ON_CPU,
        )
        sleeping = ProbeTask(
            task=0x1100, group_leader=0x1000, real_parent=0,
            mm=0x2000, pgd_kernel_va=0x3000, start_cookie=2,
            state=1, task_flags=0, pid=8, tgid=7, ppid=1, cpu=9,
            exit_state=0, abi_bits=64, auxv_valid=0, comm="zebra-worker",
            auxv_values=(0,) * 10, probe_flags=0,
        )
        snapshot = ProbeSnapshot(0, 1, "<", 64, 12, 0x1000, (running, sleeping))

        lines = format_process_snapshot(ProbeProcessResult(
            snapshot,
            (("region code: restored", "68 register values: restored"),
             ("region data: restored",)),
        ))

        self.assertEqual(lines[0].split(), ["PID", "TGID", "CPU", "STATE", "COMM", "MM", "PGD"])
        self.assertIn("7        7        2", lines[1])
        self.assertIn("quagga", lines[1])
        self.assertIn("0x2000", lines[1])
        self.assertIn("8        7        -", lines[2])
        self.assertIn("zebra-worker", lines[2])
        self.assertEqual(lines[-2], "  page 1: region code: restored; 68 register values: restored")
        self.assertEqual(lines[-1], "  page 2: region data: restored")


if __name__ == "__main__":
    unittest.main()
