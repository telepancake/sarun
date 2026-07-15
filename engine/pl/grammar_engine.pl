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
:- use_module(grammar_codec).

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
    limit_solutions(Solutions0, MaxSolutions, Solutions, LimitDiagnostics),
    sort(Queries0, Queries),
    sort(DependencyKeys0, DependencyKeys),
    append(Diagnostics0, LimitDiagnostics, Diagnostics).

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
    ready_queries(Graph, Observations, Ready),
    observation_dependency_keys(Observations, DependencyKeys),
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
    candidate_completions(Mode, Contexts, Candidates, Queries, Observations,
                          Completions),
    candidate_solutions(Candidates, Wanted, Completions, Solutions),
    observation_dependency_keys(Observations, DependencyKeys).
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

% A choice is grammar composition, not an operation switch. Alternative keys
% are immutable grammar data used to namespace context-query identities; a
% context observation can therefore be routed back through the same nested
% grammar without a client knowing which branch produced it.
choice_alternative_reply(
    [alternative(Key, Preference, Grammar)|_], Given, Wanted, Observations,
    Limits, Reply) :-
    scoped_observations(Key, Observations, InnerObservations),
    transform_relation(Grammar, Given, Wanted, InnerObservations, Limits,
                       InnerReply),
    scope_reply(Key, Preference, InnerReply, Reply).
choice_alternative_reply([_|Alternatives], Given, Wanted, Observations,
                         Limits, Reply) :-
    choice_alternative_reply(Alternatives, Given, Wanted, Observations,
                             Limits, Reply).

scoped_observations(_, [], []).
scoped_observations(Key,
                    [observed(branch(Key, Id), Query, Source, Outcome)|Rest],
                    [observed(Id, Query, Source, Outcome)|Inner]) :-
    !,
    scoped_observations(Key, Rest, Inner).
scoped_observations(Key, [_|Rest], Inner) :-
    scoped_observations(Key, Rest, Inner).

scope_reply(Key, AlternativePreference,
            reply(Solutions0, Queries0, Dependencies0, Diagnostics),
            reply(Solutions, Queries, Dependencies, Diagnostics)) :-
    scope_solutions(Solutions0, AlternativePreference, Solutions),
    scope_queries(Key, Queries0, Queries),
    scope_dependencies(Key, Dependencies0, Dependencies).

scope_solutions([], _, []).
scope_solutions([solution(Bindings, Preference0)|Rest], AlternativePreference,
                [solution(Bindings, Preference)|Solutions]) :-
    Preference is Preference0 + AlternativePreference,
    scope_solutions(Rest, AlternativePreference, Solutions).

scope_queries(_, [], []).
scope_queries(Key, [query(Id, Query)|Rest],
              [query(branch(Key, Id), Query)|Queries]) :-
    scope_queries(Key, Rest, Queries).

scope_dependencies(_, [], []).
scope_dependencies(Key, [dependency(Id, Query, Outcome)|Rest],
                   [dependency(branch(Key, Id), Query, Outcome)|Dependencies]) :-
    scope_dependencies(Key, Rest, Dependencies).

choice_replies([], [], [], [], []).
choice_replies([reply(Solutions0, Queries0, Dependencies0, Diagnostics0)|Rest],
               Solutions, Queries, Dependencies, Diagnostics) :-
    choice_replies(Rest, Solutions1, Queries1, Dependencies1, Diagnostics1),
    append(Solutions0, Solutions1, Solutions),
    append(Queries0, Queries1, Queries),
    append(Dependencies0, Dependencies1, Dependencies),
    append(Diagnostics0, Diagnostics1, Diagnostics).

% A projection template is a small pure relation over generic bindings:
%
%   constant(Value)                  immutable grammar data
%   reference(Name)                  another representation binding
%   structure(Functor, Arguments)    a compound semantic value
%   sequence(Items)                  a list semantic value
%   concatenate(Left, Right)         relational list concatenation
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
    evidence_context_query_(Contexts, Evidence, Mode, Index, _Next, Query).

evidence_context_query_(Contexts, [Item|_], Mode, Index, Next, Query) :-
    contextual_evidence(Contexts, Item, Mode, Index, Query),
    Next is Index + 1.
