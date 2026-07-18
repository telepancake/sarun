from __future__ import annotations

import ctypes
from dataclasses import replace
import hashlib
import inspect
import json
from pathlib import Path
import struct
import tempfile
import unittest

from callgate.manifest import load_and_validate_manifest
from callgate.transaction import CallGateResult
from inferiors.linux_oracle import TaskId, TaskSnapshot
from probe.abi import (
    ProbeDecodeError,
    ProbeNotPresentError,
    ProbeSnapshot,
    ProbeStaleTaskError,
    ProbeTask,
    XLATE_PRESENT,
    XLATE_SAFE_READ,
    XLATE_USER,
    _build_translation_request,
    decode_translation_response,
)
from probe.memory_reader import ProbeMemoryError, ProbeMemoryReader


HEADER = "IHHHHHBBiIIIQQIIQ"
TRANSLATION = "HHIQQQQQQIHH"
REQUEST = "IHHHHIQQIIQQQ"
TASK_PTR = 0xffff800081234000
MM_PTR = 0xffff800082345000
LINEAR_OFFSET = 0xffff800000000000


class TranslationLayout(ctypes.Structure):
    _fields_ = [
        ("record_size", ctypes.c_uint16), ("record_version", ctypes.c_uint16),
        ("flags", ctypes.c_uint32), ("task", ctypes.c_uint64),
        ("mm", ctypes.c_uint64), ("va", ctypes.c_uint64),
        ("pa", ctypes.c_uint64), ("span", ctypes.c_uint64),
        ("mapping", ctypes.c_uint64), ("page_shift", ctypes.c_uint32),
        ("level", ctypes.c_uint16), ("reserved", ctypes.c_uint16),
    ]


def probe_task(*, start=123, abi_bits=64):
    return ProbeTask(
        TASK_PTR, TASK_PTR, TASK_PTR, MM_PTR, 0xffff800083000000,
        start, 0, 0, 7, 7, 1, 0, 0, abi_bits, 0, "worker",
        (0,) * 10, 3,
    )


def translation_response(va, pa, *, mapping=4096, flags=None, status=0,
                         task=TASK_PTR, mm=MM_PTR, level=4):
    if flags is None:
        flags = XLATE_PRESENT | XLATE_USER | XLATE_SAFE_READ
    if status:
        count, written, body = 0, 64, b""
    else:
        shift = mapping.bit_length() - 1
        span = mapping - (va & (mapping - 1))
        body = struct.pack(
            "<" + TRANSLATION, 64, 1, flags, task, mm, va, pa, span,
            mapping, shift, level, 0,
        )
        count, written = 1, 128
    header = struct.pack(
        "<" + HEADER, 0x56505253, 1, 1, 64, 64, 1, 1, 64,
        status, 0, count, written, 0, task, 12, 0, 0,
    )
    return header + body


def make_manifest(directory: Path):
    kernel = directory / "vmlinux"
    binary = directory / "probe.bin"
    kernel.write_bytes(b"exact kernel")
    binary.write_bytes(bytes.fromhex("1f2003d500ca2ad4"))
    sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    snapshot_request = struct.pack(
        "<" + REQUEST, 0x56505251, 1, 1, 64, 1, 0,
        TASK_PTR, 0, 1, 0, 0, 0, 0,
    )
    document = {
        "format": "viros-callgate-v1", "architecture": "aarch64",
        "allow_transient_guest_modification": True,
        "kernel": {"vmlinux": kernel.name, "sha256": sha(kernel),
                   "build_id": "0123456789abcdef"},
        "regions": [
            {"name": "code", "role": "code", "virtual_address": "0xffff800080100000",
             "physical_address": "0x40100000", "size": 4096},
            {"name": "data", "role": "data", "virtual_address": "0xffff800082000000",
             "physical_address": "0x42000000", "size": 4096},
            {"name": "stack", "role": "stack", "virtual_address": "0xffff800082001000",
             "physical_address": "0x42001000", "size": 4096},
        ],
        "probe": {"binary": binary.name, "sha256": sha(binary), "code_region": "code",
                  "capabilities": ["snapshot-v1", "translate-va-aarch64-v1"],
                  "entry_offset": 0, "completion_offset": 4},
        "mailbox": {"data_region": "data", "request_offset": 0,
                    "request_hex": snapshot_request.hex(), "result_offset": 64,
                    "result_size": 256, "completion_magic_hex": "53525056"},
        "invocation": {"cpu": 0, "pstate": "0x3c5", "stack_region": "stack",
                       "stack_pointer": "0xffff800082002000", "timeout_seconds": 1},
    }
    path = directory / "callgate.json"
    path.write_text(json.dumps(document), encoding="utf-8")
    return load_and_validate_manifest(path)


