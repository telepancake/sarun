import struct
import unittest

from inferiors.linux_oracle import TaskId
from inferiors.probe_oracle import ProbeOracle
from probe.abi import (
    ARCH_MIPS,
    MIPS32BE_SNAPSHOT_ABI,
    MIPS32EL_SNAPSHOT_ABI,
    ProbeDecodeError,
    ProbeStatusError,
    decode_paginated,
    decode_response,
)


HEADER = "IHHHHHBBiIIIQQIIQ"
TASK = "HHIQQQQQQQQIIIIIHH16s10Q"
ROOT = 0xffff800001000000
MIPS_ROOT = 0x81234000


def task_bytes(*, task=ROOT, leader=ROOT, mm=0xffff800002000000,
               pgd=0xffff800003000000, start=123, pid=1, tgid=1,
               abi=64, aux_mask=(1 << 0) | (1 << 5),
               aux=(0x400040, 0, 0, 0, 0, 0x4064d0, 0, 0, 0, 0),
               flags=None, comm=b"init", state=1, cpu=0):
    if flags is None:
        flags = (1 if mm else 0) | (2 if task == leader else 0) | (8 if aux_mask else 0)
    return struct.pack(
        "<" + TASK, 192, 1, flags, task, leader, ROOT, mm, pgd, start,
        state, 0, pid, tgid, 0, cpu, 0, abi, aux_mask,
        comm.ljust(16, b"\0"), *aux)


def response(records, *, root=ROOT, more=False, cursor=0, status=0,
             bytes_written=None, flags=None):
    body = b"".join(records)
    if flags is None:
        flags = 1 if more else 0
    if bytes_written is None:
        bytes_written = 64 + len(body)
    header = struct.pack(
        "<" + HEADER, 0x56505253, 1, 0, 64, 192, 1, 1, 64,
        status, flags, len(records), bytes_written, cursor, root, 12, 0, 0)
    return header + body


def mips_task_bytes(byte_order, *, task=MIPS_ROOT):
    return struct.pack(
        byte_order + TASK, 192, 1, 3, task, task, 0x81233000,
        0x82345000, 0x83456000, 0x1020304050607080,
        0xfffffffffffffff0, 0x8877665544332211,
        1, 1, 0, 0, 0, 32, 0,
        b"init\0".ljust(16, b"\0"), *([0] * 10),
    )


def mips_response(snapshot_abi, records, *, root=MIPS_ROOT):
    body = b"".join(records)
    return struct.pack(
        snapshot_abi.byte_order + HEADER,
        0x56505253, 1, 0, 64, 192, ARCH_MIPS,
        snapshot_abi.endian_code, 32, 0, 0, len(records), 64 + len(body),
        0, root, 12, 0, 0,
    ) + body


class ProbeDecoderTests(unittest.TestCase):
    def test_native_aarch64_record(self):
        page = decode_response(response([task_bytes()]))
        self.assertEqual(page.tasks[0].pid, 1)
        self.assertEqual(page.tasks[0].abi_bits, 64)
        self.assertEqual(page.tasks[0].pgd_kernel_va, 0xffff800003000000)

    def test_mips32_pointer_slots_decode_in_each_target_byte_order(self):
        for snapshot_abi in (MIPS32EL_SNAPSHOT_ABI, MIPS32BE_SNAPSHOT_ABI):
            with self.subTest(snapshot_abi=snapshot_abi.name):
                page = decode_response(
                    mips_response(
                        snapshot_abi,
                        [mips_task_bytes(snapshot_abi.byte_order)],
                    ),
                    expected_abi=snapshot_abi,
                )
                task = page.tasks[0]
                self.assertEqual(
                    (page.arch, page.byte_order, page.pointer_bits),
                    (ARCH_MIPS, snapshot_abi.byte_order, 32),
                )
                self.assertEqual(
                    (task.task, task.mm, task.pgd_kernel_va),
                    (MIPS_ROOT, 0x82345000, 0x83456000),
                )
                # Non-pointer fields remain full-width even on a 32-bit target.
                self.assertEqual(task.start_cookie, 0x1020304050607080)
                self.assertEqual(task.task_flags, 0x8877665544332211)

    def test_oracle_accepts_an_explicit_mips_snapshot_abi(self):
        page = mips_response(
            MIPS32EL_SNAPSHOT_ABI,
            [mips_task_bytes(MIPS32EL_SNAPSHOT_ABI.byte_order)],
        )
        oracle = ProbeOracle(
            lambda cursor: page,
            snapshot_abi=MIPS32EL_SNAPSHOT_ABI,
        )

        snapshot = oracle.snapshot()

        self.assertEqual(snapshot.tasks[0].identity, TaskId(1, 1))
        self.assertEqual(snapshot.tasks[0].auxv, b"\0" * 8)

    def test_oracle_default_remains_aarch64_only(self):
        page = mips_response(
            MIPS32EL_SNAPSHOT_ABI,
            [mips_task_bytes(MIPS32EL_SNAPSHOT_ABI.byte_order)],
        )
        with self.assertRaisesRegex(ProbeDecodeError, "unsupported probe architecture"):
            ProbeOracle(lambda cursor: page).snapshot()

    def test_mips32_rejects_non_zero_extended_pointer_slots(self):
        record = mips_task_bytes(">", task=(1 << 32) | MIPS_ROOT)
        with self.assertRaisesRegex(ProbeDecodeError, r"task\[0\]\.task.*32-bit"):
            decode_response(
                mips_response(MIPS32BE_SNAPSHOT_ABI, [record]),
                expected_abi=MIPS32BE_SNAPSHOT_ABI,
            )

    def test_malformed_truncated_and_corrupt_responses(self):
        with self.assertRaisesRegex(ProbeDecodeError, "truncated"):
            decode_response(b"short")
        malformed = bytearray(response([task_bytes()]))
        struct.pack_into("<I", malformed, 24, 2)  # record_count
        with self.assertRaisesRegex(ProbeDecodeError, "inconsistent response length"):
            decode_response(bytes(malformed))
        with self.assertRaisesRegex(ProbeStatusError, "status -3"):
            decode_response(response([], status=-3))

    def test_pagination_validation(self):
        second = ROOT + 0x1000
        pages = {
            0: response([task_bytes()], more=True, cursor=second),
            second: response([task_bytes(task=second, leader=second, pid=2, tgid=2)], root=ROOT),
        }
        snapshot = decode_paginated(pages.__getitem__)
        self.assertEqual([task.pid for task in snapshot.tasks], [1, 2])
        cycle = {0: pages[0], second: response(
            [task_bytes(task=second, leader=second, pid=2, tgid=2)],
            root=ROOT, more=True, cursor=second)}
        with self.assertRaisesRegex(ProbeDecodeError, "cursor cycle"):
            decode_paginated(cycle.__getitem__)


