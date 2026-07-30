//! Conservative, deterministic extraction of render-time dependencies from
//! one revision's wikitext.
//!
//! This is deliberately not a renderer.  It recognizes references whose
//! target is static and marks references found only on an undecidable path as
//! possible.  Dynamic target names are left for the renderer/runtime index.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum EdgeKind {
    Template,
    Module,
    Category,
    File,
    UserEdits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawEdge {
    pub kind: EdgeKind,
    pub title: String,
    pub certainty: Certainty,
}

pub(crate) struct Extracted {
    pub edges: Vec<RawEdge>,
    pub dynamic_targets: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Certainty {
    Definite,
    Possible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InclusionContext {
    /// Parsing the page as its own content.
    Page,
    /// Parsing the page as a transcluded template.
    Transclusion,
}

#[derive(Clone, Debug)]
pub(crate) struct NamespaceMap {
    by_prefix: BTreeMap<String, EdgeKind>,
    preferred: BTreeMap<EdgeKind, String>,
    known_prefixes: BTreeSet<String>,
    first_letter_prefixes: BTreeSet<String>,
    first_letter_kinds: BTreeSet<EdgeKind>,
    main_first_letter: bool,
    magic_words_sensitive: BTreeSet<String>,
    magic_words_folded: BTreeSet<String>,
    magic_patterns_sensitive: Vec<(String, String)>,
    magic_patterns_folded: Vec<(String, String)>,
    parser_functions_sensitive: BTreeMap<String, String>,
    parser_functions_sensitive_folded: BTreeSet<String>,
    parser_functions_folded: BTreeMap<String, String>,
}

impl NamespaceMap {
    pub(crate) fn english() -> Self {
        let mut map = Self {
            by_prefix: BTreeMap::new(),
            preferred: BTreeMap::new(),
            known_prefixes: BTreeSet::new(),
            first_letter_prefixes: BTreeSet::new(),
            first_letter_kinds: BTreeSet::new(),
            main_first_letter: true,
            magic_words_sensitive: BTreeSet::new(),
            magic_words_folded: BTreeSet::new(),
            magic_patterns_sensitive: Vec::new(),
            magic_patterns_folded: Vec::new(),
            parser_functions_sensitive: BTreeMap::new(),
            parser_functions_sensitive_folded: BTreeSet::new(),
            parser_functions_folded: BTreeMap::new(),
        };
        for (kind, id, aliases) in [
            (EdgeKind::Template, "Template", &["Template"][..]),
            (EdgeKind::Module, "Module", &["Module"][..]),
            (EdgeKind::Category, "Category", &["Category"][..]),
            (EdgeKind::File, "File", &["File", "Image"][..]),
        ] {
            map.add(kind, id, aliases.iter().copied());
        }
        map
    }

    pub(crate) fn add(
        &mut self,
        kind: EdgeKind,
        preferred: &str,
        aliases: impl IntoIterator<Item = impl AsRef<str>>,
    ) {
        self.preferred.insert(kind, preferred.to_string());
        self.first_letter_kinds.insert(kind);
        self.by_prefix
            .insert(preferred.to_lowercase(), kind);
        self.known_prefixes.insert(preferred.to_lowercase());
        self.first_letter_prefixes.insert(preferred.to_lowercase());
        for alias in aliases {
            self.by_prefix
                .insert(alias.as_ref().to_lowercase(), kind);
            self.known_prefixes
                .insert(alias.as_ref().to_lowercase());
            self.first_letter_prefixes
                .insert(alias.as_ref().to_lowercase());
        }
    }

    pub(crate) fn add_known_prefix(&mut self, prefix: &str, first_letter: bool) {
        if !prefix.is_empty() {
            let prefix = prefix.to_lowercase();
            self.known_prefixes.insert(prefix.clone());
            if first_letter {
                self.first_letter_prefixes.insert(prefix);
            } else {
                self.first_letter_prefixes.remove(&prefix);
            }
        }
    }

    pub(crate) fn set_main_first_letter(&mut self, first_letter: bool) {
        self.main_first_letter = first_letter;
    }

    pub(crate) fn set_kind_first_letter(&mut self, kind: EdgeKind, first_letter: bool) {
        if first_letter {
            self.first_letter_kinds.insert(kind);
        } else {
            self.first_letter_kinds.remove(&kind);
        }
    }

    pub(crate) fn add_magic_word(
        &mut self,
        canonical: &str,
        alias: &str,
        case_sensitive: bool,
    ) {
        let normalized = alias.trim();
        let folded = normalized.to_lowercase();
        if normalized.starts_with('#') {
            let canonical = format!("#{}", canonical.trim_start_matches('#').to_lowercase());
            if case_sensitive {
                self.parser_functions_sensitive
                    .insert(normalized.to_string(), canonical);
                self.parser_functions_sensitive_folded.insert(folded);
            } else {
                self.parser_functions_folded.insert(folded, canonical);
            }
        } else if let Some(position) = normalized.find("$1") {
            let prefix = &normalized[..position];
            // Empty-prefix aliases such as `$1px` are link/image syntax, not
            // brace-invoked magic words. Treating them as template patterns
            // would make every title match the empty prefix.
            if prefix.is_empty() {
                return;
            }
            let patterns = if case_sensitive {
                &mut self.magic_patterns_sensitive
            } else {
                &mut self.magic_patterns_folded
            };
            patterns.push(if case_sensitive {
                (
                    prefix.to_string(),
                    normalized[position + 2..].to_string(),
                )
            } else {
                (
                    prefix.to_lowercase(),
                    normalized[position + 2..].to_lowercase(),
                )
            });
            patterns.sort();
            patterns.dedup();
        } else if case_sensitive {
            self.magic_words_sensitive.insert(normalized.to_string());
        } else {
            self.magic_words_folded.insert(folded);
        }
    }

    fn classify(&self, title: &str) -> Option<EdgeKind> {
        let (prefix, _) = title.split_once(':')?;
        self.by_prefix.get(&prefix.trim().to_lowercase()).copied()
    }

    fn has_known_prefix(&self, title: &str) -> bool {
        title
            .split_once(':')
            .is_some_and(|(prefix, _)| self.known_prefixes.contains(&prefix.trim().to_lowercase()))
    }

    fn is_magic_word(&self, title: &str) -> bool {
        let title = title.trim();
        let folded = title.to_lowercase();
        self.magic_words_sensitive.contains(title)
            || self.magic_words_folded.contains(&folded)
            || self
                .magic_patterns_sensitive
                .iter()
                .any(|(prefix, suffix)| title.starts_with(prefix) && title.ends_with(suffix))
            || self
                .magic_patterns_folded
                .iter()
                .any(|(prefix, suffix)| folded.starts_with(prefix) && folded.ends_with(suffix))
    }

    fn parser_function_name(&self, name: &str) -> String {
        let name = name.trim();
        let folded = name.to_lowercase();
        self.parser_functions_sensitive
            .get(name)
            .cloned()
            .or_else(|| {
                if self.parser_functions_sensitive_folded.contains(&folded) {
                    Some("#__case_mismatch__".to_string())
                } else {
                    self.parser_functions_folded.get(&folded).cloned()
                }
            })
            .unwrap_or_else(|| name.trim().to_ascii_lowercase())
    }

    pub(crate) fn kind_for_title(&self, title: &str) -> Option<EdgeKind> {
        self.classify(title)
    }

    pub(crate) fn normalize_title_for_site(&self, title: &str) -> String {
        let Some(kind) = self.classify(title) else {
            let normalized = normalized_name(title).unwrap_or_default();
            if let Some((prefix, suffix)) = normalized.split_once(':') {
                if self
                    .first_letter_prefixes
                    .contains(&prefix.trim().to_lowercase())
                {
                    return format!("{prefix}:{}", uppercase_first(suffix));
                }
                return normalized;
            }
            return if self.main_first_letter {
                uppercase_first(&normalized)
            } else {
                normalized
            };
        };
        let suffix = title.split_once(':').map_or(title, |(_, suffix)| suffix);
        self.qualify(kind, suffix)
    }

    fn qualify(&self, kind: EdgeKind, name: &str) -> String {
        let name = normalized_name(name).unwrap_or_default();
        let name = if self.first_letter_kinds.contains(&kind) {
            uppercase_first(&name)
        } else {
            name
        };
        format!(
            "{}:{}",
            self.preferred.get(&kind).map(String::as_str).unwrap_or(""),
            name,
        )
    }
}

fn uppercase_first(value: &str) -> String {
    let Some(first) = value.chars().next() else {
        return String::new();
    };
    first.to_uppercase().chain(value.chars().skip(1)).collect()
}

/// Extract references as the page's own content.
#[cfg(test)]
pub(crate) fn extract(text: &str) -> Vec<RawEdge> {
    extract_with_namespaces(text, InclusionContext::Page, &NamespaceMap::english())
}

/// Extract references in stable `(kind, title)` order, with duplicates
/// coalesced.  A definite occurrence dominates possible occurrences.
#[cfg(test)]
pub(crate) fn extract_in_context(text: &str, context: InclusionContext) -> Vec<RawEdge> {
    extract_with_namespaces(text, context, &NamespaceMap::english())
}

#[cfg(test)]
pub(crate) fn extract_with_namespaces(
    text: &str,
    context: InclusionContext,
    namespaces: &NamespaceMap,
) -> Vec<RawEdge> {
    extract_report_with_namespaces(text, context, namespaces).edges
}

pub(crate) fn extract_report_with_namespaces(
    text: &str,
    context: InclusionContext,
    namespaces: &NamespaceMap,
) -> Extracted {
    let text = inclusion_text(text, context);
    let mut found = Found {
        edges: BTreeMap::new(),
        namespaces,
        dynamic_targets: 0,
    };
    scan(&text, false, &mut found);
    let dynamic_targets = found.dynamic_targets;
    let edges = found
        .edges
        .into_iter()
        .map(|((kind, title), possible)| RawEdge {
            kind,
            title,
            certainty: if possible {
                Certainty::Possible
            } else {
                Certainty::Definite
            },
        })
        .collect();
    Extracted {
        edges,
        dynamic_targets,
    }
}

struct Found<'a> {
    edges: BTreeMap<(EdgeKind, String), bool>,
    namespaces: &'a NamespaceMap,
    dynamic_targets: u64,
}

impl Found<'_> {
    fn add(&mut self, kind: EdgeKind, target: String, possible: bool) {
        self.edges
            .entry((kind, target))
            .and_modify(|old| *old &= possible)
            .or_insert(possible);
    }
}

