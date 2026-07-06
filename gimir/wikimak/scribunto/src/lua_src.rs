//! Pure-Lua half of the mw.* host library. Scribunto itself ships large
//! parts of mw.* as Lua (mw.html is a Lua builder; mw.ustring's fallback
//! is Lua over the native string library) — porting that structure is
//! the sanctioned route (plan §3.3), so those pieces live here as source
//! rather than being re-expressed as Rust/mlua glue. The Rust side
//! (`mwlib`) supplies only the primitives a sandbox cannot compute in
//! Lua: UTF-8 codepoint math, the store (titles/messages/content), τ
//! date formatting, and hashing — all reached through the `host` table.
//!
//! ustring pattern semantics: codepoint-correct len/sub/upper/lower/
//! char/codepoint/byteoffset and codepoint-correct RESULT POSITIONS from
//! find/match (byte offsets returned by PUC's matcher are translated back
//! to codepoints via `host`). Pattern CLASSES (%a, %w, '.') still match
//! at PUC's byte granularity — correct for ASCII classes over UTF-8 text
//! and for literal (including multi-byte literal) patterns, which covers
//! the overwhelming majority of real module usage. See crate gaps.

/// Loaded as `return function(mw, host, frame) … end` and called once per
/// invoke with the freshly built `mw`/`host` tables and the frame.
pub const BOOTSTRAP: &str = r##"
return function(mw, host, frame)
local unpack = unpack or table.unpack
local floor = math.floor

--------------------------------------------------------------------- ustring
local ustring = {}
mw.ustring = ustring
ustring.maxStringLength = 2^30
ustring.maxPatternLength = 2^30

ustring.len       = host.ustring_len
ustring.sub       = host.ustring_sub
ustring.upper     = host.ustring_upper
ustring.lower     = host.ustring_lower
ustring.char      = host.ustring_char
ustring.codepoint = host.ustring_codepoint
ustring.byteoffset= host.ustring_byteoffset
ustring.offset    = host.ustring_byteoffset
ustring.rep       = string.rep
ustring.format    = string.format
ustring.toNFC     = function(s) return s end

function ustring.find(s, pattern, init, plain)
    init = init or 1
    local binit = host.cp_to_byte(s, init)
    local res = { string.find(s, pattern, binit, plain) }
    if res[1] == nil then return nil end
    res[1] = host.byte_to_cp(s, res[1])
    res[2] = host.byte_to_cp(s, res[2])
    for i = 3, #res do
        if type(res[i]) == "number" then res[i] = host.byte_to_cp(s, res[i]) end
    end
    return unpack(res)
end

function ustring.match(s, pattern, init)
    init = init or 1
    local binit = host.cp_to_byte(s, init)
    local res = { string.match(s, pattern, binit) }
    for i = 1, #res do
        if type(res[i]) == "number" then res[i] = host.byte_to_cp(s, res[i]) end
    end
    return unpack(res)
end

function ustring.gmatch(s, pattern)
    return string.gmatch(s, pattern)
end

function ustring.gsub(s, pattern, repl, n)
    if n == nil then return string.gsub(s, pattern, repl) end
    return string.gsub(s, pattern, repl, n)
end

--------------------------------------------------------------------- text
local text = {}
mw.text = text

function text.trim(s, charset)
    charset = charset or "%s"
    return (s:gsub("^[" .. charset .. "]*(.-)[" .. charset .. "]*$", "%1"))
end

function text.gsplit(s, sep, plain)
    local pos = 1
    local done = false
    return function()
        if done then return nil end
        local b, e = string.find(s, sep, pos, plain)
        if not b then
            done = true
            return string.sub(s, pos)
        end
        local piece = string.sub(s, pos, b - 1)
        pos = e + 1
        if b > e then pos = pos + 1 end
        return piece
    end
end

