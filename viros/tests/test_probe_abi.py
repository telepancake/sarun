import ctypes
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("probe_tool", ROOT / "probe" / "probe_tool.py")
PROBE_TOOL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE_TOOL)


class Request(ctypes.LittleEndianStructure):
    _fields_ = [
        ("magic", ctypes.c_uint32), ("abi_major", ctypes.c_uint16),
        ("abi_minor", ctypes.c_uint16), ("size", ctypes.c_uint16),
        ("opcode", ctypes.c_uint16), ("flags", ctypes.c_uint32),
        ("init_task", ctypes.c_uint64), ("cursor_task", ctypes.c_uint64),
        ("max_records", ctypes.c_uint32), ("reserved0", ctypes.c_uint32),
        ("reserved1", ctypes.c_uint64), ("reserved2", ctypes.c_uint64),
        ("reserved3", ctypes.c_uint64),
    ]


class Response(ctypes.LittleEndianStructure):
    _fields_ = [
        ("magic", ctypes.c_uint32), ("abi_major", ctypes.c_uint16),
        ("abi_minor", ctypes.c_uint16), ("header_size", ctypes.c_uint16),
        ("record_size", ctypes.c_uint16), ("arch", ctypes.c_uint16),
        ("endian", ctypes.c_uint8), ("pointer_bits", ctypes.c_uint8),
        ("status", ctypes.c_int32), ("flags", ctypes.c_uint32),
        ("record_count", ctypes.c_uint32), ("bytes_written", ctypes.c_uint32),
        ("next_cursor", ctypes.c_uint64), ("snapshot_root", ctypes.c_uint64),
        ("page_shift", ctypes.c_uint32), ("reserved0", ctypes.c_uint32),
        ("reserved1", ctypes.c_uint64),
    ]


class Task(ctypes.LittleEndianStructure):
    _fields_ = [
        ("record_size", ctypes.c_uint16), ("record_version", ctypes.c_uint16),
        ("probe_flags", ctypes.c_uint32), ("task", ctypes.c_uint64),
        ("group_leader", ctypes.c_uint64), ("real_parent", ctypes.c_uint64),
        ("mm", ctypes.c_uint64), ("pgd", ctypes.c_uint64),
        ("start_cookie", ctypes.c_uint64), ("state", ctypes.c_uint64),
        ("task_flags", ctypes.c_uint64), ("pid", ctypes.c_uint32),
        ("tgid", ctypes.c_uint32), ("ppid", ctypes.c_uint32),
        ("cpu", ctypes.c_uint32), ("exit_state", ctypes.c_uint32),
        ("abi_bits", ctypes.c_uint16), ("auxv_valid", ctypes.c_uint16),
        ("comm", ctypes.c_uint8 * 16), ("auxv", ctypes.c_uint64 * 10),
    ]


class SavedRegs(ctypes.LittleEndianStructure):
    _fields_ = [
        ("record_size", ctypes.c_uint16), ("record_version", ctypes.c_uint16),
        ("saved_regs_flags", ctypes.c_uint32), ("task", ctypes.c_uint64),
        ("mm", ctypes.c_uint64), ("start_cookie", ctypes.c_uint64),
        ("x", ctypes.c_uint64 * 31), ("sp", ctypes.c_uint64),
        ("pc", ctypes.c_uint64), ("pstate", ctypes.c_uint64),
    ]


def minimal_aarch64_rel(entry=True, undefined=False):
    """Construct a small ET_REL sufficient to exercise the pure-Python auditor."""
    names = b"\0.text\0.shstrtab\0.strtab\0.symtab\0"
    symbol_names = b"\0viros_probe_entry\0external\0"
    text = b"\xc0\x03\x5f\xd6"  # ret
    symbols = [b"\0" * 24]
    if entry:
        symbols.append(struct.pack("<IBBHQQ", 1, 0x12, 0, 1, 0, len(text)))
    if undefined:
        symbols.append(struct.pack("<IBBHQQ", 19, 0x10, 0, 0, 0, 0))
    symtab = b"".join(symbols)
    chunks = [b"\0" * 64, text, names, symbol_names, symtab]
    offsets = []
    cursor = 64
    image = bytearray(b"\0" * 64)
    for chunk in chunks[1:]:
        cursor = (cursor + 7) & ~7
        image.extend(b"\0" * (cursor - len(image)))
        offsets.append(cursor)
        image.extend(chunk)
        cursor += len(chunk)
    shoff = (cursor + 7) & ~7
    image.extend(b"\0" * (shoff - len(image)))
    sh = [b"\0" * 64]
    sh.append(struct.pack("<IIQQQQIIQQ", 1, 1, 0x6, 0, offsets[0], len(text), 0, 0, 4, 0))
    sh.append(struct.pack("<IIQQQQIIQQ", 7, 3, 0, 0, offsets[1], len(names), 0, 0, 1, 0))
    sh.append(struct.pack("<IIQQQQIIQQ", 17, 3, 0, 0, offsets[2], len(symbol_names), 0, 0, 1, 0))
    sh.append(struct.pack("<IIQQQQIIQQ", 25, 2, 0, 0, offsets[3], len(symtab), 3, 1, 8, 24))
    image.extend(b"".join(sh))
    header = b"\x7fELF" + bytes((2, 1, 1, 0)) + b"\0" * 8
    header += struct.pack("<HHIQQQIHHHHHH", 1, 183, 1, 0, 0, shoff, 0, 64, 0, 0, 64, 5, 2)
    image[:64] = header
    return bytes(image)


