from __future__ import annotations

import hashlib
import json
from pathlib import Path
import struct
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from probe import probe_tool


ROOT = Path(__file__).resolve().parents[1]
ARM_EABI5 = 0x05000000
ARM_HARD_FLOAT = 0x00000400
ARM_UDF_5650 = 0xE7F565F0


def _elf32(
    chunks,
    section_headers,
    *,
    elf_type: int,
    entry: int,
    flags: int = ARM_EABI5,
    shstrndx: int,
) -> bytes:
    image = bytearray(b"\0" * 52)
    offsets = []
    for contents, alignment in chunks:
        cursor = (len(image) + alignment - 1) & -alignment
        image.extend(b"\0" * (cursor - len(image)))
        offsets.append(cursor)
        image.extend(contents)
    section_offset = (len(image) + 3) & ~3
    image.extend(b"\0" * (section_offset - len(image)))
    sections = [b"\0" * 40]
    sections.extend(
        struct.pack("<IIIIIIIIII", *header)
        for header in section_headers(offsets)
    )
    image.extend(b"".join(sections))
    header = b"\x7fELF" + bytes((1, 1, 1, 0)) + b"\0" * 8
    header += struct.pack(
        "<HHIIIIIHHHHHH",
        elf_type,
        probe_tool.EM_ARM,
        1,
        entry,
        0,
        section_offset,
        flags,
        52,
        0,
        0,
        40,
        len(sections),
        shstrndx,
    )
    image[:52] = header
    return bytes(image)


def minimal_arm_rel(
    *,
    flags: int = ARM_EABI5,
    entry_value: int = 0,
    thumb_mapping_symbol: bool = False,
) -> bytes:
    names = b"\0.text\0.shstrtab\0.strtab\0.symtab\0"
    symbol_names = b"\0viros_probe_entry\0"
    if thumb_mapping_symbol:
        symbol_names += b"$t\0"
    text = struct.pack("<I", 0xE12FFF1E)  # bx lr
    symbols = b"\0" * 16
    symbols += struct.pack(
        "<IIIBBH", 1, entry_value, len(text), 0x12, 0, 1
    )
    if thumb_mapping_symbol:
        symbols += struct.pack(
            "<IIIBBH", symbol_names.index(b"$t"), 0, 0, 0, 0, 1
        )
    chunks = [(text, 4), (names, 1), (symbol_names, 1), (symbols, 4)]

    def headers(offsets):
        return [
            (
                names.index(b".text"), 1, 0x6, 0, offsets[0], len(text),
                0, 0, 4, 0,
            ),
            (
                names.index(b".shstrtab"), 3, 0, 0, offsets[1], len(names),
                0, 0, 1, 0,
            ),
            (
                names.index(b".strtab"), 3, 0, 0, offsets[2],
                len(symbol_names), 0, 0, 1, 0,
            ),
            (
                names.index(b".symtab"), 2, 0, 0, offsets[3], len(symbols),
                3, 1, 4, 16,
            ),
        ]

    return _elf32(
        chunks,
        headers,
        elf_type=probe_tool.ET_REL,
        entry=0,
        flags=flags,
        shstrndx=2,
    )


def minimal_arm_exec(base: int = 0x80100000) -> tuple[bytes, bytes]:
    names = b"\0.text\0.shstrtab\0.strtab\0.symtab\0"
    symbol_names = b"\0viros_probe_entry\0viros_probe_complete\0"
    text = struct.pack("<II", 0xE12FFF1E, ARM_UDF_5650)
    symbols = b"\0" * 16
    symbols += struct.pack("<IIIBBH", 1, base, 4, 0x12, 0, 1)
    symbols += struct.pack("<IIIBBH", 19, base + 4, 4, 0x12, 0, 1)
    chunks = [(text, 4), (names, 1), (symbol_names, 1), (symbols, 4)]

    def headers(offsets):
        return [
            (
                names.index(b".text"), 1, 0x6, base, offsets[0], len(text),
                0, 0, 4, 0,
            ),
            (
                names.index(b".shstrtab"), 3, 0, 0, offsets[1], len(names),
                0, 0, 1, 0,
            ),
            (
                names.index(b".strtab"), 3, 0, 0, offsets[2],
                len(symbol_names), 0, 0, 1, 0,
            ),
            (
                names.index(b".symtab"), 2, 0, 0, offsets[3], len(symbols),
                3, 1, 4, 16,
            ),
        ]

    return (
        _elf32(
            chunks,
            headers,
            elf_type=probe_tool.ET_EXEC,
            entry=base,
            shstrndx=2,
        ),
        text,
    )


