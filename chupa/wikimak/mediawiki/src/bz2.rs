//! Streaming, block-parallel bzip2 decoder.
//!
//! Bzip2 blocks are independently decodable but their boundary magic is
//! bit-aligned rather than byte-aligned. A scanner reads the source once,
//! turns each block into a valid one-block bzip2 stream, and feeds decoder
//! threads owned by that reader. Completed blocks are reordered before their
//! bytes are exposed. Compressed input is never persisted.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Cursor, Read};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

use bzip2::read::BzDecoder;

const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
const END_MAGIC: u64 = 0x1772_4538_5090;

/// Decoder options. A positive value is the exact maximum number of decoder
/// threads owned by this reader. Zero resolves once to the host's available
/// parallelism; the resolved count is visible through Bz2Reader::stats.
#[derive(Debug, Clone)]
pub struct Bz2Options {
    pub workers: usize,
}

/// Per-reader decoder counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bz2DecodeStats {
    pub worker_limit: usize,
    pub spawned_decoder_threads: usize,
    pub active_decoders: usize,
    pub peak_active_decoders: usize,
    pub blocks_submitted: usize,
    pub blocks_finished: usize,
}

/// Process-wide CPU admission counters.
///
/// The admission limit bounds active block decodes across readers. It does not
/// create threads and does not alter any reader's Bz2Options::workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bz2AdmissionStats {
    pub limit: usize,
    pub active_decoders: usize,
    pub peak_active_decoders: usize,
}

struct AdmissionState {
    limit: usize,
    active: usize,
    peak: usize,
}

struct DecodeAdmission {
    state: Mutex<AdmissionState>,
    wake: Condvar,
}

impl DecodeAdmission {
    fn new() -> Self {
        Self {
            state: Mutex::new(AdmissionState {
                limit: default_active_decode_budget(),
                active: 0,
                peak: 0,
            }),
            wake: Condvar::new(),
        }
    }

    fn set_limit(&self, limit: usize) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let limit = limit.max(1);
        if state.limit != limit && state.active != 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot change active bzip2 decode admission while blocks are decoding",
            ));
        }
        state.limit = limit;
        if state.active == 0 {
            state.peak = 0;
        }
        self.wake.notify_all();
        Ok(())
    }

    fn acquire(self: &Arc<Self>) -> DecodeAdmissionPermit {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.active >= state.limit {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.active += 1;
        state.peak = state.peak.max(state.active);
        DecodeAdmissionPermit {
            admission: Arc::clone(self),
        }
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        self.wake.notify_all();
    }

    fn snapshot(&self) -> Bz2AdmissionStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Bz2AdmissionStats {
            limit: state.limit,
            active_decoders: state.active,
            peak_active_decoders: state.peak,
        }
    }
}

struct DecodeAdmissionPermit {
    admission: Arc<DecodeAdmission>,
}

impl Drop for DecodeAdmissionPermit {
    fn drop(&mut self) {
        self.admission.release();
    }
}

fn decode_admission() -> Arc<DecodeAdmission> {
    static ADMISSION: OnceLock<Arc<DecodeAdmission>> = OnceLock::new();
    Arc::clone(ADMISSION.get_or_init(|| Arc::new(DecodeAdmission::new())))
}

fn default_active_decode_budget() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
}

fn resolved_workers(workers: usize) -> usize {
    if workers == 0 {
        default_active_decode_budget()
    } else {
        workers
    }
    .max(1)
}

/// Configure the explicit process-wide limit on simultaneously active bzip2
/// block decodes. This is CPU admission only: reader-owned thread counts remain
/// exactly the resolved Bz2Options::workers value.
///
/// Zero selects available parallelism. Changing the limit while a block is
/// actively decoding returns WouldBlock.
pub fn configure_active_decode_budget(budget: usize) -> io::Result<()> {
    let limit = if budget == 0 {
        default_active_decode_budget()
    } else {
        budget
    };
    decode_admission().set_limit(limit)
}

/// Read process-wide decode admission counters. The peak is measured since the
/// most recent idle call to configure_active_decode_budget.
pub fn bz2_admission_stats() -> Bz2AdmissionStats {
    decode_admission().snapshot()
}

