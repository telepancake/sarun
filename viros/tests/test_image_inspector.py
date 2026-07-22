from __future__ import annotations

import hashlib
import io
import struct
import tarfile
import unittest
import zlib

from probe.image_inspector import (
    CapturedArtifact,
    ImageInspectionError,
    inspect_selected_image,
    inspect_selected_image_mapping,
    materialize_component,
)


def artifact(
    data: bytes,
    path: str,
    *,
    box_id: int = 7,
    record_id: str | None = None,
    roles: tuple[str, ...] = (),
    architecture: str | None = None,
) -> CapturedArtifact:
    return CapturedArtifact(
        box_id=box_id,
        path=path,
        size=len(data),
        sha256=hashlib.sha256(data).hexdigest(),
        record_id=record_id or f"record:{path}",
        roles=roles,
        architecture=architecture,
    )


def sysupgrade(kernel: bytes, rootfs: bytes) -> bytes:
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in (
            ("sysupgrade-test/CONTROL", b"BOARD=test\n"),
            ("sysupgrade-test/kernel", kernel),
            ("sysupgrade-test/root", rootfs),
        ):
            row = tarfile.TarInfo(name)
            row.size = len(contents)
            row.mtime = 0
            archive.addfile(row, io.BytesIO(contents))
    return output.getvalue()


def uimage(payload: bytes, *, arch: int = 22, image_type: int = 2) -> bytes:
    name = b"synthetic".ljust(32, b"\0")
    header = struct.pack(
        ">7I4B32s", 0x27051956, 0, 0, len(payload), 0x80000, 0x80000,
        zlib.crc32(payload) & 0xFFFFFFFF, 5, arch, image_type, 0, name,
    )
    checksum = zlib.crc32(header) & 0xFFFFFFFF
    return header[:4] + struct.pack(">I", checksum) + header[8:] + payload


def fit(kernel: bytes, ramdisk: bytes) -> bytes:
    """Create a small valid inline-data FIT without relying on dtc."""

    strings = bytearray()
    names: dict[str, int] = {}

    def name_offset(name: str) -> int:
        if name not in names:
            names[name] = len(strings)
            strings.extend(name.encode() + b"\0")
        return names[name]

    structure = bytearray()

    def token(value: int) -> None:
        structure.extend(struct.pack(">I", value))

    def begin(name: str) -> None:
        token(1)
        structure.extend(name.encode() + b"\0")
        structure.extend(b"\0" * (-len(structure) & 3))

    def end() -> None:
        token(2)

    def prop(name: str, value: bytes) -> None:
        token(3)
        structure.extend(struct.pack(">II", len(value), name_offset(name)))
        structure.extend(value)
        structure.extend(b"\0" * (-len(structure) & 3))

    begin("")
    begin("images")
    for node, image_type, contents in (
        ("kernel-1", "kernel", kernel),
        ("ramdisk-1", "ramdisk", ramdisk),
    ):
        begin(node)
        prop("type", image_type.encode() + b"\0")
        prop("arch", b"arm64\0")
        prop("compression", b"none\0")
        prop("data", contents)
        begin("hash-1")
        prop("algo", b"sha256\0")
        prop("value", hashlib.sha256(contents).digest())
        end()
        end()
    end()
    begin("configurations")
    prop("default", b"config-1\0")
    begin("config-1")
    prop("kernel", b"kernel-1\0")
    prop("ramdisk", b"ramdisk-1\0")
    end()
    end()
    end()
    token(9)

    reserve = b"\0" * 16
    off_reserve = 40
    off_structure = off_reserve + len(reserve)
    off_strings = off_structure + len(structure)
    total = off_strings + len(strings)
    header = struct.pack(
        ">10I", 0xD00DFEED, total, off_structure, off_strings, off_reserve,
        17, 16, 0, len(strings), len(structure),
    )
    return header + reserve + structure + strings


