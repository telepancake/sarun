// sud-backed boxes, step 1 (see engine/DESIGN-sud.md — WORK IN PROGRESS).
// The box ran under tv's sudtrace with a plain directory upper overlaid on
// `/`; this module sweeps that upper directory into the box's sqlar
// BoxState after the command exits, so review/apply/discard/UI work on a
// sud box exactly as on a FUSE box. Post-exit sweep = final state only:
// every row is attributed to the runner's process row until the wire trace
// stream is ingested (step 2).

use crate::depot::BoxDepot;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::capture::BoxState;
use crate::depot::blob_path;
use crate::sudwire;

// ── live trace streaming (step 2) ───────────────────────────────────────────
// The runner ships the read end of the fd-1023 pipe with register; the
// engine consumes the TRACE stream as the box runs: EXEC events snapshot
// each process row from /proc WHILE THE PROCESS IS ALIVE (writer_for),
// OPEN-for-write events build the rel→writer map the post-exit sweep uses
// for per-file attribution, STDOUT/STDERR events land in the box's
// outputs table, and every byte is teed to live/<id>/sud.trace at rest.

/// Per-box streaming state, registered while a sud box runs.
pub struct Stream {
    /// rel path → process row id of the last writer seen opening it.
    pub writers: Mutex<HashMap<String, i64>>,
    /// tgid → (ts_ns, exe) of an EXEC whose /proc had already vanished;
    /// the following EV_ARGV completes it into an event-minted row.
    pub pending_exec: Mutex<HashMap<i32, (i64, String)>>,
    /// tgid → process row id, resolved once per incarnation. writer_for
    /// re-reads 4 /proc files per call and a -j30 build emits an OPEN per
    /// written file — at that rate the reader thread becomes the box-wide
    /// bottleneck (the trace pipe fills and every traced syscall waits on
    /// it). Safe against pid reuse because the wire is totally ordered:
    /// the EV_EXIT that frees a pid removes it here before any event from
    /// its successor arrives, and an in-place execve keeps its row id.
    pub pid_rows: Mutex<HashMap<i32, i64>>,
    /// Aggregated EV_PROF data: (elf class, syscall nr) -> (count, cycles).
    /// nr 0xFFFFFFFE = handler overflow bucket, 0xFFFFFFFF = wire wait.
    pub prof: Mutex<HashMap<(u32, u32), (u64, u64)>>,
    /// Reader-side drain stats: (bytes, events, nanoseconds spent applying).
    /// Backpressure diagnosis: apply-time ~ box wall time means the box ran
    /// at the READER's speed, not its own.
    pub drain: Mutex<(u64, u64, u64, Option<std::time::Instant>)>,
    /// Pipe hit EOF and every buffered event was applied.
    done: (Mutex<bool>, Condvar),
}

static STREAMS: OnceLock<Mutex<HashMap<i64, Arc<Stream>>>> = OnceLock::new();

fn streams() -> &'static Mutex<HashMap<i64, Arc<Stream>>> {
    STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Decode a raw sud TRACE stream (the `sudtrace` blob) into JSON event rows
/// for the `sudtrace` control verb / the UI's Trace pane. Each row is
/// `{ts_ns, kind, pid, tgid, ppid, extras, text}` where `kind` names the
/// event type (numeric string for anything unknown) and `text` is the blob
/// rendered lossy-UTF8 — argv/env/cwd/open paths and stdout/stderr bytes —
/// truncated at 4 KiB with a "… (N bytes)" suffix so one huge write can't
/// bloat the reply. Capped at the first `CAP` events with a `truncated`
/// flag so a giant trace can't wedge the UI.
pub fn decode_trace(bytes: &[u8]) -> serde_json::Value {
    use serde_json::json;
    const CAP: usize = 20_000;
    const TEXT_MAX: usize = 4096;
    let mut dec = sudwire::Decoder::default();
    let events = dec.feed(bytes);
    let truncated = events.len() > CAP;
    let render_text = |blob: &[u8]| -> String {
        if blob.len() > TEXT_MAX {
            let head = String::from_utf8_lossy(&blob[..TEXT_MAX]);
            format!("{head}… ({} bytes)", blob.len())
        } else {
            String::from_utf8_lossy(blob).into_owned()
        }
    };
    let rows: Vec<serde_json::Value> = events.iter().take(CAP).map(|e| {
        let kind = match e.ty {
            sudwire::EV_EXEC => "EXEC".to_string(),
            sudwire::EV_ARGV => "ARGV".to_string(),
            sudwire::EV_ENV => "ENV".to_string(),
            sudwire::EV_OPEN => "OPEN".to_string(),
            sudwire::EV_CWD => "CWD".to_string(),
            sudwire::EV_STDOUT => "STDOUT".to_string(),
            sudwire::EV_STDERR => "STDERR".to_string(),
            sudwire::EV_EXIT => "EXIT".to_string(),
            sudwire::EV_PROF => "PROF".to_string(),
            other => other.to_string(),
        };
        json!({
            "ts_ns": e.ts_ns, "kind": kind,
            "pid": e.pid, "tgid": e.tgid, "ppid": e.ppid,
            "extras": e.extras, "text": render_text(&e.blob),
        })
    }).collect();
    json!({"ok": true, "events": rows, "truncated": truncated})
}

