//! Bounded shared-memory transport for SUD filesystem requests.
//!
//! Slots carry raw FUSE request and response messages.  The state machine is
//! independently reclaimable per slot, so a dead producer cannot strand a
//! global enqueue cursor.  Filesystem semantics remain in the shared
//! `virtiofsd::server::Server`; this module only owns byte movement and waits.

use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub(crate) const RING_MAGIC: u32 = 0x5346_5247;
pub(crate) const RING_VERSION: u32 = 1;
pub(crate) const RING_FD: RawFd = 1021;
pub(crate) const FD_LANE_FD: RawFd = 1020;
pub(crate) const SLOT_COUNT: usize = 32;
pub(crate) const SLOT_DATA: usize = 32 * 1024;

const SLOT_FREE: u32 = 0;
const SLOT_WRITING: u32 = 1;
const SLOT_REQUEST: u32 = 2;
const SLOT_PROCESSING: u32 = 3;
const SLOT_RESPONSE: u32 = 4;
const SLOT_CANCELLED: u32 = 5;

const FD_MAGIC: u32 = 0x5346_4644;
const FD_VERSION: u16 = 1;
const FD_EXPORT: u16 = 1;
const FD_EXPORT_WRITE: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FdRequest {
    magic: u32,
    version: u16,
    operation: u16,
    request_id: u64,
    handle: u64,
    flags: u32,
    caller_pid: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FdResponse {
    magic: u32,
    version: u16,
    operation: u16,
    request_id: u64,
    error: i32,
    reserved: u32,
}

const _: () = assert!(size_of::<FdRequest>() == 32);
const _: () = assert!(size_of::<FdResponse>() == 24);

#[repr(C)]
struct RingHeader {
    magic: u32,
    version: u32,
    total_size: u32,
    slot_count: u32,
    slot_data: u32,
    shutdown: u32,
    request_wake: u32,
    next_id: u32,
    fd_lane_lock: u32,
    fd_lane_owner: u32,
    fd_lane_next: u32,
    reserved: [u32; 5],
}

#[repr(C, align(64))]
struct Slot {
    state: u32,
    request_len: u32,
    response_len: u32,
    flags: u32,
    request_id: u64,
    owner_tgid: i32,
    owner_tid: i32,
    reserved: [u32; 8],
    request: [u8; SLOT_DATA],
    response: [u8; SLOT_DATA],
}

#[repr(C)]
struct Ring {
    header: RingHeader,
    slots: [Slot; SLOT_COUNT],
}

const _: () = assert!(size_of::<RingHeader>() == 64);
const _: () = assert!(size_of::<Slot>() == 65_600);
const _: () = assert!(size_of::<Ring>() == 2_099_264);

/// One mapping of the shared ring. Duplicating it maps the same memfd again;
/// this mirrors the engine/tracee process boundary in unit tests.
pub(crate) struct RingMapping {
    fd: OwnedFd,
    base: NonNull<Ring>,
}

// Every mutation is synchronized through the state atomics. Non-atomic slot
// fields are written only by the state owner and published with Release.
unsafe impl Send for RingMapping {}
unsafe impl Sync for RingMapping {}

impl RingMapping {
    pub(crate) fn create() -> io::Result<Self> {
        // SAFETY: the name is a valid static C string and the returned fd is
        // immediately adopted on success.
        let raw = unsafe {
            libc::memfd_create(
                c"sarun-sud-fs".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: memfd_create returned a new owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size_of::<Ring>() as libc::off_t) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mapping = Self::map(fd)?;
        // A fresh memfd is zero-filled; publish the immutable ABI description
        // before the fd can be handed to another process.
        unsafe {
            let header = std::ptr::addr_of_mut!((*mapping.base.as_ptr()).header);
            std::ptr::write(
                header,
                RingHeader {
                    magic: RING_MAGIC,
                    version: RING_VERSION,
                    total_size: size_of::<Ring>() as u32,
                    slot_count: SLOT_COUNT as u32,
                    slot_data: SLOT_DATA as u32,
                    shutdown: 0,
                    request_wake: 0,
                    next_id: 0,
                    fd_lane_lock: 0,
                    fd_lane_owner: 0,
                    fd_lane_next: 0,
                    reserved: [0; 5],
                },
            );
        }
        Ok(mapping)
    }

    #[cfg(test)]
    pub(crate) fn duplicate(&self) -> io::Result<Self> {
        Self::from_fd(self.duplicate_fd()?)
    }

    #[cfg(test)]
    pub(crate) fn duplicate_fd(&self) -> io::Result<OwnedFd> {
        let raw = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if raw < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
    }

    pub(crate) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if stat.st_size != size_of::<Ring>() as libc::off_t {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SUD filesystem ring size {} != {}", stat.st_size, size_of::<Ring>()),
            ));
        }
        let mapping = Self::map(fd)?;
        let header = unsafe { std::ptr::addr_of!((*mapping.base.as_ptr()).header) };
        let valid = unsafe {
            (*header).magic == RING_MAGIC
                && (*header).version == RING_VERSION
                && (*header).total_size as usize == size_of::<Ring>()
                && (*header).slot_count as usize == SLOT_COUNT
                && (*header).slot_data as usize == SLOT_DATA
        };
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SUD filesystem ring ABI mismatch",
            ));
        }
        Ok(mapping)
    }

    fn map(fd: OwnedFd) -> io::Result<Self> {
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size_of::<Ring>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            base: NonNull::new(address.cast()).expect("mmap never returns null on success"),
        })
    }

    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    fn slot(&self, index: usize) -> *mut Slot {
        debug_assert!(index < SLOT_COUNT);
        unsafe { std::ptr::addr_of_mut!((*self.base.as_ptr()).slots[index]) }
    }

    fn header(&self) -> *mut RingHeader {
        unsafe { std::ptr::addr_of_mut!((*self.base.as_ptr()).header) }
    }

    fn shutdown_word(&self) -> &AtomicU32 {
        unsafe { atomic(std::ptr::addr_of_mut!((*self.header()).shutdown)) }
    }

    fn wake_word(&self) -> &AtomicU32 {
        unsafe { atomic(std::ptr::addr_of_mut!((*self.header()).request_wake)) }
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_word().store(1, Ordering::Release);
        self.wake_word().fetch_add(1, Ordering::Release);
        futex_wake(self.wake_word(), i32::MAX);
        for index in 0..SLOT_COUNT {
            let state = unsafe { slot_state(self.slot(index)) };
            futex_wake(state, i32::MAX);
        }
    }
}

