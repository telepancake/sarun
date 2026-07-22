from __future__ import annotations

import hashlib
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

from probe.image_inspector import CapturedArtifact
from probe.provider_derivation import (
    FilesystemArtifactSource,
    PAIR_REQUEST_FORMAT,
    PAIR_RESULT_FORMAT,
    ProviderDerivationError,
    REQUEST_FORMAT,
    RESULT_FORMAT,
    SelectedImageRequest,
    SelectedKernelInitramfsRequest,
    derive_selected_image,
    derive_selected_image_mapping,
    derive_selected_kernel_initramfs,
    derive_selected_kernel_initramfs_mapping,
)
from tests.test_newc_userspace import newc


def artifact(
    data: bytes,
    path: str,
    *,
    box_id: int = 17,
    roles: tuple[str, ...] = (),
    architecture: str | None = None,
) -> CapturedArtifact:
    return CapturedArtifact(
        box_id=box_id,
        path=path,
        size=len(data),
        sha256=hashlib.sha256(data).hexdigest(),
        record_id=f"write:{box_id}:{path}",
        roles=roles,
        architecture=architecture,
    )


def sysupgrade(kernel: bytes, rootfs: bytes) -> bytes:
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in (
            ("sysupgrade-board/CONTROL", b"BOARD=board\n"),
            ("sysupgrade-board/kernel", kernel),
            ("sysupgrade-board/root", rootfs),
        ):
            row = tarfile.TarInfo(name)
            row.size = len(contents)
            row.mtime = 0
            archive.addfile(row, io.BytesIO(contents))
    return output.getvalue()


