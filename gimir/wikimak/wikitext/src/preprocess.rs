//! Preprocessor/transclusion engine (plan §3.2): template expansion in
//! MediaWiki's exact order — <includeonly>/<noinclude>/<onlyinclude>,
//! {{{param|default}}}, parser functions (core + ParserFunctions),
//! magic words/variables (τ-resolved via PageStore::timestamp_micros),
//! depth/loop limits. Reference: Preprocessor_Hash.php + PPFrame
//! semantics (GPL — behavior reference only, no code reuse).
//!
//! OWNED BY: the preprocessor agent.
//!
//! Expansion model vs MediaWiki: template arguments are expanded eagerly
//! in the *calling* frame at the point the call is reached — which yields
//! MediaWiki's output for every non-pathological case because the caller
//! frame IS the current frame at that point. The observable difference is
//! only laziness (an unused/dead-branch argument that loops or errors is
//! still evaluated here); that is recorded in the crate gaps.

use crate::html;
use crate::magic::{self, BEHAVIOR_SWITCHES};
use crate::{Frame, PageStore, RenderMisses, RenderOptions, SiteConfig, Title};

const MAX_DEPTH: usize = 40;
const TEMPLATE_NS: i32 = 10;

/// Behavior switches (`__NOTOC__` &c) found on the page. Stripped from
/// the text; surfaced here so the parser/serve layers can act on them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BehaviorSwitches {
    pub no_toc: bool,
    pub force_toc: bool,
    pub toc: bool,
    pub no_editsection: bool,
    pub no_gallery: bool,
    pub index: bool,
    pub no_index: bool,
    pub hidden_cat: bool,
    pub disambig: bool,
    pub new_section_link: bool,
    pub no_new_section_link: bool,
}

pub struct Expanded {
    pub text: String,
    pub misses: RenderMisses,
    /// `{{DEFAULTSORT:key}}` — swallowed from output, surfaced here.
    pub default_sort: Option<String>,
    /// `{{DISPLAYTITLE:…}}` — swallowed from output, surfaced here.
    pub display_title: Option<String>,
    pub switches: BehaviorSwitches,
}

struct Ctx<'a> {
    store: &'a dyn PageStore,
    opts: &'a RenderOptions<'a>,
    site: &'a SiteConfig,
    render_title: &'a Title,
    ts: i64,
    misses: RenderMisses,
    default_sort: Option<String>,
    display_title: Option<String>,
    /// Prefixed titles currently on the transclusion stack (loop detect).
    stack: Vec<String>,
    depth: usize,
}

pub fn expand(
    store: &dyn PageStore,
    title: &Title,
    text: &str,
    opts: &RenderOptions<'_>,
) -> Expanded {
    let site = store.site();
    let mut ctx = Ctx {
        store,
        opts,
        site,
        render_title: title,
        ts: store.timestamp_micros(),
        misses: RenderMisses::default(),
        default_sort: None,
        display_title: None,
        stack: Vec::new(),
        depth: 0,
    };
    let mut switches = BehaviorSwitches::default();
    let stripped = strip_switches(text, &mut switches);
    let root = Frame {
        args: Default::default(),
        parent: None,
        title: title.prefixed(site),
    };
    let out = expand_body(&mut ctx, &stripped, &root, false);
    Expanded {
        text: out,
        misses: ctx.misses,
        default_sort: ctx.default_sort,
        display_title: ctx.display_title,
        switches,
    }
}

// ---------------------------------------------------------------------------
// Node tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Template(Vec<Part>),
    Arg(Vec<Part>),
    Link(Vec<Part>),
}

#[derive(Debug, Clone)]
struct Part {
    /// Present when a top-level `=` split this part into name/value.
    name: Option<Vec<Node>>,
    value: Vec<Node>,
}

#[derive(Clone, Copy, PartialEq)]
enum OpenKind {
    Root,
    Curly(usize),
    Bracket,
}

struct PartB {
    name: Option<Vec<Node>>,
    segs: Vec<Node>,
    buf: String,
}

impl PartB {
    fn new() -> Self {
        PartB { name: None, segs: Vec::new(), buf: String::new() }
    }
    fn flush(&mut self) {
        if !self.buf.is_empty() {
            self.segs.push(Node::Text(std::mem::take(&mut self.buf)));
        }
    }
    fn set_eq(&mut self) {
        self.flush();
        self.name = Some(std::mem::take(&mut self.segs));
    }
    fn finish(mut self) -> Part {
        self.flush();
        Part { name: self.name, value: self.segs }
    }
}

struct OpenB {
    kind: OpenKind,
    parts: Vec<PartB>,
}

impl OpenB {
    fn new(kind: OpenKind) -> Self {
        OpenB { kind, parts: vec![PartB::new()] }
    }
    fn cur(&mut self) -> &mut PartB {
        self.parts.last_mut().unwrap()
    }
}

/// Parse text into the preprocessor node tree. ASCII tokens only ever cut
/// at ASCII byte positions, so byte-index slicing stays UTF-8 safe.
fn parse_nodes(text: &str) -> Vec<Node> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut stack: Vec<OpenB> = vec![OpenB::new(OpenKind::Root)];
    let mut i = 0;
    while i < len {
        let b = bytes[i];
        match b {
            b'{' => {
                let n = run_len(bytes, i, b'{');
                stack.push(OpenB::new(OpenKind::Curly(n)));
                i += n;
            }
            b'}' => {
                let mut m = run_len(bytes, i, b'}');
                i += m;
                while m >= 2 {
                    let n = match stack.last().map(|o| o.kind) {
                        Some(OpenKind::Curly(n)) => n,
                        _ => break,
                    };
                    if n < 2 {
                        break;
                    }
                    let used = if n >= 3 && m >= 3 { 3 } else { 2 };
                    let open = stack.pop().unwrap();
                    let node = form_curly(open, used);
                    let extra_open = n - used;
                    if extra_open > 0 {
                        top_text(&mut stack, &"{".repeat(extra_open));
                    }
                    top_node(&mut stack, node);
                    m -= used;
                }
                if m > 0 {
                    top_text(&mut stack, &"}".repeat(m));
                }
            }
            b'[' if i + 1 < len && bytes[i + 1] == b'[' => {
                stack.push(OpenB::new(OpenKind::Bracket));
                i += 2;
            }
            b']' if i + 1 < len && bytes[i + 1] == b']' => {
                if matches!(stack.last().map(|o| o.kind), Some(OpenKind::Bracket)) {
                    let open = stack.pop().unwrap();
                    let parts: Vec<Part> = open.parts.into_iter().map(PartB::finish).collect();
                    top_node(&mut stack, Node::Link(parts));
                    i += 2;
                } else {
                    top_text(&mut stack, "]]");
                    i += 2;
                }
            }
            b'|' => {
                let top = stack.last_mut().unwrap();
                if top.kind == OpenKind::Root {
                    top.cur().buf.push('|');
                } else {
                    top.parts.push(PartB::new());
                }
                i += 1;
            }
            b'=' => {
                let top = stack.last_mut().unwrap();
                let is_curly = matches!(top.kind, OpenKind::Curly(_));
                if is_curly && top.cur().name.is_none() {
                    top.cur().set_eq();
                } else {
                    top.cur().buf.push('=');
                }
                i += 1;
            }
            _ => {
                let start = i;
                i += 1;
                while i < len && !is_token_byte(bytes[i]) {
                    i += 1;
                }
                stack.last_mut().unwrap().cur().buf.push_str(&text[start..i]);
            }
        }
    }
    // Unmatched opens: reconstruct their literal source into the parent,
    // preserving any complete inner nodes so those still expand.
    while stack.len() > 1 {
        let open = stack.pop().unwrap();
        flatten_unmatched(&mut stack, open);
    }
    let root = stack.pop().unwrap();
    root.parts.into_iter().next().unwrap().finish().value
}

