:- module(grammar_engine,
          [ neutral_input/2,
            valid_relation_mode/1,
            relation_status/3,
            relate_sequence/7,
            evidence_preference/3,
            literal_completion_evidence/7,
            project_completions/2,
            project_highlights/2,
            transform_relation/6
          ]).

:- use_module(context_relation).
:- use_module(evidence_projection).
:- use_module(ast_state_relation).
:- use_module(grammar_codec).
:- use_module(local_state_relation).
:- use_module(text_grammar_engine).
:- use_module(grammar_store).

/** <module> Grammar-independent relational sequence execution

This module owns the operational meaning of the current sequence grammar:
concrete source units, edit tears, rendered terminals, required/optional and
repeated fields, evidence, and exact consumption. It imports no sarun grammar
or catalog.

`TerminalRelation` is a grammar-supplied relation value called as either
`call(TerminalRelation, surface(Kind, Value, Surface))` or
`call(TerminalRelation, syntax(Kind, Syntax))`. The engine never switches on a
grammar name or a grammar-specific terminal kind. This callback-shaped seam is
an extraction boundary for the current flat spec vocabulary; it will become a
composable grammar-value/codec relation as the generic IR lands.
*/

%! transform_relation(+Grammar, +Given, +Wanted, +Observations, +Limits,
%!                    -Reply) is semidet.
%
% First executable grammar-value slice. Both directions consume the same
% immutable grammar value and request envelope; no parse/render operation is
% selected. Observations and limits already occupy their stable envelope slots
% while context staging and bounded solution enumeration move into this layer.

transform_relation(context_grammar, Given, Wanted, _EnvelopeObservations,
                   _Limits,
                   reply([solution(Bindings, 0)], [], DependencyKeys, [])) :-
    given_value(id, Given, Id),
    given_value(query, Given, Query),
    given_value(snapshot, Given, Snapshot),
    observe_query(Id, Query, Snapshot, Observation),
    dependency_key(Observation, DependencyKey),
    Available = [binding(observation, Observation)],
    requested_bindings(Wanted, Available, Bindings),
    DependencyKeys = [DependencyKey].
transform_relation(context_grammar, Given, Wanted, _EnvelopeObservations,
                   _Limits,
                   reply([solution(Bindings, 0)], [], [], [])) :-
    given_value(query, Given, Query),
    given_value(snapshot, Given, Snapshot),
    ( context_query(Query, Snapshot, Result)
    -> Outcome = some(Result)
    ;  Outcome = none
    ),
    Available = [binding(outcome, Outcome)],
    requested_bindings(Wanted, Available, Bindings).
transform_relation(local_state_grammar, Given, Wanted, Observations, _Limits,
                   reply(Solutions, ReadyQueries,
                         DependencyKeys, [])) :-
    given_value(steps, Given, Steps),
    given_value(initial_state, Given, Initial),
    run_state_steps(Steps, Initial, Final, Resolutions0, Queries, Delta, _),
    valid_query_graph(Queries),
    project_observations(Queries, Observations, StateObservations),
    stage_context(Queries, StateObservations, ReadyQueries, DependencyKeys),
    ( resolve_state_resolutions(Resolutions0, StateObservations, Resolutions)
    -> Available = [binding(final_state, Final),
                    binding(resolutions, Resolutions),
                    binding(delta, Delta)],
       requested_bindings(Wanted, Available, Bindings),
       Solutions = [solution(Bindings, 0)]
    ;  Solutions = []
    ).
transform_relation(symbolic_text_grammar, Given, Wanted, _Observations,
                   _Limits,
                   reply([solution(Bindings, 0)], [], [], [])) :-
    symbolic_constraints(Given, Constraints),
    given_value(final_state, Given, State),
    state_constraint_completion_pairs(Constraints, State, Pairs),
    project_completions(Pairs, Completions),
    Available = [binding(completions, Completions)],
    requested_bindings(Wanted, Available, Bindings).
transform_relation(given_grammar(Name), Given, Wanted, Observations, Limits,
                   Reply) :-
    atom(Name),
    given_value(Name, Given, GrammarReference),
    resolve_grammar(GrammarReference, Grammar),
    transform_relation(Grammar, Given, Wanted, Observations, Limits, Reply).

% A host-registered relation is invoked through the ordinary explicit context
% protocol.  The engine knows only its opaque handle and the standard relation
% envelope; it has no parser kind, command name, or client-specific callback.
% The host returns one immutable, revisioned `result(Reply)` entry.  Context
% queries in that reply are scoped into the surrounding graph.  Their exact
% observations are then included in the next pure adapter request, so replay
% and suspension never grant the host hidden access to a semantic provider.
transform_relation(registered_relation(Handle), Given, Wanted, Observations,
                   Limits, Reply) :-
    atom(Handle),
    Key = registered_relation(Handle),
    scoped_observations(Key, Observations, AdapterObservations),
    observation_dependency_keys(AdapterObservations, AdapterInputs),
    Request = relation_request(Given, Wanted, AdapterInputs, Limits),
    Id = registered_relation_call(Handle),
    Query = ask(one, registered_relation(Handle), where(Request)),
    Graph = [query(Id, Query)],
    project_observations(Graph, Observations, HostObservations),
    stage_context(Graph, HostObservations, Ready, HostDependencies),
    registered_relation_reply(Handle, Key, Id, Query, HostObservations, Ready,
                              HostDependencies, AdapterObservations, Wanted,
                              Reply).

transform_relation(ast_state_grammar(Rules), Given, Wanted, Observations,
                   Limits,
                   reply(Solutions, ReadyQueries,
                         DependencyKeys, Diagnostics)) :-
    given_value(ast, Given, Ast),
    given_value(source, Given, Source),
    initial_local_state(Given, Initial),
    derive_ast_state_steps(Rules, Ast, Source, Steps),
    run_state_steps(Steps, Initial, Final, Resolutions0, Queries, Delta,
                    LocalCompletionPairs, Applications),
    valid_query_graph(Queries),
    project_observations(Queries, Observations, StateObservations),
    stage_context(Queries, StateObservations, StateReadyQueries,
                  StateDependencyKeys),
    context_state_completion_pairs(Queries, StateObservations,
                                   ContextCompletionPairs),
    run_state_applications(Applications, Given, Observations, Limits,
                           ApplicationCompletionPairs, ApplicationQueries,
                           ApplicationDependencies, ApplicationDiagnostics),
    append(LocalCompletionPairs, ContextCompletionPairs, StateCompletionPairs),
    append(StateCompletionPairs, ApplicationCompletionPairs, CompletionPairs),
    project_completions(CompletionPairs, StateCompletions),
    ( resolve_state_resolutions(Resolutions0, StateObservations, Resolutions)
    -> Available = [binding(steps, Steps),
                    binding(final_state, Final),
                    binding(resolutions, Resolutions),
                    binding(delta, Delta),
                    binding(state_completions, StateCompletions)],
       requested_bindings(Wanted, Available, Bindings),
       Solutions = [solution(Bindings, 0)]
    ;  Solutions = []
    ),
    append(StateReadyQueries, ApplicationQueries, ReadyQueries0),
    sort(ReadyQueries0, ReadyQueries),
    append(StateDependencyKeys, ApplicationDependencies, DependencyKeys0),
    sort(DependencyKeys0, DependencyKeys),
    Diagnostics = ApplicationDiagnostics.
transform_relation(enrichment_grammar(Base, Shared, Extension, Outputs),
                   Given, Wanted, Observations, Limits, Reply) :-
    proper_atom_names(Shared),
    proper_atom_names(Outputs),
    all_unique_terms(Outputs),
    partition_names(Wanted, Outputs, ExtensionWanted, BaseWanted),
    ( ExtensionWanted = []
    -> transform_relation(Base, Given, Wanted, Observations, Limits, Reply)
    ;  append(BaseWanted, Shared, RequiredBase0),
       sort(RequiredBase0, RequiredBase),
       scoped_observations(enrichment_base, Observations, BaseObservations),
       transform_relation(Base, Given, RequiredBase, BaseObservations, Limits,
                          BaseReply0),
       scope_reply(enrichment_base, 0, BaseReply0, BaseReply),
       scoped_observations(enrichment_extension, Observations,
                           ExtensionObservations),
       enrich_reply(BaseReply, Shared, Extension, ExtensionWanted, Given,
                    Wanted, ExtensionObservations, Limits, Reply)
    ).
transform_relation(completion_union_grammar(Base, AdditionalName), Given,
                   Wanted, Observations, Limits, Reply) :-
    atom(AdditionalName),
    ( member_term(completions, Wanted)
    -> append(Wanted, [AdditionalName], InnerWanted0),
       sort(InnerWanted0, InnerWanted),
       transform_relation(Base, Given, InnerWanted, Observations, Limits,
                          InnerReply),
       union_completion_reply(InnerReply, AdditionalName, Wanted, Reply)
    ;  transform_relation(Base, Given, Wanted, Observations, Limits, Reply)
    ).
