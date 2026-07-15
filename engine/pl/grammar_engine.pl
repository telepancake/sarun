:- module(grammar_engine, [relate_sequence/7]).

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

% Core-only embedded SWI does not load library(lists).
append([], Tail, Tail).
append([Head|Items], Tail, [Head|Result]) :- append(Items, Tail, Result).
