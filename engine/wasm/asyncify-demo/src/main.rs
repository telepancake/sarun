use wasmi::{Caller, Engine, Extern, Linker, Memory, Module, Store};

const DATA_PTR: i32 = 16;       // asyncify control struct lives here
const DATA_START: i32 = 24;     // usable stack region begins
const DATA_END: i32 = 1024;     // ...and ends

// host state: how many times op was entered, for narration
struct HostState { op_calls: u32 }

fn call_i32(caller: &mut Caller<HostState>, name: &str) -> i32 {
    let f = match caller.get_export(name) { Some(Extern::Func(f)) => f, _ => panic!("no {name}") };
    let f = f.typed::<(), i32>(&caller).unwrap();
    f.call(&mut *caller, ()).unwrap()
}
fn call_void_i32(caller: &mut Caller<HostState>, name: &str, arg: i32) {
    let f = match caller.get_export(name) { Some(Extern::Func(f)) => f, _ => panic!("no {name}") };
    let f = f.typed::<i32, ()>(&caller).unwrap();
    f.call(&mut *caller, arg).unwrap()
}

fn main() {
    let engine = Engine::default();
    let wasm = std::fs::read("run.wasm").unwrap();
    let module = Module::new(&engine, &wasm[..]).unwrap();
    let mut store = Store::new(&engine, HostState { op_calls: 0 });
    let mut linker: Linker<HostState> = Linker::new(&engine);

    // The single host import. It SUSPENDS the guest on the first entry and
    // returns the real value (42) when the guest is rewound back into it.
    linker.func_wrap("host", "op", |mut caller: Caller<HostState>| -> i32 {
        caller.data_mut().op_calls += 1;
        let n = caller.data().op_calls;
        let state = call_i32(&mut caller, "asyncify_get_state"); // 0 normal,1 unwind,2 rewind
        println!("  [host.op] entry #{n}, asyncify_state={state}");
        if state == 2 {
            // We are being rewound back into the import: stop rewinding, return the real result.
            call_i32_void(&mut caller, "asyncify_stop_rewind");
            println!("  [host.op] rewound -> returning real value 42");
            return 42;
        }
        // First entry: write the asyncify control struct and begin unwinding.
        let mem = match caller.get_export("memory") { Some(Extern::Memory(m)) => m, _ => panic!("no mem") };
        write_i32(&mut caller, mem, DATA_PTR, DATA_START);
        write_i32(&mut caller, mem, DATA_PTR + 4, DATA_END);
        call_void_i32(&mut caller, "asyncify_start_unwind", DATA_PTR);
        println!("  [host.op] started unwind (suspending guest), returning dummy 0");
        0
    }).unwrap();

    let instance = linker.instantiate_and_start(&mut store, &module).unwrap();
    let run = instance.get_typed_func::<(), i32>(&store, "run").unwrap();
    let get_state = instance.get_typed_func::<(), i32>(&store, "asyncify_get_state").unwrap();
    let stop_unwind = instance.get_typed_func::<(), ()>(&store, "asyncify_stop_unwind").unwrap();
    let start_rewind = instance.get_typed_func::<i32, ()>(&store, "asyncify_start_rewind").unwrap();

    println!("[host] call run() -> expect suspend");
    let r1 = run.call(&mut store, ()).unwrap();
    println!("[host] run() returned {r1} (dummy from unwind); state={}", get_state.call(&mut store, ()).unwrap());

    println!("[host] ...do async work... then resume");
    stop_unwind.call(&mut store, ()).unwrap();
    start_rewind.call(&mut store, DATA_PTR).unwrap();
    let r2 = run.call(&mut store, ()).unwrap();
    println!("[host] run() resumed and returned {r2}");
    assert_eq!(r2, 42, "expected 42 after resume");
    println!("SUCCESS: suspended at host import and resumed under wasmi");
}

fn call_i32_void(caller: &mut Caller<HostState>, name: &str) {
    let f = match caller.get_export(name) { Some(Extern::Func(f)) => f, _ => panic!("no {name}") };
    let f = f.typed::<(), ()>(&caller).unwrap();
    f.call(&mut *caller, ()).unwrap()
}
fn write_i32(caller: &mut Caller<HostState>, mem: Memory, off: i32, val: i32) {
    mem.write(&mut *caller, off as usize, &val.to_le_bytes()).unwrap();
}
