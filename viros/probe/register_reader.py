"""Controlled saved-userspace-register reads through the frozen probe."""

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
    SAVED_REGS_V1_SIZE,
    ProbeDecodeError,
    ProbeSavedRegisters,
    ProbeSnapshot,
    ProbeTask,
    _build_saved_registers_request,
    decode_saved_registers_response,
)


class ProbeRegisterError(ProbeDecodeError):
    """A saved-register request escaped its frozen-snapshot contract."""


class Transaction(Protocol):
    def execute(self) -> CallGateResult: ...


TransactionFactory = Callable[[Target, ValidatedManifest], Transaction]


class ProbeRegisterReader:
    """Read one sleeping task's saved native AArch64 EL0 exception frame.

    A reader must first be bound to the exact decoded snapshot exposed to GDB.
    This keeps task/mm pointers and the start cookie out of the public API.
    Each read is a complete restoring call-gate transaction.
    """

    def __init__(
        self,
        target: Target,
        manifest: ValidatedManifest,
        transaction_factory: TransactionFactory = CallGateTransaction,
    ) -> None:
        if not isinstance(manifest, ValidatedManifest) or not manifest.is_validated:
            raise ProbeRegisterError("a validated call-gate manifest is required")
        if "saved-regs-aarch64-v1" not in manifest.probe_capabilities:
            raise ProbeRegisterError(
                "probe package does not advertise saved AArch64 registers")
        if len(manifest.request_bytes) != 64:
            raise ProbeRegisterError("probe request mailbox must be exactly 64 bytes")
        if manifest.result_size < RESPONSE_SIZE + SAVED_REGS_V1_SIZE:
            raise ProbeRegisterError(
                "probe result mailbox is too small for a saved-register record")
        if manifest.request_offset & 7 or manifest.result_offset & 7:
            raise ProbeRegisterError(
                "probe request/result mailboxes must be 8-byte aligned")
        if manifest.completion_magic != RESPONSE_MAGIC.to_bytes(4, "little"):
            raise ProbeRegisterError(
                "probe completion magic is not the ABI-v1 response magic")
        try:
            request = struct.unpack("<IHHHHIQQIIQQQ", manifest.request_bytes)
        except struct.error as exc:
            raise ProbeRegisterError("cannot decode the sealed snapshot request") from exc
        if (request[:3] != (0x56505251, ABI_MAJOR, ABI_MINOR)
                or request[3:5] != (64, OP_SNAPSHOT)):
            raise ProbeRegisterError(
                "saved registers require a sealed ABI-v1.2 snapshot request")
        self.target = target
        self.manifest = manifest
        self.transaction_factory = transaction_factory
        self._tasks: dict[int, ProbeTask] = {}
        self._audits: list[tuple[str, ...]] = []

    @property
    def audits(self) -> tuple[tuple[str, ...], ...]:
        return tuple(self._audits)

    def bind_snapshot(self, snapshot: ProbeSnapshot) -> None:
        if not isinstance(snapshot, ProbeSnapshot):
            raise ProbeRegisterError(
                "register reader requires a decoded probe snapshot")
        if (snapshot.abi_minor != ABI_MINOR or snapshot.arch != 1
                or snapshot.byte_order != "<" or snapshot.pointer_bits != 64):
            raise ProbeRegisterError(
                "register reader requires a native AArch64 ABI-v1.2 snapshot")
        tasks: dict[int, ProbeTask] = {}
        for record in snapshot.tasks:
            if not record.mm or record.pid <= 0 or record.tgid <= 0:
                continue
            cookie = record.stable_cookie
            if cookie in tasks:
                raise ProbeRegisterError(
                    "snapshot contains a duplicate stable task cookie")
            tasks[cookie] = record
        self._tasks = tasks
        self._audits.clear()

    def _record(self, task: TaskSnapshot) -> ProbeTask:
        if not isinstance(task, TaskSnapshot):
            raise ProbeRegisterError(
                "saved-register read requires a frozen TaskSnapshot")
        try:
            record = self._tasks[task.task_cookie]
        except KeyError as exc:
            raise ProbeRegisterError(
                "task does not belong to the bound frozen snapshot") from exc
        if (record.tgid, record.pid) != (task.identity.tgid, task.identity.tid):
            raise ProbeRegisterError(
                "task identity does not match its stable probe cookie")
        return record

    def read_registers(self, task: TaskSnapshot) -> ProbeSavedRegisters:
        record = self._record(task)
        request = _build_saved_registers_request(record)
        transaction_manifest = replace(self.manifest, request_bytes=request)
        result = self.transaction_factory(
            self.target, transaction_manifest
        ).execute()
        saved = decode_saved_registers_response(bytes(result.result))
        if (saved.task != record.task or saved.mm != record.mm
                or saved.start_cookie != record.start_cookie):
            raise ProbeRegisterError(
                "saved-register response does not match its frozen request")
        self._audits.append(tuple(result.audit))
        return saved
