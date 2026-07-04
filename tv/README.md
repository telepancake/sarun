# proctrace Output Schema

## Interface

### `/proc/proctrace/new`

Open this file to create a tracing session. The process that opens it is automatically tagged — all its descendants (via fork/exec) inherit the tag.

- **`open()`**: Creates session. The opener and all future descendants are traced.
- **`read()`**: Returns JSONL trace data. Blocks when buffer is empty. Returns 0 (EOF) when session is closed.
- **`poll()`**: Supports `POLLIN` (data available) and `POLLHUP` (session closed).
- **`close()`**: Destroys session. All tags freed.

No write/configuration step is needed. Tracing starts immediately on open.

The trace stream has backpressure: if the reader falls behind, traced processes block until the reader drains the buffer. No events are lost.

### `/proc/proctrace/sessions`

Read-only listing of active sessions with columns: ID, ROOT_TGID, #TAGS, BUF_USED, STATUS.

### Usage example

```bash
# Read trace into a file. Tracing starts immediately.
cat /proc/proctrace/new > /tmp/trace.jsonl &
READER_PID=$!

# Run the workload
make -j8
./test_suite

# Stop tracing
kill $READER_PID
```

```python
import os, subprocess, json

fd = os.open("/proc/proctrace/new", os.O_RDONLY)
f = os.fdopen(fd, "r")

proc = subprocess.Popen(["make", "-j8"])
proc.wait()

# Read all buffered events
os.set_blocking(f.fileno(), False)
for line in f:
    event = json.loads(line)
    print(event["event"], event.get("exe"), event.get("argv"))
f.close()
```

## tv — viewing traces

`tv` is a TUI viewer over a DuckDB-backed trace store. The pipeline is:

```
trace bytes ──► TraceDecoder ──► DuckDB Appender ──► foo.tvdb (mmaped, columnar, zstd)
                                                            │
                                                            ▼
                                                      SQL queries → TUI panels
```

The on-disk format is a single DuckDB native database file (`foo.tvdb`) with one
table per trace event class (`exec`, `argv`, `env`, `auxv`, `exit_`, `open_`,
`cwd`, `stdout_`, `stderr_`). Files are mmaped on open — "loading" a multi-GB
trace is opening one fd. tv never builds an in-memory copy of the trace.

```bash
# Live: pick a tracer, stream events into a fresh .tvdb and view them.
tv --tracer upttrace  -- make -j8       # ptrace, works anywhere
tv --tracer sudtrace  -- make -j8       # syscall-user-dispatch, fastest
tv --tracer modtrace  -- make -j8       # kernel module, lowest overhead

# Convert a trace file into a .tvdb (incremental, bounded memory) and open it.
tv --trace trace.bin.zst          # creates trace.tvdb if missing/stale

# Open an existing .tvdb directly (no ingest).
tv --open trace.tvdb

# Non-interactive: dump a panel mode to stdout (1=proc, 2=file, 3=event,
# 4=deps, 5=rdeps, 6=dcmds, 7=rcmds; modes 4..7 need --subject FILE).
tv --open trace.tvdb --dump=1
tv --open trace.tvdb --dump=4 --subject /path/to/output

# Subcommands.
tv dump trace.bin         # hexdump a trace stream
tv dump --selftest        # roundtrip-test the atom encoding
tv fv [path]              # file viewer
tv test                   # built-in self-tests
tv ingest <trace> -o OUT  # build .tvdb without launching the UI
```

Pass `--no-env` to omit environment variables from emitted `EXEC` events.

### Panels

All seven panel modes are SQL-backed:

| Mode | Key | Contents | Right pane |
| ---: | :-: | :--- | :--- |
| 1 | `1` | Process tree (parent-child, basename, duration, exit) | argv, env, opens, children |
| 2 | `2` | Per-path stats (R/W/E flags, opens, procs, errors) | opens log with flags+errs+who |
| 3 | `3` | Event log (EXEC/CWD/OPEN/EXIT/STDOUT/STDERR by ts_ns) | per-kind detail + owning process |
| 4 | `4` | Deps closure of subject file (recursive CTE) | file detail |
| 5 | `5` | Reverse-deps closure of subject file | file detail |
| 6 | `6` | Processes in dep closure | process detail |
| 7 | `7` | Processes in reverse-dep closure | process detail |

