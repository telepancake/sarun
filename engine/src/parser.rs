//! Parser & lens engine for sarun.
//!
//! ## Vision
//!
//! A Prolog relation-backed parser that can handle the mixed, nested,
//! context-sensitive syntaxes sarun encounters:
//!
//!   - command text: `mirror run 5`
//!   - Typed binary protocol messages: `action(mirror_run, [integer(5)])`
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
//! The UI-action grammar is the first client used to drive the generic
//! relation and FFI to completion. The live migration status and remaining
//! runtime cutovers are recorded in `PROLOG-HUB-ROADMAP.md`. Nested grammars
//! such as HTTP in a packet compose through the same relation:
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
//!   - `complete(SourceWithTear, Binding)` — run the ordinary parser in assist
//!     mode and project bindings recorded in successful parse evidence
//!
//! N-way lens composition relates each supported representation through one
//! semantic identity:
//!
//! N-way transformations between representations:
//!
//! ```prolog
//! representation(mirror_run, action, mirror_run).
//! representation(mirror_run, command, command(["mirror", "run"], identity)).
//! representation(mirror_run, key, key(r, 'Mirrors', 80)).
//! representation(mirror_run, menu, menu("Force-run this job")).
//! representation(mirror_run, wire, wire(64, mirror_run, ui, _, unit)).
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
//! Parsing and rendering execute the same bidirectional form/sequence relation;
//! completion and highlighting project the evidence produced by that relation.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionTarget {
    UiVerb,
    ControlMessage,
    UiLocal,
    CliLocal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgValue {
    Number(i64),
    Bool(bool),
    String(String),
    Path(String),
    Base64(String),
    Spec(String),
    OciSpec {
        context_tar_gz: String,
        dockerfile: String,
        tag: Option<String>,
        net_mode: String,
        build_arguments: Vec<(String, String)>,
    },
    ApiSpec {
        base_url: String,
        model: String,
        api_key: String,
    },
    Array(Vec<ArgValue>),
}

impl ArgValue {
    pub fn json(&self) -> serde_json::Value {
        match self {
            Self::Number(value) => serde_json::Value::Number((*value).into()),
            Self::Bool(value) => serde_json::Value::Bool(*value),
            Self::String(value) => serde_json::Value::String(value.clone()),
            Self::Path(value) | Self::Base64(value) | Self::Spec(value) => {
                serde_json::Value::String(value.clone())
            }
            Self::OciSpec {
                context_tar_gz,
                dockerfile,
                tag,
                net_mode,
                build_arguments,
            } => serde_json::json!({
                "context_tar_gz": context_tar_gz,
                "dockerfile": dockerfile,
                "tag": tag,
                "net": net_mode,
                "build_args": build_arguments,
            }),
            Self::ApiSpec {
                base_url,
                model,
                api_key,
            } => serde_json::json!({
                "base_url": base_url,
                "model": model,
                "api_key": api_key,
            }),
            Self::Array(values) => {
                serde_json::Value::Array(values.iter().map(Self::json).collect())
            }
        }
    }
}

/// A fully resolved action with protocol-ready arguments.
#[derive(Debug, PartialEq)]
pub enum InvocationPayload {
    Wire(crate::generated_wire::ActionRequest),
    Local,
}

#[derive(Debug)]
pub struct Invocation {
    pub action: String,
    pub handler: String,
    pub target: ActionTarget,
    pub payload: InvocationPayload,
    // Transitional source projection used only by the JSON socket transport.
    // The binary cutover deletes this field and json_args() together; request
    // materialization above is mandatory and never falls back to these values.
    pub args: Vec<ArgValue>,
    /// Exact external observations used to resolve this invocation.
    pub context: Vec<crate::prolog::ContextObservation>,
}

impl Invocation {
    pub fn dispatch_name(&self) -> &str {
        match &self.payload {
            InvocationPayload::Wire(request) => request.handler(),
            InvocationPayload::Local => &self.handler,
        }
    }

    pub fn wire_request(&self) -> Result<&crate::generated_wire::ActionRequest, String> {
        match &self.payload {
            InvocationPayload::Wire(request) => Ok(request),
            InvocationPayload::Local => {
                Err(format!("local action {} has no wire request", self.action))
            }
        }
    }

