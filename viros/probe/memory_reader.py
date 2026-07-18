"""Controlled read-only userspace memory access through the frozen probe."""

from __future__ import annotations

from dataclasses import replace
import struct
from typing import Callable, Protocol

from callgate.manifest import ValidatedManifest
from callgate.transaction import CallGateResult, CallGateTransaction, Target
from inferiors.linux_oracle import TaskSnapshot
from probe.abi import (
    ABI_MAJOR,
    ABI_MINOR,
    OP_SNAPSHOT,
    RESPONSE_MAGIC,
    RESPONSE_SIZE,
    TRANSLATION_V1_SIZE,
    XLATE_SAFE_READ,
    ProbeDecodeError,
    ProbeSnapshot,
    ProbeTask,
    ProbeTranslation,
    _build_translation_request,
    decode_translation_response,
)


class ProbeMemoryError(ProbeDecodeError):
    """A memory request escaped the frozen-snapshot translation contract."""


class Transaction(Protocol):
    def execute(self) -> CallGateResult: ...


TransactionFactory = Callable[[Target, ValidatedManifest], Transaction]


class ProbeMemoryReader:
    """Translate and read userspace without writing process memory.

    ``bind_snapshot`` is deliberately mandatory.  It ensures requests can only
    contain task/mm pointers emitted by a decoded frozen snapshot, rather than
    exposing the injected walker as an arbitrary kernel-pointer primitive.
    Every mapping translation is its own restoring ``CallGateTransaction``;
    only after restoration does the host read the returned physical span.
    """

    def __init__(
        self,
        target: Target,
        manifest: ValidatedManifest,
        transaction_factory: TransactionFactory = CallGateTransaction,
        *,
        max_read_bytes: int = 16 * 1024 * 1024,
    ) -> None:
        if not isinstance(manifest, ValidatedManifest) or not manifest.is_validated:
            raise ProbeMemoryError("a validated call-gate manifest is required")
        if "translate-va-aarch64-v1" not in manifest.probe_capabilities:
            raise ProbeMemoryError(
                "probe package does not advertise AArch64 VA translation")
        if len(manifest.request_bytes) != 64:
            raise ProbeMemoryError("probe request mailbox must be exactly 64 bytes")
        if manifest.result_size < RESPONSE_SIZE + TRANSLATION_V1_SIZE:
            raise ProbeMemoryError("probe result mailbox is too small for a translation")
        if manifest.request_offset & 7 or manifest.result_offset & 7:
            raise ProbeMemoryError("probe request/result mailboxes must be 8-byte aligned")
        if manifest.completion_magic != RESPONSE_MAGIC.to_bytes(4, "little"):
            raise ProbeMemoryError("probe completion magic is not the ABI-v1 response magic")
        try:
            request = struct.unpack("<IHHHHIQQIIQQQ", manifest.request_bytes)
        except struct.error as exc:
            raise ProbeMemoryError("cannot decode the sealed snapshot request") from exc
        if request[:5] != (0x56505251, ABI_MAJOR, ABI_MINOR, 64, OP_SNAPSHOT):
            raise ProbeMemoryError(
                "memory reads require a sealed ABI-v1.1 snapshot request")
        if (isinstance(max_read_bytes, bool) or not isinstance(max_read_bytes, int)
                or max_read_bytes <= 0):
            raise ProbeMemoryError("max_read_bytes must be a positive integer")
        self.target = target
        self.manifest = manifest
        self.transaction_factory = transaction_factory
        self.max_read_bytes = max_read_bytes
        self._tasks: dict[int, ProbeTask] = {}
        self._linear_offsets: dict[int, int] = {}
        self._snapshot_linear_offset: int | None = None
        self._audits: list[tuple[str, ...]] = []

    @property
    def audits(self) -> tuple[tuple[str, ...], ...]:
        return tuple(self._audits)

    def bind_snapshot(self, snapshot: ProbeSnapshot) -> None:
        """Replace the only task identities eligible for subsequent reads."""

        if not isinstance(snapshot, ProbeSnapshot):
            raise ProbeMemoryError("memory reader requires a decoded probe snapshot")
        if (snapshot.abi_minor != ABI_MINOR or snapshot.arch != 1
                or snapshot.byte_order != "<" or snapshot.pointer_bits != 64
                or snapshot.page_shift != 12):
            raise ProbeMemoryError(
                "memory reader requires an AArch64 4K-page ABI-v1.1 snapshot")
        tasks: dict[int, ProbeTask] = {}
        for record in snapshot.tasks:
            if not record.mm or record.pid <= 0 or record.tgid <= 0:
                continue
            cookie = record.stable_cookie
            if cookie in tasks:
                raise ProbeMemoryError("snapshot contains a duplicate stable task cookie")
            tasks[cookie] = record
        self._tasks = tasks
        self._linear_offsets.clear()
        self._snapshot_linear_offset = None
        self._audits.clear()

    def _record(self, task: TaskSnapshot) -> ProbeTask:
        if not isinstance(task, TaskSnapshot):
            raise ProbeMemoryError("memory read requires a frozen TaskSnapshot")
        try:
            record = self._tasks[task.task_cookie]
        except KeyError as exc:
            raise ProbeMemoryError("task does not belong to the bound frozen snapshot") from exc
        if (record.tgid, record.pid) != (task.identity.tgid, task.identity.tid):
            raise ProbeMemoryError("task identity does not match its stable probe cookie")
        return record

    def _linear_offset(self, record: ProbeTask) -> int:
        """Derive, validate, and cache the exact task's direct-map offset."""

        cookie = record.stable_cookie
        cached = self._linear_offsets.get(cookie)
        if cached is not None:
            return cached
        if not record.pgd_kernel_va:
            raise ProbeMemoryError("task snapshot has no mm->pgd kernel address")
        self.target.assert_stopped()
        physical = self.target.translate_virtual(
            self.manifest.cpu, record.pgd_kernel_va)
        if (isinstance(physical, bool) or not isinstance(physical, int)
                or not 0 <= physical < 1 << 64):
            raise ProbeMemoryError("QEMU returned an invalid mm->pgd physical address")
        page_mask = 0xfff
        if record.pgd_kernel_va & page_mask or physical & page_mask:
            raise ProbeMemoryError(
                "mm->pgd virtual and QEMU physical addresses must be page-aligned")
        offset = (record.pgd_kernel_va - physical) & ((1 << 64) - 1)
        if not offset or offset & page_mask:
            raise ProbeMemoryError(
                "QEMU mm->pgd translation did not yield a valid direct-map offset")
        if (self._snapshot_linear_offset is not None
                and offset != self._snapshot_linear_offset):
            raise ProbeMemoryError(
                "QEMU mm->pgd translations disagree on the direct-map offset")
        self._snapshot_linear_offset = offset
        self._linear_offsets[cookie] = offset
        return offset

    def translate(self, task: TaskSnapshot, virtual_address: int) -> ProbeTranslation:
        """Run one reversible transaction for exactly one VA translation."""

        record = self._record(task)
        request = _build_translation_request(
            record, virtual_address, self._linear_offset(record))
        # ``replace`` preserves the loader's validation seal; the only mutation
        # is an ABI request assembled from a bound snapshot record and checked VA.
        transaction_manifest = replace(self.manifest, request_bytes=request)
        result = self.transaction_factory(self.target, transaction_manifest).execute()
        translation = decode_translation_response(bytes(result.result))
        if (translation.task != record.task or translation.mm != record.mm
                or translation.virtual_address != virtual_address):
            raise ProbeMemoryError("probe translation does not match its frozen request")
        self._audits.append(tuple(result.audit))
        return translation

    def read_memory(self, task: TaskSnapshot, address: int, length: int) -> bytes:
        """Read across normal/block mappings; process-memory writes are absent."""

        record = self._record(task)
        if (isinstance(address, bool) or not isinstance(address, int)
                or isinstance(length, bool) or not isinstance(length, int)
                or address < 0 or length < 0):
            raise ProbeMemoryError("memory address and length must be nonnegative integers")
        if length > self.max_read_bytes:
            raise ProbeMemoryError(
                f"memory read exceeds the {self.max_read_bytes}-byte safety limit")
        # AArch64 userspace is always in the lower canonical half.  Compat
        # tasks are further bounded by their 32-bit process ABI.
        limit = min(1 << record.abi_bits, 1 << 63)
        if address >= limit or address + length > limit:
            raise ProbeMemoryError("memory read exceeds the task virtual address width")
        if length == 0:
            return b""

        output = bytearray()
        current = address
        remaining = length
        while remaining:
            translation = self.translate(task, current)
            if not translation.flags & XLATE_SAFE_READ:
                raise ProbeMemoryError(
                    f"probe refused a safe physical read for userspace VA {current:#x}")
            chunk = min(remaining, translation.contiguous_bytes)
            if chunk <= 0:
                raise ProbeMemoryError("probe returned a zero-length translation span")
            physical = self.target.read_physical(translation.physical_address, chunk)
            if not isinstance(physical, bytes) or len(physical) != chunk:
                raise ProbeMemoryError(
                    "physical target returned a short or non-bytes memory read")
            output.extend(physical)
            current += chunk
            remaining -= chunk
        return bytes(output)