/// Take the box's stream state (if the runner streamed a trace), waiting
/// up to 5 s for the pipe to drain — the runner closes its fd 1023 before
/// asking for the sweep, so EOF beats the sud_ingest verb in the normal
/// flow; the timeout only guards a wedged reader.
pub fn take_stream(box_id: i64) -> Option<Arc<Stream>> {
    let s = streams().lock().unwrap().remove(&box_id)?;
    let (lock, cv) = &s.done;
    let mut g = lock.lock().unwrap();
    let deadline = std::time::Duration::from_secs(5);
    while !*g {
        let (ng, timeout) = cv.wait_timeout(g, deadline).unwrap();
        g = ng;
        if timeout.timed_out() { break; }
    }
    drop(g);
    Some(s)
}

/// What a post-exit sweep folded into the box's sqlar: rows written and the
/// per-path errors that didn't ingest (empty = a clean sweep).
pub struct SweepReport {
    pub ingested: usize,
    pub errors: Vec<String>,
}

/// Sweep a finished sud box's captured state into its sqlar BoxState — the
/// whole body behind the `sud_ingest` verb. Owns three sources: the overlay
/// upper dir (`live/<id>/sud-up`), the inramfs /tmp store (keyed by the
/// `sud_ir_key` meta), and the durable TRACE stream (`live/<id>/sud.trace`,
/// folded into the sqlar so the `sudtrace` verb reads it there). `runpid` is
/// the runner's host pid, the fallback writer for anything the trace stream
/// didn't attribute. On a CLEAN sweep the upper dir + live trace file are
/// pure residue (the sqlar is authoritative from here — reruns recreate it,
/// nested launches export from it), so they're removed; on an errored sweep
/// they're kept as the sole copy of whatever failed to ingest.
/// Human name for a syscall nr per ELF class (the hot ones; anything else
/// prints as a number). i386 and x86_64 number the table differently.
fn syscall_name(class: u32, nr: u32) -> Option<&'static str> {
    let n64 = |nr: u32| Some(match nr {
        0 => "read", 1 => "write", 2 => "open", 3 => "close", 4 => "stat",
        5 => "fstat", 6 => "lstat", 8 => "lseek", 9 => "mmap",
        10 => "mprotect", 11 => "munmap", 12 => "brk", 13 => "rt_sigaction",
        14 => "rt_sigprocmask", 16 => "ioctl", 17 => "pread64",
        18 => "pwrite64", 19 => "readv", 20 => "writev", 21 => "access",
        22 => "pipe", 32 => "dup", 33 => "dup2", 39 => "getpid",
        56 => "clone", 57 => "fork", 58 => "vfork", 59 => "execve",
        60 => "exit", 61 => "wait4", 72 => "fcntl", 79 => "getcwd",
        80 => "chdir", 82 => "rename", 83 => "mkdir", 84 => "rmdir",
        86 => "link", 87 => "unlink", 88 => "symlink", 89 => "readlink",
        90 => "chmod", 110 => "getppid", 186 => "gettid", 202 => "futex",
        217 => "getdents64", 228 => "clock_gettime", 231 => "exit_group",
        257 => "openat", 262 => "newfstatat", 263 => "unlinkat",
        265 => "linkat", 266 => "symlinkat", 267 => "readlinkat",
        269 => "faccessat", 292 => "dup3", 293 => "pipe2",
        316 => "renameat2", 332 => "statx", 435 => "clone3",
        439 => "faccessat2", _ => return None,
    });
    let n32 = |nr: u32| Some(match nr {
        1 => "exit", 2 => "fork", 3 => "read", 4 => "write", 5 => "open",
        6 => "close", 7 => "waitpid", 9 => "link", 10 => "unlink",
        11 => "execve", 12 => "chdir", 15 => "chmod", 19 => "lseek",
        20 => "getpid", 33 => "access", 38 => "rename", 39 => "mkdir",
        40 => "rmdir", 41 => "dup", 42 => "pipe", 45 => "brk",
        54 => "ioctl", 55 => "fcntl", 63 => "dup2", 64 => "getppid",
        83 => "symlink", 85 => "readlink", 91 => "munmap",
        114 => "wait4", 120 => "clone", 125 => "mprotect",
        140 => "_llseek", 145 => "readv", 146 => "writev", 168 => "poll",
        174 => "rt_sigaction", 175 => "rt_sigprocmask", 180 => "pread64",
        181 => "pwrite64", 183 => "getcwd", 190 => "vfork", 192 => "mmap2",
        195 => "stat64", 196 => "lstat64", 197 => "fstat64",
        220 => "getdents64", 221 => "fcntl64", 224 => "gettid",
        240 => "futex", 252 => "exit_group", 265 => "clock_gettime",
        295 => "openat", 300 => "fstatat64", 301 => "unlinkat",
        302 => "renameat", 303 => "linkat", 304 => "symlinkat",
        305 => "readlinkat", 307 => "faccessat", 330 => "dup3",
        331 => "pipe2", 353 => "renameat2", 383 => "statx",
        _ => return None,
    });
    if class == 64 { n64(nr) } else { n32(nr) }
}

