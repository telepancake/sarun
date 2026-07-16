use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

static APP_RESOURCE: &[u8] = include_bytes!(env!("SARUN_SWIPL_RESOURCE"));
static ACTIVE: AtomicU8 = AtomicU8::new(INACTIVE);

const INACTIVE: u8 = 0;
const RUNNING: u8 = 1;
const POISONED: u8 = 2;
const LOAD_INFERENCES: i64 = 5_000_000;
const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_ITEMS: usize = 256;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_RELATION_NODES: usize = 65_536;
const MAX_RELATION_DEPTH: usize = 128;

const PL_Q_NODEBUG: c_int = 0x0004;
const PL_Q_CATCH_EXCEPTION: c_int = 0x0008;
const CVT_ATOM: u32 = 0x0000_0001;
const CVT_STRING: u32 = 0x0000_0002;
const CVT_EXCEPTION: u32 = 0x0000_1000;
const BUF_MALLOC: u32 = 0x0002_0000;
const REP_UTF8: u32 = 0x0010_0000;
const PL_CLEANUP_NO_CANCEL: c_int = 0x0002_0000;
const PL_CLEANUP_SUCCESS: c_int = 1;
const PL_ATOM_TYPE: c_int = 2;
const PL_INTEGER_TYPE: c_int = 3;
const PL_STRING_TYPE: c_int = 6;
const PL_COMPOUND_TYPE: c_int = 7;
const PL_NIL_TYPE: c_int = 8;
const PL_LIST_PAIR_TYPE: c_int = 10;

type Term = usize;
type Query = usize;
type Predicate = *mut c_void;
type Module = *mut c_void;

