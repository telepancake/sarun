from __future__ import annotations

from contextlib import contextmanager
import gzip
import hashlib
import io
from pathlib import Path
import stat
import tempfile
import unittest

from probe.image_inspector import CapturedArtifact
from probe.provider_derivation import (
    SelectedImageRequest,
    SelectedKernelInitramfsRequest,
)
from probe.selected_bundle_orchestrator import (
    CatalogExecutable,
    FixedBootProfile,
    PLAN_FORMAT,
    SelectedBundleExecutionRequest,
    SelectedBundleError,
    execute_selected_initramfs,
    orchestrate_selected_initramfs,
)
from probe.elf_load_identity import elf_load_identity
from tests.test_newc_userspace import elf32, elf64, load_elf64, newc


ARCHITECTURE_FIXTURES = {
    "aarch64": {
        "machine": 183,
        "boot_path": "arch/arm64/boot/Image",
        "boot_command": "arch/arm64/boot/.Image.cmd",
    },
    "arm": {
        "machine": 40,
        "boot_path": "arch/arm/boot/zImage",
        "boot_command": "arch/arm/boot/.zImage.cmd",
    },
    "mmips": {
        "machine": 8,
        "boot_path": "vmlinux",
        "boot_command": ".vmlinux.cmd",
    },
    "x86_64": {
        "machine": 62,
        "boot_path": "arch/x86/boot/bzImage",
        "boot_command": "arch/x86/boot/.bzImage.cmd",
    },
}


def kernel_fixture(architecture: str) -> tuple[bytes, bytes]:
    machine = int(ARCHITECTURE_FIXTURES[architecture]["machine"])
    build_id = bytes.fromhex("fedcba9876543210")
    if architecture in {"arm", "mmips"}:
        vmlinux = elf32(build_id, machine=machine, dwarf=True)
    else:
        vmlinux = elf64(build_id, machine=machine, elf_type=2, dwarf=True)
    if architecture == "x86_64":
        boot = bytearray(0x240)
        boot[0x202:0x206] = b"HdrS"
        boot[0x236:0x238] = (1).to_bytes(2, "little")
        return vmlinux, bytes(boot)
    if architecture == "aarch64":
        boot = bytearray(64)
        boot[56:60] = b"ARM\x64"
        return vmlinux, bytes(boot)
    if architecture == "arm":
        boot = bytearray(0x28)
        boot[0x24:0x28] = (0x016F2818).to_bytes(4, "little")
        return vmlinux, bytes(boot)
    return vmlinux, vmlinux


def artifact(
    data: bytes,
    path: str,
    *,
    roles: tuple[str, ...] = (),
    architecture: str | None = None,
    box_id: int = 11,
) -> CapturedArtifact:
    return CapturedArtifact(
        box_id=box_id,
        path=path,
        size=len(data),
        sha256=hashlib.sha256(data).hexdigest(),
        record_id=f"captured:{box_id}:{path}",
        roles=roles,
        architecture=architecture,
    )


class MemorySource:
    def __init__(self, rows: dict[tuple[int, str], bytes]):
        self.rows = rows

    @contextmanager
    def open_artifact(self, box_id: int, relative_path: str):
        yield io.BytesIO(self.rows[(box_id, relative_path)])


