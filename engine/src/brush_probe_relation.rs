//! Brush's typed builtin parsers as one opaque host relation client.
//!
//! This adapter consumes grammar-owned, spanned symbolic argv. It cooks only
//! explicitly resolved fragments and rejects opaque or unresolved input, so a
//! parser observation cannot silently discard shell provenance.

use brush_core::builtins::{
    BuiltinParseStatus, BuiltinParserInput, BuiltinParserObservation, ParserArgumentEvidence,
};

use crate::prolog::{RelationBinding, RelationReply, RelationSolution, RelationValue};
use crate::relation_adapter::{Adapter, Request};

pub(crate) const HANDLE: &str = "brush_typed_builtins";
const PROVIDER: &str = "builtin_parser";
const PREFERENCE: i64 = 40;

pub(crate) struct BuiltinProbeAdapter;

pub(crate) fn register() -> Result<(), String> {
    static REGISTERED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            crate::relation_adapter::register(HANDLE, std::sync::Arc::new(BuiltinProbeAdapter))
        })
        .clone()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteSpan {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CookedWord {
    text: String,
    tears: Vec<CookedTear>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CookedTear {
    edit_id: String,
    replace: ByteSpan,
    offset: usize,
    surface_len: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProbeAtTear {
    input: BuiltinParserInput,
    replace: ByteSpan,
    edit_id: String,
    tear_surface_len: usize,
    command_gap: bool,
}

#[derive(Debug)]
enum ArgvDecodeError {
    Unsupported,
    Invalid(String),
}

impl From<String> for ArgvDecodeError {
    fn from(error: String) -> Self {
        Self::Invalid(error)
    }
}

impl Adapter for BuiltinProbeAdapter {
    fn revision(&self) -> RelationValue {
        RelationValue::Compound(
            "builtin_parser_revision".into(),
            vec![RelationValue::Integer(3)],
        )
    }

    fn transform(&self, request: &Request) -> Result<RelationReply, String> {
        let words = match request_argv(request) {
            Ok(words) => words,
            Err(ArgvDecodeError::Unsupported) => return Ok(no_solution()),
            Err(ArgvDecodeError::Invalid(error)) => return Err(error),
        };
        let Some(command_name) = words.first().map(|word| word.text.clone()) else {
            return Ok(no_solution());
        };
        let Some(probe) = super::builtin_parser(&command_name) else {
            return Ok(no_solution());
        };

        let tears = words
            .iter()
            .enumerate()
            .flat_map(|(word_index, word)| {
                word.tears
                    .iter()
                    .map(move |tear| (word_index, tear.clone()))
            })
            .collect::<Vec<_>>();
        match tears.as_slice() {
            [] => {
                let observation = probe(BuiltinParserInput {
                    tear: false,
                    before: words.into_iter().map(|word| word.text).collect(),
                    prefix: String::new(),
                    suffix: String::new(),
                    after: Vec::new(),
                });
                exact_reply(request, observation)
            }
            [(word_index, tear)] => {
                let at_tear = probe_from_symbolic_tear(&words, *word_index, tear)?;
                let observation = probe(at_tear.input.clone());
                assist_reply(request, &command_name, at_tear, observation)
            }
            _ => Ok(no_solution()),
        }
    }
}

fn request_argv(request: &Request) -> Result<Vec<CookedWord>, ArgvDecodeError> {
    let mut bindings = request
        .given
        .iter()
        .filter(|binding| binding.name == "argv");
    let value = &bindings
        .next()
        .ok_or_else(|| ArgvDecodeError::Invalid("typed builtin relation omitted argv".into()))?
        .value;
    if bindings.next().is_some() {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin relation supplied argv more than once".into(),
        ));
    }
    decode_symbolic_argv(value)
}

fn decode_symbolic_argv(value: &RelationValue) -> Result<Vec<CookedWord>, ArgvDecodeError> {
    let RelationValue::Compound(name, fields) = value else {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin argv is not a compound".into(),
        ));
    };
    if name != "symbolic_argv" || fields.len() != 1 {
        return Err(ArgvDecodeError::Invalid(format!(
            "typed builtin argv has an invalid shape: {name}/{}",
            fields.len()
        )));
    }
    let RelationValue::List(words) = &fields[0] else {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin argv words are not a list".into(),
        ));
    };
    words
        .iter()
        .map(decode_symbolic_word)
        .collect::<Result<Vec<_>, _>>()
        .map(|words| words.into_iter().flatten().collect())
}

