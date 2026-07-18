"""Version-neutral data contract between Linux introspection and RSP.

The eventual live oracle may use an injected probe, DWARF/BTF, or a mixture of
both.  The RSP facade deliberately depends only on the frozen values below.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol


@dataclass(frozen=True)
class RegisterRead:
    """An already encoded, unframed GDB ``g``-packet reply.

    Literal ``x`` digits mark described registers whose values are unavailable.
    ``complete`` converts an ordinary raw register block into the same form.
    """

    payload: bytes

    def __post_init__(self) -> None:
        if (not isinstance(self.payload, bytes) or len(self.payload) % 2
                or any(byte not in b"0123456789abcdefx" for byte in self.payload)):
            raise ValueError("register reply must contain lowercase hex/x digit pairs")

    @classmethod
    def complete(cls, data: bytes) -> "RegisterRead":
        if not isinstance(data, bytes):
            raise TypeError("complete register data must be bytes")
        return cls(data.hex().encode("ascii"))


@dataclass(frozen=True, order=True)
class TaskId:
    """A GDB multiprocess identity: Linux TGID is PID, Linux PID is TID."""

    tgid: int
    tid: int

    def __post_init__(self) -> None:
        if self.tgid <= 0 or self.tid <= 0:
            raise ValueError("TGID and TID must be positive")

    def rsp(self) -> str:
        return f"p{self.tgid:x}.{self.tid:x}"


@dataclass(frozen=True)
class TaskSnapshot:
    identity: TaskId
    task_cookie: int
    comm: str
    executable: str
    auxv: bytes
    state: str = "stopped"
    current_cpu: int | None = None
    page_table_root: int | None = None


@dataclass(frozen=True)
class Snapshot:
    generation: int
    tasks: tuple[TaskSnapshot, ...]

    def task(self, identity: TaskId) -> TaskSnapshot | None:
        return next(
            (task for task in self.tasks if task.identity == identity), None
        )

    def process_task(self, tgid: int) -> TaskSnapshot | None:
        candidates = (task for task in self.tasks if task.identity.tgid == tgid)
        return min(candidates, key=lambda task: task.identity.tid, default=None)


class LinuxOracle(Protocol):
    """Operations required by the facade while the emulated VM is stopped."""

    def snapshot(self) -> Snapshot:
        """Return one internally consistent, frozen view of userspace tasks."""

    def read_memory(self, task: TaskSnapshot, address: int, length: int) -> bytes:
        """Read virtual memory in TASK's address space."""

    def write_memory(self, task: TaskSnapshot, address: int, data: bytes) -> None:
        """Write virtual memory in TASK's address space."""

    def read_registers(self, task: TaskSnapshot) -> RegisterRead | bytes:
        """Return an encoded reply, or a legacy complete raw register block."""

    def write_registers(self, task: TaskSnapshot, data: bytes) -> None:
        """Replace the available general register block for TASK."""
