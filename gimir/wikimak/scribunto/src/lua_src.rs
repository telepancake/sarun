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
-- mw.ustring.sub defaults i to 1 (Scribunto semantics); real modules rely on
-- it — IPA calls sub(s, nil, k) to take a leading slice.
function ustring.sub(s, i, j) return host.ustring_sub(s, i or 1, j) end
ustring.upper     = host.ustring_upper
ustring.lower     = host.ustring_lower
ustring.char      = host.ustring_char
ustring.codepoint = host.ustring_codepoint
ustring.byteoffset= host.ustring_byteoffset
ustring.offset    = host.ustring_byteoffset
ustring.rep       = string.rep
ustring.format    = string.format
ustring.toNFC     = function(s) return s end
ustring.toNFD     = function(s) return s end

-- gcodepoint(s, i, j): iterator over the codepoints of s[i..j] (Scribunto's
-- companion to string.gmatch, used by Lang and IPA to walk text by
-- codepoint). Built on the native codepoint extractor.
function ustring.gcodepoint(s, i, j)
    local cps = { host.ustring_codepoint(s, i or 1, j or -1) }
    local idx = 0
    return function()
        idx = idx + 1
        return cps[idx]
    end
end

-- PUC's pattern parser reads the pattern as a C string, so a LITERAL NUL
-- byte truncates it mid-class ("malformed pattern (missing ']')"). `%z` is
-- the Lua-pattern class for NUL and is parsed safely. Real modules build
-- control-character classes with an embedded NUL (CS1's invisible-char
-- strip, `[<NUL>-\8\11\12\14-\31]`), so translate literal NULs to %z before
-- handing the pattern to the byte matcher: `<NUL>-X` range starts expand to
-- `%z\1-X` (range preserved), bare NULs become `%z`.
local function fix_pat(p)
    if type(p) ~= "string" or not p:find("%z") then return p end
    p = p:gsub("%z%-", "%%z\1-")
    p = p:gsub("%z", "%%z")
    return p
end
ustring._fixPattern = fix_pat

function ustring.find(s, pattern, init, plain)
    init = init or 1
    local binit = host.cp_to_byte(s, init)
    local res = { string.find(s, fix_pat(pattern), binit, plain) }
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
    local res = { string.match(s, fix_pat(pattern), binit) }
    for i = 1, #res do
        if type(res[i]) == "number" then res[i] = host.byte_to_cp(s, res[i]) end
    end
    return unpack(res)
end

function ustring.gmatch(s, pattern)
    return string.gmatch(s, fix_pat(pattern))
end

function ustring.gsub(s, pattern, repl, n)
    if n == nil then return string.gsub(s, fix_pat(pattern), repl) end
    return string.gsub(s, fix_pat(pattern), repl, n)
end

-- Codepoint-aware string methods. Several wikis extend Lua's `string`
-- library with ustring aliases so a plain string can be lowercased/measured
-- by codepoint via method syntax (`Mode:ulower()`, `s:ulen()`). Those
-- definitions live in the wiki's Scribunto setup, NOT in a module closure, so
-- ukwiki's CS1 (which leans on `:ulower()` heavily) breaks without them.
-- Providing the same aliases here reproduces that environment.
string.ulower  = ustring.lower
string.uupper  = ustring.upper
string.ulen    = ustring.len
string.usub    = ustring.sub
string.ufind   = ustring.find
string.umatch  = ustring.match
string.ugsub   = ustring.gsub
string.ugmatch = ustring.gmatch

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

-- Parser strip-marker helpers. Scribunto's unstrip* consult the parser's
-- strip state; module args reach us already expanded, so we hold no such
-- state. unstripNoWiki returns its input verbatim (nothing to restore) and
-- unstrip/killMarkers remove any UNIQ…QINU marker syntax that leaked into
-- the text — honest no-state behavior, never fabricating content.
function text.killMarkers(s)
    return (s:gsub("\127[^\127]*QINU[^\127]*\127", ""))