fn decode_symbolic_word(value: &RelationValue) -> Result<Vec<CookedWord>, ArgvDecodeError> {
    let RelationValue::Compound(name, fields) = value else {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin symbolic word is not a compound".into(),
        ));
    };
    if name != "symbolic_word" || fields.len() != 2 {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin symbolic word has an invalid shape".into(),
        ));
    }
    decode_span(&fields[0], "span")?;
    let RelationValue::List(fragments) = &fields[1] else {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin symbolic word fragments are not a list".into(),
        ));
    };
    let mut words = vec![empty_cooked_word()];
    for fragment in fragments {
        append_expansion(&mut words, decode_fragment(fragment)?);
    }
    Ok(words)
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

fn decode_fragment(value: &RelationValue) -> Result<Vec<CookedWord>, ArgvDecodeError> {
    let RelationValue::Compound(name, fields) = value else {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin fragment is not a compound".into(),
        ));
    };
    if name != "fragment" || fields.len() != 3 {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin fragment has an invalid shape".into(),
        ));
    }
    let RelationValue::Atom(kind) = &fields[0] else {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin fragment kind is not an atom".into(),
        ));
    };
    let physical_span = decode_origin_span(&fields[1])?;
    match kind.as_str() {
        "literal" => decode_utf8(&fields[2])
            .map(cooked_text)
            .map_err(ArgvDecodeError::Invalid),
        "tear" => decode_edit_tear(&fields[2], physical_span)
            .map(cooked_tear)
            .map_err(ArgvDecodeError::Invalid),
        "reference" => decode_resolved_reference(&fields[2]),
        "opaque" => Err(ArgvDecodeError::Unsupported),
        _ => Err(ArgvDecodeError::Unsupported),
    }
}

fn decode_origin_span(value: &RelationValue) -> Result<ByteSpan, String> {
    let RelationValue::Compound(name, fields) = value else {
        return Err("typed builtin fragment origin is not a compound".into());
    };
    if name != "origin" || fields.len() != 3 {
        return Err("typed builtin fragment origin has an invalid shape".into());
    }
    decode_span(&fields[1], "span")
}

fn decode_utf8(value: &RelationValue) -> Result<String, String> {
    let RelationValue::Compound(name, fields) = value else {
        return Err("typed builtin literal is not a compound".into());
    };
    if name != "utf8" || fields.len() != 1 {
        return Err("typed builtin literal has an invalid shape".into());
    }
    let RelationValue::String(text) = &fields[0] else {
        return Err("typed builtin literal UTF-8 value is not text".into());
    };
    Ok(text.clone())
}

fn decode_edit_tear(
    value: &RelationValue,
    replace: ByteSpan,
) -> Result<(String, CookedTear), String> {
    let RelationValue::Compound(name, fields) = value else {
        return Err("typed builtin edit tear is not a compound".into());
    };
    if name != "edit_tear" || fields.len() != 3 {
        return Err("typed builtin edit tear has an invalid shape".into());
    }
    let RelationValue::Atom(edit_id) = &fields[0] else {
        return Err("typed builtin edit identity is not an atom".into());
    };
    let RelationValue::String(surface) = &fields[1] else {
        return Err("typed builtin edit surface is not text".into());
    };
    Ok((
        surface.clone(),
        CookedTear {
            edit_id: edit_id.clone(),
            replace,
            offset: surface.len(),
            surface_len: surface.len(),
        },
    ))
}

