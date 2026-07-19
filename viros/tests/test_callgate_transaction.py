from __future__ import annotations

import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from dataclasses import FrozenInstanceError, replace

from callgate.architectures import (
    AARCH64,
    ARCHITECTURES,
    MIPS32EL_MMIPS,
    MIPS_GATE_STATUS_CLEAR,
    MIPS_STATUS_EXL,
    architecture_by_name,
)
from callgate.manifest import ManifestError, load_and_validate_manifest
from callgate.transaction import (
    AARCH64_REGISTERS,
    CallGateError,
    CallGateTransaction,
    RestorationError,
    plan,
)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def make_manifest(directory: Path, **changes):
    kernel = directory / "vmlinux"
    probe = directory / "probe.bin"
    kernel.write_bytes(b"ELF kernel fixture")
    # Three aligned AArch64 instructions; their semantics are irrelevant to
    # the host-side transaction tests.
    probe.write_bytes(bytes.fromhex("1f2003d51f2003d500000014"))
    document = {
        "format": "viros-callgate-v1",
        "architecture": "aarch64",
        "allow_transient_guest_modification": True,
        "kernel": {
            "vmlinux": kernel.name,
            "sha256": sha256(kernel),
            "build_id": "0123456789abcdef",
        },
        "regions": [
            {
                "name": "code",
                "role": "code",
                "virtual_address": "0xffff800080100000",
                "physical_address": "0x40100000",
                "size": 4096,
            },
            {
                "name": "data",
                "role": "data",
                "virtual_address": "0xffff800082000000",
                "physical_address": "0x42000000",
                "size": 4096,
            },
            {
                "name": "stack",
                "role": "stack",
                "virtual_address": "0xffff800082001000",
                "physical_address": "0x42001000",
                "size": 4096,
            },
        ],
        "probe": {
            "binary": probe.name,
            "sha256": sha256(probe),
            "code_region": "code",
            "entry_offset": 0,
            "completion_offset": 4,
        },
        "mailbox": {
            "data_region": "data",
            "request_offset": 32,
            "request_hex": "01020304",
            "result_offset": 64,
            "result_size": 8,
            "completion_magic_hex": "5649524f",
        },
        "invocation": {
            "cpu": 0,
            "pstate": "0x3c5",
            "stack_region": "stack",
            "stack_pointer": "0xffff800082002000",
            "argument_address": "0xffff800082000020",
            "timeout_seconds": 0.25,
        },
    }
    for key, value in changes.items():
        document[key] = value
    path = directory / "callgate.json"
    path.write_text(json.dumps(document), encoding="utf-8")
    return path, document