impl Drop for RingMapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), size_of::<Ring>());
        }
    }
}

#[cfg(test)]
pub(crate) struct RingClient {
    mapping: Arc<RingMapping>,
    cursor: AtomicU32,
}

#[cfg(test)]
impl RingClient {
    pub(crate) fn new(mapping: Arc<RingMapping>) -> Self {
        Self {
            mapping,
            cursor: AtomicU32::new(0),
        }
    }

    pub(crate) fn request(&self, request: &[u8]) -> io::Result<Vec<u8>> {
        if request.len() > SLOT_DATA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("SUD filesystem request exceeds {SLOT_DATA} bytes"),
            ));
        }
        let (_index, slot) = self.claim_slot()?;
        let id = unsafe {
            atomic(std::ptr::addr_of_mut!((*self.mapping.header()).next_id))
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1)
        };
        unsafe {
            std::ptr::write(std::ptr::addr_of_mut!((*slot).request_len), request.len() as u32);
            std::ptr::write(std::ptr::addr_of_mut!((*slot).response_len), 0);
            std::ptr::write(std::ptr::addr_of_mut!((*slot).flags), 0);
            std::ptr::write(std::ptr::addr_of_mut!((*slot).request_id), u64::from(id));
            std::ptr::write(std::ptr::addr_of_mut!((*slot).owner_tgid), libc::getpid());
            std::ptr::write(std::ptr::addr_of_mut!((*slot).owner_tid), libc::syscall(libc::SYS_gettid) as i32);
            std::ptr::copy_nonoverlapping(
                request.as_ptr(),
                std::ptr::addr_of_mut!((*slot).request).cast::<u8>(),
                request.len(),
            );
            slot_state(slot).store(SLOT_REQUEST, Ordering::Release);
        }
        let wake = self.mapping.wake_word();
        wake.fetch_add(1, Ordering::Release);
        futex_wake(wake, 1);

        let state = unsafe { slot_state(slot) };
        loop {
            match state.load(Ordering::Acquire) {
                SLOT_RESPONSE => {
                    let length = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).response_len)) }
                        as usize;
                    if length > SLOT_DATA {
                        state.store(SLOT_FREE, Ordering::Release);
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "SUD filesystem response length exceeds its slot",
                        ));
                    }
                    let mut response = vec![0; length];
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            std::ptr::addr_of!((*slot).response).cast::<u8>(),
                            response.as_mut_ptr(),
                            length,
                        );
                    }
                    state.store(SLOT_FREE, Ordering::Release);
                    self.notify_slot_available();
                    return Ok(response);
                }
                SLOT_CANCELLED => {
                    state.store(SLOT_FREE, Ordering::Release);
                    self.notify_slot_available();
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "SUD filesystem request was cancelled",
                    ));
                }
                current if self.mapping.shutdown_word().load(Ordering::Acquire) != 0 => {
                    let _ = state.compare_exchange(
                        current,
                        SLOT_CANCELLED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "SUD filesystem server stopped",
                    ));
                }
                current => futex_wait(state, current),
            }
        }
    }

    fn claim_slot(&self) -> io::Result<(usize, *mut Slot)> {
        loop {
            if self.mapping.shutdown_word().load(Ordering::Acquire) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "SUD filesystem server stopped",
                ));
            }
            let wake = self.mapping.wake_word();
            let observed = wake.load(Ordering::Acquire);
            let start = self.cursor.fetch_add(1, Ordering::Relaxed) as usize;
            for offset in 0..SLOT_COUNT {
                let index = (start + offset) % SLOT_COUNT;
                let slot = self.mapping.slot(index);
                let state = unsafe { slot_state(slot) };
                if state
                    .compare_exchange(
                        SLOT_FREE,
                        SLOT_WRITING,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return Ok((index, slot));
                }
            }
            futex_wait(wake, observed);
        }
    }

    fn notify_slot_available(&self) {
        let wake = self.mapping.wake_word();
        wake.fetch_add(1, Ordering::Release);
        futex_wake(wake, 1);
    }
}

