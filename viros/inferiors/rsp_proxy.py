"""Multiprocess RSP facade backed by a frozen Linux task oracle."""

from __future__ import annotations

import re
from dataclasses import dataclass
from enum import Enum, auto
from html import escape
from typing import Protocol

from .linux_oracle import LinuxOracle, Snapshot, TaskId, TaskSnapshot


class QemuBackend(Protocol):
    """Execution and global translated-code breakpoints supplied by QEMU."""

    def insert_breakpoint(self, kind: int, address: int, size: int) -> None: ...

    def remove_breakpoint(self, kind: int, address: int, size: int) -> None: ...

    def resume(self) -> None: ...

    def step(self, cpu: int) -> None: ...


class FacadeState(Enum):
    STOPPED = auto()
    RUNNING = auto()


@dataclass(frozen=True, order=True)
class BreakpointKey:
    kind: int
    address: int
    size: int


_THREAD_ID = re.compile(r"^p([0-9a-fA-F]+)\.(-1|[0-9a-fA-F]+)$")
_QXFER = re.compile(
    rb"^qXfer:([^:]+):read:([^:]*):([0-9a-fA-F]+),([0-9a-fA-F]+)$"
)
_BREAKPOINT = re.compile(rb"^([Zz])([01]),([0-9a-fA-F]+),([0-9a-fA-F]+)$")