Modes 4..7 take their subject from the cursor in mode 2: navigate to a file, then press 4..7. Other keys: `/` search, `t` toggle tree/flat, `s` (mode 1) pin subtree, `?` help, `q` quit.

### Lazy indices

Heavy aggregations are materialised on first use into `tv_idx_proc`, `tv_idx_path`, `tv_idx_edge` and recorded in `tv_meta`. Subsequent opens of the same `.tvdb` reuse them — switching to a panel for the first time builds its index, switching back is instant.

### Build

DuckDB amalgamation is huge; the Makefile defaults to `clang++ -O0` because that compiles ~3× faster (~2-3 min) and uses ~⅓ the RAM (~5 GB) of `g++ -O2`. Override for release builds:

```bash
make tv DUCKDB_CXX=g++ DUCKDB_OPT=-O2
```

The resulting `tv` is statically linked; `-ldl` does not force a dynamic binary, contrary to popular belief — the linker just emits warnings about NSS at runtime, which `tv` does not exercise.

The kernel-module pieces now live under `mod/` and build standalone there:

```bash
make -C mod
./mod/modtrace -- make -j8
```

---

## Output Format

One JSON object per line (JSONL). All strings are JSON-escaped per RFC 8259.

---

## Common Fields

Present in every event.

| Field    | Type   | Description |
|----------|--------|-------------|
| `event`  | string | One of: `"EXEC"`, `"EXIT"`, `"OPEN"`, `"CWD"`, `"STDOUT"`, `"STDERR"` |
| `ts`     | number | Unix timestamp with nanosecond precision, e.g. `1711814400.123456789` |
| `pid`    | int    | Kernel thread ID |
| `tgid`   | int    | Thread group ID — the process identity. All threads share the same `tgid`. **Primary process identifier.** |
| `ppid`   | int    | Parent TGID (init namespace) |
| `nspid`  | int    | PID in the process's own PID namespace |
| `nstgid` | int    | TGID in the process's own PID namespace |

### Thread handling

Tagging is by `tgid`. Individual threads do not generate separate events. EXIT is only emitted for the thread group leader (process exit).

---

## Event: `EXEC`

Emitted after a successful `execve()`. A `CWD` event is always emitted immediately before this event for the same process.

| Field  | Type           | Description |
|--------|----------------|-------------|
| `exe`  | string ∣ null  | Resolved absolute path to the binary. Symlinks resolved. |
| `argv` | string[]       | Argument vector. Empty array `[]` if unreadable. |
| `env`  | object         | Optional. Present unless trace capture was started with `--no-env`. When present, contains the environment as `{"KEY":"VALUE",...}`. Entries without `=` have empty string value. Empty object `{}` if unreadable. |
| `auxv` | object         | ELF auxiliary vector entries (see below). |

### `auxv` fields

| Key           | Type   | Always present | Description |
|---------------|--------|----------------|-------------|
| `AT_UID`      | int    | yes            | Real user ID |
| `AT_EUID`     | int    | yes            | Effective user ID |
| `AT_GID`      | int    | yes            | Real group ID |
| `AT_EGID`     | int    | yes            | Effective group ID |
| `AT_SECURE`   | int    | yes            | 1 if setuid/setgid |
| `AT_CLKTCK`   | int    | usually        | Clock ticks per second |
| `AT_EXECFN`   | string | usually        | Filename passed to exec |
| `AT_PLATFORM` | string | usually        | Hardware platform, e.g. `"x86_64"` |

### Example

```json
{"event":"EXEC","ts":1711814400.123456789,"pid":1234,"tgid":1234,"ppid":1200,"nspid":1234,"nstgid":1234,"exe":"/usr/bin/make","argv":["make","-j8"],"env":{"HOME":"/root","PATH":"/usr/bin:/bin"},"auxv":{"AT_UID":1000,"AT_EUID":1000,"AT_GID":1000,"AT_EGID":1000,"AT_SECURE":0,"AT_EXECFN":"/usr/bin/make","AT_PLATFORM":"x86_64"}}
```