fn scan(text: &str, possible: bool, found: &mut Found<'_>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i..].starts_with(b"{{{") {
            if let Some(end) = balanced_end(text, i, b"{{{", b"}}}") {
                scan_parameter(&text[i + 3..end - 3], possible, found);
                i = end;
                continue;
            }
        } else if bytes[i..].starts_with(b"{{") {
            if let Some(end) = balanced_end(text, i, b"{{", b"}}") {
                scan_template(&text[i + 2..end - 2], possible, found);
                i = end;
                continue;
            }
        } else if bytes[i..].starts_with(b"[[") {
            if let Some(end) = balanced_end(text, i, b"[[", b"]]") {
                scan_link(&text[i + 2..end - 2], possible, found);
                i = end;
                continue;
            }
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
}

fn scan_parameter(body: &str, possible: bool, found: &mut Found<'_>) {
    let parts = split_top_level(body, b'|');
    // The parameter name is evaluated, but defaults are selected only when
    // the caller did not provide a value.
    if let Some(name) = parts.first() {
        scan(name, possible, found);
    }
    for default in parts.iter().skip(1) {
        scan(default, true, found);
    }
}

fn scan_template(body: &str, possible: bool, found: &mut Found<'_>) {
    let parts = split_top_level(body, b'|');
    let Some(raw_head) = parts.first() else {
        return;
    };
    let head = raw_head.trim();
    scan(head, possible, found);
    if let Some((function, first)) = parser_function(head) {
        scan_parser_function(function, first, &parts[1..], possible, found);
        return;
    }

    match template_target(head, found.namespaces) {
        Some(target) => found.add(EdgeKind::Template, target, possible),
        None if head.contains("{{") || head.contains("[[") => {
            found.dynamic_targets += 1;
        }
        None => {}
    }
    // Template arguments are lazy in MediaWiki.  Without expanding the
    // callee we cannot know which values it reads.
    for argument in parts.iter().skip(1) {
        scan(argument_value(argument), true, found);
    }
}

