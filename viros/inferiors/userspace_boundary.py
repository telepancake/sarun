"""Advance a reset-stopped QEMU to Linux's first userspace boundary.

The live inferior facade needs a normal kernel context before it can run its
reversible snapshot helper.  This module performs that one-time transition
directly through QEMU's RSP connection.  Boundary names are fixed per
supported architecture and are resolved from the exact ``vmlinux`` already
bound to :class:`RspQemuTarget`.
"""

from __future__ import annotations

from dataclasses import dataclass
import math
from pathlib import Path
import re
import time

from callgate.architectures import (
    AARCH64,
    ARMV7,
    MIPS32EL_MMIPS,
    X86_64,
    ArchitectureDescriptor,
)
from callgate.rsp_target import RspHardwareBreakpoint, RspQemuTarget
from probe.probe_tool import AuditError, ElfObject


_CONSOLE_OUTPUT = re.compile(rb"O(?:[0-9a-fA-F]{2})*\Z")
_STOP_REPLY = re.compile(rb"(?:S[0-9a-fA-F]{2}|T[0-9a-fA-F]{2}[ -~]*)\Z")
_STOP_THREAD = re.compile(rb"(?:^|;)thread:([^;]+);")


class UserspaceBoundaryError(RuntimeError):
    """QEMU could not be left at a verified Linux userspace boundary."""


class UserspaceBoundaryRestorationError(UserspaceBoundaryError):
    """Boundary advancement failed while its temporary state was restored."""

    def __init__(
        self,
        primary: BaseException | None,
        cleanup_errors: tuple[BaseException, ...],
    ) -> None:
        if not cleanup_errors:
            raise ValueError("at least one cleanup error is required")
        self.primary = primary
        self.cleanup_errors = cleanup_errors
        prefix = (
            f"userspace-boundary advancement failed ({primary}); " if primary else ""
        )
        details = "; ".join(str(error) for error in cleanup_errors)
        super().__init__(prefix + "temporary breakpoint restoration failed: " + details)


@dataclass(frozen=True)
class UserspaceBoundaryPoint:
    """One address and all accepted exact symbols which define it."""

    address: int
    symbols: tuple[str, ...]


@dataclass(frozen=True)
class UserspaceBoundaryStop:
    """The stopped CPU and verified boundary reached during kernel startup."""

    point: UserspaceBoundaryPoint
    cpu: int
    thread_id: str
    stop_reply: bytes


@dataclass(frozen=True)
class _BoundaryProfile:
    architecture: ArchitectureDescriptor
    symbols: tuple[str, ...]
    require_every_symbol: bool


@dataclass(frozen=True)
class _OwnedBreakpoint:
    kind: int
    address: int
    size: int
    target_token: RspHardwareBreakpoint | None = None


_PROFILES = (
    _BoundaryProfile(AARCH64, ("ret_to_user",), True),
    _BoundaryProfile(ARMV7, ("ret_to_user",), True),
    _BoundaryProfile(MIPS32EL_MMIPS, ("start_thread",), True),
    # A native x86-64 init reaches start_thread.  An IA32 init reaches
    # compat_start_thread.  Install whichever exact definitions this kernel
    # provides so the selected image chooses the correct path naturally.
    _BoundaryProfile(
        X86_64,
        ("start_thread", "compat_start_thread"),
        False,
    ),
)


def _profile_for(architecture: ArchitectureDescriptor) -> _BoundaryProfile:
    for profile in _PROFILES:
        if architecture is profile.architecture:
            return profile
    raise UserspaceBoundaryError(
        f"no reset-to-userspace profile for {architecture.name!r}"
    )


