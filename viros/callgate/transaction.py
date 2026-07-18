"""All-or-restore transaction for an injected AArch64 call gate."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol, Sequence

from .manifest import ValidatedManifest


AARCH64_REGISTERS = tuple([f"x{number}" for number in range(31)] + ["sp", "pc", "cpsr"])
AARCH64_RESTORE_ORDER = tuple(
    [f"x{number}" for number in range(31)] + ["sp", "cpsr", "pc"]
)


class CallGateError(RuntimeError):
    """Call-gate planning or execution failed."""


class RestorationError(CallGateError):
    """Execution failed and one or more target-state restorations also failed."""

    def __init__(self, primary: BaseException | None, failures: Sequence[BaseException]):
        self.primary = primary
        self.failures = tuple(failures)
        prefix = f"call gate failed ({primary}); " if primary else ""
        super().__init__(prefix + "restoration failed: " + "; ".join(map(str, failures)))


class Target(Protocol):
    """The intentionally small backend required by the transaction."""

    def assert_stopped(self) -> None: ...
    def cpu_ids(self) -> Sequence[int]: ...
    def verify_kernel(self, path: str, sha256: str, build_id: str) -> None: ...
    def translate_virtual(self, cpu: int, virtual_address: int) -> int: ...
    def verify_mapping(self, cpu: int, virtual_address: int, physical_address: int) -> None: ...
    def read_physical(self, address: int, size: int) -> bytes: ...
    def write_physical(self, address: int, data: bytes) -> None: ...
    def read_register(self, cpu: int, name: str) -> int: ...
    def write_register(self, cpu: int, name: str, value: int) -> None: ...
    def add_hardware_breakpoint(self, address: int) -> Any: ...
    def remove_breakpoint(self, token: Any) -> None: ...
    def run_cpu_until(self, cpu: int, address: int, timeout_seconds: float) -> None: ...


@dataclass(frozen=True)
class CallGateResult:
    result: bytes
    audit: tuple[str, ...]


def plan(manifest: ValidatedManifest) -> tuple[str, ...]:
    if not isinstance(manifest, ValidatedManifest) or not manifest.is_validated:
        raise CallGateError("a validated call-gate manifest is required")
    code = manifest.region(manifest.code_region)
    return (
        f"verify stopped AArch64 target against {manifest.kernel_file}",
        f"verify {len(manifest.regions)} virtual-to-physical mappings on CPU {manifest.cpu}",
        f"snapshot {sum(region.size for region in manifest.regions)} bytes of guest RAM",
        f"snapshot {len(AARCH64_REGISTERS)} core registers on every vCPU",
        f"overlay {len(manifest.probe_bytes)} probe bytes at {code.physical_address:#x}",
        f"set CPU {manifest.cpu} PC={manifest.entry_address:#x}, SP={manifest.stack_pointer:#x}, EL1h; "
        f"x0=request {manifest.request_address:#x}, x1=result {manifest.result_address:#x}, "
        f"x2={manifest.result_size:#x}, x30=completion {manifest.completion_address:#x}",
        f"resume only CPU {manifest.cpu} synchronously to {manifest.completion_address:#x}; "
        "host timeout is not yet enforceable through the stock QEMU packet set",
        "read and validate the mailbox result",
        "restore pages, registers, and breakpoint in finally; then byte-audit state",
    )


class CallGateTransaction:
    def __init__(self, target: Target, manifest: ValidatedManifest):
        if not isinstance(manifest, ValidatedManifest) or not manifest.is_validated:
            raise CallGateError("refusing to inject without a validated manifest")
        self.target = target
        self.manifest = manifest

    def dry_run(self) -> tuple[str, ...]:
        """Return an offline operation plan without accessing the target."""

        return plan(self.manifest)

    def execute(self) -> CallGateResult:
        """Execute the probe, restoring every modified resource in ``finally``."""

        manifest = self.manifest
        target = self.target
        target.assert_stopped()
        target.verify_kernel(
            str(manifest.kernel_file), manifest.kernel_sha256, manifest.kernel_build_id
        )
        cpus = tuple(target.cpu_ids())
        if manifest.cpu not in cpus:
            raise CallGateError(f"manifest CPU {manifest.cpu} is not present")
        for region in manifest.regions:
            target.verify_mapping(manifest.cpu, region.virtual_address, region.physical_address)

        # Complete every read before the first mutation. A failed snapshot is safe.
        pages = {
            region.name: target.read_physical(region.physical_address, region.size)
            for region in manifest.regions
        }
        registers = {
            (cpu, register): target.read_register(cpu, register)
            for cpu in cpus
            for register in AARCH64_REGISTERS
        }
        code = manifest.region(manifest.code_region)
        occupants = [
            cpu for cpu in cpus
            if code.virtual_address <= registers[(cpu, "pc")]
            < code.virtual_address + code.size
        ]
        if occupants:
            raise CallGateError(
                "refusing to overwrite a code region containing the PC of "
                "CPU " + ", ".join(map(str, occupants))
            )

        modified_regions: list[str] = []
        modified_registers: list[tuple[int, str]] = []
        breakpoint = None
        primary: BaseException | None = None
        result = b""
        restoration_failures: list[BaseException] = []
        try:
            # From the first write onward, conservatively treat every scratch
            # page as dirty: the injected code may use data and stack pages
            # even when the host did not initialize them.
            modified_regions.extend(region.name for region in manifest.regions)
            code_image = bytearray(pages[code.name])
            code_image[: len(manifest.probe_bytes)] = manifest.probe_bytes
            target.write_physical(code.physical_address, bytes(code_image))

            data = manifest.region(manifest.data_region)
            if manifest.request_bytes:
                data_image = bytearray(pages[data.name])
                start = manifest.request_offset
                data_image[start : start + len(manifest.request_bytes)] = manifest.request_bytes
                target.write_physical(data.physical_address, bytes(data_image))

            breakpoint = target.add_hardware_breakpoint(manifest.completion_address)
            # Once PC can be redirected, the probe is free to clobber every
            # caller-saved and callee-saved core register.
            modified_registers.extend((manifest.cpu, name) for name in AARCH64_REGISTERS)
            for register, value in (
                ("x0", manifest.request_address),
                ("x1", manifest.result_address),
                ("x2", manifest.result_size),
                ("x30", manifest.completion_address),
                ("cpsr", manifest.pstate),
                ("sp", manifest.stack_pointer),
                ("pc", manifest.entry_address),
            ):
                target.write_register(manifest.cpu, register, value)

            target.run_cpu_until(
                manifest.cpu, manifest.completion_address, manifest.timeout_seconds
            )
            result_physical = data.physical_address + manifest.result_offset
            result = target.read_physical(result_physical, manifest.result_size)
            if manifest.completion_magic and not result.startswith(manifest.completion_magic):
                raise CallGateError("probe result lacks the manifest completion magic")
        except BaseException as exc:
            primary = exc
        finally:
            if breakpoint is not None:
                try:
                    target.remove_breakpoint(breakpoint)
                except BaseException as exc:
                    restoration_failures.append(exc)
            # Restore general registers first, then SP while GDB still sees the
            # probe's innermost frame, then the original privilege state, and
            # PC last.  In particular, changing PC before SP makes QEMU/GDB
            # reject the subsequent SP write at a completion breakpoint.
            modified_set = set(modified_registers)
            restore_order = [
                (manifest.cpu, register)
                for register in AARCH64_RESTORE_ORDER
                if (manifest.cpu, register) in modified_set
            ]
            for cpu, register in restore_order:
                try:
                    target.write_register(cpu, register, registers[(cpu, register)])
                except BaseException as exc:
                    restoration_failures.append(exc)
            for name in reversed(modified_regions):
                region = manifest.region(name)
                try:
                    target.write_physical(region.physical_address, pages[name])
                except BaseException as exc:
                    restoration_failures.append(exc)

        if restoration_failures:
            raise RestorationError(primary, restoration_failures)
        if primary is not None:
            raise CallGateError(f"call-gate transaction failed: {primary}") from primary

        audit: list[str] = []
        for region in manifest.regions:
            if target.read_physical(region.physical_address, region.size) != pages[region.name]:
                raise CallGateError(f"post-restore audit failed for region {region.name}")
            audit.append(f"region {region.name}: restored")
        for key, before in registers.items():
            if target.read_register(*key) != before:
                raise CallGateError(f"post-restore audit failed for CPU {key[0]} register {key[1]}")
        audit.append(f"{len(registers)} register values: restored")
        return CallGateResult(result=result, audit=tuple(audit))
