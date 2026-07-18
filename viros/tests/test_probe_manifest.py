from __future__ import annotations

from contextlib import redirect_stdout
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import tempfile
from types import SimpleNamespace
import unittest

from callgate.manifest import load_and_validate_manifest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "probe_manifest_tool", ROOT / "probe" / "probe_tool.py"
)
PROBE_TOOL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE_TOOL)


def aarch64_elf_with_build_id(build_id: bytes) -> bytes:
    note = struct.pack("<III", 4, len(build_id), 3) + b"GNU\0" + build_id
    note += b"\0" * (-len(note) & 3)
    names = b"\0.note.gnu.build-id\0.shstrtab\0"
    image = bytearray(b"\0" * 64)
    note_offset = len(image)
    image.extend(note)
    names_offset = len(image)
    image.extend(names)
    section_offset = (len(image) + 7) & ~7
    image.extend(b"\0" * (section_offset - len(image)))
    sections = [b"\0" * 64]
    sections.append(struct.pack(
        "<IIQQQQIIQQ", names.index(b".note"), 7, 2, 0,
        note_offset, len(note), 0, 0, 4, 0,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", names.index(b".shstrtab"), 3, 0, 0,
        names_offset, len(names), 0, 0, 1, 0,
    ))
    image.extend(b"".join(sections))
    header = b"\x7fELF" + bytes((2, 1, 1, 0)) + b"\0" * 8
    header += struct.pack(
        "<HHIQQQIHHHHHH", 2, 183, 1, 0, 0, section_offset, 0,
        64, 0, 0, 64, 3, 2,
    )
    image[:64] = header
    return bytes(image)


def write_package(
    directory: Path, load_address: int, kernel_sha256: str, kernel_build_id: str
) -> tuple[Path, Path]:
    binary = directory / "viros_probe.bin"
    binary.write_bytes(bytes.fromhex("c0035fd600ca2ad4"))
    package = {
        "schema": "viros-probe-package-v1",
        "arch": "aarch64",
        "abi_major": 1,
        "abi_minor": 1,
        "abi_layout": {
            "version": 1,
            "request_v1_bytes": 64,
            "response_v1_header_bytes": 64,
            "task_v1_bytes": 192,
            "translation_v1_bytes": 64,
            "target_byte_order": "little",
        },
        "capabilities": ["snapshot-v1", "translate-va-aarch64-v1"],
        "call_abi": {
            "name": "aapcs64",
            "argument_registers": ["x0", "x1", "x2"],
            "result_register": "x0",
            "link_register": "x30",
            "stack_alignment": 16,
            "completion_trap": "brk-0x5650",
        },
        "load_address": load_address,
        "image_start": load_address,
        "image_end": load_address + binary.stat().st_size,
        "image_size": binary.stat().st_size,
        "entry_offset": 0,
        "completion_offset": 4,
        "binary": binary.name,
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "kernel": {"sha256": kernel_sha256, "build_id": kernel_build_id},
    }
    package_path = directory / "package.json"
    package_path.write_text(json.dumps(package), encoding="utf-8")
    return package_path, binary


def manifest_args(directory: Path) -> SimpleNamespace:
    load_address = 0xffff800080100000
    vmlinux = directory / "vmlinux"
    vmlinux.write_bytes(aarch64_elf_with_build_id(bytes.fromhex("0123456789abcdef")))
    package, _ = write_package(
        directory, load_address, hashlib.sha256(vmlinux.read_bytes()).hexdigest(),
        "0123456789abcdef",
    )
    return SimpleNamespace(
        package=package,
        vmlinux=vmlinux,
        output=directory / "callgate.json",
        code_gva=load_address,
        code_gpa=0x40100000,
        code_size=4096,
        data_gva=0xffff800082000000,
        data_gpa=0x42000000,
        data_size=4096,
        stack_gva=0xffff800082001000,
        stack_gpa=0x42001000,
        stack_size=4096,
        cpu=2,
        init_task=0xffff800081234000,
        pstate=0x3c5,
        timeout_seconds=1.0,
    )


