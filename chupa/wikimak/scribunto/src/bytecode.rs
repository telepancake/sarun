//! Server-scoped cache for immutable Lua chunk bytecode.
//!
//! The cache contains no Lua values and never executes a chunk while filling
//! an entry. A fresh page-scoped Lua state compiles a source chunk on a miss,
//! dumps the resulting Lua 5.1 bytecode, and the same or another fresh state
//! loads that bytecode before executing it. The map is keyed by role and
//! chunk name; each entry retains the exact source bytes alongside its
//! bytecode. A changed source therefore replaces the old entry instead of
//! accumulating every historical revision.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mlua::{ChunkMode, Error as LuaError, Function, Lua};

/// Why a chunk is being loaded. Role is deliberately part of the cache key;
/// identical text used as a module and as an embedded helper has different
/// source ownership and error attribution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LuaChunkRole {
    SandboxTrim,
    ArgsMetatable,
    HostProxy,
    Bootstrap,
    Builtin,
    Module,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LuaBytecodeCacheStats {
    /// Number of role/name entries retained by this cache.
    pub entries: usize,
    /// Bytes of exact source retained alongside those entries.
    pub source_bytes: usize,
    /// Bytes of dumped Lua bytecode retained.
    pub bytecode_bytes: usize,
    /// Bytes of current-head module source retained for safe source reuse.
    pub module_source_bytes: usize,
    /// Number of generation/title module-source entries retained.
    pub module_source_entries: usize,
    /// Number of source-to-bytecode compilations completed.
    pub compilations: u64,
    /// Number of bytecode lookups that reused an existing exact source.
    pub cache_hits: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    role: LuaChunkRole,
    name: String,
}

struct CacheEntry {
    source: Arc<[u8]>,
    bytecode: Arc<[u8]>,
}

/// Identity under which a module page's current-head source is stable. This
/// is supplied by the archive selector. Historical/as-of requests do not use
/// the cross-render source cache because their content-selection instants can
/// vary independently; their bytecode entries remain exact and replaceable.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LuaModuleSourceScope {
    generation: String,
}

impl LuaModuleSourceScope {
    pub fn new(generation: impl Into<String>) -> Self {
        Self {
            generation: generation.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ModuleSourceKey {
    scope: LuaModuleSourceScope,
    title: String,
}

struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    module_sources: HashMap<ModuleSourceKey, Option<Arc<str>>>,
    stats: LuaBytecodeCacheStats,
}

/// Immutable bytecode shared by the invokers belonging to one server or one
/// immutable archive generation.
///
/// Cloning this value clones the owner handle, not the cache contents. The
/// owning server/generation drops the last handle, so old source and bytecode
/// disappear after in-flight request handles finish.
#[derive(Clone)]
pub struct LuaBytecodeCache {
    state: Arc<Mutex<CacheState>>,
}

impl Default for LuaBytecodeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaBytecodeCache {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(CacheState {
                entries: HashMap::new(),
                module_sources: HashMap::new(),
                stats: LuaBytecodeCacheStats::default(),
            })),
        }
    }

    pub fn stats(&self) -> LuaBytecodeCacheStats {
        self.state
            .lock()
            .map(|state| state.stats.clone())
            .unwrap_or_default()
    }

    /// Look up a module source only under an explicitly known immutable
    /// generation/current-head identity. The outer `Option` distinguishes an
    /// absent cache entry from a cached missing page.
    pub fn module_source(
        &self,
        scope: &LuaModuleSourceScope,
        title: &str,
    ) -> mlua::Result<Option<Option<Arc<str>>>> {
        let state = self.state.lock().map_err(|_| {
            LuaError::RuntimeError("Lua bytecode cache lock is poisoned".to_owned())
        })?;
        Ok(state
            .module_sources
            .get(&ModuleSourceKey {
                scope: scope.clone(),
                title: title.to_owned(),
            })
            .cloned())
    }

    pub fn remember_module_source(
        &self,
        scope: &LuaModuleSourceScope,
        title: &str,
        source: Option<&str>,
    ) -> mlua::Result<()> {
        let mut state = self.state.lock().map_err(|_| {
            LuaError::RuntimeError("Lua bytecode cache lock is poisoned".to_owned())
        })?;
        let key = ModuleSourceKey {
            scope: scope.clone(),
            title: title.to_owned(),
        };
        let value = source.map(Arc::<str>::from);
        if let Some(previous) = state.module_sources.insert(key, value.clone()) {
            state.stats.module_source_bytes = state
                .stats
                .module_source_bytes
                .saturating_sub(previous.as_ref().map_or(0, |s| s.len()));
        } else {
            state.stats.module_source_entries += 1;
        }
        state.stats.module_source_bytes += value.as_ref().map_or(0, |s| s.len());
        Ok(())
    }

    /// Return a loadable function for a chunk, compiling and dumping its
    /// source exactly once while it remains the exact source for this
    /// role/name entry. Concurrent first use and source replacement are
    /// serialized by the mutex; execution is outside the mutex in the caller
    /// page VM.
    pub fn load_function(
        &self,
        lua: &Lua,
        role: LuaChunkRole,
        name: &str,
        source: &[u8],
    ) -> mlua::Result<Function> {
        let key = CacheKey {
            role,
            name: name.to_owned(),
        };
        let bytecode = {
            let mut state = self.state.lock().map_err(|_| {
                LuaError::RuntimeError("Lua bytecode cache lock is poisoned".to_owned())
            })?;
            let exact_hit = state
                .entries
                .get(&key)
                .filter(|entry| entry.source.as_ref() == source)
                .map(|entry| Arc::clone(&entry.bytecode));
            if let Some(bytecode) = exact_hit {
                state.stats.cache_hits += 1;
                bytecode
            } else {
                // `into_function` only parses/compiles the source. It does
                // not call the resulting function or otherwise execute module
                // code while the shared cache is being populated.
                let function = lua
                    .load(source)
                    .set_name(name.to_owned())
                    .set_mode(ChunkMode::Text)
                    .into_function()?;
                let source: Arc<[u8]> = Arc::from(source.to_vec());
                let bytecode: Arc<[u8]> = Arc::from(function.dump(false));
                if let Some(previous) = state.entries.insert(
                    key,
                    CacheEntry {
                        source: Arc::clone(&source),
                        bytecode: Arc::clone(&bytecode),
                    },
                ) {
                    state.stats.source_bytes = state
                        .stats
                        .source_bytes
                        .saturating_sub(previous.source.len());
                    state.stats.bytecode_bytes = state
                        .stats
                        .bytecode_bytes
                        .saturating_sub(previous.bytecode.len());
                } else {
                    state.stats.entries += 1;
                }
                state.stats.source_bytes += source.len();
                state.stats.bytecode_bytes += bytecode.len();
                state.stats.compilations += 1;
                bytecode
            }
        };

        lua.load(bytecode.as_ref())
            .set_name(name.to_owned())
            .set_mode(ChunkMode::Binary)
            .into_function()
    }
}