class RspFacade:
    """Packet-level MVP; transport framing lives in :mod:`rsp_codec`."""

    def __init__(
        self,
        oracle: LinuxOracle,
        qemu: QemuBackend,
        target_xml: bytes,
        packet_size: int = 4096,
        target_descriptions: dict[bytes, bytes] | None = None,
    ) -> None:
        self.oracle = oracle
        self.qemu = qemu
        self.target_xml = target_xml
        self.target_descriptions = dict(target_descriptions or {})
        self.target_descriptions.setdefault(b"target.xml", target_xml)
        self.packet_size = packet_size
        self.state = FacadeState.STOPPED
        self.snapshot = self._snapshot()
        # GDB commonly reads registers immediately after connecting.  Prefer a
        # task which the probe reports as physically current on a stopped vCPU;
        # sleeping tasks deliberately have no register block in probe ABI v1.
        first_task = self._preferred_task()
        first = first_task.identity if first_task else None
        self.general_thread: TaskId | None = first
        self.continue_thread: TaskId | None = first
        self.stop_thread: TaskId | None = first
        self._thread_xml = b""
        self._breakpoints: dict[BreakpointKey, set[int]] = {}

    def _snapshot(self) -> Snapshot:
        snapshot = self.oracle.snapshot()
        ordered = tuple(sorted(snapshot.tasks, key=lambda task: task.identity))
        return Snapshot(snapshot.generation, ordered)

    def refresh(self) -> Snapshot:
        self.snapshot = self._snapshot()
        return self.snapshot

    def _task(self, identity: TaskId | None = None) -> TaskSnapshot | None:
        identity = identity or self.general_thread
        return self.snapshot.task(identity) if identity else None

    def _preferred_task(self) -> TaskSnapshot | None:
        return next(
            (task for task in self.snapshot.tasks if task.current_cpu is not None),
            self.snapshot.tasks[0] if self.snapshot.tasks else None,
        )

    @staticmethod
    def _parse_thread_id(text: str) -> tuple[int, int] | None:
        match = _THREAD_ID.fullmatch(text)
        if not match:
            return None
        tgid = int(match.group(1), 16)
        tid = -1 if match.group(2) == "-1" else int(match.group(2), 16)
        return tgid, tid

    def _resolve_thread(self, text: str, allow_all: bool = False) -> TaskId | None:
        if text in ("0", "-1"):
            if text == "-1" and allow_all:
                return None
            preferred = self._preferred_task()
            return preferred.identity if preferred else None
        parsed = self._parse_thread_id(text)
        if parsed is None:
            raise ValueError("invalid thread id")
        tgid, tid = parsed
        if tid == -1:
            task = self.snapshot.process_task(tgid)
        else:
            task = self.snapshot.task(TaskId(tgid, tid))
        if task is None:
            raise LookupError("unknown thread")
        return task.identity

    def _make_threads_xml(self) -> bytes:
        parts = ['<?xml version="1.0"?><threads>']
        # The VM has not run since this snapshot was frozen.  Re-probing for
        # each of GDB's overlapping enumeration queries would only repeat the
        # reversible call gate and cannot discover a legitimate task change.
        for task in self.snapshot.tasks:
            attrs = [
                f'id="{task.identity.rsp()}"',
                f'name="{escape(task.comm, quote=True)}"',
            ]
            if task.current_cpu is not None:
                attrs.append(f'core="{task.current_cpu}"')
            body = escape(task.state)
            parts.append(f"<thread {' '.join(attrs)}>{body}</thread>")
        parts.append("</threads>")
        return "".join(parts).encode("utf-8")

    @staticmethod
    def _xfer_chunk(data: bytes, offset: int, length: int) -> bytes:
        if offset >= len(data):
            return b"l"
        end = min(offset + length, len(data))
        marker = b"l" if end == len(data) else b"m"
        return marker + data[offset:end]

    def _handle_xfer(self, payload: bytes) -> bytes | None:
        match = _QXFER.fullmatch(payload)
        if not match:
            return None
        obj, annex = match.group(1), match.group(2)
        offset, length = int(match.group(3), 16), int(match.group(4), 16)
        if length == 0:
            return b"E01"
        if obj == b"threads":
            if offset == 0 or not self._thread_xml:
                self._thread_xml = self._make_threads_xml()
            return self._xfer_chunk(self._thread_xml, offset, length)
        if obj == b"features":
            description = self.target_descriptions.get(annex)
            return b"" if description is None else self._xfer_chunk(
                description, offset, length
            )
        if obj == b"auxv" and not annex:
            task = self._task()
            return b"E01" if task is None else self._xfer_chunk(task.auxv, offset, length)
        if obj == b"exec-file":
            try:
                tgid = int(annex, 16) if annex else self.general_thread.tgid
            except (TypeError, ValueError, AttributeError):
                return b"E01"
            task = self.snapshot.process_task(tgid)
            if task is None:
                return b"E01"
            return self._xfer_chunk(task.executable.encode(), offset, length)
        return b""

    def _handle_breakpoint(self, payload: bytes) -> bytes | None:
        match = _BREAKPOINT.fullmatch(payload)
        if not match:
            return None
        if self.general_thread is None:
            return b"E01"
        insert = match.group(1) == b"Z"
        key = BreakpointKey(
            int(match.group(2)), int(match.group(3), 16), int(match.group(4), 16)
        )
        tgid = self.general_thread.tgid
        if insert:
            owners = self._breakpoints.setdefault(key, set())
            if not owners:
                self.qemu.insert_breakpoint(key.kind, key.address, key.size)
            owners.add(tgid)
        else:
            owners = self._breakpoints.get(key)
            if owners is None or tgid not in owners:
                return b"OK"
            owners.discard(tgid)
            if not owners:
                self._breakpoints.pop(key, None)
                self.qemu.remove_breakpoint(key.kind, key.address, key.size)
        return b"OK"

    def owns_breakpoint(self, tgid: int, address: int) -> bool:
        return any(
            key.address == address and tgid in owners
            for key, owners in self._breakpoints.items()
        )

    def on_stop(
        self,
        identity: TaskId,
        signal: int = 5,
        address: int | None = None,
        *,
        refresh: bool = True,
    ) -> bytes | None:
        """Attribute a downstream all-stop event, swallowing false PID hits."""

        if refresh:
            self.refresh()
        if self.snapshot.task(identity) is None:
            return None
        if address is not None and not self.owns_breakpoint(identity.tgid, address):
            self.qemu.resume()
            self.state = FacadeState.RUNNING
            return None
        self.stop_thread = identity
        self.general_thread = identity
        self.state = FacadeState.STOPPED
        return f"T{signal:02x}thread:{identity.rsp()};".encode("ascii")

    def handle(self, payload: bytes) -> bytes | None:
        """Handle one decoded packet; None means execution has resumed."""

        if payload.startswith(b"qSupported"):
            return (
                f"PacketSize={self.packet_size:x};multiprocess+;"
                "qXfer:features:read+;qXfer:threads:read+;"
                "qXfer:exec-file:read+;qXfer:auxv:read+;vContSupported+"
            ).encode("ascii")
        if payload == b"?":
            if self.stop_thread is None:
                return b"S05"
            return f"T05thread:{self.stop_thread.rsp()};".encode("ascii")
        xfer = self._handle_xfer(payload)
        if xfer is not None:
            return xfer
        breakpoint = self._handle_breakpoint(payload)
        if breakpoint is not None:
            return breakpoint
        if payload == b"qfThreadInfo":
            ids = ",".join(task.identity.rsp() for task in self.snapshot.tasks)
            return ("m" + ids).encode("ascii") if ids else b"l"
        if payload == b"qsThreadInfo":
            return b"l"
        if payload == b"qC":
            return b"QC" + (self.general_thread.rsp().encode() if self.general_thread else b"0")
        if payload.startswith(b"qAttached"):
            return b"1"
        if payload.startswith((b"Hg", b"Hc")):
            general = payload[1:2] == b"g"
            try:
                identity = self._resolve_thread(
                    payload[2:].decode("ascii"), allow_all=not general
                )
            except (UnicodeDecodeError, ValueError, LookupError):
                return b"E01"
            if general:
                self.general_thread = identity
            else:
                self.continue_thread = identity
            return b"OK"
        if payload.startswith(b"T"):
            try:
                identity = self._resolve_thread(payload[1:].decode("ascii"))
            except (UnicodeDecodeError, ValueError, LookupError):
                return b"E01"
            return b"OK" if identity is not None else b"E01"
        if payload == b"g":
            task = self._task()
            if task is None:
                return b"E01"
            try:
                return self.oracle.read_registers(task).hex().encode()
            except (NotImplementedError, OSError):
                return b"E14"
        if payload.startswith(b"p"):
            # The live probe oracle has one honest register primitive: QEMU's
            # complete `g` block for a task currently resident on a vCPU.
            # Per-register reads require target-description offset handling
            # and sleeping-register capture, neither of which ABI v1 supplies.
            return b"E14"
        if payload.startswith((b"G", b"P")):
            return b"E14"
        if payload.startswith(b"m"):
            try:
                address_text, length_text = payload[1:].split(b",", 1)
                address, length = int(address_text, 16), int(length_text, 16)
                task = self._task()
                if task is None:
                    return b"E01"
                return self.oracle.read_memory(task, address, length).hex().encode()
            except (ValueError, OSError, NotImplementedError):
                return b"E14"
        if payload.startswith((b"M", b"X")):
            return b"E14"
        if payload == b"vCont?":
            return b"vCont;c;s"
        if payload.startswith(b"vCont;c") or payload == b"c":
            if payload.startswith(b"vCont;c:"):
                try:
                    self.continue_thread = self._resolve_thread(
                        payload.split(b":", 1)[1].decode("ascii"), allow_all=True
                    )
                except (UnicodeDecodeError, ValueError, LookupError):
                    return b"E01"
            self.qemu.resume()
            self.state = FacadeState.RUNNING
            return None
        if payload.startswith(b"vCont;s") or payload == b"s":
            identity = self.continue_thread
            if payload.startswith(b"vCont;s:"):
                try:
                    identity = self._resolve_thread(
                        payload.split(b":", 1)[1].decode("ascii")
                    )
                except (UnicodeDecodeError, ValueError, LookupError):
                    return b"E01"
            task = self._task(identity)
            if task is None or task.current_cpu is None:
                return b"E01"
            self.qemu.step(task.current_cpu)
            self.state = FacadeState.RUNNING
            return None
        if payload.startswith(b"D"):
            return b"OK"
        return b""
