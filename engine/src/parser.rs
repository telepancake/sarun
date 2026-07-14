//! Parser & lens engine for sarun.
//!
//! ## Vision
//!
//! A Prolog DCG-backed parser that can handle the mixed, nested,
//! context-sensitive syntaxes sarun encounters:
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
//! ### Phase 1: Rust-only name derivation (DONE)
//!
//! The `registry` module derives CLI commands, menu labels, and function
//! names from verb identities by deterministic string transformation.
//! This covers the registry's needs.
//!
//! ### Phase 2: SWI-Prolog embedding (DESIGN)
//!
//! Embed SWI-Prolog as a static library (~8MB, negligible next to Chromium).
//! The FFI boundary:
//!
//! ```rust,ignore
//! // engine initialization
//! prolog::init();                           // PL_initialise, load .pl files
//! prolog::load_file("engine/pl/registry.pl");  // DCG grammars + facts
//! prolog::load_file("engine/pl/parse.pl");
//!
//! // query — parse a CLI string into a verb + args
//! let result = prolog::query("parse_cli(`mirror run 5`, Verb, Args)");
//! // result: Verb = "mirror_run", Args = [5]
//!
//! // inverse — unparse a verb + args into a CLI string
//! let cli = prolog::query("unparse_cli(mirror_run, [5], Str)");
//! // result: Str = "mirror run 5"
//!
//! // completion — all verbs matching a prefix
//! let completions = prolog::query("verb(V), atom_concat(mirror, _, V)");
//! // result: V = "mirror_jobs", "mirror_add", "mirror_run", ...
//! ```
//!
//! DCG grammar for the verb ↔ CLI transformation:
//!
//! ```prolog
//! :- table verb_cli/2.  % incremental tabling for O(1) lookup
//!
//! verb_cli(Verb, CliStr) :-
//!     verb(Verb),
//!     atomic_list_concat(Words, '_', Verb),
//!     maplist([W,L]>>downcase_atom(W,L), Words, Lower),
//!     atomic_list_concat(Lower, ' ', CliAtom),
//!     atom_string(CliAtom, CliStr).
//!
//! % Parse direction: string → verb
//! parse_cli(Str, Verb) :-
//!     verb_cli(Verb, Str).
//!
//! % Lens: both directions from one definition
//! cli_to_verb(Str, Verb) :- verb_cli(Verb, Str).
//! verb_to_cli(Verb, Str) :- verb_cli(Verb, Str).
//! ```
//!
//! ### Phase 3: Mixed-syntax parser (DESIGN)
//!
//! DCG grammars for nested content. Example — HTTP in a pcap:
//!
//! ```prolog
//! pcap_frame(Frame) -->
//!     pcap_header(PcapHdr),
//!     packet(PcapHdr, Packet).
//!
//! packet(Hdr, Packet) -->
//!     {Hdr = ethernet(EthHdr)},
//!     ethernet_payload(EthHdr, Payload),
//!     ip_packet(Payload, Packet).
//!
//! ip_packet(IpHdr, tcp_stream(Stream)) -->
//!     {ip_proto(IpHdr, tcp)},
//!     tcp_segment(IpHdr, Stream).
//!
//! tcp_payload(Stream, http_request(Req)) -->
//!     {tcp_reassembled(Stream, Bytes)},
//!     phrase(http_request(Req), Bytes).
//!
//! http_request(method(Method), path(Path), headers(Hs)) -->
//!     method(Method), sp, path(Path), sp,
//!     "HTTP/1.1\r\n", headers(Hs), "\r\n".
//! ```
//!
//! The grammar provides:
//!   - `parse(Bytes, AST)` — parse bytes into a structured AST
//!   - `unparse(AST, Bytes)` — serialize AST back to bytes
//!   - `complete(AST, PartialAST)` — given a partial AST, enumerate all
//!     valid completions (from DCG's relational nature: unbound variables
//!     produce all solutions)
//!
//! ### Phase 4: Lens composition (DESIGN)
//!
//! N-way transformations between representations:
//!
//! ```prolog
//! representation(verb, mirror_run).
//! representation(cli, "mirror run").
//! representation(key, r).
//! representation(menu, "Force-run this job").
//! representation(rpc, {type:ui, verb:mirror_run, args:[5]}).
//!
//! % Convert any representation to any other:
//! transform(From, To) :-
//!     representation(From, X),
//!     representation(To, X).
//! ```
//!
//! Normalization: the transformation may normalize whitespace, number
//! encoding, etc. Not strictly bijective — that's acceptable. True lenses
//! (lossless round-trip) only where it matters (patches, protocol bytes).
//!
//! ## Current API
//!
//! The current Rust API is minimal — name derivation and completion via
//! the registry. As the Prolog engine is added, `parse` and `transform`
//! will be implemented here.

