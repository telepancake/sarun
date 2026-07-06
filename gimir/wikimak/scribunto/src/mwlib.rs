//! Rust half of the mw.* host library: the primitives a pure-Lua sandbox
//! cannot compute — UTF-8 codepoint math (mw.ustring), the store-backed
//! title/message/content lookups, τ date formatting, hashing, and the
//! frame object. Assembled into a `host` table and handed to the Lua
//! `BOOTSTRAP` (see `lua_src`), which builds the ergonomic mw.* surface
//! on top.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use mlua::{Lua, Scope, Table, Value, Variadic};
use wikimak_wikitext::{
    preprocess, Frame, ModuleInvoker, PageStore, RenderOptions, SiteConfig, Title,
};

use crate::{datetime, hash};

/// MediaWiki's fixed namespace id for Module: — used directly rather than
/// through the (skeleton) `Title::parse`, so module resolution is exact
/// regardless of the preprocessor agent's normalization progress.
pub const NS_MODULE: i32 = 828;
pub const NS_MEDIAWIKI: i32 = 8;

/// Everything a single invoke borrows. Lifetime `'a` outlives the mlua
/// `scope`, so scoped host closures may capture `&Ctx` freely.
pub struct Ctx<'a> {
    pub store: &'a dyn PageStore,
    pub invoker: &'a dyn ModuleInvoker,
    pub site: &'a SiteConfig,
    pub tau_secs: i64,
    pub current_title: String,
    pub logs: &'a RefCell<Vec<String>>,
    /// Module SOURCE cache, keyed by bare module name, shared across all
    /// invokes on one `LuaInvoker` (per-render, per-τ). Caches the store
    /// hit — the expensive part — while each invoke still gets a fresh Lua
    /// state (clean memory/instruction budget isolation).
    pub source_cache: &'a RefCell<HashMap<String, Option<String>>>,
}

impl Ctx<'_> {
    fn opts(&self) -> RenderOptions<'_> {
        RenderOptions {
            invoker: Some(self.invoker),
            media: None,
            link_prefix: "./".into(),
            asof_query: String::new(),
        }
    }
}

/// Build the `Module:` title for a require/invoke name, bypassing the
/// skeleton `Title::parse` (fixed ns id 828) so module resolution is
/// exact. Accepts both "Foo" and "Module:Foo".
pub fn module_title(name: &str) -> Title {
    let bare = name
        .strip_prefix("Module:")
        .or_else(|| name.strip_prefix("module:"))
        .unwrap_or(name)
        .trim();
    Title { ns: NS_MODULE, text: first_upper(bare) }
}

pub(crate) fn first_upper(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Namespace-resolve a raw title against siteinfo. Bypasses the skeleton
/// `Title::parse`; leading ':' forces mainspace, `default_ns` applies
/// when no known prefix is present.
pub fn parse_title(raw: &str, site: &SiteConfig, default_ns: i32) -> Title {
    let t = raw.trim();
    let (forced_main, t) = match t.strip_prefix(':') {
        Some(r) => (true, r.trim()),
        None => (false, t),
    };
    let t = t.replace('_', " ");
    let t = t.trim();
    if !forced_main {
        if let Some(idx) = t.find(':') {
            let prefix = t[..idx].trim();
            let rest = t[idx + 1..].trim();
            for ns in site.namespaces.values() {
                let hit = ns.canonical.eq_ignore_ascii_case(prefix)
                    || ns.aliases.iter().any(|a| a.eq_ignore_ascii_case(prefix));
                if hit {
                    let text = if ns.case_first_letter { first_upper(rest) } else { rest.to_string() };
                    return Title { ns: ns.id, text };
                }
            }
        }
    }
    Title { ns: default_ns, text: first_upper(t) }
}

fn make_title(ns: i32, title_text: &str, site: &SiteConfig) -> Title {
    let text = title_text.trim().replace('_', " ");
    let case_first = site
        .namespaces
        .get(&ns)
        .map(|n| n.case_first_letter)
        .unwrap_or(true);
    Title {
        ns,
        text: if case_first { first_upper(text.trim()) } else { text.trim().to_string() },
    }
}

fn cp_to_byte(s: &str, cp: i64) -> i64 {
    if cp <= 1 {
        return 1;
    }
    let target = (cp - 1) as usize;
    for (n, (b, _)) in s.char_indices().enumerate() {
        if n == target {
            return b as i64 + 1;
        }
    }
    s.len() as i64 + 1
}

fn byte_to_cp(s: &str, byte: i64) -> i64 {
    if byte <= 1 {
        return 1;
    }
    let target = (byte - 1) as usize;
    let mut count = 0i64;
    for (b, _) in s.char_indices() {
        if b >= target {
            return count + 1;
        }
        count += 1;
    }
    count + 1
}

fn ustring_sub(s: &str, i: i64, j: i64) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let norm = |x: i64| if x < 0 { len + x + 1 } else { x };
    let mut a = norm(i);
    let mut b = norm(j);
    if a < 1 {
        a = 1;
    }
    if b > len {
        b = len;
    }
    if a > b {
        return String::new();
    }
    chars[(a - 1) as usize..b as usize].iter().collect()
}

