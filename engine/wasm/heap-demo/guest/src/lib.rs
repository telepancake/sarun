//! Guest whose allocator is the HOST: every alloc/free crosses to host imports,
//! so the host knows the exact live-set and can fallocate-punch freed pages.
//! Memory is imported (--import-memory) so the host backs it with an mmap slice.
#![no_std]
extern crate alloc;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};

extern "C" {
    fn host_alloc(size: usize, align: usize) -> *mut u8;
    fn host_dealloc(ptr: *mut u8, size: usize, align: usize);
    fn host_realloc(ptr: *mut u8, old_size: usize, align: usize, new_size: usize) -> *mut u8;
}
struct HostHeap;
unsafe impl GlobalAlloc for HostHeap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 { host_alloc(l.size(), l.align()) }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { host_dealloc(p, l.size(), l.align()) }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        host_realloc(p, l.size(), l.align(), new)
    }
}
#[global_allocator]
static A: HostHeap = HostHeap;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

/// Allocate `n` bytes and touch every page (so its backing pages fault in).
#[no_mangle]
pub extern "C" fn alloc_touch(n: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(n);
    unsafe { v.set_len(n); }
    let mut i = 0;
    while i < n { v[i] = 0xAB; i += 4096; } // one write per page
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}
/// Free a buffer from `alloc_touch` (drops -> dealloc -> host_dealloc -> punch).
#[no_mangle]
pub extern "C" fn free_buf(p: *mut u8, n: usize) {
    unsafe { drop(Vec::from_raw_parts(p, n, n)); }
}