fn decode_resolved_reference(value: &RelationValue) -> Result<Vec<CookedWord>, ArgvDecodeError> {
    let RelationValue::Compound(name, fields) = value else {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin reference is not a compound".into(),
        ));
    };
    if name == "state_ref" {
        return Err(ArgvDecodeError::Unsupported);
    }
    if name != "resolved_reference" || fields.len() != 3 {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin reference has an invalid shape".into(),
        ));
    }
    decode_resolved_value(&fields[2])
}

fn decode_resolved_value(value: &RelationValue) -> Result<Vec<CookedWord>, ArgvDecodeError> {
    let RelationValue::Compound(name, fields) = value else {
        return Err(ArgvDecodeError::Invalid(
            "typed builtin resolved value is not a compound".into(),
        ));
    };
    match (name.as_str(), fields.as_slice()) {
        ("symbolic_word", _) => decode_symbolic_word(value),
        ("symbolic_argv", _) => decode_symbolic_argv(value),
        ("shell_text", [RelationValue::String(text)]) => Ok(cooked_text(text.clone())),
        ("text", [RelationValue::List(segments)]) => {
            let mut words = vec![empty_cooked_word()];
            for segment in segments {
                let decoded = match segment {
                    RelationValue::String(text) => cooked_text(text.clone()),
                    RelationValue::Compound(name, fields)
                        if name == "hole" && fields.len() == 4 =>
                    {
                        let edit_id = match &fields[0] {
                            RelationValue::Atom(edit_id) => edit_id.clone(),
                            _ => {
                                return Err(ArgvDecodeError::Invalid(
                                    "typed builtin hole identity is not an atom".into(),
                                ));
                            }
                        };
                        let replace = decode_span(&fields[1], "span")?;
                        let surface = match &fields[2] {
                            RelationValue::String(surface) => surface.clone(),
                            _ => {
                                return Err(ArgvDecodeError::Invalid(
                                    "typed builtin hole surface is not text".into(),
                                ));
                            }
                        };
                        cooked_tear((
                            surface.clone(),
                            CookedTear {
                                edit_id,
                                replace,
                                offset: surface.len(),
                                surface_len: surface.len(),
                            },
                        ))
                    }
                    _ => return Err(ArgvDecodeError::Unsupported),
                };
                append_expansion(&mut words, decoded);
            }
            Ok(words)
        }
        ("state_ref", _) => Err(ArgvDecodeError::Unsupported),
        _ => Err(ArgvDecodeError::Unsupported),
    }
}

fn empty_cooked_word() -> CookedWord {
    CookedWord {
        text: String::new(),
        tears: Vec::new(),
    }
}

fn cooked_text(text: String) -> Vec<CookedWord> {
    vec![CookedWord {
        text,
        tears: Vec::new(),
    }]
}

fn cooked_tear((text, tear): (String, CookedTear)) -> Vec<CookedWord> {
    vec![CookedWord {
        text,
        tears: vec![tear],
    }]
}

fn append_expansion(words: &mut Vec<CookedWord>, mut expansion: Vec<CookedWord>) {
    if expansion.is_empty() {
        return;
    }
    if words.is_empty() {
        *words = expansion;
        return;
    }
    let first = expansion.remove(0);
    append_word(words.last_mut().expect("nonempty words"), first);
    words.extend(expansion);
}

fn append_word(target: &mut CookedWord, mut suffix: CookedWord) {
    let offset = target.text.len();
    for tear in &mut suffix.tears {
        tear.offset += offset;
    }
    target.text.push_str(&suffix.text);
    target.tears.extend(suffix.tears);
}

