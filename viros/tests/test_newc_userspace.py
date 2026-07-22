from __future__ import annotations

import gzip
import hashlib
import io
import stat
import struct
import unittest

from probe.image_bundle import _newc_header
from probe.elf_load_identity import elf_load_identity
from probe.image_inspector import CapturedArtifact
from probe.newc_userspace import (
    DebugElfCandidate,
    NewcCatalogError,
    catalog_newc_userspace,
    select_kernel_init,
)


def align4(value: bytes) -> bytes:
    return value + b"\0" * (-len(value) & 3)


def elf64(
    build_id: bytes,
    *,
    machine: int = 62,
    elf_type: int = 3,
    dwarf: bool = False,
    marker: bytes = b"",
) -> bytes:
    names = b"\0.note.gnu.build-id\0.shstrtab\0"
    if dwarf:
        names += b".debug_info\0.debug_line\0"
    name_offsets = {
        name.decode(): names.index(name)
        for name in (
            b".note.gnu.build-id", b".shstrtab", b".debug_info", b".debug_line"
        )
        if name in names
    }
    note = align4(struct.pack("<III", 4, len(build_id), 3) + b"GNU\0")
    note += align4(build_id)
    header_size = 64
    note_at = header_size
    names_at = note_at + len(note)
    debug_info = b"INFO" + marker if dwarf else b""
    info_at = names_at + len(names)
    debug_line = b"LINE" if dwarf else b""
    line_at = info_at + len(debug_info)
    shoff = (line_at + len(debug_line) + 7) & ~7
    rows = [(0, 0, 0, 0, 0, 0, 0, 0, 0, 0)]
    rows.append((name_offsets[".note.gnu.build-id"], 7, 0, 0, note_at, len(note), 0, 0, 4, 0))
    rows.append((name_offsets[".shstrtab"], 3, 0, 0, names_at, len(names), 0, 0, 1, 0))
    if dwarf:
        rows.append((name_offsets[".debug_info"], 1, 0, 0, info_at, len(debug_info), 0, 0, 1, 0))
        rows.append((name_offsets[".debug_line"], 1, 0, 0, line_at, len(debug_line), 0, 0, 1, 0))
    ident = b"\x7fELF\x02\x01\x01" + b"\0" * 9
    header = ident + struct.pack(
        "<HHIQQQIHHHHHH",
        elf_type, machine, 1, 0, 0, shoff, 0, header_size, 0, 0, 64,
        len(rows), 2,
    )
    body = header + note + names + debug_info + debug_line
    body += b"\0" * (shoff - len(body))
    return body + b"".join(struct.pack("<IIQQQQIIQQ", *row) for row in rows)


def elf32(build_id: bytes, *, machine: int = 8, dwarf: bool = False) -> bytes:
    names = b"\0.note.gnu.build-id\0.shstrtab\0"
    if dwarf:
        names += b".debug_info\0.debug_line\0"
    offsets = {
        name.decode(): names.index(name)
        for name in (
            b".note.gnu.build-id", b".shstrtab", b".debug_info", b".debug_line"
        )
        if name in names
    }
    note = align4(struct.pack("<III", 4, len(build_id), 3) + b"GNU\0")
    note += align4(build_id)
    note_at = 52
    names_at = note_at + len(note)
    info = b"INFO" if dwarf else b""
    info_at = names_at + len(names)
    line = b"LINE" if dwarf else b""
    line_at = info_at + len(info)
    shoff = (line_at + len(line) + 3) & ~3
    rows = [(0,) * 10]
    rows.append((offsets[".note.gnu.build-id"], 7, 0, 0, note_at, len(note), 0, 0, 4, 0))
    rows.append((offsets[".shstrtab"], 3, 0, 0, names_at, len(names), 0, 0, 1, 0))
    if dwarf:
        rows.append((offsets[".debug_info"], 1, 0, 0, info_at, len(info), 0, 0, 1, 0))
        rows.append((offsets[".debug_line"], 1, 0, 0, line_at, len(line), 0, 0, 1, 0))
    ident = b"\x7fELF\x01\x01\x01" + b"\0" * 9
    header = ident + struct.pack(
        "<HHIIIIIHHHHHH",
        3, machine, 1, 0, 0, shoff, 0, 52, 0, 0, 40, len(rows), 2,
    )
    body = header + note + names + info + line
    body += b"\0" * (shoff - len(body))
    return body + b"".join(struct.pack("<IIIIIIIIII", *row) for row in rows)


