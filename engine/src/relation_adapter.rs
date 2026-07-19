//! Host implementations of opaque relations.
//!
//! A registered adapter lets the generic relation engine compose an existing
//! Rust parser or codec without adding a command/language case to Prolog.  Its
//! invocation and result cross the same explicit, revisioned context protocol
//! as every other external dependency.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use crate::prolog::{
    ContextEntry, ContextQueryNode, ContextSnapshot, RelationBinding, RelationLimits,
    RelationReply, RelationValue,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub given: Vec<RelationBinding>,
    pub wanted: Vec<String>,
    pub limits: RelationLimits,
}

/// Pure host implementation of one opaque relation handle.
///
/// The revision identifies the implementation/schema version.  `transform`
/// receives only the ordinary relation envelope; semantic context must be
/// requested explicitly rather than read through hidden process state.
pub trait Adapter: Send + Sync + 'static {
    fn revision(&self) -> RelationValue;
    fn transform(&self, request: &Request) -> Result<RelationReply, String>;
}

fn registry() -> &'static RwLock<BTreeMap<String, Arc<dyn Adapter>>> {
    static REGISTRY: OnceLock<RwLock<BTreeMap<String, Arc<dyn Adapter>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Install one immutable adapter identity. Replacing an implementation under
/// an existing handle would invalidate dependency traces without changing the
/// query, so duplicate registration is rejected even when revisions match.
pub fn register(handle: impl Into<String>, adapter: Arc<dyn Adapter>) -> Result<(), String> {
    let handle = handle.into();
    if !valid_atom(&handle) {
        return Err("registered relation handle must be a lowercase Prolog atom".into());
    }
    let mut adapters = registry()
        .write()
        .map_err(|_| "registered relation registry is poisoned".to_string())?;
    if adapters.contains_key(&handle) {
        return Err(format!("registered relation {handle} already exists"));
    }
    adapters.insert(handle, adapter);
    Ok(())
}

/// Resolve a `registered_relation(Handle)` query into a query-scoped snapshot.
/// Unknown domains return `None` so the ordinary semantic provider can handle
/// them; an unknown registered handle is a visible provider error.
pub(crate) fn snapshot(node: &ContextQueryNode) -> Result<Option<ContextSnapshot>, String> {
    let RelationValue::Compound(domain_name, domain_args) = &node.query.domain else {
        return Ok(None);
    };
    if domain_name != "registered_relation" || domain_args.len() != 1 {
        return Ok(None);
    }
    let RelationValue::Atom(handle) = &domain_args[0] else {
        return Err("registered relation domain has a non-atom handle".into());
    };
    let request_value = match &node.query.selector {
        RelationValue::Compound(name, values) if name == "where" && values.len() == 1 => {
            values[0].clone()
        }
        _ => return Err("registered relation query has an invalid selector".into()),
    };
    let request = decode_request(&request_value)?;
    let adapter = registry()
        .read()
        .map_err(|_| "registered relation registry is poisoned".to_string())?
        .get(handle)
        .cloned()
        .ok_or_else(|| format!("no registered relation adapter for {handle}"))?;
    let revision = adapter.revision();
    let reply = adapter.transform(&request)?;
    if !reply.context_queries.is_empty() || !reply.dependency_keys.is_empty() {
        return Err(format!(
            "registered relation {handle} suspended on context before continuation support"
        ));
    }
    let reply = crate::prolog::relation_reply_value(&reply)?;
    let domain = node.query.domain.clone();
    Ok(Some(ContextSnapshot {
        provider: RelationValue::Compound(
            "registered_relation".into(),
            vec![RelationValue::Atom(handle.clone())],
        ),
        revision: revision.clone(),
        entries: vec![ContextEntry {
            domain,
            identity: RelationValue::Compound(
                "registered_relation_result".into(),
                vec![RelationValue::Atom(handle.clone()), revision],
            ),
            names: vec!["result".into()],
            value: RelationValue::Compound("result".into(), vec![reply]),
            attributes: vec![request_value],
        }],
    }))
}

