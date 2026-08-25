//! Scribunto: {{#invoke:Module|fn}} via embedded PUC Lua 5.1 (plan
//! §3.3), engine choice (a) — vendored PUC Lua 5.1 via mlua, for exact
//! pattern/number semantics (mw.ustring is built on Lua patterns, the
//! single biggest fidelity risk of any reimplementation).
//!
//! State-lifetime choice: one Lua state per `LuaInvoker`, which the serving
//! layer constructs per page render. This lets repeated `#invoke`s share the
//! module `require` cache and initialized module tables while keeping every
//! page render isolated. Rust host callbacks and the current frame are still
//! installed per invocation; the Lua VM is the only page-scoped state.
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
mod bytecode;
mod lua_src;
mod mwlib;
mod sandbox;

pub use bytecode::{
    LuaBytecodeCache, LuaBytecodeCacheStats, LuaChunkRole, LuaModuleSourceScope,
};
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
    // This is a cap on the retained page-scoped VM, not a resettable budget:
    // initialized module tables remain live and count toward it.
    memory_limit: usize,
    instruction_budget: u32,
    time_limit: Duration,
    lua: Lua,
    initialized: Cell<bool>,
    hook_stack: Rc<RefCell<Vec<HookFrame>>>,
    logs: RefCell<Vec<String>>,
    source_cache: RefCell<HashMap<String, Option<String>>>,
    bytecode_cache: LuaBytecodeCache,
    source_scope: Option<LuaModuleSourceScope>,
}

struct HookFrame {
    used: u64,
    deadline: Instant,
    tripped: bool,
}

impl LuaInvoker {
    pub fn new() -> Result<Self, String> {
        Ok(Self::with_limits(DEFAULT_MEMORY_LIMIT, DEFAULT_INSTRUCTION_BUDGET))
    }

    /// Construct with explicit budgets. Tests use small budgets so the
    /// runaway-loop and out-of-memory guards fire in milliseconds.
    pub fn with_limits(memory_limit: usize, instruction_budget: u32) -> Self {
        Self::with_limits_and_cache(
            memory_limit,
            instruction_budget,
            LuaBytecodeCache::new(),
        )
    }

    /// Construct a page-scoped invoker using a cache owned by its server.
    /// Lua values, globals, require tables, callbacks, and τ state remain
    /// private to this invoker; only immutable source bytecode is shared.
    pub fn with_cache(cache: LuaBytecodeCache) -> Self {
        Self::with_limits_and_cache(DEFAULT_MEMORY_LIMIT, DEFAULT_INSTRUCTION_BUDGET, cache)
    }

    /// Construct a page-scoped invoker with generation-scoped current-head
    /// module-source reuse. Mutable Lua values and τ state remain private to
    /// this invoker; the scope only authorizes shared module-source lookups.
    pub fn with_cache_and_source_scope(
        cache: LuaBytecodeCache,
        source_scope: LuaModuleSourceScope,
    ) -> Self {
        Self::with_limits_cache_and_source_scope(
            DEFAULT_MEMORY_LIMIT,
            DEFAULT_INSTRUCTION_BUDGET,
            cache,
            Some(source_scope),
        )
    }

    /// Explicit-budget variant of [`Self::with_cache`].
    pub fn with_limits_and_cache(
        memory_limit: usize,
        instruction_budget: u32,
        bytecode_cache: LuaBytecodeCache,
    ) -> Self {
        Self::with_limits_cache_and_source_scope(
            memory_limit,
            instruction_budget,
            bytecode_cache,
            None,
        )
    }

