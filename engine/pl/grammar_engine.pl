:- module(grammar_engine,
          [ neutral_input/2,
            valid_relation_mode/1,
            relation_status/3,
            relate_sequence/7,
            evidence_preference/3,
            literal_completion_evidence/7,
            project_completions/2,
            project_highlights/2
          ]).

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
