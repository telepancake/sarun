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
BUILD_ID = bytes.fromhex("0123456789abcdef")


def _aligned(image: bytearray, alignment: int = 8) -> None:
    image.extend(b"\0" * (-len(image) & (alignment - 1)))


def _elf_with_scratch(
    *, code_gva: int = CODE_GVA, code_flags: int = 0x6,
    bss_flags: int = 0x3, missing_symbol: str | None = None,
) -> bytes:
    note = struct.pack("<III", 4, len(BUILD_ID), 3) + b"GNU\0" + BUILD_ID
    note += b"\0" * (-len(note) & 3)

    symbol_values = {
        "__viros_scratch_code_start": (code_gva, 2),
        "__viros_scratch_code_end": (code_gva + PAGE_SIZE, 2),
        "__viros_scratch_data_start": (BSS_GVA, 3),
        "__viros_scratch_data_end": (BSS_GVA + PAGE_SIZE, 3),
        "__viros_scratch_stack_start": (BSS_GVA + PAGE_SIZE, 3),
        "__viros_scratch_stack_end": (BSS_GVA + 2 * PAGE_SIZE, 3),
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
            "<IBBHQQ", symbol_name_offsets[name], 0x11, 0,
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
    image.extend(bytes.fromhex("60ca2ad4") * (PAGE_SIZE // 4))
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
        "<IIQQQQIIQQ", name_offset(b".note"), 7, 0x2, 0,
        note_offset, len(note), 0, 0, 4, 0,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", name_offset(b".text"), 1, code_flags, code_gva,
        text_offset, PAGE_SIZE, 0, 0, 4096, 0,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", name_offset(b".bss"), 8, bss_flags, BSS_GVA,
        0, PAGE_SIZE * 2, 0, 0, 4096, 0,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", name_offset(b".symtab"), 2, 0, 0,
        symtab_offset, len(symtab), 5, 1, 8, 24,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", name_offset(b".strtab"), 3, 0, 0,
        strtab_offset, len(strings), 0, 0, 1, 0,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", name_offset(b".shstrtab"), 3, 0, 0,
        shstrtab_offset, len(section_names), 0, 0, 1, 0,
    ))
    image.extend(b"".join(sections))

    header = b"\x7fELF" + bytes((2, 1, 1, 0)) + b"\0" * 8
    header += struct.pack(
        "<HHIQQQIHHHHHH", 2, 183, 1, 0, 0, section_offset, 0,
        64, 0, 0, 64, len(sections), 6,
    )
    image[:64] = header
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
        self.assertIn("brk #0x5653", source)
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
