//! LuaInvoker behavior, pinned to concrete input→output. Every module
//! source below is realistic Scribunto (param handling, mw.html infobox,
//! mw.ustring UTF-8 patterns, getParent fallthrough) and every assertion
//! checks a real computed value — none would pass against a stub invoker.

use std::collections::{BTreeMap, HashMap};

use wikimak_scribunto::{LuaBytecodeCache, LuaInvoker, LuaModuleSourceScope};
use wikimak_wikitext::{
    render, Frame, ModuleInvoker, NamespaceInfo, PageStore, RenderOptions, SiteConfig, Title,
};
use std::thread;
use std::time::Instant;

struct TestStore {
    pages: HashMap<(i32, String), String>,
    reads: std::cell::RefCell<HashMap<(i32, String), usize>>,
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
            localized: canon.to_string(),
            aliases: if canon.is_empty() { vec![] } else { vec![canon.to_string()] },
            case_first_letter: true,
        };
        namespaces.insert(0, ns(0, ""));
        namespaces.insert(8, ns(8, "MediaWiki"));
        namespaces.insert(10, ns(10, "Template"));
        namespaces.insert(828, ns(828, "Module"));
        TestStore {
            pages: HashMap::new(),
            reads: std::cell::RefCell::new(HashMap::new()),
            site: SiteConfig {
                site_name: "Test Wiki".into(),
                db_name: "testwiki".into(),
                lang: "en".into(),
                rtl: false,
                namespaces,
                interwiki: BTreeMap::new(),
                ..Default::default()
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

    fn reads_for_module(&self, name: &str) -> usize {
        self.reads
            .borrow()
            .get(&(828, name.to_string()))
            .copied()
            .unwrap_or(0)
    }
}

impl PageStore for TestStore {
    fn page_text(&self, title: &Title) -> Option<String> {
        *self
            .reads
            .borrow_mut()
            .entry((title.ns, title.text.clone()))
            .or_default() += 1;
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
fn standard_bit32_library_is_available_to_modules() {
    let mut store = TestStore::new();
    store.add_module(
        "Bits",
        r#"
        local bit32 = require('bit32')
        local p = {}
        function p.main()
            return table.concat({
                bit32.band(0xff, 0x35),
                bit32.bxor(0xaa, 0xff),
                bit32.rshift(0x80000000, 31),
                bit32.extract(0x3c, 2, 4)
            }, ',')
        end
        return p
        "#,
    );
    assert_eq!(
        invoke(&store, "Bits", "main", &frame_with(&[])).unwrap(),
        "53,85,1,15"
    );
}

#[test]
fn statistical_named_parser_function_call_accepts_options_table() {
    let mut store = TestStore::new();
    store.add_module(
        "Statistical",
        r####"
        local p = {}
        function p.GetStat(frame)
            local args = frame:getParent().args
            local place = args[1] or "Россия"
            return "#Н/Д" .. frame:callParserFunction{
                name = "#tag:ref",
                args = { place .. " > Данные не обнаружены", name = "population" }
            }
        end
        return p
        "####,
    );
    let mut parent_args = BTreeMap::new();
    parent_args.insert("1".to_string(), "Россия".to_string());
    let frame = Frame {
        args: BTreeMap::new(),
        parent: Some(Box::new(Frame {
            args: parent_args,
            parent: None,
            title: "Template:Population".into(),
        })),
        title: "Test page".into(),
    };
    assert_eq!(
        invoke(&store, "Statistical", "GetStat", &frame).unwrap(),
        "#Н/Д<ref name=\"population\">Россия > Данные не обнаружены</ref>"
    );
}

#[test]
fn call_parser_function_positional_form_uses_first_argument_after_colon() {
    let mut store = TestStore::new();
    store.add_module(
        "ParserPositional",
        r####"
        local p = {}
        function p.main(frame)
            return frame:callParserFunction("#if", { "yes", "then", "else" })
        end
        return p
        "####,
    );
    assert_eq!(invoke(&store, "ParserPositional", "main", &frame_with(&[])).unwrap(), "then");
}

#[test]
fn call_parser_function_rejects_table_argument_values() {
    let mut store = TestStore::new();
    store.add_module(
        "ParserArgs",
        r####"
        local p = {}
        function p.main(frame)
            return frame:callParserFunction{ name = "#tag:ref", args = { { "nested" } } }
        end
        return p
        "####,
    );
    let err = invoke(&store, "ParserArgs", "main", &frame_with(&[])).unwrap_err();
    assert!(err.contains("invalid type table for argument"), "got: {err}");
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
fn pairs_and_ipairs_honor_scribunto_proxy_metamethods() {
    let mut store = TestStore::new();
    store.add_module(
        "ProxyIteration",
        r#"
        local p = {}
        function p.main()
            local by_name = setmetatable({}, {
                __pairs = function()
                    local values = { first = "one", second = "two" }
                    return next, values, nil
                end
            })
            local names = {}
            for key, value in pairs(by_name) do names[#names + 1] = key .. "=" .. value end
            table.sort(names)

            local by_position = setmetatable({}, {
                __ipairs = function()
                    local values = { "three", "four" }
                    return ipairs(values)
                end
            })
            local positions = {}
            for _, value in ipairs(by_position) do positions[#positions + 1] = value end
            return table.concat(names, ",") .. "/" .. table.concat(positions, ",")
        end
        return p
        "#,
    );
    assert_eq!(
        invoke(&store, "ProxyIteration", "main", &frame_with(&[])).unwrap(),
        "first=one,second=two/three,four"
    );
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

#[test]
fn mw_html_nil_removes_attributes_and_styles() {
    let mut store = TestStore::new();
    store.add_module(
        "HNil",
        r#"
        local p = {}
        function p.main()
            local d = mw.html.create("div")
            d:attr("id", "old"):attr("id", nil):attr("title", nil)
            d:css("color", "red"):css("color", nil):css("display", nil)
            d:attr("data-kept", "yes"):css("float", "left")
            return d
        end
        return p
        "#,
    );
    assert_eq!(
        invoke(&store, "HNil", "main", &frame_with(&[])).unwrap(),
        r#"<div style="float:left" data-kept="yes"></div>"#
    );
}

#[test]
fn mw_html_builder_return_uses_tostring_metamethod() {
    let mut store = TestStore::new();
    store.add_module(
        "HReturn",
        r#"
        return {
            main = function()
                return mw.html.create("span"):addClass("map"):wikitext("Rīga")
            end,
            plain = function() return {} end
        }
        "#,
    );
    assert_eq!(
        invoke(&store, "HReturn", "main", &frame_with(&[])).unwrap(),
        r#"<span class="map">Rīga</span>"#
    );
    let err = invoke(&store, "HReturn", "plain", &frame_with(&[])).unwrap_err();
    assert!(err.contains("returned a table value"), "got: {err}");
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

#[test]
fn ustring_gsub_handles_unicode_minus_class() {
    let mut store = TestStore::new();
    store.add_module(
        "UMinus",
        r##"
        return {
            main = function()
                local normalized, count = mw.ustring.gsub("-1,2 –3,4 −5,6 —7,8", "[-–−—]", "-")
                normalized = mw.ustring.gsub(normalized, ",", ".")
                local bounded, boundedCount = mw.ustring.gsub("–a-b", "[-–−—]", "_", 1)
                return normalized .. "#" .. count .. "|" .. bounded .. "#" .. boundedCount
            end
        }
        "##,
    );
    assert_eq!(
        invoke(&store, "UMinus", "main", &frame_with(&[])).unwrap(),
        "-1.2 -3.4 -5.6 -7.8#4|_a-b#1"
    );
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

#[test]
fn frame_expand_template_resolves_the_template_argument() {
    let mut store = TestStore::new();
    store.add_page(10, "Refinfo", "({{{1}}})");
    store.add_module(
        "Expand",
        r#"
        local p = {}
        function p.main(frame)
            return frame:expandTemplate{ title = 'refinfo', args = { 'недоступная ссылка' } }
        end
        return p
        "#,
    );
    assert_eq!(
        invoke(&store, "Expand", "main", &frame_with(&[])).unwrap(),
        "(недоступная ссылка)"
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
fn mw_hash_module_default_uses_fnv164() {
    let mut store = TestStore::new();
    store.add_module(
        "Hash",
        r####"
        local p = {}
        function p.main(frame)
            local args = frame.args
            return mw.hash.hashValue(args[2] or "fnv164", args[1])
        end
        return p
        "####,
    );
    let f = frame_with(&[("1", "abc")]);
    assert_eq!(
        invoke(&store, "Hash", "main", &f).unwrap(),
        "d8dcca186bafadcb"
    );
}

#[test]
fn mw_hash_lists_fnv164() {
    let mut store = TestStore::new();
    store.add_module(
        "HashAlgorithms",
        r####"
        local p = {}
        function p.main()
            return table.concat(mw.hash.listAlgorithms(), ",")
        end
        return p
        "####,
    );
    assert_eq!(
        invoke(&store, "HashAlgorithms", "main", &frame_with(&[])).unwrap(),
        "md5,sha1,fnv164"
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

#[test]
fn shared_bytecode_cache_compiles_each_chunk_once_but_keeps_fresh_states() {
    let mut store = TestStore::new();
    store.add_module(
        "SharedCache",
        r#"
        _G.__loads = (_G.__loads or 0) + 1
        return { main = function(frame) return _G.__loads .. "/" .. frame.args.value end }
        "#,
    );
    let cache = LuaBytecodeCache::new();
    let first = LuaInvoker::with_cache(cache.clone());
    let second = LuaInvoker::with_cache(cache.clone());
    let frame_a = frame_with(&[("value", "first")]);
    let frame_b = frame_with(&[("value", "second")]);

    assert_eq!(first.invoke("SharedCache", "main", &frame_a, &store).unwrap(), "1/first");
    assert_eq!(second.invoke("SharedCache", "main", &frame_b, &store).unwrap(), "1/second");

    let stats = cache.stats();
    assert_eq!(stats.compilations, 5, "TRIM, args, proxy, bootstrap, module: {stats:?}");
    assert_eq!(stats.entries, 5, "one retained role/name entry per chunk: {stats:?}");
    assert!(stats.cache_hits > 0, "second state must load cached bytecode: {stats:?}");
    assert!(stats.source_bytes > 0);
    assert!(stats.bytecode_bytes > 0);
}

#[test]
fn current_head_generation_scope_reuses_module_source_without_sharing_lua_values() {
    let mut store = TestStore::new();
    store.add_module(
        "SourceReuse",
        "_G.loads = (_G.loads or 0) + 1; return { main = function() return _G.loads end }",
    );
    let cache = LuaBytecodeCache::new();
    let scope = LuaModuleSourceScope::new("generation-source-reuse");
    let frame = frame_with(&[]);

    assert_eq!(
        LuaInvoker::with_cache_and_source_scope(cache.clone(), scope.clone())
            .invoke("SourceReuse", "main", &frame, &store)
            .unwrap(),
        "1"
    );
    assert_eq!(
        LuaInvoker::with_cache_and_source_scope(cache.clone(), scope)
            .invoke("SourceReuse", "main", &frame, &store)
            .unwrap(),
        "1"
    );
    assert_eq!(store.reads_for_module("SourceReuse"), 1);
    let stats = cache.stats();
    assert_eq!(stats.module_source_entries, 1);
    assert!(stats.module_source_bytes > 0);
}

#[test]
fn changed_module_source_replaces_the_role_name_entry_and_runs_new_code() {
    let mut old_store = TestStore::new();
    old_store.add_module(
        "Changing",
        "return { main = function() return 'old' end }",
    );
    let mut new_store = TestStore::new();
    new_store.add_module(
        "Changing",
        "return { main = function() return 'new source behavior' end }",
    );
    let cache = LuaBytecodeCache::new();
    let frame = frame_with(&[]);

    assert_eq!(
        LuaInvoker::with_cache(cache.clone())
            .invoke("Changing", "main", &frame, &old_store)
            .unwrap(),
        "old"
    );
    let before = cache.stats();
    assert_eq!(before.entries, 5);
    assert_eq!(
        LuaInvoker::with_cache(cache.clone())
            .invoke("Changing", "main", &frame, &new_store)
            .unwrap(),
        "new source behavior"
    );
    let after = cache.stats();
    assert_eq!(after.compilations, before.compilations + 1);
    assert_eq!(after.entries, before.entries, "replacement must not accumulate revisions");
    assert_ne!(after.source_bytes, before.source_bytes, "retained source bytes must be updated");
    assert!(after.bytecode_bytes > 0);
}

#[test]
fn shared_bytecode_does_not_leak_tau_globals_or_require_tables() {
    let mut first_store = TestStore::new();
    first_store.tau_micros = 101 * 1_000_000;
    first_store.add_module(
        "SharedState",
        "_G.loads = (_G.loads or 0) + 1; return { value = _G.loads }",
    );
    first_store.add_module(
        "TauAware",
        r#"
        local shared = require('SharedState')
        local p = {}
        function p.main() return os.time() .. '/' .. shared.value .. '/' .. (_G.loads or 0) end
        return p
        "#,
    );
    let mut second_store = TestStore::new();
    second_store.tau_micros = 202 * 1_000_000;
    second_store.add_module(
        "SharedState",
        "_G.loads = (_G.loads or 0) + 1; return { value = _G.loads }",
    );
    second_store.add_module(
        "TauAware",
        r#"
        local shared = require('SharedState')
        local p = {}
        function p.main() return os.time() .. '/' .. shared.value .. '/' .. (_G.loads or 0) end
        return p
        "#,
    );
    let cache = LuaBytecodeCache::new();
    let frame = frame_with(&[]);

    assert_eq!(
        LuaInvoker::with_cache(cache.clone())
            .invoke("TauAware", "main", &frame, &first_store)
            .unwrap(),
        "101/1/1"
    );
    assert_eq!(
        LuaInvoker::with_cache(cache)
            .invoke("TauAware", "main", &frame, &second_store)
            .unwrap(),
        "202/1/1"
    );
}

#[test]
fn concurrent_first_use_compiles_without_duplicate_chunks() {
    let cache = LuaBytecodeCache::new();
    let mut workers = Vec::new();
    for worker in 0..8 {
        let cache = cache.clone();
        workers.push(thread::spawn(move || {
            let mut store = TestStore::new();
            let module = "return { main = function() return 'ready' end }";
            store.add_module("Concurrent", module);
            LuaInvoker::with_cache(cache)
                .invoke("Concurrent", "main", &frame_with(&[]), &store)
                .map(|value| (worker, value))
        }));
    }
    for worker in workers {
        assert_eq!(worker.join().unwrap().unwrap().1, "ready");
    }
    let stats = cache.stats();
    assert_eq!(stats.compilations, 5, "concurrent misses must serialize compilation: {stats:?}");
    assert_eq!(stats.entries, 5);
}

#[test]
fn measured_first_and_second_render_report_cache_work_separately() {
    let mut store = TestStore::new();
    store.add_module(
        "RenderFixture",
        r#"
        local p = {}
        function p.main(frame)
            return frame.args.value .. '/' .. mw.ustring.upper('fixture')
        end
        return p
        "#,
    );
    let title = Title::parse("Measured page", store.site());
    let text = "{{#invoke:RenderFixture|main|value=ok}}";
    let cache = LuaBytecodeCache::new();

    let first_vm_started = Instant::now();
    let first_invoker = LuaInvoker::with_cache(cache.clone());
    let first_vm_elapsed = first_vm_started.elapsed();
    let first_started = Instant::now();
    let first = render(
        &store,
        &title,
        text,
        &RenderOptions {
            invoker: Some(&first_invoker),
            ..Default::default()
        },
    );
    let first_elapsed = first_started.elapsed();
    let after_first = cache.stats();

    let second_vm_started = Instant::now();
    let second_invoker = LuaInvoker::with_cache(cache.clone());
    let second_vm_elapsed = second_vm_started.elapsed();
    let second_started = Instant::now();
    let second = render(
        &store,
        &title,
        text,
        &RenderOptions {
            invoker: Some(&second_invoker),
            ..Default::default()
        },
    );
    let second_elapsed = second_started.elapsed();
    let after_second = cache.stats();

    assert!(first.misses.failed_invokes.is_empty(), "{:?}", first.misses);
    assert!(second.misses.failed_invokes.is_empty(), "{:?}", second.misses);
    assert_eq!(after_first.compilations, 5);
    assert_eq!(after_second.compilations, after_first.compilations);
    eprintln!(
        "shared Lua cache fixture: fresh_invoker_first={:?} fresh_invoker_second={:?} first_render={:?} second_render={:?} compilations={} retained_source_bytes={} retained_bytecode_bytes={}",
        first_vm_elapsed,
        second_vm_elapsed,
        first_elapsed,
        second_elapsed,
        after_second.compilations,
        after_second.source_bytes,
        after_second.bytecode_bytes,
    );
}

#[test]
fn page_session_reuses_initialized_modules_but_separate_sessions_are_isolated() {
    let mut store = TestStore::new();
    store.add_module(
        "Shared",
        r#"
        _G.__shared_loads = (_G.__shared_loads or 0) + 1
        return { loads = _G.__shared_loads }
        "#,
    );
    store.add_module(
        "Session",
        r#"
        _G.__entry_loads = (_G.__entry_loads or 0) + 1
        local shared = require("Shared")
        local p = {}
        function p.main(frame)
            return table.concat({
                _G.__entry_loads,
                shared.loads,
                tostring(shared == require("Module:Shared")),
                frame.args.value,
                mw.title.getCurrentTitle().text,
                frame:getTitle(),
                mw.getCurrentFrame():getTitle()
            }, "/")
        end
        return p
        "#,
    );

    let first = LuaInvoker::new().unwrap();
    let mut page_a = frame_with(&[("value", "one")]);
    page_a.title = "Page A".into();
    let mut page_b = frame_with(&[("value", "two")]);
    page_b.title = "Page B".into();
    assert_eq!(
        first.invoke("Session", "main", &page_a, &store).unwrap(),
        "1/1/true/one/Page A/Module:Session/Module:Session"
    );
    assert_eq!(
        first.invoke("Session", "main", &page_b, &store).unwrap(),
        "1/1/true/two/Page B/Module:Session/Module:Session"
    );

    let second = LuaInvoker::new().unwrap();
    assert_eq!(
        second.invoke("Session", "main", &page_b, &store).unwrap(),
        "1/1/true/two/Page B/Module:Session/Module:Session"
    );
}

#[test]
fn one_wikitext_render_shares_the_lua_session_between_invokes() {
    let mut store = TestStore::new();
    store.add_module(
        "SessionRender",
        r#"
        _G.__render_loads = (_G.__render_loads or 0) + 1
        local p = {}
        function p.main(frame)
            return _G.__render_loads .. "/" .. frame.args.value .. "/"
                .. mw.title.getCurrentTitle().text .. "/" .. frame:getTitle()
        end
        return p
        "#,
    );
    let invoker = LuaInvoker::new().unwrap();
    let title = Title::parse("Rendered page", store.site());
    let output = render(
        &store,
        &title,
        "{{#invoke:SessionRender|main|value=one}} {{#invoke:SessionRender|main|value=two}}",
        &RenderOptions {
            invoker: Some(&invoker),
            ..Default::default()
        },
    );
    assert!(output.misses.failed_invokes.is_empty(), "{:?}", output.misses);
    assert!(output.html.contains("1/one/Rendered page/Module:SessionRender"));
    assert!(output.html.contains("1/two/Rendered page/Module:SessionRender"));
}

#[test]
fn nested_invoke_restores_the_outer_frame_and_host_context() {
    let mut store = TestStore::new();
    store.add_module(
        "Inner",
        r#"
        return {
            main = function(frame)
                return frame.args.value .. "/" .. mw.title.getCurrentTitle().text
                    .. "/" .. mw.getCurrentFrame().args.value
            end
        }
        "#,
    );
    store.add_module(
        "Outer",
        r#"
        return {
            main = function(frame)
                local nested = frame:preprocess("{{#invoke:Inner|main|value=inner}}")
                return nested .. "/" .. frame.args.value .. "/"
                    .. mw.title.getCurrentTitle().text
            end
        }
        "#,
    );
    let invoker = LuaInvoker::new().unwrap();
    let mut frame = frame_with(&[("value", "outer")]);
    frame.title = "Page A".into();
    assert_eq!(
        invoker.invoke("Outer", "main", &frame, &store).unwrap(),
        "inner/Page A/inner/outer/Page A"
    );
}

#[test]
fn retained_lua_require_proxy_works_after_the_first_scope_is_dropped() {
    let mut store = TestStore::new();
    store.add_module("RetainedDependency", "return { value = 'ready' }");
    store.add_module(
        "RetainedRequire",
        r#"
        local page_require = require
        local p = {}
        function p.main(frame)
            local dependency = page_require("RetainedDependency")
            return dependency.value .. "/" .. frame.args.value
        end
        return p
        "#,
    );
    let invoker = LuaInvoker::new().unwrap();
    assert_eq!(
        invoker
            .invoke(
                "RetainedRequire",
                "main",
                &frame_with(&[("value", "first")]),
                &store,
            )
            .unwrap(),
        "ready/first"
    );
    assert_eq!(
        invoker
            .invoke(
                "RetainedRequire",
                "main",
                &frame_with(&[("value", "later")]),
                &store,
            )
            .unwrap(),
        "ready/later"
    );
}

#[test]
fn first_module_load_failure_does_not_poison_the_page_session() {
    let mut store = TestStore::new();
    store.add_module(
        "BrokenFirst",
        "local missing = require('NoSuchModule'); return { main = function() return missing end }",
    );
    store.add_module(
        "AfterFailure",
        "return { main = function(frame) return frame.args.value end }",
    );
    let invoker = LuaInvoker::new().unwrap();
    assert!(invoker
        .invoke("BrokenFirst", "main", &frame_with(&[]), &store)
        .is_err());
    assert_eq!(
        invoker
            .invoke(
                "AfterFailure",
                "main",
                &frame_with(&[("value", "recovered")]),
                &store,
            )
            .unwrap(),
        "recovered"
    );
}

#[test]
fn per_invocation_limit_does_not_poison_the_page_session() {
    let mut store = TestStore::new();
    store.add_module(
        "Recover",
        r#"
        local p = {}
        function p.main(frame)
            if frame.args.loop == "yes" then while true do end end
            return frame.args.value
        end
        return p
        "#,
    );
    let inv = LuaInvoker::with_limits(50 * 1024 * 1024, 5_000_000);
    let looping = frame_with(&[("loop", "yes")]);
    let err = inv.invoke("Recover", "main", &looping, &store).unwrap_err();
    assert!(err.to_lowercase().contains("time limit") || err.contains("instruction"));

    let ok = frame_with(&[("value", "alive")]);
    assert_eq!(inv.invoke("Recover", "main", &ok, &store).unwrap(), "alive");
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

// -------------------------------------------------------- limit bypass guards

// A module that wraps a runaway loop in pcall and loops again must NOT be
// able to swallow the instruction-budget error indefinitely. Before the
// re-arm fix this never returned (the periodic budget error was caught by
// the inner pcall every time); now the guard becomes uncatchable once
// tripped and the error escapes to the Rust caller.
#[test]
fn pcall_cannot_swallow_instruction_budget() {
    let mut store = TestStore::new();
    store.add_module(
        "Evil",
        "return { main = function()
            while true do pcall(function() while true do end end) end
        end }",
    );
    // Small budget so the guard trips in milliseconds.
    let inv = LuaInvoker::with_limits(50 * 1024 * 1024, 5_000_000);
    let f = frame_with(&[]);
    let err = inv.invoke("Evil", "main", &f, &store).unwrap_err();
    assert!(
        err.to_lowercase().contains("time limit") || err.contains("instruction"),
        "got: {err}"
    );
}

// Nested pcall walls (the loop lives inside two layers of pcall) also can't
// hold the render thread: control eventually returns to the outermost,
// unprotected frame where the re-armed every-instruction killer escapes.
#[test]
fn nested_pcall_cannot_swallow_budget() {
    let mut store = TestStore::new();
    store.add_module(
        "Evil2",
        "return { main = function()
            while true do
                pcall(function()
                    while true do pcall(function() while true do end end) end
                end)
            end
        end }",
    );
    let inv = LuaInvoker::with_limits(50 * 1024 * 1024, 5_000_000);
    let f = frame_with(&[]);
    let err = inv.invoke("Evil2", "main", &f, &store).unwrap_err();
    assert!(
        err.to_lowercase().contains("time limit") || err.contains("instruction"),
        "got: {err}"
    );
}

// The wall-clock deadline is an independent backstop: with a huge
// instruction budget but a tiny time limit, a plain infinite loop is still
// bounded by wall time rather than instruction count.
#[test]
fn wall_clock_backstop_fires() {
    let mut store = TestStore::new();
    store.add_module(
        "Slow",
        "return { main = function() while true do end end }",
    );
    let inv = LuaInvoker::with_limits(50 * 1024 * 1024, u32::MAX)
        .with_time_limit(std::time::Duration::from_millis(200));
    let start = std::time::Instant::now();
    let f = frame_with(&[]);
    let err = inv.invoke("Slow", "main", &f, &store).unwrap_err();
    assert!(err.to_lowercase().contains("time limit"), "got: {err}");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "wall-clock guard should fire quickly, took {:?}",
        start.elapsed()
    );
}

// ------------------------------------------------- Scribunto built-in libs

#[test]
fn strict_and_libraryutil_are_requirable() {
    // Community modules `require('strict')` and `require('libraryUtil')` — the
    // Scribunto lualib modules that never appear in a wiki closure. strict
    // installs its _G metatable and returns it; libraryUtil.checkType enforces.
    let mut store = TestStore::new();
    store.add_module(
        "Lib",
        r####"
        require('strict')
        local checkType = require('libraryUtil').checkType
        local p = {}
        function p.main(f)
            local ok = pcall(checkType, 'f', 1, 5, 'string')  -- 5 is not a string
            checkType('f', 1, 'hi', 'string')                 -- passes
            return tostring(ok) .. "/ok"
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "Lib", "main", &f).unwrap(), "false/ok");
}

#[test]
fn require_resolves_localized_module_namespace() {
    // Non-English wikis require modules by the LOCALIZED Module: prefix; the
    // source is stored under the resolved (828, Name). Give ns 828 a localized
    // alias and require through it.
    let mut namespaces = BTreeMap::new();
    let ns = |id: i32, canon: &str, localized: &str| NamespaceInfo {
        id,
        canonical: canon.to_string(),
        localized: localized.to_string(),
        aliases: Vec::new(),
        case_first_letter: true,
    };
    namespaces.insert(0, ns(0, "", ""));
    namespaces.insert(828, ns(828, "Module", "Модуль"));
    let mut store = TestStore::new();
    store.site.namespaces = namespaces;
    store.add_module("Helper", "return { v = function() return 'hi' end }");
    store.add_module(
        "Main",
        r####"
        local p = {}
        function p.main(f)
            local h = require('Модуль:Helper')  -- localized prefix
            return h.v()
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "Main", "main", &f).unwrap(), "hi");
}

// ------------------------------------------------------------- mw.title (rich)

#[test]
fn mw_title_urls_and_subpage_fields() {
    let mut store = TestStore::new();
    store.site.server = "https://ex.org".into();
    store.site.script_path = "/w".into();
    store.add_module(
        "Ti",
        r####"
        local p = {}
        function p.main(f)
            local t = mw.title.makeTitle('Module', 'Foo/Bar/Baz')
            local m = mw.title.makeTitle('Template', 'X')  -- string namespace name
            return t.rootText .. "|" .. t.baseText .. "|" .. t.subpageText
                .. "|" .. t:fullUrl('action=edit')
                .. "|" .. m.namespace .. "|" .. tostring(m.talkPageTitle.namespace)
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "Ti", "main", &f).unwrap(),
        "Foo|Foo/Bar|Baz|https://ex.org/w/Module:Foo/Bar/Baz?action=edit|10|11"
    );
}

// -------------------------------------------------------------------- mw.uri

#[test]
fn mw_uri_new_parses_components() {
    let mut store = TestStore::new();
    store.add_module(
        "U",
        r####"
        local p = {}
        function p.main(f)
            local u = mw.uri.new('https://web.archive.org/web/2017/http://x.com/a?b=1#frag')
            return u.protocol .. "|" .. u.host .. "|" .. u.fragment .. "|" .. u.query.b
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "U", "main", &f).unwrap(),
        "https|web.archive.org|frag|1"
    );
}

// --------------------------------------------------------------- mw.language

#[test]
fn mw_language_getdir_and_formatdate_string() {
    let mut store = TestStore::new();
    store.add_module(
        "L",
        r####"
        local p = {}
        function p.main(f)
            local en = mw.language.new('en')
            local ar = mw.language.new('ar')
            -- formatDate accepts a timestamp STRING (parsed like #time).
            local d = en:formatDate('F', '2022-3-1')
            local names = mw.language.fetchLanguageNames('en', 'all')  -- empty table, no data
            local cnt = 0; for _ in pairs(names) do cnt = cnt + 1 end
            return en:getDir() .. "|" .. ar:getDir() .. "|" .. d .. "|" .. cnt
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "L", "main", &f).unwrap(), "ltr|rtl|March|0");
}

#[test]
fn mw_language_formatdate_uses_russian_genitive_months() {
    let mut store = TestStore::new();
    store.site.lang = "ru".into();
    store.add_module(
        "RuDate",
        r#"
        local p = {}
        function p.main()
            return mw.language.getContentLanguage():formatDate('j xg Y', '2026-10-18')
        end
        return p
        "#,
    );
    assert_eq!(
        invoke(&store, "RuDate", "main", &frame_with(&[])).unwrap(),
        "18 октября 2026"
    );
}

#[test]
fn current_title_is_the_outer_page_while_frame_title_is_the_module() {
    let mut store = TestStore::new();
    store.add_module(
        "Titles",
        r#"
        local p = {}
        function p.main(frame)
            return mw.title.getCurrentTitle().prefixedText .. '|' .. frame:getTitle()
        end
        return p
        "#,
    );
    let frame = Frame {
        args: BTreeMap::new(),
        parent: Some(Box::new(frame_with(&[]))),
        title: "Template:Wrapper".into(),
    };
    assert_eq!(
        invoke(&store, "Titles", "main", &frame).unwrap(),
        "Test page|Module:Titles"
    );
}

// ---------------------------------------------------------------- mw.ustring

#[test]
fn ustring_nul_class_pattern_and_gcodepoint() {
    let mut store = TestStore::new();
    store.add_module(
        "U2",
        r####"
        local p = {}
        function p.main(f)
            -- A control-char class built with a literal NUL must not crash the
            -- byte matcher (it did: "malformed pattern (missing ']')").
            local ctrl = "[" .. string.char(0) .. "-" .. string.char(8) .. "]"
            local hit = mw.ustring.find("ab\tcd", ctrl) and "found" or "clean"
            -- gcodepoint iterates codepoints.
            local cps = {}
            for c in mw.ustring.gcodepoint("A€") do cps[#cps+1] = c end
            -- sub defaults i to 1.
            local lead = mw.ustring.sub("héllo", nil, 2)
            return hit .. "|" .. cps[1] .. "," .. cps[2] .. "|" .. lead
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    // "ab\tcd" has no byte in 0..8 range (\t is 9), so "clean"; € is U+20AC.
    assert_eq!(invoke(&store, "U2", "main", &f).unwrap(), "clean|65,8364|hé");
}

#[test]
fn string_gains_ustring_method_aliases() {
    // Several wikis extend `string` with codepoint-aware method aliases so a
    // plain string can `:ulower()` / `:ulen()` (ukwiki CS1 leans on this).
    let mut store = TestStore::new();
    store.add_module(
        "S",
        r####"
        local p = {}
        function p.main(f)
            local s = "CAFÉ"
            return s:ulower() .. "|" .. s:ulen()
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "S", "main", &f).unwrap(), "café|4");
}

// ------------------------------------------------------------------- mw.site

#[test]
fn mw_site_namespace_subsets_are_objects() {
    let mut store = TestStore::new();
    store.add_module(
        "Si",
        r####"
        local p = {}
        function p.main(f)
            -- subjectNamespaces[id] must be the full namespace OBJECT, not a
            -- bare name (Namespace detect reads .name / iterates .aliases).
            local main = mw.site.subjectNamespaces[0]
            local tmpl = mw.site.namespaces['Template']
            local ok = (main.name == "") and "main-ok" or "main-bad"
            return ok .. "|" .. tmpl.id .. "|" .. type(tmpl.aliases)
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "Si", "main", &f).unwrap(), "main-ok|10|table");
}

// ------------------------------------------------------------- frame:extensionTag

#[test]
fn frame_extension_tag_templatestyles_and_generic() {
    let mut store = TestStore::new();
    store.add_module(
        "E",
        r####"
        local p = {}
        function p.main(frame)
            -- templatestyles is invisible reader chrome -> empty.
            local ts = frame:extensionTag{ name = 'templatestyles', args = { src = 'M/styles.css' } }
            -- a generic tag round-trips to markup.
            local ref = frame:extensionTag('ref', 'body', { name = 'r1' })
            return "[" .. ts .. "]" .. ref
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(
        invoke(&store, "E", "main", &f).unwrap(),
        "[]<ref name=\"r1\">body</ref>"
    );
}

// ----------------------------------------------------------------- mw.wikibase

#[test]
fn mw_wikibase_present_but_empty() {
    // No Wikidata depot: mw.wikibase EXISTS (so guarding modules don't crash on
    // `index field 'wikibase'`) and every lookup returns nil/empty.
    let mut store = TestStore::new();
    store.add_module(
        "W",
        r####"
        local p = {}
        function p.main(f)
            local ent = mw.wikibase.getEntity()
            local stmts = mw.wikibase.getBestStatements('Q1', 'P1')
            return tostring(ent) .. "|" .. #stmts .. "|" .. tostring(mw.wikibase.getLabel('Q1'))
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "W", "main", &f).unwrap(), "nil|0|nil");
}

// ------------------------------------------------------------------- mw.text

#[test]
fn mw_text_unstrip_helpers() {
    let mut store = TestStore::new();
    store.add_module(
        "Tx",
        r####"
        local p = {}
        function p.main(f)
            -- unstripNoWiki has no strip state to consult -> identity.
            local a = mw.text.unstripNoWiki("plain")
            -- killMarkers removes UNIQ…QINU marker syntax.
            local b = mw.text.killMarkers("x\127UNIQ--nowiki-0-QINU\127y")
            return a .. "|" .. b
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "Tx", "main", &f).unwrap(), "plain|xy");
}

// ---------------------------------------------------------------- mw.message

#[test]
fn mw_message_new_raw_message() {
    let mut store = TestStore::new();
    store.add_module(
        "Mr",
        r####"
        local p = {}
        function p.main(f)
            -- newRawMessage uses its argument as raw text, with $N params; a
            -- single table argument is the whole parameter list.
            local a = mw.message.newRawMessage('Hello $1 and $2', {'Ada', 'Bob'}):plain()
            local b = mw.message.newRawMessage('$1%', 42):plain()
            return a .. "|" .. b
        end
        return p
        "####,
    );
    let f = frame_with(&[]);
    assert_eq!(invoke(&store, "Mr", "main", &f).unwrap(), "Hello Ada and Bob|42%");
}
