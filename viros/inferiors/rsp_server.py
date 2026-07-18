"""Unix-socket server joining an upstream GDB to the inferior facade."""

from __future__ import annotations

import os
import re
import select
import socket
import stat
from typing import Callable, Protocol

from .linux_oracle import TaskId
from .qemu_rsp import QemuRspClient
from .rsp_codec import Interrupt, Packet
from .rsp_proxy import FacadeState, RspFacade
from .rsp_transport import RspDisconnected, RspStream


StopResolver = Callable[[bytes, RspFacade], tuple[TaskId, int, int | None]]


class StoppedCpuRegisterTarget(Protocol):
    """The stopped-vCPU register primitive needed for stop attribution."""

    def read_register(self, cpu: int, name: str) -> int: ...

_STOP_THREAD = re.compile(rb"thread:([^;]+);")


def selected_task_stop(packet: bytes, facade: RspFacade) -> tuple[TaskId, int, None]:
    """MVP stop resolver used until the live Linux oracle attributes a vCPU."""

    identity = facade.continue_thread or facade.stop_thread or facade.general_thread
    if identity is None:
        raise RuntimeError("cannot attribute a stop without a selected Linux task")
    signal = 5
    if len(packet) >= 3 and packet[:1] in (b"S", b"T"):
        try:
            signal = int(packet[1:3], 16)
        except ValueError:
            pass
    return identity, signal, None


def qemu_cpu_stop_resolver(
    qemu: QemuRspClient,
    target: StoppedCpuRegisterTarget,
    cpu_threads: tuple[str, ...],
) -> StopResolver:
    """Resolve a QEMU vCPU stop to the task reported current by the probe.

    QEMU breakpoints are global to the translated virtual address, while the
    facade presents them as process-owned.  For a SIGTRAP at an address where
    the facade has a breakpoint, include the stopped vCPU's actual PC so
    :meth:`RspFacade.on_stop` can auto-resume an unowned process hit.  Other
    signals (notably a user interrupt) and trap stops unrelated to an installed
    breakpoint deliberately carry no address and must be reported upstream.
    """

    by_thread = {thread: cpu for cpu, thread in enumerate(cpu_threads)}

    def resolve(packet: bytes, facade: RspFacade) -> tuple[TaskId, int, int | None]:
        match = _STOP_THREAD.search(packet)
        if match:
            try:
                thread = match.group(1).decode("ascii")
            except UnicodeDecodeError as exc:
                raise RuntimeError("QEMU stop has a non-ASCII thread ID") from exc
        else:
            thread = qemu.current_thread()
        try:
            cpu = by_thread[thread]
        except KeyError as exc:
            raise RuntimeError(f"QEMU stopped on unknown vCPU thread {thread!r}") from exc
        candidates = [
            task for task in facade.snapshot.tasks if task.current_cpu == cpu
        ]
        if len(candidates) != 1:
            raise RuntimeError(
                f"probe reported {len(candidates)} current tasks on stopped CPU {cpu}"
            )
        signal = 5
        parsed_signal = False
        if len(packet) >= 3 and packet[:1] in (b"S", b"T"):
            try:
                signal = int(packet[1:3], 16)
                parsed_signal = True
            except ValueError:
                pass

        address: int | None = None
        details = packet[3:] if len(packet) >= 3 else b""
        watch_stop = re.search(
            rb"(?:^|;)(?:watch|rwatch|awatch):", details
        ) is not None
        if parsed_signal and signal == 5 and not watch_stop:
            pc = target.read_register(cpu, "pc")
            # A plain SIGTRAP is also used for single-step.  Only opt into
            # process-breakpoint filtering when this PC is a breakpoint owned
            # by at least one process in the frozen post-stop snapshot.
            if any(
                facade.owns_breakpoint(task.identity.tgid, pc)
                for task in facade.snapshot.tasks
            ):
                address = pc
        return candidates[0].identity, signal, address

    return resolve


class UnixRspServer:
    def __init__(
        self,
        path: str,
        facade: RspFacade,
        qemu: QemuRspClient,
        stop_resolver: StopResolver = selected_task_stop,
    ) -> None:
        self.path = os.path.abspath(path)
        self.facade = facade
        self.qemu = qemu
        self.stop_resolver = stop_resolver

    def _handle_upstream(self, upstream: RspStream) -> bool:
        event = upstream.receive_event()
        if isinstance(event, Interrupt):
            self.qemu.forward_interrupt()
            return True
        if not isinstance(event, Packet):
            return True
        upstream.send_ack(True)
        if self.facade.state is FacadeState.RUNNING:
            upstream.send_packet(b"E01")
            return True
        response = self.facade.handle(event.payload)
        if response is not None:
            upstream.send_packet(response)
        return True

    def _handle_downstream(self, upstream: RspStream) -> None:
        packet = self.qemu.receive_async_packet()
        if packet.startswith(b"O"):
            upstream.send_packet(packet)
            return
        # Re-run the read-only probe after execution stopped, before mapping
        # QEMU's vCPU identity to a Linux task.  on_stop() then consumes this
        # exact frozen snapshot rather than probing a second time.
        self.facade.refresh()
        identity, signal, address = self.stop_resolver(packet, self.facade)
        response = self.facade.on_stop(identity, signal, address, refresh=False)
        if response is not None:
            upstream.send_packet(response)

    def serve_connection(self, sock: socket.socket) -> None:
        upstream = RspStream(sock)
        try:
            while True:
                if self.facade.state is FacadeState.RUNNING:
                    readable, _, _ = select.select(
                        [upstream.socket, self.qemu.stream.socket], [], []
                    )
                    if upstream.socket in readable:
                        self._handle_upstream(upstream)
                    if self.qemu.stream.socket in readable:
                        self._handle_downstream(upstream)
                else:
                    self._handle_upstream(upstream)
        except RspDisconnected:
            return
        finally:
            upstream.close()

    def _listener(self) -> socket.socket:
        try:
            mode = os.stat(self.path).st_mode
        except FileNotFoundError:
            pass
        else:
            if not stat.S_ISSOCK(mode):
                raise FileExistsError(f"refusing to replace non-socket {self.path}")
            os.unlink(self.path)
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            listener.bind(self.path)
            listener.listen(1)
            return listener
        except BaseException:
            listener.close()
            raise

    def serve_once(self) -> None:
        listener = self._listener()
        try:
            connection, _ = listener.accept()
            self.serve_connection(connection)
        finally:
            listener.close()
            try:
                os.unlink(self.path)
            except FileNotFoundError:
                pass
