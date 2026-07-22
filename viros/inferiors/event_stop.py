"""Read one exact-kernel userspace event from a stopped QEMU CPU."""

from __future__ import annotations

from dataclasses import dataclass
import struct
from typing import Callable, Protocol

from callgate.architectures import ArchitectureDescriptor

from .kernel_events import (
    EVENT_HEADER_BYTES,
    EVENT_MAX_BYTES,
    KernelEvent,
    KernelEventDecodeError,
    EVENT_KERNEL_DIE,
    decode_kernel_event,
)
from .linux_oracle import RegisterRead, TaskId, TaskSnapshot


class EventQemuClient(Protocol):
    def read_virtual_memory(
        self,
        thread_id: str,
        address: int,
        length: int,
        *,
        address_bits: int,
    ) -> bytes: ...


class EventRegisterTarget(Protocol):
    def read_register(self, cpu: int, name: str) -> int: ...


EventRegisterEncoder = Callable[[KernelEvent], RegisterRead]
EventSignalMapper = Callable[[int], int]


class KernelEventReadError(RuntimeError):
    """The stopped CPU did not expose a valid debugger event."""


@dataclass(frozen=True)
class KernelEventStop:
    event: KernelEvent
    identity: TaskId
    gdb_signal: int
    registers: RegisterRead
    record_address: int

    def task_snapshot(self, previous: TaskSnapshot | None = None) -> TaskSnapshot:
        """Represent the event owner, preserving prior ELF metadata when exact."""

        cookie = (self.event.task << 64) | self.event.start_cookie
        reusable = previous is not None and previous.task_cookie == cookie
        return TaskSnapshot(
            identity=self.identity,
            task_cookie=cookie,
            comm=self.event.comm,
            executable=previous.executable if reusable else "",
            auxv=previous.auxv if reusable else b"",
            state=(
                "kernel-die"
                if self.event.kind == EVENT_KERNEL_DIE
                else f"signal={self.event.signal}"
            ),
            current_cpu=self.event.cpu,
            page_table_root=previous.page_table_root if reusable else None,
        )


def read_kernel_event_stop(
    *,
    qemu: EventQemuClient,
    target: EventRegisterTarget,
    cpu_threads: tuple[str, ...],
    cpu: int,
    architecture: ArchitectureDescriptor,
    event_arch: int,
    event_register_count: int,
    encode_registers: EventRegisterEncoder,
    map_signal: EventSignalMapper,
) -> KernelEventStop:
    """Read, validate, and present the event named by the first ABI argument."""

    if not 0 <= cpu < len(cpu_threads):
        raise KernelEventReadError(f"event stopped on nonexistent QEMU CPU {cpu}")
    try:
        record_address = target.read_register(
            cpu, architecture.argument_registers[0]
        )
        header = qemu.read_virtual_memory(
            cpu_threads[cpu],
            record_address,
            EVENT_HEADER_BYTES,
            address_bits=architecture.address_bits,
        )
        prefix = "<" if architecture.target_byte_order == "little" else ">"
        record_size = struct.unpack_from(prefix + "I", header, 16)[0]
        if not EVENT_HEADER_BYTES <= record_size <= EVENT_MAX_BYTES:
            raise KernelEventDecodeError("event record advertises an invalid size")
        record = (
            header
            if record_size == EVENT_HEADER_BYTES
            else header
            + qemu.read_virtual_memory(
                cpu_threads[cpu],
                record_address + EVENT_HEADER_BYTES,
                record_size - EVENT_HEADER_BYTES,
                address_bits=architecture.address_bits,
            )
        )
        event = decode_kernel_event(
            record,
            byte_order=architecture.target_byte_order,
            expected_arch=event_arch,
            expected_pointer_bits=architecture.address_bits,
            expected_registers=event_register_count,
        )
        if event.cpu != cpu:
            raise KernelEventDecodeError(
                f"event names CPU {event.cpu}, but QEMU stopped CPU {cpu}"
            )
        gdb_signal = map_signal(event.signal)
        if not 0 < gdb_signal <= 0xFF:
            raise KernelEventDecodeError("event signal has no usable GDB mapping")
        registers = encode_registers(event)
        return KernelEventStop(
            event=event,
            identity=TaskId(event.tgid, event.tid),
            gdb_signal=gdb_signal,
            registers=registers,
            record_address=record_address,
        )
    except KernelEventReadError:
        raise
    except Exception as exc:
        raise KernelEventReadError(f"cannot read stopped kernel event: {exc}") from exc