fn scan_parser_function(
    function: &str,
    first: &str,
    rest: &[&str],
    possible: bool,
    found: &mut Found<'_>,
) {
    let name = found.namespaces.parser_function_name(function);
    match name.as_str() {
        "#invoke" => {
            scan(first, possible, found);
            if let Some(module) = static_name(first) {
                found.add(
                    EdgeKind::Module,
                    if found.namespaces.classify(&module) == Some(EdgeKind::Module) {
                        normalized_name(&module).unwrap()
                    } else {
                        found.namespaces.qualify(EdgeKind::Module, &module)
                    },
                    possible,
                );
            } else {
                found.dynamic_targets += 1;
            }
            for argument in rest {
                scan(argument_value(argument), true, found);
            }
        }
        "#if" => {
            scan(first, possible, found);
            let decision = constant_text(first).map(|v| !v.trim().is_empty());
            conditional_arms(decision, rest.first(), rest.get(1), possible, found);
        }
        "#ifeq" => {
            scan(first, possible, found);
            let rhs = rest.first().copied().unwrap_or("");
            scan(rhs, possible, found);
            let decision = constant_text(first)
                .zip(constant_text(rhs))
                .and_then(|(a, b)| mediawiki_ifeq(a.trim(), b.trim()));
            conditional_arms(decision, rest.get(1), rest.get(2), possible, found);
        }
        "#ifexpr" => {
            scan(first, possible, found);
            let decision = constant_text(first).and_then(eval_ifexpr);
            conditional_arms(decision, rest.first(), rest.get(1), possible, found);
        }
        "#switch" => scan_switch(first, rest, possible, found),
        // Unknown parser functions may evaluate any argument.  They are not
        // template transclusions, but their nested static references matter.
        _ => {
            scan(first, possible, found);
            for argument in rest {
                scan(argument_value(argument), true, found);
            }
        }
    }
}

fn mediawiki_ifeq(left: &str, right: &str) -> Option<bool> {
    if left == right {
        return Some(true);
    }
    if let (Ok(left), Ok(right)) = (left.parse::<i128>(), right.parse::<i128>()) {
        return Some(left == right);
    }
    let left_number = left.parse::<f64>();
    let right_number = right.parse::<f64>();
    match (left_number, right_number) {
        (Ok(left), Ok(right)) if left.is_finite() && right.is_finite() && left != right => {
            Some(false)
        }
        (Ok(_), Ok(_)) => None,
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => Some(false),
        (Err(_), Err(_)) => Some(false),
    }
}