def minimal_aarch64_exec(base=0xffff800000100000, completion=True):
    names = b"\0.text\0.shstrtab\0.strtab\0.symtab\0"
    symbol_names = b"\0viros_probe_entry\0viros_probe_complete\0"
    text = b"\xc0\x03\x5f\xd6\x00\xca\x2a\xd4"  # ret; brk #0x5650
    symbols = [b"\0" * 24]
    symbols.append(struct.pack("<IBBHQQ", 1, 0x12, 0, 1, base, 4))
    if completion:
        symbols.append(struct.pack("<IBBHQQ", 19, 0x12, 0, 1, base + 4, 4))
    symtab = b"".join(symbols)
    chunks = [text, names, symbol_names, symtab]
    offsets = []
    cursor = 64
    image = bytearray(b"\0" * 64)
    for chunk in chunks:
        cursor = (cursor + 7) & ~7
        image.extend(b"\0" * (cursor - len(image)))
        offsets.append(cursor)
        image.extend(chunk)
        cursor += len(chunk)
    shoff = (cursor + 7) & ~7
    image.extend(b"\0" * (shoff - len(image)))
    sections = [b"\0" * 64]
    sections.append(struct.pack(
        "<IIQQQQIIQQ", 1, 1, 0x6, base, offsets[0], len(text), 0, 0, 4, 0))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", 7, 3, 0, 0, offsets[1], len(names), 0, 0, 1, 0))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", 17, 3, 0, 0, offsets[2], len(symbol_names), 0, 0, 1, 0))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", 25, 2, 0, 0, offsets[3], len(symtab), 3, 1, 8, 24))
    image.extend(b"".join(sections))
    header = b"\x7fELF" + bytes((2, 1, 1, 0)) + b"\0" * 8
    header += struct.pack(
        "<HHIQQQIHHHHHH", 2, 183, 1, base, 0, shoff, 0, 64, 0, 0, 64, 5, 2)
    image[:64] = header
    return bytes(image), text