#[derive(Debug)]
pub(crate) struct PendingRequest {
    index: usize,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) struct RingServer {
    mapping: Arc<RingMapping>,
    cursor: AtomicU32,
}

/// Engine-side lifetime of one SUD filesystem transport. Every worker feeds
/// the raw bytes into the same virtiofsd decoder used by `/dev/fuse` and QEMU.
pub(crate) struct SudFsSession {
    mapping: Arc<RingMapping>,
    workers: Vec<std::thread::JoinHandle<io::Result<()>>>,
    lane_shutdown: Option<OwnedFd>,
}

impl SudFsSession {
    pub(crate) fn start(
        filesystem: crate::sarunfs::SarunFs,
        fd: OwnedFd,
        lane_fd: OwnedFd,
        worker_count: usize,
    ) -> io::Result<Self> {
        let lane_shutdown = duplicate_fd(&lane_fd)?;
        let lane_filesystem = filesystem.clone();
        let mut session = Self::start_with(filesystem, fd, worker_count)?;
        session.workers.push(std::thread::spawn(move || {
            serve_fd_lane(lane_filesystem, lane_fd)
        }));
        session.lane_shutdown = Some(lane_shutdown);
        Ok(session)
    }

    #[cfg(test)]
    fn start_for_test<F>(filesystem: F, fd: OwnedFd, worker_count: usize) -> io::Result<Self>
    where
        F: virtiofsd::filesystem::FileSystem + Send + Sync + 'static,
    {
        Self::start_with(filesystem, fd, worker_count)
    }

