:- module(context_relation,
          [ context_query/3,
            context_tear_match/6,
            observe_query/4,
            dependency_key/2,
            valid_query_graph/1,
            stage_context/4,
            ready_queries/3,
            resolve_query_refs/3,
            valid_snapshot/1
          ]).

/** <module> Pure contextual query relation

Context access is explicit relational data. A query is
`ask(Cardinality, Domain, Selector)`, where cardinality is `empty`, `one`, or
`all`. Providers supply immutable query-scoped snapshots:

```
snapshot(source(Provider, Revision),
         [entry(Domain, Identity, Names, Value, Attributes), ...])
```

`one` fails unless exactly one distinct typed identity matches. Query graphs
may contain `ref(QueryId)` terms; a ref is ready only after the referenced
`one` query has a successful observation, and substitutes that entry's typed
value. No provider I/O occurs in this module.
*/

context_query(ask(empty, Domain, Selector), Snapshot, empty(IsEmpty)) :-
    matching_entries(Domain, Selector, Snapshot, Entries),
    ( Entries = [] -> IsEmpty = true ; IsEmpty = false ).
context_query(ask(one, Domain, Selector), Snapshot, one(Entry)) :-
    matching_entries(Domain, Selector, Snapshot, [Entry]).
context_query(ask(all, Domain, Selector), Snapshot, all(Entries)) :-
    matching_entries(Domain, Selector, Snapshot, Entries).

%! context_tear_match(+AllQuery, +Snapshot, +Surface, -Name,
%!                    -ExactQuery, -Entry) is nondet.
%
% Bind a contextual tear using the same selector and cardinality relation used
% by exact parsing.  `Name` is offered only when replacing the prefix selector
% with that exact name makes `one` succeed.  Ambiguous aliases consequently do
% not become completions which would fail immediately after insertion.

context_tear_match(ask(all, Domain, Selector), Snapshot, Surface, Name,
                   ask(one, Domain, ExactSelector), Entry) :-
    context_query(ask(all, Domain, Selector), Snapshot, all(Entries)),
    findall(NameString-(Identity-Candidate),
            ( list_member(Candidate, Entries),
              Candidate = entry(_, Identity, Names, _, _),
              list_member(CandidateName, Names),
              selector_tear_binding(Selector, Surface, CandidateName, _),
              text_string(CandidateName, NameString)
            ),
            NamePairs0),
    keysort(NamePairs0, NamePairs),
    group_name_pairs(NamePairs, NameGroups),
    list_member(Name-IdentityEntries, NameGroups),
    sort(IdentityEntries, [_Identity-Entry]),
    selector_tear_binding(Selector, Surface, Name, ExactSelector),
    Entry = entry(Domain, _, _, _, _).

group_name_pairs([], []).
group_name_pairs([Name-Value|Pairs], [Name-Values|Groups]) :-
    take_name_pairs(Pairs, Name, [Value], Values, Rest),
    group_name_pairs(Rest, Groups).

take_name_pairs([Name-Value|Pairs], Name, Values0, Values, Rest) :-
    !,
    take_name_pairs(Pairs, Name, [Value|Values0], Values, Rest).
take_name_pairs(Pairs, _Name, Values, Values, Pairs).

selector_tear_binding(prefix(Surface), Surface, Name, name(Name)) :-
    text_prefix(Surface, Name).
selector_tear_binding(within(Attribute, Selector), Surface, Name,
                      within(Attribute, ExactSelector)) :-
    selector_tear_binding(Selector, Surface, Name, ExactSelector).
selector_tear_binding(and(Left, Right), Surface, Name,
                      and(ExactLeft, Right)) :-
    selector_tear_binding(Left, Surface, Name, ExactLeft).
selector_tear_binding(and(Left, Right), Surface, Name,
                      and(Left, ExactRight)) :-
    selector_tear_binding(Right, Surface, Name, ExactRight).
selector_tear_binding(or(Left, Right), Surface, Name,
                      or(ExactLeft, Right)) :-
    selector_tear_binding(Left, Surface, Name, ExactLeft).
selector_tear_binding(or(Left, Right), Surface, Name,
                      or(Left, ExactRight)) :-
    selector_tear_binding(Right, Surface, Name, ExactRight).

observe_query(Id, Query, Snapshot,
              observed(Id, Query, Source, Outcome)) :-
    Snapshot = snapshot(Source, _),
    ( context_query(Query, Snapshot, Result)
    -> Outcome = some(Result)
    ;  Outcome = none
    ).

dependency_key(observed(Id, Query, _Source, Outcome),
               dependency(Id, Query, Outcome)).

