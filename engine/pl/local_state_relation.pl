:- module(local_state_relation,
          [ empty_local_state/1,
           context_state_completion_pairs/3,
           state_constraint_completion_pairs/3,
           state_step_constraints/2,
           valid_local_state/1,
           resolve_state_resolutions/3,
           run_state_steps/7,
           run_state_steps/8,
           symbolic_text_source/4
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

%! run_state_steps(+Steps, +Initial, -Final, -Resolutions, -Queries, -Delta,
%!                 -CompletionPairs) is semidet.
%
%  Name tears in ordinary `use/3` steps are related to the bindings visible at
%  that exact point in the state transition.  They also emit an explicit `all`
%  context query so provider-backed names can be unioned with local matches.

run_state_steps(Steps, Initial, Final, Resolutions, Queries, Delta,
                CompletionPairs) :-
    run_state_steps(Steps, Initial, Final, Resolutions, Queries, Delta,
                    CompletionPairs, _Applications).

run_state_steps(Steps, Initial, Final, Resolutions, Queries, Delta,
                CompletionPairs, Applications) :-
    proper_list(Steps),
    valid_local_state(Initial),
    run_steps(Steps, Initial, Final, Resolutions, Queries, CompletionPairs,
              Applications),
    Final = local_state(_, Delta),
    valid_local_state(Final).

%! context_state_completion_pairs(+Queries, +Observations, -Pairs) is det.
%
%  Resolve provider observations for the explicit name-completion queries
%  emitted above.  This is the external half of the same relation; consumers
%  never scan provider entries or reinterpret a language grammar.

context_state_completion_pairs(Queries, Observations, Pairs) :-
    findall(Pair,
            context_state_completion_pair(Queries, Observations, Pair),
            Pairs).

context_state_completion_pair(Queries, Observations,
    completion_key(Span, Insert)-
        (alternative(context(Domain, Identity), variable, Provider)-50)) :-
    value_member(
        query(CompletionId, Query), Queries),
    CompletionId = name_completion(_, Prefix, Hole, Suffix),
    Query = ask(all, Domain, prefix(QueryPrefix)),
    value_member(
        observed(CompletionId, Query, source(Provider, _),
                 some(all(Entries))),
        Observations),
    value_member(entry(Domain, Identity, Names, _, _), Entries),
    value_member(Name, Names),
    string(Name),
    Hole = hole(_, Span, Surface, _),
    string_concat(Prefix, Surface, QueryPrefix),
    hole_insertion(Name, Prefix, Suffix, Surface, Insert),
    Insert \= "".

%! state_constraint_completion_pairs(+Constraints, +State, -Pairs)
%  is semidet.
%
%  Resolve symbolic text references through the same scoped state used during
%  parsing, then relate a single source hole to a finite typed value domain.
%  The result uses the engine's ordinary completion-pair representation.

state_constraint_completion_pairs(Constraints, State, Pairs) :-
    proper_list(Constraints),
    valid_text_constraints(Constraints),
    valid_local_state(State),
    findall(Pair,
            state_constraint_completion_pair(Constraints, State, Pair),
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

state_constraint_completion_pair([Constraint|_], State, Pair) :-
    text_constraint_completion_pair(Constraint, State, Pair).
state_constraint_completion_pair([_|Constraints], State, Pair) :-
    state_constraint_completion_pair(Constraints, State, Pair).

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

%! symbolic_text_source(+Expression, +State, +Origin, -Source) is semidet.
%
%  Lower a symbolic text value at one exact local-state checkpoint to an
%  ordinary UTF-8 text source.  A single source hole may have a virtual byte
%  position in the lowered text which differs from its physical edit span in
%  the containing document.  The text grammar owns parsing both forms.

symbolic_text_source(Expression, State, Origin,
                     text_source(Text, exact, Origin)) :-
    resolve_text_expression(Expression, State, [], Segments),
    text_segments_string(Segments, Text),
    !.
symbolic_text_source(Expression, State, Origin,
                     text_source(Text,
                                 assist(EditId, span(VirtualStart, VirtualEnd),
                                        replace_span(PhysicalStart,
                                                     PhysicalEnd)),
                                 Origin)) :-
    resolve_text_expression(Expression, State, [], Segments),
    single_text_hole(Segments,
                     hole(EditId, span(PhysicalStart, PhysicalEnd), Surface, _),
                     Prefix, Suffix),
    string_concat(Prefix, Surface, PrefixSurface),
    string_concat(PrefixSurface, Suffix, Text),
    string_utf8_bytes(Prefix, VirtualStart),
    string_utf8_bytes(Surface, SurfaceBytes),
    VirtualEnd is VirtualStart + SurfaceBytes.

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

run_steps([], State, State, [], [], [], []).
run_steps([Step|Steps], State0, State, Resolutions, Queries,
          CompletionPairs, Applications) :-
    state_step(Step, State0, State1, StepResolutions, StepQueries,
               StepCompletionPairs, StepApplications),
    run_steps(Steps, State1, State, RestResolutions, RestQueries,
              RestCompletionPairs, RestApplications),
    append(StepResolutions, RestResolutions, Resolutions),
    append(StepQueries, RestQueries, Queries),
    append(StepCompletionPairs, RestCompletionPairs, CompletionPairs),
    append(StepApplications, RestApplications, Applications).

state_step(enter(ScopeId), local_state(Scopes, Delta),
           local_state([scope(ScopeId, [])|Scopes], Delta), [], [], [], []) :-
    ground(ScopeId).
state_step(leave(ScopeId),
           local_state([scope(ScopeId, Bindings), Parent|Scopes], Delta0),
           local_state([MergedParent|Scopes], Delta), [], [], [], []) :-
    escaping_bindings(Bindings, Escaping),
    merge_bindings(Escaping, Parent, MergedParent),
    merge_delta(Escaping, Delta0, Delta).
state_step(define(Domain, Name, Value, Lifetime, Policy),
           local_state([scope(ScopeId, Bindings0)|Scopes], Delta0),
           local_state([scope(ScopeId, Bindings)|Scopes], Delta), [], [], [], []) :-
    valid_binding(local_binding(Domain, Name, Value, Lifetime)),
    valid_policy(Policy),
    define_binding(Policy, local_binding(Domain, Name, Value, Lifetime),
                   Bindings0, Bindings),
    definition_delta(Lifetime, Domain, Name, Value, Delta0, Delta).
state_step(constraint(Constraint), State, State, [], [], [], []) :-
    ground(Constraint).
state_step(apply(Id, Grammar, Given), State, State, [], [], [],
           [application(Id, Grammar, Given, State)]) :-
    ground(Id), ground(Grammar), ground(Given).
state_step(use(Id, Domain, Name), State, State,
           [resolved(Id, local(Binding))], [], [], []) :-
    ground(Id), ground(Domain), ground(Name),
    Name \= text_hole(_, _, _),
    lookup_binding(State, Domain, Name, Binding),
    !.
state_step(use(Id, Domain, Name), State, State,
           [resolved(Id, external(ref(Id)))],
           [query(Id, ask(one, Domain, name(Name)))], [], []) :-
    ground(Id), ground(Domain), ground(Name),
    Name \= text_hole(_, _, _).
state_step(use(Id, Domain, TextHole), State, State,
           [resolved(Id, incomplete(TextHole))],
           [query(CompletionId, ask(all, Domain, prefix(QueryPrefix)))],
           CompletionPairs, []) :-
    ground(Id), ground(Domain),
    TextHole = text_hole(Prefix, Hole, Suffix),
    Hole = hole(_, _, Surface, _),
    string(Prefix), string(Suffix), string(Surface),
    string_concat(Prefix, Surface, QueryPrefix),
    CompletionId = name_completion(Id, Prefix, Hole, Suffix),
    findall(Pair,
            local_name_completion_pair(State, Domain, Prefix, Hole, Suffix,
                                       Pair),
            CompletionPairs).
state_step(require(Id, Cardinality, Domain, Selector), State, State, [],
           [query(Id, ask(Cardinality, Domain, Selector))], [], []) :-
    ground(Id),
    valid_cardinality(Cardinality),
    ground(Domain), ground(Selector).

local_name_completion_pair(State, Domain, Prefix, Hole, Suffix,
    completion_key(Span, Insert)-
        (alternative(local(Domain, Name), variable, local_state)-80)) :-
    visible_binding(State, Domain, Name, _),
    string(Name),
    Hole = hole(_, Span, Surface, _),
    hole_insertion(Name, Prefix, Suffix, Surface, Insert),
    Insert \= "".

visible_binding(State, Domain, Name, Binding) :-
    state_binding(State, Domain, Name, Candidate),
    lookup_binding(State, Domain, Name, Binding),
    Candidate == Binding.

state_binding(local_state(Scopes, _), Domain, Name, Binding) :-
    value_member(scope(_, Bindings), Scopes),
    value_member(Binding, Bindings),
    Binding = local_binding(Domain, Name, _, _).

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

string_utf8_bytes(Text, Bytes) :-
    string_codes(Text, Codes),
    utf8_codes_bytes(Codes, 0, Bytes).

utf8_codes_bytes([], Bytes, Bytes).
utf8_codes_bytes([Code|Codes], Bytes0, Bytes) :-
    utf8_codepoint_bytes(Code, Width),
    Bytes1 is Bytes0 + Width,
    utf8_codes_bytes(Codes, Bytes1, Bytes).

utf8_codepoint_bytes(Code, 1) :- Code =< 0x7f, !.
utf8_codepoint_bytes(Code, 2) :- Code =< 0x7ff, !.
utf8_codepoint_bytes(Code, 3) :- Code =< 0xffff, !.
utf8_codepoint_bytes(Code, 4) :- Code =< 0x10ffff.

append([], Tail, Tail).
append([Value|Values], Tail, [Value|Result]) :- append(Values, Tail, Result).
