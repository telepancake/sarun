"""Provider-box entry point for a Sarun-managed interactive GDB session."""

from __future__ import annotations

from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import sys

from probe.elf_load_identity import ElfLoadIdentityError, elf_load_identity


PREFIX = b"SARUN-DEBUG-CLIENT/1 "
MAX_LINE = 16 * 1024 * 1024


class ClientProtocolError(ValueError):
    pass


@dataclass(frozen=True)
class Executable:
    guest_path: str
    # Wire-compatible association identity: a GNU build ID or the canonical
    # viros PT_LOAD fingerprint when no exact build-ID association is possible.
    build_id: str
    runtime_sha256: str
    runtime_size: int
    debug_elf: str
    debug_sha256: str
    debug_size: int
    elf_class: int
    elf_machine: int
    source_view: str


@dataclass(frozen=True)
class Start:
    manifest: str
    service: str
    executables: tuple[Executable, ...]


def _atom(data: memoryview) -> tuple[bytes, memoryview]:
    if not data:
        raise ClientProtocolError("truncated wire atom")
    tag = data[0]
    if tag < 0xC0:
        return bytes((tag,)), data[1:]
    if tag < 0xF8:
        prefix, length = 1, tag - 0xC0
    else:
        width = tag - 0xF8
        if not width or len(data) < 1 + width:
            raise ClientProtocolError("invalid wire atom length")
        encoded = bytes(data[1 : 1 + width])
        if encoded[-1] == 0:
            raise ClientProtocolError("non-canonical wire atom length")
        length = int.from_bytes(encoded, "little")
        if length <= 55:
            raise ClientProtocolError("non-canonical long wire atom")
        prefix = 1 + width
    end = prefix + length
    if end > len(data):
        raise ClientProtocolError("truncated wire atom payload")
    return bytes(data[prefix:end]), data[end:]


def _u64(data: memoryview) -> tuple[int, memoryview]:
    payload, rest = _atom(data)
    if len(payload) > 8 or (payload and payload[-1] == 0):
        raise ClientProtocolError("integer is not a canonical u64")
    return int.from_bytes(payload, "little"), rest


def _text(payload: bytes, field: str) -> str:
    try:
        return payload.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ClientProtocolError(f"{field} is not UTF-8") from exc


def _relative(payload: bytes, field: str) -> str:
    value = _text(payload, field)
    parts = value.split("/")
    if not value or value.startswith("/") or "\\" in value or "\0" in value:
        raise ClientProtocolError(f"{field} is not provider-root-relative")
    if any(part in {"", ".", ".."} for part in parts):
        raise ClientProtocolError(f"{field} is not provider-root-relative")
    return value


def _decode_executable(payload: bytes) -> Executable:
    fields = memoryview(payload)
    guest, fields = _atom(fields)
    build_id, fields = _atom(fields)
    runtime_sha, fields = _atom(fields)
    runtime_size, fields = _u64(fields)
    debug_elf, fields = _atom(fields)
    debug_sha, fields = _atom(fields)
    debug_size, fields = _u64(fields)
    elf_class, fields = _u64(fields)
    elf_machine, fields = _u64(fields)
    source_view, fields = _u64(fields)
    if fields or len(runtime_sha) != 32 or len(debug_sha) != 32:
        raise ClientProtocolError("invalid executable catalog record")
    guest_path = _text(guest, "guest executable path")
    if (
        not guest_path.startswith("/")
        or "\0" in guest_path
        or any(part in {"", ".", ".."} for part in guest_path[1:].split("/"))
    ):
        raise ClientProtocolError("guest executable path is not absolute")
    build_id_text = _text(build_id, "executable association identity")
    if not re.fullmatch(r"[0-9a-f]{8,128}", build_id_text) or len(build_id_text) % 2:
        raise ClientProtocolError("invalid executable association identity")
    if not runtime_size or not debug_size:
        raise ClientProtocolError("invalid executable size")
    return Executable(
        guest_path=guest_path,
        build_id=build_id_text,
        runtime_sha256=runtime_sha.hex(),
        runtime_size=runtime_size,
        debug_elf=_relative(debug_elf, "debug ELF path"),
        debug_sha256=debug_sha.hex(),
        debug_size=debug_size,
        elf_class=elf_class,
        elf_machine=elf_machine,
        source_view={1: "provider-root"}.get(source_view, ""),
    )


