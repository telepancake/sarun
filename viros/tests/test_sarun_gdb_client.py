from __future__ import annotations

import hashlib
import struct
import tempfile
import unittest

from inferiors.sarun_gdb_client import (
    Executable,
    PREFIX,
    ClientProtocolError,
    _gdb_script,
    _validated_debug_identity,
    decode_start_line,
)
from pathlib import Path
from probe.elf_load_identity import elf_load_identity
from tests.test_newc_userspace import elf64


def atom(payload: bytes, *, compound: bool = False) -> bytes:
    if len(payload) == 1 and payload[0] < 0xC0 and not compound:
        return payload
    if len(payload) <= 55:
        return bytes((0xC0 + len(payload),)) + payload
    width = (len(payload).bit_length() + 7) // 8
    return bytes((0xF8 + width,)) + len(payload).to_bytes(width, "little") + payload


def record(*fields: bytes) -> bytes:
    return atom(b"".join(fields), compound=True)


def start_line(
    *,
    manifest: bytes = b"image/kernel/callgate.json",
    service: bytes = b"debug-1",
    profile: int = 1,
    executable_identity: bytes = b"0123456789abcdef",
) -> bytes:
    executable = record(
        atom(b"/usr/sbin/quagga"),
        atom(executable_identity),
        atom(bytes(range(32))),
        atom((1234).to_bytes(2, "little")),
        atom(b"image/symbols/quagga.elf"),
        atom(bytes(reversed(range(32)))),
        atom((5678).to_bytes(2, "little")),
        atom(b"@"),
        atom(bytes((183,))),
        atom(b"\x01"),
    )
    executable_list = record(atom(b"\x01"), executable)
    catalog = record(
        atom(b"image/image.json"),
        atom(bytes((profile,))),
        atom(b"/init"),
        executable_list,
    )
    option = record(atom(b"\x01"), catalog)
    start = record(atom(manifest), option, atom(service))
    wire = atom(b"\x01") + start
    return PREFIX + wire.hex().encode() + b"\n"


def loadable_elf64() -> bytes:
    payload = b"\x90" * 16
    file_size = 64 + 56 + len(payload)
    ident = b"\x7fELF\x02\x01\x01" + b"\0" * 9
    header = ident + struct.pack(
        "<HHIQQQIHHHHHH",
        2,
        62,
        1,
        0x400078,
        64,
        0,
        0,
        64,
        56,
        1,
        0,
        0,
        0,
    )
    program = struct.pack(
        "<IIQQQQQQ",
        1,
        5,
        0,
        0x400000,
        0x400000,
        file_size,
        file_size,
        0x1000,
    )
    return header + program + payload


def executable_for(data: bytes, identity: str) -> Executable:
    return Executable(
        guest_path="/bin/app",
        build_id=identity,
        runtime_sha256="00" * 32,
        runtime_size=len(data),
        debug_elf="image/symbols/app.elf",
        debug_sha256=hashlib.sha256(data).hexdigest(),
        debug_size=len(data),
        elf_class=64,
        elf_machine=62,
        source_view="provider-root",
    )


class ManagedGdbClientTests(unittest.TestCase):
    def test_debug_identity_accepts_gnu_build_id_without_fallback(self):
        build_id = "0123456789abcdef"
        data = elf64(bytes.fromhex(build_id), dwarf=True)
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "app"
            path.write_bytes(data)
            self.assertEqual(
                _validated_debug_identity(
                    path, executable_for(data, build_id), data
                ),
                "gnu-build-id",
            )

            with self.assertRaisesRegex(
                ClientProtocolError, "GNU build ID changed"
            ):
                _validated_debug_identity(
                    path,
                    executable_for(data, "fedcba9876543210"),
                    data,
                )

    def test_debug_identity_accepts_only_exact_loadable_fallback(self):
        data = loadable_elf64()
        fingerprint = elf_load_identity(data).fingerprint
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "app"
            path.write_bytes(data)
            executable = executable_for(data, fingerprint)
            self.assertEqual(
                _validated_debug_identity(path, executable, data),
                "loadable-content-sha256",
            )
            wrong = executable_for(data, "00" * 32)
            with self.assertRaisesRegex(
                ClientProtocolError, "executable identity changed"
            ):
                _validated_debug_identity(path, wrong, data)

    def test_typed_catalog_preserves_exact_inferior_identity(self):
        start = decode_start_line(start_line())
        self.assertEqual(start.manifest, "image/kernel/callgate.json")
        self.assertEqual(start.service, "debug-1")
        self.assertEqual(len(start.executables), 1)
        executable = start.executables[0]
        self.assertEqual(executable.guest_path, "/usr/sbin/quagga")
        self.assertEqual(executable.build_id, "0123456789abcdef")
        self.assertEqual(executable.runtime_size, 1234)
        self.assertEqual(executable.debug_elf, "image/symbols/quagga.elf")
        self.assertEqual(executable.debug_size, 5678)
        self.assertEqual(executable.elf_class, 64)
        self.assertEqual(executable.elf_machine, 183)
        self.assertEqual(executable.source_view, "provider-root")

    def test_typed_catalog_preserves_fallback_in_the_existing_identity_slot(self):
        fingerprint = b"a5" * 32
        executable = decode_start_line(
            start_line(executable_identity=fingerprint)
        ).executables[0]
        self.assertEqual(executable.build_id, fingerprint.decode())

    def test_rejects_host_paths_and_command_shaped_service_names(self):
        for line in (
            start_line(manifest=b"/host/callgate.json"),
            start_line(manifest=b"../callgate.json"),
            start_line(service=b"debug-1\nquit"),
        ):
            with self.subTest(line=line):
                with self.assertRaises(ClientProtocolError):
                    decode_start_line(line)

    def test_gdb_script_quotes_resource_paths_and_imports_dependencies(self):
        script = _gdb_script(
            decode_start_line(start_line()), [], Path('/opt/viros provider')
        ).decode()
        self.assertIn('file "/image/kernel/vmlinux"', script)
        self.assertIn("import sys, json", script)
        self.assertIn('set sysroot "/opt/viros provider/sysroot"', script)
        self.assertIn("python finalize()", script)
        self.assertIn("target remote | sarun service dial debug-1", script)

    def test_arm_and_mmips_image_profiles_are_accepted(self):
        for profile in (3, 4):
            with self.subTest(profile=profile):
                self.assertEqual(len(decode_start_line(start_line(profile=profile)).executables), 1)


if __name__ == "__main__":
    unittest.main()
