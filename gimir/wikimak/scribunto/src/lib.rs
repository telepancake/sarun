//! Scribunto: {{#invoke:Module|fn}} via embedded PUC Lua 5.1 (plan
//! §3.3). Sandbox removes io/os/require/dofile/loadfile; default limits
//! 50 MB / 7 s CPU. Frame semantics: lazy frame.args, the two-frame
//! frame:getParent() pattern, frame:preprocess / expandTemplate /
//! callParserFunction. The mw.* host library is ours: mw.text,
//! mw.title, mw.ustring (native Lua patterns — PUC exactness is why
//! the vendored-C engine was chosen), mw.html, mw.uri, mw.language
//! (plural/formatnum — stub then grow), mw.site (from SiteConfig),
//! mw.message, mw.hash. Module: pages come from the PageStore at τ.
//!
//! OWNED BY: the scribunto agent. Skeleton: a stub that errors politely
//! (renders as an inline script-error box, never aborts the page).

use wikimak_wikitext::{Frame, ModuleInvoker, PageStore};

pub struct LuaInvoker {
    _private: (),
}

impl LuaInvoker {
    pub fn new() -> Result<Self, String> {
        Ok(LuaInvoker { _private: () })
    }
}

impl Default for LuaInvoker {
    fn default() -> Self {
        LuaInvoker { _private: () }
    }
}

impl ModuleInvoker for LuaInvoker {
    fn invoke(
        &self,
        module: &str,
        function: &str,
        _frame: &Frame,
        _store: &dyn PageStore,
    ) -> Result<String, String> {
        Err(format!("Scribunto not yet wired: {module}::{function}"))
    }
}
