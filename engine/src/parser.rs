//! Parser & lens engine — design scaffolding.
//!
//! ## Vision
//!
//! A miniKanren/Prolog-backed DCG parser that can handle the mixed,
//! nested, context-sensitive syntaxes sarun encounters:
//!
//!   - CLI commands: `sarun mirror run 5`
//!   - Protocol messages: `{"type":"ui","verb":"mirror_run","args":[5]}`
//!   - Network packets: HTTP-in-TLS-in-PCAP
//!   - Patches: unified diff hunks
//!   - Build graphs: ninja/make edges
//!   - The action registry: verb ↔ CLI ↔ key ↔ menu transformations
//!
//! ## Architecture
//!
//! The parser is a separate concern from the registry. The registry
//! declares the WHAT (action names, help, keys). The parser handles the
//! HOW (converting between representations).
//!
//! ### Phase 1: Rust-only name derivation (DONE)
//!
//! The `registry` module already derives CLI commands, menu labels, and
//! function names from verb identities by deterministic string
//! transformation. This covers 90% of the registry's needs.
//!
//! ### Phase 2: Prolog DCG grammar (TODO)
//!
//! Embed SWI-Prolog (statically linked, ~8MB — negligible next to
//! Chromium) and define DCG grammars for:
//!
//!   ```prolog
//!   verb(Name) --> identifier(Name).
//!   cli(Cmd)  --> {verb(Verb), atom_chars(Verb, Chars),
//!                  split_underscore(Chars, Words),
//!                 maplist(atom_lower, Words, LowerWords),
//!                 atomic_list_concat(LowerWords, ' ', Cmd)}.
//!   ```
//!
//! The grammar is bidirectional: `verb(mirror_run)` ↔ `cli(mirror run)`.
//! Tabling handles the search efficiently.
//!
//! ### Phase 3: Mixed-syntax parser (TODO)
//!
//! DCG grammars for nested content:
//!   - HTML with embedded `<script>` (JavaScript with embedded strings)
//!   - PCAP with nested protocol layers
//!   - Unified diffs with embedded file content
//!
//! Each grammar provides parse + unparse + completion (from the
//! relational nature of DCG: unbound variables produce all solutions).
//!
//! ## Module structure
//!
//! ```
//! parser/
//!   mod.rs          — this file (public API)
//!   prolog.rs       — SWI-Prolog FFI bindings (future)
//!   grammar.rs      — DCG grammar loading and query interface (future)
//!   lens.rs         — n-way transformation (verb → CLI → protocol → ...) (future)
//!   complete.rs     — completion engine (currently delegates to registry::complete)
//! ```
//!
//! ## Current API
//!
//! The current API is minimal — just the completion proxy. As the Prolog
//! engine is added, `parse` and `transform` will be implemented here.

/// Parse a command string into a verb + args.
/// Currently a simple whitespace split; will be replaced by a proper
/// grammar that understands quoting, escaping, and type annotations.
pub fn parse_command(input: &str) -> (String, Vec<String>) {
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.is_empty() {
        return (String::new(), Vec::new());
    }
    (parts[0].to_string(), parts[1..].iter().map(|s| s.to_string()).collect())
}

/// Complete a partial verb name using the registry.
pub fn complete(prefix: &str) -> Vec<&'static str> {
    crate::registry::complete(prefix)
}

/// Transform a verb name into its CLI command form.
pub fn verb_to_cli(verb: &str) -> String {
    crate::registry::derive_cli(verb)
}

/// Transform a CLI command path into a verb name (reverse derivation).
pub fn cli_to_verb(path: &[&str]) -> Option<&'static str> {
    crate::registry::verb_for_cli(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let (verb, args) = parse_command("mirror_run 5");
        assert_eq!(verb, "mirror_run");
        assert_eq!(args, vec!["5"]);
    }

    #[test]
    fn parse_empty() {
        let (verb, args) = parse_command("");
        assert_eq!(verb, "");
        assert!(args.is_empty());
    }

    #[test]
    fn verb_cli_roundtrip() {
        let cli = verb_to_cli("mirror_run");
        assert_eq!(cli, "mirror run");
    }

    #[test]
    fn complete_prefix() {
        let matches = complete("mirror");
        assert!(matches.contains(&"mirror_run"));
    }
}
