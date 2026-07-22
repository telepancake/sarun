"""Sarun-owned entry point for the viros debugger provider.

The provider protocol is intentionally tiny and path-free on the host side.
Sarun writes a versioned ``DebugProviderStart`` atom to the provider's inherited
stdin socket.  After that atom, the very same duplex stream is QEMU's raw RSP
connection.  GDB reaches the facade through a generic, per-session Sarun
service acceptor; this module never creates a host socket name.
"""

from __future__ import annotations

from dataclasses import dataclass
import os
import re
import socket
import subprocess
import sys
from typing import BinaryIO, Callable

from .live_facade import LiveFacade, build_live_facade


PROTOCOL_VERSION = 1
MAX_FRAME_BYTES = 16 * 1024 * 1024
MAX_PATH_BYTES = 1024 * 1024
MAX_SERVICE_BYTES = 4096


class ProviderProtocolError(ValueError):
    pass


@dataclass(frozen=True)
class DebugProviderStart:
    manifest: bytes
    service: str
    image: "DebugImageCatalog | None"


@dataclass(frozen=True)
class DebugExecutable:
    guest_path: bytes
    # Kept in the established wire slot: GNU build ID when present, otherwise
    # the canonical viros PT_LOAD SHA-256 fingerprint.
    build_id: bytes
    runtime_sha256: bytes
    runtime_size: int
    debug_elf: bytes
    debug_sha256: bytes
    debug_size: int
    elf_class: int
    elf_machine: int


@dataclass(frozen=True)
class DebugImageCatalog:
    manifest: bytes
    profile: int
    init: bytes
    executables: tuple[DebugExecutable, ...]


