:- module(action_grammar,
          [ action/7,
            valid_action/1,
            parse/2,
            parse/3,
            completions/3,
            highlights/2,
            render/3,
            catalog/2,
            application/3
          ]).

:- use_module(action_catalog).
:- use_module(context_relation).

/** <module> Relational parser and representation hub

The grammar consumes neutral lexer evidence. Rust supplies UTF-8 byte spans and
source surfaces; it does not classify command names or arguments. This module
relates those surfaces to canonical actions, typed wire values, syntax,
descriptions, completions, and rendered forms using the sole action definition
in `action_catalog.pl`.

Input ends in `end(BytePosition)`. Source tokens are represented as `unit/8`;
their incoming semantic/syntax/provider fields are deliberately ignored by the
command grammar. `edit_tear/3` marks the one source range completion may
replace. `source_tear/3` remains an explicit unrecognized hole and is never
silently repaired.

The normalized result is
`command(Action, Handler, Target, WireArguments)`. Rust therefore receives a
dispatch-ready value without consulting another registry.
*/

valid_action(Action) :-
    action(Action, Handler, Target, Notation, Description, Visibility,
           Preference),
    atom(Action),
    atom(Handler),
    valid_target(Target),
    string(Notation),
    string(Description),
    valid_visibility(Visibility),
    number(Preference),
    argument_schema(Action, Schema),
    valid_schema(Schema),
    once(action_form(Action, verb, _, _)).

valid_target(ui).
valid_target(control).
valid_target(local).

valid_visibility(visible).
valid_visibility(internal).

valid_schema([]).
valid_schema([arg(Name, Kind, Cardinality, Shape)|Schema]) :-
    atom(Name),
    valid_kind(Kind),
    valid_cardinality(Cardinality),
    valid_shape(Cardinality, Shape),
    valid_schema(Schema).

valid_kind(boolean).
valid_kind(integer).
valid_kind(string).
valid_kind(path).
valid_kind(base64).
valid_kind(spec).

valid_cardinality(required).
valid_cardinality(optional).
valid_cardinality(repeated).

valid_shape(required, scalar).
valid_shape(optional, scalar).
valid_shape(repeated, array).
valid_shape(repeated, spread).

%! action_form(?Action, ?Style, ?Specs, ?Normalizer) is nondet.
%
% A form is a sequence of `literal/5` and `argument/1` specs. Normalizers
% relate source-form arguments to the handler's typed wire arguments.

action_form(Action, verb, Specs, Normalizer) :-
    action(Action, _, _, _, _, _, _),
    argument_schema(Action, Schema),
    atom_string(Action, Text),
    canonical_normalizer(Action, Normalizer),
    schema_specs(Schema, ArgSpecs),
    Specs = [literal(Action, Text, action_identifier, Action, 30)|ArgSpecs].
action_form(Action, cli, Specs, Normalizer) :-
    cli_form(Action, Words, Normalizer),
    cli_source_schema(Action, Normalizer, Schema),
    cli_literal_specs(Words, Action, Literals),
    schema_specs(Schema, ArgSpecs),
    append(Literals, ArgSpecs, Specs).

canonical_normalizer(mirror_resume, resume_false) :- !.
canonical_normalizer(_, identity).

cli_source_schema(mirror_pause, pause_true,
                  [arg(id, integer, required, scalar)]) :- !.
cli_source_schema(Action, _, Schema) :-
    argument_schema(Action, Schema).

schema_specs([], []).
schema_specs([Arg|Schema], [argument(Arg)|Specs]) :-
    schema_specs(Schema, Specs).

cli_literal_specs(Words, Action, Specs) :-
    cli_literal_specs(Words, Action, first, Specs).

cli_literal_specs([], _, _, []).
cli_literal_specs([Text|Words], Action, Position,
                  [literal(Semantic, Text, Syntax, Action, Preference)|Specs]) :-
    atom_string(Semantic, Text),
    cli_literal_metadata(Position, Syntax, Preference),
    cli_literal_specs(Words, Action, rest, Specs).

cli_literal_metadata(first, command_namespace, 10).
cli_literal_metadata(rest, action_word, 20).

%! parse(+Items, -Result) is nondet.

parse(Items, Result) :-
    parse(Items, exact, Result).

%! parse(+Items, +Mode, -Result) is nondet.

