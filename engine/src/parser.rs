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

use std::collections::{BTreeMap, BTreeSet};

/// A fully resolved action with protocol-ready arguments.
#[derive(Debug)]
pub struct Invocation {
    pub action: &'static crate::registry::ActionSpec,
    pub target: crate::registry::ActionTarget,
    pub args: Vec<crate::registry::ArgValue>,
}

impl Invocation {
    pub fn dispatch_name(&self) -> &'static str {
        self.action.dispatch_name()
    }

    pub fn json_args(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.args
                .iter()
                .map(crate::registry::ArgValue::json)
                .collect(),
        )
    }
}

/// Structured parse outcome. Invalid and unknown input are not conflated with empty input.
#[derive(Debug)]
pub enum ParseResult {
    Invocation(Invocation),
    Empty,
    Unknown(String),
    InvalidArguments(String),
    BackendError(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendStatus {
    Disabled,
    Used,
    Unsupported,
    Error(String),
}

impl BackendStatus {
    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::Error(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendResult<T> {
    pub value: T,
    pub status: BackendStatus,
}

pub type ParsedAction = Invocation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionEntry {
    pub replace: TextSpan,
    pub insert: String,
    pub display: String,
    pub annotation: String,
    pub provider: String,
    pub preference: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Highlight {
    pub span: TextSpan,
    pub syntax: String,
    pub semantic: String,
    pub origin: String,
}

/// Parse already-tokenized input using longest CLI path, full schema, and end-of-input.
pub fn parse_words(parts: &[&str]) -> ParseResult {
    let Some((first, rest)) = parts.split_first() else {
        return ParseResult::Empty;
    };

    if let Some(action) = crate::registry::find(first) {
        let Some(mut args) = action.parse_args(rest) else {
            return ParseResult::InvalidArguments(parts.join(" "));
        };
        if action.verb == "mirror_resume" {
            args.push(crate::registry::ArgValue::Bool(false));
        }
        return ParseResult::Invocation(Invocation {
            action,
            target: action.target(),
            args,
        });
    }

    for path_len in (1..=parts.len()).rev() {
        let path = &parts[..path_len];
        if crate::registry::cli_candidates(path).is_empty() {
            continue;
        }
        return match crate::registry::resolve_cli(path, &parts[path_len..]) {
            Some(resolved) => ParseResult::Invocation(Invocation {
                action: resolved.action,
                target: resolved.action.target(),
                args: resolved.args,
            }),
            None => ParseResult::InvalidArguments(parts.join(" ")),
        };
    }

    ParseResult::Unknown(parts.join(" "))
}

/// Parse a command into a typed invocation. The Prolog backend gets first
/// refusal for its closed action grammar; all unsupported input uses the
/// registry parser, so enabling the feature never removes an action.
pub fn parse(input: &str) -> ParseResult {
    #[cfg(feature = "prolog")]
    {
        return finish_prolog_parse(input, parse_prolog(input));
    }
    #[cfg(not(feature = "prolog"))]
    parse_rust(input)
}

fn parse_rust(input: &str) -> ParseResult {
    let parts: Vec<&str> = input.split_whitespace().collect();
    parse_words(&parts)
}

#[cfg(feature = "prolog")]
enum BackendAttempt<T> {
    Value(T),
    Unsupported,
    Error(String),
}

#[cfg(feature = "prolog")]
fn finish_prolog_parse(input: &str, attempt: BackendAttempt<Invocation>) -> ParseResult {
    match attempt {
        BackendAttempt::Value(invocation) => ParseResult::Invocation(invocation),
        BackendAttempt::Unsupported => parse_rust(input),
        BackendAttempt::Error(error) => ParseResult::BackendError(error),
    }
}

#[cfg(feature = "prolog")]
fn parse_prolog(input: &str) -> BackendAttempt<Invocation> {
    let Some(grammar_input) = grammar_input(input, None) else {
        return BackendAttempt::Unsupported;
    };
    let prolog = match crate::prolog::global() {
        Ok(prolog) => prolog,
        Err(error) => return BackendAttempt::Error(error),
    };
    let candidates = match prolog.parse(&grammar_input, None) {
        Ok(candidates) => candidates,
        Err(error) => return BackendAttempt::Error(error),
    };
    let Some(candidate) = candidates
        .into_iter()
        .max_by_key(|candidate| candidate.preference)
    else {
        return BackendAttempt::Unsupported;
    };
    match invocation_from_prolog(candidate.command) {
        Ok(invocation) => BackendAttempt::Value(invocation),
        Err(error) => BackendAttempt::Error(error),
    }
}

#[cfg(feature = "prolog")]
fn invocation_from_prolog(command: crate::prolog::CommandAst) -> Result<Invocation, String> {
    use crate::prolog::{Action, CommandArg};
    let verb = match command.action {
        Action::MirrorJobs => "mirror_jobs",
        Action::MirrorRun => "mirror_run",
        Action::MirrorRunPending => "mirror_run_pending",
        Action::MirrorPause => "mirror_pause",
        Action::MirrorRemove => "mirror_rm",
    };
    let action = crate::registry::find(verb)
        .ok_or_else(|| format!("grammar returned unregistered action {verb}"))?;
    let mut args = command
        .args
        .into_iter()
        .map(|arg| match arg {
            CommandArg::JobId(value) => i64::try_from(value)
                .ok()
                .map(crate::registry::ArgValue::Number),
        })
        .collect::<Option<Vec<_>>>()
        .ok_or("grammar returned an oversized job id")?;
    if command.action == Action::MirrorPause {
        args.push(crate::registry::ArgValue::Bool(true));
    }
    Ok(Invocation {
        action,
        target: action.target(),
        args,
    })
}

#[cfg(feature = "prolog")]
fn grammar_input(input: &str, cursor: Option<usize>) -> Option<crate::prolog::GrammarInput> {
    use crate::prolog::{GrammarInput, InputItem, KnownUnit, Span};
    const MAX_BUFFER_BYTES: usize = 16 * 1024;
    if input.len() > MAX_BUFFER_BYTES {
        return None;
    }
    let cursor = cursor.map(|cursor| cursor.min(input.len()));
    if cursor.is_some_and(|cursor| !input.is_char_boundary(cursor)) {
        return None;
    }
    let words = word_spans(input);
    let torn_word = cursor.and_then(|cursor| {
        words
            .iter()
            .position(|(start, end)| *start <= cursor && cursor <= *end)
    });
    let gap_tear = cursor.filter(|_| torn_word.is_none());
    let mut items = Vec::with_capacity(words.len() + usize::from(cursor.is_some()));
    for (index, &(start, end)) in words.iter().enumerate() {
        if torn_word == Some(index) {
            let at = cursor.unwrap();
            items.push(InputItem::EditTear {
                id: "edit",
                span: Span { start, end },
                surface: input[start..at].to_string(),
            });
            continue;
        }
        if gap_tear.is_some_and(|at| at <= start)
            && !items
                .iter()
                .any(|item| matches!(item, InputItem::EditTear { .. }))
        {
            let at = gap_tear.unwrap();
            items.push(InputItem::EditTear {
                id: "edit",
                span: Span { start: at, end: at },
                surface: String::new(),
            });
        }
        let surface = &input[start..end];
        if let Some((semantic, syntax, provider, preference)) = grammar_unit(surface) {
            items.push(InputItem::Unit(KnownUnit {
                semantic,
                span: Span { start, end },
                paint_spans: vec![Span { start, end }],
                surface: surface.to_string(),
                syntax,
                provider,
                preference,
            }));
        } else {
            items.push(InputItem::SourceTear {
                id: index,
                span: Span { start, end },
                surface: surface.to_string(),
            });
        }
    }
    if let Some(at) = gap_tear {
        if !items
            .iter()
            .any(|item| matches!(item, InputItem::EditTear { .. }))
        {
            items.push(InputItem::EditTear {
                id: "edit",
                span: Span { start: at, end: at },
                surface: String::new(),
            });
        }
    }
    Some(GrammarInput {
        items,
        end: input.len(),
    })
}

#[cfg(feature = "prolog")]
fn grammar_unit(
    surface: &str,
) -> Option<(crate::prolog::Semantic, &'static str, &'static str, i64)> {
    use crate::prolog::Semantic;
    let literal = match surface {
        "mirror_jobs" => ("mirror_jobs", "action_identifier", "action_mirror_jobs", 30),
        "mirror_run" => ("mirror_run", "action_identifier", "action_mirror_run", 30),
        "mirror_run_pending" => (
            "mirror_run_pending",
            "action_identifier",
            "action_mirror_run_pending",
            30,
        ),
        "mirror_pause" => (
            "mirror_pause",
            "action_identifier",
            "action_mirror_pause",
            30,
        ),
        "mirror_rm" => ("mirror_rm", "action_identifier", "action_mirror_rm", 30),
        "mirror" => ("mirror", "command_namespace", "mirror_namespace", 10),
        "ls" => ("ls", "action_word", "action_mirror_jobs", 20),
        "run" => ("run", "action_word", "mirror_run_word", 20),
        "pause" => ("pause", "action_word", "action_mirror_pause", 20),
        "rm" => ("rm", "action_word", "action_mirror_rm", 20),
        _ => {
            let value = surface.parse::<u64>().ok()?;
            return Some((Semantic::Integer(value), "integer", "job_id", 10));
        }
    };
    Some((Semantic::Atom(literal.0), literal.1, literal.2, literal.3))
}

fn word_spans(input: &str) -> Vec<(usize, usize)> {
    let mut words = Vec::new();
    let mut start = None;
    for (offset, character) in input.char_indices() {
        if character.is_whitespace() {
            if let Some(begin) = start.take() {
                words.push((begin, offset));
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(begin) = start {
        words.push((begin, input.len()));
    }
    words
}

/// Compatibility wrapper for callers that only need successful invocations.
pub fn parse_action(input: &str) -> Option<ParsedAction> {
    match parse(input) {
        ParseResult::Invocation(invocation) => Some(invocation),
        _ => None,
    }
}

/// Compatibility wrapper returning the protocol dispatch name and arguments.
/// Callers that dispatch must use `parse` to honor the invocation target.
pub fn parse_command(input: &str) -> (String, Vec<String>) {
    match parse(input) {
        ParseResult::Invocation(invocation) => (
            invocation.dispatch_name().to_string(),
            invocation
                .args
                .iter()
                .flat_map(crate::registry::ArgValue::source_tokens)
                .collect(),
        ),
        _ => (String::new(), Vec::new()),
    }
}

/// Complete a partial verb name using the registry.
#[allow(dead_code)]
pub fn complete(prefix: &str) -> Vec<&'static str> {
    crate::registry::complete(prefix)
}

/// Structured completion for an edit tear at `cursor`. The replacement span
/// covers the whole identifier even when the cursor is in its middle.
pub fn complete_at(input: &str, cursor: usize) -> BackendResult<Vec<CompletionEntry>> {
    let cursor = floor_char_boundary(input, cursor.min(input.len()));
    let entries = rust_completions(input, cursor);
    #[cfg(feature = "prolog")]
    let mut entries = entries;
    let status = {
        #[cfg(feature = "prolog")]
        {
            match prolog_completions(input, cursor) {
                BackendAttempt::Value(prolog_entries) => {
                    entries.extend(prolog_entries);
                    BackendStatus::Used
                }
                BackendAttempt::Unsupported => BackendStatus::Unsupported,
                BackendAttempt::Error(error) => BackendStatus::Error(error),
            }
        }
        #[cfg(not(feature = "prolog"))]
        {
            BackendStatus::Disabled
        }
    };
    BackendResult {
        value: merge_completion_entries(entries),
        status,
    }
}

#[cfg(feature = "prolog")]
fn prolog_completions(input: &str, cursor: usize) -> BackendAttempt<Vec<CompletionEntry>> {
    let Some(grammar_input) = grammar_input(input, Some(cursor)) else {
        return BackendAttempt::Unsupported;
    };
    let prolog = match crate::prolog::global() {
        Ok(prolog) => prolog,
        Err(error) => return BackendAttempt::Error(error),
    };
    let completions = match prolog.complete(&grammar_input, "edit") {
        Ok(completions) => completions,
        Err(error) => return BackendAttempt::Error(error),
    };
    BackendAttempt::Value(
        completions
            .into_iter()
            .map(|completion| {
                let provider = completion
                    .alternatives
                    .iter()
                    .map(|alternative| alternative.provider.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let annotation = completion
                    .alternatives
                    .iter()
                    .map(|alternative| alternative.semantic.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ");
                CompletionEntry {
                    replace: TextSpan {
                        start: completion.replace.start,
                        end: completion.replace.end,
                    },
                    insert: completion.insert,
                    display: completion.display,
                    annotation,
                    provider,
                    preference: completion.preference,
                }
            })
            .collect(),
    )
}

fn merge_completion_entries(entries: Vec<CompletionEntry>) -> Vec<CompletionEntry> {
    let mut grouped = BTreeMap::new();
    for entry in entries {
        let key = (entry.replace.start, entry.replace.end, entry.insert.clone());
        grouped
            .entry(key)
            .and_modify(|current: &mut CompletionEntry| {
                current.preference = current.preference.max(entry.preference);
                if entry.display < current.display {
                    current.display = entry.display.clone();
                }
                current.annotation = merge_labels(&current.annotation, &entry.annotation, " | ");
                current.provider = merge_labels(&current.provider, &entry.provider, ", ");
            })
            .or_insert(entry);
    }
    let mut merged: Vec<_> = grouped.into_values().collect();
    merged.sort_by(|left, right| {
        right
            .preference
            .cmp(&left.preference)
            .then_with(|| left.display.cmp(&right.display))
            .then_with(|| left.replace.start.cmp(&right.replace.start))
            .then_with(|| left.replace.end.cmp(&right.replace.end))
            .then_with(|| left.insert.cmp(&right.insert))
    });
    merged
}

fn merge_labels(left: &str, right: &str, separator: &str) -> String {
    let mut labels = BTreeSet::new();
    labels.extend(left.split(separator).filter(|label| !label.is_empty()));
    labels.extend(right.split(separator).filter(|label| !label.is_empty()));
    labels.into_iter().collect::<Vec<_>>().join(separator)
}

pub fn apply_completion(input: &str, completion: &CompletionEntry) -> String {
    if completion.replace.start > completion.replace.end
        || completion.replace.end > input.len()
        || !input.is_char_boundary(completion.replace.start)
        || !input.is_char_boundary(completion.replace.end)
    {
        return input.to_string();
    }
    format!(
        "{}{}{}",
        &input[..completion.replace.start],
        completion.insert,
        &input[completion.replace.end..]
    )
}

fn rust_completions(input: &str, cursor: usize) -> Vec<CompletionEntry> {
    let words = word_spans(input);
    let torn = words
        .iter()
        .position(|(start, end)| *start <= cursor && cursor <= *end);
    let (replace, prefix, prior) = if let Some(index) = torn {
        let (start, end) = words[index];
        (
            TextSpan { start, end },
            &input[start..cursor],
            words[..index]
                .iter()
                .map(|(start, end)| &input[*start..*end])
                .collect::<Vec<_>>(),
        )
    } else {
        (
            TextSpan {
                start: cursor,
                end: cursor,
            },
            "",
            words
                .iter()
                .take_while(|(_, end)| *end <= cursor)
                .map(|(start, end)| &input[*start..*end])
                .collect::<Vec<_>>(),
        )
    };
    let mut entries = Vec::new();
    for action in crate::registry::actions().filter(|action| action.hidden_reason().is_none()) {
        if prior.is_empty() && action.verb.starts_with(prefix) {
            entries.push(registry_completion(action, replace, action.verb));
        }
        if let Some(path) = action.cli {
            let index = prior.len();
            if index < path.len() && path[..index] == prior[..] && path[index].starts_with(prefix) {
                entries.push(registry_completion(action, replace, path[index]));
            }
        }
    }
    merge_completion_entries(entries)
}

fn registry_completion(
    action: &'static crate::registry::ActionSpec,
    replace: TextSpan,
    insert: &str,
) -> CompletionEntry {
    CompletionEntry {
        replace,
        insert: insert.to_string(),
        display: insert.to_string(),
        annotation: action.help.to_string(),
        provider: format!("registry:{}", action.verb),
        preference: 0,
    }
}

pub fn highlights(input: &str) -> BackendResult<Vec<Highlight>> {
    let fallback = || rust_highlights(input);
    #[cfg(feature = "prolog")]
    {
        return match prolog_highlights(input) {
            BackendAttempt::Value(highlights) => BackendResult {
                value: highlights,
                status: BackendStatus::Used,
            },
            BackendAttempt::Unsupported => BackendResult {
                value: fallback(),
                status: BackendStatus::Unsupported,
            },
            BackendAttempt::Error(error) => BackendResult {
                value: fallback(),
                status: BackendStatus::Error(error),
            },
        };
    }
    #[cfg(not(feature = "prolog"))]
    BackendResult {
        value: fallback(),
        status: BackendStatus::Disabled,
    }
}

#[cfg(feature = "prolog")]
fn prolog_highlights(input: &str) -> BackendAttempt<Vec<Highlight>> {
    let Some(grammar_input) = grammar_input(input, None) else {
        return BackendAttempt::Unsupported;
    };
    let prolog = match crate::prolog::global() {
        Ok(prolog) => prolog,
        Err(error) => return BackendAttempt::Error(error),
    };
    let results = match prolog.parse(&grammar_input, None) {
        Ok(results) => results,
        Err(error) => return BackendAttempt::Error(error),
    };
    let Some(candidate) = results.into_iter().max_by_key(|result| result.preference) else {
        return BackendAttempt::Unsupported;
    };
    let highlights = match prolog.highlights(&candidate) {
        Ok(highlights) => highlights,
        Err(error) => return BackendAttempt::Error(error),
    };
    BackendAttempt::Value(
        highlights
            .into_iter()
            .map(|highlight| Highlight {
                span: TextSpan {
                    start: highlight.span.start,
                    end: highlight.span.end,
                },
                syntax: highlight.syntax,
                semantic: highlight.semantic,
                origin: highlight.origin,
            })
            .collect(),
    )
}

fn rust_highlights(input: &str) -> Vec<Highlight> {
    if !matches!(parse_rust(input), ParseResult::Invocation(_)) {
        return Vec::new();
    }
    word_spans(input)
        .into_iter()
        .enumerate()
        .map(|(index, (start, end))| {
            let surface = &input[start..end];
            Highlight {
                span: TextSpan { start, end },
                syntax: if surface.parse::<i64>().is_ok() {
                    "integer"
                } else if index == 0 && surface.contains('_') {
                    "action_identifier"
                } else {
                    "action_word"
                }
                .into(),
                semantic: surface.to_string(),
                origin: "registry".into(),
            }
        })
        .collect()
}

pub fn render(invocation: &Invocation) -> BackendResult<String> {
    let fallback = || format_command(invocation.action.verb, &invocation.args);
    #[cfg(feature = "prolog")]
    {
        return match prolog_render(invocation) {
            BackendAttempt::Value(rendered) => BackendResult {
                value: rendered,
                status: BackendStatus::Used,
            },
            BackendAttempt::Unsupported => BackendResult {
                value: fallback(),
                status: BackendStatus::Unsupported,
            },
            BackendAttempt::Error(error) => BackendResult {
                value: fallback(),
                status: BackendStatus::Error(error),
            },
        };
    }
    #[cfg(not(feature = "prolog"))]
    BackendResult {
        value: fallback(),
        status: BackendStatus::Disabled,
    }
}

#[cfg(feature = "prolog")]
fn prolog_render(invocation: &Invocation) -> BackendAttempt<String> {
    let Some(command) = prolog_command(invocation) else {
        return BackendAttempt::Unsupported;
    };
    let prolog = match crate::prolog::global() {
        Ok(prolog) => prolog,
        Err(error) => return BackendAttempt::Error(error),
    };
    match prolog.render(&command, crate::prolog::RenderForm::Cli) {
        Ok(rendered) => BackendAttempt::Value(rendered.text),
        Err(crate::prolog::QueryError::NoSolution) => BackendAttempt::Unsupported,
        Err(crate::prolog::QueryError::Backend(error)) => BackendAttempt::Error(error),
    }
}

#[cfg(feature = "prolog")]
fn prolog_command(invocation: &Invocation) -> Option<crate::prolog::CommandAst> {
    use crate::prolog::{Action, CommandArg, CommandAst};
    let (action, id) = match invocation.action.verb {
        "mirror_jobs" => (Action::MirrorJobs, None),
        "mirror_run_pending" => (Action::MirrorRunPending, None),
        "mirror_run" => (
            Action::MirrorRun,
            Some(match invocation.args.first()? {
                crate::registry::ArgValue::Number(value) => u64::try_from(*value).ok()?,
                _ => return None,
            }),
        ),
        "mirror_rm" => (
            Action::MirrorRemove,
            Some(match invocation.args.first()? {
                crate::registry::ArgValue::Number(value) => u64::try_from(*value).ok()?,
                _ => return None,
            }),
        ),
        "mirror_pause"
            if invocation.args.get(1) != Some(&crate::registry::ArgValue::Bool(false)) =>
        {
            (
                Action::MirrorPause,
                Some(match invocation.args.first()? {
                    crate::registry::ArgValue::Number(value) => u64::try_from(*value).ok()?,
                    _ => return None,
                }),
            )
        }
        _ => return None,
    };
    Some(CommandAst {
        action,
        args: id
            .map(|value| vec![CommandArg::JobId(value)])
            .unwrap_or_default(),
    })
}

fn floor_char_boundary(input: &str, mut cursor: usize) -> usize {
    while cursor > 0 && !input.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Transform an action identity into its registered CLI form.
#[allow(dead_code)]
pub fn verb_to_cli(verb: &str) -> String {
    crate::registry::find(verb)
        .and_then(|action| action.cli)
        .map(|path| path.join(" "))
        .unwrap_or_else(|| verb.to_string())
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

    for a in crate::registry::actions().filter(|action| action.hidden_reason().is_none()) {
        let verb = a.verb;
        if verb.starts_with(&input_lower) {
            prefix_matches.push(verb);
        } else if verb.contains(&input_lower) {
            substring_matches.push(verb);
        }
    }

    prefix_matches.sort_unstable();
    substring_matches.sort_unstable();
    prefix_matches.extend(substring_matches);
    prefix_matches
}

/// Format a protocol verb + args using a registered CLI form when available.
pub fn format_command(verb: &str, args: &[crate::registry::ArgValue]) -> String {
    let action = if verb == "mirror_pause" {
        match args.last() {
            Some(crate::registry::ArgValue::Bool(false)) => crate::registry::find("mirror_resume"),
            _ => crate::registry::find("mirror_pause"),
        }
    } else {
        crate::registry::find(verb)
    };
    let Some(action) = action else {
        return if args.is_empty() {
            verb.to_string()
        } else {
            format!(
                "{verb} {}",
                args.iter()
                    .flat_map(crate::registry::ArgValue::source_tokens)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
    };
    let command = action
        .cli
        .map(|path| path.join(" "))
        .unwrap_or_else(|| action.verb.to_string());
    let rendered_args = if matches!(action.verb, "mirror_pause" | "mirror_resume")
        && matches!(args.last(), Some(crate::registry::ArgValue::Bool(_)))
    {
        &args[..args.len() - 1]
    } else {
        args
    };
    if rendered_args.is_empty() {
        command
    } else {
        format!(
            "{command} {}",
            rendered_args
                .iter()
                .flat_map(crate::registry::ArgValue::source_tokens)
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

pub fn format_invocation(invocation: &Invocation) -> String {
    format_command(invocation.action.verb, &invocation.args)
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
    pub target: crate::registry::ActionTarget,
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
        target: a.target(),
        cli: a.cli.map(|c| c.join(" ")),
        key: a.key,
        menu: a
            .menu
            .map(|m| m.to_string())
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
    fn parse_shared_cli_path_by_complete_schema() {
        let (verb, args) = parse_command("mirror run 5");
        assert_eq!(verb, "mirror_run");
        assert_eq!(args, vec!["5"]);

        let (verb, args) = parse_command("mirror run");
        assert_eq!(verb, "mirror_run_pending");
        assert!(args.is_empty());

        assert_eq!(parse_command("mirror run 5 extra").0, "");
    }

    #[test]
    fn typed_parse_preserves_local_and_control_targets() {
        let ParseResult::Invocation(local) = parse("quit") else {
            panic!("not parsed")
        };
        assert_eq!(local.target, crate::registry::ActionTarget::LocalUi);
        assert_eq!(parse_command("quit").0, "quit");

        for verb in ["apply", "discard"] {
            let input = format!("{verb} 5");
            let ParseResult::Invocation(invocation) = parse(&input) else {
                panic!("not parsed")
            };
            assert_eq!(
                invocation.target,
                crate::registry::ActionTarget::ControlMessage
            );
        }
        let ParseResult::Invocation(rename) = parse("rename 5 NEW") else {
            panic!("rename did not parse")
        };
        assert_eq!(
            rename.target,
            crate::registry::ActionTarget::ControlMessage
        );
        assert!(matches!(
            parse("rename 5"),
            ParseResult::InvalidArguments(_)
        ));

        let ParseResult::Invocation(ui) = parse("mirror_jobs") else {
            panic!("not parsed")
        };
        assert_eq!(ui.target, crate::registry::ActionTarget::UiVerb);
    }

    #[test]
    fn pause_and_resume_inject_wire_bool() {
        let pause = parse_action("mirror pause 5").unwrap();
        assert_eq!(pause.dispatch_name(), "mirror_pause");
        assert_eq!(
            pause.args,
            vec![
                crate::registry::ArgValue::Number(5),
                crate::registry::ArgValue::Bool(true)
            ]
        );
        let resume = parse_action("mirror resume 5").unwrap();
        assert_eq!(resume.dispatch_name(), "mirror_pause");
        assert_eq!(
            resume.args,
            vec![
                crate::registry::ArgValue::Number(5),
                crate::registry::ArgValue::Bool(false)
            ]
        );
        assert!(matches!(
            parse("mirror pause 5 true"),
            ParseResult::InvalidArguments(_)
        ));
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
    fn cli_to_verb_requires_an_unambiguous_path() {
        assert_eq!(verb_to_cli("mirror_run"), "mirror run");
        assert_eq!(cli_to_verb(&["mirror", "run"]), None);
        assert_eq!(cli_to_verb(&["mirror", "ls"]), Some("mirror_jobs"));
    }

    #[test]
    fn fuzzy_complete_prefix_is_lexically_ordered() {
        assert_eq!(
            fuzzy_complete("mirror"),
            vec![
                "mirror_add",
                "mirror_browse",
                "mirror_jobs",
                "mirror_pause",
                "mirror_read",
                "mirror_resume",
                "mirror_rm",
                "mirror_run",
                "mirror_run_pending",
            ],
        );
    }

    #[test]
    fn fuzzy_complete_substring() {
        let matches = fuzzy_complete("run");
        assert!(matches.contains(&"mirror_run"));
        assert!(matches.contains(&"mirror_run_pending"));
    }

    #[test]
    fn format_command_with_args() {
        let s = format_command("mirror_run", &[crate::registry::ArgValue::Number(5)]);
        assert_eq!(s, "mirror run 5");
    }

    #[test]
    fn registered_cli_rendering_roundtrips() {
        assert_eq!(verb_to_cli("mirror_jobs"), "mirror ls");
        assert_eq!(format_command("mirror_jobs", &[]), "mirror ls");
        assert_eq!(format_command("mirror_run_pending", &[]), "mirror run");
        assert_eq!(
            format_command(
                "mirror_pause",
                &[
                    crate::registry::ArgValue::Number(5),
                    crate::registry::ArgValue::Bool(true)
                ]
            ),
            "mirror pause 5"
        );
        assert_eq!(
            format_command(
                "mirror_pause",
                &[
                    crate::registry::ArgValue::Number(5),
                    crate::registry::ArgValue::Bool(false)
                ]
            ),
            "mirror resume 5"
        );

        for input in [
            "mirror run",
            "mirror run 5",
            "mirror pause 5",
            "mirror resume 5",
            "review.apply 12 one two",
            "review.map_ids 12 process edge",
            "ro_attach 12 2 3",
        ] {
            let invocation = parse_action(input).unwrap();
            let rendered = format_invocation(&invocation);
            assert_eq!(rendered, input);
            let reparsed = parse_action(&rendered).unwrap();
            assert_eq!(reparsed.dispatch_name(), invocation.dispatch_name());
            assert_eq!(reparsed.args, invocation.args);
            assert_eq!(reparsed.json_args(), invocation.json_args());
        }
    }

    #[test]
    fn dynamic_and_hidden_control_verbs_parse_exactly() {
        let api_log = parse_action("api_log 5").unwrap();
        assert_eq!(api_log.target, crate::registry::ActionTarget::UiVerb);
        let open_files = parse_action("open_files").unwrap();
        assert!(open_files.action.hidden_reason().is_some());
        assert!(!fuzzy_complete("").contains(&"open_files"));
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
    fn typed_source_path_and_integer_serialize_without_reinference() {
        let invocation = parse_action("mirror add git 123 /tmp 7").unwrap();
        assert_eq!(
            invocation.args,
            vec![
                crate::registry::ArgValue::String("git".into()),
                crate::registry::ArgValue::String("123".into()),
                crate::registry::ArgValue::String("/tmp".into()),
                crate::registry::ArgValue::Number(7),
            ]
        );
        assert_eq!(
            invocation.json_args(),
            serde_json::json!(["git", "123", "/tmp", 7])
        );
    }

    #[test]
    fn derived_ui_schemas_serialize_numbers_and_bools() {
        for (input, expected) in [
            ("api_log 12", serde_json::json!(["12"])),
            ("flows.detail 12 34", serde_json::json!(["12", 34])),
            ("flows.detail 34", serde_json::json!([34])),
            ("flows.packets 12 34", serde_json::json!(["12", 34])),
            ("flows.packets 34", serde_json::json!([34])),
            ("view.window 7 8 9", serde_json::json!([7, 8, 9])),
            ("prompts.ui_active true", serde_json::json!([true])),
            (
                "view.open changes 12 123 true",
                serde_json::json!(["changes", "12", "123", true]),
            ),
            (
                "review.makevars 12 123 456 10 false",
                serde_json::json!(["12", "123", "456", 10, false]),
            ),
        ] {
            assert_eq!(
                parse_action(input).unwrap().json_args(),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn derived_ui_schemas_handle_optional_and_variadic_args() {
        for (input, expected) in [
            ("flows.list", serde_json::json!([])),
            ("flows.list 12", serde_json::json!(["12"])),
            ("box_new", serde_json::json!([])),
            ("box_new 12", serde_json::json!(["12"])),
            (
                "review.decorate_many 12 one 2 three",
                serde_json::json!(["12", ["one", "2", "three"]]),
            ),
            (
                "review.map_ids 12 process 2 3 edge",
                serde_json::json!(["12", "process", [2, 3], "edge"]),
            ),
            (
                "review.map_ids 12 process edge",
                serde_json::json!(["12", "process", [], "edge"]),
            ),
            ("review.apply 12", serde_json::json!(["12"])),
            (
                "review.apply 12 one two",
                serde_json::json!(["12", ["one", "two"]]),
            ),
            (
                "review.discard 12 one two",
                serde_json::json!(["12", ["one", "two"]]),
            ),
            ("ro_attach 12 2 3", serde_json::json!(["12", 2, 3])),
        ] {
            assert_eq!(
                parse_action(input).unwrap().json_args(),
                expected,
                "{input}"
            );
        }
        assert!(matches!(
            parse("prompts.ui_active 1"),
            ParseResult::InvalidArguments(_)
        ));
    }

    #[test]
    fn numeric_looking_string_path_base64_and_spec_args_stay_strings() {
        for (input, expected) in [
            ("box_file_read 123 456", serde_json::json!(["123", "456"])),
            (
                "box_file_write 123 456 789",
                serde_json::json!(["123", "456", "789"]),
            ),
            ("box_dir_list 123 456", serde_json::json!(["123", "456"])),
            (
                "review.write_file 12 345 678",
                serde_json::json!(["12", "345", "678"]),
            ),
            ("oaita.probe 123", serde_json::json!(["123"])),
        ] {
            assert_eq!(
                parse_action(input).unwrap().json_args(),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn completion_dedup_groups_before_ranking_and_merges_metadata() {
        let entry = |insert: &str, preference, annotation: &str, provider: &str| CompletionEntry {
            replace: TextSpan { start: 0, end: 1 },
            insert: insert.into(),
            display: insert.into(),
            annotation: annotation.into(),
            provider: provider.into(),
            preference,
        };
        let merged = merge_completion_entries(vec![
            entry("same", 100, "zeta", "provider-z"),
            entry("other", 90, "other", "provider-o"),
            entry("same", 80, "alpha", "provider-a"),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].insert, "same");
        assert_eq!(merged[0].preference, 100);
        assert_eq!(merged[0].annotation, "alpha | zeta");
        assert_eq!(merged[0].provider, "provider-a, provider-z");
    }

    #[cfg(feature = "prolog")]
    #[test]
    fn prolog_and_registry_completion_duplicates_merge() {
        let result = complete_at("mirror_j", 8);
        assert_eq!(result.status, BackendStatus::Used);
        let jobs: Vec<_> = result
            .value
            .iter()
            .filter(|entry| entry.insert == "mirror_jobs")
            .collect();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].provider.contains("action_mirror_jobs"));
        assert!(jobs[0].provider.contains("registry:mirror_jobs"));
        assert!(jobs[0].annotation.contains("mirror_jobs"));
        assert!(
            jobs[0]
                .annotation
                .contains("list scheduled mirror-update jobs")
        );
    }

    #[cfg(feature = "prolog")]
    #[test]
    fn rename_uses_registry_fallback_in_prolog_mode() {
        let ParseResult::Invocation(invocation) = parse("rename 5 NEW") else {
            panic!("rename did not fall back to the registry parser")
        };
        assert_eq!(invocation.action.verb, "rename");
        assert_eq!(invocation.json_args(), serde_json::json!(["5", "NEW"]));
        let rendered = render(&invocation);
        assert_eq!(rendered.status, BackendStatus::Unsupported);
        assert_eq!(rendered.value, "rename 5 NEW");
        assert!(matches!(
            parse("rename 5"),
            ParseResult::InvalidArguments(_)
        ));
    }

    #[cfg(feature = "prolog")]
    #[test]
    fn mirror_ls_has_prolog_parse_and_render_parity() {
        let ParseResult::Invocation(invocation) = parse("mirror ls") else {
            panic!("mirror ls did not parse through the grammar")
        };
        assert_eq!(invocation.action.verb, "mirror_jobs");
        let rendered = render(&invocation);
        assert_eq!(rendered.status, BackendStatus::Used);
        assert_eq!(rendered.value, "mirror ls");
    }

    #[cfg(feature = "prolog")]
    #[test]
    fn backend_error_is_not_treated_as_unsupported() {
        assert!(matches!(
            finish_prolog_parse("mirror ls", BackendAttempt::Error("injected query failure".into())),
            ParseResult::BackendError(error) if error == "injected query failure"
        ));
        assert!(matches!(
            finish_prolog_parse("mirror add git 123 /tmp", BackendAttempt::Unsupported),
            ParseResult::Invocation(_)
        ));
    }

    #[test]
    fn representations_has_all_fields() {
        let r = representations("mirror_run").unwrap();
        assert_eq!(r.verb, "mirror_run");
        assert_eq!(r.target, crate::registry::ActionTarget::UiVerb);
        assert_eq!(r.cli.as_deref(), Some("mirror run"));
        assert_eq!(r.key, Some('r'));
        assert!(r.menu.is_some());
        assert!(!r.help.is_empty());
    }
}
