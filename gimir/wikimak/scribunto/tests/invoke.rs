//! LuaInvoker behavior, pinned to concrete input→output. Every module
//! source below is realistic Scribunto (param handling, mw.html infobox,
//! mw.ustring UTF-8 patterns, getParent fallthrough) and every assertion
//! checks a real computed value — none would pass against a stub invoker.

use std::collections::{BTreeMap, HashMap};

use wikimak_scribunto::LuaInvoker;
use wikimak_wikitext::{
    Frame, ModuleInvoker, NamespaceInfo, PageStore, SiteConfig, Title,
};

struct TestStore {
    pages: HashMap<(i32, String), String>,
    site: SiteConfig,
    tau_micros: i64,
}

impl TestStore {
    /// τ = 2005-03-01 12:34:56 UTC (unix 1_109_680_496).
    fn new() -> Self {
        let mut namespaces = BTreeMap::new();
        let ns = |id: i32, canon: &str| NamespaceInfo {
            id,
            canonical: canon.to_string(),
            aliases: if canon.is_empty() { vec![] } else { vec![canon.to_string()] },
            case_first_letter: true,
        };
        namespaces.insert(0, ns(0, ""));
        namespaces.insert(8, ns(8, "MediaWiki"));
        namespaces.insert(10, ns(10, "Template"));
        namespaces.insert(828, ns(828, "Module"));
        TestStore {
            pages: HashMap::new(),
            site: SiteConfig {
                site_name: "Test Wiki".into(),
                db_name: "testwiki".into(),
                lang: "en".into(),
                rtl: false,
                namespaces,
                interwiki: BTreeMap::new(),
            },
            tau_micros: 1_109_680_496 * 1_000_000,
        }
    }

    fn add_module(&mut self, name: &str, src: &str) {
        self.pages.insert((828, name.to_string()), src.to_string());
    }

    fn add_page(&mut self, ns: i32, name: &str, text: &str) {
        self.pages.insert((ns, name.to_string()), text.to_string());
    }
}

impl PageStore for TestStore {
    fn page_text(&self, title: &Title) -> Option<String> {
        self.pages.get(&(title.ns, title.text.clone())).cloned()
    }
    fn page_exists(&self, title: &Title) -> bool {
        self.pages.contains_key(&(title.ns, title.text.clone()))
    }
    fn site(&self) -> &SiteConfig {
        &self.site
    }
    fn timestamp_micros(&self) -> i64 {
        self.tau_micros
    }
}

fn frame_with(args: &[(&str, &str)]) -> Frame {
    let mut m = BTreeMap::new();
    for (k, v) in args {
        m.insert(k.to_string(), v.to_string());
    }
    Frame { args: m, parent: None, title: "Test page".into() }
}

fn invoke(store: &TestStore, module: &str, func: &str, frame: &Frame) -> Result<String, String> {
    let inv = LuaInvoker::new().unwrap();
    inv.invoke(module, func, frame, store)
}

// ------------------------------------------------------------------ params

#[test]
fn positional_and_named_args_echo() {
    let mut store = TestStore::new();
    store.add_module(
        "Echo",
        r#"
        local p = {}
        function p.main(frame)
            return frame.args[1] .. "/" .. (frame.args.greeting or "?") .. "/" .. (frame.args["2"] or "-")
        end
        return p
        "#,
    );
    let frame = frame_with(&[("1", "hello"), ("2", "world"), ("greeting", "hi")]);
    assert_eq!(invoke(&store, "Echo", "main", &frame).unwrap(), "hello/hi/world");
}

#[test]
fn args_iterate_with_pairs() {
    let mut store = TestStore::new();
    store.add_module(
        "Count",
        r#"
        local p = {}
        function p.main(frame)
            local positional, named = 0, 0
            for k, v in pairs(frame.args) do
                if type(k) == "number" then positional = positional + 1 else named = named + 1 end
            end
            return positional .. "," .. named
        end
        return p
        "#,
    );
    let frame = frame_with(&[("1", "a"), ("2", "b"), ("x", "c"), ("y", "d")]);
    assert_eq!(invoke(&store, "Count", "main", &frame).unwrap(), "2,2");
}

