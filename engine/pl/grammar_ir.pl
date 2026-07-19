:- module(grammar_ir, [valid_grammar/1]).

/** <module> Closed declarative grammar-value vocabulary

This module validates grammar data; it does not parse a particular grammar.
The vocabulary is deliberately shaped so imported Tree-sitter grammars and
Wireshark-style binary layouts can be represented without generating Prolog
control flow or adding an engine entry point.  Execution belongs to
`grammar_engine`; codecs and declared bounded primitives are ordinary values.

The current schema is `grammar(Source, Root, Rules, Primitives)`.  It is an
initial closed IR and will grow only through generic relational constructs,
never grammar-name switches.
*/

valid_grammar(grammar(Source, Root, Rules, Primitives)) :-
    valid_source(Source),
    atom(Root),
    proper_list(Rules),
    Rules = [_|_],
    proper_list(Primitives),
    rule_names(Rules, Names),
    all_unique(Names),
    list_member(Root, Names),
    primitive_names(Primitives, PrimitiveNames),
    all_unique(PrimitiveNames),
    valid_primitives(Primitives),
    valid_rules(Rules, Names, PrimitiveNames).

valid_source(source(text(utf8))).
valid_source(source(bytes)).

rule_names([], []).
rule_names([rule(Name, _)|Rules], [Name|Names]) :-
    atom(Name),
    rule_names(Rules, Names).

primitive_names([], []).
primitive_names([primitive(Name, _, _, _)|Primitives], [Name|Names]) :-
    atom(Name),
    primitive_names(Primitives, Names).

valid_primitives([]).
valid_primitives([primitive(Name, Arity, modes(Modes), bounded(Maximum))|Rest]) :-
    atom(Name),
    integer(Arity),
    Arity >= 0,
    proper_list(Modes),
    Modes = [_|_],
    valid_modes(Modes),
    integer(Maximum),
    Maximum >= 0,
    valid_primitives(Rest).

valid_modes([]).
valid_modes([Mode|Modes]) :-
    ( Mode = in ; Mode = out ; Mode = inout ),
    valid_modes(Modes).

valid_rules([], _, _).
valid_rules([rule(_, Expression)|Rules], Names, Primitives) :-
    valid_expression(Expression, Names, Primitives),
    valid_rules(Rules, Names, Primitives).

valid_expression(empty, _, _).
valid_expression(ref(Name), Names, _) :- list_member(Name, Names).
valid_expression(seq(Expressions), Names, Primitives) :-
    proper_list(Expressions),
    valid_expressions(Expressions, Names, Primitives).
valid_expression(choice(Expressions), Names, Primitives) :-
    proper_list(Expressions),
    Expressions = [_,_|_],
    valid_expressions(Expressions, Names, Primitives).
valid_expression(optional(Expression), Names, Primitives) :-
    valid_expression(Expression, Names, Primitives).
valid_expression(not(Expression), Names, Primitives) :-
    valid_expression(Expression, Names, Primitives).
valid_expression(repeat(Minimum, Maximum, Expression), Names, Primitives) :-
    integer(Minimum),
    Minimum >= 0,
    valid_maximum(Maximum, Minimum),
    valid_expression(Expression, Names, Primitives).
valid_expression(
    separated(Minimum, Maximum, Separator, Uniqueness, Item),
    Names, Primitives) :-
    integer(Minimum),
    Minimum >= 0,
    valid_maximum(Maximum, Minimum),
    valid_uniqueness(Uniqueness),
    valid_expression(Separator, Names, Primitives),
    valid_expression(Item, Names, Primitives).
valid_expression(field(Name, Expression), Names, Primitives) :-
    atom(Name),
    valid_expression(Expression, Names, Primitives).
valid_expression(literal(Surface, Semantic, presentation(Presentation)), _, _) :-
    text(Surface),
    ground(Semantic),
    valid_metadata(Presentation).
valid_expression(terminal(Codec, presentation(Presentation)), _, Primitives) :-
    valid_codec(Codec, Primitives),
    valid_metadata(Presentation).
valid_expression(precedence(Level, Associativity, Expression), Names,
                 Primitives) :-
    number(Level),
    valid_associativity(Associativity),
    valid_expression(Expression, Names, Primitives).
valid_expression(conflicts(RuleNames, Expression), Names, Primitives) :-
    proper_list(RuleNames),
    RuleNames = [_|_],
    all_rule_names(RuleNames, Names),
    valid_expression(Expression, Names, Primitives).
valid_expression(extras(ExtraExpressions, Expression), Names, Primitives) :-
    proper_list(ExtraExpressions),
    valid_expressions(ExtraExpressions, Names, Primitives),
    valid_expression(Expression, Names, Primitives).
valid_expression(lexical(Expression), Names, Primitives) :-
    valid_expression(Expression, Names, Primitives).
valid_expression(embed(Grammar, Boundary), _, _) :-
    valid_embedded_grammar(Grammar),
    ground(Boundary).