    fn start_with<F>(filesystem: F, fd: OwnedFd, worker_count: usize) -> io::Result<Self>
    where
        F: virtiofsd::filesystem::FileSystem + Send + Sync + 'static,
    {
        let mapping = Arc::new(RingMapping::from_fd(fd)?);
        let ring = Arc::new(RingServer::new(mapping.clone()));
        let decoder = Arc::new(virtiofsd::server::Server::new(filesystem));
        let mut workers = Vec::with_capacity(worker_count.max(1));
        for _ in 0..worker_count.max(1) {
            let ring = ring.clone();
            let decoder = decoder.clone();
            workers.push(std::thread::spawn(move || {
                let mut response = vec![0; SLOT_DATA];
                while let Some(request) = ring.wait()? {
                    match decoder.handle_fuse_message(&request.bytes, &mut response) {
                        Ok(length) => ring.complete(request, &response[..length])?,
                        Err(_) => ring.cancel(request),
                    }
                }
                Ok(())
            }));
        }
        {
            let ring = ring.clone();
            let mapping = mapping.clone();
            workers.push(std::thread::spawn(move || {
                while mapping.shutdown_word().load(Ordering::Acquire) == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    ring.reap_dead_owners();
                }
                Ok(())
            }));
        }
        Ok(Self { mapping, workers, lane_shutdown: None })
    }

    pub(crate) fn stop(mut self) -> io::Result<()> {
        self.mapping.shutdown();
        self.shutdown_lane();
        let mut first_error = None;
        for worker in self.workers.drain(..) {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Err(_) if first_error.is_none() => {
                    first_error = Some(io::Error::other("SUD filesystem worker panicked"));
                }
                _ => {}
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for SudFsSession {
    fn drop(&mut self) {
        self.mapping.shutdown();
        self.shutdown_lane();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl SudFsSession {
    fn shutdown_lane(&mut self) {
        if let Some(fd) = self.lane_shutdown.take() {
            unsafe { libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR); }
        }
    }
}

fn duplicate_fd(fd: &OwnedFd) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if raw < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}

fn serve_fd_lane(filesystem: crate::sarunfs::SarunFs, fd: OwnedFd) -> io::Result<()> {
    loop {
        let mut request = FdRequest::default();
        let received = loop {
            let result = unsafe {
                libc::recv(fd.as_raw_fd(), std::ptr::addr_of_mut!(request).cast(),
                           size_of::<FdRequest>(), 0)
            };
            if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break result;
        };
        if received == 0 { return Ok(()); }
        if received < 0 {
            let error = io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ECONNRESET) | Some(libc::ENOTCONN)) {
                return Ok(());
            }
            return Err(error);
        }

        let valid = received as usize == size_of::<FdRequest>()
            && request.magic == FD_MAGIC
            && request.version == FD_VERSION
            && request.operation == FD_EXPORT
            && request.flags & !FD_EXPORT_WRITE == 0;
        let exported = if valid {
            filesystem.export_handle(
                request.handle,
                request.flags & FD_EXPORT_WRITE != 0,
                request.caller_pid,
            )
        } else {
            Err(io::Error::from_raw_os_error(libc::EPROTO))
        };
        let response = FdResponse {
            magic: FD_MAGIC,
            version: FD_VERSION,
            operation: FD_EXPORT,
            request_id: request.request_id,
            error: exported.as_ref().err()
                .map(|error| -error.raw_os_error().unwrap_or(libc::EIO).abs())
                .unwrap_or(0),
            reserved: 0,
        };
        send_fd_response(fd.as_raw_fd(), &response, exported.as_ref().ok())?;
    }
}

fn send_fd_response(fd: RawFd, response: &FdResponse, exported: Option<&File>) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: std::ptr::from_ref(response).cast_mut().cast(),
        iov_len: size_of::<FdResponse>(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    let mut control = [0u8; 64];
    if let Some(exported) = exported {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(),
                                      exported.as_raw_fd());
        }
    }
    let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    if sent == size_of::<FdResponse>() as isize {
        Ok(())
    } else if sent < 0 {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EPIPE) | Some(libc::ECONNRESET)) {
            Ok(())
        } else {
            Err(error)
        }
    } else {
        Err(io::Error::new(io::ErrorKind::WriteZero,
                           "short SUD fd-lane response"))
    }
}

