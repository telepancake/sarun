// Prove: a file-backed MAP_SHARED mmap used AS-IS as live storage, made sparse by
// punching holes in the backing for unused regions. No serialization.
use std::os::fd::AsRawFd;
const PAGE: usize = 4096;
const LEN: usize = 64 * PAGE; // 256 KiB address window, but only touched pages cost storage

fn blocks(path: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().blocks() // 512-byte units actually allocated
}

fn main() {
    let path = "/tmp/sarun-backing.bin";
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).unwrap();
    f.set_len(LEN as u64).unwrap(); // sparse: logical size set, no blocks yet
    let fd = f.as_raw_fd();
    println!("after ftruncate {LEN}B: {} blocks allocated (sparse)", blocks(path));

    let p = unsafe {
        libc::mmap(std::ptr::null_mut(), LEN, libc::PROT_READ|libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
    };
    assert!(p != libc::MAP_FAILED, "mmap failed");
    let base = p as *mut u8;

    // Write a pattern into page 0 and page 40 (as if guest linear memory touched them).
    unsafe {
        std::ptr::write_bytes(base.add(0*PAGE), 0xAB, PAGE);
        std::ptr::write_bytes(base.add(40*PAGE), 0xCD, PAGE);
        libc::msync(p, LEN, libc::MS_SYNC);
    }
    println!("after touching 2 pages:   {} blocks (only touched pages cost storage)", blocks(path));

    // Punch a hole at page 40 — release its backing storage; reads must see zeros.
    unsafe {
        let r = libc::fallocate(fd, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                                (40*PAGE) as libc::off_t, PAGE as libc::off_t);
        assert_eq!(r, 0, "fallocate punch hole failed: {}", std::io::Error::last_os_error());
        // also drop the now-stale page from this mapping's cache
        libc::madvise(base.add(40*PAGE) as *mut libc::c_void, PAGE, libc::MADV_DONTNEED);
    }
    println!("after punching page 40:   {} blocks (storage released)", blocks(path));

    // page 0 content survives as-is; page 40 now reads back as zeros.
    let p0 = unsafe { *base.add(0*PAGE) };
    let p40 = unsafe { *base.add(40*PAGE) };
    println!("page0[0]=0x{p0:02X} (kept), page40[0]=0x{p40:02X} (punched->zero)");

    // Resume semantics: unmap, remap fresh — content persists from the file as-is.
    unsafe { libc::munmap(p, LEN); }
    let p2 = unsafe { libc::mmap(std::ptr::null_mut(), LEN, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
    let base2 = p2 as *const u8;
    let r0 = unsafe { *base2.add(0*PAGE) };
    println!("after munmap+remap: page0[0]=0x{r0:02X} (state persisted, zero-copy)");
    assert_eq!(p0, 0xAB); assert_eq!(p40, 0x00); assert_eq!(r0, 0xAB);
    println!("SUCCESS: file-backed mmap live store, sparse via hole-punch, persists as-is");
}