#[test]
fn getparent_arg_fallthrough() {
    // The classic {{#invoke}}-inside-a-template pattern: the module reads
    // its own args, falling back to the parent frame's (the template call).
    let mut store = TestStore::new();
    store.add_module(
        "Args",
        r#"
        local p = {}
        function p.main(frame)
            local parent = frame:getParent()
            local v = frame.args.n or (parent and parent.args.n) or "none"
            return "n=" .. v .. " title=" .. frame:getTitle()
        end
        return p
        "#,
    );
    let mut frame = frame_with(&[]); // invoke frame has no args
    frame.title = "Module:Args".into();
    frame.parent = Some(Box::new(Frame {
        args: BTreeMap::from([("n".to_string(), "42".to_string())]),
        parent: None,
        title: "Template:Foo".into(),
    }));
    assert_eq!(invoke(&store, "Args", "main", &frame).unwrap(), "n=42 title=Module:Args");
}

// ------------------------------------------------------------------ mw.html

#[test]
fn mw_html_infobox_builder() {
    let mut store = TestStore::new();
    store.add_module(
        "Infobox",
        r#"
        local p = {}
        function p.main(frame)
            local root = mw.html.create("table")
            root:addClass("infobox"):attr("id", "ib")
            root:tag("caption"):wikitext(frame.args.title):done()
            local tr = root:tag("tr")
            tr:tag("th"):wikitext("Born"):done()
            tr:tag("td"):wikitext(frame.args.born):done()
            return tostring(root)
        end
        return p
        "#,
    );
    let frame = frame_with(&[("title", "Ada"), ("born", "1815")]);
    let html = invoke(&store, "Infobox", "main", &frame).unwrap();
    assert_eq!(
        html,
        r#"<table class="infobox" id="ib"><caption>Ada</caption><tr><th>Born</th><td>1815</td></tr></table>"#
    );
}

#[test]
fn mw_html_css_and_void_and_escaping() {
    let mut store = TestStore::new();
    store.add_module(
        "H",
        r#"
        local p = {}
        function p.main(frame)
            local d = mw.html.create("div")
            d:css("color", "red"):css{ ["font-weight"] = "bold" }
            d:attr("title", "a & b")     -- attribute values ARE escaped
            d:wikitext("a < b & c")      -- wikitext children are NOT escaped
            d:tag("br")
            return tostring(d)
        end
        return p
        "#,
    );
    let frame = frame_with(&[]);
    assert_eq!(
        invoke(&store, "H", "main", &frame).unwrap(),
        r#"<div style="color:red;font-weight:bold" title="a &amp; b">a < b & c<br /></div>"#
    );
}

// ------------------------------------------------------------------ ustring

#[test]
fn ustring_utf8_semantics() {
    let mut store = TestStore::new();
    store.add_module(
        "U",
        r####"
        local p = {}
        local u = mw.ustring
        function p.len(f)   return tostring(u.len("héllo")) end
        function p.sub(f)   return u.sub("héllo", 2, 3) end
        function p.upper(f) return u.upper("café") end
        function p.gsub(f)  local s, n = u.gsub("a→b→c", "→", "-"); return s .. "#" .. n end
        function p.match(f) return u.match("Price: 42€", "%d+") end
        function p.find(f)  return tostring(u.find("héllo", "l")) end
        function p.cp(f)    return tostring(u.codepoint("A€", 2)) end
        function p.char(f)  return u.char(8364) end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "U", "len", &f).unwrap(), "5");
    assert_eq!(invoke(&store, "U", "sub", &f).unwrap(), "él");
    assert_eq!(invoke(&store, "U", "upper", &f).unwrap(), "CAFÉ");
    assert_eq!(invoke(&store, "U", "gsub", &f).unwrap(), "a-b-c#2");
    assert_eq!(invoke(&store, "U", "match", &f).unwrap(), "42");
    // "l" is the 3rd codepoint (é is two bytes) — codepoint index, not byte 4.
    assert_eq!(invoke(&store, "U", "find", &f).unwrap(), "3");
    assert_eq!(invoke(&store, "U", "cp", &f).unwrap(), "8364");
    assert_eq!(invoke(&store, "U", "char", &f).unwrap(), "€");
}

// ------------------------------------------------------------------ mw.text

