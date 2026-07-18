"""GDB/QEMU implementation of the call-gate transaction target protocol.

The backend accepts the GDB module as a constructor argument so every state
transition can be tested without importing or launching GDB.
"""

from __future__ import annotations

from contextlib import contextmanager
import hashlib
from pathlib import Path
import re
from typing import Any, Iterator, Sequence

from .architectures import AARCH64, ArchitectureDescriptor


_GPA = re.compile(r"\bgpa:\s*(0x[0-9a-fA-F]+)\b")
_RECEIVED = re.compile(r"received:\s*[\"']?([^\"'\r\n]+)")
_CURRENT_HMP_CPU = re.compile(r"^\s*\*\s*CPU\s+#(\d+)\b", re.MULTILINE)


class GdbTargetError(RuntimeError):
    """The live GDB/QEMU backend could not safely complete an operation."""


class PhysicalModeRestorationError(GdbTargetError):
    """An operation failed and QEMU's original memory mode was not restored."""

    def __init__(self, primary: BaseException | None, restoration: BaseException):
        self.primary = primary
        self.restoration = restoration
        prefix = f"physical-memory operation failed ({primary}); " if primary else ""
        super().__init__(prefix + f"could not restore QEMU memory mode: {restoration}")


class HardwareBreakpoint:
    """Opaque ownership token; only this backend may delete the breakpoint."""

    def __init__(self, breakpoint: Any, address: int):
        self.breakpoint = breakpoint
        self.address = address
        self.removed = False


def _sha256_file(path: str | Path) -> str:
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