    pub fn json_args(&self) -> serde_json::Value {
        serde_json::Value::Array(self.args.iter().map(ArgValue::json).collect())
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
/// object store (for example, context-free command forms and parser unit tests).
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

/// An immutable projection of the persistent state owned by one Brush shell.
///
/// Foreground consumers capture this while they have a borrowed `Shell`, then
/// release the borrow before invoking Prolog or entering a terminal UI.  The
/// snapshot deliberately contains facts, not matching policy: cardinality and
/// selectors are still evaluated by the context relation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrushSemanticSnapshot {
    logical_cwd: std::path::PathBuf,
    variables: Vec<crate::prolog::ContextEntry>,
    variable_revision: crate::prolog::RelationValue,
}

impl BrushSemanticSnapshot {
    /// A context with no persistent shell variables.  This is the normal
    /// context for document hosts that are not running inside Brush.
    pub fn empty(logical_cwd: impl Into<std::path::PathBuf>) -> Self {
        let logical_cwd = logical_cwd.into();
        let variables = Vec::new();
        let variable_revision = brush_variable_revision(&logical_cwd, &variables);
        Self {
            logical_cwd,
            variables,
            variable_revision,
        }
    }

    /// Capture the visible variable environment and logical cwd atomically
    /// from a borrowed shell. Shadowed variables are already removed by
    /// `ShellEnvironment::iter`; sorting makes identity, revisions, and query
    /// outcomes independent of HashMap iteration order.
    pub fn capture<SE: brush_core::extensions::ShellExtensions>(
        shell: &brush_core::Shell<SE>,
    ) -> Self {
        use crate::prolog::{ContextEntry, RelationValue};
        use brush_core::variables::ShellValue;

        let logical_cwd = shell.working_dir().to_path_buf();
        let mut variables = shell
            .env()
            .iter()
            .map(|(name, variable)| {
                let value = match variable.value() {
                    ShellValue::Unset(_) => RelationValue::Atom("unset".into()),
                    ShellValue::String(value) => shell_text_value(value.clone()),
                    ShellValue::AssociativeArray(values) => RelationValue::Compound(
                        "shell_associative_array".into(),
                        vec![RelationValue::List(
                            values
                                .iter()
                                .map(|(key, value)| {
                                    RelationValue::Compound(
                                        "entry".into(),
                                        vec![
                                            RelationValue::String(key.clone()),
                                            RelationValue::String(value.clone()),
                                        ],
                                    )
                                })
                                .collect(),
                        )],
                    ),
                    ShellValue::IndexedArray(values) => RelationValue::Compound(
                        "shell_indexed_array".into(),
                        vec![RelationValue::List(
                            values
                                .iter()
                                .map(|(index, value)| {
                                    RelationValue::Compound(
                                        "entry".into(),
                                        vec![
                                            RelationValue::Integer(
                                                (*index).min(i64::MAX as u64) as i64
                                            ),
                                            RelationValue::String(value.clone()),
                                        ],
                                    )
                                })
                                .collect(),
                        )],
                    ),
                    ShellValue::Dynamic { .. } => {
                        shell_text_value(variable.value().to_cow_str(shell).into_owned())
                    }
                };
                let mut attributes = Vec::new();
                if variable.is_exported() {
                    attributes.push(RelationValue::Atom("exported".into()));
                }
                if variable.is_readonly() {
                    attributes.push(RelationValue::Atom("readonly".into()));
                }
                if variable.is_treated_as_integer() {
                    attributes.push(RelationValue::Atom("integer".into()));
                }
                if variable.is_treated_as_nameref() {
                    attributes.push(RelationValue::Atom("nameref".into()));
                }
                ContextEntry {
                    domain: RelationValue::Atom("shell_variable".into()),
                    identity: RelationValue::Compound(
                        "shell_variable".into(),
                        vec![RelationValue::String(name.clone())],
                    ),
                    names: vec![name.clone()],
                    value,
                    attributes,
                }
            })
            .collect::<Vec<_>>();
        variables.sort_by(|left, right| left.names.cmp(&right.names));
        let variable_revision = brush_variable_revision(&logical_cwd, &variables);
        Self {
            logical_cwd,
            variables,
            variable_revision,
        }
    }
}

fn shell_text_value(value: String) -> crate::prolog::RelationValue {
    crate::prolog::RelationValue::Compound(
        "shell_text".into(),
        vec![crate::prolog::RelationValue::String(value)],
    )
}

fn brush_variable_revision(
    logical_cwd: &std::path::Path,
    entries: &[crate::prolog::ContextEntry],
) -> crate::prolog::RelationValue {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(b"sarun-brush-variables-v1\0");
    digest.update(logical_cwd.as_os_str().as_encoded_bytes());
    for entry in entries {
        digest.update(b"\0entry\0");
        hash_relation_value(&mut digest, &entry.domain);
        hash_relation_value(&mut digest, &entry.identity);
        for name in &entry.names {
            digest.update((name.len() as u64).to_le_bytes());
            digest.update(name.as_bytes());
        }
        hash_relation_value(&mut digest, &entry.value);
        for attribute in &entry.attributes {
            hash_relation_value(&mut digest, attribute);
        }
    }
    let digest = digest.finalize();
    let mut text = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(text, "{byte:02x}");
    }
    crate::prolog::RelationValue::Compound(
        "brush_variables_revision".into(),
        vec![crate::prolog::RelationValue::String(text)],
    )
}

