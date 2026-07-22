from __future__ import annotations

from pathlib import Path
import struct
import tempfile
import unittest

from probe.fixed_profile import FixedProfileError, scratch_gpas
from tests.test_probe_scratch import (
    BSS_GVA,
    CODE_GVA,
    MIPS_BSS_GVA,
    MIPS_CODE_GVA,
    PAGE_SIZE,
    _elf_with_scratch,
    _mips_elf_with_scratch,
)
from tests.test_viros_managed_scratch_mappings import _rename_code_start_to_text


def document(arch: str, addresses: tuple[int, int, int]) -> dict:
    return {
        "arch": arch,
        "page_size": PAGE_SIZE,
        "regions": {
            name: {"gva": address, "size": PAGE_SIZE}
            for name, address in zip(("code", "data", "stack"), addresses)
        },
    }


class FixedProfileMappingTests(unittest.TestCase):
    def paths(self, root: Path, image: bytes, boot: bytes = b"boot"):
        vmlinux, kernel = root / "vmlinux", root / "kernel"
        vmlinux.write_bytes(image)
        kernel.write_bytes(boot)
        return vmlinux, kernel

    def test_mmips_uses_kseg0_identity(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            vmlinux, kernel = self.paths(root, _mips_elf_with_scratch())
            result = scratch_gpas(
                "mmips", vmlinux, kernel,
                document(
                    "mmips",
                    (MIPS_CODE_GVA, MIPS_BSS_GVA, MIPS_BSS_GVA + PAGE_SIZE),
                ),
            )
            self.assertEqual(result["code"], MIPS_CODE_GVA - 0x80000000)
            self.assertEqual(
                result["stack"], MIPS_BSS_GVA + PAGE_SIZE - 0x80000000
            )

    def test_x86_uses_vmlinux_load_segments(self):
        image = bytearray(_elf_with_scratch())
        struct.pack_into("<H", image, 18, 62)
        phoff = len(image)
        image.extend(
            struct.pack("<IIQQQQQQ", 1, 5, 0, CODE_GVA, 0x1000000, PAGE_SIZE, PAGE_SIZE, PAGE_SIZE)
            + struct.pack("<IIQQQQQQ", 1, 6, 0, BSS_GVA, 0x2000000, 0, 2 * PAGE_SIZE, PAGE_SIZE)
        )
        struct.pack_into("<Q", image, 32, phoff)
        struct.pack_into("<HH", image, 54, 56, 2)
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            vmlinux, kernel = self.paths(root, bytes(image))
            self.assertEqual(
                scratch_gpas(
                    "x86_64", vmlinux, kernel,
                    document("x86_64", (CODE_GVA, BSS_GVA, BSS_GVA + PAGE_SIZE)),
                ),
                {"code": 0x1000000, "data": 0x2000000, "stack": 0x2001000},
            )

    def test_aarch64_uses_selected_image_text_offset(self):
        text = 0xFFFF800012080000
        image = bytearray(_elf_with_scratch(code_gva=text))
        _rename_code_start_to_text(image)
        boot = bytearray(64)
        struct.pack_into("<Q", boot, 8, 0x280000)
        boot[56:60] = b"ARM\x64"
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            vmlinux, kernel = self.paths(root, bytes(image), bytes(boot))
            self.assertEqual(
                scratch_gpas(
                    "aarch64", vmlinux, kernel,
                    document("aarch64", (text, text + PAGE_SIZE, text + 2 * PAGE_SIZE)),
                ),
                {"code": 0x40280000, "data": 0x40281000, "stack": 0x40282000},
            )

    def test_rejects_profile_architecture_mismatch(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            vmlinux, kernel = self.paths(root, _mips_elf_with_scratch())
            with self.assertRaisesRegex(FixedProfileError, "architecture"):
                scratch_gpas(
                    "mmips", vmlinux, kernel,
                    document("arm", (CODE_GVA, BSS_GVA, BSS_GVA + PAGE_SIZE)),
                )


if __name__ == "__main__":
    unittest.main()