fn probe_from_symbolic_tear(
    words: &[CookedWord],
    word_index: usize,
    tear: &CookedTear,
) -> Result<ProbeAtTear, String> {
    let word = words
        .get(word_index)
        .ok_or_else(|| "typed builtin tear word is missing".to_string())?;
    if tear.offset > word.text.len() || !word.text.is_char_boundary(tear.offset) {
        return Err("typed builtin tear has an invalid cooked offset".into());
    }
    Ok(ProbeAtTear {
        input: if word_index == 0
            && tear.offset == word.text.len()
            && tear.surface_len == 0
        {
            BuiltinParserInput {
                tear: true,
                before: vec![word.text.clone()],
                prefix: String::new(),
                suffix: String::new(),
                after: words[1..].iter().map(|word| word.text.clone()).collect(),
            }
        } else {
            BuiltinParserInput {
                tear: true,
                before: words[..word_index]
                    .iter()
                    .map(|word| word.text.clone())
                    .collect(),
                prefix: word.text[..tear.offset].into(),
                suffix: word.text[tear.offset..].into(),
                after: words[word_index + 1..]
                    .iter()
                    .map(|word| word.text.clone())
                    .collect(),
            }
        },
        replace: tear.replace,
        edit_id: tear.edit_id.clone(),
        tear_surface_len: tear.surface_len,
        command_gap: word_index == 0
            && tear.offset == word.text.len()
            && tear.surface_len == 0,
    })
}

fn exact_reply(
    request: &Request,
    observation: BuiltinParserObservation,
) -> Result<RelationReply, String> {
    match observation.status {
        BuiltinParseStatus::Complete => {
            solution_reply(request, RelationValue::Atom("complete".into()), Vec::new())
        }
        BuiltinParseStatus::Incomplete | BuiltinParseStatus::Unsupported => Ok(no_solution()),
        BuiltinParseStatus::Rejected(_) => Ok(no_solution()),
    }
}

