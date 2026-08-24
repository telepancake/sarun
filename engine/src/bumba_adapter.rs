//! Sarun-owned adapters for the reusable Bumba runtime.

struct SarunEvents;

struct SarunContext;

impl bumba::ContextProvider for SarunContext {
    fn current(&self) -> bumba::EventContext {
        bumba::EventContext {
            edge: crate::brush::current_recipe_edge(),
            correlation_id: crate::brush::current_pipeline_uid(),
        }
    }
}

impl bumba::EventSink for SarunEvents {
    fn emit(&self, event: &bumba::Event) {
        match event {
            bumba::Event::BuildGraph { edges, .. } => {
                let edges = edges
                    .iter()
                    .map(|edge| {
                        serde_json::json!({
                            "outs": edge.outputs,
                            "ins": edge.inputs,
                            "cmd": edge.command,
                        })
                    })
                    .collect::<Vec<_>>();
                let message = serde_json::json!({"type": "build_edges", "edges": edges});
                crate::runner::send_nested_prov(format!("{message}\n").as_bytes());
            }
            bumba::Event::EdgeStarted { output, command } => {
                crate::brush::send_build_edge_state(
                    Some(output),
                    command.as_deref(),
                    "start",
                    0,
                    None,
                );
            }
            bumba::Event::EdgeOutput { .. } => {}
            bumba::Event::EdgeFinished { output, code } => {
                crate::brush::send_build_edge_state(
                    Some(output),
                    None,
                    "done",
                    *code,
                    None,
                );
            }
            bumba::Event::Activity {
                description,
                age_seconds,
            } => {
                let message = serde_json::json!({
                    "type": "box_activity",
                    "items": [[description, age_seconds]],
                });
                crate::runner::send_nested_prov(format!("{message}\n").as_bytes());
            }
            bumba::Event::VariableAssignment { fields } => {
                let message = serde_json::json!({"type": "make_vars", "rows": [fields]});
                crate::runner::send_nested_prov(format!("{message}\n").as_bytes());
            }
            bumba::Event::Diagnostic { message } => eprintln!("bumba: {message}"),
        }
    }
}

fn run_recipe(
    prefix: &str,
    command: &str,
    output: &mut dyn FnMut(&[u8]),
    stderr: bumba::RecipeStderr,
    stdin: Option<std::os::fd::OwnedFd>,
) -> i32 {
    let stderr = match stderr {
        bumba::RecipeStderr::Merge => crate::brush::RecipeStderr::Merge,
        bumba::RecipeStderr::Inherit => crate::brush::RecipeStderr::Inherit,
        bumba::RecipeStderr::Null => crate::brush::RecipeStderr::Null,
    };
    crate::brush::run_recipe_in_process_prefixed(
        prefix,
        command,
        output,
        stderr,
        stdin,
    )
}

pub fn install() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let _ = bumba::set_event_sink(std::sync::Arc::new(SarunEvents));
        let _ = bumba::set_context_provider(std::sync::Arc::new(SarunContext));
        let _ = bumba::set_recipe_executor(run_recipe);
        let _ = bumba::jobserver::set_path("/.slopbox-jobserver");
    });
}
