//! A minimal, hand-written `wasi_snapshot_preview1` host for wasmi, over an
//! engine-owned virtual fd table. No `wasi-common`.
//!
//! Today every fd is an in-memory buffer (stdin = bytes we provide; stdout/stderr
//! = bytes we capture), so running a util does **zero syscalls** for its I/O.
//! The same [`Backing`] enum is where kernel fds, blob↔blob in-memory pipes, and
//! the box's logical streams will slot in. The stdio imports (`fd_read`/
//! `fd_write`) are the ones that can ever block, so they're the only candidates
//! for the asyncify suspend allowlist; everything else returns inline.

use std::collections::HashMap;
use wasmi::{Caller, Engine, Linker, Memory, Module, Store};

// ── WASI preview1 errno (only the ones we return) ───────────────────────────
const ERRNO_SUCCESS: i32 = 0;
const ERRNO_BADF: i32 = 8;
const ERRNO_NOTSUP: i32 = 58;
const ERRNO_SPIPE: i32 = 70;

// WASI filetype
const FILETYPE_CHARACTER_DEVICE: u8 = 2;

/// What an fd reads from / writes to. In-memory only today.
enum Backing {
    /// Readable byte source (stdin); `pos` is the read cursor.
    In { data: Vec<u8>, pos: usize },
    /// Writable sink (stdout/stderr); bytes are captured.
    Out(Vec<u8>),
}

/// proc_exit unwinds the guest by trapping with this carried code.
#[derive(Debug)]
struct ProcExit(i32);
impl std::fmt::Display for ProcExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "proc_exit({})", self.0)
    }
}
impl wasmi::errors::HostError for ProcExit {}

/// Per-instance host state: argv, env, the fd table, and the recorded exit code.
pub struct HostCtx {
    args: Vec<Vec<u8>>, // each NUL-free; NUL appended on write-out
    env: Vec<Vec<u8>>,
    fds: HashMap<u32, Backing>,
    exit_code: Option<i32>,
}

impl HostCtx {
    fn new(args: Vec<Vec<u8>>, env: Vec<Vec<u8>>, stdin: Vec<u8>) -> Self {
        let mut fds = HashMap::new();
        fds.insert(0, Backing::In { data: stdin, pos: 0 });
        fds.insert(1, Backing::Out(Vec::new()));
        fds.insert(2, Backing::Out(Vec::new()));
        Self { args, env, fds, exit_code: None }
    }
}

/// Result of a run: exit code + captured stdout/stderr.
pub struct RunOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

// ── small guest-memory helpers (sequence borrows; never hold two at once) ────
fn mem_of(caller: &Caller<HostCtx>) -> Memory {
    match caller.get_export("memory") {
        Some(wasmi::Extern::Memory(m)) => m,
        _ => panic!("guest has no exported `memory`"),
    }
}
fn read_u32(mem: &Memory, caller: &Caller<HostCtx>, off: u32) -> u32 {
    let mut b = [0u8; 4];
    mem.read(caller, off as usize, &mut b).unwrap();
    u32::from_le_bytes(b)
}
fn write_u32(mem: &Memory, caller: &mut Caller<HostCtx>, off: u32, v: u32) {
    mem.write(caller, off as usize, &v.to_le_bytes()).unwrap();
}
fn write_u64(mem: &Memory, caller: &mut Caller<HostCtx>, off: u32, v: u64) {
    mem.write(caller, off as usize, &v.to_le_bytes()).unwrap();
}

