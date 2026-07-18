"""Run frozen-probe snapshot pages through the reversible call gate.

The public boundary is a cursor-to-bytes callable suitable for
``probe.abi.decode_paginated`` and ``inferiors.probe_oracle.ProbeOracle``.
Every returned page was produced by a complete ``CallGateTransaction``; the
transaction has restored and audited the guest before this module decodes the
mailbox.
"""

from __future__ import annotations

from dataclasses import dataclass, replace
import struct
from typing import Callable, Protocol

from callgate.manifest import ValidatedManifest
from callgate.transaction import CallGateResult, CallGateTransaction, Target
from probe.abi import (
    AARCH64_SNAPSHOT_ABI,
    ABI_MAJOR,
    ABI_MINOR,
    REQUEST_MAGIC,
    RESPONSE_MAGIC,
    RESPONSE_SIZE,
    TASK_V1_SIZE,
    ProbeDecodeError,
    ProbePage,
    ProbeSnapshot,
    SnapshotAbi,
    build_snapshot_request,
    decode_paginated,
    decode_response,
)


REQUEST_SIZE = 64
OP_SNAPSHOT = 1
_REQUEST_FORMAT = "IHHHHIQQIIQQQ"


class ProbeRunnerError(ProbeDecodeError):
    """The sealed request or a runtime page violates the snapshot contract."""


class Transaction(Protocol):
    def execute(self) -> CallGateResult: ...


TransactionFactory = Callable[[Target, ValidatedManifest], Transaction]


@dataclass(frozen=True)
class SnapshotRequest:
    """The immutable fields bound by the validated call-gate manifest."""

    init_task: int
    max_records: int
    abi_minor: int
    snapshot_abi: SnapshotAbi


def _validate_request_template(
    manifest: ValidatedManifest, snapshot_abi: SnapshotAbi
) -> SnapshotRequest:
    if not isinstance(manifest, ValidatedManifest) or not manifest.is_validated:
        raise ProbeRunnerError("a validated call-gate manifest is required")
    if not isinstance(snapshot_abi, SnapshotAbi):
        raise ProbeRunnerError("snapshot target ABI metadata is required")
    request = manifest.request_bytes
    if len(request) != REQUEST_SIZE:
        raise ProbeRunnerError("sealed probe request must be exactly 64 bytes")
    try:
        fields = struct.unpack(snapshot_abi.byte_order + _REQUEST_FORMAT, request)
    except struct.error as exc:  # defensive: the exact length was checked above
        raise ProbeRunnerError("cannot decode sealed probe request") from exc
    (magic, major, minor, size, opcode, flags, init_task, cursor,
     max_records, reserved0, reserved1, reserved2, reserved3) = fields
    if (magic, major, size, opcode) != (
        REQUEST_MAGIC, ABI_MAJOR, REQUEST_SIZE, OP_SNAPSHOT
    ) or not 0 <= minor <= ABI_MINOR:
        raise ProbeRunnerError("sealed request is not a supported snapshot ABI-v1 request")
    if flags or reserved0 or reserved1 or reserved2 or reserved3:
        raise ProbeRunnerError("sealed snapshot request has nonzero flags or reserved fields")
    if not init_task:
        raise ProbeRunnerError("sealed snapshot request has a zero init_task")
    if init_task >= snapshot_abi.pointer_limit:
        raise ProbeRunnerError(
            "sealed init_task does not fit the configured "
            f"{snapshot_abi.pointer_bits}-bit pointer width"
        )
    if cursor:
        raise ProbeRunnerError("sealed snapshot request must begin with cursor zero")
    if not max_records:
        raise ProbeRunnerError("sealed snapshot request must set a nonzero max_records")

    data = manifest.region(manifest.data_region)
    request_start = manifest.request_offset
    request_end = request_start + REQUEST_SIZE
    result_start = manifest.result_offset
    result_end = result_start + manifest.result_size
    if request_start & 7 or result_start & 7:
        raise ProbeRunnerError("request and result offsets must be 8-byte aligned")
    if request_end > data.size or result_end > data.size:
        raise ProbeRunnerError("snapshot mailbox does not fit in the sealed data region")
    if max(request_start, result_start) < min(request_end, result_end):
        raise ProbeRunnerError("snapshot request and result buffers overlap")
    if manifest.completion_magic != struct.pack(
        snapshot_abi.byte_order + "I", RESPONSE_MAGIC
    ):
        raise ProbeRunnerError("sealed completion magic must be the ABI-v1 response magic")
    capacity = (manifest.result_size - RESPONSE_SIZE) // TASK_V1_SIZE
    if capacity <= 0 or max_records > capacity:
        raise ProbeRunnerError(
            "sealed max_records does not fit the result buffer's ABI-v1 capacity"
        )
    return SnapshotRequest(
        init_task=init_task, max_records=max_records, abi_minor=minor,
        snapshot_abi=snapshot_abi,
    )