parse(Items, Mode,
      parse_result(command(Action, Handler, Target, WireArgs), Status,
                   Evidence, Preference)) :-
    input_body(Items, Body, End),
    valid_body(Body, End),
    valid_mode(Mode),
    action(Action, Handler, Target, _, _, _, ActionPreference),
    valid_action(Action),
    action_form(Action, _Style, Specs, Normalizer),
    match_specs(Specs, Body, Mode, SourceArgs, Evidence, EditCount),
    normalize_args(Normalizer, SourceArgs, WireArgs),
    parse_status(Mode, EditCount, Status),
    evidence_preference(Evidence, ActionPreference, Preference).

valid_mode(exact).
valid_mode(assist(_)).

parse_status(exact, 0, complete).
parse_status(assist(EditId), 1, incomplete(edit(EditId))).

normalize_args(identity, Args, Args).
normalize_args(pause_true, Args, WireArgs) :-
    append(Args, [boolean(true)], WireArgs).
normalize_args(resume_false, Args, WireArgs) :-
    append(Args, [boolean(false)], WireArgs).

denormalize_args(identity, Args, Args).
denormalize_args(pause_true, WireArgs, Args) :-
    append(Args, [boolean(true)], WireArgs).
denormalize_args(resume_false, WireArgs, Args) :-
    append(Args, [boolean(false)], WireArgs).

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

% Keep the embedded application core-only. SWI's boot image provides append/1
% but not library(lists)' append/3.
append([], Tail, Tail).
append([Head|Items], Tail, [Head|Result]) :-
    append(Items, Tail, Result).

source_unit(unit(_, Span, PaintSpans, Surface, _, _, Preference, Origin),
            Span, PaintSpans, Surface, Preference, Origin).

match_specs([], [], _Mode, [], [], 0).
match_specs([literal(Semantic, Text, Syntax, Description, LitPreference)|Specs],
            [Item|Items], Mode, Args,
            [evidence(Semantic, Span, PaintSpans, Surface, Syntax,
                      Description, Preference, Origin)|Evidence], EditCount) :-
    source_unit(Item, Span, PaintSpans, Surface, SourcePreference, Origin),
    text_string(Surface, SurfaceString),
    SurfaceString = Text,
    Preference is SourcePreference + LitPreference,
    match_specs(Specs, Items, Mode, Args, Evidence, EditCount).
match_specs([literal(_Semantic, Text, _Syntax, _Description, _Preference)|Specs],
            [edit_tear(EditId, _Span, Surface)|Items], assist(EditId), Args,
            Evidence, EditCount) :-
    surface_prefix(Surface, Text),
    match_specs(Specs, Items, assist(EditId), Args, Evidence, RestCount),
    EditCount is RestCount + 1.
match_specs([argument(arg(Name, Kind, required, scalar))|Specs],
            [Item|Items], Mode, [Value|Args], [EvidenceItem|Evidence],
            EditCount) :-
    match_argument_item(Name, Kind, Item, Value, EvidenceItem),
    match_specs(Specs, Items, Mode, Args, Evidence, EditCount).
match_specs([argument(arg(Name, Kind, optional, scalar))|Specs],
            [Item|Items], Mode, [Value|Args], [EvidenceItem|Evidence],
            EditCount) :-
    match_argument_item(Name, Kind, Item, Value, EvidenceItem),
    match_specs(Specs, Items, Mode, Args, Evidence, EditCount).
match_specs([argument(arg(_, _, optional, scalar))|Specs], Items, Mode,
            Args, Evidence, EditCount) :-
    match_specs(Specs, Items, Mode, Args, Evidence, EditCount).
match_specs([argument(arg(Name, Kind, repeated, Shape))|Specs], Items0, Mode,
            Args, Evidence, EditCount) :-
    take_repeated(Name, Kind, Items0, Values, RepeatedEvidence, Items),
    repeated_arguments(Shape, Values, Specs, RepeatedArgs),
    match_specs(Specs, Items, Mode, RestArgs, RestEvidence, EditCount),
    append(RepeatedArgs, RestArgs, Args),
    append(RepeatedEvidence, RestEvidence, Evidence).

match_argument_item(Name, Kind, Item, Value,
                    evidence(Value, Span, PaintSpans, Surface, Syntax,
                             Name, Preference, Origin)) :-
    source_unit(Item, Span, PaintSpans, Surface, SourcePreference, Origin),
    parse_argument(Kind, Surface, Value),
    kind_syntax(Kind, Syntax),
    Preference is SourcePreference + 10.

take_repeated(_, _, Items, [], [], Items).
take_repeated(Name, Kind, [Item|Items0], [Value|Values],
              [Evidence|EvidenceItems], Items) :-
    match_argument_item(Name, Kind, Item, Value, Evidence),
    take_repeated(Name, Kind, Items0, Values, EvidenceItems, Items).