class ProbeAbiTests(unittest.TestCase):
    def test_fixed_layout_sizes(self):
        self.assertEqual(ctypes.sizeof(Request), 64)
        self.assertEqual(ctypes.sizeof(Response), 64)
        self.assertEqual(ctypes.sizeof(Task), 192)
        self.assertEqual(ctypes.sizeof(SavedRegs), 304)
        self.assertEqual(Task.auxv.offset, 112)

    def test_header_declares_the_same_sizes(self):
        header = (ROOT / "probe" / "include" / "viros_probe_abi.h").read_text()
        self.assertIn("VIROS_PROBE_REQUEST_SIZE  64U", header)
        self.assertIn("VIROS_PROBE_RESPONSE_SIZE 64U", header)
        self.assertIn("VIROS_PROBE_TASK_V1_SIZE  192U", header)
        self.assertIn("VIROS_PROBE_SAVED_REGS_V1_SIZE 304U", header)

    def test_auditor_accepts_minimal_aarch64_rel(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "probe.o"
            path.write_bytes(minimal_aarch64_rel())
            result = PROBE_TOOL.audit_object(path)
            self.assertEqual(result["arch"], "aarch64")
            self.assertEqual(result["alloc_bytes"], 4)

    def test_auditor_rejects_undefined_symbol(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "probe.o"
            path.write_bytes(minimal_aarch64_rel(undefined=True))
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "undefined symbols: external"):
                PROBE_TOOL.audit_object(path)

    def test_auditor_requires_entry(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "probe.o"
            path.write_bytes(minimal_aarch64_rel(entry=False))
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "viros_probe_entry"):
                PROBE_TOOL.audit_object(path)

    def test_auditor_rejects_non_elf(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "probe.o"
            path.write_bytes(b"not an ELF")
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "not an ELF"):
                PROBE_TOOL.audit_object(path)

    def test_linked_image_reports_entry_and_completion_offsets(self):
        base = 0xffff800000100000
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "probe.elf"
            image, _ = minimal_aarch64_exec(base)
            path.write_bytes(image)
            result = PROBE_TOOL.audit_linked_image(path, base)
            self.assertEqual(result["image_size"], 8)
            self.assertEqual(result["entry_offset"], 0)
            self.assertEqual(result["completion_offset"], 4)

    def test_package_invokes_supplied_tools_and_writes_manifest(self):
        base = 0xffff800000100000
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            source = directory / "source.o"
            source.write_bytes(minimal_aarch64_rel())
            build_manifest = directory / "probe.json"
            build_manifest.write_text(json.dumps({
                "schema": "viros-probe-build-v1",
                "arch": "aarch64",
                "object": source.name,
                "object_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
                "kernel": {
                    "sha256": "1" * 64,
                    "build_id": "0123456789abcdef",
                },
            }), encoding="utf-8")
            output = directory / "package"
            linked_image, flat = minimal_aarch64_exec(base)
            commands = []

            def fake_run(command, check):
                self.assertTrue(check)
                commands.append(command)
                if command[0] == "exact-aarch64-ld":
                    Path(command[command.index("-o") + 1]).write_bytes(linked_image)
                elif command[0] == "exact-aarch64-objcopy":
                    Path(command[-1]).write_bytes(flat)
                else:
                    self.fail("unexpected tool: " + command[0])
                return SimpleNamespace(returncode=0)

            args = SimpleNamespace(
                build_manifest=build_manifest, output_dir=output, load_address=base,
                cross_ld="exact-aarch64-ld", objcopy="exact-aarch64-objcopy",
                max_alloc=65536,
            )
            with mock.patch.object(PROBE_TOOL.subprocess, "run", side_effect=fake_run):
                manifest = PROBE_TOOL.package_object(args)

            self.assertEqual(commands[0][0], "exact-aarch64-ld")
            self.assertEqual(commands[1][0], "exact-aarch64-objcopy")
            self.assertEqual(manifest["schema"], "viros-probe-package-v1")
            self.assertEqual(manifest["entry_offset"], 0)
            self.assertEqual(manifest["completion_offset"], 4)
            self.assertEqual(manifest["pgd_address_kind"], "kernel-virtual-address")
            self.assertEqual(manifest["kernel"]["sha256"], "1" * 64)
            self.assertEqual(manifest["call_abi"]["argument_registers"], ["x0", "x1", "x2"])
            self.assertEqual(manifest["abi_minor"], 2)
            self.assertEqual(manifest["abi_layout"]["version"], 1)
            self.assertEqual(manifest["abi_layout"]["task_v1_bytes"], 192)
            self.assertEqual(manifest["abi_layout"]["translation_v1_bytes"], 64)
            self.assertEqual(manifest["abi_layout"]["saved_regs_v1_bytes"], 304)
            self.assertEqual(
                manifest["capabilities"],
                ["snapshot-v1", "translate-va-aarch64-v1",
                 "saved-regs-aarch64-v1"],
            )
            self.assertEqual((output / "viros_probe.bin").read_bytes(), flat)
            self.assertEqual(
                __import__("json").loads((output / "package.json").read_text())["load_address"],
                base,
            )
            loaded, loaded_binary = PROBE_TOOL.load_probe_package(output / "package.json")
            self.assertEqual(loaded["abi_minor"], 2)
            self.assertEqual(loaded_binary, (output / "viros_probe.bin").resolve())

    def test_package_rejects_unaligned_address_before_tools(self):
        with self.assertRaisesRegex(PROBE_TOOL.AuditError, "16-byte aligned"):
            PROBE_TOOL.linker_script(0x1003)

    def test_exact_kbuild_must_contain_the_supplied_vmlinux(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            linux_dir = directory / "kernel-build"
            linux_dir.mkdir()
            supplied = directory / "vmlinux"
            supplied.write_bytes(b"supplied")
            args = SimpleNamespace(
                output_dir=directory / "probe", linux_dir=linux_dir,
                vmlinux=supplied, arch="aarch64", cross_compile="cross-",
                make="make", max_alloc=65536,
            )
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "has no vmlinux"):
                PROBE_TOOL.build_object(args)

            (linux_dir / "vmlinux").write_bytes(b"different")
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "not produced"):
                PROBE_TOOL.build_object(args)

    def test_exact_kbuild_rejects_different_kernel_release(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            linux_dir = directory / "kernel-build"
            (linux_dir / "include/config").mkdir(parents=True)
            (linux_dir / "include/config/kernel.release").write_text("5.6.3\n")
            supplied = directory / "vmlinux"
            supplied.write_bytes(
                b"/builder/build_dir/linux-6.12.94/arch/arm64/kernel/foo.c\0"
            )
            (linux_dir / "vmlinux").write_bytes(supplied.read_bytes())
            args = SimpleNamespace(
                output_dir=directory / "probe", linux_dir=linux_dir,
                vmlinux=supplied, arch="aarch64", cross_compile="cross-",
                make="make", max_alloc=65536,
            )
            with self.assertRaisesRegex(
                PROBE_TOOL.AuditError, "Kbuild release 5.6.3.*6.12.94"
            ):
                PROBE_TOOL.build_object(args)


if __name__ == "__main__":
    unittest.main()