valid_expression(constraint(Constraint), _, Primitives) :-
    valid_constraint(Constraint, Primitives).
valid_expression(dispatch(Key, Cases, Default), Names, Primitives) :-
    valid_value(Key),
    proper_list(Cases),
    Cases = [_|_],
    valid_cases(Cases, Names, Primitives),
    valid_dispatch_default(Default, Names, Primitives).
valid_expression(context(Name, Ask), _, _) :-
    atom(Name),
    valid_context_query(Ask).
valid_expression(context(Name, Expression, Ask,
                         presentation(Presentation)), Names, Primitives) :-
    atom(Name),
    valid_expression(Expression, Names, Primitives),
    valid_context_query(Ask),
    valid_metadata(Presentation).

valid_expressions([], _, _).
valid_expressions([Expression|Expressions], Names, Primitives) :-
    valid_expression(Expression, Names, Primitives),
    valid_expressions(Expressions, Names, Primitives).

valid_maximum(unbounded, _).
valid_maximum(Maximum, Minimum) :-
    integer(Maximum),
    Maximum >= Minimum.

valid_uniqueness(unique).
valid_uniqueness(allow_duplicates).

valid_associativity(none).
valid_associativity(left).
valid_associativity(right).

valid_codec(text(class(Name)), _) :- atom(Name).
valid_codec(text(regex(Pattern)), _) :- string(Pattern).
valid_codec(text(codepoint(Set)), _) :- valid_codepoint_set(Set).
valid_codec(bytes(uint(Bits, Endian)), _) :-
    integer(Bits),
    Bits > 0,
    0 is Bits mod 8,
    ( Endian = little ; Endian = big ).
valid_codec(bytes(slice(Length)), _) :- valid_value(Length).
valid_codec(bytes(rest), _).
valid_codec(primitive(Name, Arguments), Primitives) :-
    list_member(Name, Primitives),
    proper_list(Arguments),
    valid_values(Arguments).

valid_codepoint_set(any).
valid_codepoint_set(chars(Characters)) :- text(Characters).
valid_codepoint_set(except(Characters)) :- text(Characters).
valid_codepoint_set(range(Low, High)) :-
    integer(Low), integer(High), 0 =< Low, Low =< High, High =< 0x10ffff.
valid_codepoint_set(union(Sets)) :-
    proper_list(Sets), Sets = [_|_], valid_codepoint_sets(Sets).

valid_codepoint_sets([]).
valid_codepoint_sets([Set|Sets]) :-
    valid_codepoint_set(Set), valid_codepoint_sets(Sets).

valid_constraint(equal(Left, Right), _) :-
    valid_value(Left),
    valid_value(Right).
valid_constraint(checksum(Algorithm, Covered, Expected), _) :-
    atom(Algorithm),
    valid_value(Covered),
    valid_value(Expected).
valid_constraint(primitive(Name, Arguments), Primitives) :-
    list_member(Name, Primitives),
    proper_list(Arguments),
    valid_values(Arguments).

valid_value(value(Name)) :- atom(Name).
valid_value(constant(Value)) :- ground(Value).
valid_value(remaining).
valid_value(add(Left, Right)) :- valid_value(Left), valid_value(Right).
valid_value(subtract(Left, Right)) :- valid_value(Left), valid_value(Right).
valid_value(multiply(Left, Right)) :- valid_value(Left), valid_value(Right).

valid_values([]).
valid_values([Value|Values]) :- valid_value(Value), valid_values(Values).

valid_cases([], _, _).
valid_cases([case(Value, Expression)|Cases], Names, Primitives) :-
    ground(Value),
    valid_expression(Expression, Names, Primitives),
    valid_cases(Cases, Names, Primitives).

valid_dispatch_default(fail, _, _).
valid_dispatch_default(default(Expression), Names, Primitives) :-
    valid_expression(Expression, Names, Primitives).

valid_embedded_grammar(grammar_ref(Name)) :- atom(Name).
valid_embedded_grammar(Grammar) :- valid_grammar(Grammar).

valid_context_query(ask(Cardinality, Domain, Selector)) :-
    ( Cardinality = empty ; Cardinality = one ; Cardinality = all ),
    ground(Domain),
    ground(Selector).

valid_metadata([]).
valid_metadata([meta(Name, Value)|Metadata]) :-
    atom(Name),
    ground(Value),
    valid_metadata(Metadata).

all_rule_names([], _).
all_rule_names([Name|RuleNames], Names) :-
    list_member(Name, Names),
    all_rule_names(RuleNames, Names).

all_unique(Values) :-
    sort(Values, Unique),
    length(Values, Count),
    length(Unique, Count).

proper_list([]).
proper_list([_|Values]) :- proper_list(Values).

list_member(Value, [Value|_]).
list_member(Value, [_|Values]) :- list_member(Value, Values).

text(Value) :- string(Value), !.
text(Value) :- atom(Value).
