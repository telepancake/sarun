//! Host-owned heap using the REAL `dlmalloc` crate (pure-Rust, no C dep), not a
//! hand-rolled allocator. Each process gets a 1 GiB sparse tmpfs slice mmap'd as
//! its linear memory (new_static); the guest's #[global_allocator] calls
//! host_alloc/host_dealloc -> this dlmalloc; dlmalloc's platform hooks
//! (`free`/`free_part`) return freed pages to the OS via fallocate(PUNCH_HOLE) on
//! the tmpfs file. That hook IS the "single line for MADV_REMOVE/punch".
//!
//! Startup: pre-grow the guest memory to a working cap once and punch it, so the
//! region is sparse and dlmalloc never cores into ungrown memory (no grow/zero
//! hazard mid-run). dlmalloc then owns [heap_base, cap).

use std::cell::Cell;
use std::os::fd::AsRawFd;
use dlmalloc::{Allocator, Dlmalloc};
use wasmi::{Caller, Engine, Extern, Linker, Memory, MemoryType, Module, Store, Val};

const PAGE: u64 = 4096;
const WPAGE: u64 = 65536;
const SLICE: u64 = 1 << 30;      // 1 GiB address space per process (sparse)
const GROWN: u64 = 128 << 20;    // pre-grown+punched working cap for this demo
const NPROC: u64 = 2;
const BIG: i32 = 64 * 1024 * 1024;

fn up(x: u64, a: u64) -> u64 { (x + a - 1) / a * a }
fn down(x: u64, a: u64) -> u64 { x / a * a }
fn blocks(p: &str) -> u64 { use std::os::unix::fs::MetadataExt; std::fs::metadata(p).map(|m| m.blocks()).unwrap_or(0) }
fn mib(b: u64) -> u64 { b * 512 / (1024 * 1024) }

fn punch(fd: i32, file_off: u64, off: u64, end: u64) {
    let ps = up(off, PAGE);
    let pe = down(end, PAGE);
    if pe > ps {
        unsafe {
            libc::fallocate(fd, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                            (file_off + ps) as libc::off_t, (pe - ps) as libc::off_t);
        }
    }
}

/// dlmalloc platform: hand it sparse core from the slice; release == punch.
struct SliceAlloc { base: usize, fd: i32, file_off: u64, heap_start: u64, cap: u64, brk: Cell<u64> }

unsafe impl Allocator for SliceAlloc {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        let size = up(size as u64, PAGE);
        let off = up(self.brk.get(), PAGE);
        if off + size > self.cap { return (core::ptr::null_mut(), 0, 0); } // within pre-grown cap
        self.brk.set(off + size);
        ((self.base + off as usize) as *mut u8, size as usize, 0)
    }
    fn remap(&self, _p: *mut u8, _o: usize, _n: usize, _mv: bool) -> *mut u8 { core::ptr::null_mut() }
    fn free_part(&self, ptr: *mut u8, oldsize: usize, newsize: usize) -> bool {
        let off = ptr as usize as u64 - self.base as u64;
        punch(self.fd, self.file_off, off + newsize as u64, off + oldsize as u64);
        true
    }
    fn free(&self, ptr: *mut u8, size: usize) -> bool {
        let off = ptr as usize as u64 - self.base as u64;
        punch(self.fd, self.file_off, off, off + size as u64);
        true
    }
    fn can_release_part(&self, _flags: u32) -> bool { true }
    fn allocates_zeros(&self) -> bool { true } // fresh tmpfs + punched + wasmi-zeroed all read zero
    fn page_size(&self) -> usize { PAGE as usize }
}

struct Proc { base: usize, dl: Dlmalloc<SliceAlloc> }