unsafe extern "C" {
    fn PL_set_resource_db_mem(data: *const u8, size: usize) -> c_int;
    fn PL_initialise(argc: c_int, argv: *mut *mut c_char) -> c_int;
    fn PL_cleanup(status_and_flags: c_int) -> c_int;
    fn PL_new_term_refs(count: usize) -> Term;
    fn PL_reset_term_refs(term: Term);
    fn PL_put_variable(term: Term) -> c_int;
    fn PL_put_int64(term: Term, value: i64) -> c_int;
    fn PL_put_chars(term: Term, flags: c_int, len: usize, text: *const c_char) -> c_int;
    fn PL_put_term(target: Term, source: Term) -> c_int;
    fn PL_put_nil(term: Term) -> c_int;
    fn PL_cons_list(list: Term, head: Term, tail: Term) -> c_int;
    fn PL_new_atom_nchars(len: usize, text: *const c_char) -> usize;
    fn PL_unregister_atom(atom: usize);
    fn PL_new_functor_sz(atom: usize, arity: usize) -> usize;
    fn PL_cons_functor_v(term: Term, functor: usize, arguments: Term) -> c_int;
    fn PL_put_term_from_chars(term: Term, flags: c_int, len: usize, text: *const c_char) -> c_int;
    fn PL_predicate(name: *const c_char, arity: c_int, module: *const c_char) -> Predicate;
    fn PL_open_query(module: Module, flags: c_int, predicate: Predicate, terms: Term) -> Query;
    fn PL_next_solution(query: Query) -> c_int;
    fn PL_cut_query(query: Query) -> c_int;
    fn PL_close_query(query: Query) -> c_int;
    fn PL_exception(query: Query) -> Term;
    fn PL_clear_exception();
    fn PL_get_arg_sz(index: usize, term: Term, argument: Term) -> c_int;
    fn PL_term_type(term: Term) -> c_int;
    fn PL_get_int64(term: Term, value: *mut i64) -> c_int;
    fn PL_get_name_arity_sz(term: Term, name: *mut usize, arity: *mut usize) -> c_int;
    fn PL_atom_nchars(atom: usize, len: *mut usize) -> *const c_char;
    fn PL_get_list(list: Term, head: Term, tail: Term) -> c_int;
    fn PL_get_nil(list: Term) -> c_int;
    fn PL_get_nchars(term: Term, len: *mut usize, text: *mut *mut c_char, flags: u32) -> c_int;
    fn PL_free(memory: *mut c_void);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Semantic {
    Atom(String),
    Integer(i64),
    Text(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownUnit {
    pub semantic: Semantic,
    pub span: Span,
    pub paint_spans: Vec<Span>,
    pub surface: String,
    pub syntax: String,
    pub provider: String,
    pub preference: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputItem {
    Unit(KnownUnit),
    EditTear {
        id: &'static str,
        span: Span,
        surface: String,
    },
    SourceTear {
        id: usize,
        span: Span,
        surface: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrammarInput {
    pub items: Vec<InputItem>,
    pub end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandValue {
    Integer(i64),
    Boolean(bool),
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
    Array(Vec<CommandValue>),
    /// A typed argument expected after an edit tear in an incomplete parse.
    /// Complete commands crossing the execution boundary never contain this.
    Hole {
        name: String,
        kind: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandAst {
    pub action: String,
    pub handler: String,
    pub target: String,
    pub args: Vec<CommandValue>,
}

/// A ground, pure value crossing the generic relation boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelationValue {
    Atom(String),
    String(String),
    Integer(i64),
    Compound(String, Vec<RelationValue>),
    List(Vec<RelationValue>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelationBinding {
    pub name: String,
    pub value: RelationValue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationLimits {
    pub max_solutions: usize,
    pub max_evidence: usize,
    pub max_output_bytes: usize,
}

impl Default for RelationLimits {
    fn default() -> Self {
        Self {
            max_solutions: 64,
            max_evidence: 4096,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelationRequest {
    pub grammar: RelationValue,
    pub given: Vec<RelationBinding>,
    pub wanted: Vec<String>,
    pub observations: Vec<RelationValue>,
    pub limits: RelationLimits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelationSolution {
    pub bindings: Vec<RelationBinding>,
    pub preference: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelationReply {
    pub solutions: Vec<RelationSolution>,
    pub context_queries: Vec<RelationValue>,
    pub dependency_keys: Vec<RelationValue>,
    pub diagnostics: Vec<RelationValue>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCardinality {
    Empty,
    One,
    All,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextQuery {
    pub cardinality: ContextCardinality,
    pub domain: RelationValue,
    pub selector: RelationValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextEntry {
    pub domain: RelationValue,
    pub identity: RelationValue,
    pub names: Vec<String>,
    pub value: RelationValue,
    pub attributes: Vec<RelationValue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSnapshot {
    pub provider: RelationValue,
    pub revision: RelationValue,
    pub entries: Vec<ContextEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextResult {
    Empty(bool),
    One(ContextEntry),
    All(Vec<ContextEntry>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextObservation {
    pub id: RelationValue,
    pub query: ContextQuery,
    pub provider: RelationValue,
    pub revision: RelationValue,
    pub outcome: Option<ContextResult>,
}

/// Provenance-free semantic projection used for parse invalidation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextDependencyKey {
    pub id: RelationValue,
    pub query: ContextQuery,
    pub outcome: Option<ContextResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextQueryNode {
    pub id: RelationValue,
    pub query: ContextQuery,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPlan {
    pub source: GrammarInput,
    pub assist_edit: Option<String>,
    pub command: CommandAst,
    pub queries: Vec<ContextQueryNode>,
    pub evidence: Vec<Evidence>,
    pub preference: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompletionPlan {
    pub source: GrammarInput,
    pub edit_id: String,
    pub action: String,
    pub replace: Span,
    pub surface: String,
    pub queries: Vec<ContextQueryNode>,
    pub target_query_id: RelationValue,
    pub preference: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseStatus {
    Complete,
    Incomplete { edit_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Evidence {
    pub semantic: String,
    pub span: Span,
    pub paint_spans: Vec<Span>,
    pub surface: String,
    pub syntax: String,
    pub provider: String,
    pub preference: i64,
    pub origin: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseCandidate {
    pub command: CommandAst,
    pub status: ParseStatus,
    pub evidence: Vec<Evidence>,
    pub preference: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionAlternative {
    pub semantic: String,
    pub syntax: String,
    pub provider: String,
    pub preference: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub replace: Span,
    pub insert: String,
    pub display: String,
    pub alternatives: Vec<CompletionAlternative>,
    pub preference: i64,
    pub rank: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueryError {
    NoSolution,
    Backend(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSolution => formatter.write_str("action grammar has no solution"),
            Self::Backend(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for QueryError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Highlight {
    pub span: Span,
    pub syntax: String,
    pub semantic: String,
    pub origin: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedCommand {
    pub text: String,
}

enum Command {
    Transform(RelationValue, usize, Sender<Result<RelationValue, String>>),
    Shutdown(Sender<Result<(), String>>),
    #[cfg(test)]
    ExhaustInferenceLimit(Sender<Result<(), String>>),
}

/// A process-global SWI-Prolog runtime whose FFI calls stay on one thread.
///
/// The public query surface is limited to typed relation transformations over
/// the embedded action grammar. Each transformation has an inference bound.
/// This recovers from nonterminating pure Prolog code without SWI signal handlers. An inference
/// bound is not a wall-clock timeout and cannot interrupt a blocking foreign
/// predicate; the embedded grammar is pure and the API cannot invoke foreign,
/// filesystem, process, `halt/0`, or user-selected predicates.
pub struct Prolog {
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Prolog {
    pub fn new() -> Result<Self, String> {
        match ACTIVE.compare_exchange(INACTIVE, RUNNING, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {}
            Err(RUNNING) => return Err("a Prolog runtime is already active".into()),
            Err(POISONED) => {
                return Err("the process-global Prolog runtime is poisoned".into());
            }
            Err(_) => return Err("the process-global Prolog runtime has an invalid state".into()),
        }

        let (commands, receiver) = mpsc::channel();
        let (initialized_tx, initialized_rx) = mpsc::sync_channel(1);
        let worker = match thread::Builder::new()
            .name("sarun-prolog".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_main(receiver, initialized_tx)
                }));
                match outcome {
                    Ok(Ok(())) => ACTIVE.store(INACTIVE, Ordering::Release),
                    Ok(Err(_)) | Err(_) => ACTIVE.store(POISONED, Ordering::Release),
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                ACTIVE.store(INACTIVE, Ordering::Release);
                return Err(format!("failed to start Prolog worker: {error}"));
            }
        };

        match initialized_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let panicked = worker.join().is_err();
                if panicked {
                    ACTIVE.store(POISONED, Ordering::Release);
                }
                Err("Prolog worker stopped during initialization".into())
            }
        }
    }

    pub fn parse(
        &self,
        input: &GrammarInput,
        assist_edit: Option<&'static str>,
    ) -> Result<Vec<ParseCandidate>, String> {
        let mode = relation_mode(assist_edit)?;
        let reply = self.transform(&RelationRequest {
            grammar: action_grammar_handle(),
            given: vec![RelationBinding {
                name: "source".into(),
                value: relation_compound("source", vec![grammar_input_value(input)?, mode]),
            }],
            wanted: vec!["command".into(), "status".into(), "evidence".into()],
            observations: vec![],
            limits: RelationLimits::default(),
        })?;
        if reply.solutions.is_empty() && only_no_solution(&reply) {
            return Ok(Vec::new());
        }
        reject_relation_diagnostics(&reply)?;
        reply
            .solutions
            .iter()
            .map(decode_relation_parse_solution)
            .collect()
    }

    pub fn transform(&self, request: &RelationRequest) -> Result<RelationReply, String> {
        let output_limit = request.limits.max_output_bytes;
        let request = encode_relation_request(request)?;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(Command::Transform(request, output_limit, reply_tx))
            .map_err(|_| "Prolog worker has stopped".to_string())?;
        let value = reply_rx
            .recv()
            .map_err(|_| "Prolog worker stopped before replying".to_string())??;
        decode_relation_reply(value)
    }

    pub fn complete(
        &self,
        input: &GrammarInput,
        edit_id: &'static str,
    ) -> Result<Vec<Completion>, String> {
        let mode = relation_mode(Some(edit_id))?;
        let reply = self.transform(&RelationRequest {
            grammar: action_grammar_handle(),
            given: vec![RelationBinding {
                name: "source".into(),
                value: relation_compound("source", vec![grammar_input_value(input)?, mode]),
            }],
            wanted: vec!["completions".into()],
            observations: vec![],
            limits: RelationLimits {
                max_solutions: 256,
                ..RelationLimits::default()
            },
        })?;
        if reply.solutions.is_empty() && only_no_solution(&reply) {
            return Ok(Vec::new());
        }
        reject_relation_diagnostics(&reply)?;
        let Some(solution) = reply.solutions.first() else {
            return Ok(Vec::new());
        };
        decode_completions_value(solution_binding(solution, "completions")?)
    }

    pub fn highlights(&self, result: &ParseCandidate) -> Result<Vec<Highlight>, String> {
        let reply = self.transform(&RelationRequest {
            grammar: action_grammar_handle(),
            given: vec![
                RelationBinding {
                    name: "command".into(),
                    value: command_relation_value(&result.command),
                },
                RelationBinding {
                    name: "evidence".into(),
                    value: relation_list(result.evidence.iter().map(evidence_relation_value)),
                },
            ],
            wanted: vec!["highlights".into()],
            observations: vec![],
            limits: RelationLimits::default(),
        })?;
        reject_relation_diagnostics(&reply)?;
        decode_highlights_value(reply_binding(&reply, "highlights")?)
    }

    pub fn render(&self, command: &CommandAst) -> Result<RenderedCommand, QueryError> {
        let reply = self
            .transform(&RelationRequest {
                grammar: action_grammar_handle(),
                given: vec![RelationBinding {
                    name: "command".into(),
                    value: command_relation_value(command),
                }],
                wanted: vec!["source".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .map_err(QueryError::Backend)?;
        if reply.solutions.is_empty() && only_no_solution(&reply) {
            return Err(QueryError::NoSolution);
        }
        reject_relation_diagnostics(&reply).map_err(QueryError::Backend)?;
        let text = match reply_binding(&reply, "source").map_err(QueryError::Backend)? {
            RelationValue::String(text) => text.clone(),
            _ => {
                return Err(QueryError::Backend(
                    "relation returned non-text source".into(),
                ));
            }
        };
        Ok(RenderedCommand { text })
    }

    /// Project the complete action help surface from the normalized relation.
    /// This is the only runtime source for verb names, argument notation, and
    /// descriptions; implementation dispatch contributes no metadata.
    pub fn action_help(&self) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
        self.relation_action_help(None, None)
    }

    pub fn action_help_matching(
        &self,
        filter: &str,
    ) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
        self.relation_action_help(None, Some(filter))
    }

    pub fn ui_action_help(&self) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
        self.relation_action_help(Some("ui"), None)
    }

    pub fn ui_action_help_matching(
        &self,
        filter: &str,
    ) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
        self.relation_action_help(Some("ui"), Some(filter))
    }

    fn relation_action_help(
        &self,
        target: Option<&str>,
        filter: Option<&str>,
    ) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
        let mut given = Vec::new();
        if let Some(target) = target {
            given.push(RelationBinding {
                name: "action_target".into(),
                value: RelationValue::Atom(target.into()),
            });
        }
        if let Some(filter) = filter {
            given.push(RelationBinding {
                name: "help_filter".into(),
                value: RelationValue::String(filter.into()),
            });
        }
        let reply = self.transform(&RelationRequest {
            grammar: action_grammar_handle(),
            given,
            wanted: vec!["help".into()],
            observations: vec![],
            limits: RelationLimits {
                max_solutions: 256,
                ..RelationLimits::default()
            },
        })?;
        reject_relation_diagnostics(&reply)?;
        reply
            .solutions
            .iter()
            .map(|solution| {
                <crate::generated_wire::ActionHelpRow as crate::generated_wire::RelationWireValue>::from_relation(
                    solution_binding(solution, "help")?,
                )
            })
            .collect()
    }

    pub fn context_query(
        &self,
        query: &ContextQuery,
        snapshot: &ContextSnapshot,
    ) -> Result<Option<ContextResult>, String> {
        let reply = self.transform(&RelationRequest {
            grammar: RelationValue::Atom("context_grammar".into()),
            given: vec![
                RelationBinding {
                    name: "query".into(),
                    value: context_query_value(query),
                },
                RelationBinding {
                    name: "snapshot".into(),
                    value: context_snapshot_value(snapshot),
                },
            ],
            wanted: vec!["outcome".into()],
            observations: vec![],
            limits: RelationLimits::default(),
        })?;
        decode_context_outcome(&parsed_relation_value(reply_binding(&reply, "outcome")?))
    }

    pub fn observe_context(
        &self,
        id: &RelationValue,
        query: &ContextQuery,
        snapshot: &ContextSnapshot,
    ) -> Result<ContextObservation, String> {
        let reply = self.transform(&RelationRequest {
            grammar: RelationValue::Atom("context_grammar".into()),
            given: vec![
                RelationBinding {
                    name: "id".into(),
                    value: id.clone(),
                },
                RelationBinding {
                    name: "query".into(),
                    value: context_query_value(query),
                },
                RelationBinding {
                    name: "snapshot".into(),
                    value: context_snapshot_value(snapshot),
                },
            ],
            wanted: vec!["observation".into()],
            observations: vec![],
            limits: RelationLimits::default(),
        })?;
        decode_context_observation(&parsed_relation_value(reply_binding(
            &reply,
            "observation",
        )?))
    }

    pub fn ready_context_queries(
        &self,
        graph: &[ContextQueryNode],
        observations: &[ContextObservation],
    ) -> Result<Vec<ContextQueryNode>, String> {
        let reply = self.transform(&RelationRequest {
            grammar: RelationValue::Atom("context_grammar".into()),
            given: vec![RelationBinding {
                name: "graph".into(),
                value: context_graph_value(graph)?,
            }],
            wanted: vec!["ready".into()],
            observations: observations
                .iter()
                .map(context_observation_value)
                .collect::<Result<_, _>>()?,
            limits: RelationLimits::default(),
        })?;
        match reply_binding(&reply, "ready")? {
            RelationValue::List(values) => values
                .iter()
                .map(|value| decode_context_node(&parsed_relation_value(value)))
                .collect(),
            _ => Err("relation returned a non-list ready query binding".into()),
        }
    }

    pub fn context_dependency_keys(
        &self,
        observations: &[ContextObservation],
    ) -> Result<Vec<ContextDependencyKey>, String> {
        let reply = self.transform(&RelationRequest {
            grammar: RelationValue::Atom("context_grammar".into()),
            given: vec![],
            wanted: vec!["dependency_keys".into()],
            observations: observations
                .iter()
                .map(context_observation_value)
                .collect::<Result<_, _>>()?,
            limits: RelationLimits::default(),
        })?;
        reply
            .dependency_keys
            .iter()
            .map(|value| decode_context_dependency_key(&parsed_relation_value(value)))
            .collect()
    }

    pub fn context_plans(
        &self,
        input: &GrammarInput,
        assist_edit: Option<&'static str>,
    ) -> Result<Vec<ContextPlan>, String> {
        let reply = self.transform(&RelationRequest {
            grammar: action_grammar_handle(),
            given: vec![RelationBinding {
                name: "source".into(),
                value: relation_compound(
                    "source",
                    vec![grammar_input_value(input)?, relation_mode(assist_edit)?],
                ),
            }],
            wanted: vec!["command".into(), "status".into(), "evidence".into()],
            observations: vec![],
            limits: RelationLimits::default(),
        })?;
        if reply.solutions.is_empty() && only_no_solution(&reply) {
            return Ok(Vec::new());
        }
        reject_relation_diagnostics(&reply)?;
        let queries = decode_context_graph_values(&reply.context_queries)?;
        reply
            .solutions
            .iter()
            .map(|solution| {
                let candidate = decode_relation_parse_solution(solution)?;
                Ok(ContextPlan {
                    source: input.clone(),
                    assist_edit: assist_edit.map(str::to_owned),
                    command: candidate.command,
                    queries: queries.clone(),
                    evidence: candidate.evidence,
                    preference: candidate.preference,
                })
            })
            .collect()
    }

    pub fn resolve_context_plan(
        &self,
        plan: &ContextPlan,
        observations: &[ContextObservation],
    ) -> Result<CommandAst, QueryError> {
        let reply = self
            .transform(&RelationRequest {
                grammar: action_grammar_handle(),
                given: vec![RelationBinding {
                    name: "source".into(),
                    value: relation_compound(
                        "source",
                        vec![
                            grammar_input_value(&plan.source).map_err(QueryError::Backend)?,
                            relation_mode(plan.assist_edit.as_deref())
                                .map_err(QueryError::Backend)?,
                        ],
                    ),
                }],
                wanted: vec!["command".into()],
                observations: observations
                    .iter()
                    .map(context_observation_value)
                    .collect::<Result<_, _>>()
                    .map_err(QueryError::Backend)?,
                limits: RelationLimits::default(),
            })
            .map_err(QueryError::Backend)?;
        if reply.solutions.is_empty() && only_no_solution(&reply) {
            return Err(QueryError::NoSolution);
        }
        reject_relation_diagnostics(&reply).map_err(QueryError::Backend)?;
        for solution in &reply.solutions {
            let command = decode_command(&parsed_relation_value(
                solution_binding(solution, "command").map_err(QueryError::Backend)?,
            ))
            .map_err(QueryError::Backend)?;
            if command.action == plan.command.action {
                return Ok(command);
            }
        }
        Err(QueryError::NoSolution)
    }

    pub fn context_completion_plans(
        &self,
        input: &GrammarInput,
        edit_id: &'static str,
    ) -> Result<Vec<ContextCompletionPlan>, String> {
        let reply = self.transform(&RelationRequest {
            grammar: action_grammar_handle(),
            given: vec![RelationBinding {
                name: "source".into(),
                value: relation_compound(
                    "source",
                    vec![grammar_input_value(input)?, relation_mode(Some(edit_id))?],
                ),
            }],
            wanted: vec!["command".into(), "completions".into()],
            observations: vec![],
            limits: RelationLimits {
                max_solutions: 256,
                ..RelationLimits::default()
            },
        })?;
        if reply.context_queries.is_empty() {
            return Ok(Vec::new());
        }
        reject_relation_diagnostics(&reply)?;
        let queries = decode_context_graph_values(&reply.context_queries)?;
        let target_query_id = queries
            .iter()
            .rev()
            .find(|node| node.query.cardinality == ContextCardinality::All)
            .map(|node| node.id.clone())
            .ok_or("context completion relation omitted its all query")?;
        let solution = reply
            .solutions
            .first()
            .ok_or("context completion relation returned no parse witness")?;
        let command = decode_command(&parsed_relation_value(solution_binding(
            solution, "command",
        )?))?;
        let (replace, surface) = input
            .items
            .iter()
            .find_map(|item| match item {
                InputItem::EditTear { id, span, surface } if *id == edit_id => {
                    Some((*span, surface.clone()))
                }
                _ => None,
            })
            .ok_or("context completion input omitted its edit tear")?;
        Ok(vec![ContextCompletionPlan {
            source: input.clone(),
            edit_id: edit_id.into(),
            action: command.action,
            replace,
            surface,
            queries,
            target_query_id,
            preference: solution.preference,
        }])
    }

    pub fn resolve_context_completion(
        &self,
        plan: &ContextCompletionPlan,
        observations: &[ContextObservation],
    ) -> Result<Vec<Completion>, String> {
        let reply = self.transform(&RelationRequest {
            grammar: action_grammar_handle(),
            given: vec![RelationBinding {
                name: "source".into(),
                value: relation_compound(
                    "source",
                    vec![
                        grammar_input_value(&plan.source)?,
                        relation_mode(Some(&plan.edit_id))?,
                    ],
                ),
            }],
            wanted: vec!["command".into(), "completions".into()],
            observations: observations
                .iter()
                .map(context_observation_value)
                .collect::<Result<_, _>>()?,
            limits: RelationLimits {
                max_solutions: 256,
                ..RelationLimits::default()
            },
        })?;
        reject_relation_diagnostics(&reply)?;
        let solution = reply
            .solutions
            .iter()
            .find(|solution| {
                solution_binding(solution, "command")
                    .ok()
                    .and_then(|value| decode_command(&parsed_relation_value(value)).ok())
                    .is_some_and(|command| command.action == plan.action)
            })
            .ok_or("context completion relation lost its parse witness")?;
        decode_completions_value(solution_binding(solution, "completions")?)
    }

    pub fn shutdown(mut self) -> Result<(), String> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), String> {
        let result = if self.worker.is_some() {
            let (reply_tx, reply_rx) = mpsc::channel();
            if self.commands.send(Command::Shutdown(reply_tx)).is_ok() {
                match reply_rx.recv() {
                    Ok(result) => result,
                    Err(_) => Err("Prolog worker stopped during cleanup".into()),
                }
            } else {
                Err("Prolog worker stopped before cleanup".into())
            }
        } else {
            Ok(())
        };
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                ACTIVE.store(POISONED, Ordering::Release);
                return Err("Prolog worker panicked".into());
            }
        }
        result
    }

    #[cfg(test)]
    fn exhaust_inference_limit(&self) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(Command::ExhaustInferenceLimit(reply_tx))
            .map_err(|_| "Prolog worker has stopped".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "Prolog worker stopped before replying".to_string())?
    }
}

impl Drop for Prolog {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn worker_main(
    receiver: mpsc::Receiver<Command>,
    initialized: mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let mut runtime = match Runtime::initialize() {
        Ok(runtime) => {
            let _ = initialized.send(Ok(()));
            runtime
        }
        Err(error) => {
            let _ = initialized.send(Err(error.clone()));
            return Err(error);
        }
    };

    while let Ok(command) = receiver.recv() {
        match command {
            Command::Transform(request, output_limit, reply) => {
                let _ = reply.send(runtime.transform(&request, output_limit));
            }
            Command::Shutdown(reply) => {
                let result = runtime.cleanup();
                let _ = reply.send(result.clone());
                return result;
            }
            #[cfg(test)]
            Command::ExhaustInferenceLimit(reply) => {
                let _ = reply.send(runtime.exhaust_inference_limit());
            }
        }
    }
    runtime.cleanup()
}

struct Runtime {
    // SWI retains its original argv pointers until cleanup.
    _arguments: Vec<Vec<u8>>,
    _argv: Vec<*mut c_char>,
    initialized: bool,
    cleanup_attempted: bool,
}

impl Runtime {
    fn initialize() -> Result<Self, String> {
        let mut arguments = vec![
            b"sarun-prolog\0".to_vec(),
            b"--quiet\0".to_vec(),
            b"--no-signals\0".to_vec(),
        ];
        let mut argv: Vec<*mut c_char> = arguments
            .iter_mut()
            .map(|argument| argument.as_mut_ptr().cast())
            .collect();
        argv.push(ptr::null_mut());

        // SAFETY: the resource and argv remain valid for the runtime's
        // lifetime, and this worker is the only thread entering SWI-Prolog.
        unsafe {
            if PL_set_resource_db_mem(APP_RESOURCE.as_ptr(), APP_RESOURCE.len()) == 0 {
                return Err("SWI-Prolog rejected the embedded application resource".into());
            }
            if PL_initialise(arguments.len() as c_int, argv.as_mut_ptr()) == 0 {
                let cleanup = PL_cleanup(1 | PL_CLEANUP_NO_CANCEL);
                return Err(format!(
                    "SWI-Prolog initialization failed (cleanup status {cleanup})"
                ));
            }
        }

        let mut runtime = Self {
            _arguments: arguments,
            _argv: argv,
            initialized: true,
            cleanup_attempted: false,
        };
        runtime.load_grammar()?;
        Ok(runtime)
    }

    fn load_grammar(&mut self) -> Result<(), String> {
        let goal = format!(
            concat!(
                "call_with_inference_limit((asserta(user:file_search_path(library,'res://library')),",
                "load_files('res://app/relation_api.pl',[silent(true)]),",
                "load_files('res://app/action_grammar.pl',[silent(true)]),",
                "load_files('res://app/brush_grammar.pl',[silent(true)]),",
                "action_grammar:valid_transport_catalog,",
                "action_grammar:valid_action_catalog,",
                "action_grammar:action_relation_grammar(ActionGrammar),",
                "grammar_store:install_grammar(sarun_actions,ActionGrammar),",
                "brush_grammar:brush_syntax_grammar(BrushSyntax),",
                "grammar_ir:valid_grammar(BrushSyntax),",
                "brush_grammar:brush_state_rules(BrushStateRules),",
                "ast_state_relation:valid_ast_state_rules(BrushStateRules),",
                "brush_grammar:brush_relation_grammar(BrushGrammar),",
                "grammar_store:install_grammar(sarun_brush,BrushGrammar)),",
                "{},R),R\\==inference_limit_exceeded"
            ),
            LOAD_INFERENCES
        );
        self.call_fixed_once(&goal, "loading embedded action grammar")
    }

    fn call_fixed_once(&mut self, goal: &str, operation: &str) -> Result<(), String> {
        // SAFETY: all terms and the query are confined to the initialized
        // worker. `goal` is assembled only from fixed application text.
        unsafe {
            let terms = PL_new_term_refs(1);
            if terms == 0 {
                return Err(format!("FLI term allocation failed while {operation}"));
            }
            if put_utf8_term(terms, goal) == 0 {
                PL_clear_exception();
                PL_reset_term_refs(terms);
                return Err(format!("invalid embedded Prolog term while {operation}"));
            }
            let predicate = PL_predicate(c"call".as_ptr(), 1, c"system".as_ptr());
            if predicate.is_null() {
                PL_reset_term_refs(terms);
                return Err(format!("FLI predicate allocation failed while {operation}"));
            }
            let query = PL_open_query(
                ptr::null_mut(),
                PL_Q_NODEBUG | PL_Q_CATCH_EXCEPTION,
                predicate,
                terms,
            );
            if query == 0 {
                PL_reset_term_refs(terms);
                return Err(format!("FLI query allocation failed while {operation}"));
            }
            let succeeded = PL_next_solution(query) != 0;
            let exception = !succeeded && PL_exception(query) != 0;
            let closed = if succeeded {
                PL_cut_query(query)
            } else {
                PL_close_query(query)
            };
            if exception || closed != 1 {
                PL_clear_exception();
            }
            PL_reset_term_refs(terms);
            if closed != 1 {
                return Err(format!("FLI query cleanup failed while {operation}"));
            }
            if exception {
                Err(format!("Prolog exception while {operation}"))
            } else if !succeeded {
                Err(format!("Prolog goal failed while {operation}"))
            } else {
                Ok(())
            }
        }
    }

    fn transform(
        &mut self,
        request: &RelationValue,
        output_limit: usize,
    ) -> Result<RelationValue, String> {
        // SAFETY: the predicate and module are fixed. The generic request is
        // recursively constructed as an FLI term; no Prolog source is parsed.
        unsafe {
            let terms = PL_new_term_refs(2);
            if terms == 0 {
                return Err("FLI term allocation failed for relation transform".into());
            }
            let mut put_budget = RelationTermBudget::new(MAX_INPUT_BYTES);
            if put_relation_value(terms, request, 0, &mut put_budget) == 0
                || PL_put_variable(terms + 1) == 0
            {
                PL_clear_exception();
                PL_reset_term_refs(terms);
                return Err("failed to construct relation transform terms".into());
            }
            let predicate = PL_predicate(c"transform".as_ptr(), 2, c"relation_api".as_ptr());
            if predicate.is_null() {
                PL_reset_term_refs(terms);
                return Err("FLI relation predicate allocation failed".into());
            }
            let query = PL_open_query(
                ptr::null_mut(),
                PL_Q_NODEBUG | PL_Q_CATCH_EXCEPTION,
                predicate,
                terms,
            );
            if query == 0 {
                PL_reset_term_refs(terms);
                return Err("FLI relation query allocation failed".into());
            }
            let succeeded = PL_next_solution(query) != 0;
            let exception = !succeeded && PL_exception(query) != 0;
            let result = if succeeded {
                let mut get_budget = RelationTermBudget::new(output_limit);
                get_relation_value(terms + 1, 0, &mut get_budget)
            } else if exception {
                Err("relation transform raised a Prolog exception".into())
            } else {
                Err("relation transform failed".into())
            };
            let closed = if succeeded {
                PL_cut_query(query)
            } else {
                PL_close_query(query)
            };
            if exception || closed != 1 {
                PL_clear_exception();
            }
            PL_reset_term_refs(terms);
            if closed != 1 {
                return Err("FLI relation query cleanup failed".into());
            }
            result
        }
    }

    #[cfg(test)]
    fn exhaust_inference_limit(&mut self) -> Result<(), String> {
        // `repeat/0` is fixed test-only text. It proves the same bounded query
        // mechanism used by typed operations leaves the worker recoverable.
        unsafe {
            let terms = PL_new_term_refs(3);
            if terms == 0 {
                return Err("FLI term allocation failed".into());
            }
            if put_utf8_term(terms, "(repeat,fail)") == 0
                || PL_put_int64(terms + 1, 1_000) == 0
                || PL_put_variable(terms + 2) == 0
            {
                PL_clear_exception();
                PL_reset_term_refs(terms);
                return Err("failed to construct test query".into());
            }
            let predicate =
                PL_predicate(c"call_with_inference_limit".as_ptr(), 3, c"system".as_ptr());
            if predicate.is_null() {
                PL_reset_term_refs(terms);
                return Err("FLI predicate allocation failed".into());
            }
            let query = PL_open_query(
                ptr::null_mut(),
                PL_Q_NODEBUG | PL_Q_CATCH_EXCEPTION,
                predicate,
                terms,
            );
            if query == 0 {
                PL_reset_term_refs(terms);
                return Err("FLI query allocation failed".into());
            }
            let succeeded = PL_next_solution(query) != 0;
            let result = if succeeded {
                get_utf8(terms + 2, CVT_ATOM)
            } else {
                Err("bounded test query failed".into())
            };
            let closed = if succeeded {
                PL_cut_query(query)
            } else {
                PL_close_query(query)
            };
            PL_reset_term_refs(terms);
            if closed != 1 {
                return Err("FLI test query cleanup failed".into());
            }
            match result?.as_str() {
                "inference_limit_exceeded" => Ok(()),
                other => Err(format!("unexpected inference-limit result: {other}")),
            }
        }
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if !self.initialized || self.cleanup_attempted {
            return if self.initialized {
                Err("SWI-Prolog cleanup was already attempted".into())
            } else {
                Ok(())
            };
        }
        self.cleanup_attempted = true;
        // SAFETY: cleanup runs once on the worker that initialized SWI.
        let status = unsafe { PL_cleanup(PL_CLEANUP_NO_CANCEL) };
        if status == PL_CLEANUP_SUCCESS {
            self.initialized = false;
            Ok(())
        } else {
            Err(format!("SWI-Prolog cleanup failed with status {status}"))
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if self.initialized && !self.cleanup_attempted && self.cleanup().is_err() {
            ACTIVE.store(POISONED, Ordering::Release);
        }
    }
}

struct RelationTermBudget {
    nodes: usize,
    bytes: usize,
    max_bytes: usize,
}

impl RelationTermBudget {
    fn new(max_bytes: usize) -> Self {
        Self {
            nodes: 0,
            bytes: 0,
            max_bytes,
        }
    }

    fn enter(&mut self, depth: usize, bytes: usize) -> bool {
        self.nodes = match self.nodes.checked_add(1) {
            Some(nodes) => nodes,
            None => return false,
        };
        self.bytes = match self.bytes.checked_add(bytes) {
            Some(total) => total,
            None => return false,
        };
        depth <= MAX_RELATION_DEPTH
            && self.nodes <= MAX_RELATION_NODES
            && self.bytes <= self.max_bytes
    }
}

unsafe fn put_relation_value(
    term: Term,
    value: &RelationValue,
    depth: usize,
    budget: &mut RelationTermBudget,
) -> c_int {
    let own_bytes = match value {
        RelationValue::Atom(value)
        | RelationValue::String(value)
        | RelationValue::Compound(value, _) => value.len(),
        RelationValue::Integer(_) | RelationValue::List(_) => 0,
    };
    if !budget.enter(depth, own_bytes) {
        return 0;
    }
    match value {
        RelationValue::Atom(value) => unsafe {
            PL_put_chars(
                term,
                PL_ATOM_TYPE | REP_UTF8 as c_int,
                value.len(),
                value.as_ptr().cast(),
            )
        },
        RelationValue::String(value) => unsafe {
            PL_put_chars(
                term,
                PL_STRING_TYPE | REP_UTF8 as c_int,
                value.len(),
                value.as_ptr().cast(),
            )
        },
        RelationValue::Integer(value) => unsafe { PL_put_int64(term, *value) },
        RelationValue::Compound(name, arguments) => unsafe {
            if checked_atom(name).is_none() || arguments.is_empty() || arguments.len() > MAX_ITEMS {
                return 0;
            }
            let argument_terms = PL_new_term_refs(arguments.len());
            if argument_terms == 0 {
                return 0;
            }
            for (index, argument) in arguments.iter().enumerate() {
                if put_relation_value(argument_terms + index, argument, depth + 1, budget) == 0 {
                    PL_reset_term_refs(argument_terms);
                    return 0;
                }
            }
            let atom = PL_new_atom_nchars(name.len(), name.as_ptr().cast());
            if atom == 0 {
                PL_reset_term_refs(argument_terms);
                return 0;
            }
            let functor = PL_new_functor_sz(atom, arguments.len());
            PL_unregister_atom(atom);
            let result = if functor == 0 {
                0
            } else {
                PL_cons_functor_v(term, functor, argument_terms)
            };
            PL_reset_term_refs(argument_terms);
            result
        },
        RelationValue::List(values) => unsafe {
            if values.len() > MAX_RELATION_NODES || PL_put_nil(term) == 0 {
                return 0;
            }
            let temporary = PL_new_term_refs(2);
            if temporary == 0 {
                return 0;
            }
            for value in values.iter().rev() {
                if put_relation_value(temporary, value, depth + 1, budget) == 0
                    || PL_put_term(temporary + 1, term) == 0
                    || PL_cons_list(term, temporary, temporary + 1) == 0
                {
                    PL_reset_term_refs(temporary);
                    return 0;
                }
            }
            PL_reset_term_refs(temporary);
            1
        },
    }
}

unsafe fn get_relation_value(
    term: Term,
    depth: usize,
    budget: &mut RelationTermBudget,
) -> Result<RelationValue, String> {
    let kind = unsafe { PL_term_type(term) };
    match kind {
        PL_ATOM_TYPE => {
            let value = unsafe { get_utf8(term, CVT_ATOM) }?;
            if !budget.enter(depth, value.len()) {
                return Err("relation reply exceeds structural bounds".into());
            }
            Ok(RelationValue::Atom(value))
        }
        PL_STRING_TYPE => {
            let value = unsafe { get_utf8(term, CVT_STRING) }?;
            if !budget.enter(depth, value.len()) {
                return Err("relation reply exceeds structural bounds".into());
            }
            Ok(RelationValue::String(value))
        }
        PL_INTEGER_TYPE => {
            if !budget.enter(depth, 0) {
                return Err("relation reply exceeds structural bounds".into());
            }
            let mut value = 0i64;
            if unsafe { PL_get_int64(term, &mut value) } == 0 {
                return Err("invalid integer in relation reply".into());
            }
            Ok(RelationValue::Integer(value))
        }
        PL_NIL_TYPE => {
            if !budget.enter(depth, 0) || unsafe { PL_get_nil(term) } == 0 {
                return Err("invalid empty list in relation reply".into());
            }
            Ok(RelationValue::List(Vec::new()))
        }
        PL_LIST_PAIR_TYPE => unsafe {
            if !budget.enter(depth, 0) {
                return Err("relation reply exceeds structural bounds".into());
            }
            let temporary = PL_new_term_refs(2);
            if temporary == 0 || PL_put_term(temporary + 1, term) == 0 {
                return Err("FLI list traversal allocation failed".into());
            }
            let mut values = Vec::new();
            while PL_term_type(temporary + 1) == PL_LIST_PAIR_TYPE {
                if values.len() >= MAX_RELATION_NODES
                    || PL_get_list(temporary + 1, temporary, temporary + 1) == 0
                {
                    PL_reset_term_refs(temporary);
                    return Err("invalid or oversized list in relation reply".into());
                }
                values.push(get_relation_value(temporary, depth + 1, budget)?);
            }
            if PL_get_nil(temporary + 1) == 0 {
                PL_reset_term_refs(temporary);
                return Err("improper list in relation reply".into());
            }
            PL_reset_term_refs(temporary);
            Ok(RelationValue::List(values))
        },
        PL_COMPOUND_TYPE => unsafe {
            let mut atom = 0usize;
            let mut arity = 0usize;
            if PL_get_name_arity_sz(term, &mut atom, &mut arity) == 0
                || arity == 0
                || arity > MAX_ITEMS
            {
                return Err("invalid compound in relation reply".into());
            }
            let mut name_len = 0usize;
            let name_pointer = PL_atom_nchars(atom, &mut name_len);
            if name_pointer.is_null() {
                return Err("invalid functor name in relation reply".into());
            }
            let name_bytes = std::slice::from_raw_parts(name_pointer.cast::<u8>(), name_len);
            let name = std::str::from_utf8(name_bytes)
                .map_err(|_| "non-UTF-8 functor in relation reply")?
                .to_string();
            if checked_atom(&name).is_none() || !budget.enter(depth, name.len()) {
                return Err("relation reply functor exceeds bounds".into());
            }
            let argument = PL_new_term_refs(1);
            if argument == 0 {
                return Err("FLI compound traversal allocation failed".into());
            }
            let mut arguments = Vec::with_capacity(arity);
            for index in 1..=arity {
                if PL_get_arg_sz(index, term, argument) == 0 {
                    PL_reset_term_refs(argument);
                    return Err("invalid compound argument in relation reply".into());
                }
                arguments.push(get_relation_value(argument, depth + 1, budget)?);
            }
            PL_reset_term_refs(argument);
            Ok(RelationValue::Compound(name, arguments))
        },
        _ => Err("unsupported value type in relation reply".into()),
    }
}

unsafe fn put_utf8_term(term: Term, text: &str) -> c_int {
    unsafe { PL_put_term_from_chars(term, REP_UTF8 as c_int, text.len(), text.as_ptr().cast()) }
}

unsafe fn get_utf8(term: Term, conversion: u32) -> Result<String, String> {
    let mut len = 0;
    let mut text = ptr::null_mut();
    let status = unsafe {
        PL_get_nchars(
            term,
            &mut len,
            &mut text,
            conversion | CVT_EXCEPTION | BUF_MALLOC | REP_UTF8,
        )
    };
    if status == 0 || text.is_null() {
        return Err("failed to extract UTF-8 from Prolog term".into());
    }
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), len) };
    let result =
        String::from_utf8(bytes.to_vec()).map_err(|_| "Prolog returned invalid UTF-8".to_string());
    unsafe { PL_free(text.cast()) };
    result
}

fn checked_atom(atom: &str) -> Option<&str> {
    (!atom.is_empty()
        && atom
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && atom.as_bytes()[0].is_ascii_lowercase())
    .then_some(atom)
}

fn relation_compound(name: &str, arguments: Vec<RelationValue>) -> RelationValue {
    RelationValue::Compound(name.to_string(), arguments)
}

fn relation_list(values: impl IntoIterator<Item = RelationValue>) -> RelationValue {
    RelationValue::List(values.into_iter().collect())
}

fn action_grammar_handle() -> RelationValue {
    relation_compound(
        "grammar_handle",
        vec![RelationValue::Atom("sarun_actions".into())],
    )
}

#[cfg(test)]
fn brush_grammar_handle() -> RelationValue {
    relation_compound(
        "grammar_handle",
        vec![RelationValue::Atom("sarun_brush".into())],
    )
}

fn relation_mode(assist_edit: Option<&str>) -> Result<RelationValue, String> {
    match assist_edit {
        None => Ok(RelationValue::Atom("exact".into())),
        Some(id) => checked_atom(id)
            .map(|id| relation_compound("assist", vec![RelationValue::Atom(id.into())]))
            .ok_or_else(|| "invalid edit tear id".into()),
    }
}

fn relation_span(span: Span) -> Result<RelationValue, String> {
    Ok(relation_compound(
        "span",
        vec![
            bounded_relation_integer(span.start, "span start")?,
            bounded_relation_integer(span.end, "span end")?,
        ],
    ))
}

fn grammar_input_value(input: &GrammarInput) -> Result<RelationValue, String> {
    if input.items.len() > MAX_ITEMS {
        return Err(format!("action grammar input exceeds {MAX_ITEMS} items"));
    }
    let mut previous_end = 0;
    let mut items = Vec::with_capacity(input.items.len() + 1);
    for item in &input.items {
        let span = match item {
            InputItem::Unit(unit) => unit.span,
            InputItem::EditTear { span, .. } | InputItem::SourceTear { span, .. } => *span,
        };
        if span.start < previous_end || span.start > span.end || span.end > input.end {
            return Err("action grammar input spans are invalid".into());
        }
        previous_end = span.end;
        let value = match item {
            InputItem::Unit(unit) => {
                if unit.paint_spans.iter().any(|paint| {
                    paint.start < unit.span.start
                        || paint.start > paint.end
                        || paint.end > unit.span.end
                }) {
                    return Err("action grammar unit has an invalid paint span".into());
                }
                let semantic = match &unit.semantic {
                    Semantic::Atom(value) => RelationValue::Atom(value.clone()),
                    Semantic::Integer(value) => {
                        relation_compound("integer", vec![RelationValue::Integer(*value)])
                    }
                    Semantic::Text(value) => {
                        relation_compound("text", vec![RelationValue::String(value.clone())])
                    }
                };
                relation_compound(
                    "unit",
                    vec![
                        semantic,
                        relation_span(unit.span)?,
                        relation_list(
                            unit.paint_spans
                                .iter()
                                .copied()
                                .map(relation_span)
                                .collect::<Result<Vec<_>, _>>()?,
                        ),
                        RelationValue::String(unit.surface.clone()),
                        RelationValue::Atom(unit.syntax.clone()),
                        RelationValue::Atom(unit.provider.clone()),
                        RelationValue::Integer(unit.preference),
                        RelationValue::Atom("lexer".into()),
                    ],
                )
            }
            InputItem::EditTear { id, span, surface } => {
                let id = checked_atom(id).ok_or("invalid edit tear id")?;
                relation_compound(
                    "edit_tear",
                    vec![
                        RelationValue::Atom(id.into()),
                        relation_span(*span)?,
                        RelationValue::String(surface.clone()),
                    ],
                )
            }
            InputItem::SourceTear { id, span, surface } => relation_compound(
                "source_tear",
                vec![
                    RelationValue::Atom(format!("source{id}")),
                    relation_span(*span)?,
                    RelationValue::String(surface.clone()),
                ],
            ),
        };
        items.push(value);
    }
    items.push(relation_compound(
        "end",
        vec![bounded_relation_integer(input.end, "input end")?],
    ));
    Ok(relation_list(items))
}

fn command_value_relation(value: &CommandValue) -> RelationValue {
    match value {
        CommandValue::Integer(value) => {
            relation_compound("integer", vec![RelationValue::Integer(*value)])
        }
        CommandValue::Boolean(value) => {
            relation_compound("boolean", vec![RelationValue::Atom(value.to_string())])
        }
        CommandValue::String(value) => {
            relation_compound("string", vec![RelationValue::String(value.clone())])
        }
        CommandValue::Path(value) => {
            relation_compound("path", vec![RelationValue::String(value.clone())])
        }
        CommandValue::Base64(value) => {
            relation_compound("base64", vec![RelationValue::String(value.clone())])
        }
        CommandValue::Spec(value) => {
            relation_compound("spec", vec![RelationValue::String(value.clone())])
        }
        CommandValue::OciSpec {
            context_tar_gz,
            dockerfile,
            tag,
            net_mode,
            build_arguments,
        } => relation_compound(
            "oci_spec",
            vec![
                RelationValue::String(context_tar_gz.clone()),
                RelationValue::String(dockerfile.clone()),
                tag.as_ref().map_or_else(
                    || RelationValue::Atom("none".into()),
                    |value| relation_compound("some", vec![RelationValue::String(value.clone())]),
                ),
                RelationValue::String(net_mode.clone()),
                relation_list(build_arguments.iter().map(|(key, value)| {
                    relation_compound(
                        "pair",
                        vec![
                            RelationValue::String(key.clone()),
                            RelationValue::String(value.clone()),
                        ],
                    )
                })),
            ],
        ),
        CommandValue::ApiSpec {
            base_url,
            model,
            api_key,
        } => relation_compound(
            "api_spec",
            vec![
                RelationValue::String(base_url.clone()),
                RelationValue::String(model.clone()),
                RelationValue::String(api_key.clone()),
            ],
        ),
        CommandValue::Array(values) => relation_compound(
            "array",
            vec![relation_list(values.iter().map(command_value_relation))],
        ),
        CommandValue::Hole { name, kind } => relation_compound(
            "hole",
            vec![
                RelationValue::Atom(name.clone()),
                RelationValue::Atom(kind.clone()),
            ],
        ),
    }
}

fn command_relation_value(command: &CommandAst) -> RelationValue {
    relation_compound(
        "command",
        vec![
            RelationValue::Atom(command.action.clone()),
            RelationValue::Atom(command.handler.clone()),
            RelationValue::Atom(command.target.clone()),
            relation_list(command.args.iter().map(command_value_relation)),
        ],
    )
}

fn evidence_relation_value(evidence: &Evidence) -> RelationValue {
    relation_compound(
        "evidence",
        vec![
            RelationValue::Atom(evidence.semantic.clone()),
            relation_span(evidence.span).expect("decoded evidence span remains bounded"),
            relation_list(evidence.paint_spans.iter().copied().map(|span| {
                relation_span(span).expect("decoded evidence paint span remains bounded")
            })),
            RelationValue::String(evidence.surface.clone()),
            RelationValue::Atom(evidence.syntax.clone()),
            RelationValue::Atom(evidence.provider.clone()),
            RelationValue::Integer(evidence.preference),
            RelationValue::Atom(evidence.origin.clone()),
        ],
    )
}

fn encode_relation_binding(binding: &RelationBinding) -> Result<RelationValue, String> {
    let name = checked_atom(&binding.name).ok_or("invalid relation binding name")?;
    Ok(relation_compound(
        "binding",
        vec![RelationValue::Atom(name.to_string()), binding.value.clone()],
    ))
}

fn bounded_relation_integer(value: usize, field: &str) -> Result<RelationValue, String> {
    i64::try_from(value)
        .map(RelationValue::Integer)
        .map_err(|_| format!("relation {field} exceeds signed 64-bit range"))
}

fn encode_relation_request(request: &RelationRequest) -> Result<RelationValue, String> {
    if request.given.len() > MAX_ITEMS
        || request.wanted.len() > MAX_ITEMS
        || request.observations.len() > MAX_ITEMS
    {
        return Err(format!(
            "relation request exceeds {MAX_ITEMS} envelope items"
        ));
    }
    if request.limits.max_solutions == 0 || request.limits.max_solutions > 1024 {
        return Err("relation solution limit must be between 1 and 1024".into());
    }
    if request.limits.max_evidence > MAX_RELATION_NODES {
        return Err(format!(
            "relation evidence limit exceeds {MAX_RELATION_NODES}"
        ));
    }
    if request.limits.max_output_bytes == 0 || request.limits.max_output_bytes > MAX_OUTPUT_BYTES {
        return Err(format!(
            "relation output limit must be between 1 and {MAX_OUTPUT_BYTES} bytes"
        ));
    }
    let given = request
        .given
        .iter()
        .map(encode_relation_binding)
        .collect::<Result<Vec<_>, _>>()?;
    let wanted = request
        .wanted
        .iter()
        .map(|name| {
            checked_atom(name)
                .map(|name| RelationValue::Atom(name.to_string()))
                .ok_or_else(|| "invalid wanted relation binding name".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(relation_compound(
        "request",
        vec![
            request.grammar.clone(),
            relation_compound("given", vec![relation_list(given)]),
            relation_compound("want", vec![relation_list(wanted)]),
            relation_compound(
                "observations",
                vec![relation_list(request.observations.clone())],
            ),
            relation_compound(
                "limits",
                vec![
                    bounded_relation_integer(request.limits.max_solutions, "solution limit")?,
                    bounded_relation_integer(request.limits.max_evidence, "evidence limit")?,
                    bounded_relation_integer(request.limits.max_output_bytes, "output limit")?,
                ],
            ),
        ],
    ))
}

fn take_relation_compound(
    value: RelationValue,
    expected: &str,
    arity: usize,
) -> Result<Vec<RelationValue>, String> {
    match value {
        RelationValue::Compound(name, arguments)
            if name == expected && arguments.len() == arity =>
        {
            Ok(arguments)
        }
        _ => Err(format!("invalid {expected}/{arity} in relation reply")),
    }
}

fn take_relation_list(
    value: RelationValue,
    description: &str,
) -> Result<Vec<RelationValue>, String> {
    match value {
        RelationValue::List(values) => Ok(values),
        _ => Err(format!("expected {description} list in relation reply")),
    }
}

fn decode_relation_binding_value(value: RelationValue) -> Result<RelationBinding, String> {
    let mut arguments = take_relation_compound(value, "binding", 2)?;
    let value = arguments.pop().unwrap();
    let name = match arguments.pop().unwrap() {
        RelationValue::Atom(name) if checked_atom(&name).is_some() => name,
        _ => return Err("invalid binding name in relation reply".into()),
    };
    Ok(RelationBinding { name, value })
}

fn decode_relation_solution(value: RelationValue) -> Result<RelationSolution, String> {
    let mut arguments = take_relation_compound(value, "solution", 2)?;
    let preference = match arguments.pop().unwrap() {
        RelationValue::Integer(value) => value,
        _ => return Err("invalid solution preference in relation reply".into()),
    };
    let bindings = take_relation_list(arguments.pop().unwrap(), "binding")?
        .into_iter()
        .map(decode_relation_binding_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RelationSolution {
        bindings,
        preference,
    })
}

fn decode_relation_reply(value: RelationValue) -> Result<RelationReply, String> {
    let arguments = take_relation_compound(value, "reply", 4)?;
    let mut fields = arguments.into_iter();
    let solutions = take_relation_list(fields.next().unwrap(), "solution")?
        .into_iter()
        .map(decode_relation_solution)
        .collect::<Result<Vec<_>, _>>()?;
    let context_queries = take_relation_list(fields.next().unwrap(), "context query")?;
    let dependency_keys = take_relation_list(fields.next().unwrap(), "dependency key")?;
    let diagnostics = take_relation_list(fields.next().unwrap(), "diagnostic")?;
    Ok(RelationReply {
        solutions,
        context_queries,
        dependency_keys,
        diagnostics,
    })
}

fn reply_binding<'a>(reply: &'a RelationReply, name: &str) -> Result<&'a RelationValue, String> {
    let solution = reply.solutions.first().ok_or_else(|| {
        format!(
            "relation returned no solution for {name}: {:?}",
            reply.diagnostics
        )
    })?;
    solution
        .bindings
        .iter()
        .find(|binding| binding.name == name)
        .map(|binding| &binding.value)
        .ok_or_else(|| format!("relation solution omitted requested {name} binding"))
}

fn solution_binding<'a>(
    solution: &'a RelationSolution,
    name: &str,
) -> Result<&'a RelationValue, String> {
    solution
        .bindings
        .iter()
        .find(|binding| binding.name == name)
        .map(|binding| &binding.value)
        .ok_or_else(|| format!("relation solution omitted requested {name} binding"))
}

fn only_no_solution(reply: &RelationReply) -> bool {
    reply.diagnostics.as_slice()
        == [relation_compound(
            "diagnostic",
            vec![RelationValue::Atom("no_solution".into())],
        )]
}

fn reject_relation_diagnostics(reply: &RelationReply) -> Result<(), String> {
    if reply.diagnostics.is_empty() || only_no_solution(reply) {
        Ok(())
    } else {
        Err(format!(
            "action relation diagnostics: {:?}",
            reply.diagnostics
        ))
    }
}

fn decode_relation_parse_solution(solution: &RelationSolution) -> Result<ParseCandidate, String> {
    let command = decode_command(&parsed_relation_value(solution_binding(
        solution, "command",
    )?))?;
    let status = match solution_binding(solution, "status")? {
        RelationValue::Atom(value) if value == "complete" => ParseStatus::Complete,
        RelationValue::Compound(name, values) if name == "incomplete" && values.len() == 1 => {
            match &values[0] {
                RelationValue::Compound(edit, id) if edit == "edit" && id.len() == 1 => {
                    let RelationValue::Atom(id) = &id[0] else {
                        return Err("relation returned invalid edit status".into());
                    };
                    ParseStatus::Incomplete {
                        edit_id: id.clone(),
                    }
                }
                _ => return Err("relation returned invalid incomplete status".into()),
            }
        }
        _ => return Err("relation returned invalid parse status".into()),
    };
    let evidence = match solution_binding(solution, "evidence")? {
        RelationValue::List(values) => values
            .iter()
            .map(|value| decode_evidence(&parsed_relation_value(value)))
            .collect::<Result<_, _>>()?,
        _ => return Err("relation returned non-list evidence".into()),
    };
    Ok(ParseCandidate {
        command,
        status,
        evidence,
        preference: solution.preference,
    })
}

fn context_query_value(query: &ContextQuery) -> RelationValue {
    let cardinality = match query.cardinality {
        ContextCardinality::Empty => "empty",
        ContextCardinality::One => "one",
        ContextCardinality::All => "all",
    };
    relation_compound(
        "ask",
        vec![
            RelationValue::Atom(cardinality.into()),
            query.domain.clone(),
            query.selector.clone(),
        ],
    )
}

fn context_entry_value(entry: &ContextEntry) -> RelationValue {
    relation_compound(
        "entry",
        vec![
            entry.domain.clone(),
            entry.identity.clone(),
            relation_list(entry.names.iter().cloned().map(RelationValue::String)),
            entry.value.clone(),
            relation_list(entry.attributes.clone()),
        ],
    )
}

fn context_snapshot_value(snapshot: &ContextSnapshot) -> RelationValue {
    relation_compound(
        "snapshot",
        vec![
            relation_compound(
                "source",
                vec![snapshot.provider.clone(), snapshot.revision.clone()],
            ),
            relation_list(snapshot.entries.iter().map(context_entry_value)),
        ],
    )
}

fn context_result_value(result: &ContextResult) -> RelationValue {
    match result {
        ContextResult::Empty(value) => relation_compound(
            "empty",
            vec![RelationValue::Atom(
                if *value { "true" } else { "false" }.into(),
            )],
        ),
        ContextResult::One(entry) => relation_compound("one", vec![context_entry_value(entry)]),
        ContextResult::All(entries) => relation_compound(
            "all",
            vec![relation_list(entries.iter().map(context_entry_value))],
        ),
    }
}

fn context_observation_value(observation: &ContextObservation) -> Result<RelationValue, String> {
    let outcome = observation.outcome.as_ref().map_or_else(
        || RelationValue::Atom("none".into()),
        |result| relation_compound("some", vec![context_result_value(result)]),
    );
    Ok(relation_compound(
        "observed",
        vec![
            observation.id.clone(),
            context_query_value(&observation.query),
            relation_compound(
                "source",
                vec![observation.provider.clone(), observation.revision.clone()],
            ),
            outcome,
        ],
    ))
}

fn context_graph_value(graph: &[ContextQueryNode]) -> Result<RelationValue, String> {
    graph
        .iter()
        .map(|node| {
            Ok(relation_compound(
                "query",
                vec![node.id.clone(), context_query_value(&node.query)],
            ))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(relation_list)
}

fn decode_context_graph_values(values: &[RelationValue]) -> Result<Vec<ContextQueryNode>, String> {
    values
        .iter()
        .map(|value| decode_context_node(&parsed_relation_value(value)))
        .collect()
}

fn parsed_relation_value(value: &RelationValue) -> ParsedTerm {
    match value {
        RelationValue::Atom(value) => ParsedTerm::Atom(value.clone()),
        RelationValue::String(value) => ParsedTerm::String(value.clone()),
        RelationValue::Integer(value) => ParsedTerm::Integer(*value),
        RelationValue::Compound(name, arguments) => ParsedTerm::Compound(
            name.clone(),
            arguments.iter().map(parsed_relation_value).collect(),
        ),
        RelationValue::List(values) => {
            ParsedTerm::List(values.iter().map(parsed_relation_value).collect())
        }
    }
}

#[derive(Clone, Debug)]
enum ParsedTerm {
    Atom(String),
    String(String),
    Integer(i64),
    Compound(String, Vec<ParsedTerm>),
    List(Vec<ParsedTerm>),
}

fn compound<'a>(
    term: &'a ParsedTerm,
    name: &str,
    arity: usize,
) -> Result<&'a [ParsedTerm], String> {
    match term {
        ParsedTerm::Compound(actual, args) if actual == name && args.len() == arity => Ok(args),
        _ => Err(format!("invalid {name}/{arity} in action grammar response")),
    }
}

fn list(term: &ParsedTerm) -> Result<&[ParsedTerm], String> {
    match term {
        ParsedTerm::List(values) => Ok(values),
        _ => Err("expected list in action grammar response".into()),
    }
}

fn atom(term: &ParsedTerm) -> Result<&str, String> {
    match term {
        ParsedTerm::Atom(value) => Ok(value),
        _ => Err("expected atom in action grammar response".into()),
    }
}

fn integer(term: &ParsedTerm) -> Result<i64, String> {
    match term {
        ParsedTerm::Integer(value) => Ok(*value),
        _ => Err("expected integer in action grammar response".into()),
    }
}

fn nonnegative(term: &ParsedTerm) -> Result<usize, String> {
    usize::try_from(integer(term)?)
        .map_err(|_| "negative or oversized integer in grammar response".into())
}

fn text(term: &ParsedTerm) -> Result<&str, String> {
    match term {
        ParsedTerm::String(value) | ParsedTerm::Atom(value) => Ok(value),
        _ => Err("expected text in grammar response".into()),
    }
}

fn term_text(term: &ParsedTerm) -> String {
    match term {
        ParsedTerm::Atom(value) | ParsedTerm::String(value) => value.clone(),
        ParsedTerm::Integer(value) => value.to_string(),
        ParsedTerm::Compound(name, args) => format!(
            "{}({})",
            name,
            args.iter().map(term_text).collect::<Vec<_>>().join(",")
        ),
        ParsedTerm::List(values) => format!(
            "[{}]",
            values.iter().map(term_text).collect::<Vec<_>>().join(",")
        ),
    }
}

fn decode_relation_value(term: &ParsedTerm) -> Result<RelationValue, String> {
    Ok(match term {
        ParsedTerm::Atom(value) => RelationValue::Atom(value.clone()),
        ParsedTerm::String(value) => RelationValue::String(value.clone()),
        ParsedTerm::Integer(value) => RelationValue::Integer(*value),
        ParsedTerm::Compound(functor, arguments) => RelationValue::Compound(
            functor.clone(),
            arguments
                .iter()
                .map(decode_relation_value)
                .collect::<Result<_, _>>()?,
        ),
        ParsedTerm::List(values) => RelationValue::List(
            values
                .iter()
                .map(decode_relation_value)
                .collect::<Result<_, _>>()?,
        ),
    })
}

fn decode_context_query(term: &ParsedTerm) -> Result<ContextQuery, String> {
    let args = compound(term, "ask", 3)?;
    let cardinality = match atom(&args[0])? {
        "empty" => ContextCardinality::Empty,
        "one" => ContextCardinality::One,
        "all" => ContextCardinality::All,
        _ => return Err("invalid context query cardinality".into()),
    };
    Ok(ContextQuery {
        cardinality,
        domain: decode_relation_value(&args[1])?,
        selector: decode_relation_value(&args[2])?,
    })
}

fn decode_context_entry(term: &ParsedTerm) -> Result<ContextEntry, String> {
    let args = compound(term, "entry", 5)?;
    Ok(ContextEntry {
        domain: decode_relation_value(&args[0])?,
        identity: decode_relation_value(&args[1])?,
        names: list(&args[2])?
            .iter()
            .map(|name| text(name).map(str::to_owned))
            .collect::<Result<_, _>>()?,
        value: decode_relation_value(&args[3])?,
        attributes: list(&args[4])?
            .iter()
            .map(decode_relation_value)
            .collect::<Result<_, _>>()?,
    })
}

fn decode_context_result(term: &ParsedTerm) -> Result<ContextResult, String> {
    if let Ok(args) = compound(term, "empty", 1) {
        return Ok(ContextResult::Empty(match atom(&args[0])? {
            "true" => true,
            "false" => false,
            _ => return Err("invalid empty context result".into()),
        }));
    }
    if let Ok(args) = compound(term, "one", 1) {
        return Ok(ContextResult::One(decode_context_entry(&args[0])?));
    }
    if let Ok(args) = compound(term, "all", 1) {
        return Ok(ContextResult::All(
            list(&args[0])?
                .iter()
                .map(decode_context_entry)
                .collect::<Result<_, _>>()?,
        ));
    }
    Err("invalid context result".into())
}

fn decode_context_outcome(term: &ParsedTerm) -> Result<Option<ContextResult>, String> {
    if atom(term).ok() == Some("none") {
        return Ok(None);
    }
    let args = compound(term, "some", 1)?;
    Ok(Some(decode_context_result(&args[0])?))
}

fn decode_context_observation(term: &ParsedTerm) -> Result<ContextObservation, String> {
    let args = compound(term, "observed", 4)?;
    let source = compound(&args[2], "source", 2)?;
    Ok(ContextObservation {
        id: decode_relation_value(&args[0])?,
        query: decode_context_query(&args[1])?,
        provider: decode_relation_value(&source[0])?,
        revision: decode_relation_value(&source[1])?,
        outcome: decode_context_outcome(&args[3])?,
    })
}

fn decode_context_node(term: &ParsedTerm) -> Result<ContextQueryNode, String> {
    let args = compound(term, "query", 2)?;
    Ok(ContextQueryNode {
        id: decode_relation_value(&args[0])?,
        query: decode_context_query(&args[1])?,
    })
}

fn decode_context_dependency_key(term: &ParsedTerm) -> Result<ContextDependencyKey, String> {
    let args = compound(term, "dependency", 3)?;
    Ok(ContextDependencyKey {
        id: decode_relation_value(&args[0])?,
        query: decode_context_query(&args[1])?,
        outcome: decode_context_outcome(&args[2])?,
    })
}

fn decode_span(term: &ParsedTerm) -> Result<Span, String> {
    let args = compound(term, "span", 2)?;
    Ok(Span {
        start: nonnegative(&args[0])?,
        end: nonnegative(&args[1])?,
    })
}

fn decode_command_value(term: &ParsedTerm) -> Result<CommandValue, String> {
    if let Ok(args) = compound(term, "integer", 1) {
        return Ok(CommandValue::Integer(integer(&args[0])?));
    }
    if let Ok(args) = compound(term, "boolean", 1) {
        return match atom(&args[0])? {
            "true" => Ok(CommandValue::Boolean(true)),
            "false" => Ok(CommandValue::Boolean(false)),
            _ => Err("invalid boolean returned by grammar".into()),
        };
    }
    if let Ok(args) = compound(term, "string", 1) {
        return Ok(CommandValue::String(text(&args[0])?.to_string()));
    }
    if let Ok(args) = compound(term, "path", 1) {
        return Ok(CommandValue::Path(text(&args[0])?.to_string()));
    }
    if let Ok(args) = compound(term, "base64", 1) {
        return Ok(CommandValue::Base64(text(&args[0])?.to_string()));
    }
    if let Ok(args) = compound(term, "spec", 1) {
        return Ok(CommandValue::Spec(text(&args[0])?.to_string()));
    }
    if let Ok(args) = compound(term, "oci_spec", 5) {
        let tag = if atom(&args[2]).ok() == Some("none") {
            None
        } else {
            let value = compound(&args[2], "some", 1)?;
            Some(text(&value[0])?.to_string())
        };
        let build_arguments = list(&args[4])?
            .iter()
            .map(|pair| {
                let fields = compound(pair, "pair", 2)?;
                Ok((text(&fields[0])?.to_string(), text(&fields[1])?.to_string()))
            })
            .collect::<Result<_, String>>()?;
        return Ok(CommandValue::OciSpec {
            context_tar_gz: text(&args[0])?.to_string(),
            dockerfile: text(&args[1])?.to_string(),
            tag,
            net_mode: text(&args[3])?.to_string(),
            build_arguments,
        });
    }
    if let Ok(args) = compound(term, "api_spec", 3) {
        return Ok(CommandValue::ApiSpec {
            base_url: text(&args[0])?.to_string(),
            model: text(&args[1])?.to_string(),
            api_key: text(&args[2])?.to_string(),
        });
    }
    if let Ok(args) = compound(term, "array", 1) {
        return Ok(CommandValue::Array(
            list(&args[0])?
                .iter()
                .map(decode_command_value)
                .collect::<Result<_, _>>()?,
        ));
    }
    if let Ok(args) = compound(term, "hole", 2) {
        return Ok(CommandValue::Hole {
            name: atom(&args[0])?.to_string(),
            kind: atom(&args[1])?.to_string(),
        });
    }
    Err("invalid typed command value returned by grammar".into())
}

fn decode_command(term: &ParsedTerm) -> Result<CommandAst, String> {
    let args = compound(term, "command", 4)?;
    let command_args = list(&args[3])?
        .iter()
        .map(decode_command_value)
        .collect::<Result<Vec<_>, String>>()?;
    Ok(CommandAst {
        action: atom(&args[0])?.to_string(),
        handler: atom(&args[1])?.to_string(),
        target: atom(&args[2])?.to_string(),
        args: command_args,
    })
}

fn decode_evidence(term: &ParsedTerm) -> Result<Evidence, String> {
    let args = compound(term, "evidence", 8)?;
    Ok(Evidence {
        semantic: term_text(&args[0]),
        span: decode_span(&args[1])?,
        paint_spans: list(&args[2])?
            .iter()
            .map(decode_span)
            .collect::<Result<_, _>>()?,
        surface: text(&args[3])?.to_string(),
        syntax: atom(&args[4])?.to_string(),
        provider: atom(&args[5])?.to_string(),
        preference: integer(&args[6])?,
        origin: term_text(&args[7]),
    })
}

fn decode_completions_value(value: &RelationValue) -> Result<Vec<Completion>, String> {
    decode_completions_term(&parsed_relation_value(value))
}

fn decode_completions_term(value: &ParsedTerm) -> Result<Vec<Completion>, String> {
    list(value)?
        .iter()
        .map(|term| {
            let args = compound(term, "completion", 5)?;
            let alternatives = list(&args[2])?
                .iter()
                .map(|term| {
                    let args = compound(term, "alternative", 4)?;
                    Ok(CompletionAlternative {
                        semantic: term_text(&args[0]),
                        syntax: atom(&args[1])?.to_string(),
                        provider: term_text(&args[2]),
                        preference: integer(&args[3])?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let insert = text(&args[1])?.to_string();
            Ok(Completion {
                replace: decode_span(&args[0])?,
                display: insert.clone(),
                insert,
                alternatives,
                preference: integer(&args[3])?,
                rank: nonnegative(&args[4])?,
            })
        })
        .collect()
}

fn decode_highlights_value(value: &RelationValue) -> Result<Vec<Highlight>, String> {
    decode_highlights_term(&parsed_relation_value(value))
}

fn decode_highlights_term(value: &ParsedTerm) -> Result<Vec<Highlight>, String> {
    list(value)?
        .iter()
        .map(|term| {
            let args = compound(term, "highlight", 4)?;
            Ok(Highlight {
                span: decode_span(&args[0])?,
                syntax: atom(&args[1])?.to_string(),
                semantic: term_text(&args[2]),
                origin: term_text(&args[3]),
            })
        })
        .collect()
}

pub fn global() -> Result<&'static Prolog, String> {
    static PROLOG: OnceLock<Result<Prolog, String>> = OnceLock::new();
    PROLOG
        .get_or_init(Prolog::new)
        .as_ref()
        .map_err(Clone::clone)
}

pub(crate) fn ensure_linked() {
    std::hint::black_box(APP_RESOURCE);
    std::hint::black_box(PL_initialise as unsafe extern "C" fn(c_int, *mut *mut c_char) -> c_int);
    std::hint::black_box(Prolog::parse as fn(&Prolog, &GrammarInput, Option<&'static str>) -> _);
    std::hint::black_box(Prolog::complete as fn(&Prolog, &GrammarInput, &'static str) -> _);
    std::hint::black_box(Prolog::highlights as fn(&Prolog, &ParseCandidate) -> _);
    std::hint::black_box(Prolog::render as fn(&Prolog, &CommandAst) -> _);
    std::hint::black_box(Prolog::action_help as fn(&Prolog) -> _);
    std::hint::black_box(Prolog::ui_action_help as fn(&Prolog) -> _);
    std::hint::black_box(
        Prolog::context_query as fn(&Prolog, &ContextQuery, &ContextSnapshot) -> _,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grammar_words(words: &[&str]) -> GrammarInput {
        let mut start = 0;
        let mut items = Vec::with_capacity(words.len());
        for word in words {
            let end = start + word.len();
            items.push(InputItem::Unit(KnownUnit {
                semantic: Semantic::Text((*word).into()),
                span: Span { start, end },
                paint_spans: vec![Span { start, end }],
                surface: (*word).into(),
                syntax: "command_source".into(),
                provider: "rust".into(),
                preference: 0,
            }));
            start = end + 1;
        }
        GrammarInput {
            items,
            end: start.saturating_sub(1),
        }
    }

    fn command(action: &str, id: Option<i64>) -> CommandAst {
        CommandAst {
            action: action.into(),
            handler: action.into(),
            target: "ui".into(),
            args: id
                .map(|value| vec![CommandValue::Integer(value)])
                .unwrap_or_default(),
        }
    }

    fn rv_atom(value: &str) -> RelationValue {
        RelationValue::Atom(value.into())
    }

    fn rv_string(value: &str) -> RelationValue {
        RelationValue::String(value.into())
    }

    fn rv_integer(value: i64) -> RelationValue {
        RelationValue::Integer(value)
    }

    fn rv_compound(name: &str, arguments: Vec<RelationValue>) -> RelationValue {
        RelationValue::Compound(name.into(), arguments)
    }

    fn rv_list(values: Vec<RelationValue>) -> RelationValue {
        RelationValue::List(values)
    }

    #[test]
    fn generic_transform_crosses_structured_fli_in_both_directions() {
        let literal = rv_compound(
            "literal",
            vec![
                rv_atom("greeting"),
                rv_string("hello"),
                rv_atom("keyword"),
                rv_atom("greeting"),
                rv_integer(20),
            ],
        );
        let argument = rv_compound(
            "argument",
            vec![rv_compound(
                "arg",
                vec![
                    rv_atom("name"),
                    rv_atom("word"),
                    rv_atom("required"),
                    rv_atom("scalar"),
                ],
            )],
        );
        let terminal = rv_compound(
            "terminal",
            vec![
                rv_atom("word"),
                rv_atom("identifier"),
                rv_list(vec![rv_compound(
                    "surface",
                    vec![
                        rv_compound("word", vec![rv_string("world")]),
                        rv_string("world"),
                    ],
                )]),
            ],
        );
        let grammar = rv_compound(
            "sequence_grammar",
            vec![
                rv_list(vec![literal, argument]),
                rv_compound("terminals", vec![rv_list(vec![terminal])]),
                rv_compound("separator", vec![rv_string(" ")]),
                rv_compound("contexts", vec![rv_list(vec![])]),
            ],
        );
        let span = |start, end| rv_compound("span", vec![rv_integer(start), rv_integer(end)]);
        let unit = |surface: &str, start, end| {
            rv_compound(
                "unit",
                vec![
                    rv_atom("ignored"),
                    span(start, end),
                    rv_list(vec![span(start, end)]),
                    rv_string(surface),
                    rv_atom("source"),
                    rv_atom("foreign_source"),
                    rv_integer(3),
                    rv_atom("foreign_test"),
                ],
            )
        };
        let source = rv_compound(
            "source",
            vec![
                rv_list(vec![
                    unit("hello", 0, 5),
                    unit("world", 6, 11),
                    rv_compound("end", vec![rv_integer(11)]),
                ]),
                rv_atom("exact"),
            ],
        );
        let prolog = global().unwrap();
        let parsed = prolog
            .transform(&RelationRequest {
                grammar: grammar.clone(),
                given: vec![RelationBinding {
                    name: "source".into(),
                    value: source,
                }],
                wanted: vec!["arguments".into(), "status".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.solutions.len(), 1);
        assert_eq!(parsed.solutions[0].bindings[1].value, rv_atom("complete"));
        let arguments = parsed.solutions[0].bindings[0].value.clone();
        let rendered = prolog
            .transform(&RelationRequest {
                grammar,
                given: vec![RelationBinding {
                    name: "arguments".into(),
                    value: arguments,
                }],
                wanted: vec!["source".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert_eq!(
            rendered.solutions[0].bindings[0].value,
            rv_string("hello world")
        );
    }

    #[test]
    fn raw_utf8_grammar_crosses_the_embedded_structured_boundary() {
        let metadata = rv_compound(
            "presentation",
            vec![rv_list(vec![rv_compound(
                "meta",
                vec![rv_atom("syntax"), rv_atom("text")],
            )])],
        );
        let terminal = rv_compound(
            "terminal",
            vec![
                rv_compound(
                    "text",
                    vec![rv_compound("codepoint", vec![rv_atom("any")])],
                ),
                metadata,
            ],
        );
        let grammar = rv_compound(
            "grammar",
            vec![
                rv_compound(
                    "source",
                    vec![rv_compound("text", vec![rv_atom("utf8")])],
                ),
                rv_atom("root"),
                rv_list(vec![rv_compound(
                    "rule",
                    vec![
                        rv_atom("root"),
                        rv_compound(
                            "repeat",
                            vec![rv_integer(1), rv_atom("unbounded"), terminal],
                        ),
                    ],
                )]),
                rv_list(vec![]),
            ],
        );
        let source = rv_compound(
            "text_source",
            vec![rv_string("λa"), rv_atom("exact"), rv_atom("rust_test")],
        );
        let reply = global()
            .unwrap()
            .transform(&RelationRequest {
                grammar,
                given: vec![RelationBinding {
                    name: "source".into(),
                    value: source,
                }],
                wanted: vec!["ast".into(), "status".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert!(reply.diagnostics.is_empty());
        assert_eq!(reply.solutions.len(), 1);
        assert_eq!(
            reply.solutions[0].bindings[0].value,
            rv_compound(
                "node",
                vec![
                    rv_atom("root"),
                    rv_compound("span", vec![rv_integer(0), rv_integer(3)]),
                    rv_compound(
                        "repeated",
                        vec![rv_list(vec![
                            rv_compound("codepoint", vec![rv_integer(955)]),
                            rv_compound("codepoint", vec![rv_integer(97)]),
                        ])],
                    ),
                ],
            )
        );
        assert_eq!(reply.solutions[0].bindings[1].value, rv_atom("complete"));
    }

    #[test]
    fn installed_brush_word_grammar_is_a_generic_relation_client() {
        let reply = global()
            .unwrap()
            .transform(&RelationRequest {
                grammar: brush_grammar_handle(),
                given: vec![RelationBinding {
                    name: "source".into(),
                    value: rv_compound(
                        "text_source",
                        vec![
                            rv_string("pré\"$name$(echo hi)\""),
                            rv_atom("exact"),
                            rv_atom("rust_test"),
                        ],
                    ),
                }],
                wanted: vec!["ast".into(), "status".into(), "highlights".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert!(reply.diagnostics.is_empty());
        assert_eq!(reply.solutions.len(), 1);
        assert_eq!(reply.solutions[0].bindings[1].value, rv_atom("complete"));
        let RelationValue::Compound(root, fields) = &reply.solutions[0].bindings[0].value else {
            panic!("Brush grammar did not return an AST node");
        };
        assert_eq!(root, "node");
        assert_eq!(fields[0], rv_atom("shell_program"));
        assert_eq!(
            fields[1],
            rv_compound("span", vec![rv_integer(0), rv_integer(21)])
        );
        let RelationValue::List(highlights) = &reply.solutions[0].bindings[2].value else {
            panic!("Brush grammar did not return highlight evidence");
        };
        assert!(highlights.iter().any(|highlight| {
            matches!(highlight,
                RelationValue::Compound(name, values)
                    if name == "highlight" && values[1] == rv_atom("variable"))
        }));

        let assist = global()
            .unwrap()
            .transform(&RelationRequest {
                grammar: brush_grammar_handle(),
                given: vec![RelationBinding {
                    name: "source".into(),
                    value: rv_compound(
                        "text_source",
                        vec![
                            rv_string("echo hi)"),
                            rv_compound(
                                "assist",
                                vec![
                                    rv_atom("edit"),
                                    rv_compound(
                                        "span",
                                        vec![rv_integer(0), rv_integer(0)],
                                    ),
                                ],
                            ),
                            rv_atom("rust_test"),
                        ],
                    ),
                }],
                wanted: vec!["completions".into(), "status".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert!(assist.diagnostics.is_empty());
        assert_eq!(assist.solutions.len(), 1);
        let completions =
            decode_completions_value(&assist.solutions[0].bindings[0].value).unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].replace, Span { start: 0, end: 0 });
        assert_eq!(completions[0].insert, "$(");
        assert_eq!(
            assist.solutions[0].bindings[1].value,
            rv_compound("incomplete", vec![rv_compound("edit", vec![rv_atom("edit")])])
        );
    }

    #[test]
    fn installed_brush_grammar_enriches_parse_with_local_state() {
        let source = rv_compound(
            "text_source",
            vec![
                rv_string("x=123; echo $x"),
                rv_atom("exact"),
                rv_atom("rust_test"),
            ],
        );
        let initial = rv_compound(
            "local_state",
            vec![
                rv_list(vec![rv_compound(
                    "scope",
                    vec![rv_atom("root"), rv_list(vec![])],
                )]),
                rv_list(vec![]),
            ],
        );
        let reply = global()
            .unwrap()
            .transform(&RelationRequest {
                grammar: brush_grammar_handle(),
                given: vec![
                    RelationBinding {
                        name: "source".into(),
                        value: source,
                    },
                    RelationBinding {
                        name: "initial_state".into(),
                        value: initial,
                    },
                ],
                wanted: vec!["resolutions".into(), "delta".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert!(reply.diagnostics.is_empty());
        assert!(reply.context_queries.is_empty());
        assert_eq!(reply.solutions.len(), 1);
        assert_eq!(
            reply.solutions[0].bindings[1].value,
            rv_list(vec![rv_compound(
                "state_change",
                vec![
                    rv_atom("shell_variable"),
                    rv_string("x"),
                    rv_compound("shell_text", vec![rv_string("123")]),
                ],
            )])
        );
    }

    #[test]
    fn installed_brush_grammar_consumes_variable_observation() {
        let id = rv_compound(
            "node_ref",
            vec![
                rv_atom("simple_parameter"),
                rv_compound("span", vec![rv_integer(5), rv_integer(7)]),
            ],
        );
        let query = rv_compound(
            "ask",
            vec![
                rv_atom("one"),
                rv_atom("shell_variable"),
                rv_compound("name", vec![rv_string("z")]),
            ],
        );
        let entry = rv_compound(
            "entry",
            vec![
                rv_atom("shell_variable"),
                rv_atom("variable_z"),
                rv_list(vec![rv_string("z")]),
                rv_compound("shell_text", vec![rv_string("value")]),
                rv_list(vec![]),
            ],
        );
        let outcome = rv_compound(
            "some",
            vec![rv_compound("one", vec![entry.clone()])],
        );
        let observation = rv_compound(
            "observed",
            vec![
                id.clone(),
                query.clone(),
                rv_compound(
                    "source",
                    vec![rv_atom("brush_variables"), rv_integer(7)],
                ),
                outcome.clone(),
            ],
        );
        let reply = global()
            .unwrap()
            .transform(&RelationRequest {
                grammar: brush_grammar_handle(),
                given: vec![
                    RelationBinding {
                        name: "source".into(),
                        value: rv_compound(
                            "text_source",
                            vec![
                                rv_string("echo $z"),
                                rv_atom("exact"),
                                rv_atom("rust_test"),
                            ],
                        ),
                    },
                    RelationBinding {
                        name: "initial_state".into(),
                        value: rv_compound(
                            "local_state",
                            vec![
                                rv_list(vec![rv_compound(
                                    "scope",
                                    vec![rv_atom("root"), rv_list(vec![])],
                                )]),
                                rv_list(vec![]),
                            ],
                        ),
                    },
                ],
                wanted: vec!["resolutions".into()],
                observations: vec![observation],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert!(reply.diagnostics.is_empty());
        assert!(reply.context_queries.is_empty());
        assert_eq!(
            reply.dependency_keys,
            vec![rv_compound(
                "dependency",
                vec![id.clone(), query, outcome],
            )]
        );
        assert_eq!(
            reply.solutions[0].bindings[0].value,
            rv_list(vec![rv_compound(
                "resolved",
                vec![
                    id,
                    rv_compound(
                        "external",
                        vec![rv_compound("one", vec![entry])],
                    ),
                ],
            )])
        );
    }

    #[test]
    fn local_state_crosses_the_embedded_relation_without_local_queries() {
        let define_x = rv_compound(
            "define",
            vec![
                rv_atom("shell_variable"),
                rv_string("x"),
                rv_compound("integer", vec![rv_integer(123)]),
                rv_atom("escaping"),
                rv_atom("replace"),
            ],
        );
        let use_x = rv_compound(
            "use",
            vec![rv_atom("local_x"), rv_atom("shell_variable"), rv_string("x")],
        );
        let use_z = rv_compound(
            "use",
            vec![rv_atom("free_z"), rv_atom("shell_variable"), rv_string("z")],
        );
        let initial = rv_compound(
            "local_state",
            vec![
                rv_list(vec![rv_compound(
                    "scope",
                    vec![rv_atom("root"), rv_list(vec![])],
                )]),
                rv_list(vec![]),
            ],
        );
        let reply = global()
            .unwrap()
            .transform(&RelationRequest {
                grammar: rv_atom("local_state_grammar"),
                given: vec![
                    RelationBinding {
                        name: "steps".into(),
                        value: rv_list(vec![define_x, use_x, use_z]),
                    },
                    RelationBinding {
                        name: "initial_state".into(),
                        value: initial,
                    },
                ],
                wanted: vec!["delta".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert!(reply.diagnostics.is_empty());
        assert_eq!(reply.solutions.len(), 1);
        assert_eq!(
            reply.context_queries,
            vec![rv_compound(
                "query",
                vec![
                    rv_atom("free_z"),
                    rv_compound(
                        "ask",
                        vec![
                            rv_atom("one"),
                            rv_atom("shell_variable"),
                            rv_compound("name", vec![rv_string("z")]),
                        ],
                    ),
                ],
            )]
        );
        assert_eq!(
            reply.solutions[0].bindings[0].value,
            rv_list(vec![rv_compound(
                "state_change",
                vec![
                    rv_atom("shell_variable"),
                    rv_string("x"),
                    rv_compound("integer", vec![rv_integer(123)]),
                ],
            )])
        );
    }

    #[test]
    fn generic_transform_enforces_request_specific_output_bound() {
        let prolog = global().unwrap();
        let error = prolog
            .transform(&RelationRequest {
                grammar: rv_atom("unknown_grammar"),
                given: vec![],
                wanted: vec!["source".into()],
                observations: vec![],
                limits: RelationLimits {
                    max_output_bytes: 1,
                    ..RelationLimits::default()
                },
            })
            .unwrap_err();
        assert!(error.contains("bounds"), "{error}");

        let error = prolog
            .transform(&RelationRequest {
                grammar: rv_atom("unknown_grammar"),
                given: vec![],
                wanted: vec!["source".into()],
                observations: vec![],
                limits: RelationLimits {
                    max_output_bytes: MAX_OUTPUT_BYTES + 1,
                    ..RelationLimits::default()
                },
            })
            .unwrap_err();
        assert!(error.contains("output limit"), "{error}");
    }

    #[test]
    fn installed_action_grammar_is_reached_by_opaque_handle() {
        let prolog = global().unwrap();
        let reply = prolog
            .transform(&RelationRequest {
                grammar: rv_compound("grammar_handle", vec![rv_atom("sarun_actions")]),
                given: vec![RelationBinding {
                    name: "command".into(),
                    value: rv_compound(
                        "command",
                        vec![
                            rv_atom("mirror_resume"),
                            rv_atom("mirror_pause"),
                            rv_atom("ui"),
                            rv_list(vec![
                                rv_compound("integer", vec![rv_integer(7)]),
                                rv_compound("boolean", vec![rv_atom("false")]),
                            ]),
                        ],
                    ),
                }],
                wanted: vec!["source".into()],
                observations: vec![],
                limits: RelationLimits::default(),
            })
            .unwrap();
        assert!(reply.diagnostics.is_empty(), "{:?}", reply.diagnostics);
        assert_eq!(reply.solutions.len(), 1);
        assert_eq!(
            reply.solutions[0].bindings[0].value,
            rv_string("mirror resume 7")
        );
    }

    #[test]
    fn typed_relation_runtime_is_embedded_bounded_and_closed() {
        let prolog = global().unwrap();
        let duplicate = match Prolog::new() {
            Ok(_) => panic!("created duplicate runtime"),
            Err(error) => error,
        };
        assert!(duplicate.contains("already active"));
        assert_eq!(
            prolog.render(&command("mirror_jobs", None)).unwrap().text,
            "mirror jobs",
        );
        assert_eq!(
            prolog.render(&command("mirror_run", Some(7))).unwrap().text,
            "mirror run 7",
        );
        assert_eq!(
            prolog.render(&command("kill", Some(5))).unwrap().text,
            "kill 5",
        );
        assert_eq!(
            prolog
                .render(&command("mirror_run_pending", None))
                .unwrap()
                .text,
            "mirror run pending",
        );
        let parsed_oci = prolog
            .parse(
                &grammar_words(&[
                    "oci",
                    "build",
                    r#"{"context_tar_gz":"eA==","dockerfile":"FROM","tag":null,"net":"tap","build_args":[]}"#,
                ]),
                None,
            )
            .unwrap();
        assert_eq!(parsed_oci.len(), 1);
        assert_eq!(
            parsed_oci[0].command.args,
            vec![CommandValue::OciSpec {
                context_tar_gz: "eA==".into(),
                dockerfile: "FROM".into(),
                tag: None,
                net_mode: "tap".into(),
                build_arguments: Vec::new(),
            }],
        );
        prolog.exhaust_inference_limit().unwrap();
        assert_eq!(
            prolog.render(&command("mirror_rm", Some(11))).unwrap().text,
            "mirror rm 11",
        );
    }

    #[test]
    fn render_no_solution_is_distinct_from_backend_decode_error() {
        let mut invalid = command("missing_action", None);
        invalid.handler = "missing_handler".into();
        assert_eq!(
            global().unwrap().render(&invalid),
            Err(QueryError::NoSolution)
        );
    }

    #[test]
    fn request_and_input_bounds_are_enforced() {
        let prolog = global().unwrap();
        let input = GrammarInput {
            items: (0..=MAX_ITEMS)
                .map(|id| InputItem::SourceTear {
                    id,
                    span: Span { start: 0, end: 0 },
                    surface: String::new(),
                })
                .collect(),
            end: 0,
        };
        assert!(prolog.parse(&input, None).unwrap_err().contains("items"));
    }

    #[test]
    fn contextual_query_graph_roundtrips_through_embedded_relation() {
        fn atom(value: &str) -> RelationValue {
            RelationValue::Atom(value.into())
        }
        fn compound(name: &str, args: Vec<RelationValue>) -> RelationValue {
            RelationValue::Compound(name.into(), args)
        }

        let prolog = global().unwrap();
        let box_query = ContextQuery {
            cardinality: ContextCardinality::One,
            domain: atom("box"),
            selector: compound("name", vec![RelationValue::String("work".into())]),
        };
        let entry = ContextEntry {
            domain: atom("box"),
            identity: RelationValue::Integer(2),
            names: vec!["work".into()],
            value: compound("box_id", vec![RelationValue::Integer(2)]),
            attributes: vec![atom("running")],
        };
        let snapshot = ContextSnapshot {
            provider: atom("boxes"),
            revision: RelationValue::Integer(7),
            entries: vec![entry.clone()],
        };
        assert_eq!(
            prolog.context_query(&box_query, &snapshot).unwrap(),
            Some(ContextResult::One(entry.clone())),
        );
        let observation = prolog
            .observe_context(&atom("box_query"), &box_query, &snapshot)
            .unwrap();
        assert_eq!(observation.outcome, Some(ContextResult::One(entry)));
        let dependency = prolog
            .context_dependency_keys(&[observation.clone()])
            .unwrap();
        let mut refreshed = observation.clone();
        refreshed.revision = RelationValue::Integer(8);
        assert_eq!(
            dependency,
            prolog
                .context_dependency_keys(&[refreshed.clone()])
                .unwrap(),
        );
        refreshed.outcome = None;
        assert_ne!(
            dependency,
            prolog.context_dependency_keys(&[refreshed]).unwrap(),
        );

        let path_query = ContextQuery {
            cardinality: ContextCardinality::All,
            domain: atom("path"),
            selector: compound(
                "within",
                vec![
                    compound("box", vec![compound("ref", vec![atom("box_query")])]),
                    compound("prefix", vec![RelationValue::String("src/".into())]),
                ],
            ),
        };
        let ready = prolog
            .ready_context_queries(
                &[
                    ContextQueryNode {
                        id: atom("box_query"),
                        query: box_query,
                    },
                    ContextQueryNode {
                        id: atom("path_query"),
                        query: path_query,
                    },
                ],
                &[observation],
            )
            .unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, atom("path_query"));
        assert_eq!(
            ready[0].query.selector,
            compound(
                "within",
                vec![
                    compound(
                        "box",
                        vec![compound("box_id", vec![RelationValue::Integer(2)])],
                    ),
                    compound("prefix", vec![RelationValue::String("src/".into())]),
                ],
            ),
        );
    }

    #[test]
    fn contextual_command_plans_and_resolution_cross_embedded_boundary() {
        let prolog = global().unwrap();
        let plans = prolog
            .context_plans(&grammar_words(&["rename", "work", "new-name"]), None)
            .unwrap();
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.command.args[0], CommandValue::String("work".into()));
        assert_eq!(plan.queries.len(), 1);
        assert_eq!(plan.queries[0].query.cardinality, ContextCardinality::One);
        assert_eq!(
            plan.queries[0].query.domain,
            RelationValue::Atom("box".into())
        );
        let entry = ContextEntry {
            domain: RelationValue::Atom("box".into()),
            identity: RelationValue::Integer(5),
            names: vec!["work".into()],
            value: RelationValue::Compound("integer".into(), vec![RelationValue::Integer(5)]),
            attributes: Vec::new(),
        };
        let observation = ContextObservation {
            id: plan.queries[0].id.clone(),
            query: plan.queries[0].query.clone(),
            provider: RelationValue::Atom("boxes".into()),
            revision: RelationValue::Integer(7),
            outcome: Some(ContextResult::One(entry)),
        };
        assert_eq!(
            prolog
                .resolve_context_plan(plan, &[observation])
                .unwrap()
                .args,
            vec![
                CommandValue::Integer(5),
                CommandValue::String("new-name".into()),
            ],
        );
    }

    #[test]
    fn contextual_completion_plan_crosses_embedded_boundary() {
        let mut input = grammar_words(&["rename"]);
        input.items.push(InputItem::EditTear {
            id: "edit",
            span: Span { start: 7, end: 9 },
            surface: "wo".into(),
        });
        input.end = 9;

        let prolog = global().unwrap();
        let plans = prolog.context_completion_plans(&input, "edit").unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].action, "rename");
        assert_eq!(plans[0].replace, Span { start: 7, end: 9 });
        assert_eq!(plans[0].surface, "wo");
        assert_eq!(plans[0].queries.len(), 1);
        assert_eq!(plans[0].target_query_id, plans[0].queries[0].id);
        assert_eq!(plans[0].preference, 90);
        assert_eq!(
            plans[0].queries[0].query.cardinality,
            ContextCardinality::All
        );
        assert_eq!(
            plans[0].queries[0].query.selector,
            RelationValue::Compound("prefix".into(), vec![RelationValue::String("wo".into())],),
        );

        let entry = ContextEntry {
            domain: RelationValue::Atom("box".into()),
            identity: RelationValue::Integer(5),
            names: vec!["5".into(), "work".into()],
            value: RelationValue::Compound("integer".into(), vec![RelationValue::Integer(5)]),
            attributes: Vec::new(),
        };
        let observation = ContextObservation {
            id: plans[0].queries[0].id.clone(),
            query: plans[0].queries[0].query.clone(),
            provider: RelationValue::Atom("boxes".into()),
            revision: RelationValue::Integer(7),
            outcome: Some(ContextResult::All(vec![entry])),
        };
        let completions = prolog
            .resolve_context_completion(&plans[0], &[observation])
            .unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].insert, "work");
        assert_eq!(completions[0].alternatives[0].provider, "boxes");
    }

    #[test]
    fn incomplete_tear_parse_crosses_embedded_boundary() {
        let input = GrammarInput {
            items: vec![
                InputItem::Unit(KnownUnit {
                    semantic: Semantic::Text("mirror".into()),
                    span: Span { start: 0, end: 6 },
                    paint_spans: vec![Span { start: 0, end: 6 }],
                    surface: "mirror".into(),
                    syntax: "command_source".into(),
                    provider: "rust".into(),
                    preference: 0,
                }),
                InputItem::EditTear {
                    id: "edit",
                    span: Span { start: 7, end: 8 },
                    surface: "r".into(),
                },
            ],
            end: 8,
        };

        let candidates = global().unwrap().parse(&input, Some("edit")).unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.command.action == "mirror_run")
            .expect("ordinary parser did not bind the tear to mirror_run");
        assert_eq!(
            candidate.status,
            ParseStatus::Incomplete {
                edit_id: "edit".into()
            }
        );
        assert_eq!(
            candidate.command.args,
            vec![CommandValue::Hole {
                name: "id".into(),
                kind: "integer".into(),
            }]
        );
        assert!(
            candidate
                .evidence
                .iter()
                .any(|evidence| evidence.origin.contains("tear(edit,literal"))
        );
    }

    #[test]
    fn ui_help_is_projected_from_the_embedded_relation() {
        let rows = global().unwrap().ui_action_help().unwrap();
        assert_eq!(rows.len(), 90);
        let verbs = rows
            .iter()
            .map(|row| row.verb.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(verbs.len(), rows.len());
        assert!(!verbs.contains("verbs"));
        assert!(verbs.contains("view open"));
        assert!(!verbs.contains("quit"));
        let display_path = rows
            .iter()
            .find(|row| row.verb.as_str() == "display path")
            .expect("UI action missing from relation help surface");
        assert_eq!(display_path.arguments.as_str(), "SID");

        let filtered = global().unwrap().ui_action_help_matching("mirror").unwrap();
        assert!(filtered.len() >= 5);
        assert!(filtered.iter().all(|row| {
            row.verb.as_str().contains("mirror") || row.description.as_str().contains("mirror")
        }));
    }
}
