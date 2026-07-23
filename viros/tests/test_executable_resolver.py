from __future__ import annotations

from dataclasses import dataclass
import hashlib
import struct
import unittest

from callgate.architectures import AARCH64, ARMV7, MIPS32EL_MMIPS, X86_64
from inferiors.executable_resolver import (
    AT_EXECFN,
    AT_NULL,
    AT_PHDR,
    AT_PHENT,
    AT_PHNUM,
    ET_DYN,
    ET_EXEC,
    LiveExecutableResolver,
    MAX_PROGRAM_HEADERS,
    PT_LOAD,
    PT_NOTE,
    PT_PHDR,
)
from inferiors.linux_oracle import Snapshot, TaskId, TaskSnapshot
from inferiors.rsp_proxy import RspFacade
from inferiors.sarun_provider import DebugExecutable
from probe.elf_load_identity import elf_load_identity


@dataclass(frozen=True)
class MemoryElf:
    data: bytes
    address: int
    phdr: int
    phent: int
    phnum: int
    elf_class: int
    machine: int


class Memory:
    def __init__(self, *regions: tuple[int, bytes]) -> None:
        self.regions = list(regions)
        self.reads: list[tuple[int, int]] = []

    def read(self, task: TaskSnapshot, address: int, length: int) -> bytes:
        self.reads.append((address, length))
        for start, data in self.regions:
            if start <= address and address + length <= start + len(data):
                offset = address - start
                return data[offset : offset + length]
        raise OSError("unmapped")


def _align4(data: bytes) -> bytes:
    return data + b"\0" * (-len(data) & 3)


def memory_elf(
    elf_class: int,
    machine: int,
    build_id: bytes | None,
    *,
    elf_type: int = ET_DYN,
    writable: bool = False,
) -> MemoryElf:
    header_size = 64 if elf_class == 64 else 52
    phent = 56 if elf_class == 64 else 32
    phnum = 3 if build_id is not None else 2
    table_size = phent * phnum
    note = b""
    if build_id is not None:
        note = _align4(struct.pack("<III", 4, len(build_id), 3) + b"GNU\0")
        note += _align4(build_id)
    note_offset = 0x180
    load_size = 0x240
    link_base = 0 if elf_type == ET_DYN else (0x400000 if elf_class == 64 else 0x10000)
    load_bias = (
        (0x555500000000 if elf_class == 64 else 0x50000000)
        if elf_type == ET_DYN
        else 0
    )
    ident = b"\x7fELF" + bytes((2 if elf_class == 64 else 1, 1, 1)) + b"\0" * 9
    if elf_class == 64:
        header = ident + struct.pack(
            "<HHIQQQIHHHHHH",
            elf_type,
            machine,
            1,
            link_base + 0x100,
            header_size,
            0,
            0,
            header_size,
            phent,
            phnum,
            0,
            0,
            0,
        )
        def encode_phdr(kind, flags, offset, virtual, filesz, memsz, align):
            return struct.pack(
                "<IIQQQQQQ",
                kind,
                flags,
                offset,
                virtual,
                virtual,
                filesz,
                memsz,
                align,
            )
    else:
        header = ident + struct.pack(
            "<HHIIIIIHHHHHH",
            elf_type,
            machine,
            1,
            link_base + 0x100,
            header_size,
            0,
            0,
            header_size,
            phent,
            phnum,
            0,
            0,
            0,
        )
        def encode_phdr(kind, flags, offset, virtual, filesz, memsz, align):
            return struct.pack(
                "<IIIIIIII",
                kind,
                offset,
                virtual,
                virtual,
                filesz,
                memsz,
                flags,
                align,
            )
    phdrs = [
        encode_phdr(
            PT_PHDR,
            4,
            header_size,
            link_base + header_size,
            table_size,
            table_size,
            8 if elf_class == 64 else 4,
        ),
        encode_phdr(
            PT_LOAD,
            5 | (2 if writable else 0),
            0,
            link_base,
            load_size,
            load_size,
            0x1000,
        ),
    ]
    if build_id is not None:
        phdrs.append(
            encode_phdr(
                PT_NOTE,
                4,
                note_offset,
                link_base + note_offset,
                len(note),
                len(note),
                4,
            )
        )
    data = bytearray(load_size)
    data[:header_size] = header
    data[header_size : header_size + table_size] = b"".join(phdrs)
    data[note_offset : note_offset + len(note)] = note
    address = load_bias + link_base
    return MemoryElf(
        bytes(data),
        address,
        address + header_size,
        phent,
        phnum,
        elf_class,
        machine,
    )


