from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from probe import kernel_bundle


class KernelBundleTests(unittest.TestCase):
    @staticmethod
    def _tool(path: Path) -> Path:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\nexit 0\n")
        path.chmod(0o755)
        return path

    def test_kbuild_output_supplies_unambiguous_kernel_paths(self):
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw)
            vmlinux = output / "vmlinux"
            boot = output / "arch/arm64/boot/Image"
            vmlinux.write_bytes(b"elf")
            boot.parent.mkdir(parents=True)
            boot.write_bytes(b"image")
            self.assertEqual(
                kernel_bundle.default_vmlinux(output, None), vmlinux
            )
            self.assertEqual(
                kernel_bundle.default_boot_image(output, "aarch64", None), boot
            )
            explicit = output / "other-image"
            self.assertEqual(
                kernel_bundle.default_boot_image(output, "aarch64", explicit),
                explicit,
            )

    def test_missing_standard_boot_image_has_actionable_error(self):
        with tempfile.TemporaryDirectory() as raw:
            with self.assertRaisesRegex(
                kernel_bundle.BundleError, r"arch/x86/boot/bzImage.*--boot-image"
            ):
                kernel_bundle.default_boot_image(Path(raw), "x86_64", None)

    def test_exact_absolute_kbuild_commands_supply_tools(self):
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "kernel"
            tool_dir = Path(raw) / "sdk/bin"
            make = self._tool(tool_dir / "make")
            compiler = self._tool(tool_dir / "mipsel-openwrt-linux-musl-gcc")
            linker = self._tool(tool_dir / "mipsel-openwrt-linux-musl-ld")
            objcopy = self._tool(tool_dir / "mipsel-openwrt-linux-musl-objcopy")
            event = output / "kernel/viros/.viros_event.o.cmd"
            event.parent.mkdir(parents=True)
            event.write_text(f"cmd_kernel/viros/event.o := {compiler} -c event.c\n")
            scratch = output / "kernel/viros/.viros_scratch.o.cmd"
            scratch.write_text(f"cmd_kernel/viros/scratch.o := {compiler} -c scratch.S\n")
            (output / ".vmlinux.cmd").write_text(
                f"cmd_vmlinux := {linker} -o vmlinux; {make} -f postlink\n"
            )
            image_cmd = output / "arch/mips/boot/.vmlinux.bin.cmd"
            image_cmd.parent.mkdir(parents=True)
            image_cmd.write_text(f"cmd_image := {objcopy} -O binary vmlinux image\n")

            self.assertEqual(
                kernel_bundle.infer_recorded_tool(output, "make"), str(make)
            )
            self.assertEqual(
                kernel_bundle.infer_recorded_tool(output, "compiler"), str(compiler)
            )
            self.assertEqual(
                kernel_bundle.infer_recorded_tool(output, "cross-ld"), str(linker)
            )
            self.assertEqual(
                kernel_bundle.infer_recorded_tool(output, "objcopy"), str(objcopy)
            )
            self.assertEqual(
                kernel_bundle.infer_cross_compile(compiler, linker, objcopy),
                str(tool_dir / "mipsel-openwrt-linux-musl-"),
            )

    def test_bare_recorded_tool_replays_captured_path_and_unavailable_fails(self):
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw)
            record = output / "kernel/viros/.viros_event.o.cmd"
            record.parent.mkdir(parents=True)
            record.write_text("cmd_event.o := mipsel-openwrt-linux-musl-gcc -c event.c\n")
            captured = self._tool(Path(raw) / "sdk/bin/compiler")
            with mock.patch.object(
                kernel_bundle.shutil, "which", return_value=str(captured)
            ):
                self.assertEqual(
                    kernel_bundle.infer_recorded_tool(output, "compiler"),
                    "mipsel-openwrt-linux-musl-gcc",
                )

            relative = self._tool(
                output / "sdk/bin/mipsel-openwrt-linux-musl-gcc"
            )
            record.write_text(
                "cmd_event.o := sdk/bin/mipsel-openwrt-linux-musl-gcc -c event.c\n"
            )
            self.assertEqual(
                kernel_bundle.infer_recorded_tool(output, "compiler"),
                str(relative.absolute()),
            )

            missing = output / "missing/mipsel-openwrt-linux-musl-gcc"
            record.write_text(f"cmd_event.o := {missing} -c event.c\n")
            with self.assertRaisesRegex(
                kernel_bundle.BundleError, r"not available in this captured build box"
            ):
                kernel_bundle.infer_recorded_tool(output, "compiler")

    def test_clang_record_does_not_guess_cross_compile(self):
        with self.assertRaisesRegex(
            kernel_bundle.BundleError, r"pass --cross-compile explicitly"
        ):
            kernel_bundle.infer_cross_compile(
                Path("/sdk/bin/clang-21"),
                Path("/sdk/bin/ld.lld-21"),
                Path("/sdk/bin/llvm-objcopy-21"),
            )

    def test_probe_build_command_preserves_exact_llvm_selection(self):
        effective = kernel_bundle.effective_probe_make_args(
            ["LLVM=-21", "LLVM_IAS=1"], Path("/sdk/bin/clang-21")
        )
        self.assertEqual(
            effective,
            ["LLVM=-21", "LLVM_IAS=1", "CC=/sdk/bin/clang-21"],
        )
        command = kernel_bundle.probe_build_command(
            python="/managed/python",
            kbuild_output=Path("/build/kernel"),
            output=Path("/bundle/probe-build"),
            architecture="aarch64",
            cross_compile="aarch64-openwrt-linux-musl-",
            make="/sdk/bin/make",
            make_args=effective,
            vmlinux=Path("/bundle/vmlinux"),
        )
        self.assertEqual(command[0], "/managed/python")
        self.assertIn("/sdk/bin/make", command)
        self.assertEqual(
            command[-6:],
            [
                "--make-arg", "LLVM=-21",
                "--make-arg", "LLVM_IAS=1",
                "--make-arg", "CC=/sdk/bin/clang-21",
            ],
        )
        cross = command.index("--cross-compile")
        self.assertEqual(command[cross + 1], "aarch64-openwrt-linux-musl-")

    def test_explicit_tool_identity_records_version_path_and_hash(self):
        with tempfile.TemporaryDirectory() as raw:
            wrapper = Path(raw) / "tool-wrapper"
            wrapper.write_text(
                "#!/bin/sh\nprintf '%s version 21.0.1\\n' \"$(basename -- \"$0\")\"\n"
            )
            wrapper.chmod(0o755)
            tool = Path(raw) / "clang-21"
            tool.symlink_to(wrapper.name)
            identity = kernel_bundle.resolve_tool(str(tool), "compiler")
            self.assertEqual(identity["argument"], str(tool))
            self.assertEqual(identity["path"], str(tool.absolute()))
            self.assertEqual(identity["resolved_path"], str(wrapper.resolve()))
            self.assertEqual(identity["version"], "clang-21 version 21.0.1")
            self.assertEqual(
                identity["sha256"], hashlib.sha256(wrapper.read_bytes()).hexdigest()
            )

    def test_manifest_hashes_every_portable_artifact_and_toolchain(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "vmlinux").write_bytes(b"symbols")
            (root / "kernel").write_bytes(b"boot")
            (root / "kernel.config").write_text("CONFIG_DEBUG_INFO=y\n")
            (root / "nested").mkdir()
            (root / "nested/artifact").write_bytes(b"payload")
            tools = {
                "compiler": {
                    "argument": "clang-21",
                    "path": "/sdk/bin/clang-21",
                    "resolved_path": "/sdk/bin/clang-21",
                    "sha256": "a" * 64,
                    "version": "clang version 21",
                }
            }
            with mock.patch.object(kernel_bundle, "gnu_build_id", return_value="12345678"):
                document = kernel_bundle.write_bundle_manifest(
                    root,
                    architecture="aarch64",
                    kbuild_output=Path("/box/build"),
                    cross_compile="aarch64-linux-musl-",
                    requested_make_args=["LLVM=-21"],
                    probe_make_args=["LLVM=-21", "CC=/sdk/bin/clang-21"],
                    toolchain=tools,
                    original_vmlinux=Path("/box/build/vmlinux"),
                    original_boot_image=Path("/box/build/Image"),
                    runtime_offset=0,
                )
            self.assertEqual(document["format"], kernel_bundle.BUNDLE_FORMAT)
            self.assertEqual(document["kbuild_arch"], "arm64")
            self.assertEqual(
                document["toolchain"]["requested_make_args"], ["LLVM=-21"]
            )
            self.assertEqual(
                document["toolchain"]["probe_make_args"],
                ["LLVM=-21", "CC=/sdk/bin/clang-21"],
            )
            self.assertEqual(document["toolchain"]["tools"], tools)
            rows = {row["path"]: row for row in document["artifacts"]}
            self.assertEqual(
                set(rows), {"kernel", "kernel.config", "nested/artifact", "vmlinux"}
            )
            self.assertEqual(
                rows["nested/artifact"]["sha256"],
                hashlib.sha256(b"payload").hexdigest(),
            )
            self.assertEqual(json.loads((root / "bundle.json").read_text()), document)

    def test_non_mmips_derives_mappings_but_rejects_partial_override(self):
        arguments = [
            "--arch", "x86_64",
            "--kbuild-output", "/build",
            "--vmlinux", "/build/vmlinux",
            "--boot-image", "/build/bzImage",
            "--output-dir", "/bundle",
            "--cross-compile", "x86_64-linux-gnu-",
            "--make", "/sdk/make",
            "--compiler", "/sdk/gcc",
            "--cross-ld", "/sdk/ld",
            "--objcopy", "/sdk/objcopy",
        ]
        with mock.patch.object(
            kernel_bundle, "build_bundle", return_value=Path("/bundle")
        ) as build:
            self.assertEqual(kernel_bundle.main(arguments), 0)
            build.assert_called_once()
        with self.assertRaisesRegex(kernel_bundle.BundleError, "all three"):
            kernel_bundle.main([*arguments, "--code-gpa", "0x1000"])

    def test_mmips_rejects_caller_gpa_instead_of_overriding_kseg0(self):
        arguments = [
            "--arch", "mmips",
            "--kbuild-output", "/build",
            "--vmlinux", "/build/vmlinux",
            "--boot-image", "/build/vmlinux",
            "--output-dir", "/bundle",
            "--cross-compile", "mipsel-linux-musl-",
            "--make", "/sdk/make",
            "--compiler", "/sdk/gcc",
            "--cross-ld", "/sdk/ld",
            "--objcopy", "/sdk/objcopy",
            "--code-gpa", "0x1000",
        ]
        with self.assertRaisesRegex(kernel_bundle.BundleError, "derived from KSEG0"):
            kernel_bundle.main(arguments)

    def test_make_assignments_reject_shell_or_line_syntax(self):
        self.assertEqual(
            kernel_bundle.validate_make_args(["LLVM=-21", "LLVM_IAS=1"]),
            ["LLVM=-21", "LLVM_IAS=1"],
        )
        for invalid in ("LLVM", "LLVM=-21\nCC=other", "A-B=value"):
            with self.assertRaises(kernel_bundle.BundleError):
                kernel_bundle.validate_make_args([invalid])


if __name__ == "__main__":
    unittest.main()
