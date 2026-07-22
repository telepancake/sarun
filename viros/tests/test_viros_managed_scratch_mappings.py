from __future__ import annotations

import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest

from tests.test_probe_scratch import (
    BSS_GVA,
    CODE_GVA,
    PAGE_SIZE,
    _elf_with_scratch,
    _mips_elf_with_scratch,
)


PROJECT = Path(__file__).resolve().parents[1]
VIROS = PROJECT / "viros.sh"


def _rename_code_start_to_text(image: bytearray) -> None:
    old = b"__viros_scratch_code_start\0"
    offset = image.index(old)
    replacement = b"_text\0" + b"\0" * (len(old) - len(b"_text\0"))
    image[offset:offset + len(old)] = replacement


def _run_mapping(target: str, vmlinux: bytes, document: dict) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory(prefix="scratch-map-", dir=PROJECT) as raw:
        directory = Path(raw)
        kernel = directory / "vmlinux"
        scratch = directory / "scratch.json"
        output = directory / "arguments"
        kernel.write_bytes(vmlinux)
        scratch.write_text(json.dumps(document), encoding="utf-8")
        environment = os.environ.copy()
        environment["VIROS_SOURCE_ONLY"] = "1"
        result = subprocess.run(
            [
                "bash", "-c",
                'source "$1"; write_managed_scratch_gpa_arguments '
                '"$2" "$3" "$4" "$5" python3',
                "_", str(VIROS), target, str(kernel), str(scratch), str(output),
            ],
            cwd=PROJECT,
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=10,
            check=False,
        )
        if output.exists():
            result.mapping_arguments = output.read_text(encoding="ascii").splitlines()
        else:
            result.mapping_arguments = []
        return result


def _scratch_document(arch: str, addresses: tuple[int, int, int]) -> dict:
    return {
        "arch": arch,
        "page_size": PAGE_SIZE,
        "regions": {
            name: {"gva": address, "size": PAGE_SIZE}
            for name, address in zip(("code", "data", "stack"), addresses)
        },
    }


class ManagedScratchMappingTests(unittest.TestCase):
    def test_x86_uses_each_containing_load_segment_physical_address(self):
        image = bytearray(_elf_with_scratch())
        struct.pack_into("<H", image, 18, 62)
        phoff = len(image)
        headers = (
            struct.pack(
                "<IIQQQQQQ", 1, 5, 0, CODE_GVA, 0x01000000,
                PAGE_SIZE, PAGE_SIZE, PAGE_SIZE,
            )
            + struct.pack(
                "<IIQQQQQQ", 1, 6, 0, BSS_GVA, 0x02000000,
                0, 2 * PAGE_SIZE, PAGE_SIZE,
            )
        )
        image.extend(headers)
        struct.pack_into("<Q", image, 32, phoff)
        struct.pack_into("<HH", image, 54, 56, 2)
        result = _run_mapping(
            "x86", bytes(image),
            _scratch_document(
                "x86_64", (CODE_GVA, BSS_GVA, BSS_GVA + PAGE_SIZE),
            ),
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(
            result.mapping_arguments,
            [
                "--code-gpa", "0x1000000",
                "--data-gpa", "0x2000000",
                "--stack-gpa", "0x2001000",
            ],
        )

    def test_arm_virt_uses_published_link_and_machine_load_relationship(self):
        text = 0x80008000
        image = bytearray(_mips_elf_with_scratch(code_gva=text))
        struct.pack_into("<H", image, 18, 40)
        _rename_code_start_to_text(image)
        result = _run_mapping(
            "arm", bytes(image),
            _scratch_document("arm", (text, text + PAGE_SIZE, text + 2 * PAGE_SIZE)),
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(
            result.mapping_arguments,
            [
                "--code-gpa", "0x40008000",
                "--data-gpa", "0x40009000",
                "--stack-gpa", "0x4000a000",
            ],
        )

    def test_aarch64_virt_rejects_a_different_kernel_link_layout(self):
        image = bytearray(_elf_with_scratch())
        _rename_code_start_to_text(image)
        result = _run_mapping(
            "arm64", bytes(image),
            _scratch_document(
                "aarch64", (CODE_GVA, CODE_GVA + PAGE_SIZE, CODE_GVA + 2 * PAGE_SIZE),
            ),
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expected the published configuration address", result.stdout)

    def test_aarch64_virt_uses_published_link_and_machine_load_relationship(self):
        text = 0xFFFF800010080000
        image = bytearray(_elf_with_scratch(code_gva=text))
        _rename_code_start_to_text(image)
        result = _run_mapping(
            "arm64", bytes(image),
            _scratch_document(
                "aarch64", (text, text + PAGE_SIZE, text + 2 * PAGE_SIZE),
            ),
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(
            result.mapping_arguments,
            [
                "--code-gpa", "0x40080000",
                "--data-gpa", "0x40081000",
                "--stack-gpa", "0x40082000",
            ],
        )

    def test_managed_manifest_call_supplies_all_derived_gpas(self):
        script = VIROS.read_text(encoding="utf-8")
        body = script.split("build_inferiors_artifacts() {", 1)[1].split(
            "\nfind_kernel_config() {", 1,
        )[0]
        self.assertIn("write_managed_scratch_gpa_arguments", body)
        self.assertIn('((${#scratch_gpa_args[@]} == 6))', body)
        self.assertIn('"${scratch_gpa_args[@]}" --cpu 0', body)
        self.assertIn('if [[ "$target" != mmips ]]', body)


if __name__ == "__main__":
    unittest.main()
