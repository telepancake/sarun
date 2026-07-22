"""Socket transport primitives for the GDB Remote Serial Protocol."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass
import select
from typing import Deque, Protocol

from .rsp_codec import Ack, Event, Interrupt, InvalidPacket, Packet, RspCodec, frame_packet


@dataclass(frozen=True)
class _AcknowledgedPacket:
    payload: bytes


class RspDisconnected(ConnectionError):
    pass


class DuplexByteStream(Protocol):
    """The byte-stream operations RSP needs.

    A socket implements this directly. Sarun's generic service relay instead
    presents the two directions as child stdout and child stdin.
    """

    def fileno(self) -> int: ...

    def recv(self, size: int) -> bytes: ...

    def sendall(self, data: bytes) -> None: ...

    def close(self) -> None: ...


class RspStream:
    """An ACK-mode RSP stream over an already connected socket.

    Packet acknowledgement is explicit on receive and automatic while waiting
    for an acknowledgement of a packet sent by this endpoint.
    """

    def __init__(self, sock: DuplexByteStream) -> None:
        self.socket = sock
        self.codec = RspCodec()
        self._events: Deque[Event | _AcknowledgedPacket] = deque()

    def fileno(self) -> int:
        return self.socket.fileno()

    def close(self) -> None:
        self.socket.close()

    def send_ack(self, positive: bool = True) -> None:
        self.socket.sendall(b"+" if positive else b"-")

    def send_interrupt(self) -> None:
        self.socket.sendall(b"\x03")

    def _fill(self, timeout: float | None) -> None:
        readable, _, _ = select.select([self.socket], [], [], timeout)
        if not readable:
            raise TimeoutError("timed out waiting for RSP traffic")
        data = self.socket.recv(65536)
        if not data:
            raise RspDisconnected("RSP peer disconnected")
        self._events.extend(self.codec.feed(data))

    def receive_event(self, timeout: float | None = None) -> Event | _AcknowledgedPacket:
        while True:
            if not self._events:
                self._fill(timeout)
            event = self._events.popleft()
            if isinstance(event, InvalidPacket):
                self.send_ack(False)
                continue
            return event

    def receive_packet(self, timeout: float | None = None) -> bytes:
        """Return the next packet, positively acknowledging it."""

        while True:
            event = self.receive_event(timeout)
            if isinstance(event, _AcknowledgedPacket):
                return event.payload
            if isinstance(event, Packet):
                self.send_ack(True)
                return event.payload

    def send_packet(
        self, payload: bytes | str, timeout: float | None = None, retries: int = 3
    ) -> None:
        """Send one packet and wait for its ACK, retransmitting on NACK."""

        framed = frame_packet(payload)
        deferred: list[Event] = []
        try:
            for _ in range(retries + 1):
                self.socket.sendall(framed)
                while True:
                    event = self.receive_event(timeout)
                    if isinstance(event, Ack):
                        if event.positive:
                            return
                        break
                    if isinstance(event, Packet):
                        # A fast peer may send its response adjacent to the
                        # ACK.  Acknowledge now and preserve it for the caller.
                        self.send_ack(True)
                        deferred.append(_AcknowledgedPacket(event.payload))
                    else:
                        deferred.append(event)
            raise ConnectionError("RSP peer repeatedly rejected the packet")
        finally:
            self._events.extendleft(reversed(deferred))