/// Print the box's syscall-cost profile + reader drain stats to the engine
/// stderr at sweep time: WHERE a slow box's time went — which syscalls, how
/// much of it was waiting on the trace wire (engine backpressure), and what
/// fraction of the box's wall the reader spent applying events.
fn report_profile(id: i64, st: &Stream) {
    let prof = st.prof.lock().unwrap();
    if prof.is_empty() { return; }
    let total_cycles: u64 = prof.values().map(|v| v.1).sum();
    if total_cycles == 0 { return; }
    let mut rows: Vec<((u32, u32), (u64, u64))> =
        prof.iter().map(|(k, v)| (*k, *v)).collect();
    drop(prof);
    rows.sort_by_key(|r| std::cmp::Reverse(r.1 .1));
    eprintln!("sarun-engine: box {id} sud syscall profile \
               (handler rdtsc cycles; top offenders):");
    for ((class, nr), (count, cycles)) in rows.iter().take(14) {
        let pct = *cycles as f64 * 100.0 / total_cycles as f64;
        let name = match *nr {
            0xFFFF_FFFF => "[trace-wire wait]".to_string(),
            0xFFFF_FFFE => format!("[nr>=512/{class}]"),
            n => match syscall_name(*class, n) {
                Some(s) => format!("{s}/{class}"),
                None => format!("nr{n}/{class}"),
            },
        };
        eprintln!("sarun-engine:   {name:<24} {count:>10} calls  \
                   {cycles:>14} cy  {pct:5.1}%");
    }
    let (bytes, events, apply_ns, start) = {
        let d = st.drain.lock().unwrap();
        (d.0, d.1, d.2, d.3)
    };
    let wall = start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    let apply = apply_ns as f64 / 1e9;
    let busy = if wall > 0.0 { apply * 100.0 / wall } else { 0.0 };
    eprintln!("sarun-engine:   reader: {events} events, {:.1} MB; \
               apply {apply:.1}s over {wall:.1}s box wall ({busy:.0}% busy \
               — near 100% means the box ran at the ENGINE's speed)",
              bytes as f64 / 1e6);
}

pub fn sweep(b: &BoxState, id: i64, runpid: u32) -> SweepReport {
    let live = crate::paths::live_home().join(id.to_string());
    let upper = live.join("sud-up");
    // take_stream waits for the trace pipe to drain, so the rel→writer
    // attribution map is complete before the upper is swept.
    let stream = take_stream(id);
    if let Some(s) = &stream {
        report_profile(id, s);
    }
    let writers = stream
        .map(|s| s.writers.lock().unwrap().clone())
        .unwrap_or_default();
    let (mut ingested, mut errors) = ingest_upper(b, &upper, runpid, &writers);
    // /tmp lives in the inramfs shared-memory store, not the upper dir —
    // ingest it under the same rel→writer attribution and drop its shms.
    if let Some(key) = b.get_meta("sud_ir_key").filter(|k| !k.is_empty()) {
        let fallback = if runpid > 0 { b.writer_for(runpid) } else { 0 };
        let (n, mut errs) = ingest_inramfs(b, &key, fallback, &writers);
        ingested += n;
        errors.append(&mut errs);
    }
    let trace_path = live.join("sud.trace");
    if let Ok(bytes) = std::fs::read(&trace_path) {
        b.set_sudtrace(&bytes);
    }
    if errors.is_empty() {
        let _ = std::fs::remove_dir_all(&upper);
        let _ = std::fs::remove_file(&trace_path);
    }
    SweepReport { ingested, errors }
}

/// Decode a box's durable TRACE blob to JSON event rows for the `sudtrace`
/// verb / the UI Trace pane. Prefers the live BoxState's own connection when
/// the box is running (no rival on-disk handle racing serve); else opens the
/// at-rest sqlar. A box with no trace (every FUSE box) answers a clean error.
pub fn trace_events_json(live: Option<Arc<BoxState>>, id: i64)
                         -> serde_json::Value {
    let blob = match live {
        Some(b) => b.get_sudtrace(),
        None => BoxState::create(id).ok().and_then(|b| b.get_sudtrace()),
    };
    match blob {
        Some(bytes) => decode_trace(&bytes),
        None => serde_json::json!({"ok": false,
                                   "error": "box has no sud trace"}),
    }
}