    fn with_limits_cache_and_source_scope(
        memory_limit: usize,
        instruction_budget: u32,
        bytecode_cache: LuaBytecodeCache,
        source_scope: Option<LuaModuleSourceScope>,
    ) -> Self {
        LuaInvoker {
            memory_limit,
            instruction_budget,
            time_limit: DEFAULT_TIME_LIMIT,
            lua: Lua::new(),
            initialized: Cell::new(false),
            hook_stack: Rc::new(RefCell::new(Vec::new())),
            logs: RefCell::new(Vec::new()),
            source_cache: RefCell::new(HashMap::new()),
            bytecode_cache,
            source_scope,
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

        if !self.initialized.get() {
            self.lua.set_memory_limit(self.memory_limit).map_err(|e| {
                LuaError::RuntimeError(format!("Lua memory limit setup failed: {e}"))
            })?;
            sandbox::apply(&self.lua, tau_secs, &self.bytecode_cache)?;
            self.initialized.set(true);
        }

        // Normalize the invoke frame's title to the module's canonical
        // prefixed title. MediaWiki resolves `{{#invoke:citation/CS1|…}}` to
        // `Module:Citation/CS1`, and `frame:getTitle()` returns THAT — modules
        // rely on it (CS1 does `getTitle():gsub('^Module:Citation/CS1','')` to
        // find its own subpage suffix; the raw lowercased title would leave the
        // pattern unmatched and mangle every submodule path). The frozen
        // preprocessor hands us the raw name, so we canonicalize here.
        let canonical_title = mwlib::module_title(module).canonical_prefixed(store.site());
        // `frame` is the calling frame.  The article page is the outermost
        // frame, while the module frame itself remains the title exposed by
        // frame:getTitle().  Keeping those meanings separate matters to
        // hatnote/citation modules, which use mw.title.getCurrentTitle() to
        // inspect the page being rendered.
        let mut current_frame = frame;
        while let Some(parent) = current_frame.parent.as_deref() {
            current_frame = parent;
        }
        let current_title = mwlib::parse_title(&current_frame.title, store.site(), 0)
            .canonical_prefixed(store.site());
        let mut frame = frame.clone();
        frame.title = canonical_title.clone();
        let frame = &frame;

        let ctx = Ctx {
            store,
            invoker: self,
            site: store.site(),
            tau_secs,
            current_title,
            logs: &self.logs,
            source_cache: &self.source_cache,
            bytecode_cache: &self.bytecode_cache,
            source_scope: self.source_scope.as_ref(),
        };

        let lua = &self.lua;
        let globals = lua.globals();
        let previous_host: Value = globals.get("__sarun_current_host")?;
        let previous_frame: Value = globals.get("__sarun_current_frame")?;
        let previous_methods: Value = globals.get("__frame_methods")?;
        self.hook_stack.borrow_mut().push(HookFrame {
            used: 0,
            deadline: Instant::now() + self.time_limit,
            tripped: false,
        });

        let result = lua.scope(|scope| {
            let result = (|| {
                lua.set_memory_limit(self.memory_limit).map_err(|e| {
                    LuaError::RuntimeError(format!("Lua memory limit setup failed: {e}"))
                })?;
                let main_frame = mwlib::install(lua, scope, &ctx, frame)?;

                // Loading the entry module is deliberately outside the
                // invocation hook. Its top-level initialization is part of
                // module setup; the called function is what the per-invoke
                // instruction/time limits meter.
                let module_table = load_entry_module(lua, module)?;
                let func: Value = module_table.get(function)?;
                let func = match func {
                    Value::Function(f) => f,
                    _ => {
                        return Err(LuaError::RuntimeError(format!(
                            "Script error: The function \"{function}\" does not exist in module \"{module}\"."
                        )))
                    }
                };

                install_hook(lua, &self.hook_stack, self.instruction_budget)?;
                let ret: Value = func.call(main_frame)?;
                coerce_return(ret)
            })();

            // Scoped Rust callbacks must not survive this invocation. Restore
            // the caller's host/frame before the Scope is dropped; this is
            // also what makes nested frame:preprocess/#invoke safe.
            let restore = (|| {
                globals.set("__sarun_current_host", previous_host.clone())?;
                globals.set("__sarun_current_frame", previous_frame.clone())?;
                globals.set("frame", previous_frame.clone())?;
                globals.set("__frame_methods", previous_methods.clone())?;
                Ok::<(), LuaError>(())
            })();
            match (result, restore) {
                (_, Err(e)) => Err(e),
                (result, Ok(())) => result,
            }
        });

        self.hook_stack.borrow_mut().pop();
        if let Some(previous) = self.hook_stack.borrow().last() {
            if previous.tripped {
                install_killer(lua)?;
            } else {
                install_hook(lua, &self.hook_stack, self.instruction_budget)?;
            }
        } else {
            lua.remove_hook();
        }
        result
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

fn install_hook(
    lua: &Lua,
    stack: &Rc<RefCell<Vec<HookFrame>>>,
    budget: u32,
) -> mlua::Result<()> {
    let interval = budget.min(HOOK_INTERVAL).max(1);
    let stack = Rc::clone(stack);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(interval),
        move |lua, _debug| {
            let trip = {
                let mut stack = stack.borrow_mut();
                let Some(current) = stack.last_mut() else {
                    return Ok(VmState::Continue);
                };
                let total = current.used.saturating_add(interval as u64);
                current.used = total;
                total >= budget as u64 || Instant::now() >= current.deadline
            };
            if trip {
                if let Some(current) = stack.borrow_mut().last_mut() {
                    current.tripped = true;
                }
                install_killer(lua)?;
                return Err(LuaError::RuntimeError(LIMIT_MESSAGE.to_string()));
            }
            Ok(VmState::Continue)
        },
    );
    Ok(())
}

fn install_killer(lua: &Lua) -> mlua::Result<()> {
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(1),
        |_lua, _debug| Err::<VmState, LuaError>(LuaError::RuntimeError(LIMIT_MESSAGE.to_string())),
    );
    Ok(())
}

fn load_entry_module(lua: &Lua, module: &str) -> mlua::Result<Table> {
    let require: mlua::Function = lua.globals().get("require")?;
    let value: Value = require.call(module.to_string())?;
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
        // Lenient decode: a module that sliced a multibyte value mid-character
        // can return bytes that aren't valid UTF-8. The output feeds HTML
        // (which must be UTF-8), so substitute U+FFFD rather than failing the
        // whole invoke — the citation renders with one glyph mangled instead
        // of vanishing into a script-error box.
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => String::new(),
        Value::Table(t) => {
            let tostring = t
                .metatable()
                .and_then(|mt| mt.raw_get::<Value>("__tostring").ok())
                .and_then(|v| match v {
                    Value::Function(f) => Some(f),
                    _ => None,
                });
            match tostring {
                Some(f) => f.call::<String>(t)?,
                None => {
                    return Err(LuaError::RuntimeError(
                        "Script error: the invoked function returned a table value; it must return a string."
                            .to_string(),
                    ))
                }
            }
        }
        Value::UserData(u) => {
            let tostring = u.metatable()?.get::<Value>("__tostring")?;
            match tostring {
                Value::Function(f) => f.call::<String>(u)?,
                _ => {
                    return Err(LuaError::RuntimeError(
                        "Script error: the invoked function returned a userdata value; it must return a string."
                            .to_string(),
                    ))
                }
            }
        }
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