fn decode_request(value: &RelationValue) -> Result<Request, String> {
    let RelationValue::Compound(name, fields) = value else {
        return Err("registered relation request is not a compound".into());
    };
    if name != "relation_request" || fields.len() != 3 {
        return Err("registered relation request has an invalid shape".into());
    }
    let RelationValue::List(given) = &fields[0] else {
        return Err("registered relation given value is not a list".into());
    };
    let given = given
        .iter()
        .map(decode_binding)
        .collect::<Result<Vec<_>, _>>()?;
    let RelationValue::List(wanted) = &fields[1] else {
        return Err("registered relation wanted value is not a list".into());
    };
    let wanted = wanted
        .iter()
        .map(|value| match value {
            RelationValue::Atom(name) if valid_atom(name) => Ok(name.clone()),
            _ => Err("registered relation wanted value is not an atom".to_string()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let RelationValue::Compound(limits_name, limits) = &fields[2] else {
        return Err("registered relation limits value is not a compound".into());
    };
    if limits_name != "limits" || limits.len() != 3 {
        return Err("registered relation limits have an invalid shape".into());
    }
    let limits = RelationLimits {
        max_solutions: positive_usize(&limits[0], "solution")?,
        max_evidence: nonnegative_usize(&limits[1], "evidence")?,
        max_output_bytes: positive_usize(&limits[2], "output")?,
    };
    Ok(Request {
        given,
        wanted,
        limits,
    })
}

fn decode_binding(value: &RelationValue) -> Result<RelationBinding, String> {
    let RelationValue::Compound(name, fields) = value else {
        return Err("registered relation binding is not a compound".into());
    };
    if name != "binding" || fields.len() != 2 {
        return Err("registered relation binding has an invalid shape".into());
    }
    let RelationValue::Atom(name) = &fields[0] else {
        return Err("registered relation binding name is not an atom".into());
    };
    if !valid_atom(name) {
        return Err("registered relation binding name is invalid".into());
    }
    Ok(RelationBinding {
        name: name.clone(),
        value: fields[1].clone(),
    })
}

fn positive_usize(value: &RelationValue, field: &str) -> Result<usize, String> {
    let parsed = nonnegative_usize(value, field)?;
    (parsed > 0)
        .then_some(parsed)
        .ok_or_else(|| format!("registered relation {field} limit must be positive"))
}

fn nonnegative_usize(value: &RelationValue, field: &str) -> Result<usize, String> {
    match value {
        RelationValue::Integer(value) => usize::try_from(*value)
            .map_err(|_| format!("registered relation {field} limit is invalid")),
        _ => Err(format!(
            "registered relation {field} limit is not an integer"
        )),
    }
}

fn valid_atom(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && value.as_bytes()[0].is_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture;

    impl Adapter for Fixture {
        fn revision(&self) -> RelationValue {
            RelationValue::Compound("fixture_revision".into(), vec![RelationValue::Integer(1)])
        }

        fn transform(&self, request: &Request) -> Result<RelationReply, String> {
            assert_eq!(request.wanted, ["status"]);
            Ok(RelationReply {
                solutions: vec![crate::prolog::RelationSolution {
                    bindings: vec![RelationBinding {
                        name: "status".into(),
                        value: RelationValue::Atom("complete".into()),
                    }],
                    preference: 17,
                }],
                context_queries: vec![],
                dependency_keys: vec![],
                diagnostics: vec![],
            })
        }
    }

    #[test]
    fn registered_relation_round_trips_through_explicit_context() {
        let handle = "rust_relation_fixture";
        let _ = register(handle, Arc::new(Fixture));
        let prolog = crate::prolog::global().unwrap();
        let given = vec![RelationBinding {
            name: "source".into(),
            value: RelationValue::String("fixture".into()),
        }];
        let limits = RelationLimits::default();
        let mut request = crate::prolog::RelationRequest {
            grammar: RelationValue::Compound(
                "registered_relation".into(),
                vec![RelationValue::Atom(handle.into())],
            ),
            given,
            wanted: vec!["status".into()],
            observations: vec![],
            limits,
        };
        let pending = prolog.transform(&request).unwrap();
        let graph = pending
            .context_queries
            .iter()
            .map(|value| {
                let RelationValue::Compound(_, fields) = value else {
                    panic!("query node must be compound")
                };
                let RelationValue::Compound(_, ask) = &fields[1] else {
                    panic!("ask must be compound")
                };
                ContextQueryNode {
                    id: fields[0].clone(),
                    query: crate::prolog::ContextQuery {
                        cardinality: crate::prolog::ContextCardinality::One,
                        domain: ask[1].clone(),
                        selector: ask[2].clone(),
                    },
                }
            })
            .collect::<Vec<_>>();
        let observations =
            crate::parser::execute_context_graph(prolog, &graph, &crate::parser::EmptyContext)
                .unwrap()
                .unwrap();
        request.observations = observations
            .iter()
            .map(crate::prolog::context_observation_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let resolved = prolog.transform(&request).unwrap();
        assert_eq!(resolved.solutions.len(), 1);
        assert_eq!(resolved.solutions[0].preference, 17);
        assert_eq!(
            resolved.solutions[0].bindings,
            [RelationBinding {
                name: "status".into(),
                value: RelationValue::Atom("complete".into()),
            }]
        );
        assert_eq!(resolved.dependency_keys.len(), 1);
    }
}