impl RingServer {
    pub(crate) fn new(mapping: Arc<RingMapping>) -> Self {
        Self {
            mapping,
            cursor: AtomicU32::new(0),
        }
    }

    pub(crate) fn next(&self) -> io::Result<Option<PendingRequest>> {
        if self.mapping.shutdown_word().load(Ordering::Acquire) != 0 {
            return Ok(None);
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) as usize;
        for offset in 0..SLOT_COUNT {
            let index = (start + offset) % SLOT_COUNT;
            let slot = self.mapping.slot(index);
            let state = unsafe { slot_state(slot) };
            if state
                .compare_exchange(
                    SLOT_REQUEST,
                    SLOT_PROCESSING,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue;
            }
            let length = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).request_len)) }
                as usize;
            if length > SLOT_DATA {
                state.store(SLOT_CANCELLED, Ordering::Release);
                futex_wake(state, 1);
                continue;
            }
            let mut bytes = vec![0; length];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    std::ptr::addr_of!((*slot).request).cast::<u8>(),
                    bytes.as_mut_ptr(),
                    length,
                );
            }
            return Ok(Some(PendingRequest {
                index,
                bytes,
            }));
        }
        Ok(None)
    }

    pub(crate) fn wait(&self) -> io::Result<Option<PendingRequest>> {
        loop {
            let wake = self.mapping.wake_word();
            let observed = wake.load(Ordering::Acquire);
            if let Some(request) = self.next()? {
                return Ok(Some(request));
            }
            if self.mapping.shutdown_word().load(Ordering::Acquire) != 0 {
                return Ok(None);
            }
            futex_wait(wake, observed);
        }
    }

    pub(crate) fn complete(&self, request: PendingRequest, response: &[u8]) -> io::Result<()> {
        if response.len() > SLOT_DATA {
            self.cancel(request);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SUD filesystem response exceeds {SLOT_DATA} bytes"),
            ));
        }
        let slot = self.mapping.slot(request.index);
        let state = unsafe { slot_state(slot) };
        if state.load(Ordering::Acquire) != SLOT_PROCESSING {
            return Ok(());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                response.as_ptr(),
                std::ptr::addr_of_mut!((*slot).response).cast::<u8>(),
                response.len(),
            );
            std::ptr::write(std::ptr::addr_of_mut!((*slot).response_len), response.len() as u32);
        }
        if state
            .compare_exchange(
                SLOT_PROCESSING,
                SLOT_RESPONSE,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
        {
            futex_wake(state, 1);
        }
        Ok(())
    }

    pub(crate) fn cancel(&self, request: PendingRequest) {
        let slot = self.mapping.slot(request.index);
        let state = unsafe { slot_state(slot) };
        if state
            .compare_exchange(
                SLOT_PROCESSING,
                SLOT_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            futex_wake(state, 1);
        }
    }

    /// Reclaim slots belonging to a process known to be dead. PROCESSING is
    /// left to its worker, which will publish or discard the completion.
    pub(crate) fn reap_tgid(&self, tgid: i32) -> usize {
        let mut reaped = 0;
        for index in 0..SLOT_COUNT {
            let slot = self.mapping.slot(index);
            let owner = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).owner_tgid)) };
            if owner != tgid {
                continue;
            }
            let state = unsafe { slot_state(slot) };
            let current = state.load(Ordering::Acquire);
            if matches!(current, SLOT_WRITING | SLOT_REQUEST | SLOT_RESPONSE | SLOT_CANCELLED)
                && state
                    .compare_exchange(
                        current,
                        SLOT_FREE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                reaped += 1;
            }
        }
        if reaped != 0 {
            let wake = self.mapping.wake_word();
            wake.fetch_add(1, Ordering::Release);
            futex_wake(wake, reaped as i32);
        }
        reaped
    }

    fn reap_dead_owners(&self) -> usize {
        let mut owners = std::collections::BTreeSet::new();
        for index in 0..SLOT_COUNT {
            let slot = self.mapping.slot(index);
            let state = unsafe { slot_state(slot) }.load(Ordering::Acquire);
            if !matches!(state, SLOT_WRITING | SLOT_REQUEST | SLOT_RESPONSE | SLOT_CANCELLED) {
                continue;
            }
            let owner = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).owner_tgid)) };
            if owner > 0 {
                owners.insert(owner);
            }
        }
        owners
            .into_iter()
            .filter(|owner| {
                let result = unsafe { libc::kill(*owner, 0) };
                result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            })
            .map(|owner| self.reap_tgid(owner))
            .sum()
    }
}

