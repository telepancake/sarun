"""Asynchronous step-over control for facade-owned QEMU breakpoints.

An internal breakpoint is reported to the facade as a meaningful debugger
event.  When the user continues, QEMU must first execute the instruction at
that address without immediately stopping at the same breakpoint again.  This
module owns that small state machine; transport and upstream packet handling
remain outside it.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum, auto
from typing import Iterable, Protocol


class InternalBreakpointBackend(Protocol):
    """The QEMU operations needed by the internal step-over sequence."""

    def insert_breakpoint(self, kind: int, address: int, size: int) -> None: ...

    def remove_breakpoint(self, kind: int, address: int, size: int) -> None: ...

    def step_thread(self, thread_id: str) -> None: ...

    def resume(self) -> None: ...


@dataclass(frozen=True, order=True)
class InternalBreakpoint:
    """One breakpoint owned exclusively by the facade."""

    address: int
    size: int
    kind: int = 1

    def __post_init__(self) -> None:
        if (
            isinstance(self.address, bool)
            or not isinstance(self.address, int)
            or self.address < 0
        ):
            raise ValueError("internal breakpoint address must be nonnegative")
        if (
            isinstance(self.size, bool)
            or not isinstance(self.size, int)
            or self.size <= 0
        ):
            raise ValueError("internal breakpoint size must be positive")
        if (
            isinstance(self.kind, bool)
            or not isinstance(self.kind, int)
            or self.kind not in {0, 1}
        ):
            raise ValueError("internal breakpoint kind must be software or hardware")


class InternalBreakpointState(Enum):
    NEW = auto()
    READY = auto()
    STOPPED = auto()
    STEPPING = auto()
    CLOSED = auto()
    FAILED = auto()


@dataclass(frozen=True)
class InternalBreakpointFailure:
    """A failed operation and any failures encountered while restoring state."""

    operation: str
    primary: BaseException
    restoration: tuple[BaseException, ...] = ()


class InternalBreakpointError(RuntimeError):
    """The controller could not complete an operation safely."""

    def __init__(self, failure: InternalBreakpointFailure) -> None:
        self.failure = failure
        suffix = ""
        if failure.restoration:
            suffix = f"; {len(failure.restoration)} restoration operation(s) failed"
        super().__init__(f"{failure.operation} failed: {failure.primary}{suffix}")


@dataclass(frozen=True)
class InternalBreakpointStop:
    """The raw QEMU stop currently represented by an internal event."""

    thread_id: str
    breakpoint: InternalBreakpoint


class InternalBreakpointController:
    """Own internal breakpoints and hide their continue-time step stop."""

    def __init__(
        self,
        backend: InternalBreakpointBackend,
        breakpoints: Iterable[InternalBreakpoint],
    ) -> None:
        configured = tuple(breakpoints)
        if not configured:
            raise ValueError("at least one internal breakpoint is required")
        if not all(isinstance(item, InternalBreakpoint) for item in configured):
            raise TypeError("breakpoints must be InternalBreakpoint values")
        if len(configured) != len(set(configured)):
            raise ValueError("internal breakpoints must be unique")
        addresses = tuple(item.address for item in configured)
        if len(addresses) != len(set(addresses)):
            raise ValueError("internal breakpoint addresses must be unique")

        self.backend = backend
        self.breakpoints = configured
        self.state = InternalBreakpointState.NEW
        self.failure: InternalBreakpointFailure | None = None
        self.stop: InternalBreakpointStop | None = None
        self._installed: set[InternalBreakpoint] = set()
        self._by_address = {item.address: item for item in configured}

    @property
    def installed_breakpoints(self) -> tuple[InternalBreakpoint, ...]:
        """Return only breakpoints known to have completed QEMU insertion."""

        return tuple(item for item in self.breakpoints if item in self._installed)

    def owns_address(self, address: int) -> bool:
        """Whether ``address`` belongs to this controller, never to a user set."""

        return address in self._by_address

    def _raise_failed(self) -> None:
        if self.failure is None:
            raise RuntimeError("internal breakpoint controller has no failure record")
        raise InternalBreakpointError(self.failure)

    def _require_not_failed(self) -> None:
        if self.state is InternalBreakpointState.FAILED:
            self._raise_failed()

    def _fail(
        self,
        operation: str,
        primary: BaseException,
        restoration: Iterable[BaseException] = (),
    ) -> None:
        self.failure = InternalBreakpointFailure(
            operation, primary, tuple(restoration)
        )
        self.state = InternalBreakpointState.FAILED
        self._raise_failed()

    @staticmethod
    def _insert(
        backend: InternalBreakpointBackend, breakpoint: InternalBreakpoint
    ) -> None:
        backend.insert_breakpoint(
            breakpoint.kind, breakpoint.address, breakpoint.size
        )

    @staticmethod
    def _remove(
        backend: InternalBreakpointBackend, breakpoint: InternalBreakpoint
    ) -> None:
        backend.remove_breakpoint(
            breakpoint.kind, breakpoint.address, breakpoint.size
        )

    def _restore(
        self, breakpoints: Iterable[InternalBreakpoint]
    ) -> list[BaseException]:
        failures: list[BaseException] = []
        for breakpoint in breakpoints:
            try:
                self._insert(self.backend, breakpoint)
            except BaseException as exc:
                failures.append(exc)
            else:
                self._installed.add(breakpoint)
        return failures

    def install(self) -> None:
        """Install every configured breakpoint exactly once."""

        self._require_not_failed()
        if self.state is not InternalBreakpointState.NEW:
            raise RuntimeError("internal breakpoints can only be installed once")
        inserted: list[InternalBreakpoint] = []
        try:
            for breakpoint in self.breakpoints:
                self._insert(self.backend, breakpoint)
                self._installed.add(breakpoint)
                inserted.append(breakpoint)
        except BaseException as primary:
            restoration: list[BaseException] = []
            for breakpoint in reversed(inserted):
                try:
                    self._remove(self.backend, breakpoint)
                except BaseException as exc:
                    restoration.append(exc)
                else:
                    self._installed.discard(breakpoint)
            self._fail("install internal breakpoints", primary, restoration)
        self.state = InternalBreakpointState.READY

    def note_stop(
        self,
        thread_id: str,
        address: int,
        *,
        signal: int = 5,
        watchpoint: bool = False,
    ) -> bool:
        """Claim a plain trap at an owned address and remember its raw thread.

        False leaves the stop entirely to the ordinary user-breakpoint path.
        In particular, addresses not configured here are never inferred from
        or added to the controller's ownership set.
        """

        self._require_not_failed()
        if self.state is not InternalBreakpointState.READY:
            raise RuntimeError("controller is not ready to recognize a stop")
        if signal != 5 or watchpoint:
            return False
        breakpoint = self._by_address.get(address)
        if breakpoint is None:
            return False
        if not isinstance(thread_id, str) or not thread_id:
            raise ValueError("stopped QEMU thread ID must be a nonempty string")
        if breakpoint not in self._installed:
            raise RuntimeError("owned breakpoint is not installed")
        self.stop = InternalBreakpointStop(thread_id, breakpoint)
        self.state = InternalBreakpointState.STOPPED
        return True

    def begin_continue(self) -> bool:
        """Start the internal remove-and-step sequence if one is pending.

        True means the controller sent the thread-specific step and the caller
        must not also send an ordinary QEMU continue.  False means there is no
        internal stop and the caller retains normal continue handling.
        """

        self._require_not_failed()
        if self.state is InternalBreakpointState.READY:
            return False
        if self.state is not InternalBreakpointState.STOPPED or self.stop is None:
            raise RuntimeError("controller cannot continue in its current state")

        breakpoint = self.stop.breakpoint
        try:
            self._remove(self.backend, breakpoint)
        except BaseException as primary:
            self._fail("remove internal breakpoint before step", primary)
        self._installed.discard(breakpoint)

        try:
            self.backend.step_thread(self.stop.thread_id)
        except BaseException as primary:
            restoration = self._restore((breakpoint,))
            self._fail("single-step stopped QEMU thread", primary, restoration)
        self.state = InternalBreakpointState.STEPPING
        return True

    def finish_step(
        self,
        thread_id: str,
        signal: int,
        *,
        watchpoint: bool = False,
    ) -> bool:
        """Restore the removed breakpoint and classify the resulting stop.

        A plain SIGTRAP from the stepped raw thread is internal bookkeeping: it
        is followed by an ordinary QEMU resume and True tells the caller not to
        report it upstream.  Any different stop is returned to normal handling
        after the breakpoint has been restored.
        """

        self._require_not_failed()
        if self.state is not InternalBreakpointState.STEPPING:
            return False
        if self.stop is None:
            raise RuntimeError("stepping state lacks its stopped-thread record")

        restoration = self._restore((self.stop.breakpoint,))
        if restoration:
            self._fail(
                "restore internal breakpoint after step",
                restoration[0],
                restoration[1:],
            )

        housekeeping = (
            thread_id == self.stop.thread_id and signal == 5 and not watchpoint
        )
        if not housekeeping:
            self.stop = None
            self.state = InternalBreakpointState.READY
            return False

        try:
            self.backend.resume()
        except BaseException as primary:
            self._fail("resume after internal breakpoint step", primary)
        self.stop = None
        self.state = InternalBreakpointState.READY
        return True

    def uninstall(self) -> None:
        """Remove all currently installed internal breakpoints."""

        self._require_not_failed()
        if self.state not in {
            InternalBreakpointState.READY,
            InternalBreakpointState.STOPPED,
        }:
            raise RuntimeError("controller cannot uninstall in its current state")
        failures: list[BaseException] = []
        for breakpoint in reversed(self.breakpoints):
            if breakpoint not in self._installed:
                continue
            try:
                self._remove(self.backend, breakpoint)
            except BaseException as exc:
                failures.append(exc)
            else:
                self._installed.discard(breakpoint)
        if failures:
            self._fail("uninstall internal breakpoints", failures[0], failures[1:])
        self.stop = None
        self.state = InternalBreakpointState.CLOSED
