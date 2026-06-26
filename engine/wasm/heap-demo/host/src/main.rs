//! Host owns the heap. A tmpfs file is sliced into per-process 1 GiB chunks; each
//! chunk is mmap'd and handed to wasmi as the instance's linear memory
//! (new_static). The guest's allocator calls host_alloc/host_dealloc, so the host
//! tracks the exact live-set and `fallocate(PUNCH_HOLE)`s freed pages back to the
//! OS. Memory grows lazily within the sparse chunk; only touched pages cost.

use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use wasmi::{Caller, Engine, Extern, Linker, Memory, MemoryType, Module, Store, Val};

const PAGE: u64 = 4096;
const WPAGE: u64 = 65536;          // wasm page
const CHUNK: u64 = 1 << 30;        // 1 GiB per process
const NPROC: u64 = 2;

fn up(x: u64, a: u64) -> u64 { (x + a - 1) / a * a }
fn down(x: u64, a: u64) -> u64 { x / a * a }

/// Host-owned heap over [base, CHUNK) of one process's linear memory.
struct Heap {
    base: u64,
    brk: u64,
    free: BTreeMap<u64, u64>, // offset -> size (within linear memory)
}
impl Heap {
    fn new() -> Self { Heap { base: 0, brk: 0, free: BTreeMap::new() } }
    fn set_base(&mut self, b: u64) { self.base = b; self.brk = b; }
    fn alloc(&mut self, size: u64, align: u64) -> u64 {
        let size = up(size.max(1), 16);
        let align = align.max(16);
        // first-fit reuse
        let mut take = None;
        for (&off, &sz) in self.free.iter() {
            if off % align == 0 && sz >= size { take = Some((off, sz)); break; }
        }
        if let Some((off, sz)) = take {
            self.free.remove(&off);
            if sz > size { self.free.insert(off + size, sz - size); }
            return off;
        }
        let off = up(self.brk, align);
        self.brk = off + size;
        off
    }
    /// Free [off,size); coalesce; return the page-aligned inner range to punch.
    fn dealloc(&mut self, off: u64, size: u64) -> Option<(u64, u64)> {
        let size = up(size.max(1), 16);
        let mut start = off;
        let mut end = off + size;
        if let Some((&lo, &ls)) = self.free.range(..start).next_back() {
            if lo + ls == start { start = lo; self.free.remove(&lo); }
        }
        if let Some(&rs) = self.free.get(&end) {
            let e = end; end += rs; self.free.remove(&e);
        }
        self.free.insert(start, end - start);
        let ps = up(start, PAGE);
        let pe = down(end, PAGE);
        if pe > ps { Some((ps, pe - ps)) } else { None }
    }
}

struct Proc { fd: i32, file_off: u64, map: *mut u8, mem: Option<Memory>, heap: Heap }

fn blocks(path: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map(|m| m.blocks()).unwrap_or(0)
}
fn mib(b: u64) -> u64 { b * 512 / (1024 * 1024) } // 512B blocks -> MiB

fn punch(fd: i32, map: *mut u8, file_off: u64, lin_off: u64, len: u64) {
    unsafe {
        libc::fallocate(fd, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        (file_off + lin_off) as libc::off_t, len as libc::off_t);
        libc::madvise(map.add(lin_off as usize) as *mut libc::c_void, len as usize, libc::MADV_DONTNEED);
    }
}

fn link(linker: &mut Linker<Proc>) {
    linker.func_wrap("env", "host_alloc", |mut c: Caller<Proc>, size: i32, align: i32| -> i32 {
        let off = c.data_mut().heap.alloc(size as u64, align as u64);
        let need = off + up((size as u64).max(1), 16);
        let mem = c.data().mem.unwrap();
        let cur = mem.size(&c) * WPAGE;
        if need > cur {
            let add = up(need - cur, WPAGE) / WPAGE;
            let _ = mem.grow(&mut c, add);
        }
        off as i32
    }).unwrap();
    linker.func_wrap("env", "host_dealloc", |mut c: Caller<Proc>, ptr: i32, size: i32, _align: i32| {
        if let Some((lin_off, len)) = c.data_mut().heap.dealloc(ptr as u64, size as u64) {
            let (fd, fo, map) = { let p = c.data(); (p.fd, p.file_off, p.map) };
            punch(fd, map, fo, lin_off, len);
        }
    }).unwrap();
}

fn main() {
    let wasm_path = std::env::args().nth(1)
        .unwrap_or_else(|| "../guest/target/wasm32-unknown-unknown/release/heap_guest.wasm".into());
    let wasm = std::fs::read(&wasm_path).expect("guest wasm (build it first)");
    let path = "/dev/shm/sarun-heaps.bin";
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).unwrap();
    f.set_len(NPROC * CHUNK).unwrap(); // sparse: 2 GiB logical, 0 blocks
    let fd = f.as_raw_fd();
    println!("file: {} ({} GiB logical, {} blocks)", path, NPROC * CHUNK >> 30, blocks(path));

    let engine = Engine::default();
    let big: i32 = 64 * 1024 * 1024; // 64 MiB allocation per process

    for i in 0..NPROC {
        let file_off = i * CHUNK;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), CHUNK as usize, libc::PROT_READ | libc::PROT_WRITE,
                       libc::MAP_SHARED, fd, file_off as libc::off_t)
        };
        assert!(p != libc::MAP_FAILED, "mmap chunk {i}");
        let buf: &'static mut [u8] = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, CHUNK as usize) };

        let module = Module::new(&engine, &wasm[..]).unwrap();
        let mut store = Store::new(&engine, Proc { fd, file_off, map: p as *mut u8, mem: None, heap: Heap::new() });
        let mut linker: Linker<Proc> = Linker::new(&engine);
        link(&mut linker);
        let ty = MemoryType::new(128, Some(16384)); // 8 MiB min, 1 GiB max
        let mem = Memory::new_static(&mut store, ty, buf).unwrap();
        linker.define("env", "memory", mem).unwrap();
        let inst = linker.instantiate_and_start(&mut store, &module).unwrap();

        let heap_base = match inst.get_export(&store, "__heap_base").unwrap() {
            Extern::Global(g) => match g.get(&store) { Val::I32(v) => v as u64, _ => panic!() },
            _ => panic!(),
        };
        store.data_mut().heap.set_base(heap_base);
        store.data_mut().mem = Some(mem);

        let alloc_touch = inst.get_typed_func::<i32, i32>(&store, "alloc_touch").unwrap();
        let free_buf = inst.get_typed_func::<(i32, i32), ()>(&store, "free_buf").unwrap();

        println!("\n[proc {i}] heap_base={heap_base:#x}; blocks before alloc: {} MiB", mib(blocks(path)));
        let ptr = alloc_touch.call(&mut store, big).unwrap();
        println!("[proc {i}] alloc_touch(64 MiB) -> off {ptr:#x}; blocks now: {} MiB", mib(blocks(path)));
        free_buf.call(&mut store, (ptr, big)).unwrap();
        println!("[proc {i}] free_buf -> punched; blocks now: {} MiB", mib(blocks(path)));
    }
    println!("\nfinal file blocks: {} MiB (logical size still {} GiB, sparse)", mib(blocks(path)), NPROC * CHUNK >> 30);
}