evidence_context_query_(Contexts, [Item|Items], Mode, Index, Next, Query) :-
    ( contextual_evidence(Contexts, Item, Mode, Index, _)
    -> Following is Index + 1
    ;  Following = Index
    ),
    evidence_context_query_(Contexts, Items, Mode, Following, Next, Query).

contextual_evidence(Contexts,
                    evidence(_, _, _, Surface, _, Name, _, Origin),
                    Mode, Index, query(q(Index), Ask)) :-
    context_descriptor(Name, Cardinality, Domain, root, Contexts),
    context_ask(Mode, Origin, Cardinality, Domain, Surface, Ask).

context_ask(exact, _, Cardinality, Domain, Surface,
            ask(Cardinality, Domain, name(Surface))) :-
    context_cardinality(Cardinality).
context_ask(assist(EditId), tear(EditId, argument(_, _)), one, Domain, Surface,
            ask(all, Domain, prefix(Surface))).
context_ask(assist(EditId), tear(EditId, argument(_, _)), all, Domain, Surface,
            ask(all, Domain, prefix(Surface))).
context_ask(assist(EditId), tear(EditId, argument(_, _)), empty, Domain, Surface,
            ask(empty, Domain, prefix(Surface))).

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
    context_descriptor(Name, one, Domain, root, Contexts),
    candidate_member(query(Id, Query), Queries),
    Query = ask(all, Domain, prefix(Surface)),
    candidate_member(
        observed(Id, Query, Source, some(all(Entries))), Observations),
    Source = source(Provider, _Revision),
    context_tear_match(Query, snapshot(Source, Entries), Surface, Text,
                       _ExactQuery,
                       entry(Domain, Identity, _Names, _Value, _Attributes)).

observation_dependency_keys(Observations, Keys) :-
    findall(Key,
            ( candidate_member(Observation, Observations),
              dependency_key(Observation, Key)
            ),
            Keys).