class FakeTarget:
    def __init__(self, manifest):
        self.manifest = manifest
        self.memory = {
            region.physical_address: bytearray(
                bytes([index + 1]) * region.size
            )
            for index, region in enumerate(manifest.regions)
        }
        self.registers = {
            (cpu, register): cpu * 1000 + index
            for cpu in (0, 1)
            for index, register in enumerate(AARCH64_REGISTERS)
        }
        self.original_registers = dict(self.registers)
        self.register_writes = []
        self.reject_sp_restore_after_pc = False
        self.original_pc_restored = False
        self.writes = []
        self.events = []
        self.breakpoints = set()
        self.fail_run = None
        self.fail_snapshot_at = None
        self.fail_write_number = None
        self.fail_restore_address = None
        self._write_number = 0
        self._original = {base: bytes(data) for base, data in self.memory.items()}
        self.restore_counts = {base: 0 for base in self.memory}
        self.entry_registers = None

    def _find(self, address, size):
        for base, data in self.memory.items():
            if base <= address and address + size <= base + len(data):
                return base, data, address - base
        raise RuntimeError(f"unmapped physical address {address:#x}")

    def assert_stopped(self):
        self.events.append("stopped")

    def cpu_ids(self):
        return (0, 1)

    def verify_kernel(self, path, digest, build_id):
        self.events.append("kernel")
        if digest != self.manifest.kernel_sha256:
            raise RuntimeError("wrong kernel")

    def verify_mapping(self, cpu, virtual, physical):
        self.events.append(("mapping", virtual, physical))

    def read_physical(self, address, size):
        if self.fail_snapshot_at == address and not self.writes:
            raise RuntimeError("snapshot fault")
        _, data, offset = self._find(address, size)
        return bytes(data[offset : offset + size])

    def write_physical(self, address, data):
        self._write_number += 1
        if self.fail_write_number == self._write_number:
            raise RuntimeError("write fault")
        if self.fail_restore_address == address and data == self._original.get(address):
            raise RuntimeError("restore fault")
        if data == self._original.get(address):
            self.restore_counts[address] += 1
        _, destination, offset = self._find(address, len(data))
        destination[offset : offset + len(data)] = data
        self.writes.append((address, bytes(data)))

    def read_register(self, cpu, name):
        return self.registers[(cpu, name)]

    def write_register(self, cpu, name, value):
        if (
            self.reject_sp_restore_after_pc
            and name == "sp"
            and value == self.original_registers[(cpu, "sp")]
            and self.original_pc_restored
        ):
            raise RuntimeError("SP is unmodifiable after restoring PC")
        if name == "pc" and value == self.original_registers[(cpu, "pc")]:
            self.original_pc_restored = True
        self.register_writes.append((cpu, name, value))
        self.registers[(cpu, name)] = value

    def add_hardware_breakpoint(self, address):
        token = ("breakpoint", address)
        self.breakpoints.add(token)
        return token

    def remove_breakpoint(self, token):
        self.breakpoints.remove(token)

    def run_cpu_until(self, cpu, address, timeout_seconds):
        self.events.append(("run", cpu, address, timeout_seconds))
        self.entry_registers = {
            name: self.registers[(cpu, name)] for name in ("x0", "x1", "x2", "x30")
        }
        if self.fail_run:
            raise self.fail_run
        # Model arbitrary probe clobbering, including stack and a register that
        # was not used to enter the gate.
        self.registers[(cpu, "x17")] = 0xDEADBEEF
        stack = self.manifest.region(self.manifest.stack_region)
        self.memory[stack.physical_address][-16:] = b"probe stack use!"
        data = self.manifest.region(self.manifest.data_region)
        offset = self.manifest.result_offset
        self.memory[data.physical_address][offset : offset + 8] = b"VIROSOK!"


def mips_manifest(manifest, *, pstate=None):
    virtual_addresses = {
        "code": 0x80100000,
        "data": 0x82000000,
        "stack": 0x82001000,
    }
    regions = tuple(
        replace(region, virtual_address=virtual_addresses[region.role])
        for region in manifest.regions
    )
    code = next(region for region in regions if region.role == "code")
    data = next(region for region in regions if region.role == "data")
    stack = next(region for region in regions if region.role == "stack")
    return replace(
        manifest,
        architecture=MIPS32EL_MMIPS,
        regions=regions,
        entry_address=code.virtual_address,
        completion_address=code.virtual_address + 4,
        pstate=pstate,
        stack_pointer=stack.virtual_address + stack.size,
        request_address=data.virtual_address + manifest.request_offset,
        result_address=data.virtual_address + manifest.result_offset,
    )


