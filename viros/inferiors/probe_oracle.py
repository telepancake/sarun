"""Adapter from frozen-probe pages to the version-neutral Linux oracle."""

from __future__ import annotations

import struct
from typing import Callable, Protocol

from probe.abi import (
    AUX_TAGS,
    TASK_ON_CPU,
    ProbeSavedRegisters,
    ProbeTask,
    SnapshotAbi,
    decode_paginated,
)

from .linux_oracle import Snapshot, TaskId, TaskSnapshot


PageFetcher = Callable[[int], bytes]
ExecutableResolver = Callable[[ProbeTask], str]


class MemoryReader(Protocol):
    def bind_snapshot(self, snapshot) -> None: ...
    def read_memory(self, task: TaskSnapshot, address: int, length: int) -> bytes: ...


class RegisterReader(Protocol):
    def bind_snapshot(self, snapshot) -> None: ...
    def read_registers(self, task: TaskSnapshot) -> ProbeSavedRegisters: ...


def _auxv_bytes(task: ProbeTask, byte_order: str) -> bytes:
    prefix = "<" if byte_order == "<" else ">"
    code = "I" if task.abi_bits == 32 else "Q"
    limit = (1 << task.abi_bits) - 1
    result = bytearray()
    for index, tag in enumerate(AUX_TAGS):
        if not task.auxv_valid & (1 << index):
            continue
        value = task.auxv_values[index]
        if value > limit:
            raise ValueError(
                f"auxv value {value:#x} does not fit the task's {task.abi_bits}-bit ABI")
        result.extend(struct.pack(prefix + code * 2, tag, value))
    result.extend(struct.pack(prefix + code * 2, 0, 0))
    return bytes(result)


class ProbeOracle:
    """LinuxOracle backed by one or more injected-probe result pages."""

    def __init__(
        self,
        fetch_page: PageFetcher,
        executable_resolver: ExecutableResolver | None = None,
        memory_reader: MemoryReader | None = None,
        register_reader: RegisterReader | None = None,
        snapshot_abi: SnapshotAbi | None = None,
    ) -> None:
        self.fetch_page = fetch_page
        self.executable_resolver = executable_resolver or (lambda task: "")
        self.memory_reader = memory_reader
        self.register_reader = register_reader
        self.snapshot_abi = snapshot_abi
        self._generation = 0
        self._pgd_kernel_va: dict[TaskId, int] = {}

    def snapshot(self) -> Snapshot:
        probe = decode_paginated(
            self.fetch_page, expected_abi=self.snapshot_abi
        )
        if self.memory_reader is not None:
            self.memory_reader.bind_snapshot(probe)
        if self.register_reader is not None:
            self.register_reader.bind_snapshot(probe)
        tasks = []
        pgds: dict[TaskId, int] = {}
        for record in probe.tasks:
            # PID zero and mm-less kernel threads are not GDB user inferiors.
            if record.pid <= 0 or record.tgid <= 0 or not record.mm:
                continue
            identity = TaskId(record.tgid, record.pid)
            pgds[identity] = record.pgd_kernel_va
            current_cpu = record.cpu if record.probe_flags & TASK_ON_CPU else None
            state = "running" if current_cpu is not None else f"state=0x{record.state:x}"
            tasks.append(TaskSnapshot(
                identity=identity,
                task_cookie=record.stable_cookie,
                comm=record.comm,
                executable=self.executable_resolver(record),
                auxv=_auxv_bytes(record, probe.byte_order),
                state=state,
                current_cpu=current_cpu,
                # Probe v1 reports a kernel VA for mm->pgd.  Do not present it
                # as a usable physical page-table root before translation.
                page_table_root=None,
            ))
        self._generation += 1
        self._pgd_kernel_va = pgds
        return Snapshot(self._generation, tuple(tasks))

    def pgd_kernel_va(self, task: TaskSnapshot | TaskId) -> int:
        identity = task.identity if isinstance(task, TaskSnapshot) else task
        try:
            return self._pgd_kernel_va[identity]
        except KeyError as exc:
            raise LookupError(f"no probe pgd for {identity.rsp()}") from exc

    def read_memory(self, task: TaskSnapshot, address: int, length: int) -> bytes:
        if self.memory_reader is None:
            raise NotImplementedError(
                "probe memory reader is not attached to this snapshot oracle")
        return self.memory_reader.read_memory(task, address, length)

    def write_memory(self, task: TaskSnapshot, address: int, data: bytes) -> None:
        raise NotImplementedError("the frozen probe is read-only and has no memory-write backend")

    def read_registers(self, task: TaskSnapshot) -> ProbeSavedRegisters:
        if self.register_reader is None:
            raise NotImplementedError(
                "probe package does not provide saved sleeping-task registers")
        return self.register_reader.read_registers(task)

    def write_registers(self, task: TaskSnapshot, data: bytes) -> None:
        raise NotImplementedError("probe v1 does not yet provide a register-write backend")