def _read_exact(fd: int, length: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < length:
        chunk = os.read(fd, length - len(chunks))
        if not chunk:
            raise ProviderProtocolError("truncated provider start frame")
        chunks.extend(chunk)
    return bytes(chunks)


def _read_atom(fd: int, maximum: int, *, compound: bool = False) -> bytes:
    tag = _read_exact(fd, 1)[0]
    if tag < 0xC0:
        if compound:
            raise ProviderProtocolError("provider start is not a compound atom")
        payload = bytes((tag,))
    elif tag < 0xF8:
        payload = _read_exact(fd, tag - 0xC0)
    else:
        width = tag - 0xF8
        if width == 0:
            raise ProviderProtocolError("zero-width long atom length")
        encoded_length = _read_exact(fd, width)
        if encoded_length[-1] == 0:
            raise ProviderProtocolError("non-minimal long atom length")
        length = int.from_bytes(encoded_length, "little")
        if length <= 55:
            raise ProviderProtocolError("non-canonical long atom length")
        if length > maximum:
            raise ProviderProtocolError("provider atom exceeds its size bound")
        return _read_exact(fd, length)
    if len(payload) > maximum:
        raise ProviderProtocolError("provider atom exceeds its size bound")
    return payload


def _take_atom(payload: memoryview, maximum: int) -> tuple[bytes, memoryview]:
    if not payload:
        raise ProviderProtocolError("provider start has too few fields")
    tag = payload[0]
    if tag < 0xC0:
        return bytes((tag,)), payload[1:]
    if tag < 0xF8:
        prefix = 1
        length = tag - 0xC0
    else:
        width = tag - 0xF8
        if width == 0 or len(payload) < 1 + width:
            raise ProviderProtocolError("malformed provider field length")
        encoded_length = bytes(payload[1 : 1 + width])
        if encoded_length[-1] == 0:
            raise ProviderProtocolError("non-minimal provider field length")
        length = int.from_bytes(encoded_length, "little")
        if length <= 55:
            raise ProviderProtocolError("non-canonical provider field length")
        prefix = 1 + width
    if length > maximum:
        raise ProviderProtocolError("provider field exceeds its size bound")
    end = prefix + length
    if len(payload) < end:
        raise ProviderProtocolError("truncated provider start field")
    return bytes(payload[prefix:end]), payload[end:]


def _uint(payload: bytes, maximum: int) -> int:
    if len(payload) > maximum or (payload and payload[-1] == 0):
        raise ProviderProtocolError("non-canonical provider integer")
    return int.from_bytes(payload, "little")


def _safe_relative(path: bytes) -> bool:
    return bool(path) and not (
        b"\0" in path
        or b"\\" in path
        or path.startswith(b"/")
        or any(part in {b"", b".", b".."} for part in path.split(b"/"))
    )


def _safe_guest_absolute(path: bytes) -> bool:
    return path.startswith(b"/") and _safe_relative(path[1:])


def _safe_kernel_init(path: bytes) -> bool:
    return _safe_guest_absolute(path) and not any(
        byte <= 0x20 or byte == 0x7F for byte in path
    )


def _decode_executable(payload: bytes) -> DebugExecutable:
    fields = memoryview(payload)
    guest_path, fields = _take_atom(fields, MAX_PATH_BYTES)
    build_id, fields = _take_atom(fields, MAX_SERVICE_BYTES)
    runtime_sha256, fields = _take_atom(fields, 32)
    runtime_size_raw, fields = _take_atom(fields, 8)
    debug_elf, fields = _take_atom(fields, MAX_PATH_BYTES)
    debug_sha256, fields = _take_atom(fields, 32)
    debug_size_raw, fields = _take_atom(fields, 8)
    elf_class_raw, fields = _take_atom(fields, 2)
    elf_machine_raw, fields = _take_atom(fields, 2)
    source_view_raw, fields = _take_atom(fields, 8)
    if fields:
        raise ProviderProtocolError("debug executable has trailing fields")
    runtime_size = _uint(runtime_size_raw, 8)
    debug_size = _uint(debug_size_raw, 8)
    elf_class = _uint(elf_class_raw, 2)
    elf_machine = _uint(elf_machine_raw, 2)
    source_view = _uint(source_view_raw, 8)
    if not _safe_guest_absolute(guest_path):
        raise ProviderProtocolError("debug executable guest path is invalid")
    if not re.fullmatch(rb"[0-9a-f]{8,128}", build_id) or len(build_id) % 2:
        raise ProviderProtocolError(
            "debug executable association identity is invalid"
        )
    if len(runtime_sha256) != 32 or not runtime_size:
        raise ProviderProtocolError("debug executable runtime identity is invalid")
    if not _safe_relative(debug_elf):
        raise ProviderProtocolError("debug executable ELF path is invalid")
    if len(debug_sha256) != 32 or not debug_size:
        raise ProviderProtocolError("debug executable debug-file identity is invalid")
    if elf_class not in {32, 64} or not elf_machine or source_view != 1:
        raise ProviderProtocolError("debug executable identity is invalid")
    return DebugExecutable(
        guest_path,
        build_id,
        runtime_sha256,
        runtime_size,
        debug_elf,
        debug_sha256,
        debug_size,
        elf_class,
        elf_machine,
    )


def _decode_image_option(payload: bytes) -> DebugImageCatalog | None:
    fields = memoryview(payload)
    present_raw, fields = _take_atom(fields, 8)
    present = _uint(present_raw, 8)
    if present == 0:
        if fields:
            raise ProviderProtocolError("empty image option has trailing fields")
        return None
    if present != 1:
        raise ProviderProtocolError("invalid image option tag")
    catalog_payload, fields = _take_atom(fields, MAX_FRAME_BYTES)
    if fields:
        raise ProviderProtocolError("image option has trailing fields")
    catalog = memoryview(catalog_payload)
    manifest, catalog = _take_atom(catalog, MAX_PATH_BYTES)
    profile_raw, catalog = _take_atom(catalog, 8)
    init, catalog = _take_atom(catalog, MAX_PATH_BYTES)
    executable_list, catalog = _take_atom(catalog, MAX_FRAME_BYTES)
    if catalog:
        raise ProviderProtocolError("image catalog has trailing fields")
    profile = _uint(profile_raw, 8)
    if not _safe_relative(manifest) or profile not in {1, 2, 3, 4} or not _safe_kernel_init(init):
        raise ProviderProtocolError("image catalog boot identity is invalid")
    items = memoryview(executable_list)
    count_raw, items = _take_atom(items, 8)
    count = _uint(count_raw, 8)
    executables = []
    for _ in range(count):
        executable, items = _take_atom(items, MAX_FRAME_BYTES)
        executables.append(_decode_executable(executable))
    if items:
        raise ProviderProtocolError("executable catalog has trailing fields")
    return DebugImageCatalog(manifest, profile, init, tuple(executables))


def _read_provider_start_after_version(fd: int) -> DebugProviderStart:
    """Read the v1 start body after its version atom was consumed."""

    fields = memoryview(_read_atom(fd, MAX_FRAME_BYTES, compound=True))
    manifest, fields = _take_atom(fields, MAX_PATH_BYTES)
    service_bytes, fields = _take_atom(fields, MAX_SERVICE_BYTES)
    image_option, fields = _take_atom(fields, MAX_FRAME_BYTES)
    if fields:
        raise ProviderProtocolError("provider start has trailing fields")
    if not _safe_relative(manifest):
        raise ProviderProtocolError("provider manifest path is invalid")
    try:
        service = service_bytes.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ProviderProtocolError("provider service name is not UTF-8") from exc
    if not service or "\0" in service:
        raise ProviderProtocolError("provider service name is invalid")
    return DebugProviderStart(manifest, service, _decode_image_option(image_option))


def read_provider_start(fd: int) -> DebugProviderStart:
    """Read exactly the typed v1 start frame, leaving following RSP bytes unread."""

    version = _read_atom(fd, 8)
    if _uint(version, 8) != PROTOCOL_VERSION:
        raise ProviderProtocolError(
            f"unsupported provider protocol version; expected {PROTOCOL_VERSION}"
        )
    return _read_provider_start_after_version(fd)


class PipeDuplex:
    """Adapt a relay's stdout/stdin pair to the RSP duplex-stream protocol."""

    def __init__(self, reader: BinaryIO, writer: BinaryIO) -> None:
        self.reader = reader
        self.writer = writer

    def fileno(self) -> int:
        return self.reader.fileno()

    def recv(self, size: int) -> bytes:
        return os.read(self.reader.fileno(), size)

    def sendall(self, data: bytes) -> None:
        view = memoryview(data)
        while view:
            written = os.write(self.writer.fileno(), view)
            if written == 0:
                raise BrokenPipeError("Sarun service relay stopped accepting bytes")
            view = view[written:]

    def close(self) -> None:
        for stream in (self.reader, self.writer):
            try:
                stream.close()
            except OSError:
                pass


class SarunServiceAccept:
    """One generic Sarun service accept slot used as the GDB-facing stream."""

    def __init__(self, service: str) -> None:
        self.process = subprocess.Popen(
            ["sarun", "service", "accept", service],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
            close_fds=True,
        )
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        self.stream = PipeDuplex(self.process.stdout, self.process.stdin)

    def close(self) -> None:
        self.stream.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


FacadeBuilder = Callable[..., LiveFacade]
ServiceFactory = Callable[[str], SarunServiceAccept]
PreRspPrepare = Callable[[object], object]


def run_provider(
    stdin_fd: int = 0,
    *,
    facade_builder: FacadeBuilder = build_live_facade,
    service_factory: ServiceFactory = SarunServiceAccept,
    pre_rsp_prepare: PreRspPrepare | None = None,
    handshake_write_fd: int | None = None,
) -> None:
    version = _uint(_read_atom(stdin_fd, 8), 8)
    if version == PROTOCOL_VERSION:
        start = _read_provider_start_after_version(stdin_fd)
    elif version == 2:
        from .provider_handshake import (
            SelectedBundleError,
            serve_pre_rsp_handshake_after_version,
        )

        def unavailable(_execution):
            raise SelectedBundleError(
                "selected-boot preparation is unavailable in this provider composition"
            )

        if not serve_pre_rsp_handshake_after_version(
            stdin_fd,
            pre_rsp_prepare or unavailable,
            write_fd=handshake_write_fd,
        ):
            return
        start = read_provider_start(stdin_fd)
    else:
        raise ProviderProtocolError(
            "unsupported provider protocol version; expected 1 or 2"
        )
    inherited_fd = os.dup(stdin_fd)
    qemu_stream = socket.socket(fileno=inherited_fd)
    try:
        live = facade_builder(
            qemu_socket=None,
            gdb_socket=None,
            manifest_path=os.path.join("/", os.fsdecode(start.manifest)),
            qemu_stream=qemu_stream,
        )
    except BaseException:
        qemu_stream.close()
        raise

    service: SarunServiceAccept | None = None
    try:
        service = service_factory(start.service)
        live.server.serve_connection(service.stream)
    finally:
        if service is not None:
            service.close()
        live.close()


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    if arguments:
        print("viros-facade: this provider accepts no arguments", file=sys.stderr)
        return 2
    try:
        run_provider()
    except Exception as exc:
        print(f"viros-facade: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