/// Spawn the reader thread for one sud box: tee `fd` (pipe read end,
/// owned here) into `trace_path` and apply events to `b` as they arrive.
pub fn stream_events(box_id: i64, fd: i32, b: Arc<BoxState>,
                     trace_path: std::path::PathBuf) {
    let st = Arc::new(Stream {
        writers: Mutex::new(HashMap::new()),
        pending_exec: Mutex::new(HashMap::new()),
        pid_rows: Mutex::new(HashMap::new()),
        prof: Mutex::new(HashMap::new()),
        drain: Mutex::new((0, 0, 0, None)),
        done: (Mutex::new(false), Condvar::new()),
    });
    streams().lock().unwrap().insert(box_id, st.clone());
    std::thread::spawn(move || {
        use std::io::Write;
        let mut tee = std::fs::File::create(&trace_path).ok();
        let mut dec = sudwire::Decoder::default();
        // per-tgid logical cwd (from EV_CWD) for resolving relative
        // OPEN paths; dirfd-relative opens stay unresolved (fallback
        // attribution applies).
        let mut cwds: HashMap<i32, String> = HashMap::new();
        let mut buf = [0u8; 65536];
        // If applying events ever panics, this thread MUST NOT die with
        // the pipe still open: nothing else drains it, it fills at 64 KiB,
        // and from then on every traced syscall in the box blocks in its
        // trace write — the whole build freezes while the engine looks
        // healthy. Degrade to drain+tee (the sweep still ingests final
        // state) and say so loudly.
        let mut apply_dead = false;
        loop {
            let n = unsafe {
                libc::read(fd, buf.as_mut_ptr().cast(), buf.len())
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) { continue; }
                break;
            }
            if n == 0 { break; }
            if let Some(t) = tee.as_mut() {
                let _ = t.write_all(&buf[..n as usize]);
            }
            if apply_dead { continue; }
            let t0 = std::time::Instant::now();
            let mut n_events = 0u64;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for ev in dec.feed(&buf[..n as usize]) {
                    apply_event(&b, &st, &mut cwds, &ev);
                    n_events += 1;
                }
            }));
            {
                let mut d = st.drain.lock().unwrap();
                if d.3.is_none() { d.3 = Some(t0); }
                d.0 += n as u64;
                d.1 += n_events;
                d.2 += t0.elapsed().as_nanos() as u64;
            }
            if r.is_err() {
                apply_dead = true;
                eprintln!("sarun-engine: sud trace apply PANICKED for box \
                           {box_id}; live process/output capture stops here \
                           (the pipe keeps draining so the box keeps \
                           running; the post-exit sweep still ingests \
                           final state)");
            }
        }
        unsafe { libc::close(fd); }
        let (lock, cv) = &st.done;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    });
}

/// Write-intent test on OPEN flags: any access mode beyond O_RDONLY, or
/// creation/truncation.
fn open_writes(flags: i64) -> bool {
    let f = flags as i32;
    (f & libc::O_ACCMODE) != libc::O_RDONLY
        || f & (libc::O_CREAT | libc::O_TRUNC) != 0
}

/// Paths the runner carves out of the overlay (rule order in run_sud) —
/// writes there never reach the upper, so don't attribute them.
fn is_passthrough(abs: &str) -> bool {
    // /tmp is NOT here: it's the box's inramfs mount, captured at sweep,
    // so OPEN events under it feed attribution like any overlay path.
    for p in ["/proc/", "/dev/", "/sys/"] {
        if abs.starts_with(p) { return true; }
    }
    abs.starts_with(&*crate::paths::state_home().to_string_lossy())
        || abs.starts_with(&*crate::paths::mnt_point().to_string_lossy())
}

