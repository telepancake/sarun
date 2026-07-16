:- module(ast_state_relation,
          [ derive_ast_state_steps/4,
            valid_ast_state_rules/1
          ]).

/** <module> Declarative AST-to-local-state adaptation

Syntax grammars produce neutral trees.  A client can separately supply data
which relates selected named nodes and fields to the generic scoped-state
steps.  This keeps language semantics out of the engine while avoiding a
language-specific AST walker in Rust or Prolog.

Rules have this form:

  state_rule(node(RuleName), Captures, before(Steps), after(Steps))

Captures select the current node span or a named field's span, value, or exact
UTF-8 source text.  `slot(Name)` and `node_identity` in step templates are
substituted while the tree is walked.  Before/after emissions make lexical
scope entry and exit ordinary data.
*/

valid_ast_state_rules(Rules) :-
    proper_list(Rules),
    valid_ast_state_rules_(Rules).

valid_ast_state_rules_([]).
valid_ast_state_rules_([
    state_rule(node(RuleName), Captures, before(Before), after(After))|Rules
]) :-
    atom(RuleName),
    proper_list(Captures),
    valid_captures(Captures, []),
    proper_list(Before),
    proper_list(After),
    ground(Before), ground(After),
    valid_ast_state_rules_(Rules).

valid_captures([], _).
valid_captures([capture(Name, Selector)|Captures], Seen) :-
    atom(Name),
    \+ member_eq(Name, Seen),
    valid_capture_selector(Selector),
    valid_captures(Captures, [Name|Seen]).

valid_capture_selector(node_span).
valid_capture_selector(field_span(Name)) :- atom(Name).
valid_capture_selector(field_value(Name)) :- atom(Name).
valid_capture_selector(field_text(Name)) :- atom(Name).

derive_ast_state_steps(Rules, Ast, Source, Steps) :-
    valid_ast_state_rules(Rules),
    ground(Ast),
    source_text(Source, _),
    walk_tree(Ast, Rules, Source, Steps).

walk_tree(node(Name, Span, Value), Rules, Source, Steps) :- !,
    applicable_emissions(Rules, Name, Span, Value, Source, Before, After),
    walk_tree(Value, Rules, Source, Children),
    append(Before, Children, BeforeChildren),
    append(BeforeChildren, After, Steps).
walk_tree(Value, Rules, Source, Steps) :-
    compound(Value), !,
    Value =.. [_|Arguments],
    walk_trees(Arguments, Rules, Source, Steps).
walk_tree(_, _, _, []).

walk_trees([], _, _, []).
walk_trees([Value|Values], Rules, Source, Steps) :-
    walk_tree(Value, Rules, Source, First),
    walk_trees(Values, Rules, Source, Rest),
    append(First, Rest, Steps).

applicable_emissions([], _, _, _, _, [], []).
applicable_emissions([Rule|Rules], Name, Span, Value, Source, Before, After) :-
    rule_emissions(Rule, Name, Span, Value, Source, RuleBefore, RuleAfter), !,
    applicable_emissions(Rules, Name, Span, Value, Source, RestBefore,
                         RestAfter),
    append(RuleBefore, RestBefore, Before),
    append(RestAfter, RuleAfter, After).
applicable_emissions([_|Rules], Name, Span, Value, Source, Before, After) :-
    applicable_emissions(Rules, Name, Span, Value, Source, Before, After).

rule_emissions(
    state_rule(node(Name), CaptureSpecs, before(BeforeTemplates),
               after(AfterTemplates)),
    Name, Span, Value, Source, Before, After) :-
    capture_values(CaptureSpecs, Span, Value, Source, Captures),
    NodeIdentity = node_ref(Name, Span),
    instantiate_templates(BeforeTemplates, Captures, NodeIdentity, Before),
    instantiate_templates(AfterTemplates, Captures, NodeIdentity, After),
    ground(Before), ground(After).

capture_values([], _, _, _, []).
capture_values([capture(Name, Selector)|Specs], Span, Value, Source,
               [Name-Captured|Captures]) :-
    capture_value(Selector, Span, Value, Source, Captured),
    capture_values(Specs, Span, Value, Source, Captures).

capture_value(node_span, Span, _, _, Span).
capture_value(field_span(Name), _, Value, _, Span) :-
    node_field(Name, Value, Span, _).