class Factory:
    def __init__(self, responses):
        self.responses = list(responses)
        self.requests = []

    def __call__(self, target, manifest):
        factory = self

        class Transaction:
            def execute(self):
                factory.requests.append(manifest.request_bytes)
                return CallGateResult(factory.responses.pop(0), ("restored",))

        return Transaction()


class Target:
    def __init__(self):
        self.reads = []
        self.translations = []

    def assert_stopped(self):
        pass

    def translate_virtual(self, cpu, address):
        self.translations.append((cpu, address))
        return address - LINEAR_OFFSET

    def read_physical(self, address, size):
        self.reads.append((address, size))
        return bytes(((address + offset) & 0xff) for offset in range(size))


class ProbeTranslationTests(unittest.TestCase):
    def test_static_layout_and_header_assertion(self):
        self.assertEqual(ctypes.sizeof(TranslationLayout), 64)
        header = (Path(__file__).parents[1] / "probe/include/viros_probe_abi.h").read_text()
        self.assertIn("VIROS_PROBE_TRANSLATION_V1_SIZE 64U", header)
        self.assertIn("sizeof(struct viros_probe_translation_v1)", header)

    def test_request_binds_task_mm_cookie_and_va(self):
        fields = struct.unpack(
            "<" + REQUEST,
            _build_translation_request(probe_task(), 0x1234, LINEAR_OFFSET))
        self.assertEqual(fields[:6], (0x56505251, 1, 1, 64, 2, 0))
        self.assertEqual((fields[6], fields[7], fields[8], fields[9]),
                         (TASK_PTR, MM_PTR, 0, 0))
        self.assertEqual((fields[10], fields[11], fields[12]),
                         (123, 0x1234, LINEAR_OFFSET))
        with self.assertRaisesRegex(ProbeDecodeError, "32-bit"):
            _build_translation_request(probe_task(abi_bits=32), 1 << 32, LINEAR_OFFSET)
        with self.assertRaisesRegex(ProbeDecodeError, "64-bit"):
            _build_translation_request(probe_task(), 1 << 63, LINEAR_OFFSET)

    def test_decodes_page_and_block_geometry(self):
        page = decode_translation_response(translation_response(0x1ff0, 0x801ff0))
        self.assertEqual((page.contiguous_bytes, page.mapping_bytes, page.level),
                         (16, 4096, 4))
        block = decode_translation_response(translation_response(
            0x201234, 0x40201234, mapping=2 * 1024 * 1024,
            flags=XLATE_PRESENT | XLATE_USER | XLATE_SAFE_READ | (1 << 4), level=3))
        self.assertEqual(block.mapping_bytes, 2 * 1024 * 1024)
        self.assertEqual(block.contiguous_bytes, block.mapping_bytes - 0x1234)

    def test_explicit_error_statuses_and_malformed_safe_flag(self):
        with self.assertRaises(ProbeStaleTaskError):
            decode_translation_response(translation_response(0, 0, status=-5))
        with self.assertRaises(ProbeNotPresentError):
            decode_translation_response(translation_response(0, 0, status=-6))
        with self.assertRaisesRegex(ProbeDecodeError, "unsafe SAFE_READ"):
            decode_translation_response(translation_response(
                0, 0x1000, flags=XLATE_PRESENT | XLATE_SAFE_READ))


class ProbeMemoryReaderTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.manifest = make_manifest(Path(self.temporary.name))
        self.record = probe_task()
        self.snapshot = ProbeSnapshot(1, 1, "<", 64, 12, TASK_PTR, (self.record,))
        self.task = TaskSnapshot(
            TaskId(7, 7), self.record.stable_cookie, "worker", "", b"")

    def tearDown(self):
        self.temporary.cleanup()

    def test_chunks_across_mappings_and_runs_transaction_per_translation(self):
        factory = Factory([
            translation_response(0x1ff0, 0x801ff0),
            translation_response(0x2000, 0x902000),
        ])
        target = Target()
        reader = ProbeMemoryReader(
            target, self.manifest, factory)
        reader.bind_snapshot(self.snapshot)
        data = reader.read_memory(self.task, 0x1ff0, 32)
        self.assertEqual(len(data), 32)
        self.assertEqual(target.reads, [(0x801ff0, 16), (0x902000, 16)])
        self.assertEqual(len(factory.requests), 2)
        self.assertEqual([struct.unpack("<" + REQUEST, request)[11]
                          for request in factory.requests], [0x1ff0, 0x2000])
        self.assertEqual(reader.audits, (("restored",), ("restored",)))
        self.assertEqual(
            target.translations,
            [(0, self.record.pgd_kernel_va)],
        )

    def test_rejects_unbound_reused_and_unsafe_tasks_without_physical_read(self):
        factory = Factory([translation_response(
            0x1000, 0x800000, flags=XLATE_PRESENT | XLATE_USER)])
        target = Target()
        reader = ProbeMemoryReader(
            target, self.manifest, factory, max_read_bytes=32)
        with self.assertRaisesRegex(ProbeMemoryError, "bound frozen snapshot"):
            reader.read_memory(self.task, 0, 1)
        reader.bind_snapshot(self.snapshot)
        reused = TaskSnapshot(TaskId(7, 7), self.record.stable_cookie + 1, "worker", "", b"")
        with self.assertRaisesRegex(ProbeMemoryError, "bound frozen snapshot"):
            reader.read_memory(reused, 0, 1)
        with self.assertRaisesRegex(ProbeMemoryError, "safety limit"):
            reader.read_memory(self.task, 0, 33)
        with self.assertRaisesRegex(ProbeMemoryError, "virtual address width"):
            reader.read_memory(self.task, 1 << 63, 1)
        with self.assertRaisesRegex(ProbeMemoryError, "refused a safe"):
            reader.read_memory(self.task, 0x1000, 1)
        self.assertEqual(target.reads, [])

    def test_rebinding_snapshot_invalidates_prior_task_cookie(self):
        reader = ProbeMemoryReader(Target(), self.manifest, Factory([]))
        reader.bind_snapshot(self.snapshot)
        replacement = replace(
            self.record, task=TASK_PTR + 0x1000,
            group_leader=TASK_PTR + 0x1000, start_cookie=124,
        )
        reader.bind_snapshot(replace(self.snapshot, tasks=(replacement,)))
        with self.assertRaisesRegex(ProbeMemoryError, "bound frozen snapshot"):
            reader.read_memory(self.task, 0x1000, 1)

    def test_public_api_has_no_caller_supplied_direct_map_relation(self):
        constructor = inspect.signature(ProbeMemoryReader)
        read_memory = inspect.signature(ProbeMemoryReader.read_memory)
        for parameter in ("linear_offset", "linear_map_offset", "direct_map_offset"):
            self.assertNotIn(parameter, constructor.parameters)
            self.assertNotIn(parameter, read_memory.parameters)

    def test_zero_length_needs_no_transaction(self):
        factory = Factory([])
        reader = ProbeMemoryReader(
            Target(), self.manifest, factory)
        reader.bind_snapshot(self.snapshot)
        self.assertEqual(reader.read_memory(self.task, 0x1000, 0), b"")
        self.assertEqual(factory.requests, [])

    def test_rejects_unaligned_qemu_pgd_translation_before_transaction(self):
        class UnalignedTarget(Target):
            def translate_virtual(self, cpu, address):
                return address - LINEAR_OFFSET + 1

        factory = Factory([])
        reader = ProbeMemoryReader(UnalignedTarget(), self.manifest, factory)
        reader.bind_snapshot(self.snapshot)
        with self.assertRaisesRegex(ProbeMemoryError, "page-aligned"):
            reader.read_memory(self.task, 0x1000, 1)
        self.assertEqual(factory.requests, [])

    def test_rejects_mismatched_direct_map_offsets_between_tasks(self):
        second_record = replace(
            self.record, task=TASK_PTR + 0x1000, group_leader=TASK_PTR + 0x1000,
            mm=MM_PTR + 0x1000,
            pgd_kernel_va=self.record.pgd_kernel_va + 0x1000,
            pid=8, tgid=8,
        )
        second_task = TaskSnapshot(
            TaskId(8, 8), second_record.stable_cookie, "other", "", b"")

        class MismatchTarget(Target):
            def translate_virtual(self, cpu, address):
                if address == second_record.pgd_kernel_va:
                    return address - LINEAR_OFFSET - 0x1000
                return address - LINEAR_OFFSET

        factory = Factory([translation_response(0x1000, 0x801000)])
        reader = ProbeMemoryReader(MismatchTarget(), self.manifest, factory)
        reader.bind_snapshot(replace(
            self.snapshot, tasks=(self.record, second_record)))
        reader.read_memory(self.task, 0x1000, 1)
        with self.assertRaisesRegex(ProbeMemoryError, "disagree"):
            reader.read_memory(second_task, 0x1000, 1)
        self.assertEqual(len(factory.requests), 1)

    def test_old_snapshot_manifest_cannot_enable_translation(self):
        old = replace(self.manifest, request_bytes=bytes(
            bytearray(self.manifest.request_bytes[:6])
            + b"\0\0" + self.manifest.request_bytes[8:]
        ))
        with self.assertRaisesRegex(ProbeMemoryError, "ABI-v1.1"):
            ProbeMemoryReader(Target(), old, Factory([]))


if __name__ == "__main__":
    unittest.main()