class ProbeOracleTests(unittest.TestCase):
    def test_attached_memory_reader_is_bound_to_the_same_decoded_snapshot(self):
        class Reader:
            def __init__(self):
                self.snapshot = None
                self.calls = []

            def bind_snapshot(self, snapshot):
                self.snapshot = snapshot

            def read_memory(self, task, address, length):
                self.calls.append((task, address, length))
                return b"x" * length

        reader = Reader()
        oracle = ProbeOracle(lambda cursor: response([task_bytes()]), memory_reader=reader)
        task = oracle.snapshot().tasks[0]
        self.assertEqual(reader.snapshot.tasks[0].task, ROOT)
        self.assertEqual(oracle.read_memory(task, 0x1000, 3), b"xxx")
        self.assertEqual(reader.calls, [(task, 0x1000, 3)])

    def test_native_auxv_and_pgd_is_not_mislabelled_physical(self):
        oracle = ProbeOracle(lambda cursor: response([task_bytes()]))
        snapshot = oracle.snapshot()
        task = snapshot.tasks[0]
        self.assertEqual(task.identity, TaskId(1, 1))
        self.assertIsNone(task.page_table_root)
        self.assertEqual(oracle.pgd_kernel_va(task), 0xffff800003000000)
        pairs = struct.iter_unpack("<QQ", task.auxv)
        self.assertEqual(list(pairs), [(3, 0x400040), (9, 0x4064d0), (0, 0)])

    def test_compat32_auxv_uses_32bit_words(self):
        record = task_bytes(
            abi=32, aux_mask=(1 << 0) | (1 << 5) | (1 << 6),
            aux=(0x10034, 0, 0, 0, 0, 0x17c50, 0x7fff1234, 0, 0, 0))
        task = ProbeOracle(lambda cursor: response([record])).snapshot().tasks[0]
        self.assertEqual(
            list(struct.iter_unpack("<II", task.auxv)),
            [(3, 0x10034), (9, 0x17c50), (25, 0x7fff1234), (0, 0)])

    def test_stable_identity_includes_start_cookie(self):
        records = [task_bytes(start=100), task_bytes(start=100), task_bytes(start=101)]
        index = 0

        def fetch(cursor):
            return response([records[index]])

        oracle = ProbeOracle(fetch)
        first_snapshot = oracle.snapshot()
        first = first_snapshot.tasks[0]
        index = 1
        second_snapshot = oracle.snapshot()
        second = second_snapshot.tasks[0]
        index = 2
        reused_snapshot = oracle.snapshot()
        reused = reused_snapshot.tasks[0]
        self.assertEqual(first.task_cookie, second.task_cookie)
        self.assertNotEqual(first.task_cookie, reused.task_cookie)
        self.assertEqual(
            (first_snapshot.generation, second_snapshot.generation, reused_snapshot.generation),
            (1, 2, 3))

    def test_oracle_fetches_all_pages_and_backends_are_explicit(self):
        second = ROOT + 0x1000
        calls = []
        pages = {
            0: response([task_bytes()], more=True, cursor=second),
            second: response([task_bytes(task=second, leader=second, pid=42, tgid=42)], root=ROOT),
        }

        def fetch(cursor):
            calls.append(cursor)
            return pages[cursor]

        oracle = ProbeOracle(fetch, lambda record: f"/proc/{record.tgid}/exe")
        snapshot = oracle.snapshot()
        self.assertEqual(calls, [0, second])
        self.assertEqual([task.identity for task in snapshot.tasks], [TaskId(1, 1), TaskId(42, 42)])
        self.assertEqual(snapshot.tasks[1].executable, "/proc/42/exe")
        with self.assertRaises(NotImplementedError):
            oracle.read_memory(snapshot.tasks[0], 0, 1)
        with self.assertRaises(NotImplementedError):
            oracle.read_registers(snapshot.tasks[0])


if __name__ == "__main__":
    unittest.main()