def auxv(
    image: MemoryElf,
    *,
    execfn: int | None = None,
    phnum: int | None = None,
) -> bytes:
    code = "Q" if image.elf_class == 64 else "I"
    pairs = [
        (AT_PHDR, image.phdr),
        (AT_PHENT, image.phent),
        (AT_PHNUM, image.phnum if phnum is None else phnum),
    ]
    if execfn is not None:
        pairs.append((AT_EXECFN, execfn))
    pairs.append((AT_NULL, 0))
    return b"".join(struct.pack("<" + code * 2, *pair) for pair in pairs)


def task_for(
    image: MemoryElf, *, execfn: int | None = None, cookie: int = 7
) -> TaskSnapshot:
    return TaskSnapshot(
        TaskId(42, 42),
        cookie,
        "program",
        "",
        auxv(image, execfn=execfn),
        current_cpu=0,
    )


def catalog_row(
    path: bytes,
    identity: bytes,
    *,
    elf_class: int,
    machine: int,
) -> DebugExecutable:
    encoded_identity = (
        identity.hex().encode("ascii")
        if not all(byte in b"0123456789abcdef" for byte in identity)
        else identity
    )
    return DebugExecutable(
        path,
        encoded_identity,
        hashlib.sha256(path).digest(),
        1,
        b"debug/program",
        hashlib.sha256(identity).digest(),
        1,
        elf_class,
        machine,
    )


