import hashlib
import json
from pathlib import Path
import struct
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from callgate.architectures import (
    ARCHITECTURES,
    X86_64,
    X86_EFLAGS_AC,
    X86_EFLAGS_DF,
    X86_EFLAGS_FIXED,
    X86_EFLAGS_IF,
    X86_EFLAGS_NT,
    X86_EFLAGS_RF,
    X86_EFLAGS_TF,
    X86_EFLAGS_VM,
    architecture_by_name,
)
from callgate.manifest import load_and_validate_manifest
from callgate.rsp_target import RspQemuTarget
from probe.abi import (
    ABI_MAJOR,
    ABI_MINOR,
    ARCH_X86,
    ENDIAN_LITTLE,
    RESPONSE_MAGIC,
    RESPONSE_SIZE,
    TASK_AUX_VALID,
    TASK_GROUP_LEADER,
    TASK_HAS_MM,
    TASK_V1_SIZE,
    X86_64_SNAPSHOT_ABI,
    build_snapshot_request,
    decode_response,
)
from probe import probe_tool


X86_NAMES = (
    "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp",
    "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15",
    "rip", "eflags", "cs", "ss", "gs_base", "k_gs_base",
)


def x86_original_registers():
    registers = {name: number for number, name in enumerate(X86_NAMES)}
    registers.update(
        rip=0xFFFFFFFF81001234,
        rsp=0xFFFFFFFF82002000,
        eflags=(
            X86_EFLAGS_FIXED | X86_EFLAGS_IF | X86_EFLAGS_TF
            | X86_EFLAGS_DF | X86_EFLAGS_NT | X86_EFLAGS_RF
            | X86_EFLAGS_VM | X86_EFLAGS_AC
        ),
        cs=0x10,
        ss=0x18,
        gs_base=0xFFFF888000000000,
        k_gs_base=0,
    )
    return registers


def x86_response(*, abi_bits=32):
    comm = b"init\0" + b"\0" * 11
    auxv = (0x8048034, 32, 8, 4096, 0, 0x8049000, 0, 0, 0, 0)
    aux_valid = (1 << 4) - 1
    task_flags = TASK_HAS_MM | TASK_GROUP_LEADER | TASK_AUX_VALID
    task = struct.pack(
        "<HHIQQQQQQQQIIIIIHH16s10Q",
        TASK_V1_SIZE, 1, task_flags,
        0xFFFFFFFF82200000, 0xFFFFFFFF82200000,
        0xFFFFFFFF82100000, 0xFFFFFFFF82300000,
        0xFFFFFFFF82400000, 0x12345678, 1, 0,
        1, 1, 0, 0, 0, abi_bits, aux_valid, comm, *auxv,
    )
    header = struct.pack(
        "<IHHHHHBBiIIIQQIIQ",
        RESPONSE_MAGIC, ABI_MAJOR, ABI_MINOR, RESPONSE_SIZE, TASK_V1_SIZE,
        ARCH_X86, ENDIAN_LITTLE, 64, 0, 0, 1,
        RESPONSE_SIZE + TASK_V1_SIZE, 0, 0xFFFFFFFF82200000,
        12, 0, 0,
    )
    return header + task


def x86_elf(*, linked=False, base=0xFFFFFFFF81010000):
    names = b"\0.text\0.shstrtab\0.strtab\0.symtab\0"
    symbol_names = b"\0viros_probe_entry\0viros_probe_complete\0"
    text = bytes.fromhex("e800000000cc9090") if linked else b"\xc3"
    symbols = [b"\0" * 24]
    symbols.append(struct.pack(
        "<IBBHQQ", 1, 0x12, 0, 1, base if linked else 0,
        5 if linked else 1,
    ))
    if linked:
        symbols.append(struct.pack("<IBBHQQ", 19, 0x12, 0, 1, base + 5, 1))
    symtab = b"".join(symbols)
    chunks = (text, names, symbol_names, symtab)
    offsets = []
    image = bytearray(b"\0" * 64)
    for chunk in chunks:
        cursor = (len(image) + 7) & ~7
        image.extend(b"\0" * (cursor - len(image)))
        offsets.append(cursor)
        image.extend(chunk)
    shoff = (len(image) + 7) & ~7
    image.extend(b"\0" * (shoff - len(image)))
    sections = [b"\0" * 64]
    sections.append(struct.pack(
        "<IIQQQQIIQQ", 1, 1, 0x6, base if linked else 0,
        offsets[0], len(text), 0, 0, 16, 0,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", 7, 3, 0, 0, offsets[1], len(names), 0, 0, 1, 0,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", 17, 3, 0, 0, offsets[2], len(symbol_names), 0, 0, 1, 0,
    ))
    sections.append(struct.pack(
        "<IIQQQQIIQQ", 25, 2, 0, 0, offsets[3], len(symtab), 3, 1, 8, 24,
    ))
    image.extend(b"".join(sections))
    header = b"\x7fELF" + bytes((2, 1, 1, 0)) + b"\0" * 8
    header += struct.pack(
        "<HHIQQQIHHHHHH", 2 if linked else 1, 62, 1,
        base if linked else 0, 0, shoff, 0,
        64, 0, 0, 64, 5, 2,
    )
    image[:64] = header
    return bytes(image), text