class GdbQemuTarget:
    """A stopped all-stop QEMU target controlled through GDB Python."""

    supports_bounded_resume = False

    def __init__(
        self,
        gdb_module: Any,
        architecture: ArchitectureDescriptor = AARCH64,
    ):
        if not isinstance(architecture, ArchitectureDescriptor):
            raise TypeError("architecture must be an ArchitectureDescriptor")
        self.gdb = gdb_module
        self.architecture = architecture

    def _inferior(self):
        inferior = self.gdb.selected_inferior()
        if inferior is None:
            raise GdbTargetError("GDB has no selected inferior")
        return inferior

    def _threads(self) -> tuple[Any, ...]:
        threads = tuple(self._inferior().threads())
        if not threads:
            raise GdbTargetError("the selected inferior exposes no QEMU CPUs")
        return tuple(sorted(threads, key=lambda thread: getattr(thread, "global_num", thread.num)))

    def _thread_for_cpu(self, cpu: int):
        threads = self._threads()
        if not isinstance(cpu, int) or isinstance(cpu, bool) or cpu < 0 or cpu >= len(threads):
            raise GdbTargetError(f"QEMU CPU {cpu!r} is not present")
        return threads[cpu]

    @contextmanager
    def _selected_cpu(self, cpu: int) -> Iterator[Any]:
        previous = self.gdb.selected_thread()
        thread = self._thread_for_cpu(cpu)
        thread.switch()
        try:
            yield thread
        finally:
            if previous is not None:
                try:
                    if not hasattr(previous, "is_valid") or previous.is_valid():
                        previous.switch()
                except Exception as exc:
                    raise GdbTargetError(f"could not restore the selected GDB thread: {exc}") from exc

    def assert_stopped(self) -> None:
        running = []
        for cpu, thread in enumerate(self._threads()):
            try:
                if thread.is_running():
                    running.append(cpu)
            except Exception as exc:
                raise GdbTargetError(f"cannot query QEMU CPU {cpu} state: {exc}") from exc
        if running:
            raise GdbTargetError("all QEMU CPUs must be stopped; running: " + ", ".join(map(str, running)))

    def cpu_ids(self) -> Sequence[int]:
        return tuple(range(len(self._threads())))

    def verify_kernel(self, path: str, sha256: str, build_id: str) -> None:
        expected = Path(path).resolve()
        progspace = self.gdb.current_progspace()
        loaded_name = getattr(progspace, "filename", None)
        if not loaded_name:
            raise GdbTargetError("GDB has no kernel symbol file loaded")
        loaded = Path(loaded_name).resolve()
        if loaded != expected:
            raise GdbTargetError(
                f"loaded kernel symbols are {loaded}, manifest requires {expected}"
            )
        actual = _sha256_file(loaded)
        if actual != sha256:
            raise GdbTargetError(
                f"loaded kernel SHA-256 is {actual}, manifest requires {sha256}"
            )
        # SHA-256 binds the exact symbol file.  The build ID is reserved for a
        # later guest-memory/probe handshake; this backend does not pretend it
        # can infer that identity from the stopped guest yet.
        if not build_id:
            raise GdbTargetError("manifest kernel build ID is empty")

    def _hmp_current_cpu(self) -> int:
        try:
            output = self.gdb.execute(
                "monitor info cpus", from_tty=False, to_string=True
            )
        except Exception as exc:
            raise GdbTargetError(f"cannot query QEMU's current HMP CPU: {exc}") from exc
        match = _CURRENT_HMP_CPU.search(output or "")
        if not match:
            raise GdbTargetError(
                "QEMU monitor did not identify its current CPU in 'info cpus' output"
            )
        return int(match.group(1))

    def translate_virtual(self, cpu: int, virtual_address: int) -> int:
        """Translate one GVA while restoring both GDB and HMP CPU selection."""

        previous_hmp: int | None = None
        primary: BaseException | None = None
        output = ""
        with self._selected_cpu(cpu):
            try:
                # HMP keeps its own current-CPU selection; changing the GDB
                # thread alone does not affect ``gva2gpa``.
                previous_hmp = self._hmp_current_cpu()
                self.gdb.execute(
                    f"monitor cpu {cpu}", from_tty=False, to_string=True
                )
                output = self.gdb.execute(
                    f"monitor gva2gpa {virtual_address:#x}",
                    from_tty=False,
                    to_string=True,
                )
            except BaseException as exc:
                primary = exc
            finally:
                if previous_hmp is not None:
                    try:
                        self.gdb.execute(
                            f"monitor cpu {previous_hmp}",
                            from_tty=False,
                            to_string=True,
                        )
                    except BaseException as cleanup:
                        raise GdbTargetError(
                            f"mapping query failed ({primary}); could not restore "
                            f"HMP CPU {previous_hmp}: {cleanup}"
                        ) from cleanup
        if primary is not None:
            raise GdbTargetError(
                f"QEMU could not translate {virtual_address:#x}: {primary}"
            ) from primary
        match = _GPA.search(output or "")
        if not match:
            raise GdbTargetError(
                f"QEMU did not report a GPA for {virtual_address:#x}: {(output or '').strip()}"
            )
        return int(match.group(1), 16)

    def verify_mapping(self, cpu: int, virtual_address: int, physical_address: int) -> None:
        actual = self.translate_virtual(cpu, virtual_address)
        if actual != physical_address:
            raise GdbTargetError(
                f"mapping mismatch for {virtual_address:#x}: QEMU reports {actual:#x}, "
                f"manifest requires {physical_address:#x}"
            )

    def _query_physical_mode(self) -> bool:
        try:
            output = self.gdb.execute(
                "maintenance packet qqemu.PhyMemMode",
                from_tty=False,
                to_string=True,
            )
        except Exception as exc:
            raise GdbTargetError(f"cannot query QEMU physical memory mode: {exc}") from exc
        match = _RECEIVED.search(output or "")
        if not match:
            raise GdbTargetError(
                "QEMU physical-memory-mode query returned an unrecognized response: "
                + (output or "").strip()
            )
        value = match.group(1).strip()
        if value not in {"0", "1"}:
            raise GdbTargetError(f"QEMU physical-memory-mode query returned {value!r}")
        return value == "1"

    def _set_physical_mode(self, enabled: bool) -> None:
        value = int(enabled)
        try:
            output = self.gdb.execute(
                f"maintenance packet Qqemu.PhyMemMode:{value}",
                from_tty=False,
                to_string=True,
            )
        except Exception as exc:
            raise GdbTargetError(f"cannot set QEMU physical memory mode to {value}: {exc}") from exc
        match = _RECEIVED.search(output or "")
        if not match or match.group(1).strip() != "OK":
            raise GdbTargetError(
                f"QEMU rejected physical memory mode {value}: {(output or '').strip()}"
            )

    @contextmanager
    def _physical_memory(self) -> Iterator[None]:
        original = self._query_physical_mode()
        primary: BaseException | None = None
        try:
            if not original:
                self._set_physical_mode(True)
            yield
        except BaseException as exc:
            primary = exc
        finally:
            try:
                # Set explicitly even when it was already physical.  This
                # audits that QEMU remains responsive after every operation.
                self._set_physical_mode(original)
            except BaseException as exc:
                raise PhysicalModeRestorationError(primary, exc) from exc
        if primary is not None:
            raise primary

    def read_physical(self, address: int, size: int) -> bytes:
        if address < 0 or size <= 0:
            raise GdbTargetError("physical read address/size is invalid")
        with self._physical_memory():
            try:
                return self._inferior().read_memory(address, size).tobytes()
            except Exception as exc:
                raise GdbTargetError(
                    f"cannot read {size} physical bytes at {address:#x}: {exc}"
                ) from exc

    def write_physical(self, address: int, data: bytes) -> None:
        if address < 0 or not data:
            raise GdbTargetError("physical write address/data is invalid")
        with self._physical_memory():
            try:
                self._inferior().write_memory(address, data, len(data))
            except Exception as exc:
                raise GdbTargetError(
                    f"cannot write {len(data)} physical bytes at {address:#x}: {exc}"
                ) from exc

    def read_register(self, cpu: int, name: str) -> int:
        try:
            bits = self.architecture.register_bits(name)
        except KeyError as exc:
            raise GdbTargetError(
                f"unsupported {self.architecture.display_name} core register: {name}"
            ) from exc
        with self._selected_cpu(cpu):
            try:
                self._select_innermost_frame()
                # GDB exposes registers with their target-defined signedness;
                # kernel pointers commonly arrive as negative Python ints.
                # Preserve the descriptor-declared architectural bit pattern.
                return int(self.gdb.parse_and_eval("$" + name)) & ((1 << bits) - 1)
            except Exception as exc:
                raise GdbTargetError(f"cannot read CPU {cpu} register {name}: {exc}") from exc

    def write_register(self, cpu: int, name: str, value: int) -> None:
        try:
            bits = self.architecture.register_bits(name)
        except KeyError as exc:
            raise GdbTargetError(
                f"unsupported {self.architecture.display_name} core register: {name}"
            ) from exc
        if (
            not isinstance(value, int)
            or isinstance(value, bool)
            or value < 0
            or value >= 1 << bits
        ):
            raise GdbTargetError(f"invalid value for register {name}: {value!r}")
        with self._selected_cpu(cpu):
            try:
                self._select_innermost_frame()
                self.gdb.execute(
                    f"set ${name} = {value:#x}", from_tty=False, to_string=True
                )
            except Exception as exc:
                raise GdbTargetError(f"cannot write CPU {cpu} register {name}: {exc}") from exc

    def _select_innermost_frame(self) -> None:
        """Keep register writes attached to the actual stopped CPU frame."""

        newest_frame = getattr(self.gdb, "newest_frame", None)
        if newest_frame is None:
            return
        frame = newest_frame()
        if frame is not None:
            frame.select()

    def add_hardware_breakpoint(self, address: int) -> HardwareBreakpoint:
        try:
            breakpoint = self.gdb.Breakpoint(
                f"*{address:#x}",
                type=self.gdb.BP_HARDWARE_BREAKPOINT,
                internal=True,
                temporary=False,
            )
        except Exception as exc:
            raise GdbTargetError(f"cannot create completion breakpoint at {address:#x}: {exc}") from exc
        return HardwareBreakpoint(breakpoint, address)

    def remove_breakpoint(self, token: HardwareBreakpoint) -> None:
        if not isinstance(token, HardwareBreakpoint):
            raise GdbTargetError("breakpoint token was not created by this backend")
        if token.removed:
            return
        try:
            if not hasattr(token.breakpoint, "is_valid") or token.breakpoint.is_valid():
                token.breakpoint.delete()
            token.removed = True
        except Exception as exc:
            raise GdbTargetError(
                f"cannot remove completion breakpoint at {token.address:#x}: {exc}"
            ) from exc

    def _scheduler_locking(self) -> str:
        value = self.gdb.parameter("scheduler-locking")
        if isinstance(value, bool):
            return "on" if value else "off"
        return str(value)

    def run_cpu_until(self, cpu: int, address: int, timeout_seconds: float) -> None:
        if timeout_seconds <= 0:
            raise GdbTargetError("resume timeout must be positive")
        original_locking = self._scheduler_locking()
        primary: BaseException | None = None
        with self._selected_cpu(cpu):
            try:
                self.gdb.execute("set scheduler-locking on", from_tty=False, to_string=True)
                # With stock GDB/QEMU, Python cannot query InferiorThread state
                # while a background remote continue is active.  Synchronous
                # continue is therefore the only restorable MVP primitive: it
                # returns after the completion breakpoint (or another stop).
                # ``timeout_seconds`` remains a manifest safety declaration,
                # but cannot be enforced until QEMU provides a bounded-run
                # packet or the backend gains a safe out-of-band interrupter.
                self.gdb.execute("continue", from_tty=False, to_string=True)
                actual = self.read_register(cpu, self.architecture.pc_register)
                if actual != address:
                    raise GdbTargetError(
                        f"QEMU CPU {cpu} stopped at {actual:#x}, expected {address:#x}"
                    )
            except BaseException as exc:
                primary = exc
            finally:
                try:
                    self.gdb.execute(
                        f"set scheduler-locking {original_locking}",
                        from_tty=False,
                        to_string=True,
                    )
                except BaseException as cleanup:
                    raise GdbTargetError(
                        f"resume failed ({primary}); cleanup also failed: {cleanup}"
                    ) from cleanup
        if primary is not None:
            raise primary
