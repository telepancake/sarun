//! Streaming, block-parallel bzip2 decoder.
//!
//! Bzip2 blocks are independently decodable but their boundary magic is
//! bit-aligned rather than byte-aligned. A scanner reads the source once,
//! turns each block into a valid one-block bzip2 stream, and feeds a bounded
//! worker pool. The reader reorders completed blocks before exposing bytes to
//! the XML parser. At most a small multiple of the 900 KiB bzip2 block size is
//! resident; the compressed input is never staged on disk.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Cursor, Read};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, mpsc};

use bzip2::read::BzDecoder;

const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
const END_MAGIC: u64 = 0x1772_4538_5090;

/// Decoder options. Zero selects the host's available parallelism.
#[derive(Debug, Clone)]
pub struct Bz2Options {
    pub workers: usize,
}

struct Job {
    index: usize,
    stream: Vec<u8>,
}

enum Event {
    Block(usize, io::Result<Vec<u8>>),
    Done {
        blocks: usize,
        error: Option<io::Error>,
    },
}

/// An order-preserving reader backed by a bounded parallel decode pool.
pub struct Bz2Reader<R: Read + Send + 'static> {
    events: mpsc::Receiver<Event>,
    pending: BTreeMap<usize, io::Result<Vec<u8>>>,
    current: Cursor<Vec<u8>>,
    next: usize,
    done: Option<(usize, Option<io::Error>)>,
    _source: PhantomData<fn() -> R>,
}

/// Wrap a bzip2-compressed byte source in a streaming parallel decoder.
pub fn new_bz2_reader<R: Read + Send + 'static>(source: R, opts: Bz2Options) -> Bz2Reader<R> {
    let workers = if opts.workers == 0 {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    } else {
        opts.workers
    }
    .max(1);
    let capacity = workers.saturating_mul(2).max(2);
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Job>(capacity);
    let (events_tx, events_rx) = mpsc::sync_channel::<Event>(capacity);
    let jobs_rx = Arc::new(Mutex::new(jobs_rx));

    for _ in 0..workers {
        let jobs = Arc::clone(&jobs_rx);
        let events = events_tx.clone();
        std::thread::spawn(move || {
            loop {
                let job = {
                    let receiver = match jobs.lock() {
                        Ok(receiver) => receiver,
                        Err(_) => return,
                    };
                    match receiver.recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    }
                };
                let result = decode_block(&job.stream);
                if events.send(Event::Block(job.index, result)).is_err() {
                    return;
                }
            }
        });
    }

    let scanner_events = events_tx.clone();
    std::thread::spawn(move || {
        let mut blocks = 0usize;
        let error = scan_stream(source, |stream| {
            let index = blocks;
            jobs_tx
                .send(Job { index, stream })
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "decoder stopped"))?;
            blocks += 1;
            Ok(())
        })
        .err();
        drop(jobs_tx);
        let _ = scanner_events.send(Event::Done { blocks, error });
    });
    drop(events_tx);

    Bz2Reader {
        events: events_rx,
        pending: BTreeMap::new(),
        current: Cursor::new(Vec::new()),
        next: 0,
        done: None,
        _source: PhantomData,
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
                Ok(Event::Done { blocks, error }) => self.done = Some((blocks, error)),
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
