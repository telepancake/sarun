// Tree-sitter-driven structural locators — the "lenses" the design called for.
// Lets `inspect` enumerate named definitions in a file and lets `write` splice
// content into a definition's byte range without the model counting lines.
//
// Locator grammar additions (parsed by inspect::parse_locator):
//   path symbols              — list all named definitions in `path`
//   path symbol <name>        — focus on definition `name` (first occurrence)
//   path symbol <name>[N]     — Nth occurrence, 1-based (disambiguates name
//                                collisions across nested scopes)
//
// Language coverage (V1): rust, python, bash. The dispatch is keyed off the
// file extension; non-recognised extensions return None and the caller falls
// back to the legacy line-range view, so structural support is purely additive.

use tree_sitter::{Node, Parser};

/// One named definition extracted from a parsed file.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    /// Short tag of what this symbol is — "fn", "struct", "class", "method",
    /// "trait", etc. The exact tag is language-specific; treat it as a label,
    /// not a programmatic discriminant.
    pub kind: String,
    /// Inclusive byte range that covers the entire definition (header through
    /// closing brace). `write` replaces exactly this range.
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based line numbers for the inspect/read output.
    pub start_line: usize,
    pub end_line: usize,
    /// Nesting depth — 0 = top-level, 1 = inside a class/mod/impl/trait, etc.
    /// Used to render the listing readably (`  method Foo::bar` vs `fn baz`).
    pub depth: usize,
}

/// Parse `bytes` according to the language implied by `path`'s extension and
/// return the list of named definitions, walked in source order. Returns None
/// when the extension isn't recognised — the caller falls back to line view.
pub fn parse_symbols(path: &str, bytes: &[u8]) -> Option<Vec<Symbol>> {
    let lang = language_for(path)?;
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(bytes, None)?;
    let mut out = Vec::new();
    walk(tree.root_node(), bytes, path, 0, &mut out);
    Some(out)
}

fn language_for(path: &str) -> Option<tree_sitter::Language> {
    let ext = std::path::Path::new(path)
        .extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "py" => Some(tree_sitter_python::LANGUAGE.into()),
        "sh" | "bash" => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    }
}

/// Whether to walk INTO a definition's body looking for more named children.
/// `impl`/`class`/`trait`/`mod` host methods we want to surface; `fn` does not
/// (nested functions are rare and listing them by their inner name without
/// scope is more confusing than useful).
fn recurse_into(kind: &str) -> bool {
    matches!(kind, "impl" | "class" | "trait" | "mod" | "enum")
}

fn walk(node: Node, bytes: &[u8], path: &str, depth: usize, out: &mut Vec<Symbol>) {
    let ext = std::path::Path::new(path)
        .extension().and_then(|e| e.to_str()).unwrap_or("");
    if let Some((kind, name)) = classify(ext, node, bytes) {
        out.push(Symbol {
            name,
            kind: kind.to_string(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            depth,
        });
        if recurse_into(kind) {
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                walk(child, bytes, path, depth + 1, out);
            }
        }
        return;
    }
    // Not a definition node — descend looking for ones inside.
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        walk(child, bytes, path, depth, out);
    }
}

/// (kind, name) for nodes we treat as definitions in each language. Returns
/// None for nodes that aren't definitions (the walker will then recurse).
fn classify(ext: &str, node: Node, bytes: &[u8]) -> Option<(&'static str, String)> {
    let nk = node.kind();
    let kind: &'static str = match (ext, nk) {
        // Rust ─────────────────────────────────────────────────────────────
        ("rs", "function_item")        => "fn",
        ("rs", "struct_item")          => "struct",
        ("rs", "enum_item")            => "enum",
        ("rs", "trait_item")           => "trait",
        ("rs", "mod_item")             => "mod",
        ("rs", "impl_item")            => "impl",
        ("rs", "macro_definition")     => "macro",
        ("rs", "const_item")           => "const",
        ("rs", "static_item")          => "static",
        ("rs", "type_item")            => "type",
        // Python ───────────────────────────────────────────────────────────
        ("py", "function_definition") => "fn",
        ("py", "class_definition")    => "class",
        // Bash ─────────────────────────────────────────────────────────────
        ("sh", "function_definition") => "fn",
        ("bash", "function_definition") => "fn",
        _ => return None,
    };
    let name = symbol_name(ext, nk, node, bytes)?;
    Some((kind, name))
}

/// Pull a human-readable identifier out of `node`. Most definitions have a
/// `name` field exposed by tree-sitter; impl blocks don't, so we synthesise
/// `Type` or `Trait for Type` from their fields. Returns None when we can't
/// produce anything sensible — caller drops the symbol entry rather than
/// emit "?".
fn symbol_name(ext: &str, nk: &str, node: Node, bytes: &[u8]) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(name_node.utf8_text(bytes).ok()?.to_string());
    }
    if ext == "rs" && nk == "impl_item" {
        let type_part = node.child_by_field_name("type")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(String::from);
        let trait_part = node.child_by_field_name("trait")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(String::from);
        return match (trait_part, type_part) {
            (Some(t), Some(ty)) => Some(format!("{t} for {ty}")),
            (None,    Some(ty)) => Some(ty),
            _ => None,
        };
    }
    // Generic fallback: scan children for the first identifier-ish leaf.
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if matches!(child.kind(),
                    "identifier" | "type_identifier" | "field_identifier" | "word") {
            return child.utf8_text(bytes).ok().map(String::from);
        }
    }
    None
}

/// Find the Nth occurrence of `name` in `symbols` (1-based). Returns None
/// when there aren't that many matches.
pub fn find_symbol<'a>(symbols: &'a [Symbol], name: &str, occurrence: usize)
    -> Option<&'a Symbol>
{
    symbols.iter().filter(|s| s.name == name).nth(occurrence.saturating_sub(1))
}