transform_relation(Grammar, Given, Wanted, Observations, Limits, Reply) :-
    Grammar = grammar(source(text(utf8)), _, _, _),
    transform_text_grammar(Grammar, Given, Wanted, Observations, Limits, Reply).

transform_relation(choice_grammar(Alternatives), Given, Wanted, Observations,
                   Limits,
                   reply(Solutions, Queries, DependencyKeys, Diagnostics)) :-
    Limits = limits(MaxSolutions, _MaxEvidence, _MaxOutputBytes),
    findall(Reply,
            choice_alternative_reply(Alternatives, Given, Wanted,
                                     Observations, Limits, Reply),
            Replies),
    Replies = [_|_],
    choice_replies(Replies, Solutions0, Queries0, DependencyKeys0,
                   Diagnostics0),
    choice_completion_projection(Solutions0, Solutions1),
    limit_solutions(Solutions1, MaxSolutions, Solutions, LimitDiagnostics),
    sort(Queries0, Queries),
    sort(DependencyKeys0, DependencyKeys),
    append(Diagnostics0, LimitDiagnostics, Diagnostics).

transform_relation(compose_grammar(Left, Shared, Right), Given, Wanted,
                   Observations, Limits, Reply) :-
    proper_atom_names(Shared),
    ( compose_direction(Left, left, Shared, Right, right, Given, Wanted,
                        Observations, Limits, Reply)
    -> true
    ;  compose_direction(Right, right, Shared, Left, left, Given, Wanted,
                         Observations, Limits, Reply)
    ).

transform_relation(binding_grammar(Names), Given, Wanted, _Observations,
                   _Limits,
                   reply([solution(Bindings, 0)], [], [], [])) :-
    proper_atom_names(Names),
    names_within(Wanted, Names),
    requested_bindings(Wanted, Given, Bindings).

transform_relation(projection_grammar(_Grammar, Projections), Given, Wanted,
                   _Observations, _Limits, Reply) :-
    standalone_projection_reply(Projections, Given, Wanted, Reply),
    !.
transform_relation(projection_grammar(Grammar, Projections), Given, Wanted,
                   Observations, Limits, Reply) :-
    projection_inner_request(Projections, Given, Wanted, InnerGiven,
                             InnerWanted),
    transform_relation(Grammar, InnerGiven, InnerWanted, Observations, Limits,
                       InnerReply),
    project_reply(Projections, Wanted, InnerReply, Reply).


transform_relation(context_grammar, Given, Wanted, Observations, _Limits,
                   reply([solution(Bindings, 0)], [], DependencyKeys, [])) :-
    given_value(graph, Given, Graph),
    project_observations(Graph, Observations, GraphObservations),
    ready_queries(Graph, GraphObservations, Ready),
    observation_dependency_keys(GraphObservations, DependencyKeys),
    Available = [binding(ready, Ready)],
    requested_bindings(Wanted, Available, Bindings).
transform_relation(context_grammar, _Given, Wanted, Observations, _Limits,
                   reply([solution(Bindings, 0)], [], DependencyKeys, [])) :-
    Observations = [_|_],
    observation_dependency_keys(Observations, DependencyKeys),
    Available = [binding(dependency_keys, DependencyKeys)],
    requested_bindings(Wanted, Available, Bindings).

transform_relation(
    sequence_grammar(Specs, terminals(Terminals), separator(Separator),
                     contexts(Contexts)),
    Given, Wanted, Observations,
    limits(MaxSolutions, MaxEvidence, _MaxOutputBytes),
    reply(Solutions, Queries, DependencyKeys, Diagnostics)) :-
    given_value(source, Given, source(Items, Mode)),
    neutral_input(Items, Body),
    valid_relation_mode(Mode),
    text_value(Separator),
    SearchLimit is MaxSolutions + 1,
    findnsols(SearchLimit, Candidate,
              sequence_candidate(Specs, Terminals, Body, Mode, MaxEvidence,
                                 Candidate),
              Candidates0),
    Candidates0 = [_|_],
    limit_solutions(Candidates0, MaxSolutions, Candidates, Diagnostics),
    candidate_context_queries(Contexts, Mode, Candidates, Queries),
    valid_query_graph(Queries),
    project_observations(Queries, Observations, CandidateObservations),
    candidate_completions(Mode, Contexts, Candidates, Queries,
                          CandidateObservations,
                          Completions),
    candidate_solutions(Candidates, Specs, Contexts, Queries,
                        CandidateObservations, Wanted, Completions, Solutions),
    observation_dependency_keys(CandidateObservations, DependencyKeys).
transform_relation(
    sequence_grammar(Specs, terminals(Terminals), separator(Separator),
                     contexts(_Contexts)),
    Given, Wanted, _Observations, _Limits,
    reply([solution(Bindings, 0)], [], [], [])) :-
    given_value(arguments, Given, Arguments),
    relate_sequence(Specs, RenderedItems, render,
                    grammar_engine:data_terminal(Terminals),
                    Arguments, _Evidence, 0),
    rendered_surfaces(RenderedItems, Surfaces),
    join_surfaces(Surfaces, Separator, Text),
    Available = [binding(source, Text),
                 binding(rendered_items, RenderedItems)],
    requested_bindings(Wanted, Available, Bindings).
transform_relation(
    sequence_grammar(_Specs, terminals(_Terminals), separator(_Separator),
                     contexts(_Contexts)),
    Given, Wanted, _Observations, _Limits,
    reply([solution(Bindings, 0)], [], [], [])) :-
    given_value(evidence, Given, Evidence),
    project_highlights(Evidence, Highlights),
    Available = [binding(highlights, Highlights)],
    requested_bindings(Wanted, Available, Bindings).

registered_relation_reply(_, Key, _, _, [], Ready, HostDependencies,
                          AdapterObservations, _,
                          reply([], Ready, Dependencies, [])) :-
    observation_dependency_keys(AdapterObservations, AdapterDependencies0),
    scope_dependencies(Key, AdapterDependencies0, AdapterDependencies),
    append(HostDependencies, AdapterDependencies, Dependencies0),
    sort(Dependencies0, Dependencies),
    !.
registered_relation_reply(
    Handle, Key, Id, Query,
    [observed(Id, Query, _,
              some(one(entry(_, _, _, result(AdapterReply), _))))],
    [], HostDependencies, AdapterObservations, Wanted, Reply) :-
    AdapterReply = reply(Solutions, InnerGraph, [], Diagnostics),
    valid_registered_solutions(Solutions, Wanted),
    proper_ground_list(Diagnostics),
    valid_query_graph(InnerGraph),
    project_observations(InnerGraph, AdapterObservations,
                         InnerObservations),
    stage_context(InnerGraph, InnerObservations, InnerReady,
                  _CurrentDependencies),
    observation_dependency_keys(AdapterObservations,
                                AdapterDependencies0),
    scope_dependencies(Key, AdapterDependencies0, AdapterDependencies),
    append(HostDependencies, AdapterDependencies, Dependencies0),
    sort(Dependencies0, Dependencies),
    registered_relation_inner_reply(Handle, Key, Solutions, InnerGraph,
                                    InnerReady, Dependencies, Diagnostics,
                                    Reply),
    !.
registered_relation_reply(Handle, _, _, _, [_], [], Dependencies, _, _,
                          reply([], [], Dependencies,
                                [diagnostic(registered_relation_unavailable(
                                    Handle))])).

registered_relation_inner_reply(_, _, Solutions, [], [], Dependencies,
                                Diagnostics,
                                reply(Solutions, [], Dependencies,
                                      Diagnostics)) :- !.
registered_relation_inner_reply(_, Key, Solutions, [_|_], InnerReady,
                                Dependencies, Diagnostics,
                                reply(Solutions, Ready, Dependencies,
                                      Diagnostics)) :-
    InnerReady = [_|_],
    scope_queries(Key, InnerReady, Ready),
    !.
registered_relation_inner_reply(Handle, _, _, [_|_], [], Dependencies,
                                Diagnostics,
                                reply([], [], Dependencies, AllDiagnostics)) :-
    append(Diagnostics,
           [diagnostic(registered_relation_stalled(Handle))],
           AllDiagnostics).

valid_registered_solutions([], _).
valid_registered_solutions([solution(Bindings, Preference)|Solutions], Wanted) :-
    integer(Preference),
    ground(Bindings),
    requested_bindings(Wanted, Bindings, Bindings),
    valid_registered_solutions(Solutions, Wanted).

proper_ground_list([]).
proper_ground_list([Value|Values]) :-
    ground(Value),
    proper_ground_list(Values).

symbolic_constraints(Given, Constraints) :-
    given_value(constraints, Given, Constraints), !.