fn hash_relation_value(digest: &mut sha2::Sha256, value: &crate::prolog::RelationValue) {
    use crate::prolog::RelationValue;
    use sha2::Digest as _;
    match value {
        RelationValue::Atom(value) => {
            digest.update(b"a");
            digest.update((value.len() as u64).to_le_bytes());
            digest.update(value.as_bytes());
        }
        RelationValue::String(value) => {
            digest.update(b"s");
            digest.update((value.len() as u64).to_le_bytes());
            digest.update(value.as_bytes());
        }
        RelationValue::Integer(value) => {
            digest.update(b"i");
            digest.update(value.to_le_bytes());
        }
        RelationValue::Compound(name, arguments) => {
            digest.update(b"c");
            digest.update((name.len() as u64).to_le_bytes());
            digest.update(name.as_bytes());
            digest.update((arguments.len() as u64).to_le_bytes());
            for argument in arguments {
                hash_relation_value(digest, argument);
            }
        }
        RelationValue::List(values) => {
            digest.update(b"l");
            digest.update((values.len() as u64).to_le_bytes());
            for value in values {
                hash_relation_value(digest, value);
            }
        }
    }
}

/// Parse process argv through the same relation while preserving each argument
/// as one source unit, including embedded whitespace and empty strings.
pub fn parse_argv(parts: &[String], context: &dyn ContextProvider) -> ParseResult {
    parse_surfaces(parts.iter().map(String::as_str), context)
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
    parse_input(grammar_input, input.to_string(), context)
}

fn parse_surfaces<'a>(
    surfaces: impl IntoIterator<Item = &'a str>,
    context: &dyn ContextProvider,
) -> ParseResult {
    use crate::prolog::{GrammarInput, InputItem, KnownUnit, Semantic, Span};
    const MAX_BUFFER_BYTES: usize = 16 * 1024;
    const MAX_ARGUMENTS: usize = 256;
    let surfaces: Vec<&str> = surfaces.into_iter().collect();
    let unknown = surfaces.join(" ");
    let mut items = Vec::new();
    let mut offset = 0usize;
    for surface in surfaces {
        if items.len() >= MAX_ARGUMENTS
            || offset
                .checked_add(surface.len())
                .is_none_or(|end| end > MAX_BUFFER_BYTES)
        {
            return ParseResult::BackendError("command argv exceeds parser limits".into());
        }
        let end = offset + surface.len();
        let span = Span { start: offset, end };
        items.push(InputItem::Unit(KnownUnit {
            semantic: Semantic::Text(surface.into()),
            span,
            paint_spans: vec![span],
            surface: surface.into(),
            syntax: "source".into(),
            provider: "argv".into(),
            preference: 0,
        }));
        offset = end.saturating_add(1);
    }
    if items.is_empty() {
        return ParseResult::Empty;
    }
    parse_input(
        GrammarInput {
            items,
            end: offset.saturating_sub(1),
        },
        unknown,
        context,
    )
}

