#!/usr/bin/env python3
"""Provider-side derivation of exact selected boot payloads.

This module is the narrow boundary between Sarun's recorded-file graph and
ViroS' image inspection code.  The request contains only immutable captured
file identities.  Filesystem placement is deliberately outside the request:
the provider supplies an :class:`ArtifactSource` backed by its read-only
attachments.  This keeps host paths, installation conventions, environment
variables, and user-provided kernel/rootfs selectors out of the protocol.

The combined-image request schema is::

    {
      "format": "viros-provider-derivation-request-v1",
      "selected": CapturedArtifact,
      "captured_artifacts": [CapturedArtifact, ...]
    }

``CapturedArtifact`` is the strict mapping accepted by
``image_inspector.CapturedArtifact``: ``box_id``, provider-relative ``path``,
``size``, lowercase ``sha256``, ``record_id``, optional finite ``roles``, and
optional ``architecture``.  The catalog is the finite transitive input set
already selected by Sarun's provenance resolver; this module never expands it.

An exact kernel/initramfs launch instead uses
``viros-provider-kernel-initramfs-request-v1`` with ``kernel``, ``initramfs``,
and the same finite ``captured_artifacts`` catalog.  Its two payloads are
verified and materialized byte-for-byte; inspection never substitutes a
different catalog candidate.

Result schema::

    {
      "format": "viros-provider-derivation-result-v1",
      "request_format": "viros-provider-derivation-request-v1",
      "derivation": <viros-selected-image-derivation-v1>,
      "materialized_components": [
        {"role", "media_type", "path", "size", "sha256"}, ...
      ]
    }

Every catalog file is opened by exact ``(box_id, path)`` identity and checked
before its metadata is allowed to influence inspection.  Components are
written below the caller-supplied workspace using the relative artifact names
already emitted by the inspector.  A failed derivation leaves no output tree.
"""

from __future__ import annotations

from contextlib import AbstractContextManager
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import shutil
import stat
import tempfile
from typing import BinaryIO, Mapping, Protocol

from probe.image_inspector import (
    MAX_IMAGE_BYTES,
    CapturedArtifact,
    ImageInspectionError,
    inspect_selected_image,
    materialize_component,
)


REQUEST_FORMAT = "viros-provider-derivation-request-v1"
RESULT_FORMAT = "viros-provider-derivation-result-v1"
PAIR_REQUEST_FORMAT = "viros-provider-kernel-initramfs-request-v1"
PAIR_RESULT_FORMAT = "viros-provider-kernel-initramfs-result-v1"
MAX_CAPTURED_ARTIFACTS = 100_000
_COPY_CHUNK = 1024 * 1024


class ProviderDerivationError(RuntimeError):
    """A request, attached artifact, or materialized result is inconsistent."""


class ArtifactSource(Protocol):
    """Exact access to the finite set of read-only attached box files."""

    def open_artifact(
        self, box_id: int, relative_path: str
    ) -> AbstractContextManager[BinaryIO]:
        """Open one regular captured file by its typed provenance identity."""


