"""Unix-socket server joining an upstream GDB to the inferior facade."""

from __future__ import annotations

import os
import re
import select
import socket
import stat
from dataclasses import dataclass
from typing import Callable, Protocol

from .event_stop import KernelEventStop
from .internal_breakpoints import InternalBreakpointController
from .linux_oracle import RegisterRead, TaskId
from .qemu_rsp import QemuRspClient
from .rsp_codec import Interrupt, Packet
from .rsp_proxy import FacadeState, RspFacade
from .rsp_transport import DuplexByteStream, RspDisconnected, RspStream


@dataclass(frozen=True)
class StopResolution:
    identity: TaskId
    signal: int
    address: int | None = None
    registers: RegisterRead | None = None

    def __iter__(self):
        # Preserve the original three-value resolver boundary for callers
        # which only need ordinary breakpoint attribution.
        yield self.identity
        yield self.signal
        yield self.address


StopResolver = Callable[[bytes, RspFacade], StopResolution]
InternalStopResolver = Callable[[int, int], KernelEventStop | None]


class StoppedCpuRegisterTarget(Protocol):
    """The stopped-vCPU register primitive needed for stop attribution."""

    def read_register(self, cpu: int, name: str) -> int: ...

_STOP_THREAD = re.compile(rb"thread:([^;]+);")


def selected_task_stop(packet: bytes, facade: RspFacade) -> StopResolution:
    """MVP stop resolver used until the live Linux oracle attributes a vCPU."""

    facade.refresh()
    identity = facade.continue_thread or facade.stop_thread or facade.general_thread
    if identity is None:
        raise RuntimeError("cannot attribute a stop without a selected Linux task")
    signal = 5
    if len(packet) >= 3 and packet[:1] in (b"S", b"T"):
        try:
            signal = int(packet[1:3], 16)
        except ValueError:
            pass
    return StopResolution(identity, signal)


def qemu_cpu_stop_resolver(
    qemu: QemuRspClient,
    target: StoppedCpuRegisterTarget,
    cpu_threads: tuple[str, ...],
    internal_stop: InternalStopResolver | None = None,
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

    def resolve(packet: bytes, facade: RspFacade) -> StopResolution:
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
            if internal_stop is not None:
                event = internal_stop(cpu, pc)
                if event is not None:
                    facade.merge_event_task(
                        event.task_snapshot(facade.snapshot.task(event.identity))
                    )
                    return StopResolution(
                        event.identity,
                        event.gdb_signal,
                        registers=event.registers,
                    )
        facade.refresh()
        candidates = [
            task for task in facade.snapshot.tasks if task.current_cpu == cpu
        ]
        if len(candidates) != 1:
            raise RuntimeError(
                f"probe reported {len(candidates)} current tasks on stopped CPU {cpu}"
            )
        if parsed_signal and signal == 5 and not watch_stop:
            # A plain SIGTRAP is also used for single-step.  Only opt into
            # process-breakpoint filtering when this PC is a breakpoint owned
            # by at least one process in the frozen post-stop snapshot.
            if any(
                facade.owns_breakpoint(task.identity.tgid, pc)
                for task in facade.snapshot.tasks
            ):
                address = pc
        return StopResolution(candidates[0].identity, signal, address)

    return resolve


class UnixRspServer:
    def __init__(
        self,
        path: str | None,
        facade: RspFacade,
        qemu: QemuRspClient,
        stop_resolver: StopResolver = selected_task_stop,
        internal_breakpoints: InternalBreakpointController | None = None,
    ) -> None:
        self.path = os.path.abspath(path) if path is not None else None
        self.facade = facade
        self.qemu = qemu
        self.stop_resolver = stop_resolver
        self.internal_breakpoints = internal_breakpoints

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
        if self.internal_breakpoints is not None:
            match = _STOP_THREAD.search(packet)
            if match:
                try:
                    raw_thread = match.group(1).decode("ascii")
                except UnicodeDecodeError as exc:
                    raise RuntimeError(
                        "QEMU stop has a non-ASCII thread ID"
                    ) from exc
            else:
                raw_thread = self.qemu.current_thread()
            raw_signal = 5
            if len(packet) >= 3 and packet[:1] in (b"S", b"T"):
                try:
                    raw_signal = int(packet[1:3], 16)
                except ValueError:
                    pass
            watchpoint = re.search(
                rb"(?:^|;)(?:watch|rwatch|awatch):", packet[3:]
            ) is not None
            if self.internal_breakpoints.finish_step(
                raw_thread, raw_signal, watchpoint=watchpoint
            ):
                return
        # Ordinary stops refresh inside the resolver before CPU attribution.
        # Exact-kernel event records instead carry their own task identity and
        # saved frame, avoiding nested task enumeration at a constrained
        # signal-delivery boundary.
        resolution = self.stop_resolver(packet, self.facade)
        response = self.facade.on_stop(
            resolution.identity,
            resolution.signal,
            resolution.address,
            resolution.registers,
            refresh=False,
        )
        if response is not None:
            upstream.send_packet(response)

    def serve_connection(self, sock: DuplexByteStream) -> None:
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
        if self.path is None:
            raise RuntimeError("this RSP server has no filesystem listener")
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
        if self.path is None:
            raise RuntimeError("this RSP server has no filesystem listener")
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
