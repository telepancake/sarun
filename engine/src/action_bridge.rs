//! Sarun-owned glue between the command AST and generated wire AST.
//!
//! Neither the text grammar nor the binary codec imports this module. Most
//! fields cross by structural matching: this bridge offers the small set of
//! wrapper/option/list candidates that can represent a command value, and the
//! generated closed wire relation accepts only a correctly typed shape. The
//! few source-level forms whose ASTs intentionally differ are explicit below.

use crate::generated_wire::{ACTION_REQUEST_IDENTITIES, ActionRequest};
use crate::prolog::{CommandAst, CommandValue, RelationValue};

const MAX_CANDIDATES: usize = 4096;

pub fn materialize(command: &CommandAst) -> Result<ActionRequest, String> {
    if command.target != "ui" && command.target != "control" {
        return Err(format!(
            "action {} targets {}, not the binary action wire",
            command.action, command.target
        ));
    }
    let code = ACTION_REQUEST_IDENTITIES
        .iter()
        .find_map(|(handler, code)| (*handler == command.handler).then_some(*code))
        .ok_or_else(|| format!("unknown binary action handler {}", command.handler))?;

    let candidates = match command.handler.as_str() {
        "ro_attach" => ro_attach_candidates(&command.args)?,
        "view.open" => view_open_candidates(&command.args)?,
        "view.filter" => view_filter_candidates(&command.args)?,
        _ => structural_sequences(&command.args)?,
    };
    unique_request(&command.handler, code, candidates)
}

fn unique_request(
    handler: &str,
    code: u64,
    candidates: Vec<Vec<RelationValue>>,
) -> Result<ActionRequest, String> {
    let mut matches = Vec::new();
    for values in candidates {
        if let Ok(request) = ActionRequest::from_relation(handler, code, &values)
            && !matches.contains(&request)
        {
            matches.push(request);
            if matches.len() > 1 {
                return Err(format!(
                    "command AST maps ambiguously to binary handler {handler}"
                ));
            }
        }
    }
    matches
        .pop()
        .ok_or_else(|| format!("command AST has no binary shape for handler {handler}"))
}

fn structural_sequences(arguments: &[CommandValue]) -> Result<Vec<Vec<RelationValue>>, String> {
    let base = cartesian_values(arguments)?;
    let mut candidates = Vec::new();
    for values in base {
        push_candidate(&mut candidates, values.clone())?;
        option_variants(&values, 0, &mut Vec::new(), &mut candidates)?;
        for split in 0..values.len() {
            let mut grouped = values[..split].to_vec();
            grouped.push(RelationValue::List(values[split..].to_vec()));
            push_candidate(&mut candidates, grouped)?;
        }
    }

    // Optional source fields are absent from CommandAst. The wire AST carries
    // that absence explicitly. Empty repeated fields are similarly explicit.
    let seeds = candidates.clone();
    for values in seeds {
        insert_default_variants(&values, 2, &mut candidates)?;
    }
    Ok(candidates)
}

fn cartesian_values(arguments: &[CommandValue]) -> Result<Vec<Vec<RelationValue>>, String> {
    let mut rows = vec![Vec::new()];
    for argument in arguments {
        let alternatives = value_candidates(argument)?;
        let mut next = Vec::new();
        for row in rows {
            for value in &alternatives {
                let mut candidate = row.clone();
                candidate.push(value.clone());
                push_candidate(&mut next, candidate)?;
            }
        }
        rows = next;
    }
    Ok(rows)
}

fn value_candidates(value: &CommandValue) -> Result<Vec<RelationValue>, String> {
    Ok(match value {
        CommandValue::Integer(value) => vec![RelationValue::Integer(*value)],
        CommandValue::Boolean(value) => vec![RelationValue::Atom(value.to_string())],
        CommandValue::String(value) => {
            let mut values = vec![RelationValue::String(value.clone())];
            // Boolean syntax has its own CommandValue variant. Do not let a
            // string that happens to spell a Prolog boolean erase that type
            // distinction while adapting enum-like strings.
            if relation_atom(value) && value != "true" && value != "false" {
                values.push(RelationValue::Atom(value.clone()));
            }
            values
        }
        CommandValue::Path(value) => vec![RelationValue::String(value.clone())],
        CommandValue::Base64(value) => vec![RelationValue::Compound(
            "base64".into(),
            vec![RelationValue::String(value.clone())],
        )],
        CommandValue::Spec(value) => {
            let mut values = vec![RelationValue::String(value.clone())];
            if let Some(filter) = filter_value(value)? {
                values.push(filter);
            }
            values
        }
        CommandValue::OciSpec {
            context_tar_gz,
            dockerfile,
            tag,
            net_mode,
            build_arguments,
        } => vec![RelationValue::Compound(
            "record".into(),
            vec![
                RelationValue::Compound(
                    "base64".into(),
                    vec![RelationValue::String(context_tar_gz.clone())],
                ),
                RelationValue::String(dockerfile.clone()),
                option_string(tag.as_deref()),
                RelationValue::Atom(net_mode.clone()),
                RelationValue::List(
                    build_arguments
                        .iter()
                        .map(|(key, value)| {
                            RelationValue::Compound(
                                "pair".into(),
                                vec![
                                    RelationValue::String(key.clone()),
                                    RelationValue::String(value.clone()),
                                ],
                            )
                        })
                        .collect(),
                ),
            ],
        )],
        CommandValue::ApiSpec {
            base_url,
            model,
            api_key,
        } => vec![RelationValue::Compound(
            "record".into(),
            vec![
                RelationValue::String(base_url.clone()),
                RelationValue::String(model.clone()),
                RelationValue::String(api_key.clone()),
            ],
        )],
        CommandValue::Array(values) => {
            let rows = cartesian_values(values)?;
            rows.into_iter().map(RelationValue::List).collect()
        }
        CommandValue::Hole { name, kind } => {
            return Err(format!(
                "cannot materialize unresolved {kind} argument {name}"
            ));
        }
    })
}

