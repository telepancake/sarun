"""Serialized client for QEMU's system-emulation GDB stub."""

from __future__ import annotations

import re
import socket
import threading
from contextlib import contextmanager
from typing import Iterator

from .rsp_transport import RspStream


class RspRemoteError(RuntimeError):
    def __init__(self, reply: bytes) -> None:
        super().__init__(f"remote RSP error: {reply.decode('ascii', errors='replace')}")
        self.reply = reply


class RspRestorationError(RuntimeError):
    """An RSP operation failed and its state restoration failed as well."""

    def __init__(self, primary: BaseException | None, cleanup: BaseException) -> None:
        self.primary = primary
        self.cleanup = cleanup
        prefix = f"RSP operation failed ({primary}); " if primary is not None else ""
        super().__init__(prefix + f"state restoration failed: {cleanup}")


_THREAD_ID = re.compile(r"(?:-1|0|[0-9a-fA-F]+|p[0-9a-fA-F]+\.(?:-1|0|[0-9a-fA-F]+))\Z")


class QemuRspClient:
    """One lock-serialized downstream RSP connection.

    QEMU 11 system mode remains in ACK mode.  Resume commands have no immediate
    response; their eventual stop packet is consumed separately by the server.
    """

    def __init__(self, sock: socket.socket, timeout: float = 5.0) -> None:
        self.stream = RspStream(sock)
        self.timeout = timeout
        self._lock = threading.RLock()
        self._vcont_actions: frozenset[str] | None = None

    @classmethod
    def connect_unix(cls, path: str, timeout: float = 5.0) -> "QemuRspClient":
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(None)
        sock.connect(path)
        return cls(sock, timeout)

    def fileno(self) -> int:
        return self.stream.fileno()

    def close(self) -> None:
        self.stream.close()

    @staticmethod
    def _checked(reply: bytes) -> bytes:
        if reply.startswith(b"E"):
            raise RspRemoteError(reply)
        return reply

    def request(self, payload: bytes | str) -> bytes:
        with self._lock:
            self.stream.send_packet(payload, self.timeout)
            return self._checked(self.stream.receive_packet(self.timeout))

    def command_no_reply(self, payload: bytes | str) -> None:
        with self._lock:
            self.stream.send_packet(payload, self.timeout)

    def receive_async_packet(self, timeout: float | None = None) -> bytes:
        with self._lock:
            return self.stream.receive_packet(timeout)

    def forward_interrupt(self) -> None:
        # A raw interrupt is allowed while a resume command has no pending RSP
        # response, so it does not need the request/reply lock.
        self.stream.send_interrupt()

    def insert_breakpoint(self, kind: int, address: int, size: int) -> None:
        reply = self.request(f"Z{kind},{address:x},{size:x}")
        if reply != b"OK":
            raise RspRemoteError(reply)

    def remove_breakpoint(self, kind: int, address: int, size: int) -> None:
        reply = self.request(f"z{kind},{address:x},{size:x}")
        if reply != b"OK":
            raise RspRemoteError(reply)

    def resume(self) -> None:
        self.command_no_reply(b"c")

    def require_vcont_action(self, action: str) -> None:
        if action not in {"c", "s"}:
            raise ValueError("unsupported vCont action")
        if self._vcont_actions is None:
            supported = self.request(b"vCont?")
            values = supported.decode("ascii", errors="replace").split(";")
            if not values or values[0] != "vCont":
                raise RspRemoteError(supported)
            self._vcont_actions = frozenset(values[1:])
        if action not in self._vcont_actions:
            raise RspRemoteError(b"vCont action " + action.encode("ascii") + b" unsupported")

    def resume_thread(self, thread_id: str) -> None:
        """Resume exactly one vCPU using QEMU's per-thread vCont support."""

        if not isinstance(thread_id, str) or not _THREAD_ID.fullmatch(thread_id):
            raise ValueError("invalid RSP thread ID")
        self.require_vcont_action("c")
        self.command_no_reply(f"vCont;c:{thread_id}")

    def step_thread(self, thread_id: str) -> None:
        """Step exactly one vCPU without changing QEMU's ``Hc`` selection."""

        if not isinstance(thread_id, str) or not _THREAD_ID.fullmatch(thread_id):
            raise ValueError("invalid RSP thread ID")
        self.require_vcont_action("s")
        self.command_no_reply(f"vCont;s:{thread_id}")

    def step(self, cpu: int) -> None:
        if not isinstance(cpu, int) or isinstance(cpu, bool) or cpu < 0:
            raise ValueError("invalid CPU number")
        threads = self.thread_ids()
        if cpu >= len(threads):
            raise ValueError("CPU number is not present")
        self.step_thread(threads[cpu])

    def monitor_command(self, command: str) -> str:
        """Run one HMP command through ``qRcmd`` and collect its output."""

        if not isinstance(command, str) or not command:
            raise ValueError("monitor command must be a nonempty string")
        encoded_command = command.encode("utf-8").hex()
        chunks: list[bytes] = []
        with self._lock:
            self.stream.send_packet(f"qRcmd,{encoded_command}", self.timeout)
            while True:
                reply = self._checked(self.stream.receive_packet(self.timeout))
                if reply == b"OK":
                    break
                if not reply.startswith(b"O"):
                    raise RspRemoteError(reply)
                try:
                    chunks.append(bytes.fromhex(reply[1:].decode("ascii")))
                except (UnicodeDecodeError, ValueError) as exc:
                    raise RspRemoteError(reply) from exc
        return b"".join(chunks).decode("utf-8", errors="replace")

    def read_xfer(self, object_name: str, annex: str, chunk_size: int = 0x800) -> bytes:
        """Read a complete qXfer object, rejecting malformed chunk markers."""

        if not object_name or not annex or chunk_size <= 0:
            raise ValueError("invalid qXfer object, annex, or chunk size")
        result = bytearray()
        while True:
            reply = self.request(
                f"qXfer:{object_name}:read:{annex}:{len(result):x},{chunk_size:x}"
            )
            if not reply or reply[:1] not in (b"m", b"l"):
                raise RspRemoteError(reply)
            if reply == b"m":
                raise RspRemoteError(reply)
            body = reply[1:]
            if len(body) > chunk_size:
                raise RspRemoteError(reply)
            result.extend(body)
            if reply[:1] == b"l":
                return bytes(result)

    def thread_ids(self) -> tuple[str, ...]:
        """Return QEMU's raw RSP thread IDs in enumeration order."""

        result: list[str] = []
        command = b"qfThreadInfo"
        while True:
            reply = self.request(command)
            if reply == b"l":
                break
            if not reply.startswith(b"m"):
                raise RspRemoteError(reply)
            try:
                values = reply[1:].decode("ascii").split(",")
            except UnicodeDecodeError as exc:
                raise RspRemoteError(reply) from exc
            if not values or any(not value for value in values):
                raise RspRemoteError(reply)
            result.extend(values)
            command = b"qsThreadInfo"
        if not result:
            raise RspRemoteError(b"no QEMU threads")
        return tuple(result)

    def current_thread(self) -> str:
        reply = self.request(b"qC")
        if not reply.startswith(b"QC") or len(reply) == 2:
            raise RspRemoteError(reply)
        try:
            return reply[2:].decode("ascii")
        except UnicodeDecodeError as exc:
            raise RspRemoteError(reply) from exc

    def select_thread(self, operation: str, thread_id: str) -> None:
        if operation not in {"g", "c"} or not isinstance(
            thread_id, str
        ) or not _THREAD_ID.fullmatch(thread_id):
            raise ValueError("invalid RSP thread selection")
        reply = self.request(f"H{operation}{thread_id}")
        if reply != b"OK":
            raise RspRemoteError(reply)

    def read_register(self, register_number: int) -> bytes:
        if register_number < 0:
            raise ValueError("register number must not be negative")
        reply = self.request(f"p{register_number:x}")
        try:
            return bytes.fromhex(reply.decode("ascii"))
        except (UnicodeDecodeError, ValueError) as exc:
            raise RspRemoteError(reply) from exc

    def read_register_block(self, thread_id: str) -> bytes:
        """Read QEMU's complete ``g`` register block for one stopped vCPU.

        The previous general-register selection is restored before returning.
        This is intentionally a vCPU primitive; mapping a Linux task to that
        vCPU remains the oracle's responsibility.
        """

        if not isinstance(thread_id, str) or not _THREAD_ID.fullmatch(thread_id):
            raise ValueError("invalid RSP thread ID")
        with self._lock:
            previous = self.current_thread()
            primary: BaseException | None = None
            result = b""
            try:
                self.select_thread("g", thread_id)
                reply = self.request(b"g")
                try:
                    result = bytes.fromhex(reply.decode("ascii"))
                except (UnicodeDecodeError, ValueError) as exc:
                    raise RspRemoteError(reply) from exc
            except BaseException as exc:
                primary = exc
            finally:
                try:
                    self.select_thread("g", previous)
                except BaseException as cleanup:
                    raise RspRestorationError(primary, cleanup) from cleanup
            if primary is not None:
                raise primary
            return result

    def write_register(self, register_number: int, value: bytes) -> None:
        if register_number < 0 or not value:
            raise ValueError("invalid register number or value")
        reply = self.request(f"P{register_number:x}={value.hex()}")
        if reply != b"OK":
            raise RspRemoteError(reply)

    def query_physical_mode(self) -> bool:
        reply = self.request(b"qqemu.PhyMemMode")
        if reply not in (b"0", b"1"):
            raise RspRemoteError(reply)
        return reply == b"1"

    def set_physical_mode(self, enabled: bool) -> None:
        reply = self.request(f"Qqemu.PhyMemMode:{int(enabled)}")
        if reply != b"OK":
            raise RspRemoteError(reply)

    @contextmanager
    def physical_memory(self) -> Iterator[None]:
        """Temporarily select physical memory, restoring observable state."""

        with self._lock:
            original = self.query_physical_mode()
            primary: BaseException | None = None
            try:
                if not original:
                    self.set_physical_mode(True)
                yield
            except BaseException as exc:
                primary = exc
            finally:
                # Set explicitly to turn restoration into an acknowledged RSP
                # operation even when the target was already in physical mode.
                try:
                    self.set_physical_mode(original)
                except BaseException as cleanup:
                    raise RspRestorationError(primary, cleanup) from cleanup
            if primary is not None:
                raise primary

    def _read_memory(self, address: int, length: int) -> bytes:
        encoded = self.request(f"m{address:x},{length:x}")
        try:
            data = bytes.fromhex(encoded.decode("ascii"))
        except (UnicodeDecodeError, ValueError) as exc:
            raise RspRemoteError(encoded) from exc
        if len(data) != length:
            raise RspRemoteError(encoded)
        return data

    def _write_memory(self, address: int, data: bytes) -> None:
        reply = self.request(f"M{address:x},{len(data):x}:{data.hex()}")
        if reply != b"OK":
            raise RspRemoteError(reply)

    def read_physical(self, address: int, length: int) -> bytes:
        """Read physical memory and restore QEMU's previous memory mode."""

        if address < 0 or length <= 0:
            raise ValueError("invalid physical read")
        with self.physical_memory():
            result = bytearray()
            while len(result) < length:
                size = min(0x400, length - len(result))
                result.extend(self._read_memory(address + len(result), size))
            return bytes(result)

    def write_physical(self, address: int, data: bytes) -> None:
        """Write physical memory and restore QEMU's previous memory mode."""

        if address < 0 or not data:
            raise ValueError("invalid physical write")
        with self.physical_memory():
            for offset in range(0, len(data), 0x400):
                self._write_memory(address + offset, data[offset : offset + 0x400])
