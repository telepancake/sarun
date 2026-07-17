"""Linux userspace debugging through QEMU's system GDB stub.

QEMU exposes virtual CPUs rather than guest processes.  These commands use
the matching Linux kernel debug information to wait for a task to be current,
read its saved ELF auxiliary vector, and relocate local userspace symbols.
Nothing has to run inside the guest.
"""

import os
import struct

import gdb


AT_NULL = 0
AT_PHDR = 3
AT_PHENT = 4
AT_PHNUM = 5
AT_PAGESZ = 6
AT_BASE = 7
AT_ENTRY = 9
AT_RANDOM = 25

ET_EXEC = 2
ET_DYN = 3
PT_LOAD = 1
PT_PHDR = 6
SHT_SYMTAB = 2
SHT_DYNSYM = 11


def target_byte_order():
    filename = gdb.current_progspace().filename
    if filename:
        try:
            with open(filename, "rb") as elf:
                ident = elf.read(16)
            if ident[:4] == b"\x7fELF" and ident[5] in (1, 2):
                return "<" if ident[5] == 1 else ">"
        except OSError:
            pass
    # Every target used by this harness is little-endian except MIPSBE and
    # PowerPC.  GDB's architecture name is enough as a last resort.
    architecture = gdb.selected_frame().architecture().name().lower()
    return ">" if architecture in ("mips", "powerpc:common") else "<"


def pointer_size():
    return gdb.lookup_type("void").pointer().sizeof


def read_integer(address, size=None):
    size = size or pointer_size()
    formats = {4: "I", 8: "Q"}
    try:
        data = gdb.selected_inferior().read_memory(address, size).tobytes()
        return struct.unpack(target_byte_order() + formats[size], data)[0]
    except (gdb.error, MemoryError, KeyError, struct.error) as exc:
        raise gdb.MemoryError("cannot read {:#x}".format(address)) from exc


def elf_symbol_size(path, wanted):
    """Return an ELF symbol's st_size without relying on complete DWARF."""
    path = os.path.abspath(os.path.expanduser(path))
    with open(path, "rb") as elf:
        ident = elf.read(16)
        if len(ident) != 16 or ident[:4] != b"\x7fELF":
            return None
        elf64 = ident[4] == 2
        endian = "<" if ident[5] == 1 else ">"
        header_format = endian + (
            "HHIQQQIHHHHHH" if elf64 else "HHIIIIIHHHHHH"
        )
        header = struct.unpack(
            header_format, elf.read(struct.calcsize(header_format))
        )
        shoff, shentsize, shnum = header[5], header[10], header[11]
        section_format = endian + (
            "IIQQQQIIQQ" if elf64 else "IIIIIIIIII"
        )
        section_size = struct.calcsize(section_format)
        sections = []
        for index in range(shnum):
            elf.seek(shoff + index * shentsize)
            sections.append(struct.unpack(section_format, elf.read(section_size)))
        symbol_format = endian + ("IBBHQQ" if elf64 else "IIIBBH")
        symbol_size = struct.calcsize(symbol_format)
        for section in sections:
            section_type, offset, size, link, entry_size = (
                section[1], section[4], section[5], section[6], section[9]
            )
            if section_type not in (SHT_SYMTAB, SHT_DYNSYM):
                continue
            strings = sections[link]
            elf.seek(strings[4])
            string_data = elf.read(strings[5])
            for symbol_offset in range(offset, offset + size, entry_size):
                elf.seek(symbol_offset)
                symbol = struct.unpack(symbol_format, elf.read(symbol_size))
                name_offset = symbol[0]
                end = string_data.find(b"\0", name_offset)
                name = string_data[name_offset:end].decode(errors="replace")
                if name == wanted:
                    return symbol[5] if elf64 else symbol[2]
    return None