---

## Event: `EXIT`

Emitted when a process (thread group leader) exits.

| Field         | Type   | Condition                  | Description |
|---------------|--------|----------------------------|-------------|
| `status`      | string | always                     | `"exited"` or `"signaled"` |
| `code`        | int    | `status == "exited"`       | Exit code (0–255) |
| `signal`      | int    | `status == "signaled"`     | Signal number |
| `core_dumped` | bool   | `status == "signaled"`     | Core dump produced |
| `raw`         | int    | always                     | Raw kernel exit code |

### Examples

```json
{"event":"EXIT","ts":1711814401.456,"pid":1234,"tgid":1234,"ppid":1200,"nspid":1234,"nstgid":1234,"status":"exited","code":0,"raw":0}
{"event":"EXIT","ts":1711814402.012,"pid":1236,"tgid":1236,"ppid":1234,"nspid":1236,"nstgid":1236,"status":"signaled","signal":11,"core_dumped":true,"raw":139}
```

---

## Event: `OPEN`

Emitted when a tagged process opens a file. Also emitted synthetically
for each file descriptor a process inherits across `execve()` (see
*Inherited fds* below).

| Field       | Type           | Condition     | Description |
|-------------|----------------|---------------|-------------|
| `path`      | string ∣ null  | always        | Path as passed to the syscall, or the resolved path of an inherited fd (e.g. `"pipe:[12345]"` for pipes, `"socket:[67890]"` for sockets). May be relative to current CWD for real `open()` calls. |
| `flags`     | string[]       | always        | Open flags, e.g. `["O_RDONLY"]` or `["O_WRONLY","O_CREAT","O_TRUNC"]` |
| `fd`        | int            | on success    | Returned (or inherited) file descriptor number |
| `ino`       | int            | on success    | Inode number of the opened file |
| `dev`       | string         | on success    | Device in `"major:minor"` format (identifies the filesystem) |
| `err`       | int            | on failure    | Negative errno (e.g. `-2` for ENOENT) |
| `inherited` | bool           | inherited fds | `true` for synthetic OPEN events emitted at exec time for inherited fds. Absent (or `false`) for real `open()` calls. |

### Identifying files

Two paths refer to the same file if and only if their `(dev, ino)` pairs match. This handles hardlinks, bind mounts, and different relative paths to the same file.

### Inherited fds (`"inherited": true`)

Immediately after every `EXEC` event, one synthetic `OPEN` event is
emitted for **each open file descriptor the new program inherited**
from its predecessor (i.e. every fd not marked `O_CLOEXEC`). These
events carry `"inherited": true` and describe the fd's current state
in the post-exec process: resolved path, flags, and `(dev, ino)`.

This lets consumers reconstruct what files a program "already had
open" when it started — even if the program itself never calls
`open()`. For example, in `cat file1.txt | sort > file2.txt`, the
`sort` process never opens anything, yet the trace contains inherited
OPEN events showing `fd 0` bound to the pipe from `cat` and `fd 1`
bound to `file2.txt`.

#### Pairing pipe / socket endpoints

Both ends of a kernel pipe share the **same pipefs inode**. So when
you see two inherited OPEN events with matching `(dev, ino)` in two
different tgids — one with a write-capable `flags` set and one with a
read-capable one — they are the two ends of the same pipe. This is
how pipelines like `cat a | sort > b` can be reconstructed end-to-end
from the trace:

1. `cat` has an inherited OPEN for `fd 1` → `pipe:[N]` with `O_WRONLY`.
2. `sort` has an inherited OPEN for `fd 0` → `pipe:[N]` with `O_RDONLY`.
3. The matching `ino=N` (and matching `dev` for pipefs) links them.

The same `(dev, ino)` matching rule identifies the endpoints of a
`socketpair()` when both ends live in the same socket inode (kernel
socket objects share a sockfs inode per endpoint — when the kernel
assigns them distinct inodes, they will not match; in that case the
pairing must be inferred by other means).

### Examples

Real open:

```json
{"event":"OPEN","ts":1711814400.200,"pid":1234,"tgid":1234,"ppid":1200,"nspid":1234,"nstgid":1234,"path":"/etc/passwd","flags":["O_RDONLY"],"fd":3,"ino":524297,"dev":"259:2"}
{"event":"OPEN","ts":1711814400.400,"pid":1234,"tgid":1234,"ppid":1200,"nspid":1234,"nstgid":1234,"path":"nosuchfile","flags":["O_RDONLY"],"err":-2}
```

Inherited fds in `cat a | sort > b` — note the shared `ino` on the
pipe between the two processes:

```json
{"event":"OPEN","ts":1711814400.101,"pid":2001,"tgid":2001,"ppid":2000,"nspid":2001,"nstgid":2001,"path":"pipe:[88231]","flags":["O_WRONLY"],"fd":1,"ino":88231,"dev":"0:12","inherited":true}
{"event":"OPEN","ts":1711814400.102,"pid":2002,"tgid":2002,"ppid":2000,"nspid":2002,"nstgid":2002,"path":"pipe:[88231]","flags":["O_RDONLY"],"fd":0,"ino":88231,"dev":"0:12","inherited":true}
{"event":"OPEN","ts":1711814400.102,"pid":2002,"tgid":2002,"ppid":2000,"nspid":2002,"nstgid":2002,"path":"/tmp/b","flags":["O_WRONLY","O_CREAT","O_TRUNC"],"fd":1,"ino":91002,"dev":"259:2","inherited":true}
```

---

## Event: `CWD`

Emitted when a process's working directory changes. This occurs:

1. Immediately before every `EXEC` event (captures the initial cwd of the new program).
2. On successful `chdir()` / `fchdir()` calls.

| Field  | Type           | Description |
|--------|----------------|-------------|
| `path` | string ∣ null  | Absolute path of the new working directory |

### Example

```json
{"event":"CWD","ts":1711814400.122,"pid":1234,"tgid":1234,"ppid":1200,"nspid":1234,"nstgid":1234,"path":"/home/user/project"}
```

### Resolving relative OPEN paths

OPEN events may contain relative paths. To resolve them, find the most recent CWD event for the same `tgid` that precedes the OPEN's `ts`. The CWD path is guaranteed to be emitted before any EXEC, so every process has a known working directory from its first event onward.

---

## Event: `STDOUT`

Emitted when a tagged process writes to fd 1, but only if fd 1 points to the same terminal/pty as the session creator's stdout. Writes to redirected stdout (files, pipes) are not captured.

| Field  | Type   | Description |
|--------|--------|-------------|
| `len`  | int    | Bytes captured (capped at 4096 per write call) |
| `data` | string | Captured content, JSON-escaped |

### Example

```json
{"event":"STDOUT","ts":1711814400.500,"pid":1235,"tgid":1235,"ppid":1234,"nspid":1235,"nstgid":1235,"len":18,"data":"gcc -c -O2 main.c\n"}
```

---

## Event: `STDERR`

Emitted when a tagged process writes to fd 2.

On kernels 4.20+ (`ksys_write`): captures all fd==2 writes including to `/dev/null`.
On older kernels (`vfs_write` fallback): captures fd-2 writes via inode matching; `/dev/null` writes are filtered out.

| Field  | Type   | Description |
|--------|--------|-------------|
| `len`  | int    | Bytes captured (capped at 4096) |
| `data` | string | Captured content, JSON-escaped |

### Example

```json
{"event":"STDERR","ts":1711814400.600,"pid":1236,"tgid":1236,"ppid":1235,"nspid":1236,"nstgid":1236,"len":35,"data":"error: 'foo' undeclared in main.c\n"}
```

---

## Event Ordering

Within a single process (`tgid`), events are emitted in causal order:

1. `CWD` (initial working directory)
2. `EXEC` (the program starts)
3. `OPEN` with `"inherited":true` — one per fd the new program inherited from its predecessor (pipes, redirects, stdio, etc.)
4. `OPEN`, `CWD`, `STDOUT`, `STDERR` (during execution, in real-time order)
5. `EXIT` (process ends)

Across different processes, events are ordered by `ts` but may interleave. The ring buffer preserves per-session insertion order.