fn is_token_byte(b: u8) -> bool {
    matches!(b, b'{' | b'}' | b'[' | b']' | b'|' | b'=')
}

fn run_len(bytes: &[u8], i: usize, c: u8) -> usize {
    let mut n = 0;
    while i + n < bytes.len() && bytes[i + n] == c {
        n += 1;
    }
    n
}

fn top_text(stack: &mut [OpenB], s: &str) {
    stack.last_mut().unwrap().cur().buf.push_str(s);
}

fn top_node(stack: &mut [OpenB], node: Node) {
    let cur = stack.last_mut().unwrap().cur();
    cur.flush();
    cur.segs.push(node);
}

fn form_curly(open: OpenB, used: usize) -> Node {
    let parts: Vec<Part> = open.parts.into_iter().map(PartB::finish).collect();
    if used >= 3 {
        Node::Arg(parts)
    } else {
        Node::Template(parts)
    }
}

fn flatten_unmatched(stack: &mut [OpenB], open: OpenB) {
    let opener = match open.kind {
        OpenKind::Curly(n) => "{".repeat(n),
        OpenKind::Bracket => "[[".to_string(),
        OpenKind::Root => String::new(),
    };
    top_text(stack, &opener);
    for (idx, part) in open.parts.into_iter().enumerate() {
        if idx > 0 {
            top_text(stack, "|");
        }
        let part = part.finish();
        if let Some(name) = part.name {
            for n in name {
                top_node(stack, n);
            }
            top_text(stack, "=");
        }
        for n in part.value {
            top_node(stack, n);
        }
    }
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

fn expand_body(ctx: &mut Ctx, text: &str, frame: &Frame, transcluding: bool) -> String {
    let t = apply_inclusion(text, transcluding);
    let t = strip_comments(&t);
    let (t, prot) = protect_tags(&t);
    let nodes = parse_nodes(&t);
    let mut out = expand_nodes(ctx, &nodes, frame);
    restore_tags(&mut out, &prot);
    out
}

fn expand_nodes(ctx: &mut Ctx, nodes: &[Node], frame: &Frame) -> String {
    let mut out = String::new();
    for n in nodes {
        match n {
            Node::Text(s) => out.push_str(s),
            Node::Template(parts) => out.push_str(&expand_template(ctx, parts, frame)),
            Node::Arg(parts) => out.push_str(&expand_arg(ctx, parts, frame)),
            Node::Link(parts) => out.push_str(&expand_link(ctx, parts, frame)),
        }
    }
    out
}

fn expand_link(ctx: &mut Ctx, parts: &[Part], frame: &Frame) -> String {
    let inner: Vec<String> = parts.iter().map(|p| expand_part_full(ctx, p, frame)).collect();
    format!("[[{}]]", inner.join("|"))
}

fn expand_arg(ctx: &mut Ctx, parts: &[Part], frame: &Frame) -> String {
    let key = expand_nodes(ctx, &parts[0].value, frame).trim().to_string();
    if let Some(v) = frame.args.get(&key) {
        return v.clone();
    }
    if parts.len() > 1 {
        return expand_nodes(ctx, &parts[1].value, frame);
    }
    format!("{{{{{{{key}}}}}}}")
}

fn expand_part_full(ctx: &mut Ctx, part: &Part, frame: &Frame) -> String {
    match &part.name {
        Some(name) => format!(
            "{}={}",
            expand_nodes(ctx, name, frame),
            expand_nodes(ctx, &part.value, frame)
        ),
        None => expand_nodes(ctx, &part.value, frame),
    }
}

fn head_nodes(part0: &Part) -> Vec<Node> {
    match &part0.name {
        Some(name) => {
            let mut v = name.clone();
            v.push(Node::Text("=".into()));
            v.extend(part0.value.iter().cloned());
            v
        }
        None => part0.value.clone(),
    }
}

/// (static name, colon-seen, arg0-nodes) or None when the name is dynamic
/// (a non-text node precedes any colon → treat the call as a template).
fn detect(head: &[Node]) -> Option<(String, bool, Vec<Node>)> {
    let mut name = String::new();
    let mut arg0: Vec<Node> = Vec::new();
    let mut colon = false;
    for node in head {
        if colon {
            arg0.push(node.clone());
            continue;
        }
        match node {
            Node::Text(s) => {
                if let Some(idx) = s.find(':') {
                    name.push_str(&s[..idx]);
                    colon = true;
                    let rest = &s[idx + 1..];
                    if !rest.is_empty() {
                        arg0.push(Node::Text(rest.to_string()));
                    }
                } else {
                    name.push_str(s);
                }
            }
            _ => return None,
        }
    }
    Some((name, colon, arg0))
}

fn expand_template(ctx: &mut Ctx, parts: &[Part], frame: &Frame) -> String {
    let head = head_nodes(&parts[0]);
    let mut detected = detect(&head);
    // Strip transparent prefixes (subst:/safesubst:/msg:/raw:) and re-detect.
    while let Some((name, true, arg0)) = &detected {
        let key = name.trim().to_ascii_lowercase();
        if matches!(key.as_str(), "subst" | "safesubst" | "msg" | "raw" | "msgnw") {
            let a = arg0.clone();
            detected = detect(&a);
        } else {
            break;
        }
    }

    if let Some((name, colon, arg0)) = &detected {
        let key = name.trim();
        if *colon {
            if let Some(r) = dispatch_colon(ctx, key, arg0, &parts[1..], frame) {
                return r;
            }
        }
        let upper = key.to_ascii_uppercase();
        let arg_str = if *colon {
            Some(expand_nodes(ctx, arg0, frame).trim().to_string())
        } else {
            None
        };
        if let Some(r) = magic_word(ctx, &upper, arg_str.as_deref()) {
            return r;
        }
    }

    // Template transclusion. Reconstruct the title from the (possibly
    // prefix-stripped) detection so `{{subst:G}}` transcludes G, not subst:G.
    let title_str = match &detected {
        Some((name, true, arg0)) => format!("{}:{}", name, expand_nodes(ctx, arg0, frame)),
        Some((name, false, _)) => name.clone(),
        None => expand_nodes(ctx, &head, frame),
    };
    transclude(ctx, &title_str, &parts[1..], frame)
}

/// DISPLAYTITLE/DEFAULTSORT (swallowed) and magic variables. Returns None
/// so a genuinely unknown name falls through to template transclusion.
fn magic_word(ctx: &mut Ctx, upper: &str, arg: Option<&str>) -> Option<String> {
    match upper {
        "DISPLAYTITLE" => {
            if let Some(a) = arg {
                ctx.display_title = Some(a.to_string());
            }
            return Some(String::new());
        }
        "DEFAULTSORT" | "DEFAULTSORTKEY" | "DEFAULTCATEGORYSORT" => {
            if let Some(a) = arg {
                ctx.default_sort = Some(a.to_string());
            }
            return Some(String::new());
        }
        _ => {}
    }
    let subject = match arg {
        Some(a) => resolve_title(a, 0, ctx.site),
        None => ctx.render_title.clone(),
    };
    magic::magic_variable(upper, &subject, ctx.site, ctx.ts, arg.is_some())
}

fn transclude(ctx: &mut Ctx, title_str: &str, arg_parts: &[Part], frame: &Frame) -> String {
    let title = resolve_title(title_str, TEMPLATE_NS, ctx.site);
    let prefixed = title.prefixed(ctx.site);
    if ctx.stack.iter().any(|t| t == &prefixed) {
        return html::error_box(&format!("Template loop detected: [[{prefixed}]]"));
    }
    if ctx.depth >= MAX_DEPTH {
        return html::error_box(&format!("Template recursion depth limit exceeded ([[{prefixed}]])"));
    }
    let body = match ctx.store.page_text(&title) {
        Some(b) => b,
        None => {
            ctx.misses.missing_templates.push(prefixed.clone());
            return red_link(&title, ctx.site);
        }
    };
    let child = build_frame(ctx, &prefixed, arg_parts, frame);
    ctx.stack.push(prefixed);
    ctx.depth += 1;
    let out = expand_body(ctx, &body, &child, true);
    ctx.depth -= 1;
    ctx.stack.pop();
    out
}

fn red_link(title: &Title, site: &SiteConfig) -> String {
    let p = title.prefixed(site);
    if title.ns == 0 {
        format!("[[:{p}]]")
    } else {
        format!("[[{p}]]")
    }
}

fn build_frame(ctx: &mut Ctx, title: &str, arg_parts: &[Part], caller: &Frame) -> Frame {
    let mut args = std::collections::BTreeMap::new();
    let mut pos = 0u32;
    for part in arg_parts {
        match &part.name {
            Some(name_nodes) => {
                let name = expand_nodes(ctx, name_nodes, caller).trim().to_string();
                let val = expand_nodes(ctx, &part.value, caller).trim().to_string();
                args.insert(name, val);
            }
            None => {
                pos += 1;
                // Positional values keep surrounding whitespace (MediaWiki).
                let val = expand_nodes(ctx, &part.value, caller);
                args.insert(pos.to_string(), val);
            }
        }
    }
    Frame {
        args,
        parent: Some(Box::new(caller.clone())),
        title: title.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Parser functions / colon transforms
// ---------------------------------------------------------------------------

fn dispatch_colon(
    ctx: &mut Ctx,
    name: &str,
    arg0: &[Node],
    rest: &[Part],
    frame: &Frame,
) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let s = match lower.as_str() {
        "#if" => pf_if(ctx, arg0, rest, frame),
        "#ifeq" => pf_ifeq(ctx, arg0, rest, frame),
        "#iferror" => pf_iferror(ctx, arg0, rest, frame),
        "#ifexpr" => pf_ifexpr(ctx, arg0, rest, frame),
        "#ifexist" => pf_ifexist(ctx, arg0, rest, frame),
        "#switch" => pf_switch(ctx, arg0, rest, frame),
        "#expr" => pf_expr(ctx, arg0, frame),
        "#time" | "#timel" => pf_time(ctx, arg0, rest, frame),
        "#titleparts" => pf_titleparts(ctx, arg0, rest, frame),
        "#tag" => pf_tag(ctx, arg0, rest, frame),
        "#invoke" => pf_invoke(ctx, arg0, rest, frame),
        "lc" => trim_arg(ctx, arg0, frame).to_lowercase(),
        "uc" => trim_arg(ctx, arg0, frame).to_uppercase(),
        "lcfirst" => first_case(&trim_arg(ctx, arg0, frame), false),
        "ucfirst" => first_case(&trim_arg(ctx, arg0, frame), true),
        "padleft" => pf_pad(ctx, arg0, rest, frame, true),
        "padright" => pf_pad(ctx, arg0, rest, frame, false),
        "urlencode" => pf_urlencode(ctx, arg0, rest, frame),
        "anchorencode" => anchor_encode(&expand_nodes(ctx, arg0, frame)),
        "ns" => pf_ns(ctx, arg0, frame, false),
        "nse" => pf_ns(ctx, arg0, frame, true),
        "plural" => pf_plural(ctx, arg0, rest, frame),
        "formatnum" => pf_formatnum(ctx, arg0, rest, frame),
        "int" => pf_int(ctx, arg0, frame),
        _ => return None,
    };
    Some(s)
}

fn trim_arg(ctx: &mut Ctx, nodes: &[Node], frame: &Frame) -> String {
    expand_nodes(ctx, nodes, frame).trim().to_string()
}

fn part_trim(ctx: &mut Ctx, part: &Part, frame: &Frame) -> String {
    expand_part_full(ctx, part, frame).trim().to_string()
}

/// Expand only a part's VALUE (dropping any `name=` prefix) — what #switch
/// returns for a matched case, whose label lives in the name.
fn part_value_trim(ctx: &mut Ctx, part: &Part, frame: &Frame) -> String {
    expand_nodes(ctx, &part.value, frame).trim().to_string()
}

fn pf_if(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let cond = trim_arg(ctx, arg0, frame);
    if !cond.is_empty() {
        rest.first().map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
    } else {
        rest.get(1).map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
    }
}

fn pf_ifeq(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let a = trim_arg(ctx, arg0, frame);
    let b = rest.first().map(|p| part_trim(ctx, p, frame)).unwrap_or_default();
    let eq = match (parse_num(&a), parse_num(&b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    };
    if eq {
        rest.get(1).map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
    } else {
        rest.get(2).map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
    }
}

fn pf_iferror(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let test = expand_nodes(ctx, arg0, frame);
    let is_err = test.contains("class=\"error\"");
    if is_err {
        rest.first().map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
    } else if rest.len() > 1 {
        part_trim(ctx, &rest[1], frame)
    } else {
        test
    }
}

fn pf_ifexpr(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let expr = trim_arg(ctx, arg0, frame);
    match eval_expr(&expr) {
        Ok(v) => {
            if v != 0.0 {
                rest.first().map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
            } else {
                rest.get(1).map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
            }
        }
        Err(e) => html::error_box(&e),
    }
}

fn pf_ifexist(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let raw = trim_arg(ctx, arg0, frame);
    let title = resolve_title(&raw, 0, ctx.site);
    let exists = ctx.store.page_exists(&title);
    if exists {
        rest.first().map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
    } else {
        rest.get(1).map(|p| part_trim(ctx, p, frame)).unwrap_or_default()
    }
}

fn pf_expr(ctx: &mut Ctx, arg0: &[Node], frame: &Frame) -> String {
    let expr = trim_arg(ctx, arg0, frame);
    match eval_expr(&expr) {
        Ok(v) => format_number(v),
        Err(e) => html::error_box(&e),
    }
}

fn pf_switch(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let value = trim_arg(ctx, arg0, frame);
    let value_num = parse_num(&value);
    let mut matched = false;
    let mut default_val: Option<String> = None;
    let n = rest.len();
    for (idx, part) in rest.iter().enumerate() {
        let is_last = idx == n - 1;
        match &part.name {
            Some(name_nodes) => {
                let case = expand_nodes(ctx, name_nodes, frame).trim().to_string();
                if case == "#default" {
                    default_val = Some(part_value_trim(ctx, part, frame));
                    continue;
                }
                if !matched && case_matches(&value, value_num, &case) {
                    matched = true;
                }
                if matched {
                    return part_value_trim(ctx, part, frame);
                }
            }
            None => {
                let case = part_value_trim(ctx, part, frame);
                if is_last {
                    // Trailing value with no `=` is the default.
                    return case;
                }
                if !matched && case_matches(&value, value_num, &case) {
                    matched = true;
                }
            }
        }
    }
    default_val.unwrap_or_default()
}

fn case_matches(value: &str, value_num: Option<f64>, case: &str) -> bool {
    match (value_num, parse_num(case)) {
        (Some(a), Some(b)) => a == b,
        _ => value == case,
    }
}

fn pf_titleparts(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let title = trim_arg(ctx, arg0, frame);
    let numparts = rest
        .first()
        .and_then(|p| part_trim(ctx, p, frame).parse::<i32>().ok())
        .unwrap_or(0);
    let offset = rest
        .get(1)
        .and_then(|p| part_trim(ctx, p, frame).parse::<i32>().ok())
        .unwrap_or(0);
    let parts: Vec<&str> = title.split('/').collect();
    let n = parts.len() as i32;
    let mut start = if offset > 0 {
        offset - 1
    } else if offset < 0 {
        n + offset
    } else {
        0
    };
    start = start.clamp(0, n);
    let mut end = if numparts > 0 {
        start + numparts
    } else if numparts < 0 {
        n + numparts
    } else {
        n
    };
    end = end.clamp(start, n);
    parts[start as usize..end as usize].join("/")
}

fn pf_tag(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let tag = trim_arg(ctx, arg0, frame);
    let mut content: Option<String> = None;
    let mut attrs = String::new();
    for part in rest {
        match &part.name {
            Some(name_nodes) => {
                let k = expand_nodes(ctx, name_nodes, frame).trim().to_string();
                let v = expand_nodes(ctx, &part.value, frame).trim().to_string();
                attrs.push_str(&format!(" {k}=\"{v}\""));
            }
            None if content.is_none() => {
                content = Some(expand_nodes(ctx, &part.value, frame));
            }
            None => {}
        }
    }
    match content {
        Some(c) => format!("<{tag}{attrs}>{c}</{tag}>"),
        None => format!("<{tag}{attrs}></{tag}>"),
    }
}

fn pf_invoke(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let module = trim_arg(ctx, arg0, frame);
    let function = rest.first().map(|p| part_trim(ctx, p, frame)).unwrap_or_default();
    if ctx.opts.invoker.is_none() {
        ctx.misses.failed_invokes.push(format!("Module:{module}#{function}"));
        return html::error_box(&format!("Script error: no Scribunto engine (Module:{module})"));
    }
    // Frame the module sees: the #invoke args after the function name;
    // parent = the calling template frame.
    let mut args = std::collections::BTreeMap::new();
    let mut pos = 0u32;
    let invoke_args = if rest.is_empty() { &rest[..] } else { &rest[1..] };
    for part in invoke_args {
        match &part.name {
            Some(name_nodes) => {
                let k = expand_nodes(ctx, name_nodes, frame).trim().to_string();
                let v = expand_nodes(ctx, &part.value, frame).trim().to_string();
                args.insert(k, v);
            }
            None => {
                pos += 1;
                args.insert(pos.to_string(), expand_nodes(ctx, &part.value, frame));
            }
        }
    }
    let inv_frame = Frame {
        args,
        parent: Some(Box::new(frame.clone())),
        title: format!("Module:{module}"),
    };
    let invoker = ctx.opts.invoker.unwrap();
    match invoker.invoke(&module, &function, &inv_frame, ctx.store) {
        Ok(s) => s,
        Err(e) => {
            ctx.misses.failed_invokes.push(format!("Module:{module}#{function}"));
            html::error_box(&format!("Script error: {e}"))
        }
    }
}

fn pf_pad(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame, left: bool) -> String {
    let s = trim_arg(ctx, arg0, frame);
    let width = rest
        .first()
        .and_then(|p| part_trim(ctx, p, frame).parse::<usize>().ok())
        .unwrap_or(0);
    let padchar = rest
        .get(1)
        .map(|p| part_trim(ctx, p, frame))
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "0".to_string());
    let cur: Vec<char> = s.chars().collect();
    if cur.len() >= width {
        return s;
    }
    let padchars: Vec<char> = padchar.chars().collect();
    let need = width - cur.len();
    let mut fill = String::new();
    for i in 0..need {
        fill.push(padchars[i % padchars.len()]);
    }
    if left {
        format!("{fill}{s}")
    } else {
        format!("{s}{fill}")
    }
}

fn pf_urlencode(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let s = expand_nodes(ctx, arg0, frame);
    let kind = rest.first().map(|p| part_trim(ctx, p, frame)).unwrap_or_default();
    url_encode(s.trim(), &kind)
}

fn pf_ns(ctx: &mut Ctx, arg0: &[Node], frame: &Frame, encode: bool) -> String {
    let a = trim_arg(ctx, arg0, frame);
    let name = ns_name_lookup(ctx.site, &a).unwrap_or_default();
    if encode {
        magic::encode_title(&name)
    } else {
        name
    }
}

fn pf_plural(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let count = trim_arg(ctx, arg0, frame);
    let n = parse_num(&count).unwrap_or(0.0);
    let forms: Vec<String> = rest.iter().map(|p| part_trim(ctx, p, frame)).collect();
    if forms.is_empty() {
        return String::new();
    }
    // Explicit "k=form" overrides win first.
    for part in rest {
        if let Some(name_nodes) = &part.name {
            let k = expand_nodes(ctx, name_nodes, frame).trim().to_string();
            if let Ok(kn) = k.parse::<i64>() {
                if (kn as f64 - n).abs() < f64::EPSILON {
                    return expand_nodes(ctx, &part.value, frame).trim().to_string();
                }
            }
        }
    }
    // English default rule; hook point for CLDR plural rules per language.
    let idx = if (n - 1.0).abs() < f64::EPSILON { 0 } else { forms.len() - 1 };
    forms[idx].clone()
}

fn pf_formatnum(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let s = trim_arg(ctx, arg0, frame);
    let reverse = rest
        .first()
        .map(|p| part_trim(ctx, p, frame).eq_ignore_ascii_case("R"))
        .unwrap_or(false);
    if reverse {
        return s.replace(',', "");
    }
    match parse_num(&s) {
        Some(_) => group_thousands(&s),
        None => s,
    }
}

fn pf_int(ctx: &mut Ctx, arg0: &[Node], frame: &Frame) -> String {
    let key = trim_arg(ctx, arg0, frame);
    // MediaWiki: message from the MediaWiki: namespace at τ; else ⧼key⧽.
    let title = resolve_title(&key, 8, ctx.site);
    match ctx.store.page_text(&title) {
        Some(t) => t,
        None => format!("⧼{key}⧽"),
    }
}

fn pf_time(ctx: &mut Ctx, arg0: &[Node], rest: &[Part], frame: &Frame) -> String {
    let format = expand_nodes(ctx, arg0, frame);
    let format = format.trim();
    let datearg = rest.first().map(|p| part_trim(ctx, p, frame)).unwrap_or_default();
    let civ = if datearg.is_empty() || datearg.eq_ignore_ascii_case("now") {
        magic::civil_from_micros(ctx.ts)
    } else {
        match parse_datetime(&datearg) {
            Some(c) => c,
            None => return html::error_box("Error: Invalid time."),
        }
    };
    format_time(format, &civ)
}

// ---------------------------------------------------------------------------
// #time formatting + datetime parse
// ---------------------------------------------------------------------------

fn format_time(fmt: &str, c: &magic::Civil) -> String {
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\\' => {
                if i + 1 < chars.len() {
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            'Y' => out.push_str(&format!("{:04}", c.year)),
            'y' => out.push_str(&format!("{:02}", (c.year % 100 + 100) % 100)),
            'n' => out.push_str(&c.month.to_string()),
            'm' => out.push_str(&format!("{:02}", c.month)),
            'M' => out.push_str(&magic::month_name(c.month)[..3]),
            'F' => out.push_str(magic::month_name(c.month)),
            'j' => out.push_str(&c.day.to_string()),
            'd' => out.push_str(&format!("{:02}", c.day)),
            'l' => out.push_str(magic::weekday_name(c.dow)),
            'D' => out.push_str(&magic::weekday_name(c.dow)[..3]),
            'N' => out.push_str(&(if c.dow == 0 { 7 } else { c.dow }).to_string()),
            'w' => out.push_str(&c.dow.to_string()),
            'a' => out.push_str(if c.hour < 12 { "am" } else { "pm" }),
            'A' => out.push_str(if c.hour < 12 { "AM" } else { "PM" }),
            'g' => out.push_str(&h12(c.hour).to_string()),
            'h' => out.push_str(&format!("{:02}", h12(c.hour))),
            'G' => out.push_str(&c.hour.to_string()),
            'H' => out.push_str(&format!("{:02}", c.hour)),
            'i' => out.push_str(&format!("{:02}", c.min)),
            's' => out.push_str(&format!("{:02}", c.sec)),
            'U' => out.push_str(&c.unix.to_string()),
            'W' => out.push_str(&format!("{:02}", magic::iso_week(c))),
            'L' => out.push_str(if is_leap(c.year) { "1" } else { "0" }),
            't' => out.push_str(&days_in_month(c.year, c.month).to_string()),
            _ => out.push(ch),
        }
        i += 1;
    }
    out
}

fn h12(h: u32) -> u32 {
    let m = h % 12;
    if m == 0 {
        12
    } else {
        m
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Parse the common ISO-ish datetime forms {{#time}} sees in dumps.
fn parse_datetime(s: &str) -> Option<magic::Civil> {
    let s = s.trim();
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if s.chars().all(|c| c.is_ascii_digit()) && s.len() == 14 {
        let g = |a: usize, b: usize| digits[a..b].parse::<i64>().ok();
        return Some(magic::civil_from_parts(
            g(0, 4)?,
            g(4, 6)? as u32,
            g(6, 8)? as u32,
            g(8, 10)? as u32,
            g(10, 12)? as u32,
            g(12, 14)? as u32,
        ));
    }
    let (date, time) = match s.find(['T', ' ']) {
        Some(p) => (&s[..p], s[p + 1..].trim_end_matches('Z').trim()),
        None => (s, ""),
    };
    let dp: Vec<&str> = date.split(['-', '/', '.']).collect();
    if dp.len() != 3 {
        return None;
    }
    let year = dp[0].parse::<i64>().ok()?;
    let month = dp[1].parse::<u32>().ok()?;
    let day = dp[2].parse::<u32>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut h, mut mi, mut se) = (0u32, 0u32, 0u32);
    if !time.is_empty() {
        let tp: Vec<&str> = time.split(':').collect();
        h = tp.first().and_then(|v| v.parse().ok())?;
        mi = tp.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
        se = tp.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
    }
    Some(magic::civil_from_parts(year, month, day, h, mi, se))
}

// ---------------------------------------------------------------------------
// #expr evaluator
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Mod,
    Fmod,
    IDiv,
    Round,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Not,
    Sin,
    Cos,
    Tan,
    Ln,
    Exp,
    Abs,
    Floor,
    Ceil,
    Trunc,
}

#[derive(Clone, Copy, Debug)]
enum Tk {
    Num(f64),
    Op(Op),
    LP,
    RP,
}

fn eval_expr(input: &str) -> Result<f64, String> {
    let toks = tokenize_expr(input)?;
    if toks.is_empty() {
        return Ok(0.0);
    }
    let mut p = ExprParser { toks, pos: 0 };
    let v = p.parse(0)?;
    if p.pos != p.toks.len() {
        return Err("Expression error: unexpected token".into());
    }
    Ok(v)
}

fn tokenize_expr(s: &str) -> Result<Vec<Tk>, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'0'..=b'9' | b'.' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    let mut j = i + 1;
                    if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                        j += 1;
                    }
                    if j < b.len() && b[j].is_ascii_digit() {
                        i = j;
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                let num: f64 = s[start..i]
                    .parse()
                    .map_err(|_| "Expression error: malformed number".to_string())?;
                out.push(Tk::Num(num));
            }
            b'+' => {
                out.push(Tk::Op(Op::Add));
                i += 1;
            }
            b'-' => {
                out.push(Tk::Op(Op::Sub));
                i += 1;
            }
            b'*' => {
                if i + 1 < b.len() && b[i + 1] == b'*' {
                    out.push(Tk::Op(Op::Pow));
                    i += 2;
                } else {
                    out.push(Tk::Op(Op::Mul));
                    i += 1;
                }
            }
            b'/' => {
                out.push(Tk::Op(Op::Div));
                i += 1;
            }
            b'^' => {
                out.push(Tk::Op(Op::Pow));
                i += 1;
            }
            b'(' => {
                out.push(Tk::LP);
                i += 1;
            }
            b')' => {
                out.push(Tk::RP);
                i += 1;
            }
            b'=' => {
                out.push(Tk::Op(Op::Eq));
                i += 1;
                if i < b.len() && b[i] == b'=' {
                    i += 1;
                }
            }
            b'<' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tk::Op(Op::Le));
                    i += 2;
                } else if i + 1 < b.len() && b[i + 1] == b'>' {
                    out.push(Tk::Op(Op::Ne));
                    i += 2;
                } else {
                    out.push(Tk::Op(Op::Lt));
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tk::Op(Op::Ge));
                    i += 2;
                } else {
                    out.push(Tk::Op(Op::Gt));
                    i += 1;
                }
            }
            b'!' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tk::Op(Op::Ne));
                    i += 2;
                } else {
                    return Err("Expression error: unexpected '!'".into());
                }
            }
            _ if c.is_ascii_alphabetic() => {
                let start = i;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let w = s[start..i].to_ascii_lowercase();
                match w.as_str() {
                    "mod" => out.push(Tk::Op(Op::Mod)),
                    "fmod" => out.push(Tk::Op(Op::Fmod)),
                    "div" => out.push(Tk::Op(Op::IDiv)),
                    "round" => out.push(Tk::Op(Op::Round)),
                    "and" => out.push(Tk::Op(Op::And)),
                    "or" => out.push(Tk::Op(Op::Or)),
                    "not" => out.push(Tk::Op(Op::Not)),
                    "sin" => out.push(Tk::Op(Op::Sin)),
                    "cos" => out.push(Tk::Op(Op::Cos)),
                    "tan" => out.push(Tk::Op(Op::Tan)),
                    "ln" => out.push(Tk::Op(Op::Ln)),
                    "exp" => out.push(Tk::Op(Op::Exp)),
                    "abs" => out.push(Tk::Op(Op::Abs)),
                    "floor" => out.push(Tk::Op(Op::Floor)),
                    "ceil" => out.push(Tk::Op(Op::Ceil)),
                    "trunc" | "int" => out.push(Tk::Op(Op::Trunc)),
                    "pi" => out.push(Tk::Num(std::f64::consts::PI)),
                    "e" => out.push(Tk::Num(std::f64::consts::E)),
                    _ => return Err(format!("Expression error: unrecognised word '{w}'")),
                }
            }
            _ => return Err(format!("Expression error: unexpected character '{}'", c as char)),
        }
    }
    Ok(out)
}