def arm_elf_with_build_id(build_id: bytes) -> bytes:
    note = struct.pack("<III", 4, len(build_id), 3) + b"GNU\0" + build_id
    note += b"\0" * (-len(note) & 3)
    names = b"\0.note.gnu.build-id\0.shstrtab\0"
    chunks = [(note, 4), (names, 1)]

    def headers(offsets):
        return [
            (
                names.index(b".note"), 7, 2, 0, offsets[0], len(note),
                0, 0, 4, 0,
            ),
            (
                names.index(b".shstrtab"), 3, 0, 0, offsets[1], len(names),
                0, 0, 1, 0,
            ),
        ]

    return _elf32(
        chunks,
        headers,
        elf_type=probe_tool.ET_EXEC,
        entry=0,
        shstrndx=2,
    )


class Armv7ProbePackageTests(unittest.TestCase):
    def test_kernel_source_has_arm_state_completion_boundary(self):
        source = (ROOT / "probe" / "kernel" / "viros_probe.c").read_text()
        kbuild = (ROOT / "probe" / "kernel" / "Kbuild").read_text()
        self.assertIn("#elif defined(CONFIG_ARM)", source)
        self.assertIn('asm(".pushsection .text.viros_probe_complete', source)
        self.assertIn('".arm\\n"', source)
        self.assertIn('".word 0xe7f565f0\\n"', source)
        self.assertIn('".size viros_probe_complete', source)
        self.assertIn("ifeq ($(CONFIG_ARM),y)", kbuild)
        self.assertIn("CFLAGS_viros_probe.o += -marm", kbuild)
        self.assertIn("-fno-unwind-tables", kbuild)

    def test_arm_auditor_accepts_arm_state_eabi5(self):
        with tempfile.TemporaryDirectory(dir=ROOT) as directory:
            path = Path(directory) / "probe.o"
            path.write_bytes(minimal_arm_rel())
            result = probe_tool.audit_object(path, "arm")
            self.assertEqual(result["arch"], "arm")
            self.assertEqual(result["elf_class"], 32)
            self.assertEqual(result["byte_order"], "little")

    def test_arm_auditor_rejects_wrong_eabi_hard_float_and_thumb(self):
        cases = (
            (minimal_arm_rel(flags=0), "EABI version 5"),
            (
                minimal_arm_rel(flags=ARM_EABI5 | ARM_HARD_FLOAT),
                "hard-float",
            ),
            (minimal_arm_rel(entry_value=1), "Thumb code"),
            (minimal_arm_rel(thumb_mapping_symbol=True), "Thumb code"),
        )
        with tempfile.TemporaryDirectory(dir=ROOT) as directory:
            path = Path(directory) / "probe.o"
            for image, message in cases:
                with self.subTest(message=message):
                    path.write_bytes(image)
                    with self.assertRaisesRegex(probe_tool.AuditError, message):
                        probe_tool.audit_object(path, "arm")

    def test_arm_package_is_snapshot_only_and_records_aapcs32(self):
        base = 0x80100000
        with tempfile.TemporaryDirectory(dir=ROOT) as directory_text:
            directory = Path(directory_text)
            source = directory / "source.o"
            source.write_bytes(minimal_arm_rel())
            build_manifest = directory / "probe.json"
            build_manifest.write_text(
                json.dumps(
                    {
                        "schema": probe_tool.PROBE_BUILD_SCHEMA,
                        "arch": "arm",
                        "object": source.name,
                        "object_sha256": hashlib.sha256(
                            source.read_bytes()
                        ).hexdigest(),
                        "kernel": {
                            "sha256": "3" * 64,
                            "build_id": "0123456789abcdef",
                        },
                    }
                ),
                encoding="utf-8",
            )
            output = directory / "package"
            linked_image, flat = minimal_arm_exec(base)
            commands = []

            def fake_run(command, check):
                self.assertTrue(check)
                commands.append(command)
                if command[0] == "exact-arm-ld":
                    Path(command[command.index("-o") + 1]).write_bytes(
                        linked_image
                    )
                elif command[0] == "exact-arm-objcopy":
                    Path(command[-1]).write_bytes(flat)
                else:
                    self.fail("unexpected tool: " + command[0])
                return SimpleNamespace(returncode=0)

            args = SimpleNamespace(
                build_manifest=build_manifest,
                output_dir=output,
                load_address=base,
                cross_ld="exact-arm-ld",
                objcopy="exact-arm-objcopy",
                max_alloc=65536,
            )
            with mock.patch.object(
                probe_tool.subprocess, "run", side_effect=fake_run
            ):
                manifest = probe_tool.package_object(args)

            self.assertEqual(commands[0][1:3], ["-m", "armelf_linux_eabi"])
            self.assertEqual(manifest["arch"], "arm")
            self.assertEqual(manifest["capabilities"], ["snapshot-v1"])
            self.assertEqual(
                manifest["call_abi"],
                {
                    "name": "aapcs32",
                    "argument_registers": ["r0", "r1", "r2"],
                    "result_register": "r0",
                    "link_register": "lr",
                    "stack_alignment": 8,
                    "completion_trap": "udf-0x5650",
                },
            )
            self.assertEqual(manifest["elf_abi"]["isa"], "armv7-a")
            self.assertFalse(manifest["elf_abi"]["thumb"])
            self.assertNotIn("translation_v1_bytes", manifest["abi_layout"])
            self.assertNotIn("saved_regs_v1_bytes", manifest["abi_layout"])
            self.assertEqual(struct.unpack_from("<I", flat, 4)[0], ARM_UDF_5650)
            loaded, binary = probe_tool.load_probe_package(
                output / "package.json"
            )
            self.assertEqual(loaded["call_abi"]["link_register"], "lr")
            self.assertEqual(binary, (output / "viros_probe.bin").resolve())

    def test_arm_callgate_manifest_uses_32_bit_snapshot_contract(self):
        base = 0x80100000
        build_id = "0123456789abcdef"
        with tempfile.TemporaryDirectory(dir=ROOT) as directory_text:
            directory = Path(directory_text)
            vmlinux = directory / "vmlinux"
            vmlinux.write_bytes(arm_elf_with_build_id(bytes.fromhex(build_id)))
            binary = directory / "viros_probe.bin"
            binary.write_bytes(struct.pack("<II", 0xE12FFF1E, ARM_UDF_5650))
            package = {
                "schema": probe_tool.PROBE_PACKAGE_SCHEMA,
                "arch": "arm",
                "abi_major": 1,
                "abi_minor": probe_tool.PROBE_ABI_MINOR,
                "abi_layout": {
                    "version": 1,
                    "request_v1_bytes": 64,
                    "response_v1_header_bytes": 64,
                    "task_v1_bytes": 192,
                    "target_byte_order": "little",
                },
                "capabilities": ["snapshot-v1"],
                "call_abi": {
                    "name": "aapcs32",
                    "argument_registers": ["r0", "r1", "r2"],
                    "result_register": "r0",
                    "link_register": "lr",
                    "stack_alignment": 8,
                    "completion_trap": "udf-0x5650",
                },
                "elf_abi": {
                    "class": 32,
                    "byte_order": "little",
                    "machine": "EM_ARM",
                    "isa": "armv7-a",
                    "abi": "aapcs32",
                    "float": "soft",
                    "pic": False,
                    "thumb": False,
                },
                "load_address": base,
                "image_start": base,
                "image_end": base + len(binary.read_bytes()),
                "image_size": len(binary.read_bytes()),
                "entry_offset": 0,
                "completion_offset": 4,
                "binary": binary.name,
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "kernel": {
                    "sha256": hashlib.sha256(vmlinux.read_bytes()).hexdigest(),
                    "build_id": build_id,
                },
            }
            package_path = directory / "package.json"
            package_path.write_text(json.dumps(package), encoding="utf-8")
            args = SimpleNamespace(
                package=package_path,
                vmlinux=vmlinux,
                output=directory / "callgate.json",
                scratch_regions=None,
                code_gva=base,
                code_gpa=0x00100000,
                code_size=4096,
                data_gva=0x82000000,
                data_gpa=0x02000000,
                data_size=4096,
                stack_gva=0x82001000,
                stack_gpa=0x02001000,
                stack_size=4096,
                cpu=0,
                init_task=0x81234000,
                pstate=None,
                timeout_seconds=1.0,
            )
            manifest = probe_tool.create_callgate_manifest(args)
            self.assertEqual(manifest["architecture"], "arm")
            self.assertEqual(manifest["probe"]["capabilities"], ["snapshot-v1"])
            self.assertEqual(manifest["invocation"]["stack_pointer"], "0x82002000")
            self.assertNotIn("pstate", manifest["invocation"])
            request = bytes.fromhex(manifest["mailbox"]["request_hex"])
            self.assertEqual(struct.unpack_from("<Q", request, 16)[0], 0x81234000)

            args.pstate = 0x1D3
            args.output = directory / "invalid-callgate.json"
            with self.assertRaisesRegex(probe_tool.AuditError, "only valid"):
                probe_tool.create_callgate_manifest(args)

    def test_arm_linker_rejects_unaligned_and_out_of_range_addresses(self):
        script = probe_tool.linker_script(0x80100000, "arm")
        self.assertIn("ALIGN(4)", script)
        self.assertIn("*(.ARM.exidx*)", script)
        with self.assertRaisesRegex(probe_tool.AuditError, "4-byte aligned"):
            probe_tool.linker_script(0x80100002, "arm")
        with self.assertRaisesRegex(probe_tool.AuditError, "32-bit"):
            probe_tool.linker_script(0x100000000, "arm")


if __name__ == "__main__":
    unittest.main()
