"""Guarded GDB command surface for the experimental AArch64 call gate."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import sys


try:
    import gdb
except ImportError:  # Permit host-side syntax/import checks.
    gdb = None


SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from callgate.gdb_target import GdbQemuTarget, GdbTargetError
from callgate.manifest import ManifestError, load_and_validate_manifest
from callgate.transaction import CallGateError, CallGateResult, CallGateTransaction, plan
from probe.abi import ProbeDecodeError, ProbeSnapshot, TASK_ON_CPU
from probe.snapshot_runner import ProbeSnapshotRunner


@dataclass(frozen=True)
class ProbeProcessResult:
    """One decoded snapshot and the restoration audit for each probe page."""

    snapshot: ProbeSnapshot
    audits: tuple[tuple[str, ...], ...]


def validated_plan(manifest_path: str) -> tuple[str, ...]:
    """Validate all manifest-bound files and return a non-mutating plan."""

    return plan(load_and_validate_manifest(manifest_path))


def run_live_probe(manifest_path: str, gdb_module) -> CallGateResult:
    """Validate a manifest, then execute it through the live GDB/QEMU target."""

    manifest = load_and_validate_manifest(manifest_path)
    # Keep construction of anything target-facing after validation.  The
    # backend and transaction receive only this sealed object, never untrusted
    # JSON or paths from a partially checked manifest.
    if not manifest.is_validated:  # Defensive even though the loader seals it.
        raise ManifestError("manifest validation did not produce a sealed result")
    target = GdbQemuTarget(gdb_module)
    return CallGateTransaction(target, manifest).execute()


def run_probe_snapshot(manifest_path: str, gdb_module) -> ProbeProcessResult:
    """Run all frozen-probe pages and return their decoded task snapshot."""

    manifest = load_and_validate_manifest(manifest_path)
    # As with the raw command, target construction is deliberately kept after
    # all files and manifest bindings have been validated and sealed.
    if not manifest.is_validated:
        raise ManifestError("manifest validation did not produce a sealed result")
    target = GdbQemuTarget(gdb_module)
    runner = ProbeSnapshotRunner(target, manifest)
    snapshot = runner.snapshot()
    return ProbeProcessResult(snapshot=snapshot, audits=runner.audits)


def format_live_result(result: CallGateResult) -> tuple[str, ...]:
    """Render a completed transaction without losing binary response bytes."""

    return (
        f"Probe response ({len(result.result)} bytes): {result.result.hex()}",
        "Restoration audit:",
        *(f"  {entry}" for entry in result.audit),
    )


def format_process_snapshot(result: ProbeProcessResult) -> tuple[str, ...]:
    """Render the probe's process/thread records and per-page restoration audit."""

    rows = ["PID      TGID     CPU STATE              COMM             MM                 PGD"]
    for task in result.snapshot.tasks:
        cpu = str(task.cpu) if task.probe_flags & TASK_ON_CPU else "-"
        mm = f"{task.mm:#x}" if task.mm else "-"
        pgd = f"{task.pgd_kernel_va:#x}" if task.pgd_kernel_va else "-"
        rows.append(
            f"{task.pid:<8} {task.tgid:<8} {cpu:<3} {task.state:#018x} "
            f"{task.comm[:16]:<16} {mm:<18} {pgd}"
        )
    if not result.snapshot.tasks:
        rows.append("(no tasks returned)")
    rows.append("Restoration audits:")
    rows.extend(
        f"  page {number}: " + "; ".join(audit)
        for number, audit in enumerate(result.audits, 1)
    )
    return tuple(rows)


if gdb is not None:
    class VirosProbePlan(gdb.Command):
        """Validate and audit an AArch64 call-gate manifest without injection."""

        def __init__(self):
            super().__init__("viros-probe-plan", gdb.COMMAND_SUPPORT)

        def invoke(self, argument, from_tty):
            argv = gdb.string_to_argv(argument)
            if len(argv) != 1:
                raise gdb.GdbError("usage: viros-probe-plan MANIFEST.json")
            try:
                operations = validated_plan(argv[0])
            except ManifestError as exc:
                raise gdb.GdbError(str(exc)) from exc
            gdb.write("Validated manifest; no guest state was accessed or modified.\n")
            for number, operation in enumerate(operations, 1):
                gdb.write(f"  {number:2d}. {operation}\n")


    class VirosProbeRun(gdb.Command):
        """Run a reversible AArch64 probe described by MANIFEST.json.

        The manifest and its exact kernel/probe files are validated before the
        target is accessed.  Stock QEMU has no automatic instruction-budget
        timeout for this operation.  If the probe does not complete, press
        Ctrl-C; the transaction then performs its cleanup/restoration path.
        """

        def __init__(self):
            super().__init__("viros-probe-run", gdb.COMMAND_RUNNING)

        def invoke(self, argument, from_tty):
            argv = gdb.string_to_argv(argument)
            if len(argv) != 1:
                raise gdb.GdbError("usage: viros-probe-run MANIFEST.json")
            gdb.write(
                "Warning: stock QEMU provides no automatic instruction-budget "
                "timeout; press Ctrl-C to interrupt and restore target state.\n"
            )
            try:
                result = run_live_probe(argv[0], gdb)
            except (ManifestError, CallGateError, GdbTargetError) as exc:
                raise gdb.GdbError(str(exc)) from exc
            for line in format_live_result(result):
                gdb.write(line + "\n")


    class VirosProbePs(gdb.Command):
        """Run the reversible AArch64 probe and print its task snapshot."""

        def __init__(self):
            super().__init__("viros-probe-ps", gdb.COMMAND_RUNNING)

        def invoke(self, argument, from_tty):
            argv = gdb.string_to_argv(argument)
            if len(argv) != 1:
                raise gdb.GdbError("usage: viros-probe-ps MANIFEST.json")
            gdb.write(
                "Warning: stock QEMU provides no automatic instruction-budget "
                "timeout; press Ctrl-C to interrupt and restore target state.\n"
            )
            try:
                result = run_probe_snapshot(argv[0], gdb)
            except (ManifestError, CallGateError, GdbTargetError, ProbeDecodeError) as exc:
                raise gdb.GdbError(str(exc)) from exc
            for line in format_process_snapshot(result):
                gdb.write(line + "\n")


    VirosProbePlan()
    VirosProbeRun()
    VirosProbePs()