matching_entries(Domain, Selector, snapshot(_, Entries), Matching) :-
    valid_snapshot_entries(Entries),
    findall(Identity-Entry,
            ( list_member(Entry, Entries),
              Entry = entry(EntryDomain, Identity, _, _, _),
              EntryDomain == Domain,
              once(selector_matches(Selector, Entry))
            ),
            Pairs),
    keysort(Pairs, Sorted),
    pair_values(Sorted, Matching).

selector_matches(any, _).
selector_matches(name(Name), entry(_, _, Names, _, _)) :-
    list_member(Candidate, Names),
    same_text(Candidate, Name),
    !.
selector_matches(prefix(Prefix), entry(_, _, Names, _, _)) :-
    list_member(Candidate, Names),
    text_prefix(Prefix, Candidate),
    !.
selector_matches(where(Attribute), entry(_, _, _, _, Attributes)) :-
    list_member(Attribute, Attributes).
selector_matches(within(Attribute, Selector), Entry) :-
    selector_matches(where(Attribute), Entry),
    selector_matches(Selector, Entry).
selector_matches(and(Left, Right), Entry) :-
    selector_matches(Left, Entry),
    selector_matches(Right, Entry).
selector_matches(or(Left, _), Entry) :- selector_matches(Left, Entry), !.
selector_matches(or(_, Right), Entry) :- selector_matches(Right, Entry).

same_text(Left, Right) :-
    text_string(Left, LeftString),
    text_string(Right, RightString),
    LeftString = RightString.

text_prefix(Prefix, Text) :-
    text_string(Prefix, PrefixString),
    text_string(Text, TextString),
    string_length(PrefixString, Length),
    sub_string(TextString, 0, Length, _, PrefixString).

text_string(Text, Text) :- string(Text), !.
text_string(Text, String) :- atom(Text), atom_string(Text, String).

valid_snapshot(snapshot(source(Provider, Revision), Entries)) :-
    ground(Provider),
    ground(Revision),
    valid_snapshot_entries(Entries).

valid_snapshot_entries(Entries) :-
    proper_list(Entries),
    valid_entries(Entries, []).

valid_entries([], _).
valid_entries([entry(Domain, Identity, Names, Value, Attributes)|Entries], Seen) :-
    ground(Domain),
    ground(Identity),
    ground(Value),
    proper_text_list(Names),
    Names = [_|_],
    proper_ground_list(Attributes),
    \+ list_member(Domain-Identity, Seen),
    valid_entries(Entries, [Domain-Identity|Seen]).

proper_text_list([]).
proper_text_list([Text|Texts]) :-
    ( string(Text) ; atom(Text) ),
    proper_text_list(Texts).

proper_ground_list([]).
proper_ground_list([Term|Terms]) :-
    ground(Term),
    proper_ground_list(Terms).

%! valid_query_graph(+Graph) is semidet.

valid_query_graph(Graph) :-
    proper_list(Graph),
    valid_query_nodes(Graph, [], Graph),
    graph_acyclic(Graph).

valid_query_nodes([], _, _).
valid_query_nodes([query(Id, Ask)|Nodes], Seen, Graph) :-
    ground(Id),
    \+ list_member(Id, Seen),
    valid_ask(Ask),
    query_dependencies(Ask, Dependencies),
    valid_dependencies(Dependencies, Id, Graph),
    valid_query_nodes(Nodes, [Id|Seen], Graph).

valid_ask(ask(Cardinality, Domain, Selector)) :-
    valid_cardinality(Cardinality),
    ground(Domain),
    ground(Selector).

valid_cardinality(empty).
valid_cardinality(one).
valid_cardinality(all).

valid_dependencies([], _, _).
valid_dependencies([Dependency|Dependencies], Id, Graph) :-
    Dependency \== Id,
    query_node(Graph, Dependency, ask(one, _, _)),
    valid_dependencies(Dependencies, Id, Graph).

query_dependencies(Term, Dependencies) :-
    findall(Id, term_ref(Term, Id), Refs),
    sort(Refs, Dependencies).

term_ref(ref(Id), Id).
term_ref(Term, Id) :-
    compound(Term),
    Term \= ref(_),
    Term =.. [_|Arguments],
    list_member(Argument, Arguments),
    term_ref(Argument, Id).

graph_acyclic(Graph) :-
    \+ ( query_node(Graph, Id, _), dependency_path(Graph, Id, Id, []) ).

