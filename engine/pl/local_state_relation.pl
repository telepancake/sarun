:- module(local_state_relation,
          [ empty_local_state/1,
           state_constraint_completion_pairs/3,
           state_constraint_completion_pairs/4,
           state_step_constraints/2,
           valid_local_state/1,
           resolve_state_resolutions/3,
           run_state_steps/6
          ]).

/** <module> Pure scoped state transitions for relational grammars

Local declarations and assignments are resolved before any external context
query is produced. Scopes are top-first. Escaping definitions remain available
locally and are also accumulated as an explicit output delta; lexical bindings
vanish when their scope closes.
*/

empty_local_state(local_state([scope(root, [])], [])).

valid_local_state(local_state(Scopes, Delta)) :-
    proper_list(Scopes),
    Scopes = [_|_],
    valid_scopes(Scopes),
    proper_list(Delta),
    valid_delta(Delta).

%! run_state_steps(+Steps, +Initial, -Final, -Resolutions, -Queries, -Delta)
%  is semidet.

run_state_steps(Steps, Initial, Final, Resolutions, Queries, Delta) :-
    proper_list(Steps),
    valid_local_state(Initial),
    run_steps(Steps, Initial, Final, Resolutions, Queries),
    Final = local_state(_, Delta),
    valid_local_state(Final).

%! state_constraint_completion_pairs(+Constraints, +State, -Pairs)
%  is semidet.
%
%  Resolve symbolic text references through the same scoped state used during
%  parsing, then relate a single source hole to a finite typed value domain.
%  The result uses the engine's ordinary completion-pair representation.

state_constraint_completion_pairs(Constraints, State, Pairs) :-
    state_constraint_completion_pairs(Constraints, State, [], Pairs).

state_constraint_completion_pairs(Constraints, State, Signatures, Pairs) :-
    proper_list(Constraints),
    valid_text_constraints(Constraints),
    proper_list(Signatures), valid_signatures(Signatures),
    valid_local_state(State),
    findall(Pair,
            state_constraint_completion_pair(Constraints, State, Signatures,
                                             Pair),
            Pairs).

valid_text_constraints([]).
valid_text_constraints([Constraint|Constraints]) :-
    valid_text_constraint(Constraint),
    valid_text_constraints(Constraints).

valid_text_constraint(
    text_constraint(Expression, one_of(Values), presentation(Syntax))) :-
    valid_text_expression(Expression),
    proper_list(Values), Values = [_|_],
    valid_constraint_values(Values),
    atom(Syntax).
valid_text_constraint(invocation_constraint(Command, Arguments)) :-
    valid_text_expression(Command),
    proper_list(Arguments), valid_text_expressions(Arguments).

valid_text_expressions([]).
valid_text_expressions([Expression|Expressions]) :-
    valid_text_expression(Expression),
    valid_text_expressions(Expressions).

valid_signatures([]).
valid_signatures([signature(Command, Rules)|Signatures]) :-
    string(Command),
    proper_list(Rules), valid_signature_rules(Rules),
    valid_signatures(Signatures).

valid_signature_rules([]).
valid_signature_rules([
    following(Flag, one_of(Values), presentation(Syntax))|Rules
]) :-
    string(Flag),
    proper_list(Values), Values = [_|_], valid_constraint_values(Values),
    atom(Syntax),
    valid_signature_rules(Rules).

valid_text_expression(text(Segments)) :-
    proper_list(Segments),
    valid_text_segments(Segments).

valid_text_segments([]).
valid_text_segments([Segment|Segments]) :-
    valid_text_segment(Segment),
    valid_text_segments(Segments).

valid_text_segment(Text) :- string(Text), !.
valid_text_segment(hole(EditId, span(Start, End), Surface, Codec)) :-
    ground(EditId), integer(Start), integer(End), 0 =< Start, Start =< End,
    string(Surface), ground(Codec), !.
valid_text_segment(reference(Domain, Name)) :-
    ground(Domain), ground(Name), !.
valid_text_segment(text(Segments)) :-
    valid_text_expression(text(Segments)).

valid_constraint_values([]).
valid_constraint_values([value(Text, Semantic, Description, Preference)|Values]) :-
    string(Text), ground(Semantic), ground(Description), number(Preference),
    valid_constraint_values(Values).

state_constraint_completion_pair([Constraint|_], State, Signatures, Pair) :-
    constraint_completion_pair(Constraint, State, Signatures, Pair).