def load_elf64(
    *,
    elf_type: int = 2,
    dwarf: bool = False,
    stripped_sections: bool = False,
    load_marker: bytes = b"linked-load-image",
    dwarf_marker: bytes = b"",
    build_id: bytes | None = None,
) -> bytes:
    """Linked ELF whose debug/section tables are outside its PT_LOAD span."""

    load_size = 0x200
    header_size = 64
    phoff = header_size
    phentsize = 56
    names = b"\0.shstrtab\0.debug_info\0.debug_line\0"
    if build_id is not None:
        names += b".note.gnu.build-id\0"
    offsets = {
        name.decode(): names.index(name)
        for name in (b".shstrtab", b".debug_info", b".debug_line", b".note.gnu.build-id")
        if name in names
    }
    names_at = load_size
    info = b"INFO" + dwarf_marker if dwarf else b""
    info_at = names_at + len(names)
    line = b"LINE" if dwarf else b""
    line_at = info_at + len(info)
    note = b""
    if build_id is not None:
        note = align4(struct.pack("<III", 4, len(build_id), 3) + b"GNU\0")
        note += align4(build_id)
    note_at = line_at + len(line)
    shoff = (note_at + len(note) + 7) & ~7
    rows = [(0,) * 10]
    rows.append((offsets[".shstrtab"], 3, 0, 0, names_at, len(names), 0, 0, 1, 0))
    if dwarf:
        rows.append((offsets[".debug_info"], 1, 0, 0, info_at, len(info), 0, 0, 1, 0))
        rows.append((offsets[".debug_line"], 1, 0, 0, line_at, len(line), 0, 0, 1, 0))
    if build_id is not None:
        rows.append(
            (offsets[".note.gnu.build-id"], 7, 0, 0, note_at, len(note), 0, 0, 4, 0)
        )
    ident = b"\x7fELF\x02\x01\x01" + b"\0" * 9
    if stripped_sections:
        encoded_shoff, shentsize, shnum, shstrndx = 0, 0, 0, 0
    else:
        encoded_shoff, shentsize, shnum, shstrndx = shoff, 64, len(rows), 1
    header = ident + struct.pack(
        "<HHIQQQIHHHHHH",
        elf_type,
        62,
        1,
        0x400100,
        phoff,
        encoded_shoff,
        0,
        header_size,
        phentsize,
        1,
        shentsize,
        shnum,
        shstrndx,
    )
    load = bytearray(load_size)
    load[: len(header)] = header
    load[phoff : phoff + phentsize] = struct.pack(
        "<IIQQQQQQ",
        1,
        5,
        0,
        0x400000,
        0x400000,
        load_size,
        load_size + 0x40,
        0x1000,
    )
    load[0x100 : 0x100 + len(load_marker)] = load_marker
    if stripped_sections:
        return bytes(load)
    body = bytes(load) + names + info + line + note
    body += b"\0" * (shoff - len(body))
    return body + b"".join(struct.pack("<IIQQQQIIQQ", *row) for row in rows)