/// Parse a command string into a verb + args.
///
/// Currently a simple whitespace split. The Prolog engine will replace
/// this with a proper grammar that understands quoting, escaping, and
/// type annotations.
///
/// "mirror run 5" → ("mirror_run", ["5"]) via cli_map lookup
/// "mirror_run 5" → ("mirror_run", ["5"]) via exact verb match
pub fn parse_command(input: &str) -> (String, Vec<String>) {
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.is_empty() {
        return (String::new(), Vec::new());
    }

    // Try exact verb match first
    if crate::registry::find(parts[0]).is_some() {
        return (parts[0].to_string(), parts[1..].iter().map(|s| s.to_string()).collect());
    }

    // Try CLI path: join all non-arg tokens with _ and check the registry
    // A "non-arg" is a token that's all lowercase letters (part of a verb name).
    // An "arg" starts with a digit, is a known flag, or contains uppercase.
    let mut verb_parts: Vec<&str> = Vec::new();
    let mut args: Vec<String> = Vec::new();
    for part in &parts {
        if verb_parts.is_empty()
            || (part.chars().all(|c| c.is_ascii_lowercase() || c == '.')
                && !is_arg(part))
        {
            // Try extending the verb path
            let mut candidate = verb_parts.clone();
            candidate.push(part);
            let candidate_str: String = candidate.iter().copied().collect::<Vec<_>>().join("_");
            if crate::registry::find(&candidate_str).is_some() {
                verb_parts = candidate;
                continue;
            }
            // Also try as a CLI path
            if crate::registry::verb_for_cli(&candidate).is_some() {
                verb_parts = candidate;
                continue;
            }
        }
        args.push(part.to_string());
    }

    if verb_parts.is_empty() {
        return (parts[0].to_string(), parts[1..].iter().map(|s| s.to_string()).collect());
    }

    let verb = if let Some(v) = crate::registry::verb_for_cli(&verb_parts) {
        v.to_string()
    } else {
        verb_parts.join("_")
    };

    (verb, args)
}

fn is_arg(s: &str) -> bool {
    s.parse::<i64>().is_ok()
        || s == "true" || s == "false"
        || s.contains('/')
        || s.contains('=')
}

/// Complete a partial verb name using the registry.
#[allow(dead_code)]
pub fn complete(prefix: &str) -> Vec<&'static str> {
    crate::registry::complete(prefix)
}

/// Transform a verb name into its CLI command form.
#[allow(dead_code)]
pub fn verb_to_cli(verb: &str) -> String {
    crate::registry::derive_cli(verb)
}

/// Transform a CLI command path into a verb name (reverse derivation).
#[allow(dead_code)]
pub fn cli_to_verb(path: &[&str]) -> Option<&'static str> {
    crate::registry::verb_for_cli(path)
}

/// Fuzzy-match a partial input against all known verbs.
/// Returns completions sorted by relevance (exact prefix match first,
/// then substring match).
pub fn fuzzy_complete(input: &str) -> Vec<&'static str> {
    let input_lower = input.to_lowercase();
    let mut prefix_matches: Vec<&'static str> = Vec::new();
    let mut substring_matches: Vec<&'static str> = Vec::new();

    for a in crate::registry::ACTIONS {
        let verb = a.verb;
        if verb.starts_with(&input_lower) {
            prefix_matches.push(verb);
        } else if verb.contains(&input_lower) {
            substring_matches.push(verb);
        }
    }

    prefix_matches.extend(substring_matches);
    prefix_matches
}

