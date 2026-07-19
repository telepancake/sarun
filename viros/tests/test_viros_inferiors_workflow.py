from __future__ import annotations

import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest


PROJECT = Path(__file__).resolve().parents[1]


def write_executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(body).lstrip(), encoding="utf-8")
    path.chmod(0o755)


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


class InferiorsWorkflowTests(unittest.TestCase):
    @unittest.skipUnless(
        os.environ.get("VIROS_RUN_SOCKET_TESTS") == "1",
        "requires VIROS_RUN_SOCKET_TESTS=1 because the managed sandbox denies AF_UNIX bind",
    )
    def test_mock_workflow_cleans_children_and_project_local_sockets(self):
        with tempfile.TemporaryDirectory(prefix="w-", dir=PROJECT) as raw:
            work = Path(raw)
            records = work / "records"
            records.mkdir()
            boot = work / "local-initramfs-kernel.bin"
            manifest = work / "callgate.json"
            vmlinux = work / "exact-vmlinux"
            boot.write_bytes(b"bootable local OpenWrt kernel")
            manifest.write_text("{}\n", encoding="utf-8")
            vmlinux.write_bytes(b"matching local vmlinux")

            fake_python = work / "fake-python"
            write_executable(
                fake_python,
                r"""
                #!/usr/bin/env python3
                import os
                from pathlib import Path
                import socket
                import sys

                if "-c" in sys.argv:
                    print(os.environ["FAKE_VMLINUX"])
                    raise SystemExit(0)
                if sys.argv[1:3] != ["-m", "inferiors.live_facade"]:
                    raise SystemExit("unexpected fake-python invocation")
                gdb_path = sys.argv[sys.argv.index("--gdb-socket") + 1]
                Path(os.environ["RECORDS"], "facade.pid").write_text(str(os.getpid()))
                server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                server.bind(gdb_path)
                server.listen(1)
                connection, _ = server.accept()
                connection.close()
                server.close()
                try:
                    os.unlink(gdb_path)
                except FileNotFoundError:
                    pass
                """,
            )

            fake_qemu = work / "tools" / "qemu" / "bin" / "qemu-system-aarch64"
            write_executable(
                fake_qemu,
                r"""
                #!/usr/bin/env python3
                import os
                from pathlib import Path
                import signal
                import socket
                import sys
                import time

                Path(os.environ["RECORDS"], "qemu.pid").write_text(str(os.getpid()))
                Path(os.environ["RECORDS"], "qemu.args").write_text("\n".join(sys.argv[1:]))
                gdb_spec = sys.argv[sys.argv.index("-gdb") + 1]
                gdb_path = gdb_spec.split("unix:path=", 1)[1].split(",", 1)[0]
                chardev = sys.argv[sys.argv.index("-chardev") + 1]
                console_path = chardev.split("path=", 1)[1].split(",", 1)[0]
                servers = []
                for path in (gdb_path, console_path):
                    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    server.bind(path)
                    server.listen(2)
                    servers.append(server)
                stopping = False
                def stop(_signum, _frame):
                    global stopping
                    stopping = True
                signal.signal(signal.SIGTERM, stop)
                while not stopping:
                    time.sleep(0.02)
                for server in servers:
                    server.close()
                """,
            )

            fake_gdb = work / "tools" / "gdb" / "bin" / "gdb"
            write_executable(
                fake_gdb,
                r"""
                #!/usr/bin/env python3
                import os
                from pathlib import Path
                import socket
                import sys

                record = Path(os.environ["RECORDS"], "gdb.args")
                with record.open("a", encoding="utf-8") as stream:
                    stream.write("INVOCATION\n" + "\n".join(sys.argv[1:]) + "\n")
                if "-batch" in sys.argv:
                    print("viros-bootstrap: stopped at ret_to_user")
                    raise SystemExit(0)
                commands = [sys.argv[index + 1] for index, value in enumerate(sys.argv[:-1]) if value == "-ex"]
                remote = next(value.removeprefix("target remote ") for value in commands if value.startswith("target remote "))
                client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                client.connect(remote)
                client.close()
                """,
            )

            uv_source = work / "uv-source"
            write_executable(
                uv_source / "uv",
                """
                #!/usr/bin/env bash
                printf '%s\\n' "$FAKE_PYTHON"
                """,
            )
            archive = work / "downloads" / "uv-0.11.21-{}-unknown-linux-gnu.tar.gz".format(
                "x86_64" if os.uname().machine == "x86_64" else "aarch64"
            )
            archive.parent.mkdir(parents=True)
            with tarfile.open(archive, "w:gz") as stream:
                stream.add(uv_source, arcname="uv-package")
            uv_hash = hashlib.sha256(archive.read_bytes()).hexdigest()

            environment = os.environ.copy()
            environment.update(
                {
                    "VIROS_WORKDIR": str(work),
                    "FAKE_PYTHON": str(fake_python),
                    "FAKE_VMLINUX": str(vmlinux),
                    "RECORDS": str(records),
                    "DEBUG_BOOT_TIMEOUT": "5",
                    "UV_X86_64_SHA256": uv_hash,
                    "UV_AARCH64_SHA256": uv_hash,
                }
            )
            result = subprocess.run(
                [
                    str(PROJECT / "viros.sh"),
                    "inferiors",
                    "openwrt-arm64",
                    str(manifest),
                    str(boot),
                ],
                cwd=work,
                env=environment,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=15,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertIn("facade GDB socket:", result.stdout)
            self.assertIn("session logs:", result.stdout)
            self.assertIn(str(boot), (records / "qemu.args").read_text())
            gdb_arguments = (records / "gdb.args").read_text()
            self.assertEqual(gdb_arguments.count("INVOCATION\n"), 2)
            self.assertIn(str(vmlinux), gdb_arguments)
            self.assertIn("thbreak ret_to_user", gdb_arguments)

            for name in ("qemu.pid", "facade.pid"):
                self.assertFalse(alive(int((records / name).read_text())), name)
            self.assertEqual(list((work / "build").glob("i?-*")), [])
            sessions = list((work / "artifacts" / "openwrt-arm64").glob("inferiors-*"))
            self.assertEqual(len(sessions), 1)
            self.assertTrue((sessions[0] / "bootstrap-gdb.log").is_file())
            self.assertTrue((sessions[0] / "facade.log").is_file())

    def test_rejects_official_default_or_missing_inputs_before_launch(self):
        result = subprocess.run(
            [str(PROJECT / "viros.sh"), "inferiors", "openwrt-arm64"],
            cwd=PROJECT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage: ./viros.sh inferiors", result.stdout)

        result = subprocess.run(
            [str(PROJECT / "viros.sh"), "inferiors", "mmips", "unexpected"],
            cwd=PROJECT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage: ./viros.sh inferiors mmips", result.stdout)

    def test_mmips_workflow_uses_matching_kernel_init_and_console_shape(self):
        script = (PROJECT / "viros.sh").read_text(encoding="utf-8")
        body = script.split("inferiors_stage() {", 1)[1].split(
            "\ndebug_qemu_failed()", 1
        )[0]
        self.assertIn('manifest="$ARTIFACTS/mmips/inferiors/callgate.json"', body)
        self.assertIn('-M malta -cpu 34Kf -smp 1 -m 256M', body)
        self.assertIn('-ex \'thbreak start_thread\' -ex continue', body)
        self.assertIn('-ex "thbreak *$init_entry" -ex continue', body)
        self.assertIn('console_chardev=mikrotik-mmips-uart', body)
        self.assertIn("-ex 'set architecture mips'", body)


if __name__ == "__main__":
    unittest.main()
