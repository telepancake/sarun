:- module(action_grammar,
          [ action/6,
            valid_action/1,
            parse/2,
            parse/3,
            completions/3,
            highlights/2,
            render/3,
            application/3
          ]).

/** <module> Core-only grammar for mirror actions over lexer evidence

The grammar consumes a list ending in `end(BytePosition)`. Other items are:

  * `unit(Semantic, Span, PaintSpans, Surface, Syntax, DescriptionKey,
    Preference, Origin)` for a semantic unit recognized by the lexer;
  * `edit_tear(Id, Span, Surface)` for the one source range assist mode may
    replace; and
  * `source_tear(Id, Span, Surface)` for unrecognized source that the grammar
    must not silently repair.

Spans are ordered, non-overlapping, half-open UTF-8 byte spans represented as
`span(Start, End)`. Paint spans must be ordered, non-overlapping, and contained
by their owning unit. A successful parse returns
`parse_result(command(Action, Args), Status, Evidence, Preference)`. Evidence
contains only real `unit/8` items, so highlighting is a projection of a grammar
derivation rather than a second classification pass.

This module intentionally imports no libraries. It is loadable by sarun's
core-only embedded SWI-Prolog runtime.
*/

:- dynamic action/6.

% action(Name, ArgumentSchema, VerbForm, CliForm, DescriptionKey, Preference).
% Forms are sequences of literal/5 and argument/1 terms. Description values
% are provider keys; resolving them to prose belongs outside this module.

action(mirror_jobs, [],
       [literal(mirror_jobs, "mirror_jobs", action_identifier,
                action_mirror_jobs, 30)],
       [literal(mirror, "mirror", command_namespace, mirror_namespace, 10),
        literal(ls, "ls", action_word, action_mirror_jobs, 20)],
       action_mirror_jobs, 90).

action(mirror_run, [job_id],
       [literal(mirror_run, "mirror_run", action_identifier,
                action_mirror_run, 30),
        argument(job_id)],
       [literal(mirror, "mirror", command_namespace, mirror_namespace, 10),
        literal(run, "run", action_word, mirror_run_word, 20),
        argument(job_id)],
       action_mirror_run, 100).

action(mirror_run_pending, [],
       [literal(mirror_run_pending, "mirror_run_pending", action_identifier,
                action_mirror_run_pending, 30)],
       [literal(mirror, "mirror", command_namespace, mirror_namespace, 10),
        literal(run, "run", action_word, action_mirror_run_pending, 20)],
       action_mirror_run_pending, 85).

action(mirror_pause, [job_id],
       [literal(mirror_pause, "mirror_pause", action_identifier,
                action_mirror_pause, 30),
        argument(job_id)],
       [literal(mirror, "mirror", command_namespace, mirror_namespace, 10),
        literal(pause, "pause", action_word, action_mirror_pause, 20),
        argument(job_id)],
       action_mirror_pause, 80).

action(mirror_rm, [job_id],
       [literal(mirror_rm, "mirror_rm", action_identifier,
                action_mirror_rm, 30),
        argument(job_id)],
       [literal(mirror, "mirror", command_namespace, mirror_namespace, 10),
        literal(rm, "rm", action_word, action_mirror_rm, 20),
        argument(job_id)],
       action_mirror_rm, 75).

%! valid_action(+Name) is semidet.
%
% Validate the complete action fact, including exact agreement between each
% form's argument sequence and the declared schema.

valid_action(Name) :-
    action(Name, Schema, Verb, Cli, Description, Preference),
    valid_action_fact(Name, Schema, Verb, Cli, Description, Preference).

valid_action_fact(Name, Schema, Verb, Cli, Description, Preference) :-
    atom(Name),
    valid_schema(Schema),
    valid_form(Verb, Schema),
    valid_form(Cli, Schema),
    atom(Description),
    number(Preference).

valid_schema([]).
valid_schema([job_id|Schema]) :-
    valid_schema(Schema).

