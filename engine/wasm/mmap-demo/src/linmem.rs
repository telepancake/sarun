//! Prove the keystone: a wasm guest's linear memory can BE an mmap'd file,
//! through wasmi's PUBLIC `Memory::new_static` API — zero-copy, no wasmi patch.
//!
//! The module imports its memory (`env.memory`); the host mmaps the backing file
//! and hands that buffer to `Memory::new_static`, then defines it as the import.
//! The guest's stores land directly in the file. (A real blob is built with
//! `wasm-ld --import-memory` to get the imported-memory shape.)

use wasmi::{Engine, Linker, Memory, MemoryType, Module, Store};
use std::os::fd::AsRawFd;

// Guest imports its memory and writes a known word at offset 100.
const WAT: &str = r#"
(module
  (import "env" "memory" (memory 1))
  (func (export "poke")
    (i32.store (i32.const 100) (i32.const 0x41424344))))
"#;

fn main() {
    let path = "/tmp/sarun-linmem.bin";
    let f = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).truncate(true).open(path).unwrap();
    let len = 64 * 1024; // 1 wasm page; size to max pages (sparse) for real blobs
    f.set_len(len as u64).unwrap();
    let p = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
                   libc::MAP_SHARED, f.as_raw_fd(), 0)
    };
    assert!(p != libc::MAP_FAILED, "mmap failed");
    // The mmap region IS the linear memory backing. 'static: it outlives the store
    // (the engine holds the mapping for the box's lifetime / reconstructs on resume).
    let buf: &'static mut [u8] = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, len) };

    let engine = Engine::default();
    let module = Module::new(&engine, WAT.as_bytes()).unwrap();
    let mut store = Store::new(&engine, ());
    let ty = MemoryType::new(1, Some(1));
    let mem = Memory::new_static(&mut store, ty, buf).unwrap();
    let mut linker: Linker<()> = Linker::new(&engine);
    linker.define("env", "memory", mem).unwrap();
    let inst = linker.instantiate_and_start(&mut store, &module).unwrap();
    inst.get_typed_func::<(), ()>(&store, "poke").unwrap().call(&mut store, ()).unwrap();
    unsafe { libc::msync(p, len, libc::MS_SYNC); }

    // Read the FILE (not the wasm memory) — proves the guest wrote straight into it.
    let on_disk = std::fs::read(path).unwrap();
    println!("file[100..104] = {:02X?}  (guest stored 0x41424344 LE)", &on_disk[100..104]);
    assert_eq!(&on_disk[100..104], &[0x44, 0x43, 0x42, 0x41]);
    println!("SUCCESS: wasmi linear memory IS the mmap'd file (zero-copy via public new_static)");
}
