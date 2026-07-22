"""Construct and serve a live Linux-inferior RSP facade.

This module owns QEMU's sole GDB connection.  It uses the reversible probe to
enumerate tasks and, on AArch64 when sealed package capabilities permit, to
read selected-process memory and a sleeping task's saved native EL0 frame.
Current tasks use QEMU's complete stopped-vCPU register block.  MMIPS also
uses that stopped vCPU for current-task and kernel memory reads; sleeping
tasks remain snapshot-only.  Supplemental FP/system registers remain
unavailable through per-register reads.  Register and process-memory writes,
compat saved frames, and unsupported reads fail with an RSP error instead of
returning invented state.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import socket
from typing import Callable, Mapping, Protocol
import xml.etree.ElementTree as ET

from callgate.architectures import (
    AARCH64,
    ARMV7,
    MIPS32EL_MMIPS,
    X86_64,
    ArchitectureDescriptor,
)
from callgate.manifest import ValidatedManifest, load_and_validate_manifest
from callgate.rsp_target import RspQemuTarget
from callgate.transaction import CallGateError
from probe.abi import (
    AARCH64_SNAPSHOT_ABI,
    ARMV7LE_SNAPSHOT_ABI,
    MIPS32EL_SNAPSHOT_ABI,
    X86_64_SNAPSHOT_ABI,
    ProbeDecodeError,
    SnapshotAbi,
)
from probe.memory_reader import ProbeMemoryReader
from probe.register_reader import ProbeRegisterReader
from probe.snapshot_runner import ProbeSnapshotRunner
from probe.probe_tool import AuditError, ElfObject

from .arm_events import (
    AARCH64_EVENT_REGISTERS,
    ARMV7_EVENT_REGISTERS,
    Armv7PartialRegisterLayout,
    encode_aarch64_event_registers,
    encode_armv7_event_registers,
)
from .event_stop import KernelEventReadError, read_kernel_event_stop
from .gdb_signals import mips_linux_signal_to_gdb, standard_linux_signal_to_gdb
from .internal_breakpoints import (
    InternalBreakpoint,
    InternalBreakpointController,
    InternalBreakpointState,
)
from .kernel_events import ARCH_AARCH64, ARCH_ARM, ARCH_MIPS, ARCH_X86
from .linux_oracle import RegisterRead, Snapshot, TaskSnapshot
from .mips_events import MIPS_EVENT_REGISTER_COUNT, encode_mips_event_registers
from .partial_registers import (
    Aarch64PartialRegisterLayout,
    PartialRegisterError,
    X86KernelEventRegisterLayout,
)
from .probe_oracle import ProbeOracle
from .qemu_rsp import QemuRspClient, RspRemoteError, RspRestorationError
from .rsp_proxy import RspFacade
from .rsp_server import UnixRspServer, qemu_cpu_stop_resolver


MAX_DESCRIPTION_FILES = 32
MAX_DESCRIPTION_BYTES = 1024 * 1024

# QEMU's legacy 32-bit MIPS GDB ABI has a fixed 73-register ``g`` packet.
# The first 38 entries are the descriptor's exact call-gate core map.  The
# remainder are the standard GDB MIPS FPU slots and Linux restart register;
# QEMU returns zero-filled slots when the modeled CPU has no FPU.
_MMIPS_G_PACKET_SUFFIX = tuple(
    [(f"f{number}", 32, "ieee_single", None) for number in range(32)]
    + [
        ("fcsr", 32, None, "float"),
        ("fir", 32, None, "float"),
        ("restart", 32, None, "system"),
    ]
)


class SnapshotFetcher(Protocol):
    def __call__(self, cursor: int) -> bytes: ...


ClientFactory = Callable[[str, float], QemuRspClient]
RunnerFactory = Callable[[RspQemuTarget, ValidatedManifest], SnapshotFetcher]
MemoryReaderFactory = Callable[[RspQemuTarget, ValidatedManifest], ProbeMemoryReader]
RegisterReaderFactory = Callable[[RspQemuTarget, ValidatedManifest], ProbeRegisterReader]


def load_target_descriptions(client: QemuRspClient) -> dict[bytes, bytes]:
    """Fetch target.xml and every XInclude it names from stopped QEMU."""

    descriptions: dict[bytes, bytes] = {}
    visiting: set[bytes] = set()
    total = 0

    def visit(annex: bytes) -> None:
        nonlocal total
        if annex in descriptions:
            return
        if annex in visiting:
            raise RspRemoteError(b"cyclic target-description include")
        if len(descriptions) + len(visiting) >= MAX_DESCRIPTION_FILES:
            raise RspRemoteError(b"too many target-description includes")
        visiting.add(annex)
        try:
            text = annex.decode("ascii")
            document = client.read_xfer("features", text)
            total += len(document)
            if total > MAX_DESCRIPTION_BYTES:
                raise RspRemoteError(b"target descriptions are too large")
            try:
                root = ET.fromstring(document)
            except ET.ParseError as exc:
                # QEMU generates ``xi:include`` elements but, in releases as
                # recent as 11, relies on gdb-target.dtd to declare the prefix.
                # ElementTree intentionally does not process that external DTD.
                # Normalize only this exact generated element spelling; any
                # other malformed XML or unbound prefix remains an error.
                if b"<xi:include" not in document or b"xmlns:xi=" in document:
                    raise RspRemoteError(b"malformed target description") from exc
                normalized = document.replace(b"<xi:include", b"<include")
                try:
                    root = ET.fromstring(normalized)
                except ET.ParseError as normalized_exc:
                    raise RspRemoteError(
                        b"malformed target description"
                    ) from normalized_exc
            for element in root.iter():
                if element.tag.rsplit("}", 1)[-1] != "include":
                    continue
                href = element.attrib.get("href")
                if not href:
                    raise RspRemoteError(b"target-description include lacks href")
                try:
                    child = href.encode("ascii")
                except UnicodeEncodeError as exc:
                    raise RspRemoteError(
                        b"non-ASCII target-description annex"
                    ) from exc
                visit(child)
            descriptions[annex] = document
        finally:
            visiting.remove(annex)

    visit(b"target.xml")
    return descriptions


def _mmips_target_xml() -> bytes:
    """Describe exactly the fixed 73-register legacy QEMU MIPS block."""

    cpu_registers = []
    cp0_registers = []
    for register in MIPS32EL_MMIPS.core_registers:
        element = (
            f'<reg name="{register.name}" bitsize="{register.bits}" '
            f'regnum="{register.rsp_number}"/>'
        )
        if register.name in {"status", "badvaddr", "cause"}:
            cp0_registers.append(element)
        else:
            cpu_registers.append(element)

    fpu_registers = []
    linux_registers = []
    for number, (name, bits, register_type, group) in enumerate(
        _MMIPS_G_PACKET_SUFFIX, start=len(MIPS32EL_MMIPS.core_registers)
    ):
        attributes = [
            f'name="{name}"', f'bitsize="{bits}"', f'regnum="{number}"'
        ]
        if register_type is not None:
            attributes.append(f'type="{register_type}"')
        if group is not None:
            attributes.append(f'group="{group}"')
        element = "<reg " + " ".join(attributes) + "/>"
        (linux_registers if name == "restart" else fpu_registers).append(element)

    document = (
        '<?xml version="1.0"?>'
        '<!DOCTYPE target SYSTEM "gdb-target.dtd">'
        '<target><architecture>mips</architecture><osabi>GNU/Linux</osabi>'
        '<feature name="org.gnu.gdb.mips.cpu">'
        + "".join(cpu_registers)
        + '</feature><feature name="org.gnu.gdb.mips.cp0">'
        + "".join(cp0_registers)
        + '</feature><feature name="org.gnu.gdb.mips.fpu">'
        + "".join(fpu_registers)
        + '</feature><feature name="org.gnu.gdb.mips.linux">'
        + "".join(linux_registers)
        + "</feature></target>"
    )
    return document.encode("ascii")


def _legacy_mmips_descriptions(
    qemu: QemuRspClient, cpu_threads: tuple[str, ...]
) -> dict[bytes, bytes]:
    """Validate and describe QEMU's XML-less fixed MMIPS register block."""

    if not cpu_threads:
        raise RspRemoteError(b"QEMU exposed no CPU register thread")
    expected = (
        sum(register.bits // 8 for register in MIPS32EL_MMIPS.core_registers)
        + sum(bits // 8 for _, bits, _, _ in _MMIPS_G_PACKET_SUFFIX)
    )
    for thread in cpu_threads:
        observed = len(qemu.read_register_block(thread))
        if observed != expected:
            raise RspRemoteError(
                f"legacy MMIPS QEMU CPU {thread} returned a {observed}-byte g "
                f"packet; the standard fixed layout requires {expected} bytes".encode(
                    "ascii"
                )
            )
    return {b"target.xml": _mmips_target_xml()}


def _snapshot_abi(architecture: ArchitectureDescriptor) -> SnapshotAbi:
    if architecture is AARCH64:
        return AARCH64_SNAPSHOT_ABI
    if architecture is ARMV7:
        return ARMV7LE_SNAPSHOT_ABI
    if architecture is MIPS32EL_MMIPS:
        return MIPS32EL_SNAPSHOT_ABI
    if architecture is X86_64:
        return X86_64_SNAPSHOT_ABI
    raise ValueError(f"unsupported snapshot architecture {architecture.name!r}")


class CurrentCpuProbeOracle:
    """Select authoritative current-vCPU or saved sleeping-task registers."""

    def __init__(
        self,
        probe: ProbeOracle,
        qemu: QemuRspClient,
        cpu_threads: tuple[str, ...],
        partial_registers: Aarch64PartialRegisterLayout | None = None,
        qemu_current_memory_address_bits: int | None = None,
        kernel_memory_address_bits: int | None = None,
    ) -> None:
        self.probe = probe
        self.qemu = qemu
        self.cpu_threads = cpu_threads
        self.partial_registers = partial_registers
        self.qemu_current_memory_address_bits = qemu_current_memory_address_bits
        self.kernel_memory_address_bits = kernel_memory_address_bits

    def snapshot(self) -> Snapshot:
        return self.probe.snapshot()

    @staticmethod
    def _unsupported(operation: str) -> OSError:
        return OSError(f"probe ABI v1 does not support {operation}")

    def read_registers(self, task: TaskSnapshot) -> RegisterRead:
        cpu = task.current_cpu
        if cpu is not None:
            if not 0 <= cpu < len(self.cpu_threads):
                raise OSError(f"probe reported nonexistent QEMU CPU {cpu}")
            try:
                return RegisterRead.complete(
                    self.qemu.read_register_block(self.cpu_threads[cpu]))
            except Exception as exc:
                raise OSError(
                    f"cannot read stopped QEMU CPU {cpu} registers") from exc
        if self.partial_registers is None:
            raise self._unsupported("sleeping-task registers")
        try:
            saved = self.probe.read_registers(task)
            values = {f"x{index}": value for index, value in enumerate(saved.x)}
            values.update(sp=saved.sp, pc=saved.pc, cpsr=saved.pstate)
            return RegisterRead(self.partial_registers.encode_g_packet(values))
        except (CallGateError, ProbeDecodeError, PartialRegisterError) as exc:
            raise OSError("cannot read the task's saved userspace registers") from exc

    def write_registers(self, task: TaskSnapshot, data: bytes) -> None:
        raise self._unsupported("Linux-task register writes")

    def read_memory(self, task: TaskSnapshot, address: int, length: int) -> bytes:
        if task.state == "kernel-die" and task.current_cpu is not None:
            cpu = task.current_cpu
            if not 0 <= cpu < len(self.cpu_threads):
                raise OSError(f"event reported nonexistent QEMU CPU {cpu}")
            try:
                return self.qemu.read_virtual_memory(
                    self.cpu_threads[cpu],
                    address,
                    length,
                    address_bits=self.kernel_memory_address_bits or 64,
                )
            except Exception as exc:
                raise OSError("cannot read stopped kernel virtual memory") from exc
        try:
            return self.probe.read_memory(task, address, length)
        except NotImplementedError as exc:
            if (
                self.qemu_current_memory_address_bits is None
                or task.current_cpu is None
            ):
                raise self._unsupported("process virtual memory") from exc
            cpu = task.current_cpu
            if not 0 <= cpu < len(self.cpu_threads):
                raise OSError(f"probe reported nonexistent QEMU CPU {cpu}") from exc
            try:
                return self.qemu.read_virtual_memory(
                    self.cpu_threads[cpu],
                    address,
                    length,
                    address_bits=self.qemu_current_memory_address_bits,
                )
            except RspRestorationError:
                # Continuing after downstream selection or mode restoration
                # failed would make later reads ambiguous.  Let the server
                # close this debugger connection instead of returning E14.
                raise
            except Exception as qemu_error:
                raise OSError(
                    f"cannot read stopped QEMU CPU {cpu} memory"
                ) from qemu_error

    def write_memory(self, task: TaskSnapshot, address: int, data: bytes) -> None:
        raise self._unsupported("process virtual-memory writes")


@dataclass
class LiveFacade:
    manifest: ValidatedManifest
    qemu: QemuRspClient
    target: RspQemuTarget
    runner: SnapshotFetcher
    oracle: CurrentCpuProbeOracle
    facade: RspFacade
    server: UnixRspServer
    descriptions: Mapping[bytes, bytes]
    internal_breakpoints: InternalBreakpointController | None = None

    def close(self) -> None:
        try:
            if (
                self.internal_breakpoints is not None
                and self.internal_breakpoints.state
                in {InternalBreakpointState.READY, InternalBreakpointState.STOPPED}
            ):
                self.internal_breakpoints.uninstall()
        finally:
            self.qemu.close()


def _defined_kernel_symbol(kernel: Path, name: str) -> int | None:
    """Return one exact defined vmlinux symbol, or None when it is absent."""

    try:
        records = [
            item
            for item in ElfObject(kernel).symbol_records()
            if item["name"] == name and item["shndx"] != 0
        ]
    except (AuditError, OSError):
        # Compatibility with sealed legacy/test manifests whose kernel file
        # predates the optional built-in event observer.  Exact live kernels
        # have already been independently identified by RspQemuTarget.
        return None
    if not records:
        return None
    values = {int(item["value"]) for item in records}
    if len(values) != 1:
        raise ValueError(f"kernel has conflicting definitions of {name}")
    return values.pop()


def _connect_client(path: str, timeout: float) -> QemuRspClient:
    return QemuRspClient.connect_unix(path, timeout)


def _make_runner(
    target: RspQemuTarget, manifest: ValidatedManifest
) -> ProbeSnapshotRunner:
    return ProbeSnapshotRunner(
        target, manifest, snapshot_abi=_snapshot_abi(manifest.architecture)
    )


def _make_memory_reader(
    target: RspQemuTarget, manifest: ValidatedManifest
) -> ProbeMemoryReader:
    return ProbeMemoryReader(target, manifest)


def _make_register_reader(
    target: RspQemuTarget, manifest: ValidatedManifest
) -> ProbeRegisterReader:
    return ProbeRegisterReader(target, manifest)


def build_live_facade(
    *,
    qemu_socket: str | None,
    gdb_socket: str | None,
    manifest_path: str | Path,
    timeout: float = 5.0,
    stop_on_connect: bool = False,
    qemu_stream: socket.socket | None = None,
    client_factory: ClientFactory = _connect_client,
    runner_factory: RunnerFactory = _make_runner,
    memory_reader_factory: MemoryReaderFactory = _make_memory_reader,
    register_reader_factory: RegisterReaderFactory = _make_register_reader,
) -> LiveFacade:
    """Validate, snapshot, and construct one stopped-QEMU facade instance."""

    if timeout <= 0:
        raise ValueError("timeout must be positive")
    if (qemu_socket is None) == (qemu_stream is None):
        raise ValueError("provide exactly one QEMU socket path or inherited stream")
    # Manifest/file validation intentionally precedes opening QEMU's socket.
    manifest = load_and_validate_manifest(manifest_path)
    qemu = (
        QemuRspClient(qemu_stream, timeout)
        if qemu_stream is not None
        else client_factory(str(qemu_socket), timeout)
    )
    try:
        if stop_on_connect:
            qemu.interrupt_and_wait_for_stop()
        target = RspQemuTarget(
            qemu,
            manifest.kernel_file,
            manifest.kernel_build_id,
            manifest.architecture,
        )
        target.assert_stopped()
        target.verify_kernel(
            str(manifest.kernel_file),
            manifest.kernel_sha256,
            manifest.kernel_build_id,
        )
        try:
            descriptions = load_target_descriptions(qemu)
        except RspRemoteError as exc:
            if manifest.architecture is not MIPS32EL_MMIPS or exc.reply != b"":
                raise
            cpu_threads = qemu.thread_ids()
            descriptions = _legacy_mmips_descriptions(qemu, cpu_threads)
        else:
            cpu_threads = qemu.thread_ids()
        snapshot_abi = _snapshot_abi(manifest.architecture)
        runner = runner_factory(target, manifest)
        memory_reader = None
        if "translate-va-aarch64-v1" in manifest.probe_capabilities:
            memory_reader = memory_reader_factory(target, manifest)
        register_reader = None
        partial_registers = None
        if "saved-regs-aarch64-v1" in manifest.probe_capabilities:
            register_reader = register_reader_factory(target, manifest)
            if not cpu_threads:
                raise RspRemoteError(b"QEMU exposed no CPU register thread")
            observed_g_bytes = len(qemu.read_register_block(cpu_threads[0]))
            partial_registers = Aarch64PartialRegisterLayout.from_target_descriptions(
                descriptions,
                byte_order="little",
                observed_g_bytes=observed_g_bytes,
            )
        probe = ProbeOracle(
            runner,
            memory_reader=memory_reader,
            register_reader=register_reader,
            snapshot_abi=snapshot_abi,
        )
        oracle = CurrentCpuProbeOracle(
            probe,
            qemu,
            cpu_threads,
            partial_registers,
            qemu_current_memory_address_bits=(
                manifest.architecture.address_bits
                if manifest.architecture in {
                    ARMV7,
                    MIPS32EL_MMIPS,
                    X86_64,
                }
                else None
            ),
            kernel_memory_address_bits=manifest.architecture.address_bits,
        )
        facade = RspFacade(
            oracle,
            qemu,
            descriptions[b"target.xml"],
            target_descriptions=descriptions,
        )
        internal_breakpoints = None
        internal_stop = None
        event_address = _defined_kernel_symbol(
            manifest.kernel_file, "viros_event_stop"
        )
        if event_address is not None:
            event_address &= (1 << manifest.architecture.address_bits) - 1
            if manifest.architecture is MIPS32EL_MMIPS:
                event_arch = ARCH_MIPS
                event_register_count = MIPS_EVENT_REGISTER_COUNT
                event_encoder = encode_mips_event_registers
                event_signal_mapper = mips_linux_signal_to_gdb
            elif manifest.architecture is AARCH64:
                if partial_registers is None:
                    if not cpu_threads:
                        raise RspRemoteError(b"QEMU exposed no CPU register thread")
                    partial_registers = (
                        Aarch64PartialRegisterLayout.from_target_descriptions(
                            descriptions,
                            byte_order=manifest.architecture.target_byte_order,
                            observed_g_bytes=len(
                                qemu.read_register_block(cpu_threads[0])
                            ),
                        )
                    )
                event_arch = ARCH_AARCH64
                event_register_count = len(AARCH64_EVENT_REGISTERS)
                event_encoder = lambda event: encode_aarch64_event_registers(
                    event, partial_registers
                )
                event_signal_mapper = standard_linux_signal_to_gdb
            elif manifest.architecture is ARMV7:
                if not cpu_threads:
                    raise RspRemoteError(b"QEMU exposed no CPU register thread")
                arm_event_layout = (
                    Armv7PartialRegisterLayout.from_target_descriptions(
                        descriptions,
                        byte_order=manifest.architecture.target_byte_order,
                        observed_g_bytes=len(
                            qemu.read_register_block(cpu_threads[0])
                        ),
                    )
                )
                event_arch = ARCH_ARM
                event_register_count = len(ARMV7_EVENT_REGISTERS)
                event_encoder = lambda event: encode_armv7_event_registers(
                    event, arm_event_layout
                )
                event_signal_mapper = standard_linux_signal_to_gdb
            elif manifest.architecture is X86_64:
                if not cpu_threads:
                    raise RspRemoteError(b"QEMU exposed no CPU register thread")
                x86_event_layout = (
                    X86KernelEventRegisterLayout.from_target_descriptions(
                        descriptions,
                        byte_order="little",
                        observed_g_bytes=len(
                            qemu.read_register_block(cpu_threads[0])
                        ),
                    )
                )
                event_arch = ARCH_X86
                event_register_count = 21
                event_encoder = lambda event: RegisterRead(
                    x86_event_layout.encode_kernel_event(event)
                )
                event_signal_mapper = standard_linux_signal_to_gdb
            else:
                raise ValueError(
                    "kernel event observer is present, but this live facade does "
                    f"not yet present {manifest.architecture.display_name} events"
                )
            internal_breakpoints = InternalBreakpointController(
                qemu,
                (
                    InternalBreakpoint(
                        event_address,
                        manifest.architecture.breakpoint_size,
                        # x86 TCG does not consistently advertise a hardware
                        # breakpoint facility.  The other supported system
                        # targets keep kernel text intact by using Z1.
                        kind=0 if manifest.architecture is X86_64 else 1,
                    ),
                ),
            )
            internal_breakpoints.install()

            def internal_stop(cpu: int, pc: int):
                assert internal_breakpoints is not None
                if not internal_breakpoints.note_stop(cpu_threads[cpu], pc):
                    return None
                try:
                    return read_kernel_event_stop(
                        qemu=qemu,
                        target=target,
                        cpu_threads=cpu_threads,
                        cpu=cpu,
                        architecture=manifest.architecture,
                        event_arch=event_arch,
                        event_register_count=event_register_count,
                        encode_registers=event_encoder,
                        map_signal=event_signal_mapper,
                    )
                except KernelEventReadError:
                    # Keep the controller in STOPPED state.  Closing the
                    # facade can then remove the owned breakpoint safely.
                    raise

            facade.internal_continue = internal_breakpoints
        server = UnixRspServer(
            str(gdb_socket),
            facade,
            qemu,
            qemu_cpu_stop_resolver(qemu, target, cpu_threads, internal_stop),
            internal_breakpoints,
        )
        return LiveFacade(
            manifest,
            qemu,
            target,
            runner,
            oracle,
            facade,
            server,
            descriptions,
            internal_breakpoints,
        )
    except BaseException:
        qemu.close()
        raise


def _snapshot_json(snapshot: Snapshot) -> str:
    return json.dumps(
        {
            "generation": snapshot.generation,
            "tasks": [
                {
                    "tgid": task.identity.tgid,
                    "tid": task.identity.tid,
                    "comm": task.comm,
                    "state": task.state,
                    "current_cpu": task.current_cpu,
                }
                for task in snapshot.tasks
            ],
        },
        indent=2,
        sort_keys=True,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Expose stopped QEMU Linux tasks as GDB inferiors"
    )
    parser.add_argument("--qemu-socket", required=True)
    parser.add_argument("--gdb-socket", required=True)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument(
        "--stop-on-connect",
        action="store_true",
        help="interrupt a running QEMU and validate its stop before setup",
    )
    parser.add_argument(
        "--snapshot-only",
        action="store_true",
        help="validate and print one task snapshot without opening a GDB listener",
    )
    args = parser.parse_args(argv)

    live = build_live_facade(
        qemu_socket=args.qemu_socket,
        gdb_socket=args.gdb_socket,
        manifest_path=args.manifest,
        timeout=args.timeout,
        stop_on_connect=args.stop_on_connect,
    )
    try:
        if args.snapshot_only:
            print(_snapshot_json(live.facade.snapshot))
            return 0
        print(f"serving one GDB connection on {live.server.path}", flush=True)
        live.server.serve_once()
        return 0
    finally:
        live.close()


if __name__ == "__main__":
    raise SystemExit(main())