class FakeX86Qemu:
    def __init__(self):
        core = "".join(
            f'<reg name="{name}" bitsize="{32 if name in {"eflags", "cs", "ss"} else 64}" '
            f'regnum="{number}"/>'
            for number, name in enumerate(X86_NAMES)
        )
        self.xml = {
            "target.xml": (
                b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
                b"<architecture>i386:x86-64</architecture>"
                b'<xi:include href="core.xml"/></target>'
            ),
            "core.xml": f"<feature>{core}</feature>".encode(),
        }
        self.current = "1"
        self.registers = {
            number: (0x1000 + number).to_bytes(
                4 if name in {"eflags", "cs", "ss"} else 8, "little"
            )
            for number, name in enumerate(X86_NAMES)
        }

    def read_xfer(self, object_name, annex):
        if object_name != "features":
            raise AssertionError(object_name)
        return self.xml[annex]

    def thread_ids(self):
        return ("1",)

    def current_thread(self):
        return self.current

    def select_thread(self, operation, thread):
        if operation != "g" or thread != "1":
            raise AssertionError((operation, thread))
        self.current = thread

    def read_register(self, number):
        return self.registers[number]

    def write_register(self, number, value):
        self.registers[number] = value


class X86ArchitectureTests(unittest.TestCase):
    def test_descriptor_is_registered_and_uses_real_sysv_entry_state(self):
        self.assertIs(ARCHITECTURES["x86_64"], X86_64)
        self.assertIs(architecture_by_name("x86_64"), X86_64)
        self.assertIsNone(X86_64.link_register)
        self.assertEqual(X86_64.argument_registers, ("rdi", "rsi", "rdx"))
        self.assertEqual(X86_64.qemu_architecture_names, ("i386:x86-64",))
        self.assertEqual(X86_64.breakpoint_size, 1)

        values = dict(X86_64.entry_register_values(
            request_address=0xFFFFFFFF83000000,
            result_address=0xFFFFFFFF83001000,
            result_size=4096,
            completion_address=0xFFFFFFFF81010005,
            control_state=None,
            original_registers=x86_original_registers(),
            stack_pointer=0xFFFFFFFF84002000,
            entry_address=0xFFFFFFFF81010000,
        ))
        self.assertEqual(values["rdi"], 0xFFFFFFFF83000000)
        self.assertEqual(values["rsi"], 0xFFFFFFFF83001000)
        self.assertEqual(values["rdx"], 4096)
        self.assertEqual(values["rsp"], 0xFFFFFFFF84002000)
        self.assertEqual(values["rip"], 0xFFFFFFFF81010000)
        self.assertEqual(values["eflags"], X86_EFLAGS_FIXED)
        self.assertEqual(values["ss"], 0x18)
        self.assertEqual(values["cs"], 0x10)
        self.assertNotIn("gs_base", values)
        self.assertNotIn("k_gs_base", values)
        self.assertNotIn(0xFFFFFFFF81010005, values.values())

        user = x86_original_registers()
        user.update({
            "cs": 0x23,
            "ss": 0x2B,
            "rip": 0x08049053,
            "gs_base": 0x12340000,
            "k_gs_base": 0xFFFF888000010000,
        })
        ordered = X86_64.entry_register_values(
            request_address=0xFFFFFFFF83000000,
            result_address=0xFFFFFFFF83001000,
            result_size=4096,
            completion_address=0xFFFFFFFF81010005,
            control_state=None,
            original_registers=user,
            stack_pointer=0xFFFFFFFF84002000,
            entry_address=0xFFFFFFFF81010000,
        )
        self.assertEqual(
            ordered[:4],
            (
                ("ss", 0x18),
                ("cs", 0x10),
                ("gs_base", 0xFFFF888000010000),
                ("k_gs_base", 0x12340000),
            ),
        )

    def test_descriptor_accepts_userspace_but_rejects_incoherent_stops(self):
        user = x86_original_registers()
        user.update({"cs": 0x33, "ss": 0x2B, "rip": 0x401000})
        X86_64.validate_original_state(user)
        incoherent = dict(user)
        incoherent["ss"] = 0x18
        with self.assertRaisesRegex(ValueError, "matching.*segments"):
            X86_64.validate_original_state(incoherent)
        low_pc = x86_original_registers()
        low_pc["rip"] = 0x401000
        with self.assertRaisesRegex(ValueError, "high-half PC"):
            X86_64.validate_original_state(low_pc)

    def test_rsp_target_reads_and_writes_current_x86_64_cpu_registers(self):
        with tempfile.TemporaryDirectory() as temporary:
            kernel = Path(temporary) / "vmlinux"
            kernel.write_bytes(b"x86 kernel")
            client = FakeX86Qemu()
            target = RspQemuTarget(client, kernel, "01234567", X86_64)
            self.assertEqual(target.read_register(0, "rip"), 0x1010)
            target.write_register(0, "rdi", 0x1122334455667788)
            self.assertEqual(client.registers[5], bytes.fromhex("8877665544332211"))