symbolic_constraints(Given, Constraints) :-
    given_value(steps, Given, Steps),
    state_step_constraints(Steps, Constraints).

initial_local_state(Given, Initial) :-
    given_value(initial_state, Given, Initial), !.
initial_local_state(_, Initial) :-
    empty_local_state(Initial).

% State applications are ordinary nested relation requests captured by the
% AST/state walk at the exact point where they occur.  The generic engine knows
% neither the node name nor the supplied grammar's language.  Failure to match
% one annotation grammar contributes no projection; diagnostics from an
% executable but unsupported nested grammar remain visible.
run_state_applications([], _, _, _, [], [], [], []).
run_state_applications([Application|Applications], Given, Observations, Limits,
                       CompletionPairs, Queries, Dependencies, Diagnostics) :-
    state_application_reply(Application, Given, Observations, Limits, Reply),
    reply_completion_pairs(Reply, FirstPairs),
    Reply = reply(_, FirstQueries, FirstDependencies, FirstDiagnostics),
    run_state_applications(Applications, Given, Observations, Limits,
                           RestPairs, RestQueries, RestDependencies,
                           RestDiagnostics),
    append(FirstPairs, RestPairs, CompletionPairs),
    append(FirstQueries, RestQueries, Queries0),
    sort(Queries0, Queries),
    append(FirstDependencies, RestDependencies, Dependencies0),
    sort(Dependencies0, Dependencies),
    append(FirstDiagnostics, RestDiagnostics, Diagnostics).

state_application_reply(
    application(Id, Grammar, ApplicationTemplates, State), Given, Observations,
    Limits,
    Reply) :-
    Key = state_application(Id),
    application_binding_names(ApplicationTemplates, ApplicationNames),
    proper_atom_names(ApplicationNames),
    all_unique_terms(ApplicationNames),
    materialize_application_bindings(ApplicationTemplates, State, Key,
                                     ApplicationBindings),
    bindings_without_names(Given, ApplicationNames, InheritedGiven),
    merge_bindings(InheritedGiven, ApplicationBindings, ApplicationGiven),
    scoped_observations(Key, Observations, ApplicationObservations),
    ( transform_relation(Grammar, ApplicationGiven, [completions],
                         ApplicationObservations, Limits, InnerReply)
    -> scope_reply(Key, 0, InnerReply, Reply)
    ;  Reply = reply([], [], [], [])
    ),
    !.
state_application_reply(_, _, _, _, reply([], [], [], [])).

application_binding_names([], []).
application_binding_names([binding(Name, _)|Bindings], [Name|Names]) :-
    application_binding_names(Bindings, Names).

materialize_application_bindings([], _, _, []).
materialize_application_bindings(
    [binding(Name, state_text_source(Expression))|Bindings], State, Origin,
    [binding(Name, Source)|Materialized]) :-
    atom(Name),
    symbolic_text_source(Expression, State, Origin, Source),
    materialize_application_bindings(Bindings, State, Origin, Materialized).

materialize_application_bindings(
    [binding(Name, Value)|Bindings], State, Origin,
    [binding(Name, Value)|Materialized]) :-
    atom(Name), ground(Value),
    materialize_application_bindings(Bindings, State, Origin, Materialized).

bindings_without_names([], _, []).
bindings_without_names([binding(Name, _)|Bindings], Names, Rest) :-
    member_term(Name, Names),
    !,
    bindings_without_names(Bindings, Names, Rest).
bindings_without_names([Binding|Bindings], Names, [Binding|Rest]) :-
    bindings_without_names(Bindings, Names, Rest).

reply_completion_pairs(reply(Solutions, _, _, _), Pairs) :-
    findall(Pair,
            ( candidate_member(solution(Bindings, _), Solutions),
              given_value(completions, Bindings, Completions),
              completion_pairs(Completions, CompletionPairs),
              candidate_member(Pair, CompletionPairs)
            ),
            Pairs).

union_completion_reply(
    reply(Solutions0, Queries, Dependencies, Diagnostics), AdditionalName,
    Wanted, reply(Solutions, Queries, Dependencies, Diagnostics)) :-
    union_completion_solutions(Solutions0, AdditionalName, Wanted, Merged),
    sort(Merged, Solutions).

union_completion_solutions([], _, _, []).
union_completion_solutions(
    [solution(Bindings0, Preference)|Solutions0], AdditionalName, Wanted,
    [solution(Bindings, Preference)|Solutions]) :-
    given_value(completions, Bindings0, BaseCompletions),
    given_value(AdditionalName, Bindings0, AdditionalCompletions),
    completion_pairs(BaseCompletions, BasePairs),
    completion_pairs(AdditionalCompletions, AdditionalPairs),
    append(BasePairs, AdditionalPairs, Pairs),
    project_completions(Pairs, Completions),
    replace_named_binding(completions, Completions, Bindings0, Available),
    requested_bindings(Wanted, Available, Bindings),
    union_completion_solutions(Solutions0, AdditionalName, Wanted, Solutions).

replace_named_binding(Name, Value, [binding(Name, _)|Bindings],
                      [binding(Name, Value)|Bindings]) :- !.
replace_named_binding(Name, Value, [Binding|Bindings], [Binding|Replaced]) :-
    replace_named_binding(Name, Value, Bindings, Replaced).

proper_atom_names([]).
proper_atom_names([Name|Names]) :-
    atom(Name),
    proper_atom_names(Names).

all_unique_terms(Values) :-
    sort(Values, Unique),
    length(Values, Count),
    length(Unique, Count).

partition_names([], _, [], []).
partition_names([Name|Names], Selected, [Name|Chosen], Rest) :-
    member_term(Name, Selected), !,
    partition_names(Names, Selected, Chosen, Rest).
partition_names([Name|Names], Selected, Chosen, [Name|Rest]) :-
    partition_names(Names, Selected, Chosen, Rest).

enrich_reply(reply(BaseSolutions, BaseQueries, BaseDependencies,
                   BaseDiagnostics),
             Shared, Extension, ExtensionWanted, Given, Wanted, Observations,
             Limits, reply(Solutions, Queries, Dependencies, Diagnostics)) :-
    BaseSolutions = [_|_],
    findall(enriched(BaseSolution, ExtensionReply),
            ( candidate_member(BaseSolution, BaseSolutions),
              BaseSolution = solution(BaseBindings, _),
              select_named_bindings(Shared, BaseBindings, SharedBindings),
              merge_bindings(Given, SharedBindings, ExtensionGiven),
              transform_relation(Extension, ExtensionGiven, ExtensionWanted,
                                 Observations, Limits, ExtensionReply0),
              scope_reply(enrichment_extension, 0, ExtensionReply0,
                          ExtensionReply)
            ),
            Enriched),
    Enriched = [_|_],
    flatten_enriched(Enriched, Wanted, Solutions0, ExtensionQueries,
                     ExtensionDependencies, ExtensionDiagnostics),
    Limits = limits(MaxSolutions, _, _),
    limit_solutions(Solutions0, MaxSolutions, Solutions, LimitDiagnostics),
    append(BaseQueries, ExtensionQueries, Queries0),
    append(BaseDependencies, ExtensionDependencies, Dependencies0),
    append(BaseDiagnostics, ExtensionDiagnostics, Diagnostics0),
    append(Diagnostics0, LimitDiagnostics, Diagnostics),
    sort(Queries0, Queries),
    sort(Dependencies0, Dependencies).

select_named_bindings([], _, []).
select_named_bindings([Name|Names], Bindings,
                      [binding(Name, Value)|Selected]) :-
    given_value(Name, Bindings, Value),
    select_named_bindings(Names, Bindings, Selected).

flatten_enriched([], _, [], [], [], []).
flatten_enriched([
    enriched(solution(BaseBindings, BasePreference),
             reply(ExtensionSolutions, Queries0, Dependencies0, Diagnostics0))
    |Enriched], Wanted, Solutions, Queries, Dependencies, Diagnostics) :-
    join_extension_solutions(ExtensionSolutions, BaseBindings, BasePreference,
                             Wanted, Joined),
    flatten_enriched(Enriched, Wanted, RestSolutions, RestQueries,
                     RestDependencies, RestDiagnostics),
    append(Joined, RestSolutions, Solutions),
    append(Queries0, RestQueries, Queries),
    append(Dependencies0, RestDependencies, Dependencies),
    append(Diagnostics0, RestDiagnostics, Diagnostics).

join_extension_solutions([], _, _, _, []).
join_extension_solutions(
    [solution(ExtensionBindings, ExtensionPreference)|ExtensionSolutions],
    BaseBindings, BasePreference, Wanted,
    [solution(Bindings, Preference)|Solutions]) :-
    merge_bindings(BaseBindings, ExtensionBindings, Available),
    requested_bindings(Wanted, Available, Bindings),
    Preference is BasePreference + ExtensionPreference,
    join_extension_solutions(ExtensionSolutions, BaseBindings, BasePreference,
                             Wanted, Solutions).