fn option_variants(
    values: &[RelationValue],
    index: usize,
    current: &mut Vec<RelationValue>,
    output: &mut Vec<Vec<RelationValue>>,
) -> Result<(), String> {
    if index == values.len() {
        return push_candidate(output, current.clone());
    }
    current.push(values[index].clone());
    option_variants(values, index + 1, current, output)?;
    current.pop();
    current.push(RelationValue::Compound(
        "some".into(),
        vec![values[index].clone()],
    ));
    option_variants(values, index + 1, current, output)?;
    current.pop();
    Ok(())
}

fn insert_default_variants(
    values: &[RelationValue],
    remaining: usize,
    output: &mut Vec<Vec<RelationValue>>,
) -> Result<(), String> {
    if remaining == 0 {
        return Ok(());
    }
    for at in 0..=values.len() {
        for default in [
            RelationValue::Atom("none".into()),
            RelationValue::Atom("true".into()),
            RelationValue::List(Vec::new()),
        ] {
            let mut inserted = values.to_vec();
            inserted.insert(at, default);
            push_candidate(output, inserted.clone())?;
            insert_default_variants(&inserted, remaining - 1, output)?;
        }
    }
    Ok(())
}

fn ro_attach_candidates(arguments: &[CommandValue]) -> Result<Vec<Vec<RelationValue>>, String> {
    let Some((box_source, attachments)) = arguments.split_first() else {
        return Ok(Vec::new());
    };
    let box_values = value_candidates(box_source)?;
    let attachment_rows = cartesian_values(attachments)?;
    let mut candidates = Vec::new();
    for box_value in box_values {
        for attachments in &attachment_rows {
            let attachments = attachments
                .iter()
                .cloned()
                .map(|value| RelationValue::Compound("box".into(), vec![value]))
                .collect();
            push_candidate(
                &mut candidates,
                vec![box_value.clone(), RelationValue::List(attachments)],
            )?;
        }
    }
    Ok(candidates)
}

fn view_open_candidates(arguments: &[CommandValue]) -> Result<Vec<Vec<RelationValue>>, String> {
    let [kind, box_source, rest @ ..] = arguments else {
        return Ok(Vec::new());
    };
    let CommandValue::String(kind) = kind else {
        return Ok(Vec::new());
    };
    let box_values = value_candidates(box_source)?;
    let forms = match rest {
        [] => vec![(RelationValue::Atom("none".into()), true)],
        [CommandValue::Boolean(running)] => {
            vec![(RelationValue::Atom("none".into()), *running)]
        }
        [filter] => filter_candidates(filter)?
            .into_iter()
            .map(|filter| (filter, true))
            .collect(),
        [filter, CommandValue::Boolean(running)] => filter_candidates(filter)?
            .into_iter()
            .map(|filter| (filter, *running))
            .collect(),
        _ => Vec::new(),
    };
    let mut candidates = Vec::new();
    for box_value in box_values {
        for (filter, running) in &forms {
            push_candidate(
                &mut candidates,
                vec![
                    RelationValue::Atom(kind.clone()),
                    box_value.clone(),
                    filter.clone(),
                    RelationValue::Atom(running.to_string()),
                ],
            )?;
        }
    }
    Ok(candidates)
}

fn view_filter_candidates(arguments: &[CommandValue]) -> Result<Vec<Vec<RelationValue>>, String> {
    let [view, filter] = arguments else {
        return Ok(Vec::new());
    };
    let views = value_candidates(view)?;
    let filters = filter_candidates(filter)?;
    let mut candidates = Vec::new();
    for view in views {
        for filter in &filters {
            push_candidate(&mut candidates, vec![view.clone(), filter.clone()])?;
        }
    }
    Ok(candidates)
}

