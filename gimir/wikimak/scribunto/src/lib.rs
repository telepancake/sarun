//! Scribunto: {{#invoke:Module|fn}} via embedded PUC Lua 5.1 (plan
//! §3.3), engine choice (a) — vendored PUC Lua 5.1 via mlua, for exact
//! pattern/number semantics (mw.ustring is built on Lua patterns, the
//! single biggest fidelity risk of any reimplementation).
//!
//! State-lifetime choice: a FRESH Lua state per invoke. This gives each
//! invocation clean, independent memory and instruction budgets (the
//! infinite-loop / OOM guards can't leak across invokes) and clean
//! sandbox teardown. Module SOURCE is cached per `LuaInvoker` instance
//! (Rust-side, keyed by module name) so repeated invokes on one render
//! don't re-hit the store at τ; within a single invoke, `require`
//! additionally caches compiled module TABLES (diamond-dependency dedup).
//! A `LuaInvoker` is therefore per-render (per-τ): its source cache is
//! only valid for one τ.
//!
//! Failure discipline (plan §3): every error path — missing module,
//! non-table return, Lua runtime error, memory limit, instruction budget
//! — returns `Err(String)`, which the renderer shows as an inline
//! script-error box. Nothing panics; nothing is silently dropped.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Error as LuaError, HookTriggers, Lua, Table, Value, VmState};
use wikimak_wikitext::{Frame, ModuleInvoker, PageStore};

mod datetime;
mod hash;
mod lua_src;
mod mwlib;
mod sandbox;

use mwlib::Ctx;

/// 50 MB, matching Scribunto's default Lua memory limit (plan §3.3).
const DEFAULT_MEMORY_LIMIT: usize = 50 * 1024 * 1024;
/// ~7 s of PUC Lua at a few hundred M instr/s: the CPU-time analogue is an
/// instruction budget. Deliberately coarse.
const DEFAULT_INSTRUCTION_BUDGET: u32 = 400_000_000;
/// Wall-clock backstop for the invoked function. Independent of the
/// instruction budget: even if instruction counting under-approximates the
/// real cost, no invoke runs past this. Set well above the instruction
/// budget's expected runtime so it never pre-empts normal execution.
const DEFAULT_TIME_LIMIT: Duration = Duration::from_secs(15);
/// Instructions between hook firings (wall-clock + budget checks). Small
/// enough for ~ms wall-clock resolution, large enough that metering a
/// normal invoke stays cheap.
const HOOK_INTERVAL: u32 = 1_000_000;
/// Message raised when either guard trips. Contains "time limit exceeded"
/// so [`script_error_line`] surfaces it as the real cause.
const LIMIT_MESSAGE: &str =
    "Lua time limit exceeded: the invoked function ran too long or exceeded its instruction budget";

pub struct LuaInvoker {
    memory_limit: usize,
    instruction_budget: u32,
    time_limit: Duration,
    logs: RefCell<Vec<String>>,
    source_cache: RefCell<HashMap<String, Option<String>>>,
}

impl LuaInvoker {
    pub fn new() -> Result<Self, String> {
        Ok(Self::with_limits(DEFAULT_MEMORY_LIMIT, DEFAULT_INSTRUCTION_BUDGET))
    }

