"""A reversible-callgate target implemented directly over QEMU's RSP stub.

This adapter deliberately does not depend on GDB.  It is suitable for the
downstream side of the Linux-inferior facade, where that facade must remain the
only RSP client connected to QEMU.
"""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
from pathlib import Path
import posixpath
import re
import time
from typing import Iterator, Sequence
import xml.etree.ElementTree as ET

from inferiors.qemu_rsp import QemuRspClient

from .transaction import AARCH64_REGISTERS


_GPA = re.compile(r"\bgpa:\s*(0x[0-9a-fA-F]+)\b")
_CURRENT_HMP_CPU = re.compile(r"^\s*\*\s*CPU\s+#(\d+)\b", re.MULTILINE)


class RspTargetError(RuntimeError):
    """QEMU's RSP target could not safely perform a call-gate operation."""


@dataclass(frozen=True)
class _Register:
    number: int
    bits: int


@dataclass
class RspHardwareBreakpoint:
    address: int
    size: int = 4
    removed: bool = False


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


class RspQemuTarget:
    """Stopped all-stop AArch64 QEMU controlled through normal RSP packets."""

    # This is a host wall-clock bound: direct socket ownership lets this
    # backend send ^C and wait for the corresponding stop packet.  It is not
    # an emulated-instruction budget.
    supports_bounded_resume = True

    def __init__(
        self,
        client: QemuRspClient,
        kernel_file: str | Path,
        kernel_build_id: str,
    ) -> None:
        self.client = client
        self.kernel_file = Path(kernel_file).resolve()
        self.kernel_build_id = kernel_build_id.lower()
        self._threads: tuple[str, ...] | None = None
        self._register_map: dict[str, _Register] | None = None
        self._stop_synchronized = True

    def _require_stop_synchronized(self) -> None:
        if not self._stop_synchronized:
            raise RspTargetError(
                "QEMU stop state is unknown after a failed resume/interrupt; "
                "refusing further target access"
            )

    def assert_stopped(self) -> None:
        self._require_stop_synchronized()
        try:
            reply = self.client.request(b"?")
        except TimeoutError as exc:
            raise RspTargetError("QEMU did not answer a stopped-state query") from exc
        if reply[:1] not in (b"S", b"T"):
            raise RspTargetError(f"QEMU is not at a resumable stop: {reply!r}")

    def _thread_ids(self) -> tuple[str, ...]:
        if self._threads is None:
            try:
                self._threads = self.client.thread_ids()
            except Exception as exc:
                raise RspTargetError(f"cannot enumerate QEMU CPUs: {exc}") from exc
        return self._threads

    def cpu_ids(self) -> Sequence[int]:
        return tuple(range(len(self._thread_ids())))

    def _thread_for_cpu(self, cpu: int) -> str:
        threads = self._thread_ids()
        if not isinstance(cpu, int) or isinstance(cpu, bool) or not 0 <= cpu < len(threads):
            raise RspTargetError(f"QEMU CPU {cpu!r} is not present")
        return threads[cpu]

    def verify_kernel(self, path: str, sha256: str, build_id: str) -> None:
        required = Path(path).resolve()
        if required != self.kernel_file:
            raise RspTargetError(
                f"target kernel metadata names {self.kernel_file}, manifest requires {required}"
            )
        if not self.kernel_file.is_file():
            raise RspTargetError(f"target kernel file is missing: {self.kernel_file}")
        actual = _sha256_file(self.kernel_file)
        if actual != sha256:
            raise RspTargetError(
                f"target kernel SHA-256 is {actual}, manifest requires {sha256}"
            )
        if not build_id or build_id.lower() != self.kernel_build_id:
            raise RspTargetError(
                f"target kernel build ID is {self.kernel_build_id}, manifest requires {build_id}"
            )

    def _hmp_current_cpu(self) -> int:
        output = self.client.monitor_command("info cpus")
        match = _CURRENT_HMP_CPU.search(output)
        if not match:
            raise RspTargetError(
                "QEMU monitor did not identify its current CPU in 'info cpus' output"
            )
        return int(match.group(1))

    def translate_virtual(self, cpu: int, virtual_address: int) -> int:
        """Translate one GVA while restoring QEMU's HMP CPU selection."""

        self._require_stop_synchronized()
        self._thread_for_cpu(cpu)
        previous: int | None = None
        primary: BaseException | None = None
        output = ""
        try:
            previous = self._hmp_current_cpu()
            self.client.monitor_command(f"cpu {cpu}")
            output = self.client.monitor_command(f"gva2gpa {virtual_address:#x}")
        except BaseException as exc:
            primary = exc
        finally:
            if previous is not None:
                try:
                    self.client.monitor_command(f"cpu {previous}")
                except BaseException as cleanup:
                    raise RspTargetError(
                        f"mapping query failed ({primary}); could not restore HMP CPU {previous}: {cleanup}"
                    ) from cleanup
        if primary is not None:
            raise RspTargetError(
                f"QEMU could not translate {virtual_address:#x}: {primary}"
            ) from primary
        match = _GPA.search(output)
        if not match:
            raise RspTargetError(
                f"QEMU did not report a GPA for {virtual_address:#x}: {output.strip()}"
            )
        return int(match.group(1), 16)

    def verify_mapping(self, cpu: int, virtual_address: int, physical_address: int) -> None:
        actual = self.translate_virtual(cpu, virtual_address)
        if actual != physical_address:
            raise RspTargetError(
                f"mapping mismatch for {virtual_address:#x}: QEMU reports {actual:#x}, "
                f"manifest requires {physical_address:#x}"
            )

    def read_physical(self, address: int, size: int) -> bytes:
        self._require_stop_synchronized()
        if address < 0 or size <= 0:
            raise RspTargetError("physical read address/size is invalid")
        try:
            return self.client.read_physical(address, size)
        except Exception as exc:
            raise RspTargetError(
                f"cannot read {size} physical bytes at {address:#x}: {exc}"
            ) from exc

    def write_physical(self, address: int, data: bytes) -> None:
        self._require_stop_synchronized()
        if address < 0 or not data:
            raise RspTargetError("physical write address/data is invalid")
        try:
            self.client.write_physical(address, data)
        except Exception as exc:
            raise RspTargetError(
                f"cannot write {len(data)} physical bytes at {address:#x}: {exc}"
            ) from exc

    def _load_register_map(self) -> dict[str, _Register]:
        registers: dict[str, _Register] = {}
        visiting: set[str] = set()
        next_number = 0
        architecture: str | None = None

        def visit(annex: str) -> None:
            nonlocal next_number, architecture
            if annex in visiting:
                raise RspTargetError(f"cyclic QEMU target-description include: {annex}")
            visiting.add(annex)
            try:
                document = self.client.read_xfer("features", annex)
                root = ET.fromstring(document)
            except RspTargetError:
                raise
            except Exception as exc:
                raise RspTargetError(
                    f"cannot read QEMU target-description annex {annex!r}: {exc}"
                ) from exc

            def walk(element: ET.Element) -> None:
                nonlocal next_number, architecture
                local = element.tag.rsplit("}", 1)[-1]
                if local == "architecture" and element.text:
                    architecture = element.text.strip()
                elif local == "include":
                    href = element.attrib.get("href")
                    if not href:
                        raise RspTargetError("target-description include has no href")
                    if any(key.rsplit("}", 1)[-1] not in {"href", "parse"} for key in element.attrib):
                        raise RspTargetError("unsupported target-description include attributes")
                    if element.attrib.get("parse", "xml") != "xml":
                        raise RspTargetError("target-description include is not XML")
                    if any(character in href for character in (":", "?", "#")) or href.startswith("/"):
                        raise RspTargetError(
                            f"unsafe target-description include href: {href!r}"
                        )
                    included = posixpath.normpath(
                        posixpath.join(posixpath.dirname(annex), href)
                    )
                    if included == ".." or included.startswith("../"):
                        raise RspTargetError(
                            f"unsafe target-description include href: {href!r}"
                        )
                    visit(included)
                    return
                elif local == "reg":
                    name = element.attrib.get("name")
                    bits_text = element.attrib.get("bitsize")
                    if not name or not bits_text:
                        raise RspTargetError("target-description register is incomplete")
                    try:
                        bits = int(bits_text, 10)
                        number = int(element.attrib.get("regnum", str(next_number)), 0)
                    except ValueError as exc:
                        raise RspTargetError(
                            f"invalid target-description register {name!r}"
                        ) from exc
                    if bits <= 0 or bits % 8 or number < next_number:
                        raise RspTargetError(
                            f"invalid size or regnum for target-description register {name!r}"
                        )
                    next_number = number + 1
                    if name in registers:
                        raise RspTargetError(f"duplicate target register {name!r}")
                    registers[name] = _Register(number, bits)
                for child in element:
                    walk(child)

            walk(root)
            visiting.remove(annex)

        visit("target.xml")
        if architecture not in {"aarch64", "aarch64:little"}:
            raise RspTargetError(
                f"QEMU target architecture is {architecture!r}, expected AArch64"
            )
        missing = set(AARCH64_REGISTERS) - registers.keys()
        if missing:
            raise RspTargetError(
                "QEMU target description lacks registers: " + ", ".join(sorted(missing))
            )
        for name in AARCH64_REGISTERS:
            expected = 32 if name == "cpsr" else 64
            if registers[name].bits != expected:
                raise RspTargetError(
                    f"QEMU describes {name} as {registers[name].bits} bits, expected {expected}"
                )
        return registers

    def _register(self, name: str) -> _Register:
        if name not in AARCH64_REGISTERS:
            raise RspTargetError(f"unsupported AArch64 core register: {name}")
        if self._register_map is None:
            self._register_map = self._load_register_map()
        return self._register_map[name]

    @contextmanager
    def _selected_general_cpu(self, cpu: int) -> Iterator[None]:
        selected = self._thread_for_cpu(cpu)
        primary: BaseException | None = None
        try:
            previous = self.client.current_thread()
            self.client.select_thread("g", selected)
            yield
        except BaseException as exc:
            primary = exc
        finally:
            if "previous" in locals():
                try:
                    self.client.select_thread("g", previous)
                except BaseException as cleanup:
                    raise RspTargetError(
                        f"register operation failed ({primary}); could not restore Hg selection: {cleanup}"
                    ) from cleanup
        if primary is not None:
            raise primary

    def read_register(self, cpu: int, name: str) -> int:
        self._require_stop_synchronized()
        register = self._register(name)
        try:
            with self._selected_general_cpu(cpu):
                raw = self.client.read_register(register.number)
        except Exception as exc:
            raise RspTargetError(f"cannot read CPU {cpu} register {name}: {exc}") from exc
        if len(raw) != register.bits // 8:
            raise RspTargetError(
                f"QEMU returned {len(raw)} bytes for {name}, expected {register.bits // 8}"
            )
        return int.from_bytes(raw, "little")

    def write_register(self, cpu: int, name: str, value: int) -> None:
        self._require_stop_synchronized()
        register = self._register(name)
        if (
            not isinstance(value, int)
            or isinstance(value, bool)
            or value < 0
            or value >= 1 << register.bits
        ):
            raise RspTargetError(f"invalid value for register {name}: {value!r}")
        try:
            with self._selected_general_cpu(cpu):
                self.client.write_register(
                    register.number, value.to_bytes(register.bits // 8, "little")
                )
        except Exception as exc:
            raise RspTargetError(f"cannot write CPU {cpu} register {name}: {exc}") from exc

    def add_hardware_breakpoint(self, address: int) -> RspHardwareBreakpoint:
        self._require_stop_synchronized()
        token = RspHardwareBreakpoint(address)
        try:
            self.client.insert_breakpoint(1, address, token.size)
        except Exception as exc:
            raise RspTargetError(
                f"cannot create completion breakpoint at {address:#x}: {exc}"
            ) from exc
        return token

    def remove_breakpoint(self, token: RspHardwareBreakpoint) -> None:
        self._require_stop_synchronized()
        if not isinstance(token, RspHardwareBreakpoint):
            raise RspTargetError("breakpoint token was not created by this backend")
        if token.removed:
            return
        try:
            self.client.remove_breakpoint(1, token.address, token.size)
            token.removed = True
        except Exception as exc:
            raise RspTargetError(
                f"cannot remove completion breakpoint at {token.address:#x}: {exc}"
            ) from exc

    def _receive_stop(self, timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for QEMU to stop")
            packet = self.client.receive_async_packet(remaining)
            if packet.startswith(b"O"):
                continue
            if packet[:1] in (b"S", b"T"):
                self._stop_synchronized = True
                return packet
            raise RspTargetError(f"unexpected QEMU resume reply: {packet!r}")

    def run_cpu_until(self, cpu: int, address: int, timeout_seconds: float) -> None:
        if timeout_seconds <= 0:
            raise RspTargetError("resume timeout must be positive")
        thread = self._thread_for_cpu(cpu)
        previous = self.client.current_thread()
        primary: BaseException | None = None
        try:
            self.client.select_thread("g", thread)
            # Plain ``c`` resumes the entire VM in QEMU system emulation.
            # vCont with one explicit thread is the stock packet that keeps
            # the other snapshotted vCPUs stopped during injection.
            self.client.require_vcont_action("c")
            self._stop_synchronized = False
            self.client.resume_thread(thread)
            try:
                self._receive_stop(timeout_seconds)
            except BaseException as exc:
                # Unlike the GDB Python backend, this adapter owns the socket
                # and can issue an out-of-band interrupt.  This applies to
                # Ctrl-C as well as wall-clock expiry: do not unwind into the
                # restoration transaction until a stop packet is consumed.
                try:
                    self.client.forward_interrupt()
                    self._receive_stop(self.client.timeout)
                except BaseException as synchronization:
                    raise RspTargetError(
                        f"QEMU stop synchronization failed after {exc}: {synchronization}"
                    ) from synchronization
                if isinstance(exc, TimeoutError):
                    raise RspTargetError(
                        f"QEMU CPU {cpu} exceeded the {timeout_seconds:g}s "
                        "call-gate timeout; interrupted"
                    ) from exc
                raise
            actual = self.read_register(cpu, "pc")
            if actual != address:
                raise RspTargetError(
                    f"QEMU CPU {cpu} stopped at {actual:#x}, expected {address:#x}"
                )
        except BaseException as exc:
            primary = exc
        finally:
            failures: list[BaseException] = []
            for operation in ("g",):
                if not self._stop_synchronized:
                    break
                try:
                    self.client.select_thread(operation, previous)
                except BaseException as exc:
                    failures.append(exc)
            if failures:
                raise RspTargetError(
                    f"resume failed ({primary}); could not restore downstream Hg selection: "
                    + "; ".join(map(str, failures))
                ) from failures[0]
        if primary is not None:
            raise primary