def elf_has_section_table(path):
    try:
        with open(path, "rb") as elf:
            ident = elf.read(16)
            if len(ident) != 16 or ident[:4] != b"\x7fELF":
                return False
            elf64 = ident[4] == 2
            endian = "<" if ident[5] == 1 else ">"
            header_format = endian + (
                "HHIQQQIHHHHHH" if elf64 else "HHIIIIIHHHHHH"
            )
            header = struct.unpack(
                header_format, elf.read(struct.calcsize(header_format))
            )
            return header[5] != 0 and header[11] != 0
    except (OSError, struct.error):
        return False


class ElfLayout:
    def __init__(self, path, elf_class, elf_type, entry, phoff, phentsize,
                 phnum, program_headers):
        self.path = path
        self.elf_class = elf_class
        self.elf_type = elf_type
        self.entry = entry
        self.phoff = phoff
        self.phentsize = phentsize
        self.phnum = phnum
        self.program_headers = program_headers

    def load_bias(self, runtime_phdr):
        for header in self.program_headers:
            if header["type"] == PT_PHDR:
                return runtime_phdr - header["vaddr"]

        phdr_end = self.phoff + self.phentsize * self.phnum
        for header in self.program_headers:
            file_end = header["offset"] + header["filesz"]
            if (header["type"] == PT_LOAD and
                    header["offset"] <= self.phoff and phdr_end <= file_end):
                phdr_vaddr = header["vaddr"] + self.phoff - header["offset"]
                return runtime_phdr - phdr_vaddr

        raise gdb.GdbError(
            "cannot relate AT_PHDR to a PT_PHDR or PT_LOAD segment in " +
            self.path
        )


def read_elf_layout(path):
    path = os.path.abspath(os.path.expanduser(path))
    try:
        elf = open(path, "rb")
    except OSError as exc:
        raise gdb.GdbError("cannot open userspace ELF {}: {}".format(
            path, exc)) from exc

    with elf:
        ident = elf.read(16)
        if len(ident) != 16 or ident[:4] != b"\x7fELF":
            raise gdb.GdbError("not an ELF file: " + path)
        if ident[4] not in (1, 2) or ident[5] not in (1, 2):
            raise gdb.GdbError("unsupported ELF class or byte order: " + path)

        elf_class = 32 if ident[4] == 1 else 64
        endian = "<" if ident[5] == 1 else ">"
        header_format = endian + (
            "HHIIIIIHHHHHH" if elf_class == 32 else "HHIQQQIHHHHHH"
        )
        header_size = struct.calcsize(header_format)
        raw_header = elf.read(header_size)
        if len(raw_header) != header_size:
            raise gdb.GdbError("truncated ELF header: " + path)
        header = struct.unpack(header_format, raw_header)
        elf_type, entry, phoff = header[0], header[3], header[4]
        phentsize, phnum = header[8], header[9]

        program_format = endian + (
            "IIIIIIII" if elf_class == 32 else "IIQQQQQQ"
        )
        program_size = struct.calcsize(program_format)
        if phentsize < program_size:
            raise gdb.GdbError("invalid ELF program-header size: " + path)

        program_headers = []
        for index in range(phnum):
            elf.seek(phoff + index * phentsize)
            raw_program = elf.read(program_size)
            if len(raw_program) != program_size:
                raise gdb.GdbError("truncated ELF program headers: " + path)
            program = struct.unpack(program_format, raw_program)
            if elf_class == 32:
                p_type, p_offset, p_vaddr, p_filesz = (
                    program[0], program[1], program[2], program[4]
                )
            else:
                p_type, p_offset, p_vaddr, p_filesz = (
                    program[0], program[2], program[3], program[5]
                )
            program_headers.append({
                "type": p_type,
                "offset": p_offset,
                "vaddr": p_vaddr,
                "filesz": p_filesz,
            })

    if elf_type not in (ET_EXEC, ET_DYN):
        raise gdb.GdbError("userspace ELF is neither ET_EXEC nor ET_DYN: " + path)
    return ElfLayout(path, elf_class, elf_type, entry, phoff, phentsize,
                     phnum, program_headers)