#[test]
fn mw_text_helpers() {
    let mut store = TestStore::new();
    store.add_module(
        "T",
        r####"
        local p = {}
        function p.trim(f)  return "[" .. mw.text.trim("  hi  ") .. "]" end
        function p.split(f) return table.concat(mw.text.split("a,b,,c", ",", true), "|") end
        function p.list(f)  return mw.text.listToText({"a", "b", "c"}) end
        function p.json(f)
            local t = mw.text.jsonDecode('{"a":1,"b":[2,3],"c":"x"}')
            return t.a .. "/" .. t.b[2] .. "/" .. t.c
        end
        function p.jsonenc(f) return mw.text.jsonEncode({10, 20, 30}) end
        function p.nowiki(f) return mw.text.nowiki("[[x]]") end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "T", "trim", &f).unwrap(), "[hi]");
    assert_eq!(invoke(&store, "T", "split", &f).unwrap(), "a|b||c");
    assert_eq!(invoke(&store, "T", "list", &f).unwrap(), "a, b and c");
    assert_eq!(invoke(&store, "T", "json", &f).unwrap(), "1/3/x");
    assert_eq!(invoke(&store, "T", "jsonenc", &f).unwrap(), "[10,20,30]");
    assert_eq!(invoke(&store, "T", "nowiki", &f).unwrap(), "&#91;&#91;x&#93;&#93;");
}

// ------------------------------------------------------------------ mw.title

#[test]
fn mw_title_lookup_and_content() {
    let mut store = TestStore::new();
    store.add_page(10, "Foo", "template body");
    store.add_module(
        "Ti",
        r####"
        local p = {}
        function p.main(f)
            local t = mw.title.new("Template:Foo")
            local missing = mw.title.new("Template:Nope")
            return t.namespace .. "|" .. t.text .. "|" .. t.prefixedText
                .. "|" .. tostring(t.exists) .. "|" .. tostring(missing.exists)
                .. "|" .. (t:getContent() or "nil")
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "Ti", "main", &f).unwrap(),
        "10|Foo|Template:Foo|true|false|template body"
    );
}

// ------------------------------------------------------------------ language

#[test]
fn mw_language_formatting_uses_tau() {
    let mut store = TestStore::new();
    store.add_module(
        "L",
        r####"
        local p = {}
        function p.main(f)
            local lang = mw.language.getContentLanguage()
            return lang:formatNum(1234567) .. "|" .. lang:ucfirst("hello")
                .. "|" .. lang:lcfirst("Hello") .. "|" .. lang:formatDate("Y-m-d")
                .. "|" .. lang:formatDate("j F Y")
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "L", "main", &f).unwrap(),
        "1,234,567|Hello|hello|2005-03-01|1 March 2005"
    );
}

// ------------------------------------------------------------------ os / τ

#[test]
fn os_date_and_time_honor_tau() {
    let mut store = TestStore::new();
    store.add_module(
        "O",
        r####"
        local p = {}
        function p.main(f)
            return os.date("!%Y-%m-%d %H:%M:%S") .. "|" .. tostring(os.time())
                .. "|" .. os.date("!*t").year
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "O", "main", &f).unwrap(),
        "2005-03-01 12:34:56|1109680496|2005"
    );
}

// ------------------------------------------------------------------ hash / message

#[test]
fn mw_hash_sha1() {
    let mut store = TestStore::new();
    store.add_module(
        "Ha",
        r####"
        local p = {}
        function p.main(f) return mw.hash.hashValue("sha1", "abc") end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "Ha", "main", &f).unwrap(),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
}

#[test]
fn mw_message_fallback_and_override() {
    let mut store = TestStore::new();
    store.add_page(8, "Mainpage", "Welcome $1");
    store.add_module(
        "Me",
        r####"
        local p = {}
        function p.main(f)
            local a = mw.message.new("no-such-key"):plain()
            local b = mw.message.new("Mainpage"):params("Bob"):plain()
            return a .. "|" .. b
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "Me", "main", &f).unwrap(),
        "\u{29FC}no-such-key\u{29FD}|Welcome Bob"
    );
}

// ------------------------------------------------------------------ require / loadData

#[test]
fn require_submodule_and_cache() {
    let mut store = TestStore::new();
    store.add_module(
        "Shared",
        r####"
        _G.__load_count = (_G.__load_count or 0) + 1
        return { double = function(x) return x * 2 end, loads = _G.__load_count }
        "####,
    );
    store.add_module(
        "Main",
        r####"
        local p = {}
        function p.main(f)
            local a = require("Module:Shared")
            local b = require("Module:Shared") -- cached: same table, one load
            return a.double(21) .. "|" .. a.loads .. "|" .. tostring(a == b)
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "Main", "main", &f).unwrap(), "42|1|true");
}

// ------------------------------------------------------------------ frame:preprocess

