// sud-backed boxes, step 1 (see engine/DESIGN-sud.md — WORK IN PROGRESS).
// The box ran under tv's sudtrace with a plain directory upper overlaid on
// `/`; this module sweeps that upper directory into the box's sqlar
// BoxState after the command exits, so review/apply/discard/UI work on a
// sud box exactly as on a FUSE box. Post-exit sweep = final state only:
// every row is attributed to the runner's process row until the wire trace
// stream is ingested (step 2).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::capture::BoxState;
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

/// Decode a raw sud TRACE stream (the `sudtrace` blob) into the closed result
/// type generated from the relation. Each row retains time, process identity,
/// extras, and text; unknown numeric event kinds remain an explicit sum case.
/// `text` is the blob
/// rendered lossy-UTF8 — argv/env/cwd/open paths and stdout/stderr bytes —
/// truncated at 4 KiB with a "… (N bytes)" suffix so one huge write can't
/// bloat the reply. Capped at the first `CAP` events with a `truncated`
/// flag so a giant trace can't wedge the UI.
pub fn decode_trace(bytes: &[u8]) -> Result<crate::generated_wire::SudTraceView, String> {
    use crate::generated_wire::{
        LIMIT_COLLECTION_ITEMS, LIMIT_TEXT_BYTES, SudEvent, SudEventKind, SudTraceView,
    };
    use crate::wire::{BoundedText, BoundedVec};
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
    let rows = events.iter().take(CAP).map(|e| {
        let kind = match e.ty {
            sudwire::EV_EXEC => SudEventKind::Exec,
            sudwire::EV_ARGV => SudEventKind::Argv,
            sudwire::EV_ENV => SudEventKind::Env,
            sudwire::EV_OPEN => SudEventKind::Open,
            sudwire::EV_CWD => SudEventKind::Cwd,
            sudwire::EV_STDOUT => SudEventKind::Stdout,
            sudwire::EV_STDERR => SudEventKind::Stderr,
            sudwire::EV_EXIT => SudEventKind::Exit,
            sudwire::EV_PROF => SudEventKind::Prof,
            code => SudEventKind::Unknown { code },
        };
        Ok(SudEvent {
            time_ns: u64::try_from(e.ts_ns)
                .map_err(|_| "TRACE event has a negative timestamp")?,
            kind,
            pid: u32::try_from(e.pid).map_err(|_| "TRACE event has an invalid pid")?,
            tgid: u32::try_from(e.tgid).map_err(|_| "TRACE event has an invalid tgid")?,
            ppid: u32::try_from(e.ppid).map_err(|_| "TRACE event has an invalid ppid")?,
            extras: BoundedVec::<i64, 0, LIMIT_COLLECTION_ITEMS>::new(e.extras.clone())
                .map_err(|error| format!("TRACE event extras exceed bounds: {error:?}"))?,
            text: BoundedText::<LIMIT_TEXT_BYTES>::new(render_text(&e.blob))
                .map_err(|error| format!("TRACE event text exceeds bounds: {error:?}"))?,
        })
    }).collect::<Result<Vec<_>, String>>()?;
    Ok(SudTraceView {
        events: BoundedVec::new(rows)
            .map_err(|error| format!("TRACE event count exceeds bounds: {error:?}"))?,
        truncated,
    })
}

/// Take the box's stream state (if the runner streamed a trace), waiting
/// up to 5 s for fd 1023 to drain. The runner closes the write end before its
/// box channel, so EOF normally precedes teardown; the timeout only guards a
/// wedged reader.
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

/// Finalize transport-independent SUD trace/provenance state. Filesystem state
/// is already captured synchronously by `SarunFs`; teardown only waits for the
/// trace pipe, records its compact binary stream, and removes the live tee.
pub fn finish_stream(b: &BoxState, id: i64) {
    let live = crate::paths::live_home().join(id.to_string());
    let stream = take_stream(id);
    if let Some(s) = &stream {
        report_profile(id, s);
    }
    let trace_path = live.join("sud.trace");
    if let Ok(bytes) = std::fs::read(&trace_path) {
        b.set_sudtrace(&bytes);
    }
    let _ = std::fs::remove_file(&trace_path);
}

/// Decode a box's durable TRACE blob to the generated typed result for the
/// `sudtrace` verb / the UI Trace pane. Prefers the live BoxState's own
/// connection when the box is running (no rival on-disk handle racing serve);
/// else opens the at-rest sqlar. A box with no trace answers a clean error.
pub fn trace_events(
    live: Option<Arc<BoxState>>,
    id: i64,
) -> Result<crate::generated_wire::SudTraceView, String> {
    let blob = match live {
        Some(b) => b.get_sudtrace(),
        None => BoxState::create(id).ok().and_then(|b| b.get_sudtrace()),
    };
    match blob {
        Some(bytes) => decode_trace(&bytes),
        None => Err("box has no sud trace".into()),
    }
}

/// Spawn the reader thread for one sud box: tee `fd` (pipe read end,
/// owned here) into `trace_path` and apply events to `b` as they arrive.
pub fn stream_events(box_id: i64, fd: i32, b: Arc<BoxState>,
                     trace_path: std::path::PathBuf) {
    let st = Arc::new(Stream {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sudwire::{self, EvState};

    /// decode_trace renders an encoder-built stream into typed relation rows:
    /// EXEC/OPEN/STDOUT/EXIT cases, per-event text, extras verbatim.
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

        let v = decode_trace(&stream).unwrap();
        assert!(!v.truncated);
        let rows = v.events.as_slice();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, crate::generated_wire::SudEventKind::Exec);
        assert_eq!(rows[0].text.as_str(), "/bin/sh");
        assert_eq!(rows[1].kind, crate::generated_wire::SudEventKind::Open);
        assert_eq!(rows[1].text.as_str(), "out.txt");
        assert_eq!(rows[1].extras.as_slice()[0], 0o101);
        assert_eq!(rows[2].kind, crate::generated_wire::SudEventKind::Stdout);
        assert_eq!(rows[2].text.as_str(), "hi\n");
        assert_eq!(rows[3].kind, crate::generated_wire::SudEventKind::Exit);
    }

    #[test]
    fn decode_trace_preserves_unknown_kinds_and_rejects_invalid_identity() {
        let mut enc = EvState::default();
        let mut stream = sudwire::version_atom();
        stream.extend(enc.build_event(1, 42, 1, 9, 9, 1, 9, 9, &[], b"future"));
        let view = decode_trace(&stream).unwrap();
        assert_eq!(
            view.events.as_slice()[0].kind,
            crate::generated_wire::SudEventKind::Unknown { code: 42 }
        );

        let mut enc = EvState::default();
        let mut invalid = sudwire::version_atom();
        invalid.extend(enc.build_event(1, sudwire::EV_EXEC, 1, -1, 9, 1, 9, 9, &[], b""));
        assert!(decode_trace(&invalid).unwrap_err().contains("invalid pid"));
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
        let v = decode_trace(&stream).unwrap();
        let text = v.events.as_slice()[0].text.as_str();
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
        let v = decode_trace(&stream).unwrap();
        assert!(v.truncated);
        assert_eq!(v.events.as_slice().len(), 20_000);
    }

}
