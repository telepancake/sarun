from __future__ import annotations

from contextlib import redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import struct
import tempfile
import unittest

from probe.scratch import scratch_tool


ROOT = Path(__file__).resolve().parents[1]
PAGE_SIZE = 4096
CODE_GVA = 0xFFFF800080100000
BSS_GVA = 0xFFFF800082000000
MIPS_CODE_GVA = 0x80100000
MIPS_BSS_GVA = 0x82000000
BUILD_ID = bytes.fromhex("0123456789abcdef")


def _aligned(image: bytearray, alignment: int = 8) -> None:
    image.extend(b"\0" * (-len(image) & (alignment - 1)))


def _elf_with_scratch(
    *, code_gva: int = CODE_GVA, code_flags: int = 0x6,
    bss_flags: int = 0x3, missing_symbol: str | None = None,
    byte_order: str = "<", machine: int = 183, page_size: int = PAGE_SIZE,
) -> bytes:
    encoding = 1 if byte_order == "<" else 2
    note = struct.pack(byte_order + "III", 4, len(BUILD_ID), 3)
    note += b"GNU\0" + BUILD_ID
    note += b"\0" * (-len(note) & 3)

    symbol_values = {
        "__viros_scratch_code_start": (code_gva, 2),
        "__viros_scratch_code_end": (code_gva + page_size, 2),
        "__viros_scratch_data_start": (BSS_GVA, 3),
        "__viros_scratch_data_end": (BSS_GVA + page_size, 3),
        "__viros_scratch_stack_start": (BSS_GVA + page_size, 3),
        "__viros_scratch_stack_end": (BSS_GVA + 2 * page_size, 3),
    }
    if missing_symbol:
        del symbol_values[missing_symbol]

    strings = bytearray(b"\0")
    symbol_name_offsets = {}
    for name in symbol_values:
        symbol_name_offsets[name] = len(strings)
        strings.extend(name.encode("ascii") + b"\0")
    symtab = bytearray(b"\0" * 24)
    for name, (value, section_index) in symbol_values.items():
        symtab.extend(struct.pack(
            byte_order + "IBBHQQ", symbol_name_offsets[name], 0x11, 0,
            section_index, value, 0,
        ))

    section_names = (
        b"\0.note.gnu.build-id\0.text\0.bss\0.symtab\0.strtab\0.shstrtab\0"
    )
    image = bytearray(b"\0" * 64)
    note_offset = len(image)
    image.extend(note)
    _aligned(image)
    text_offset = len(image)
    # AArch64 BRK #0x5653, repeated.  Contents only need to make a valid ELF
    # fixture; the source-level trap policy is checked separately below.
    image.extend(bytes.fromhex("60ca2ad4") * (page_size // 4))
    _aligned(image)
    symtab_offset = len(image)
    image.extend(symtab)
    strtab_offset = len(image)
    image.extend(strings)
    shstrtab_offset = len(image)
    image.extend(section_names)
    _aligned(image)
    section_offset = len(image)

    def name_offset(name: bytes) -> int:
        return section_names.index(name)

    sections = [b"\0" * 64]
    sections.append(struct.pack(
        byte_order + "IIQQQQIIQQ", name_offset(b".note"), 7, 0x2, 0,
        note_offset, len(note), 0, 0, 4, 0,
    ))
    sections.append(struct.pack(
        byte_order + "IIQQQQIIQQ", name_offset(b".text"), 1, code_flags,
        code_gva, text_offset, page_size, 0, 0, page_size, 0,
    ))
    sections.append(struct.pack(
        byte_order + "IIQQQQIIQQ", name_offset(b".bss"), 8, bss_flags,
        BSS_GVA, 0, page_size * 2, 0, 0, page_size, 0,
    ))
    sections.append(struct.pack(
        byte_order + "IIQQQQIIQQ", name_offset(b".symtab"), 2, 0, 0,
        symtab_offset, len(symtab), 5, 1, 8, 24,
    ))
    sections.append(struct.pack(
        byte_order + "IIQQQQIIQQ", name_offset(b".strtab"), 3, 0, 0,
        strtab_offset, len(strings), 0, 0, 1, 0,
    ))
    sections.append(struct.pack(
        byte_order + "IIQQQQIIQQ", name_offset(b".shstrtab"), 3, 0, 0,
        shstrtab_offset, len(section_names), 0, 0, 1, 0,
    ))
    image.extend(b"".join(sections))

    header = b"\x7fELF" + bytes((2, encoding, 1, 0)) + b"\0" * 8
    header += struct.pack(
        byte_order + "HHIQQQIHHHHHH", 2, machine, 1, 0, 0,
        section_offset, 0,
        64, 0, 0, 64, len(sections), 6,
    )
    image[:64] = header
    return bytes(image)


def _mips_elf_with_scratch(
    *, code_gva: int = MIPS_CODE_GVA, bss_gva: int = MIPS_BSS_GVA,
    byte_order: str = "<", code_flags: int = 0x6, bss_flags: int = 0x3,
    machine: int = 8, page_size: int = PAGE_SIZE,
) -> bytes:
    encoding = 1 if byte_order == "<" else 2
    note = struct.pack(byte_order + "III", 4, len(BUILD_ID), 3)
    note += b"GNU\0" + BUILD_ID
    note += b"\0" * (-len(note) & 3)
    symbol_values = {
        "__viros_scratch_code_start": (code_gva, 2),
        "__viros_scratch_code_end": (code_gva + page_size, 2),
        "__viros_scratch_data_start": (bss_gva, 3),
        "__viros_scratch_data_end": (bss_gva + page_size, 3),
        "__viros_scratch_stack_start": (bss_gva + page_size, 3),
        "__viros_scratch_stack_end": (bss_gva + 2 * page_size, 3),
    }
    strings = bytearray(b"\0")
    symbol_name_offsets = {}
    for name in symbol_values:
        symbol_name_offsets[name] = len(strings)
        strings.extend(name.encode("ascii") + b"\0")
    symtab = bytearray(b"\0" * 16)
    for name, (value, section_index) in symbol_values.items():
        symtab.extend(struct.pack(
            byte_order + "IIIBBH", symbol_name_offsets[name], value, 0,
            0x11, 0, section_index,
        ))

    section_names = (
        b"\0.note.gnu.build-id\0.text\0.bss\0.symtab\0.strtab\0.shstrtab\0"
    )
    image = bytearray(b"\0" * 64)
    note_offset = len(image)
    image.extend(note)
    _aligned(image)
    text_offset = len(image)
    image.extend(struct.pack(byte_order + "I", 0x0015940D) * (page_size // 4))
    _aligned(image)
    symtab_offset = len(image)
    image.extend(symtab)
    strtab_offset = len(image)
    image.extend(strings)
    shstrtab_offset = len(image)
    image.extend(section_names)
    _aligned(image)
    section_offset = len(image)

    def name_offset(name: bytes) -> int:
        return section_names.index(name)

    sections = [b"\0" * 40]
    sections.append(struct.pack(
        byte_order + "IIIIIIIIII", name_offset(b".note"), 7, 0x2, 0,
        note_offset, len(note), 0, 0, 4, 0,
    ))
    sections.append(struct.pack(
        byte_order + "IIIIIIIIII", name_offset(b".text"), 1, code_flags,
        code_gva, text_offset, page_size, 0, 0, page_size, 0,
    ))
    sections.append(struct.pack(
        byte_order + "IIIIIIIIII", name_offset(b".bss"), 8, bss_flags,
        bss_gva, 0, page_size * 2, 0, 0, page_size, 0,
    ))
    sections.append(struct.pack(
        byte_order + "IIIIIIIIII", name_offset(b".symtab"), 2, 0, 0,
        symtab_offset, len(symtab), 5, 1, 4, 16,
    ))
    sections.append(struct.pack(
        byte_order + "IIIIIIIIII", name_offset(b".strtab"), 3, 0, 0,
        strtab_offset, len(strings), 0, 0, 1, 0,
    ))
    sections.append(struct.pack(
        byte_order + "IIIIIIIIII", name_offset(b".shstrtab"), 3, 0, 0,
        shstrtab_offset, len(section_names), 0, 0, 1, 0,
    ))
    image.extend(b"".join(sections))

    header = b"\x7fELF" + bytes((1, encoding, 1, 0)) + b"\0" * 8
    header += struct.pack(
        byte_order + "HHIIIIIHHHHHH", 2, machine, 1, 0, 0,
        section_offset, 0,
        52, 0, 0, 40, len(sections), 6,
    )
    image[:52] = header
    return bytes(image)


class ScratchToolTests(unittest.TestCase):
    def test_discovers_exact_boundaries_and_kernel_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            vmlinux = Path(temporary) / "vmlinux"
            vmlinux.write_bytes(_elf_with_scratch())

            result = scratch_tool.discover_regions(vmlinux)

            self.assertEqual(result["schema"], "viros-scratch-regions-v1")
            self.assertEqual(result["page_size"], PAGE_SIZE)
            self.assertEqual(result["regions"]["code"]["gva"], CODE_GVA)
            self.assertEqual(result["regions"]["data"]["size"], PAGE_SIZE)
            self.assertEqual(
                result["regions"]["stack"]["gva"], BSS_GVA + PAGE_SIZE,
            )
            self.assertEqual(result["vmlinux"]["build_id"], BUILD_ID.hex())
            self.assertEqual(
                result["vmlinux"]["sha256"],
                hashlib.sha256(vmlinux.read_bytes()).hexdigest(),
            )
            for region in result["regions"].values():
                self.assertNotIn("gpa", region)

    def test_discovers_x86_64_four_kib_regions_without_assuming_gpa(self):
        with tempfile.TemporaryDirectory() as temporary:
            vmlinux = Path(temporary) / "vmlinux"
            vmlinux.write_bytes(_elf_with_scratch(machine=scratch_tool.EM_X86_64))

            result = scratch_tool.discover_regions(vmlinux)

            self.assertEqual(result["arch"], "x86_64")
            self.assertEqual(result["page_size"], PAGE_SIZE)
            self.assertEqual(set(result["regions"]), {"code", "data", "stack"})
            self.assertEqual(result["regions"]["code"]["gva"], CODE_GVA)
            self.assertEqual(result["vmlinux"]["build_id"], BUILD_ID.hex())
            for region in result["regions"].values():
                self.assertNotIn("gpa", region)

    def test_discovers_arm_four_kib_regions_without_assuming_gpa(self):
        with tempfile.TemporaryDirectory() as temporary:
            vmlinux = Path(temporary) / "vmlinux"
            vmlinux.write_bytes(_mips_elf_with_scratch(machine=scratch_tool.EM_ARM))

            result = scratch_tool.discover_regions(vmlinux)

            self.assertEqual(result["arch"], "arm")
            self.assertEqual(result["page_size"], PAGE_SIZE)
            self.assertEqual(set(result["regions"]), {"code", "data", "stack"})
            self.assertEqual(result["regions"]["code"]["gva"], MIPS_CODE_GVA)
            self.assertEqual(result["vmlinux"]["build_id"], BUILD_ID.hex())
            for region in result["regions"].values():
                self.assertNotIn("gpa", region)

    def test_x86_64_requires_elf64_little_endian_and_four_kib_regions(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            elf32 = directory / "elf32"
            elf32.write_bytes(_mips_elf_with_scratch(
                machine=scratch_tool.EM_X86_64,
            ))
            with self.assertRaisesRegex(
                scratch_tool.ScratchError, "x86_64.*ELF64 little-endian",
            ):
                scratch_tool.discover_regions(elf32)

            big_endian = directory / "big-endian"
            big_endian.write_bytes(_elf_with_scratch(
                machine=scratch_tool.EM_X86_64, byte_order=">",
            ))
            with self.assertRaisesRegex(
                scratch_tool.ScratchError, "x86_64.*ELF64 little-endian",
            ):
                scratch_tool.discover_regions(big_endian)

            large_regions = directory / "large-regions"
            large_regions.write_bytes(_elf_with_scratch(
                machine=scratch_tool.EM_X86_64, page_size=16384,
            ))
            with self.assertRaisesRegex(
                scratch_tool.ScratchError, "supported x86_64 page",
            ):
                scratch_tool.discover_regions(large_regions)

            overflow = directory / "runtime-overflow"
            overflow.write_bytes(_elf_with_scratch(
                machine=scratch_tool.EM_X86_64,
            ))
            with self.assertRaisesRegex(scratch_tool.ScratchError, "64-bit"):
                scratch_tool.discover_regions(
                    overflow, scratch_tool.UINT64_LIMIT - PAGE_SIZE,
                )

    def test_arm_requires_elf32_little_endian_and_four_kib_regions(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            elf64 = directory / "elf64"
            elf64.write_bytes(_elf_with_scratch(machine=scratch_tool.EM_ARM))
            with self.assertRaisesRegex(
                scratch_tool.ScratchError, "arm.*ELF32 little-endian",
            ):
                scratch_tool.discover_regions(elf64)

            big_endian = directory / "big-endian"
            big_endian.write_bytes(_mips_elf_with_scratch(
                machine=scratch_tool.EM_ARM, byte_order=">",
            ))
            with self.assertRaisesRegex(
                scratch_tool.ScratchError, "arm.*ELF32 little-endian",
            ):
                scratch_tool.discover_regions(big_endian)

            large_regions = directory / "large-regions"
            large_regions.write_bytes(_mips_elf_with_scratch(
                machine=scratch_tool.EM_ARM, page_size=16384,
            ))
            with self.assertRaisesRegex(
                scratch_tool.ScratchError, "supported arm page",
            ):
                scratch_tool.discover_regions(large_regions)

            overflow = directory / "runtime-overflow"
            overflow.write_bytes(_mips_elf_with_scratch(
                machine=scratch_tool.EM_ARM,
            ))
            with self.assertRaisesRegex(scratch_tool.ScratchError, "32-bit"):
                scratch_tool.discover_regions(overflow, 0x80000000)

    def test_discovers_mmips_kseg0_regions_and_derives_physical_addresses(self):
        with tempfile.TemporaryDirectory() as temporary:
            vmlinux = Path(temporary) / "vmlinux"
            vmlinux.write_bytes(_mips_elf_with_scratch())

            result = scratch_tool.discover_regions(vmlinux, 2 * PAGE_SIZE)

            self.assertEqual(result["arch"], "mmips")
            self.assertEqual(result["page_size"], PAGE_SIZE)
            self.assertEqual(
                result["regions"]["code"]["gva"],
                MIPS_CODE_GVA + 2 * PAGE_SIZE,
            )
            self.assertEqual(
                result["regions"]["code"]["gpa"],
                MIPS_CODE_GVA + 2 * PAGE_SIZE - 0x80000000,
            )
            self.assertEqual(
                result["regions"]["stack"]["gpa"],
                MIPS_BSS_GVA + PAGE_SIZE + 2 * PAGE_SIZE - 0x80000000,
            )
            self.assertEqual(result["vmlinux"]["build_id"], BUILD_ID.hex())

    def test_mmips_requires_elf32_little_endian_and_kseg0(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            elf64 = directory / "elf64"
            elf64_image = bytearray(_elf_with_scratch())
            struct.pack_into("<H", elf64_image, 18, 8)
            elf64.write_bytes(elf64_image)
            with self.assertRaisesRegex(
                scratch_tool.ScratchError, "ELF32 little-endian",
            ):
                scratch_tool.discover_regions(elf64)

            big_endian = directory / "big-endian"
            big_endian.write_bytes(_mips_elf_with_scratch(byte_order=">"))
            with self.assertRaisesRegex(
                scratch_tool.ScratchError, "ELF32 little-endian",
            ):
                scratch_tool.discover_regions(big_endian)

            outside = directory / "outside-kseg0"
            outside.write_bytes(_mips_elf_with_scratch(code_gva=0x7FF00000))
            with self.assertRaisesRegex(scratch_tool.ScratchError, "KSEG0"):
                scratch_tool.discover_regions(outside)

            relocated_outside = directory / "relocated-outside-kseg0"
            relocated_outside.write_bytes(_mips_elf_with_scratch())
            with self.assertRaisesRegex(scratch_tool.ScratchError, "KSEG0"):
                scratch_tool.discover_regions(relocated_outside, 0x20000000)

    def test_cli_applies_page_aligned_runtime_offset_and_writes_json(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            vmlinux = directory / "vmlinux"
            output = directory / "scratch.json"
            vmlinux.write_bytes(_elf_with_scratch())
            stdout = io.StringIO()

            with redirect_stdout(stdout):
                scratch_tool.main([
                    str(vmlinux), "--runtime-offset", "0x200000",
                    "--output", str(output),
                ])

            published = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(published, json.loads(stdout.getvalue()))
            self.assertEqual(
                published["regions"]["code"]["gva"], CODE_GVA + 0x200000,
            )
            self.assertEqual(
                published["regions"]["code"]["link_gva"], CODE_GVA,
            )

    def test_rejects_missing_symbol_and_unsafe_section_permissions(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            missing = directory / "missing"
            missing.write_bytes(_elf_with_scratch(
                missing_symbol="__viros_scratch_stack_end",
            ))
            with self.assertRaisesRegex(scratch_tool.ScratchError, "missing defined"):
                scratch_tool.discover_regions(missing)

            writable_code = directory / "writable-code"
            writable_code.write_bytes(_elf_with_scratch(code_flags=0x7))
            with self.assertRaisesRegex(scratch_tool.ScratchError, "unsafe ELF flags"):
                scratch_tool.discover_regions(writable_code)

            executable_bss = directory / "executable-bss"
            executable_bss.write_bytes(_elf_with_scratch(bss_flags=0x7))
            with self.assertRaisesRegex(scratch_tool.ScratchError, "unsafe ELF flags"):
                scratch_tool.discover_regions(executable_bss)

    def test_rejects_unaligned_symbol_and_runtime_offset(self):
        with tempfile.TemporaryDirectory() as temporary:
            vmlinux = Path(temporary) / "vmlinux"
            vmlinux.write_bytes(_elf_with_scratch(code_gva=CODE_GVA + 4))
            with self.assertRaisesRegex(scratch_tool.ScratchError, "not page aligned"):
                scratch_tool.discover_regions(vmlinux)

            vmlinux.write_bytes(_elf_with_scratch())
            with self.assertRaisesRegex(scratch_tool.ScratchError, "offset.*page aligned"):
                scratch_tool.discover_regions(vmlinux, 1)

    def test_kernel_object_is_passive_built_in_and_trap_filled(self):
        source = (ROOT / "probe/scratch/kernel/viros_scratch.S").read_text(
            encoding="utf-8"
        )
        kbuild = (ROOT / "probe/scratch/kernel/Kbuild").read_text(encoding="utf-8")

        self.assertIn("obj-y += viros_scratch.o", kbuild)
        self.assertNotIn("obj-m", kbuild)
        self.assertIn(".rept VIROS_SCRATCH_PAGE_SIZE / 4", source)
        self.assertNotIn(".text.viros_scratch", source)
        self.assertGreaterEqual(source.count('.pushsection .text,"ax"'), 3)
        self.assertIn("brk #0x5653", source)
        self.assertIn("#elif defined(CONFIG_MIPS)", source)
        self.assertIn(".set nomips16", source)
        self.assertIn(".set nomicromips", source)
        self.assertIn(".set mips32", source)
        self.assertIn(".word 0x0015940d", source)
        self.assertNotIn(".init", source)
        self.assertNotIn(" bl ", source)
        self.assertNotIn("svc ", source)
        for suffix in (
            "code_start", "code_end", "data_start", "data_end",
            "stack_start", "stack_end",
        ):
            self.assertIn(f"__viros_scratch_{suffix}", source)


if __name__ == "__main__":
    unittest.main()