fn conditional_arms(
    decision: Option<bool>,
    yes: Option<&&str>,
    no: Option<&&str>,
    possible: bool,
    found: &mut Found<'_>,
) {
    match decision {
        Some(true) => scan(yes.copied().unwrap_or(""), possible, found),
        Some(false) => scan(no.copied().unwrap_or(""), possible, found),
        None => {
            scan(yes.copied().unwrap_or(""), true, found);
            scan(no.copied().unwrap_or(""), true, found);
        }
    }
}

fn scan_switch(expr: &str, arms: &[&str], possible: bool, found: &mut Found<'_>) {
    scan(expr, possible, found);
    let Some(expr) = constant_text(expr) else {
        for arm in arms {
            if let Some((key, value)) = split_top_level_once(arm, b'=') {
                if !key.trim().eq_ignore_ascii_case("#default") {
                    scan(key, true, found);
                }
                scan(value, true, found);
            } else {
                scan(arm, true, found);
            }
        }
        return;
    };

    let needle = expr.trim();
    let mut pending_match = false;
    let mut pending_dynamic = false;
    let mut dynamic_before = false;
    let mut default = None;
    for arm in arms {
        if let Some((key, value)) = split_top_level_once(arm, b'=') {
            let key = key.trim();
            if key.eq_ignore_ascii_case("#default") {
                default = Some(value);
            } else {
                scan(key, true, found);
                let constant = constant_text(key);
                if pending_match {
                    scan(value, possible, found);
                    return;
                }
                if constant.is_some_and(|v| v.trim() == needle) {
                    scan(value, possible || dynamic_before, found);
                    return;
                }
                if pending_dynamic || constant.is_none() {
                    scan(value, true, found);
                }
                if constant.is_none() {
                    dynamic_before = true;
                }
            }
            pending_match = false;
            pending_dynamic = false;
        } else {
            scan(arm, true, found);
            match constant_text(arm) {
                Some(value) if value.trim() == needle => {
                    // Bare cases fall through to the next `key = value` arm.
                    pending_match = true;
                }
                Some(_) => {}
                None => {
                    pending_dynamic = true;
                    dynamic_before = true;
                }
            }
        }
    }
    if let Some(default) = default {
        scan(default, possible || dynamic_before, found);
    }
}

fn scan_link(body: &str, possible: bool, found: &mut Found<'_>) {
    let parts = split_top_level(body, b'|');
    let Some(head) = parts.first() else {
        return;
    };
    scan(head, possible, found);
    let Some(mut target) = static_name(head) else {
        if matches!(
            found.namespaces.kind_for_title(head),
            Some(EdgeKind::Category | EdgeKind::File)
        ) {
            found.dynamic_targets += 1;
        }
        return;
    };
    if target.starts_with(':') {
        return;
    }
    target = target.trim().to_string();
    match found.namespaces.classify(&target) {
        Some(EdgeKind::Category) => {
            found.add(EdgeKind::Category, normalized_name(&target).unwrap(), possible);
        }
        Some(EdgeKind::File) => {
            found.add(EdgeKind::File, normalized_name(&target).unwrap(), possible);
        }
        _ => {}
    }
    // File captions can contain links/templates.
    for part in parts.iter().skip(1) {
        scan(part, possible, found);
    }
}

fn parser_function(head: &str) -> Option<(&str, &str)> {
    let (name, first) = split_top_level_once(head, b':')?;
    name.trim_start().starts_with('#').then_some((name, first))
}

fn template_target(head: &str, namespaces: &NamespaceMap) -> Option<String> {
    let mut name = static_name(head)?;
    for prefix in ["subst:", "safesubst:"] {
        if name
            .get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
        {
            name = name[prefix.len()..].trim_start().to_string();
        }
    }
    if name.is_empty() {
        return None;
    }
    if namespaces.is_magic_word(&name) {
        return None;
    }
    if let Some(main) = name.strip_prefix(':') {
        return normalized_name(main);
    }
    if namespaces.has_known_prefix(&name) {
        normalized_name(&name)
    } else {
        Some(namespaces.qualify(EdgeKind::Template, &name))
    }
}

fn argument_value(argument: &str) -> &str {
    split_top_level_once(argument, b'=')
        .map(|(_, value)| value)
        .unwrap_or(argument)
}