names_within([], _).
names_within([Name|Rest], Names) :-
    member_term(Name, Names),
    names_within(Rest, Names).

compose_direction(First, FirstKey, Shared, Second, SecondKey, Given, Wanted,
                  Observations, Limits,
                  reply(Solutions, Queries, Dependencies, Diagnostics)) :-
    scoped_observations(FirstKey, Observations, FirstObservations),
    transform_relation(First, Given, Shared, FirstObservations, Limits,
                       FirstReply),
    FirstReply = reply(FirstSolutions, FirstQueries0, FirstDependencies0,
                       FirstDiagnostics),
    FirstSolutions = [_|_],
    scoped_observations(SecondKey, Observations, SecondObservations),
    findall(joined(FirstPreference, SecondReply),
            ( candidate_member(solution(SharedBindings, FirstPreference),
                               FirstSolutions),
              merge_bindings(Given, SharedBindings, SecondGiven),
              transform_relation(Second, SecondGiven, Wanted,
                                 SecondObservations, Limits, SecondReply)
            ),
            Joined),
    Joined = [_|_],
    compose_joined(Joined, SecondKey, Solutions0, SecondQueries,
                   SecondDependencies, SecondDiagnostics),
    Limits = limits(MaxSolutions, _, _),
    limit_solutions(Solutions0, MaxSolutions, Solutions, LimitDiagnostics),
    scope_queries(FirstKey, FirstQueries0, FirstQueries),
    scope_dependencies(FirstKey, FirstDependencies0, FirstDependencies),
    append(FirstQueries, SecondQueries, Queries0),
    sort(Queries0, Queries),
    append(FirstDependencies, SecondDependencies, Dependencies0),
    sort(Dependencies0, Dependencies),
    append(FirstDiagnostics, SecondDiagnostics, Diagnostics0),
    append(Diagnostics0, LimitDiagnostics, Diagnostics).

merge_bindings(Bindings, [], Bindings).
merge_bindings(Bindings0, [binding(Name, Value)|Bindings], Merged) :-
    put_binding(Name, Value, Bindings0, Bindings1),
    merge_bindings(Bindings1, Bindings, Merged).

compose_joined([], _, [], [], [], []).
compose_joined([joined(FirstPreference,
                       reply(SecondSolutions0, Queries0, Dependencies0,
                             Diagnostics0))|Joined],
               SecondKey, Solutions, Queries, Dependencies, Diagnostics) :-
    add_solution_preference(SecondSolutions0, FirstPreference,
                            SecondSolutions),
    scope_queries(SecondKey, Queries0, ScopedQueries),
    scope_dependencies(SecondKey, Dependencies0, ScopedDependencies),
    compose_joined(Joined, SecondKey, RestSolutions, RestQueries,
                   RestDependencies, RestDiagnostics),
    append(SecondSolutions, RestSolutions, Solutions),
    append(ScopedQueries, RestQueries, Queries),
    append(ScopedDependencies, RestDependencies, Dependencies),
    append(Diagnostics0, RestDiagnostics, Diagnostics).

add_solution_preference([], _, []).
add_solution_preference([solution(Bindings, Preference0)|Solutions], Added,
                        [solution(Bindings, Preference)|Adjusted]) :-
    Preference is Preference0 + Added,
    add_solution_preference(Solutions, Added, Adjusted).

% Constant metadata is a representation in its own right: asking for it must
% not manufacture a dummy source merely to enter the inner sequence grammar.
% A standalone request is allowed only when every wanted projection has no
% references; groundness alone would let a relational list template invent a
% shorter value before the inner grammar had supplied its binding.
standalone_projection_reply(Projections, Given, Wanted,
                            reply([solution(Bindings, 0)], [], [], [])) :-
    projection_wanted_references(Projections, Wanted, []),
    wanted_without_projections(Wanted, Projections, []),
    inverse_project_given(Projections, Given, Prepared),
    projected_available(Projections, Wanted, Prepared, Available),
    requested_bindings(Wanted, Available, Bindings),
    ground(Bindings).

% A choice is grammar composition, not an operation switch. Alternative keys
% are immutable grammar data used to namespace context-query identities; a
% context observation can therefore be routed back through the same nested
% grammar without a client knowing which branch produced it.
choice_alternative_reply(
    [alternative(Key, Preference, Grammar)|_], Given, Wanted, Observations,
    Limits, Reply) :-
    choice_alternative_applicable(Grammar, Given),
    scoped_observations(Key, Observations, InnerObservations),
    transform_relation(Grammar, Given, Wanted, InnerObservations, Limits,
                       InnerReply),
    scope_reply(Key, Preference, InnerReply, Reply).
choice_alternative_reply([_|Alternatives], Given, Wanted, Observations,
                         Limits, Reply) :-
    choice_alternative_reply(Alternatives, Given, Wanted, Observations,
                             Limits, Reply).

% Reject a choice cheaply when both the source and the branch expose a
% discriminating first literal.  This is an engine optimization over generic
% grammar data, not an action-language index: unknown grammar shapes, empty
% sources, and non-literal prefixes conservatively retain the branch.  The
% comparison deliberately has the same exact/prefix behavior as match_literal
% so pruning cannot change parse or completion results.
choice_alternative_applicable(Grammar, Given) :-
    ( given_value(source, Given, source([Item|_], Mode)),
      grammar_initial_literal(Grammar, Text)
    -> initial_literal_may_match(Item, Mode, Text)
    ;  true
    ).

grammar_initial_literal(
    sequence_grammar([literal(_, Text, _, _, _)|_], _, _, _), Text).
grammar_initial_literal(projection_grammar(Grammar, _), Text) :-
    grammar_initial_literal(Grammar, Text).

initial_literal_may_match(
    unit(_, _, _, Surface, _, _, _, _), Mode, Text) :-
    source_mode(Mode),
    text_string(Surface, SurfaceString),
    SurfaceString = Text.
initial_literal_may_match(edit_tear(EditId, _, Surface), assist(EditId), Text) :-
    surface_prefix(Surface, Text).
initial_literal_may_match(source_tear(_, _, _), _, _).

scoped_observations(_, [], []).
scoped_observations(Key,
                    [observed(branch(Key, Id), Query, Source, Outcome)|Rest],
                    [observed(Id, InnerQuery, Source, Outcome)|Inner]) :-
    !,
    unscope_term_refs(Query, Key, InnerQuery),
    scoped_observations(Key, Rest, Inner).
scoped_observations(Key, [_|Rest], Inner) :-
    scoped_observations(Key, Rest, Inner).

scope_reply(Key, AlternativePreference,
            reply(Solutions0, Queries0, Dependencies0, Diagnostics),
            reply(Solutions, Queries, Dependencies, Diagnostics)) :-
    scope_solutions(Key, Solutions0, AlternativePreference, Solutions),
    scope_queries(Key, Queries0, Queries),
    scope_dependencies(Key, Dependencies0, Dependencies).

scope_solutions(_, [], _, []).
scope_solutions(Key, [solution(Bindings0, Preference0)|Rest], AlternativePreference,
                [solution(Bindings, Preference)|Solutions]) :-
    Preference is Preference0 + AlternativePreference,
    scope_binding_preferences(Key, Bindings0, AlternativePreference, Bindings),
    scope_solutions(Key, Rest, AlternativePreference, Solutions).

scope_binding_preferences(_, [], _, []).
scope_binding_preferences(Key,
                          [binding(completions, Completions0)|Bindings], Added,
                          [binding(completions, Completions)|Scoped]) :-
    !,
    scope_completions(Key, Completions0, Added, Completions),
    scope_binding_preferences(Key, Bindings, Added, Scoped).
scope_binding_preferences(Key, [Binding|Bindings], Added, [Binding|Scoped]) :-
    scope_binding_preferences(Key, Bindings, Added, Scoped).

scope_completions(_, [], _, []).
scope_completions(Key,
    [completion(Span, Text, Alternatives0, Preference0, Rank)|Completions],
    Added,
    [completion(Span, Text, Alternatives, Preference, Rank)|Scoped]) :-
    Preference is Preference0 + Added,
    scope_completion_alternatives(Key, Alternatives0, Added, Alternatives),
    scope_completions(Key, Completions, Added, Scoped).

scope_completion_alternatives(_, [], _, []).
scope_completion_alternatives(Key,
    [alternative(context(Domain, Identity), Syntax, Description,
                 Preference0)|Alternatives], Added,
    [alternative(context(Key, Domain, Identity), Syntax, Description,
                 Preference)|Scoped]) :-
    !,
    Preference is Preference0 + Added,
    scope_completion_alternatives(Key, Alternatives, Added, Scoped).