def write_scratch_regions(
    args: SimpleNamespace, *, sha256: str | None = None,
    build_id: str = "0123456789abcdef",
) -> Path:
    path = Path(args.output).parent / "scratch.json"
    document = {
        "schema": "viros-scratch-regions-v1",
        "arch": "aarch64",
        "page_size": 4096,
        "runtime_offset": 0,
        "vmlinux": {
            "path": str(Path(args.vmlinux).resolve()),
            "sha256": sha256 or hashlib.sha256(Path(args.vmlinux).read_bytes()).hexdigest(),
            "build_id": build_id,
        },
        "regions": {
            "code": {"gva": args.code_gva, "size": args.code_size},
            "data": {"gva": args.data_gva, "size": args.data_size},
            "stack": {"gva": args.stack_gva, "size": args.stack_size},
        },
    }
    path.write_text(json.dumps(document), encoding="utf-8")
    return path


def select_scratch_mode(args: SimpleNamespace, scratch: Path) -> None:
    args.scratch_regions = scratch
    for name in ("code", "data", "stack"):
        setattr(args, f"{name}_gva", None)
        setattr(args, f"{name}_size", None)


class ProbeManifestTests(unittest.TestCase):
    def test_snapshot_only_abi_1_0_package_remains_loadable(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            package = json.loads(args.package.read_text(encoding="utf-8"))
            package["abi_minor"] = 0
            package["abi_layout"] = {
                "request_bytes": 64,
                "response_header_bytes": 64,
                "task_record_bytes": 192,
                "target_byte_order": "little",
            }
            package.pop("capabilities")
            args.package.write_text(json.dumps(package), encoding="utf-8")

            loaded, _ = PROBE_TOOL.load_probe_package(args.package)
            self.assertEqual(loaded["abi_minor"], 0)
            PROBE_TOOL.create_callgate_manifest(args)
            validated = load_and_validate_manifest(args.output)
            self.assertEqual(validated.probe_capabilities, ("snapshot-v1",))
            request = struct.unpack(
                "<IHHHHIQQIIQQQ",
                validated.request_bytes,
            )
            self.assertEqual(request[2], 0)

    def test_public_cli_command_accepts_explicit_mapping_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            argv = [
                "callgate-manifest", str(args.package),
                "--vmlinux", str(args.vmlinux), "--output", str(args.output),
                "--code-gva", hex(args.code_gva), "--code-gpa", hex(args.code_gpa),
                "--code-size", hex(args.code_size),
                "--data-gva", hex(args.data_gva), "--data-gpa", hex(args.data_gpa),
                "--data-size", hex(args.data_size),
                "--stack-gva", hex(args.stack_gva), "--stack-gpa", hex(args.stack_gpa),
                "--stack-size", hex(args.stack_size),
                "--cpu", str(args.cpu), "--init-task", hex(args.init_task),
            ]
            output = io.StringIO()
            with redirect_stdout(output):
                PROBE_TOOL.main(argv)

            self.assertTrue(load_and_validate_manifest(args.output).is_validated)
            self.assertEqual(json.loads(output.getvalue())["invocation"]["cpu"], args.cpu)

    def test_public_cli_accepts_scratch_document_and_only_three_gpas(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            scratch = write_scratch_regions(args)
            argv = [
                "callgate-manifest", str(args.package),
                "--vmlinux", str(args.vmlinux), "--output", str(args.output),
                "--scratch-regions", str(scratch),
                "--code-gpa", hex(args.code_gpa),
                "--data-gpa", hex(args.data_gpa),
                "--stack-gpa", hex(args.stack_gpa),
                "--cpu", str(args.cpu), "--init-task", hex(args.init_task),
            ]
            output = io.StringIO()

            with redirect_stdout(output):
                PROBE_TOOL.main(argv)

            validated = load_and_validate_manifest(args.output)
            self.assertEqual(validated.entry_address, args.code_gva)
            self.assertEqual(validated.stack_pointer, args.stack_gva + args.stack_size)
            self.assertEqual(json.loads(output.getvalue())["regions"][1]["size"], 4096)

    def test_scratch_document_must_match_exact_vmlinux_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            scratch = write_scratch_regions(args, sha256="0" * 64)
            select_scratch_mode(args, scratch)
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "scratch.*SHA-256"):
                PROBE_TOOL.create_callgate_manifest(args)
            self.assertFalse(args.output.exists())

            scratch = write_scratch_regions(args, build_id="fedcba9876543210")
            args.scratch_regions = scratch
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "scratch.*build ID"):
                PROBE_TOOL.create_callgate_manifest(args)
            self.assertFalse(args.output.exists())

    def test_rejects_mixed_scratch_and_explicit_or_missing_gpa_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            scratch = write_scratch_regions(args)
            select_scratch_mode(args, scratch)
            args.code_gva = 0xffff800080100000
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "cannot be mixed"):
                PROBE_TOOL.create_callgate_manifest(args)

            args.code_gva = None
            args.stack_gpa = None
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "all three physical"):
                PROBE_TOOL.create_callgate_manifest(args)

    def test_rejects_partial_fully_explicit_mode(self):
        with tempfile.TemporaryDirectory() as temporary:
            args = manifest_args(Path(temporary))
            args.data_size = None
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "explicit region mode"):
                PROBE_TOOL.create_callgate_manifest(args)

    def test_bridge_writes_a_runtime_valid_manifest_and_snapshot_request(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            document = PROBE_TOOL.create_callgate_manifest(args)
            validated = load_and_validate_manifest(args.output)

            self.assertTrue(validated.is_validated)
            self.assertEqual(validated.kernel_build_id, "0123456789abcdef")
            self.assertEqual(validated.cpu, 2)
            self.assertEqual(validated.entry_address, args.code_gva)
            self.assertEqual(validated.completion_address, args.code_gva + 4)
            self.assertEqual(validated.stack_pointer, args.stack_gva + args.stack_size)
            self.assertEqual(validated.result_size, args.data_size - 64)
            self.assertEqual(validated.completion_magic, bytes.fromhex("53525056"))
            self.assertEqual(
                validated.probe_capabilities,
                ("snapshot-v1", "translate-va-aarch64-v1"),
            )

            request = struct.unpack("<IHHHHIQQIIQQQ", validated.request_bytes)
            self.assertEqual(request[0], 0x56505251)
            self.assertEqual(request[3:5], (64, 1))
            self.assertEqual(request[6], args.init_task)
            self.assertEqual(request[8], 20)
            self.assertFalse(Path(document["kernel"]["vmlinux"]).is_absolute())
            self.assertFalse(Path(document["probe"]["binary"]).is_absolute())

    def test_tampered_sealed_binary_does_not_replace_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            args.output.write_text("keep me", encoding="utf-8")
            (directory / "viros_probe.bin").write_bytes(b"tampered")

            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "SHA-256 mismatch"):
                PROBE_TOOL.create_callgate_manifest(args)
            self.assertEqual(args.output.read_text(encoding="utf-8"), "keep me")

    def test_absolute_link_address_must_equal_code_gva(self):
        with tempfile.TemporaryDirectory() as temporary:
            args = manifest_args(Path(temporary))
            args.code_gva += 0x1000
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "does not match"):
                PROBE_TOOL.create_callgate_manifest(args)
            self.assertFalse(args.output.exists())

    def test_runtime_validation_happens_before_atomic_replace(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            args.output.write_text("previous manifest", encoding="utf-8")
            args.stack_gpa = args.data_gpa

            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "generated.*overlap"):
                PROBE_TOOL.create_callgate_manifest(args)
            self.assertEqual(args.output.read_text(encoding="utf-8"), "previous manifest")
            self.assertEqual(list(directory.glob(".callgate.json.*.tmp")), [])

    def test_vmlinux_must_carry_a_gnu_build_id(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            args.vmlinux.write_bytes(aarch64_elf_with_build_id(b""))
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "GNU build ID"):
                PROBE_TOOL.create_callgate_manifest(args)

    def test_package_cannot_be_rebound_to_a_different_vmlinux(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            args.vmlinux.write_bytes(
                aarch64_elf_with_build_id(bytes.fromhex("fedcba9876543210"))
            )
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "SHA-256 does not match"):
                PROBE_TOOL.create_callgate_manifest(args)
            self.assertFalse(args.output.exists())

    def test_output_must_not_overwrite_any_input(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            args = manifest_args(directory)
            for output, description in (
                (args.package, "probe package"),
                (directory / "viros_probe.bin", "probe binary"),
                (args.vmlinux, "vmlinux"),
            ):
                with self.subTest(description=description):
                    args.output = output
                    with self.assertRaisesRegex(
                        PROBE_TOOL.AuditError, f"input {description}"
                    ):
                        PROBE_TOOL.create_callgate_manifest(args)

    def test_output_must_not_overwrite_scratch_document(self):
        with tempfile.TemporaryDirectory() as temporary:
            args = manifest_args(Path(temporary))
            scratch = write_scratch_regions(args)
            select_scratch_mode(args, scratch)
            args.output = scratch
            with self.assertRaisesRegex(PROBE_TOOL.AuditError, "scratch regions"):
                PROBE_TOOL.create_callgate_manifest(args)


if __name__ == "__main__":
    unittest.main()