@dataclass(frozen=True)
class SelectedImageRequest:
    selected: CapturedArtifact
    captured_artifacts: tuple[CapturedArtifact, ...]

    def __post_init__(self) -> None:
        if not isinstance(self.selected, CapturedArtifact):
            raise ProviderDerivationError("selected artifact has the wrong type")
        if not isinstance(self.captured_artifacts, tuple) or any(
            not isinstance(row, CapturedArtifact) for row in self.captured_artifacts
        ):
            raise ProviderDerivationError("captured artifact catalog has the wrong type")
        if len(self.captured_artifacts) > MAX_CAPTURED_ARTIFACTS:
            raise ProviderDerivationError("captured artifact catalog is too large")

        # A physical captured path denotes one immutable file.  Repeated graph
        # edges are fine, but conflicting rows for that path are not.
        locations: dict[tuple[int, str], CapturedArtifact] = {}
        for row in (self.selected, *self.captured_artifacts):
            key = (row.box_id, row.path)
            previous = locations.get(key)
            if previous is not None and (
                previous.size != row.size or previous.sha256 != row.sha256
            ):
                raise ProviderDerivationError(
                    f"conflicting identities for captured file box {row.box_id}:{row.path}"
                )
            locations[key] = row

    @classmethod
    def from_mapping(cls, value: Mapping[str, object]) -> "SelectedImageRequest":
        if not isinstance(value, Mapping):
            raise ProviderDerivationError("provider derivation request must be a mapping")
        allowed = {"format", "selected", "captured_artifacts"}
        extra = set(value) - allowed
        if extra:
            raise ProviderDerivationError(
                f"unknown provider derivation request fields: {sorted(extra)!r}"
            )
        if value.get("format") != REQUEST_FORMAT:
            raise ProviderDerivationError(
                f"provider derivation request format must be {REQUEST_FORMAT!r}"
            )
        selected = value.get("selected")
        catalog = value.get("captured_artifacts")
        if not isinstance(selected, Mapping):
            raise ProviderDerivationError("selected artifact must be a mapping")
        if not isinstance(catalog, list):
            raise ProviderDerivationError("captured_artifacts must be a list")
        if len(catalog) > MAX_CAPTURED_ARTIFACTS:
            raise ProviderDerivationError("captured artifact catalog is too large")
        try:
            return cls(
                selected=CapturedArtifact.from_mapping(selected),
                captured_artifacts=tuple(
                    CapturedArtifact.from_mapping(row)
                    if isinstance(row, Mapping)
                    else _raise_malformed_catalog()
                    for row in catalog
                ),
            )
        except ImageInspectionError as exc:
            raise ProviderDerivationError(str(exc)) from exc

    def descriptor(self) -> dict[str, object]:
        return {
            "format": REQUEST_FORMAT,
            "selected": self.selected.descriptor(),
            "captured_artifacts": [
                row.descriptor() for row in self.captured_artifacts
            ],
        }


@dataclass(frozen=True)
class SelectedKernelInitramfsRequest:
    """Two exact boot payloads plus their finite provenance catalog.

    ``kernel`` and ``initramfs`` are selections, not hints.  Consumers must
    use these two identities as the boot payloads and may use
    ``captured_artifacts`` only to derive matching debugger resources.
    """

    kernel: CapturedArtifact
    initramfs: CapturedArtifact
    captured_artifacts: tuple[CapturedArtifact, ...]

    def __post_init__(self) -> None:
        if not isinstance(self.kernel, CapturedArtifact):
            raise ProviderDerivationError("selected kernel has the wrong type")
        if not isinstance(self.initramfs, CapturedArtifact):
            raise ProviderDerivationError("selected initramfs has the wrong type")
        if not isinstance(self.captured_artifacts, tuple) or any(
            not isinstance(row, CapturedArtifact) for row in self.captured_artifacts
        ):
            raise ProviderDerivationError("captured artifact catalog has the wrong type")
        if len(self.captured_artifacts) > MAX_CAPTURED_ARTIFACTS:
            raise ProviderDerivationError("captured artifact catalog is too large")
        if (self.kernel.box_id, self.kernel.path) == (
            self.initramfs.box_id,
            self.initramfs.path,
        ):
            raise ProviderDerivationError(
                "selected kernel and initramfs must be different captured files"
            )
        _validate_location_identities(
            (self.kernel, self.initramfs, *self.captured_artifacts)
        )

    @classmethod
    def from_mapping(
        cls, value: Mapping[str, object]
    ) -> "SelectedKernelInitramfsRequest":
        if not isinstance(value, Mapping):
            raise ProviderDerivationError(
                "provider kernel/initramfs request must be a mapping"
            )
        allowed = {"format", "kernel", "initramfs", "captured_artifacts"}
        extra = set(value) - allowed
        if extra:
            raise ProviderDerivationError(
                "unknown provider kernel/initramfs request fields: "
                f"{sorted(extra)!r}"
            )
        if value.get("format") != PAIR_REQUEST_FORMAT:
            raise ProviderDerivationError(
                "provider kernel/initramfs request format must be "
                f"{PAIR_REQUEST_FORMAT!r}"
            )
        kernel = value.get("kernel")
        initramfs = value.get("initramfs")
        catalog = value.get("captured_artifacts")
        if not isinstance(kernel, Mapping):
            raise ProviderDerivationError("selected kernel must be a mapping")
        if not isinstance(initramfs, Mapping):
            raise ProviderDerivationError("selected initramfs must be a mapping")
        if not isinstance(catalog, list):
            raise ProviderDerivationError("captured_artifacts must be a list")
        if len(catalog) > MAX_CAPTURED_ARTIFACTS:
            raise ProviderDerivationError("captured artifact catalog is too large")
        try:
            return cls(
                kernel=CapturedArtifact.from_mapping(kernel),
                initramfs=CapturedArtifact.from_mapping(initramfs),
                captured_artifacts=tuple(
                    CapturedArtifact.from_mapping(row)
                    if isinstance(row, Mapping)
                    else _raise_malformed_catalog()
                    for row in catalog
                ),
            )
        except ImageInspectionError as exc:
            raise ProviderDerivationError(str(exc)) from exc

    def descriptor(self) -> dict[str, object]:
        return {
            "format": PAIR_REQUEST_FORMAT,
            "kernel": self.kernel.descriptor(),
            "initramfs": self.initramfs.descriptor(),
            "captured_artifacts": [
                row.descriptor() for row in self.captured_artifacts
            ],
        }


