"""Incremental codec for the GDB Remote Serial Protocol byte stream."""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Packet:
    payload: bytes


@dataclass(frozen=True)
class Ack:
    positive: bool


@dataclass(frozen=True)
class Interrupt:
    pass


@dataclass(frozen=True)
class InvalidPacket:
    reason: str


Event = Packet | Ack | Interrupt | InvalidPacket


def _escape(payload: bytes) -> bytes:
    encoded = bytearray()
    for byte in payload:
        if byte in b"#$}*":
            encoded.extend((ord("}"), byte ^ 0x20))
        else:
            encoded.append(byte)
    return bytes(encoded)


def _unescape(encoded: bytes) -> bytes:
    payload = bytearray()
    index = 0
    while index < len(encoded):
        byte = encoded[index]
        if byte == ord("}"):
            index += 1
            if index == len(encoded):
                raise ValueError("truncated escape")
            payload.append(encoded[index] ^ 0x20)
        elif byte == ord("*"):
            index += 1
            if not payload or index == len(encoded):
                raise ValueError("invalid run-length encoding")
            repetitions = encoded[index] - 29
            if repetitions < 3:
                raise ValueError("invalid run-length count")
            payload.extend(bytes((payload[-1],)) * repetitions)
        else:
            payload.append(byte)
        index += 1
    return bytes(payload)


def frame_packet(payload: bytes | str) -> bytes:
    """Frame PAYLOAD, escaping bytes and checksumming transmitted content."""

    if isinstance(payload, str):
        payload = payload.encode("ascii")
    encoded = _escape(payload)
    checksum = sum(encoded) & 0xFF
    return b"$" + encoded + f"#{checksum:02x}".encode("ascii")


class RspCodec:
    """Consume arbitrarily chunked RSP traffic and emit logical events."""

    def __init__(self) -> None:
        self._state = "idle"
        self._encoded = bytearray()
        self._checksum = bytearray()

    def _reset(self) -> None:
        self._state = "idle"
        self._encoded.clear()
        self._checksum.clear()

    def feed(self, data: bytes) -> list[Event]:
        events: list[Event] = []
        for byte in data:
            if self._state == "idle":
                if byte == ord("$"):
                    self._encoded.clear()
                    self._checksum.clear()
                    self._state = "payload"
                elif byte == ord("+"):
                    events.append(Ack(True))
                elif byte == ord("-"):
                    events.append(Ack(False))
                elif byte == 0x03:
                    events.append(Interrupt())
                # Ignore transport noise outside a packet.
            elif self._state == "payload":
                if byte == ord("#"):
                    self._state = "checksum"
                elif byte == ord("$"):
                    # Resynchronize immediately on a new packet marker.
                    self._encoded.clear()
                    self._checksum.clear()
                else:
                    self._encoded.append(byte)
            else:
                self._checksum.append(byte)
                if len(self._checksum) != 2:
                    continue
                try:
                    received = int(self._checksum.decode("ascii"), 16)
                except ValueError:
                    events.append(InvalidPacket("non-hex checksum"))
                    self._reset()
                    continue
                actual = sum(self._encoded) & 0xFF
                if received != actual:
                    events.append(
                        InvalidPacket(
                            f"checksum mismatch: got {received:02x}, "
                            f"expected {actual:02x}"
                        )
                    )
                    self._reset()
                    continue
                try:
                    events.append(Packet(_unescape(bytes(self._encoded))))
                except ValueError as exc:
                    events.append(InvalidPacket(str(exc)))
                self._reset()
        return events
