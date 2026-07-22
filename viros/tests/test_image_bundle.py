from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest import mock


SOURCE = Path(__file__).parents[1] / "probe" / "image_bundle.py"
SPEC = importlib.util.spec_from_file_location("staged_image_bundle", SOURCE)
assert SPEC and SPEC.loader
image_bundle = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(image_bundle)


class ImageBundleTests(unittest.TestCase):
    @staticmethod
    def fake_elf(payload: bytes) -> bytes:
        return b"\x7fELF\x02\x01" + b"\0" * 10 + b"\x02\0\xb7\0" + payload

    @staticmethod
    def fake_load_elf(payload: bytes, debug_tail: bytes = b"") -> bytes:
        """Small AArch64 ET_EXEC with one exact executable PT_LOAD segment."""

        ident = b"\x7fELF\x02\x01\x01" + b"\0" * 9
        load_size = 64 + 56 + len(payload)
        # A debugger copy can point at section data after PT_LOAD.  The shared
        # identity normalizes these four section-table locator fields only.
        shoff = load_size if debug_tail else 0
        header = ident + struct.pack(
            "<HHIQQQIHHHHHH",
            2, 183, 1, 0x400000, 64, shoff, 0,
            64, 56, 1, 64 if debug_tail else 0, 1 if debug_tail else 0, 0,
        )
        program = struct.pack(
            "<IIQQQQQQ",
            1, 5, 0, 0x400000, 0x400000, load_size, load_size, 0x1000,
        )
        return header + program + payload + debug_tail

    @staticmethod
    def no_build_id_identity(path, architecture, *, require_debug):
        if architecture != "aarch64":
            raise image_bundle.ImageBundleError("wrong fixture architecture")
        if require_debug and not path.name.endswith(".debug"):
            raise image_bundle.ImageBundleError("DWARF is absent")
        contents = path.read_bytes()
        try:
            load = image_bundle.elf_load_identity(contents)
        except image_bundle.ElfLoadIdentityError as exc:
            raise image_bundle.ImageBundleError(str(exc)) from exc
        return {
            "build_id": None,
            "loadable_sha256": load.fingerprint,
            "sha256": hashlib.sha256(contents).hexdigest(),
            "size": len(contents),
            "elf_class": load.elf_class,
            "elf_machine": load.machine,
            "_load_identity": load,
            "_contents": contents,
        }

    @classmethod
    def mixed_build_id_identity(cls, runtime_build_id, debug_build_id):
        def identity(path, architecture, *, require_debug):
            value = cls.no_build_id_identity(
                path, architecture, require_debug=require_debug,
            )
            value["build_id"] = debug_build_id if require_debug else runtime_build_id
            return value
        return identity

    def fixture(self, root: Path):
        rootfs = root / "rootfs"
        (rootfs / "sbin").mkdir(parents=True)
        (rootfs / "usr/sbin").mkdir(parents=True)
        (rootfs / "sbin/init").write_bytes(b"init")
        (rootfs / "usr/sbin/quagga").write_bytes(b"runtime")
        kernel = root / "kernel"
        kernel.mkdir()
        (kernel / "kernel").write_bytes(b"kernel")
        (kernel / "bundle.json").write_text(json.dumps({
            "format": "viros-kernel-bundle-v1",
            "architecture": "aarch64",
        }))
        debug = root / "quagga.debug"
        debug.write_bytes(b"debug with DWARF")
        return rootfs, kernel, debug

    @staticmethod
    def arguments(rootfs: Path, kernel: Path, debug: Path, output: Path):
        return image_bundle.parser().parse_args([
            "--arch", "aarch64",
            "--rootfs", str(rootfs),
            "--kernel-bundle", str(kernel),
            "--output-dir", str(output),
            "--executable", f"/usr/sbin/quagga={debug}",
        ])

    @staticmethod
    def automatic_arguments(rootfs: Path, kernel: Path, output: Path):
        return image_bundle.parser().parse_args([
            "--arch", "aarch64",
            "--rootfs", str(rootfs),
            "--kernel-bundle", str(kernel),
            "--output-dir", str(output),
        ])

    @staticmethod
    def fake_identity(path, architecture, *, require_debug):
        if architecture != "aarch64" or path.read_bytes()[:4] != b"\x7fELF":
            raise image_bundle.ImageBundleError("not a matching ELF")
        name = path.name.removesuffix(".debug")
        identities = {
            "quagga": "0123456789abcdef",
            "zebra": "fedcba9876543210",
            "unmatched": "1111111122222222",
        }
        build_id = identities.get(name)
        if build_id is None:
            raise image_bundle.ImageBundleError("not a fixture ELF")
        if require_debug and not path.name.endswith(".debug"):
            raise image_bundle.ImageBundleError("DWARF is absent")
        value = path.read_bytes()
        return {
            "build_id": build_id,
            "sha256": hashlib.sha256(value).hexdigest(),
            "size": len(value),
            "elf_class": 64,
            "elf_machine": 183,
        }

    def test_manifest_relates_runtime_and_debug_elf_by_build_id(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, debug = self.fixture(root)
            output = root / "published"

            def identity(path, architecture, *, require_debug):
                self.assertEqual(architecture, "aarch64")
                value = path.read_bytes()
                return {
                    "build_id": "0123456789abcdef",
                    "sha256": hashlib.sha256(value).hexdigest(),
                    "size": len(value),
                    "elf_class": 64,
                    "elf_machine": 183,
                }

            with mock.patch.object(image_bundle, "elf_identity", side_effect=identity):
                image_bundle.build(self.arguments(rootfs, kernel, debug, output))

            document = json.loads((output / "image.json").read_text())
            self.assertEqual(document["format"], "viros-image-bundle-v1")
            self.assertEqual(document["boot"], {
                "profile": "virt-initramfs-aarch64-v1",
                "kernel_bundle": "kernel/bundle.json",
                "initramfs": "rootfs.cpio",
                "init": "/sbin/init",
            })
            executable = document["userspace"]["executables"][0]
            self.assertEqual(executable["guest_path"], "/usr/sbin/quagga")
            self.assertEqual(executable["build_id"], "0123456789abcdef")
            self.assertEqual(executable["source_view"], "provider-root")
            self.assertEqual(executable["debug_size"], len(b"debug with DWARF"))
            self.assertEqual(
                (output / executable["debug_elf"]).read_bytes(), b"debug with DWARF"
            )
            artifacts = {row["path"]: row for row in document["artifacts"]}
            self.assertIn("rootfs.cpio", artifacts)
            self.assertIn("kernel/bundle.json", artifacts)
            self.assertIn("symbols/0123456789abcdef.elf", artifacts)

    def test_runtime_and_debug_build_id_mismatch_is_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, debug = self.fixture(root)
            output = root / "published"

            def identity(path, _architecture, *, require_debug):
                return {
                    "build_id": "bbbbbbbb" if require_debug else "aaaaaaaa",
                    "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                    "size": path.stat().st_size,
                    "elf_class": 64,
                    "elf_machine": 183,
                }

            with mock.patch.object(image_bundle, "elf_identity", side_effect=identity):
                with self.assertRaisesRegex(image_bundle.ImageBundleError, "build IDs differ"):
                    image_bundle.build(self.arguments(rootfs, kernel, debug, output))
            self.assertFalse(output.exists())

    def test_catalog_is_discovered_from_executable_rootfs_elf_build_ids(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, _debug = self.fixture(root)
            runtime = rootfs / "usr/sbin/quagga"
            runtime.write_bytes(self.fake_elf(b"runtime"))
            runtime.chmod(0o755)
            debug = root / "build_dir/quagga.debug"
            debug.parent.mkdir()
            debug.write_bytes(self.fake_elf(b"debug with DWARF"))
            output = root / "published"

            with (
                mock.patch.object(image_bundle, "elf_identity", side_effect=self.fake_identity),
                mock.patch.object(image_bundle, "discovery_roots", return_value=[root]),
            ):
                image_bundle.build(self.automatic_arguments(rootfs, kernel, output))

            document = json.loads((output / "image.json").read_text())
            executable = document["userspace"]["executables"]
            self.assertEqual([row["guest_path"] for row in executable], ["/usr/sbin/quagga"])
            self.assertEqual(executable[0]["build_id"], "0123456789abcdef")
            self.assertEqual(
                (output / executable[0]["debug_elf"]).read_bytes(),
                self.fake_elf(b"debug with DWARF"),
            )

    def test_explicit_associations_and_discovered_associations_are_combined(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, quagga_debug = self.fixture(root)
            quagga_runtime = rootfs / "usr/sbin/quagga"
            quagga_runtime.write_bytes(self.fake_elf(b"quagga runtime"))
            quagga_debug.write_bytes(self.fake_elf(b"quagga debug"))
            (rootfs / "usr/sbin/zebra").write_bytes(self.fake_elf(b"zebra runtime"))
            (rootfs / "usr/sbin/zebra").chmod(0o755)
            zebra_debug = root / "build_dir/zebra.debug"
            zebra_debug.parent.mkdir()
            zebra_debug.write_bytes(self.fake_elf(b"zebra debug"))
            output = root / "published"

            with (
                mock.patch.object(image_bundle, "elf_identity", side_effect=self.fake_identity),
                mock.patch.object(image_bundle, "discovery_roots", return_value=[root]),
            ):
                image_bundle.build(self.arguments(rootfs, kernel, quagga_debug, output))

            document = json.loads((output / "image.json").read_text())
            self.assertEqual(
                [row["guest_path"] for row in document["userspace"]["executables"]],
                ["/usr/sbin/quagga", "/usr/sbin/zebra"],
            )

    def test_ambiguous_debug_contents_for_one_build_id_are_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, _debug = self.fixture(root)
            runtime = rootfs / "usr/sbin/quagga"
            runtime.write_bytes(self.fake_elf(b"runtime"))
            runtime.chmod(0o755)
            one = root / "one/quagga.debug"
            two = root / "two/quagga.debug"
            one.parent.mkdir()
            two.parent.mkdir()
            one.write_bytes(self.fake_elf(b"first DWARF"))
            two.write_bytes(self.fake_elf(b"second DWARF"))
            output = root / "published"

            with (
                mock.patch.object(image_bundle, "elf_identity", side_effect=self.fake_identity),
                mock.patch.object(image_bundle, "discovery_roots", return_value=[root]),
            ):
                with self.assertRaisesRegex(image_bundle.ImageBundleError, "ambiguous"):
                    image_bundle.build(self.automatic_arguments(rootfs, kernel, output))
            self.assertFalse(output.exists())

    def test_catalog_falls_back_to_exact_loadable_content_without_build_ids(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, _debug = self.fixture(root)
            runtime = rootfs / "usr/sbin/quagga"
            runtime.write_bytes(self.fake_load_elf(b"same mapped program"))
            runtime.chmod(0o755)
            debug = root / "build_dir/quagga.debug"
            debug.parent.mkdir()
            debug.write_bytes(self.fake_load_elf(
                b"same mapped program", b"unmapped DWARF sections",
            ))
            output = root / "published"

            with (
                mock.patch.object(
                    image_bundle, "elf_identity", side_effect=self.no_build_id_identity,
                ),
                mock.patch.object(image_bundle, "discovery_roots", return_value=[root]),
            ):
                image_bundle.build(self.automatic_arguments(rootfs, kernel, output))

            document = json.loads((output / "image.json").read_text())
            executable = document["userspace"]["executables"]
            self.assertEqual([row["guest_path"] for row in executable], ["/usr/sbin/quagga"])
            expected = image_bundle.elf_load_identity(runtime.read_bytes()).fingerprint
            self.assertEqual(executable[0]["build_id"], expected)
            self.assertEqual(len(expected), 64)
            self.assertEqual(
                (output / executable[0]["debug_elf"]).read_bytes(), debug.read_bytes(),
            )

    def test_loadable_fallback_keeps_dwarf_content_ambiguity_visible(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, _debug = self.fixture(root)
            runtime = rootfs / "usr/sbin/quagga"
            runtime.write_bytes(self.fake_load_elf(b"same mapped program"))
            runtime.chmod(0o755)
            one = root / "one/quagga.debug"
            two = root / "two/quagga.debug"
            one.parent.mkdir()
            two.parent.mkdir()
            one.write_bytes(self.fake_load_elf(b"same mapped program", b"DWARF one"))
            two.write_bytes(self.fake_load_elf(b"same mapped program", b"DWARF two"))
            output = root / "published"

            with (
                mock.patch.object(
                    image_bundle, "elf_identity", side_effect=self.no_build_id_identity,
                ),
                mock.patch.object(image_bundle, "discovery_roots", return_value=[root]),
            ):
                with self.assertRaisesRegex(
                    image_bundle.ImageBundleError,
                    "loadable identity .* ambiguous DWARF ELF contents",
                ):
                    image_bundle.build(self.automatic_arguments(rootfs, kernel, output))
            self.assertFalse(output.exists())

    def test_explicit_build_id_less_pair_requires_equal_loadable_content(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, debug = self.fixture(root)
            runtime = rootfs / "usr/sbin/quagga"
            runtime.write_bytes(self.fake_load_elf(b"runtime mapped bytes"))
            debug.write_bytes(self.fake_load_elf(
                b"different mapped bytes", b"DWARF",
            ))
            output = root / "published"

            with mock.patch.object(
                image_bundle, "elf_identity", side_effect=self.no_build_id_identity,
            ):
                with self.assertRaisesRegex(
                    image_bundle.ImageBundleError, "loadable contents differ",
                ):
                    image_bundle.build(self.arguments(rootfs, kernel, debug, output))
            self.assertFalse(output.exists())

    def test_automatic_fallback_uses_retained_debug_elf_identity_when_one_id_is_absent(self):
        cases = (
            ("runtime-id-only", "aa" * 16, None),
            ("debug-id-only", None, "bb" * 16),
        )
        for label, runtime_build_id, debug_build_id in cases:
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                root = Path(raw)
                rootfs, kernel, _debug = self.fixture(root)
                runtime = rootfs / "usr/sbin/quagga"
                runtime.write_bytes(self.fake_load_elf(b"same mapped program"))
                runtime.chmod(0o755)
                debug = root / "build_dir/quagga.debug"
                debug.parent.mkdir()
                debug.write_bytes(self.fake_load_elf(
                    b"same mapped program", b"unmapped DWARF sections",
                ))
                output = root / "published"

                with (
                    mock.patch.object(
                        image_bundle,
                        "elf_identity",
                        side_effect=self.mixed_build_id_identity(
                            runtime_build_id, debug_build_id,
                        ),
                    ),
                    mock.patch.object(image_bundle, "discovery_roots", return_value=[root]),
                ):
                    image_bundle.build(self.automatic_arguments(rootfs, kernel, output))

                row = json.loads((output / "image.json").read_text())["userspace"][
                    "executables"
                ][0]
                debug_fingerprint = image_bundle.elf_load_identity(
                    debug.read_bytes()
                ).fingerprint
                self.assertEqual(row["build_id"], debug_build_id or debug_fingerprint)

    def test_explicit_fallback_uses_retained_debug_elf_identity_when_one_id_is_absent(self):
        cases = (
            ("runtime-id-only", "cc" * 16, None),
            ("debug-id-only", None, "dd" * 16),
        )
        for label, runtime_build_id, debug_build_id in cases:
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                root = Path(raw)
                rootfs, kernel, debug = self.fixture(root)
                runtime = rootfs / "usr/sbin/quagga"
                runtime.write_bytes(self.fake_load_elf(b"same mapped program"))
                debug.write_bytes(self.fake_load_elf(
                    b"same mapped program", b"unmapped DWARF sections",
                ))
                output = root / "published"

                with mock.patch.object(
                    image_bundle,
                    "elf_identity",
                    side_effect=self.mixed_build_id_identity(
                        runtime_build_id, debug_build_id,
                    ),
                ):
                    image_bundle.build(self.arguments(rootfs, kernel, debug, output))

                row = json.loads((output / "image.json").read_text())["userspace"][
                    "executables"
                ][0]
                debug_fingerprint = image_bundle.elf_load_identity(
                    debug.read_bytes()
                ).fingerprint
                self.assertEqual(row["build_id"], debug_build_id or debug_fingerprint)

    def test_executable_without_matching_dwarf_elf_is_not_cataloged(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            rootfs, kernel, _debug = self.fixture(root)
            runtime = rootfs / "usr/sbin/unmatched"
            runtime.write_bytes(self.fake_elf(b"runtime"))
            runtime.chmod(0o755)
            output = root / "published"

            with (
                mock.patch.object(image_bundle, "elf_identity", side_effect=self.fake_identity),
                mock.patch.object(image_bundle, "discovery_roots", return_value=[root]),
            ):
                image_bundle.build(self.automatic_arguments(rootfs, kernel, output))

            document = json.loads((output / "image.json").read_text())
            self.assertEqual(document["userspace"]["executables"], [])

    def test_initramfs_is_deterministic_and_contains_symlinks(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            tree = root / "tree"
            (tree / "bin").mkdir(parents=True)
            (tree / "bin/tool").write_bytes(b"payload")
            (tree / "bin/link").symlink_to("tool")
            one, two = root / "one.cpio", root / "two.cpio"
            image_bundle.write_initramfs(tree, one)
            image_bundle.write_initramfs(tree, two)
            self.assertEqual(one.read_bytes(), two.read_bytes())
            self.assertIn(b"bin/tool\0", one.read_bytes())
            self.assertIn(b"bin/link\0", one.read_bytes())
            self.assertIn(b"TRAILER!!!\0", one.read_bytes())

    def test_guest_paths_cannot_leave_rootfs(self):
        for value in ("relative", "/usr/../host", "/usr\\host", "/bad\0path"):
            with self.subTest(value=value):
                with self.assertRaises(image_bundle.ImageBundleError):
                    image_bundle.safe_guest_path(value)

    def test_kernel_init_rejects_kernel_command_line_separators(self):
        self.assertEqual(image_bundle.safe_kernel_init("/sbin/init"), "/sbin/init")
        for value in ("/sbin/my init", "/sbin/init\tdebug", "/sbin/init\n"):
            with self.subTest(value=value):
                with self.assertRaises(image_bundle.ImageBundleError):
                    image_bundle.safe_kernel_init(value)


if __name__ == "__main__":
    unittest.main()