class FakeMipsTarget(FakeTarget):
    def __init__(self, manifest):
        super().__init__(manifest)
        self.registers = {
            (cpu, register): cpu * 1000 + index
            for cpu in (0, 1)
            for index, register in enumerate(manifest.architecture.register_names)
        }
        self.registers[(0, "r0")] = 0
        self.registers[(0, "status")] = 0x1040FF1D
        self.registers[(0, "pc")] = 0x80001234
        self.original_registers = dict(self.registers)

    def run_cpu_until(self, cpu, address, timeout_seconds):
        self.events.append(("run", cpu, address, timeout_seconds))
        self.entry_registers = {
            name: self.registers[(cpu, name)]
            for name in ("r4", "r5", "r6", "r25", "r31", "status", "r29", "pc")
        }
        if self.fail_run:
            raise self.fail_run
        self.registers[(cpu, "r17")] = 0xDEADBEEF
        stack = self.manifest.region(self.manifest.stack_region)
        self.memory[stack.physical_address][-16:] = b"probe stack use!"
        data = self.manifest.region(self.manifest.data_region)
        offset = self.manifest.result_offset
        self.memory[data.physical_address][offset : offset + 8] = b"VIROSOK!"


class TransactionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        path, _ = make_manifest(Path(self.temp.name))
        self.manifest = load_and_validate_manifest(path)
        self.target = FakeTarget(self.manifest)
        self.before_memory = {
            address: bytes(data) for address, data in self.target.memory.items()
        }
        self.before_registers = dict(self.target.registers)

    def tearDown(self):
        self.temp.cleanup()

    def assert_restored(self):
        self.assertEqual(
            {address: bytes(data) for address, data in self.target.memory.items()},
            self.before_memory,
        )
        self.assertEqual(self.target.registers, self.before_registers)
        self.assertFalse(self.target.breakpoints)

    def test_success_returns_result_and_restores_everything(self):
        result = CallGateTransaction(self.target, self.manifest).execute()
        self.assertEqual(result.result, b"VIROSOK!")
        self.assertEqual(
            self.target.entry_registers,
            {
                "x0": self.manifest.request_address,
                "x1": self.manifest.result_address,
                "x2": self.manifest.result_size,
                "x30": self.manifest.completion_address,
            },
        )
        self.assertIn("68 register values: restored", result.audit)
        self.assertEqual(set(self.target.restore_counts.values()), {1})
        self.assert_restored()

    def test_descriptor_preserves_exact_legacy_entry_write_order(self):
        CallGateTransaction(self.target, self.manifest).execute()
        self.assertIs(self.manifest.architecture, AARCH64)
        self.assertEqual(AARCH64_REGISTERS, AARCH64.register_names)
        self.assertEqual(
            self.target.register_writes[:7],
            [
                (0, "x0", self.manifest.request_address),
                (0, "x1", self.manifest.result_address),
                (0, "x2", self.manifest.result_size),
                (0, "x30", self.manifest.completion_address),
                (0, "cpsr", self.manifest.pstate),
                (0, "sp", self.manifest.stack_pointer),
                (0, "pc", self.manifest.entry_address),
            ],
        )

    def test_descriptor_preserves_exact_legacy_plan(self):
        self.assertEqual(
            plan(self.manifest),
            (
                f"verify stopped AArch64 target against {self.manifest.kernel_file}",
                "verify 3 virtual-to-physical mappings on CPU 0",
                "snapshot 12288 bytes of guest RAM",
                "snapshot 34 core registers on every vCPU",
                "overlay 12 probe bytes at 0x40100000",
                "set CPU 0 PC=0xffff800080100000, SP=0xffff800082002000, EL1h; "
                "x0=request 0xffff800082000020, x1=result 0xffff800082000040, "
                "x2=0x8, x30=completion 0xffff800080100004",
                "resume only CPU 0 synchronously to 0xffff800080100004; "
                "host timeout is not yet enforceable through the stock QEMU packet set",
                "read and validate the mailbox result",
                "restore pages, registers, and breakpoint in finally; then byte-audit state",
            ),
        )

    def test_sp_is_restored_before_cpsr_and_pc_with_pc_last(self):
        self.target.reject_sp_restore_after_pc = True
        CallGateTransaction(self.target, self.manifest).execute()
        restores = [
            name
            for cpu, name, value in self.target.register_writes
            if cpu == self.manifest.cpu
            and value == self.target.original_registers[(cpu, name)]
        ]
        self.assertLess(restores.index("sp"), restores.index("cpsr"))
        self.assertLess(restores.index("cpsr"), restores.index("pc"))
        self.assertEqual(restores[-1], "pc")
        self.assert_restored()

    def test_invocation_sets_cpsr_before_sp_and_pc(self):
        CallGateTransaction(self.target, self.manifest).execute()
        invocation = [
            name
            for cpu, name, value in self.target.register_writes
            if cpu == self.manifest.cpu
            and value != self.target.original_registers[(cpu, name)]
        ][:7]
        self.assertLess(invocation.index("cpsr"), invocation.index("sp"))
        self.assertLess(invocation.index("sp"), invocation.index("pc"))

    def test_run_failure_restores_everything(self):
        self.target.fail_run = TimeoutError("instruction budget exhausted")
        with self.assertRaisesRegex(CallGateError, "instruction budget exhausted"):
            CallGateTransaction(self.target, self.manifest).execute()
        self.assert_restored()

    def test_injection_write_failure_restores_prior_writes(self):
        self.target.fail_write_number = 2
        with self.assertRaisesRegex(CallGateError, "write fault"):
            CallGateTransaction(self.target, self.manifest).execute()
        self.assert_restored()

    def test_snapshot_failure_performs_no_writes(self):
        self.target.fail_snapshot_at = self.manifest.region("data").physical_address
        with self.assertRaisesRegex(RuntimeError, "snapshot fault"):
            CallGateTransaction(self.target, self.manifest).execute()
        self.assertEqual(self.target.writes, [])
        self.assert_restored()

    def test_code_region_containing_any_cpu_pc_is_rejected_before_writes(self):
        code = self.manifest.region(self.manifest.code_region)
        self.target.registers[(1, "pc")] = code.virtual_address + 4
        self.before_registers = dict(self.target.registers)
        with self.assertRaisesRegex(CallGateError, "containing the PC of CPU 1"):
            CallGateTransaction(self.target, self.manifest).execute()
        self.assertEqual(self.target.writes, [])
        self.assert_restored()

    def test_restoration_failure_is_not_hidden(self):
        self.target.fail_run = RuntimeError("probe exploded")
        self.target.fail_restore_address = self.manifest.region("code").physical_address
        with self.assertRaises(RestorationError) as caught:
            CallGateTransaction(self.target, self.manifest).execute()
        self.assertIn("probe exploded", str(caught.exception.primary))
        self.assertIn("restore fault", str(caught.exception))

    def test_dry_run_never_accesses_target(self):
        operations = CallGateTransaction(self.target, self.manifest).dry_run()
        self.assertTrue(any("no guest" not in item for item in operations))
        self.assertEqual(self.target.events, [])
        self.assertEqual(self.target.writes, [])

    def test_unvalidated_input_is_rejected_before_target_access(self):
        with self.assertRaisesRegex(CallGateError, "validated manifest"):
            CallGateTransaction(self.target, None)
        self.assertEqual(self.target.events, [])

    def test_bad_completion_magic_restores_everything(self):
        self.target.run_cpu_until = lambda *args: None
        with self.assertRaisesRegex(CallGateError, "completion magic"):
            CallGateTransaction(self.target, self.manifest).execute()
        self.assert_restored()


class MipsTransactionPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        path, _ = make_manifest(Path(self.temp.name))
        self.manifest = mips_manifest(load_and_validate_manifest(path))
        self.target = FakeMipsTarget(self.manifest)
        self.before_memory = {
            address: bytes(data) for address, data in self.target.memory.items()
        }
        self.before_registers = dict(self.target.registers)

    def tearDown(self):
        self.temp.cleanup()

    def assert_restored(self):
        self.assertEqual(
            {address: bytes(data) for address, data in self.target.memory.items()},
            self.before_memory,
        )
        self.assertEqual(self.target.registers, self.before_registers)
        self.assertFalse(self.target.breakpoints)

    def test_o32_entry_values_derive_status_and_restore_everything(self):
        original_status = self.before_registers[(0, "status")]
        expected_status = (
            original_status & ~MIPS_GATE_STATUS_CLEAR
        ) | MIPS_STATUS_EXL

        result = CallGateTransaction(self.target, self.manifest).execute()

        self.assertEqual(result.result, b"VIROSOK!")
        self.assertEqual(
            self.target.entry_registers,
            {
                "r4": self.manifest.request_address,
                "r5": self.manifest.result_address,
                "r6": self.manifest.result_size,
                "r25": self.manifest.entry_address,
                "r31": self.manifest.completion_address,
                "status": expected_status,
                "r29": self.manifest.stack_pointer,
                "pc": self.manifest.entry_address,
            },
        )
        self.assertEqual(
            self.target.register_writes[:8],
            [
                (0, "r4", self.manifest.request_address),
                (0, "r5", self.manifest.result_address),
                (0, "r6", self.manifest.result_size),
                (0, "r25", self.manifest.entry_address),
                (0, "r31", self.manifest.completion_address),
                (0, "status", expected_status),
                (0, "r29", self.manifest.stack_pointer),
                (0, "pc", self.manifest.entry_address),
            ],
        )
        self.assertIn("76 register values: restored", result.audit)
        self.assert_restored()

    def test_status_formula_preserves_unrelated_original_bits(self):
        for original_status in (0, 0x1D, 0x1040FF1D, 0xFFFFFFFF):
            with self.subTest(original_status=original_status):
                registers = {
                    register: 0
                    for register in MIPS32EL_MMIPS.register_names
                }
                registers["status"] = original_status
                values = dict(MIPS32EL_MMIPS.entry_register_values(
                    request_address=0x82000020,
                    result_address=0x82000040,
                    result_size=8,
                    completion_address=0x80100004,
                    control_state=None,
                    original_registers=registers,
                    stack_pointer=0x82002000,
                    entry_address=0x80100000,
                ))
                self.assertEqual(
                    values["status"],
                    (original_status & ~MIPS_GATE_STATUS_CLEAR) | MIPS_STATUS_EXL,
                )

    def test_original_state_precondition_fails_before_any_write(self):
        self.target.registers[(0, "r0")] = 1
        with self.assertRaisesRegex(CallGateError, "r0.*zero"):
            CallGateTransaction(self.target, self.manifest).execute()
        self.assertEqual(self.target.writes, [])
        self.assertEqual(self.target.register_writes, [])
        self.assertFalse(self.target.breakpoints)

    def test_manifest_status_is_never_accepted_for_mips(self):
        manifest = replace(self.manifest, pstate=0x2)
        target = FakeMipsTarget(manifest)
        with self.assertRaisesRegex(CallGateError, "must not come from the manifest"):
            CallGateTransaction(target, manifest).execute()
        self.assertEqual(target.writes, [])
        self.assertEqual(target.register_writes, [])

    def test_descriptor_is_registered_for_snapshot_packages(self):
        self.assertEqual(MIPS32EL_MMIPS.stack_alignment, 8)
        self.assertEqual(MIPS32EL_MMIPS.argument_registers, ("r4", "r5", "r6"))
        self.assertEqual(MIPS32EL_MMIPS.entry_address_registers, ("r25",))
        self.assertFalse(MIPS32EL_MMIPS.manifest_control_state)
        self.assertEqual(MIPS32EL_MMIPS.known_capabilities, {"snapshot-v1"})
        self.assertIs(ARCHITECTURES["mmips"], MIPS32EL_MMIPS)
        self.assertIs(architecture_by_name("mmips"), MIPS32EL_MMIPS)