class ProbeSnapshotRunner:
    """Stateful ABI-v1 page fetcher backed by reversible transactions.

    Cursor zero always starts (or explicitly restarts) a snapshot.  Every
    nonzero cursor must be the exact ``next_cursor`` advertised by the prior
    page, so callers cannot turn the probe into an arbitrary task-pointer
    reader.
    """

    def __init__(
        self,
        target: Target,
        manifest: ValidatedManifest,
        transaction_factory: TransactionFactory = CallGateTransaction,
        *,
        snapshot_abi: SnapshotAbi = AARCH64_SNAPSHOT_ABI,
    ) -> None:
        self.target = target
        self.manifest = manifest
        self.request = _validate_request_template(manifest, snapshot_abi)
        self._transaction_factory = transaction_factory
        self._expected_cursor = 0
        self._metadata: tuple[int, int, str, int, int, int] | None = None
        self._seen_tasks: set[int] = set()
        self._seen_cursors: set[int] = set()
        self._audits: list[tuple[str, ...]] = []

    @property
    def audits(self) -> tuple[tuple[str, ...], ...]:
        """Restoration audit returned by each completed transaction."""

        return tuple(self._audits)

    def _reset_sequence(self) -> None:
        self._expected_cursor = 0
        self._metadata = None
        self._seen_tasks.clear()
        self._seen_cursors.clear()

    def _request_bytes(self, cursor: int) -> bytes:
        return build_snapshot_request(
            self.request.init_task, cursor, self.request.max_records,
            abi_minor=self.request.abi_minor,
            snapshot_abi=self.request.snapshot_abi,
        )

    def _validate_page(self, page: ProbePage, cursor: int) -> None:
        abi = self.request.snapshot_abi
        if page.abi_minor != self.request.abi_minor:
            raise ProbeRunnerError("probe response is not the exact sealed ABI-v1 page")
        if (page.arch, page.byte_order, page.pointer_bits) != (
            abi.arch, abi.byte_order, abi.pointer_bits
        ):
            raise ProbeRunnerError(
                "probe response does not match the sealed "
                f"{abi.name} snapshot ABI"
            )
        if page.snapshot_root != self.request.init_task:
            raise ProbeRunnerError("probe response snapshot_root does not match sealed init_task")
        if not page.tasks or page.tasks[0].task != (cursor or self.request.init_task):
            raise ProbeRunnerError("probe page did not begin at its requested cursor")
        if len(page.tasks) > self.request.max_records:
            raise ProbeRunnerError("probe page exceeds sealed max_records")

        metadata = (
            page.abi_minor, page.arch, page.byte_order, page.pointer_bits,
            page.page_shift, page.snapshot_root,
        )
        if self._metadata is None:
            self._metadata = metadata
        elif metadata != self._metadata:
            raise ProbeRunnerError("probe page metadata changed during pagination")
        for task in page.tasks:
            if task.task in self._seen_tasks:
                raise ProbeRunnerError(f"duplicate task pointer {task.task:#x} in snapshot")
            self._seen_tasks.add(task.task)
        if page.more and (
            page.next_cursor == cursor or page.next_cursor in self._seen_cursors
        ):
            raise ProbeRunnerError(f"pagination cursor cycle at {page.next_cursor:#x}")

    def fetch_page(self, cursor: int) -> bytes:
        """Execute, restore, decode, and validate one requested snapshot page."""

        abi = self.request.snapshot_abi
        if (isinstance(cursor, bool) or not isinstance(cursor, int)
                or not 0 <= cursor < abi.pointer_limit):
            if abi.pointer_bits == 64:
                raise ProbeRunnerError("snapshot cursor must be an unsigned 64-bit integer")
            raise ProbeRunnerError(
                f"snapshot cursor must fit the configured {abi.pointer_bits}-bit pointer width"
            )
        if cursor == 0:
            self._reset_sequence()
            self._seen_cursors.add(0)
        elif cursor != self._expected_cursor:
            raise ProbeRunnerError(
                f"snapshot cursor mismatch: expected {self._expected_cursor:#x}, got {cursor:#x}"
            )

        # ``replace`` leaves the validated source object untouched and carries
        # its private validation seal.  The only changed bytes were constructed
        # above from frozen, already-validated fields and the sequenced cursor.
        page_manifest = replace(
            self.manifest, request_bytes=self._request_bytes(cursor)
        )
        try:
            result = self._transaction_factory(self.target, page_manifest).execute()
            page = decode_response(
                bytes(result.result), expected_abi=self.request.snapshot_abi
            )
            self._validate_page(page, cursor)
        except BaseException:
            self._reset_sequence()
            raise

        self._audits.append(tuple(result.audit))
        if page.more:
            self._expected_cursor = page.next_cursor
            self._seen_cursors.add(page.next_cursor)
        else:
            self._reset_sequence()
        return bytes(result.result)

    __call__ = fetch_page

    def snapshot(self, max_pages: int = 4096) -> ProbeSnapshot:
        """Return one complete decoded snapshot."""

        try:
            return decode_paginated(
                self.fetch_page, max_pages=max_pages,
                expected_abi=self.request.snapshot_abi,
            )
        except BaseException:
            self._reset_sequence()
            raise
