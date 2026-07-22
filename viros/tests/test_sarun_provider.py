from __future__ import annotations

import os
import subprocess
import unittest
from unittest import mock

from inferiors.sarun_provider import (
    DebugExecutable,
    DebugImageCatalog,
    DebugProviderStart,
    PipeDuplex,
    ProviderProtocolError,
    SarunServiceAccept,
    main,
    read_provider_start,
    run_provider,
)


def atom(payload: bytes, *, compound: bool = False) -> bytes:
    if len(payload) == 1 and payload[0] < 0xC0 and not compound:
        return payload
    if len(payload) <= 55:
        return bytes((0xC0 + len(payload),)) + payload
    width = (len(payload).bit_length() + 7) // 8
    return bytes((0xF8 + width,)) + len(payload).to_bytes(width, "little") + payload


def uint(value: int) -> bytes:
    size = (value.bit_length() + 7) // 8
    return atom(value.to_bytes(size, "little"))


def no_image() -> bytes:
    return atom(uint(0), compound=True)


def image_catalog(
    *,
    profile: int = 1,
    elf_class: int = 64,
    machine: int = 183,
    executable_identity: bytes = b"0123456789abcdef",
) -> bytes:
    executable = b"".join(
        (
            atom(b"/usr/sbin/quagga"),
            atom(executable_identity),
            atom(bytes(range(32))),
            uint(1234),
            atom(b"openwrt/debug/quagga"),
            atom(bytes(reversed(range(32)))),
            uint(4321),
            uint(elf_class),
            uint(machine),
            uint(1),
        )
    )
    executable_list = uint(1) + atom(executable, compound=True)
    catalog = b"".join(
        (
            atom(b"openwrt/image.json"),
            uint(profile),
            atom(b"/sbin/init"),
            atom(executable_list, compound=True),
        )
    )
    return atom(uint(1) + atom(catalog, compound=True), compound=True)


def start_frame(
    manifest: bytes = b"bundle/callgate.json",
    service: str = "debug-7",
    image: bytes | None = None,
) -> bytes:
    fields = atom(manifest) + atom(service.encode()) + (no_image() if image is None else image)
    return atom(b"\x01") + atom(fields, compound=True)