/// Build the frame Lua table for a Rust [`Frame`]: args table (positional
/// under integer keys, named under string keys, with the string/number
/// bridge metatable) plus the frame methods. The parent chain is built
/// eagerly so `frame:getParent()` returns the second frame.
fn build_frame(
    lua: &Lua,
    ctx: &Ctx,
    frame: &Frame,
    methods: &Table,
) -> mlua::Result<Table> {
    let tbl = lua.create_table()?;
    let args = lua.create_table()?;
    for (k, v) in &frame.args {
        match k.parse::<i64>() {
            Ok(n) if n > 0 && n.to_string() == *k => args.raw_set(n, v.clone())?,
            _ => args.raw_set(k.clone(), v.clone())?,
        }
    }
    let args_mt: Table = lua.load(crate::lua_src::ARGS_METATABLE).eval()?;
    args.set_metatable(Some(args_mt));
    tbl.raw_set("args", args)?;
    tbl.raw_set("title", frame.title.clone())?;

    if let Some(parent) = &frame.parent {
        let ptbl = build_frame(lua, ctx, parent, methods)?;
        tbl.raw_set("__parent", ptbl)?;
    }

    let mt = lua.create_table()?;
    mt.raw_set("__index", methods.clone())?;
    tbl.set_metatable(Some(mt));
    Ok(tbl)
}