repeated_arguments(array, Values, Specs, Args) :-
    ( Values = [_|_]
    -> Args = [array(Values)]
    ; specs_have_argument(Specs)
    -> Args = [array([])]
    ;  Args = []
    ).
repeated_arguments(spread, Values, _Specs, Values).

specs_have_argument([argument(_)|_]) :- !.
specs_have_argument([_|Specs]) :-
    specs_have_argument(Specs).

parse_argument(boolean, Surface, boolean(Value)) :-
    text_string(Surface, Text),
    ( Text = "true" -> Value = true ; Text = "false" -> Value = false ).
parse_argument(integer, Surface, integer(Value)) :-
    text_string(Surface, Text),
    number_string(Value, Text),
    integer(Value).
parse_argument(string, Surface, string(Text)) :- text_string(Surface, Text).
parse_argument(path, Surface, string(Text)) :- text_string(Surface, Text).
parse_argument(base64, Surface, string(Text)) :- text_string(Surface, Text).
parse_argument(spec, Surface, string(Text)) :- text_string(Surface, Text).

kind_syntax(boolean, boolean).
kind_syntax(integer, integer).
kind_syntax(string, string).
kind_syntax(path, path).
kind_syntax(base64, base64).
kind_syntax(spec, spec).

evidence_preference([], Preference, Preference).
evidence_preference([evidence(_, _, _, _, _, _, ItemPreference, _)|Evidence],
                    Accumulator, Preference) :-
    Next is Accumulator + ItemPreference,
    evidence_preference(Evidence, Next, Preference).

%! completions(+Items, +EditTearId, -Completions) is det.

completions(Items, EditId, Completions) :-
    ( input_body(Items, Body, End), valid_body(Body, End)
    -> findall(Visible-(Alternative-Preference),
               completion_candidate(Body, EditId, Visible,
                                    Alternative, Preference),
               Pairs),
       merge_completion_pairs(Pairs, Candidates),
       sort_candidates(Candidates, Sorted),
       rank_completions(Sorted, 1, Completions)
    ;  Completions = []
    ).

completion_candidate(Body, EditId, completion_key(Span, Text),
                     alternative(Semantic, Syntax, Description), Preference) :-
    action(Action, _, _, _, _, visible, ActionPreference),
    action_form(Action, _Style, Specs, _Normalizer),
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

match_known_item(literal(_, Text, _, _, _), Item) :-
    source_unit(Item, _, _, Surface, _, _),
    text_string(Surface, Text).
match_known_item(argument(arg(_, Kind, _, _)), Item) :-
    source_unit(Item, _, _, Surface, _, _),
    parse_argument(Kind, Surface, _).

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

render(command(Action, Handler, Target, WireArgs), Style, Text) :-
    action(Action, Handler, Target, _, _, _, _),
    action_form(Action, Style, Specs, Normalizer),
    denormalize_args(Normalizer, WireArgs, SourceArgs),
    render_specs(Specs, SourceArgs, Parts),
    join_parts(Parts, Text).

render_specs([], [], []).
render_specs([literal(_, Text, _, _, _)|Specs], Args, [Text|Parts]) :-
    render_specs(Specs, Args, Parts).
render_specs([argument(arg(_, _, required, scalar))|Specs],
             [Value|Args], [Text|Parts]) :-
    render_value(Value, Text),
    render_specs(Specs, Args, Parts).
render_specs([argument(arg(_, _, optional, scalar))|Specs], Args0, Parts) :-
    minimum_arguments(Specs, RequiredAfter),
    list_length(Args0, Available),
    ( Available > RequiredAfter
    -> Args0 = [Value|Args], render_value(Value, Text), Parts = [Text|Rest]
    ;  Args = Args0, Parts = Rest
    ),
    render_specs(Specs, Args, Rest).
render_specs([argument(arg(_, _, repeated, array))|Specs], Args0, Parts) :-
    ( Args0 = [array(Values)|Args]
    -> render_values(Values, ValueParts), append(ValueParts, Rest, Parts)
    ;  Args = Args0, Parts = Rest
    ),
    render_specs(Specs, Args, Rest).
render_specs([argument(arg(_, _, repeated, spread))|Specs], Args0, Parts) :-
    minimum_arguments(Specs, RequiredAfter),
    list_length(Args0, Available),
    Take is Available - RequiredAfter,
    Take >= 0,
    split_count(Take, Args0, Values, Args),
    render_values(Values, ValueParts),
    append(ValueParts, Rest, Parts),
    render_specs(Specs, Args, Rest).

minimum_arguments([], 0).
minimum_arguments([literal(_, _, _, _, _)|Specs], Count) :-
    minimum_arguments(Specs, Count).