fn apply_event(b: &BoxState, st: &Stream,
               cwds: &mut HashMap<i32, String>, ev: &sudwire::Event) {
    // Test hook: a real apply panic must DEGRADE the reader (drain-only),
    // never wedge the box — see the catch_unwind in stream_events. Gated
    // on an env var nothing sets outside that test.
    static TEST_PANIC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *TEST_PANIC.get_or_init(|| {
        std::env::var_os("SARUN_TEST_APPLY_PANIC").is_some()
    }) && ev.ty == sudwire::EV_OPEN {
        panic!("SARUN_TEST_APPLY_PANIC");
    }
    match ev.ty {
        sudwire::EV_EXEC => {
            // Snapshot the process row while /proc/<tgid> is alive —
            // this is what post-exit sweeps structurally can't do. An
            // in-place execve (vendor `exec real-tool "$@"` wrappers)
            // keeps (tgid,start), so refresh the existing row's image
            // rather than trusting the first snapshot (capture.rs
            // exec_refresh) — otherwise the process table shows the
            // wrapper/shim forever and the real compilers never appear.
            // A pid gone before the event arrives (echo/as-sized tools
            // finish inside the pipe latency) minted NO row at all:
            // stash the event's exe and let the EV_ARGV right behind it
            // complete an event-data row instead.
            if ev.tgid > 0 {
                let exe = String::from_utf8_lossy(&ev.blob).into_owned();
                match b.exec_refresh(ev.tgid as u32, &exe) {
                    Some(rid) => {
                        st.pid_rows.lock().unwrap().insert(ev.tgid, rid);
                    }
                    None => {
                        st.pending_exec.lock().unwrap()
                            .insert(ev.tgid, (ev.ts_ns as i64, exe));
                    }
                }
            }
        }
        sudwire::EV_ARGV => {
            let pend = st.pending_exec.lock().unwrap().remove(&ev.tgid);
            if let Some((ts, exe)) = pend {
                let argv: Vec<String> = ev.blob.split(|&b| b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect();
                let cwd = cwds.get(&ev.tgid).cloned().unwrap_or_default();
                if let Some(rid) = b.record_proc_event(ev.tgid as u32,
                                                       ev.ppid as u32,
                                                       ts, &exe, &cwd, &argv) {
                    st.pid_rows.lock().unwrap().insert(ev.tgid, rid);
                }
            }
        }
        sudwire::EV_CWD => {
            if let Ok(p) = String::from_utf8(ev.blob.clone()) {
                cwds.insert(ev.tgid, p);
            }
        }
        sudwire::EV_OPEN => {
            // extras = {flags, fd, ino, dev_major, dev_minor, err, inh}
            let [flags, _, _, _, _, err, inh] = ev.extras[..] else {
                return;
            };
            if err != 0 || inh != 0 || !open_writes(flags) { return; }
            let Ok(path) = std::str::from_utf8(&ev.blob) else { return };
            let abs = if path.starts_with('/') {
                path.to_string()
            } else if let Some(cwd) = cwds.get(&ev.tgid) {
                format!("{}/{}", cwd.trim_end_matches('/'), path)
            } else {
                return; // relative with unknown cwd — fallback applies
            };
            if is_passthrough(&abs) { return; }
            let rel = abs.trim_start_matches('/').to_string();
            if rel.is_empty() { return; }
            let cached = st.pid_rows.lock().unwrap().get(&ev.tgid).copied();
            let w = match cached {
                Some(rid) => rid,
                None => {
                    let rid = b.writer_for(ev.tgid as u32);
                    st.pid_rows.lock().unwrap().insert(ev.tgid, rid);
                    rid
                }
            };
            st.writers.lock().unwrap().insert(rel, w);
        }
        // Match the FUSE sink numbering in overlay.rs: stdout = 0, stderr
        // = 1 (NOT the fd numbers) so the outputs table is backend-identical.
        sudwire::EV_STDOUT => b.add_output(0, ev.tgid as u32, &ev.blob),
        sudwire::EV_STDERR => b.add_output(1, ev.tgid as u32, &ev.blob),
        sudwire::EV_EXIT => {
            // The pid is free for kernel reuse from here — drop its row
            // binding so a successor with the same pid re-resolves.
            st.pid_rows.lock().unwrap().remove(&ev.tgid);
            cwds.remove(&ev.tgid);
        }
        sudwire::EV_PROF => {
            // blob: LE u32 elf class, then {u32 nr, u32 count, u64 cycles}
            // triples (see trace.h). Aggregate across the box's processes.
            if ev.blob.len() >= 4 {
                let u32le = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let class = u32le(&ev.blob);
                let mut p = st.prof.lock().unwrap();
                for tri in ev.blob[4..].chunks_exact(16) {
                    let nr = u32le(&tri[0..4]);
                    let count = u32le(&tri[4..8]) as u64;
                    let cycles = u64::from_le_bytes(tri[8..16].try_into().unwrap());
                    let e = p.entry((class, nr)).or_insert((0, 0));
                    e.0 += count;
                    e.1 += cycles;
                }
            }
        }
        _ => {}
    }
}

// ── layer registry ──────────────────────────────────────────────────────────
// Each sud box's register-time layer list (upper first, then lowers,
// host implied at the bottom). A nested launch under a RUNNING ancestor
// flattens against this — the live upper dir + everything it saw below.

static LAYERS: OnceLock<Mutex<HashMap<i64, Vec<String>>>> = OnceLock::new();

fn layer_map() -> &'static Mutex<HashMap<i64, Vec<String>>> {
    LAYERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_layers(box_id: i64, layers: Vec<String>) {
    layer_map().lock().unwrap().insert(box_id, layers);
}

pub fn layers(box_id: i64) -> Option<Vec<String>> {
    layer_map().lock().unwrap().get(&box_id).cloned()
}

// ── inramfs ingest ──────────────────────────────────────────────────────────

/// Mirror the box's inramfs /tmp store into the sqlar under `tmp/…`,
/// then drop the backing shm objects. Same row semantics as the upper
/// sweep; attribution comes from the same rel→writer map (OPEN events
/// under /tmp carry the visible path).
pub fn ingest_inramfs(b: &BoxState, key: &str, fallback: i64,
                      writers: &HashMap<String, i64>)
                      -> (usize, Vec<String>) {
    let shm_dir = std::path::Path::new("/dev/shm");
    let mut n = 0usize;
    let mut errs = vec![];
    let entries = match crate::sudir::read_store(shm_dir, key) {
        Ok(e) => e,
        Err(e) => { return (0, vec![format!("inramfs: {e}")]); }
    };
    if !entries.is_empty() {
        b.set_dir("tmp", 0o041777, fallback);
        n += 1;
    }
    for ent in &entries {
        let rel = format!("tmp/{}", ent.rel);
        let writer = writers.get(&rel).copied().unwrap_or(fallback);
        match &ent.kind {
            crate::sudir::IrKind::Dir { mode } => {
                b.set_dir(&rel, mode | 0o040000, writer);
                n += 1;
            }
            crate::sudir::IrKind::Symlink { target } => {
                b.set_symlink(&rel, target, writer);
                n += 1;
            }
            crate::sudir::IrKind::File { mode, data } => {
                let rowid = b.ensure_file_row(&rel, 0o100000 | mode, writer);
                let bp = blob_path(b.id, rowid);
                if let Some(parent) = bp.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&bp, data) {
                    Ok(()) => {
                        b.finalize_file(&rel, data.len() as i64,
                                        now_wall_ns(), writer);
                        n += 1;
                    }
                    Err(e) => errs.push(format!("{rel}: blob: {e}")),
                }
            }
        }
    }
    crate::sudir::unlink_store(shm_dir, key);
    (n, errs)
}