valid_form([literal(Semantic, Text, Syntax, Description, Preference)|Specs],
           Schema) :-
    valid_literal(Semantic, Text, Syntax, Description, Preference),
    valid_specs(Specs, Schema).

valid_specs([], []).
valid_specs([literal(Semantic, Text, Syntax, Description, Preference)|Specs],
            Schema) :-
    valid_literal(Semantic, Text, Syntax, Description, Preference),
    valid_specs(Specs, Schema).
valid_specs([argument(job_id)|Specs], [job_id|Schema]) :-
    valid_specs(Specs, Schema).

valid_literal(Semantic, Text, Syntax, Description, Preference) :-
    ground(Semantic),
    string(Text),
    atom(Syntax),
    atom(Description),
    number(Preference).

%! parse(+Items, -Result) is nondet.
%
% Parse without repairing any input.

parse(Items, Result) :-
    parse(Items, exact, Result).

%! parse(+Items, +Mode, -Result) is nondet.
%
% Mode is `exact` or `assist(EditTearId)`. An assist parse is a complete
% derivation after replacing exactly that tear, but its evidence excludes the
% hypothetical replacement.

parse(Items, Mode, parse_result(command(Action, Args), Status,
                                Evidence, Preference)) :-
    input_body(Items, Body, End),
    valid_body(Body, End),
    valid_mode(Mode),
    action(Action, Schema, Verb, Cli, Description, ActionPreference),
    valid_action_fact(Action, Schema, Verb, Cli, Description, ActionPreference),
    ( Specs = Verb ; Specs = Cli ),
    match_complete(Specs, Body, Mode, Args, [], Evidence, [], EditCount),
    args_match_schema(Args, Schema),
    parse_status(Mode, EditCount, Status),
    evidence_preference(Evidence, ActionPreference, Preference).

valid_mode(exact).
valid_mode(assist(_)).

parse_status(exact, 0, complete).
parse_status(assist(EditId), 1, incomplete(edit(EditId))).

input_body([end(End)], [], End) :-
    integer(End),
    End >= 0.
input_body([Item|Items], [Item|Body], End) :-
    input_body(Items, Body, End).

valid_body(Body, End) :-
    valid_items(Body, 0, End).

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
    text(Surface),
    number(Preference).
valid_item(edit_tear(_, Span, Surface), End) :-
    valid_span(Span, End),
    text(Surface).
valid_item(source_tear(_, Span, Surface), End) :-
    valid_span(Span, End),
    text(Surface).

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
proper_list([_|Items]) :-
    proper_list(Items).

text(Text) :- string(Text), !.
text(Text) :- atom(Text).

match_complete([], [], _Mode, Args, Args, Evidence, Evidence, 0).
match_complete([literal(Semantic, _Text, Syntax, _Description, _Preference)|Specs],
               [unit(Semantic, Span, PaintSpans, Surface, Syntax,
                     Description, Preference, Origin)|Items],
               Mode, Args0, Args, Evidence0, Evidence, EditCount) :-
    Evidence0 = [evidence(Semantic, Span, PaintSpans, Surface, Syntax,
                          Description, Preference, Origin)|Evidence1],
    match_complete(Specs, Items, Mode, Args0, Args,
                   Evidence1, Evidence, EditCount).
match_complete([argument(job_id)|Specs],
               [unit(integer(Id), Span, PaintSpans, Surface, integer,
                     Description, Preference, Origin)|Items],
               Mode, [job_id(Id)|Args0], Args, Evidence0, Evidence, EditCount) :-
    integer(Id),
    Id >= 0,
    Evidence0 = [evidence(integer(Id), Span, PaintSpans, Surface, integer,
                          Description, Preference, Origin)|Evidence1],
    match_complete(Specs, Items, Mode, Args0, Args,
                   Evidence1, Evidence, EditCount).
match_complete([literal(_Semantic, Text, _Syntax, _Description, _Preference)|Specs],
               [edit_tear(EditId, _Span, Surface)|Items],
               assist(EditId), Args0, Args, Evidence0, Evidence, EditCount) :-
    surface_prefix(Surface, Text),
    match_complete(Specs, Items, assist(EditId), Args0, Args,
                   Evidence0, Evidence, RestEditCount),
    EditCount is RestEditCount + 1.

