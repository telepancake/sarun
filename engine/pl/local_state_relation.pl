:- module(local_state_relation,
          [ empty_local_state/1,
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
