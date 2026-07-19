//! Brush's typed builtin parsers as one opaque host relation client.
//!
//! This adapter is intentionally conservative. It proves the existing
//! `Command::new` parser can supply ordinary relation evidence without first
//! copying its grammar into `CommandSyntax`. The current input projection only
//! accepts already-cooked, whitespace-separated words with a tear at a word
//! boundary. Quoting, expansion provenance, and contextual value providers
//! require the richer argv/continuation protocol recorded in
//! `BRUSH-RELATION-MIGRATION.md`; those shapes fail closed here.

use brush_core::builtins::{
    CommandArgumentEvidence, CommandParseObservation, CommandParseStatus, CommandProbeInput,
};

use crate::prolog::{RelationBinding, RelationReply, RelationSolution, RelationValue};
use crate::relation_adapter::{Adapter, Request};

pub(crate) const HANDLE: &str = "brush_typed_builtins";
const PROVIDER: &str = "brush_clap";
const PREFERENCE: i64 = 40;

pub(crate) struct BuiltinProbeAdapter;

pub(crate) fn register() -> Result<(), String> {
    crate::relation_adapter::register(HANDLE, std::sync::Arc::new(BuiltinProbeAdapter))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteSpan {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Word {
    text: String,
    span: ByteSpan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SourceMode {
    Exact,
    Assist {
        edit_id: String,
        virtual_span: ByteSpan,
        physical_span: Option<ByteSpan>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Source {
    text: String,
    mode: SourceMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProbeAtTear {
    input: CommandProbeInput,
    replace: ByteSpan,
    needs_separator: bool,
    edit_id: String,
}

impl Adapter for BuiltinProbeAdapter {
    fn revision(&self) -> RelationValue {
        RelationValue::Compound(
            "brush_typed_builtin_revision".into(),
            vec![RelationValue::Integer(1)],
        )
    }

    fn transform(&self, request: &Request) -> Result<RelationReply, String> {
        let source = request_source(request)?;
        let words = simple_words(&source.text).ok_or_else(|| {
            "typed builtin probe needs rich shell-word input for this source".to_string()
        })?;
        let Some(command_name) = words.first().map(|word| word.text.clone()) else {
            return Ok(no_solution());
        };
        let Some(probe) = super::builtin_command_probe(&command_name) else {
            return Ok(no_solution());
        };

        match source.mode {
            SourceMode::Exact => {
                let observation = probe(CommandProbeInput {
                    before: words.into_iter().map(|word| word.text).collect(),
                    prefix: String::new(),
                    suffix: String::new(),
                    after: Vec::new(),
                });
                exact_reply(request, observation)
            }
            SourceMode::Assist {
                edit_id,
                virtual_span,
                physical_span,
            } => {
                let Some(at_tear) =
                    probe_at_tear(&source.text, &words, &edit_id, virtual_span, physical_span)
                else {
                    return Ok(diagnostic("rich_argv_required"));
                };
                let observation = probe(at_tear.input.clone());
                assist_reply(request, &command_name, at_tear, observation)
            }
        }
    }
}

fn request_source(request: &Request) -> Result<Source, String> {
    let value = request
        .given
        .iter()
        .find(|binding| binding.name == "source")
        .map(|binding| &binding.value)
        .ok_or_else(|| "typed builtin relation omitted source".to_string())?;
    let RelationValue::Compound(name, fields) = value else {
        return Err("typed builtin source is not a compound".into());
    };
    if name != "text_source" || fields.len() != 3 {
        return Err("typed builtin source has an invalid shape".into());
    }
    let RelationValue::String(text) = &fields[0] else {
        return Err("typed builtin source is not text".into());
    };
    let mode = decode_mode(&fields[1])?;
    Ok(Source {
        text: text.clone(),
        mode,
    })
}

fn decode_mode(value: &RelationValue) -> Result<SourceMode, String> {
    if value == &RelationValue::Atom("exact".into()) {
        return Ok(SourceMode::Exact);
    }
    let RelationValue::Compound(name, fields) = value else {
        return Err("typed builtin source mode is invalid".into());
    };
    if name != "assist" || !(fields.len() == 2 || fields.len() == 3) {
        return Err("typed builtin assist mode is invalid".into());
    }
    let edit_id = match &fields[0] {
        RelationValue::Atom(value) => value.clone(),
        _ => return Err("typed builtin edit identity is not an atom".into()),
    };
    let virtual_span = decode_span(&fields[1], "span")?;
    let physical_span = fields
        .get(2)
        .map(|value| decode_span(value, "replace_span"))
        .transpose()?;
    Ok(SourceMode::Assist {
        edit_id,
        virtual_span,
        physical_span,
    })
}

fn decode_span(value: &RelationValue, expected: &str) -> Result<ByteSpan, String> {
    let RelationValue::Compound(name, fields) = value else {
        return Err(format!("typed builtin {expected} is not a compound"));
    };
    if name != expected || fields.len() != 2 {
        return Err(format!("typed builtin {expected} has an invalid shape"));
    }
    let [RelationValue::Integer(start), RelationValue::Integer(end)] = fields.as_slice() else {
        return Err(format!("typed builtin {expected} is not integral"));
    };
    let start = usize::try_from(*start)
        .map_err(|_| format!("typed builtin {expected} start is invalid"))?;
    let end =
        usize::try_from(*end).map_err(|_| format!("typed builtin {expected} end is invalid"))?;
    if start > end {
        return Err(format!("typed builtin {expected} is reversed"));
    }
    Ok(ByteSpan { start, end })
}

fn simple_words(text: &str) -> Option<Vec<Word>> {
    if text
        .chars()
        .any(|character| matches!(character, '\n' | '\\' | '\'' | '"' | '$' | '`'))
    {
        return None;
    }
    let mut words = Vec::new();
    let mut start = None;
    for (offset, character) in text.char_indices() {
        if matches!(character, ' ' | '\t' | '\r') {
            if let Some(begin) = start.take() {
                words.push(Word {
                    text: text[begin..offset].into(),
                    span: ByteSpan {
                        start: begin,
                        end: offset,
                    },
                });
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(begin) = start {
        words.push(Word {
            text: text[begin..].into(),
            span: ByteSpan {
                start: begin,
                end: text.len(),
            },
        });
    }
    Some(words)
}

fn probe_at_tear(
    text: &str,
    words: &[Word],
    edit_id: &str,
    virtual_span: ByteSpan,
    physical_span: Option<ByteSpan>,
) -> Option<ProbeAtTear> {
    if virtual_span.start != virtual_span.end
        || virtual_span.end > text.len()
        || !text.is_char_boundary(virtual_span.end)
    {
        return None;
    }
    let cursor = virtual_span.end;
    let command = words.first()?;
    if cursor < command.span.end {
        return None;
    }

    // An exact command word followed by a tear is command-argument position,
    // even when the user has not typed the separating space yet.
    if cursor == command.span.end && words.len() == 1 {
        return Some(ProbeAtTear {
            input: CommandProbeInput {
                before: vec![command.text.clone()],
                prefix: String::new(),
                suffix: String::new(),
                after: Vec::new(),
            },
            replace: physical_span.unwrap_or(ByteSpan {
                start: cursor,
                end: cursor,
            }),
            needs_separator: !ends_with_separator(&text[..cursor]),
            edit_id: edit_id.into(),
        });
    }

    if let Some((index, word)) = words
        .iter()
        .enumerate()
        .find(|(_, word)| word.span.start <= cursor && cursor <= word.span.end)
    {
        if index == 0 {
            return None;
        }
        return Some(ProbeAtTear {
            input: CommandProbeInput {
                before: words[..index]
                    .iter()
                    .map(|word| word.text.clone())
                    .collect(),
                prefix: text[word.span.start..cursor].into(),
                suffix: text[cursor..word.span.end].into(),
                after: words[index + 1..]
                    .iter()
                    .map(|word| word.text.clone())
                    .collect(),
            },
            replace: physical_span.unwrap_or(ByteSpan {
                start: word.span.start,
                end: cursor,
            }),
            needs_separator: false,
            edit_id: edit_id.into(),
        });
    }

    let before = words
        .iter()
        .take_while(|word| word.span.end <= cursor)
        .map(|word| word.text.clone())
        .collect::<Vec<_>>();
    let after = words
        .iter()
        .skip_while(|word| word.span.start < cursor)
        .map(|word| word.text.clone())
        .collect::<Vec<_>>();
    Some(ProbeAtTear {
        input: CommandProbeInput {
            before,
            prefix: String::new(),
            suffix: String::new(),
            after,
        },
        replace: physical_span.unwrap_or(ByteSpan {
            start: cursor,
            end: cursor,
        }),
        needs_separator: cursor > 0 && !ends_with_separator(&text[..cursor]),
        edit_id: edit_id.into(),
    })
}

fn ends_with_separator(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|character| matches!(character, ' ' | '\t' | '\r'))
}

fn exact_reply(
    request: &Request,
    observation: CommandParseObservation,
) -> Result<RelationReply, String> {
    match observation.status {
        CommandParseStatus::Complete => {
            solution_reply(request, RelationValue::Atom("complete".into()), Vec::new())
        }
        CommandParseStatus::Incomplete => Ok(no_solution()),
        CommandParseStatus::Rejected(_) => Ok(diagnostic("parser_rejected")),
    }
}

fn assist_reply(
    request: &Request,
    command_name: &str,
    at_tear: ProbeAtTear,
    observation: CommandParseObservation,
) -> Result<RelationReply, String> {
    if matches!(observation.status, CommandParseStatus::Rejected(_)) {
        return Ok(diagnostic("parser_rejected"));
    }
    let mut candidates = Vec::<(String, Vec<RelationValue>)>::new();
    let mut context_queries = Vec::new();
    for continuation in observation.literal_continuations {
        let literal = continuation.literal;
        candidates.push((
            literal.clone(),
            vec![clap_alternative(
                command_name,
                &continuation.argument,
                &literal,
            )],
        ));
    }
    let mut arguments = observation.expected;
    arguments.extend(observation.tear_arguments);
    for argument in arguments {
        for value in &argument.possible_values {
            if value.starts_with(&at_tear.input.prefix) && value != &at_tear.input.prefix {
                candidates.push((
                    value.clone(),
                    vec![clap_alternative(command_name, &argument, value)],
                ));
            }
        }
        let Some(domain) = value_hint_domain(argument.value_hint) else {
            continue;
        };
        let query = argument_context_query(command_name, &argument, &at_tear, domain);
        let Some(observation) = request
            .observations
            .iter()
            .find(|observation| observation.id == query.id && observation.query == query.query)
        else {
            context_queries.push(crate::prolog::context_query_node_value(&query));
            continue;
        };
        let Some(crate::prolog::ContextResult::All(entries)) = &observation.outcome else {
            continue;
        };
        for entry in entries {
            for name in &entry.names {
                if name.starts_with(&at_tear.input.prefix) && name != &at_tear.input.prefix {
                    candidates.push((
                        name.clone(),
                        vec![context_alternative(domain, &entry.identity)],
                    ));
                }
            }
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    let mut grouped = Vec::<(String, Vec<RelationValue>)>::new();
    for (insert, alternatives) in candidates {
        if let Some((_, existing)) = grouped.last_mut().filter(|(value, _)| value == &insert) {
            existing.extend(alternatives);
        } else {
            grouped.push((insert, alternatives));
        }
    }
    let mut candidates = grouped;
    candidates.truncate(request.limits.max_evidence);
    let completions = candidates
        .into_iter()
        .enumerate()
        .map(|(rank, (candidate, alternatives))| {
            completion_value(
                at_tear.replace,
                if at_tear.needs_separator {
                    format!(" {candidate}")
                } else {
                    candidate
                },
                alternatives,
                rank + 1,
            )
        })
        .collect();
    let mut reply = solution_reply(
        request,
        RelationValue::Compound(
            "incomplete".into(),
            vec![RelationValue::Atom(at_tear.edit_id)],
        ),
        completions,
    )?;
    reply.context_queries = context_queries;
    Ok(reply)
}

fn completion_value(
    replace: ByteSpan,
    insert: String,
    alternatives: Vec<RelationValue>,
    rank: usize,
) -> RelationValue {
    RelationValue::Compound(
        "completion".into(),
        vec![
            span_value(replace),
            RelationValue::String(insert),
            RelationValue::List(alternatives),
            RelationValue::Integer(PREFERENCE),
            RelationValue::Integer(i64::try_from(rank).unwrap_or(i64::MAX)),
        ],
    )
}

fn clap_alternative(
    command_name: &str,
    argument: &CommandArgumentEvidence,
    value: &str,
) -> RelationValue {
    RelationValue::Compound(
        "alternative".into(),
        vec![
            RelationValue::Compound(
                "clap_argument".into(),
                vec![
                    RelationValue::String(command_name.into()),
                    RelationValue::String(argument.id.clone()),
                    RelationValue::String(value.into()),
                ],
            ),
            RelationValue::Atom("builtin_argument".into()),
            RelationValue::Atom(PROVIDER.into()),
            RelationValue::Integer(PREFERENCE),
        ],
    )
}

fn context_alternative(domain: &str, identity: &RelationValue) -> RelationValue {
    RelationValue::Compound(
        "alternative".into(),
        vec![
            RelationValue::Compound(
                "context".into(),
                vec![RelationValue::Atom(domain.into()), identity.clone()],
            ),
            RelationValue::Atom("builtin_argument".into()),
            RelationValue::Atom(PROVIDER.into()),
            RelationValue::Integer(PREFERENCE),
        ],
    )
}

fn value_hint_domain(hint: clap::ValueHint) -> Option<&'static str> {
    match hint {
        clap::ValueHint::AnyPath => Some("filesystem_path"),
        clap::ValueHint::FilePath => Some("filesystem_file"),
        clap::ValueHint::DirPath => Some("filesystem_directory"),
        clap::ValueHint::ExecutablePath => Some("filesystem_executable"),
        _ => None,
    }
}

fn argument_context_query(
    command_name: &str,
    argument: &CommandArgumentEvidence,
    at_tear: &ProbeAtTear,
    domain: &str,
) -> crate::prolog::ContextQueryNode {
    crate::prolog::ContextQueryNode {
        id: RelationValue::Compound(
            "builtin_argument_context".into(),
            vec![
                RelationValue::String(command_name.into()),
                RelationValue::String(argument.id.clone()),
                span_value(at_tear.replace),
            ],
        ),
        query: crate::prolog::ContextQuery {
            cardinality: crate::prolog::ContextCardinality::All,
            domain: RelationValue::Atom(domain.into()),
            selector: RelationValue::Compound(
                "prefix".into(),
                vec![RelationValue::String(at_tear.input.prefix.clone())],
            ),
        },
    }
}

fn solution_reply(
    request: &Request,
    status: RelationValue,
    completions: Vec<RelationValue>,
) -> Result<RelationReply, String> {
    let available = [
        RelationBinding {
            name: "status".into(),
            value: status,
        },
        RelationBinding {
            name: "completions".into(),
            value: RelationValue::List(completions),
        },
    ];
    let bindings = request
        .wanted
        .iter()
        .map(|wanted| {
            available
                .iter()
                .find(|binding| &binding.name == wanted)
                .cloned()
                .ok_or_else(|| format!("typed builtin relation cannot project {wanted}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RelationReply {
        solutions: vec![RelationSolution {
            bindings,
            preference: PREFERENCE,
        }],
        context_queries: vec![],
        dependency_keys: vec![],
        diagnostics: vec![],
    })
}

fn span_value(span: ByteSpan) -> RelationValue {
    RelationValue::Compound(
        "span".into(),
        vec![
            RelationValue::Integer(i64::try_from(span.start).unwrap_or(i64::MAX)),
            RelationValue::Integer(i64::try_from(span.end).unwrap_or(i64::MAX)),
        ],
    )
}

fn no_solution() -> RelationReply {
    RelationReply {
        solutions: vec![],
        context_queries: vec![],
        dependency_keys: vec![],
        diagnostics: vec![],
    }
}

fn diagnostic(name: &str) -> RelationReply {
    RelationReply {
        solutions: vec![],
        context_queries: vec![],
        dependency_keys: vec![],
        diagnostics: vec![RelationValue::Compound(
            "diagnostic".into(),
            vec![RelationValue::Atom(name.into())],
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prolog::RelationLimits;

    fn request(source: &str, mode: RelationValue, wanted: &[&str]) -> Request {
        Request {
            given: vec![RelationBinding {
                name: "source".into(),
                value: RelationValue::Compound(
                    "text_source".into(),
                    vec![
                        RelationValue::String(source.into()),
                        mode,
                        RelationValue::Atom("test".into()),
                    ],
                ),
            }],
            wanted: wanted.iter().map(|value| (*value).into()).collect(),
            observations: vec![],
            limits: RelationLimits::default(),
        }
    }

    fn assist(cursor: usize) -> RelationValue {
        RelationValue::Compound(
            "assist".into(),
            vec![
                RelationValue::Atom("edit".into()),
                RelationValue::Compound(
                    "span".into(),
                    vec![
                        RelationValue::Integer(cursor as i64),
                        RelationValue::Integer(cursor as i64),
                    ],
                ),
            ],
        )
    }

    fn completion_texts(reply: &RelationReply) -> Vec<String> {
        let RelationValue::List(values) = &reply.solutions[0]
            .bindings
            .iter()
            .find(|binding| binding.name == "completions")
            .unwrap()
            .value
        else {
            panic!("completion binding must be a list")
        };
        values
            .iter()
            .map(|value| {
                let RelationValue::Compound(_, fields) = value else {
                    panic!("completion must be a compound")
                };
                let RelationValue::String(text) = &fields[1] else {
                    panic!("completion insertion must be text")
                };
                text.clone()
            })
            .collect()
    }

    #[test]
    fn bind_finite_values_come_from_runtime_parser_probe() {
        let source = "bind -m ";
        let reply = BuiltinProbeAdapter
            .transform(&request(
                source,
                assist(source.len()),
                &["status", "completions"],
            ))
            .unwrap();
        assert_eq!(
            completion_texts(&reply),
            [
                "emacs-ctlx",
                "emacs-meta",
                "emacs-standard",
                "vi-command",
                "vi-insert",
            ]
        );
    }

    #[test]
    fn bind_finite_value_completion_preserves_and_proves_same_word_suffix() {
        let source = "bind -m em-standard";
        let cursor = "bind -m em".len();
        let reply = BuiltinProbeAdapter
            .transform(&request(source, assist(cursor), &["status", "completions"]))
            .unwrap();
        assert_eq!(completion_texts(&reply), ["emacs"]);

        let incompatible = "bind -m em-not-a-keymap";
        let reply = BuiltinProbeAdapter
            .transform(&request(
                incompatible,
                assist(cursor),
                &["status", "completions"],
            ))
            .unwrap();
        assert!(reply.solutions.is_empty());
        assert_eq!(
            reply.diagnostics,
            [RelationValue::Compound(
                "diagnostic".into(),
                vec![RelationValue::Atom("parser_rejected".into())],
            )]
        );
    }

    #[test]
    fn exact_bind_uses_the_same_typed_parser() {
        let reply = BuiltinProbeAdapter
            .transform(&request(
                "bind -m emacs-standard",
                RelationValue::Atom("exact".into()),
                &["status"],
            ))
            .unwrap();
        assert_eq!(
            reply.solutions[0].bindings[0].value,
            RelationValue::Atom("complete".into())
        );
    }

    #[test]
    fn edit_path_replays_from_explicit_context_observation() {
        let source = "edit ";
        let mut request = request(source, assist(source.len()), &["status", "completions"]);
        let pending = BuiltinProbeAdapter.transform(&request).unwrap();
        assert_eq!(
            pending.solutions[0]
                .bindings
                .iter()
                .find(|binding| binding.name == "completions")
                .unwrap()
                .value,
            RelationValue::List(vec![])
        );
        let graph =
            crate::prolog::context_query_nodes_from_values(&pending.context_queries).unwrap();
        assert_eq!(graph.len(), 1);
        assert_eq!(
            graph[0].query.domain,
            RelationValue::Atom("filesystem_path".into())
        );
        request
            .observations
            .push(crate::prolog::ContextDependencyKey {
                id: graph[0].id.clone(),
                query: graph[0].query.clone(),
                outcome: Some(crate::prolog::ContextResult::All(vec![
                    crate::prolog::ContextEntry {
                        domain: RelationValue::Atom("filesystem_path".into()),
                        identity: RelationValue::String("/tmp/test1.sh".into()),
                        names: vec!["./test1.sh".into()],
                        value: RelationValue::Compound(
                            "filesystem_path".into(),
                            vec![RelationValue::String("/tmp/test1.sh".into())],
                        ),
                        attributes: vec![RelationValue::Atom("file".into())],
                    },
                ])),
            });
        let resolved = BuiltinProbeAdapter.transform(&request).unwrap();
        assert!(resolved.context_queries.is_empty());
        assert_eq!(completion_texts(&resolved), ["./test1.sh"]);
    }

    #[test]
    fn edit_path_round_trips_through_registered_relation_and_provider() {
        let _ = register();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("test1.sh"), b"").unwrap();
        let source = "edit ./t";
        let mut request = crate::prolog::RelationRequest {
            grammar: RelationValue::Compound(
                "registered_relation".into(),
                vec![RelationValue::Atom(HANDLE.into())],
            ),
            given: vec![RelationBinding {
                name: "source".into(),
                value: RelationValue::Compound(
                    "text_source".into(),
                    vec![
                        RelationValue::String(source.into()),
                        assist(source.len()),
                        RelationValue::Atom("test".into()),
                    ],
                ),
            }],
            wanted: vec!["status".into(), "completions".into()],
            observations: vec![],
            limits: RelationLimits::default(),
        };
        let prolog = crate::prolog::global().unwrap();
        let context = crate::parser::FilesystemContext::new(temp.path());
        let mut query_rounds = 0;
        let mut resolved = None;
        for _ in 0..8 {
            let reply = prolog.transform(&request).unwrap();
            assert!(reply.diagnostics.is_empty(), "{reply:#?}");
            if reply.context_queries.is_empty() {
                resolved = Some(reply);
                break;
            }
            query_rounds += 1;
            let graph =
                crate::prolog::context_query_nodes_from_values(&reply.context_queries).unwrap();
            let observations = crate::parser::execute_context_graph(prolog, &graph, &context)
                .unwrap()
                .unwrap();
            for observation in observations {
                let value = crate::prolog::context_observation_value(&observation).unwrap();
                if !request.observations.contains(&value) {
                    request.observations.push(value);
                }
            }
        }
        let resolved = resolved.expect("registered Brush relation must settle");
        assert_eq!(query_rounds, 3);
        assert_eq!(completion_texts(&resolved), ["./test1.sh"]);
        assert_eq!(resolved.dependency_keys.len(), 2);
    }

    #[test]
    fn shell_quoting_fails_closed_until_rich_words_cross_boundary() {
        let reply = BuiltinProbeAdapter.transform(&request(
            "bind -m 'emacs-standard'",
            RelationValue::Atom("exact".into()),
            &["status"],
        ));
        assert!(reply.is_err());
    }
}