fn parse_input(
    grammar_input: crate::prolog::GrammarInput,
    unknown: String,
    context: &dyn ContextProvider,
) -> ParseResult {
    let prolog = match crate::prolog::global() {
        Ok(prolog) => prolog,
        Err(error) => return ParseResult::BackendError(error),
    };
    let resolved = match resolve_best_plan(prolog, &grammar_input, context) {
        Ok(resolved) => resolved,
        Err(error) => return ParseResult::BackendError(error),
    };
    let Some(resolved) = resolved else {
        return ParseResult::Unknown(unknown);
    };
    match invocation_from_command(resolved.command, resolved.observations) {
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
) -> Result<
    Option<(
        crate::prolog::CommandAst,
        Vec<crate::prolog::ContextObservation>,
    )>,
    String,
> {
    let Some(observations) = execute_context_graph(prolog, &plan.queries, context)? else {
        return Ok(None);
    };
    match prolog.resolve_context_plan(plan, &observations) {
        Ok(command) => Ok(Some((command, observations))),
        Err(crate::prolog::QueryError::NoSolution) => Ok(None),
        Err(crate::prolog::QueryError::Backend(error)) => Err(error),
    }
}

pub(crate) fn execute_context_graph(
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
                    "context relation returned unknown ready query {:?}",
                    resolved.id
                ));
            };
            let snapshot = match crate::relation_adapter::snapshot(&resolved)? {
                Some(snapshot) => snapshot,
                None => context.snapshot(&resolved)?,
            };
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

/// Resolve one Brush document analysis, including every explicit external
/// query emitted by the ordinary relation.  Consumers use this single result
/// for status, highlights, completions, and state deltas; they must not run a
/// neighboring context algorithm for an individual projection.
pub fn analyze_brush_document_resolved(
    request: &crate::prolog::BrushDocumentRequest,
    context: &dyn ContextProvider,
) -> Result<crate::prolog::BrushDocumentAnalysis, String> {
    const MAX_CONTEXT_ROUNDS: usize = 8;
    let mut request = request.clone();
    for _ in 0..MAX_CONTEXT_ROUNDS {
        let analysis = crate::prolog::analyze_brush_document(&request)?;
        if analysis.context_queries.is_empty() {
            return Ok(analysis);
        }
        let prolog = crate::prolog::global()?;
        let Some(observations) = execute_context_graph(prolog, &analysis.context_queries, context)?
        else {
            return Err("Brush context query graph has no ready query".into());
        };
        if !merge_context_observations(&mut request.observations, observations) {
            return Ok(analysis);
        }
    }
    Err(format!(
        "Brush context query graph did not stabilize after {MAX_CONTEXT_ROUNDS} rounds"
    ))
}

/// Add newly-ready observations without discarding observations from earlier
/// dependency stages. A relation can reveal another query only after an
/// earlier one succeeds; the accumulated set is therefore keyed by the
/// relation's scoped query identity, not by provider revision or vector slot.
fn merge_context_observations(
    accumulated: &mut Vec<crate::prolog::ContextObservation>,
    additions: Vec<crate::prolog::ContextObservation>,
) -> bool {
    let mut changed = false;
    for addition in additions {
        if let Some(index) = accumulated
            .iter()
            .position(|existing| existing.id == addition.id && existing.query == addition.query)
        {
            if accumulated[index] != addition {
                accumulated[index] = addition;
                changed = true;
            }
        } else {
            accumulated.push(addition);
            changed = true;
        }
    }
    changed
}

/// Query-scoped filesystem snapshots rooted in a caller's logical working
/// directory. Path interpretation and matching remain in the Prolog context
/// relation; this provider only enumerates the smallest directory snapshot
/// needed by a typed filesystem prefix query.
pub struct FilesystemContext {
    logical_cwd: std::path::PathBuf,
}

impl FilesystemContext {
    pub fn new(logical_cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            logical_cwd: logical_cwd.into(),
        }
    }

    fn prefix<'a>(selector: &'a crate::prolog::RelationValue) -> Option<&'a str> {
        use crate::prolog::RelationValue;
        match selector {
            RelationValue::Compound(name, arguments)
                if name == "prefix" && arguments.len() == 1 =>
            {
                match &arguments[0] {
                    RelationValue::String(prefix) | RelationValue::Atom(prefix) => Some(prefix),
                    _ => None,
                }
            }
            RelationValue::Compound(name, arguments)
                if matches!(name.as_str(), "within" | "and" | "or") =>
            {
                arguments.iter().find_map(Self::prefix)
            }
            _ => None,
        }
    }

    fn filesystem_domain(domain: &crate::prolog::RelationValue) -> Option<&str> {
        match domain {
            crate::prolog::RelationValue::Atom(domain)
                if matches!(
                    domain.as_str(),
                    "filesystem_path"
                        | "filesystem_file"
                        | "filesystem_directory"
                        | "filesystem_executable"
                ) =>
            {
                Some(domain)
            }
            _ => None,
        }
    }
}