/// Register all preview1 imports the coreutils blob references. The stdio paths
/// are real; filesystem ops the stream utils never hit are honest errno stubs.
fn link(linker: &mut Linker<HostCtx>) {
    let m = "wasi_snapshot_preview1";

    // proc_exit(code) → trap carrying the code.
    linker.func_wrap(m, "proc_exit", |mut caller: Caller<HostCtx>, code: i32| -> Result<(), wasmi::Error> {
        caller.data_mut().exit_code = Some(code);
        Err(wasmi::Error::host(ProcExit(code)))
    }).unwrap();

    // sched_yield() → no-op success.
    linker.func_wrap(m, "sched_yield", |_: Caller<HostCtx>| -> i32 { ERRNO_SUCCESS }).unwrap();

    // random_get(buf, len) → fill (deterministic; randomness isn't load-bearing here).
    linker.func_wrap(m, "random_get", |mut caller: Caller<HostCtx>, buf: i32, len: i32| -> i32 {
        let mem = mem_of(&caller);
        let bytes = vec![0u8; len.max(0) as usize];
        mem.write(&mut caller, buf as usize, &bytes).unwrap();
        ERRNO_SUCCESS
    }).unwrap();

    // clock_time_get(id, precision, retptr) → fixed timestamp (never blocks).
    linker.func_wrap(m, "clock_time_get", |mut caller: Caller<HostCtx>, _id: i32, _prec: i64, retptr: i32| -> i32 {
        let mem = mem_of(&caller);
        write_u64(&mem, &mut caller, retptr as u32, 0);
        ERRNO_SUCCESS
    }).unwrap();

    // args_sizes_get(argc_ptr, bufsize_ptr)
    linker.func_wrap(m, "args_sizes_get", |mut caller: Caller<HostCtx>, argc_ptr: i32, buf_ptr: i32| -> i32 {
        let mem = mem_of(&caller);
        let (argc, bufsize) = {
            let ctx = caller.data();
            (ctx.args.len() as u32, ctx.args.iter().map(|a| a.len() as u32 + 1).sum::<u32>())
        };
        write_u32(&mem, &mut caller, argc_ptr as u32, argc);
        write_u32(&mem, &mut caller, buf_ptr as u32, bufsize);
        ERRNO_SUCCESS
    }).unwrap();

    // args_get(argv_ptr, argv_buf_ptr)
    linker.func_wrap(m, "args_get", |mut caller: Caller<HostCtx>, argv_ptr: i32, buf_ptr: i32| -> i32 {
        let mem = mem_of(&caller);
        let args = caller.data().args.clone();
        let mut p = argv_ptr as u32;
        let mut b = buf_ptr as u32;
        for a in &args {
            write_u32(&mem, &mut caller, p, b);
            p += 4;
            mem.write(&mut caller, b as usize, a).unwrap();
            mem.write(&mut caller, b as usize + a.len(), &[0u8]).unwrap();
            b += a.len() as u32 + 1;
        }
        ERRNO_SUCCESS
    }).unwrap();

    // environ_sizes_get / environ_get (mirror args)
    linker.func_wrap(m, "environ_sizes_get", |mut caller: Caller<HostCtx>, c_ptr: i32, buf_ptr: i32| -> i32 {
        let mem = mem_of(&caller);
        let (c, bufsize) = {
            let ctx = caller.data();
            (ctx.env.len() as u32, ctx.env.iter().map(|e| e.len() as u32 + 1).sum::<u32>())
        };
        write_u32(&mem, &mut caller, c_ptr as u32, c);
        write_u32(&mem, &mut caller, buf_ptr as u32, bufsize);
        ERRNO_SUCCESS
    }).unwrap();
    linker.func_wrap(m, "environ_get", |mut caller: Caller<HostCtx>, env_ptr: i32, buf_ptr: i32| -> i32 {
        let mem = mem_of(&caller);
        let env = caller.data().env.clone();
        let mut p = env_ptr as u32;
        let mut b = buf_ptr as u32;
        for e in &env {
            write_u32(&mem, &mut caller, p, b);
            p += 4;
            mem.write(&mut caller, b as usize, e).unwrap();
            mem.write(&mut caller, b as usize + e.len(), &[0u8]).unwrap();
            b += e.len() as u32 + 1;
        }
        ERRNO_SUCCESS
    }).unwrap();

    // fd_fdstat_get(fd, retptr): report a character device for known fds.
    linker.func_wrap(m, "fd_fdstat_get", |mut caller: Caller<HostCtx>, fd: i32, retptr: i32| -> i32 {
        if !caller.data().fds.contains_key(&(fd as u32)) { return ERRNO_BADF; }
        let mem = mem_of(&caller);
        // fdstat: filetype@0 (u8), flags@2 (u16), rights_base@8 (u64), rights_inh@16 (u64)
        let mut buf = [0u8; 24];
        buf[0] = FILETYPE_CHARACTER_DEVICE;
        mem.write(&mut caller, retptr as usize, &buf).unwrap();
        ERRNO_SUCCESS
    }).unwrap();

    // fd_prestat_get: no preopens → BADF for every fd (ends std's preopen scan).
    linker.func_wrap(m, "fd_prestat_get", |_: Caller<HostCtx>, _fd: i32, _retptr: i32| -> i32 { ERRNO_BADF }).unwrap();
    linker.func_wrap(m, "fd_prestat_dir_name", |_: Caller<HostCtx>, _fd: i32, _p: i32, _l: i32| -> i32 { ERRNO_BADF }).unwrap();

    // fd_seek: stdio is a pipe → ESPIPE.
    linker.func_wrap(m, "fd_seek", |_: Caller<HostCtx>, _fd: i32, _off: i64, _whence: i32, _retptr: i32| -> i32 { ERRNO_SPIPE }).unwrap();

    // fd_close: drop the backing.
    linker.func_wrap(m, "fd_close", |mut caller: Caller<HostCtx>, fd: i32| -> i32 {
        if caller.data_mut().fds.remove(&(fd as u32)).is_some() { ERRNO_SUCCESS } else { ERRNO_BADF }
    }).unwrap();

    // fd_write(fd, iovs, iovs_len, nwritten_ptr) — gather from guest mem → backing.
    linker.func_wrap(m, "fd_write", |mut caller: Caller<HostCtx>, fd: i32, iovs: i32, iovs_len: i32, nw_ptr: i32| -> i32 {
        let mem = mem_of(&caller);
        // gather (read-only borrow), into an owned buffer
        let mut data: Vec<u8> = Vec::new();
        for i in 0..iovs_len as u32 {
            let base = iovs as u32 + i * 8;
            let ptr = read_u32(&mem, &caller, base);
            let len = read_u32(&mem, &caller, base + 4) as usize;
            let mut chunk = vec![0u8; len];
            mem.read(&caller, ptr as usize, &mut chunk).unwrap();
            data.extend_from_slice(&chunk);
        }
        let n = data.len() as u32;
        match caller.data_mut().fds.get_mut(&(fd as u32)) {
            Some(Backing::Out(buf)) => buf.extend_from_slice(&data),
            Some(Backing::In { .. }) | None => return ERRNO_BADF,
        }
        write_u32(&mem, &mut caller, nw_ptr as u32, n);
        ERRNO_SUCCESS
    }).unwrap();

    // fd_read(fd, iovs, iovs_len, nread_ptr) — scatter from backing → guest mem.
    linker.func_wrap(m, "fd_read", |mut caller: Caller<HostCtx>, fd: i32, iovs: i32, iovs_len: i32, nr_ptr: i32| -> i32 {
        let mem = mem_of(&caller);
        // collect iov (ptr,len) targets first (read-only borrow)
        let mut targets: Vec<(u32, usize)> = Vec::new();
        for i in 0..iovs_len as u32 {
            let base = iovs as u32 + i * 8;
            let ptr = read_u32(&mem, &caller, base);
            let len = read_u32(&mem, &caller, base + 4) as usize;
            targets.push((ptr, len));
        }
        // pull bytes from the backing
        let mut produced: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut total = 0u32;
        {
            let backing = match caller.data_mut().fds.get_mut(&(fd as u32)) {
                Some(b) => b,
                None => return ERRNO_BADF,
            };
            match backing {
                Backing::In { data, pos } => {
                    for (ptr, len) in targets {
                        let avail = data.len().saturating_sub(*pos);
                        let take = avail.min(len);
                        if take == 0 { break; }
                        let slice = data[*pos..*pos + take].to_vec();
                        *pos += take;
                        total += take as u32;
                        produced.push((ptr, slice));
                    }
                }
                Backing::Out(_) => return ERRNO_BADF,
            }
        }
        for (ptr, bytes) in produced {
            mem.write(&mut caller, ptr as usize, &bytes).unwrap();
        }
        write_u32(&mem, &mut caller, nr_ptr as u32, total);
        ERRNO_SUCCESS
    }).unwrap();

    // ── filesystem ops the stream utils never hit: honest stubs ─────────────
    for name in [
        "fd_filestat_get",
        "fd_filestat_set_size",
        "fd_readdir",
        "path_filestat_get",
        "path_open",
        "path_readlink",
        "path_create_directory",
        "path_remove_directory",
        "path_unlink_file",
    ] {
        // Each has a different arity; wrap with the matching shape returning NOTSUP.
        match name {
            "fd_filestat_get" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i32| ERRNO_NOTSUP).unwrap(),
            "fd_filestat_set_size" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i64| ERRNO_NOTSUP).unwrap(),
            "fd_readdir" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i32, _: i32, _: i64, _: i32| ERRNO_NOTSUP).unwrap(),
            "path_filestat_get" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i32, _: i32, _: i32, _: i32| ERRNO_NOTSUP).unwrap(),
            "path_open" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i32, _: i32, _: i32, _: i32, _: i64, _: i64, _: i32, _: i32| ERRNO_NOTSUP).unwrap(),
            "path_readlink" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i32, _: i32, _: i32, _: i32, _: i32| ERRNO_NOTSUP).unwrap(),
            "path_create_directory" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i32, _: i32| ERRNO_NOTSUP).unwrap(),
            "path_remove_directory" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i32, _: i32| ERRNO_NOTSUP).unwrap(),
            "path_unlink_file" => linker.func_wrap(m, name, |_: Caller<HostCtx>, _: i32, _: i32, _: i32| ERRNO_NOTSUP).unwrap(),
            _ => unreachable!(),
        };
    }
}