enum Event {
    Block(usize, io::Result<Vec<u8>>),
    Done {
        blocks: usize,
        error: Option<io::Error>,
    },
}

struct Job {
    index: usize,
    stream: Vec<u8>,
}

struct ReaderControl {
    state: Mutex<ReaderControlState>,
    wake: Condvar,
}

struct ReaderControlState {
    in_flight: usize,
    cancelled: bool,
}

impl ReaderControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(ReaderControlState {
                in_flight: 0,
                cancelled: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn reserve(&self, window: usize) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.cancelled && state.in_flight >= window {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.cancelled {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bzip2 reader was dropped",
            ));
        }
        state.in_flight += 1;
        Ok(())
    }

    fn complete(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight = state.in_flight.saturating_sub(1);
        self.wake.notify_all();
    }

    fn is_cancelled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancelled
    }

    fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        self.wake.notify_all();
    }
}

struct ReaderCounters {
    worker_limit: usize,
    spawned_decoder_threads: usize,
    active_decoders: AtomicUsize,
    peak_active_decoders: AtomicUsize,
    blocks_submitted: AtomicUsize,
    blocks_finished: AtomicUsize,
}

impl ReaderCounters {
    fn new(worker_limit: usize, spawned_decoder_threads: usize) -> Self {
        Self {
            worker_limit,
            spawned_decoder_threads,
            active_decoders: AtomicUsize::new(0),
            peak_active_decoders: AtomicUsize::new(0),
            blocks_submitted: AtomicUsize::new(0),
            blocks_finished: AtomicUsize::new(0),
        }
    }

    fn enter_decode(&self) {
        let active = self.active_decoders.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_active_decoders
            .fetch_max(active, Ordering::SeqCst);
    }

    fn leave_decode(&self) {
        self.active_decoders.fetch_sub(1, Ordering::SeqCst);
    }

    fn snapshot(&self) -> Bz2DecodeStats {
        Bz2DecodeStats {
            worker_limit: self.worker_limit,
            spawned_decoder_threads: self.spawned_decoder_threads,
            active_decoders: self.active_decoders.load(Ordering::SeqCst),
            peak_active_decoders: self.peak_active_decoders.load(Ordering::SeqCst),
            blocks_submitted: self.blocks_submitted.load(Ordering::SeqCst),
            blocks_finished: self.blocks_finished.load(Ordering::SeqCst),
        }
    }
}

#[cfg(test)]
struct DecodeTestHook {
    enter: Arc<dyn Fn() + Send + Sync + 'static>,
    exit: Arc<dyn Fn() + Send + Sync + 'static>,
}

#[cfg(test)]
struct DecodeTestHookGuard {
    hook: Arc<DecodeTestHook>,
}

#[cfg(test)]
static DECODE_TEST_HOOK: OnceLock<Mutex<Option<Arc<DecodeTestHook>>>> = OnceLock::new();

#[cfg(test)]
impl Drop for DecodeTestHookGuard {
    fn drop(&mut self) {
        (self.hook.exit)();
    }
}

#[cfg(test)]
fn decode_test_hook_enter() -> Option<DecodeTestHookGuard> {
    let hook = DECODE_TEST_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    hook.map(|hook| {
        (hook.enter)();
        DecodeTestHookGuard { hook }
    })
}

#[cfg(test)]
fn install_decode_test_hook(hook: Option<Arc<DecodeTestHook>>) {
    *DECODE_TEST_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

#[cfg(test)]
static SPAWN_FAILURE_AFTER: OnceLock<Mutex<Option<usize>>> = OnceLock::new();

fn spawn_named(
    name: String,
    task: impl FnOnce() + Send + 'static,
) -> io::Result<std::thread::JoinHandle<()>> {
    #[cfg(test)]
    {
        let mut failure = SPAWN_FAILURE_AFTER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(remaining) = failure.as_mut() {
            if *remaining == 0 {
                return Err(io::Error::other("injected bzip2 thread spawn failure"));
            }
            *remaining -= 1;
        }
    }
    std::thread::Builder::new().name(name).spawn(task)
}

static NEXT_READER_ID: AtomicUsize = AtomicUsize::new(0);

/// An order-preserving reader backed by decoder threads owned by this reader.
pub struct Bz2Reader<R: Read + Send + 'static> {
    events: mpsc::Receiver<Event>,
    pending: BTreeMap<usize, io::Result<Vec<u8>>>,
    current: Cursor<Vec<u8>>,
    next: usize,
    done: Option<(usize, Option<io::Error>)>,
    control: Arc<ReaderControl>,
    counters: Arc<ReaderCounters>,
    _worker_threads: Vec<std::thread::JoinHandle<()>>,
    _scanner_thread: Option<std::thread::JoinHandle<()>>,
    _source: PhantomData<fn() -> R>,
}