impl ContextProvider for FilesystemContext {
    fn snapshot(
        &self,
        request: &crate::prolog::ContextQueryNode,
    ) -> Result<crate::prolog::ContextSnapshot, String> {
        use crate::prolog::{ContextEntry, ContextSnapshot, RelationValue};
        use std::time::UNIX_EPOCH;

        let Some(domain) = Self::filesystem_domain(&request.query.domain) else {
            return Ok(ContextSnapshot {
                provider: RelationValue::Atom("filesystem_context".into()),
                revision: RelationValue::Integer(0),
                entries: Vec::new(),
            });
        };
        let prefix = Self::prefix(&request.query.selector).unwrap_or("");
        let (display_parent, leaf_prefix) = prefix
            .rfind('/')
            .map_or(("", prefix), |slash| prefix.split_at(slash + 1));
        let operand_parent = if display_parent.is_empty() {
            std::path::Path::new(".")
        } else {
            std::path::Path::new(display_parent)
        };
        let directory = if operand_parent.is_absolute() {
            operand_parent.to_path_buf()
        } else {
            self.logical_cwd.join(operand_parent)
        };
        let metadata = std::fs::metadata(&directory).ok();
        let modified = metadata
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok());
        let revision = RelationValue::Compound(
            "filesystem_revision".into(),
            vec![
                RelationValue::String(directory.to_string_lossy().into_owned()),
                RelationValue::Integer(
                    modified.map_or(0, |duration| duration.as_secs().min(i64::MAX as u64) as i64),
                ),
                RelationValue::Integer(
                    modified.map_or(0, |duration| duration.subsec_nanos() as i64),
                ),
            ],
        );
        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&directory) {
            for entry in read_dir.flatten() {
                let Some(leaf) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !leaf.starts_with(leaf_prefix)
                    || (!leaf_prefix.starts_with('.') && leaf.starts_with('.'))
                {
                    continue;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let is_directory = file_type.is_dir();
                let is_file = file_type.is_file();
                #[cfg(unix)]
                let is_executable = {
                    use std::os::unix::fs::PermissionsExt as _;
                    entry
                        .metadata()
                        .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
                };
                #[cfg(not(unix))]
                let is_executable = is_file;
                if (domain == "filesystem_file" && !is_file)
                    || (domain == "filesystem_directory" && !is_directory)
                    || (domain == "filesystem_executable" && !is_executable)
                {
                    continue;
                }
                let mut display_name = format!("{display_parent}{leaf}");
                if is_directory {
                    display_name.push('/');
                }
                let identity_path = entry.path();
                let kind = if is_directory {
                    "directory"
                } else if file_type.is_symlink() {
                    "symlink"
                } else {
                    "file"
                };
                entries.push(ContextEntry {
                    domain: request.query.domain.clone(),
                    identity: RelationValue::String(identity_path.to_string_lossy().into_owned()),
                    names: vec![display_name],
                    value: RelationValue::Compound(
                        "filesystem_path".into(),
                        vec![RelationValue::String(
                            identity_path.to_string_lossy().into_owned(),
                        )],
                    ),
                    attributes: vec![RelationValue::Atom(kind.into())],
                });
            }
        }
        entries.sort_by(|left, right| left.names.cmp(&right.names));
        Ok(ContextSnapshot {
            provider: RelationValue::Compound(
                "filesystem".into(),
                vec![RelationValue::String(
                    self.logical_cwd.to_string_lossy().into_owned(),
                )],
            ),
            revision,
            entries,
        })
    }
}

impl ContextProvider for BrushSemanticSnapshot {
    fn snapshot(
        &self,
        request: &crate::prolog::ContextQueryNode,
    ) -> Result<crate::prolog::ContextSnapshot, String> {
        use crate::prolog::{ContextSnapshot, RelationValue};
        if request.query.domain == RelationValue::Atom("shell_variable".into()) {
            return Ok(ContextSnapshot {
                provider: RelationValue::Compound(
                    "brush_shell_variables".into(),
                    vec![RelationValue::String(
                        self.logical_cwd.to_string_lossy().into_owned(),
                    )],
                ),
                revision: self.variable_revision.clone(),
                entries: self.variables.clone(),
            });
        }
        if FilesystemContext::filesystem_domain(&request.query.domain).is_some() {
            return FilesystemContext::new(&self.logical_cwd).snapshot(request);
        }
        Err(format!(
            "no Brush context provider for domain {:?}",
            request.query.domain
        ))
    }
}

