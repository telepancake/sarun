"""Construct and serve the live AArch64 Linux-inferior RSP facade.

This module owns QEMU's sole GDB connection.  It uses the reversible probe to
enumerate tasks and, when the sealed package advertises the translation
capability, to read selected-process memory.  It forwards a complete register
block only when that task is reported current on a stopped vCPU.  Sleeping-task
registers, process-memory writes, and memory reads from legacy snapshot-only
packages fail with an RSP error instead of returning invented state.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
from typing import Callable, Mapping, Protocol
import xml.etree.ElementTree as ET

from callgate.manifest import ValidatedManifest, load_and_validate_manifest
from callgate.rsp_target import RspQemuTarget
from probe.memory_reader import ProbeMemoryReader
from probe.snapshot_runner import ProbeSnapshotRunner

from .linux_oracle import Snapshot, TaskSnapshot
from .probe_oracle import ProbeOracle
from .qemu_rsp import QemuRspClient, RspRemoteError
from .rsp_proxy import RspFacade
from .rsp_server import UnixRspServer, qemu_cpu_stop_resolver


MAX_DESCRIPTION_FILES = 32
MAX_DESCRIPTION_BYTES = 1024 * 1024


class SnapshotFetcher(Protocol):
    def __call__(self, cursor: int) -> bytes: ...


ClientFactory = Callable[[str, float], QemuRspClient]
RunnerFactory = Callable[[RspQemuTarget, ValidatedManifest], SnapshotFetcher]
MemoryReaderFactory = Callable[[RspQemuTarget, ValidatedManifest], ProbeMemoryReader]


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


class CurrentCpuProbeOracle:
    """Add the one register operation probe ABI v1 can implement honestly."""

    def __init__(
        self,
        probe: ProbeOracle,
        qemu: QemuRspClient,
        cpu_threads: tuple[str, ...],
    ) -> None:
        self.probe = probe
        self.qemu = qemu
        self.cpu_threads = cpu_threads

    def snapshot(self) -> Snapshot:
        return self.probe.snapshot()

    @staticmethod
    def _unsupported(operation: str) -> OSError:
        return OSError(f"probe ABI v1 does not support {operation}")

    def read_registers(self, task: TaskSnapshot) -> bytes:
        cpu = task.current_cpu
        if cpu is None:
            raise self._unsupported("sleeping-task registers")
        if not 0 <= cpu < len(self.cpu_threads):
            raise OSError(f"probe reported nonexistent QEMU CPU {cpu}")
        try:
            return self.qemu.read_register_block(self.cpu_threads[cpu])
        except Exception as exc:
            raise OSError(f"cannot read stopped QEMU CPU {cpu} registers") from exc

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
    return ProbeSnapshotRunner(target, manifest)


def _make_memory_reader(
    target: RspQemuTarget, manifest: ValidatedManifest
) -> ProbeMemoryReader:
    return ProbeMemoryReader(target, manifest)


def build_live_facade(
    *,
    qemu_socket: str,
    gdb_socket: str,
    manifest_path: str | Path,
    timeout: float = 5.0,
    client_factory: ClientFactory = _connect_client,
    runner_factory: RunnerFactory = _make_runner,
    memory_reader_factory: MemoryReaderFactory = _make_memory_reader,
) -> LiveFacade:
    """Validate, snapshot, and construct one stopped-QEMU facade instance."""

    if timeout <= 0:
        raise ValueError("timeout must be positive")
    # Manifest/file validation intentionally precedes opening QEMU's socket.
    manifest = load_and_validate_manifest(manifest_path)
    qemu = client_factory(str(qemu_socket), timeout)
    try:
        target = RspQemuTarget(
            qemu, manifest.kernel_file, manifest.kernel_build_id
        )
        target.assert_stopped()
        target.verify_kernel(
            str(manifest.kernel_file),
            manifest.kernel_sha256,
            manifest.kernel_build_id,
        )
        descriptions = load_target_descriptions(qemu)
        cpu_threads = qemu.thread_ids()
        runner = runner_factory(target, manifest)
        memory_reader = None
        if "translate-va-aarch64-v1" in manifest.probe_capabilities:
            memory_reader = memory_reader_factory(target, manifest)
        probe = ProbeOracle(runner, memory_reader=memory_reader)
        oracle = CurrentCpuProbeOracle(probe, qemu, cpu_threads)
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
        description="Expose stopped AArch64 QEMU Linux tasks as GDB inferiors"
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
