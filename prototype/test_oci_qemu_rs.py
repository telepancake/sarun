#!/usr/bin/env python3
"""End-to-end OCI execution through Sarun's portable Linux runner.

On arm64 macOS this exercises both QEMU backends: HVF for the arm64 fixture and
TCG for the amd64 fixture.  On an arm64 Linux host the arm64 fixture exercises
the native runner.  The caller may override the two static Linux probes through
SARUN_OCI_PROBE and SARUN_OCI_X86_64_PROBE.
"""

import gzip
import hashlib
import io
import json
import os
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

from sarun_test_paths import ENGINE_BIN, HOST_ARCH, HOST_SYSTEM
from test_pty_ui_rs import PtyClient


FAILURES = []


def check(condition, message):
    print(("  ok  " if condition else " FAIL ") + message)
    if not condition:
        FAILURES.append(message)


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def write_blob(layout, data):
    value = digest(data)
    path = layout / "blobs" / "sha256" / value.split(":", 1)[1]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return value, len(data)


def layer(probe):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as archive:
        def directory(name, mode=0o755):
            item = tarfile.TarInfo(name)
            item.type = tarfile.DIRTYPE
            item.mode = mode
            item.mtime = 1_700_000_000
            archive.addfile(item)

        def regular(name, data, mode=0o644):
            item = tarfile.TarInfo(name)
            item.size = len(data)
            item.mode = mode
            item.mtime = 1_700_000_000
            archive.addfile(item, io.BytesIO(data))

        for name in ("bin", "dev", "etc", "proc", "sys", "work"):
            directory(name)
        directory("tmp", 0o1777)
        regular("bin/probe", probe.read_bytes(), 0o755)
        regular("etc/marker", b"oci-rootfs-marker\n")

    uncompressed = raw.getvalue()
    compressed = io.BytesIO()
    with gzip.GzipFile(fileobj=compressed, mode="wb", mtime=0) as stream:
        stream.write(uncompressed)
    return uncompressed, compressed.getvalue()


def build_layout(layout, probe, architecture):
    layout.mkdir(parents=True)
    (layout / "oci-layout").write_text(
        json.dumps({"imageLayoutVersion": "1.0.0"}), encoding="utf-8"
    )
    uncompressed, compressed = layer(probe)
    layer_digest, layer_size = write_blob(layout, compressed)
    config = {
        "architecture": architecture,
        "os": "linux",
        "config": {
            "Env": ["PATH=/bin", "OCI_FIXTURE_MARK=image-env"],
            "WorkingDir": "/work",
            "Cmd": ["/bin/probe", "env", "OCI_FIXTURE_MARK"],
            "User": "1234:1235",
        },
        "rootfs": {"type": "layers", "diff_ids": [digest(uncompressed)]},
    }
    config_bytes = json.dumps(config, separators=(",", ":")).encode()
    config_digest, config_size = write_blob(layout, config_bytes)
    manifest = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_size,
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_size,
            }
        ],
    }
    manifest_bytes = json.dumps(manifest, separators=(",", ":")).encode()
    manifest_digest, manifest_size = write_blob(layout, manifest_bytes)
    index = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest_size,
                "platform": {"architecture": architecture, "os": "linux"},
            }
        ],
    }
    (layout / "index.json").write_text(json.dumps(index), encoding="utf-8")