def _validate_location_identities(rows: tuple[CapturedArtifact, ...]) -> None:
    locations: dict[tuple[int, str], CapturedArtifact] = {}
    for row in rows:
        key = (row.box_id, row.path)
        previous = locations.get(key)
        if previous is not None and (
            previous.size != row.size or previous.sha256 != row.sha256
        ):
            raise ProviderDerivationError(
                f"conflicting identities for captured file box {row.box_id}:{row.path}"
            )
        locations[key] = row


def _raise_malformed_catalog() -> CapturedArtifact:
    raise ProviderDerivationError("captured_artifacts rows must be mappings")


class FilesystemArtifactSource:
    """Artifact source with explicit box roots supplied by the provider.

    The mapping is an internal composition input, not part of the wire request.
    A live Sarun adapter can implement :class:`ArtifactSource` directly; this
    implementation is useful when the engine exposes each attachment at a
    distinct already-resolved root.
    """

    def __init__(self, box_roots: Mapping[int, Path]):
        self._roots: dict[int, Path] = {}
        for box_id, root in box_roots.items():
            if isinstance(box_id, bool) or not isinstance(box_id, int) or box_id < 0:
                raise ProviderDerivationError("artifact-source box id must be non-negative")
            try:
                resolved = Path(root).resolve(strict=True)
            except OSError as exc:
                raise ProviderDerivationError(
                    f"attached root for box {box_id} is unavailable: {exc}"
                ) from exc
            if not resolved.is_dir():
                raise ProviderDerivationError(
                    f"attached root for box {box_id} is not a directory"
                )
            self._roots[box_id] = resolved

    def open_artifact(
        self, box_id: int, relative_path: str
    ) -> AbstractContextManager[BinaryIO]:
        root = self._roots.get(box_id)
        if root is None:
            raise ProviderDerivationError(f"box {box_id} is not attached to the provider")
        # CapturedArtifact has already validated POSIX, normalized components.
        candidate = root.joinpath(*relative_path.split("/"))
        try:
            resolved = candidate.resolve(strict=True)
            resolved.relative_to(root)
            metadata = resolved.stat()
        except (OSError, ValueError) as exc:
            raise ProviderDerivationError(
                f"captured file is unavailable in attached box {box_id}:{relative_path}"
            ) from exc
        if not stat.S_ISREG(metadata.st_mode):
            raise ProviderDerivationError(
                f"captured artifact is not a regular file: box {box_id}:{relative_path}"
            )
        try:
            return resolved.open("rb")
        except OSError as exc:
            raise ProviderDerivationError(
                f"cannot open captured file box {box_id}:{relative_path}: {exc}"
            ) from exc