def resolve_userspace_boundaries(
    kernel_file: str | Path,
    architecture: ArchitectureDescriptor,
) -> tuple[UserspaceBoundaryPoint, ...]:
    """Resolve fixed, exact, defined boundary symbols from one ``vmlinux``."""

    profile = _profile_for(architecture)
    kernel = Path(kernel_file)
    try:
        records = ElfObject(kernel).symbol_records()
    except (AuditError, OSError) as exc:
        raise UserspaceBoundaryError(
            f"cannot read boundary symbols from exact vmlinux {kernel}: {exc}"
        ) from exc

    values: dict[str, set[int]] = {name: set() for name in profile.symbols}
    for record in records:
        name = record["name"]
        # Equality is intentional.  Prefixes, compiler suffixes, and similarly
        # named entry helpers are not interchangeable kernel boundaries.
        if name in values and record["shndx"] != 0:
            values[name].add(int(record["value"]))

    conflicts = {
        name: addresses for name, addresses in values.items() if len(addresses) > 1
    }
    if conflicts:
        name, addresses = next(iter(conflicts.items()))
        formatted = ", ".join(f"{address:#x}" for address in sorted(addresses))
        raise UserspaceBoundaryError(
            f"exact vmlinux has conflicting definitions of {name}: {formatted}"
        )

    missing = tuple(name for name, addresses in values.items() if not addresses)
    if profile.require_every_symbol and missing:
        raise UserspaceBoundaryError(
            "exact vmlinux lacks required defined symbol(s): " + ", ".join(missing)
        )
    if len(missing) == len(profile.symbols):
        raise UserspaceBoundaryError(
            "exact vmlinux lacks an accepted defined userspace boundary: "
            + ", ".join(profile.symbols)
        )

    address_symbols: dict[int, list[str]] = {}
    for name in profile.symbols:
        if not values[name]:
            continue
        address = next(iter(values[name]))
        if not 0 < address < 1 << architecture.address_bits:
            raise UserspaceBoundaryError(
                f"vmlinux symbol {name} has out-of-range address {address:#x}"
            )
        if address % architecture.instruction_alignment:
            raise UserspaceBoundaryError(
                f"vmlinux symbol {name} has unaligned address {address:#x}"
            )
        address_symbols.setdefault(address, []).append(name)

    return tuple(
        UserspaceBoundaryPoint(address, tuple(symbols))
        for address, symbols in address_symbols.items()
    )


def _receive_stop(client, timeout_seconds: float) -> bytes:
    """Consume console output and return one strict RSP stop reply."""

    deadline = time.monotonic() + timeout_seconds
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("timed out waiting for QEMU to stop")
        packet = client.receive_async_packet(remaining)
        if _CONSOLE_OUTPUT.fullmatch(packet):
            continue
        if _STOP_REPLY.fullmatch(packet):
            return packet
        raise UserspaceBoundaryError(
            f"unexpected QEMU startup reply: {packet!r}"
        )


def _install_breakpoint(
    target: RspQemuTarget, point: UserspaceBoundaryPoint
) -> _OwnedBreakpoint:
    if target.architecture is not X86_64:
        token = target.add_hardware_breakpoint(point.address)
        return _OwnedBreakpoint(1, token.address, token.size, token)

    # x86 TCG does not consistently provide usable Z1 hardware breakpoints.
    # QEMU's Z0 packet owns and restores the substituted instruction bytes.
    size = target.architecture.breakpoint_size
    try:
        target.client.insert_breakpoint(0, point.address, size)
    except BaseException as exc:
        raise UserspaceBoundaryError(
            f"cannot create x86 userspace-boundary breakpoint at "
            f"{point.address:#x}: {exc}"
        ) from exc
    return _OwnedBreakpoint(0, point.address, size)


def _remove_breakpoint(target: RspQemuTarget, owned: _OwnedBreakpoint) -> None:
    if owned.target_token is not None:
        target.remove_breakpoint(owned.target_token)
        return
    try:
        target.client.remove_breakpoint(
            owned.kind, owned.address, owned.size
        )
    except BaseException as exc:
        raise UserspaceBoundaryError(
            f"cannot remove x86 userspace-boundary breakpoint at "
            f"{owned.address:#x}: {exc}"
        ) from exc


def _set_target_stop_synchronized(
    target: RspQemuTarget, synchronized: bool
) -> None:
    """Keep RspQemuTarget's fail-closed state coherent for a full-VM resume."""

    # RspQemuTarget's existing bounded call-gate resume owns this state itself.
    # This one-time full-VM startup uses QemuRspClient.resume directly, so it
    # must make the same transition before any later target primitive is used.
    target._stop_synchronized = synchronized


def _stop_thread(client, stop_reply: bytes) -> str:
    match = _STOP_THREAD.search(stop_reply[3:]) if stop_reply.startswith(b"T") else None
    if match is None:
        return client.current_thread()
    try:
        return match.group(1).decode("ascii")
    except UnicodeDecodeError as exc:
        raise UserspaceBoundaryError(
            f"QEMU stop reply has a non-ASCII thread ID: {stop_reply!r}"
        ) from exc


