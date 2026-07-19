"""Construct and serve a live Linux-inferior RSP facade.

This module owns QEMU's sole GDB connection.  It uses the reversible probe to
enumerate tasks and, on AArch64 when sealed package capabilities permit, to
read selected-process memory and a sleeping task's saved native EL0 frame.
Current tasks use QEMU's complete stopped-vCPU register block; sleeping tasks
use the exact core ``g``-packet prefix observed from QEMU.  Supplemental
FP/system registers remain unavailable through per-register reads.  Register
and process-memory writes, compat saved frames, and reads from legacy
snapshot-only packages fail with an RSP error instead of returning invented
state.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
from typing import Callable, Mapping, Protocol
import xml.etree.ElementTree as ET

from callgate.architectures import (
    AARCH64,
    MIPS32EL_MMIPS,
    ArchitectureDescriptor,
)
from callgate.manifest import ValidatedManifest, load_and_validate_manifest
from callgate.rsp_target import RspQemuTarget
from callgate.transaction import CallGateError
from probe.abi import (
    AARCH64_SNAPSHOT_ABI,
    MIPS32EL_SNAPSHOT_ABI,
    ProbeDecodeError,
    SnapshotAbi,
)
from probe.memory_reader import ProbeMemoryReader
from probe.register_reader import ProbeRegisterReader
from probe.snapshot_runner import ProbeSnapshotRunner

from .linux_oracle import RegisterRead, Snapshot, TaskSnapshot
from .partial_registers import (
    Aarch64PartialRegisterLayout,
    PartialRegisterError,
)
from .probe_oracle import ProbeOracle
from .qemu_rsp import QemuRspClient, RspRemoteError
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
                raise RspRemoteError(b"malformed target description") from exc
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
    if architecture is MIPS32EL_MMIPS:
        return MIPS32EL_SNAPSHOT_ABI
    raise ValueError(f"unsupported snapshot architecture {architecture.name!r}")


class CurrentCpuProbeOracle:
    """Select authoritative current-vCPU or saved sleeping-task registers."""

    def __init__(
        self,
        probe: ProbeOracle,
        qemu: QemuRspClient,
        cpu_threads: tuple[str, ...],
        partial_registers: Aarch64PartialRegisterLayout | None = None,
    ) -> None:
        self.probe = probe
        self.qemu = qemu
        self.cpu_threads = cpu_threads
        self.partial_registers = partial_registers

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
        try:
            return self.probe.read_memory(task, address, length)
        except NotImplementedError as exc:
            raise self._unsupported("process virtual memory") from exc

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

    def close(self) -> None:
        self.qemu.close()


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
    qemu_socket: str,
    gdb_socket: str,
    manifest_path: str | Path,
    timeout: float = 5.0,
    client_factory: ClientFactory = _connect_client,
    runner_factory: RunnerFactory = _make_runner,
    memory_reader_factory: MemoryReaderFactory = _make_memory_reader,
    register_reader_factory: RegisterReaderFactory = _make_register_reader,
) -> LiveFacade:
    """Validate, snapshot, and construct one stopped-QEMU facade instance."""

    if timeout <= 0:
        raise ValueError("timeout must be positive")
    # Manifest/file validation intentionally precedes opening QEMU's socket.
    manifest = load_and_validate_manifest(manifest_path)
    qemu = client_factory(str(qemu_socket), timeout)
    try:
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
            probe, qemu, cpu_threads, partial_registers)
        facade = RspFacade(
            oracle,
            qemu,
            descriptions[b"target.xml"],
            target_descriptions=descriptions,
        )
        server = UnixRspServer(
            str(gdb_socket),
            facade,
            qemu,
            qemu_cpu_stop_resolver(qemu, target, cpu_threads),
        )
        return LiveFacade(
            manifest, qemu, target, runner, oracle, facade, server, descriptions
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
