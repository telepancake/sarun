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
//! The mandatory embedded SWI-Prolog relation is the semantic hub. Rust
//! supplies byte-spanned neutral source units and receives typed values; it
//! does not maintain a second action catalog or parser.
//!
//! The current complete UI-action grammar is the first client of the generic
//! relation. Later clients compose nested grammars such as HTTP in a packet:
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
//! N-way lens composition relates each supported representation through one
//! semantic identity:
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
//! Parsing, rendering, completion, highlighting, catalog projection, and
//! conversion are different operations over that same relation.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionTarget {
    UiVerb,
    ControlMessage,
    LocalUi,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgValue {
    Number(i64),
    Bool(bool),
    String(String),
    Array(Vec<ArgValue>),
}

impl ArgValue {
    pub fn json(&self) -> serde_json::Value {
        match self {
            Self::Number(value) => serde_json::Value::Number((*value).into()),
            Self::Bool(value) => serde_json::Value::Bool(*value),
            Self::String(value) => serde_json::Value::String(value.clone()),
            Self::Array(values) => {
                serde_json::Value::Array(values.iter().map(Self::json).collect())
            }
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

/// A fully resolved action with protocol-ready arguments.
#[derive(Debug)]
pub struct Invocation {
    pub action: String,
    pub handler: String,
    pub target: ActionTarget,
    pub args: Vec<ArgValue>,
    /// Exact external observations used to resolve this invocation.
    pub context: Vec<crate::prolog::ContextObservation>,
}

impl Invocation {
    pub fn dispatch_name(&self) -> &str {
        &self.handler
    }

    pub fn json_args(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.args
                .iter()
                .map(ArgValue::json)
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
    BackendError(String),
}

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

/// Supplies a snapshot for an explicit query emitted by the relation.
///
/// Implementations route semantic domains to engine state, sockets, or other
/// stores. They do not decide which source position needs which domain or
/// cardinality: the complete typed request is supplied by Prolog. The pure
/// relation evaluates that request against the returned snapshot.
pub trait ContextProvider {
    fn snapshot(
        &self,
        request: &crate::prolog::ContextQueryNode,
    ) -> Result<crate::prolog::ContextSnapshot, String>;
}

/// A real, empty external context for callers whose grammar has no live
/// object store (for example, context-free CLI forms and parser unit tests).
pub struct EmptyContext;

impl ContextProvider for EmptyContext {
    fn snapshot(
        &self,
        _request: &crate::prolog::ContextQueryNode,
    ) -> Result<crate::prolog::ContextSnapshot, String> {
        Ok(crate::prolog::ContextSnapshot {
            provider: crate::prolog::RelationValue::Atom("empty_context".into()),
            revision: crate::prolog::RelationValue::Integer(0),
            entries: Vec::new(),
        })
    }
}

/// Parse neutral source words through the same mandatory relation as text input.
pub fn parse_words(parts: &[&str], context: &dyn ContextProvider) -> ParseResult {
    parse(&parts.join(" "), context)
}

/// Parse a command into a typed, wire-ready invocation through the mandatory
/// Prolog relation. There is no alternate parser.
pub fn parse(input: &str, context: &dyn ContextProvider) -> ParseResult {
    if input.trim().is_empty() {
        return ParseResult::Empty;
    }
    let Some(grammar_input) = grammar_input(input, None) else {
        return ParseResult::BackendError("command input exceeds parser limits".into());
    };
    let prolog = match crate::prolog::global() {
        Ok(prolog) => prolog,
        Err(error) => return ParseResult::BackendError(error),
    };
    let resolved = match resolve_best_plan(prolog, &grammar_input, context) {
        Ok(resolved) => resolved,
        Err(error) => return ParseResult::BackendError(error),
    };
    let Some(resolved) = resolved else {
        return ParseResult::Unknown(input.to_string());
    };
    match invocation_from_prolog(resolved.command, resolved.observations) {
        Ok(invocation) => ParseResult::Invocation(invocation),
        Err(error) => ParseResult::BackendError(error),
    }
}

struct ResolvedPlan {
    command: crate::prolog::CommandAst,
    evidence: Vec<crate::prolog::Evidence>,
    preference: i64,
    observations: Vec<crate::prolog::ContextObservation>,
}

fn resolve_best_plan(
    prolog: &crate::prolog::Prolog,
    input: &crate::prolog::GrammarInput,
    context: &dyn ContextProvider,
) -> Result<Option<ResolvedPlan>, String> {
    let mut plans = prolog.context_plans(input, None)?;
    plans.sort_by_key(|plan| std::cmp::Reverse(plan.preference));
    let mut provider_error = None;
    for plan in plans {
        match execute_context_plan(prolog, &plan, context) {
            Ok(Some((command, observations))) => {
                return Ok(Some(ResolvedPlan {
                    command,
                    evidence: plan.evidence,
                    preference: plan.preference,
                    observations,
                }));
            }
            Ok(None) => {}
            Err(error) => {
                provider_error.get_or_insert(error);
            }
        };
    }
    match provider_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

fn execute_context_plan(
    prolog: &crate::prolog::Prolog,
    plan: &crate::prolog::ContextPlan,
    context: &dyn ContextProvider,
) -> Result<Option<(crate::prolog::CommandAst, Vec<crate::prolog::ContextObservation>)>, String> {
    let Some(observations) = execute_context_graph(prolog, &plan.queries, context)? else {
        return Ok(None);
    };
    match prolog.resolve_context_plan(plan, &observations) {
        Ok(command) => Ok(Some((command, observations))),
        Err(crate::prolog::QueryError::NoSolution) => Ok(None),
        Err(crate::prolog::QueryError::Backend(error)) => Err(error),
    }
}

fn execute_context_graph(
    prolog: &crate::prolog::Prolog,
    graph: &[crate::prolog::ContextQueryNode],
    context: &dyn ContextProvider,
) -> Result<Option<Vec<crate::prolog::ContextObservation>>, String> {
    let mut observations = Vec::with_capacity(graph.len());
    while observations.len() < graph.len() {
        let ready = prolog.ready_context_queries(graph, &observations)?;
        if ready.is_empty() {
            return Ok(None);
        }
        for resolved in ready {
            let Some(original) = graph.iter().find(|node| node.id == resolved.id) else {
                return Err(format!(
                    "context relation returned unknown ready query {}",
                    resolved.id
                ));
            };
            let snapshot = context.snapshot(&resolved)?;
            let outcome = prolog.context_query(&resolved.query, &snapshot)?;
            observations.push(crate::prolog::ContextObservation {
                id: original.id.clone(),
                query: original.query.clone(),
                provider: snapshot.provider,
                revision: snapshot.revision,
                outcome,
            });
        }
    }
    Ok(Some(observations))
}

fn invocation_from_prolog(
    command: crate::prolog::CommandAst,
    context: Vec<crate::prolog::ContextObservation>,
) -> Result<Invocation, String> {
    use crate::prolog::CommandValue;
    fn convert_value(value: CommandValue) -> ArgValue {
        match value {
            CommandValue::Integer(value) => ArgValue::Number(value),
            CommandValue::Boolean(value) => ArgValue::Bool(value),
            CommandValue::String(value) => ArgValue::String(value),
            CommandValue::Array(values) => {
                ArgValue::Array(values.into_iter().map(convert_value).collect())
            }
        }
    }
    let target = match command.target.as_str() {
        "ui" => ActionTarget::UiVerb,
        "control" => ActionTarget::ControlMessage,
        "local" => ActionTarget::LocalUi,
        other => return Err(format!("grammar returned invalid action target {other}")),
    };
    let args = command
        .args
        .into_iter()
        .map(convert_value)
        .collect();
    Ok(Invocation {
        action: command.action,
        handler: command.handler,
        target,
        args,
        context,
    })
}

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
        let surface = input[start..end].to_string();
        items.push(InputItem::Unit(KnownUnit {
            semantic: crate::prolog::Semantic::Text(surface.clone()),
            span: Span { start, end },
            paint_spans: vec![Span { start, end }],
            surface,
            syntax: "source".into(),
            provider: "command_source".into(),
            preference: 0,
        }));
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

/// Structured completion for an edit tear at `cursor`. The replacement span
/// covers the whole identifier even when the cursor is in its middle.
pub fn complete_at(
    input: &str,
    cursor: usize,
    context: &dyn ContextProvider,
) -> Result<Vec<CompletionEntry>, String> {
    let cursor = floor_char_boundary(input, cursor.min(input.len()));
    let Some(grammar_input) = grammar_input(input, Some(cursor)) else {
        return Err("command input exceeds parser limits".into());
    };
    let prolog = crate::prolog::global()?;
    let mut completions = prolog.complete(&grammar_input, "edit")?;
    for plan in prolog.context_completion_plans(&grammar_input, "edit")? {
        let Some(observations) = execute_context_graph(prolog, &plan.queries, context)? else {
            continue;
        };
        completions.extend(prolog.resolve_context_completion(&plan, &observations)?);
    }
    Ok(merge_completion_entries(completion_entries(completions)))
}

fn completion_entries(completions: Vec<crate::prolog::Completion>) -> Vec<CompletionEntry> {
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
        .collect()
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

pub fn highlights(input: &str, context: &dyn ContextProvider) -> Result<Vec<Highlight>, String> {
    let Some(grammar_input) = grammar_input(input, None) else {
        return Err("command input exceeds parser limits".into());
    };
    let prolog = crate::prolog::global()?;
    let Some(resolved) = resolve_best_plan(prolog, &grammar_input, context)? else {
        return Ok(Vec::new());
    };
    let candidate = crate::prolog::ParseCandidate {
        command: resolved.command,
        status: crate::prolog::ParseStatus::Complete,
        evidence: resolved.evidence,
        preference: resolved.preference,
    };
    let highlights = prolog.highlights(&candidate)?;
    Ok(
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
pub fn render(invocation: &Invocation) -> Result<String, String> {
    let command = prolog_command(invocation);
    let prolog = crate::prolog::global()?;
    prolog
        .render(&command, crate::prolog::RenderForm::Cli)
        .map(|rendered| rendered.text)
        .map_err(|error| error.to_string())
}

fn prolog_command(invocation: &Invocation) -> crate::prolog::CommandAst {
    fn convert_value(value: &ArgValue) -> crate::prolog::CommandValue {
        match value {
            ArgValue::Number(value) => crate::prolog::CommandValue::Integer(*value),
            ArgValue::Bool(value) => crate::prolog::CommandValue::Boolean(*value),
            ArgValue::String(value) => crate::prolog::CommandValue::String(value.clone()),
            ArgValue::Array(values) => {
                crate::prolog::CommandValue::Array(values.iter().map(convert_value).collect())
            }
        }
    }
    crate::prolog::CommandAst {
        action: invocation.action.clone(),
        handler: invocation.handler.clone(),
        target: match invocation.target {
            ActionTarget::UiVerb => "ui",
            ActionTarget::ControlMessage => "control",
            ActionTarget::LocalUi => "local",
        }
        .into(),
        args: invocation.args.iter().map(convert_value).collect(),
    }
}

fn floor_char_boundary(input: &str, mut cursor: usize) -> usize {
    while cursor > 0 && !input.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixtureContext;

    impl ContextProvider for FixtureContext {
        fn snapshot(
            &self,
            request: &crate::prolog::ContextQueryNode,
        ) -> Result<crate::prolog::ContextSnapshot, String> {
            use crate::prolog::{ContextEntry, ContextSnapshot, RelationValue};
            let string = |value: &str| {
                RelationValue::Compound(
                    "string".into(),
                    vec![RelationValue::String(value.into())],
                )
            };
            let entry = match &request.query.domain {
                RelationValue::Atom(domain) if domain == "box" => ContextEntry {
                    domain: RelationValue::Atom("box".into()),
                    identity: RelationValue::Integer(5),
                    names: vec!["5".into(), "work".into()],
                    value: string("5"),
                    attributes: Vec::new(),
                },
                RelationValue::Atom(domain) if domain == "path" => ContextEntry {
                    domain: RelationValue::Atom("path".into()),
                    identity: RelationValue::Compound(
                        "path".into(),
                        vec![
                            RelationValue::String("5".into()),
                            RelationValue::String("src/main.rs".into()),
                        ],
                    ),
                    names: vec!["src/main.rs".into()],
                    value: string("src/main.rs"),
                    attributes: vec![RelationValue::Compound(
                        "box".into(),
                        vec![string("5")],
                    )],
                },
                _ => return Err("unexpected fixture context domain".into()),
            };
            Ok(ContextSnapshot {
                provider: RelationValue::Atom("fixture".into()),
                revision: RelationValue::Integer(1),
                entries: vec![entry],
            })
        }
    }

    fn invocation(input: &str) -> Invocation {
        match parse(input, &EmptyContext) {
            ParseResult::Invocation(invocation) => invocation,
            other => panic!("{input:?} did not parse: {other:?}"),
        }
    }

    #[test]
    fn shared_cli_form_is_resolved_by_complete_schema() {
        let pending = invocation("mirror run");
        assert_eq!(pending.action, "mirror_run_pending");
        assert!(pending.args.is_empty());

        let one = invocation("mirror run 5");
        assert_eq!(one.action, "mirror_run");
        assert_eq!(one.args, vec![ArgValue::Number(5)]);
        assert!(matches!(
            parse("mirror run 5 extra", &EmptyContext),
            ParseResult::Unknown(_)
        ));
    }

    #[test]
    fn alias_normalization_returns_wire_ready_handler_and_arguments() {
        let pause = invocation("mirror pause 5");
        assert_eq!(pause.handler, "mirror_pause");
        assert_eq!(pause.args, vec![ArgValue::Number(5), ArgValue::Bool(true)]);

        let resume = invocation("mirror resume 5");
        assert_eq!(resume.action, "mirror_resume");
        assert_eq!(resume.handler, "mirror_pause");
        assert_eq!(resume.args, vec![ArgValue::Number(5), ArgValue::Bool(false)]);
    }

    #[test]
    fn parse_and_render_use_the_same_relation() {
        for input in ["mirror ls", "mirror run 5", "mirror pause 5", "mirror resume 5"] {
            let parsed = invocation(input);
            assert_eq!(render(&parsed).unwrap(), input);
        }
    }

    #[test]
    fn completion_preserves_utf8_boundaries() {
        let input = "mirror rün";
        assert!(complete_at(input, input.len(), &EmptyContext).is_ok());
        let inside_umlaut = input.find('ü').unwrap() + 1;
        assert!(grammar_input(input, Some(inside_umlaut)).is_none());
    }

    #[test]
    fn dependent_completion_resolves_box_before_querying_paths() {
        let input = "writer_id work src";
        let completions = complete_at(input, input.len(), &FixtureContext).unwrap();
        assert!(completions.iter().any(|entry| {
            entry.insert == "src/main.rs"
                && entry.provider == "fixture"
                && entry.annotation.contains("context(writer_id,path")
        }));
    }

    #[test]
    fn highlighting_requires_successful_context_observations() {
        assert!(!highlights("rename work new-name", &FixtureContext)
            .unwrap()
            .is_empty());
        assert!(highlights("rename missing new-name", &FixtureContext)
            .unwrap()
            .is_empty());
    }
}