#[test]
fn frame_preprocess_substitutes_params() {
    // preprocess::expand is (currently) a passthrough for template calls,
    // so this pins our param-substitution bridge specifically.
    let mut store = TestStore::new();
    store.add_module(
        "Pre",
        r####"
        local p = {}
        function p.main(frame)
            return frame:preprocess("x={{{1}}} y={{{missing|def}}} z={{{2|}}}")
        end
        return p
        "####,
    );
    let frame = frame_with(&[("1", "AAA"), ("2", "BBB")]);
    assert_eq!(
        invoke(&store, "Pre", "main", &frame).unwrap(),
        "x=AAA y=def z=BBB"
    );
}

// ------------------------------------------------------------------ logging

#[test]
fn mw_log_is_collected() {
    let mut store = TestStore::new();
    store.add_module(
        "Lg",
        r####"
        local p = {}
        function p.main(f)
            mw.log("first", "second")
            mw.log("third")
            return "ok"
        end
        return p
        "####,
    );
    let inv = LuaInvoker::new().unwrap();
    let f = frame_with(&[]);
    assert_eq!(inv.invoke("Lg", "main", &f, &store).unwrap(), "ok");
    assert_eq!(inv.logs(), vec!["first\tsecond".to_string(), "third".to_string()]);
}

// ------------------------------------------------------------------ error paths

#[test]
fn missing_module_is_error_not_panic() {
    let store = TestStore::new();
    let f = frame_with(&[]);
    let err = invoke(&store, "Ghost", "main", &f).unwrap_err();
    assert!(err.contains("No such module"), "got: {err}");
    assert!(err.contains("Ghost"), "got: {err}");
}

#[test]
fn non_table_return_is_error() {
    let mut store = TestStore::new();
    store.add_module("Bad", "return 42");
    let f = frame_with(&[]);
    let err = invoke(&store, "Bad", "main", &f).unwrap_err();
    assert!(err.contains("must return a table"), "got: {err}");
}

#[test]
fn missing_function_is_error() {
    let mut store = TestStore::new();
    store.add_module("Ok", "return { main = function() return 'x' end }");
    let f = frame_with(&[]);
    let err = invoke(&store, "Ok", "nope", &f).unwrap_err();
    assert!(err.contains("does not exist"), "got: {err}");
}

#[test]
fn runtime_error_becomes_script_error() {
    let mut store = TestStore::new();
    store.add_module(
        "Boom",
        "return { main = function() error('kaboom') end }",
    );
    let f = frame_with(&[]);
    let err = invoke(&store, "Boom", "main", &f).unwrap_err();
    assert!(err.contains("kaboom"), "got: {err}");
}

#[test]
fn infinite_loop_hits_instruction_budget() {
    let mut store = TestStore::new();
    store.add_module(
        "Loop",
        "return { main = function() while true do end end }",
    );
    // Small budget so the guard fires in milliseconds instead of ~7 s.
    let inv = LuaInvoker::with_limits(50 * 1024 * 1024, 5_000_000);
    let f = frame_with(&[]);
    let err = inv.invoke("Loop", "main", &f, &store).unwrap_err();
    assert!(
        err.to_lowercase().contains("time limit") || err.contains("instruction"),
        "got: {err}"
    );
}

#[test]
fn runaway_allocation_hits_memory_limit() {
    let mut store = TestStore::new();
    store.add_module(
        "Mem",
        r####"
        return { main = function()
            local s = "x"
            for i = 1, 40 do s = s .. s end
            return s
        end }
        "####,
    );
    // 8 MB cap: doubling blows past it within ~23 iterations.
    let inv = LuaInvoker::with_limits(8 * 1024 * 1024, 400_000_000);
    let f = frame_with(&[]);
    let err = inv.invoke("Mem", "main", &f, &store).unwrap_err();
    assert!(err.to_lowercase().contains("memory"), "got: {err}");
}

// ------------------------------------------------------------------ sandbox

#[test]
fn dangerous_globals_removed() {
    let mut store = TestStore::new();
    store.add_module(
        "Sb",
        r####"
        local p = {}
        function p.main(f)
            return tostring(io) .. "|" .. tostring(package) .. "|" .. tostring(loadstring)
                .. "|" .. tostring(dofile) .. "|" .. tostring(os.execute)
                .. "|" .. type(debug.traceback)
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "Sb", "main", &f).unwrap(),
        "nil|nil|nil|nil|nil|function"
    );
}