impl<R: Read + Send + 'static> Bz2Reader<R> {
    /// Snapshot this reader's decoder ownership and activity counters.
    pub fn stats(&self) -> Bz2DecodeStats {
        self.counters.snapshot()
    }

    fn failed(worker_limit: usize, error: io::Error) -> Self {
        let (sender, events) = mpsc::sync_channel(1);
        drop(sender);
        Self {
            events,
            pending: BTreeMap::new(),
            current: Cursor::new(Vec::new()),
            next: 0,
            done: Some((0, Some(error))),
            control: Arc::new(ReaderControl::new()),
            counters: Arc::new(ReaderCounters::new(worker_limit, 0)),
            _worker_threads: Vec::new(),
            _scanner_thread: None,
            _source: PhantomData,
        }
    }
}

/// Construct a streaming block-parallel decoder.
///
/// Construction starts exactly the resolved number of reader-owned decoder
/// threads plus one non-decoding scanner thread. Any thread spawn/resource
/// failure is returned and already-started decoder threads are joined.
pub fn try_new_bz2_reader<R: Read + Send + 'static>(
    source: R,
    opts: Bz2Options,
) -> io::Result<Bz2Reader<R>> {
    let workers = resolved_workers(opts.workers);
    let window = workers.checked_mul(2).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "bzip2 worker count is too large",
        )
    })?;
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Job>(workers);
    let (events_tx, events_rx) = mpsc::sync_channel::<Event>(window);
    let jobs_rx = Arc::new(Mutex::new(jobs_rx));
    let control = Arc::new(ReaderControl::new());
    let counters = Arc::new(ReaderCounters::new(workers, workers));
    let reader_id = NEXT_READER_ID.fetch_add(1, Ordering::Relaxed);
    let mut worker_threads = Vec::new();

    for worker in 0..workers {
        let jobs = Arc::clone(&jobs_rx);
        let events = events_tx.clone();
        let worker_control = Arc::clone(&control);
        let worker_counters = Arc::clone(&counters);
        let result = spawn_named(
            format!("sarun-bz2-{reader_id}-decoder-{worker}"),
            move || run_decoder_worker(jobs, events, worker_control, worker_counters),
        );
        match result {
            Ok(handle) => worker_threads.push(handle),
            Err(error) => {
                drop(jobs_tx);
                drop(events_tx);
                for handle in worker_threads {
                    let _ = handle.join();
                }
                return Err(error);
            }
        }
    }

    let scanner_events = events_tx.clone();
    let scanner_control = Arc::clone(&control);
    let scanner_counters = Arc::clone(&counters);
    let scanner = spawn_named(format!("sarun-bz2-{reader_id}-scanner"), move || {
        let mut blocks = 0usize;
        let scanned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scan_stream(source, |stream| {
                scanner_control.reserve(window)?;
                let index = blocks;
                if jobs_tx.send(Job { index, stream }).is_err() {
                    scanner_control.complete();
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "bzip2 decoder workers stopped",
                    ));
                }
                scanner_counters
                    .blocks_submitted
                    .fetch_add(1, Ordering::SeqCst);
                blocks += 1;
                Ok(())
            })
        }));
        let error = match scanned {
            Ok(result) => result.err(),
            Err(_) => Some(io::Error::other("bzip2 scanner thread panicked")),
        };
        let _ = scanner_events.send(Event::Done { blocks, error });
    });
    let scanner_thread = match scanner {
        Ok(handle) => handle,
        Err(error) => {
            drop(events_tx);
            for handle in worker_threads {
                let _ = handle.join();
            }
            return Err(error);
        }
    };
    drop(events_tx);

    Ok(Bz2Reader {
        events: events_rx,
        pending: BTreeMap::new(),
        current: Cursor::new(Vec::new()),
        next: 0,
        done: None,
        control,
        counters,
        _worker_threads: worker_threads,
        _scanner_thread: Some(scanner_thread),
        _source: PhantomData,
    })
}