function text.split(s, sep, plain)
    local out = {}
    for piece in text.gsplit(s, sep, plain) do
        out[#out + 1] = piece
    end
    return out
end

function text.listToText(list, sep, conj)
    sep = sep or ", "
    conj = conj or " and "
    local n = #list
    if n == 0 then return "" end
    if n == 1 then return tostring(list[1]) end
    if n == 2 then return tostring(list[1]) .. conj .. tostring(list[2]) end
    local parts = {}
    for i = 1, n - 1 do parts[i] = tostring(list[i]) end
    return table.concat(parts, sep) .. conj .. tostring(list[n])
end

local ENTITIES = {
    ["<"] = "&lt;", [">"] = "&gt;", ["&"] = "&amp;",
    ['"'] = "&quot;", ["'"] = "&#039;", [" "] = "&#32;",
}
function text.encode(s, charset)
    charset = charset or "<>&\"'"
    return (s:gsub("[" .. charset:gsub("(%W)", "%%%1") .. "]", function(c)
        return ENTITIES[c] or ("&#" .. host.byte(c) .. ";")
    end))
end

local NAMED = {
    lt = "<", gt = ">", amp = "&", quot = '"', apos = "'", nbsp = "\194\160",
}
function text.decode(s, decodeNamedEntities)
    s = s:gsub("&#[xX](%x+);", function(h) return host.ustring_char(tonumber(h, 16)) end)
    s = s:gsub("&#(%d+);", function(d) return host.ustring_char(tonumber(d)) end)
    if decodeNamedEntities then
        s = s:gsub("&(%a+);", function(n) return NAMED[n] or ("&" .. n .. ";") end)
    else
        s = s:gsub("&(lt);", "<"):gsub("&(gt);", ">"):gsub("&(amp);", "&")
             :gsub("&(quot);", '"'):gsub("&(apos);", "'")
    end
    return s
end

function text.nowiki(s)
    -- Insert entities so wikitext metacharacters render literally.
    s = s:gsub("&", "&amp;")
    local map = {
        ['"'] = "&#34;", ["&"] = "&amp;", ["'"] = "&#39;", ["<"] = "&lt;",
        [">"] = "&gt;", ["="] = "&#61;", ["["] = "&#91;", ["]"] = "&#93;",
        ["{"] = "&#123;", ["|"] = "&#124;", ["}"] = "&#125;",
    }
    for ch, ent in pairs(map) do
        if ch ~= "&" then s = s:gsub("%" .. ch, ent) end
    end
    return (s:gsub("\n([%*#:;])", "\n&#%1;"))
end

function text.tag(name, attrs, content)
    if type(name) == "table" then
        content = name.content
        attrs = name.attrs
        name = name.name
    end
    local buf = { "<", name }
    if attrs then
        local keys = {}
        for k in pairs(attrs) do keys[#keys + 1] = k end
        table.sort(keys)
        for _, k in ipairs(keys) do
            buf[#buf + 1] = " " .. k .. '="' .. text.encode(tostring(attrs[k]), "<>&\"") .. '"'
        end
    end
    if content == nil or content == false then
        buf[#buf + 1] = " />"
        return table.concat(buf)
    end
    buf[#buf + 1] = ">"
    buf[#buf + 1] = tostring(content)
    buf[#buf + 1] = "</" .. name .. ">"
    return table.concat(buf)
end

function text.truncate(str, length, ellipsis, adjustLength)
    ellipsis = ellipsis or "..."
    local slen = ustring.len(str)
    if length == nil or slen <= math.abs(length) then return str end
    if length > 0 then
        local keep = length
        if adjustLength then keep = length - ustring.len(ellipsis) end
        if keep < 0 then keep = 0 end
        return ustring.sub(str, 1, keep) .. ellipsis
    else
        local keep = -length
        if adjustLength then keep = keep - ustring.len(ellipsis) end
        if keep < 0 then keep = 0 end
        return ellipsis .. ustring.sub(str, slen - keep + 1)
    end
end

--------------------------------------------------------------------- json
local function json_encode(v, seen)
    local t = type(v)
    if t == "nil" then return "null"
    elseif t == "boolean" then return v and "true" or "false"
    elseif t == "number" then
        if floor(v) == v and math.abs(v) < 1e15 then return string.format("%d", v) end
        return string.format("%.14g", v)
    elseif t == "string" then
        local esc = v:gsub('[%z\1-\31\\"]', function(c)
            local m = { ['"'] = '\\"', ["\\"] = "\\\\", ["\n"] = "\\n", ["\r"] = "\\r",
                        ["\t"] = "\\t", ["\b"] = "\\b", ["\f"] = "\\f" }
            return m[c] or string.format("\\u%04x", string.byte(c))
        end)
        return '"' .. esc .. '"'
    elseif t == "table" then
        seen = seen or {}
        if seen[v] then error("mw.text.jsonEncode: cannot encode a table with cyclic references") end
        seen[v] = true
        local n = 0
        local isarr = true
        for k in pairs(v) do
            n = n + 1
            if type(k) ~= "number" or k ~= floor(k) or k < 1 then isarr = false end
        end
        local out
        if n == 0 then
            out = "{}"
        elseif isarr and n == #v then
            local parts = {}
            for i = 1, n do parts[i] = json_encode(v[i], seen) end
            out = "[" .. table.concat(parts, ",") .. "]"
        else
            local keys = {}
            for k in pairs(v) do keys[#keys + 1] = k end
            table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
            local parts = {}
            for _, k in ipairs(keys) do
                parts[#parts + 1] = json_encode(tostring(k), seen) .. ":" .. json_encode(v[k], seen)
            end
            out = "{" .. table.concat(parts, ",") .. "}"
        end
        seen[v] = nil
        return out
    end
    error("mw.text.jsonEncode: cannot encode a " .. t)
end
function text.jsonEncode(value, flags) return json_encode(value) end

local function json_decode(s)
    local pos = 1
    local function skip()
        local _, e = s:find("^[ \t\r\n]*", pos)
        pos = e + 1
    end
    local parse_value
    local function parse_string()
        pos = pos + 1
        local buf = {}
        while true do
            local c = s:sub(pos, pos)
            if c == "" then error("mw.text.jsonDecode: unterminated string") end
            if c == '"' then pos = pos + 1; break end
            if c == "\\" then
                local n = s:sub(pos + 1, pos + 1)
                local m = { ['"'] = '"', ["\\"] = "\\", ["/"] = "/", n_ = "\n",
                            t_ = "\t", r_ = "\r", b_ = "\b", f_ = "\f" }
                if n == "u" then
                    local hex = s:sub(pos + 2, pos + 5)
                    buf[#buf + 1] = host.ustring_char(tonumber(hex, 16))
                    pos = pos + 6
                else
                    local simple = { ["n"] = "\n", ["t"] = "\t", ["r"] = "\r",
                                     ["b"] = "\b", ["f"] = "\f", ['"'] = '"',
                                     ["\\"] = "\\", ["/"] = "/" }
                    buf[#buf + 1] = simple[n] or n
                    pos = pos + 2
                end
            else
                buf[#buf + 1] = c
                pos = pos + 1
            end
        end
        return table.concat(buf)
    end
    local function parse_number()
        local b, e = s:find("^%-?%d+%.?%d*[eE]?[%+%-]?%d*", pos)
        local num = tonumber(s:sub(b, e))
        pos = e + 1
        return num
    end
    parse_value = function()
        skip()
        local c = s:sub(pos, pos)
        if c == "{" then
            pos = pos + 1
            local obj = {}
            skip()
            if s:sub(pos, pos) == "}" then pos = pos + 1; return obj end
            while true do
                skip()
                local k = parse_string()
                skip()
                pos = pos + 1 -- ':'
                obj[k] = parse_value()
                skip()
                local d = s:sub(pos, pos)
                pos = pos + 1
                if d == "}" then break end
            end
            return obj
        elseif c == "[" then
            pos = pos + 1
            local arr = {}
            skip()
            if s:sub(pos, pos) == "]" then pos = pos + 1; return arr end
            while true do
                arr[#arr + 1] = parse_value()
                skip()
                local d = s:sub(pos, pos)
                pos = pos + 1
                if d == "]" then break end
            end
            return arr
        elseif c == '"' then
            return parse_string()
        elseif c == "t" then pos = pos + 4; return true
        elseif c == "f" then pos = pos + 5; return false
        elseif c == "n" then pos = pos + 4; return nil
        else
            return parse_number()
        end
    end
    local ok, res = pcall(parse_value)
    if not ok then error(res) end
    return res
end
function text.jsonDecode(s, flags) return json_decode(s) end

--------------------------------------------------------------------- html
local html = {}
mw.html = html
local HtmlMeta = {}
HtmlMeta.__index = HtmlMeta
local VOID = { br = true, hr = true, img = true, input = true, meta = true,
               link = true, wbr = true, area = true, base = true, col = true }

local function new_node(tagName)
    return setmetatable({
        tagName = tagName,
        attributes = {},   -- ordered { {name=,val=}, ... }
        styles = {},       -- ordered { {name=,val=}, ... }
        classes = {},
        nodes = {},        -- children: strings or builders
        parent = nil,
    }, HtmlMeta)
end

function html.create(tagName, args)
    local node = new_node(tagName or "")
    node.root = node
    return node
end

function HtmlMeta:tag(tagName, args)
    local child = new_node(tagName)
    child.parent = self
    child.root = self.root
    self.nodes[#self.nodes + 1] = child
    return child
end

function HtmlMeta:node(builder)
    if builder then self.nodes[#self.nodes + 1] = builder end
    return self
end

function HtmlMeta:wikitext(...)
    local n = select("#", ...)
    for i = 1, n do
        local v = select(i, ...)
        if v ~= nil then self.nodes[#self.nodes + 1] = tostring(v) end
    end
    return self
end

function HtmlMeta:newline()
    self.nodes[#self.nodes + 1] = "\n"
    return self
end

function HtmlMeta:attr(name, value)
    if type(name) == "table" then
        for k, v in pairs(name) do self:attr(k, v) end
        return self
    end
    for _, a in ipairs(self.attributes) do
        if a.name == name then a.val = value; return self end
    end
    self.attributes[#self.attributes + 1] = { name = name, val = value }
    return self
end

function HtmlMeta:addClass(class)
    if class ~= nil and class ~= "" then self.classes[#self.classes + 1] = tostring(class) end
    return self
end

function HtmlMeta:css(name, value)
    if type(name) == "table" then
        for k, v in pairs(name) do self:css(k, v) end
        return self
    end
    for _, s in ipairs(self.styles) do
        if s.name == name then s.val = value; return self end
    end
    self.styles[#self.styles + 1] = { name = name, val = value }
    return self
end

function HtmlMeta:cssText(text)
    self._cssText = (self._cssText or "") .. tostring(text) .. ";"
    return self
end

function HtmlMeta:done()
    return self.parent or self
end

function HtmlMeta:allDone()
    return self.root or self
end

local function render_attrs(node)
    local buf = {}
    local styleBuf = {}
    if #node.classes > 0 then
        buf[#buf + 1] = ' class="' .. text.encode(table.concat(node.classes, " "), "<>&\"") .. '"'
    end
    for _, s in ipairs(node.styles) do
        styleBuf[#styleBuf + 1] = s.name .. ":" .. tostring(s.val)
    end
    if node._cssText then styleBuf[#styleBuf + 1] = node._cssText:gsub(";$", "") end
    if #styleBuf > 0 then
        buf[#buf + 1] = ' style="' .. text.encode(table.concat(styleBuf, ";"), "<>&\"") .. '"'
    end
    for _, a in ipairs(node.attributes) do
        buf[#buf + 1] = " " .. a.name .. '="' .. text.encode(tostring(a.val), "<>&\"") .. '"'
    end
    return table.concat(buf)
end

local function render_node(node, buf)
    if type(node) == "string" then
        buf[#buf + 1] = node
        return
    end
    if node.tagName == nil or node.tagName == "" then
        for _, child in ipairs(node.nodes) do render_node(child, buf) end
        return
    end
    buf[#buf + 1] = "<" .. node.tagName .. render_attrs(node)
    if VOID[node.tagName] and #node.nodes == 0 then
        buf[#buf + 1] = " />"
        return
    end
    buf[#buf + 1] = ">"
    for _, child in ipairs(node.nodes) do render_node(child, buf) end
    buf[#buf + 1] = "</" .. node.tagName .. ">"
end

function HtmlMeta:tostring()
    local buf = {}
    render_node(self, buf)
    return table.concat(buf)
end
HtmlMeta.__tostring = HtmlMeta.tostring

--------------------------------------------------------------------- uri
local uri = {}
mw.uri = uri
uri.encode = function(s, enctype) return host.uri_encode(s, enctype or "QUERY") end
uri.decode = function(s, enctype) return host.uri_decode(s, enctype or "QUERY") end
function uri.anchorEncode(s)
    return (host.uri_encode(tostring(s):gsub(" ", "_"), "WIKI"))
end
local function build_url(base, page, query)
    local url = base .. host.uri_encode(tostring(page):gsub(" ", "_"), "WIKI")
    if query then
        local parts = {}
        for k, v in pairs(query) do
            parts[#parts + 1] = host.uri_encode(k, "QUERY") .. "=" .. host.uri_encode(tostring(v), "QUERY")
        end
        if #parts > 0 then url = url .. "?" .. table.concat(parts, "&") end
    end
    return url
end
function uri.localUrl(page, query) return build_url(mw.site.scriptPath .. "/", page, query) end
function uri.fullUrl(page, query) return build_url(mw.site.server .. mw.site.scriptPath .. "/", page, query) end
function uri.canonicalUrl(page, query) return uri.fullUrl(page, query) end

--------------------------------------------------------------------- language
local Lang = {}
Lang.__index = Lang
function Lang:getCode() return self.code end
function Lang:isRTL() return host.rtl end
function Lang:lc(s) return host.ustring_lower(s) end
function Lang:uc(s) return host.ustring_upper(s) end
function Lang:lcfirst(s)
    if s == "" then return s end
    return host.ustring_lower(host.ustring_sub(s, 1, 1)) .. host.ustring_sub(s, 2)
end
function Lang:ucfirst(s)
    if s == "" then return s end
    return host.ustring_upper(host.ustring_sub(s, 1, 1)) .. host.ustring_sub(s, 2)
end
function Lang:caseFold(s) return host.ustring_lower(s) end
function Lang:formatNum(n, opts)
    local s = tostring(n)
    local sign = ""
    if s:sub(1, 1) == "-" then sign = "-"; s = s:sub(2) end
    local int, frac = s:match("^(%d*)(.*)$")
    int = int:reverse():gsub("(%d%d%d)", "%1,"):reverse():gsub("^,", "")
    return sign .. int .. frac
end
function Lang:formatDate(fmt, timestamp, local_)
    return host.format_date(fmt, timestamp)
end
function Lang:parseFormattedNumber(s)
    return tonumber((tostring(s):gsub(",", "")))
end

local language = {}
mw.language = language
function language.new(code) return setmetatable({ code = code }, Lang) end
language.getContentLanguage = function() return language.new(host.lang_code) end
mw.getContentLanguage = language.getContentLanguage
language.isRTL = function(code) return host.rtl end
language.fetchLanguageName = function(code, inLanguage) return "" end
language.isKnownLanguageTag = function(code) return false end
language.isSupportedLanguage = function(code) return false end

--------------------------------------------------------------------- message
local Msg = {}
Msg.__index = Msg
function Msg:params(...)
    local args = { ... }
    if type(args[1]) == "table" then args = args[1] end
    for _, v in ipairs(args) do self.args[#self.args + 1] = v end
    return self
end
Msg.rawParams = Msg.params
function Msg:numParams(...) return self:params(...) end
function Msg:plain()
    local s = host.message_plain(self.key)
    for i, v in ipairs(self.args) do
        s = s:gsub("%$" .. i, tostring(v))
    end
    return s
end
Msg.text = Msg.plain
function Msg:exists() return host.message_exists(self.key) end
function Msg:isDisabled() return false end
local message = {}
mw.message = message
function message.new(key, ...)
    return setmetatable({ key = key, args = { ... } }, Msg)
end
function message.newFallbackSequence(...) return message.new((select(1, ...))) end
function message.rawParam(v) return v end
function message.numParam(v) return v end

--------------------------------------------------------------------- hash
local hash = {}
mw.hash = hash
function hash.hashValue(algo, value)
    algo = tostring(algo):lower()
    if algo == "sha1" then return host.sha1(value) end
    if algo == "md5" then return host.md5(value) end
    error("mw.hash.hashValue: unsupported algorithm '" .. tostring(algo) .. "'")
end
function hash.listAlgorithms() return { "md5", "sha1" } end

--------------------------------------------------------------------- log / misc
mw.logQueue = {}
function mw.log(...)
    local parts = {}
    for i = 1, select("#", ...) do parts[i] = tostring(select(i, ...)) end
    local line = table.concat(parts, "\t")
    mw.logQueue[#mw.logQueue + 1] = line
    host.log(line)
end
function mw.logObject(obj, prefix)
    local s = (prefix and (prefix .. " = ") or "") .. (text.jsonEncode(obj))
    mw.logQueue[#mw.logQueue + 1] = s
    host.log(s)
end
function mw.dumpObject(obj) return text.jsonEncode(obj) end

function mw.clone(v)
    local seen = {}
    local function cp(x)
        if type(x) ~= "table" then return x end
        if seen[x] then return seen[x] end
        local t = {}
        seen[x] = t
        for k, val in pairs(x) do t[cp(k)] = cp(val) end
        return setmetatable(t, getmetatable(x))
    end
    return cp(v)
end

function mw.isSubsting() return false end
mw.getCurrentFrame = function() return frame end
-- Store-backed loaders (require is installed by the Rust host after this
-- bootstrap; referenced lazily so it exists by call time).
function mw.loadData(name) return require(name) end
function mw.loadJsonData(name) return text.jsonDecode(host.page_content(name) or "null") end

--------------------------------------------------------------------- title
local title = {}
mw.title = title
local TitleMeta = {}
TitleMeta.__index = TitleMeta
TitleMeta.__eq = function(a, b)
    return rawget(a, "prefixedText") == rawget(b, "prefixedText")
end
TitleMeta.__tostring = function(t) return rawget(t, "prefixedText") end
function TitleMeta:getContent() return host.page_content(self.prefixedText) end
local function wrap_title(raw)
    if raw == nil then return nil end
    return setmetatable(raw, TitleMeta)
end
function title.new(text_, namespace)
    if text_ == nil then return nil end
    return wrap_title(host.title_resolve(tostring(text_), namespace))
end
function title.makeTitle(namespace, titleText, fragment)
    if titleText == nil then return nil end
    local resolved = host.title_make(namespace, tostring(titleText))
    if fragment then resolved.fragment = fragment end
    return wrap_title(resolved)
end
function title.getCurrentTitle() return title.new(host.current_title) end
title.equals = function(a, b)
    return rawget(a, "prefixedText") == rawget(b, "prefixedText")
end

--------------------------------------------------------------------- site
mw.site = host.site

end
"##;

/// Metatable for `frame.args`: positional args are stored under integer
/// keys and named args under string keys; this bridge lets both `args[1]`
/// and `args["1"]` reach the positional value (MediaWiki treats them as
/// one), the observable half of the "lazy args" contract now that the
/// preprocessor hands us pre-expanded strings.
pub const ARGS_METATABLE: &str = r#"
return {
    __index = function(t, k)
        if type(k) == "string" then
            local n = tonumber(k)
            if n and n == math.floor(n) then return rawget(t, n) end
        elseif type(k) == "number" then
            return rawget(t, tostring(k))
        end
        return nil
    end,
}
"#;