@dataclass(frozen=True)
class MaterializedComponent:
    role: str
    media_type: str
    path: str
    size: int
    sha256: str

    def descriptor(self) -> dict[str, object]:
        return {
            "role": self.role,
            "media_type": self.media_type,
            "path": self.path,
            "size": self.size,
            "sha256": self.sha256,
        }


@dataclass(frozen=True)
class ProviderDerivationResult:
    derivation: Mapping[str, object]
    materialized_components: tuple[MaterializedComponent, ...]

    def descriptor(self) -> dict[str, object]:
        return {
            "format": RESULT_FORMAT,
            "request_format": REQUEST_FORMAT,
            "derivation": dict(self.derivation),
            "materialized_components": [
                component.descriptor() for component in self.materialized_components
            ],
        }


@dataclass(frozen=True)
class ProviderKernelInitramfsResult:
    derivation: Mapping[str, object]
    materialized_components: tuple[MaterializedComponent, ...]

    def descriptor(self) -> dict[str, object]:
        return {
            "format": PAIR_RESULT_FORMAT,
            "request_format": PAIR_REQUEST_FORMAT,
            "derivation": dict(self.derivation),
            "materialized_components": [
                component.descriptor() for component in self.materialized_components
            ],
        }


def _checked_stream_identity(
    stream: BinaryIO, expected: CapturedArtifact, *, keep: bool
) -> bytes | None:
    digest = hashlib.sha256()
    size = 0
    selected_chunks: list[bytes] | None = [] if keep else None
    while True:
        chunk = stream.read(_COPY_CHUNK)
        if not isinstance(chunk, bytes):
            raise ProviderDerivationError("artifact source returned a non-binary stream")
        if not chunk:
            break
        size += len(chunk)
        if size > expected.size:
            raise ProviderDerivationError(
                f"captured file changed size: box {expected.box_id}:{expected.path}"
            )
        digest.update(chunk)
        if selected_chunks is not None:
            selected_chunks.append(chunk)
    if size != expected.size or digest.hexdigest() != expected.sha256:
        raise ProviderDerivationError(
            f"captured file identity mismatch: box {expected.box_id}:{expected.path}"
        )
    return b"".join(selected_chunks) if selected_chunks is not None else None


def _read_and_verify(
    source: ArtifactSource, row: CapturedArtifact, *, keep: bool
) -> bytes | None:
    try:
        with source.open_artifact(row.box_id, row.path) as stream:
            return _checked_stream_identity(stream, row, keep=keep)
    except ProviderDerivationError:
        raise
    except (OSError, ValueError) as exc:
        raise ProviderDerivationError(
            f"cannot read captured file box {row.box_id}:{row.path}: {exc}"
        ) from exc


def read_verified_artifact(
    source: ArtifactSource, row: CapturedArtifact
) -> bytes:
    """Read one exact catalog artifact through the provider boundary.

    Downstream internal derivation stages use this instead of recovering a
    filesystem path from an attachment.  Keeping the identity check here also
    means those stages cannot accidentally start trusting a path after the
    selected-image catalog was validated.
    """

    contents = _read_and_verify(source, row, keep=True)
    if contents is None:  # ``keep=True`` makes this unreachable by contract.
        raise ProviderDerivationError("verified artifact bytes were not retained")
    return contents


def _safe_output_path(workspace: Path, relative_path: str) -> Path:
    # Artifact names originate in the inspector, but validate the boundary
    # again so future inspector formats cannot escape the transaction tree.
    if (
        not relative_path
        or relative_path.startswith("/")
        or "\\" in relative_path
        or "\x00" in relative_path
        or any(part in {"", ".", ".."} for part in relative_path.split("/"))
    ):
        raise ProviderDerivationError(
            f"inspector returned an unsafe materialized path: {relative_path!r}"
        )
    return workspace.joinpath(*relative_path.split("/"))


