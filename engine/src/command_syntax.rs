//! Neutral command syntax and its one-way adapter into the ordinary grammar IR.
//!
//! Command implementations own these immutable values. Brush collects them;
//! Prolog receives only the resulting generic grammar vocabulary. Neither side
//! needs to know the other's parser representation.

use crate::prolog::RelationValue;
use std::collections::BTreeSet;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSyntax {
    pub name: String,
    pub description: String,
    pub options: Vec<CommandOptionSyntax>,
    pub positionals: Vec<CommandPositionalSyntax>,
    /// Accept otherwise-unmodelled words, except spellings declared in
    /// `options`. This is useful while incrementally importing a large grammar:
    /// a known option can never fall through and evade its declared domain.
    pub allow_opaque_words: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOptionSyntax {
    pub id: String,
    pub spellings: Vec<String>,
    pub description: String,
    pub value: CommandValueSyntax,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPositionalSyntax {
    pub id: String,
    pub description: String,
    pub value: CommandValueSyntax,
    pub minimum: usize,
    pub maximum: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandValueSyntax {
    Finite(FiniteDomainSyntax),
    Context {
        domain: String,
        syntax: String,
        lexical: LexicalSyntax,
        cardinality: ContextCardinalitySyntax,
        selector: ContextSelectorSyntax,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LexicalSyntax {
    NonEmptyCodepointsExcept(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCardinalitySyntax {
    Empty,
    One,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextSelectorSyntax {
    NameFromSurface,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FiniteDomainSyntax {
    pub values: Vec<FiniteValueSyntax>,
    pub separator: Option<char>,
    /// Lowered as `unique(value)`: exact parsed RelationValue identity, not a
    /// source-insensitive semantic projection.
    pub unique: bool,
    pub syntax: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FiniteValueSyntax {
    pub surface: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandSyntaxError {
    TooMany(&'static str),
    Empty(&'static str),
    Invalid(&'static str),
    Duplicate { kind: &'static str, value: String },
    InvalidCardinality { id: String },
    SeparatorConflict { id: String, surface: String },
    RuleNameCollision(String),
}

impl fmt::Display for CommandSyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooMany(kind) => write!(formatter, "too many {kind} in command syntax"),
            Self::Empty(kind) => write!(formatter, "empty {kind} in command syntax"),
            Self::Invalid(kind) => write!(formatter, "invalid {kind} in command syntax"),
            Self::Duplicate { kind, value } => {
                write!(formatter, "duplicate {kind} {value:?} in command syntax")
            }
            Self::InvalidCardinality { id } => {
                write!(formatter, "invalid positional cardinality for {id:?}")
            }
            Self::SeparatorConflict { id, surface } => write!(
                formatter,
                "finite value {surface:?} conflicts with separator for {id:?}"
            ),
            Self::RuleNameCollision(name) => {
                write!(
                    formatter,
                    "generated grammar rule name collision for {name:?}"
                )
            }
        }
    }
}

fn required_text(value: &str, kind: &'static str) -> Result<(), CommandSyntaxError> {
    if value.is_empty() {
        Err(CommandSyntaxError::Empty(kind))
    } else if value.len() > 1024 {
        Err(CommandSyntaxError::TooMany(kind))
    } else {
        Ok(())
    }
}

fn validate_value(
    id: &str,
    value: &CommandValueSyntax,
    rule_names: &mut BTreeSet<String>,
    command: &str,
) -> Result<(), CommandSyntaxError> {
    match value {
        CommandValueSyntax::Finite(domain) => {
            required_text(&domain.syntax, "finite-domain syntax")?;
            if domain.separator.is_some_and(char::is_whitespace) {
                return Err(CommandSyntaxError::SeparatorConflict {
                    id: id.into(),
                    surface: "whitespace separator".into(),
                });
            }
            if domain.values.is_empty() {
                return Err(CommandSyntaxError::Empty("finite domain"));
            }
            if domain.values.len() > 256 {
                return Err(CommandSyntaxError::TooMany("finite-domain values"));
            }
            let mut surfaces = BTreeSet::new();
            for value in &domain.values {
                required_text(&value.surface, "finite value surface")?;
                required_text(&value.description, "finite value description")?;
                if !surfaces.insert(value.surface.clone()) {
                    return Err(CommandSyntaxError::Duplicate {
                        kind: "finite value surface",
                        value: value.surface.clone(),
                    });
                }
                if domain
                    .separator
                    .is_some_and(|separator| value.surface.contains(separator))
                {
                    return Err(CommandSyntaxError::SeparatorConflict {
                        id: id.into(),
                        surface: value.surface.clone(),
                    });
                }
            }
        }
        CommandValueSyntax::Context {
            domain,
            syntax,
            lexical,
            ..
        } => {
            required_text(domain, "context domain")?;
            required_text(syntax, "context syntax")?;
            match lexical {
                LexicalSyntax::NonEmptyCodepointsExcept(excluded) if excluded.len() <= 1024 => {}
                LexicalSyntax::NonEmptyCodepointsExcept(_) => {
                    return Err(CommandSyntaxError::TooMany("excluded codepoints"));
                }
            }
            let name = rule_name(command, id, "context");
            if !rule_names.insert(name.clone()) {
                return Err(CommandSyntaxError::RuleNameCollision(name));
            }
        }
    }
    Ok(())
}

fn validate(commands: &[CommandSyntax]) -> Result<(), CommandSyntaxError> {
    if commands.is_empty() {
        return Err(CommandSyntaxError::Empty("command list"));
    }
    if commands.len() > 256 {
        return Err(CommandSyntaxError::TooMany("commands"));
    }
    let mut command_names = BTreeSet::new();
    let mut rule_names = BTreeSet::from(["builtin_invocation".into(), "required_space".into()]);
    let mut syntax_entries = commands.len();
    for command in commands {
        required_text(&command.name, "command name")?;
        required_text(&command.description, "command description")?;
        if !command_names.insert(command.name.clone()) {
            return Err(CommandSyntaxError::Duplicate {
                kind: "command name",
                value: command.name.clone(),
            });
        }
        if command.options.len() > 256 || command.positionals.len() > 256 {
            return Err(CommandSyntaxError::TooMany("command arguments"));
        }
        syntax_entries = syntax_entries
            .saturating_add(command.options.len())
            .saturating_add(command.positionals.len());
        let mut ids = BTreeSet::new();
        let mut spellings = BTreeSet::new();
        for option in &command.options {
            required_text(&option.id, "option id")?;
            required_text(&option.description, "option description")?;
            if !ids.insert(option.id.clone()) {
                return Err(CommandSyntaxError::Duplicate {
                    kind: "argument id",
                    value: option.id.clone(),
                });
            }
            if option.spellings.is_empty() || option.spellings.len() > 32 {
                return Err(CommandSyntaxError::TooMany("option spellings"));
            }
            for spelling in &option.spellings {
                required_text(spelling, "option spelling")?;
                if spelling.chars().any(char::is_whitespace) {
                    return Err(CommandSyntaxError::Invalid("token-exact option spelling"));
                }
                if !spellings.insert(spelling.clone()) {
                    return Err(CommandSyntaxError::Duplicate {
                        kind: "option spelling",
                        value: spelling.clone(),
                    });
                }
            }
            syntax_entries = syntax_entries.saturating_add(option.spellings.len());
            if let CommandValueSyntax::Finite(domain) = &option.value {
                syntax_entries = syntax_entries.saturating_add(domain.values.len());
            }
            validate_value(&option.id, &option.value, &mut rule_names, &command.name)?;
        }
        for positional in &command.positionals {
            required_text(&positional.id, "positional id")?;
            required_text(&positional.description, "positional description")?;
            if !ids.insert(positional.id.clone()) {
                return Err(CommandSyntaxError::Duplicate {
                    kind: "argument id",
                    value: positional.id.clone(),
                });
            }
            if positional
                .maximum
                .is_some_and(|maximum| positional.minimum > maximum)
                || positional.minimum > 256
            {
                return Err(CommandSyntaxError::InvalidCardinality {
                    id: positional.id.clone(),
                });
            }
            validate_value(
                &positional.id,
                &positional.value,
                &mut rule_names,
                &command.name,
            )?;
            if let CommandValueSyntax::Finite(domain) = &positional.value {
                syntax_entries = syntax_entries.saturating_add(domain.values.len());
            }
        }
        if syntax_entries > 8192 {
            return Err(CommandSyntaxError::TooMany("total syntax entries"));
        }
    }
    Ok(())
}

fn compound(name: &str, arguments: Vec<RelationValue>) -> RelationValue {
    RelationValue::Compound(name.into(), arguments)
}

fn list(values: Vec<RelationValue>) -> RelationValue {
    RelationValue::List(values)
}

fn choice(mut expressions: Vec<RelationValue>) -> RelationValue {
    if expressions.len() == 1 {
        expressions.remove(0)
    } else {
        compound("choice", vec![list(expressions)])
    }
}

fn presentation_metadata(
    syntax: &str,
    description: &str,
    preference: Option<i64>,
) -> Vec<RelationValue> {
    let mut metadata = vec![
        compound(
            "meta",
            vec![
                RelationValue::Atom("syntax".into()),
                RelationValue::Atom(syntax.into()),
            ],
        ),
        compound(
            "meta",
            vec![
                RelationValue::Atom("description".into()),
                RelationValue::String(description.into()),
            ],
        ),
    ];
    if let Some(preference) = preference {
        metadata.push(compound(
            "meta",
            vec![
                RelationValue::Atom("preference".into()),
                RelationValue::Integer(preference),
            ],
        ));
    }
    metadata
}

fn presentation(syntax: &str, description: &str, preference: Option<i64>) -> RelationValue {
    compound(
        "presentation",
        vec![list(presentation_metadata(syntax, description, preference))],
    )
}

fn symbolic_presentation(syntax: &str, description: &str) -> RelationValue {
    let mut metadata = presentation_metadata(syntax, description, None);
    metadata.push(compound(
        "meta",
        vec![
            RelationValue::Atom("tear".into()),
            RelationValue::Atom("symbolic".into()),
        ],
    ));
    compound("presentation", vec![list(metadata)])
}

fn literal(
    surface: &str,
    semantic: RelationValue,
    syntax: &str,
    description: &str,
) -> RelationValue {
    compound(
        "literal",
        vec![
            RelationValue::String(surface.into()),
            semantic,
            presentation(syntax, description, Some(30)),
        ],
    )
}

fn semantic(kind: &str, command: &str, id: &str, surface: &str) -> RelationValue {
    compound(
        kind,
        vec![
            RelationValue::String(command.into()),
            RelationValue::String(id.into()),
            RelationValue::String(surface.into()),
        ],
    )
}

fn required_space() -> RelationValue {
    compound("ref", vec![RelationValue::Atom("required_space".into())])
}

fn sequence(expressions: Vec<RelationValue>) -> RelationValue {
    compound("seq", vec![list(expressions)])
}

fn rule_name(command: &str, id: &str, suffix: &str) -> String {
    let mut result = String::from("command_");
    for character in command
        .chars()
        .chain(['_'])
        .chain(id.chars())
        .chain(['_'])
        .chain(suffix.chars())
    {
        if character.is_ascii_alphanumeric() || character == '_' {
            result.push(character.to_ascii_lowercase());
        } else {
            result.push('_');
        }
    }
    result
}

fn finite_literal(
    command: &str,
    id: &str,
    domain: &FiniteDomainSyntax,
    value: &FiniteValueSyntax,
) -> RelationValue {
    literal(
        &value.surface,
        semantic("command_value", command, id, &value.surface),
        &domain.syntax,
        &value.description,
    )
}

fn finite_expression(command: &str, id: &str, domain: &FiniteDomainSyntax) -> RelationValue {
    let items = choice(
        domain
            .values
            .iter()
            .map(|value| finite_literal(command, id, domain, value))
            .collect(),
    );
    let Some(separator) = domain.separator else {
        return items;
    };
    compound(
        "separated",
        vec![
            RelationValue::Integer(1),
            RelationValue::Atom("unbounded".into()),
            literal(
                &separator.to_string(),
                RelationValue::Atom("value_separator".into()),
                "operator",
                "value separator",
            ),
            if domain.unique {
                compound("unique", vec![RelationValue::Atom("value".into())])
            } else {
                RelationValue::Atom("allow_duplicates".into())
            },
            items,
        ],
    )
}

fn context_expression(
    command: &str,
    id: &str,
    description: &str,
    domain: &str,
    syntax: &str,
    lexical: &LexicalSyntax,
    cardinality: ContextCardinalitySyntax,
    selector: ContextSelectorSyntax,
) -> RelationValue {
    let LexicalSyntax::NonEmptyCodepointsExcept(excluded) = lexical;
    let path_codepoint = compound(
        "terminal",
        vec![
            compound(
                "text",
                vec![compound(
                    "codepoint",
                    vec![compound(
                        "except",
                        vec![RelationValue::String(excluded.clone())],
                    )],
                )],
            ),
            symbolic_presentation(syntax, description),
        ],
    );
    compound(
        "context",
        vec![
            RelationValue::Atom(rule_name(command, id, "context")),
            compound(
                "repeat",
                vec![
                    RelationValue::Integer(1),
                    RelationValue::Atom("unbounded".into()),
                    path_codepoint,
                ],
            ),
            compound(
                "ask",
                vec![
                    RelationValue::Atom(
                        match cardinality {
                            ContextCardinalitySyntax::Empty => "empty",
                            ContextCardinalitySyntax::One => "one",
                            ContextCardinalitySyntax::All => "all",
                        }
                        .into(),
                    ),
                    RelationValue::Atom(domain.into()),
                    match selector {
                        ContextSelectorSyntax::NameFromSurface => compound(
                            "name",
                            vec![compound(
                                "value",
                                vec![RelationValue::Atom("surface".into())],
                            )],
                        ),
                    },
                ],
            ),
            presentation(syntax, description, None),
        ],
    )
}

fn value_expression(
    command: &str,
    id: &str,
    description: &str,
    value: &CommandValueSyntax,
) -> RelationValue {
    match value {
        CommandValueSyntax::Finite(domain) => finite_expression(command, id, domain),
        CommandValueSyntax::Context {
            domain,
            syntax,
            lexical,
            cardinality,
            selector,
        } => context_expression(
            command,
            id,
            description,
            domain,
            syntax,
            lexical,
            *cardinality,
            *selector,
        ),
    }
}

fn non_whitespace_codepoint() -> RelationValue {
    compound(
        "terminal",
        vec![
            compound(
                "text",
                vec![compound(
                    "codepoint",
                    vec![compound(
                        "except",
                        vec![RelationValue::String(" \t\r\n".into())],
                    )],
                )],
            ),
            presentation("argument", "command argument", None),
        ],
    )
}

fn opaque_word(excluded_options: &[RelationValue]) -> RelationValue {
    let mut expressions = Vec::new();
    if !excluded_options.is_empty() {
        expressions.push(compound("not", vec![choice(excluded_options.to_vec())]));
    }
    expressions.push(compound(
        "repeat",
        vec![
            RelationValue::Integer(1),
            RelationValue::Atom("unbounded".into()),
            non_whitespace_codepoint(),
        ],
    ));
    sequence(expressions)
}

fn command_expression(command: &CommandSyntax) -> RelationValue {
    let command_literal = literal(
        &command.name,
        compound("command", vec![RelationValue::String(command.name.clone())]),
        "command",
        &command.description,
    );
    let mut options = Vec::new();
    let mut excluded_options = Vec::new();
    for option in &command.options {
        for spelling in &option.spellings {
            let flag = literal(
                spelling,
                semantic("command_option", &command.name, &option.id, spelling),
                "builtin_flag",
                &option.description,
            );
            excluded_options.push(sequence(vec![
                literal(
                    spelling,
                    RelationValue::Atom("reserved_option".into()),
                    "builtin_flag",
                    &option.description,
                ),
                compound("not", vec![non_whitespace_codepoint()]),
            ]));
            options.push(sequence(vec![
                flag,
                required_space(),
                value_expression(
                    &command.name,
                    &option.id,
                    &option.description,
                    &option.value,
                ),
            ]));
        }
    }

    let mut invocation = vec![command_literal];
    let mut unordered = options;
    if command.allow_opaque_words {
        unordered.push(opaque_word(&excluded_options));
    }
    if !unordered.is_empty() {
        invocation.push(compound(
            "repeat",
            vec![
                RelationValue::Integer(0),
                RelationValue::Atom("unbounded".into()),
                sequence(vec![required_space(), choice(unordered)]),
            ],
        ));
    }

    for positional in &command.positionals {
        let tail = sequence(vec![
            required_space(),
            value_expression(
                &command.name,
                &positional.id,
                &positional.description,
                &positional.value,
            ),
        ]);
        let expression = match (positional.minimum, positional.maximum) {
            (1, Some(1)) => tail,
            (0, Some(1)) => compound("optional", vec![tail]),
            (minimum, maximum) => compound(
                "repeat",
                vec![
                    RelationValue::Integer(minimum as i64),
                    maximum.map_or_else(
                        || RelationValue::Atom("unbounded".into()),
                        |maximum| RelationValue::Integer(maximum as i64),
                    ),
                    tail,
                ],
            ),
        };
        invocation.push(expression);
    }
    sequence(invocation)
}

/// Lower command-owned syntax into the same generic grammar IR used by every
/// other text grammar. This adapter has no command-name branches.
pub fn grammar(commands: &[CommandSyntax]) -> Result<RelationValue, CommandSyntaxError> {
    validate(commands)?;
    let command_expressions = commands.iter().map(command_expression).collect::<Vec<_>>();
    let rules = vec![
        compound(
            "rule",
            vec![
                RelationValue::Atom("builtin_invocation".into()),
                choice(command_expressions),
            ],
        ),
        compound(
            "rule",
            vec![
                RelationValue::Atom("required_space".into()),
                compound(
                    "repeat",
                    vec![
                        RelationValue::Integer(1),
                        RelationValue::Atom("unbounded".into()),
                        compound(
                            "terminal",
                            vec![
                                compound(
                                    "text",
                                    vec![compound(
                                        "codepoint",
                                        vec![compound(
                                            "chars",
                                            vec![RelationValue::String(" \t\r".into())],
                                        )],
                                    )],
                                ),
                                presentation("trivia", "whitespace", None),
                            ],
                        ),
                    ],
                ),
            ],
        ),
    ];
    Ok(RelationValue::Compound(
        "grammar".into(),
        vec![
            compound(
                "source",
                vec![compound("text", vec![RelationValue::Atom("utf8".into())])],
            ),
            RelationValue::Atom("builtin_invocation".into()),
            list(rules),
            list(vec![]),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finite_command() -> CommandSyntax {
        CommandSyntax {
            name: "sample".into(),
            description: "sample command".into(),
            options: vec![CommandOptionSyntax {
                id: "kind".into(),
                spellings: vec!["--kind".into()],
                description: "select kind".into(),
                value: CommandValueSyntax::Finite(FiniteDomainSyntax {
                    values: vec![
                        FiniteValueSyntax {
                            surface: "a".into(),
                            description: "first".into(),
                        },
                        FiniteValueSyntax {
                            surface: "b".into(),
                            description: "second".into(),
                        },
                    ],
                    separator: Some(','),
                    unique: true,
                    syntax: "sample_kind".into(),
                }),
            }],
            positionals: vec![],
            allow_opaque_words: true,
        }
    }

    #[test]
    fn finite_unique_list_lowers_to_one_terse_generic_node() {
        let grammar = grammar(&[finite_command()]).unwrap();
        let debug = format!("{grammar:?}");
        assert_eq!(debug.matches("\"separated\"").count(), 1);
        assert!(debug.contains("Compound(\"unique\", [Atom(\"value\")])"));
        assert!(!debug.contains("values_"));
    }

    #[test]
    fn validation_rejects_duplicate_surfaces_and_commands() {
        let mut duplicate_surface = finite_command();
        let CommandValueSyntax::Finite(domain) = &mut duplicate_surface.options[0].value else {
            unreachable!()
        };
        domain.values[1].surface = "a".into();
        assert!(matches!(
            grammar(&[duplicate_surface]),
            Err(CommandSyntaxError::Duplicate {
                kind: "finite value surface",
                ..
            })
        ));

        let command = finite_command();
        assert!(matches!(
            grammar(&[command.clone(), command]),
            Err(CommandSyntaxError::Duplicate {
                kind: "command name",
                ..
            })
        ));
    }

    #[test]
    fn validation_rejects_ambiguous_and_invalid_schema_shapes() {
        let mut duplicate_option = finite_command();
        let mut second = duplicate_option.options[0].clone();
        second.id = "other".into();
        duplicate_option.options.push(second);
        assert!(matches!(
            grammar(&[duplicate_option]),
            Err(CommandSyntaxError::Duplicate {
                kind: "option spelling",
                ..
            })
        ));

        let mut separator_conflict = finite_command();
        let CommandValueSyntax::Finite(domain) = &mut separator_conflict.options[0].value else {
            unreachable!()
        };
        domain.values[0].surface = "a,b".into();
        assert!(matches!(
            grammar(&[separator_conflict]),
            Err(CommandSyntaxError::SeparatorConflict { .. })
        ));

        let mut empty_domain = finite_command();
        let CommandValueSyntax::Finite(domain) = &mut empty_domain.options[0].value else {
            unreachable!()
        };
        domain.values.clear();
        assert!(matches!(
            grammar(&[empty_domain]),
            Err(CommandSyntaxError::Empty("finite domain"))
        ));

        let mut invalid_cardinality = finite_command();
        invalid_cardinality
            .positionals
            .push(CommandPositionalSyntax {
                id: "operand".into(),
                description: "operand".into(),
                value: invalid_cardinality.options[0].value.clone(),
                minimum: 2,
                maximum: Some(1),
            });
        assert!(matches!(
            grammar(&[invalid_cardinality]),
            Err(CommandSyntaxError::InvalidCardinality { .. })
        ));
    }

    #[test]
    fn validation_rejects_generated_context_rule_name_collisions() {
        let context = |id: &str| CommandPositionalSyntax {
            id: id.into(),
            description: "contextual name".into(),
            value: CommandValueSyntax::Context {
                domain: "names".into(),
                syntax: "name".into(),
                lexical: LexicalSyntax::NonEmptyCodepointsExcept(" ".into()),
                cardinality: ContextCardinalitySyntax::One,
                selector: ContextSelectorSyntax::NameFromSurface,
            },
            minimum: 0,
            maximum: Some(1),
        };
        let command = CommandSyntax {
            name: "sample".into(),
            description: "sample command".into(),
            options: vec![],
            positionals: vec![context("a-b"), context("a_b")],
            allow_opaque_words: false,
        };
        assert!(matches!(
            grammar(&[command]),
            Err(CommandSyntaxError::RuleNameCollision(_))
        ));
    }
}