candidate_solutions([], _, _, []).
candidate_solutions(
    [candidate(Arguments, Evidence, Status, Preference, Highlights)|Candidates],
    Wanted, Completions, [solution(Bindings, Preference)|Solutions]) :-
    Available = [binding(arguments, Arguments),
                 binding(evidence, Evidence),
                 binding(status, Status),
                 binding(highlights, Highlights),
                 binding(completions, Completions)],
    requested_bindings(Wanted, Available, Bindings),
    candidate_solutions(Candidates, Wanted, Completions, Solutions).

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
    repeated_arguments(Shape, Values, Specs, RepeatedArguments),
    append(RepeatedArguments, RestArguments, Arguments),
    relate_repeated(TerminalRelation, Name, Kind, Values, Items0, Mode,
                    RepeatedEvidence, RepeatedEditCount, Items),
    relate_sequence(Specs, Items, Mode, TerminalRelation, RestArguments,
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

literal_completion_evidence(
    EditId,
    [evidence(Semantic, Span, _, _, Syntax, Description, _,
              tear(EditId, literal(Text)))|_],
    Span, Text, Semantic, Syntax, Description).
literal_completion_evidence(EditId, [_|Evidence], Span, Text, Semantic, Syntax,
                            Description) :-
    literal_completion_evidence(EditId, Evidence, Span, Text, Semantic, Syntax,
                                Description).

%! project_completions(+Pairs, -Completions) is det.
%
% `Pairs` are `completion_key(Span, Surface)-(Alternative-Preference)` parse
% witnesses.  Grouping identical visible edits, retaining semantic ambiguity,
% choosing best scores, deterministic ordering, and rank assignment are engine
% behavior shared by every grammar.

project_completions([], []).
project_completions(Pairs, Completions) :-
    keysort(Pairs, SortedPairs),
    group_visible_pairs(SortedPairs, Candidates),
    sort_candidates(Candidates, Sorted),
    rank_completions(Sorted, 1, Completions).

group_visible_pairs([], []).
group_visible_pairs([Visible-Value|Pairs],
                    [candidate(Visible, Alternatives, Preference)|Candidates]) :-
    take_visible_pairs(Pairs, Visible, [Value], Values, Rest),
    merge_alternatives(Values, Alternatives, Preference),
    group_visible_pairs(Rest, Candidates).

take_visible_pairs([Visible-Value|Pairs], Visible, Values0, Values, Rest) :-
    !,
    take_visible_pairs(Pairs, Visible, [Value|Values0], Values, Rest).
take_visible_pairs(Pairs, _Visible, Values, Values, Pairs).

merge_alternatives(Values, Alternatives, Preference) :-
    alternative_pairs(Values, Pairs),
    keysort(Pairs, Sorted),
    group_alternative_pairs(Sorted, Alternatives, Preference).

alternative_pairs([], []).
alternative_pairs([Alternative-Preference|Values],
                  [Alternative-Preference|Pairs]) :-
    alternative_pairs(Values, Pairs).

group_alternative_pairs([Alternative-Value|Pairs],
                        [Merged|Alternatives], Preference) :-
    take_alternative_pairs(Pairs, Alternative, Value, Best, Rest),
    Alternative = alternative(Semantic, Syntax, Description),
    Merged = alternative(Semantic, Syntax, Description, Best),
    group_alternative_pairs_rest(Rest, Alternatives, RestPreference),
    max_number(Best, RestPreference, Preference).

group_alternative_pairs_rest([], [], -1.0Inf).
group_alternative_pairs_rest([Alternative-Value|Pairs],
                             [Merged|Alternatives], Preference) :-
    take_alternative_pairs(Pairs, Alternative, Value, Best, Rest),
    Alternative = alternative(Semantic, Syntax, Description),
    Merged = alternative(Semantic, Syntax, Description, Best),
    group_alternative_pairs_rest(Rest, Alternatives, RestPreference),
    max_number(Best, RestPreference, Preference).

take_alternative_pairs([Alternative-Value|Pairs], Alternative,
                       Best0, Best, Rest) :-
    !,
    max_number(Best0, Value, Best1),
    take_alternative_pairs(Pairs, Alternative, Best1, Best, Rest).
take_alternative_pairs(Pairs, _Alternative, Best, Best, Pairs).

max_number(A, B, A) :- A >= B, !.
max_number(_A, B, B).

sort_candidates(Candidates, Sorted) :-
    candidate_rank_pairs(Candidates, Pairs),
    keysort(Pairs, RankedPairs),
    pair_values(RankedPairs, Sorted).

candidate_rank_pairs([], []).
candidate_rank_pairs([Candidate|Candidates],
                     [rank_key(Negative, Visible)-Candidate|Pairs]) :-
    Candidate = candidate(Visible, _Alternatives, Preference),
    Negative is -Preference,
    candidate_rank_pairs(Candidates, Pairs).

pair_values([], []).
pair_values([_-Value|Pairs], [Value|Values]) :- pair_values(Pairs, Values).

rank_completions([], _, []).
rank_completions([candidate(completion_key(Span, Text), Alternatives,
                            Preference)|Candidates], Rank,
                 [completion(Span, Text, Alternatives, Preference, Rank)|Rest]) :-
    NextRank is Rank + 1,
    rank_completions(Candidates, NextRank, Rest).

project_highlights(Evidence, Highlights) :-
    evidence_highlights(Evidence, Highlights, []).

evidence_highlights([], Highlights, Highlights).
evidence_highlights([evidence(Semantic, _Span, PaintSpans, _Surface, Syntax,
                              _Description, _Preference, Origin)|Evidence],
                    Highlights0, Highlights) :-
    paint_highlights(PaintSpans, Syntax, Semantic, Origin,
                     Highlights0, Highlights1),
    evidence_highlights(Evidence, Highlights1, Highlights).

paint_highlights([], _Syntax, _Semantic, _Origin, Highlights, Highlights).
paint_highlights([PaintSpan|PaintSpans], Syntax, Semantic, Origin,
                 [highlight(PaintSpan, Syntax, Semantic, Origin)|Highlights0],
                 Highlights) :-
    paint_highlights(PaintSpans, Syntax, Semantic, Origin,
                     Highlights0, Highlights).

% Core-only embedded SWI does not load library(lists).
append([], Tail, Tail).
append([Head|Items], Tail, [Head|Result]) :- append(Items, Tail, Result).
