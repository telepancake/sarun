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
const QUERY_INFERENCES: i64 = 100_000;
const LOAD_INFERENCES: i64 = 5_000_000;
const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_ITEMS: usize = 256;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

const PL_Q_NODEBUG: c_int = 0x0004;
const PL_Q_CATCH_EXCEPTION: c_int = 0x0008;
const CVT_ATOM: u32 = 0x0000_0001;
const CVT_STRING: u32 = 0x0000_0002;
const CVT_EXCEPTION: u32 = 0x0000_1000;
const BUF_MALLOC: u32 = 0x0002_0000;
const REP_UTF8: u32 = 0x0010_0000;
const PL_CLEANUP_NO_CANCEL: c_int = 0x0002_0000;
const PL_CLEANUP_SUCCESS: c_int = 1;

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
    fn PL_put_term_from_chars(term: Term, flags: c_int, len: usize, text: *const c_char) -> c_int;
    fn PL_predicate(name: *const c_char, arity: c_int, module: *const c_char) -> Predicate;
    fn PL_open_query(module: Module, flags: c_int, predicate: Predicate, terms: Term) -> Query;
    fn PL_next_solution(query: Query) -> c_int;
    fn PL_cut_query(query: Query) -> c_int;
    fn PL_close_query(query: Query) -> c_int;
    fn PL_exception(query: Query) -> Term;
    fn PL_clear_exception();
    fn PL_get_arg_sz(index: usize, term: Term, argument: Term) -> c_int;
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
    pub id: String,
    pub query: ContextQuery,
    pub provider: RelationValue,
    pub revision: RelationValue,
    pub outcome: Option<ContextResult>,
}