def request_and_source(
    *,
    compressed: bool = True,
    include_config: bool = True,
    architecture: str = "x86_64",
):
    build_id = bytes.fromhex("0123456789abcdef")
    runtime = elf64(build_id)
    debug = elf64(build_id, dwarf=True)
    archive = newc([
        ("init", stat.S_IFREG | 0o755, b"#!/bin/sh\n"),
        ("usr/sbin/quagga", stat.S_IFREG | 0o755, runtime),
    ])
    vmlinux, boot_image = kernel_fixture(architecture)
    fixture = ARCHITECTURE_FIXTURES[architecture]
    boot_path = str(fixture["boot_path"])
    boot_command = str(fixture["boot_command"])
    static_tool = elf64(bytes.fromhex("aaaaaaaa55555555"), elf_type=2)
    selected_bytes = gzip.compress(archive, mtime=0) if compressed else archive
    selected = artifact(selected_bytes, "out/openwrt-initramfs.cpio.gz")
    kernel_roles = ("vmlinux", "kernel-boot") if architecture == "mmips" else ("vmlinux",)
    rows_with_bytes = [
        (artifact(vmlinux, "build/linux/vmlinux", roles=kernel_roles, architecture=architecture), vmlinux),
        (artifact(b"loader", "build/linux/vmlinux-gdb.py"), b"loader"),
        (artifact(b"helper", "build/linux/scripts/gdb/linux/tasks.py"), b"helper"),
        (artifact(b"cmd_x := /sdk/bin/gcc -c scratch.S\n", "build/linux/kernel/viros/.viros_scratch.o.cmd"), b"cmd_x := /sdk/bin/gcc -c scratch.S\n"),
        (artifact(b"cmd_x := /sdk/bin/gcc -c event.c\n", "build/linux/kernel/viros/.viros_event.o.cmd"), b"cmd_x := /sdk/bin/gcc -c event.c\n"),
        (artifact(b"cmd_x := /sdk/bin/ld -o vmlinux; /sdk/bin/make post; /sdk/bin/objcopy vmlinux image\n" if architecture == "mmips" else b"cmd_x := /sdk/bin/ld -o vmlinux; /sdk/bin/make post\n", "build/linux/.vmlinux.cmd"), b"cmd_x := /sdk/bin/ld -o vmlinux; /sdk/bin/make post; /sdk/bin/objcopy vmlinux image\n" if architecture == "mmips" else b"cmd_x := /sdk/bin/ld -o vmlinux; /sdk/bin/make post\n"),
        (artifact(debug, "build/packages/quagga.debug"), debug),
        (artifact(static_tool, "sdk/bin/make"), static_tool),
        (artifact(static_tool, "sdk/bin/gcc"), static_tool),
        (artifact(static_tool, "sdk/bin/ld"), static_tool),
        (artifact(static_tool, "sdk/bin/objcopy"), static_tool),
    ]
    if architecture != "mmips":
        rows_with_bytes.extend((
            (artifact(boot_image, f"build/linux/{boot_path}", roles=("kernel-boot",), architecture=architecture), boot_image),
            (artifact(b"cmd_x := /sdk/bin/objcopy vmlinux image\n", f"build/linux/{boot_command}"), b"cmd_x := /sdk/bin/objcopy vmlinux image\n"),
        ))
    if include_config:
        rows_with_bytes.append((artifact(b"CONFIG_DEBUG_INFO=y\n", "build/linux/.config"), b"CONFIG_DEBUG_INFO=y\n"))
    catalog = tuple(row for row, _contents in rows_with_bytes)
    source_rows = {(selected.box_id, selected.path): selected_bytes}
    source_rows.update({(row.box_id, row.path): contents for row, contents in rows_with_bytes})
    return SelectedImageRequest(selected, catalog), MemorySource(source_rows), archive


def execution_request(
    request: SelectedImageRequest | SelectedKernelInitramfsRequest,
    fixed_profile: FixedBootProfile | None = None,
) -> SelectedBundleExecutionRequest:
    by_path = {row.path: row for row in request.captured_artifacts}
    return SelectedBundleExecutionRequest(request, (
        CatalogExecutable("make", "/sdk/bin/make", by_path["sdk/bin/make"]),
        CatalogExecutable("compiler", "/sdk/bin/gcc", by_path["sdk/bin/gcc"]),
        CatalogExecutable("cross-ld", "/sdk/bin/ld", by_path["sdk/bin/ld"]),
        CatalogExecutable("objcopy", "/sdk/bin/objcopy", by_path["sdk/bin/objcopy"]),
    ), fixed_profile)