args_match_schema([], []).
args_match_schema([job_id(Id)|Args], [job_id|Schema]) :-
    integer(Id),
    Id >= 0,
    args_match_schema(Args, Schema).

evidence_preference([], Preference, Preference).
evidence_preference([evidence(_, _, _, _, _, _, ItemPreference, _)|Evidence],
                    Accumulator, Preference) :-
    Next is Accumulator + ItemPreference,
    evidence_preference(Evidence, Next, Preference).

%! completions(+Items, +EditTearId, -Completions) is det.
%
% Completions are ranked and deduplicated by their visible identity: edit span
% plus complete replacement text. Each `completion/5` contains all distinct
% grammar alternatives as
% `alternative(Semantic, Syntax, DescriptionKey, Preference)`. Candidates must
% match all source around the tear and come from a schema-valid action form.

completions(Items, EditId, Completions) :-
    (   input_body(Items, Body, End),
        valid_body(Body, End)
    ->  findall(Visible-(Alternative-Preference),
                completion_candidate(Body, EditId, Visible,
                                     Alternative, Preference),
                Pairs),
        merge_completion_pairs(Pairs, Candidates),
        sort_candidates(Candidates, Sorted),
        rank_completions(Sorted, 1, Completions)
    ;   Completions = []
    ).

completion_candidate(Body, EditId, completion_key(Span, Text),
                     alternative(Semantic, Syntax, Description), Preference) :-
    action(Action, Schema, Verb, Cli, ActionDescription, ActionPreference),
    valid_action_fact(Action, Schema, Verb, Cli,
                      ActionDescription, ActionPreference),
    ( Specs = Verb ; Specs = Cli ),
    split_edit(Body, EditId, Before, Span, Surface, After),
    split_literal(Specs, BeforeSpecs,
                  literal(Semantic, Text, Syntax, Description,
                          TerminalPreference),
                  AfterSpecs),
    match_known_prefix(BeforeSpecs, Before),
    surface_prefix(Surface, Text),
    viable_suffix(AfterSpecs, After),
    Preference is ActionPreference + TerminalPreference.

split_edit([edit_tear(EditId, Span, Surface)|After], EditId,
           [], Span, Surface, After).
split_edit([Item|Items], EditId, [Item|Before], Span, Surface, After) :-
    split_edit(Items, EditId, Before, Span, Surface, After).

split_literal([Literal|Specs], [], Literal, Specs) :-
    Literal = literal(_, _, _, _, _).
split_literal([Spec|Specs], [Spec|Before], Literal, After) :-
    split_literal(Specs, Before, Literal, After).

match_known_prefix([], []).
match_known_prefix([Spec|Specs], [Item|Items]) :-
    match_known_item(Spec, Item),
    match_known_prefix(Specs, Items).

match_known_item(literal(Semantic, _Text, Syntax, _Description, _Preference),
                 unit(Semantic, _, _, _, Syntax, _, _, _)).
match_known_item(argument(job_id),
                 unit(integer(Id), _, _, _, integer, _, _, _)) :-
    integer(Id),
    Id >= 0.

viable_suffix([], []).
viable_suffix([_|_], []).
viable_suffix([Spec|Specs], [Item|Items]) :-
    match_known_item(Spec, Item),
    viable_suffix(Specs, Items).

surface_prefix(Surface, Text) :-
    text_string(Surface, SurfaceString),
    text_string(Text, TextString),
    sub_string(TextString, 0, _, _, SurfaceString).

text_string(Text, Text) :- string(Text), !.
text_string(Text, String) :- atom_string(Text, String).

merge_completion_pairs([], []).
merge_completion_pairs(Pairs, Candidates) :-
    keysort(Pairs, Sorted),
    group_visible_pairs(Sorted, Candidates).

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
pair_values([_-Value|Pairs], [Value|Values]) :-
    pair_values(Pairs, Values).