/// Provenance-free semantic projection used for parse invalidation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextDependencyKey {
    pub id: String,
    pub query: ContextQuery,
    pub outcome: Option<ContextResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextQueryNode {
    pub id: String,
    pub query: ContextQuery,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextBinding {
    pub query_id: String,
    pub argument_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPlan {
    pub command: CommandAst,
    pub queries: Vec<ContextQueryNode>,
    pub bindings: Vec<ContextBinding>,
    pub evidence: Vec<Evidence>,
    pub preference: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompletionPlan {
    pub action: String,
    pub replace: Span,
    pub surface: String,
    pub queries: Vec<ContextQueryNode>,
    pub target_query_id: String,
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

#[derive(Clone, Copy)]
enum Operation {
    Parse,
    Complete,
    Highlights,
    Render,
    ActionHelp,
    ContextQuery,
    ContextObserve,
    ContextReady,
    ContextDependencies,
    ContextPlan,
    ContextResolve,
    ContextCompletion,
    ContextCompletionResolve,
    ActionRequest,
}

impl Operation {
    fn atom(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Complete => "complete",
            Self::Highlights => "highlights",
            Self::Render => "render",
            Self::ActionHelp => "action_help",
            Self::ContextQuery => "context_query",
            Self::ContextObserve => "context_observe",
            Self::ContextReady => "context_ready",
            Self::ContextDependencies => "context_dependencies",
            Self::ContextPlan => "context_plan",
            Self::ContextResolve => "context_resolve",
            Self::ContextCompletion => "context_completion",
            Self::ContextCompletionResolve => "context_completion_resolve",
            Self::ActionRequest => "action_request",
        }
    }
}

enum Command {
    Application(Operation, String, Sender<Result<String, String>>),
    Shutdown(Sender<Result<(), String>>),
    #[cfg(test)]
    ExhaustInferenceLimit(Sender<Result<(), String>>),
}

/// A process-global SWI-Prolog runtime whose FFI calls stay on one thread.
///
/// The public query surface is limited to typed operations over the embedded
/// action grammar. Each operation has an inference bound. This recovers from
/// nonterminating pure Prolog code without SWI signal handlers. An inference
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
        let items = encode_input(input)?;
        let mode = assist_edit.map_or_else(
            || "exact".to_string(),
            |id| {
                checked_atom(id)
                    .map(|id| format!("assist({id})"))
                    .unwrap_or_else(|| "invalid".to_string())
            },
        );
        if mode == "invalid" {
            return Err("invalid edit tear id".into());
        }
        let response = self.application(Operation::Parse, format!("request({items},{mode})"))?;
        decode_parse_response(&response)
    }

    pub fn complete(
        &self,
        input: &GrammarInput,
        edit_id: &'static str,
    ) -> Result<Vec<Completion>, String> {
        let id = checked_atom(edit_id).ok_or("invalid edit tear id")?;
        let items = encode_input(input)?;
        let response = self.application(Operation::Complete, format!("request({items},{id})"))?;
        decode_completion_response(&response)
    }

    pub fn highlights(&self, result: &ParseCandidate) -> Result<Vec<Highlight>, String> {
        let request = format!("request({})", encode_parse_candidate(result));
        let response = self.application(Operation::Highlights, request)?;
        decode_highlight_response(&response)
    }

    pub fn render(&self, command: &CommandAst) -> Result<RenderedCommand, QueryError> {
        let request = format!("request({})", encode_command(command));
        let response = self
            .application(Operation::Render, request)
            .map_err(QueryError::Backend)?;
        let text = decode_render_response(&response)?;
        Ok(RenderedCommand { text })
    }

    pub fn action_request(
        &self,
        command: &CommandAst,
    ) -> Result<crate::generated_wire::ActionRequest, QueryError> {
        let response = self
            .application(
                Operation::ActionRequest,
                format!("request({})", encode_command(command)),
            )
            .map_err(QueryError::Backend)?;
        decode_action_request_response(&response)
    }

    /// Project the complete action help surface from the normalized relation.
    /// This is the only runtime source for verb names, argument notation, and
    /// descriptions; implementation dispatch contributes no metadata.
    pub fn action_help(&self) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
        let response = self.application(Operation::ActionHelp, "request(all)".into())?;
        decode_action_help_response(&response)
    }

    pub fn ui_action_help(&self) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
        let response = self.application(Operation::ActionHelp, "request(ui)".into())?;
        decode_action_help_response(&response)
    }

    pub fn ui_action_help_matching(
        &self,
        filter: &str,
    ) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
        let response = self.application(
            Operation::ActionHelp,
            format!("request(ui,{})", quote_string(filter)),
        )?;
        decode_action_help_response(&response)
    }

    pub fn context_query(
        &self,
        query: &ContextQuery,
        snapshot: &ContextSnapshot,
    ) -> Result<Option<ContextResult>, String> {
        let request = format!(
            "request({},{})",
            encode_context_query(query)?,
            encode_context_snapshot(snapshot)?,
        );
        let response = self.application(Operation::ContextQuery, request)?;
        decode_context_outcome_response(&response)
    }

    pub fn observe_context(
        &self,
        id: &str,
        query: &ContextQuery,
        snapshot: &ContextSnapshot,
    ) -> Result<ContextObservation, String> {
        let request = format!(
            "request({},{},{})",
            quote_atom(id),
            encode_context_query(query)?,
            encode_context_snapshot(snapshot)?,
        );
        let response = self.application(Operation::ContextObserve, request)?;
        decode_context_observation_response(&response)
    }

    pub fn ready_context_queries(
        &self,
        graph: &[ContextQueryNode],
        observations: &[ContextObservation],
    ) -> Result<Vec<ContextQueryNode>, String> {
        let graph = encode_context_graph(graph)?;
        let observations = observations
            .iter()
            .map(encode_context_observation)
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        let response = self.application(
            Operation::ContextReady,
            format!("request({graph},[{observations}])"),
        )?;
        decode_context_ready_response(&response)
    }

    pub fn context_dependency_keys(
        &self,
        observations: &[ContextObservation],
    ) -> Result<Vec<ContextDependencyKey>, String> {
        let observations = observations
            .iter()
            .map(encode_context_observation)
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        let response = self.application(
            Operation::ContextDependencies,
            format!("request([{observations}])"),
        )?;
        decode_context_dependency_keys_response(&response)
    }

    pub fn context_plans(
        &self,
        input: &GrammarInput,
        assist_edit: Option<&'static str>,
    ) -> Result<Vec<ContextPlan>, String> {
        let items = encode_input(input)?;
        let mode = assist_edit.map_or_else(
            || Ok("exact".to_string()),
            |id| {
                checked_atom(id)
                    .map(|id| format!("assist({id})"))
                    .ok_or("invalid edit tear id")
            },
        )?;
        let response =
            self.application(Operation::ContextPlan, format!("request({items},{mode})"))?;
        decode_context_plans_response(&response)
    }

    pub fn resolve_context_plan(
        &self,
        plan: &ContextPlan,
        observations: &[ContextObservation],
    ) -> Result<CommandAst, QueryError> {
        let observations = observations
            .iter()
            .map(encode_context_observation)
            .collect::<Result<Vec<_>, _>>()
            .map_err(QueryError::Backend)?
            .join(",");
        let request = format!(
            "request({},[{}])",
            encode_context_plan(plan).map_err(QueryError::Backend)?,
            observations,
        );
        let response = self
            .application(Operation::ContextResolve, request)
            .map_err(QueryError::Backend)?;
        decode_context_command_response(&response)
    }

    pub fn context_completion_plans(
        &self,
        input: &GrammarInput,
        edit_id: &'static str,
    ) -> Result<Vec<ContextCompletionPlan>, String> {
        let items = encode_input(input)?;
        let id = checked_atom(edit_id).ok_or("invalid edit tear id")?;
        let response = self.application(
            Operation::ContextCompletion,
            format!("request({items},{id})"),
        )?;
        decode_context_completion_plans_response(&response)
    }

    pub fn resolve_context_completion(
        &self,
        plan: &ContextCompletionPlan,
        observations: &[ContextObservation],
    ) -> Result<Vec<Completion>, String> {
        let observations = observations
            .iter()
            .map(encode_context_observation)
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        let request = format!(
            "request({},[{}])",
            encode_context_completion_plan(plan)?,
            observations,
        );
        let response = self.application(Operation::ContextCompletionResolve, request)?;
        decode_completion_response(&response)
    }

    fn application(&self, operation: Operation, request: String) -> Result<String, String> {
        if request.len() > MAX_INPUT_BYTES {
            return Err(format!(
                "action grammar request exceeds {MAX_INPUT_BYTES} bytes"
            ));
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(Command::Application(operation, request, reply_tx))
            .map_err(|_| "Prolog worker has stopped".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "Prolog worker stopped before replying".to_string())?
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
            Command::Application(operation, request, reply) => {
                let _ = reply.send(runtime.application(operation, &request));
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
        runtime.load_application()?;
        Ok(runtime)
    }

    fn load_application(&mut self) -> Result<(), String> {
        let goal = format!(
            concat!(
                "call_with_inference_limit((asserta(user:file_search_path(library,'res://library')),",
                "load_files('res://app/action_grammar.pl',[silent(true)]),",
                "action_grammar:valid_transport_catalog,",
                "action_grammar:valid_action_catalog),",
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

    fn application(&mut self, operation: Operation, request: &str) -> Result<String, String> {
        let request_string = serde_json::to_string(request)
            .map_err(|error| format!("failed to encode grammar request: {error}"))?;
        let goal = format!(
            "action_grammar:application({},{request_string},Output)",
            operation.atom(),
        );
        // SAFETY: the only callable predicate and operation atoms are fixed.
        // Request text is a quoted string datum decoded by application/3,
        // never a goal. All calls stay on the dedicated worker.
        unsafe {
            let terms = PL_new_term_refs(3);
            if terms == 0 {
                return Err("FLI term allocation failed".into());
            }
            if put_utf8_term(terms, &goal) == 0
                || PL_put_int64(terms + 1, QUERY_INFERENCES) == 0
                || PL_put_variable(terms + 2) == 0
            {
                PL_clear_exception();
                PL_reset_term_refs(terms);
                return Err("failed to construct bounded Prolog application query".into());
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
            let exception = !succeeded && PL_exception(query) != 0;
            let result = if succeeded {
                self.extract_application_result(terms)
            } else if exception {
                Err("action grammar raised a Prolog exception".into())
            } else {
                Err("action grammar application failed".into())
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
                return Err("FLI query cleanup failed".into());
            }
            result
        }
    }

    unsafe fn extract_application_result(&mut self, terms: Term) -> Result<String, String> {
        let limit_result = unsafe { get_utf8(terms + 2, CVT_ATOM) }?;
        if limit_result == "inference_limit_exceeded" {
            return Err(format!(
                "action grammar exceeded its {QUERY_INFERENCES}-inference limit"
            ));
        }
        let extracted = unsafe { PL_new_term_refs(2) };
        if extracted == 0 {
            return Err("FLI result term allocation failed".into());
        }
        if unsafe { PL_get_arg_sz(2, terms, extracted) } == 0
            || unsafe { PL_get_arg_sz(3, extracted, extracted + 1) } == 0
        {
            return Err("embedded grammar returned an invalid application term".into());
        }
        let output = unsafe { get_utf8(extracted + 1, CVT_STRING) }?;
        if output.len() > MAX_OUTPUT_BYTES {
            return Err(format!(
                "action grammar response exceeds {MAX_OUTPUT_BYTES} bytes"
            ));
        }
        Ok(output)
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

fn quote_string(text: &str) -> String {
    serde_json::to_string(text).expect("serializing a Rust string cannot fail")
}

fn quote_atom(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

fn encode_span(span: Span) -> String {
    format!("span({},{})", span.start, span.end)
}

fn encode_input(input: &GrammarInput) -> Result<String, String> {
    if input.items.len() > MAX_ITEMS {
        return Err(format!("action grammar input exceeds {MAX_ITEMS} items"));
    }
    let mut previous_end = 0;
    let mut encoded = Vec::with_capacity(input.items.len() + 1);
    for item in &input.items {
        let (span, surface) = match item {
            InputItem::Unit(unit) => (unit.span, unit.surface.as_str()),
            InputItem::EditTear { span, surface, .. }
            | InputItem::SourceTear { span, surface, .. } => (*span, surface.as_str()),
        };
        if span.start < previous_end || span.start > span.end || span.end > input.end {
            return Err("action grammar input has invalid or overlapping spans".into());
        }
        previous_end = span.end;
        if surface.len() > MAX_INPUT_BYTES {
            return Err("action grammar item surface is too large".into());
        }
        encoded.push(match item {
            InputItem::Unit(unit) => {
                if unit.paint_spans.iter().any(|paint| {
                    paint.start < unit.span.start
                        || paint.start > paint.end
                        || paint.end > unit.span.end
                }) {
                    return Err("action grammar unit has an invalid paint span".into());
                }
                let semantic = match &unit.semantic {
                    Semantic::Atom(atom) => quote_atom(atom),
                    Semantic::Integer(value) => format!("integer({value})"),
                    Semantic::Text(value) => format!("text({})", quote_string(value)),
                };
                let syntax = quote_atom(&unit.syntax);
                let provider = quote_atom(&unit.provider);
                let paints = unit
                    .paint_spans
                    .iter()
                    .map(|span| encode_span(*span))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "unit({semantic},{},[{paints}],{},{syntax},{provider},{},lexer)",
                    encode_span(unit.span),
                    quote_string(&unit.surface),
                    unit.preference,
                )
            }
            InputItem::EditTear { id, span, surface } => format!(
                "edit_tear({},{},{})",
                checked_atom(id).ok_or("invalid edit tear id")?,
                encode_span(*span),
                quote_string(surface),
            ),
            InputItem::SourceTear { id, span, surface } => format!(
                "source_tear(source{id},{},{})",
                encode_span(*span),
                quote_string(surface),
            ),
        });
    }
    encoded.push(format!("end({})", input.end));
    let result = format!("[{}]", encoded.join(","));
    if result.len() > MAX_INPUT_BYTES {
        return Err(format!(
            "action grammar request exceeds {MAX_INPUT_BYTES} bytes"
        ));
    }
    Ok(result)
}

fn encode_command_value(value: &CommandValue) -> String {
    match value {
        CommandValue::Integer(value) => format!("integer({value})"),
        CommandValue::Boolean(value) => format!("boolean({value})"),
        CommandValue::String(value) => format!("string({})", quote_string(value)),
        CommandValue::Path(value) => format!("path({})", quote_string(value)),
        CommandValue::Base64(value) => format!("base64({})", quote_string(value)),
        CommandValue::Spec(value) => format!("spec({})", quote_string(value)),
        CommandValue::OciSpec {
            context_tar_gz,
            dockerfile,
            tag,
            net_mode,
            build_arguments,
        } => {
            let tag = tag.as_ref().map_or_else(
                || "none".to_string(),
                |value| format!("some({})", quote_string(value)),
            );
            let build_arguments = build_arguments
                .iter()
                .map(|(key, value)| format!("pair({},{})", quote_string(key), quote_string(value)))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "oci_spec({},{},{},{},[{}])",
                quote_string(context_tar_gz),
                quote_string(dockerfile),
                tag,
                quote_string(net_mode),
                build_arguments,
            )
        }
        CommandValue::ApiSpec {
            base_url,
            model,
            api_key,
        } => format!(
            "api_spec({},{},{})",
            quote_string(base_url),
            quote_string(model),
            quote_string(api_key),
        ),
        CommandValue::Array(values) => format!(
            "array([{}])",
            values
                .iter()
                .map(encode_command_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        CommandValue::Hole { name, kind } => {
            format!("hole({},{})", quote_atom(name), quote_atom(kind))
        }
    }
}

fn encode_command(command: &CommandAst) -> String {
    let args = command
        .args
        .iter()
        .map(encode_command_value)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "command({},{},{},[{}])",
        quote_atom(&command.action),
        quote_atom(&command.handler),
        quote_atom(&command.target),
        args
    )
}

fn encode_relation_value(value: &RelationValue) -> Result<String, String> {
    Ok(match value {
        RelationValue::Atom(value) => quote_atom(value),
        RelationValue::String(value) => quote_string(value),
        RelationValue::Integer(value) => value.to_string(),
        RelationValue::Compound(functor, arguments) => {
            let functor = checked_atom(functor).ok_or("invalid relation value functor")?;
            let arguments = arguments
                .iter()
                .map(encode_relation_value)
                .collect::<Result<Vec<_>, _>>()?
                .join(",");
            format!("{functor}({arguments})")
        }
        RelationValue::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(encode_relation_value)
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        ),
    })
}

fn encode_context_query(query: &ContextQuery) -> Result<String, String> {
    let cardinality = match query.cardinality {
        ContextCardinality::Empty => "empty",
        ContextCardinality::One => "one",
        ContextCardinality::All => "all",
    };
    Ok(format!(
        "ask({cardinality},{},{})",
        encode_relation_value(&query.domain)?,
        encode_relation_value(&query.selector)?,
    ))
}

fn encode_context_entry(entry: &ContextEntry) -> Result<String, String> {
    let names = entry
        .names
        .iter()
        .map(|name| quote_string(name))
        .collect::<Vec<_>>()
        .join(",");
    let attributes = entry
        .attributes
        .iter()
        .map(encode_relation_value)
        .collect::<Result<Vec<_>, _>>()?
        .join(",");
    Ok(format!(
        "entry({},{},[{}],{},[{}])",
        encode_relation_value(&entry.domain)?,
        encode_relation_value(&entry.identity)?,
        names,
        encode_relation_value(&entry.value)?,
        attributes,
    ))
}

fn encode_context_snapshot(snapshot: &ContextSnapshot) -> Result<String, String> {
    let entries = snapshot
        .entries
        .iter()
        .map(encode_context_entry)
        .collect::<Result<Vec<_>, _>>()?
        .join(",");
    Ok(format!(
        "snapshot(source({},{}),[{}])",
        encode_relation_value(&snapshot.provider)?,
        encode_relation_value(&snapshot.revision)?,
        entries,
    ))
}

fn encode_context_result(result: &ContextResult) -> Result<String, String> {
    Ok(match result {
        ContextResult::Empty(value) => format!("empty({value})"),
        ContextResult::One(entry) => format!("one({})", encode_context_entry(entry)?),
        ContextResult::All(entries) => format!(
            "all([{}])",
            entries
                .iter()
                .map(encode_context_entry)
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        ),
    })
}

fn encode_context_observation(observation: &ContextObservation) -> Result<String, String> {
    let outcome = match &observation.outcome {
        Some(result) => format!("some({})", encode_context_result(result)?),
        None => "none".into(),
    };
    Ok(format!(
        "observed({},{},source({},{}),{})",
        quote_atom(&observation.id),
        encode_context_query(&observation.query)?,
        encode_relation_value(&observation.provider)?,
        encode_relation_value(&observation.revision)?,
        outcome,
    ))
}

fn encode_context_graph(graph: &[ContextQueryNode]) -> Result<String, String> {
    Ok(format!(
        "[{}]",
        graph
            .iter()
            .map(encode_context_node)
            .collect::<Result<Vec<_>, String>>()?
            .join(",")
    ))
}

fn encode_context_node(node: &ContextQueryNode) -> Result<String, String> {
    Ok(format!(
        "query({},{})",
        quote_atom(&node.id),
        encode_context_query(&node.query)?,
    ))
}

fn encode_evidence_items(evidence: &[Evidence]) -> String {
    evidence
        .iter()
        .map(|item| {
            let paints = item
                .paint_spans
                .iter()
                .map(|span| encode_span(*span))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "evidence({},{},[{}],{},{},{},{},{})",
                quote_atom(&item.semantic),
                encode_span(item.span),
                paints,
                quote_string(&item.surface),
                quote_atom(&item.syntax),
                quote_atom(&item.provider),
                item.preference,
                quote_atom(&item.origin),
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn encode_context_plan(plan: &ContextPlan) -> Result<String, String> {
    let bindings = plan
        .bindings
        .iter()
        .map(|binding| {
            format!(
                "bind({},arg({}),entry_value)",
                quote_atom(&binding.query_id),
                binding.argument_index,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "plan({}, {}, [{}], [{}], {})",
        encode_command(&plan.command),
        encode_context_graph(&plan.queries)?,
        bindings,
        encode_evidence_items(&plan.evidence),
        plan.preference,
    ))
}

fn encode_context_completion_plan(plan: &ContextCompletionPlan) -> Result<String, String> {
    Ok(format!(
        "completion_context({},{},{},{},{},{})",
        quote_atom(&plan.action),
        encode_span(plan.replace),
        quote_string(&plan.surface),
        encode_context_graph(&plan.queries)?,
        quote_atom(&plan.target_query_id),
        plan.preference,
    ))
}

fn encode_parse_candidate(result: &ParseCandidate) -> String {
    let status = match &result.status {
        ParseStatus::Complete => "complete".to_string(),
        ParseStatus::Incomplete { edit_id } => format!("incomplete(edit({}))", quote_atom(edit_id)),
    };
    let evidence = encode_evidence_items(&result.evidence);
    format!(
        "parse_result({},{},[{}],{})",
        encode_command(&result.command),
        status,
        evidence,
        result.preference,
    )
}

#[derive(Clone, Debug)]
enum ParsedTerm {
    Atom(String),
    String(String),
    Integer(i64),
    Compound(String, Vec<ParsedTerm>),
    List(Vec<ParsedTerm>),
}

struct TermParser<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> TermParser<'a> {
    fn parse(input: &'a str) -> Result<ParsedTerm, String> {
        let mut parser = Self {
            input: input.as_bytes(),
            position: 0,
        };
        let term = parser.term()?;
        parser.space();
        if parser.position != parser.input.len() {
            return Err("trailing data in action grammar response".into());
        }
        Ok(term)
    }

    fn term(&mut self) -> Result<ParsedTerm, String> {
        self.space();
        match self.peek() {
            Some(b'[') => self.list(),
            Some(b'\"') => self.string(),
            Some(b'\'') => self.quoted_atom(),
            Some(b'-' | b'0'..=b'9') => self.integer(),
            Some(_) => self.atom_or_compound(),
            None => Err("unexpected end of action grammar response".into()),
        }
    }

    fn list(&mut self) -> Result<ParsedTerm, String> {
        self.position += 1;
        let mut values = Vec::new();
        self.space();
        if self.take(b']') {
            return Ok(ParsedTerm::List(values));
        }
        loop {
            values.push(self.term()?);
            self.space();
            if self.take(b']') {
                return Ok(ParsedTerm::List(values));
            }
            if !self.take(b',') {
                return Err("invalid list in action grammar response".into());
            }
        }
    }

    fn string(&mut self) -> Result<ParsedTerm, String> {
        let start = self.position;
        self.position += 1;
        let mut escaped = false;
        while let Some(byte) = self.peek() {
            self.position += 1;
            if escaped {
                escaped = false;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'\"' {
                let source = std::str::from_utf8(&self.input[start..self.position])
                    .map_err(|_| "non-UTF-8 action grammar string")?;
                let value = serde_json::from_str(source)
                    .map_err(|error| format!("invalid action grammar string: {error}"))?;
                return Ok(ParsedTerm::String(value));
            }
        }
        Err("unterminated string in action grammar response".into())
    }

    fn quoted_atom(&mut self) -> Result<ParsedTerm, String> {
        self.position += 1;
        let mut value = String::new();
        while let Some(byte) = self.peek() {
            self.position += 1;
            match byte {
                b'\'' => return Ok(ParsedTerm::Atom(value)),
                b'\\' => {
                    let escaped = self.peek().ok_or("unterminated atom escape")?;
                    self.position += 1;
                    value.push(escaped as char);
                }
                _ if byte.is_ascii() => value.push(byte as char),
                _ => return Err("non-ASCII quoted atom in action grammar response".into()),
            }
        }
        Err("unterminated atom in action grammar response".into())
    }

    fn integer(&mut self) -> Result<ParsedTerm, String> {
        let start = self.position;
        if self.peek() == Some(b'-') {
            self.position += 1;
        }
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        let source = std::str::from_utf8(&self.input[start..self.position]).unwrap();
        source
            .parse()
            .map(ParsedTerm::Integer)
            .map_err(|_| "invalid integer in action grammar response".into())
    }

    fn atom_or_compound(&mut self) -> Result<ParsedTerm, String> {
        let start = self.position;
        while self
            .peek()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
        {
            self.position += 1;
        }
        if start == self.position {
            return Err("invalid atom in action grammar response".into());
        }
        let name = std::str::from_utf8(&self.input[start..self.position])
            .unwrap()
            .to_string();
        self.space();
        if !self.take(b'(') {
            return Ok(ParsedTerm::Atom(name));
        }
        let mut args = Vec::new();
        loop {
            args.push(self.term()?);
            self.space();
            if self.take(b')') {
                return Ok(ParsedTerm::Compound(name, args));
            }
            if !self.take(b',') {
                return Err("invalid compound in action grammar response".into());
            }
        }
    }

    fn space(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }
    fn take(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }
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

fn response_value(response: &str) -> Result<ParsedTerm, String> {
    let term = TermParser::parse(response)?;
    if let Ok(args) = compound(&term, "ok", 1) {
        let mut args = args.to_vec();
        return Ok(args.remove(0));
    }
    if let Ok(args) = compound(&term, "error", 1) {
        return Err(format!(
            "action grammar rejected request: {}",
            term_text(&args[0])
        ));
    }
    Err("invalid action grammar application response".into())
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
        id: atom(&args[0])?.to_owned(),
        query: decode_context_query(&args[1])?,
        provider: decode_relation_value(&source[0])?,
        revision: decode_relation_value(&source[1])?,
        outcome: decode_context_outcome(&args[3])?,
    })
}

fn decode_context_node(term: &ParsedTerm) -> Result<ContextQueryNode, String> {
    let args = compound(term, "query", 2)?;
    Ok(ContextQueryNode {
        id: atom(&args[0])?.to_owned(),
        query: decode_context_query(&args[1])?,
    })
}

fn decode_context_dependency_key(term: &ParsedTerm) -> Result<ContextDependencyKey, String> {
    let args = compound(term, "dependency", 3)?;
    Ok(ContextDependencyKey {
        id: atom(&args[0])?.to_owned(),
        query: decode_context_query(&args[1])?,
        outcome: decode_context_outcome(&args[2])?,
    })
}

fn decode_context_binding(term: &ParsedTerm) -> Result<ContextBinding, String> {
    let args = compound(term, "bind", 3)?;
    let argument = compound(&args[1], "arg", 1)?;
    if atom(&args[2])? != "entry_value" {
        return Err("invalid context binding projection".into());
    }
    let argument_index = nonnegative(&argument[0])?;
    if argument_index == 0 {
        return Err("context binding uses zero argument index".into());
    }
    Ok(ContextBinding {
        query_id: atom(&args[0])?.to_owned(),
        argument_index,
    })
}

fn decode_context_plan(term: &ParsedTerm) -> Result<ContextPlan, String> {
    let args = compound(term, "plan", 5)?;
    Ok(ContextPlan {
        command: decode_command(&args[0])?,
        queries: list(&args[1])?
            .iter()
            .map(decode_context_node)
            .collect::<Result<_, _>>()?,
        bindings: list(&args[2])?
            .iter()
            .map(decode_context_binding)
            .collect::<Result<_, _>>()?,
        evidence: list(&args[3])?
            .iter()
            .map(decode_evidence)
            .collect::<Result<_, _>>()?,
        preference: integer(&args[4])?,
    })
}

fn decode_context_completion_plan(term: &ParsedTerm) -> Result<ContextCompletionPlan, String> {
    let args = compound(term, "completion_context", 6)?;
    Ok(ContextCompletionPlan {
        action: atom(&args[0])?.to_owned(),
        replace: decode_span(&args[1])?,
        surface: text(&args[2])?.to_owned(),
        queries: list(&args[3])?
            .iter()
            .map(decode_context_node)
            .collect::<Result<_, _>>()?,
        target_query_id: atom(&args[4])?.to_owned(),
        preference: integer(&args[5])?,
    })
}

fn decode_context_outcome_response(response: &str) -> Result<Option<ContextResult>, String> {
    decode_context_outcome(&response_value(response)?)
}

fn decode_context_observation_response(response: &str) -> Result<ContextObservation, String> {
    decode_context_observation(&response_value(response)?)
}

fn decode_context_ready_response(response: &str) -> Result<Vec<ContextQueryNode>, String> {
    let value = response_value(response)?;
    list(&value)?.iter().map(decode_context_node).collect()
}

fn decode_context_dependency_keys_response(
    response: &str,
) -> Result<Vec<ContextDependencyKey>, String> {
    let value = response_value(response)?;
    list(&value)?
        .iter()
        .map(decode_context_dependency_key)
        .collect()
}

fn decode_context_plans_response(response: &str) -> Result<Vec<ContextPlan>, String> {
    let value = response_value(response)?;
    list(&value)?.iter().map(decode_context_plan).collect()
}

fn decode_context_completion_plans_response(
    response: &str,
) -> Result<Vec<ContextCompletionPlan>, String> {
    let value = response_value(response)?;
    list(&value)?
        .iter()
        .map(decode_context_completion_plan)
        .collect()
}

fn decode_context_command_response(response: &str) -> Result<CommandAst, QueryError> {
    let term = TermParser::parse(response).map_err(QueryError::Backend)?;
    if let Ok(args) = compound(&term, "ok", 1) {
        return decode_command(&args[0]).map_err(QueryError::Backend);
    }
    if let Ok(args) = compound(&term, "error", 1) {
        if atom(&args[0]).ok() == Some("no_solution") {
            return Err(QueryError::NoSolution);
        }
        return Err(QueryError::Backend(format!(
            "action grammar rejected request: {}",
            term_text(&args[0])
        )));
    }
    Err(QueryError::Backend(
        "invalid context resolution response".into(),
    ))
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

fn decode_parse_candidate(term: &ParsedTerm) -> Result<ParseCandidate, String> {
    let args = compound(term, "parse_result", 4)?;
    let status = if atom(&args[1]).ok() == Some("complete") {
        ParseStatus::Complete
    } else {
        let incomplete = compound(&args[1], "incomplete", 1)?;
        let edit = compound(&incomplete[0], "edit", 1)?;
        ParseStatus::Incomplete {
            edit_id: atom(&edit[0])?.to_string(),
        }
    };
    Ok(ParseCandidate {
        command: decode_command(&args[0])?,
        status,
        evidence: list(&args[2])?
            .iter()
            .map(decode_evidence)
            .collect::<Result<_, _>>()?,
        preference: integer(&args[3])?,
    })
}

fn decode_parse_response(response: &str) -> Result<Vec<ParseCandidate>, String> {
    let value = response_value(response)?;
    list(&value)?.iter().map(decode_parse_candidate).collect()
}

fn decode_completion_response(response: &str) -> Result<Vec<Completion>, String> {
    let value = response_value(response)?;
    list(&value)?
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

fn decode_highlight_response(response: &str) -> Result<Vec<Highlight>, String> {
    let value = response_value(response)?;
    list(&value)?
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

fn decode_render_response(response: &str) -> Result<String, QueryError> {
    let term = TermParser::parse(response).map_err(QueryError::Backend)?;
    if let Ok(args) = compound(&term, "ok", 1) {
        return text(&args[0])
            .map(str::to_string)
            .map_err(QueryError::Backend);
    }
    if let Ok(args) = compound(&term, "error", 1) {
        if atom(&args[0]).ok() == Some("no_solution") {
            return Err(QueryError::NoSolution);
        }
        return Err(QueryError::Backend(format!(
            "action grammar rejected request: {}",
            term_text(&args[0])
        )));
    }
    Err(QueryError::Backend(
        "invalid action grammar application response".into(),
    ))
}

fn decode_action_help_response(
    response: &str,
) -> Result<Vec<crate::generated_wire::ActionHelpRow>, String> {
    use crate::generated_wire::{ActionHelpRow, LIMIT_SHORT_BYTES, LIMIT_TEXT_BYTES};
    use crate::wire::BoundedText;

    let value = response_value(response)?;
    let mut result = Vec::new();
    for row in list(&value)? {
        let fields = compound(row, "record", 3)?;
        let bounded = |value: &ParsedTerm, maximum: usize| -> Result<String, String> {
            let value = text(value)?.to_owned();
            if value.len() > maximum {
                return Err(format!(
                    "relation catalog text exceeds its declared {maximum}-byte bound"
                ));
            }
            Ok(value)
        };
        result.push(ActionHelpRow {
            verb: BoundedText::<LIMIT_SHORT_BYTES>::new(bounded(&fields[0], LIMIT_SHORT_BYTES)?)
                .map_err(|error| format!("invalid relation verb: {error:?}"))?,
            arguments: BoundedText::<LIMIT_TEXT_BYTES>::new(bounded(&fields[1], LIMIT_TEXT_BYTES)?)
                .map_err(|error| format!("invalid relation argument notation: {error:?}"))?,
            description: BoundedText::<LIMIT_TEXT_BYTES>::new(bounded(
                &fields[2],
                LIMIT_TEXT_BYTES,
            )?)
            .map_err(|error| format!("invalid relation description: {error:?}"))?,
        });
    }
    Ok(result)
}

fn decode_action_request_response(
    response: &str,
) -> Result<crate::generated_wire::ActionRequest, QueryError> {
    let term = TermParser::parse(response).map_err(QueryError::Backend)?;
    if let Ok(ok) = compound(&term, "ok", 1) {
        let request = compound(&ok[0], "action_request", 3).map_err(QueryError::Backend)?;
        let handler = atom(&request[0]).map_err(QueryError::Backend)?;
        let code: u64 = integer(&request[1])
            .map_err(QueryError::Backend)?
            .try_into()
            .map_err(|_| QueryError::Backend("negative relation action opcode".into()))?;
        let values = list(&request[2])
            .map_err(QueryError::Backend)?
            .iter()
            .map(decode_relation_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(QueryError::Backend)?;
        return crate::generated_wire::ActionRequest::from_relation(handler, code, &values)
            .map_err(QueryError::Backend);
    }
    if let Ok(error) = compound(&term, "error", 1) {
        if atom(&error[0]).ok() == Some("no_solution") {
            return Err(QueryError::NoSolution);
        }
        return Err(QueryError::Backend(format!(
            "action grammar rejected request conversion: {}",
            term_text(&error[0])
        )));
    }
    Err(QueryError::Backend(
        "invalid action request relation response".into(),
    ))
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
    std::hint::black_box(Prolog::action_request as fn(&Prolog, &CommandAst) -> _);
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

    #[test]
    fn typed_application_is_embedded_bounded_and_closed() {
        let prolog = global().unwrap();
        let duplicate = match Prolog::new() {
            Ok(_) => panic!("created duplicate runtime"),
            Err(error) => error,
        };
        assert!(duplicate.contains("already active"));
        assert_eq!(
            prolog
                .render(&command("mirror_jobs", None))
                .unwrap()
                .text,
            "mirror jobs",
        );
        assert_eq!(
            prolog
                .render(&command("mirror_run", Some(7)))
                .unwrap()
                .text,
            "mirror run 7",
        );
        assert_eq!(
            prolog
                .render(&command("kill", Some(5)))
                .unwrap()
                .text,
            "kill 5",
        );
        assert_eq!(
            prolog
                .render(&command("mirror_run_pending", None))
                .unwrap()
                .text,
            "mirror run pending",
        );
        assert_eq!(
            prolog
                .action_request(&CommandAst {
                    action: "mirror_resume".into(),
                    handler: "mirror_pause".into(),
                    target: "ui".into(),
                    args: vec![CommandValue::Integer(7), CommandValue::Boolean(false)],
                })
                .unwrap(),
            crate::generated_wire::ActionRequest::MirrorPause {
                id: 7,
                paused: false,
            },
        );
        let oci = prolog
            .action_request(&CommandAst {
                action: "oci.build".into(),
                handler: "oci.build".into(),
                target: "ui".into(),
                args: vec![CommandValue::OciSpec {
                    context_tar_gz: "eA==".into(),
                    dockerfile: "FROM scratch\n".into(),
                    tag: Some("example:test".into()),
                    net_mode: "tap".into(),
                    build_arguments: vec![("A".into(), "one".into())],
                }],
            })
            .unwrap();
        let crate::generated_wire::ActionRequest::OciBuild { spec } = oci else {
            panic!("structured relation returned the wrong request variant")
        };
        assert_eq!(spec.context_tar_gz.as_slice(), b"x");
        assert_eq!(spec.dockerfile.as_slice(), b"FROM scratch\n");
        assert_eq!(spec.tag.as_ref().unwrap().as_str(), "example:test");
        assert_eq!(spec.net_mode, crate::generated_wire::NetMode::Tap);
        assert_eq!(spec.build_arguments.as_map().len(), 1);
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
        assert!(matches!(
            prolog.action_request(&parsed_oci[0].command).unwrap(),
            crate::generated_wire::ActionRequest::OciBuild { .. }
        ));
        prolog.exhaust_inference_limit().unwrap();
        assert_eq!(
            prolog
                .render(&command("mirror_rm", Some(11)))
                .unwrap()
                .text,
            "mirror rm 11",
        );
    }

    #[test]
    fn render_no_solution_is_distinct_from_backend_decode_error() {
        assert_eq!(
            decode_render_response("error(no_solution)"),
            Err(QueryError::NoSolution)
        );
        assert!(matches!(
            decode_render_response("not_a_response"),
            Err(QueryError::Backend(_))
        ));
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
            .observe_context("box_query", &box_query, &snapshot)
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
                        id: "box_query".into(),
                        query: box_query,
                    },
                    ContextQueryNode {
                        id: "path_query".into(),
                        query: path_query,
                    },
                ],
                &[observation],
            )
            .unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "path_query");
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
        assert_eq!(
            plan.bindings,
            vec![ContextBinding {
                query_id: "q1".into(),
                argument_index: 1,
            }],
        );

        let entry = ContextEntry {
            domain: RelationValue::Atom("box".into()),
            identity: RelationValue::Integer(5),
            names: vec!["work".into()],
            value: RelationValue::Compound("integer".into(), vec![RelationValue::Integer(5)]),
            attributes: Vec::new(),
        };
        let observation = ContextObservation {
            id: "q1".into(),
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
        assert_eq!(plans[0].target_query_id, "q1");
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
        assert_eq!(rows.len(), 91);
        let verbs = rows
            .iter()
            .map(|row| row.verb.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(verbs.len(), rows.len());
        assert!(verbs.contains("verbs"));
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
            row.verb.as_str().contains("mirror")
                || row.description.as_str().contains("mirror")
        }));
    }
}