fn main() {
    let wasm_path = std::env::args().nth(1)
        .unwrap_or_else(|| "../guest/target/wasm32-unknown-unknown/release/heap_guest.wasm".into());
    let wasm = std::fs::read(&wasm_path).expect("guest wasm (build it first)");
    let path = "/dev/shm/sarun-heaps.bin";
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).unwrap();
    f.set_len(NPROC * SLICE).unwrap();
    let fd = f.as_raw_fd();
    let engine = Engine::default();
    println!("file: {} GiB logical, {} MiB blocks (sparse), real allocator = dlmalloc crate",
             NPROC * SLICE >> 30, mib(blocks(path)));

    for i in 0..NPROC {
        let file_off = i * SLICE;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), SLICE as usize, libc::PROT_READ | libc::PROT_WRITE,
                       libc::MAP_SHARED, fd, file_off as libc::off_t)
        };
        assert!(p != libc::MAP_FAILED);
        let base = p as usize;
        let buf: &'static mut [u8] = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, SLICE as usize) };

        let module = Module::new(&engine, &wasm[..]).unwrap();
        // SliceAlloc filled in after we know heap_base.
        let dl = Dlmalloc::new_with_allocator(SliceAlloc {
            base, fd, file_off, heap_start: 0, cap: GROWN, brk: Cell::new(0),
        });
        let mut store = Store::new(&engine, Proc { base, dl });
        let mut linker: Linker<Proc> = Linker::new(&engine);
        linker.func_wrap("env", "host_alloc", |mut c: Caller<Proc>, size: i32, align: i32| -> i32 {
            let base = c.data().base;
            let p = unsafe { c.data_mut().dl.malloc(size as usize, (align.max(1)) as usize) };
            if p.is_null() { return 0; }
            (p as usize - base) as i32
        }).unwrap();
        linker.func_wrap("env", "host_dealloc", |mut c: Caller<Proc>, ptr: i32, size: i32, align: i32| {
            let base = c.data().base;
            unsafe { c.data_mut().dl.free((base + ptr as usize) as *mut u8, size as usize, (align.max(1)) as usize) };
        }).unwrap();

        let ty = MemoryType::new(128, Some(16384)); // 8 MiB min, 1 GiB max; buffer is the 1 GiB mmap
        let mem = Memory::new_static(&mut store, ty, buf).unwrap();
        linker.define("env", "memory", mem).unwrap();
        let inst = linker.instantiate_and_start(&mut store, &module).unwrap();
        let heap_base = match inst.get_export(&store, "__heap_base").unwrap() {
            Extern::Global(g) => match g.get(&store) { Val::I32(v) => v as u64, _ => panic!() }, _ => panic!(),
        };

        // Pre-grow guest memory to the working cap, then punch [heap_base,cap) sparse.
        let cur = mem.size(&store) * WPAGE;
        if GROWN > cur { mem.grow(&mut store, (GROWN - cur) / WPAGE).unwrap(); }
        punch(fd, file_off, heap_base, GROWN);
        // hand dlmalloc its core window
        { let a = store.data().dl.allocator(); a.brk.set(heap_base); /* heap_start implicit via brk */ }

        let alloc_touch = inst.get_typed_func::<i32, i32>(&store, "alloc_touch").unwrap();
        let free_buf = inst.get_typed_func::<(i32, i32), ()>(&store, "free_buf").unwrap();
        println!("\n[proc {i}] heap_base={heap_base:#x}; after pre-grow+punch: {} MiB blocks", mib(blocks(path)));
        let ptr = alloc_touch.call(&mut store, BIG).unwrap();
        println!("[proc {i}] alloc_touch(64 MiB) -> off {ptr:#x}; blocks: {} MiB", mib(blocks(path)));
        free_buf.call(&mut store, (ptr, BIG)).unwrap();
        println!("[proc {i}] free_buf -> dlmalloc free hook punched; blocks: {} MiB", mib(blocks(path)));
    }
    println!("\nfinal: {} MiB blocks for a {} GiB-logical file", mib(blocks(path)), NPROC * SLICE >> 30);
}