def newc(members: list[tuple[str, int, bytes]], *, crc: bool = False) -> bytes:
    output = io.BytesIO()
    inode = 1
    for name, mode, contents in members:
        encoded = name.encode() + b"\0"
        header = _newc_header(
            inode=inode, mode=mode, uid=0, gid=0, nlink=1, mtime=0,
            size=len(contents), dev_major=0, dev_minor=0, rdev_major=0,
            rdev_minor=0, name_size=len(encoded),
        )
        if crc:
            header = b"070702" + header[6:-8] + f"{sum(contents) & 0xffffffff:08x}".encode()
        output.write(header)
        output.write(encoded)
        output.write(b"\0" * (-(110 + len(encoded)) & 3))
        output.write(contents)
        output.write(b"\0" * (-len(contents) & 3))
        inode += 1
    trailer = b"TRAILER!!!\0"
    output.write(_newc_header(
        inode=inode, mode=0, uid=0, gid=0, nlink=1, mtime=0, size=0,
        dev_major=0, dev_minor=0, rdev_major=0, rdev_minor=0,
        name_size=len(trailer),
    ))
    output.write(trailer)
    output.write(b"\0" * (-(110 + len(trailer)) & 3))
    return output.getvalue()


def candidate(
    data: bytes, path: str, *, box: int = 7, architecture: str | None = None,
) -> DebugElfCandidate:
    return DebugElfCandidate(
        CapturedArtifact(
            box_id=box,
            path=path,
            size=len(data),
            sha256=hashlib.sha256(data).hexdigest(),
            record_id=f"link:{path}",
            architecture=architecture,
        ),
        data,
    )


