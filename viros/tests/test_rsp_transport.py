import os
import socket
import subprocess
import tempfile
import threading
import time
import unittest

from inferiors.linux_oracle import Snapshot, TaskId, TaskSnapshot
from inferiors.qemu_rsp import QemuRspClient
from inferiors.rsp_proxy import FacadeState, RspFacade
from inferiors.rsp_server import UnixRspServer
from inferiors.rsp_transport import RspStream
from inferiors.rsp_codec import frame_packet
from test_rsp_support import memory_duplex_pair


def aarch64_target_xml():
    registers = "".join(
        f'<reg name="x{index}" bitsize="64" regnum="{index}"/>'
        for index in range(31)
    )
    registers += (
        '<reg name="sp" bitsize="64" type="data_ptr" regnum="31"/>'
        '<reg name="pc" bitsize="64" type="code_ptr" regnum="32"/>'
        '<reg name="cpsr" bitsize="32" regnum="33"/>'
    )
    return (
        '<?xml version="1.0"?><target><architecture>aarch64</architecture>'
        '<feature name="org.gnu.gdb.aarch64.core">'
        + registers
        + "</feature></target>"
    ).encode()


class TinyOracle:
    def snapshot(self):
        return Snapshot(1, (
            TaskSnapshot(TaskId(1, 1), 1, "init", "/sbin/init", b"AUX"),
            TaskSnapshot(TaskId(42, 42), 2, "zebra", "/usr/sbin/zebra", b"AUX2"),
        ))

    def read_memory(self, task, address, length):
        return bytes(length)

    def write_memory(self, task, address, data):
        pass

    def read_registers(self, task):
        return bytes(31 * 8 + 8 + 8 + 4)

    def write_registers(self, task, data):
        pass


class RspTransportTests(unittest.TestCase):
    def make_server(self, path, qemu_socket):
        qemu = QemuRspClient(qemu_socket, timeout=2)
        facade = RspFacade(TinyOracle(), qemu, aarch64_target_xml())
        return UnixRspServer(path, facade, qemu), facade

    def test_bad_checksum_is_nacked_and_stream_recovers(self):
        receiver_endpoint, sender_endpoint = memory_duplex_pair()
        receiver = RspStream(receiver_endpoint)
        try:
            sender_endpoint.sendall(b"$qC#00" + frame_packet(b"qC"))
            self.assertEqual(receiver.receive_packet(2), b"qC")
            self.assertEqual(sender_endpoint.recv(2), b"-+")
        finally:
            receiver.close()
            sender_endpoint.close()

    def test_nack_retransmits_packet(self):
        client_endpoint, remote_endpoint = memory_duplex_pair()
        client = RspStream(client_endpoint)
        framed = frame_packet(b"qC")
        errors = []

        def remote():
            try:
                self.assertEqual(remote_endpoint.recv(len(framed)), framed)
                remote_endpoint.sendall(b"-")
                self.assertEqual(remote_endpoint.recv(len(framed)), framed)
                remote_endpoint.sendall(b"+")
            except BaseException as exc:
                errors.append(exc)

        thread = threading.Thread(target=remote)
        thread.start()
        try:
            client.send_packet(b"qC", 2)
            thread.join(2)
            self.assertFalse(thread.is_alive())
            if errors:
                raise errors[0]
        finally:
            client.close()
            remote_endpoint.close()
            thread.join(2)

    @unittest.skipUnless(
        os.environ.get("VIROS_RUN_SOCKET_TESTS") == "1",
        "set VIROS_RUN_SOCKET_TESTS=1 to exercise a real AF_UNIX listener",
    )
    def test_real_unix_listener_acknowledges_and_replies(self):
        qemu_client, qemu_remote = socket.socketpair()
        server = None
        thread = None
        gdb = None
        try:
            with tempfile.TemporaryDirectory(prefix="viros-rsp-", dir=".") as directory:
                path = os.path.join(directory, "gdb.sock")
                server, _ = self.make_server(path, qemu_client)
                thread = threading.Thread(target=server.serve_once)
                thread.start()
                deadline = time.monotonic() + 2
                while not os.path.exists(path) and time.monotonic() < deadline:
                    time.sleep(0.005)
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(path)
                gdb = RspStream(sock)
                gdb.send_packet(b"qC", 2)
                self.assertEqual(gdb.receive_packet(2), b"QCp1.1")
                gdb.close()
                gdb = None
                thread.join(2)
                self.assertFalse(thread.is_alive())
        finally:
            if gdb is not None:
                gdb.close()
            if server is not None:
                server.qemu.close()
            else:
                qemu_client.close()
            qemu_remote.close()
            if thread is not None:
                thread.join(2)

    @unittest.skipUnless(
        os.environ.get("VIROS_RUN_SOCKET_TESTS") == "1"
        and os.path.isfile("tools/gdb/bin/gdb")
        and os.access("tools/gdb/bin/gdb", os.X_OK),
        "requires VIROS_RUN_SOCKET_TESTS=1 and the project GDB build",
    )
    def test_real_gdb_creates_distinct_inferiors(self):
        qemu_client, qemu_remote = memory_duplex_pair()
        server = None
        thread = None
        try:
            with tempfile.TemporaryDirectory(prefix="viros-gdb-", dir=".") as directory:
                path = os.path.abspath(os.path.join(directory, "gdb.sock"))
                server, _ = self.make_server(path, qemu_client)
                thread = threading.Thread(target=server.serve_once)
                thread.start()
                deadline = time.monotonic() + 2
                while not os.path.exists(path) and time.monotonic() < deadline:
                    time.sleep(0.005)
                result = subprocess.run(
                    [
                        os.path.abspath("tools/gdb/bin/gdb"),
                        "-nx",
                        "-batch",
                        "-ex",
                        "set pagination off",
                        "-ex",
                        f"target remote {path}",
                        "-ex",
                        "info inferiors",
                    ],
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    timeout=15,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertIn("[New inferior 2]", result.stdout)
                self.assertRegex(result.stdout, r"(?m)^\*?\s*1\s+process 1\b")
                self.assertRegex(result.stdout, r"(?m)^\s*2\s+process 42\b")
                thread.join(2)
                self.assertFalse(thread.is_alive())
        finally:
            if server is not None:
                server.qemu.close()
            else:
                qemu_client.close()
            qemu_remote.close()
            if thread is not None:
                thread.join(2)

    def test_ctrl_c_is_forwarded_and_stop_is_returned(self):
        upstream_server, upstream_client = memory_duplex_pair()
        qemu_client, qemu_remote_socket = memory_duplex_pair()
        server, facade = self.make_server("unused.sock", qemu_client)
        facade.state = FacadeState.RUNNING
        thread = threading.Thread(target=server.serve_connection, args=(upstream_server,))
        thread.start()

        self.assertEqual(upstream_client.send(b"\x03"), 1)
        self.assertEqual(qemu_remote_socket.recv(1), b"\x03")
        remote = RspStream(qemu_remote_socket)
        remote.send_packet(b"T02thread:1;", 2)
        gdb = RspStream(upstream_client)
        self.assertEqual(gdb.receive_packet(2), b"T02thread:p1.1;")
        gdb.close()
        thread.join(2)
        self.assertFalse(thread.is_alive())
        remote.close()
        server.qemu.close()


if __name__ == "__main__":
    unittest.main()
