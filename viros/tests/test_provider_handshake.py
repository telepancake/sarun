from __future__ import annotations

import hashlib
import os
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
import tempfile
import threading
import unittest
from unittest import mock

from inferiors.provider_handshake import (
    HandshakeProtocolError,
    PreparedBoot,
    Rejected,
    ResourceIdentity,
    SelectedBundleTransaction,
    encode_decision,
    encode_prepare,
    read_outcome,
    read_prepare,
    serve_pre_rsp_handshake,
)
from inferiors.sarun_provider import run_provider
from probe.image_inspector import CapturedArtifact
from probe.provider_derivation import (
    SelectedImageRequest,
    SelectedKernelInitramfsRequest,
)
from probe.selected_bundle_orchestrator import (
    CatalogExecutable,
    FixedBootProfile,
    SelectedBundleExecutionRequest,
)


def artifact(
    path: str,
    contents: bytes,
    *,
    box_id: int = 17,
    architecture: str | None = None,
) -> CapturedArtifact:
    return CapturedArtifact(
        box_id=box_id,
        path=path,
        size=len(contents),
        sha256=hashlib.sha256(contents).hexdigest(),
        record_id=f"write:{box_id}:{path}",
        architecture=architecture,
    )


def execution(*, pair: bool = False) -> SelectedBundleExecutionRequest:
    kernel = artifact("out/bzImage", b"kernel", architecture="x86_64")
    initramfs = artifact("out/rootfs.cpio", b"070701initramfs")
    image = artifact("out/firmware.bin", b"firmware", architecture="x86_64")
    tool_specs = (
        ("make", "/usr/bin/make"),
        ("compiler", "/usr/bin/gcc"),
        ("cross-ld", "/usr/bin/ld"),
        ("objcopy", "/usr/bin/objcopy"),
    )
    tools = tuple(artifact(path[1:], label.encode()) for label, path in tool_specs)
    catalog = (kernel, initramfs, *tools)
    if pair:
        selected = SelectedKernelInitramfsRequest(kernel, initramfs, catalog)
    else:
        selected = SelectedImageRequest(image, catalog)
    bindings = tuple(
        CatalogExecutable(label, path, row)
        for (label, path), row in zip(tool_specs, tools, strict=True)
    )
    return SelectedBundleExecutionRequest(selected, bindings, FixedBootProfile.X86_64)


def resource(path: str, byte: int) -> ResourceIdentity:
    return ResourceIdentity(path, byte, bytes((byte,)) * 32)


def wire_atom(payload: bytes, *, compound: bool = False) -> bytes:
    if len(payload) == 1 and payload[0] < 0xC0 and not compound:
        return payload
    if len(payload) <= 55:
        return bytes((0xC0 + len(payload),)) + payload
    width = (len(payload).bit_length() + 7) // 8
    return bytes((0xF8 + width,)) + len(payload).to_bytes(width, "little") + payload


def legacy_start_frame() -> bytes:
    no_image = wire_atom(wire_atom(b""), compound=True)
    body = wire_atom(b"derived/kernel-bundle/callgate.json")
    body += wire_atom(b"debug-service")
    body += no_image
    return b"\x01" + wire_atom(body, compound=True)


class PipeChannels:
    """Runner/provider duplex made from two sandbox-safe unidirectional pipes."""

    def __init__(self):
        self.provider_read, self.runner_write = os.pipe()
        self.runner_read, self.provider_write = os.pipe()

    def close(self):
        for fd in (
            self.provider_read,
            self.runner_write,
            self.runner_read,
            self.provider_write,
        ):
            try:
                os.close(fd)
            except OSError:
                pass


class FakeTransaction:
    def __init__(self, token: bytes = b"t" * 32):
        self._prepared = PreparedBoot(
            token,
            resource("derived/kernel-bundle/bundle.json", 1),
            resource("derived/image-bundle/image.json", 2),
            resource("derived/kernel-bundle/kernel", 3),
            resource("derived/image-bundle/rootfs.cpio", 4),
            "/init",
        )
        self.committed = False
        self.aborted = False

    @property
    def prepared(self):
        return self._prepared

    def commit(self):
        self.committed = True

    def abort(self):
        self.aborted = True