def wait_socket(path, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                stream.settimeout(0.5)
                stream.connect(str(path))
                return True
        except OSError:
            time.sleep(0.1)
    return False


def invoke(env, *args, timeout=180):
    result = subprocess.run(
        [str(ENGINE_BIN), *args],
        env=env,
        text=True,
        capture_output=True,
        timeout=timeout,
    )
    print(f"$ sarun {' '.join(args)}")
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    return result


def main():
    if HOST_ARCH != "aarch64":
        raise SystemExit(
            f"test_oci_qemu_rs currently needs an arm64 host/fixture (got {HOST_ARCH})"
        )
    if not ENGINE_BIN.is_file():
        raise SystemExit(f"missing release engine: {ENGINE_BIN}")
    arm64_probe = Path(
        os.environ.get(
            "SARUN_OCI_PROBE", "/private/tmp/sarun-appliance-probe-aarch64"
        )
    )
    if not arm64_probe.is_file():
        raise SystemExit(
            f"missing static arm64 Linux probe: {arm64_probe}; cross-compile "
            "prototype/appliance_probe.c first"
        )
    x86_64_probe = Path(
        os.environ.get(
            "SARUN_OCI_X86_64_PROBE",
            "/private/tmp/sarun-appliance-probe-x86_64",
        )
    )
    if HOST_SYSTEM == "darwin" and not x86_64_probe.is_file():
        raise SystemExit(
            f"missing static x86_64 Linux probe: {x86_64_probe}; cross-compile "
            "prototype/appliance_probe.c first"
        )

    # Darwin's sockaddr_un path limit is only 104 bytes.  Keep every XDG path
    # deliberately short so the daemon's ui/control sockets fit.
    temporary = Path(tempfile.mkdtemp(prefix="sq-", dir="/private/tmp"))
    env = os.environ.copy()
    for key, directory in (
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CONFIG_HOME", "config"),
    ):
        env[key] = str(temporary / directory)
        (temporary / directory).mkdir()
    env["SLOPBOX_NS"] = "oci-qemu"
    appliance_root = Path.home() / ".cache" / "sarun" / "appliances" / "v1"
    if appliance_root.is_dir():
        env["SARUN_APPLIANCE_ROOT"] = str(appliance_root)

    layout = temporary / "layout-arm64"
    build_layout(layout, arm64_probe, "arm64")
    x86_64_layout = temporary / "layout-amd64"
    if HOST_SYSTEM == "darwin":
        build_layout(x86_64_layout, x86_64_probe, "amd64")
    build_context = temporary / "build"
    build_context.mkdir()
    (build_context / "Dockerfile").write_text(
        'FROM OCIARM\n'
        'USER 0:0\n'
        'RUN ["/bin/probe","write","/built","hello-from-dockerfile-run"]\n'
        'CMD ["/bin/probe","read","/built"]\n',
        encoding="utf-8",
    )

    namespace = f"slopbox.{env['SLOPBOX_NS']}"
    sock = Path(env["XDG_RUNTIME_DIR"]) / namespace / "ui.sock"
    daemon_log = temporary / "daemon.log"
    daemon_stream = daemon_log.open("wb")
    daemon = subprocess.Popen(
        [str(ENGINE_BIN), "serve"],
        env=env,
        stdout=daemon_stream,
        stderr=subprocess.STDOUT,
    )
    try:
        if not wait_socket(sock):
            raise RuntimeError(
                "engine socket did not appear:\n"
                + daemon_log.read_text(encoding="utf-8", errors="replace")
            )

        loaded = invoke(env, "oci", "load", f"oci-layout:{layout}", "OCIARM")
        check(loaded.returncode == 0, "synthetic arm64 OCI image loads")

        default = invoke(env, "oci", "run", "--net", "off", "--name", "DEFAULT", "OCIARM")
        check(default.returncode == 0, "OCI default command exits successfully")
        check(
            "OCI_FIXTURE_MARK=image-env" in default.stdout,
            "OCI image environment reaches the guest",
        )

        cwd = invoke(
            env,
            "oci",
            "run",
            "--net",
            "off",
            "--name",
            "CWD",
            "OCIARM",
            "--",
            "/bin/probe",
            "cwd",
        )
        check(cwd.returncode == 0 and "/work" in cwd.stdout, "OCI WorkingDir is applied")

        read = invoke(
            env,
            "oci",
            "run",
            "--net",
            "off",
            "--name",
            "READ",
            "OCIARM",
            "--",
            "/bin/probe",
            "read",
            "/etc/marker",
        )
        check(
            read.returncode == 0 and "oci-rootfs-marker" in read.stdout,
            "explicit OCI command reads the image rootfs",
        )

        identity = invoke(
            env,
            "oci",
            "run",
            "--net",
            "off",
            "--name",
            "IDENTITY",
            "OCIARM",
            "--",
            "/bin/probe",
            "identity",
        )
        check(
            identity.returncode == 0 and "UID=1234 GID=1235" in identity.stdout,
            "OCI numeric User reaches the guest",
        )

        # This is the UI's actual transport boundary: App::open_pty submits
        # this semantic argv through pty_spawn, then renders FRAME_PTY_DATA.
        ui = PtyClient(
            str(sock),
            [
                str(ENGINE_BIN),
                "oci",
                "run",
                "--net",
                "off",
                "--name",
                "UI",
                "OCIARM",
                "--",
                "/bin/probe",
                "tty",
            ],
            environment=env,
            cwd=str(temporary),
        )
        ui_output, ui_eof = ui.drain(timeout=30)
        ui.close()
        print("UI PTY output:", ui_output.decode(errors="replace"))
        check('"ok":true' in ui.ack, "UI PTY accepts the OCI launch")
        check(ui_eof, "UI PTY receives end-of-process")
        check(
            b"TTY 1 1 1" in ui_output,
            "OCI guest receives a real terminal when launched from the UI",
        )

        if HOST_SYSTEM == "darwin":
            x86_loaded = invoke(
                env, "oci", "load", f"oci-layout:{x86_64_layout}", "OCIX86"
            )
            check(x86_loaded.returncode == 0, "synthetic amd64 OCI image loads")

            x86_read = invoke(
                env,
                "oci",
                "run",
                "--net",
                "off",
                "--name",
                "X86READ",
                "OCIX86",
                "--",
                "/bin/probe",
                "read",
                "/etc/marker",
            )
            check(
                x86_read.returncode == 0
                and "oci-rootfs-marker" in x86_read.stdout,
                "foreign amd64 OCI command runs under emulation",
            )
            check(
                "accelerator tcg" in x86_read.stderr,
                "foreign amd64 OCI execution selects TCG internally",
            )

            x86_ui = PtyClient(
                str(sock),
                [
                    str(ENGINE_BIN),
                    "oci",
                    "run",
                    "--net",
                    "off",
                    "--name",
                    "UIX86",
                    "OCIX86",
                    "--",
                    "/bin/probe",
                    "tty",
                ],
                environment=env,
                cwd=str(temporary),
            )
            x86_ui_output, x86_ui_eof = x86_ui.drain(timeout=90)
            x86_ui.close()
            print("amd64 UI PTY output:", x86_ui_output.decode(errors="replace"))
            check('"ok":true' in x86_ui.ack, "UI PTY accepts the amd64 OCI launch")
            check(x86_ui_eof, "amd64 UI PTY receives end-of-process")
            check(
                b"TTY 1 1 1" in x86_ui_output,
                "foreign amd64 OCI guest receives a real terminal from the UI",
            )

        built = invoke(
            env,
            "oci",
            "build",
            "--net",
            "off",
            "-t",
            "OCIBUILT",
            str(build_context),
        )
        check(built.returncode == 0, "Dockerfile RUN executes through the Linux runner")

        built_run = invoke(
            env, "oci", "run", "--net", "off", "--name", "BUILTRUN", "OCIBUILT"
        )
        check(built_run.returncode == 0, "built OCI image's default command exits successfully")
        check(
            "hello-from-dockerfile-run" in built_run.stdout,
            "Dockerfile RUN filesystem write persists into the built image",
        )
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=5)
        daemon_stream.close()
        if FAILURES:
            print(
                "\nengine log:\n"
                + daemon_log.read_text(encoding="utf-8", errors="replace")
            )
        if os.environ.get("SARUN_KEEP_TEST_STATE") != "1":
            shutil.rmtree(temporary)

    if FAILURES:
        raise SystemExit(f"{len(FAILURES)} OCI/QEMU checks failed")
    print(f"PASS: OCI execution on {HOST_SYSTEM}/{HOST_ARCH}")


if __name__ == "__main__":
    main()
