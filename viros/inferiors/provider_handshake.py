"""Typed preparation handshake before a Sarun provider becomes an RSP stream.

Protocol version 1 remains the existing :class:`DebugProviderStart` message.
Version 2 is an optional preparation exchange on the same full-duplex socket::

    runner   -> Prepare(selected boot, finite catalog, tool bindings)
    provider -> Prepared(token, exact future resource identities) | Rejected
    runner   -> Commit(token) | Abort(token)
    provider -> Committed(token) | Aborted(token)
    runner   -> the unchanged version-1 DebugProviderStart, then raw QEMU RSP

Selections contain only captured box/file identities.  Files are opened by an
injected ``ArtifactSource`` backed by Sarun's read-only attachments; host paths,
environment variables and QEMU arguments are deliberately absent from the
wire.  A successful preparation is staged until the matching token is
committed, so RSP can never observe a half-published debugger bundle.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import os
from pathlib import Path, PurePosixPath
import secrets
import shutil
import tempfile
from typing import Callable, Protocol

from probe.image_inspector import CapturedArtifact, ImageInspectionError
from probe.provider_derivation import (
    ArtifactSource,
    ProviderDerivationError,
    SelectedImageRequest,
    SelectedKernelInitramfsRequest,
)
from probe.selected_bundle_orchestrator import (
    CatalogExecutable,
    FixedBootProfile,
    SelectedBundleError,
    SelectedBundleExecutionRequest,
    execute_selected_initramfs,
)


PROTOCOL_VERSION = 2
MAX_FRAME_BYTES = 16 * 1024 * 1024
MAX_PATH_BYTES = 1024 * 1024
MAX_TEXT_BYTES = 64 * 1024
MAX_CATALOG_ARTIFACTS = 100_000
MAX_ROLES = 64
MAX_EXECUTABLES = 4
TOKEN_BYTES = 32

MESSAGE_PREPARE = 1
MESSAGE_COMMIT = 2
MESSAGE_ABORT = 3

OUTCOME_PREPARED = 1
OUTCOME_REJECTED = 2
OUTCOME_COMMITTED = 3
OUTCOME_ABORTED = 4

SELECTION_IMAGE = 1
SELECTION_LINUX = 2

PROFILE_AARCH64 = 1
PROFILE_X86_64 = 2
PROFILE_ARM = 3
PROFILE_MMIPS = 4

_PROFILE_FROM_TAG = {
    PROFILE_AARCH64: FixedBootProfile.AARCH64,
    PROFILE_X86_64: FixedBootProfile.X86_64,
    PROFILE_ARM: FixedBootProfile.ARM,
    PROFILE_MMIPS: FixedBootProfile.MMIPS,
}
_PROFILE_TO_TAG = {profile: tag for tag, profile in _PROFILE_FROM_TAG.items()}


class HandshakeProtocolError(ValueError):
    """The peer sent a non-canonical, out-of-bounds, or invalid transition."""


@dataclass(frozen=True)
class ResourceIdentity:
    path: str
    size: int
    sha256: bytes

    def __post_init__(self) -> None:
        if not _safe_relative_text(self.path):
            raise HandshakeProtocolError("prepared resource path is invalid")
        if isinstance(self.size, bool) or not isinstance(self.size, int) or self.size < 0:
            raise HandshakeProtocolError("prepared resource size is invalid")
        if not isinstance(self.sha256, bytes) or len(self.sha256) != 32:
            raise HandshakeProtocolError("prepared resource digest is invalid")


@dataclass(frozen=True)
class PreparedBoot:
    token: bytes
    kernel_manifest: ResourceIdentity
    image_manifest: ResourceIdentity
    kernel: ResourceIdentity
    initramfs: ResourceIdentity
    kernel_init: str

    def __post_init__(self) -> None:
        if not isinstance(self.token, bytes) or len(self.token) != TOKEN_BYTES:
            raise HandshakeProtocolError("preparation token is invalid")
        if not _safe_guest_init(self.kernel_init):
            raise HandshakeProtocolError("prepared kernel init is invalid")


@dataclass(frozen=True)
class Rejected:
    code: str
    detail: str

    def __post_init__(self) -> None:
        if not _safe_code(self.code):
            raise HandshakeProtocolError("rejection code is invalid")
        if not self.detail or "\0" in self.detail or len(self.detail.encode()) > MAX_TEXT_BYTES:
            raise HandshakeProtocolError("rejection detail is invalid")


@dataclass(frozen=True)
class PrepareRequest:
    execution: SelectedBundleExecutionRequest


class PreparationTransaction(Protocol):
    @property
    def prepared(self) -> PreparedBoot: ...

    def commit(self) -> None: ...

    def abort(self) -> None: ...


Prepare = Callable[[SelectedBundleExecutionRequest], PreparationTransaction]


def _read_exact(fd: int, length: int) -> bytes:
    result = bytearray()
    while len(result) < length:
        chunk = os.read(fd, length - len(result))
        if not chunk:
            raise HandshakeProtocolError("truncated provider handshake")
        result.extend(chunk)
    return bytes(result)


def _read_atom(fd: int, maximum: int, *, compound: bool = False) -> bytes:
    tag = _read_exact(fd, 1)[0]
    if tag < 0xC0:
        if compound:
            raise HandshakeProtocolError("expected a compound handshake atom")
        payload = bytes((tag,))
    elif tag < 0xF8:
        payload = _read_exact(fd, tag - 0xC0)
    else:
        width = tag - 0xF8
        if width == 0:
            raise HandshakeProtocolError("zero-width long atom length")
        encoded = _read_exact(fd, width)
        if encoded[-1] == 0:
            raise HandshakeProtocolError("non-minimal long atom length")
        length = int.from_bytes(encoded, "little")
        if length <= 55:
            raise HandshakeProtocolError("non-canonical long atom length")
        if length > maximum:
            raise HandshakeProtocolError("handshake atom exceeds its size bound")
        return _read_exact(fd, length)
    if len(payload) > maximum:
        raise HandshakeProtocolError("handshake atom exceeds its size bound")
    return payload


def _take_atom(payload: memoryview, maximum: int) -> tuple[bytes, memoryview]:
    if not payload:
        raise HandshakeProtocolError("handshake record has too few fields")
    tag = payload[0]
    if tag < 0xC0:
        return bytes((tag,)), payload[1:]
    if tag < 0xF8:
        prefix = 1
        length = tag - 0xC0
    else:
        width = tag - 0xF8
        if width == 0 or len(payload) < 1 + width:
            raise HandshakeProtocolError("malformed handshake field length")
        encoded = bytes(payload[1 : 1 + width])
        if encoded[-1] == 0:
            raise HandshakeProtocolError("non-minimal handshake field length")
        length = int.from_bytes(encoded, "little")
        if length <= 55:
            raise HandshakeProtocolError("non-canonical handshake field length")
        prefix = 1 + width
    if length > maximum:
        raise HandshakeProtocolError("handshake field exceeds its size bound")
    end = prefix + length
    if len(payload) < end:
        raise HandshakeProtocolError("truncated handshake field")
    return bytes(payload[prefix:end]), payload[end:]


def _atom(payload: bytes, *, compound: bool = False) -> bytes:
    if len(payload) == 1 and payload[0] < 0xC0 and not compound:
        return payload
    if len(payload) <= 55:
        return bytes((0xC0 + len(payload),)) + payload
    width = (len(payload).bit_length() + 7) // 8
    return bytes((0xF8 + width,)) + len(payload).to_bytes(width, "little") + payload


def _uint_atom(value: int) -> bytes:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise HandshakeProtocolError("cannot encode a negative handshake integer")
    width = (value.bit_length() + 7) // 8
    return _atom(value.to_bytes(width, "little"))


def _uint(payload: bytes, maximum: int = 8) -> int:
    if len(payload) > maximum or (payload and payload[-1] == 0):
        raise HandshakeProtocolError("non-canonical handshake integer")
    return int.from_bytes(payload, "little")


def _text(payload: bytes, label: str, maximum: int = MAX_TEXT_BYTES) -> str:
    if len(payload) > maximum or b"\0" in payload:
        raise HandshakeProtocolError(f"{label} is invalid")
    try:
        return payload.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise HandshakeProtocolError(f"{label} is not UTF-8") from exc


def _safe_relative_text(path: str) -> bool:
    return bool(path) and not (
        "\0" in path
        or "\\" in path
        or path.startswith("/")
        or any(part in {"", ".", ".."} for part in path.split("/"))
    )


def _safe_guest_init(path: str) -> bool:
    return (
        path.startswith("/")
        and _safe_relative_text(path[1:])
        and not any(byte <= 0x20 or byte == 0x7F for byte in path.encode())
    )


def _safe_code(value: str) -> bool:
    return bool(value) and len(value) <= 64 and all(
        character.isascii() and (character.islower() or character.isdigit() or character == "-")
        for character in value
    )


def _bounded_detail(error: BaseException) -> str:
    detail = str(error).replace("\0", "�") or error.__class__.__name__
    encoded = detail.encode("utf-8")
    if len(encoded) <= MAX_TEXT_BYTES:
        return detail
    suffix = b"..."
    shortened = encoded[: MAX_TEXT_BYTES - len(suffix)]
    while shortened:
        try:
            return shortened.decode("utf-8") + suffix.decode()
        except UnicodeDecodeError:
            shortened = shortened[:-1]
    return "preparation failed"


def _take_record(payload: bytes, count: int) -> list[bytes]:
    fields = memoryview(payload)
    result = []
    for _ in range(count):
        value, fields = _take_atom(fields, MAX_FRAME_BYTES)
        result.append(value)
    if fields:
        raise HandshakeProtocolError("handshake record has trailing fields")
    return result


def _decode_string_list(payload: bytes, maximum_count: int) -> tuple[str, ...]:
    fields = memoryview(payload)
    count_raw, fields = _take_atom(fields, 8)
    count = _uint(count_raw)
    if count > maximum_count:
        raise HandshakeProtocolError("handshake list exceeds its item bound")
    result = []
    for _ in range(count):
        item, fields = _take_atom(fields, MAX_TEXT_BYTES)
        result.append(_text(item, "handshake list item"))
    if fields:
        raise HandshakeProtocolError("handshake list has trailing fields")
    return tuple(result)


def _decode_artifact(payload: bytes) -> CapturedArtifact:
    box_id, path, size, sha256, record_id, roles, architecture = _take_record(payload, 7)
    if len(sha256) != 32:
        raise HandshakeProtocolError("captured artifact digest is not SHA-256")
    architecture_fields = memoryview(architecture)
    present_raw, architecture_fields = _take_atom(architecture_fields, 8)
    present = _uint(present_raw)
    if present == 0:
        architecture_text = None
    elif present == 1:
        value, architecture_fields = _take_atom(architecture_fields, MAX_TEXT_BYTES)
        architecture_text = _text(value, "artifact architecture")
    else:
        raise HandshakeProtocolError("invalid artifact architecture option")
    if architecture_fields:
        raise HandshakeProtocolError("artifact architecture option has trailing fields")
    mapping = {
        "box_id": _uint(box_id),
        "path": _text(path, "artifact path", MAX_PATH_BYTES),
        "size": _uint(size),
        "sha256": sha256.hex(),
        "record_id": _text(record_id, "artifact record identity"),
        "roles": list(_decode_string_list(roles, MAX_ROLES)),
    }
    if architecture_text is not None:
        mapping["architecture"] = architecture_text
    try:
        return CapturedArtifact.from_mapping(mapping)
    except ImageInspectionError as exc:
        raise HandshakeProtocolError(str(exc)) from exc


def _decode_artifact_list(payload: bytes) -> tuple[CapturedArtifact, ...]:
    fields = memoryview(payload)
    count_raw, fields = _take_atom(fields, 8)
    count = _uint(count_raw)
    if count > MAX_CATALOG_ARTIFACTS:
        raise HandshakeProtocolError("captured artifact catalog is too large")
    result = []
    for _ in range(count):
        artifact, fields = _take_atom(fields, MAX_FRAME_BYTES)
        result.append(_decode_artifact(artifact))
    if fields:
        raise HandshakeProtocolError("artifact catalog has trailing fields")
    return tuple(result)


def _encode_string_list(values: tuple[str, ...]) -> bytes:
    return _atom(
        _uint_atom(len(values)) + b"".join(_atom(value.encode()) for value in values),
        compound=True,
    )


def _encode_artifact(row: CapturedArtifact) -> bytes:
    architecture = _uint_atom(0)
    if row.architecture is not None:
        architecture = _uint_atom(1) + _atom(row.architecture.encode())
    return _atom(
        _uint_atom(row.box_id)
        + _atom(row.path.encode())
        + _uint_atom(row.size)
        + _atom(bytes.fromhex(row.sha256))
        + _atom(row.record_id.encode())
        + _encode_string_list(row.roles)
        + _atom(architecture, compound=True),
        compound=True,
    )


def _encode_artifact_list(rows: tuple[CapturedArtifact, ...]) -> bytes:
    return _atom(
        _uint_atom(len(rows)) + b"".join(_encode_artifact(row) for row in rows),
        compound=True,
    )


def _decode_selection(payload: bytes, catalog: tuple[CapturedArtifact, ...]):
    fields = memoryview(payload)
    tag_raw, fields = _take_atom(fields, 8)
    tag = _uint(tag_raw)
    if tag == SELECTION_IMAGE:
        selected, fields = _take_atom(fields, MAX_FRAME_BYTES)
        result = SelectedImageRequest(_decode_artifact(selected), catalog)
    elif tag == SELECTION_LINUX:
        kernel, fields = _take_atom(fields, MAX_FRAME_BYTES)
        initramfs, fields = _take_atom(fields, MAX_FRAME_BYTES)
        result = SelectedKernelInitramfsRequest(
            _decode_artifact(kernel), _decode_artifact(initramfs), catalog
        )
    else:
        raise HandshakeProtocolError("unknown selected boot case")
    if fields:
        raise HandshakeProtocolError("selected boot has trailing fields")
    return result


def _decode_executables(
    payload: bytes, catalog: tuple[CapturedArtifact, ...]
) -> tuple[CatalogExecutable, ...]:
    fields = memoryview(payload)
    count_raw, fields = _take_atom(fields, 8)
    count = _uint(count_raw)
    if count > MAX_EXECUTABLES:
        raise HandshakeProtocolError("catalog executable list is too large")
    identities = {
        (row.box_id, row.path, row.record_id, row.size, row.sha256): row for row in catalog
    }
    result = []
    for _ in range(count):
        item, fields = _take_atom(fields, MAX_FRAME_BYTES)
        label, argv0, artifact, mode = _take_record(item, 4)
        decoded_artifact = _decode_artifact(artifact)
        key = (
            decoded_artifact.box_id,
            decoded_artifact.path,
            decoded_artifact.record_id,
            decoded_artifact.size,
            decoded_artifact.sha256,
        )
        if identities.get(key) != decoded_artifact:
            raise HandshakeProtocolError(
                "catalog executable is outside the finite provenance catalog"
            )
        try:
            result.append(
                CatalogExecutable(
                    label=_text(label, "catalog executable label"),
                    argv0=_text(argv0, "catalog executable argv0", MAX_PATH_BYTES),
                    artifact=decoded_artifact,
                    mode=_uint(mode, 2),
                )
            )
        except SelectedBundleError as exc:
            raise HandshakeProtocolError(str(exc)) from exc
    if fields:
        raise HandshakeProtocolError("catalog executable list has trailing fields")
    return tuple(result)


def decode_prepare_payload(payload: bytes) -> PrepareRequest:
    """Decode the fields of a version-2 ``Prepare`` message."""

    tag, profile, selection, catalog, executables = _take_record(payload, 5)
    if _uint(tag) != MESSAGE_PREPARE:
        raise HandshakeProtocolError("first handshake message is not Prepare")
    catalog_rows = _decode_artifact_list(catalog)
    selected = _decode_selection(selection, catalog_rows)
    tools = _decode_executables(executables, catalog_rows)
    try:
        fixed_profile = _PROFILE_FROM_TAG[_uint(profile)]
    except KeyError as exc:
        raise HandshakeProtocolError("unknown fixed QEMU boot profile") from exc
    try:
        return PrepareRequest(
            SelectedBundleExecutionRequest(selected, tools, fixed_profile)
        )
    except (ProviderDerivationError, SelectedBundleError) as exc:
        raise HandshakeProtocolError(str(exc)) from exc


def encode_prepare(execution: SelectedBundleExecutionRequest) -> bytes:
    """Reference encoder for the exact version-2 request Rust must mirror."""

    selected = execution.selected_boot
    if execution.fixed_profile is None:
        raise HandshakeProtocolError("preparation request requires a fixed QEMU boot profile")
    try:
        profile_tag = _PROFILE_TO_TAG[execution.fixed_profile]
    except KeyError as exc:
        raise HandshakeProtocolError("unknown fixed QEMU boot profile") from exc
    catalog = selected.captured_artifacts
    if len(catalog) > MAX_CATALOG_ARTIFACTS:
        raise HandshakeProtocolError("captured artifact catalog is too large")
    if isinstance(selected, SelectedImageRequest):
        selection = _uint_atom(SELECTION_IMAGE) + _encode_artifact(selected.selected)
    elif isinstance(selected, SelectedKernelInitramfsRequest):
        selection = (
            _uint_atom(SELECTION_LINUX)
            + _encode_artifact(selected.kernel)
            + _encode_artifact(selected.initramfs)
        )
    else:
        raise HandshakeProtocolError("unknown typed boot selection")
    executable_items = []
    for row in execution.executables:
        executable_items.append(
            _atom(
                _atom(row.label.encode())
                + _atom(row.argv0.encode())
                + _encode_artifact(row.artifact)
                + _uint_atom(row.mode),
                compound=True,
            )
        )
    executables = _atom(
        _uint_atom(len(executable_items)) + b"".join(executable_items), compound=True
    )
    fields = (
        _uint_atom(MESSAGE_PREPARE)
        + _uint_atom(profile_tag)
        + _atom(selection, compound=True)
        + _encode_artifact_list(catalog)
        + executables
    )
    frame = _uint_atom(PROTOCOL_VERSION) + _atom(fields, compound=True)
    if len(frame) > MAX_FRAME_BYTES:
        raise HandshakeProtocolError("preparation request exceeds its frame bound")
    return frame


def read_prepare(fd: int) -> PrepareRequest:
    if _uint(_read_atom(fd, 8)) != PROTOCOL_VERSION:
        raise HandshakeProtocolError("unsupported preparation protocol version")
    return read_prepare_after_version(fd)


def read_prepare_after_version(fd: int) -> PrepareRequest:
    """Read ``Prepare`` after a dispatcher already consumed version 2."""

    return decode_prepare_payload(_read_atom(fd, MAX_FRAME_BYTES, compound=True))


def _encode_resource(resource: ResourceIdentity) -> bytes:
    return _atom(
        _atom(resource.path.encode())
        + _uint_atom(resource.size)
        + _atom(resource.sha256),
        compound=True,
    )


def encode_outcome(outcome: PreparedBoot | Rejected | tuple[str, bytes]) -> bytes:
    """Encode one provider outcome as a versioned canonical frame.

    ``("committed", token)`` and ``("aborted", token)`` are terminal outcomes.
    """

    if isinstance(outcome, PreparedBoot):
        fields = (
            _uint_atom(OUTCOME_PREPARED)
            + _atom(outcome.token)
            + _encode_resource(outcome.kernel_manifest)
            + _encode_resource(outcome.image_manifest)
            + _encode_resource(outcome.kernel)
            + _encode_resource(outcome.initramfs)
            + _atom(outcome.kernel_init.encode())
        )
    elif isinstance(outcome, Rejected):
        fields = (
            _uint_atom(OUTCOME_REJECTED)
            + _atom(outcome.code.encode())
            + _atom(outcome.detail.encode())
        )
    else:
        state, token = outcome
        tag = {"committed": OUTCOME_COMMITTED, "aborted": OUTCOME_ABORTED}.get(state)
        if tag is None or not isinstance(token, bytes) or len(token) != TOKEN_BYTES:
            raise HandshakeProtocolError("terminal provider outcome is invalid")
        fields = _uint_atom(tag) + _atom(token)
    frame = _uint_atom(PROTOCOL_VERSION) + _atom(fields, compound=True)
    if len(frame) > MAX_FRAME_BYTES:
        raise HandshakeProtocolError("provider outcome exceeds its frame bound")
    return frame


def _decode_resource(payload: bytes) -> ResourceIdentity:
    path, size, sha256 = _take_record(payload, 3)
    return ResourceIdentity(
        _text(path, "prepared resource path", MAX_PATH_BYTES),
        _uint(size),
        sha256,
    )


def read_outcome(fd: int) -> PreparedBoot | Rejected | tuple[str, bytes]:
    """Reference decoder for provider outcomes and terminal acknowledgements."""

    if _uint(_read_atom(fd, 8)) != PROTOCOL_VERSION:
        raise HandshakeProtocolError("unsupported outcome protocol version")
    fields = memoryview(_read_atom(fd, MAX_FRAME_BYTES, compound=True))
    tag_raw, fields = _take_atom(fields, 8)
    tag = _uint(tag_raw)
    if tag == OUTCOME_PREPARED:
        token, fields = _take_atom(fields, TOKEN_BYTES)
        resources = []
        for _ in range(4):
            resource, fields = _take_atom(fields, MAX_FRAME_BYTES)
            resources.append(_decode_resource(resource))
        init, fields = _take_atom(fields, MAX_PATH_BYTES)
        result: PreparedBoot | Rejected | tuple[str, bytes] = PreparedBoot(
            token,
            resources[0],
            resources[1],
            resources[2],
            resources[3],
            _text(init, "prepared kernel init", MAX_PATH_BYTES),
        )
    elif tag == OUTCOME_REJECTED:
        code, fields = _take_atom(fields, 64)
        detail, fields = _take_atom(fields, MAX_TEXT_BYTES)
        result = Rejected(
            _text(code, "rejection code", 64),
            _text(detail, "rejection detail"),
        )
    elif tag in {OUTCOME_COMMITTED, OUTCOME_ABORTED}:
        token, fields = _take_atom(fields, TOKEN_BYTES)
        if len(token) != TOKEN_BYTES:
            raise HandshakeProtocolError("terminal outcome token is invalid")
        result = ("committed" if tag == OUTCOME_COMMITTED else "aborted", token)
    else:
        raise HandshakeProtocolError("unknown provider outcome case")
    if fields:
        raise HandshakeProtocolError("provider outcome has trailing fields")
    return result


def _write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        if written <= 0:
            raise BrokenPipeError("provider handshake peer stopped accepting bytes")
        view = view[written:]


def _read_decision(fd: int, token: bytes) -> str:
    if _uint(_read_atom(fd, 8)) != PROTOCOL_VERSION:
        raise HandshakeProtocolError("unsupported decision protocol version")
    fields = memoryview(_read_atom(fd, MAX_FRAME_BYTES, compound=True))
    tag_raw, fields = _take_atom(fields, 8)
    received_token, fields = _take_atom(fields, TOKEN_BYTES)
    if fields:
        raise HandshakeProtocolError("preparation decision has trailing fields")
    if received_token != token:
        raise HandshakeProtocolError("preparation decision token does not match")
    tag = _uint(tag_raw)
    if tag == MESSAGE_COMMIT:
        return "commit"
    if tag == MESSAGE_ABORT:
        return "abort"
    raise HandshakeProtocolError("expected Commit or Abort after Prepared")


def encode_decision(commit: bool, token: bytes) -> bytes:
    if not isinstance(token, bytes) or len(token) != TOKEN_BYTES:
        raise HandshakeProtocolError("preparation decision token is invalid")
    fields = _uint_atom(MESSAGE_COMMIT if commit else MESSAGE_ABORT) + _atom(token)
    return _uint_atom(PROTOCOL_VERSION) + _atom(fields, compound=True)


def serve_pre_rsp_handshake(
    fd: int, prepare: Prepare, *, write_fd: int | None = None
) -> bool:
    """Run one complete v2 preparation transaction.

    Return ``True`` only after a matching commit has been published.  A normal
    caller then reads the unchanged version-1 ``DebugProviderStart`` frame from
    the same descriptor and finally treats all following bytes as raw RSP.
    """

    if _uint(_read_atom(fd, 8)) != PROTOCOL_VERSION:
        raise HandshakeProtocolError("unsupported preparation protocol version")
    return serve_pre_rsp_handshake_after_version(fd, prepare, write_fd=write_fd)


def serve_pre_rsp_handshake_after_version(
    fd: int, prepare: Prepare, *, write_fd: int | None = None
) -> bool:
    """Run the exchange after a dispatcher already consumed version 2."""

    response_fd = fd if write_fd is None else write_fd

    try:
        request = read_prepare_after_version(fd)
        transaction = prepare(request.execution)
    except (HandshakeProtocolError, ProviderDerivationError, SelectedBundleError, OSError) as exc:
        _write_all(
            response_fd,
            encode_outcome(Rejected("preparation-failed", _bounded_detail(exc))),
        )
        return False
    prepared = transaction.prepared
    _write_all(response_fd, encode_outcome(prepared))
    try:
        decision = _read_decision(fd, prepared.token)
        if decision == "abort":
            transaction.abort()
            _write_all(response_fd, encode_outcome(("aborted", prepared.token)))
            return False
        transaction.commit()
        _write_all(response_fd, encode_outcome(("committed", prepared.token)))
        return True
    except BaseException:
        transaction.abort()
        raise


def _file_identity(root: Path, relative: str) -> ResourceIdentity:
    if not _safe_relative_text(relative):
        raise SelectedBundleError("internal bundle resource path is invalid")
    path = root.joinpath(*relative.split("/"))
    try:
        contents = path.read_bytes()
    except OSError as exc:
        raise SelectedBundleError(f"internal bundle resource is unavailable: {relative}") from exc
    return ResourceIdentity(relative, len(contents), hashlib.sha256(contents).digest())


class SelectedBundleTransaction:
    """Staged adapter around ``execute_selected_initramfs``.

    ``provider_root`` and ``destination`` are construction-time integration
    resources, never peer-controlled fields.  Every path reported on the wire
    is relative to ``provider_root`` and names the future committed location.
    """

    def __init__(
        self,
        execution: SelectedBundleExecutionRequest,
        source: ArtifactSource,
        provider_root: Path,
        destination: str,
        *,
        executor=execute_selected_initramfs,
        token_factory: Callable[[], bytes] = lambda: secrets.token_bytes(TOKEN_BYTES),
    ) -> None:
        if not _safe_relative_text(destination):
            raise SelectedBundleError("internal bundle destination is invalid")
        self._provider_root = Path(provider_root).resolve(strict=True)
        if not self._provider_root.is_dir():
            raise SelectedBundleError("provider resource root is not a directory")
        self._destination = self._provider_root.joinpath(*destination.split("/"))
        try:
            self._destination.resolve(strict=False).relative_to(self._provider_root)
        except ValueError as exc:
            raise SelectedBundleError("internal bundle destination leaves provider root") from exc
        if self._destination.exists():
            raise SelectedBundleError("internal bundle destination already exists")
        self._transaction_root = Path(
            tempfile.mkdtemp(prefix=".viros-prepare.", dir=self._provider_root)
        )
        self._staged = self._transaction_root / "bundle"
        self._finished = False
        try:
            result = executor(execution, source, self._staged)
            if Path(result.output_root).resolve() != self._staged.resolve():
                raise SelectedBundleError("bundle executor returned an unexpected output")
            init = result.plan.kernel_init
            prefix = destination.rstrip("/")
            token = token_factory()
            self._prepared = PreparedBoot(
                token=token,
                kernel_manifest=_file_identity(self._staged, "kernel-bundle/bundle.json"),
                image_manifest=_file_identity(self._staged, "image-bundle/image.json"),
                kernel=_file_identity(self._staged, "kernel-bundle/kernel"),
                initramfs=_file_identity(self._staged, "image-bundle/rootfs.cpio"),
                kernel_init=init,
            )
            self._prepared = PreparedBoot(
                self._prepared.token,
                *(
                    ResourceIdentity(f"{prefix}/{row.path}", row.size, row.sha256)
                    for row in (
                        self._prepared.kernel_manifest,
                        self._prepared.image_manifest,
                        self._prepared.kernel,
                        self._prepared.initramfs,
                    )
                ),
                self._prepared.kernel_init,
            )
        except BaseException:
            shutil.rmtree(self._transaction_root, ignore_errors=True)
            raise

    @property
    def prepared(self) -> PreparedBoot:
        return self._prepared

    def commit(self) -> None:
        if self._finished:
            raise HandshakeProtocolError("preparation transaction is already finished")
        self._destination.parent.mkdir(parents=True, exist_ok=True)
        os.replace(self._staged, self._destination)
        shutil.rmtree(self._transaction_root, ignore_errors=True)
        self._finished = True

    def abort(self) -> None:
        if not self._finished:
            shutil.rmtree(self._transaction_root, ignore_errors=True)
            self._finished = True


def selected_bundle_preparer(
    source: ArtifactSource, provider_root: Path, destination: str
) -> Prepare:
    """Bind provider-owned resources to the typed handshake callback."""

    return lambda execution: SelectedBundleTransaction(
        execution, source, provider_root, destination
    )
