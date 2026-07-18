from __future__ import annotations

import hashlib
import json
from dataclasses import replace
from pathlib import Path
import struct
import tempfile
import unittest

from callgate.manifest import load_and_validate_manifest
from callgate.transaction import CallGateResult
from probe.abi import ProbeStatusError
from probe.snapshot_runner import ProbeRunnerError, ProbeSnapshotRunner


HEADER = "IHHHHHBBiIIIQQIIQ"
TASK = "HHIQQQQQQQQIIIIIHH16s10Q"
REQUEST = "IHHHHIQQIIQQQ"
ROOT = 0xffff800081234000
SECOND = ROOT + 0x1000


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def task_bytes(task=ROOT, pid=1):
    return struct.pack(
        "<" + TASK, 192, 1, 3, task, task, ROOT,
        0xffff800082000000, 0xffff800083000000, pid, 0, 0,
        pid, pid, 0, 0, 0, 64, 0, b"init\0".ljust(16, b"\0"),
        *([0] * 10),
    )


def response(records, *, more=False, next_cursor=0, root=ROOT, status=0):
    body = b"".join(records)
    header = struct.pack(
        "<" + HEADER, 0x56505253, 1, 0, 64, 192, 1, 1, 64,
        status, 1 if more else 0, len(records), 64 + len(body),
        next_cursor, root, 12, 0, 0,
    )
    return header + body


def make_manifest(directory: Path, max_records=2):
    kernel = directory / "vmlinux"
    probe = directory / "probe.bin"
    kernel.write_bytes(b"exact kernel")
    probe.write_bytes(bytes.fromhex("1f2003d500ca2ad4"))
    request = struct.pack(
        "<" + REQUEST, 0x56505251, 1, 0, 64, 1, 0,
        ROOT, 0, max_records, 0, 0, 0, 0,
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
                    "result_size": 64 + max_records * 192,
                    "completion_magic_hex": "53525056"},
        "invocation": {"cpu": 0, "pstate": "0x3c5", "stack_region": "stack",
                       "stack_pointer": "0xffff800082002000", "timeout_seconds": 1},
    }
    path = directory / "callgate.json"
    path.write_text(json.dumps(document), encoding="utf-8")
    return load_and_validate_manifest(path)


class RecordingFactory:
    def __init__(self, pages):
        self.pages = list(pages)
        self.manifests = []
        self.restorations = 0

    def __call__(self, target, manifest):
        factory = self

        class Transaction:
            def execute(self):
                factory.manifests.append(manifest)
                try:
                    return CallGateResult(factory.pages.pop(0), ("restored",))
                finally:
                    # Models the existing transaction's mandatory finally; the
                    # runner must use execute() for every page, including errors.
                    factory.restorations += 1

        return Transaction()


class ProbeSnapshotRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.manifest = make_manifest(Path(self.temporary.name))
        self.original_request = self.manifest.request_bytes

    def tearDown(self):
        self.temporary.cleanup()

    def test_one_page_returns_snapshot_without_mutating_sealed_manifest(self):
        factory = RecordingFactory([response([task_bytes()])])
        runner = ProbeSnapshotRunner(object(), self.manifest, factory)
        snapshot = runner.snapshot()

        self.assertEqual([task.pid for task in snapshot.tasks], [1])
        self.assertEqual(self.manifest.request_bytes, self.original_request)
        self.assertTrue(factory.manifests[0].is_validated)
        request = struct.unpack("<" + REQUEST, factory.manifests[0].request_bytes)
        self.assertEqual((request[6], request[7], request[8]), (ROOT, 0, 2))
        self.assertEqual(runner.audits, (("restored",),))

    def test_pagination_runs_one_restoring_transaction_per_cursor(self):
        factory = RecordingFactory([
            response([task_bytes()], more=True, next_cursor=SECOND),
            response([task_bytes(SECOND, 42)]),
        ])
        runner = ProbeSnapshotRunner(object(), self.manifest, factory)
        snapshot = runner.snapshot()

        cursors = [
            struct.unpack("<" + REQUEST, item.request_bytes)[7]
            for item in factory.manifests
        ]
        self.assertEqual(cursors, [0, SECOND])
        self.assertEqual([task.pid for task in snapshot.tasks], [1, 42])
        self.assertEqual(factory.restorations, 2)

    def test_unadvertised_cursor_is_rejected_before_transaction(self):
        factory = RecordingFactory([
            response([task_bytes()], more=True, next_cursor=SECOND),
            response([task_bytes(SECOND, 42)]),
        ])
        runner = ProbeSnapshotRunner(object(), self.manifest, factory)
        runner.fetch_page(0)
        with self.assertRaisesRegex(ProbeRunnerError, "cursor mismatch"):
            runner.fetch_page(SECOND + 8)
        self.assertEqual(factory.restorations, 1)
        runner.fetch_page(SECOND)
        self.assertEqual(factory.restorations, 2)

    def test_probe_error_is_decoded_after_transaction_restoration(self):
        factory = RecordingFactory([response([], status=-3)])
        runner = ProbeSnapshotRunner(object(), self.manifest, factory)
        with self.assertRaisesRegex(ProbeStatusError, "status -3"):
            runner.fetch_page(0)
        self.assertEqual(factory.restorations, 1)
        self.assertEqual(runner.audits, ())

    def test_response_cursor_mismatch_is_rejected(self):
        factory = RecordingFactory([response([task_bytes(SECOND, 42)])])
        runner = ProbeSnapshotRunner(object(), self.manifest, factory)
        with self.assertRaisesRegex(ProbeRunnerError, "requested cursor"):
            runner.fetch_page(0)
        self.assertEqual(factory.restorations, 1)

    def test_sealed_template_fields_and_init_task_binding_are_rechecked(self):
        fields = list(struct.unpack("<" + REQUEST, self.original_request))
        fields[5] = 1  # flags are frozen at zero in ABI v1
        with self.assertRaisesRegex(ProbeRunnerError, "flags or reserved"):
            ProbeSnapshotRunner(
                object(), replace(self.manifest, request_bytes=struct.pack("<" + REQUEST, *fields))
            )

        factory = RecordingFactory([response([task_bytes()], root=ROOT + 8)])
        runner = ProbeSnapshotRunner(object(), self.manifest, factory)
        with self.assertRaisesRegex(ProbeRunnerError, "sealed init_task"):
            runner.fetch_page(0)
        self.assertEqual(factory.restorations, 1)


if __name__ == "__main__":
    unittest.main()