minimum_arguments([argument(arg(_, _, required, scalar))|Specs], Count) :-
    minimum_arguments(Specs, Rest), Count is Rest + 1.
minimum_arguments([argument(arg(_, _, optional, scalar))|Specs], Count) :-
    minimum_arguments(Specs, Count).
minimum_arguments([argument(arg(_, _, repeated, _))|Specs], Count) :-
    minimum_arguments(Specs, Count).

list_length([], 0).
list_length([_|Items], Length) :-
    list_length(Items, Rest), Length is Rest + 1.

split_count(0, Items, [], Items) :- !.
split_count(Count, [Item|Items], [Item|Taken], Rest) :-
    Count > 0, Next is Count - 1, split_count(Next, Items, Taken, Rest).

render_values([], []).
render_values([Value|Values], [Text|Texts]) :-
    render_value(Value, Text), render_values(Values, Texts).

render_value(string(Text), Text).
render_value(integer(Value), Text) :- number_string(Value, Text).
render_value(boolean(Value), Text) :- atom_string(Value, Text).

join_parts([], "").
join_parts([Part], Part).
join_parts([Part|Parts], Text) :-
    Parts = [_|_],
    join_parts(Parts, Rest),
    string_concat(Part, " ", Prefix),
    string_concat(Prefix, Rest, Text).

%! catalog(+Visibility, -Rows) is det.

catalog(Visibility, Rows) :-
    findall(action_info(Action, Handler, Target, Schema, Notation,
                        Description, RowVisibility, Preference,
                        Representations),
            ( action(Action, Handler, Target, Notation, Description,
                     RowVisibility, Preference),
              visibility_matches(Visibility, RowVisibility),
              argument_schema(Action, Schema),
              findall(representation(Kind, Value),
                      representation(Action, Kind, Value),
                      Representations)
            ),
            Rows).

visibility_matches(all, _).
visibility_matches(visible, visible).
visibility_matches(internal, internal).

%! application(+Operation, +InputString, -OutputString) is det.

application(Operation, InputString, OutputString) :-
    ( atom(Operation)
    -> application_atom(Operation, InputString, Response)
    ;  Response = error(invalid_operation)
    ),
    term_string(Response, OutputString,
                [quoted(true), ignore_ops(true), numbervars(true)]).

application_atom(Operation, InputString, Response) :-
    ( decode_request(InputString, Request)
    -> dispatch_application(Operation, Request, Response)
    ;  Response = error(invalid_request)
    ).

decode_request(InputString, Request) :-
    string(InputString),
    catch(term_string(Request, InputString, [syntax_errors(error)]), _, fail),
    ground(Request).

dispatch_application(parse, request(Items, Mode), ok(Results)) :-
    !, findall(Result, parse(Items, Mode, Result), Results).
dispatch_application(complete, request(Items, EditId), ok(Completions)) :-
    !, completions(Items, EditId, Completions).
dispatch_application(highlights, request(Result), ok(Highlights)) :-
    !, ( highlights(Result, Highlights) -> true ; Highlights = [] ).
dispatch_application(render, request(Command, Style), Response) :-
    !, ( render(Command, Style, Text)
       -> Response = ok(Text)
       ;  Response = error(no_solution)
       ).
dispatch_application(catalog, request(Visibility), ok(Rows)) :-
    !, catalog(Visibility, Rows).
dispatch_application(convert, request(FromKind, From, ToKind), ok(Results)) :-
    !, findall(To, convert(FromKind, From, ToKind, To), Results).
dispatch_application(context_query, request(Query, Snapshot), ok(Outcome)) :-
    !, ( context_query(Query, Snapshot, Result)
       -> Outcome = some(Result)
       ;  Outcome = none
       ).
dispatch_application(context_observe, request(Id, Query, Snapshot), ok(Observation)) :-
    !, observe_query(Id, Query, Snapshot, Observation).
dispatch_application(context_ready, request(Graph, Observations), ok(Ready)) :-
    !, ready_queries(Graph, Observations, Ready).
dispatch_application(parse, _, error(invalid_request)) :- !.
dispatch_application(complete, _, error(invalid_request)) :- !.
dispatch_application(highlights, _, error(invalid_request)) :- !.
dispatch_application(render, _, error(invalid_request)) :- !.
dispatch_application(catalog, _, error(invalid_request)) :- !.
dispatch_application(convert, _, error(invalid_request)) :- !.
dispatch_application(context_query, _, error(invalid_request)) :- !.
dispatch_application(context_observe, _, error(invalid_request)) :- !.
dispatch_application(context_ready, _, error(invalid_request)) :- !.
dispatch_application(_, _, error(invalid_operation)).