class RawTask:
    def __init__(self, address, layout):
        self.address = address
        self.layout = layout

    @property
    def pid(self):
        return read_integer(self.address + self.layout.pid_offset, 4)

    @property
    def comm(self):
        memory = gdb.selected_inferior().read_memory(
            self.address + self.layout.comm_offset, 32
        ).tobytes()
        return memory.split(b"\0", 1)[0].decode(errors="replace")

    @property
    def mm(self):
        return read_integer(self.address + self.layout.mm_offset)


class RawTaskLayout:
    def __init__(self, init_address, task_size, tasks_offset, pid_offset,
                 comm_offset, mm_offset, auxv_offset):
        self.init_address = init_address
        self.task_size = task_size
        self.tasks_offset = tasks_offset
        self.pid_offset = pid_offset
        self.comm_offset = comm_offset
        self.mm_offset = mm_offset
        self.auxv_offset = auxv_offset


RAW_LAYOUT = None


def read_auxv_at(address, word_size=None):
    word_size = word_size or pointer_size()
    result = {}
    for index in range(64):
        key = read_integer(address + index * 2 * word_size, word_size)
        value = read_integer(address + (index * 2 + 1) * word_size, word_size)
        if key == AT_NULL:
            break
        if key > 0x1000 or key in result:
            raise gdb.GdbError("invalid saved auxiliary vector")
        result[key] = value
    return result