def _write_exact(path: Path, contents: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        with path.open("xb") as stream:
            stream.write(contents)
            stream.flush()
            os.fsync(stream.fileno())
    except OSError as exc:
        raise ProviderDerivationError(f"cannot materialize {path.name}: {exc}") from exc


def derive_selected_kernel_initramfs(
    request: SelectedKernelInitramfsRequest,
    source: ArtifactSource,
    output_root: Path,
) -> ProviderKernelInitramfsResult:
    """Verify and materialize an exact selected kernel/initramfs pair.

    Both selected files and every row in the finite provenance catalog are
    checked before the transaction directory is created.  The initramfs is
    inspected to establish that it is newc or gzip-newc, but the selected
    bytes themselves are retained for QEMU rather than re-encoded.
    """

    output_root = Path(output_root)
    if output_root.exists():
        raise ProviderDerivationError("provider derivation output already exists")
    parent = output_root.parent
    if not parent.is_dir():
        raise ProviderDerivationError("provider derivation output parent is unavailable")
    if request.kernel.size > MAX_IMAGE_BYTES:
        raise ProviderDerivationError("selected kernel exceeds the ViroS inspection limit")
    if request.initramfs.size > MAX_IMAGE_BYTES:
        raise ProviderDerivationError(
            "selected initramfs exceeds the ViroS inspection limit"
        )

    selected_keys = {
        (request.kernel.box_id, request.kernel.path): "kernel",
        (request.initramfs.box_id, request.initramfs.path): "initramfs",
    }
    rows: dict[tuple[int, str], CapturedArtifact] = {}
    for row in request.captured_artifacts:
        rows[(row.box_id, row.path)] = row
    rows[(request.kernel.box_id, request.kernel.path)] = request.kernel
    rows[(request.initramfs.box_id, request.initramfs.path)] = request.initramfs
    selected_data: dict[str, bytes] = {}
    for key in sorted(rows):
        contents = _read_and_verify(source, rows[key], keep=key in selected_keys)
        role = selected_keys.get(key)
        if role is not None:
            if contents is None:
                raise ProviderDerivationError(
                    f"selected {role} captured file was not verified"
                )
            selected_data[role] = contents

    try:
        initramfs_inspection = inspect_selected_image(
            selected_data["initramfs"],
            request.initramfs,
            request.captured_artifacts,
        )
    except ImageInspectionError as exc:
        raise ProviderDerivationError(str(exc)) from exc
    if initramfs_inspection.get("layout") not in {"cpio-newc", "gzip-cpio-newc"}:
        raise ProviderDerivationError(
            "selected initramfs must be a cpio newc or cpio.gz file"
        )

    staging = Path(tempfile.mkdtemp(prefix=f".{output_root.name}.", dir=parent))
    completed = False
    try:
        _write_exact(staging / "kernel", selected_data["kernel"])
        _write_exact(staging / "initramfs", selected_data["initramfs"])
        components = (
            MaterializedComponent(
                role="kernel",
                media_type="application/octet-stream",
                path="kernel",
                size=request.kernel.size,
                sha256=request.kernel.sha256,
            ),
            MaterializedComponent(
                role="initramfs",
                media_type=(
                    "application/gzip"
                    if initramfs_inspection.get("layout") == "gzip-cpio-newc"
                    else "application/x-cpio"
                ),
                path="initramfs",
                size=request.initramfs.size,
                sha256=request.initramfs.sha256,
            ),
        )
        derivation = {
            "format": "viros-selected-kernel-initramfs-derivation-v1",
            "layout": "selected-kernel-initramfs",
            "kernel": request.kernel.descriptor(),
            "initramfs": request.initramfs.descriptor(),
            "initramfs_layout": initramfs_inspection["layout"],
            "compatibility": {
                "artifact_identity": "size-sha256",
                "kernel_bundle_format": "viros-kernel-bundle-v1",
                "image_bundle_format": "viros-image-bundle-v1",
                "boot_input_complete": True,
            },
        }
        os.replace(staging, output_root)
        completed = True
        return ProviderKernelInitramfsResult(derivation, components)
    finally:
        if not completed:
            shutil.rmtree(staging, ignore_errors=True)


def derive_selected_image(
    request: SelectedImageRequest,
    source: ArtifactSource,
    output_root: Path,
) -> ProviderDerivationResult:
    """Verify, inspect, and transactionally materialize one selected image."""

    output_root = Path(output_root)
    if output_root.exists():
        raise ProviderDerivationError("provider derivation output already exists")
    parent = output_root.parent
    if not parent.is_dir():
        raise ProviderDerivationError("provider derivation output parent is unavailable")
    if request.selected.size > MAX_IMAGE_BYTES:
        raise ProviderDerivationError("selected image exceeds the ViroS inspection limit")

    # Deduplicate graph edges while preserving a deterministic identity order.
    rows: dict[tuple[int, str], CapturedArtifact] = {}
    for row in request.captured_artifacts:
        rows[(row.box_id, row.path)] = row
    selected_key = (request.selected.box_id, request.selected.path)
    rows[selected_key] = request.selected
    selected_data: bytes | None = None
    for key in sorted(rows):
        row = rows[key]
        contents = _read_and_verify(source, row, keep=key == selected_key)
        if key == selected_key:
            selected_data = contents
    if selected_data is None:
        raise ProviderDerivationError("selected captured file was not verified")
    try:
        inspection = inspect_selected_image(
            selected_data, request.selected, request.captured_artifacts
        )
    except ImageInspectionError as exc:
        raise ProviderDerivationError(str(exc)) from exc

    staging = Path(tempfile.mkdtemp(prefix=f".{output_root.name}.", dir=parent))
    completed = False
    materialized: list[MaterializedComponent] = []
    try:
        components = inspection.get("components")
        if not isinstance(components, list):
            raise ProviderDerivationError("inspector returned no typed components")
        for component in components:
            if not isinstance(component, Mapping):
                raise ProviderDerivationError("inspector returned a malformed component")
            artifact = component.get("artifact")
            role = component.get("role")
            media_type = component.get("media_type")
            if (
                not isinstance(artifact, Mapping)
                or not isinstance(role, str)
                or not isinstance(media_type, str)
            ):
                raise ProviderDerivationError("inspector returned a malformed component")
            relative_path = artifact.get("path")
            if not isinstance(relative_path, str):
                raise ProviderDerivationError("component artifact has no relative path")
            try:
                contents = materialize_component(selected_data, component)
            except ImageInspectionError as exc:
                raise ProviderDerivationError(str(exc)) from exc
            destination = _safe_output_path(staging, relative_path)
            _write_exact(destination, contents)
            size = artifact.get("size")
            digest = artifact.get("sha256")
            if (
                isinstance(size, bool)
                or not isinstance(size, int)
                or not isinstance(digest, str)
            ):
                raise ProviderDerivationError("component artifact has no typed identity")
            materialized.append(MaterializedComponent(
                role=role,
                media_type=media_type,
                path=relative_path,
                size=size,
                sha256=digest,
            ))
        materialized.sort(key=lambda row: (row.role, row.path))
        os.replace(staging, output_root)
        completed = True
    finally:
        if not completed:
            shutil.rmtree(staging, ignore_errors=True)

    return ProviderDerivationResult(inspection, tuple(materialized))


def derive_selected_image_mapping(
    request: Mapping[str, object],
    source: ArtifactSource,
    output_root: Path,
) -> dict[str, object]:
    """Strict mapping adapter for the eventual provider wire handler."""

    return derive_selected_image(
        SelectedImageRequest.from_mapping(request), source, output_root
    ).descriptor()


def derive_selected_kernel_initramfs_mapping(
    request: Mapping[str, object],
    source: ArtifactSource,
    output_root: Path,
) -> dict[str, object]:
    """Strict mapping adapter for an exact selected kernel/initramfs pair."""

    return derive_selected_kernel_initramfs(
        SelectedKernelInitramfsRequest.from_mapping(request), source, output_root
    ).descriptor()
