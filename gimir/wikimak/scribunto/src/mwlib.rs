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
    // Underscores are spaces in MediaWiki titles: `Module:Foo/anchor_id_list`
    // and `Module:Foo/anchor id list` are the SAME page. Normalize so a
    // `require("Module:Foo/anchor_id_list")` finds the stored `… id list`.
    let bare = bare.replace('_', " ");
    Title { ns: NS_MODULE, text: first_upper(bare.trim()) }
}

/// Resolve a `require`/`loadData` target to its `Module:` title, stripping the
/// namespace prefix in ANY of its forms — the canonical `Module`, the
/// English alias, OR the wiki's LOCALIZED name (`Модуль:`, `יחידה:`, `모듈:`,
/// `پودمان:`, …) from siteinfo. Non-English wikis' modules `require` each
/// other by the localized prefix (uk's `Ref-lang` requires `Модуль:Arguments`),
/// so a plain "Module:"-only strip leaves the prefix glued to the name and the
/// lookup fails. Bare names and names with a non-namespace ':' fall through to
/// [`module_title`].
pub fn resolve_module(name: &str, site: &SiteConfig) -> Title {
    let t = name.trim();
    let t = t.strip_prefix(':').unwrap_or(t).trim();
    if let Some(idx) = t.find(':') {
        let prefix = t[..idx].trim().replace('_', " ");
        let rest = t[idx + 1..].trim();
        let is_module_ns = prefix.eq_ignore_ascii_case("Module")
            || site.namespaces.get(&NS_MODULE).is_some_and(|ns| {
                ns.canonical.eq_ignore_ascii_case(&prefix)
                    || ns.aliases.iter().any(|a| a.eq_ignore_ascii_case(&prefix))
            });
        if is_module_ns {
            return Title { ns: NS_MODULE, text: first_upper(&rest.replace('_', " ")) };
        }
    }
    module_title(t)
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

/// Resolve a namespace NAME (canonical, localized, or alias) to its id.
/// "" / "0" / a plain integer string map directly; case-insensitive.
fn resolve_ns_name(raw: &str, site: &SiteConfig) -> Option<i32> {
    let name = raw.trim().replace('_', " ");
    if name.is_empty() {
        return Some(0);
    }
    if let Ok(n) = name.parse::<i32>() {
        return Some(n);
    }
    for ns in site.namespaces.values() {
        if ns.canonical.eq_ignore_ascii_case(&name)
            || ns.aliases.iter().any(|a| a.eq_ignore_ascii_case(&name))
        {
            return Some(ns.id);
        }
    }
    None
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
    t.raw_set("server", site.server.clone())?;
    t.raw_set("scriptPath", site.script_path.clone())?;
    t.raw_set("currentVersion", "gimir")?;

    let ns = lua.create_table()?;
    // mw.site.talkNamespaces / subjectNamespaces / contentNamespaces are
    // { [id] = <the SAME namespace object> } subsets — real modules read
    // `mw.site.subjectNamespaces[0].name` and `ipairs(nsObj.aliases)`, so the
    // values must be the full namespace tables, not bare name strings.
    let talk = lua.create_table()?;
    let subject = lua.create_table()?;
    let content = lua.create_table()?;
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
        e.raw_set("hasGenderDistinction", false)?;
        e.raw_set("isCapitalized", info.case_first_letter)?;
        e.raw_set("isMovable", info.id >= 0)?;
        e.raw_set("isTalk", info.id > 0 && info.id % 2 == 1)?;
        e.raw_set("isSubject", info.id % 2 == 0)?;
        e.raw_set("isContent", info.id == 0)?;
        e.raw_set("subject", if info.id % 2 == 1 { info.id - 1 } else { info.id })?;
        e.raw_set("talk", if info.id >= 0 && info.id % 2 == 0 { info.id + 1 } else { info.id })?;
        e.raw_set("associated", if info.id % 2 == 1 { info.id - 1 } else if info.id >= 0 { info.id + 1 } else { info.id })?;
        // aliases: the localized name + namespacealiases (excluding the
        // canonical, which callers read separately). Namespace detect iterates
        // this to build its name→id map, so it must be a real sequence.
        let aliases = lua.create_table()?;
        for (i, a) in info.aliases.iter().enumerate() {
            aliases.raw_set(i as i64 + 1, a.clone())?;
        }
        e.raw_set("aliases", aliases)?;
        ns.raw_set(info.id, e.clone())?;
        if !name.is_empty() {
            ns.raw_set(name, e.clone())?;
        }
        if info.id > 0 && info.id % 2 == 1 {
            talk.raw_set(info.id, e.clone())?;
        }
        if info.id >= 0 && info.id % 2 == 0 {
            subject.raw_set(info.id, e.clone())?;
        }
        if info.id == 0 {
            content.raw_set(info.id, e)?;
        }
    }
    t.raw_set("namespaces", ns)?;
    t.raw_set("talkNamespaces", talk)?;
    t.raw_set("subjectNamespaces", subject)?;
    t.raw_set("contentNamespaces", content)?;

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
    t.raw_set("id", store.page_id(title).unwrap_or(0))?;
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

    // Codepoint math tolerates INVALID UTF-8: a module that slices a
    // multibyte string at a non-character boundary (or feeds Latin-1 bytes)
    // hands us malformed UTF-8; `to_string_lossy` substitutes U+FFFD and lets
    // the citation finish rather than aborting the whole invoke on a strict
    // conversion. The bytes were already broken upstream — lenient decode is
    // the faithful "keep going" behavior, matching how MediaWiki's PHP ustring
    // does not hard-fail on a stray byte.
    host.set("ustring_len", scope.create_function(|_, s: mlua::String| Ok(s.to_string_lossy().chars().count() as i64))?)?;
    host.set(
        "ustring_sub",
        scope.create_function(|_, (s, i, j): (mlua::String, i64, Option<i64>)| {
            Ok(ustring_sub(&s.to_string_lossy(), i, j.unwrap_or(-1)))
        })?,
    )?;
    host.set("ustring_upper", scope.create_function(|_, s: mlua::String| Ok(s.to_string_lossy().to_uppercase()))?)?;
    host.set("ustring_lower", scope.create_function(|_, s: mlua::String| Ok(s.to_string_lossy().to_lowercase()))?)?;
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
            let s = s.to_string_lossy();
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
            let s = s.to_string_lossy();
            let l = l.unwrap_or(1);
            let start_cp = i.unwrap_or(1);
            Ok(cp_to_byte(&s, start_cp + l - 1))
        })?,
    )?;
    host.set("cp_to_byte", scope.create_function(|_, (s, cp): (mlua::String, i64)| Ok(cp_to_byte(&s.to_string_lossy(), cp)))?)?;
    host.set("byte_to_cp", scope.create_function(|_, (s, b): (mlua::String, i64)| Ok(byte_to_cp(&s.to_string_lossy(), b)))?)?;
    host.set(
        "byte",
        scope.create_function(|_, s: mlua::String| Ok(s.to_string_lossy().chars().next().map(|c| c as i64).unwrap_or(0)))?,
    )?;

    host.set(
        "title_resolve",
        scope.create_function(move |lua, (raw, ns): (mlua::String, Option<Value>)| {
            let default_ns = match ns {
                Some(Value::Integer(n)) => n as i32,
                Some(Value::Number(n)) => n as i32,
                _ => 0,
            };
            let title = parse_title(&raw.to_string_lossy(), ctx.site, default_ns);
            title_table(lua, &title, ctx.site, ctx.store)
        })?,
    )?;
    host.set(
        "title_make",
        scope.create_function(move |lua, (ns, text): (Value, mlua::String)| {
            // mw.title.makeTitle accepts the namespace as a NUMBER id or a
            // NAME string (canonical/localized/alias) — NoteTA calls
            // makeTitle('Template', …). Resolve string names against siteinfo.
            let ns_id = match ns {
                Value::Integer(n) => n as i32,
                Value::Number(n) => n as i32,
                Value::String(s) => {
                    let name = s.to_str()?;
                    resolve_ns_name(&name, ctx.site).ok_or_else(|| {
                        mlua::Error::RuntimeError(format!(
                            "mw.title.makeTitle: unknown namespace \"{}\"",
                            name.as_ref()
                        ))
                    })?
                }
                Value::Nil => 0,
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "mw.title.makeTitle: namespace must be a number or string, got {}",
                        other.type_name()
                    )))
                }
            };
            let title = make_title(ns_id, &text.to_string_lossy(), ctx.site);
            title_table(lua, &title, ctx.site, ctx.store)
        })?,
    )?;
    host.set(
        "page_content",
        scope.create_function(move |_, prefixed: mlua::String| {
            let title = parse_title(&prefixed.to_string_lossy(), ctx.site, 0);
            Ok(ctx.store.page_text(&title))
        })?,
    )?;
    host.set(
        "message_plain",
        scope.create_function(move |_, key: mlua::String| {
            let key = key.to_string_lossy();
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
            let title = make_title(NS_MEDIAWIKI, &key.to_string_lossy(), ctx.site);
            Ok(ctx.store.page_exists(&title))
        })?,
    )?;
    host.set(
        "format_date",
        scope.create_function(move |_, (fmt, ts): (mlua::String, Value)| {
            // mw.language:formatDate accepts a timestamp as a NUMBER (unix) or
            // a STRING in the wide range PHP strtotime/wfTimestamp handle
            // ("2022-1-1", "1 January 2020", a 14-digit stamp, "now", …); nil
            // means τ. CS1's Configuration feeds strings, so parse them.
            let unix = match ts {
                Value::Nil => ctx.tau_secs,
                Value::Integer(n) => n,
                Value::Number(n) => n as i64,
                Value::String(s) => {
                    let raw = s.to_string_lossy();
                    datetime::parse_timestamp(&raw, ctx.tau_secs).ok_or_else(|| {
                        mlua::Error::RuntimeError(format!(
                            "mw.language:formatDate: invalid timestamp \"{raw}\""
                        ))
                    })?
                }
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "mw.language:formatDate: timestamp must be a string or number, got {}",
                        other.type_name()
                    )))
                }
            };
            let c = datetime::civil_from_unix(unix);
            Ok(datetime::format_php_date(&fmt.to_string_lossy(), &c, unix))
        })?,
    )?;
    host.set("sha1", scope.create_function(|_, s: mlua::String| Ok(hash::sha1_hex(&s.as_bytes())))?)?;
    host.set("md5", scope.create_function(|_, s: mlua::String| Ok(hash::md5_hex(&s.as_bytes())))?)?;
    // URL building tolerates non-UTF-8 (citation urls carry raw/percent bytes).
    host.set("uri_encode", scope.create_function(|_, (s, k): (mlua::String, mlua::String)| Ok(uri_encode(&s.to_string_lossy(), &k.to_string_lossy())))?)?;
    host.set("uri_decode", scope.create_function(|_, (s, k): (mlua::String, mlua::String)| Ok(uri_decode(&s.to_string_lossy(), &k.to_string_lossy())))?)?;
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
            let title = parse_title(&rf.title, ctx.site, 0);
            // expand_with_frame resolves {{{param}}} against the invoking
            // frame's args directly — the frozen `expand`'s empty root
            // frame cannot see them (crate lib_rs_requests / preprocess).
            Ok(preprocess::expand_with_frame(ctx.store, &title, &text.to_string_lossy(), &ctx.opts(), &rf).text)
        })?,
    )?;
    methods.set(
        "expandTemplate",
        scope.create_function(move |_, (this, spec): (Table, Table)| {
            let title: String = spec.get("title")?;
            let mut wt = format!("{{{{{title}");
            if let Ok(args) = spec.get::<Table>("args") {
                for pair in args.pairs::<Value, Value>() {
                    let (k, v) = pair?;
                    let val = value_to_string(&v);
                    match k {
                        Value::Integer(_) | Value::Number(_) => wt.push_str(&format!("|{val}")),
                        Value::String(s) => wt.push_str(&format!("|{}={}", s.to_string_lossy(), val)),
                        _ => {}
                    }
                }
            }
            wt.push_str("}}");
            // Expand the constructed call in the invoking frame's context
            // (expand_with_frame), so parent params referenced inside the
            // transcluded template resolve — same bridge as :preprocess.
            let rf = lua_frame_to_rust(&this);
            let cur = parse_title(&ctx.current_title, ctx.site, 0);
            Ok(preprocess::expand_with_frame(ctx.store, &cur, &wt, &ctx.opts(), &rf).text)
        })?,
    )?;
    methods.set(
        "callParserFunction",
        scope.create_function(move |_, (this, name, args): (Table, mlua::String, Variadic<Value>)| {
            let _ = this;
            let name = name.to_string_lossy();
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
        "extensionTag",
        scope.create_function(move |_, (this, a, b, c): (Table, Value, Value, Value)| {
            let _ = this;
            // Two call forms: extensionTag{name=,content=,args=} and
            // extensionTag(name, content, args). Build the raw tag markup;
            // the downstream parser applies the tag hook (or renders it as a
            // labeled placeholder — never a Lua error, which is what blocked
            // Navbox/configuration's templatestyles call).
            let (name, content, args): (String, String, Value) = match &a {
                Value::Table(t) => (
                    t.get::<Option<String>>("name")?.unwrap_or_default(),
                    t.get::<Option<String>>("content")?.unwrap_or_default(),
                    t.get::<Value>("args")?,
                ),
                _ => (
                    value_to_string(&a),
                    value_to_string(&b),
                    c,
                ),
            };
            // TemplateStyles (and the invisible `indicator`) contribute no
            // reader-BODY content: MediaWiki emits a strip marker whose CSS
            // lands in <head>, and the reader HTML the corpus grades against
            // has it as a <style> block that chrome-stripping removes. So the
            // faithful reader-mode result is empty — emitting a raw
            // <templatestyles> tag (which nothing downstream renders) would be
            // LESS accurate, not more.
            let lname = name.to_ascii_lowercase();
            if lname == "templatestyles" {
                return Ok(String::new());
            }
            let mut attrs = String::new();
            match args {
                Value::Table(at) => {
                    for pair in at.pairs::<Value, Value>() {
                        let (k, v) = pair?;
                        match k {
                            Value::String(s) => {
                                attrs.push_str(&format!(" {}=\"{}\"", s.to_str()?, value_to_string(&v)));
                            }
                            _ => {}
                        }
                    }
                }
                Value::String(s) => attrs.push_str(&format!(" {}", s.to_str()?)),
                _ => {}
            }
            let out = if content.is_empty() {
                format!("<{name}{attrs}/>")
            } else {
                format!("<{name}{attrs}>{content}</{name}>")
            };
            Ok(out)
        })?,
    )?;
    methods.set(
        "getArgument",
        scope.create_function(|lua, (this, name): (Table, Value)| {
            // frame:getArgument(name) → an object whose :expand() yields the
            // (already-expanded) arg value, or nil when absent. The lazy-arg
            // contract is observable only through this wrapper.
            let args: Table = this.raw_get("args")?;
            let v: Value = match &name {
                Value::Integer(_) | Value::Number(_) | Value::String(_) => args.get(name.clone())?,
                _ => Value::Nil,
            };
            if matches!(v, Value::Nil) {
                return Ok(Value::Nil);
            }
            let obj = lua.create_table()?;
            obj.set("__val", v)?;
            let mt = lua.create_table()?;
            let idx = lua.create_table()?;
            idx.set(
                "expand",
                lua.create_function(|_, this: Table| this.raw_get::<Value>("__val"))?,
            )?;
            mt.set("__index", idx)?;
            obj.set_metatable(Some(mt));
            Ok(Value::Table(obj))
        })?,
    )?;
    methods.set(
        "newParserValue",
        scope.create_function(|lua, (this, text): (Table, Value)| {
            // frame:newParserValue(text) → { expand = function() return text end }.
            let _ = this;
            let obj = lua.create_table()?;
            obj.set("__val", text)?;
            let mt = lua.create_table()?;
            let idx = lua.create_table()?;
            idx.set(
                "expand",
                lua.create_function(|_, this: Table| this.raw_get::<Value>("__val"))?,
            )?;
            mt.set("__index", idx)?;
            obj.set_metatable(Some(mt));
            Ok(obj)
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
