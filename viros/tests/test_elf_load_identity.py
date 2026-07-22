from __future__ import annotations

import struct
import unittest

from probe.elf_load_identity import (
    ElfLoadIdentityError,
    elf_load_identity,
    same_loadable_content,
)
from tests.test_newc_userspace import load_elf64


class ElfLoadIdentityTests(unittest.TestCase):
    def test_section_table_stripping_does_not_change_load_identity(self):
        stripped = load_elf64(stripped_sections=True)
        full = load_elf64(dwarf=True, dwarf_marker=b"source-lines")
        stripped_identity = elf_load_identity(stripped)
        full_identity = elf_load_identity(full)
        self.assertEqual(stripped_identity.fingerprint, full_identity.fingerprint)
        self.assertTrue(
            same_loadable_content(
                stripped, stripped_identity, full, full_identity
            )
        )

    def test_mapped_byte_change_fails_filter_and_direct_proof(self):
        left = load_elf64(stripped_sections=True, load_marker=b"left")
        right = load_elf64(dwarf=True, load_marker=b"right")
        left_identity = elf_load_identity(left)
        right_identity = elf_load_identity(right)
        self.assertNotEqual(left_identity.fingerprint, right_identity.fingerprint)
        self.assertFalse(
            same_loadable_content(left, left_identity, right, right_identity)
        )

    def test_requires_nonempty_executable_load_span(self):
        data = bytearray(load_elf64(stripped_sections=True))
        struct.pack_into("<I", data, 64 + 4, 4)  # PT_LOAD flags: read-only.
        with self.assertRaisesRegex(ElfLoadIdentityError, "executable PT_LOAD"):
            elf_load_identity(bytes(data))

    def test_segments_are_sorted_by_canonical_load_layout(self):
        data = bytearray(load_elf64(stripped_sections=True))
        struct.pack_into("<H", data, 56, 2)
        struct.pack_into(
            "<IIQQQQQQ",
            data,
            64 + 56,
            1,
            4,
            0x180,
            0x300000,
            0x300000,
            0x10,
            0x20,
            0x1000,
        )
        identity = elf_load_identity(bytes(data))
        self.assertEqual(
            [segment.virtual_address for segment in identity.segments],
            [0x300000, 0x400000],
        )


if __name__ == "__main__":
    unittest.main()
