import threading
import unittest

from inferiors.qemu_rsp import QemuRspClient, RspRemoteError, RspRestorationError
from inferiors.rsp_transport import RspStream
from test_rsp_support import memory_duplex_pair


class ScriptedRemote:
    def __init__(self, stream, script):
        self.stream = stream
        self.script = script
        self.seen = []
        self.error = None

    def run(self):
        try:
            for expected, reply in self.script:
                packet = self.stream.receive_packet(2)
                self.seen.append(packet)
                if packet != expected:
                    raise AssertionError((packet, expected))
                if reply is not None:
                    self.stream.send_packet(reply, 2)
        except BaseException as exc:
            self.error = exc


class QemuRspClientTests(unittest.TestCase):
    def run_script(self, script, operation):
        client_socket, remote_socket = memory_duplex_pair()
        client = QemuRspClient(client_socket, timeout=2)
        remote = ScriptedRemote(RspStream(remote_socket), script)
        thread = threading.Thread(target=remote.run)
        thread.start()
        try:
            result = operation(client)
        finally:
            thread.join(3)
            client.close()
            remote.stream.close()
        self.assertFalse(thread.is_alive())
        if remote.error:
            raise remote.error
        return result, remote.seen

    def test_physical_read_enters_and_restores_mode(self):
        script = [
            (b"qqemu.PhyMemMode", b"0"),
            (b"Qqemu.PhyMemMode:1", b"OK"),
            (b"m1000,4", b"01020304"),
            (b"Qqemu.PhyMemMode:0", b"OK"),
        ]
        result, seen = self.run_script(script, lambda client: client.read_physical(0x1000, 4))
        self.assertEqual(result, b"\x01\x02\x03\x04")
        self.assertEqual(seen, [command for command, _ in script])

    def test_physical_read_restores_mode_after_remote_error(self):
        script = [
            (b"qqemu.PhyMemMode", b"0"),
            (b"Qqemu.PhyMemMode:1", b"OK"),
            (b"m2000,4", b"E14"),
            (b"Qqemu.PhyMemMode:0", b"OK"),
        ]

        def operation(client):
            with self.assertRaises(RspRemoteError):
                client.read_physical(0x2000, 4)

        _, seen = self.run_script(script, operation)
        self.assertEqual(seen[-1], b"Qqemu.PhyMemMode:0")

    def test_physical_mode_reports_operation_and_cleanup_failures(self):
        script = [
            (b"qqemu.PhyMemMode", b"0"),
            (b"Qqemu.PhyMemMode:1", b"OK"),
            (b"m2000,4", b"E14"),
            (b"Qqemu.PhyMemMode:0", b"E01"),
        ]

        def operation(client):
            with self.assertRaises(RspRestorationError) as caught:
                client.read_physical(0x2000, 4)
            self.assertIsInstance(caught.exception.primary, RspRemoteError)
            self.assertIsInstance(caught.exception.cleanup, RspRemoteError)
            self.assertIn("E14", str(caught.exception))
            self.assertIn("E01", str(caught.exception))

        self.run_script(script, operation)

    def test_physical_write_enters_and_restores_mode(self):
        script = [
            (b"qqemu.PhyMemMode", b"0"),
            (b"Qqemu.PhyMemMode:1", b"OK"),
            (b"M3000,4:deadbeef", b"OK"),
            (b"Qqemu.PhyMemMode:0", b"OK"),
        ]
        _, seen = self.run_script(
            script, lambda client: client.write_physical(0x3000, bytes.fromhex("deadbeef"))
        )
        self.assertEqual(seen, [command for command, _ in script])

    def test_large_physical_read_is_packet_safely_chunked_in_one_mode_window(self):
        first = bytes((index & 0xFF for index in range(0x400)))
        second = b"x"
        script = [
            (b"qqemu.PhyMemMode", b"0"),
            (b"Qqemu.PhyMemMode:1", b"OK"),
            (b"m1000,400", first.hex().encode()),
            (b"m1400,1", second.hex().encode()),
            (b"Qqemu.PhyMemMode:0", b"OK"),
        ]
        result, _ = self.run_script(script, lambda client: client.read_physical(0x1000, 0x401))
        self.assertEqual(result, first + second)

    def test_monitor_command_collects_console_output_packets(self):
        command = b"qRcmd," + b"gva2gpa 0xffff0000".hex().encode()
        script = [
            (command, b"O" + b"gpa: ".hex().encode()),
            # ScriptedRemote sends at most one reply per request, so the two
            # remaining packets are represented as independently queued sends
            # by a specialized remote below.
        ]
        client_socket, remote_socket = memory_duplex_pair()
        client = QemuRspClient(client_socket, timeout=2)
        stream = RspStream(remote_socket)
        error = []

        def remote_run():
            try:
                self.assertEqual(stream.receive_packet(2), command)
                stream.send_packet(b"O" + b"gpa: ".hex().encode(), 2)
                stream.send_packet(b"O" + b"0x1234\n".hex().encode(), 2)
                stream.send_packet(b"OK", 2)
            except BaseException as exc:
                error.append(exc)

        thread = threading.Thread(target=remote_run)
        thread.start()
        try:
            self.assertEqual(client.monitor_command("gva2gpa 0xffff0000"), "gpa: 0x1234\n")
        finally:
            thread.join(3)
            client.close()
            stream.close()
        self.assertFalse(thread.is_alive())
        if error:
            raise error[0]

    def test_xfer_thread_and_register_primitives(self):
        script = [
            (b"qXfer:features:read:target.xml:0,800", b"m<tar"),
            (b"qXfer:features:read:target.xml:4,800", b"lget/>"),
            (b"qfThreadInfo", b"m1,2"),
            (b"qsThreadInfo", b"l"),
            (b"qC", b"QC2"),
            (b"Hg1", b"OK"),
            (b"p20", b"7856341200000000"),
            (b"P20=efcdab9000000000", b"OK"),
        ]

        def operation(client):
            self.assertEqual(client.read_xfer("features", "target.xml"), b"<target/>")
            self.assertEqual(client.thread_ids(), ("1", "2"))
            self.assertEqual(client.current_thread(), "2")
            client.select_thread("g", "1")
            self.assertEqual(client.read_register(0x20), bytes.fromhex("7856341200000000"))
            client.write_register(0x20, bytes.fromhex("efcdab9000000000"))

        _, seen = self.run_script(script, operation)
        self.assertEqual(seen, [command for command, _ in script])

    def test_thread_specific_resume_requires_and_uses_vcont(self):
        script = [
            (b"vCont?", b"vCont;c;C;s;S"),
            (b"vCont;c:p1.2", None),
        ]
        _, seen = self.run_script(script, lambda client: client.resume_thread("p1.2"))
        self.assertEqual(seen, [command for command, _ in script])

    def test_step_uses_explicit_vcont_thread_without_changing_hc(self):
        script = [
            (b"qfThreadInfo", b"m1,2"),
            (b"qsThreadInfo", b"l"),
            (b"vCont?", b"vCont;c;s"),
            (b"vCont;s:2", None),
        ]
        _, seen = self.run_script(script, lambda client: client.step(1))
        self.assertNotIn(b"Hc2", seen)
        self.assertEqual(seen, [command for command, _ in script])

    def test_vcont_capabilities_are_cached_across_continue_and_step(self):
        script = [
            (b"vCont?", b"vCont;c;s"),
            (b"vCont;c:1", None),
            (b"vCont;s:2", None),
        ]

        def operation(client):
            client.resume_thread("1")
            client.step_thread("2")

        _, seen = self.run_script(script, operation)
        self.assertEqual(seen.count(b"vCont?"), 1)

    def test_complete_register_block_selects_cpu_and_restores_selection(self):
        script = [
            (b"qC", b"QC2"),
            (b"Hg1", b"OK"),
            (b"g", b"01020304"),
            (b"Hg2", b"OK"),
        ]
        result, seen = self.run_script(
            script, lambda client: client.read_register_block("1")
        )
        self.assertEqual(result, bytes.fromhex("01020304"))
        self.assertEqual(seen, [command for command, _ in script])


if __name__ == "__main__":
    unittest.main()
