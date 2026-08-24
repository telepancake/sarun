use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventContext {
    pub edge: Option<String>,
    pub correlation_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildEdge {
    pub outputs: Vec<String>,
    pub inputs: Vec<String>,
    pub command: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    BuildGraph { tool: String, edges: Vec<BuildEdge> },
    EdgeStarted { output: String, command: Option<String> },
    EdgeOutput { output: String, bytes: Vec<u8> },
    EdgeFinished { output: String, code: i32 },
    Activity { description: String, age_seconds: f64 },
    VariableAssignment { fields: serde_json::Value },
    Diagnostic { message: String },
}

pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: &Event);
}

pub trait ContextProvider: Send + Sync + 'static {
    fn current(&self) -> EventContext;
}

#[derive(Debug)]
struct TracingSink;

impl EventSink for TracingSink {
    fn emit(&self, event: &Event) {
        tracing::debug!(target: "bumba", event = ?event);
    }
}

static EVENT_SINK: OnceLock<Arc<dyn EventSink>> = OnceLock::new();
static CONTEXT_PROVIDER: OnceLock<Arc<dyn ContextProvider>> = OnceLock::new();

/// Install the process-wide event sink used by build workers.
///
/// Brush, rkati, and n2 currently expose process-global hook installation, so
/// Bumba deliberately has one event configuration per process. Per-invocation
/// cwd, environment, streams, and filesystem providers remain scoped.
pub fn set_event_sink(sink: Arc<dyn EventSink>) -> Result<(), Arc<dyn EventSink>> {
    EVENT_SINK.set(sink)
}

/// Install a host context provider for correlating observations with the
/// host's own unit of work. Bumba does not interpret either field.
pub fn set_context_provider(
    provider: Arc<dyn ContextProvider>,
) -> Result<(), Arc<dyn ContextProvider>> {
    CONTEXT_PROVIDER.set(provider)
}

pub(crate) fn current_context() -> EventContext {
    CONTEXT_PROVIDER
        .get()
        .map(|provider| provider.current())
        .unwrap_or_default()
}

pub(crate) fn emit(event: Event) {
    EVENT_SINK
        .get_or_init(|| Arc::new(TracingSink))
        .emit(&event);
}