end
function text.unstripNoWiki(s) return s end
function text.unstrip(s) return text.killMarkers(s) end

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
    -- query may be a table (k=v pairs) or an already-formatted query string
    -- (mw.title's fullUrl callers pass strings, e.g. 'action=watch').
    if type(query) == "string" then
        if query ~= "" then url = url .. "?" .. query end
    elseif type(query) == "table" then
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

-- mw.uri.new(s): parse a URL into its components. Real modules read the
-- pieces (webarchive reads uri.host to label archive services); the object
-- also stringifies back. Query is parsed into a { key = value } table.
local UriMeta = {}
UriMeta.__index = UriMeta
local function parse_query(q)
    local t = {}
    if not q or q == "" then return t end
    for pair in (q .. "&"):gmatch("([^&]*)&") do
        if pair ~= "" then
            local k, v = pair:match("^([^=]*)=?(.*)$")
            t[host.uri_decode(k, "QUERY")] = host.uri_decode(v, "QUERY")
        end
    end
    return t
end
function UriMeta:__tostring()
    local out = {}
    if self.protocol then out[#out + 1] = self.protocol .. ":" end
    if self.host then
        out[#out + 1] = "//"
        if self.userInfo and self.userInfo ~= "" then out[#out + 1] = self.userInfo .. "@" end
        out[#out + 1] = self.host
        if self.port then out[#out + 1] = ":" .. self.port end
    end
    out[#out + 1] = self.path or ""
    if self.query and next(self.query) then
        local parts = {}
        for k, v in pairs(self.query) do
            parts[#parts + 1] = host.uri_encode(k, "QUERY") .. "=" .. host.uri_encode(tostring(v), "QUERY")
        end
        out[#out + 1] = "?" .. table.concat(parts, "&")
    end
    if self.fragment then out[#out + 1] = "#" .. self.fragment end
    return table.concat(out)
end
function UriMeta:parse(s) return uri.new(s) end
function UriMeta:fullUrl() return tostring(self) end
function uri.new(s)
    s = tostring(s or "")
    local obj = setmetatable({ query = {} }, UriMeta)
    -- fragment
    local frag
    s, frag = s:match("^([^#]*)#?(.*)$")
    if frag and frag ~= "" then obj.fragment = frag end
    -- scheme
    local proto, rest = s:match("^([%a][%w+.-]*):(.*)$")
    if proto then obj.protocol = proto:lower() else rest = s end
    -- authority
    if rest:sub(1, 2) == "//" then
        local auth, tail = rest:sub(3):match("^([^/]*)(.*)$")
        rest = tail
        local userinfo, hostport = auth:match("^([^@]*)@(.*)$")
        if userinfo then obj.userInfo = userinfo else hostport = auth end
        local h, p = hostport:match("^(.-):(%d+)$")
        if h then obj.host = h; obj.port = tonumber(p) else obj.host = hostport end
    end
    -- path + query
    local path, q = rest:match("^([^?]*)%??(.*)$")
    obj.path = path
    if q and q ~= "" then obj.query = parse_query(q) end
    return obj
end

--------------------------------------------------------------------- language
local Lang = {}
Lang.__index = Lang
function Lang:getCode() return self.code end
-- Directionality: the content language's is known from siteinfo (host.rtl);
-- for an explicitly-constructed language we fall back to a small known-RTL
-- tag set (the scripts real modules build lang spans for).
local RTL_CODES = {
    ar=true, arc=true, dv=true, fa=true, ha=true, he=true, khw=true, ks=true,
    ku=true, ps=true, sd=true, ur=true, uz_AL=true, yi=true, ug=true, ["ckb"]=true,
    ["arz"]=true, ["azb"]=true, ["ary"]=true, ["fa-af"]=true, ["ur-PK"]=true,
}
function Lang:isRTL()
    if self.code == host.lang_code then return host.rtl end
    return RTL_CODES[self.code] == true
end
function Lang:getDir() return self:isRTL() and "rtl" or "ltr" end
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
mw.getLanguage = language.new
language.isRTL = function(code) return host.rtl end
language.fetchLanguageName = function(code, inLanguage) return "" end
-- fetchLanguageNames / getFallbacksFor need Wikimedia's CLDR language-name
-- and fallback tables, which we don't ship. Return the correct TYPE (an
-- empty table) so callers that iterate the result (CS1's language-tag map,
-- for one) run to completion instead of hitting a nil-call — an honest
-- "no data" degradation, not a value that dodges a specific error.
language.fetchLanguageNames = function(code, filter) return {} end
language.getFallbacksFor = function(code) return {} end
language.isKnownLanguageTag = function(code) return false end
language.isSupportedLanguage = function(code) return false end
language.isValidCode = function(code) return type(code) == "string" and code ~= "" end
language.isValidBuiltInCode = function(code) return false end

--------------------------------------------------------------------- message
local Msg = {}
Msg.__index = Msg
function Msg:params(...)
    local args = { ... }
    -- A single table argument is treated as the whole parameter list
    -- (Scribunto's messageMetatable:params semantics), so callers may pass
    -- either `msg:params(a, b)` or `msg:params({a, b})`.
    if #args == 1 and type(args[1]) == "table" then args = args[1] end
    for _, v in ipairs(args) do self.args[#self.args + 1] = v end
    return self
end
Msg.rawParams = Msg.params
function Msg:numParams(...) return self:params(...) end
function Msg:substituteParams(s)
    local params = self.args
    return (s:gsub("%$(%d+)", function(n)
        local v = params[tonumber(n)]
        if v == nil then return "$" .. n end
        return tostring(v)
    end))
end
function Msg:plain()
    -- Raw messages carry their own text; keyed ones resolve through the
    -- MediaWiki: namespace at τ (falling back to ⧼key⧽ when unset).
    local s = self.raw or host.message_plain(self.keys and self.keys[1] or self.key)
    return self:substituteParams(s)
end
Msg.text = Msg.plain
Msg.plainText = Msg.plain
function Msg:exists()
    if self.raw then return true end
    for _, k in ipairs(self.keys or { self.key }) do
        if host.message_exists(k) then return true end
    end
    return false
end
function Msg:isDisabled()
    if self.raw then return self.raw == "" or self.raw == "-" end
    return not self:exists()
end
function Msg:isBlank()
    local s = self.raw or (self:exists() and host.message_plain(self.keys and self.keys[1] or self.key) or "")
    return s == ""
end
Msg.inLanguage = function(self, lang) return self end
Msg.useDatabase = function(self, val) return self end
local message = {}
mw.message = message
function message.new(key, ...)
    local o = setmetatable({ key = key, keys = { key }, args = {} }, Msg)
    return o:params(...)
end
function message.newRawMessage(msg, ...)
    local o = setmetatable({ raw = msg, args = {} }, Msg)
    return o:params(...)
end
function message.newFallbackSequence(...)
    local keys = { ... }
    return setmetatable({ key = keys[1], keys = keys, args = {} }, Msg)
end
function message.rawParam(v) return v end
function message.numParam(v) return v end
mw.message.getDefaultLanguage = function() return mw.language.getContentLanguage() end

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
-- mw.addWarning surfaces a preview-only warning in MediaWiki's parser output;
-- the reader view never shows it, so collecting it into the log queue (never
-- nil) is the faithful reader-mode behavior.
function mw.addWarning(text) host.log("warning: " .. tostring(text)) end
-- mw.ext.* are per-extension interfaces (Commons tabular/JSON data via
-- mw.ext.data, ParserFunctions helpers, …). We ship no extension backends.
-- mw.ext.data.get(page) fetches a Commons `.tab` dataset; we have none, so it
-- returns an empty-but-STRUCTURED result ({ data = {}, schema = {fields={}} }).
-- Non-English CS1 config calls `mw.ext.data.get(...).data` without a nil
-- guard (only the en version checks `mw.ext.data == nil`), so returning an
-- empty dataset — rather than nil — lets those modules iterate to an empty
-- id-limit table and keep running: the honest "no tabular data" degradation.
mw.ext = {}
mw.ext.data = {
    get = function(page, langcode)
        return { data = {}, schema = { fields = {} } }
    end,
}
-- Store-backed loaders (require is installed by the Rust host after this
-- bootstrap; referenced lazily so it exists by call time).
function mw.loadData(name) return require(name) end
function mw.loadJsonData(name) return text.jsonDecode(host.page_content(name) or "null") end

--------------------------------------------------------------------- title
local title = {}
mw.title = title
local TitleMeta = {}
local wrap_title

-- Methods and lazily-computed fields on a title object. Kept in one table
-- so TitleMeta.__index can serve `t:method()` calls and `t.field` field
-- reads (talkPageTitle etc.) that would otherwise recurse if built eagerly.
local title_methods = {}
function title_methods.getContent(self) return host.page_content(self.prefixedText) end
function title_methods.fullUrl(self, query, proto)
    return uri.fullUrl(self.prefixedText, query)
end
function title_methods.localUrl(self, query)
    return uri.localUrl(self.prefixedText, query)
end
function title_methods.canonicalUrl(self, query)
    return uri.canonicalUrl(self.prefixedText, query)
end
function title_methods.partialUrl(self)
    return host.uri_encode(tostring(self.prefixedText):gsub(" ", "_"), "WIKI")
end
function title_methods.inNamespace(self, ns)
    local info = mw.site.namespaces[ns]
    local id = info and info.id or ns
    return self.namespace == id
end
function title_methods.inNamespaces(self, ...)
    for _, ns in ipairs({ ... }) do
        if self:inNamespace(ns) then return true end
    end
    return false
end
function title_methods.hasSubjectNamespace(self, ns)
    local info = mw.site.namespaces[self.namespace]
    local subj = info and info.subject or self.namespace
    local want = mw.site.namespaces[ns]
    return subj == (want and want.id or ns)
end
function title_methods.subPageTitle(self, text_)
    return title.makeTitle(self.namespace, self.text .. "/" .. tostring(text_))
end
function title_methods.isSubpageOf(self, other)
    local a, b = tostring(self.prefixedText), tostring(other.prefixedText)
    return a:sub(1, #b + 1) == (b .. "/")
end

-- Split `text` on '/' for subpage-derived fields, honoring whether the
-- namespace actually allows subpages (main namespace does not).
local function subpage_parts(ns, text_)
    local info = mw.site.namespaces[ns]
    if not (info and info.hasSubpages) or not text_:find("/", 1, true) then
        return text_, text_, text_
    end
    local root = text_:match("^([^/]*)")
    local base = text_:match("^(.*)/[^/]*$") or text_
    local sub = text_:match("([^/]*)$") or text_
    return root, base, sub
end

TitleMeta.__eq = function(a, b)
    return rawget(a, "prefixedText") == rawget(b, "prefixedText")
end
TitleMeta.__tostring = function(t) return rawget(t, "prefixedText") end
TitleMeta.__index = function(t, k)
    local m = title_methods[k]
    if m ~= nil then return m end
    local ns = rawget(t, "namespace")
    local txt = rawget(t, "text")
    if k == "rootText" then
        local r = select(1, subpage_parts(ns, txt)); return r
    elseif k == "baseText" then
        local _, b = subpage_parts(ns, txt); return b
    elseif k == "subpageText" then
        local _, _, s = subpage_parts(ns, txt); return s
    elseif k == "isSubpage" then
        local info = mw.site.namespaces[ns]
        if info and info.hasSubpages and txt:find("/", 1, true) then return true end
        return false
    elseif k == "isTalkPage" then
        return ns > 0 and ns % 2 == 1
    elseif k == "subjectPageTitle" then
        local info = mw.site.namespaces[ns]
        local subj = info and info.subject or ns
        return title.makeTitle(subj, txt)
    elseif k == "talkPageTitle" then
        local info = mw.site.namespaces[ns]
        local tk = info and info.talk
        if tk == nil or tk < 0 or tk == ns then return nil end
        return title.makeTitle(tk, txt)
    elseif k == "basePageTitle" then
        local _, b = subpage_parts(ns, txt)
        return title.makeTitle(ns, b)
    elseif k == "rootPageTitle" then
        local r = select(1, subpage_parts(ns, txt))
        return title.makeTitle(ns, r)
    elseif k == "canTalk" then
        local info = mw.site.namespaces[ns]
        return (info and info.talk and info.talk >= 0 and info.talk ~= ns) == true
    end
    return nil
end

wrap_title = function(raw)
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
title.compare = function(a, b)
    local x, y = tostring(a.prefixedText), tostring(b.prefixedText)
    if x < y then return -1 elseif x > y then return 1 else return 0 end
end

--------------------------------------------------------------------- wikibase
-- We ship no Wikidata depot, so no page has a linked entity. That is exactly
-- the state of a real wiki whose pages aren't connected to Wikidata:
-- mw.wikibase EXISTS but every lookup returns nil/empty. Providing the table
-- (rather than leaving mw.wikibase nil) lets the many infobox/citation
-- modules that guard on `mw.wikibase.getEntity()` take their no-data path
-- instead of crashing on `attempt to index field 'wikibase'`. This is honest
-- degradation — the correct answer given no data — not a value that dodges a
-- specific error. When a Wikidata depot is wired in, this table is replaced.
local wikibase = {}
mw.wikibase = wikibase
wikibase.getEntity = function(id) return nil end
wikibase.getEntityObject = function(id) return nil end
wikibase.getEntityIdForCurrentPage = function() return nil end
wikibase.getEntityIdForTitle = function(title, site) return nil end
wikibase.getEntityUrl = function(id) return nil end
wikibase.getLabel = function(id) return nil end
wikibase.getLabelWithLang = function(id) return nil, nil end
wikibase.getLabelByLang = function(id, lang) return nil end
wikibase.getDescription = function(id) return nil end
wikibase.getDescriptionWithLang = function(id) return nil, nil end
wikibase.getSitelink = function(id, globalSiteId) return nil end
wikibase.getBadges = function(id, globalSiteId) return {} end
wikibase.sitelinkExists = function(page, globalSiteId) return false end
wikibase.getBestStatements = function(id, property) return {} end
wikibase.getAllStatements = function(id, property) return {} end
wikibase.getReferencedEntityId = function(id, property, toIds) return nil end
wikibase.getPropertyOrder = function() return nil end
wikibase.getPropertyId = function(label) return nil end
wikibase.resolvePropertyId = function(propertyLabelOrId) return nil end
wikibase.formatValue = function(snak) return nil end
wikibase.formatValues = function(snaks) return nil end
wikibase.renderSnak = function(snak) return "" end
wikibase.renderSnaks = function(snaks) return "" end
wikibase.getEntityIdForTitle = function(title, globalSiteId) return nil end
wikibase.isValidEntityId = function(id) return false end
wikibase.entityExists = function(id) return false end
mw.wikibase.lexeme = { getEntity = function() return nil end }
mw.wikibase.entity = {}

--------------------------------------------------------------------- site
mw.site = host.site
-- interwikiMap needs the instance's interwikimap-at-τ, which the corpus
-- fixtures don't carry; return an empty list (correct type) so callers that
-- iterate it (CS1's language-prefix map) complete. The stats.* live counts
-- likewise have no data source here — 0 is the honest answer, not a dodge.
mw.site.interwikiMap = function(filter) return {} end
if mw.site.stats then
    mw.site.stats.pagesInCategory = function(category, which) return 0 end
    mw.site.stats.pagesInNamespace = function(ns) return 0 end
    mw.site.stats.usersInGroup = function(group) return 0 end
end

end
"##;

/// Scribunto-shipped Lua libraries that modules `require()` by bare name —
/// they live in Scribunto's `lualib/`, NOT as wiki `Module:` pages, so they
/// are never in a captured closure. Real Scribunto resolves them through the
/// package path / preloaded `package.loaded`; we resolve them here as a
/// require fallback keyed by exact name. Returning `None` means "not a
/// built-in, look in the store."
pub fn builtin_lib(name: &str) -> Option<&'static str> {
    match name {
        // `require('mw')` returns the live mw table (a global by module-run
        // time). Scribunto preloads it in package.loaded; we mirror that.
        "mw" => Some("return mw"),
        // Some modules require an mw.* sub-library by name; return the live
        // sub-table (built by the bootstrap before any module runs).
        "mw.ustring" => Some("return mw.ustring"),
        "mw.text" => Some("return mw.text"),
        "mw.title" => Some("return mw.title"),
        "mw.uri" => Some("return mw.uri"),
        "mw.language" => Some("return mw.language"),
        "mw.message" => Some("return mw.message"),
        "mw.html" => Some("return mw.html"),
        "mw.hash" => Some("return mw.hash"),
        "mw.site" => Some("return mw.site"),
        "libraryUtil" | "Module:libraryUtil" => Some(LIBRARYUTIL),
        "strict" | "Module:strict" => Some(STRICT),
        _ => None,
    }
}

/// Scribunto's `libraryUtil` — argument-type checkers used pervasively by
/// mw.* libraries and by community modules (Arguments, Hatnote, …). Faithful
/// port of Scribunto's `lualib/libraryUtil.lua`; no `debug` dependency.
const LIBRARYUTIL: &str = r#"
local libraryUtil = {}

function libraryUtil.checkType( name, argIdx, arg, expectType, nilOk )
    if arg == nil and nilOk then return end
    if type( arg ) ~= expectType then
        local msg
        if arg == nil then msg = 'no value' else msg = type( arg ) end
        error( string.format( "bad argument #%d to '%s' (%s expected, got %s)",
            argIdx, name, expectType, msg ), 3 )
    end
end

function libraryUtil.checkTypeMulti( name, argIdx, arg, expectTypes )
    local argType = type( arg )
    for _, expectType in ipairs( expectTypes ) do
        if argType == expectType then return end
    end
    local n = #expectTypes
    local typeList
    if n > 1 then
        typeList = table.concat( expectTypes, ', ', 1, n - 1 ) .. ', or ' .. expectTypes[n]
    else
        typeList = expectTypes[1]
    end
    error( string.format( "bad argument #%d to '%s' (%s expected, got %s)",
        argIdx, name, typeList, argType ), 3 )
end

function libraryUtil.checkTypeForIndex( index, value, expectType )
    if type( value ) ~= expectType then
        error( string.format( "value for index '%s' must be %s, %s given",
            index, expectType, type( value ) ), 3 )
    end
end

function libraryUtil.checkTypeForNamedArg( name, argName, arg, expectType, nilOk )
    if arg == nil and nilOk then return end
    if type( arg ) ~= expectType then
        error( string.format( "bad named argument %s to '%s' (%s expected, got %s)",
            argName, name, expectType, type( arg ) == 'nil' and 'no value' or type( arg ) ), 3 )
    end
end

function libraryUtil.makeCheckSelfFunction( libraryName, varName, selfObj, selfObjDesc )
    return function ( self, method )
        if self ~= selfObj then
            error( string.format(
                "%s: invalid %s. Did you call %s with a dot instead of a colon, " ..
                "i.e. %s.%s() instead of %s:%s()?",
                libraryName, selfObjDesc, method, varName, method, varName, method ), 3 )
        end
    end
end

return libraryUtil
"#;

/// Scribunto's `strict` — flags reads/writes of undeclared globals. The real
/// module uses `debug.getinfo` to exempt the main chunk and C; our sandbox
/// exposes only `debug.traceback`, so `what()` degrades to permissive
/// (treats every frame as "C"), matching Scribunto's own behavior when
/// `debug.getinfo` is unavailable: `require('strict')` is a safe no-op that
/// still installs the metatable other modules may inspect. `require`s
/// nothing, returns the `_G` metatable like the original.
const STRICT: &str = r#"
local mt = getmetatable( _G )
if mt == nil then
    mt = {}
    setmetatable( _G, mt )
end

mt.__declared = mt.__declared or {}

local getinfo = debug and debug.getinfo
local function what()
    if not getinfo then return 'C' end
    local d = getinfo( 3, 'S' )
    return d and d.what or 'C'
end

mt.__newindex = function ( t, n, v )
    if not mt.__declared[n] then
        local w = what()
        if w ~= 'main' and w ~= 'C' then
            error( "assign to undeclared variable '" .. n .. "'", 2 )
        end
        mt.__declared[n] = true
    end
    rawset( t, n, v )
end

mt.__index = function ( t, n )
    if not mt.__declared[n] and what() ~= 'C' then
        error( "variable '" .. n .. "' is not declared", 2 )
    end
    return rawget( t, n )
end

return mt
"#;

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