    /// Construct with explicit budgets. Tests use small budgets so the
    /// runaway-loop and out-of-memory guards fire in milliseconds.
    pub fn with_limits(memory_limit: usize, instruction_budget: u32) -> Self {
        LuaInvoker {
            memory_limit,
            instruction_budget,
            time_limit: DEFAULT_TIME_LIMIT,
            logs: RefCell::new(Vec::new()),
            source_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Override the wall-clock backstop (tests use a small limit so the
    /// time guard fires in milliseconds).
    pub fn with_time_limit(mut self, time_limit: Duration) -> Self {
        self.time_limit = time_limit;
        self
    }

    /// Debug console output (`mw.log` / `mw.logObject`) collected across
    /// this invoker's invokes, in order.
    pub fn logs(&self) -> Vec<String> {
        self.logs.borrow().clone()
    }

    pub fn clear_logs(&self) {
        self.logs.borrow_mut().clear();
    }

    fn run(
        &self,
        module: &str,
        function: &str,
        frame: &Frame,
        store: &dyn PageStore,
    ) -> Result<String, LuaError> {
        let tau_secs = store.timestamp_micros().div_euclid(1_000_000);
        let ctx = Ctx {
            store,
            invoker: self,
            site: store.site(),
            tau_secs,
            current_title: frame.title.clone(),
            logs: &self.logs,
            source_cache: &self.source_cache,
        };

        let lua = Lua::new();
        sandbox::apply(&lua, tau_secs)?;

        let memory_limit = self.memory_limit;
        let budget = self.instruction_budget;
        let deadline = Instant::now() + self.time_limit;

        lua.scope(|scope| {
            let main_frame = mwlib::install(&lua, scope, &ctx, frame)?;
            install_require(&lua, scope, &ctx)?;

            // Load the entry module (its top-level code runs UNMETERED —
            // the budget guards the invoked function, matching how a
            // runaway loop in module logic manifests).
            let module_table = load_entry_module(&lua, &ctx, module)?;
            let func: Value = module_table.get(function)?;
            let func = match func {
                Value::Function(f) => f,
                _ => {
                    return Err(LuaError::RuntimeError(format!(
                        "Script error: The function \"{function}\" does not exist in module \"{module}\"."
                    )))
                }
            };

            // Meter the module function only. The hook fires periodically
            // and enforces BOTH the instruction budget (a running total, so
            // work hidden inside pcall still counts) and the wall-clock
            // deadline.
            //
            // The error a hook raises is an ordinary catchable Lua error: a
            // module can wrap risky work in `pcall` and swallow it, then loop
            // again — the exact bypass that makes a plain periodic error no
            // backstop at all. So once EITHER guard trips, the hook re-arms
            // itself to fire every single instruction with a killer that
            // ALWAYS errors. From then on, the moment control returns to any
            // frame outside a pcall (which a runaway loop must reach, since a
            // pcall that catches the error then returns to its caller), the
            // error re-raises and escapes to this Rust caller within a couple
            // of instructions. pcall can no longer make forward progress.
            let _ = lua.set_memory_limit(memory_limit);
            let interval = budget.min(HOOK_INTERVAL).max(1);
            let used = Rc::new(Cell::new(0u64));
            lua.set_hook(
                HookTriggers::new().every_nth_instruction(interval),
                move |lua, _debug| {
                    let total = used.get().saturating_add(interval as u64);
                    used.set(total);
                    if total >= budget as u64 || Instant::now() >= deadline {
                        lua.set_hook(
                            HookTriggers::new().every_nth_instruction(1),
                            |_lua, _debug| {
                                Err::<VmState, LuaError>(LuaError::RuntimeError(
                                    LIMIT_MESSAGE.to_string(),
                                ))
                            },
                        );
                        return Err(LuaError::RuntimeError(LIMIT_MESSAGE.to_string()));
                    }
                    Ok(VmState::Continue)
                },
            );

            let ret: Value = func.call(main_frame)?;
            lua.remove_hook();
            coerce_return(ret)
        })
    }
}

impl Default for LuaInvoker {
    fn default() -> Self {
        Self::with_limits(DEFAULT_MEMORY_LIMIT, DEFAULT_INSTRUCTION_BUDGET)
    }
}

impl ModuleInvoker for LuaInvoker {
    fn invoke(
        &self,
        module: &str,
        function: &str,
        frame: &Frame,
        store: &dyn PageStore,
    ) -> Result<String, String> {
        self.run(module, function, frame, store)
            .map_err(|e| format_error(&e, module, function))
    }
}

fn fetch_source(ctx: &Ctx, name: &str) -> Option<String> {
    let title = mwlib::module_title(name);
    let key = title.text.clone();
    if let Some(cached) = ctx.source_cache.borrow().get(&key) {
        return cached.clone();
    }
    let src = ctx.store.page_text(&title);
    ctx.source_cache.borrow_mut().insert(key, src.clone());
    src
}

/// Store-backed `require` restricted to `Module:` pages, with a per-invoke
/// compiled-table cache. Replaces PUC's filesystem `require` (removed in
/// the sandbox) — the sanctioned Scribunto model.
fn install_require<'scope, 'env, 'a>(
    lua: &'scope Lua,
    scope: &'scope mlua::Scope<'scope, 'env>,
    ctx: &'a Ctx<'a>,
) -> mlua::Result<()>
where
    'a: 'scope,
{
    lua.globals().set("__loaded", lua.create_table()?)?;
    let require = scope.create_function(move |lua, name: String| {
        let loaded: Table = lua.globals().get("__loaded")?;
        let cached: Value = loaded.get(name.clone())?;
        if !matches!(cached, Value::Nil) {
            return Ok(cached);
        }
        let src = fetch_source(ctx, &name).ok_or_else(|| {
            LuaError::RuntimeError(format!("Script error: No such module \"{name}\"."))
        })?;
        let value: Value = lua
            .load(&src)
            .set_name(name.clone())
            .eval()
            .map_err(|e| LuaError::RuntimeError(format!("Script error in {name}: {e}")))?;
        loaded.set(name, value.clone())?;
        Ok(value)
    })?;
    lua.globals().set("require", require)?;
    Ok(())
}

fn load_entry_module(lua: &Lua, ctx: &Ctx, module: &str) -> mlua::Result<Table> {
    let src = fetch_source(ctx, module)
        .ok_or_else(|| LuaError::RuntimeError(format!("Script error: No such module \"{module}\".")))?;
    let value: Value = lua
        .load(&src)
        .set_name(format!("Module:{module}"))
        .eval()
        .map_err(|e| LuaError::RuntimeError(format!("Script error: Lua error loading Module:{module}: {e}")))?;
    match value {
        Value::Table(t) => Ok(t),
        other => Err(LuaError::RuntimeError(format!(
            "Script error: Module:{module} returned a {} value; it must return a table.",
            other.type_name()
        ))),
    }
}

fn coerce_return(v: Value) -> mlua::Result<String> {
    Ok(match v {
        Value::String(s) => s.to_str()?.to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => String::new(),
        other => {
            return Err(LuaError::RuntimeError(format!(
                "Script error: the invoked function returned a {} value; it must return a string.",
                other.type_name()
            )))
        }
    })
}

/// Flatten an mlua error into the single-line script-error string the
/// renderer shows. Memory-limit hits are normalized so they read as such
/// regardless of which allocation tripped.
fn format_error(e: &LuaError, module: &str, function: &str) -> String {
    let text = e.to_string();
    let mem = matches!(e, LuaError::MemoryError(_))
        || text.contains("not enough memory")
        || text.contains("memory allocation");
    if mem {
        return format!(
            "Script error: Module:{module} function \"{function}\" exceeded the Lua memory limit."
        );
    }
    if let Some(msg) = script_error_line(&text) {
        return msg;
    }
    format!("Script error in Module:{module} (\"{function}\"): {}", first_line(&text))
}

/// Pull the "Script error…"/"Lua …limit…" clause out of mlua's wrapped,
/// traceback-bearing message so the box shows the real cause.
fn script_error_line(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(idx) = line.find("Script error") {
            return Some(line[idx..].trim_end().to_string());
        }
        if line.contains("time limit exceeded") {
            let idx = line.find("Lua time limit").unwrap_or(0);
            return Some(line[idx..].trim_end().to_string());
        }
    }
    None
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_string()
}
