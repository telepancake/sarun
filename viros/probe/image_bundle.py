#!/usr/bin/env python3
"""Publish one exact initramfs image and its debugger identities.

This command is intended to run inside the Sarun box which built the image.
The resulting directory is captured in that box and later selected by Sarun's
normal box/attachment lookup.  The manifest deliberately contains no host
installation paths and accepts no QEMU argument fragments.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import struct
import sys
import tempfile

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from probe.probe_tool import AuditError, ElfObject, gnu_build_id
from probe.elf_load_identity import (
    ElfLoadIdentityError,
    elf_load_identity,
    same_loadable_content,
)


FORMAT = "viros-image-bundle-v1"
PROFILES = {
    "aarch64": "virt-initramfs-aarch64-v1",
    "arm": "virt-initramfs-arm-v1",
    "mmips": "malta-initramfs-mipsel-v1",
    "x86_64": "microvm-initramfs-x86_64-v1",
}
MACHINES = {
    "aarch64": 183,
    "arm": 40,
    "mmips": 8,
    "x86_64": 62,
}
MAX_ELF_CANDIDATES = 100_000
MAX_ELF_CANDIDATE_BYTES = 128 * 1024 * 1024 * 1024


class ImageBundleError(RuntimeError):
    pass


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def safe_guest_path(value: str) -> str:
    if not value.startswith("/") or "\x00" in value or "\\" in value:
        raise ImageBundleError(f"guest executable must be an absolute POSIX path: {value!r}")
    parts = PurePosixPath(value).parts
    if any(part in (".", "..") for part in parts):
        raise ImageBundleError(f"unsafe guest executable path: {value!r}")
    return value


def safe_kernel_init(value: str) -> str:
    value = safe_guest_path(value)
    if any(byte <= 0x20 or byte == 0x7F for byte in value.encode("utf-8")):
        raise ImageBundleError(
            f"kernel init path cannot contain whitespace or control bytes: {value!r}"
        )
    return value


def parse_executable(value: str) -> tuple[str, Path]:
    try:
        guest, debug = value.split("=", 1)
    except ValueError as exc:
        raise ImageBundleError(
            "--executable must be GUEST_ABSOLUTE_PATH=UNSTRIPPED_ELF"
        ) from exc
    if not debug:
        raise ImageBundleError("--executable has an empty unstripped ELF path")
    return safe_guest_path(guest), Path(debug)


def checked_root_member(root: Path, guest_path: str) -> Path:
    candidate = root.joinpath(*PurePosixPath(guest_path).parts[1:])
    try:
        candidate.resolve(strict=True).relative_to(root.resolve(strict=True))
    except (OSError, ValueError) as exc:
        raise ImageBundleError(
            f"guest executable {guest_path!r} is absent from the rootfs tree or leaves it"
        ) from exc
    if not candidate.is_file():
        raise ImageBundleError(f"guest executable is not a regular file: {guest_path}")
    return candidate


def elf_identity(path: Path, architecture: str, *, require_debug: bool) -> dict[str, object]:
    try:
        contents = path.read_bytes()
        load_identity = elf_load_identity(contents)
    except (OSError, ElfLoadIdentityError) as exc:
        raise ImageBundleError(f"cannot identify ELF {path}: {exc}") from exc
    if load_identity.machine != MACHINES[architecture]:
        raise ImageBundleError(
            f"{path}: ELF machine {load_identity.machine} does not match {architecture}"
        )
    build_id: str | None = None
    try:
        elf = ElfObject(path)
    except (OSError, AuditError) as exc:
        # A deployed ELF may have its section-header table removed entirely.
        # Its PT_LOAD identity is still exact enough for runtime association,
        # but a debugger ELF must retain sections so DWARF can be verified.
        if require_debug:
            raise ImageBundleError(f"cannot identify ELF {path}: {exc}") from exc
    else:
        section_names = {section["name"] for section in elf.sections}
        has_debug_info = ".debug_info" in section_names or ".zdebug_info" in section_names
        has_debug_line = ".debug_line" in section_names or ".zdebug_line" in section_names
        if require_debug and not (has_debug_info and has_debug_line):
            raise ImageBundleError(
                f"{path}: unstripped debugger ELF must contain DWARF info and line tables"
            )
        try:
            build_id = gnu_build_id(path, architecture)
        except AuditError as exc:
            if "no GNU build ID was found" not in str(exc):
                raise ImageBundleError(f"cannot identify ELF {path}: {exc}") from exc
        except OSError as exc:
            raise ImageBundleError(f"cannot identify ELF {path}: {exc}") from exc
    return {
        "build_id": build_id,
        "loadable_sha256": load_identity.fingerprint,
        "sha256": sha256_file(path),
        "size": path.stat().st_size,
        "elf_class": load_identity.elf_class,
        "elf_machine": load_identity.machine,
        # Private publisher values used for direct normalized PT_LOAD byte
        # comparison after the fingerprint/layout filter.
        "_load_identity": load_identity,
        "_contents": contents,
    }


def _is_below(path: Path, directory: Path) -> bool:
    try:
        path.relative_to(directory)
        return True
    except ValueError:
        return False


def discovery_roots(
    rootfs: Path, kernel_bundle: Path, output: Path, workspace: Path | None = None,
) -> list[Path]:
    """Return a small deterministic set of captured-tree roots to examine.

    A normal publication runs from the firmware build's working directory, so
    that tree is the primary source.  The three artifact parents cover callers
    which run the publisher from a sibling directory.  Contained roots are
    collapsed, and the filesystem root is never accepted as a discovery root.
    """

    anchors = [
        (workspace or Path.cwd()).resolve(),
        rootfs.resolve().parent,
        kernel_bundle.resolve().parent,
        output.resolve().parent,
    ]
    candidates = sorted(
        {path for path in anchors if path.is_dir() and path != Path(path.anchor)},
        key=lambda path: (len(path.parts), path.as_posix()),
    )
    roots: list[Path] = []
    for candidate in candidates:
        if any(_is_below(candidate, root) for root in roots):
            continue
        roots.append(candidate)
    return roots


def _regular_files(roots: list[Path], excluded: tuple[Path, ...]):
    """Yield regular files in stable order without crossing symlinked trees."""

    seen: set[Path] = set()
    excluded = tuple(path.resolve() for path in excluded)
    for root in roots:
        root = root.resolve()
        if root in seen or any(_is_below(root, path) for path in excluded):
            continue
        seen.add(root)
        for directory, names, files in os.walk(root, followlinks=False):
            directory_path = Path(directory)
            names[:] = sorted(
                name for name in names
                if not any(
                    _is_below((directory_path / name).resolve(), path)
                    for path in excluded
                )
            )
            for name in sorted(files):
                path = directory_path / name
                try:
                    metadata = path.lstat()
                except OSError:
                    continue
                if not stat.S_ISREG(metadata.st_mode):
                    continue
                yield path


def _matching_elf_image(path: Path, architecture: str) -> bool:
    """Cheaply select linked ELFs for the requested machine.

    Build trees contain very many object and archive members.  Reading the ELF
    identification and fixed header first keeps those files out of the more
    detailed DWARF/build-ID reader.
    """

    try:
        with path.open("rb") as stream:
            header = stream.read(20)
    except OSError:
        return False
    if len(header) < 20 or header[:4] != b"\x7fELF" or header[4] not in {1, 2}:
        return False
    if header[5] == 1:
        byteorder = "little"
    elif header[5] == 2:
        byteorder = "big"
    else:
        return False
    elf_type = int.from_bytes(header[16:18], byteorder)
    machine = int.from_bytes(header[18:20], byteorder)
    return elf_type in {2, 3} and machine == MACHINES[architecture]


def _bounded_elf_candidates(paths, architecture: str, *, executable_only: bool = False):
    count = 0
    total_bytes = 0
    for path in paths:
        try:
            metadata = path.stat()
        except OSError:
            continue
        if executable_only and not metadata.st_mode & 0o111:
            continue
        if not _matching_elf_image(path, architecture):
            continue
        count += 1
        total_bytes += metadata.st_size
        if count > MAX_ELF_CANDIDATES or total_bytes > MAX_ELF_CANDIDATE_BYTES:
            raise ImageBundleError(
                "executable discovery exceeded the linked-ELF candidate bound "
                f"({MAX_ELF_CANDIDATES} files or "
                f"{MAX_ELF_CANDIDATE_BYTES} bytes)"
            )
        yield path


def discover_runtime_executables(
    rootfs: Path, architecture: str, explicit_guests: set[str],
) -> list[tuple[str, Path, dict[str, object]]]:
    """Identify executable runtime ELFs which can participate in association."""

    rows = []
    rootfs = rootfs.resolve()
    paths = _bounded_elf_candidates(
        _regular_files([rootfs], ()), architecture, executable_only=True,
    )
    for path in paths:
        guest_path = "/" + path.relative_to(rootfs).as_posix()
        if guest_path in explicit_guests:
            continue
        try:
            identity = elf_identity(path, architecture, require_debug=False)
        except ImageBundleError:
            # Scripts, malformed ELFs, and ELFs for another architecture
            # remain valid rootfs content but cannot be associated safely.
            continue
        rows.append((guest_path, path, identity))
    return rows


def discover_debug_elves(
    runtime_identities: list[dict[str, object]], architecture: str, roots: list[Path],
    excluded: tuple[Path, ...],
) -> dict[tuple[str, object], list[tuple[Path, dict[str, object]]]]:
    """Index relevant DWARF ELFs by build ID and normalized PT_LOAD identity."""

    build_ids = {
        str(identity["build_id"])
        for identity in runtime_identities
        if identity.get("build_id") is not None
    }
    load_keys = {
        identity["_load_identity"].filter_key
        for identity in runtime_identities
        if "_load_identity" in identity
    }
    if not build_ids and not load_keys:
        return {}
    result: dict[tuple[str, object], list[tuple[Path, dict[str, object]]]] = {}
    paths = _bounded_elf_candidates(_regular_files(roots, excluded), architecture)
    for path in paths:
        try:
            identity = elf_identity(path, architecture, require_debug=True)
        except ImageBundleError:
            continue
        build_id = identity.get("build_id")
        if build_id is not None and str(build_id) in build_ids:
            result.setdefault(("build-id", str(build_id)), []).append(
                (path.resolve(), identity)
            )
        if load_keys and "_load_identity" in identity:
            load_key = identity["_load_identity"].filter_key
            if load_key in load_keys:
                result.setdefault(("loadable", load_key), []).append(
                    (path.resolve(), identity)
                )
    return result


def _association_identifier(identity: dict[str, object]) -> str:
    build_id = identity.get("build_id")
    return str(build_id) if build_id is not None else str(identity["loadable_sha256"])


def _same_loadable(left: dict[str, object], right: dict[str, object]) -> bool:
    return same_loadable_content(
        left["_contents"], left["_load_identity"],
        right["_contents"], right["_load_identity"],
    )


def select_debug_elf(
    runtime: dict[str, object],
    indexed: dict[tuple[str, object], list[tuple[Path, dict[str, object]]]],
) -> tuple[Path, dict[str, object]] | None:
    """Select one content-unique debugger ELF for one runtime identity."""

    runtime_build_id = runtime.get("build_id")
    candidates = (
        indexed.get(("build-id", str(runtime_build_id)), [])
        if runtime_build_id is not None
        else []
    )
    association_kind = "build-id"
    if not candidates:
        if "_load_identity" not in runtime:
            return None
        association_kind = "loadable"
        load_key = runtime["_load_identity"].filter_key
        candidates = []
        for candidate in indexed.get(("loadable", load_key), []):
            candidate_build_id = candidate[1].get("build_id")
            # Two different, present GNU build IDs are affirmative evidence
            # of different link results.  PT_LOAD fallback is permitted only
            # because at least one side lacks the note.
            if runtime_build_id is not None and candidate_build_id is not None:
                continue
            if _same_loadable(runtime, candidate[1]):
                candidates.append(candidate)
    if not candidates:
        return None
    by_content: dict[str, list[tuple[Path, dict[str, object]]]] = {}
    for candidate in candidates:
        by_content.setdefault(str(candidate[1]["sha256"]), []).append(candidate)
    if len(by_content) != 1:
        paths = sorted(
            path.as_posix()
            for copies in by_content.values()
            for path, _identity in copies
        )
        identity = (
            str(runtime_build_id)
            if association_kind == "build-id"
            else str(runtime["loadable_sha256"])
        )
        label = "build ID" if association_kind == "build-id" else "loadable identity"
        raise ImageBundleError(
            f"{label} {identity} has ambiguous DWARF ELF contents: "
            + ", ".join(paths)
        )
    copies = next(iter(by_content.values()))
    return min(copies, key=lambda item: item[0].as_posix())


def _pad4(stream, size: int) -> None:
    padding = (-size) & 3
    if padding:
        stream.write(b"\0" * padding)


def _newc_header(
    *, inode: int, mode: int, uid: int, gid: int, nlink: int, mtime: int,
    size: int, dev_major: int, dev_minor: int, rdev_major: int,
    rdev_minor: int, name_size: int,
) -> bytes:
    fields = (
        inode, mode, uid, gid, nlink, mtime, size, dev_major, dev_minor,
        rdev_major, rdev_minor, name_size, 0,
    )
    return b"070701" + b"".join(f"{value & 0xffffffff:08x}".encode() for value in fields)


def write_initramfs(root: Path, output: Path) -> None:
    """Write a deterministic uncompressed newc archive without invoking host tools."""

    root = root.resolve(strict=True)
    members = sorted(root.rglob("*"), key=lambda path: path.relative_to(root).as_posix())
    inode = 1
    with output.open("wb") as stream:
        for path in members:
            relative = path.relative_to(root).as_posix()
            metadata = path.lstat()
            mode = metadata.st_mode
            if stat.S_ISREG(mode):
                contents = path.read_bytes()
                rdev_major = rdev_minor = 0
            elif stat.S_ISLNK(mode):
                contents = os.readlink(path).encode()
                rdev_major = rdev_minor = 0
            elif stat.S_ISDIR(mode):
                contents = b""
                rdev_major = rdev_minor = 0
            elif stat.S_ISCHR(mode) or stat.S_ISBLK(mode):
                contents = b""
                rdev_major = os.major(metadata.st_rdev)
                rdev_minor = os.minor(metadata.st_rdev)
            elif stat.S_ISFIFO(mode):
                contents = b""
                rdev_major = rdev_minor = 0
            else:
                raise ImageBundleError(f"unsupported rootfs member type: /{relative}")
            name = relative.encode() + b"\0"
            stream.write(_newc_header(
                inode=inode, mode=mode, uid=metadata.st_uid, gid=metadata.st_gid,
                # Each archive member owns its bytes. This deliberately
                # expands rootfs hard links instead of publishing incomplete
                # newc hard-link groups with host inode identities.
                nlink=1, mtime=0, size=len(contents),
                dev_major=0, dev_minor=0, rdev_major=rdev_major,
                rdev_minor=rdev_minor, name_size=len(name),
            ))
            stream.write(name)
            _pad4(stream, 110 + len(name))
            stream.write(contents)
            _pad4(stream, len(contents))
            inode += 1
        trailer = b"TRAILER!!!\0"
        stream.write(_newc_header(
            inode=inode, mode=0, uid=0, gid=0, nlink=1, mtime=0, size=0,
            dev_major=0, dev_minor=0, rdev_major=0, rdev_minor=0,
            name_size=len(trailer),
        ))
        stream.write(trailer)
        _pad4(stream, 110 + len(trailer))


def artifact_rows(root: Path) -> list[dict[str, object]]:
    rows = []
    for path in sorted(root.rglob("*")):
        if path.is_file() and not path.is_symlink() and path.name != "image.json":
            rows.append({
                "path": path.relative_to(root).as_posix(),
                "size": path.stat().st_size,
                "sha256": sha256_file(path),
            })
    return rows


def build(args: argparse.Namespace) -> Path:
    rootfs = args.rootfs.resolve(strict=True)
    kernel_bundle = args.kernel_bundle.resolve(strict=True)
    output = args.output_dir.resolve()
    if output.exists():
        raise ImageBundleError(f"output already exists: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    if not rootfs.is_dir():
        raise ImageBundleError(f"rootfs is not a directory: {rootfs}")
    if not kernel_bundle.is_dir() or not (kernel_bundle / "bundle.json").is_file():
        raise ImageBundleError("--kernel-bundle must contain bundle.json")
    kernel_document = json.loads((kernel_bundle / "bundle.json").read_text(encoding="utf-8"))
    if kernel_document.get("format") != "viros-kernel-bundle-v1":
        raise ImageBundleError("kernel bundle has the wrong format")
    if kernel_document.get("architecture") != args.arch:
        raise ImageBundleError("kernel bundle architecture does not match --arch")
    checked_root_member(rootfs, args.init)

    requested = [parse_executable(value) for value in args.executable]
    seen_guest: set[str] = set()
    associations: list[tuple[str, Path, dict[str, object], Path, dict[str, object]]] = []
    for guest_path, debug_path in requested:
        if guest_path in seen_guest:
            raise ImageBundleError(f"duplicate guest executable: {guest_path}")
        seen_guest.add(guest_path)
        runtime_path = checked_root_member(rootfs, guest_path)
        runtime = elf_identity(runtime_path, args.arch, require_debug=False)
        resolved_debug = debug_path.resolve(strict=True)
        debug = elf_identity(resolved_debug, args.arch, require_debug=True)
        runtime_build_id = runtime.get("build_id")
        debug_build_id = debug.get("build_id")
        if runtime_build_id is not None and debug_build_id is not None:
            if runtime_build_id != debug_build_id:
                raise ImageBundleError(
                    f"{guest_path}: runtime and debugger ELF build IDs differ"
                )
        elif not _same_loadable(runtime, debug):
            raise ImageBundleError(
                f"{guest_path}: runtime and debugger ELF loadable contents differ"
            )
        associations.append((guest_path, runtime_path, runtime, resolved_debug, debug))

    discovered_runtime = discover_runtime_executables(rootfs, args.arch, seen_guest)
    discovered_debug = discover_debug_elves(
        [identity for _guest, _path, identity in discovered_runtime],
        args.arch,
        discovery_roots(rootfs, kernel_bundle, output),
        (rootfs, kernel_bundle, output),
    )
    for guest_path, runtime_path, runtime in discovered_runtime:
        match = select_debug_elf(runtime, discovered_debug)
        if match is None:
            continue
        debug_path, debug = match
        associations.append((guest_path, runtime_path, runtime, debug_path, debug))

    executable_rows: list[dict[str, object]] = []
    staging = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent))
    try:
        shutil.copytree(kernel_bundle, staging / "kernel", symlinks=False)
        write_initramfs(rootfs, staging / "rootfs.cpio")
        (staging / "symbols").mkdir()
        copied_by_id: dict[str, tuple[str, str]] = {}
        for guest_path, _runtime_path, runtime, debug_path, debug in associations:
            # The retained debugger ELF is what the managed client validates.
            # In a one-sided build-ID fallback its stable identifier therefore
            # comes from that candidate, not from the stripped runtime.
            build_id = _association_identifier(debug)
            destination = f"symbols/{build_id}.elf"
            prior = copied_by_id.get(build_id)
            if prior is None:
                shutil.copy2(debug_path, staging / destination)
                copied_by_id[build_id] = (str(debug["sha256"]), destination)
            elif prior[0] != debug["sha256"]:
                raise ImageBundleError(
                    f"build ID {build_id} identifies different debugger ELF contents"
                )
            executable_rows.append({
                "guest_path": guest_path,
                "build_id": build_id,
                "runtime_sha256": runtime["sha256"],
                "runtime_size": runtime["size"],
                "debug_elf": destination,
                "debug_sha256": debug["sha256"],
                "debug_size": debug["size"],
                "elf_class": runtime["elf_class"],
                "elf_machine": runtime["elf_machine"],
                # DWARF paths are resolved in the immutable provider box root.
                "source_view": "provider-root",
            })
        executable_rows.sort(key=lambda row: str(row["guest_path"]))
        document = {
            "format": FORMAT,
            "architecture": args.arch,
            "boot": {
                "profile": PROFILES[args.arch],
                "kernel_bundle": "kernel/bundle.json",
                "initramfs": "rootfs.cpio",
                "init": args.init,
            },
            "userspace": {"executables": executable_rows},
            "artifacts": artifact_rows(staging),
        }
        with (staging / "image.json").open("w", encoding="utf-8") as stream:
            json.dump(document, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(staging, output)
        return output
    except Exception:
        shutil.rmtree(staging, ignore_errors=True)
        raise


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--arch", required=True, choices=tuple(PROFILES))
    result.add_argument("--rootfs", required=True, type=Path)
    result.add_argument("--kernel-bundle", required=True, type=Path)
    result.add_argument("--output-dir", required=True, type=Path)
    result.add_argument("--init", default="/sbin/init", type=safe_kernel_init)
    result.add_argument(
        "--executable", action="append", default=[], metavar="GUEST=ELF",
        help=(
            "optional exact runtime/debug association; executable rootfs ELFs "
            "are otherwise associated from the captured build tree by GNU "
            "build ID, or exact normalized PT_LOAD content when no ID exists"
        ),
    )
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    output = build(args)
    print(f"Published named-box image bundle: {output}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ImageBundleError, OSError, json.JSONDecodeError) as exc:
        print(f"image-bundle: {exc}", file=os.sys.stderr)
        raise SystemExit(1)