fn filter_candidates(value: &CommandValue) -> Result<Vec<RelationValue>, String> {
    let text = match value {
        CommandValue::String(text) | CommandValue::Spec(text) => text,
        _ => return Ok(Vec::new()),
    };
    Ok(filter_value(text)?.into_iter().collect())
}

fn filter_value(text: &str) -> Result<Option<RelationValue>, String> {
    if text.is_empty() || text == "none" {
        return Ok(Some(RelationValue::Atom("none".into())));
    }
    let Some((kind, pattern)) = text.split_once(':') else {
        return Ok(None);
    };
    if !relation_atom(kind) {
        return Ok(None);
    }
    Ok(Some(RelationValue::Compound(
        "some".into(),
        vec![RelationValue::List(vec![RelationValue::Compound(
            "record".into(),
            vec![
                RelationValue::Atom(kind.into()),
                RelationValue::String(pattern.into()),
                RelationValue::Atom("and".into()),
                RelationValue::Atom("false".into()),
                RelationValue::Atom("true".into()),
            ],
        )])],
    )))
}

fn option_string(value: Option<&str>) -> RelationValue {
    value.map_or_else(
        || RelationValue::Atom("none".into()),
        |value| RelationValue::Compound("some".into(), vec![RelationValue::String(value.into())]),
    )
}

fn relation_atom(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && value.as_bytes()[0].is_ascii_lowercase()
}

fn push_candidate(
    candidates: &mut Vec<Vec<RelationValue>>,
    candidate: Vec<RelationValue>,
) -> Result<(), String> {
    if candidates.len() >= MAX_CANDIDATES {
        return Err(format!(
            "command AST adaptation exceeds {MAX_CANDIDATES} candidates"
        ));
    }
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(handler: &str, args: Vec<CommandValue>) -> CommandAst {
        CommandAst {
            action: handler.into(),
            handler: handler.into(),
            target: "ui".into(),
            args,
        }
    }

    #[test]
    fn structural_bridge_adapts_options_and_repeated_fields() {
        assert!(matches!(
            materialize(&command("flows.detail", vec![CommandValue::Integer(9)])),
            Ok(ActionRequest::FlowsDetail {
                sid: None,
                frame: 9
            })
        ));
        assert!(matches!(
            materialize(&command(
                "flows.detail",
                vec![CommandValue::Integer(4), CommandValue::Integer(9)]
            )),
            Ok(ActionRequest::FlowsDetail {
                sid: Some(4),
                frame: 9
            })
        ));
        assert!(matches!(
            materialize(&command(
                "review.apply",
                vec![
                    CommandValue::Integer(7),
                    CommandValue::Array(vec![
                        CommandValue::Path("one".into()),
                        CommandValue::Path("two".into()),
                    ]),
                ]
            )),
            Ok(ActionRequest::ReviewApply { sid: 7, paths }) if paths.as_slice().len() == 2
        ));
    }

    #[test]
    fn semantic_bridges_are_sarun_glue_not_grammar_cases() {
        assert!(matches!(
            materialize(&command(
                "ro_attach",
                vec![
                    CommandValue::Integer(7),
                    CommandValue::Integer(2),
                    CommandValue::Integer(3),
                ]
            )),
            Ok(ActionRequest::RoAttach { r#box: 7, attachments })
                if attachments.as_slice().len() == 2
        ));
        assert!(matches!(
            materialize(&command(
                "view.open",
                vec![
                    CommandValue::String("changes".into()),
                    CommandValue::Integer(7),
                    CommandValue::String("path:src/main.rs".into()),
                    CommandValue::Boolean(false),
                ]
            )),
            Ok(ActionRequest::ViewOpen {
                r#box: 7,
                filter: Some(_),
                running_only: false,
                ..
            })
        ));
        let oci = materialize(&command(
            "oci.build",
            vec![CommandValue::OciSpec {
                context_tar_gz: "eA==".into(),
                dockerfile: "FROM scratch\n".into(),
                tag: Some("example:test".into()),
                net_mode: "tap".into(),
                build_arguments: vec![("A".into(), "one".into())],
            }],
        ))
        .unwrap();
        let ActionRequest::OciBuild { spec } = oci else {
            panic!("bridge produced the wrong structured request")
        };
        assert_eq!(spec.context_tar_gz.as_slice(), b"x");
        assert_eq!(spec.dockerfile.as_slice(), b"FROM scratch\n");
        assert_eq!(spec.tag.as_ref().unwrap().as_str(), "example:test");
        assert_eq!(spec.net_mode, crate::generated_wire::NetMode::Tap);
        assert_eq!(spec.build_arguments.as_map().len(), 1);
    }

    #[test]
    fn generated_wire_ast_rejects_bad_adaptations() {
        let error = materialize(&command(
            "mirror_pause",
            vec![
                CommandValue::Integer(1),
                CommandValue::String("false".into()),
            ],
        ))
        .unwrap_err();
        assert!(error.contains("no binary shape"), "{error}");
    }
}