fn normalized_name(name: &str) -> Option<String> {
    let normalized = name
        .trim()
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn static_name(text: &str) -> Option<String> {
    (!text.contains("{{") && !text.contains("[["))
        .then(|| normalized_name(text))
        .flatten()
}

fn constant_text(text: &str) -> Option<&str> {
    (!text.contains("{{") && !text.contains("[[") && !text.contains("}}}")).then_some(text)
}

fn split_top_level(text: &str, delimiter: u8) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut curly = 0usize;
    let mut square = 0usize;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i..].starts_with(b"{{{") {
            curly += 3;
            i += 3;
        } else if bytes[i..].starts_with(b"{{") {
            curly += 2;
            i += 2;
        } else if bytes[i..].starts_with(b"}}}") && curly >= 3 {
            curly -= 3;
            i += 3;
        } else if bytes[i..].starts_with(b"}}") && curly >= 2 {
            curly -= 2;
            i += 2;
        } else if bytes[i..].starts_with(b"[[") {
            square += 2;
            i += 2;
        } else if bytes[i..].starts_with(b"]]") && square >= 2 {
            square -= 2;
            i += 2;
        } else if bytes[i] == delimiter && curly == 0 && square == 0 {
            parts.push(&text[start..i]);
            start = i + 1;
            i += 1;
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    parts.push(&text[start..]);
    parts
}

fn split_top_level_once(text: &str, delimiter: u8) -> Option<(&str, &str)> {
    let parts = split_top_level(text, delimiter);
    (parts.len() > 1).then(|| {
        let offset = parts[0].len();
        (&text[..offset], &text[offset + 1..])
    })
}