state_constraint_completion_pair([_|Constraints], State, Signatures, Pair) :-
    state_constraint_completion_pair(Constraints, State, Signatures, Pair).

constraint_completion_pair(Constraint, State, _, Pair) :-
    text_constraint_completion_pair(Constraint, State, Pair).
constraint_completion_pair(
    invocation_constraint(Command, Arguments), State, Signatures, Pair) :-
    resolve_text_string(Command, State, CommandText),
    signature_member(signature(CommandText, Rules), Signatures),
    signature_constraint(Rules, Arguments, State, Constraint),
    text_constraint_completion_pair(Constraint, State, Pair).

signature_constraint([
    following(Flag, Domain, Presentation)|_
], Arguments, State,
    text_constraint(Value, Domain, Presentation)) :-
    following_argument(Arguments, State, Flag, Value).
signature_constraint([_|Rules], Arguments, State, Constraint) :-
    signature_constraint(Rules, Arguments, State, Constraint).

following_argument([FlagExpression, Value|_], State, Flag, Value) :-
    resolve_text_string(FlagExpression, State, Flag).
following_argument([_|Arguments], State, Flag, Value) :-
    following_argument(Arguments, State, Flag, Value).

resolve_text_string(Expression, State, Text) :-
    resolve_text_expression(Expression, State, [], Segments),
    text_segments_string(Segments, Text).

signature_member(Signature, [Signature|_]).
signature_member(Signature, [_|Signatures]) :-
    signature_member(Signature, Signatures).

state_step_constraints(Steps, Constraints) :-
    proper_list(Steps),
    step_constraints(Steps, Constraints).

step_constraints([], []).
step_constraints([constraint(Constraint)|Steps], [Constraint|Constraints]) :- !,
    step_constraints(Steps, Constraints).
step_constraints([_|Steps], Constraints) :-
    step_constraints(Steps, Constraints).

text_constraint_completion_pair(
    text_constraint(Expression, one_of(Values), presentation(Syntax)),
    State,
    completion_key(Span, Insert)-
        (alternative(Semantic, Syntax, Description)-Preference)) :-
    atom(Syntax),
    proper_list(Values),
    resolve_text_expression(Expression, State, [], Segments),
    single_text_hole(Segments, Hole, Prefix, Suffix),
    Hole = hole(_EditId, Span, Surface, _Codec),
    value_member(value(Text, Semantic, Description, Preference), Values),
    string(Text), ground(Semantic), ground(Description), number(Preference),
    hole_insertion(Text, Prefix, Suffix, Surface, Insert).

resolve_text_expression(text(Segments), State, Seen, Resolved) :-
    proper_list(Segments),
    resolve_text_segments(Segments, State, Seen, Resolved).

resolve_text_segments([], _, _, []).
resolve_text_segments([Segment|Segments], State, Seen, Resolved) :-
    resolve_text_segment(Segment, State, Seen, First),
    resolve_text_segments(Segments, State, Seen, Rest),
    append(First, Rest, Resolved).

resolve_text_segment(Text, _, _, [Text]) :- string(Text), !.
resolve_text_segment(hole(EditId, Span, Surface, Codec), _, _,
                     [hole(EditId, Span, Surface, Codec)]) :-
    ground(hole(EditId, Span, Surface, Codec)),
    string(Surface), !.
resolve_text_segment(reference(Domain, Name), State, Seen, Resolved) :-
    ground(Domain), ground(Name),
    \+ binding_key_member(Domain, Name, Seen),
    lookup_binding(State, Domain, Name,
                   local_binding(Domain, Name, Value, _)),
    resolve_text_expression(Value, State, [Domain-Name|Seen], Resolved).
resolve_text_segment(text(Segments), State, Seen, Resolved) :-
    resolve_text_expression(text(Segments), State, Seen, Resolved).

single_text_hole(Segments, Hole, Prefix, Suffix) :-
    append(PrefixSegments, [Hole|SuffixSegments], Segments),
    Hole = hole(_, _, _, _),
    text_segments_string(PrefixSegments, Prefix),
    text_segments_string(SuffixSegments, Suffix).

text_segments_string([], "").
text_segments_string([Text|Texts], Combined) :-
    string(Text),
    text_segments_string(Texts, Rest),
    string_concat(Text, Rest, Combined).

