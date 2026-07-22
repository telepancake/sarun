from __future__ import annotations

import os
from pathlib import Path
import json
import subprocess
import tempfile
import threading
import time
import unittest

from test_rsp_support import memory_duplex_pair
from test_rsp_transport import RspTransportTests


PROJECT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(
    os.environ.get("VIROS_RUN_SOCKET_TESTS") == "1",
    "set VIROS_RUN_SOCKET_TESTS=1 for the real GDB integration",
)
class RealGdbExecutableIdentityTests(unittest.TestCase):
    def test_qxfer_executable_names_are_available_to_gdb_python(self):
        qemu_client, qemu_remote = memory_duplex_pair()
        server = thread = None
        try:
            with tempfile.TemporaryDirectory(prefix="managed-gdb-", dir=".") as directory:
                path = os.path.abspath(os.path.join(directory, "gdb.sock"))
                fixture = RspTransportTests()
                server, _ = fixture.make_server(path, qemu_client)
                thread = threading.Thread(target=server.serve_once)
                thread.start()
                deadline = time.monotonic() + 2
                while not os.path.exists(path) and time.monotonic() < deadline:
                    time.sleep(0.005)
                result = subprocess.run(
                    [
                        str(PROJECT / "tools/gdb/bin/gdb"),
                        "-nx",
                        "-batch",
                        "-ex",
                        f"target remote {path}",
                        "-ex",
                        "python print('EXEC=' + '|'.join(str(i.progspace.filename) for i in gdb.inferiors()))",
                        "-ex",
                        "python [(gdb.execute('inferior %d' % i.num, to_string=True), print('PACKET%d=' % i.pid + gdb.execute('maintenance packet qXfer:exec-file:read:%x:0,1000' % i.pid, to_string=True).strip())) for i in gdb.inferiors()]",
                    ],
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    timeout=15,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertIn("PACKET1=sending: qXfer:exec-file:read:1:0,1000", result.stdout)
                self.assertIn('received: "l/sbin/init"', result.stdout)
                self.assertIn('received: "l/usr/sbin/zebra"', result.stdout)
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

    def test_catalog_loads_symbols_without_using_host_guest_paths(self):
        qemu_client, qemu_remote = memory_duplex_pair()
        server = thread = None
        try:
            with tempfile.TemporaryDirectory(prefix="managed-symbols-", dir=".") as directory:
                path = os.path.abspath(os.path.join(directory, "gdb.sock"))
                fixture = RspTransportTests()
                server, _ = fixture.make_server(path, qemu_client)
                thread = threading.Thread(target=server.serve_once)
                thread.start()
                deadline = time.monotonic() + 2
                while not os.path.exists(path) and time.monotonic() < deadline:
                    time.sleep(0.005)
                rows = json.dumps(
                    [
                        {
                            "guest_path": "/usr/sbin/zebra",
                            "debug_elf": "/bin/ls",
                            "build_id": "0123456789abcdef",
                        }
                    ],
                    separators=(",", ":"),
                )
                result = subprocess.run(
                    [
                        str(PROJECT / "tools/gdb/bin/gdb"),
                        "-nx",
                        "-batch",
                        "-ex",
                        "set confirm off",
                        "-ex",
                        f"set sysroot {PROJECT / 'tools/gdb/sysroot'}",
                        "-ex",
                        f"python import sys; sys.path.insert(0, {str(PROJECT)!r})",
                        "-ex",
                        "python from inferiors.sarun_gdb_symbols import install, finalize",
                        "-ex",
                        f"python install({rows!r} and __import__('json').loads({rows!r}), '/bin/sh')",
                        "-ex",
                        f"target remote {path}",
                        "-ex",
                        "python finalize()",
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
                self.assertIn("inferior 2 /usr/sbin/zebra -> /bin/ls", result.stdout)
                self.assertIn("kernel symbols are inferior 3", result.stdout)
                self.assertNotIn("Reading symbols from /sbin/init", result.stdout)
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


if __name__ == "__main__":
    unittest.main()
