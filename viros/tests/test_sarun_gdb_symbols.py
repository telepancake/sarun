from __future__ import annotations

import importlib
import sys
import types
import unittest


class Event:
    def __init__(self):
        self.callback = None

    def connect(self, callback):
        self.callback = callback


class Inferior:
    def __init__(self, number, filename, pid):
        self.num = number
        self.pid = pid
        self.progspace = types.SimpleNamespace(filename=filename)

    def is_valid(self):
        return True


class ManagedGdbSymbolTests(unittest.TestCase):
    def setUp(self):
        self.inferiors = [Inferior(1, "/vmlinux", 0), Inferior(2, None, 42)]
        self.selected = self.inferiors[0]
        self.commands = []
        self.output = []
        fake = types.ModuleType("gdb")
        fake.error = RuntimeError
        fake.inferiors = lambda: self.inferiors
        fake.selected_inferior = lambda: self.selected
        fake.write = self.output.append
        fake.events = types.SimpleNamespace(
            new_inferior=Event(), stop=Event(), new_objfile=Event()
        )

        def execute(command, **_kwargs):
            self.commands.append(command)
            if command.startswith("inferior "):
                number = int(command.split()[1])
                self.selected = next(row for row in self.inferiors if row.num == number)
            elif command.startswith("maintenance packet "):
                return 'sending: request\nreceived: "l/usr/sbin/quagga"\n'
            elif command.startswith("add-inferior "):
                self.inferiors.append(Inferior(3, "/image/kernel/vmlinux", 0))
            return ""

        fake.execute = execute
        self.old_gdb = sys.modules.get("gdb")
        sys.modules["gdb"] = fake
        sys.modules.pop("inferiors.sarun_gdb_symbols", None)
        self.module = importlib.import_module("inferiors.sarun_gdb_symbols")
        self.fake = fake

    def tearDown(self):
        sys.modules.pop("inferiors.sarun_gdb_symbols", None)
        if self.old_gdb is None:
            sys.modules.pop("gdb", None)
        else:
            sys.modules["gdb"] = self.old_gdb

    def test_install_loads_exact_catalog_elf_and_restores_kernel_inferior(self):
        self.module.install(
            [
                {
                    "guest_path": "/usr/sbin/quagga",
                    "debug_elf": "/image/debug/quagga with symbols",
                    "build_id": "0123456789abcdef",
                }
            ],
            "/image/kernel/vmlinux",
        )
        self.assertIn('file "/image/debug/quagga with symbols"', self.commands)
        self.assertIn('add-symbol-file "/image/kernel/vmlinux"', self.commands)
        self.assertEqual(self.selected.num, 1)
        self.assertIn("inferior 2 /usr/sbin/quagga", self.output[0])
        self.assertIsNotNone(self.fake.events.stop.callback)
        self.assertIsNone(self.fake.events.new_inferior.callback)
        self.module.finalize()
        self.assertEqual(len(self.inferiors), 3)
        self.assertIn("kernel symbols are inferior 3", self.output[-1])

    def test_loadable_fallback_kind_is_part_of_loaded_symbol_identity(self):
        self.module.install(
            [
                {
                    "guest_path": "/usr/sbin/quagga",
                    "debug_elf": "/image/debug/quagga",
                    "build_id": "a5" * 32,
                    "identity_kind": "loadable-content-sha256",
                }
            ],
            "/image/kernel/vmlinux",
        )
        self.assertIn(
            "loadable-content-sha256 " + "a5" * 32,
            self.output[0],
        )


if __name__ == "__main__":
    unittest.main()
