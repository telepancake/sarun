"""GDB-resident exact symbol association for managed Sarun inferiors."""

from __future__ import annotations

import ast
import gdb


def _quote(value):
    return '"' + str(value).replace("\\", "\\\\").replace('"', '\\"') + '"'


class Catalog:
    def __init__(self, rows, kernel):
        self.by_guest = {row["guest_path"]: row for row in rows}
        self.kernel = kernel
        self.loaded = {}
        self.refreshing = False
        self.kernel_inferior = None

    @staticmethod
    def _remote_executable(inferior):
        if inferior.pid <= 0:
            return None
        reply = gdb.execute(
            f"maintenance packet qXfer:exec-file:read:{inferior.pid:x}:0,1000",
            from_tty=False,
            to_string=True,
        )
        for line in reply.splitlines():
            if not line.startswith("received: "):
                continue
            try:
                payload = ast.literal_eval(line.removeprefix("received: "))
            except (SyntaxError, ValueError):
                return None
            if isinstance(payload, str) and payload.startswith("l/"):
                return payload[1:]
        return None

    def refresh(self, _event=None):
        if self.refreshing:
            return
        self.refreshing = True
        original = gdb.selected_inferior()
        try:
            for inferior in gdb.inferiors():
                try:
                    gdb.execute(f"inferior {inferior.num}", to_string=True)
                    guest = self._remote_executable(inferior)
                    row = self.by_guest.get(guest)
                    if row is None:
                        continue
                    identity_kind = row.get("identity_kind", "gnu-build-id")
                    identity = (
                        guest,
                        row["debug_elf"],
                        identity_kind,
                        row["build_id"],
                    )
                    if self.loaded.get(inferior.num) == identity:
                        continue
                    self.loaded[inferior.num] = identity
                    gdb.execute(
                        f"file {_quote(row['debug_elf'])}",
                        from_tty=False,
                        to_string=True,
                    )
                    # The stopped process's address space is also the useful
                    # view of kernel memory. Keep kernel symbols beside its
                    # relocated main executable.
                    gdb.execute(
                        f"add-symbol-file {_quote(self.kernel)}",
                        from_tty=False,
                        to_string=True,
                    )
                    gdb.write(
                        f"sarun: inferior {inferior.num} {guest} -> "
                        f"{row['debug_elf']} ({identity_kind} {row['build_id']})\n"
                    )
                except gdb.error:
                    self.loaded.pop(inferior.num, None)
                    continue
        finally:
            if original.is_valid():
                try:
                    gdb.execute(f"inferior {original.num}", to_string=True)
                except gdb.error:
                    pass
            self.refreshing = False

    def add_kernel_inferior(self):
        if self.kernel_inferior is not None:
            return
        original = gdb.selected_inferior()
        before = {inferior.num for inferior in gdb.inferiors()}
        gdb.execute(
            f"add-inferior -exec {_quote(self.kernel)}",
            from_tty=False,
            to_string=True,
        )
        added = [inferior for inferior in gdb.inferiors() if inferior.num not in before]
        if len(added) != 1:
            raise gdb.error("Sarun could not create the kernel symbol inferior")
        self.kernel_inferior = added[0].num
        gdb.write(f"sarun: kernel symbols are inferior {self.kernel_inferior}\n")
        if original.is_valid():
            gdb.execute(f"inferior {original.num}", to_string=True)


_catalog = None


def install(rows, kernel):
    global _catalog
    _catalog = Catalog(rows, kernel)
    # GDB emits new_inferior while its remote target is still constructing the
    # new program space; switching inferiors from that callback is invalid.
    # Every usable process is covered by the subsequent stop event, and the
    # initial stopped set is handled by finalize() after `target remote`.
    gdb.events.stop.connect(_catalog.refresh)
    if hasattr(gdb.events, "new_objfile"):
        gdb.events.new_objfile.connect(_catalog.refresh)
    _catalog.refresh()


def finalize():
    if _catalog is None:
        raise gdb.error("Sarun GDB catalog is not installed")
    _catalog.refresh()
    _catalog.add_kernel_inferior()
