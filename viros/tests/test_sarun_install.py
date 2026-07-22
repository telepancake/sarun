from __future__ import annotations

from pathlib import Path
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[1]


class SarunInstallTests(unittest.TestCase):
    def test_public_command_selects_only_the_named_provider_box(self):
        command = f"""
            export VIROS_SOURCE_ONLY=1
            source {str(ROOT / 'viros.sh')!r}
            sarun() {{ printf '<%s>\\n' "$@"; }}
            sarun_install_stage
        """
        result = subprocess.run(
            ["bash", "-c", command],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(
            result.stdout.splitlines()[:5],
            ["<run>", "<--net>", "<off>", "<VIROS-DEBUG>", "<-->"],
        )
        self.assertEqual(result.stdout.splitlines()[-1], "<_sarun-install>")

    def test_provider_resources_and_loader_search_are_box_internal(self):
        script = (ROOT / "viros.sh").read_text()
        self.assertIn("local destination=/opt/viros", script)
        self.assertIn("sarun service declare viros-debug", script)
        self.assertIn("$ORIGIN/../../python/managed/lib", script)
        self.assertNotIn("VIROS_PROVIDER", script)
        client = (ROOT / "inferiors/sarun_gdb_client.py").read_text()
        self.assertNotIn("LD_LIBRARY_PATH", client)
        self.assertIn('environment["PYTHONHOME"]', client)
        self.assertIn("python import json; import gdb", script)


if __name__ == "__main__":
    unittest.main()
