//! Pass a *running* wasm function between separate OS processes (and namespaces)
//! via the single backing file. `save` runs until a host import unwinds it, writes
//! the linear memory to the file, exits. `load` — a different process — restores
//! the file into a fresh instance, rewinds, and the guest finishes there.
//!
//! Proves the migration primitive: the continuation travels in the file; any
//! process that maps it resumes it. The resuming process runs its OWN host imports
//! (so in sarun the import executes in that process's namespace).

use wasmi::{Caller, Engine, Extern, Linker, Memory, Module, Store};

const DATA_PTR: i32 = 16;
const DATA_START: i32 = 24;
const DATA_END: i32 = 1024;

fn ci32(c: &mut Caller<()>, n: &str) -> i32 {
    let Extern::Func(f) = c.get_export(n).unwrap() else { panic!() };
    f.typed::<(), i32>(&c).unwrap().call(&mut *c, ()).unwrap()
}
fn cvoid(c: &mut Caller<()>, n: &str) {
    let Extern::Func(f) = c.get_export(n).unwrap() else { panic!() };
    f.typed::<(), ()>(&c).unwrap().call(&mut *c, ()).unwrap()
}
fn cvoid_i(c: &mut Caller<()>, n: &str, a: i32) {
    let Extern::Func(f) = c.get_export(n).unwrap() else { panic!() };
    f.typed::<i32, ()>(&c).unwrap().call(&mut *c, a).unwrap()
}

fn link(l: &mut Linker<()>) {
    l.func_wrap("host", "op", |mut c: Caller<()>| -> i32 {
        if ci32(&mut c, "asyncify_get_state") == 2 {
            cvoid(&mut c, "asyncify_stop_rewind");
            return 42;
        }
        let Extern::Memory(m) = c.get_export("memory").unwrap() else { panic!() };
        m.write(&mut c, DATA_PTR as usize, &DATA_START.to_le_bytes()).unwrap();
        m.write(&mut c, (DATA_PTR + 4) as usize, &DATA_END.to_le_bytes()).unwrap();
        cvoid_i(&mut c, "asyncify_start_unwind", DATA_PTR);
        0
    }).unwrap();
}

fn instance(engine: &Engine, store: &mut Store<()>, wasm: &[u8]) -> wasmi::Instance {
    let module = Module::new(engine, wasm).unwrap();
    let mut l = Linker::new(engine);
    link(&mut l);
    l.instantiate_and_start(store, &module).unwrap()
}
fn mem(store: &Store<()>, inst: &wasmi::Instance) -> Memory {
    match inst.get_export(store, "memory").unwrap() { Extern::Memory(m) => m, _ => panic!() }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let file = std::env::args().nth(2).unwrap_or_else(|| "/tmp/sarun-migrate.bin".into());
    let wasm = std::fs::read("run.wasm").unwrap();
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let inst = instance(&engine, &mut store, &wasm);

    match mode.as_str() {
        "save" => {
            let run = inst.get_typed_func::<(), i32>(&store, "run").unwrap();
            let _ = run.call(&mut store, ()).unwrap(); // unwinds, returns dummy
            let bytes = mem(&store, &inst).data(&store).to_vec();
            std::fs::write(&file, &bytes).unwrap();
            println!("[save pid {}] parked running guest into {file} ({} bytes)", std::process::id(), bytes.len());
        }
        "load" => {
            let saved = std::fs::read(&file).unwrap();
            mem(&store, &inst).write(&mut store, 0, &saved).unwrap();
            inst.get_typed_func::<i32, ()>(&store, "asyncify_start_rewind").unwrap()
                .call(&mut store, DATA_PTR).unwrap();
            let code = inst.get_typed_func::<(), i32>(&store, "run").unwrap()
                .call(&mut store, ()).unwrap();
            println!("[load pid {}] resumed from {file} -> run() returned {code}", std::process::id());
            assert_eq!(code, 42);
            println!("MIGRATE-OK");
        }
        _ => { eprintln!("usage: migrate save|load <file>"); std::process::exit(2); }
    }
}