class NewcUserspaceCatalogTests(unittest.TestCase):
    def test_kernel_init_selection_is_fixed_and_requires_executable_file(self):
        archive = newc([
            ("sbin/init", stat.S_IFREG | 0o755, b"fallback"),
            ("init", stat.S_IFREG | 0o755, b"preferred"),
        ])
        self.assertEqual(select_kernel_init(archive), "/init")
        with self.assertRaisesRegex(NewcCatalogError, "executable /init"):
            select_kernel_init(newc([
                ("init", stat.S_IFREG | 0o644, b"not executable"),
            ]))

    def test_gzip_catalog_matches_executable_and_shared_object_by_build_id(self):
        quagga_id = bytes.fromhex("0123456789abcdef")
        library_id = bytes.fromhex("deadbeef01020304")
        init_id = bytes.fromhex("1111111122222222")
        quagga = elf64(quagga_id)
        library = elf64(library_id)
        init = elf64(init_id)
        archive = newc([
            ("./usr/sbin/quagga", stat.S_IFREG | 0o755, quagga),
            ("lib/libroute.so.1", stat.S_IFREG | 0o644, library),
            ("init", stat.S_IFREG | 0o755, init),
            ("etc/banner", stat.S_IFREG | 0o644, b"OpenWrt"),
        ])
        result = catalog_newc_userspace(
            gzip.compress(archive, mtime=0),
            (
                candidate(elf64(library_id, dwarf=True), "debug/libroute.so.1"),
                candidate(elf64(quagga_id, dwarf=True), "debug/quagga"),
            ),
        )

        self.assertEqual(result.archive_format, "gzip-newc")
        self.assertEqual([row["guest_path"] for row in result.executables], [
            "/lib/libroute.so.1", "/usr/sbin/quagga",
        ])
        self.assertEqual(result.unmatched, ("/init",))
        row = result.executables[1]
        self.assertEqual(row["build_id"], quagga_id.hex())
        self.assertEqual(row["runtime_sha256"], hashlib.sha256(quagga).hexdigest())
        self.assertEqual(row["debug_elf"], "debug/quagga")
        self.assertEqual((row["elf_class"], row["elf_machine"]), (64, 62))
        self.assertEqual(row["source_view"], "provider-root")

    def test_architecture_identity_must_match_even_when_build_id_matches(self):
        build_id = bytes.fromhex("0123456789abcdef")
        runtime = elf64(build_id, machine=183)
        wrong_arch = elf64(build_id, machine=62, dwarf=True)
        result = catalog_newc_userspace(
            newc([("sbin/init", stat.S_IFREG | 0o755, runtime)]),
            (candidate(wrong_arch, "debug/wrong"),),
        )
        self.assertEqual(result.executables, ())
        self.assertEqual(result.unmatched, ("/sbin/init",))

    def test_32_bit_mmips_runtime_and_debug_identity(self):
        build_id = bytes.fromhex("cafebabe01020304")
        runtime = elf32(build_id)
        debug = elf32(build_id, dwarf=True)
        result = catalog_newc_userspace(
            newc([("sbin/daemon", stat.S_IFREG | 0o755, runtime)]),
            (candidate(debug, "debug/daemon"),),
        )
        self.assertEqual(len(result.executables), 1)
        self.assertEqual(
            (result.executables[0]["elf_class"], result.executables[0]["elf_machine"]),
            (32, 8),
        )

    def test_same_build_id_with_different_dwarf_contents_is_ambiguous(self):
        build_id = bytes.fromhex("0123456789abcdef")
        runtime = elf64(build_id)
        candidates = (
            candidate(elf64(build_id, dwarf=True, marker=b"A"), "debug/one"),
            candidate(elf64(build_id, dwarf=True, marker=b"B"), "debug/two"),
        )
        with self.assertRaisesRegex(NewcCatalogError, "ambiguous DWARF"):
            catalog_newc_userspace(
                newc([("bin/app", stat.S_IFREG | 0o755, runtime)]), candidates
            )

    def test_identical_candidate_copies_choose_deterministically(self):
        build_id = bytes.fromhex("0123456789abcdef")
        runtime = elf64(build_id)
        debug = elf64(build_id, dwarf=True)
        result = catalog_newc_userspace(
            newc([("bin/app", stat.S_IFREG | 0o755, runtime)]),
            (
                candidate(debug, "z/debug", box=8),
                candidate(debug, "a/debug", box=7),
            ),
        )
        self.assertEqual(result.executables[0]["debug_elf"], "a/debug")

    def test_build_id_less_stripped_runtime_matches_exact_load_image(self):
        runtime = load_elf64(stripped_sections=True)
        debug = load_elf64(dwarf=True)
        result = catalog_newc_userspace(
            newc([("usr/sbin/quagga", stat.S_IFREG | 0o755, runtime)]),
            (candidate(debug, "build/quagga"),),
        )
        self.assertEqual(result.unmatched, ())
        self.assertEqual(len(result.executables), 1)
        row = result.executables[0]
        self.assertEqual(row["guest_path"], "/usr/sbin/quagga")
        self.assertEqual(row["debug_elf"], "build/quagga")
        self.assertEqual(row["build_id"], elf_load_identity(runtime).fingerprint)

    def test_build_id_less_load_content_mismatch_does_not_associate(self):
        runtime = load_elf64(stripped_sections=True, load_marker=b"runtime")
        debug = load_elf64(dwarf=True, load_marker=b"different")
        result = catalog_newc_userspace(
            newc([("bin/app", stat.S_IFREG | 0o755, runtime)]),
            (candidate(debug, "build/app"),),
        )
        self.assertEqual(result.executables, ())
        self.assertEqual(result.unmatched, ("/bin/app",))

    def test_build_id_less_different_dwarf_files_are_ambiguous(self):
        runtime = load_elf64(stripped_sections=True)
        first = load_elf64(dwarf=True, dwarf_marker=b"A")
        second = load_elf64(dwarf=True, dwarf_marker=b"B")
        with self.assertRaisesRegex(NewcCatalogError, "PT_LOAD identity.*ambiguous"):
            catalog_newc_userspace(
                newc([("bin/app", stat.S_IFREG | 0o755, runtime)]),
                (
                    candidate(first, "build/one"),
                    candidate(second, "build/two"),
                ),
            )

    def test_build_id_less_identical_copies_choose_deterministically(self):
        runtime = load_elf64(stripped_sections=True)
        debug = load_elf64(dwarf=True)
        result = catalog_newc_userspace(
            newc([("bin/app", stat.S_IFREG | 0o755, runtime)]),
            (
                candidate(debug, "z/app", box=9),
                candidate(debug, "a/app", box=8),
            ),
        )
        self.assertEqual(result.executables[0]["debug_elf"], "a/app")

    def test_build_id_less_pie_requires_same_elf_type(self):
        runtime = load_elf64(elf_type=3, stripped_sections=True)
        pie_debug = load_elf64(elf_type=3, dwarf=True)
        result = catalog_newc_userspace(
            newc([("bin/pie", stat.S_IFREG | 0o755, runtime)]),
            (candidate(pie_debug, "build/pie"),),
        )
        self.assertEqual(len(result.executables), 1)

        wrong_type = load_elf64(elf_type=2, dwarf=True)
        result = catalog_newc_userspace(
            newc([("bin/pie", stat.S_IFREG | 0o755, runtime)]),
            (candidate(wrong_type, "build/not-pie"),),
        )
        self.assertEqual(result.executables, ())
        self.assertEqual(result.unmatched, ("/bin/pie",))

    def test_candidate_identity_wins_when_only_runtime_has_build_id(self):
        runtime = load_elf64(build_id=bytes.fromhex("0123456789abcdef"))
        debug = load_elf64(dwarf=True)
        result = catalog_newc_userspace(
            newc([("bin/app", stat.S_IFREG | 0o755, runtime)]),
            (candidate(debug, "build/app"),),
        )
        self.assertEqual(len(result.executables), 1)
        self.assertEqual(
            result.executables[0]["build_id"],
            elf_load_identity(debug).fingerprint,
        )

    def test_debug_only_file_with_different_load_bytes_does_not_match(self):
        runtime = load_elf64(stripped_sections=True)
        debug_only = load_elf64(
            dwarf=True,
            load_marker=b"debug-only-placeholder",
        )
        result = catalog_newc_userspace(
            newc([("bin/app", stat.S_IFREG | 0o755, runtime)]),
            (candidate(debug_only, "build/app.debug"),),
        )
        self.assertEqual(result.executables, ())
        self.assertEqual(result.unmatched, ("/bin/app",))

    def test_candidate_descriptor_identity_is_checked(self):
        data = elf64(bytes.fromhex("0123456789abcdef"), dwarf=True)
        descriptor = CapturedArtifact(
            box_id=1, path="debug/app", size=len(data), sha256="0" * 64,
            record_id="record",
        )
        with self.assertRaisesRegex(NewcCatalogError, "SHA-256 mismatch"):
            DebugElfCandidate(descriptor, data)

    def test_candidate_architecture_metadata_is_checked(self):
        build_id = bytes.fromhex("0123456789abcdef")
        runtime = elf64(build_id)
        debug = elf64(build_id, dwarf=True)
        with self.assertRaisesRegex(NewcCatalogError, "architecture mismatch"):
            catalog_newc_userspace(
                newc([("bin/app", stat.S_IFREG | 0o755, runtime)]),
                (candidate(debug, "debug/app", architecture="aarch64"),),
            )

    def test_unsafe_duplicate_and_truncated_archives_are_rejected(self):
        for archive, message in (
            (newc([("../escape", stat.S_IFREG | 0o755, b"x")]), "unsafe"),
            (newc([
                ("bin/app", stat.S_IFREG | 0o755, b"x"),
                ("./bin/app", stat.S_IFREG | 0o755, b"y"),
            ]), "duplicate"),
            (newc([("bin/app", stat.S_IFREG | 0o755, b"x")])[:-20], "truncated|trailer"),
        ):
            with self.subTest(message=message):
                with self.assertRaisesRegex(NewcCatalogError, message):
                    catalog_newc_userspace(archive, ())

    def test_crc_newc_checksum_is_enforced(self):
        archive = newc([("bin/app", stat.S_IFREG | 0o755, b"contents")], crc=True)
        self.assertEqual(catalog_newc_userspace(archive, ()).archive_format, "newc")
        changed = bytearray(archive)
        changed[archive.index(b"contents")] ^= 1
        with self.assertRaisesRegex(NewcCatalogError, "checksum"):
            catalog_newc_userspace(bytes(changed), ())


if __name__ == "__main__":
    unittest.main()