class ProviderHandshakeCodecTests(unittest.TestCase):
    def test_combined_image_and_linux_pair_round_trip(self):
        for pair in (False, True):
            with self.subTest(pair=pair):
                expected = execution(pair=pair)
                read_fd, write_fd = os.pipe()
                try:
                    os.write(write_fd, encode_prepare(expected))
                    decoded = read_prepare(read_fd).execution
                finally:
                    os.close(read_fd)
                    os.close(write_fd)
                self.assertEqual(decoded, expected)

    def test_fixed_profiles_are_closed_and_round_trip_with_sarun_codes(self):
        for profile in FixedBootProfile:
            with self.subTest(profile=profile):
                expected = replace(execution(pair=True), fixed_profile=profile)
                read_fd, write_fd = os.pipe()
                try:
                    os.write(write_fd, encode_prepare(expected))
                    decoded = read_prepare(read_fd).execution
                finally:
                    os.close(read_fd)
                    os.close(write_fd)
                self.assertIs(decoded.fixed_profile, profile)

        with self.assertRaisesRegex(HandshakeProtocolError, "requires a fixed"):
            encode_prepare(replace(execution(), fixed_profile=None))

    def test_executable_must_belong_to_finite_catalog(self):
        original = execution()
        outsider = artifact("elsewhere/gcc", b"gcc")
        malformed = SelectedBundleExecutionRequest(
            original.selected_boot,
            (
                CatalogExecutable("make", "/usr/bin/make", original.executables[0].artifact),
                CatalogExecutable("compiler", "elsewhere/gcc", outsider),
                *original.executables[2:],
            ),
            FixedBootProfile.X86_64,
        )
        read_fd, write_fd = os.pipe()
        try:
            os.write(write_fd, encode_prepare(malformed))
            with self.assertRaisesRegex(
                HandshakeProtocolError, "outside the finite provenance catalog"
            ):
                read_prepare(read_fd)
        finally:
            os.close(read_fd)
            os.close(write_fd)

    def test_outcome_codec_is_closed_and_preserves_exact_identities(self):
        expected = FakeTransaction().prepared
        read_fd, write_fd = os.pipe()
        try:
            from inferiors.provider_handshake import encode_outcome

            os.write(write_fd, encode_outcome(expected))
            self.assertEqual(read_outcome(read_fd), expected)
            os.write(write_fd, encode_outcome(Rejected("not-ready", "missing exact vmlinux")))
            self.assertEqual(
                read_outcome(read_fd),
                Rejected("not-ready", "missing exact vmlinux"),
            )
        finally:
            os.close(read_fd)
            os.close(write_fd)