fn now_wall_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64).unwrap_or(0)
}

// ── nesting: same-in-same (sud under sud) ───────────────────────────────────
// Wrapper-in-wrapper is impossible (both wrappers link at one fixed text
// address, and the outer wrapper's execve interception would wrap the
// inner wrapper binary), so a nested sud box is FLATTENED: one wrapper
// invocation whose overlay rule stacks the child's upper over each
// ancestor's captured state over the host. Ancestor state is authoritative
// in the sqlar (apply/discard mutate it after the sweep), so the lower is
// MATERIALIZED from the BoxState — the stale sud-up directory is never
// used as a lower.

/// Materialize box `aid`'s at-rest captured state into `dest` as a sud
/// overlay lower: files hardlink (fall back to copy) from the blob pool,
/// whiteout rows become char-0:0 markers, dirs/symlinks/specials their
/// on-disk selves. Returns the entry count.
pub fn export_box(aid: i64, dest: &Path) -> Result<usize, String> {
    use std::os::unix::fs::PermissionsExt;
    let b = BoxState::create(aid).map_err(|e| format!("sqlar {aid}: {e}"))?;
    b.load_mirror();
    let _ = std::fs::remove_dir_all(dest); // stale from a prior nest run
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let kinds = b.kinds.read().unwrap();
    let mut rels: Vec<&String> = kinds.keys().collect();
    rels.sort(); // parents sort before children
    let mut n = 0usize;
    for rel in rels {
        let p = dest.join(rel);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cpath = || std::ffi::CString::new(
            p.as_os_str().as_encoded_bytes()).unwrap();
        match &kinds[rel] {
            crate::capture::Entry::Dir { mode, .. } => {
                let _ = std::fs::create_dir_all(&p);
                let _ = std::fs::set_permissions(&p,
                    std::fs::Permissions::from_mode(mode & 0o7777));
            }
            crate::capture::Entry::File { rowid, mode } => {
                let src = blob_path(aid, *rowid);
                let _ = std::fs::remove_file(&p);
                if std::fs::hard_link(&src, &p).is_err() {
                    std::fs::copy(&src, &p)
                        .map_err(|e| format!("{rel}: {e}"))?;
                }
                let _ = std::fs::set_permissions(&p,
                    std::fs::Permissions::from_mode(mode & 0o7777));
            }
            crate::capture::Entry::Symlink { target } => {
                let _ = std::fs::remove_file(&p);
                std::os::unix::fs::symlink(target, &p)
                    .map_err(|e| format!("{rel}: symlink: {e}"))?;
            }
            // A hole materializes nothing into the sud lower tree: the
            // backdrop shows through at runtime, not at materialization.
            crate::capture::Entry::Hole => {}
            crate::capture::Entry::Whiteout => {
                let _ = std::fs::remove_file(&p);
                if unsafe { libc::mknod(cpath().as_ptr(),
                                        libc::S_IFCHR, 0) } != 0 {
                    return Err(format!("{rel}: whiteout mknod: {}",
                        std::io::Error::last_os_error()));
                }
            }
            crate::capture::Entry::Special { mode, rdev } => {
                // Best-effort (fifos work unprivileged; devices may not).
                let _ = std::fs::remove_file(&p);
                let _ = unsafe {
                    libc::mknod(cpath().as_ptr(), *mode, *rdev as libc::dev_t)
                };
            }
        }
        n += 1;
    }
    Ok(n)
}