/// Format a verb + args as a human-readable command string.
#[allow(dead_code)]
pub fn format_command(verb: &str, args: &[String]) -> String {
    if args.is_empty() {
        verb_to_cli(verb)
    } else {
        format!("{} {}", verb_to_cli(verb), args.join(" "))
    }
}

/// Get help text for a specific verb.
#[allow(dead_code)]
pub fn help_for(verb: &str) -> Option<String> {
    crate::registry::find(verb).map(|a| {
        let mut parts = vec![format!("{} ({})", a.verb, a.args)];
        if let Some(k) = a.key {
            parts.push(format!("key: '{}'", k));
        }
        if let Some(c) = a.ctx {
            parts.push(format!("context: {}", c));
        }
        if let Some(c) = a.cli {
            parts.push(format!("CLI: sarun {}", c.join(" ")));
        }
        parts.push(a.help.to_string());
        parts.join(" · ")
    })
}

/// All known representations of an action.
#[allow(dead_code)]
pub struct ActionRepr {
    pub verb: &'static str,
    pub cli: Option<String>,
    pub key: Option<char>,
    pub menu: Option<String>,
    pub help: &'static str,
}

/// Get all representations of an action (the "lens" view).
#[allow(dead_code)]
pub fn representations(verb: &str) -> Option<ActionRepr> {
    crate::registry::find(verb).map(|a| ActionRepr {
        verb: a.verb,
        cli: a.cli.map(|c| c.join(" ")),
        key: a.key,
        menu: a.menu.map(|m| m.to_string())
            .or_else(|| Some(crate::registry::derive_menu(a.verb))),
        help: a.help,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exact_verb() {
        let (verb, args) = parse_command("mirror_run 5");
        assert_eq!(verb, "mirror_run");
        assert_eq!(args, vec!["5"]);
    }

    #[test]
    fn parse_cli_path() {
        let (verb, args) = parse_command("mirror run 5");
        assert_eq!(verb, "mirror_run");
        assert_eq!(args, vec!["5"]);
    }

    #[test]
    fn parse_no_args() {
        let (verb, args) = parse_command("mirror_jobs");
        assert_eq!(verb, "mirror_jobs");
        assert!(args.is_empty());
    }

    #[test]
    fn parse_empty() {
        let (verb, _) = parse_command("");
        assert!(verb.is_empty());
    }

    #[test]
    fn verb_cli_roundtrip() {
        let cli = verb_to_cli("mirror_run");
        assert_eq!(cli, "mirror run");
        let verb = cli_to_verb(&["mirror", "run"]);
        assert!(verb.is_some());
    }

    #[test]
    fn fuzzy_complete_prefix() {
        let matches = fuzzy_complete("mirror");
        assert!(matches.contains(&"mirror_run"));
        assert!(matches.contains(&"mirror_jobs"));
        // Prefix matches come first
        assert_eq!(matches[0], "mirror_add"); // BTreeMap ordering in registry
    }

    #[test]
    fn fuzzy_complete_substring() {
        let matches = fuzzy_complete("run");
        assert!(matches.contains(&"mirror_run"));
        assert!(matches.contains(&"mirror_run_pending"));
    }

    #[test]
    fn format_command_with_args() {
        let s = format_command("mirror_run", &["5".to_string()]);
        assert_eq!(s, "mirror run 5");
    }

    #[test]
    fn format_command_no_args() {
        let s = format_command("mirror_jobs", &[]);
        assert_eq!(s, "mirror jobs");
    }

    #[test]
    fn help_for_known_verb() {
        let h = help_for("mirror_run").unwrap();
        assert!(h.contains("mirror_run"));
        assert!(h.contains("force-run"));
        assert!(h.contains("'r'"));
    }

    #[test]
    fn help_for_unknown_verb() {
        assert!(help_for("nonexistent").is_none());
    }

    #[test]
    fn representations_has_all_fields() {
        let r = representations("mirror_run").unwrap();
        assert_eq!(r.verb, "mirror_run");
        assert_eq!(r.cli.as_deref(), Some("mirror run"));
        assert_eq!(r.key, Some('r'));
        assert!(r.menu.is_some());
        assert!(!r.help.is_empty());
    }
}