class ManifestTests(unittest.TestCase):
    def test_aarch64_descriptor_registry_is_immutable(self):
        self.assertIs(architecture_by_name("aarch64"), AARCH64)
        self.assertEqual(AARCH64.register_bits("cpsr"), 32)
        self.assertEqual(AARCH64.register_bits("pc"), 64)
        with self.assertRaises(FrozenInstanceError):
            AARCH64.stack_alignment = 8

    def test_legacy_argument_address_must_match_derived_request_pointer(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["invocation"]["argument_address"] = "0xffff800082000000"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "must equal data_region"):
                load_and_validate_manifest(path)

    def test_mmips_snapshot_manifest_uses_32_bit_regions_and_derived_status(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["architecture"] = "mmips"
            document["probe"]["capabilities"] = ["snapshot-v1"]
            for region, virtual in zip(
                document["regions"], (0x80100000, 0x82000000, 0x82001000)
            ):
                region["virtual_address"] = hex(virtual)
            document["invocation"].pop("pstate")
            document["invocation"]["stack_pointer"] = "0x82002000"
            document["invocation"]["argument_address"] = "0x82000020"
            path.write_text(json.dumps(document), encoding="utf-8")

            manifest = load_and_validate_manifest(path)

            self.assertIs(manifest.architecture, MIPS32EL_MMIPS)
            self.assertIsNone(manifest.pstate)
            self.assertEqual(manifest.probe_capabilities, ("snapshot-v1",))
            self.assertEqual(manifest.entry_address, 0x80100000)

    def test_mmips_manifest_rejects_pstate_and_non_32_bit_regions(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["architecture"] = "mmips"
            document["probe"]["capabilities"] = ["snapshot-v1"]
            for region, virtual in zip(
                document["regions"], (0x80100000, 0x82000000, 0x82001000)
            ):
                region["virtual_address"] = hex(virtual)
            document["invocation"]["stack_pointer"] = "0x82002000"
            document["invocation"]["argument_address"] = "0x82000020"
            with self.subTest(field="pstate"):
                path.write_text(json.dumps(document), encoding="utf-8")
                with self.assertRaisesRegex(ManifestError, "pstate must be omitted"):
                    load_and_validate_manifest(path)

            document["invocation"].pop("pstate")
            document["regions"][0]["virtual_address"] = "0x100000000"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "does not fit in 32 bits"):
                load_and_validate_manifest(path)

    def test_probe_hash_mismatch_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["probe"]["sha256"] = "0" * 64
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "SHA-256 mismatch"):
                load_and_validate_manifest(path)

    def test_manifest_must_explicitly_allow_transient_modification(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["allow_transient_guest_modification"] = False
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "must be true"):
                load_and_validate_manifest(path)

    def test_regions_must_not_overlap(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["regions"][1]["physical_address"] = "0x40100000"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "overlap"):
                load_and_validate_manifest(path)

    def test_region_roles_are_unique(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["regions"].append({
                "name": "extra-data",
                "role": "data",
                "virtual_address": "0xffff800082002000",
                "physical_address": "0x42002000",
                "size": 4096,
            })
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "duplicate region role: data"):
                load_and_validate_manifest(path)

    def test_region_ranges_must_not_wrap_64_bit_address_space(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["regions"][0]["virtual_address"] = "0xfffffffffffff000"
            document["regions"][0]["size"] = 8192
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "range does not fit in 64 bits"):
                load_and_validate_manifest(path)

    def test_mailbox_request_and_result_must_not_overlap(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["mailbox"]["result_offset"] = 32
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "must not overlap"):
                load_and_validate_manifest(path)

    def test_entry_and_completion_must_be_distinct(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["probe"]["completion_offset"] = 0
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "must be distinct"):
                load_and_validate_manifest(path)

    def test_register_values_must_fit_their_architectural_width(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["invocation"]["pstate"] = 1 << 32
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "pstate must fit in 32 bits"):
                load_and_validate_manifest(path)

    def test_el1h_and_daif_are_required(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path, document = make_manifest(directory)
            document["invocation"]["pstate"] = "0x0"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ManifestError, "EL1h with DAIF masked"):
                load_and_validate_manifest(path)


if __name__ == "__main__":
    unittest.main()