def decode_start_line(line: bytes) -> Start:
    if not line.endswith(b"\n") or not line.startswith(PREFIX):
        raise ClientProtocolError("missing managed GDB start record")
    try:
        encoded = bytes.fromhex(line[len(PREFIX) : -1].decode("ascii"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise ClientProtocolError("managed GDB start record is not canonical hex") from exc
    version, rest = _u64(memoryview(encoded))
    if version != 1:
        raise ClientProtocolError("unsupported managed GDB protocol version")
    record, rest = _atom(rest)
    if rest:
        raise ClientProtocolError("managed GDB start has trailing bytes")
    fields = memoryview(record)
    manifest, fields = _atom(fields)
    option, fields = _atom(fields)
    service, fields = _atom(fields)
    if fields:
        raise ClientProtocolError("managed GDB start has trailing fields")

    option_fields = memoryview(option)
    present, option_fields = _u64(option_fields)
    executables: tuple[Executable, ...] = ()
    if present == 1:
        catalog, option_fields = _atom(option_fields)
        catalog_fields = memoryview(catalog)
        catalog_manifest, catalog_fields = _atom(catalog_fields)
        profile, catalog_fields = _u64(catalog_fields)
        init, catalog_fields = _atom(catalog_fields)
        encoded_list, catalog_fields = _atom(catalog_fields)
        if catalog_fields:
            raise ClientProtocolError("image catalog has trailing fields")
        list_fields = memoryview(encoded_list)
        count, list_fields = _u64(list_fields)
        if count > 65536:
            raise ClientProtocolError("executable catalog exceeds its bound")
        _relative(catalog_manifest, "image manifest")
        init_path = _text(init, "image init")
        if profile not in {1, 2, 3, 4} or not init_path.startswith("/") or "\0" in init_path:
            raise ClientProtocolError("invalid image boot identity")
        rows = []
        for _ in range(count):
            row, list_fields = _atom(list_fields)
            rows.append(_decode_executable(row))
        if list_fields:
            raise ClientProtocolError("executable catalog has trailing fields")
        executables = tuple(rows)
    elif present != 0:
        raise ClientProtocolError("invalid image catalog option")
    if option_fields:
        raise ClientProtocolError("image catalog option has trailing fields")
    service_name = _text(service, "session service")
    if not re.fullmatch(r"[A-Za-z0-9_-][A-Za-z0-9_.-]{0,63}", service_name):
        raise ClientProtocolError("session service name is invalid")
    if any(executable.source_view != "provider-root" for executable in executables):
        raise ClientProtocolError("unsupported executable source view")
    return Start(
        manifest=_relative(manifest, "callgate manifest"),
        service=service_name,
        executables=executables,
    )


def _gnu_build_id(
    data: bytes, path: Path, elf_class: int, elf_machine: int
) -> str | None:
    minimum = 64 if elf_class == 64 else 52
    if len(data) < minimum or data[:4] != b"\x7fELF":
        raise ClientProtocolError(f"{path}: not an ELF file")
    found_class = {1: 32, 2: 64}.get(data[4])
    endian = {1: "<", 2: ">"}.get(data[5])
    if found_class != elf_class or endian is None:
        raise ClientProtocolError(f"{path}: ELF class does not match catalog")
    if struct.unpack_from(endian + "H", data, 18)[0] != elf_machine:
        raise ClientProtocolError(f"{path}: ELF machine does not match catalog")
    if elf_class == 64:
        shoff = struct.unpack_from(endian + "Q", data, 40)[0]
        shentsize, shnum = struct.unpack_from(endian + "HH", data, 58)
        section = endian + "IIQQQQIIQQ"
    else:
        shoff = struct.unpack_from(endian + "I", data, 32)[0]
        shentsize, shnum = struct.unpack_from(endian + "HH", data, 46)
        section = endian + "IIIIIIIIII"
    section_size = struct.calcsize(section)
    if shnum == 0 and shoff == 0:
        return None
    if shentsize < section_size or shoff + shnum * shentsize > len(data):
        raise ClientProtocolError(f"{path}: malformed ELF section table")
    identifiers: set[str] = set()
    for index in range(shnum):
        values = struct.unpack_from(section, data, shoff + index * shentsize)
        if values[1] != 7:
            continue
        offset, size = values[4], values[5]
        at, end = offset, offset + size
        if end > len(data):
            raise ClientProtocolError(f"{path}: malformed ELF note section")
        while at + 12 <= end:
            namesz, descsz, note_type = struct.unpack_from(endian + "III", data, at)
            at += 12
            if at + namesz > end:
                raise ClientProtocolError(f"{path}: malformed ELF note name")
            name = data[at : at + namesz].rstrip(b"\0")
            at = (at + namesz + 3) & ~3
            if at + descsz > end:
                raise ClientProtocolError(f"{path}: malformed ELF note description")
            desc = data[at : at + descsz]
            at = (at + descsz + 3) & ~3
            if name == b"GNU" and note_type == 3:
                if not 4 <= len(desc) <= 64:
                    raise ClientProtocolError(
                        f"{path}: GNU build ID has an unsupported size"
                    )
                identifiers.add(desc.hex())
    if len(identifiers) > 1:
        raise ClientProtocolError(f"{path}: ELF has conflicting GNU build IDs")
    return next(iter(identifiers)) if identifiers else None


def _validated_debug_identity(
    path: Path, executable: Executable, data: bytes
) -> str:
    """Validate the preferred GNU identity or the deterministic fallback."""

    build_id = _gnu_build_id(
        data, path, executable.elf_class, executable.elf_machine
    )
    if build_id == executable.build_id:
        return "gnu-build-id"
    try:
        load_identity = elf_load_identity(data)
    except ElfLoadIdentityError as exc:
        detail = (
            "GNU build ID changed"
            if build_id is not None
            else "GNU build ID is missing and loadable-content identity is unavailable"
        )
        raise ClientProtocolError(f"{path}: {detail}: {exc}") from exc
    if (
        load_identity.elf_class != executable.elf_class
        or load_identity.machine != executable.elf_machine
    ):
        raise ClientProtocolError(
            f"{path}: loadable-content ELF identity does not match catalog"
        )
    if load_identity.fingerprint != executable.build_id:
        raise ClientProtocolError(f"{path}: executable identity changed")
    return "loadable-content-sha256"


def validate_catalog(start: Start) -> list[dict[str, object]]:
    rows = []
    for executable in start.executables:
        path = Path("/") / executable.debug_elf
        stat = path.stat()
        if stat.st_size != executable.debug_size:
            raise ClientProtocolError(f"{path}: debug ELF size changed")
        data = path.read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        if digest != executable.debug_sha256:
            raise ClientProtocolError(f"{path}: debug ELF content changed")
        identity_kind = _validated_debug_identity(path, executable, data)
        row = asdict(executable)
        row["debug_elf"] = str(path)
        row["identity_kind"] = identity_kind
        rows.append(row)
    return rows


def _gdb_quote(value: Path | str) -> str:
    return '"' + str(value).replace("\\", "\\\\").replace('"', '\\"') + '"'


def _gdb_script(start: Start, rows: list[dict[str, object]], root: Path) -> bytes:
    bundle = (Path("/") / start.manifest).parent
    kernel = bundle / "vmlinux"
    catalog = json.dumps(rows, sort_keys=True, separators=(",", ":"))
    commands = [
        "set pagination off",
        "set confirm off",
        "set python print-stack full",
        f"set sysroot {_gdb_quote(root / 'sysroot')}",
        f"set auto-load safe-path {_gdb_quote(bundle)}",
        f"file {_gdb_quote(kernel)}",
        f"source {_gdb_quote(bundle / 'vmlinux-gdb.py')}",
        "python",
        "import sys, json",
        f"sys.path.insert(0, {str(root)!r})",
        "from inferiors.sarun_gdb_symbols import install, finalize",
        f"install(json.loads({catalog!r}), {str(kernel)!r})",
        "end",
        "set remotetimeout 10",
        f"target remote | sarun service dial {start.service}",
        "python finalize()",
        "info inferiors",
    ]
    return ("\n".join(commands) + "\n").encode()


def main() -> int:
    line = sys.stdin.buffer.readline(MAX_LINE + 1)
    if len(line) > MAX_LINE:
        raise ClientProtocolError("managed GDB start record exceeds its bound")
    start = decode_start_line(line)
    rows = validate_catalog(start)
    root = Path(__file__).resolve().parents[1]
    gdb = root / "tools/gdb/bin/gdb"
    python_lib = root / "tools/python/managed/lib"
    if not gdb.is_file() or not python_lib.is_dir():
        raise ClientProtocolError("named provider box is incomplete; reinstall it")
    script_fd = os.memfd_create("sarun-gdb-start", os.MFD_CLOEXEC)
    os.write(script_fd, _gdb_script(start, rows, root))
    os.lseek(script_fd, 0, os.SEEK_SET)
    os.set_inheritable(script_fd, True)
    environment = os.environ.copy()
    environment["PYTHONHOME"] = str(root / "tools/python/managed")
    os.execve(
        gdb,
        [str(gdb), "-nx", "-q", "-x", f"/proc/self/fd/{script_fd}"],
        environment,
    )
    return 127


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ClientProtocolError, OSError) as exc:
        print(f"viros-gdb-managed: {exc}", file=sys.stderr)
        raise SystemExit(2)