class SarunProviderProtocolTests(unittest.TestCase):
    def test_start_frame_leaves_pipelined_qemu_rsp_bytes_unread(self):
        provider, runner = os.pipe()
        raw_rsp = b"+$T05thread:01;#00"
        try:
            os.write(runner, start_frame() + raw_rsp)
            self.assertEqual(
                read_provider_start(provider),
                DebugProviderStart(b"bundle/callgate.json", "debug-7", None),
            )
            self.assertEqual(os.read(provider, len(raw_rsp)), raw_rsp)
        finally:
            os.close(provider)
            os.close(runner)

    def test_start_frame_rejects_wrong_version_utf8_and_trailing_fields(self):
        cases = [
            atom(b"\x02") + atom(atom(b"m") + atom(b"svc") + no_image(), compound=True),
            atom(b"\x01") + atom(atom(b"m") + atom(b"\xff") + no_image(), compound=True),
            atom(b"\x01")
            + atom(atom(b"m") + atom(b"svc") + no_image() + atom(b"extra"), compound=True),
        ]
        for encoded in cases:
            with self.subTest(encoded=encoded):
                provider, runner = os.pipe()
                try:
                    os.write(runner, encoded)
                    with self.assertRaises(ProviderProtocolError):
                        read_provider_start(provider)
                finally:
                    os.close(provider)
                    os.close(runner)

    def test_start_frame_delivers_validated_userspace_catalog(self):
        provider, runner = os.pipe()
        try:
            os.write(runner, start_frame(image=image_catalog()))
            start = read_provider_start(provider)
        finally:
            os.close(provider)
            os.close(runner)
        self.assertEqual(
            start.image,
            DebugImageCatalog(
                b"openwrt/image.json",
                1,
                b"/sbin/init",
                (
                    DebugExecutable(
                        b"/usr/sbin/quagga",
                        b"0123456789abcdef",
                        bytes(range(32)),
                        1234,
                        b"openwrt/debug/quagga",
                        bytes(reversed(range(32))),
                        4321,
                        64,
                        183,
                    ),
                ),
            ),
        )

    def test_start_frame_preserves_loadable_content_fallback_identity(self):
        fingerprint = b"a5" * 32
        provider, runner = os.pipe()
        try:
            os.write(
                runner,
                start_frame(
                    image=image_catalog(executable_identity=fingerprint)
                ),
            )
            image = read_provider_start(provider).image
        finally:
            os.close(provider)
            os.close(runner)
        self.assertIsNotNone(image)
        self.assertEqual(image.executables[0].build_id, fingerprint)

    def test_start_frame_accepts_fixed_armv7_and_mmips_profiles(self):
        for profile, machine in ((3, 40), (4, 8)):
            with self.subTest(profile=profile):
                provider, runner = os.pipe()
                try:
                    os.write(
                        runner,
                        start_frame(
                            image=image_catalog(
                                profile=profile, elf_class=32, machine=machine
                            )
                        ),
                    )
                    image = read_provider_start(provider).image
                finally:
                    os.close(provider)
                    os.close(runner)
                self.assertIsNotNone(image)
                self.assertEqual(image.profile, profile)
                self.assertEqual(image.executables[0].elf_class, 32)
                self.assertEqual(image.executables[0].elf_machine, machine)

    def test_start_frame_rejects_non_resource_relative_manifest_paths(self):
        for manifest in (b"/host/file", b"../bundle/callgate.json", b"a//b", b"a\\b"):
            with self.subTest(manifest=manifest):
                provider, runner = os.pipe()
                try:
                    os.write(runner, start_frame(manifest))
                    with self.assertRaises(ProviderProtocolError):
                        read_provider_start(provider)
                finally:
                    os.close(provider)
                    os.close(runner)

    def test_pipe_duplex_maps_stdout_to_reads_and_stdin_to_writes(self):
        request_read, request_write = os.pipe()
        response_read, response_write = os.pipe()
        stream = PipeDuplex(
            os.fdopen(request_read, "rb", buffering=0),
            os.fdopen(response_write, "wb", buffering=0),
        )
        try:
            os.write(request_write, b"request")
            self.assertEqual(stream.recv(7), b"request")
            stream.sendall(b"response")
            self.assertEqual(os.read(response_read, 8), b"response")
        finally:
            stream.close()
            os.close(request_write)
            os.close(response_read)

    def test_service_accept_uses_only_the_generic_per_session_command(self):
        request_read, request_write = os.pipe()
        response_read, response_write = os.pipe()

        class Process:
            stdin = os.fdopen(response_write, "wb", buffering=0)
            stdout = os.fdopen(request_read, "rb", buffering=0)

            def wait(self, timeout=None):
                return 0

        with mock.patch("subprocess.Popen", return_value=Process()) as popen:
            accept = SarunServiceAccept("debug-session-23")
            accept.close()
        popen.assert_called_once_with(
            ["sarun", "service", "accept", "debug-session-23"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
            close_fds=True,
        )
        os.close(request_write)
        os.close(response_read)

    def test_run_provider_uses_inherited_rsp_and_service_stream(self):
        provider, runner = os.pipe()
        os.write(runner, start_frame(b"bundle/exact.json", "debug-44") + b"raw-rsp")
        calls = []

        class InheritedStream:
            def __init__(self, fileno):
                self.fd = fileno

            def recv(self, size):
                return os.read(self.fd, size)

            def close(self):
                if self.fd >= 0:
                    os.close(self.fd)
                    self.fd = -1

        class Server:
            def serve_connection(self, stream):
                calls.append(("serve", stream))

        class Live:
            server = Server()

            def close(self):
                calls.append(("live-close",))
                self.qemu.close()

        live = Live()

        def build(**kwargs):
            self.assertIsNone(kwargs["qemu_socket"])
            self.assertIsNone(kwargs["gdb_socket"])
            self.assertEqual(kwargs["manifest_path"], "/bundle/exact.json")
            self.assertEqual(kwargs["qemu_stream"].recv(7), b"raw-rsp")
            live.qemu = kwargs["qemu_stream"]
            return live

        class Service:
            stream = object()

            def close(self):
                calls.append(("service-close",))

        def service(name):
            self.assertEqual(name, "debug-44")
            return Service()

        try:
            with mock.patch(
                "inferiors.sarun_provider.socket.socket", InheritedStream
            ):
                run_provider(provider, facade_builder=build, service_factory=service)
        finally:
            os.close(provider)
            os.close(runner)
        self.assertEqual(
            calls,
            [("serve", Service.stream), ("service-close",), ("live-close",)],
        )

    def test_provider_entrypoint_has_no_argument_or_environment_fallback(self):
        with mock.patch("inferiors.sarun_provider.run_provider") as run:
            self.assertEqual(main(["--manifest", "/host/file"]), 2)
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