def _verified_stop(
    target: RspQemuTarget,
    points: tuple[UserspaceBoundaryPoint, ...],
    stop_reply: bytes,
) -> UserspaceBoundaryStop:
    signal = int(stop_reply[1:3], 16)
    if signal != 5:
        raise UserspaceBoundaryError(
            f"QEMU stopped with signal {signal}, not an owned breakpoint trap"
        )
    client = target.client
    thread_id = _stop_thread(client, stop_reply)
    thread_ids = client.thread_ids()
    try:
        cpu = thread_ids.index(thread_id)
    except ValueError as exc:
        raise UserspaceBoundaryError(
            f"QEMU stopped on unknown thread {thread_id!r}"
        ) from exc

    cpu_ids = tuple(target.cpu_ids())
    if len(cpu_ids) != len(thread_ids) or cpu not in cpu_ids:
        raise UserspaceBoundaryError(
            "QEMU CPU enumeration changed while reaching userspace"
        )
    pc = target.read_register(cpu, target.architecture.pc_register)
    owned = {point.address: point for point in points}
    try:
        point = owned[pc]
    except KeyError as exc:
        expected = ", ".join(f"{address:#x}" for address in owned)
        raise UserspaceBoundaryError(
            f"QEMU stopped at {pc:#x}, not an owned userspace boundary ({expected})"
        ) from exc
    return UserspaceBoundaryStop(point, cpu, thread_id, stop_reply)


def reach_userspace_boundary(
    target: RspQemuTarget,
    *,
    timeout_seconds: float,
) -> UserspaceBoundaryStop:
    """Boot stopped QEMU to a fixed userspace-return boundary and stop it.

    This is intended to run after the target and exact kernel have been
    validated, but before construction of the first :class:`ProbeOracle`
    snapshot.  QEMU remains stopped on success and on every recoverable
    failure.  Every successfully installed temporary breakpoint is removed.
    """

    if (
        isinstance(timeout_seconds, bool)
        or not isinstance(timeout_seconds, (int, float))
        or not math.isfinite(timeout_seconds)
        or timeout_seconds <= 0
    ):
        raise ValueError("userspace-boundary timeout must be positive")

    target.assert_stopped()
    points = resolve_userspace_boundaries(target.kernel_file, target.architecture)
    client = target.client
    tokens: list[_OwnedBreakpoint] = []
    resumed = False
    stopped = True
    primary: BaseException | None = None
    result: UserspaceBoundaryStop | None = None
    cleanup_errors: list[BaseException] = []

    try:
        for point in points:
            tokens.append(_install_breakpoint(target, point))

        # Mark the local state before sending ``c``: a transport failure can
        # occur after QEMU has accepted the command, so recovery must still
        # send an interrupt and consume a stop reply.
        resumed = True
        stopped = False
        _set_target_stop_synchronized(target, False)
        client.resume()
        stop_reply = _receive_stop(client, float(timeout_seconds))
        stopped = True
        _set_target_stop_synchronized(target, True)
        result = _verified_stop(target, points, stop_reply)
    except BaseException as exc:
        primary = exc
    finally:
        if resumed and not stopped:
            try:
                client.forward_interrupt()
                client_timeout = float(getattr(client, "timeout", timeout_seconds))
                if client_timeout <= 0:
                    client_timeout = float(timeout_seconds)
                _receive_stop(client, min(float(timeout_seconds), client_timeout))
                stopped = True
                _set_target_stop_synchronized(target, True)
            except BaseException as exc:
                cleanup_errors.append(
                    UserspaceBoundaryError(
                        f"could not restore QEMU's stopped state: {exc}"
                    )
                )

        if stopped:
            for token in reversed(tokens):
                try:
                    _remove_breakpoint(target, token)
                except BaseException as exc:
                    cleanup_errors.append(exc)
        elif tokens:
            cleanup_errors.append(
                UserspaceBoundaryError(
                    "temporary breakpoints could not be removed because QEMU's "
                    "stopped state is unknown"
                )
            )

    if cleanup_errors:
        raise UserspaceBoundaryRestorationError(
            primary, tuple(cleanup_errors)
        ) from cleanup_errors[0]
    if primary is not None:
        if isinstance(primary, TimeoutError):
            raise UserspaceBoundaryError(
                f"QEMU did not reach a userspace boundary within "
                f"{float(timeout_seconds):g}s; it was interrupted and left stopped"
            ) from primary
        raise primary
    if result is None:  # pragma: no cover - defensive invariant
        raise AssertionError("userspace boundary completed without a stop record")
    return result


__all__ = [
    "UserspaceBoundaryError",
    "UserspaceBoundaryPoint",
    "UserspaceBoundaryRestorationError",
    "UserspaceBoundaryStop",
    "reach_userspace_boundary",
    "resolve_userspace_boundaries",
]