class LiveExecutableResolverTests(unittest.TestCase):
    def test_gnu_build_id_resolves_all_supported_little_endian_targets(self):
        cases = (
            (X86_64, 64, 62),
            (AARCH64, 64, 183),
            (ARMV7, 32, 40),
            (MIPS32EL_MMIPS, 32, 8),
        )
        for architecture, elf_class, machine in cases:
            with self.subTest(architecture=architecture.name):
                build_id = bytes.fromhex("0123456789abcdef")
                image = memory_elf(elf_class, machine, build_id)
                memory = Memory((image.address, image.data))
                resolver = LiveExecutableResolver(
                    architecture,
                    [
                        catalog_row(
                            b"/usr/sbin/program",
                            build_id,
                            elf_class=elf_class,
                            machine=machine,
                        )
                    ],
                )
                self.assertEqual(
                    resolver.resolve(task_for(image), memory.read),
                    "/usr/sbin/program",
                )
                self.assertTrue(memory.reads)

    def test_unmapped_and_malformed_memory_are_unresolved_and_bounded(self):
        image = memory_elf(64, 62, bytes.fromhex("0123456789abcdef"))
        row = catalog_row(
            b"/bin/program",
            bytes.fromhex("0123456789abcdef"),
            elf_class=64,
            machine=62,
        )
        resolver = LiveExecutableResolver(X86_64, [row])
        self.assertEqual(resolver.resolve(task_for(image), Memory().read), "")

        malformed = task_for(image)
        malformed = TaskSnapshot(
            malformed.identity,
            malformed.task_cookie,
            malformed.comm,
            "",
            auxv(image, phnum=MAX_PROGRAM_HEADERS + 1),
            current_cpu=0,
        )
        memory = Memory((image.address, image.data))
        self.assertEqual(resolver.resolve(malformed, memory.read), "")
        self.assertEqual(memory.reads, [])

        broken = bytearray(image.data)
        # The first note namesz is read from a bounded PT_NOTE and cannot turn
        # into an unbounded follow-up memory request.
        struct.pack_into("<I", broken, 0x180, 0x7FFFFFFF)
        memory = Memory((image.address, bytes(broken)))
        self.assertEqual(resolver.resolve(task_for(image), memory.read), "")
        self.assertLessEqual(max(length for _, length in memory.reads), 64 * 1024)

    def test_ambiguous_identity_requires_matching_execfn_after_identity(self):
        build_id = bytes.fromhex("0123456789abcdef")
        image = memory_elf(64, 62, build_id)
        rows = [
            catalog_row(b"/bin/one", build_id, elf_class=64, machine=62),
            catalog_row(b"/bin/two", build_id, elf_class=64, machine=62),
        ]
        resolver = LiveExecutableResolver(X86_64, rows)
        memory = Memory((image.address, image.data))
        self.assertEqual(resolver.resolve(task_for(image), memory.read), "")

        execfn_address = image.address + len(image.data) + 0x1000
        memory = Memory(
            (image.address, image.data),
            (execfn_address, b"/bin/two\0" + b"\0" * 256),
        )
        self.assertEqual(
            resolver.resolve(task_for(image, execfn=execfn_address), memory.read),
            "/bin/two",
        )

        other_id = bytes.fromhex("fedcba9876543210")
        wrong_identity = LiveExecutableResolver(
            X86_64,
            [catalog_row(b"/bin/two", other_id, elf_class=64, machine=62)],
        )
        self.assertEqual(
            wrong_identity.resolve(
                task_for(image, execfn=execfn_address), memory.read
            ),
            "",
        )

    def test_exact_nonwritable_exec_pt_load_fallback_matches_fingerprint(self):
        image = memory_elf(64, 62, None, elf_type=ET_EXEC)
        fingerprint = elf_load_identity(image.data).fingerprint.encode("ascii")
        resolver = LiveExecutableResolver(
            X86_64,
            [
                catalog_row(
                    b"/bin/static",
                    fingerprint,
                    elf_class=64,
                    machine=62,
                )
            ],
        )
        self.assertEqual(
            resolver.resolve(
                task_for(image), Memory((image.address, image.data)).read
            ),
            "/bin/static",
        )

        writable = memory_elf(64, 62, None, elf_type=ET_EXEC, writable=True)
        self.assertEqual(
            resolver.resolve(
                task_for(writable), Memory((writable.address, writable.data)).read
            ),
            "",
        )

    def test_load_identity_can_follow_an_unmatched_runtime_build_id(self):
        image = memory_elf(
            64,
            62,
            bytes.fromhex("0123456789abcdef"),
            elf_type=ET_EXEC,
        )
        fingerprint = elf_load_identity(image.data).fingerprint.encode("ascii")
        resolver = LiveExecutableResolver(
            X86_64,
            [
                catalog_row(
                    b"/bin/debug-without-note",
                    fingerprint,
                    elf_class=64,
                    machine=62,
                )
            ],
        )
        self.assertEqual(
            resolver.resolve(
                task_for(image), Memory((image.address, image.data)).read
            ),
            "/bin/debug-without-note",
        )

    def test_rsp_snapshot_rechecks_identity_across_exec_with_same_task_cookie(self):
        first_id = bytes.fromhex("0123456789abcdef")
        second_id = bytes.fromhex("fedcba9876543210")
        first = memory_elf(64, 62, first_id)
        second = memory_elf(64, 62, second_id)
        self.assertEqual((first.address, first.phdr), (second.address, second.phdr))
        resolver = LiveExecutableResolver(
            X86_64,
            [
                catalog_row(b"/bin/first", first_id, elf_class=64, machine=62),
                catalog_row(b"/bin/second", second_id, elf_class=64, machine=62),
            ],
        )

        class Oracle:
            generation = 0
            image = first
            read_tids = []

            def snapshot(inner_self):
                inner_self.generation += 1
                leader = TaskSnapshot(
                    TaskId(42, 42),
                    99,
                    "program",
                    "",
                    auxv(inner_self.image),
                )
                current = TaskSnapshot(
                    TaskId(42, 43),
                    99,
                    "program",
                    "",
                    auxv(inner_self.image),
                    current_cpu=0,
                )
                return Snapshot(inner_self.generation, (leader, current))

            def read_memory(inner_self, task, address, length):
                inner_self.read_tids.append(task.identity.tid)
                if task.current_cpu is not None:
                    raise OSError("preferred thread is temporarily unreadable")
                return Memory(
                    (inner_self.image.address, inner_self.image.data)
                ).read(task, address, length)

        oracle = Oracle()
        facade = RspFacade(
            oracle,
            object(),
            b"<target/>",
            executable_resolver=resolver,
        )
        self.assertEqual(
            facade.handle(b"qXfer:exec-file:read:2a:0,100"), b"l/bin/first"
        )
        self.assertEqual(oracle.read_tids[:2], [43, 42])
        oracle.image = second
        facade.refresh()
        self.assertEqual(
            facade.handle(b"qXfer:exec-file:read:2a:0,100"), b"l/bin/second"
        )


if __name__ == "__main__":
    unittest.main()
