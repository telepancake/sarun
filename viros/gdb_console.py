"""Interactive QEMU serial-console switching for viros GDB sessions."""

import os
import select
import socket
import termios
import time
import tty

import gdb


class VirosConsole(gdb.Command):
    """Resume the guest on its serial console; Ctrl-] returns to GDB."""

    def __init__(self):
        super().__init__("viros-console", gdb.COMMAND_RUNNING)

    def invoke(self, argument, from_tty):
        if argument.strip():
            raise gdb.GdbError("viros-console takes no arguments")

        path = os.environ.get("VIROS_CONSOLE_SOCKET")
        if not path:
            raise gdb.GdbError("this GDB session has no viros console socket")
        if not os.isatty(0) or not os.isatty(1):
            raise gdb.GdbError("viros-console requires an interactive terminal")

        console = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        deadline = time.monotonic() + 5
        while True:
            try:
                console.connect(path)
                break
            except (FileNotFoundError, ConnectionRefusedError) as exc:
                if time.monotonic() >= deadline:
                    console.close()
                    raise gdb.GdbError(
                        f"could not connect to QEMU console {path}: {exc}"
                    ) from exc
                time.sleep(0.05)

        stdin_fd = 0
        stdout_fd = 1
        saved_terminal = termios.tcgetattr(stdin_fd)
        target_started = False

        gdb.write("Entering VM serial console; press Ctrl-] to break into GDB.\n")
        gdb.flush(gdb.STDOUT)
        try:
            gdb.execute("continue&", from_tty=False, to_string=True)
            target_started = True
            tty.setraw(stdin_fd)

            while True:
                readable, _, _ = select.select((stdin_fd, console), (), ())
                if stdin_fd in readable:
                    data = os.read(stdin_fd, 4096)
                    if not data:
                        break
                    escape = data.find(b"\x1d")
                    if escape >= 0:
                        if escape:
                            console.sendall(data[:escape])
                        break
                    console.sendall(data)
                if console in readable:
                    data = console.recv(4096)
                    if not data:
                        break
                    os.write(stdout_fd, data)
        finally:
            termios.tcsetattr(stdin_fd, termios.TCSADRAIN, saved_terminal)
            console.close()
            gdb.write("\nReturning to GDB; stopping the VM...\n")
            gdb.flush(gdb.STDOUT)
            if target_started:
                try:
                    gdb.execute("interrupt", from_tty=False, to_string=True)
                except gdb.error as exc:
                    # A guest breakpoint or shutdown may already have stopped it.
                    gdb.write(f"VM was already stopped: {exc}\n")


VirosConsole()