struct ExprParser {
    toks: Vec<Tk>,
    pos: usize,
}

impl ExprParser {
    fn peek(&self) -> Option<Tk> {
        self.toks.get(self.pos).copied()
    }

    fn parse(&mut self, min_prec: u8) -> Result<f64, String> {
        let mut left = self.parse_unary()?;
        while let Some(Tk::Op(op)) = self.peek() {
            let prec = match binary_prec(op) {
                Some(p) => p,
                None => break,
            };
            if prec < min_prec {
                break;
            }
            self.pos += 1;
            let next_min = if op == Op::Pow { prec } else { prec + 1 };
            let right = self.parse(next_min)?;
            left = apply_binary(op, left, right)?;
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Tk::Op(op)) if is_prefix(op) => {
                self.pos += 1;
                let v = self.parse_unary()?;
                apply_prefix(op, v)
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Tk::Num(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(Tk::LP) => {
                self.pos += 1;
                let v = self.parse(0)?;
                match self.peek() {
                    Some(Tk::RP) => {
                        self.pos += 1;
                        Ok(v)
                    }
                    _ => Err("Expression error: missing ')'".into()),
                }
            }
            _ => Err("Expression error: expected operand".into()),
        }
    }
}

fn is_prefix(op: Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Not
            | Op::Sin
            | Op::Cos
            | Op::Tan
            | Op::Ln
            | Op::Exp
            | Op::Abs
            | Op::Floor
            | Op::Ceil
            | Op::Trunc
    )
}