class SelectedImageInspectorTests(unittest.TestCase):
    def test_sysupgrade_constituents_match_captured_build_records(self):
        kernel = b"KERNEL FROM BUILD"
        rootfs = b"hsqs" + b"ROOTFS FROM BUILD"
        image = sysupgrade(kernel, rootfs)
        selected = artifact(image, "bin/targets/test/sysupgrade.tar")
        records = (
            artifact(kernel, "build/kernel", roles=("kernel-boot",), architecture="aarch64"),
            artifact(rootfs, "build/root.squashfs", roles=("rootfs",), architecture="aarch64"),
        )

        first = inspect_selected_image(image, selected, records)
        second = inspect_selected_image(image, selected, tuple(reversed(records)))

        self.assertEqual(first, second)
        self.assertEqual(first["format"], "viros-selected-image-derivation-v1")
        self.assertEqual(first["layout"], "openwrt-sysupgrade-tar")
        self.assertEqual(first["architecture"], "aarch64")
        self.assertEqual(first["profile"], "virt-initramfs-aarch64-v1")
        self.assertTrue(first["compatibility"]["boot_input_complete"])
        rows = {row["role"]: row for row in first["components"]}
        self.assertEqual(rows["kernel"]["identity"]["sha256"], hashlib.sha256(kernel).hexdigest())
        self.assertEqual(rows["kernel"]["captured_matches"][0]["path"], "build/kernel")
        self.assertEqual(rows["rootfs"]["captured_matches"][0]["path"], "build/root.squashfs")
        self.assertEqual(rows["rootfs"]["locator"]["kind"], "container-member")
        self.assertEqual(materialize_component(image, rows["kernel"]), kernel)
        self.assertEqual(materialize_component(image, rows["rootfs"]), rootfs)

    def test_compressed_sysupgrade_member_is_materialized_without_host_tar(self):
        kernel = b"compressed tar kernel"
        rootfs = b"hsqscompressed tar root"
        plain = sysupgrade(kernel, rootfs)
        compressor = zlib.compressobj(wbits=16 + zlib.MAX_WBITS)
        image = compressor.compress(plain) + compressor.flush()
        result = inspect_selected_image(image, artifact(image, "bin/sysupgrade.bin"))
        rows = {row["role"]: row for row in result["components"]}
        self.assertNotIn("offset", rows["kernel"]["locator"])
        self.assertEqual(materialize_component(image, rows["kernel"]), kernel)
        self.assertEqual(materialize_component(image, rows["rootfs"]), rootfs)

    def test_non_sysupgrade_tar_is_not_classified_from_a_kernel_filename(self):
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w") as archive:
            row = tarfile.TarInfo("kernel")
            row.size = 1
            archive.addfile(row, io.BytesIO(b"x"))
        image = output.getvalue()
        result = inspect_selected_image(image, artifact(image, "out/archive.tar"))
        self.assertEqual(result["layout"], "opaque")

    def test_uimage_crc_and_header_give_exact_kernel_range(self):
        payload = b"arm64 kernel bytes"
        image = uimage(payload)
        result = inspect_selected_image(
            image,
            artifact(image, "bin/kernel.uImage"),
            (artifact(payload, "build/Image", roles=("kernel-boot",), architecture="aarch64"),),
        )
        self.assertEqual(result["layout"], "uimage")
        self.assertEqual(result["architecture"], "aarch64")
        component = result["components"][0]
        self.assertEqual(component["role"], "kernel")
        self.assertEqual(component["locator"], {"kind": "selected-range", "offset": 64, "length": len(payload)})
        self.assertEqual(component["captured_matches"][0]["path"], "build/Image")

    def test_uimage_crc_mismatch_is_rejected(self):
        image = bytearray(uimage(b"kernel"))
        image[-1] ^= 1
        with self.assertRaisesRegex(ImageInspectionError, "payload CRC"):
            inspect_selected_image(bytes(image), artifact(bytes(image), "out/bad.uImage"))

    def test_openwrt_combined_uimage_maps_aligned_squashfs(self):
        kernel = b"kernel"
        rootfs = b"hsqs" + b"rootfs"
        image = uimage(kernel) + b"\xff" * 8 + rootfs
        result = inspect_selected_image(
            image,
            artifact(image, "bin/combined.bin"),
            (
                artifact(kernel, "build/Image", roles=("kernel-boot",), architecture="aarch64"),
                artifact(rootfs, "build/root.squashfs", roles=("rootfs",), architecture="aarch64"),
            ),
        )
        self.assertEqual([row["role"] for row in result["components"]], ["kernel", "rootfs"])
        self.assertEqual(result["components"][1]["captured_matches"][0]["path"], "build/root.squashfs")
        self.assertTrue(result["compatibility"]["boot_input_complete"])

    def test_fit_default_configuration_yields_kernel_and_initramfs(self):
        kernel = b"FIT-KERNEL"
        initramfs = b"070701" + b"FIT-INITRAMFS"
        image = fit(kernel, initramfs)
        result = inspect_selected_image(
            image,
            artifact(image, "bin/firmware.itb"),
            (
                artifact(kernel, "build/Image", roles=("kernel-boot",), architecture="aarch64"),
                artifact(initramfs, "build/rootfs.cpio", roles=("initramfs",), architecture="aarch64"),
            ),
        )
        self.assertEqual(result["layout"], "fit")
        self.assertEqual(result["configuration"], "config-1")
        self.assertEqual(result["architecture"], "aarch64")
        self.assertEqual([row["role"] for row in result["components"]], ["kernel", "initramfs"])
        self.assertTrue(result["compatibility"]["boot_input_complete"])

    def test_fit_hash_mismatch_is_rejected(self):
        image = bytearray(fit(b"UNIQUE-KERNEL-CONTENT", b"ramdisk"))
        position = image.find(b"UNIQUE-KERNEL-CONTENT")
        self.assertGreater(position, 0)
        image[position] ^= 1
        selected = artifact(bytes(image), "bin/bad.itb")
        with self.assertRaisesRegex(ImageInspectionError, "sha256 does not match"):
            inspect_selected_image(bytes(image), selected)

    def test_raw_and_gzip_formats_are_recognized_from_bytes(self):
        cpio = b"070701" + b"synthetic-newc"
        compressor = zlib.compressobj(wbits=16 + zlib.MAX_WBITS)
        compressed = compressor.compress(cpio) + compressor.flush()
        result = inspect_selected_image(compressed, artifact(compressed, "out/rootfs.bin"))
        self.assertEqual(result["layout"], "gzip-cpio-newc")
        self.assertEqual(result["components"][0]["role"], "initramfs")
        self.assertEqual(result["components"][0]["identity"]["sha256"], hashlib.sha256(cpio).hexdigest())
        self.assertEqual(result["components"][0]["locator"]["codec"], "gzip")
        self.assertEqual(materialize_component(compressed, result["components"][0]), cpio)

    def test_raw_elf_architecture_does_not_depend_on_filename(self):
        elf = b"\x7fELF\x02\x01" + b"\0" * 12 + struct.pack("<H", 183) + b"kernel"
        result = inspect_selected_image(elf, artifact(elf, "output/blob"))
        self.assertEqual((result["layout"], result["architecture"]), ("elf-kernel", "aarch64"))

    def test_selected_identity_and_mapping_schema_are_strict(self):
        image = b"hsqsrootfs"
        selected = artifact(image, "out/rootfs")
        wrong = CapturedArtifact(
            box_id=selected.box_id, path=selected.path, size=selected.size,
            sha256="0" * 64, record_id=selected.record_id,
        )
        with self.assertRaisesRegex(ImageInspectionError, "does not match"):
            inspect_selected_image(image, wrong)
        mapping = selected.descriptor()
        mapping["surprise"] = True
        with self.assertRaisesRegex(ImageInspectionError, "unknown captured"):
            inspect_selected_image_mapping(image, mapping)

    def test_catalog_roles_can_type_an_otherwise_opaque_raw_image(self):
        image = b"vendor disk representation"
        selected = artifact(image, "bin/selected.img", roles=("disk",), architecture="mmips")
        result = inspect_selected_image(image, selected)
        self.assertEqual(result["layout"], "captured-opaque")
        self.assertEqual(result["components"][0]["role"], "disk")
        self.assertEqual(result["architecture"], "mmips")
        self.assertEqual(result["profile"], "malta-initramfs-mipsel-v1")
        self.assertTrue(result["compatibility"]["boot_input_complete"])


if __name__ == "__main__":
    unittest.main()