fn assist_reply(
    request: &Request,
    command_name: &str,
    at_tear: ProbeAtTear,
    observation: BuiltinParserObservation,
) -> Result<RelationReply, String> {
    match observation.status {
        BuiltinParseStatus::Rejected(_) => return Ok(no_solution()),
        BuiltinParseStatus::Unsupported => return Ok(no_solution()),
        BuiltinParseStatus::Complete | BuiltinParseStatus::Incomplete => {}
    }
    let mut candidates = Vec::<(String, Vec<RelationValue>)>::new();
    let mut context_queries = Vec::new();
    for continuation in observation.literal_continuations {
        let parser_literal = continuation.literal;
        let mut literal = parser_literal.clone();
        if at_tear.command_gap {
            literal.insert(0, ' ');
        }
        candidates.push((
            if !continuation.expected.is_empty() && at_tear.input.suffix.is_empty() {
                format!("{literal} ")
            } else {
                literal.clone()
            },
            vec![parser_alternative(
                command_name,
                &continuation.argument,
                &parser_literal,
            )],
        ));
        if at_tear.input.suffix.is_empty() {
            for expected in &continuation.expected {
                for value in &expected.finite_values {
                    candidates.push((
                        format!("{literal} {value}"),
                        vec![
                            parser_alternative(
                                command_name,
                                &continuation.argument,
                                &parser_literal,
                            ),
                            parser_alternative(command_name, expected, value),
                        ],
                    ));
                }
            }
        }
    }
    let mut arguments = observation.expected;
    arguments.extend(observation.tear_arguments);
    for argument in arguments {
        for value in &argument.finite_values {
            if value.starts_with(&at_tear.input.prefix) && value != &at_tear.input.prefix {
                candidates.push((
                    value.clone(),
                    vec![parser_alternative(command_name, &argument, value)],
                ));
            }
        }
        let Some(domain) = argument.value_domain.as_deref() else {
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
        .filter_map(|(rank, (candidate, alternatives))| {
            completion_insertion(&candidate, &at_tear)
                .filter(|insert| !insert.is_empty())
                .map(|insert| completion_value(at_tear.replace, insert, alternatives, rank + 1))
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

fn completion_insertion(candidate: &str, at_tear: &ProbeAtTear) -> Option<String> {
    let prefix_len = at_tear.input.prefix.len();
    let surface_len = at_tear.tear_surface_len;
    let outside_prefix_len = prefix_len.checked_sub(surface_len)?;
    if outside_prefix_len > candidate.len()
        || !candidate.is_char_boundary(outside_prefix_len)
        || !candidate.starts_with(&at_tear.input.prefix)
    {
        return None;
    }
    Some(candidate[outside_prefix_len..].into())
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

fn parser_alternative(
    command_name: &str,
    argument: &ParserArgumentEvidence,
    value: &str,
) -> RelationValue {
    RelationValue::Compound(
        "alternative".into(),
        vec![
            RelationValue::Compound(
                "parser_argument".into(),
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

fn argument_context_query(
    command_name: &str,
    argument: &ParserArgumentEvidence,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prolog::RelationLimits;

    fn span(start: usize, end: usize) -> RelationValue {
        span_value(ByteSpan { start, end })
    }

    fn origin(start: usize, end: usize) -> RelationValue {
        RelationValue::Compound(
            "origin".into(),
            vec![
                RelationValue::Atom("test".into()),
                span(start, end),
                RelationValue::List(vec![]),
            ],
        )
    }

    fn fragment(kind: &str, start: usize, end: usize, payload: RelationValue) -> RelationValue {
        RelationValue::Compound(
            "fragment".into(),
            vec![
                RelationValue::Atom(kind.into()),
                origin(start, end),
                payload,
            ],
        )
    }

    fn literal(start: usize, end: usize, text: &str) -> RelationValue {
        fragment(
            "literal",
            start,
            end,
            RelationValue::Compound("utf8".into(), vec![RelationValue::String(text.into())]),
        )
    }

    fn tear(start: usize, end: usize, surface: &str) -> RelationValue {
        fragment(
            "tear",
            start,
            end,
            RelationValue::Compound(
                "edit_tear".into(),
                vec![
                    RelationValue::Atom("edit".into()),
                    RelationValue::String(surface.into()),
                    RelationValue::Atom("any".into()),
                ],
            ),
        )
    }

    fn reference(start: usize, end: usize, id: &str, resolved: RelationValue) -> RelationValue {
        fragment(
            "reference",
            start,
            end,
            RelationValue::Compound(
                "resolved_reference".into(),
                vec![
                    RelationValue::Atom(id.into()),
                    RelationValue::Atom("test_resolution".into()),
                    resolved,
                ],
            ),
        )
    }

    fn word(start: usize, end: usize, fragments: Vec<RelationValue>) -> RelationValue {
        RelationValue::Compound(
            "symbolic_word".into(),
            vec![span(start, end), RelationValue::List(fragments)],
        )
    }

    fn argv(words: Vec<RelationValue>) -> RelationValue {
        RelationValue::Compound("symbolic_argv".into(), vec![RelationValue::List(words)])
    }

    fn literal_word(start: usize, text: &str) -> RelationValue {
        word(
            start,
            start + text.len(),
            vec![literal(start, start + text.len(), text)],
        )
    }

    fn request(argv: RelationValue, wanted: &[&str]) -> Request {
        Request {
            given: vec![RelationBinding {
                name: "argv".into(),
                value: argv,
            }],
            wanted: wanted.iter().map(|value| (*value).into()).collect(),
            observations: vec![],
            limits: RelationLimits::default(),
        }
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

    fn first_completion_alternative(reply: &RelationReply) -> &RelationValue {
        let RelationValue::List(completions) = &reply.solutions[0]
            .bindings
            .iter()
            .find(|binding| binding.name == "completions")
            .unwrap()
            .value
        else {
            panic!("completion binding must be a list")
        };
        let RelationValue::Compound(name, fields) = &completions[0] else {
            panic!("completion must have the expected shape")
        };
        assert_eq!(name, "completion");
        let RelationValue::List(alternatives) = &fields[2] else {
            panic!("completion alternatives must be a list")
        };
        &alternatives[0]
    }

    #[test]
    fn bind_finite_values_come_from_runtime_parser_probe() {
        let argv_input = argv(vec![
            literal_word(0, "bind"),
            literal_word(5, "-m"),
            word(8, 8, vec![tear(8, 8, "")]),
        ]);
        let reply = BuiltinProbeAdapter
            .transform(&request(argv_input, &["status", "completions"]))
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
        assert_eq!(
            first_completion_alternative(&reply),
            &RelationValue::Compound(
                "alternative".into(),
                vec![
                    RelationValue::Compound(
                        "parser_argument".into(),
                        vec![
                            RelationValue::String("bind".into()),
                            RelationValue::String("keymap".into()),
                            RelationValue::String("emacs-ctlx".into()),
                        ],
                    ),
                    RelationValue::Atom("builtin_argument".into()),
                    RelationValue::Atom("builtin_parser".into()),
                    RelationValue::Integer(PREFERENCE),
                ],
            )
        );
    }

    #[test]
    fn unsupported_parser_projection_is_relation_no_solution() {
        let observation = BuiltinParserObservation {
            status: BuiltinParseStatus::Unsupported,
            tear_arguments: Vec::new(),
            expected: Vec::new(),
            literal_continuations: Vec::new(),
        };
        let request = request(argv(vec![literal_word(0, "test")]), &["status"]);
        let reply = exact_reply(&request, observation).unwrap();
        assert!(reply.solutions.is_empty());
        assert!(reply.diagnostics.is_empty());
    }

    #[test]
    fn bind_finite_value_completion_preserves_and_proves_same_word_suffix() {
        let argv_input = argv(vec![
            literal_word(0, "bind"),
            literal_word(5, "-m"),
            word(8, 19, vec![tear(8, 10, "em"), literal(10, 19, "-standard")]),
        ]);
        let reply = BuiltinProbeAdapter
            .transform(&request(argv_input, &["status", "completions"]))
            .unwrap();
        assert_eq!(completion_texts(&reply), ["emacs"]);

        let incompatible = argv(vec![
            literal_word(0, "bind"),
            literal_word(5, "-m"),
            word(
                8,
                23,
                vec![tear(8, 10, "em"), literal(10, 23, "-not-a-keymap")],
            ),
        ]);
        let reply = BuiltinProbeAdapter
            .transform(&request(incompatible, &["status", "completions"]))
            .unwrap();
        assert!(reply.solutions.is_empty());
        assert!(reply.diagnostics.is_empty());
    }

    #[test]
    fn source_cursor_tear_inserts_only_the_missing_literal_suffix() {
        let argv_input = argv(vec![
            literal_word(0, "bind"),
            literal_word(5, "-m"),
            word(8, 10, vec![literal(8, 10, "em"), tear(10, 10, "")]),
        ]);
        let reply = BuiltinProbeAdapter
            .transform(&request(argv_input, &["status", "completions"]))
            .unwrap();
        assert_eq!(
            completion_texts(&reply),
            ["acs-ctlx", "acs-meta", "acs-standard"]
        );
    }

    #[test]
    fn command_end_tear_composes_following_option_and_value_evidence() {
        let argv_input = argv(vec![word(
            0,
            4,
            vec![literal(0, 4, "bind"), tear(4, 4, "")],
        )]);
        let reply = BuiltinProbeAdapter
            .transform(&request(argv_input, &["status", "completions"]))
            .unwrap();
        let completions = completion_texts(&reply);
        for expected in [" -m ", " -m emacs-standard", " -m vi-insert"] {
            assert!(completions.iter().any(|completion| completion == expected));
        }
    }

    #[test]
    fn exact_bind_uses_the_same_typed_parser() {
        let argv = argv(vec![
            literal_word(0, "bind"),
            literal_word(5, "-m"),
            literal_word(8, "emacs-standard"),
        ]);
        let reply = BuiltinProbeAdapter
            .transform(&request(argv, &["status"]))
            .unwrap();
        assert_eq!(
            reply.solutions[0].bindings[0].value,
            RelationValue::Atom("complete".into())
        );
    }

    #[test]
    fn edit_path_replays_from_explicit_context_observation() {
        let argv = argv(vec![
            literal_word(0, "edit"),
            word(5, 5, vec![tear(5, 5, "")]),
        ]);
        let mut request = request(argv, &["status", "completions"]);
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
        let argv = argv(vec![
            literal_word(0, "edit"),
            word(5, 8, vec![tear(5, 8, "./t")]),
        ]);
        let mut request = crate::prolog::RelationRequest {
            grammar: RelationValue::Compound(
                "registered_relation".into(),
                vec![RelationValue::Atom(HANDLE.into())],
            ),
            given: vec![RelationBinding {
                name: "argv".into(),
                value: argv,
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
    fn resolved_symbolic_argv_and_shell_text_are_cooked_recursively() {
        let expanded = argv(vec![
            literal_word(5, "-m"),
            word(
                8,
                23,
                vec![reference(
                    8,
                    23,
                    "value",
                    RelationValue::Compound(
                        "shell_text".into(),
                        vec![RelationValue::String("emacs-standard".into())],
                    ),
                )],
            ),
        ]);
        let argv = argv(vec![
            literal_word(0, "bind"),
            word(5, 23, vec![reference(5, 23, "args", expanded)]),
        ]);
        let reply = BuiltinProbeAdapter
            .transform(&request(argv, &["status"]))
            .unwrap();
        assert_eq!(
            reply.solutions[0].bindings[0].value,
            RelationValue::Atom("complete".into())
        );
    }

    #[test]
    fn resolved_text_hole_preserves_tear_provenance() {
        let resolved = RelationValue::Compound(
            "text".into(),
            vec![RelationValue::List(vec![
                RelationValue::Compound(
                    "hole".into(),
                    vec![
                        RelationValue::Atom("edit".into()),
                        span(8, 10),
                        RelationValue::String("em".into()),
                        RelationValue::Atom("any".into()),
                    ],
                ),
                RelationValue::String("-standard".into()),
            ])],
        );
        let argv = argv(vec![
            literal_word(0, "bind"),
            literal_word(5, "-m"),
            word(8, 19, vec![reference(8, 19, "variable_a", resolved)]),
        ]);
        let reply = BuiltinProbeAdapter
            .transform(&request(argv, &["status", "completions"]))
            .unwrap();
        assert_eq!(completion_texts(&reply), ["emacs"]);
        let RelationValue::List(completions) = &reply.solutions[0]
            .bindings
            .iter()
            .find(|binding| binding.name == "completions")
            .unwrap()
            .value
        else {
            panic!("completion binding must be a list")
        };
        let RelationValue::Compound(_, fields) = &completions[0] else {
            panic!("completion must be a compound")
        };
        assert_eq!(fields[0], span(8, 10));
    }

    #[test]
    fn opaque_and_unresolved_fragments_are_failed_relation_branches() {
        let opaque = fragment(
            "opaque",
            0,
            4,
            RelationValue::Compound(
                "opaque".into(),
                vec![
                    RelationValue::Atom("shell_word".into()),
                    RelationValue::String("bind".into()),
                ],
            ),
        );
        let reply = BuiltinProbeAdapter
            .transform(&request(argv(vec![word(0, 4, vec![opaque])]), &["status"]));
        assert!(reply.unwrap().solutions.is_empty());

        let unresolved = fragment(
            "reference",
            5,
            7,
            RelationValue::Compound(
                "state_ref".into(),
                vec![
                    RelationValue::Atom("use_a".into()),
                    RelationValue::Atom("shell_variable".into()),
                    RelationValue::String("A".into()),
                ],
            ),
        );
        let reply = BuiltinProbeAdapter.transform(&request(
            argv(vec![literal_word(0, "bind"), word(5, 7, vec![unresolved])]),
            &["status"],
        ));
        assert!(reply.unwrap().solutions.is_empty());
    }
}