capture_value(field_value(Name), _, Value, _, FieldValue) :-
    node_field(Name, Value, _, FieldValue).
capture_value(field_text(Name), _, Value, Source, Text) :-
    node_field(Name, Value, Span, _),
    source_span_text(Source, Span, Text).

% Fields are searched within the current named node, but never through a child
% named node.  A field wrapping a child node is still visible at its owner.
node_field(Name, field(Name, Span, Value), Span, Value) :- !.
node_field(_, node(_, _, _), _, _) :- !, fail.
node_field(Name, Value, Span, FieldValue) :-
    compound(Value),
    Value =.. [_|Arguments],
    node_field_arguments(Name, Arguments, Span, FieldValue).

node_field_arguments(Name, [Value|_], Span, FieldValue) :-
    node_field(Name, Value, Span, FieldValue), !.
node_field_arguments(Name, [_|Values], Span, FieldValue) :-
    node_field_arguments(Name, Values, Span, FieldValue).

instantiate_templates([], _, _, []).
instantiate_templates([Template|Templates], Captures, NodeIdentity,
                      [Value|Values]) :-
    instantiate_template(Template, Captures, NodeIdentity, Value),
    instantiate_templates(Templates, Captures, NodeIdentity, Values).

instantiate_template(slot(Name), Captures, _, Value) :- !,
    capture_member(Name, Captures, Value).
instantiate_template(node_identity, _, NodeIdentity, NodeIdentity) :- !.
instantiate_template(Value, _, _, Value) :- atomic(Value), !.
instantiate_template(Template, Captures, NodeIdentity, Value) :-
    Template =.. [Functor|Arguments],
    instantiate_template_arguments(Arguments, Captures, NodeIdentity, Values),
    Value =.. [Functor|Values].

instantiate_template_arguments([], _, _, []).
instantiate_template_arguments([Template|Templates], Captures, NodeIdentity,
                               [Value|Values]) :-
    instantiate_template(Template, Captures, NodeIdentity, Value),
    instantiate_template_arguments(Templates, Captures, NodeIdentity, Values).

capture_member(Name, [Name-Value|_], Value) :- !.
capture_member(Name, [_|Captures], Value) :-
    capture_member(Name, Captures, Value).

source_span_text(Source, span(ByteStart, ByteEnd), Text) :-
    source_text(Source, Whole),
    integer(ByteStart), integer(ByteEnd),
    0 =< ByteStart, ByteStart =< ByteEnd,
    byte_character_offset(Whole, ByteStart, CharacterStart),
    byte_character_offset(Whole, ByteEnd, CharacterEnd),
    CharacterLength is CharacterEnd - CharacterStart,
    sub_string(Whole, CharacterStart, CharacterLength, _, Text).

source_text(text_source(Text, _, _), Text) :- string(Text).
source_text(Text, Text) :- string(Text).

byte_character_offset(Text, TargetByte, Character) :-
    byte_character_offset(Text, TargetByte, 1, 0, 0, Character).

byte_character_offset(_, TargetByte, _, Character, TargetByte, Character) :- !.
byte_character_offset(Text, TargetByte, Index, Character0, Byte0, Character) :-
    Byte0 < TargetByte,
    string_code(Index, Text, Code),
    utf8_codepoint_bytes(Code, Width),
    Byte1 is Byte0 + Width,
    Byte1 =< TargetByte,
    Index1 is Index + 1,
    Character1 is Character0 + 1,
    byte_character_offset(Text, TargetByte, Index1, Character1, Byte1,
                          Character).

utf8_codepoint_bytes(Code, 1) :- Code =< 0x7f, !.
utf8_codepoint_bytes(Code, 2) :- Code =< 0x7ff, !.
utf8_codepoint_bytes(Code, 3) :- Code =< 0xffff, !.
utf8_codepoint_bytes(Code, 4) :- Code =< 0x10ffff.

member_eq(Value, [Seen|_]) :- Value == Seen, !.
member_eq(Value, [_|Values]) :- member_eq(Value, Values).

proper_list([]).
proper_list([_|Values]) :- proper_list(Values).

append([], Tail, Tail).
append([Value|Values], Tail, [Value|Result]) :- append(Values, Tail, Result).