def pair_request_and_source(
    *, compressed: bool = True, architecture: str = "x86_64"
):
    request, source, archive = request_and_source(
        compressed=compressed, architecture=architecture
    )
    _vmlinux, standard_kernel = kernel_fixture(architecture)
    selected_kernel_bytes = standard_kernel + b"\0"
    selected_kernel = artifact(
        selected_kernel_bytes,
        "out/selected-bzImage",
        architecture=architecture,
    )
    competing_bytes = standard_kernel + b"\1"
    competing = artifact(
        competing_bytes,
        "other/linux/bzImage",
        roles=("kernel-boot",),
        architecture=architecture,
    )
    catalog = (*request.captured_artifacts, competing)
    source.rows[(selected_kernel.box_id, selected_kernel.path)] = selected_kernel_bytes
    source.rows[(competing.box_id, competing.path)] = competing_bytes
    return (
        SelectedKernelInitramfsRequest(
            selected_kernel, request.selected, catalog
        ),
        source,
        archive,
        selected_kernel_bytes,
    )


class SelectedBundleOrchestratorTests(unittest.TestCase):
    def test_loadable_fallback_identity_reaches_internal_image_bundle(self):
        request, source, _archive = request_and_source()
        runtime = load_elf64(stripped_sections=True)
        debug = load_elf64(dwarf=True)
        archive = newc(
            [
                ("init", stat.S_IFREG | 0o755, b"#!/bin/sh\n"),
                ("usr/sbin/quagga", stat.S_IFREG | 0o755, runtime),
            ]
        )
        selected_bytes = gzip.compress(archive, mtime=0)
        selected = artifact(selected_bytes, request.selected.path)
        old_debug = next(
            row
            for row in request.captured_artifacts
            if row.path == "build/packages/quagga.debug"
        )
        replacement = artifact(debug, old_debug.path)
        catalog = tuple(
            replacement if row is old_debug else row
            for row in request.captured_artifacts
        )
        changed = SelectedImageRequest(selected, catalog)
        source.rows[(selected.box_id, selected.path)] = selected_bytes
        source.rows[(replacement.box_id, replacement.path)] = debug
        fingerprint = elf_load_identity(debug).fingerprint

        def fake_kernel_builder(arguments):
            arguments.output_dir.mkdir()
            (arguments.output_dir / "vmlinux").write_bytes(b"kernel symbols")
            (arguments.output_dir / "kernel").write_bytes(b"boot image")
            (arguments.output_dir / "bundle.json").write_text(
                '{"format":"viros-kernel-bundle-v1",'
                '"architecture":"x86_64"}\n'
            )
            return arguments.output_dir

        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "complete"
            result = execute_selected_initramfs(
                execution_request(changed),
                source,
                output,
                _kernel_builder=fake_kernel_builder,
            )
            row = result.plan.userspace.executables[0]
            self.assertEqual(row["build_id"], fingerprint)
            symbol = output / "image-bundle/symbols" / f"{fingerprint}.elf"
            self.assertEqual(symbol.read_bytes(), debug)

    def test_all_fixed_profiles_execute_combined_and_pair_selections(self):
        for fixed_profile in FixedBootProfile:
            for pair in (False, True):
                with self.subTest(
                    architecture=fixed_profile.architecture, pair=pair
                ):
                    if pair:
                        request, source, _archive, expected_boot = (
                            pair_request_and_source(
                                architecture=fixed_profile.architecture
                            )
                        )
                    else:
                        request, source, _archive = request_and_source(
                            architecture=fixed_profile.architecture
                        )
                        expected_boot = kernel_fixture(
                            fixed_profile.architecture
                        )[1]
                    observed = {}

                    def fake_kernel_builder(arguments):
                        observed["arch"] = arguments.arch
                        observed["boot"] = arguments.boot_image.read_bytes()
                        arguments.output_dir.mkdir()
                        (arguments.output_dir / "vmlinux").write_bytes(
                            b"kernel symbols"
                        )
                        (arguments.output_dir / "kernel").write_bytes(
                            arguments.boot_image.read_bytes()
                        )
                        (arguments.output_dir / "bundle.json").write_text(
                            '{"format":"viros-kernel-bundle-v1",'
                            f'"architecture":"{arguments.arch}"}}\n'
                        )
                        return arguments.output_dir

                    with tempfile.TemporaryDirectory() as raw:
                        output = Path(raw) / "complete"
                        result = execute_selected_initramfs(
                            execution_request(request, fixed_profile),
                            source,
                            output,
                            _kernel_builder=fake_kernel_builder,
                        )
                        descriptor = result.plan.descriptor()
                        image = __import__("json").loads(
                            (output / "image-bundle/image.json").read_text()
                        )
                        self.assertEqual(observed["arch"], fixed_profile.architecture)
                        self.assertEqual(observed["boot"], expected_boot)
                        self.assertEqual(
                            descriptor["architecture"], fixed_profile.architecture
                        )
                        self.assertEqual(descriptor["profile"], fixed_profile.profile)
                        self.assertEqual(
                            image["architecture"], fixed_profile.architecture
                        )
                        self.assertEqual(
                            image["boot"]["profile"], fixed_profile.profile
                        )

    def test_profile_rejects_any_conflicting_architecture_tag(self):
        request, source, _archive = request_and_source()
        foreign = artifact(
            b"not consulted",
            "build/packages/foreign.debug",
            architecture="arm",
        )
        changed = SelectedImageRequest(
            request.selected, (*request.captured_artifacts, foreign)
        )
        source.rows[(foreign.box_id, foreign.path)] = b"not consulted"
        with tempfile.TemporaryDirectory() as raw:
            with self.assertRaisesRegex(
                SelectedBundleError, "incompatible with fixed profile"
            ):
                orchestrate_selected_initramfs(
                    changed,
                    source,
                    Path(raw) / "derived",
                    fixed_profile=FixedBootProfile.X86_64,
                )

    def test_omitted_profile_is_accepted_only_when_tags_are_unambiguous(self):
        request, source, _archive = request_and_source()
        foreign = artifact(b"foreign", "foreign", architecture="arm")
        changed = SelectedImageRequest(
            request.selected, (*request.captured_artifacts, foreign)
        )
        source.rows[(foreign.box_id, foreign.path)] = b"foreign"
        with tempfile.TemporaryDirectory() as raw:
            with self.assertRaisesRegex(
                SelectedBundleError, "exactly one fixed architecture"
            ):
                orchestrate_selected_initramfs(
                    changed, source, Path(raw) / "derived"
                )

    def test_selected_pair_preserves_initramfs_and_forces_selected_kernel(self):
        request, source, archive, selected_kernel = pair_request_and_source()
        selected_initramfs = source.rows[
            (request.initramfs.box_id, request.initramfs.path)
        ]
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "derived"
            plan = orchestrate_selected_initramfs(request, source, output).plan
            self.assertEqual(
                plan.selected_derivation["layout"], "selected-kernel-initramfs"
            )
            self.assertEqual(
                (output / "selected-image" / str(plan.initramfs["path"])).read_bytes(),
                selected_initramfs,
            )
            self.assertNotEqual(selected_initramfs, archive)
            self.assertEqual(
                plan.kernel_inputs["boot_image"]["sha256"],
                hashlib.sha256(selected_kernel).hexdigest(),
            )
            self.assertEqual(
                [row.code for row in plan.missing_requirements],
                ["catalog-backed-kbuild-executor"],
            )

    def test_selected_pair_accepts_uncompressed_newc(self):
        request, source, archive, _selected_kernel = pair_request_and_source(
            compressed=False
        )
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "derived"
            plan = orchestrate_selected_initramfs(request, source, output).plan
            self.assertEqual(
                plan.selected_derivation["initramfs_layout"], "cpio-newc"
            )
            self.assertEqual(
                (output / "selected-image/initramfs").read_bytes(), archive
            )

    def test_pair_executor_passes_exact_selected_kernel_to_bundle_builder(self):
        request, source, _archive, selected_kernel = pair_request_and_source()
        selected_initramfs = source.rows[
            (request.initramfs.box_id, request.initramfs.path)
        ]
        observed = {}

        def fake_kernel_builder(arguments):
            observed["boot"] = arguments.boot_image.read_bytes()
            observed["inside"] = arguments.boot_image.is_relative_to(
                arguments.kbuild_output
            )
            arguments.output_dir.mkdir()
            (arguments.output_dir / "vmlinux").write_bytes(b"kernel symbols")
            (arguments.output_dir / "kernel").write_bytes(
                arguments.boot_image.read_bytes()
            )
            (arguments.output_dir / "bundle.json").write_text(
                '{"format":"viros-kernel-bundle-v1","architecture":"x86_64"}\n'
            )
            return arguments.output_dir

        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "complete"
            result = execute_selected_initramfs(
                execution_request(request),
                source,
                output,
                _kernel_builder=fake_kernel_builder,
            )
            self.assertTrue(result.plan.ready)
            self.assertEqual(observed["boot"], selected_kernel)
            self.assertTrue(observed["inside"])
            self.assertEqual(
                (output / "image-bundle/rootfs.cpio").read_bytes(),
                selected_initramfs,
            )
            self.assertEqual(
                (output / "image-bundle/kernel/kernel").read_bytes(),
                selected_kernel,
            )

    def test_selected_gzip_newc_produces_exact_userspace_and_kernel_plan(self):
        request, source, archive = request_and_source()
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "derived"
            result = orchestrate_selected_initramfs(request, source, output)
            document = result.plan.descriptor()

            self.assertEqual(document["format"], PLAN_FORMAT)
            self.assertEqual(document["architecture"], "x86_64")
            self.assertFalse(document["ready"])
            self.assertEqual(document["kernel_init"], "/init")
            self.assertEqual(
                (output / document["image_bundle"]["initramfs"]).read_bytes(),
                archive,
            )
            executable = document["userspace"]["executables"][0]
            self.assertEqual(executable["guest_path"], "/usr/sbin/quagga")
            self.assertEqual(executable["debug_elf"], "build/packages/quagga.debug")
            self.assertEqual(
                document["kernel_bundle"]["inputs"]["kbuild_output"],
                {"box_id": 11, "path": "build/linux"},
            )
            self.assertEqual(
                document["kernel_bundle"]["recorded_tools"],
                {
                    "compiler": ["/sdk/bin/gcc"],
                    "cross-ld": ["/sdk/bin/ld"],
                    "make": ["/sdk/bin/make"],
                    "objcopy": ["/sdk/bin/objcopy"],
                },
            )
            self.assertEqual(
                [row["code"] for row in document["missing_requirements"]],
                ["catalog-backed-kbuild-executor"],
            )
            self.assertEqual(
                (output / "bundle-plan.json").read_text().endswith("\n"), True
            )

    def test_raw_newc_uses_the_same_catalog_bounded_plan(self):
        request, source, archive = request_and_source(compressed=False)
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "derived"
            plan = orchestrate_selected_initramfs(request, source, output).plan
            self.assertEqual(plan.selected_derivation["layout"], "cpio-newc")
            self.assertEqual(
                (output / "selected-image" / str(plan.initramfs["path"])).read_bytes(),
                archive,
            )

    def test_missing_kbuild_evidence_is_reported_without_path_guessing(self):
        request, source, _archive = request_and_source(include_config=False)
        with tempfile.TemporaryDirectory() as raw:
            result = orchestrate_selected_initramfs(
                request, source, Path(raw) / "derived"
            )
            missing = {
                row["code"]: row for row in result.plan.descriptor()["missing_requirements"]
            }
            self.assertIn("missing-kbuild-config", missing)
            self.assertEqual(
                missing["missing-kbuild-config"]["expected"],
                ["box 11:build/linux/.config"],
            )
            self.assertNotIn("path", result.plan.descriptor())

    def test_non_newc_selection_is_rejected_transactionally(self):
        selected = artifact(b"hsqsnot-newc", "out/root.squashfs")
        request = SelectedImageRequest(selected, ())
        source = MemorySource({(selected.box_id, selected.path): b"hsqsnot-newc"})
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "derived"
            with self.assertRaisesRegex(SelectedBundleError, "only cpio"):
                orchestrate_selected_initramfs(request, source, output)
            self.assertFalse(output.exists())

    def test_archive_without_executable_init_is_rejected(self):
        archive = newc([("etc/banner", stat.S_IFREG | 0o644, b"hello")])
        selected = artifact(archive, "out/initramfs.cpio")
        request = SelectedImageRequest(selected, ())
        source = MemorySource({(selected.box_id, selected.path): archive})
        with tempfile.TemporaryDirectory() as raw:
            with self.assertRaisesRegex(SelectedBundleError, "executable /init"):
                orchestrate_selected_initramfs(
                    request, source, Path(raw) / "derived"
                )

    def test_catalog_executor_emits_both_actual_bundle_directories(self):
        request, source, archive = request_and_source()
        observed = {}

        def fake_kernel_builder(arguments):
            observed["arguments"] = arguments
            arguments.output_dir.mkdir()
            (arguments.output_dir / "vmlinux").write_bytes(b"kernel symbols")
            (arguments.output_dir / "kernel").write_bytes(b"boot image")
            (arguments.output_dir / "bundle.json").write_text(
                '{"format":"viros-kernel-bundle-v1","architecture":"x86_64"}\n'
            )
            return arguments.output_dir

        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "complete"
            result = execute_selected_initramfs(
                execution_request(request), source, output,
                _kernel_builder=fake_kernel_builder,
            )
            self.assertTrue(result.plan.ready)
            self.assertTrue((output / "kernel-bundle/bundle.json").is_file())
            image = __import__("json").loads(
                (output / "image-bundle/image.json").read_text()
            )
            self.assertEqual(image["format"], "viros-image-bundle-v1")
            self.assertEqual(image["boot"]["init"], "/init")
            self.assertEqual((output / "image-bundle/rootfs.cpio").read_bytes(), archive)
            executable = image["userspace"]["executables"][0]
            self.assertEqual(executable["debug_elf"], "symbols/0123456789abcdef.elf")
            self.assertTrue((output / "image-bundle" / executable["debug_elf"]).is_file())
            self.assertFalse((output / "private-catalog").exists())
            arguments = observed["arguments"]
            self.assertEqual(arguments.kbuild_output.name, "linux")
            self.assertEqual(Path(arguments.compiler).name, "gcc")
            self.assertNotEqual(Path(arguments.compiler), Path("/sdk/bin/gcc"))

    def test_dynamic_tool_reports_missing_private_loader_closure(self):
        request, source, _archive = request_and_source()
        dynamic = bytearray(64 + 56)
        dynamic[:20] = b"\x7fELF\x02\x01\x01" + b"\0" * 9 + b"\x02\0\x3e\0"
        dynamic[32:40] = (64).to_bytes(8, "little")
        dynamic[54:56] = (56).to_bytes(2, "little")
        dynamic[56:58] = (1).to_bytes(2, "little")
        dynamic[64:68] = (3).to_bytes(4, "little")
        dynamic_bytes = bytes(dynamic)
        old = next(row for row in request.captured_artifacts if row.path == "sdk/bin/gcc")
        replacement = artifact(dynamic_bytes, old.path)
        catalog = tuple(
            replacement if row is old else row for row in request.captured_artifacts
        )
        changed = SelectedImageRequest(request.selected, catalog)
        source.rows[(replacement.box_id, replacement.path)] = dynamic_bytes
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "complete"
            with self.assertRaisesRegex(
                SelectedBundleError, "loader metadata is missing for compiler"
            ):
                execute_selected_initramfs(
                    execution_request(changed), source, output,
                    _kernel_builder=lambda _arguments: self.fail("must not build"),
                )
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