unsafe fn atomic<'a>(word: *mut u32) -> &'a AtomicU32 {
    unsafe { &*word.cast::<AtomicU32>() }
}

unsafe fn slot_state<'a>(slot: *mut Slot) -> &'a AtomicU32 {
    unsafe { atomic(std::ptr::addr_of_mut!((*slot).state)) }
}

fn futex_wait(word: &AtomicU32, expected: u32) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT,
            expected,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0,
        );
    }
}

fn futex_wake(word: &AtomicU32, count: i32) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAKE,
            count,
            0,
            0,
            0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::time::Duration;
    use virtiofsd::filesystem::{Context, DirectoryIterator, Entry, FileSystem, ROOT_ID};
    use virtiofsd::fuse::{Attr, InHeader, OutHeader};

    #[test]
    fn rust_layout_matches_the_freestanding_c_abi() {
        assert_eq!(std::mem::offset_of!(Ring, slots), 64);
        assert_eq!(std::mem::offset_of!(Slot, request), 64);
        assert_eq!(std::mem::offset_of!(Slot, response), 64 + SLOT_DATA);
        assert_eq!(size_of::<Ring>(), 2_099_264);
    }

    #[test]
    fn duplicate_mappings_exchange_bounded_messages() {
        let owner = Arc::new(RingMapping::create().unwrap());
        let peer = Arc::new(owner.duplicate().unwrap());
        let server = Arc::new(RingServer::new(peer));
        let worker = {
            let server = server.clone();
            std::thread::spawn(move || {
                let request = server.wait().unwrap().unwrap();
                assert_eq!(request.bytes, b"canonical fuse bytes");
                server.complete(request, b"one decoder reply").unwrap();
            })
        };
        let client = RingClient::new(owner);
        assert_eq!(client.request(b"canonical fuse bytes").unwrap(), b"one decoder reply");
        worker.join().unwrap();
    }

    #[test]
    fn concurrent_producers_do_not_lose_or_cross_replies() {
        let client_mapping = Arc::new(RingMapping::create().unwrap());
        let server_mapping = Arc::new(client_mapping.duplicate().unwrap());
        let server = Arc::new(RingServer::new(server_mapping.clone()));
        let workers = (0..4)
            .map(|_| {
                let server = server.clone();
                std::thread::spawn(move || {
                    while let Some(request) = server.wait().unwrap() {
                        let response = request.bytes.clone();
                        server.complete(request, &response).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        let clients = (0..8)
            .map(|client| {
                let mapping = client_mapping.clone();
                std::thread::spawn(move || {
                    let ring = RingClient::new(mapping);
                    for sequence in 0..250u32 {
                        let request = format!("client={client};sequence={sequence}").into_bytes();
                        assert_eq!(ring.request(&request).unwrap(), request);
                    }
                })
            })
            .collect::<Vec<_>>();
        for client in clients {
            client.join().unwrap();
        }
        server_mapping.shutdown();
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn shutdown_releases_waiting_clients_and_servers() {
        let mapping = Arc::new(RingMapping::create().unwrap());
        let server = Arc::new(RingServer::new(mapping.clone()));
        let waiter = {
            let server = server.clone();
            std::thread::spawn(move || assert!(server.wait().unwrap().is_none()))
        };
        std::thread::sleep(std::time::Duration::from_millis(10));
        mapping.shutdown();
        waiter.join().unwrap();
        let client = RingClient::new(mapping);
        assert_eq!(client.request(b"late").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn dead_owner_slots_are_independently_reclaimed() {
        let mapping = Arc::new(RingMapping::create().unwrap());
        let slot = mapping.slot(7);
        unsafe {
            std::ptr::write(std::ptr::addr_of_mut!((*slot).owner_tgid), 444);
            slot_state(slot).store(SLOT_REQUEST, Ordering::Release);
        }
        let server = RingServer::new(mapping);
        assert_eq!(server.reap_tgid(444), 1);
        assert_eq!(unsafe { slot_state(slot) }.load(Ordering::Acquire), SLOT_FREE);
    }

    struct NoDirectory;

    impl DirectoryIterator for NoDirectory {
        fn next(&mut self) -> Option<virtiofsd::filesystem::DirEntry<'_>> {
            None
        }
    }

    struct LookupFs;

    impl FileSystem for LookupFs {
        type Inode = u64;
        type Handle = u64;
        type DirIter = NoDirectory;

        fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<Entry> {
            if parent != ROOT_ID || name.to_bytes() != b"hello" {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            Ok(Entry {
                inode: 2,
                generation: 0,
                attr: Attr {
                    ino: 2,
                    mode: libc::S_IFREG | 0o444,
                    nlink: 1,
                    ..Default::default()
                },
                attr_timeout: Duration::ZERO,
                entry_timeout: Duration::ZERO,
            })
        }
    }

    #[test]
    fn session_feeds_messages_through_the_canonical_decoder() {
        let client_mapping = Arc::new(RingMapping::create().unwrap());
        let server_fd = client_mapping.duplicate_fd().unwrap();
        let session = SudFsSession::start_for_test(LookupFs, server_fd, 2).unwrap();
        let header = InHeader {
            len: (size_of::<InHeader>() + 6) as u32,
            opcode: 1,
            unique: 77,
            nodeid: ROOT_ID,
            pid: std::process::id(),
            ..Default::default()
        };
        let mut request = Vec::with_capacity(header.len as usize);
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&header as *const InHeader).cast::<u8>(),
                size_of::<InHeader>(),
            )
        };
        request.extend_from_slice(bytes);
        request.extend_from_slice(b"hello\0");
        let response = RingClient::new(client_mapping).request(&request).unwrap();
        let output = unsafe { std::ptr::read_unaligned(response.as_ptr().cast::<OutHeader>()) };
        assert_eq!((output.unique, output.error), (77, 0));
        session.stop().unwrap();
    }

    #[test]
    fn descriptor_lane_validates_and_correlates_requests() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "sarun-fd-lane-{}-{:?}", std::process::id(),
            std::thread::current().id()));
        std::fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", &root); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let filesystem = crate::sarunfs::SarunFs::new(root.clone());
        let mut sockets = [-1; 2];
        assert_eq!(unsafe {
            libc::socketpair(libc::AF_UNIX,
                             libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                             0, sockets.as_mut_ptr())
        }, 0);
        let server = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
        let client = unsafe { OwnedFd::from_raw_fd(sockets[1]) };
        let worker = std::thread::spawn(move || serve_fd_lane(filesystem, server));
        let request = FdRequest {
            magic: FD_MAGIC,
            version: FD_VERSION,
            operation: FD_EXPORT,
            request_id: 44,
            handle: u64::MAX,
            flags: 0,
            caller_pid: std::process::id(),
        };
        assert_eq!(unsafe {
            libc::send(client.as_raw_fd(), std::ptr::from_ref(&request).cast(),
                       size_of::<FdRequest>(), 0)
        }, size_of::<FdRequest>() as isize);
        let mut response = FdResponse::default();
        assert_eq!(unsafe {
            libc::recv(client.as_raw_fd(), std::ptr::addr_of_mut!(response).cast(),
                       size_of::<FdResponse>(), 0)
        }, size_of::<FdResponse>() as isize);
        assert_eq!(response.request_id, 44);
        assert_eq!(response.error, -libc::EBADF);
        drop(client);
        worker.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
