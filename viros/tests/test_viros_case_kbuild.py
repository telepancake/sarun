from __future__ import annotations

import hashlib
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import textwrap
import unittest


PROJECT = Path(__file__).resolve().parents[1]


def write_executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(body).lstrip(), encoding="utf-8")
    path.chmod(0o755)


class CaseKbuildTests(unittest.TestCase):
    def run_shell(self, work: Path, script: str, **extra_env: str):
        environment = os.environ.copy()
        environment.update(
            {
                "VIROS_WORKDIR": str(work),
                "VIROS_SOURCE_ONLY": "1",
                "VIROS_CASE_KBUILD_MIN_KIB": "0",
                **extra_env,
            }
        )
        return subprocess.run(
            ["bash", "-c", f'source "$1"\n{script}', "case-test", str(PROJECT / "viros.sh")],
            cwd=work,
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )

    def make_fake_bwrap(self, work: Path) -> Path:
        fake = work / "fake-bin" / "bwrap"
        write_executable(
            fake,
            r"""
            #!/usr/bin/env python3
            import os
            from pathlib import Path
            import shutil
            import subprocess
            import sys

            args = sys.argv[1:]
            Path(os.environ["BWRAP_LOG"]).write_text("\n".join(args))
            arity = {
                "--die-with-parent": 0,
                "--ro-bind": 2,
                "--dev-bind": 2,
                "--proc": 1,
                "--tmpfs": 1,
                "--dir": 1,
                "--bind": 2,
                "--setenv": 2,
            }
            env = os.environ.copy()
            tmpfs = None
            directories = []
            index = 0
            while index < len(args) and args[index].startswith("--"):
                option = args[index]
                count = arity[option]
                values = args[index + 1:index + 1 + count]
                if option == "--setenv":
                    env[values[0]] = values[1]
                elif option == "--tmpfs":
                    tmpfs = Path(values[0])
                elif option == "--dir":
                    directories.append(Path(values[0]))
                index += count + 1
            for directory in directories:
                directory.mkdir(parents=True, exist_ok=True)
            result = subprocess.run(args[index:], env=env, check=False)
            # A real mount discards tmpfs contents before bwrap exits.
            if tmpfs is not None and tmpfs.is_dir():
                for child in tmpfs.iterdir():
                    if child.is_dir() and not child.is_symlink():
                        shutil.rmtree(child)
                    else:
                        child.unlink()
            raise SystemExit(result.returncode)
            """,
        )
        return fake

    def test_bounded_workspace_uses_project_path_and_discards_ephemeral_files(self):
        with tempfile.TemporaryDirectory(prefix="case-kbuild-", dir=PROJECT) as raw:
            work = Path(raw)
            fake = self.make_fake_bwrap(work)
            log = work / "bwrap.args"
            retained = work / "build/kernel-arm"
            retained.mkdir(parents=True)
            result = self.run_shell(
                work,
                r"""
                mkdir -p "$WORKDIR/export"
                run_case_sensitive_workspace fixture "$WORKDIR/export" \
                    /bin/sh -c '
                        test "$TMPDIR" = "$VIROS_WORKDIR/tmp"
                        test "$TMP" = "$VIROS_WORKDIR/tmp"
                        test "$TEMP" = "$VIROS_WORKDIR/tmp"
                        printf retained > "$VIROS_KBUILD_EXPORT/result"
                        printf transient > "$VIROS_WORKDIR/transient"
                        printf temporary > "$TMPDIR/compiler-temporary"
                    '
                test "$(cat "$WORKDIR/export/result")" = retained
                test ! -e "$BUILD/.case-kbuild-fixture"
                """,
                PATH=f"{fake.parent}:{os.environ['PATH']}",
                BWRAP_LOG=str(log),
                VIROS_KBUILD_RETAINED_BIND=str(retained),
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            arguments = log.read_text(encoding="utf-8")
            self.assertIn("--die-with-parent", arguments)
            self.assertIn("--tmpfs", arguments)
            self.assertIn(str(work / "build" / ".case-kbuild-fixture"), arguments)
            self.assertIn(str(retained), arguments)
            self.assertIn("VIROS_KBUILD_RETAINED", arguments)
            self.assertIn("TMPDIR", arguments)
            self.assertIn(str(work / "build/.case-kbuild-fixture/tmp"), arguments)
            self.assertNotIn("/tmp", arguments.splitlines())

    def test_case_insensitive_build_routes_before_source_or_toolchain_setup(self):
        with tempfile.TemporaryDirectory(prefix="case-route-", dir=PROJECT) as raw:
            work = Path(raw)
            result = self.run_shell(
                work,
                r"""
                workdir_is_case_sensitive() { return 1; }
                build_debug_kernel_casefold() { printf 'routed:%s\n' "$1"; }
                build_debug_kernel arm
                """,
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertIn("routed:arm", result.stdout)

    def test_doctor_accepts_usable_case_sensitive_fallback(self):
        with tempfile.TemporaryDirectory(prefix="case-doctor-", dir=PROJECT) as raw:
            work = Path(raw)
            fake = self.make_fake_bwrap(work)
            result = self.run_shell(
                work,
                r"""
                workdir_is_case_sensitive() { return 1; }
                case_kbuild_available_kib() { printf '8388608\n'; }
                doctor_stage || true
                """,
                PATH=f"{fake.parent}:{os.environ['PATH']}",
                BWRAP_LOG=str(work / "bwrap.args"),
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertIn("ready    bwrap case-sensitive Kbuild tmpfs", result.stdout)
            self.assertNotIn("missing  case-sensitive VIROS_WORKDIR", result.stdout)

    def test_help_describes_automatic_project_local_fallback(self):
        with tempfile.TemporaryDirectory(prefix="case-help-", dir=PROJECT) as raw:
            work = Path(raw)
            result = subprocess.run(
                [str(PROJECT / "viros.sh"), "help"],
                cwd=work,
                env={**os.environ, "VIROS_WORKDIR": str(work)},
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertIn("short-lived bwrap tmpfs below build/", result.stdout)

    def test_tmpfs_worker_extracts_only_kernel_patch_and_configs(self):
        with tempfile.TemporaryDirectory(prefix="case-gpl-", dir=PROJECT) as raw:
            work = Path(raw)
            archive = work / "downloads" / (
                "mikrotik-gpl-c3e110db1d35886c96ee14e16fc5a06bcac59692.tar.gz"
            )
            fixture = work / "fixture"
            root = fixture / (
                "mikrotik-gpl-c3e110db1d35886c96ee14e16fc5a06bcac59692"
                "/2025-03-19"
            )
            (root / "configs").mkdir(parents=True)
            (root / "linux-5.6.3.patch").write_text("patch\n", encoding="utf-8")
            (root / "configs" / "arm.config").write_text("config\n", encoding="utf-8")
            (root / "unrelated-large-tree").mkdir()
            (root / "unrelated-large-tree" / "ignored").write_bytes(b"x" * 4096)
            archive.parent.mkdir(parents=True)
            archive_root = next(fixture.iterdir())
            with tarfile.open(archive, "w:gz") as stream:
                stream.add(archive_root, arcname=archive_root.name)

            result = self.run_shell(
                work,
                r"""
                export VIROS_KBUILD_TMPFS_ACTIVE=1
                prepare_mikrotik_source
                test -s "$SOURCES/mikrotik-gpl/2025-03-19/linux-5.6.3.patch"
                test -s "$SOURCES/mikrotik-gpl/2025-03-19/configs/arm.config"
                test ! -e "$SOURCES/mikrotik-gpl/2025-03-19/unrelated-large-tree"
                """,
            )
            self.assertEqual(result.returncode, 0, result.stdout)

    def test_retained_fixture_contains_exact_output_and_identity_record(self):
        with tempfile.TemporaryDirectory(prefix="case-retain-", dir=PROJECT) as raw:
            work = Path(raw)
            (work / "downloads").mkdir()
            (work / "downloads" / "linux-5.6.3.tar.xz").write_bytes(b"linux")
            patch = work / "sources/mikrotik-gpl/2025-03-19/linux-5.6.3.patch"
            patch.parent.mkdir(parents=True)
            patch.write_bytes(b"patch")
            out = work / "build/kernel-arm"
            out.mkdir(parents=True)
            (out / ".config").write_bytes(b"config")
            (out / "vmlinux").write_bytes(b"vmlinux")
            source_gdb = work / "source/scripts/gdb"
            (source_gdb / "linux").mkdir(parents=True)
            (source_gdb / "vmlinux-gdb.py").write_bytes(b"helper")
            (source_gdb / "linux/tasks.py").write_bytes(b"tasks")
            (out / "vmlinux-gdb.py").symlink_to(source_gdb / "vmlinux-gdb.py")
            (out / "scripts/gdb/linux").mkdir(parents=True)
            (out / "scripts/gdb/linux/tasks.py").symlink_to(
                source_gdb / "linux/tasks.py"
            )
            artifact = work / "artifacts/arm"
            artifact.mkdir(parents=True)
            (artifact / "vmlinux.debug").write_bytes(b"debug")
            export = work / "export"
            export.mkdir()

            result = self.run_shell(
                work,
                r"""
                export VIROS_KBUILD_TMPFS_ACTIVE=1
                export VIROS_KBUILD_EXPORT="$WORKDIR/export"
                retain_case_kbuild_workspace arm
                test -s "$WORKDIR/export/kernel-arm/.viros-case-kbuild"
                test -s "$WORKDIR/export/kernel-arm/vmlinux-gdb.py"
                test -s "$WORKDIR/export/artifacts/arm/vmlinux.debug"
                """,
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            identity = (export / "kernel-arm/.viros-case-kbuild").read_text()
            self.assertIn("format viros-case-kbuild-v2", identity)
            self.assertIn("target arm", identity)
            retained_helper = export / "kernel-arm/vmlinux-gdb.py"
            retained_task = export / "kernel-arm/scripts/gdb/linux/tasks.py"
            self.assertTrue(retained_helper.is_file())
            self.assertFalse(retained_helper.is_symlink())
            self.assertTrue(retained_task.is_file())
            self.assertFalse(retained_task.is_symlink())

    def test_retention_rejects_casefold_filename_collisions(self):
        with tempfile.TemporaryDirectory(prefix="case-collision-", dir=PROJECT) as raw:
            work = Path(raw)
            tree = work / "tree"
            tree.mkdir()
            fake_find = work / "fake-bin/find"
            write_executable(
                fake_find,
                """
                #!/bin/sh
                printf '%s\\n' Generated.h generated.h
                """,
            )
            result = self.run_shell(
                work,
                r"""
                assert_casefold_portable_tree "$WORKDIR/tree"
                """,
                PATH=f"{fake_find.parent}:{os.environ['PATH']}",
            )
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("collide on a case-insensitive filesystem", result.stdout)

    def test_retention_prunes_and_records_only_casefold_colliding_files(self):
        with tempfile.TemporaryDirectory(prefix="case-prune-", dir=PROJECT) as raw:
            work = Path(raw)
            tree = work / "tree/net/netfilter"
            tree.mkdir(parents=True)
            (tree / ".xt_tcpmss.mod.cmd").write_bytes(b"lower")
            (tree / ".xt_TCPMSS.mod.cmd").write_bytes(b"upper")
            (tree / "unrelated.o").write_bytes(b"keep")
            if len(tuple(tree.iterdir())) != 3:
                self.skipTest("fixture needs a case-sensitive directory")

            result = self.run_shell(
                work,
                r"""
                prune_casefold_colliding_intermediates "$WORKDIR/tree"
                assert_casefold_portable_tree "$WORKDIR/tree"
                test -s "$WORKDIR/tree/.viros-casefold-pruned"
                test -s "$WORKDIR/tree/net/netfilter/unrelated.o"
                test ! -e "$WORKDIR/tree/net/netfilter/.xt_tcpmss.mod.cmd"
                test ! -e "$WORKDIR/tree/net/netfilter/.xt_TCPMSS.mod.cmd"
                """,
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            listing = (work / "tree/.viros-casefold-pruned").read_text()
            self.assertIn(".xt_tcpmss.mod.cmd", listing)
            self.assertIn(".xt_TCPMSS.mod.cmd", listing)

    def test_rehydrated_worker_reconnects_source_and_runs_foreground_command(self):
        with tempfile.TemporaryDirectory(prefix="case-rehydrate-", dir=PROJECT) as raw:
            work = Path(raw)
            linux = work / "downloads/linux-5.6.3.tar.xz"
            linux.parent.mkdir(parents=True)
            linux.write_bytes(b"published-linux")
            retained = work / "retained"
            retained.mkdir()
            (retained / ".config").write_bytes(b"config")
            (retained / "vmlinux").write_bytes(b"vmlinux")
            (retained / "vmlinux-gdb.py").write_bytes(b"helper")
            (retained / "source").symlink_to("/expired/source")
            (retained / ".viros-original-output").symlink_to("/expired/output")
            (retained / ".fixture.cmd").write_text(
                "source := /expired/source/include/linux/kernel.h\n"
                "output := /expired/output/include/generated/autoconf.h\n",
                encoding="utf-8",
            )
            identity = {
                "linux_archive_sha256": hashlib.sha256(b"published-linux").hexdigest(),
                "linux_update_sha256": hashlib.sha256(b"published-update").hexdigest(),
                "config_sha256": hashlib.sha256(b"config").hexdigest(),
                "vmlinux_sha256": hashlib.sha256(b"vmlinux").hexdigest(),
            }
            (retained / ".viros-case-kbuild").write_text(
                "\n".join(
                    [
                        "format viros-case-kbuild-v2",
                        "target arm",
                        "linux_version 5.6.3",
                        "gpl_commit c3e110db1d35886c96ee14e16fc5a06bcac59692",
                        *(f"{key} {value}" for key, value in identity.items()),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            export = work / "module"
            export.mkdir()
            result = self.run_shell(
                work,
                r"""
                prepare_mikrotik_kernel_source() {
                    local src patchfile
                    src=$(mikrotik_kernel_source)
                    patchfile="$SOURCES/mikrotik-gpl/2025-03-19/linux-5.6.3.patch"
                    mkdir -p "$src/scripts" "$(dirname -- "$patchfile")"
                    printf source > "$src/Makefile"
                    printf published-update > "$patchfile"
                    printf '%s\n' '#!/bin/sh' 'printf "include %s/Makefile\\n" "$1" > Makefile' > "$src/scripts/mkmakefile"
                    chmod +x "$src/scripts/mkmakefile"
                }
                setup_kernel_build_context() {
                    export ARCH=arm CROSS_COMPILE=fixture-
                }
                export VIROS_KBUILD_TMPFS_ACTIVE=1
                export VIROS_KBUILD_RETAINED="$WORKDIR/retained"
                export VIROS_KBUILD_EXPORT="$WORKDIR/module"
                rehydrate_case_kbuild_worker arm /bin/sh -c '
                    test -e "$VIROS_KERNEL_OUTPUT/source/Makefile"
                    test -f "$VIROS_KERNEL_OUTPUT/vmlinux-gdb.py"
                    grep -F "$VIROS_KERNEL_SOURCE/Makefile" "$VIROS_KERNEL_OUTPUT/Makefile"
                    grep -F "$VIROS_KERNEL_SOURCE/include" "$VIROS_KERNEL_OUTPUT/.fixture.cmd"
                    grep -F "$VIROS_KERNEL_OUTPUT/include" "$VIROS_KERNEL_OUTPUT/.fixture.cmd"
                    ! grep -F /expired/source "$VIROS_KERNEL_OUTPUT/.fixture.cmd"
                    ! grep -F /expired/output "$VIROS_KERNEL_OUTPUT/.fixture.cmd"
                    test "$ARCH" = arm
                    test "$CROSS_COMPILE" = fixture-
                    printf ready > result
                '
                """,
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertEqual((export / "result").read_text(), "ready")


if __name__ == "__main__":
    unittest.main()