scope_completion_alternatives(Key,
    [alternative(Semantic, Syntax, Description, Preference0)|Alternatives],
    Added,
    [alternative(Semantic, Syntax, Description, Preference)|Scoped]) :-
    Preference is Preference0 + Added,
    scope_completion_alternatives(Key, Alternatives, Added, Scoped).

scope_queries(_, [], []).
scope_queries(Key, [query(Id, Query)|Rest],
              [query(branch(Key, Id), ScopedQuery)|Queries]) :-
    scope_term_refs(Query, Key, ScopedQuery),
    scope_queries(Key, Rest, Queries).

scope_dependencies(_, [], []).
scope_dependencies(Key, [dependency(Id, Query, Outcome)|Rest],
                   [dependency(branch(Key, Id), ScopedQuery, Outcome)|Dependencies]) :-
    scope_term_refs(Query, Key, ScopedQuery),
    scope_dependencies(Key, Rest, Dependencies).

scope_term_refs(ref(Id), Key, ref(branch(Key, Id))) :- !.
scope_term_refs(Term, _, Term) :- atomic(Term), !.
scope_term_refs(Term, Key, Scoped) :-
    Term =.. [Functor|Arguments],
    scope_term_ref_list(Arguments, Key, ScopedArguments),
    Scoped =.. [Functor|ScopedArguments].

scope_term_ref_list([], _, []).
scope_term_ref_list([Argument|Arguments], Key, [Scoped|ScopedArguments]) :-
    scope_term_refs(Argument, Key, Scoped),
    scope_term_ref_list(Arguments, Key, ScopedArguments).

unscope_term_refs(ref(branch(Key, Id)), Key, ref(Id)) :- !.
unscope_term_refs(Term, _, Term) :- atomic(Term), !.
unscope_term_refs(Term, Key, Unscoped) :-
    Term =.. [Functor|Arguments],
    unscope_term_ref_list(Arguments, Key, UnscopedArguments),
    Unscoped =.. [Functor|UnscopedArguments].

unscope_term_ref_list([], _, []).
unscope_term_ref_list([Argument|Arguments], Key,
                      [Unscoped|UnscopedArguments]) :-
    unscope_term_refs(Argument, Key, Unscoped),
    unscope_term_ref_list(Arguments, Key, UnscopedArguments).

choice_replies([], [], [], [], []).
choice_replies([reply(Solutions0, Queries0, Dependencies0, Diagnostics0)|Rest],
               Solutions, Queries, Dependencies, Diagnostics) :-
    choice_replies(Rest, Solutions1, Queries1, Dependencies1, Diagnostics1),
    append(Solutions0, Solutions1, Solutions),
    append(Queries0, Queries1, Queries),
    append(Dependencies0, Dependencies1, Dependencies),
    append(Diagnostics0, Diagnostics1, Diagnostics).

choice_completion_projection(Solutions0, Solutions) :-
    findall(Pair, solution_completion_pair(Solutions0, Pair), Pairs),
    ( Pairs = []
    -> Solutions = Solutions0
    ;  project_completions(Pairs, Completions),
       replace_solution_completions(Solutions0, Completions, Projected),
       sort(Projected, Solutions)
    ).

solution_completion_pair(Solutions, Pair) :-
    candidate_member(solution(Bindings, _), Solutions),
    given_value(completions, Bindings, Completions),
    candidate_member(
        completion(Span, Text, Alternatives, _Preference, _Rank),
        Completions),
    candidate_member(
        alternative(Semantic, Syntax, Description, AlternativePreference),
        Alternatives),
    Pair = completion_key(Span, Text)-
               (alternative(Semantic, Syntax, Description)-
                AlternativePreference).

replace_solution_completions([], _, []).
replace_solution_completions([solution(Bindings0, Preference)|Solutions0],
                             Completions,
                             [solution(Bindings, Preference)|Solutions]) :-
    replace_completion_binding(Bindings0, Completions, Bindings),
    replace_solution_completions(Solutions0, Completions, Solutions).

replace_completion_binding([], _, []).
replace_completion_binding([binding(completions, _)|Bindings], Completions,
                           [binding(completions, Completions)|Replaced]) :-
    !,
    replace_completion_binding(Bindings, Completions, Replaced).
replace_completion_binding([Binding|Bindings], Completions,
                           [Binding|Replaced]) :-
    replace_completion_binding(Bindings, Completions, Replaced).

% A projection template is a small pure relation over generic bindings:
%
%   constant(Value)                  immutable grammar data
%   reference(Name)                  another representation binding
%   structure(Functor, Arguments)    a compound semantic value
%   sequence(Items)                  a list semantic value
%   concatenate(Left, Right)         relational list concatenation
%   substring_any(Haystacks)         a given text occurs in one text value
%
% This is deliberately data rather than a grammar-supplied predicate. The
% engine evaluates the same template forward after parsing and backward before
% rendering.
projection_inner_request(Projections, Given, Wanted, InnerGiven,
                         InnerWanted) :-
    inverse_project_given(Projections, Given, InnerGiven0),
    projection_wanted_references(Projections, Wanted, References),
    wanted_without_projections(Wanted, Projections, PassthroughWanted),
    append(PassthroughWanted, References, InnerWanted0),
    sort(InnerWanted0, InnerWanted),
    bindings_without_projections(InnerGiven0, Projections, InnerGiven).

inverse_project_given([], Given, Given).
inverse_project_given([projection(Name, Template)|Projections], Given0,
                      Given) :-
    ( given_value(Name, Given0, Value)
    -> template_value(Template, Value, Given0, Given1)
    ;  Given1 = Given0
    ),
    inverse_project_given(Projections, Given1, Given).

bindings_without_projections([], _, []).
bindings_without_projections([binding(Name, _)|Bindings], Projections, Rest) :-
    projection_named(Name, Projections),
    !,
    bindings_without_projections(Bindings, Projections, Rest).
bindings_without_projections([Binding|Bindings], Projections,
                             [Binding|Rest]) :-
    bindings_without_projections(Bindings, Projections, Rest).

projection_wanted_references([], _, []).
projection_wanted_references([projection(Name, Template)|Projections], Wanted,
                             References) :-
    projection_wanted_references(Projections, Wanted, Rest),
    ( member_term(Name, Wanted)
    -> template_references(Template, Names), append(Names, Rest, References)
    ;  References = Rest
    ).

wanted_without_projections([], _, []).
wanted_without_projections([Name|Names], Projections, Rest) :-
    projection_named(Name, Projections),
    !,
    wanted_without_projections(Names, Projections, Rest).
wanted_without_projections([Name|Names], Projections, [Name|Rest]) :-
    wanted_without_projections(Names, Projections, Rest).

projection_named(Name, [projection(Name, _)|_]).
projection_named(Name, [_|Projections]) :- projection_named(Name, Projections).

project_reply(Projections, Wanted,
              reply(Solutions0, Queries, Dependencies, Diagnostics),
              reply(Solutions, Queries, Dependencies, Diagnostics)) :-
    project_solutions(Solutions0, Projections, Wanted, Solutions).

project_solutions([], _, _, []).
project_solutions([solution(InnerBindings, Preference)|Rest], Projections,
                  Wanted, [solution(Bindings, Preference)|Solutions]) :-
    projected_available(Projections, Wanted, InnerBindings, Available),
    requested_bindings(Wanted, Available, Bindings),
    project_solutions(Rest, Projections, Wanted, Solutions).

projected_available([], _, Bindings, Bindings).
projected_available([projection(Name, Template)|Projections], Wanted,
                    Bindings0, Available) :-
    ( member_term(Name, Wanted)
    -> template_value(Template, Value, Bindings0, Bindings1),
       put_binding(Name, Value, Bindings1, Bindings2)
    ;  Bindings2 = Bindings0
    ),
    projected_available(Projections, Wanted, Bindings2, Available).

template_value(constant(Value), Value, Bindings, Bindings).
template_value(reference(Name), Value, Bindings0, Bindings) :-
    put_binding(Name, Value, Bindings0, Bindings).
template_value(structure(Functor, Templates), Value, Bindings0, Bindings) :-
    atom(Functor),
    ( ground(Value)
    -> Value =.. [Functor|Values],
       template_values(Templates, Values, Bindings0, Bindings)
    ;  template_values(Templates, Values, Bindings0, Bindings),
       Value =.. [Functor|Values]
    ).
template_value(sequence(Templates), Values, Bindings0, Bindings) :-
    template_values(Templates, Values, Bindings0, Bindings).
template_value(concatenate(Left, Right), Values, Bindings0, Bindings) :-
    template_value(Left, LeftValues, Bindings0, Bindings1),
    template_value(Right, RightValues, Bindings1, Bindings),
    append(LeftValues, RightValues, Values).