rank_completions([], _, []).
rank_completions([candidate(completion_key(Span, Text), Alternatives,
                            Preference)|Candidates], Rank,
                 [completion(Span, Text, Alternatives, Preference, Rank)|Rest]) :-
    NextRank is Rank + 1,
    rank_completions(Candidates, NextRank, Rest).

%! highlights(+ParseResult, -Highlights) is det.
%
% Project paint spans from evidence in a successful derivation. There is no
% lexical fallback, and assist replacements never enter Evidence.

highlights(parse_result(_Command, _Status, Evidence, _Preference), Highlights) :-
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

%! render(+Command, +Style, -Text) is semidet.
%
% Render the normalized command as `verb` or `cli` from the same validated
% action fact used by the parser.

render(command(Action, Args), Style, Text) :-
    action(Action, Schema, Verb, Cli, Description, Preference),
    valid_action_fact(Action, Schema, Verb, Cli, Description, Preference),
    args_match_schema(Args, Schema),
    style_specs(Style, Verb, Cli, Specs),
    render_specs(Specs, Args, Parts, []),
    join_parts(Parts, Text).

style_specs(verb, Verb, _Cli, Verb).
style_specs(cli, _Verb, Cli, Cli).

render_specs([], Args, [], Args).
render_specs([literal(_Semantic, Text, _Syntax, _Description, _Preference)|Specs],
             Args, [Text|Parts], RemainingArgs) :-
    render_specs(Specs, Args, Parts, RemainingArgs).
render_specs([argument(job_id)|Specs], [job_id(Id)|Args], [Text|Parts],
             RemainingArgs) :-
    integer(Id),
    Id >= 0,
    number_string(Id, Text),
    render_specs(Specs, Args, Parts, RemainingArgs).

join_parts([Part], Part).
join_parts([Part|Parts], Text) :-
    Parts = [_|_],
    join_parts(Parts, Rest),
    string_concat(Part, " ", Prefix),
    string_concat(Prefix, Rest, Text).

%! application(+Operation, +InputString, -OutputString) is det.
%
% Closed application entry point for typed FFI callers. Operation is one of
% `parse`, `complete`, `highlights`, or `render`; InputString is decoded as a
% ground request term and is never invoked as a goal. OutputString is a
% canonical serialized `ok(Value)` or `error(Reason)` term.

application(Operation, InputString, OutputString) :-
    (   atom(Operation)
    ->  application_atom(Operation, InputString, Response)
    ;   Response = error(invalid_operation)
    ),
    term_string(Response, OutputString,
                [quoted(true), ignore_ops(true), numbervars(true)]).

application_atom(Operation, InputString, Response) :-
    (   decode_request(InputString, Request)
    ->  dispatch_application(Operation, Request, Response)
    ;   Response = error(invalid_request)
    ).

decode_request(InputString, Request) :-
    string(InputString),
    catch(term_string(Request, InputString, [syntax_errors(error)]), _, fail),
    ground(Request).

dispatch_application(parse, request(Items, Mode), ok(Results)) :-
    !,
    findall(Result, parse(Items, Mode, Result), Results).
dispatch_application(complete, request(Items, EditId), ok(Completions)) :-
    !,
    completions(Items, EditId, Completions).
dispatch_application(highlights, request(Result), ok(Highlights)) :-
    !,
    (   highlights(Result, Highlights)
    ->  true
    ;   Highlights = []
    ).
dispatch_application(render, request(Command, Style), Response) :-
    !,
    (   render(Command, Style, Text)
    ->  Response = ok(Text)
    ;   Response = error(no_solution)
    ).
dispatch_application(parse, _Request, error(invalid_request)) :- !.
dispatch_application(complete, _Request, error(invalid_request)) :- !.
dispatch_application(highlights, _Request, error(invalid_request)) :- !.
dispatch_application(render, _Request, error(invalid_request)) :- !.
dispatch_application(_Operation, _Request, error(unknown_operation)).