fn binary_prec(op: Op) -> Option<u8> {
    Some(match op {
        Op::Or => 2,
        Op::And => 3,
        Op::Eq | Op::Ne | Op::Lt | Op::Gt | Op::Le | Op::Ge => 4,
        Op::Round => 5,
        Op::Add | Op::Sub => 6,
        Op::Mul | Op::Div | Op::Mod | Op::Fmod | Op::IDiv => 7,
        Op::Pow => 8,
        _ => return None,
    })
}

fn apply_prefix(op: Op, v: f64) -> Result<f64, String> {
    Ok(match op {
        Op::Add => v,
        Op::Sub => -v,
        Op::Not => {
            if v != 0.0 {
                0.0
            } else {
                1.0
            }
        }
        Op::Sin => v.sin(),
        Op::Cos => v.cos(),
        Op::Tan => v.tan(),
        Op::Ln => v.ln(),
        Op::Exp => v.exp(),
        Op::Abs => v.abs(),
        Op::Floor => v.floor(),
        Op::Ceil => v.ceil(),
        Op::Trunc => v.trunc(),
        _ => return Err("Expression error: bad unary".into()),
    })
}

fn apply_binary(op: Op, a: f64, b: f64) -> Result<f64, String> {
    Ok(match op {
        Op::Add => a + b,
        Op::Sub => a - b,
        Op::Mul => a * b,
        Op::Div | Op::IDiv => {
            if b == 0.0 {
                return Err("Division by zero".into());
            }
            a / b
        }
        Op::Pow => a.powf(b),
        Op::Mod => {
            let bi = b as i64;
            if bi == 0 {
                return Err("Division by zero".into());
            }
            ((a as i64) % bi) as f64
        }
        Op::Fmod => {
            if b == 0.0 {
                return Err("Division by zero".into());
            }
            a % b
        }
        Op::Round => round_to(a, b as i32),
        Op::Eq => bool_f(a == b),
        Op::Ne => bool_f(a != b),
        Op::Lt => bool_f(a < b),
        Op::Gt => bool_f(a > b),
        Op::Le => bool_f(a <= b),
        Op::Ge => bool_f(a >= b),
        Op::And => bool_f(a != 0.0 && b != 0.0),
        Op::Or => bool_f(a != 0.0 || b != 0.0),
        _ => return Err("Expression error: bad operator".into()),
    })
}