def find_auxv_offset(mm_address):
    word_size = pointer_size()
    scan_size = 4096
    try:
        memory = gdb.selected_inferior().read_memory(
            mm_address, scan_size
        ).tobytes()
    except (gdb.error, MemoryError):
        return None
    code = "I" if word_size == 4 else "Q"
    words = struct.unpack(
        target_byte_order() + code * (len(memory) // word_size), memory
    )
    # AT_PHDR, AT_PHENT and AT_PHNUM are consecutive in Linux's saved auxv.
    # Starting there is sufficient for relocation and reaches AT_ENTRY and
    # AT_RANDOM without needing the configuration-dependent leading entries.
    # mm_struct does not promise that saved_auxv starts at an even pointer-word
    # index relative to the structure base.
    for index in range(0, len(words) - 6):
        if words[index] != AT_PHDR or words[index + 2] != AT_PHENT:
            continue
        if words[index + 4] != AT_PHNUM:
            continue
        try:
            auxv = read_auxv_at(mm_address + index * word_size, word_size)
        except (gdb.error, gdb.GdbError, gdb.MemoryError):
            continue
        if AT_ENTRY in auxv and AT_RANDOM in auxv:
            return index * word_size
    return None


def printable_comm(memory, offset):
    raw = memory[offset:offset + 32].split(b"\0", 1)[0]
    return bool(raw) and len(raw) < 32 and all(0x20 <= byte < 0x7f for byte in raw)


def infer_raw_task_layout():
    global RAW_LAYOUT
    init_address = int(gdb.parse_and_eval("&init_task"))
    if RAW_LAYOUT and RAW_LAYOUT.init_address == init_address:
        return RAW_LAYOUT

    filename = gdb.current_progspace().filename
    task_size = elf_symbol_size(filename, "init_task") if filename else None
    if not task_size or task_size < 512 or task_size > 65536:
        raise gdb.GdbError(
            "cannot determine init_task size from the kernel ELF symbol table"
        )
    word_size = pointer_size()
    inferior = gdb.selected_inferior()
    init_memory = inferior.read_memory(init_address, task_size).tobytes()
    comm_offset = init_memory.find(b"swapper/0\0")
    if comm_offset < 0:
        comm_offset = init_memory.find(b"swapper\0")
    if comm_offset < 0:
        raise gdb.GdbError("cannot locate init_task's command name")

    candidates = []
    for tasks_offset in range(0, task_size - 2 * word_size, word_size):
        node = init_address + tasks_offset
        try:
            next_node = read_integer(node)
            previous_node = read_integer(node + word_size)
            if next_node == node or previous_node == node:
                continue
            if (read_integer(next_node + word_size) != node or
                    read_integer(previous_node) != node):
                continue
            next_task = next_node - tasks_offset
            memory = inferior.read_memory(next_task, task_size).tobytes()
            if printable_comm(memory, comm_offset):
                candidates.append((tasks_offset, next_task, memory))
        except (gdb.error, gdb.MemoryError, MemoryError):
            continue
    if not candidates:
        raise gdb.GdbError("cannot infer the Linux task-list offset")

    for tasks_offset, first_task, first_memory in candidates:
        pid_candidates = []
        for offset in range(tasks_offset + 2 * word_size,
                            comm_offset - 4, 4):
            initial = struct.unpack_from(target_byte_order() + "I", init_memory,
                                         offset)[0]
            first = struct.unpack_from(target_byte_order() + "I", first_memory,
                                       offset)[0]
            if initial == 0 and first == 1:
                pid_candidates.append(offset)
        consecutive = [
            offset for offset in pid_candidates if offset + 4 in pid_candidates
        ]
        if not consecutive:
            continue
        pid_offset = consecutive[0]

        seen_pointers = set()
        for mm_offset in range(0, task_size - word_size + 1, word_size):
            mm_address = struct.unpack_from(
                target_byte_order() + ("I" if word_size == 4 else "Q"),
                first_memory, mm_offset
            )[0]
            if not mm_address or mm_address in seen_pointers:
                continue
            seen_pointers.add(mm_address)
            auxv_offset = find_auxv_offset(mm_address)
            if auxv_offset is None:
                continue
            RAW_LAYOUT = RawTaskLayout(
                init_address, task_size, tasks_offset, pid_offset,
                comm_offset, mm_offset, auxv_offset
            )
            gdb.write(
                "Kernel has reduced DWARF; inferred task offsets from the "
                "live init_task and PID 1.\n"
            )
            return RAW_LAYOUT
    raise gdb.GdbError(
        "cannot infer PID 1's mm and saved auxiliary-vector offsets"
    )


def current_task():
    # The upstream helper uses the per-CPU current_task symbol, which is not
    # present on every architecture (notably MIPS).  The scheduler runqueue is
    # per-CPU on all supported Linux targets and retains its curr pointer even
    # while that task is executing in userspace.
    try:
        cpu = max(0, gdb.selected_thread().num - 1)
        runqueues = gdb.parse_and_eval("&runqueues")
        try:
            offset = int(gdb.parse_and_eval(
                "__per_cpu_offset[{}]".format(cpu)
            ))
        except gdb.error:
            offset = 0
        runqueue_type = gdb.lookup_type("struct rq").pointer()
        runqueue = gdb.Value(int(runqueues) + offset).cast(
            runqueue_type
        ).dereference()
        return runqueue["curr"].dereference()
    except gdb.error as exc:
        raise gdb.GdbError(
            "cannot identify the current Linux runqueue; matching kernel "
            "DWARF with struct rq and runqueues is required"
        ) from exc


def scheduled_task():
    return current_task()


def typed_linux_tasks():
    task_type = gdb.lookup_type("struct task_struct")
    task_pointer = task_type.pointer()
    init = gdb.parse_and_eval("init_task")
    tasks_field = next(
        field for field in task_type.fields() if field.name == "tasks"
    )
    tasks_offset = tasks_field.bitpos // 8
    list_head = init["tasks"].address

    yield init
    node = init["tasks"]["next"]
    while int(node) != int(list_head):
        task = gdb.Value(int(node) - tasks_offset).cast(
            task_pointer
        ).dereference()
        yield task
        node = task["tasks"]["next"]


def raw_linux_tasks():
    layout = infer_raw_task_layout()
    init_node = layout.init_address + layout.tasks_offset
    yield RawTask(layout.init_address, layout)
    node = read_integer(init_node)
    count = 0
    while node != init_node:
        yield RawTask(node - layout.tasks_offset, layout)
        node = read_integer(node)
        count += 1
        if count > 32768:
            raise gdb.GdbError("Linux task list did not return to init_task")


def linux_tasks():
    try:
        task_type = gdb.lookup_type("struct task_struct").strip_typedefs()
        if task_type.code == gdb.TYPE_CODE_STRUCT and task_type.fields():
            yield from typed_linux_tasks()
            return
    except gdb.error:
        pass
    yield from raw_linux_tasks()


def task_by_pid(pid):
    for task in linux_tasks():
        if task_identity(task)[0] == pid:
            return task
    raise gdb.GdbError("No task of PID {}".format(pid))


def task_identity(task):
    if isinstance(task, RawTask):
        return task.pid, task.comm
    return int(task["pid"]), task["comm"].string(errors="replace")


def selector_matches(selector, task):
    pid, comm = task_identity(task)
    if selector[0] == "pid":
        return pid == selector[1]
    return comm == selector[1]


def parse_selector(text):
    if text.startswith("pid:"):
        try:
            return "pid", int(text[4:], 0)
        except ValueError as exc:
            raise gdb.GdbError("invalid PID selector: " + text) from exc
    if text.startswith("comm:"):
        return "comm", text[5:]
    try:
        return "pid", int(text, 0)
    except ValueError:
        return "comm", text


def selector_text(selector):
    return "{}:{}".format(selector[0], selector[1])


def task_auxv(task):
    if isinstance(task, RawTask):
        if not task.mm:
            raise gdb.GdbError("selected task has no userspace mm")
        return read_auxv_at(task.mm + task.layout.auxv_offset)
    mm_pointer = task["mm"]
    if int(mm_pointer) == 0:
        raise gdb.GdbError("selected task has no userspace mm")
    mm = mm_pointer.dereference()
    try:
        saved = mm["saved_auxv"]
    except gdb.error as exc:
        raise gdb.GdbError(
            "this kernel's struct mm_struct has no saved_auxv debug field"
        ) from exc

    try:
        low, high = saved.type.strip_typedefs().range()
    except gdb.error as exc:
        raise gdb.GdbError("saved_auxv is not an inspectable array") from exc

    result = {}
    index = low
    while index + 1 <= high:
        key = int(saved[index])
        value = int(saved[index + 1])
        if key == AT_NULL:
            break
        result[key] = value
        index += 2
    return result


def task_executable_name(task):
    if isinstance(task, RawTask):
        return "?"
    try:
        exe_file = task["mm"].dereference()["exe_file"]
        if int(exe_file) == 0:
            return "?"
        return exe_file.dereference()["f_path"]["dentry"].dereference()[
            "d_name"
        ]["name"].string(errors="replace")
    except (gdb.error, TypeError):
        return "?"


def task_address(task):
    return task.address if isinstance(task, RawTask) else int(task.address)


def task_mm_address(task):
    return task.mm if isinstance(task, RawTask) else int(task["mm"])


class UserSession:
    def __init__(self):
        self.pid = None
        self.comm = None
        self.symbol_file = None
        self.runtime_entry = None
        self.bias = None
        self.cookie_address = None
        self.cookie = None

    def unload(self):
        if self.symbol_file:
            quoted = gdb_quote(self.symbol_file)
            try:
                gdb.execute("remove-symbol-file " + quoted, to_string=True)
            except gdb.error:
                # The user may already have removed it manually.
                pass
        self.pid = None
        self.comm = None
        self.symbol_file = None
        self.runtime_entry = None
        self.bias = None
        self.cookie_address = None
        self.cookie = None

    def capture_address_space_cookie(self, auxv):
        self.cookie_address = auxv.get(AT_RANDOM)
        self.cookie = None
        if self.cookie_address:
            try:
                memory = gdb.selected_inferior().read_memory(
                    self.cookie_address, 16
                )
                self.cookie = memory.tobytes()
            except (gdb.error, MemoryError):
                self.cookie_address = None

    @staticmethod
    def address_space_matches(pid, cookie_address, cookie):
        if cookie_address is not None:
            if cookie is None:
                return False
            try:
                memory = gdb.selected_inferior().read_memory(
                    cookie_address, len(cookie)
                )
                return memory.tobytes() == cookie
            except (gdb.error, MemoryError):
                return False
        try:
            current_pid, _ = task_identity(current_task())
            return current_pid == pid
        except (gdb.error, gdb.GdbError):
            # Some architectures have no current_task symbol while stopped in
            # userspace.  Without AT_RANDOM there is no architecture-neutral
            # address-space identity available to this lightweight layer.
            return True


SESSION = UserSession()


def gdb_quote(text):
    return '"{}"'.format(text.replace("\\", "\\\\").replace('"', '\\"'))


class FocusBreakpoint(gdb.Breakpoint):
    def __init__(self, pid, cookie_address, cookie):
        super().__init__("finish_task_switch", internal=True)
        self.pid = pid
        self.cookie_address = cookie_address
        self.cookie = cookie
        self.matched = False

    def stop(self):
        if self.cookie_address is not None and self.cookie is None:
            try:
                self.cookie = gdb.selected_inferior().read_memory(
                    self.cookie_address, 16
                ).tobytes()
                self.matched = True
                return True
            except (gdb.error, MemoryError):
                return False
        self.matched = UserSession.address_space_matches(
            self.pid, self.cookie_address, self.cookie
        )
        return self.matched


def focus_task(selector):
    matches = [task for task in linux_tasks() if selector_matches(selector, task)]
    if not matches:
        raise gdb.GdbError("No task matching {}".format(selector_text(selector)))
    if len(matches) > 1:
        raise gdb.GdbError(
            "{} matches more than one task; select it by PID".format(
                selector_text(selector)
            )
        )
    task = matches[0]
    pid, _ = task_identity(task)
    auxv = task_auxv(task)
    cookie_address = auxv.get(AT_RANDOM)
    cookie = None
    if cookie_address:
        try:
            cookie = gdb.selected_inferior().read_memory(
                cookie_address, 16
            ).tobytes()
        except (gdb.error, MemoryError):
            pass
    if UserSession.address_space_matches(pid, cookie_address, cookie):
        return task

    try:
        breakpoint = FocusBreakpoint(pid, cookie_address, cookie)
    except gdb.error as exc:
        raise gdb.GdbError(
            "finish_task_switch is unavailable; matching kernel symbols are "
            "required to follow a userspace task"
        ) from exc

    gdb.write("Waiting for {} to be scheduled...\n".format(
        selector_text(selector)))
    try:
        gdb.execute("continue")
    finally:
        if breakpoint.is_valid():
            breakpoint.delete()

    if not breakpoint.matched:
        raise gdb.GdbError(
            "the target stopped at an unrelated breakpoint; remove or "
            "disable it and retry"
        )
    return task


def load_user_symbols(task, executable, symbol_file=None):
    pid, comm = task_identity(task)
    layout = read_elf_layout(executable)
    symbols = os.path.abspath(os.path.expanduser(symbol_file or executable))
    if not os.path.isfile(symbols):
        raise gdb.GdbError("userspace symbol file does not exist: " + symbols)

    auxv = task_auxv(task)
    if AT_PHDR not in auxv:
        raise gdb.GdbError("the selected task's saved auxv has no AT_PHDR")
    bias = layout.load_bias(auxv[AT_PHDR])
    runtime_entry = bias + layout.entry

    if AT_ENTRY in auxv and auxv[AT_ENTRY] != runtime_entry:
        raise gdb.GdbError(
            "local ELF does not match pid {}: calculated entry {:#x}, guest "
            "AT_ENTRY {:#x}".format(pid, runtime_entry, auxv[AT_ENTRY])
        )

    SESSION.unload()
    symbols_loaded = elf_has_section_table(symbols)
    if symbols_loaded:
        gdb.execute("add-symbol-file {} -o {:#x}".format(
            gdb_quote(symbols), bias))
    SESSION.pid = pid
    SESSION.comm = comm
    SESSION.symbol_file = symbols if symbols_loaded else None
    SESSION.runtime_entry = runtime_entry
    SESSION.bias = bias
    SESSION.capture_address_space_cookie(auxv)

    gdb.write(
        "Selected {}-bit userspace pid {} ({}) at bias {:#x}; "
        "runtime entry {:#x}.\n".format(
            layout.elf_class, pid, comm, bias, runtime_entry)
    )
    if symbols_loaded:
        gdb.write("Loaded userspace symbols from {}.\n".format(symbols))
    else:
        gdb.write(
            "The selected symbol file has no ELF section table; process "
            "filtering and numeric breakpoints work, but source symbols "
            "require the matching unstripped or separate debug ELF.\n"
        )
    if SESSION.cookie is None:
        gdb.write(
            "Warning: AT_RANDOM could not be read; userspace breakpoints "
            "cannot be filtered by address space on this stop.\n"
        )
    if layout.elf_class == 32 and gdb.selected_frame().architecture().name().endswith("64"):
        gdb.write(
            "Warning: this is a compat 32-bit process under a 64-bit kernel; "
            "register decoding and unwinding depend on the target stub's "
            "compat-mode support.\n"
        )
    return pid


class ProcessBreakpoint(gdb.Breakpoint):
    def __init__(self, specification, pid, temporary=False):
        # QEMU implements these as translated-code breakpoints.  Asking GDB
        # for a hardware breakpoint avoids its attempt to read and patch a
        # userspace page which may not have entered a software-managed MIPS
        # TLB yet.
        architecture = gdb.selected_frame().architecture().name().lower()
        breakpoint_type = (
            gdb.BP_BREAKPOINT if "i386" in architecture
            else gdb.BP_HARDWARE_BREAKPOINT
        )
        super().__init__(specification, type=breakpoint_type,
                         temporary=temporary)
        self.pid = pid
        self.cookie_address = SESSION.cookie_address
        self.cookie = SESSION.cookie

    def stop(self):
        return SESSION.address_space_matches(
            self.pid, self.cookie_address, self.cookie
        )


class VirosUserFocus(gdb.Command):
    """Wait until a Linux PID or comm is the task running on a virtual CPU.

viros-user-focus PID|COMM
The explicit forms pid:NUMBER and comm:NAME resolve numeric ambiguity.
"""

    def __init__(self):
        super().__init__("viros-user-focus", gdb.COMMAND_RUNNING)

    def invoke(self, argument, from_tty):
        argv = gdb.string_to_argv(argument)
        if len(argv) != 1:
            raise gdb.GdbError("usage: viros-user-focus PID|COMM")
        task = focus_task(parse_selector(argv[0]))
        pid, comm = task_identity(task)
        gdb.write("Current Linux task is pid {} ({}).\n".format(pid, comm))


class VirosPs(gdb.Command):
    """List Linux tasks using matching kernel symbols and live task data."""

    def __init__(self):
        super().__init__("viros-ps", gdb.COMMAND_DATA)

    def invoke(self, argument, from_tty):
        if argument.strip():
            raise gdb.GdbError("viros-ps takes no arguments")
        for task in linux_tasks():
            pid, comm = task_identity(task)
            mm = task_mm_address(task)
            gdb.write("{:#x} {:6d} mm={:#x} {}\n".format(
                task_address(task), pid, mm, comm
            ))


class VirosUserInfo(gdb.Command):
    """Show the current task's executable identity and saved ELF auxv."""

    def __init__(self):
        super().__init__("viros-user-info", gdb.COMMAND_DATA)

    def invoke(self, argument, from_tty):
        argv = gdb.string_to_argv(argument)
        if len(argv) > 1:
            raise gdb.GdbError("usage: viros-user-info [PID]")
        if argv:
            try:
                pid = int(argv[0], 0)
            except ValueError as exc:
                raise gdb.GdbError("invalid PID: " + argv[0]) from exc
            task = task_by_pid(pid)
        else:
            task = scheduled_task()
        pid, comm = task_identity(task)
        auxv = task_auxv(task)
        gdb.write("pid {} ({}) executable {}\n".format(
            pid, comm, task_executable_name(task)))
        names = (
            (AT_PHDR, "AT_PHDR"), (AT_PHENT, "AT_PHENT"),
            (AT_PHNUM, "AT_PHNUM"), (AT_PAGESZ, "AT_PAGESZ"),
            (AT_BASE, "AT_BASE"), (AT_ENTRY, "AT_ENTRY"),
            (AT_RANDOM, "AT_RANDOM"),
        )
        for key, name in names:
            if key in auxv:
                gdb.write("  {:9s} {:#x}\n".format(name, auxv[key]))


class VirosUserLoad(gdb.Command):
    """Relocate local userspace symbols for a selected Linux task.

viros-user-load PID LOCAL-EXECUTABLE [LOCAL-SYMBOL-FILE]
LOCAL-EXECUTABLE supplies the ELF program headers.  LOCAL-SYMBOL-FILE may be
the same unstripped ELF or a separate debug file.
"""

    def __init__(self):
        super().__init__("viros-user-load", gdb.COMMAND_FILES)

    def invoke(self, argument, from_tty):
        argv = gdb.string_to_argv(argument)
        if len(argv) not in (2, 3):
            raise gdb.GdbError(
                "usage: viros-user-load PID LOCAL-EXECUTABLE "
                "[LOCAL-SYMBOL-FILE]"
            )
        try:
            pid = int(argv[0], 0)
        except ValueError as exc:
            raise gdb.GdbError("invalid PID: " + argv[0]) from exc
        load_user_symbols(
            task_by_pid(pid), argv[1], argv[2] if len(argv) == 3 else None
        )


class VirosUserDebug(gdb.Command):
    """Focus a Linux task and relocate its local userspace symbols.

viros-user-debug PID|COMM LOCAL-EXECUTABLE [LOCAL-SYMBOL-FILE]
"""

    def __init__(self):
        super().__init__("viros-user-debug", gdb.COMMAND_RUNNING)

    def invoke(self, argument, from_tty):
        argv = gdb.string_to_argv(argument)
        if len(argv) not in (2, 3):
            raise gdb.GdbError(
                "usage: viros-user-debug PID|COMM LOCAL-EXECUTABLE "
                "[LOCAL-SYMBOL-FILE]"
            )
        task = focus_task(parse_selector(argv[0]))
        load_user_symbols(task, argv[1], argv[2] if len(argv) == 3 else None)


class VirosUserBreak(gdb.Command):
    """Set a breakpoint that stops only in the selected userspace PID.

Use LOCATION entry to stop at the selected executable's relocated ELF entry.
"""

    def __init__(self, name="viros-user-break", temporary=False):
        super().__init__(name, gdb.COMMAND_BREAKPOINTS)
        self.command_name = name
        self.temporary = temporary

    def invoke(self, argument, from_tty):
        specification = argument.strip()
        if not specification:
            raise gdb.GdbError("usage: {} LOCATION".format(self.command_name))
        if SESSION.pid is None:
            raise gdb.GdbError("run viros-user-debug or viros-user-load first")
        if specification == "entry":
            specification = "*{:#x}".format(SESSION.runtime_entry)
        breakpoint = ProcessBreakpoint(
            specification, SESSION.pid, temporary=self.temporary
        )
        gdb.write("Breakpoint {} is restricted to Linux pid {} ({}).\n".format(
            breakpoint.number, SESSION.pid, SESSION.comm))


class VirosUserUnload(gdb.Command):
    """Remove the userspace symbol file loaded by viros-user-load."""

    def __init__(self):
        super().__init__("viros-user-unload", gdb.COMMAND_FILES)

    def invoke(self, argument, from_tty):
        if argument.strip():
            raise gdb.GdbError("viros-user-unload takes no arguments")
        SESSION.unload()
        gdb.write("Removed viros userspace symbols.\n")


VirosUserFocus()
VirosPs()
VirosUserInfo()
VirosUserLoad()
VirosUserDebug()
VirosUserBreak()
VirosUserBreak("viros-user-tbreak", temporary=True)
VirosUserUnload()