hole_insertion(Text, Prefix, Suffix, Surface, Insert) :-
    string_length(Text, TextLength),
    string_length(Prefix, PrefixLength),
    string_length(Suffix, SuffixLength),
    InsertLength is TextLength - PrefixLength - SuffixLength,
    InsertLength >= 0,
    sub_string(Text, 0, PrefixLength, _, Prefix),
    sub_string(Text, PrefixLength, InsertLength, SuffixLength, Insert),
    sub_string(Text, _, SuffixLength, 0, Suffix),
    string_length(Surface, SurfaceLength),
    sub_string(Insert, 0, SurfaceLength, _, Surface).

value_member(Value, [Value|_]).
value_member(Value, [_|Values]) :- value_member(Value, Values).

run_steps([], State, State, [], []).
run_steps([Step|Steps], State0, State, Resolutions, Queries) :-
    state_step(Step, State0, State1, StepResolutions, StepQueries),
    run_steps(Steps, State1, State, RestResolutions, RestQueries),
    append(StepResolutions, RestResolutions, Resolutions),
    append(StepQueries, RestQueries, Queries).

state_step(enter(ScopeId), local_state(Scopes, Delta),
           local_state([scope(ScopeId, [])|Scopes], Delta), [], []) :-
    ground(ScopeId).
state_step(leave(ScopeId),
           local_state([scope(ScopeId, Bindings), Parent|Scopes], Delta0),
           local_state([MergedParent|Scopes], Delta), [], []) :-
    escaping_bindings(Bindings, Escaping),
    merge_bindings(Escaping, Parent, MergedParent),
    merge_delta(Escaping, Delta0, Delta).
state_step(define(Domain, Name, Value, Lifetime, Policy),
           local_state([scope(ScopeId, Bindings0)|Scopes], Delta0),
           local_state([scope(ScopeId, Bindings)|Scopes], Delta), [], []) :-
    valid_binding(local_binding(Domain, Name, Value, Lifetime)),
    valid_policy(Policy),
    define_binding(Policy, local_binding(Domain, Name, Value, Lifetime),
                   Bindings0, Bindings),
    definition_delta(Lifetime, Domain, Name, Value, Delta0, Delta).
state_step(constraint(Constraint), State, State, [], []) :-
    ground(Constraint).
state_step(use(Id, Domain, Name), State, State,
           [resolved(Id, local(Binding))], []) :-
    ground(Id), ground(Domain), ground(Name),
    lookup_binding(State, Domain, Name, Binding),
    !.
state_step(use(Id, Domain, Name), State, State,
           [resolved(Id, external(ref(Id)))],
           [query(Id, ask(one, Domain, name(Name)))]) :-
    ground(Id), ground(Domain), ground(Name).
state_step(require(Id, Cardinality, Domain, Selector), State, State, [],
           [query(Id, ask(Cardinality, Domain, Selector))]) :-
    ground(Id),
    valid_cardinality(Cardinality),
    ground(Domain), ground(Selector).

resolve_state_resolutions([], _, []).
resolve_state_resolutions(
    [resolved(Id, external(ref(Id)))|Resolutions], Observations,
    [resolved(Id, External)|Resolved]) :- !,
    ( resolution_observation(Id, Observations, Outcome)
    -> observed_external(Outcome, External)
    ;  External = external(ref(Id))
    ),
    resolve_state_resolutions(Resolutions, Observations, Resolved).
resolve_state_resolutions([Resolution|Resolutions], Observations,
                          [Resolution|Resolved]) :-
    resolve_state_resolutions(Resolutions, Observations, Resolved).

resolution_observation(Id, [observed(Id, _, _, Outcome)|_], Outcome) :- !.
resolution_observation(Id, [_|Observations], Outcome) :-
    resolution_observation(Id, Observations, Outcome).

observed_external(some(Result), external(Result)).
% A failed `one` observation makes the semantic relation fail. `empty` and
% `all` queries always have a value, including false and an empty list.
observed_external(none, _) :- fail.

valid_policy(replace).
valid_policy(unique).

valid_cardinality(empty).
valid_cardinality(one).
valid_cardinality(all).

valid_scopes([]).
valid_scopes([scope(Id, Bindings)|Scopes]) :-
    ground(Id),
    proper_list(Bindings),
    valid_bindings(Bindings, []),
    valid_scopes(Scopes).