dependency_path(Graph, From, Target, Seen) :-
    query_node(Graph, From, Ask),
    query_dependencies(Ask, Dependencies),
    list_member(Next, Dependencies),
    ( Next == Target
    ; \+ list_member(Next, Seen),
      dependency_path(Graph, Next, Target, [Next|Seen])
    ).

query_node([query(Id, Ask)|_], Id, Ask).
query_node([_|Nodes], Id, Ask) :- query_node(Nodes, Id, Ask).

%! ready_queries(+Graph, +Observations, -Ready) is det.

ready_queries(Graph, Observations, Ready) :-
    ( stage_context(Graph, Observations, Ready, _)
    -> true
    ;  Ready = []
    ).

%! stage_context(+Graph, +Observations, -Ready, -DependencyKeys) is semidet.
%
% Validate a staged query/observation exchange, return queries whose typed
% dependencies are now ready, and project provenance-free invalidation keys.

stage_context(Graph, Observations, Ready, DependencyKeys) :-
    valid_query_graph(Graph),
    valid_observations(Observations, Graph),
    findall(query(Id, Resolved),
            ( query_node(Graph, Id, Ask),
              \+ observation_for(Observations, Id, _),
              resolve_refs(Ask, Observations, Resolved)
            ),
            Ready),
    observation_dependencies(Observations, DependencyKeys).

observation_dependencies([], []).
observation_dependencies([Observation|Observations],
                         [Dependency|Dependencies]) :-
    dependency_key(Observation, Dependency),
    observation_dependencies(Observations, Dependencies).

% Project one graph query into the concrete query actually sent to its
% provider, using successful `one` observations for typed dependencies.
resolve_query_refs(Query, Observations, Resolved) :-
    resolve_refs(Query, Observations, Resolved).

valid_observations([], _).
valid_observations([Observation|Observations], Graph) :-
    Observation = observed(Id, Query, source(Provider, Revision), Outcome),
    ground(Observation),
    query_node(Graph, Id, Query),
    ground(Provider),
    ground(Revision),
    valid_outcome(Query, Outcome),
    \+ observation_for(Observations, Id, _),
    valid_observations(Observations, Graph).

valid_outcome(_, none).
valid_outcome(ask(empty, _, _), some(empty(Boolean))) :-
    ( Boolean = true ; Boolean = false ).
valid_outcome(ask(one, Domain, _),
              some(one(entry(Domain, Identity, Names, Value, Attributes)))) :-
    valid_entries([entry(Domain, Identity, Names, Value, Attributes)], []).
valid_outcome(ask(all, Domain, _), some(all(Entries))) :-
    valid_entries_for_domain(Entries, Domain),
    entries_are_sorted(Entries).

valid_entries_for_domain([], _).
valid_entries_for_domain([entry(Domain, Identity, Names, Value, Attributes)|Entries],
                         Domain) :-
    valid_entries([entry(Domain, Identity, Names, Value, Attributes)], []),
    valid_entries_for_domain(Entries, Domain).

entries_are_sorted([]).
entries_are_sorted([_]).
entries_are_sorted([entry(_, Left, _, _, _), entry(_, Right, _, _, _)|Entries]) :-
    Left @< Right,
    entries_are_sorted([entry(_, Right, _, _, _)|Entries]).

observation_for([observed(Id, Query, Source, Outcome)|_], Id,
                observed(Id, Query, Source, Outcome)).
observation_for([_|Observations], Id, Observation) :-
    observation_for(Observations, Id, Observation).

resolve_refs(ref(Id), Observations, Value) :-
    observation_for(Observations, Id,
                    observed(Id, _, _,
                             some(one(entry(_, _, _, Value, _))))),
    !.
resolve_refs(ref(_), _Observations, _Value) :- !, fail.
resolve_refs(Term, _Observations, Term) :- atomic(Term), !.
resolve_refs(Term, Observations, Resolved) :-
    Term =.. [Functor|Arguments],
    resolve_ref_list(Arguments, Observations, ResolvedArguments),
    Resolved =.. [Functor|ResolvedArguments].

resolve_ref_list([], _, []).
resolve_ref_list([Argument|Arguments], Observations,
                 [Resolved|ResolvedArguments]) :-
    resolve_refs(Argument, Observations, Resolved),
    resolve_ref_list(Arguments, Observations, ResolvedArguments).

proper_list([]).
proper_list([_|Items]) :- proper_list(Items).

list_member(Item, [Head|_]) :- Item = Head.
list_member(Item, [_|Items]) :- list_member(Item, Items).

pair_values([], []).
pair_values([_-Value|Pairs], [Value|Values]) :- pair_values(Pairs, Values).