class ProviderHandshakeStateTests(unittest.TestCase):
    def exchange(self, *, commit: bool):
        channels = PipeChannels()
        transaction = FakeTransaction()
        result: list[object] = []

        def server():
            try:
                result.append(
                    serve_pre_rsp_handshake(
                        channels.provider_read,
                        lambda request: transaction,
                        write_fd=channels.provider_write,
                    )
                )
            except BaseException as exc:  # Preserve thread failures for the test.
                result.append(exc)

        thread = threading.Thread(target=server)
        thread.start()
        try:
            os.write(channels.runner_write, encode_prepare(execution(pair=True)))
            prepared = read_outcome(channels.runner_read)
            self.assertIsInstance(prepared, PreparedBoot)
            os.write(channels.runner_write, encode_decision(commit, prepared.token))
            terminal = read_outcome(channels.runner_read)
        finally:
            thread.join(timeout=5)
            channels.close()
        self.assertFalse(thread.is_alive())
        self.assertFalse(result and isinstance(result[0], BaseException), result)
        return transaction, result[0], terminal

    def test_matching_commit_publishes_and_acknowledges(self):
        transaction, served, terminal = self.exchange(commit=True)
        self.assertTrue(served)
        self.assertTrue(transaction.committed)
        self.assertFalse(transaction.aborted)
        self.assertEqual(terminal, ("committed", transaction.prepared.token))

    def test_abort_discards_and_does_not_enter_rsp_mode(self):
        transaction, served, terminal = self.exchange(commit=False)
        self.assertFalse(served)
        self.assertFalse(transaction.committed)
        self.assertTrue(transaction.aborted)
        self.assertEqual(terminal, ("aborted", transaction.prepared.token))

    def test_mismatched_commit_token_aborts_transaction(self):
        channels = PipeChannels()
        transaction = FakeTransaction()
        errors = []

        def server():
            try:
                serve_pre_rsp_handshake(
                    channels.provider_read,
                    lambda request: transaction,
                    write_fd=channels.provider_write,
                )
            except BaseException as exc:
                errors.append(exc)

        thread = threading.Thread(target=server)
        thread.start()
        os.write(channels.runner_write, encode_prepare(execution()))
        prepared = read_outcome(channels.runner_read)
        os.write(channels.runner_write, encode_decision(True, b"x" * 32))
        thread.join(timeout=5)
        channels.close()
        self.assertFalse(thread.is_alive())
        self.assertIsInstance(errors[0], HandshakeProtocolError)
        self.assertTrue(transaction.aborted)
        self.assertFalse(transaction.committed)
        self.assertNotEqual(prepared.token, b"x" * 32)

    def test_preparation_failure_is_a_bounded_rejection(self):
        channels = PipeChannels()
        result = []

        def fail(_request):
            raise OSError("captured artifact is unavailable")

        thread = threading.Thread(
            target=lambda: result.append(
                serve_pre_rsp_handshake(
                    channels.provider_read,
                    fail,
                    write_fd=channels.provider_write,
                )
            )
        )
        thread.start()
        os.write(channels.runner_write, encode_prepare(execution()))
        outcome = read_outcome(channels.runner_read)
        thread.join(timeout=5)
        channels.close()
        self.assertEqual(outcome, Rejected("preparation-failed", "captured artifact is unavailable"))
        self.assertEqual(result, [False])

    def test_committed_v2_exchange_transitions_to_unchanged_v1_start_and_rsp(self):
        channels = PipeChannels()
        transaction = FakeTransaction()
        calls = []
        failures = []

        class Server:
            def serve_connection(self, stream):
                calls.append(("serve", stream))

        class Live:
            server = Server()

            def close(self):
                self.qemu.close()
                calls.append(("live-close",))

        live = Live()

        class InheritedStream:
            def __init__(self, fileno):
                self.fd = fileno

            def recv(self, size):
                return os.read(self.fd, size)

            def close(self):
                if self.fd >= 0:
                    os.close(self.fd)
                    self.fd = -1

        def build(**kwargs):
            self.assertEqual(
                kwargs["manifest_path"],
                "/derived/kernel-bundle/callgate.json",
            )
            self.assertEqual(kwargs["qemu_stream"].recv(7), b"raw-rsp")
            live.qemu = kwargs["qemu_stream"]
            return live

        class Service:
            stream = object()

            def close(self):
                calls.append(("service-close",))

        def provider_thread():
            try:
                run_provider(
                    channels.provider_read,
                    facade_builder=build,
                    service_factory=lambda name: Service(),
                    pre_rsp_prepare=lambda request: transaction,
                    handshake_write_fd=channels.provider_write,
                )
            except BaseException as exc:
                failures.append(exc)

        thread = threading.Thread(target=provider_thread)
        with mock.patch("inferiors.sarun_provider.socket.socket", InheritedStream):
            thread.start()
            os.write(channels.runner_write, encode_prepare(execution(pair=True)))
            prepared = read_outcome(channels.runner_read)
            os.write(channels.runner_write, encode_decision(True, prepared.token))
            self.assertEqual(
                read_outcome(channels.runner_read), ("committed", prepared.token)
            )
            os.write(channels.runner_write, legacy_start_frame() + b"raw-rsp")
            thread.join(timeout=5)
        channels.close()
        self.assertFalse(thread.is_alive())
        self.assertEqual(failures, [])
        self.assertTrue(transaction.committed)
        self.assertEqual(
            calls,
            [("serve", Service.stream), ("service-close",), ("live-close",)],
        )


class SelectedBundleTransactionTests(unittest.TestCase):
    def fake_executor(self, requested, _source, output):
        self.assertEqual(requested, execution(pair=True))
        for relative, contents in (
            ("kernel-bundle/bundle.json", b"kernel-manifest"),
            ("image-bundle/image.json", b"image-manifest"),
            ("kernel-bundle/kernel", b"kernel"),
            ("image-bundle/rootfs.cpio", b"initramfs"),
        ):
            path = output.joinpath(*relative.split("/"))
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)
        return SimpleNamespace(
            output_root=output, plan=SimpleNamespace(kernel_init="/sbin/init")
        )

    def test_commit_atomically_publishes_provider_relative_resources(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            transaction = SelectedBundleTransaction(
                execution(pair=True),
                object(),
                root,
                "sessions/42",
                executor=self.fake_executor,
                token_factory=lambda: b"z" * 32,
            )
            self.assertFalse((root / "sessions/42").exists())
            self.assertEqual(
                transaction.prepared.kernel.path,
                "sessions/42/kernel-bundle/kernel",
            )
            self.assertEqual(transaction.prepared.kernel_init, "/sbin/init")
            transaction.commit()
            self.assertEqual(
                (root / "sessions/42/kernel-bundle/kernel").read_bytes(),
                b"kernel",
            )

    def test_abort_removes_staging_without_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            transaction = SelectedBundleTransaction(
                execution(pair=True),
                object(),
                root,
                "sessions/43",
                executor=self.fake_executor,
            )
            transaction.abort()
            self.assertFalse((root / "sessions/43").exists())
            self.assertEqual(list(root.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