template_value(substring_any(Templates), Needle, Bindings0, Bindings) :-
    string(Needle),
    template_values(Templates, Haystacks, Bindings0, Bindings),
    substring_member(Needle, Haystacks).

substring_member(Needle, [Haystack|_]) :-
    string(Haystack),
    sub_string(Haystack, _, _, _, Needle),
    !.
substring_member(Needle, [_|Haystacks]) :-
    substring_member(Needle, Haystacks).

template_values([], [], Bindings, Bindings).
template_values([Template|Templates], [Value|Values], Bindings0, Bindings) :-
    template_value(Template, Value, Bindings0, Bindings1),
    template_values(Templates, Values, Bindings1, Bindings).

put_binding(Name, Value, Bindings, Bindings) :-
    given_value(Name, Bindings, Existing),
    !,
    Existing = Value.
put_binding(Name, Value, Bindings, [binding(Name, Value)|Bindings]).

template_references(constant(_), []).
template_references(reference(Name), [Name]).
template_references(structure(_, Templates), Names) :-
    templates_references(Templates, Names).
template_references(sequence(Templates), Names) :-
    templates_references(Templates, Names).
template_references(concatenate(Left, Right), Names) :-
    template_references(Left, LeftNames),
    template_references(Right, RightNames),
    append(LeftNames, RightNames, Names).
template_references(substring_any(Templates), Names) :-
    templates_references(Templates, Names).

templates_references([], []).
templates_references([Template|Templates], Names) :-
    template_references(Template, Names0),
    templates_references(Templates, Names1),
    append(Names0, Names1, Names).

member_term(Value, [Head|_]) :- Value == Head, !.
member_term(Value, [_|Values]) :- member_term(Value, Values).

sequence_candidate(Specs, Terminals, Body, Mode, MaxEvidence,
                   candidate(Arguments, Evidence, Status, Preference,
                             Highlights)) :-
    relate_sequence(Specs, Body, Mode,
                    grammar_engine:data_terminal(Terminals),
                    Arguments, Evidence, EditCount),
    length(Evidence, EvidenceCount),
    EvidenceCount =< MaxEvidence,
    relation_status(Mode, EditCount, Status),
    evidence_preference(Evidence, 0, Preference),
    project_highlights(Evidence, Highlights).

limit_solutions(Candidates0, Maximum, Candidates,
                [diagnostic(solution_limit(Maximum))]) :-
    take_prefix(Maximum, Candidates0, Candidates, Rest),
    Rest = [_|_],
    !.
limit_solutions(Candidates, _, Candidates, []).

take_prefix(0, Values, [], Values) :- !.
take_prefix(_, [], [], []).
take_prefix(Count, [Value|Values], [Value|Prefix], Rest) :-
    Count > 0,
    Next is Count - 1,
    take_prefix(Next, Values, Prefix, Rest).

candidate_completions(exact, _, _, _, _, []).
candidate_completions(assist(EditId), Contexts, Candidates, Queries,
                      Observations, Completions) :-
    findall(completion_key(Span, Text)-
                (alternative(Semantic, Syntax, Description)-Preference),
            ( candidate_member(
                  candidate(_, Evidence, _, Preference, _), Candidates),
              literal_completion_evidence(
                  EditId, Evidence, Span, Text, Semantic, Syntax, Description)
            ),
            LiteralPairs),
    findall(completion_key(Span, Text)-
                (alternative(context(Domain, Identity), context_argument,
                             Provider)-Preference),
            context_completion_pair(
                EditId, Contexts, Candidates, Queries, Observations,
                Span, Text, Domain, Identity, Provider, Preference),
            ContextPairs),
    append(LiteralPairs, ContextPairs, Pairs),
    project_completions(Pairs, Completions).

candidate_context_queries(Contexts, exact, Candidates, Queries) :-
    findall(Query,
            ( candidate_member(candidate(_, Evidence, _, _, _), Candidates),
              evidence_context_query(Contexts, Evidence, exact, 1, Query)
            ),
            Queries0),
    sort(Queries0, Queries).
candidate_context_queries(Contexts, assist(EditId), Candidates, Queries) :-
    findall(Query,
            ( candidate_member(candidate(_, Evidence, _, _, _), Candidates),
              evidence_context_query(
                  Contexts, Evidence, assist(EditId), 1, Query)
            ),
            Queries0),
    sort(Queries0, Queries).

% Exact and assist queries share stable q(N) identifiers determined solely by
% contextual evidence order. Non-contextual literals do not perturb them.
evidence_context_query(Contexts, Evidence, Mode, Index, Query) :-
    evidence_context_query_(Contexts, Evidence, Mode, Index, _Next, [],
                            _Known, Query).

evidence_context_query_(Contexts, [Item|_], Mode, Index, Next, Known,
                        NextKnown, Query) :-
    contextual_evidence(Contexts, Item, Mode, Index, Known, Query, Name),
    NextKnown = [Name-q(Index)|Known],
    Next is Index + 1.
evidence_context_query_(Contexts, [Item|Items], Mode, Index, Next, Known,
                        NextKnown, Query) :-
    ( contextual_evidence(Contexts, Item, Mode, Index, Known, _, Name)
    -> Following is Index + 1,
       FollowingKnown = [Name-q(Index)|Known]
    ;  Following = Index,
       FollowingKnown = Known
    ),
    evidence_context_query_(Contexts, Items, Mode, Following, Next,
                            FollowingKnown, NextKnown, Query).

contextual_evidence(Contexts,
                    evidence(_, _, _, Surface, _, Name, _, Origin),
                    Mode, Index, Known, query(q(Index), Ask), Name) :-
    context_descriptor(Name, Cardinality, Domain, Scope, Contexts),
    context_ask(Mode, Origin, Cardinality, Domain, Scope, Known, Surface, Ask).

context_ask(exact, _, Cardinality, Domain, Scope, Known, Surface,
            ask(Cardinality, Domain, Selector)) :-
    context_selector(Scope, Known, name(Surface), Selector),
    context_cardinality(Cardinality).
context_ask(assist(_), Origin, Cardinality, Domain, Scope, Known, Surface,
            ask(Cardinality, Domain, Selector)) :-
    Origin \= tear(_, _),
    context_selector(Scope, Known, name(Surface), Selector),
    context_cardinality(Cardinality).
context_ask(assist(EditId), tear(EditId, argument(_, _)), one, Domain, Scope,
            Known, Surface, ask(all, Domain, Selector)) :-
    context_selector(Scope, Known, prefix(Surface), Selector).
context_ask(assist(EditId), tear(EditId, argument(_, _)), all, Domain, Scope,
            Known, Surface, ask(all, Domain, Selector)) :-
    context_selector(Scope, Known, prefix(Surface), Selector).
context_ask(assist(EditId), tear(EditId, argument(_, _)), empty, Domain, Scope,
            Known, Surface, ask(empty, Domain, Selector)) :-
    context_selector(Scope, Known, prefix(Surface), Selector).

context_selector(root, _, Selector, Selector).
context_selector(within(Template), Known, Selector,
                 within(Resolved, Selector)) :-
    resolve_context_template(Template, Known, Resolved).

resolve_context_template(argument(Name), Known, ref(Id)) :-
    known_context(Name, Known, Id), !.
resolve_context_template(Term, _, Term) :- atomic(Term), !.
resolve_context_template(Term, Known, Resolved) :-
    Term =.. [Functor|Arguments],
    resolve_context_templates(Arguments, Known, ResolvedArguments),
    Resolved =.. [Functor|ResolvedArguments].

resolve_context_templates([], _, []).
resolve_context_templates([Argument|Arguments], Known,
                          [Resolved|ResolvedArguments]) :-
    resolve_context_template(Argument, Known, Resolved),
    resolve_context_templates(Arguments, Known, ResolvedArguments).

known_context(Name, [Name-Id|_], Id).
known_context(Name, [_|Known], Id) :- known_context(Name, Known, Id).

context_cardinality(empty).
context_cardinality(one).
context_cardinality(all).

context_descriptor(Name, Cardinality, Domain, Scope,
                   [context(Name, Cardinality, Domain, Scope)|_]).
context_descriptor(Name, Cardinality, Domain, Scope, [_|Contexts]) :-
    context_descriptor(Name, Cardinality, Domain, Scope, Contexts).

context_completion_pair(
    EditId, Contexts, Candidates, Queries, Observations,
    Span, Text, Domain, Identity, Provider, Preference) :-
    candidate_member(candidate(_, Evidence, _, Preference, _), Candidates),
    candidate_member(
        evidence(_, Span, _, Surface, _, Name, _,
                 tear(EditId, argument(Name, _))), Evidence),
    context_descriptor(Name, one, Domain, _Scope, Contexts),
    candidate_member(query(Id, Query), Queries),
    Query = ask(all, Domain, Selector),
    selector_has_prefix(Selector, Surface),
    candidate_member(
        observed(Id, Query, Source, some(all(Entries))), Observations),
    Source = source(Provider, _Revision),
    resolve_query_refs(Query, Observations, ResolvedQuery),
    context_tear_match(ResolvedQuery, snapshot(Source, Entries), Surface, Text,
                       _ExactQuery,
                       entry(Domain, Identity, _Names, _Value, _Attributes)).