class X86SnapshotAbiTests(unittest.TestCase):
    def test_request_and_ia32_task_decode_under_x86_64_kernel_metadata(self):
        request = build_snapshot_request(
            0xFFFFFFFF82200000, 0, 16, snapshot_abi=X86_64_SNAPSHOT_ABI
        )
        fields = struct.unpack("<IHHHHIQQIIQQQ", request)
        self.assertEqual(fields[6], 0xFFFFFFFF82200000)

        page = decode_response(x86_response(), expected_abi=X86_64_SNAPSHOT_ABI)
        self.assertEqual(page.arch, ARCH_X86)
        self.assertEqual(page.pointer_bits, 64)
        self.assertEqual(len(page.tasks), 1)
        self.assertEqual(page.tasks[0].abi_bits, 32)
        self.assertEqual(page.tasks[0].comm, "init")

    def test_x86_manifest_is_snapshot_only_and_has_no_pstate(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            kernel = directory / "vmlinux"
            probe = directory / "probe.bin"
            kernel.write_bytes(b"x86 kernel")
            probe.write_bytes(bytes.fromhex("e800000000cc"))
            digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
            document = {
                "format": "viros-callgate-v1",
                "architecture": "x86_64",
                "allow_transient_guest_modification": True,
                "kernel": {
                    "vmlinux": kernel.name,
                    "sha256": digest(kernel),
                    "build_id": "0123456789abcdef",
                },
                "regions": [
                    {"name": "code", "role": "code",
                     "virtual_address": "0xffffffff81010000",
                     "physical_address": "0x10100000", "size": 4096},
                    {"name": "data", "role": "data",
                     "virtual_address": "0xffffffff83000000",
                     "physical_address": "0x12000000", "size": 4096},
                    {"name": "stack", "role": "stack",
                     "virtual_address": "0xffffffff84000000",
                     "physical_address": "0x13000000", "size": 4096},
                ],
                "probe": {
                    "binary": probe.name, "sha256": digest(probe),
                    "code_region": "code", "capabilities": ["snapshot-v1"],
                    "entry_offset": 0, "completion_offset": 5,
                },
                "mailbox": {
                    "data_region": "data", "request_offset": 0,
                    "request_hex": build_snapshot_request(
                        0xFFFFFFFF82200000, 0, 8,
                        snapshot_abi=X86_64_SNAPSHOT_ABI,
                    ).hex(),
                    "result_offset": 64, "result_size": 2048,
                    "completion_magic_hex": struct.pack("<I", RESPONSE_MAGIC).hex(),
                },
                "invocation": {
                    "cpu": 0, "stack_region": "stack",
                    "stack_pointer": "0xffffffff84001000",
                    "timeout_seconds": 1.0,
                },
            }
            path = directory / "callgate.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            validated = load_and_validate_manifest(path)
            self.assertIs(validated.architecture, X86_64)
            self.assertIsNone(validated.pstate)
            self.assertEqual(validated.probe_capabilities, ("snapshot-v1",))
            self.assertEqual(validated.completion_address, 0xFFFFFFFF81010005)


class X86ProbePackagingTests(unittest.TestCase):
    def test_x86_audit_requires_elf64_little_endian_identity(self):
        wrong_class = SimpleNamespace(
            path=Path("probe.o"), elf_class=32, endian="<",
            machine=probe_tool.EM_X86_64,
        )
        with self.assertRaisesRegex(probe_tool.AuditError, "ELF64 little-endian"):
            probe_tool._audit_x86_64_identity(wrong_class)

        wrong_byte_order = SimpleNamespace(
            path=Path("probe.o"), elf_class=64, endian=">",
            machine=probe_tool.EM_X86_64,
        )
        with self.assertRaisesRegex(probe_tool.AuditError, "ELF64 little-endian"):
            probe_tool._audit_x86_64_identity(wrong_byte_order)

    def test_kernel_source_declares_x86_metadata_and_internal_completion(self):
        root = Path(__file__).resolve().parents[1]
        source = (root / "probe/kernel/viros_probe.c").read_text()
        header = (root / "probe/include/viros_probe_abi.h").read_text()
        kbuild = (root / "probe/kernel/Kbuild").read_text()
        for text in (
            "defined(CONFIG_X86_64)",
            "call viros_probe_main",
            '"int3\\n"',
            "test_tsk_thread_flag(task, TIF_ADDR32)",
            "VIROS_PROBE_ARCH_X86",
        ):
            with self.subTest(text=text):
                self.assertIn(text, source)
        self.assertIn("VIROS_PROBE_ARCH_X86 = 4", header)
        self.assertIn("-mno-red-zone", kbuild)

    def test_x86_object_link_and_package_metadata_are_strict(self):
        base = 0xFFFFFFFF81010000
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source = directory / "source.o"
            source.write_bytes(x86_elf()[0])
            audit = probe_tool.audit_object(source, "x86_64")
            self.assertEqual(audit["elf_class"], 64)
            self.assertEqual(audit["byte_order"], "little")

            build = directory / "probe.json"
            build.write_text(json.dumps({
                "schema": "viros-probe-build-v1",
                "arch": "x86_64",
                "object": source.name,
                "object_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
                "kernel": {
                    "sha256": "1" * 64,
                    "build_id": "0123456789abcdef",
                },
            }), encoding="utf-8")
            output = directory / "package"
            executable, flat = x86_elf(linked=True, base=base)
            commands = []

            def fake_run(command, check):
                self.assertTrue(check)
                commands.append(command)
                if command[0] == "x86_64-linux-gnu-ld":
                    Path(command[command.index("-o") + 1]).write_bytes(executable)
                elif command[0] == "x86_64-linux-gnu-objcopy":
                    Path(command[-1]).write_bytes(flat)
                else:
                    self.fail(f"unexpected tool {command[0]}")
                return SimpleNamespace(returncode=0)

            args = SimpleNamespace(
                build_manifest=build,
                output_dir=output,
                load_address=base,
                cross_ld="x86_64-linux-gnu-ld",
                objcopy="x86_64-linux-gnu-objcopy",
                max_alloc=65536,
            )
            with mock.patch.object(
                probe_tool.subprocess, "run", side_effect=fake_run
            ):
                package = probe_tool.package_object(args)

            self.assertEqual(commands[0][1:3], ["-m", "elf_x86_64"])
            self.assertEqual(package["arch"], "x86_64")
            self.assertEqual(package["capabilities"], ["snapshot-v1"])
            self.assertEqual(package["completion_offset"], 5)
            self.assertEqual(
                package["call_abi"]["argument_registers"],
                ["rdi", "rsi", "rdx"],
            )
            self.assertNotIn("link_register", package["call_abi"])
            self.assertEqual(
                package["call_abi"]["return_path"],
                "internal-call-fallthrough",
            )
            self.assertFalse(package["elf_abi"]["red_zone"])
            loaded, binary = probe_tool.load_probe_package(
                output / "package.json"
            )
            self.assertEqual(loaded["elf_abi"]["machine"], "EM_X86_64")
            self.assertEqual(binary, (output / "viros_probe.bin").resolve())

    def test_x86_linker_rejects_low_or_unaligned_load_addresses(self):
        with self.assertRaisesRegex(probe_tool.AuditError, "canonical kernel"):
            probe_tool.linker_script(0x100000, "x86_64")
        with self.assertRaisesRegex(probe_tool.AuditError, "16-byte aligned"):
            probe_tool.linker_script(0xFFFFFFFF81010001, "x86_64")


if __name__ == "__main__":
    unittest.main()
