//! Single-backing-file suspend/resume across a FULL teardown.
//!
//! Phase A: run the guest until a host import unwinds it (parked at a clean
//! boundary, all state in linear memory), then write the entire linear memory to
//! ONE file and drop the engine/store/instance completely.
//! Phase B: a fresh engine/store/instance, restore that file into the new memory,
//! `asyncify_start_rewind`, re-enter — the guest resumes and finishes.
//!
//! For one process the checkpoint is just its linear memory (+ mutable globals
//! for real programs; the demo has none). For N processes the file gains a
//! per-process section and the host fd/pipe state, written only after the barrier
//! (all processes unwound). This is the single mechanism, proven on one process.

use wasmi::{Caller, Engine, Extern, Linker, Memory, Module, Store};

const DATA_PTR: i32 = 16;
const DATA_START: i32 = 24;
const DATA_END: i32 = 1024;
const SNAP: &str = "/tmp/sarun-snap.bin";

#[derive(Default)]
struct H { unwound: bool }

fn call_i32(c: &mut Caller<H>, n: &str) -> i32 {
    let Extern::Func(f) = c.get_export(n).unwrap() else { panic!() };
    f.typed::<(), i32>(&c).unwrap().call(&mut *c, ()).unwrap()
}
fn call_void(c: &mut Caller<H>, n: &str) {
    let Extern::Func(f) = c.get_export(n).unwrap() else { panic!() };
    f.typed::<(), ()>(&c).unwrap().call(&mut *c, ()).unwrap()
}
fn call_void_i32(c: &mut Caller<H>, n: &str, a: i32) {
    let Extern::Func(f) = c.get_export(n).unwrap() else { panic!() };
    f.typed::<i32, ()>(&c).unwrap().call(&mut *c, a).unwrap()
}

// host.op: first entry starts an unwind (suspend); on rewind it returns 42.
fn link(linker: &mut Linker<H>) {
    linker.func_wrap("host", "op", |mut c: Caller<H>| -> i32 {
        let state = call_i32(&mut c, "asyncify_get_state"); // 0 normal, 2 rewind
        if state == 2 {
            call_void(&mut c, "asyncify_stop_rewind");
            return 42;
        }
        let Extern::Memory(mem) = c.get_export("memory").unwrap() else { panic!() };
        mem.write(&mut c, DATA_PTR as usize, &DATA_START.to_le_bytes()).unwrap();
        mem.write(&mut c, (DATA_PTR + 4) as usize, &DATA_END.to_le_bytes()).unwrap();
        call_void_i32(&mut c, "asyncify_start_unwind", DATA_PTR);
        c.data_mut().unwound = true;
        0
    }).unwrap();
}

fn mem(store: &Store<H>, inst: &wasmi::Instance) -> Memory {
    match inst.get_export(store, "memory").unwrap() { Extern::Memory(m) => m, _ => panic!() }
}

fn main() {
    let wasm = std::fs::read("run.wasm").expect("run.wasm (build it: wasm-opt --asyncify ...)");

    // ── Phase A: run, unwind, snapshot the whole linear memory to ONE file ──
    {
        let engine = Engine::default();
        let module = Module::new(&engine, &wasm[..]).unwrap();
        let mut store = Store::new(&engine, H::default());
        let mut linker = Linker::new(&engine);
        link(&mut linker);
        let inst = linker.instantiate_and_start(&mut store, &module).unwrap();
        let run = inst.get_typed_func::<(), i32>(&store, "run").unwrap();
        let _ = run.call(&mut store, ()).unwrap(); // returns (dummy) after unwinding
        assert!(store.data().unwound, "guest should have unwound");
        let m = mem(&store, &inst);
        let bytes = m.data(&store).to_vec();           // ENTIRE linear memory
        std::fs::write(SNAP, &bytes).unwrap();
        println!("[A] guest suspended at host.op; wrote {} bytes -> {SNAP}", bytes.len());
        // engine/store/instance all dropped here — full teardown.
    }

    // ── Phase B: fresh everything, restore the file, rewind, finish ─────────
    let code = {
        let engine = Engine::default();
        let module = Module::new(&engine, &wasm[..]).unwrap();
        let mut store = Store::new(&engine, H::default());
        let mut linker = Linker::new(&engine);
        link(&mut linker);
        let inst = linker.instantiate_and_start(&mut store, &module).unwrap();
        let m = mem(&store, &inst);
        let saved = std::fs::read(SNAP).unwrap();
        m.write(&mut store, 0, &saved).unwrap();        // restore full memory
        println!("[B] fresh instance; restored {} bytes; rewinding", saved.len());
        let start_rewind = inst.get_typed_func::<i32, ()>(&store, "asyncify_start_rewind").unwrap();
        start_rewind.call(&mut store, DATA_PTR).unwrap();
        let run = inst.get_typed_func::<(), i32>(&store, "run").unwrap();
        run.call(&mut store, ()).unwrap()
    };
    println!("[B] resumed across teardown -> run() returned {code}");
    assert_eq!(code, 42);
    println!("SUCCESS: single-file suspend/resume across full teardown");
}