/// Run a wasm blob's `_start` with `argv`/`env`/`stdin`, capturing stdout/stderr.
/// argv[0] should be the applet name; the blob dispatches on it.
pub fn run_blob(
    wasm: &[u8],
    argv: &[&str],
    env: &[&str],
    stdin: Vec<u8>,
) -> Result<RunOutput, wasmi::Error> {
    let engine = Engine::default();
    let module = Module::new(&engine, wasm)?;
    let args = argv.iter().map(|s| s.as_bytes().to_vec()).collect();
    let env = env.iter().map(|s| s.as_bytes().to_vec()).collect();
    let mut store = Store::new(&engine, HostCtx::new(args, env, stdin));
    let mut linker: Linker<HostCtx> = Linker::new(&engine);
    link(&mut linker);

    let instance = linker.instantiate_and_start(&mut store, &module)?;
    let start = instance.get_typed_func::<(), ()>(&store, "_start")?;
    let run_res = start.call(&mut store, ());

    let code = match &run_res {
        Ok(()) => 0,
        Err(e) => {
            if let Some(ProcExit(c)) = e.downcast_ref::<ProcExit>() {
                *c
            } else {
                return Err(run_res.unwrap_err());
            }
        }
    };
    let ctx = store.data();
    let take_out = |fd: u32| match ctx.fds.get(&fd) {
        Some(Backing::Out(b)) => b.clone(),
        _ => Vec::new(),
    };
    Ok(RunOutput { code, stdout: take_out(1), stderr: take_out(2) })
}