fn invocation_from_command(
    command: crate::prolog::CommandAst,
    context: Vec<crate::prolog::ContextObservation>,
) -> Result<Invocation, String> {
    use crate::prolog::CommandValue;
    fn convert_value(value: CommandValue) -> Result<ArgValue, String> {
        match value {
            CommandValue::Integer(value) => Ok(ArgValue::Number(value)),
            CommandValue::Boolean(value) => Ok(ArgValue::Bool(value)),
            CommandValue::String(value) => Ok(ArgValue::String(value)),
            CommandValue::Path(value) => Ok(ArgValue::Path(value)),
            CommandValue::Base64(value) => Ok(ArgValue::Base64(value)),
            CommandValue::Spec(value) => Ok(ArgValue::Spec(value)),
            CommandValue::OciSpec {
                context_tar_gz,
                dockerfile,
                tag,
                net_mode,
                build_arguments,
            } => Ok(ArgValue::OciSpec {
                context_tar_gz,
                dockerfile,
                tag,
                net_mode,
                build_arguments,
            }),
            CommandValue::ApiSpec {
                base_url,
                model,
                api_key,
            } => Ok(ArgValue::ApiSpec {
                base_url,
                model,
                api_key,
            }),
            CommandValue::Array(values) => Ok(ArgValue::Array(
                values
                    .into_iter()
                    .map(convert_value)
                    .collect::<Result<_, _>>()?,
            )),
            CommandValue::Hole { name, kind } => Err(format!(
                "grammar returned incomplete {kind} argument {name} for execution"
            )),
        }
    }
    let target = match command.target.as_str() {
        "ui" => ActionTarget::UiVerb,
        "control" => ActionTarget::ControlMessage,
        "local" => ActionTarget::UiLocal,
        "cli" => ActionTarget::CliLocal,
        other => return Err(format!("grammar returned invalid action target {other}")),
    };
    let payload = match target {
        ActionTarget::UiVerb | ActionTarget::ControlMessage => {
            let request = crate::action_bridge::materialize(&command)?;
            if request.handler() != command.handler {
                return Err(format!(
                    "wire request handler {} disagrees with parsed handler {}",
                    request.handler(),
                    command.handler
                ));
            }
            InvocationPayload::Wire(request)
        }
        ActionTarget::UiLocal | ActionTarget::CliLocal => InvocationPayload::Local,
    };
    let args = command
        .args
        .into_iter()
        .map(convert_value)
        .collect::<Result<_, _>>()?;
    Ok(Invocation {
        action: command.action,
        handler: command.handler,
        target,
        payload,
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
    Ok(highlights
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
        .collect())
}
pub fn render(invocation: &Invocation) -> Result<String, String> {
    let command = prolog_command(invocation);
    let prolog = crate::prolog::global()?;
    prolog
        .render(&command)
        .map(|rendered| rendered.text)
        .map_err(|error| error.to_string())
}

fn prolog_command(invocation: &Invocation) -> crate::prolog::CommandAst {
    fn convert_value(value: &ArgValue) -> crate::prolog::CommandValue {
        match value {
            ArgValue::Number(value) => crate::prolog::CommandValue::Integer(*value),
            ArgValue::Bool(value) => crate::prolog::CommandValue::Boolean(*value),
            ArgValue::String(value) => crate::prolog::CommandValue::String(value.clone()),
            ArgValue::Path(value) => crate::prolog::CommandValue::Path(value.clone()),
            ArgValue::Base64(value) => crate::prolog::CommandValue::Base64(value.clone()),
            ArgValue::Spec(value) => crate::prolog::CommandValue::Spec(value.clone()),
            ArgValue::OciSpec {
                context_tar_gz,
                dockerfile,
                tag,
                net_mode,
                build_arguments,
            } => crate::prolog::CommandValue::OciSpec {
                context_tar_gz: context_tar_gz.clone(),
                dockerfile: dockerfile.clone(),
                tag: tag.clone(),
                net_mode: net_mode.clone(),
                build_arguments: build_arguments.clone(),
            },
            ArgValue::ApiSpec {
                base_url,
                model,
                api_key,
            } => crate::prolog::CommandValue::ApiSpec {
                base_url: base_url.clone(),
                model: model.clone(),
                api_key: api_key.clone(),
            },
            ArgValue::Array(values) => {
                crate::prolog::CommandValue::Array(values.iter().map(convert_value).collect())
            }
        }
    }
    crate::prolog::CommandAst {
        action: invocation.action.clone(),
        handler: invocation.dispatch_name().to_string(),
        target: match invocation.target {
            ActionTarget::UiVerb => "ui",
            ActionTarget::ControlMessage => "control",
            ActionTarget::UiLocal => "local",
            ActionTarget::CliLocal => "cli",
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

    fn shell_snapshot(variables: &[(&str, &str)]) -> BrushSemanticSnapshot {
        let mut shell: brush_core::Shell = brush_core::Shell::default();
        shell
            .set_working_dir(std::env::temp_dir())
            .expect("set test shell cwd");
        for &(name, value) in variables {
            shell
                .set_env_global(name, brush_core::ShellVariable::new(value))
                .expect("set test shell variable");
        }
        BrushSemanticSnapshot::capture(&shell)
    }

    struct FixtureContext;

    impl ContextProvider for FixtureContext {
        fn snapshot(
            &self,
            request: &crate::prolog::ContextQueryNode,
        ) -> Result<crate::prolog::ContextSnapshot, String> {
            use crate::prolog::{ContextEntry, ContextSnapshot, RelationValue};
            let entry = match &request.query.domain {
                RelationValue::Atom(domain) if domain == "box" => ContextEntry {
                    domain: RelationValue::Atom("box".into()),
                    identity: RelationValue::Integer(5),
                    names: vec!["5".into(), "work".into()],
                    value: RelationValue::Compound(
                        "integer".into(),
                        vec![RelationValue::Integer(5)],
                    ),
                    attributes: Vec::new(),
                },
                RelationValue::Atom(domain) if domain == "path" => ContextEntry {
                    domain: RelationValue::Atom("path".into()),
                    identity: RelationValue::Compound(
                        "path".into(),
                        vec![
                            RelationValue::Integer(5),
                            RelationValue::String("src/main.rs".into()),
                        ],
                    ),
                    names: vec!["src/main.rs".into()],
                    value: RelationValue::Compound(
                        "path".into(),
                        vec![RelationValue::String("src/main.rs".into())],
                    ),
                    attributes: vec![RelationValue::Compound(
                        "box".into(),
                        vec![RelationValue::Compound(
                            "integer".into(),
                            vec![RelationValue::Integer(5)],
                        )],
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

    #[test]
    fn brush_snapshot_is_ordered_and_revision_is_content_deterministic() {
        let left = shell_snapshot(&[("BETA", "2"), ("ALPHA", "1")]);
        let right = shell_snapshot(&[("ALPHA", "1"), ("BETA", "2")]);
        assert_eq!(left, right);

        let request = crate::prolog::ContextQueryNode {
            id: crate::prolog::RelationValue::Atom("variables".into()),
            query: crate::prolog::ContextQuery {
                cardinality: crate::prolog::ContextCardinality::All,
                domain: crate::prolog::RelationValue::Atom("shell_variable".into()),
                selector: crate::prolog::RelationValue::Compound(
                    "prefix".into(),
                    vec![crate::prolog::RelationValue::String(String::new())],
                ),
            },
        };
        let snapshot = left.snapshot(&request).unwrap();
        let names = snapshot
            .entries
            .iter()
            .map(|entry| entry.names[0].as_str())
            .collect::<Vec<_>>();
        assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(names.contains(&"ALPHA"));
        assert!(names.contains(&"BETA"));
    }

    #[test]
    fn resolved_brush_analysis_completes_and_tracks_persistent_variables() {
        let before = shell_snapshot(&[("PERSISTENT", "value"), ("UNRELATED", "before")]);

        let source = "echo $PERS";
        let completed_source = "echo $PERSISTENT";
        let request = crate::prolog::BrushDocumentRequest {
            source: source.into(),
            assist: Some(crate::prolog::Span {
                start: source.len(),
                end: source.len(),
            }),
            initial_bindings: vec![],
            observations: vec![],
        };
        let first = analyze_brush_document_resolved(&request, &before).unwrap();
        assert!(first.candidates.iter().any(|candidate| {
            candidate.completions.iter().any(|completion| {
                let replace = completion.replace.clone();
                replace.start <= replace.end
                    && replace.end <= source.len()
                    && format!(
                        "{}{}{}",
                        &source[..replace.start],
                        completion.insert,
                        &source[replace.end..]
                    ) == completed_source
            })
        }));

        let value_changed = shell_snapshot(&[("PERSISTENT", "new value")]);
        let exact_request = crate::prolog::BrushDocumentRequest {
            source: "echo $PERSISTENT".into(),
            assist: None,
            initial_bindings: vec![],
            observations: vec![],
        };
        let exact_before = analyze_brush_document_resolved(&exact_request, &before).unwrap();
        let exact_after = analyze_brush_document_resolved(&exact_request, &value_changed).unwrap();
        assert_ne!(exact_before.dependency_keys, exact_after.dependency_keys);
    }

    fn invocation(input: &str) -> Invocation {
        match parse(input, &EmptyContext) {
            ParseResult::Invocation(invocation) => invocation,
            other => panic!("{input:?} did not parse: {other:?}"),
        }
    }

    #[test]
    fn action_identifier_has_one_mechanical_text_encoding() {
        let pending = invocation("mirror run pending");
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
    fn argv_is_a_distinct_source_representation_with_stable_boundaries() {
        let argv = [
            "brush",
            "-c",
            "printf '%s\\n' \"$1\"",
            "brush",
            "argument with spaces",
            "",
        ]
        .map(str::to_string);
        let invocation = match parse_argv(&argv, &EmptyContext) {
            ParseResult::Invocation(invocation) => invocation,
            other => panic!("relation did not parse standalone Brush argv: {other:?}"),
        };
        assert_eq!(invocation.action, "brush");
        assert_eq!(invocation.handler, "brush");
        assert_eq!(invocation.target, ActionTarget::CliLocal);
        assert_eq!(
            invocation.args,
            argv[1..]
                .iter()
                .cloned()
                .map(ArgValue::String)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn argument_projection_returns_wire_ready_handler_and_arguments() {
        let pause = invocation("mirror pause 5");
        assert_eq!(pause.dispatch_name(), "mirror_pause");
        assert_eq!(pause.args, vec![ArgValue::Number(5), ArgValue::Bool(true)]);
        assert_eq!(
            pause.payload,
            InvocationPayload::Wire(crate::generated_wire::ActionRequest::MirrorPause {
                id: 5,
                paused: true,
            })
        );

        let resume = invocation("mirror resume 5");
        assert_eq!(resume.action, "mirror_resume");
        assert_eq!(resume.dispatch_name(), "mirror_pause");
        assert_eq!(
            resume.args,
            vec![ArgValue::Number(5), ArgValue::Bool(false)]
        );
        assert_eq!(
            resume.payload,
            InvocationPayload::Wire(crate::generated_wire::ActionRequest::MirrorPause {
                id: 5,
                paused: false,
            })
        );
    }

    #[test]
    fn parse_and_render_use_the_same_relation() {
        for input in [
            "mirror jobs",
            "mirror run 5",
            "mirror pause 5",
            "mirror resume 5",
        ] {
            let parsed = invocation(input);
            assert_eq!(render(&parsed).unwrap(), input);
        }
        let local = invocation("refresh");
        assert_eq!(local.payload, InvocationPayload::Local);
        assert!(local.wire_request().is_err());
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
        let input = "writer id work src";
        let completions = complete_at(input, input.len(), &FixtureContext).unwrap();
        assert!(completions.iter().any(|entry| {
            entry.insert == "src/main.rs"
                && entry.provider == "fixture"
                && entry.annotation.contains("context(writer_id,path")
        }));
    }

    #[test]
    fn contextual_completion_result_reparses_and_renders() {
        let input = "kill wo";
        let completion = complete_at(input, input.len(), &FixtureContext)
            .unwrap()
            .into_iter()
            .find(|entry| entry.insert == "work")
            .expect("live box completion");
        let completed = apply_completion(input, &completion);
        assert_eq!(completed, "kill work");
        let ParseResult::Invocation(invocation) = parse(&completed, &FixtureContext) else {
            panic!("completed command did not parse");
        };
        assert_eq!(
            invocation.payload,
            InvocationPayload::Wire(crate::generated_wire::ActionRequest::Kill { sid: 5 }),
        );
        assert_eq!(render(&invocation).unwrap(), "kill 5");
        assert!(!highlights(&completed, &FixtureContext).unwrap().is_empty());
    }

    #[test]
    fn highlighting_requires_successful_context_observations() {
        assert!(
            !highlights("rename work new-name", &FixtureContext)
                .unwrap()
                .is_empty()
        );
        assert!(
            highlights("rename missing new-name", &FixtureContext)
                .unwrap()
                .is_empty()
        );
    }
}