/// Compatibility constructor.
///
/// New code that needs construction-time resource errors should use
/// try_new_bz2_reader. This wrapper never panics on spawn failure; because its
/// existing signature cannot return an error, the first read reports it.
pub fn new_bz2_reader<R: Read + Send + 'static>(source: R, opts: Bz2Options) -> Bz2Reader<R> {
    let workers = resolved_workers(opts.workers);
    match try_new_bz2_reader(source, opts) {
        Ok(reader) => reader,
        Err(error) => Bz2Reader::failed(workers, error),
    }
}

fn run_decoder_worker(
    jobs: Arc<Mutex<mpsc::Receiver<Job>>>,
    events: mpsc::SyncSender<Event>,
    control: Arc<ReaderControl>,
    counters: Arc<ReaderCounters>,
) {
    loop {
        let job = {
            let receiver = jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match receiver.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        if control.is_cancelled() {
            control.complete();
            continue;
        }
        let admission = decode_admission().acquire();
        if control.is_cancelled() {
            drop(admission);
            control.complete();
            continue;
        }
        counters.enter_decode();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decode_block(&job.stream)
        }))
        .unwrap_or_else(|_| Err(io::Error::other("bzip2 decoder worker panicked")));
        counters.leave_decode();
        drop(admission);
        counters.blocks_finished.fetch_add(1, Ordering::SeqCst);
        if events.send(Event::Block(job.index, result)).is_err() {
            control.complete();
            return;
        }
    }
}

impl<R: Read + Send + 'static> Drop for Bz2Reader<R> {
    fn drop(&mut self) {
        self.control.cancel();
        decode_admission().wake.notify_all();
    }
}

impl<R: Read + Send + 'static> Read for Bz2Reader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let read = self.current.read(output)?;
            if read != 0 {
                return Ok(read);
            }
            if let Some(result) = self.pending.remove(&self.next) {
                self.control.complete();
                self.next += 1;
                self.current = Cursor::new(result?);
                continue;
            }
            if let Some((blocks, error)) = self.done.as_mut() {
                if self.next == *blocks {
                    return match error.take() {
                        Some(error) => Err(error),
                        None => Ok(0),
                    };
                }
            }
            match self.events.recv() {
                Ok(Event::Block(index, result)) => {
                    self.pending.insert(index, result);
                }
                Ok(Event::Done { blocks, error }) => {
                    self.done = Some((blocks, error));
                }
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "bzip2 decoder workers stopped",
                    ));
                }
            }
        }
    }
}

fn decode_block(stream: &[u8]) -> io::Result<Vec<u8>> {
    #[cfg(test)]
    let _decode_hook = decode_test_hook_enter();
    let mut decoder = BzDecoder::new(Cursor::new(stream));
    let mut output = Vec::new();
    decoder.read_to_end(&mut output)?;
    Ok(output)
}

fn scan_stream<R: Read>(
    source: R,
    mut emit: impl FnMut(Vec<u8>) -> io::Result<()>,
) -> io::Result<()> {
    let mut input = BufferedBits::new(source);
    loop {
        let Some(header) = input.read_header()? else {
            return Ok(());
        };
        let block_size = header[3];
        let mut combined_crc = 0u32;
        let mut magic = input.read_bits(48)?;
        loop {
            if magic == END_MAGIC {
                let advertised = input.read_bits(32)? as u32;
                if advertised != combined_crc {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "bzip2 combined CRC mismatch: expected {advertised:08x}, got {combined_crc:08x}"
                        ),
                    ));
                }
                input.align_byte();
                break;
            }
            if magic != BLOCK_MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid bzip2 block magic {magic:012x}"),
                ));
            }

            let block_crc = input.read_bits(32)? as u32;
            combined_crc = combined_crc.rotate_left(1) ^ block_crc;
            let mut block = BitWriter::new();
            block.push_bits(BLOCK_MAGIC, 48);
            block.push_bits(block_crc as u64, 32);
            let boundary = input.find_magic()?;
            input.copy_to(&mut block, boundary)?;
            magic = input.read_bits(48)?;
            block.push_bits(END_MAGIC, 48);
            block.push_bits(block_crc as u64, 32);
            let mut stream = Vec::with_capacity(block.bytes.len() + 12);
            stream.extend_from_slice(b"BZh");
            stream.push(block_size);
            stream.extend_from_slice(&block.finish());
            emit(stream)?;
            input.compact();
        }
    }
}