fn bool_f(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

fn round_to(a: f64, decimals: i32) -> f64 {
    let factor = 10f64.powi(decimals);
    let scaled = a * factor;
    // PHP round: half away from zero.
    let r = if scaled >= 0.0 {
        (scaled + 0.5).floor()
    } else {
        (scaled - 0.5).ceil()
    };
    r / factor
}

/// MediaWiki numeric formatting: 14 significant digits, trailing zeros
/// trimmed, integer results without a decimal point.
fn format_number(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-INF".into() } else { "INF".into() };
    }
    if x == 0.0 {
        return "0".into();
    }
    if x.fract() == 0.0 && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    let e = x.abs().log10().floor() as i32;
    let decimals = (13 - e).max(0) as usize;
    let mut s = format!("{x:.decimals$}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    match t.parse::<f64>() {
        Ok(v) if v.is_finite() => Some(v),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// String transforms
// ---------------------------------------------------------------------------

fn first_case(s: &str, upper: bool) -> String {
    let mut ch = s.chars();
    match ch.next() {
        Some(c) => {
            let f: String = if upper {
                c.to_uppercase().collect()
            } else {
                c.to_lowercase().collect()
            };
            format!("{f}{}", ch.as_str())
        }
        None => String::new(),
    }
}

fn url_encode(s: &str, kind: &str) -> String {
    let is_path = kind.eq_ignore_ascii_case("path");
    let space = if kind.eq_ignore_ascii_case("wiki") {
        '_'
    } else {
        '+' // QUERY default
    };
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            b' ' => {
                if is_path {
                    out.push_str("%20");
                } else {
                    out.push(space);
                }
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn anchor_encode(s: &str) -> String {
    // MediaWiki anchorencode: spaces → underscores, a small set of markup
    // characters percent-encoded. Common-case fidelity, not exhaustive.
    let s = s.trim();
    let mut out = String::new();
    for c in s.chars() {
        match c {
            ' ' => out.push('_'),
            '[' | ']' | '{' | '}' | '|' | '#' | '<' | '>' => {
                out.push_str(&format!("%{:02X}", c as u32))
            }
            _ => out.push(c),
        }
    }
    out
}

fn group_thousands(s: &str) -> String {
    let neg = s.starts_with('-');
    let body = s.trim_start_matches('-');
    let (int, frac) = match body.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (body, None),
    };
    let digits: Vec<char> = int.chars().collect();
    let mut grouped = String::new();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*c);
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(&grouped);
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    out
}

// ---------------------------------------------------------------------------
// Title / namespace resolution (local — the frozen Title::parse is a stub)
// ---------------------------------------------------------------------------

fn resolve_title(raw: &str, default_ns: i32, site: &SiteConfig) -> Title {
    let name = raw.trim();
    let (name, forced_main) = match name.strip_prefix(':') {
        Some(r) => (r.trim(), true),
        None => (name, false),
    };
    let dns = if forced_main { 0 } else { default_ns };
    let (ns, rest) = match name.split_once(':') {
        Some((prefix, rest)) => match ns_id_lookup(site, prefix.trim()) {
            Some(id) => (id, rest.trim()),
            None => (dns, name),
        },
        None => (dns, name),
    };
    Title {
        ns,
        text: normalize_text(rest, ns, site),
    }
}

fn normalize_text(s: &str, ns: i32, site: &SiteConfig) -> String {
    let collapsed = s.replace('_', " ");
    let collapsed = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    let upper_first = site
        .namespaces
        .get(&ns)
        .map(|n| n.case_first_letter)
        .unwrap_or(true);
    if upper_first {
        first_case(&collapsed, true)
    } else {
        collapsed
    }
}

fn ns_id_lookup(site: &SiteConfig, prefix: &str) -> Option<i32> {
    let want = normalize_ns_key(prefix);
    if want.is_empty() {
        return None;
    }
    for (id, info) in &site.namespaces {
        if normalize_ns_key(&info.canonical) == want {
            return Some(*id);
        }
        for alias in &info.aliases {
            if normalize_ns_key(alias) == want {
                return Some(*id);
            }
        }
    }
    None
}

fn ns_name_lookup(site: &SiteConfig, arg: &str) -> Option<String> {
    let a = arg.trim();
    if a.is_empty() || a == "0" {
        return Some(String::new());
    }
    let id = match a.parse::<i32>() {
        Ok(n) => n,
        Err(_) => ns_id_lookup(site, a)?,
    };
    if id == 0 {
        return Some(String::new());
    }
    site.namespaces.get(&id).map(ns_display_name)
}

/// Localized display name: `aliases[0]` when present, else the canonical.
fn ns_display_name(n: &crate::NamespaceInfo) -> String {
    n.aliases.first().cloned().unwrap_or_else(|| n.canonical.clone())
}

fn normalize_ns_key(s: &str) -> String {
    s.replace('_', " ").trim().to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Inclusion transforms, comments, protected tags, behavior switches
// ---------------------------------------------------------------------------

fn apply_inclusion(text: &str, transcluding: bool) -> String {
    if transcluding {
        if contains_ci(text, "<onlyinclude>") {
            return collect_regions(text, "<onlyinclude>", "</onlyinclude>");
        }
        let t = remove_region(text, "<noinclude>", "</noinclude>");
        remove_tags(&t, &["<includeonly>", "</includeonly>"])
    } else {
        let t = remove_region(text, "<includeonly>", "</includeonly>");
        remove_tags(&t, &["<noinclude>", "</noinclude>", "<onlyinclude>", "</onlyinclude>"])
    }
}

/// Case-insensitive substring search returning a byte index. ASCII-only
/// lowercasing keeps byte offsets aligned with the original.
fn find_ci_from(hay: &str, needle: &str, start: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || start + n.len() > h.len() {
        return None;
    }
    'outer: for i in start..=h.len() - n.len() {
        for j in 0..n.len() {
            if !h[i + j].eq_ignore_ascii_case(&n[j]) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    find_ci_from(hay, needle, 0).is_some()
}

fn remove_region(text: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while let Some(o) = find_ci_from(text, open, i) {
        out.push_str(&text[i..o]);
        match find_ci_from(text, close, o + open.len()) {
            Some(c) => i = c + close.len(),
            None => {
                i = text.len();
                break;
            }
        }
    }
    out.push_str(&text[i..]);
    out
}

fn remove_tags(text: &str, tags: &[&str]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < text.len() {
        // If a tag starts exactly here, drop it.
        let here = tags
            .iter()
            .find(|t| find_ci_from(text, t, i) == Some(i));
        if let Some(t) = here {
            i += t.len();
            continue;
        }
        let next = tags.iter().filter_map(|t| find_ci_from(text, t, i)).min();
        match next {
            Some(n) => {
                out.push_str(&text[i..n]);
                i = n;
            }
            None => {
                out.push_str(&text[i..]);
                break;
            }
        }
    }
    out
}

fn collect_regions(text: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while let Some(o) = find_ci_from(text, open, i) {
        let cs = o + open.len();
        match find_ci_from(text, close, cs) {
            Some(c) => {
                out.push_str(&text[cs..c]);
                i = c + close.len();
            }
            None => {
                out.push_str(&text[cs..]);
                break;
            }
        }
    }
    out
}

/// Strip HTML comments with MediaWiki's newline-eating rule: a comment
/// that (with surrounding spaces/tabs) fills a line takes one newline too.
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    loop {
        match text[i..].find("<!--") {
            None => {
                out.push_str(&text[i..]);
                break;
            }
            Some(rel) => {
                let cstart = i + rel;
                let cend = match text[cstart + 4..].find("-->") {
                    Some(e) => cstart + 4 + e + 3,
                    None => len,
                };
                let mut ws_start = cstart;
                while ws_start > 0 && matches!(bytes[ws_start - 1], b' ' | b'\t') {
                    ws_start -= 1;
                }
                let mut ws_end = cend;
                while ws_end < len && matches!(bytes[ws_end], b' ' | b'\t') {
                    ws_end += 1;
                }
                let lead = ws_start == 0 || bytes[ws_start - 1] == b'\n';
                let trail = ws_end == len || bytes[ws_end] == b'\n';
                if lead && trail {
                    out.push_str(&text[i..ws_start]);
                    let mut resume = ws_end;
                    if resume < len && bytes[resume] == b'\n' {
                        resume += 1;
                    }
                    i = resume;
                } else {
                    out.push_str(&text[i..cstart]);
                    i = cend;
                }
                if i >= len {
                    break;
                }
            }
        }
    }
    out
}

/// Protect <nowiki>/<pre> contents from preprocessing. Restored verbatim
/// after expansion (the parser handles their final rendering).
fn protect_tags(text: &str) -> (String, Vec<String>) {
    let mut store = Vec::new();
    let mut out = String::new();
    let mut i = 0;
    let tags = ["nowiki", "pre"];
    'scan: while i < text.len() {
        for tag in tags {
            let open = format!("<{tag}");
            if find_ci_from(text, &open, i) == Some(i) {
                let after = &text[i + open.len()..];
                let ok = after
                    .as_bytes()
                    .first()
                    .map(|b| matches!(b, b'>' | b' ' | b'/' | b'\t' | b'\n'))
                    .unwrap_or(false);
                if ok {
                    let close = format!("</{tag}>");
                    let end = match find_ci_from(text, &close, i) {
                        Some(c) => c + close.len(),
                        None => text.len(),
                    };
                    let marker = format!("\u{0}\u{0}P{}\u{0}\u{0}", store.len());
                    store.push(text[i..end].to_string());
                    out.push_str(&marker);
                    i = end;
                    continue 'scan;
                }
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, store)
}

fn restore_tags(text: &mut String, store: &[String]) {
    for (idx, original) in store.iter().enumerate() {
        let marker = format!("\u{0}\u{0}P{idx}\u{0}\u{0}");
        *text = text.replace(&marker, original);
    }
}

fn strip_switches(text: &str, sw: &mut BehaviorSwitches) -> String {
    let mut out = text.to_string();
    for word in BEHAVIOR_SWITCHES {
        if out.contains(word) {
            set_switch(sw, word);
            out = out.replace(word, "");
        }
    }
    out
}

fn set_switch(sw: &mut BehaviorSwitches, word: &str) {
    match word {
        "__NOTOC__" => sw.no_toc = true,
        "__FORCETOC__" => sw.force_toc = true,
        "__TOC__" => sw.toc = true,
        "__NOEDITSECTION__" => sw.no_editsection = true,
        "__NOGALLERY__" => sw.no_gallery = true,
        "__INDEX__" => sw.index = true,
        "__NOINDEX__" => sw.no_index = true,
        "__HIDDENCAT__" => sw.hidden_cat = true,
        "__DISAMBIG__" => sw.disambig = true,
        "__NEWSECTIONLINK__" => sw.new_section_link = true,
        "__NONEWSECTIONLINK__" => sw.no_new_section_link = true,
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Redirects
// ---------------------------------------------------------------------------

/// Localized `#REDIRECT` synonyms. English first; extend for other wikis
/// (the depot supplies the localized magic-word list at import).
const REDIRECT_SYNONYMS: [&str; 1] = ["#redirect"];

/// `#REDIRECT [[Target]]` on RAW wikitext. Case-insensitive; tolerant of
/// an optional trailing `#section`/`|label` and a leading `:`.
pub fn parse_redirect(text: &str) -> Option<String> {
    let t = text.trim_start();
    let lower = t.to_ascii_lowercase();
    let syn = REDIRECT_SYNONYMS.iter().find(|s| lower.starts_with(**s))?;
    let rest = &t[syn.len()..];
    let open = rest.find("[[")?;
    let close = rest[open + 2..].find("]]")?;
    let inner = &rest[open + 2..open + 2 + close];
    let target = inner
        .split('|')
        .next()
        .unwrap_or(inner)
        .split('#')
        .next()
        .unwrap_or(inner)
        .trim();
    let target = target.strip_prefix(':').unwrap_or(target).trim();
    if target.is_empty() {
        None
    } else {
        Some(target.to_string())
    }
}