fn lua_frame_to_rust(tbl: &Table) -> Frame {
    let mut args = BTreeMap::new();
    if let Ok(args_tbl) = tbl.raw_get::<Table>("args") {
        // Iterate the raw pairs (bypasses the number/string bridge).
        for pair in args_tbl.pairs::<Value, Value>() {
            if let Ok((k, v)) = pair {
                let key = match k {
                    Value::Integer(n) => n.to_string(),
                    Value::Number(n) => (n as i64).to_string(),
                    Value::String(s) => s.to_str().map(|x| x.to_string()).unwrap_or_default(),
                    _ => continue,
                };
                let val = match v {
                    Value::String(s) => s.to_str().map(|x| x.to_string()).unwrap_or_default(),
                    Value::Integer(n) => n.to_string(),
                    Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                args.insert(key, val);
            }
        }
    }
    let title = tbl.raw_get::<String>("title").unwrap_or_default();
    Frame { args, parent: None, title }
}

/// Substitute `{{{name}}}` / `{{{name|default}}}` from the frame args
/// before running the (frame-less) preprocessor. This is the pragmatic
/// bridge for `frame:preprocess` — the frozen `preprocess::expand`
/// signature takes no frame, so its own {{{param}}} resolution can't see
/// these args; we resolve them here first. Nested/edge param forms are
/// out of scope (see crate gaps).
fn substitute_params(text: &str, args: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 <= bytes.len() && &text[i..i + 3] == "{{{" {
            if let Some(end) = text[i + 3..].find("}}}") {
                let inner = &text[i + 3..i + 3 + end];
                let (name, default) = match inner.find('|') {
                    Some(p) => (&inner[..p], Some(&inner[p + 1..])),
                    None => (inner, None),
                };
                let name = name.trim();
                if let Some(v) = args.get(name) {
                    out.push_str(v);
                } else if let Some(d) = default {
                    out.push_str(d);
                } else {
                    out.push_str("{{{");
                    out.push_str(name);
                    out.push_str("}}}");
                }
                i += 3 + end + 3;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn uri_encode(s: &str, kind: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        let extra = match kind {
            "WIKI" => matches!(b, b'/' | b':'),
            "PATH" => matches!(b, b'/'),
            _ => false,
        };
        if keep || extra {
            out.push(b as char);
        } else if b == b' ' && kind == "QUERY" {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn uri_decode(s: &str, kind: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' if kind == "QUERY" => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn build_site_table(lua: &Lua, site: &SiteConfig) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("siteName", site.site_name.clone())?;
    // SiteConfig carries no server/scriptPath; best-effort placeholders
    // until the contract grows them (see crate lib_rs_requests).
    t.raw_set("server", "")?;
    t.raw_set("scriptPath", "/w")?;
    t.raw_set("currentVersion", "gimir")?;

    let ns = lua.create_table()?;
    for info in site.namespaces.values() {
        let e = lua.create_table()?;
        let name = if info.canonical.is_empty() {
            info.aliases.first().cloned().unwrap_or_default()
        } else {
            info.canonical.clone()
        };
        e.raw_set("id", info.id)?;
        e.raw_set("name", name.clone())?;
        e.raw_set("canonicalName", info.canonical.clone())?;
        e.raw_set("displayName", name.clone())?;
        e.raw_set("hasSubpages", info.id != 0)?;
        e.raw_set("isTalk", info.id > 0 && info.id % 2 == 1)?;
        e.raw_set("isSubject", info.id % 2 == 0)?;
        e.raw_set("isContent", info.id == 0)?;
        e.raw_set("subject", if info.id % 2 == 1 { info.id - 1 } else { info.id })?;
        e.raw_set("talk", if info.id >= 0 && info.id % 2 == 0 { info.id + 1 } else { info.id })?;
        ns.raw_set(info.id, e.clone())?;
        if !name.is_empty() {
            ns.raw_set(name, e)?;
        }
    }
    t.raw_set("namespaces", ns)?;

    let stats = lua.create_table()?;
    for k in ["pages", "articles", "files", "edits", "users", "activeUsers", "admins"] {
        stats.raw_set(k, 0)?;
    }
    t.raw_set("stats", stats)?;
    Ok(t)
}

fn title_table(lua: &Lua, title: &Title, site: &SiteConfig, store: &dyn PageStore) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let prefixed = title.prefixed(site);
    t.raw_set("namespace", title.ns)?;
    t.raw_set("id", 0)?;
    t.raw_set("text", title.text.clone())?;
    t.raw_set("prefixedText", prefixed.clone())?;
    t.raw_set("fullText", prefixed)?;
    t.raw_set("fragment", "")?;
    t.raw_set("exists", store.page_exists(title))?;
    t.raw_set("isRedirect", false)?;
    t.raw_set("nsText", site.namespaces.get(&title.ns).map(|n| n.canonical.clone()).unwrap_or_default())?;
    Ok(t)
}

/// Build the `host` table and the frame methods table, run the Lua
/// `BOOTSTRAP` to assemble `mw`, set both `mw` and `frame` as globals, and
/// return the main frame table (which the caller passes to the invoked
/// function).
pub fn install<'scope, 'env, 'a>(
    lua: &'scope Lua,
    scope: &'scope Scope<'scope, 'env>,
    ctx: &'a Ctx<'a>,
    frame: &'a Frame,
) -> mlua::Result<Table>
where
    'a: 'scope,
{
    let host = lua.create_table()?;

    host.set("ustring_len", scope.create_function(|_, s: mlua::String| Ok(s.to_str()?.chars().count() as i64))?)?;
    host.set(
        "ustring_sub",
        scope.create_function(|_, (s, i, j): (mlua::String, i64, Option<i64>)| {
            Ok(ustring_sub(&s.to_str()?, i, j.unwrap_or(-1)))
        })?,
    )?;
    host.set("ustring_upper", scope.create_function(|_, s: mlua::String| Ok(s.to_str()?.to_uppercase()))?)?;
    host.set("ustring_lower", scope.create_function(|_, s: mlua::String| Ok(s.to_str()?.to_lowercase()))?)?;
    host.set(
        "ustring_char",
        scope.create_function(|_, cps: Variadic<i64>| {
            let mut out = String::new();
            for c in cps {
                match u32::try_from(c).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => return Err(mlua::Error::RuntimeError(format!("mw.ustring.char: bad codepoint {c}"))),
                }
            }
            Ok(out)
        })?,
    )?;
    host.set(
        "ustring_codepoint",
        scope.create_function(|_, (s, i, j): (mlua::String, Option<i64>, Option<i64>)| {
            let s = s.to_str()?;
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            let norm = |x: i64| if x < 0 { len + x + 1 } else { x };
            let a = norm(i.unwrap_or(1)).max(1);
            let b = norm(j.unwrap_or(a)).min(len);
            let mut out = Vec::new();
            let mut k = a;
            while k <= b {
                out.push(chars[(k - 1) as usize] as i64);
                k += 1;
            }
            Ok(Variadic::from_iter(out))
        })?,
    )?;
    host.set(
        "ustring_byteoffset",
        scope.create_function(|_, (s, l, i): (mlua::String, Option<i64>, Option<i64>)| {
            let s = s.to_str()?;
            let l = l.unwrap_or(1);
            let start_cp = i.unwrap_or(1);
            Ok(cp_to_byte(&s, start_cp + l - 1))
        })?,
    )?;
    host.set("cp_to_byte", scope.create_function(|_, (s, cp): (mlua::String, i64)| Ok(cp_to_byte(&s.to_str()?, cp)))?)?;
    host.set("byte_to_cp", scope.create_function(|_, (s, b): (mlua::String, i64)| Ok(byte_to_cp(&s.to_str()?, b)))?)?;
    host.set(
        "byte",
        scope.create_function(|_, s: mlua::String| Ok(s.to_str()?.chars().next().map(|c| c as i64).unwrap_or(0)))?,
    )?;

    host.set(
        "title_resolve",
        scope.create_function(move |lua, (raw, ns): (mlua::String, Option<Value>)| {
            let default_ns = match ns {
                Some(Value::Integer(n)) => n as i32,
                Some(Value::Number(n)) => n as i32,
                _ => 0,
            };
            let title = parse_title(&raw.to_str()?, ctx.site, default_ns);
            title_table(lua, &title, ctx.site, ctx.store)
        })?,
    )?;
    host.set(
        "title_make",
        scope.create_function(move |lua, (ns, text): (i64, mlua::String)| {
            let title = make_title(ns as i32, &text.to_str()?, ctx.site);
            title_table(lua, &title, ctx.site, ctx.store)
        })?,
    )?;
    host.set(
        "page_content",
        scope.create_function(move |_, prefixed: mlua::String| {
            let title = parse_title(&prefixed.to_str()?, ctx.site, 0);
            Ok(ctx.store.page_text(&title))
        })?,
    )?;
    host.set(
        "message_plain",
        scope.create_function(move |_, key: mlua::String| {
            let key = key.to_str()?.to_string();
            let title = make_title(NS_MEDIAWIKI, &key, ctx.site);
            match ctx.store.page_text(&title) {
                Some(t) => Ok(t),
                None => Ok(format!("\u{29FC}{key}\u{29FD}")),
            }
        })?,
    )?;
    host.set(
        "message_exists",
        scope.create_function(move |_, key: mlua::String| {
            let title = make_title(NS_MEDIAWIKI, &key.to_str()?, ctx.site);
            Ok(ctx.store.page_exists(&title))
        })?,
    )?;
    host.set(
        "format_date",
        scope.create_function(move |_, (fmt, ts): (mlua::String, Option<i64>)| {
            let unix = ts.unwrap_or(ctx.tau_secs);
            let c = datetime::civil_from_unix(unix);
            Ok(datetime::format_php_date(&fmt.to_str()?, &c, unix))
        })?,
    )?;
    host.set("sha1", scope.create_function(|_, s: mlua::String| Ok(hash::sha1_hex(&s.as_bytes())))?)?;
    host.set("md5", scope.create_function(|_, s: mlua::String| Ok(hash::md5_hex(&s.as_bytes())))?)?;
    host.set("uri_encode", scope.create_function(|_, (s, k): (mlua::String, mlua::String)| Ok(uri_encode(&s.to_str()?, &k.to_str()?)))?)?;
    host.set("uri_decode", scope.create_function(|_, (s, k): (mlua::String, mlua::String)| Ok(uri_decode(&s.to_str()?, &k.to_str()?)))?)?;
    host.set(
        "log",
        scope.create_function(move |_, s: mlua::String| {
            ctx.logs.borrow_mut().push(s.to_str()?.to_string());
            Ok(())
        })?,
    )?;
    host.set("site", build_site_table(lua, ctx.site)?)?;
    host.set("lang_code", ctx.site.lang.clone())?;
    host.set("rtl", ctx.site.rtl)?;
    host.set("current_title", ctx.current_title.clone())?;

    // Frame methods (shared metatable __index for every frame table).
    let methods = lua.create_table()?;
    methods.set("getTitle", scope.create_function(|_, this: Table| this.raw_get::<String>("title"))?)?;
    methods.set(
        "getParent",
        scope.create_function(|_, this: Table| this.raw_get::<Value>("__parent"))?,
    )?;
    methods.set(
        "preprocess",
        scope.create_function(move |_, (this, text): (Table, mlua::String)| {
            let rf = lua_frame_to_rust(&this);
            let substituted = substitute_params(&text.to_str()?, &rf.args);
            let title = parse_title(&rf.title, ctx.site, 0);
            Ok(preprocess::expand(ctx.store, &title, &substituted, &ctx.opts()).text)
        })?,
    )?;
    methods.set(
        "expandTemplate",
        scope.create_function(move |_, (this, spec): (Table, Table)| {
            let _ = this;
            let title: String = spec.get("title")?;
            let mut wt = format!("{{{{{title}");
            if let Ok(args) = spec.get::<Table>("args") {
                for pair in args.pairs::<Value, Value>() {
                    let (k, v) = pair?;
                    let val = value_to_string(&v);
                    match k {
                        Value::Integer(_) | Value::Number(_) => wt.push_str(&format!("|{val}")),
                        Value::String(s) => wt.push_str(&format!("|{}={}", s.to_str()?, val)),
                        _ => {}
                    }
                }
            }
            wt.push_str("}}");
            let cur = parse_title(&ctx.current_title, ctx.site, 0);
            Ok(preprocess::expand(ctx.store, &cur, &wt, &ctx.opts()).text)
        })?,
    )?;
    methods.set(
        "callParserFunction",
        scope.create_function(move |_, (this, name, args): (Table, mlua::String, Variadic<Value>)| {
            let _ = this;
            let name = name.to_str()?.to_string();
            let mut parts: Vec<String> = Vec::new();
            for a in args.iter() {
                if let Value::Table(t) = a {
                    for pair in t.clone().sequence_values::<Value>() {
                        parts.push(value_to_string(&pair?));
                    }
                } else {
                    parts.push(value_to_string(a));
                }
            }
            let joined = parts.join("|");
            let wt = if joined.is_empty() {
                format!("{{{{{name}}}}}")
            } else {
                format!("{{{{{name}:{joined}}}}}")
            };
            let cur = parse_title(&ctx.current_title, ctx.site, 0);
            Ok(preprocess::expand(ctx.store, &cur, &wt, &ctx.opts()).text)
        })?,
    )?;
    methods.set(
        "newChild",
        scope.create_function(move |lua, (this, spec): (Table, Option<Table>)| {
            let child = lua.create_table()?;
            let args = lua.create_table()?;
            let mut title = this.raw_get::<String>("title").unwrap_or_default();
            if let Some(spec) = spec {
                if let Ok(t) = spec.get::<String>("title") {
                    title = t;
                }
                if let Ok(a) = spec.get::<Table>("args") {
                    for pair in a.pairs::<Value, Value>() {
                        let (k, v) = pair?;
                        args.raw_set(k, v)?;
                    }
                }
            }
            let args_mt: Table = lua.load(crate::lua_src::ARGS_METATABLE).eval()?;
            args.set_metatable(Some(args_mt));
            child.raw_set("args", args)?;
            child.raw_set("title", title)?;
            child.raw_set("__parent", this)?;
            let mt = lua.create_table()?;
            let methods: Table = lua.globals().get::<Table>("__frame_methods")?;
            mt.raw_set("__index", methods)?;
            child.set_metatable(Some(mt));
            Ok(child)
        })?,
    )?;
    lua.globals().set("__frame_methods", methods.clone())?;

    let main_frame = build_frame(lua, ctx, frame, &methods)?;

    let mw = lua.create_table()?;
    let bootstrap: mlua::Function = lua.load(crate::lua_src::BOOTSTRAP).eval()?;
    bootstrap.call::<()>((mw.clone(), host, main_frame.clone()))?;
    lua.globals().set("mw", mw)?;
    lua.globals().set("frame", main_frame.clone())?;

    Ok(main_frame)
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.to_str().map(|x| x.to_string()).unwrap_or_default(),
        Value::Integer(n) => n.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => String::new(),
        _ => String::new(),
    }
}