struct BufferedBits<R: Read> {
    inner: BufReader<R>,
    bytes: Vec<u8>,
    position: usize,
    eof: bool,
}

impl<R: Read> BufferedBits<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
            bytes: Vec::new(),
            position: 0,
            eof: false,
        }
    }

    fn fill(&mut self) -> io::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let start = self.bytes.len();
        self.bytes.resize(start + 64 * 1024, 0);
        let read = self.inner.read(&mut self.bytes[start..])?;
        self.bytes.truncate(start + read);
        if read == 0 {
            self.eof = true;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn ensure(&mut self, end_bit: usize) -> io::Result<bool> {
        while self.bytes.len().saturating_mul(8) < end_bit {
            if !self.fill()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn read_bits(&mut self, count: usize) -> io::Result<u64> {
        if !self.ensure(self.position + count)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated bzip2 stream",
            ));
        }
        let mut value = 0u64;
        for _ in 0..count {
            let byte = self.bytes[self.position / 8];
            let bit = byte >> (7 - self.position % 8) & 1;
            value = (value << 1) | u64::from(bit);
            self.position += 1;
        }
        Ok(value)
    }

    fn find_magic(&mut self) -> io::Result<usize> {
        let mut byte_index = self.position / 8;
        let mut first_offset = self.position % 8;
        loop {
            while byte_index + 8 <= self.bytes.len() {
                let word = u64::from_be_bytes(
                    self.bytes[byte_index..byte_index + 8]
                        .try_into()
                        .expect("eight-byte window"),
                );
                for offset in first_offset..8 {
                    let candidate = word >> (16 - offset) & ((1u64 << 48) - 1);
                    if candidate == BLOCK_MAGIC || candidate == END_MAGIC {
                        let boundary = byte_index * 8 + offset;
                        if boundary >= self.position {
                            return Ok(boundary);
                        }
                    }
                }
                byte_index += 1;
                first_offset = 0;
            }
            if !self.fill()? {
                let end = self.bytes.len().saturating_mul(8);
                while self.position + 48 <= end {
                    let candidate = self.bits_at(self.position, 48);
                    if candidate == BLOCK_MAGIC || candidate == END_MAGIC {
                        return Ok(self.position);
                    }
                    self.position += 1;
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated bzip2 block",
                ));
            }
        }
    }

    fn bits_at(&self, position: usize, count: usize) -> u64 {
        let mut value = 0u64;
        for bit_index in position..position + count {
            let byte = self.bytes[bit_index / 8];
            value = (value << 1) | u64::from(byte >> (7 - bit_index % 8) & 1);
        }
        value
    }

    fn copy_to(&mut self, output: &mut BitWriter, end: usize) -> io::Result<()> {
        if end < self.position || !self.ensure(end)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated bzip2 block",
            ));
        }
        while self.position + 8 <= end {
            let index = self.position / 8;
            let offset = self.position % 8;
            let byte = if offset == 0 {
                self.bytes[index]
            } else {
                (self.bytes[index] << offset) | (self.bytes[index + 1] >> (8 - offset))
            };
            output.push_byte(byte);
            self.position += 8;
        }
        while self.position < end {
            let byte = self.bytes[self.position / 8];
            output.push_bit(byte >> (7 - self.position % 8) & 1 != 0);
            self.position += 1;
        }
        Ok(())
    }

    fn align_byte(&mut self) {
        self.position = self.position.div_ceil(8) * 8;
        self.compact();
    }

    fn compact(&mut self) {
        let consumed = self.position / 8;
        if consumed != 0 {
            self.bytes.drain(..consumed);
            self.position -= consumed * 8;
        }
    }

    fn read_header(&mut self) -> io::Result<Option<[u8; 4]>> {
        debug_assert_eq!(self.position % 8, 0);
        if !self.ensure(self.position + 1)? {
            return Ok(None);
        }
        if !self.ensure(self.position + 32)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated bzip2 header",
            ));
        }
        let start = self.position / 8;
        let header: [u8; 4] = self.bytes[start..start + 4]
            .try_into()
            .expect("four-byte header");
        self.position += 32;
        if &header[..3] != b"BZh" || !(b'1'..=b'9').contains(&header[3]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid bzip2 stream header",
            ));
        }
        Ok(Some(header))
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bits: 0,
        }
    }

    fn push_bit(&mut self, bit: bool) {
        if self.bits % 8 == 0 {
            self.bytes.push(0);
        }
        if bit {
            let shift = 7 - self.bits % 8;
            *self.bytes.last_mut().expect("just pushed byte") |= 1 << shift;
        }
        self.bits += 1;
    }

    fn push_bits(&mut self, value: u64, count: usize) {
        for shift in (0..count).rev() {
            self.push_bit((value >> shift) & 1 != 0);
        }
    }

    fn push_byte(&mut self, byte: u8) {
        debug_assert_eq!(self.bits % 8, 0);
        self.bytes.push(byte);
        self.bits += 8;
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::Compression;
    use bzip2::write::BzEncoder;
    use std::io::Write;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    fn test_serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn encoded(seed: u64, size: usize) -> (Vec<u8>, Vec<u8>) {
        let mut state = seed;
        let mut plain = vec![0u8; size];
        for chunk in plain.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
        }
        let mut encoder = BzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&plain).expect("encode fixture");
        (encoder.finish().expect("finish fixture"), plain)
    }

    fn decode_with_stats(
        compressed: Vec<u8>,
        workers: usize,
    ) -> io::Result<(Vec<u8>, Bz2DecodeStats)> {
        let mut reader = try_new_bz2_reader(Cursor::new(compressed), Bz2Options { workers })?;
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        Ok((output, reader.stats()))
    }

    fn hook(
        enter: impl Fn() + Send + Sync + 'static,
        exit: impl Fn() + Send + Sync + 'static,
    ) -> Arc<DecodeTestHook> {
        Arc::new(DecodeTestHook {
            enter: Arc::new(enter),
            exit: Arc::new(exit),
        })
    }

    struct SpawnFailureGuard;

    impl Drop for SpawnFailureGuard {
        fn drop(&mut self) {
            *SPAWN_FAILURE_AFTER
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    fn fail_spawn_after(successes: usize) -> SpawnFailureGuard {
        *SPAWN_FAILURE_AFTER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(successes);
        SpawnFailureGuard
    }

    #[test]
    fn exact_per_reader_peak_is_one_and_four() {
        let _serial = test_serial();
        configure_active_decode_budget(4).expect("configure admission");
        let (compressed, plain) = encoded(0x1042, 2 * 1024 * 1024);

        let (serial, serial_stats) =
            decode_with_stats(compressed.clone(), 1).expect("one-worker decode");
        assert_eq!(serial, plain);
        assert_eq!(serial_stats.worker_limit, 1);
        assert_eq!(serial_stats.spawned_decoder_threads, 1);
        assert_eq!(serial_stats.peak_active_decoders, 1);
        assert_eq!(serial_stats.active_decoders, 0);

        configure_active_decode_budget(4).expect("reset admission peak");
        let barrier = Arc::new(Barrier::new(4));
        let entered = Arc::new(AtomicUsize::new(0));
        let enter_barrier = Arc::clone(&barrier);
        let enter_count = Arc::clone(&entered);
        install_decode_test_hook(Some(hook(
            move || {
                if enter_count.fetch_add(1, Ordering::SeqCst) < 4 {
                    enter_barrier.wait();
                }
            },
            || {},
        )));
        let (parallel, parallel_stats) =
            decode_with_stats(compressed, 4).expect("four-worker decode");
        install_decode_test_hook(None);

        assert_eq!(parallel, plain);
        assert_eq!(parallel_stats.worker_limit, 4);
        assert_eq!(parallel_stats.spawned_decoder_threads, 4);
        assert_eq!(parallel_stats.peak_active_decoders, 4);
        assert_eq!(parallel_stats.active_decoders, 0);
        assert_eq!(parallel_stats.blocks_submitted, parallel_stats.blocks_finished);
        assert_eq!(bz2_admission_stats().peak_active_decoders, 4);
    }

    #[test]
    fn two_readers_have_one_decoder_each_and_share_explicit_admission() {
        let _serial = test_serial();
        configure_active_decode_budget(2).expect("configure admission");
        let barrier = Arc::new(Barrier::new(2));
        let entered = Arc::new(AtomicUsize::new(0));
        let enter_barrier = Arc::clone(&barrier);
        let enter_count = Arc::clone(&entered);
        install_decode_test_hook(Some(hook(
            move || {
                if enter_count.fetch_add(1, Ordering::SeqCst) < 2 {
                    enter_barrier.wait();
                }
            },
            || {},
        )));
        let (compressed_a, plain_a) = encoded(0x2042, 1024 * 1024);
        let (compressed_b, plain_b) = encoded(0x3042, 1024 * 1024);
        let first = thread::spawn(move || decode_with_stats(compressed_a, 1));
        let second = thread::spawn(move || decode_with_stats(compressed_b, 1));
        let (output_a, stats_a) = first.join().expect("first reader").expect("first decode");
        let (output_b, stats_b) = second.join().expect("second reader").expect("second decode");
        install_decode_test_hook(None);

        assert_eq!(output_a, plain_a);
        assert_eq!(output_b, plain_b);
        assert_eq!(stats_a.spawned_decoder_threads, 1);
        assert_eq!(stats_b.spawned_decoder_threads, 1);
        assert_eq!(stats_a.peak_active_decoders, 1);
        assert_eq!(stats_b.peak_active_decoders, 1);
        assert_eq!(bz2_admission_stats().peak_active_decoders, 2);
    }

    #[test]
    fn construction_spawn_failures_are_errors_and_never_panics() {
        let _serial = test_serial();
        let failure = fail_spawn_after(0);
        let result = try_new_bz2_reader(Cursor::new(Vec::new()), Bz2Options { workers: 1 });
        let error = match result {
            Ok(_) => panic!("injected decoder spawn failure must be returned"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Other);
        drop(failure);

        let failure = fail_spawn_after(1);
        let result = try_new_bz2_reader(Cursor::new(Vec::new()), Bz2Options { workers: 1 });
        assert!(result.is_err(), "scanner spawn failure must be returned");
        drop(failure);

        let failure = fail_spawn_after(0);
        let mut compatibility =
            new_bz2_reader(Cursor::new(Vec::new()), Bz2Options { workers: 1 });
        let mut byte = [0u8; 1];
        assert!(
            compatibility.read(&mut byte).is_err(),
            "compatibility constructor must surface spawn failure from read"
        );
        drop(failure);
    }

    #[test]
    fn decode_panic_is_reported_and_does_not_poison_later_readers() {
        let _serial = test_serial();
        configure_active_decode_budget(1).expect("configure admission");
        let panicked = Arc::new(AtomicBool::new(false));
        let enter_panicked = Arc::clone(&panicked);
        install_decode_test_hook(Some(hook(
            move || {
                if !enter_panicked.swap(true, Ordering::SeqCst) {
                    panic!("injected decode panic");
                }
            },
            || {},
        )));
        let (compressed, _) = encoded(0x4042, 512 * 1024);
        assert!(decode_with_stats(compressed, 1).is_err());
        install_decode_test_hook(None);

        let (compressed, plain) = encoded(0x5042, 512 * 1024);
        let (output, stats) = decode_with_stats(compressed, 1).expect("later decode");
        assert_eq!(output, plain);
        assert_eq!(stats.peak_active_decoders, 1);
        assert_eq!(stats.active_decoders, 0);
    }
}