selector_has_prefix(prefix(Surface), Surface).
selector_has_prefix(within(_, Selector), Surface) :-
    selector_has_prefix(Selector, Surface).
selector_has_prefix(and(Left, _), Surface) :- selector_has_prefix(Left, Surface).
selector_has_prefix(and(_, Right), Surface) :-
    selector_has_prefix(Right, Surface).
selector_has_prefix(or(Left, _), Surface) :- selector_has_prefix(Left, Surface).
selector_has_prefix(or(_, Right), Surface) :- selector_has_prefix(Right, Surface).

observation_dependency_keys(Observations, Keys) :-
    findall(Key,
            ( candidate_member(Observation, Observations),
              dependency_key(Observation, Key)
            ),
            Keys).

candidate_solutions([], _, _, _, _, _, _, []).
candidate_solutions(
    [candidate(Arguments, Evidence, Status, Preference, Highlights)|Candidates],
    Specs, Contexts, Queries, Observations, Wanted, Completions,
    [solution(Bindings, Preference)|Solutions]) :-
    resolve_context_arguments(Specs, Contexts, Evidence, Queries, Observations,
                              Arguments, ResolvedArguments),
    Available = [binding(arguments, ResolvedArguments),
                 binding(evidence, Evidence),
                 binding(status, Status),
                 binding(highlights, Highlights),
                 binding(completions, Completions)],
    requested_bindings(Wanted, Available, Bindings),
    candidate_solutions(Candidates, Specs, Contexts, Queries, Observations,
                        Wanted, Completions, Solutions).

resolve_context_arguments(Specs, Contexts, Evidence, Queries, Observations,
                          Arguments, Resolved) :-
    context_replacements(Evidence, Specs, Contexts, Queries, Observations,
                         1, 1, Replacements),
    replace_arguments(Replacements, Arguments, Resolved).

context_replacements([], _, _, _, _, _, _, []).
context_replacements(
    [evidence(_, _, _, _, _, Name, _, Origin)|Evidence], Specs, Contexts,
    Queries, Observations, QueryIndex, ArgumentIndex, Replacements) :-
    ( spec_argument_name(Name, Specs)
    -> NextArgument is ArgumentIndex + 1,
       ( context_descriptor(Name, _, _, _, Contexts)
       -> Id = q(QueryIndex),
          NextQuery is QueryIndex + 1,
          resolve_context_replacement(
              Origin, Id, Queries, Observations, ArgumentIndex,
              Replacements, Rest)
       ;  NextQuery = QueryIndex,
          Replacements = Rest
       )
    ;  NextArgument = ArgumentIndex,
       NextQuery = QueryIndex,
       Replacements = Rest
    ),
    context_replacements(Evidence, Specs, Contexts, Queries, Observations,
                         NextQuery, NextArgument, Rest).

% A contextual value in exact source is valid only when its declared `one`
% query succeeds. An edit tear deliberately remains a hole while completion
% observes its `all` query. Keeping this rule in ordinary solution production
% makes parsing, highlighting, and completion share the same validity test.
resolve_context_replacement(tear(_, _), _, _, _, _, Rest, Rest) :- !.
resolve_context_replacement(_, Id, Queries, Observations, ArgumentIndex,
                            Replacements, Rest) :-
    candidate_member(query(Id, Query), Queries),
    ( candidate_member(observed(Id, Query, _, Outcome), Observations)
    -> Outcome = some(one(entry(_, _, _, Value, _))),
       Replacements = [replace(ArgumentIndex, Value)|Rest]
    ;  Replacements = Rest
    ).

spec_argument_name(Name, [argument(arg(Name, _, _, _))|_]).
spec_argument_name(Name, [_|Specs]) :- spec_argument_name(Name, Specs).

replace_arguments([], Arguments, Arguments).
replace_arguments([replace(Index, Value)|Replacements], Arguments0,
                  Arguments) :-
    replace_argument(Index, Arguments0, Value, Arguments1),
    replace_arguments(Replacements, Arguments1, Arguments).

replace_argument(1, [_|Values], Value, [Value|Values]) :- !.
replace_argument(Index, [Head|Values], Value, [Head|Result]) :-
    Index > 1,
    Next is Index - 1,
    replace_argument(Next, Values, Value, Result).

candidate_member(Candidate, [Candidate|_]).
candidate_member(Candidate, [_|Candidates]) :-
    candidate_member(Candidate, Candidates).

given_value(Name, [binding(Name, Value)|_], Value).
given_value(Name, [_|Bindings], Value) :- given_value(Name, Bindings, Value).

requested_bindings([], _, []).
requested_bindings([Name|Names], Available, [binding(Name, Value)|Bindings]) :-
    given_value(Name, Available, Value),
    requested_bindings(Names, Available, Bindings).

data_terminal(Terminals, syntax(Kind, Syntax)) :-
    terminal_data(Kind, Syntax, _Surfaces, Terminals).
data_terminal(Terminals, surface(Kind, Value, Surface)) :-
    terminal_data(Kind, _Syntax, Codec, Terminals),
    terminal_value(Codec, Value, Surface).

terminal_data(Kind, Syntax, Surfaces,
              [terminal(Kind, Syntax, Surfaces)|_]).
terminal_data(Kind, Syntax, Surfaces, [_|Terminals]) :-
    terminal_data(Kind, Syntax, Surfaces, Terminals).

surface_data(Value, Surface, [surface(Value, Surface)|_]).
surface_data(Value, Surface, [_|Surfaces]) :-
    surface_data(Value, Surface, Surfaces).

terminal_value(codec(Codec), Value, Surface) :-
    codec_value(Codec, Value, Surface).
terminal_value(Surfaces, Value, Surface) :-
    proper_list(Surfaces),
    surface_data(Value, Surface, Surfaces).

rendered_surfaces([], []).
rendered_surfaces([rendered(Surface)|Items], [Surface|Surfaces]) :-
    rendered_surfaces(Items, Surfaces).

join_surfaces([], _, "").
join_surfaces([Surface], _, Surface).
join_surfaces([Surface|Surfaces], Separator, Text) :-
    Surfaces = [_|_],
    join_surfaces(Surfaces, Separator, Rest),
    string_concat(Surface, Separator, Prefix),
    string_concat(Prefix, Rest, Text).

%! relate_sequence(+Specs, +Items, +Mode, +TerminalRelation,
%!                 -Arguments, -Evidence, -EditCount) is nondet.

%! neutral_input(+Items, -Body) is semidet.
%
% Validate the bounded, grammar-neutral source envelope and remove its end
% marker.  Grammars never need to repeat span/list/text safety checks.

neutral_input(Items, Body) :-
    input_body(Items, Body, End),
    valid_items(Body, 0, End).

valid_relation_mode(exact).
valid_relation_mode(assist(_)).

relation_status(exact, 0, complete).
relation_status(assist(EditId), 1, incomplete(edit(EditId))).

relate_sequence([], [], _Mode, _TerminalRelation, [], [], 0).
% After an edit tear, a form may end with an expected continuation. Required
% arguments remain explicit holes; concrete input to the right of a tear is
% never skipped.
relate_sequence(Specs, [], assist(_), _TerminalRelation, Arguments, [], 0) :-
    specs_require_input(Specs),
    missing_arguments(Specs, Arguments).
relate_sequence(
    [literal(Semantic, Text, Syntax, Description, LiteralPreference)|Specs],
    [Item|Items], Mode, TerminalRelation, Arguments,
    [EvidenceItem|Evidence], EditCount) :-
    match_literal(
        literal(Semantic, Text, Syntax, Description, LiteralPreference),
        Item, Mode, EvidenceItem, ItemEditCount),
    relate_sequence(Specs, Items, Mode, TerminalRelation, Arguments,
                    Evidence, RestCount),
    EditCount is RestCount + ItemEditCount.
relate_sequence([argument(arg(Name, Kind, required, scalar))|Specs],
                [Item|Items], Mode, TerminalRelation, [Value|Arguments],
                [EvidenceItem|Evidence], EditCount) :-
    match_argument(TerminalRelation, Name, Kind, Item, Mode, Value,
                   EvidenceItem, ItemEditCount),
    relate_sequence(Specs, Items, Mode, TerminalRelation, Arguments,
                    Evidence, RestCount),
    EditCount is RestCount + ItemEditCount.