/// Walk `upper` (the sud overlay's upper directory) and mirror it into the
/// box's sqlar. Char-0:0 device nodes are the sud/overlayfs whiteout marker
/// and become whiteout rows. `writers` (from the trace stream) attributes
/// each path to the process that opened it for writing; anything unmatched
/// falls back to the runner's process row. Returns (rows written, errors).
pub fn ingest_upper(b: &BoxState, upper: &Path, runpid: u32,
                    writers: &HashMap<String, i64>)
                    -> (usize, Vec<String>) {
    let fallback = if runpid > 0 { b.writer_for(runpid) } else { 0 };
    let mut n = 0usize;
    let mut errs = Vec::new();
    walk(b, upper, "", fallback, writers, &mut n, &mut errs);
    (n, errs)
}

/// Read every extended attribute off the upper file at `p` (via
/// l*xattr, so symlinks aren't followed) and mirror it into the box's
/// sqlar. The box set these through the wrapper's intercepted setxattr,
/// which lands them as real xattrs on the upper file. `trusted.overlay.*`
/// is skipped defensively (a real-overlayfs internal namespace; sud uses
/// char-dev whiteouts, not xattrs, so it should never appear — but a host
/// file copied up could carry one).
fn capture_xattrs(b: &BoxState, p: &Path, rel: &str, errs: &mut Vec<String>) {
    let cpath = match std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) {
        Ok(c) => c, Err(_) => return,
    };
    let sz = unsafe {
        libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0)
    };
    if sz <= 0 { return; } // 0 = none, <0 = unsupported/gone: nothing to do
    let mut names = vec![0u8; sz as usize];
    let got = unsafe {
        libc::llistxattr(cpath.as_ptr(), names.as_mut_ptr().cast(),
                         names.len())
    };
    if got <= 0 { return; }
    names.truncate(got as usize);
    for key in names.split(|c| *c == 0).filter(|k| !k.is_empty()) {
        let Ok(kstr) = std::str::from_utf8(key) else { continue };
        if kstr.starts_with("trusted.overlay.") { continue; }
        let ckey = match std::ffi::CString::new(key) {
            Ok(c) => c, Err(_) => continue,
        };
        let vsz = unsafe {
            libc::lgetxattr(cpath.as_ptr(), ckey.as_ptr(),
                            std::ptr::null_mut(), 0)
        };
        if vsz < 0 { continue; }
        let mut val = vec![0u8; vsz as usize];
        let vgot = unsafe {
            libc::lgetxattr(cpath.as_ptr(), ckey.as_ptr(),
                            val.as_mut_ptr().cast(), val.len())
        };
        if vgot < 0 {
            errs.push(format!("{rel}: getxattr {kstr}: {}",
                              std::io::Error::last_os_error()));
            continue;
        }
        val.truncate(vgot as usize);
        b.set_xattr(rel, kstr, &val);
    }
}