fn balanced_end(text: &str, start: usize, open: &[u8], close: &[u8]) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut stack = vec![(open, close)];
    let mut i = start + open.len();
    while i < bytes.len() {
        let (current_open, current_close) = *stack.last()?;
        if bytes[i..].starts_with(b"{{{") {
            stack.push((b"{{{", b"}}}"));
            i += 3;
        } else if bytes[i..].starts_with(b"{{") {
            stack.push((b"{{", b"}}"));
            i += 2;
        } else if bytes[i..].starts_with(b"[[") {
            stack.push((b"[[", b"]]"));
            i += 2;
        } else if bytes[i..].starts_with(current_close) {
            i += current_close.len();
            stack.pop();
            if stack.is_empty() {
                return Some(i);
            }
        } else if bytes[i..].starts_with(current_open) {
            stack.push((current_open, current_close));
            i += current_open.len();
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    None
}

// -------------------------------------------------------------------------
// Inclusion and opaque-tag handling

fn inclusion_text(text: &str, context: InclusionContext) -> String {
    let text = strip_opaque(text);
    let has_onlyinclude = context == InclusionContext::Transclusion
        && contains_open_tag(&text, "onlyinclude");
    let mut out = String::with_capacity(text.len());
    let mut only = 0usize;
    let mut no = 0usize;
    let mut include = 0usize;
    let mut i = 0;
    while i < text.len() {
        if let Some(tag) = tag_at(&text, i) {
            match tag.name.as_str() {
                "onlyinclude" => update_depth(&mut only, &tag),
                "noinclude" => update_depth(&mut no, &tag),
                "includeonly" => update_depth(&mut include, &tag),
                _ => {
                    if inclusion_visible(context, has_onlyinclude, only, no, include) {
                        out.push_str(&text[i..tag.end]);
                    }
                }
            }
            i = tag.end;
        } else {
            let next = text[i..].find('<').map_or(text.len(), |n| i + n);
            if inclusion_visible(context, has_onlyinclude, only, no, include) {
                out.push_str(&text[i..next]);
            }
            i = if next == i {
                let ch = text[i..].chars().next().unwrap();
                if inclusion_visible(context, has_onlyinclude, only, no, include) {
                    out.push(ch);
                }
                i + ch.len_utf8()
            } else {
                next
            };
        }
    }
    out
}

fn inclusion_visible(
    context: InclusionContext,
    has_onlyinclude: bool,
    only: usize,
    no: usize,
    include: usize,
) -> bool {
    match context {
        InclusionContext::Page => include == 0,
        InclusionContext::Transclusion if has_onlyinclude => only > 0 && no == 0,
        InclusionContext::Transclusion => no == 0,
    }
}

fn update_depth(depth: &mut usize, tag: &Tag) {
    if tag.self_closing {
        return;
    }
    if tag.closing {
        *depth = depth.saturating_sub(1);
    } else {
        *depth += 1;
    }
}

fn strip_opaque(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with("<!--") {
            i = text[i + 4..]
                .find("-->")
                .map_or(text.len(), |n| i + 4 + n + 3);
            continue;
        }
        if let Some(tag) = tag_at(text, i) {
            if matches!(
                tag.name.as_str(),
                "nowiki" | "pre" | "source" | "syntaxhighlight"
            ) {
                if tag.closing || tag.self_closing {
                    i = tag.end;
                } else {
                    i = find_close_tag(text, tag.end, &tag.name).unwrap_or(text.len());
                }
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn contains_open_tag(text: &str, name: &str) -> bool {
    let mut i = 0;
    while i < text.len() {
        if let Some(tag) = tag_at(text, i) {
            if tag.name == name && !tag.closing && !tag.self_closing {
                return true;
            }
            i = tag.end;
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    false
}

fn find_close_tag(text: &str, mut i: usize, name: &str) -> Option<usize> {
    let mut depth = 1usize;
    while i < text.len() {
        if let Some(tag) = tag_at(text, i) {
            if tag.name == name {
                if tag.closing {
                    depth -= 1;
                    if depth == 0 {
                        return Some(tag.end);
                    }
                } else if !tag.self_closing {
                    depth += 1;
                }
            }
            i = tag.end;
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    None
}

struct Tag {
    name: String,
    closing: bool,
    self_closing: bool,
    end: usize,
}

fn tag_at(text: &str, start: usize) -> Option<Tag> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'<') {
        return None;
    }
    let mut i = start + 1;
    let closing = bytes.get(i) == Some(&b'/');
    i += usize::from(closing);
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    let name_start = i;
    while bytes
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-')
    {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = text[name_start..i].to_ascii_lowercase();
    let mut tail = i;
    let mut quote = None;
    while let Some(&byte) = bytes.get(tail) {
        match (quote, byte) {
            (Some(open), close) if open == close => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => break,
            _ => {}
        }
        tail += 1;
    }
    if bytes.get(tail) != Some(&b'>') {
        return None;
    }
    let self_closing = text[i..tail].trim_end().ends_with('/');
    Some(Tag {
        name,
        closing,
        self_closing,
        end: tail + 1,
    })
}

// -------------------------------------------------------------------------
// Small constant evaluator for #ifexpr.

fn eval_ifexpr(input: &str) -> Option<bool> {
    let mut parser = ExprParser {
        input: input.as_bytes(),
        pos: 0,
    };
    let value = parser.expr(0)?;
    parser.space();
    (parser.pos == parser.input.len()).then_some(value != 0.0)
}

struct ExprParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl ExprParser<'_> {
    fn expr(&mut self, min_prec: u8) -> Option<f64> {
        self.space();
        let mut lhs = if self.take(b"(") {
            let value = self.expr(0)?;
            self.space();
            self.take(b")").then_some(value)?
        } else if self.take_word("not") {
            f64::from(self.expr(7)? == 0.0)
        } else if self.take(b"-") {
            -self.expr(7)?
        } else if self.take(b"+") {
            self.expr(7)?
        } else {
            self.number()?
        };
        loop {
            self.space();
            let saved = self.pos;
            let Some((prec, op)) = self.operator() else {
                break;
            };
            if prec < min_prec {
                self.pos = saved;
                break;
            }
            let rhs = self.expr(prec + 1)?;
            lhs = match op {
                "or" => f64::from(lhs != 0.0 || rhs != 0.0),
                "and" => f64::from(lhs != 0.0 && rhs != 0.0),
                "=" | "==" => f64::from(lhs == rhs),
                "!=" | "<>" => f64::from(lhs != rhs),
                "<" => f64::from(lhs < rhs),
                "<=" => f64::from(lhs <= rhs),
                ">" => f64::from(lhs > rhs),
                ">=" => f64::from(lhs >= rhs),
                "+" => lhs + rhs,
                "-" => lhs - rhs,
                "*" => lhs * rhs,
                "/" => {
                    if rhs == 0.0 {
                        return None;
                    }
                    lhs / rhs
                }
                _ => return None,
            };
        }
        Some(lhs)
    }

    fn operator(&mut self) -> Option<(u8, &'static str)> {
        for (token, prec, op) in [
            ("or", 1, "or"),
            ("and", 2, "and"),
            ("<=", 3, "<="),
            (">=", 3, ">="),
            ("!=", 3, "!="),
            ("<>", 3, "<>"),
            ("==", 3, "=="),
            ("=", 3, "="),
            ("<", 3, "<"),
            (">", 3, ">"),
            ("+", 4, "+"),
            ("-", 4, "-"),
            ("*", 5, "*"),
            ("/", 5, "/"),
        ] {
            let matched = if token.as_bytes()[0].is_ascii_alphabetic() {
                self.take_word(token)
            } else {
                self.take(token.as_bytes())
            };
            if matched {
                return Some((prec, op));
            }
        }
        None
    }

    fn number(&mut self) -> Option<f64> {
        self.space();
        let start = self.pos;
        while self
            .input
            .get(self.pos)
            .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
        {
            self.pos += 1;
        }
        (self.pos > start)
            .then(|| std::str::from_utf8(&self.input[start..self.pos]).ok()?.parse().ok())
            .flatten()
    }

    fn space(&mut self) {
        while self
            .input
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    fn take(&mut self, token: &[u8]) -> bool {
        if self.input[self.pos..].starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    fn take_word(&mut self, word: &str) -> bool {
        let bytes = word.as_bytes();
        if self.input[self.pos..]
            .get(..bytes.len())
            .is_some_and(|got| got.eq_ignore_ascii_case(bytes))
            && !self
                .input
                .get(self.pos + bytes.len())
                .is_some_and(u8::is_ascii_alphabetic)
        {
            self.pos += bytes.len();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str, context: InclusionContext) -> Vec<(EdgeKind, String, Certainty)> {
        let edges = match context {
            InclusionContext::Page => extract(text),
            InclusionContext::Transclusion => extract_in_context(text, context),
        };
        edges
            .into_iter()
            .map(|r| (r.kind, r.title, r.certainty))
            .collect()
    }

    #[test]
    fn inclusion_contexts_and_opaque_regions() {
        let source = "\
{{Both}}
<noinclude>{{PageOnly}}</noinclude>
<includeonly>{{TranscludedOnly}}</includeonly>
<!-- {{Comment}} -->
<nowiki>{{Nowiki}}</nowiki>
<pre>{{Pre}}</pre>
<source lang=rust>{{Source}}</source>
<syntaxhighlight lang='x'>{{Highlight}}</syntaxhighlight>";
        assert_eq!(
            names(source, InclusionContext::Page),
            vec![
                (EdgeKind::Template, "Template:Both".into(), Certainty::Definite),
                (EdgeKind::Template, "Template:PageOnly".into(), Certainty::Definite),
            ]
        );
        assert_eq!(
            names(source, InclusionContext::Transclusion),
            vec![
                (EdgeKind::Template, "Template:Both".into(), Certainty::Definite),
                (
                    EdgeKind::Template,
                    "Template:TranscludedOnly".into(),
                    Certainty::Definite
                ),
            ]
        );
    }

    #[test]
    fn onlyinclude_selects_transcluded_fragments() {
        let source =
            "{{Outside}}<ONLYINCLUDE>{{Inside}}</ONLYINCLUDE><onlyinclude>{{Also}}</onlyinclude>";
        assert_eq!(
            names(source, InclusionContext::Page),
            vec![
                (EdgeKind::Template, "Template:Also".into(), Certainty::Definite),
                (EdgeKind::Template, "Template:Inside".into(), Certainty::Definite),
                (EdgeKind::Template, "Template:Outside".into(), Certainty::Definite),
            ]
        );
        assert_eq!(
            names(source, InclusionContext::Transclusion),
            vec![
                (EdgeKind::Template, "Template:Also".into(), Certainty::Definite),
                (EdgeKind::Template, "Template:Inside".into(), Certainty::Definite),
            ]
        );
    }

    #[test]
    fn static_conditionals_select_only_reached_arms() {
        let source = "\
{{#if: yes |{{IfYes}}|{{IfNo}}}}
{{#ifeq: x | x |{{EqYes}}|{{EqNo}}}}
{{#ifexpr: (2 + 3) * 4 >= 20 and not 0 |{{ExprYes}}|{{ExprNo}}}}
{{#switch: b|a={{A}}|b={{B}}|#default={{Default}}}}";
        assert_eq!(
            names(source, InclusionContext::Page),
            vec![
                (EdgeKind::Template, "Template:B".into(), Certainty::Definite),
                (EdgeKind::Template, "Template:EqYes".into(), Certainty::Definite),
                (EdgeKind::Template, "Template:ExprYes".into(), Certainty::Definite),
                (EdgeKind::Template, "Template:IfYes".into(), Certainty::Definite),
            ]
        );
    }

    #[test]
    fn unknown_conditionals_emit_possible_edges() {
        let source = "\
{{#if: {{{flag|}}} |{{Yes}}|{{No}}}}
{{#ifeq: {{Value}} | x |{{Equal}}|{{Different}}}}
{{#switch: {{{kind}}}|a={{A}}|#default={{D}}}}";
        assert_eq!(
            names(source, InclusionContext::Page),
            vec![
                (EdgeKind::Template, "Template:A".into(), Certainty::Possible),
                (EdgeKind::Template, "Template:D".into(), Certainty::Possible),
                (EdgeKind::Template, "Template:Different".into(), Certainty::Possible),
                (EdgeKind::Template, "Template:Equal".into(), Certainty::Possible),
                (EdgeKind::Template, "Template:No".into(), Certainty::Possible),
                (EdgeKind::Template, "Template:Value".into(), Certainty::Definite),
                (EdgeKind::Template, "Template:Yes".into(), Certainty::Possible),
            ]
        );
    }

    #[test]
    fn extracts_reference_kinds_and_coalesces_certainty() {
        let source = "\
{{Box|lazy={{Maybe}}}}{{#if:1|{{Maybe}}}}
{{#invoke: Data | main}}
[[Category:Examples|E]]
[[File:Picture.jpg|thumb|{{Caption}}]]
[[:Category:Not membership]]";
        assert_eq!(
            names(source, InclusionContext::Page),
            vec![
                (EdgeKind::Template, "Template:Box".into(), Certainty::Definite),
                (
                    EdgeKind::Template,
                    "Template:Caption".into(),
                    Certainty::Definite
                ),
                (EdgeKind::Template, "Template:Maybe".into(), Certainty::Definite),
                (EdgeKind::Module, "Module:Data".into(), Certainty::Definite),
                (
                    EdgeKind::Category,
                    "Category:Examples".into(),
                    Certainty::Definite
                ),
                (EdgeKind::File, "File:Picture.jpg".into(), Certainty::Definite),
            ]
        );
    }

    #[test]
    fn localized_namespace_names_and_aliases_are_classified() {
        let mut namespaces = NamespaceMap::english();
        namespaces.add(EdgeKind::Template, "Шаблон", ["Ш", "Template"]);
        namespaces.add(EdgeKind::Module, "Модуль", ["Module"]);
        namespaces.add(EdgeKind::Category, "Категория", ["К", "Category"]);
        namespaces.add(EdgeKind::File, "Файл", ["Изображение", "File", "Image"]);
        namespaces.add_known_prefix("Участник", true);
        namespaces.add_known_prefix("w", false);
        let edges = extract_with_namespaces(
            "{{Шаблон:Карточка}}{{#invoke:Модуль:Данные|f}}\
             {{Участник:Пример}}{{w:Remote}}\
             [[Категория:Тест]][[Изображение:Карта.png]]",
            InclusionContext::Page,
            &namespaces,
        );
        assert!(edges.iter().any(|edge| {
            edge.kind == EdgeKind::Template && edge.title == "Шаблон:Карточка"
        }));
        assert!(edges.iter().any(|edge| {
            edge.kind == EdgeKind::Module && edge.title == "Модуль:Данные"
        }));
        assert!(edges.iter().any(|edge| {
            edge.kind == EdgeKind::Category && edge.title == "Категория:Тест"
        }));
        assert!(edges.iter().any(|edge| {
            edge.kind == EdgeKind::File && edge.title == "Изображение:Карта.png"
        }));
        assert!(edges.iter().any(|edge| {
            edge.kind == EdgeKind::Template && edge.title == "Участник:Пример"
        }));
        assert!(edges.iter().any(|edge| {
            edge.kind == EdgeKind::Template && edge.title == "w:Remote"
        }));
    }

    #[test]
    fn dynamic_targets_are_counted_separately_from_static_misses() {
        let namespaces = NamespaceMap::english();
        let extracted = extract_report_with_namespaces(
            "[[Category:{{name}}]]",
            InclusionContext::Page,
            &namespaces,
        );
        assert_eq!(extracted.dynamic_targets, 1);
    }

    #[test]
    fn dynamic_switch_case_makes_its_value_and_default_possible() {
        let edges = extract(
            "{{#switch:x|{{K}}={{A}}|#default={{D}}}}",
        );
        for title in ["Template:A", "Template:D"] {
            assert!(edges.iter().any(|edge| {
                edge.title == title && edge.certainty == Certainty::Possible
            }));
        }
    }

    #[test]
    fn site_magic_word_aliases_are_not_templates_and_local_functions_keep_semantics() {
        let mut namespaces = NamespaceMap::english();
        namespaces.add_magic_word("defaultsort", "DEFAULTSORT:$1", false);
        namespaces.add_magic_word("if", "#ja", false);
        namespaces.add_magic_word("img_width", "$1px", false);
        let extracted = extract_with_namespaces(
            "{{DEFAULTSORT:Key}}{{#ja:yes|{{A}}|{{B}}}}{{StillATemplate}}",
            InclusionContext::Page,
            &namespaces,
        );
        assert!(extracted.iter().any(|edge| edge.title == "Template:A"));
        assert!(extracted
            .iter()
            .any(|edge| edge.title == "Template:StillATemplate"));
        assert!(!extracted.iter().any(|edge| edge.title == "Template:B"));
        assert!(!extracted
            .iter()
            .any(|edge| edge.title.contains("DEFAULTSORT")));
    }

    #[test]
    fn first_letter_titles_and_numeric_ifeq_follow_mediawiki_rules() {
        let edges = extract("{{foo}}{{#ifeq:01|1|{{equal}}|{{different}}}}");
        assert!(edges.iter().any(|edge| edge.title == "Template:Foo"));
        assert!(edges.iter().any(|edge| edge.title == "Template:Equal"));
        assert!(!edges
            .iter()
            .any(|edge| edge.title == "Template:Different"));
    }

    #[test]
    fn case_sensitive_magic_aliases_require_exact_case() {
        let mut namespaces = NamespaceMap::english();
        namespaces.add_magic_word("defaultsort", "SORT:$1", true);
        let edges = extract_with_namespaces(
            "{{SORT:key}}{{sort:key}}",
            InclusionContext::Page,
            &namespaces,
        );
        assert!(!edges.iter().any(|edge| edge.title == "Template:SORT:key"));
        assert!(edges.iter().any(|edge| edge.title == "Template:Sort:key"));

        namespaces.add_magic_word("if", "#IF", true);
        let exact = extract_with_namespaces(
            "{{#IF:yes|{{A}}|{{B}}}}",
            InclusionContext::Page,
            &namespaces,
        );
        assert!(exact.iter().any(|edge| edge.title == "Template:A"));
        assert!(!exact.iter().any(|edge| edge.title == "Template:B"));
        let wrong_case = extract_with_namespaces(
            "{{#if:yes|{{A}}|{{B}}}}",
            InclusionContext::Page,
            &namespaces,
        );
        assert!(wrong_case.iter().all(|edge| edge.certainty == Certainty::Possible));
    }
}