relate_sequence([argument(arg(Name, Kind, optional, scalar))|Specs],
                [Item|Items], Mode, TerminalRelation, [Value|Arguments],
                [EvidenceItem|Evidence], EditCount) :-
    match_argument(TerminalRelation, Name, Kind, Item, Mode, Value,
                   EvidenceItem, ItemEditCount),
    relate_sequence(Specs, Items, Mode, TerminalRelation, Arguments,
                    Evidence, RestCount),
    EditCount is RestCount + ItemEditCount.
relate_sequence([argument(arg(_, _, optional, scalar))|Specs], Items, Mode,
                TerminalRelation, Arguments, Evidence, EditCount) :-
    relate_sequence(Specs, Items, Mode, TerminalRelation, Arguments,
                    Evidence, EditCount).
relate_sequence([argument(arg(Name, Kind, repeated, Shape))|Specs], Items0,
                Mode, TerminalRelation, Arguments, Evidence, EditCount) :-
    source_mode(Mode),
    relate_repeated(TerminalRelation, Name, Kind, Values, Items0, Mode,
                    RepeatedEvidence, RepeatedEditCount, Items),
    relate_sequence(Specs, Items, Mode, TerminalRelation, RestArguments,
                    RestEvidence, RestEditCount),
    repeated_arguments(Shape, Values, Specs, RepeatedArguments),
    append(RepeatedArguments, RestArguments, Arguments),
    append(RepeatedEvidence, RestEvidence, Evidence),
    EditCount is RepeatedEditCount + RestEditCount.
relate_sequence([argument(arg(Name, Kind, repeated, Shape))|Specs], Items0,
                render, TerminalRelation, Arguments, Evidence, EditCount) :-
    repeated_arguments(Shape, Values, Specs, RepeatedArguments),
    append(RepeatedArguments, RestArguments, Arguments),
    relate_repeated(TerminalRelation, Name, Kind, Values, Items0, render,
                    RepeatedEvidence, RepeatedEditCount, Items),
    relate_sequence(Specs, Items, render, TerminalRelation, RestArguments,
                    RestEvidence, RestEditCount),
    append(RepeatedEvidence, RestEvidence, Evidence),
    EditCount is RepeatedEditCount + RestEditCount.

match_literal(
    literal(Semantic, Text, Syntax, Description, LiteralPreference),
    Item, Mode,
    evidence(Semantic, Span, PaintSpans, Surface, Syntax, Description,
             Preference, Origin), 0) :-
    source_mode(Mode),
    source_unit(Item, Span, PaintSpans, Surface, SourcePreference, Origin),
    text_string(Surface, SurfaceString),
    SurfaceString = Text,
    Preference is SourcePreference + LiteralPreference.
match_literal(
    literal(Semantic, Text, Syntax, Description, LiteralPreference),
    edit_tear(EditId, Span, Surface), assist(EditId),
    evidence(Semantic, Span, [], Surface, Syntax, Description,
             LiteralPreference, tear(EditId, literal(Text))), 1) :-
    surface_prefix(Surface, Text).
match_literal(literal(_, Text, _, _, _), rendered(Text), render,
              rendered, 0).

match_argument(TerminalRelation, Name, Kind, Item, Mode, Value,
               evidence(Value, Span, PaintSpans, Surface, Syntax, Name,
                        Preference, Origin), 0) :-
    source_mode(Mode),
    source_unit(Item, Span, PaintSpans, Surface, SourcePreference, Origin),
    call(TerminalRelation, surface(Kind, Value, Surface)),
    call(TerminalRelation, syntax(Kind, Syntax)),
    Preference is SourcePreference + 10.
match_argument(TerminalRelation, Name, Kind,
               edit_tear(EditId, Span, Surface), assist(EditId),
               hole(Name, Kind),
               evidence(hole(Name, Kind), Span, [], Surface, Syntax, Name, 10,
                        tear(EditId, argument(Name, Kind))), 1) :-
    call(TerminalRelation, syntax(Kind, Syntax)).
match_argument(TerminalRelation, _Name, Kind, rendered(Surface), render,
               Value, rendered, 0) :-
    call(TerminalRelation, surface(Kind, Value, Surface)).

source_mode(exact).
source_mode(assist(_)).

source_unit(unit(_, Span, PaintSpans, Surface, _, _, Preference, Origin),
            Span, PaintSpans, Surface, Preference, Origin).

relate_repeated(_, _, _, [], Items, _Mode, [], 0, Items).
relate_repeated(TerminalRelation, Name, Kind, [Value|Values], [Item|Items0],
                Mode, [Evidence|EvidenceItems], EditCount, Items) :-
    match_argument(TerminalRelation, Name, Kind, Item, Mode, Value, Evidence,
                   ItemEditCount),
    relate_repeated(TerminalRelation, Name, Kind, Values, Items0, Mode,
                    EvidenceItems, RestEditCount, Items),
    EditCount is ItemEditCount + RestEditCount.

specs_require_input([literal(_, _, _, _, _)|_]).
specs_require_input([argument(arg(_, _, required, scalar))|_]).
specs_require_input([_|Specs]) :- specs_require_input(Specs).

missing_arguments([], []).
missing_arguments([literal(_, _, _, _, _)|Specs], Arguments) :-
    missing_arguments(Specs, Arguments).
missing_arguments([argument(arg(Name, Kind, required, scalar))|Specs],
                  [hole(Name, Kind)|Arguments]) :-
    missing_arguments(Specs, Arguments).
missing_arguments([argument(arg(_, _, optional, scalar))|Specs], Arguments) :-
    missing_arguments(Specs, Arguments).
missing_arguments([argument(arg(_, _, repeated, Shape))|Specs], Arguments) :-
    repeated_arguments(Shape, [], Specs, RepeatedArguments),
    missing_arguments(Specs, RestArguments),
    append(RepeatedArguments, RestArguments, Arguments).

repeated_arguments(array, Values, _Specs, [array(Values)]) :- Values = [_|_].
repeated_arguments(array, [], Specs, [array([])]) :- specs_have_argument(Specs).
repeated_arguments(array, [], Specs, []) :- \+ specs_have_argument(Specs).
repeated_arguments(spread, Values, _Specs, Values).

specs_have_argument([argument(_)|_]) :- !.
specs_have_argument([_|Specs]) :- specs_have_argument(Specs).

surface_prefix(Surface, Text) :-
    text_string(Surface, SurfaceString),
    text_string(Text, TextString),
    sub_string(TextString, 0, _, _, SurfaceString).

text_string(Text, Text) :- string(Text), !.
text_string(Text, String) :- atom_string(Text, String).

input_body([end(End)], [], End) :-
    integer(End),
    End >= 0.
input_body([Item|Items], [Item|Body], End) :-
    input_body(Items, Body, End).

valid_items([], PreviousStop, End) :-
    PreviousStop =< End.
valid_items([Item|Items], PreviousStop, End) :-
    item_span(Item, span(Start, Stop)),
    Start >= PreviousStop,
    valid_item(Item, End),
    valid_items(Items, Stop, End).

item_span(unit(_, Span, _, _, _, _, _, _), Span).
item_span(edit_tear(_, Span, _), Span).
item_span(source_tear(_, Span, _), Span).

valid_item(unit(_, span(Start, Stop), PaintSpans, Surface, _, _, Preference, _),
           End) :-
    valid_span(span(Start, Stop), End),
    proper_list(PaintSpans),
    valid_paint_spans(PaintSpans, Start, Start, Stop),
    text_value(Surface),
    number(Preference).
valid_item(edit_tear(_, Span, Surface), End) :-
    valid_span(Span, End),
    text_value(Surface).
valid_item(source_tear(_, Span, Surface), End) :-
    valid_span(Span, End),
    text_value(Surface).

valid_paint_spans([], _, _, _).
valid_paint_spans([span(Start, Stop)|Spans], PreviousStop,
                  OwnerStart, OwnerStop) :-
    integer(Start),
    integer(Stop),
    OwnerStart =< Start,
    PreviousStop =< Start,
    Start =< Stop,
    Stop =< OwnerStop,
    valid_paint_spans(Spans, Stop, OwnerStart, OwnerStop).

valid_span(span(Start, Stop), End) :-
    integer(Start),
    integer(Stop),
    0 =< Start,
    Start =< Stop,
    Stop =< End.

proper_list([]).
proper_list([_|Items]) :- proper_list(Items).

text_value(Text) :- string(Text), !.
text_value(Text) :- atom(Text).

evidence_preference([], Preference, Preference).
evidence_preference([evidence(_, _, _, _, _, _, ItemPreference, _)|Evidence],
                    Accumulator, Preference) :-
    Next is Accumulator + ItemPreference,
    evidence_preference(Evidence, Next, Preference).

% Core-only embedded SWI does not load library(lists).
append([], Tail, Tail).
append([Head|Items], Tail, [Head|Result]) :- append(Items, Tail, Result).