fn walk(b: &BoxState, dir: &Path, rel: &str, fallback: i64,
        writers: &HashMap<String, i64>,
        n: &mut usize, errs: &mut Vec<String>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => { errs.push(format!("{}: {e}", dir.display())); return; }
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            errs.push(format!("{}: non-utf8 name", dir.display()));
            continue;
        };
        let crel = if rel.is_empty() { name.to_string() }
                   else { format!("{rel}/{name}") };
        let p = ent.path();
        let md = match p.symlink_metadata() {
            Ok(m) => m,
            Err(e) => { errs.push(format!("{crel}: {e}")); continue; }
        };
        let mode = md.mode();
        let ftype = md.file_type();
        let writer = writers.get(&crel).copied().unwrap_or(fallback);
        if ftype.is_dir() {
            b.set_dir(&crel, mode, writer);
            capture_xattrs(b, &p, &crel, errs);
            *n += 1;
            walk(b, &p, &crel, fallback, writers, n, errs);
        } else if ftype.is_symlink() {
            match std::fs::read_link(&p) {
                Ok(t) => { b.set_symlink(&crel, &t, writer); *n += 1; }
                Err(e) => errs.push(format!("{crel}: readlink: {e}")),
            }
        } else if mode & 0o170000 == 0o020000 && md.rdev() == 0 {
            // char 0:0 — the overlayfs whiteout marker sud's overlay uses.
            b.set_whiteout(&crel, writer);
            *n += 1;
        } else if ftype.is_file() {
            let rowid = b.ensure_file_row(&crel, mode, writer);
            let bp = blob_path(b.id, rowid);
            if let Some(parent) = bp.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::copy(&p, &bp) {
                Ok(sz) => {
                    let mtime_ns = md.mtime()
                        .saturating_mul(1_000_000_000)
                        .saturating_add(md.mtime_nsec());
                    b.finalize_file(&crel, sz as i64, mtime_ns, writer);
                    capture_xattrs(b, &p, &crel, errs);
                    *n += 1;
                }
                Err(e) => errs.push(format!("{crel}: blob copy: {e}")),
            }
        } else {
            // fifo / real device node.
            b.set_special(&crel, mode, md.rdev(), writer);
            *n += 1;
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::sudwire::{self, EvState};

    /// decode_trace renders an encoder-built stream into typed JSON rows:
    /// EXEC/OPEN/STDOUT/EXIT names, per-event text, extras verbatim.
    #[test]
    fn decode_trace_names_kinds_and_text() {
        let mut enc = EvState::default();
        let mut stream = sudwire::version_atom();
        stream.extend(enc.build_event(1, sudwire::EV_EXEC, 100, 9, 9, 1,
                                      9, 9, &[], b"/bin/sh"));
        stream.extend(enc.build_event(1, sudwire::EV_OPEN, 200, 9, 9, 1,
                                      9, 9, &[0o101, 3, 5, 1, 2, 0, 0],
                                      b"out.txt"));
        stream.extend(enc.build_event(1, sudwire::EV_STDOUT, 300, 9, 9, 1,
                                      9, 9, &[], b"hi\n"));
        stream.extend(enc.build_exit(1, 400, 9, 9, 1, 0));

        let v = decode_trace(&stream);
        assert_eq!(v["ok"], true);
        assert_eq!(v["truncated"], false);
        let rows = v["events"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["kind"], "EXEC");
        assert_eq!(rows[0]["text"], "/bin/sh");
        assert_eq!(rows[1]["kind"], "OPEN");
        assert_eq!(rows[1]["text"], "out.txt");
        assert_eq!(rows[1]["extras"][0], 0o101);
        assert_eq!(rows[2]["kind"], "STDOUT");
        assert_eq!(rows[2]["text"], "hi\n");
        assert_eq!(rows[3]["kind"], "EXIT");
    }

    /// A blob past TEXT_MAX (4 KiB) is truncated with a "… (N bytes)" suffix
    /// so one huge write can't bloat the reply.
    #[test]
    fn decode_trace_truncates_long_text() {
        let mut enc = EvState::default();
        let mut stream = sudwire::version_atom();
        let big = vec![b'a'; 5000];
        stream.extend(enc.build_event(1, sudwire::EV_STDOUT, 1, 9, 9, 1,
                                      9, 9, &[], &big));
        let v = decode_trace(&stream);
        let text = v["events"][0]["text"].as_str().unwrap();
        assert!(text.ends_with("… (5000 bytes)"), "got: {}", &text[..40]);
        assert!(text.starts_with(&"a".repeat(4096)));
    }

    /// More than CAP events flips `truncated` and clamps the row count.
    #[test]
    fn decode_trace_caps_event_count() {
        let mut enc = EvState::default();
        let mut stream = sudwire::version_atom();
        for _ in 0..20_001 {
            stream.extend(enc.build_event(1, sudwire::EV_EXEC, 1, 9, 9, 1,
                                          9, 9, &[], b""));
        }
        let v = decode_trace(&stream);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["events"].as_array().unwrap().len(), 20_000);
    }

    /// A clean sweep of a sud box's upper dir mirrors its files into the sqlar
    /// and reports the row count; the now-redundant upper residue is removed.
    /// (Runs against a temp XDG_STATE_HOME — no boxes, no wrapper.)
    #[test]
    fn sweep_ingests_upper_and_clears_residue() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "sarun-sweep-{}-{:?}", std::process::id(),
            std::time::SystemTime::now()));
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: state_home() is XDG_STATE_HOME-derived; the lock serializes
        // this against every other test that repoints it.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let id = 7701;
        let b = BoxState::create(id).unwrap();
        // Plant an upper dir with a dir + a file, the shapes ingest_upper walks.
        let upper = crate::paths::live_home().join(id.to_string())
            .join("sud-up");
        std::fs::create_dir_all(upper.join("d")).unwrap();
        std::fs::write(upper.join("d").join("f.txt"), b"hi").unwrap();

        // runpid 0 → fallback writer 0, so no /proc read is attempted.
        let r = sweep(&b, id, 0);
        assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
        assert_eq!(r.ingested, 2); // the dir + the file
        assert!(b.entry("d/f.txt").is_some());
        // Clean sweep → the upper residue is gone.
        assert!(!upper.exists(), "clean sweep should remove the upper dir");
    }
}