valid_bindings([], _).
valid_bindings([Binding|Bindings], Seen) :-
    valid_binding(Binding),
    Binding = local_binding(Domain, Name, _, _),
    \+ binding_key_member(Domain, Name, Seen),
    valid_bindings(Bindings, [Domain-Name|Seen]).

valid_binding(local_binding(Domain, Name, Value, Lifetime)) :-
    ground(Domain), ground(Name), ground(Value),
    valid_lifetime(Lifetime).

valid_lifetime(lexical).
valid_lifetime(escaping).

valid_delta([]).
valid_delta([state_change(Domain, Name, Value)|Delta]) :-
    ground(Domain), ground(Name), ground(Value),
    \+ delta_key_member(Domain, Name, Delta),
    valid_delta(Delta).

define_binding(unique, Binding, Bindings, [Binding|Bindings]) :-
    Binding = local_binding(Domain, Name, _, _),
    \+ binding_key_member(Domain, Name, Bindings).
define_binding(replace, Binding, Bindings0, [Binding|Bindings]) :-
    Binding = local_binding(Domain, Name, _, _),
    remove_binding(Domain, Name, Bindings0, Bindings).

lookup_binding(local_state(Scopes, _), Domain, Name, Binding) :-
    lookup_scopes(Scopes, Domain, Name, Binding).

lookup_scopes([scope(_, Bindings)|_], Domain, Name, Binding) :-
    binding_member(Domain, Name, Bindings, Binding),
    !.
lookup_scopes([_|Scopes], Domain, Name, Binding) :-
    lookup_scopes(Scopes, Domain, Name, Binding).

binding_member(Domain, Name,
               [local_binding(Domain, Name, Value, Lifetime)|_],
               local_binding(Domain, Name, Value, Lifetime)).
binding_member(Domain, Name, [_|Bindings], Binding) :-
    binding_member(Domain, Name, Bindings, Binding).

binding_key_member(Domain, Name, [Domain-Name|_]).
binding_key_member(Domain, Name, [_|Bindings]) :-
    binding_key_member(Domain, Name, Bindings).

remove_binding(_, _, [], []).
remove_binding(Domain, Name,
               [local_binding(Domain, Name, _, _)|Bindings], Rest) :- !,
    remove_binding(Domain, Name, Bindings, Rest).
remove_binding(Domain, Name, [Binding|Bindings], [Binding|Rest]) :-
    remove_binding(Domain, Name, Bindings, Rest).

escaping_bindings([], []).
escaping_bindings([local_binding(Domain, Name, Value, escaping)|Bindings],
                  [local_binding(Domain, Name, Value, escaping)|Escaping]) :- !,
    escaping_bindings(Bindings, Escaping).
escaping_bindings([_|Bindings], Escaping) :-
    escaping_bindings(Bindings, Escaping).

merge_bindings([], Parent, Parent).
merge_bindings([Binding|Bindings], scope(Id, Parent0), Merged) :-
    define_binding(replace, Binding, Parent0, Parent),
    merge_bindings(Bindings, scope(Id, Parent), Merged).

definition_delta(lexical, _, _, _, Delta, Delta).
definition_delta(escaping, Domain, Name, Value, Delta0, Delta) :-
    put_delta(Domain, Name, Value, Delta0, Delta).

merge_delta([], Delta, Delta).
merge_delta([local_binding(Domain, Name, Value, escaping)|Bindings], Delta0,
            Delta) :-
    put_delta(Domain, Name, Value, Delta0, Delta1),
    merge_delta(Bindings, Delta1, Delta).

put_delta(Domain, Name, Value, Delta0,
          [state_change(Domain, Name, Value)|Delta]) :-
    remove_delta(Domain, Name, Delta0, Delta).

remove_delta(_, _, [], []).
remove_delta(Domain, Name,
             [state_change(Domain, Name, _)|Delta], Rest) :- !,
    remove_delta(Domain, Name, Delta, Rest).
remove_delta(Domain, Name, [Change|Delta], [Change|Rest]) :-
    remove_delta(Domain, Name, Delta, Rest).

delta_key_member(Domain, Name, [state_change(Domain, Name, _)|_]).
delta_key_member(Domain, Name, [_|Delta]) :-
    delta_key_member(Domain, Name, Delta).

proper_list([]).
proper_list([_|Values]) :- proper_list(Values).

append([], Tail, Tail).
append([Value|Values], Tail, [Value|Result]) :- append(Values, Tail, Result).