class ProviderDerivationTests(unittest.TestCase):
    def write_artifact(self, root: Path, relative: str, contents: bytes) -> None:
        path = root.joinpath(*relative.split("/"))
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)

    def test_exact_catalog_is_verified_and_components_are_materialized(self):
        kernel = b"kernel from captured build"
        rootfs = b"hsqsrootfs from captured build"
        image = sysupgrade(kernel, rootfs)
        selected = artifact(image, "bin/targets/board/sysupgrade.tar")
        kernel_row = artifact(
            kernel, "build/Image", roles=("kernel-boot",), architecture="aarch64"
        )
        rootfs_row = artifact(
            rootfs, "build/root.squashfs", roles=("rootfs",), architecture="aarch64"
        )
        request = SelectedImageRequest(selected, (kernel_row, rootfs_row))

        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            attached = base / "box-17"
            attached.mkdir()
            self.write_artifact(attached, selected.path, image)
            self.write_artifact(attached, kernel_row.path, kernel)
            self.write_artifact(attached, rootfs_row.path, rootfs)
            output = base / "provider-work" / "image"
            output.parent.mkdir()

            result = derive_selected_image(
                request, FilesystemArtifactSource({17: attached}), output
            )
            document = result.descriptor()

            self.assertEqual(document["format"], RESULT_FORMAT)
            self.assertEqual(document["request_format"], REQUEST_FORMAT)
            self.assertEqual(document["derivation"]["architecture"], "aarch64")
            self.assertEqual(
                [row["role"] for row in document["materialized_components"]],
                ["kernel", "rootfs"],
            )
            materialized = {
                row["role"]: (output / row["path"]).read_bytes()
                for row in document["materialized_components"]
            }
            self.assertEqual(materialized, {"kernel": kernel, "rootfs": rootfs})

    def test_every_catalog_identity_is_checked_before_output_is_created(self):
        image = b"hsqsselected root"
        selected = artifact(image, "out/image.bin", roles=("rootfs",), architecture="arm")
        other = artifact(b"recorded bytes", "out/vmlinux", roles=("vmlinux",))
        request = SelectedImageRequest(selected, (other,))

        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            attached = base / "box"
            attached.mkdir()
            self.write_artifact(attached, selected.path, image)
            self.write_artifact(attached, other.path, b"changed bytes!")
            output = base / "result"
            with self.assertRaisesRegex(ProviderDerivationError, "identity mismatch"):
                derive_selected_image(
                    request, FilesystemArtifactSource({17: attached}), output
                )
            self.assertFalse(output.exists())

    def test_mapping_schema_rejects_unknown_fields_and_unattached_boxes(self):
        image = b"hsqsroot"
        selected = artifact(image, "out/image.bin", roles=("rootfs",))
        mapping = SelectedImageRequest(selected, ()).descriptor()
        mapping["kernel"] = "manual-selector"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(ProviderDerivationError, "unknown provider"):
                derive_selected_image_mapping(
                    mapping, FilesystemArtifactSource({17: root}), root / "result"
                )

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(ProviderDerivationError, "not attached"):
                derive_selected_image(
                    SelectedImageRequest(selected, ()),
                    FilesystemArtifactSource({}),
                    root / "result",
                )

    def test_paths_cannot_leave_explicit_attachment_root(self):
        image = b"hsqsroot"
        selected = artifact(image, "links/image.bin", roles=("rootfs",))
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            attached = base / "attached"
            outside = base / "outside"
            attached.mkdir()
            outside.mkdir()
            (outside / "image.bin").write_bytes(image)
            (attached / "links").symlink_to(outside, target_is_directory=True)
            source = FilesystemArtifactSource({17: attached})
            with self.assertRaisesRegex(ProviderDerivationError, "unavailable"):
                derive_selected_image(
                    SelectedImageRequest(selected, ()), source, base / "result"
                )

    def test_selected_identity_conflict_is_rejected_at_request_boundary(self):
        image = b"selected"
        selected = artifact(image, "out/image")
        conflicting = CapturedArtifact(
            box_id=selected.box_id,
            path=selected.path,
            size=selected.size,
            sha256="0" * 64,
            record_id="different-edge",
        )
        with self.assertRaisesRegex(ProviderDerivationError, "conflicting identities"):
            SelectedImageRequest(selected, (conflicting,))

    def test_selected_kernel_initramfs_pair_is_exact_and_preserves_gzip(self):
        import gzip
        import stat

        kernel = b"exact selected kernel"
        archive = newc([("init", stat.S_IFREG | 0o755, b"#!/bin/sh\n")])
        initramfs = gzip.compress(archive, mtime=0)
        kernel_row = artifact(kernel, "out/bzImage", architecture="x86_64")
        initramfs_row = artifact(initramfs, "out/rootfs.cpio.gz")
        support = artifact(b"support", "build/linux/.config")
        request = SelectedKernelInitramfsRequest(
            kernel_row, initramfs_row, (support,)
        )

        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            attached = base / "box"
            attached.mkdir()
            self.write_artifact(attached, kernel_row.path, kernel)
            self.write_artifact(attached, initramfs_row.path, initramfs)
            self.write_artifact(attached, support.path, b"support")
            output = base / "result"
            result = derive_selected_kernel_initramfs(
                request, FilesystemArtifactSource({17: attached}), output
            )

            self.assertEqual(result.descriptor()["format"], PAIR_RESULT_FORMAT)
            self.assertEqual(
                result.descriptor()["request_format"], PAIR_REQUEST_FORMAT
            )
            self.assertEqual((output / "kernel").read_bytes(), kernel)
            self.assertEqual((output / "initramfs").read_bytes(), initramfs)
            self.assertEqual(
                result.derivation["initramfs_layout"], "gzip-cpio-newc"
            )
            mapped = request.descriptor()
            second = base / "mapped"
            document = derive_selected_kernel_initramfs_mapping(
                mapped, FilesystemArtifactSource({17: attached}), second
            )
            self.assertEqual(document["format"], PAIR_RESULT_FORMAT)

    def test_pair_checks_both_selected_identities_before_creating_output(self):
        import stat

        archive = newc([("init", stat.S_IFREG | 0o755, b"ok")])
        kernel = artifact(b"kernel", "out/kernel")
        initramfs = artifact(archive, "out/initramfs")
        request = SelectedKernelInitramfsRequest(kernel, initramfs, ())
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            attached = base / "box"
            attached.mkdir()
            self.write_artifact(attached, kernel.path, b"changed")
            self.write_artifact(attached, initramfs.path, archive)
            output = base / "result"
            with self.assertRaisesRegex(
                ProviderDerivationError, "changed size|identity mismatch"
            ):
                derive_selected_kernel_initramfs(
                    request, FilesystemArtifactSource({17: attached}), output
                )
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
