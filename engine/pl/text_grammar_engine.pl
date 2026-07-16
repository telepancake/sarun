:- module(text_grammar_engine, [transform_text_grammar/6]).

:- use_module(grammar_ir).

/** <module> Grammar-independent recursive UTF-8 text relation

Executes the raw-text subset of immutable grammar IR values. Character
matching uses SWI strings, but every externally visible span is an UTF-8 byte
span advanced directly from codepoints. No grammar-specific token name or rule
is known here.
*/

transform_text_grammar(Grammar, Given, Wanted, Observations, Limits, Reply) :-
    valid_grammar(Grammar),
    ( executable_text_grammar(Grammar)
    -> transform_executable_text_grammar(Grammar, Given, Wanted, Observations,
                                         Limits, Reply)
    ;  Reply = reply([], [], [], [diagnostic(unsupported_text_grammar)])
    ).

transform_executable_text_grammar(
    grammar(source(text(utf8)), Root, Rules, []), Given, Wanted, _Observations,
    limits(MaxSolutions, MaxEvidence, _MaxOutputBytes),
    reply(Solutions, [], [], Diagnostics)) :-
    given_value(source, Given, text_source(Text, exact, Origin)),
    string(Text),
    string_length(Text, CharacterCount),
    SearchLimit is MaxSolutions + 1,
    findnsols(SearchLimit, Solution,
              complete_text_solution(Root, Rules, Text, CharacterCount,
                                     Origin, MaxEvidence, Wanted, Solution),
              Candidates),
    Candidates = [_|_],
    limit_solutions(Candidates, MaxSolutions, Solutions, Diagnostics).

executable_text_grammar(grammar(source(text(utf8)), _, Rules, [])) :-
    executable_text_rules(Rules).

executable_text_rules([]).
executable_text_rules([rule(_, Expression)|Rules]) :-
    executable_text_expression(Expression),
    executable_text_rules(Rules).

executable_text_expression(empty).
executable_text_expression(ref(_)).
executable_text_expression(seq(Expressions)) :-
    executable_text_expressions(Expressions).
executable_text_expression(choice(Expressions)) :-
    executable_text_expressions(Expressions).
executable_text_expression(optional(Expression)) :-
    executable_text_expression(Expression).
executable_text_expression(not(Expression)) :-
    executable_text_expression(Expression).
executable_text_expression(repeat(_, _, Expression)) :-
    executable_text_expression(Expression).
executable_text_expression(field(_, Expression)) :-
    executable_text_expression(Expression).
executable_text_expression(literal(_, _, presentation(_))).
executable_text_expression(
    terminal(text(codepoint(_)), presentation(_))).

executable_text_expressions([]).
executable_text_expressions([Expression|Expressions]) :-
    executable_text_expression(Expression),
    executable_text_expressions(Expressions).

complete_text_solution(Root, Rules, Text, CharacterCount, Origin, Maximum,
                       Wanted, solution(Bindings, 0)) :-
    rule_expression(Root, Rules, Expression),
    match_expression(Expression, Rules, Text, Origin, Maximum,
                     cursor(0, 0), cursor(CharacterCount, ByteEnd),
                     Value, Evidence, 0, _Depth),
    length(Evidence, EvidenceCount),
    EvidenceCount =< Maximum,
    Ast = node(Root, span(0, ByteEnd), Value),
    project_highlights(Evidence, Highlights),
    Available = [binding(ast, Ast),
                 binding(evidence, Evidence),
                 binding(highlights, Highlights),
                 binding(status, complete)],
    requested_bindings(Wanted, Available, Bindings).

match_expression(empty, _, _, _, _, Cursor, Cursor, empty, [], Depth, Depth).
match_expression(ref(Name), Rules, Text, Origin, Maximum, Start, End,
                 node(Name, Span, Value), Evidence, Depth0, Depth) :-
    Depth0 < Maximum,
    NextDepth is Depth0 + 1,
    rule_expression(Name, Rules, Expression),
    match_expression(Expression, Rules, Text, Origin, Maximum, Start, End,
                     Value, Evidence, NextDepth, Depth),
    cursor_span(Start, End, Span).
match_expression(seq(Expressions), Rules, Text, Origin, Maximum, Start, End,
                 sequence(Values), Evidence, Depth0, Depth) :-
    match_expressions(Expressions, Rules, Text, Origin, Maximum, Start, End,
                      Values, Evidence, Depth0, Depth).
match_expression(choice(Expressions), Rules, Text, Origin, Maximum, Start, End,
                 choice(Index, Value), Evidence, Depth0, Depth) :-
    expression_choice(Expressions, 1, Index, Expression),
    match_expression(Expression, Rules, Text, Origin, Maximum, Start, End,
                     Value, Evidence, Depth0, Depth).
match_expression(optional(Expression), Rules, Text, Origin, Maximum, Start,
                 End, some(Value), Evidence, Depth0, Depth) :-
    match_expression(Expression, Rules, Text, Origin, Maximum, Start, End,
                     Value, Evidence, Depth0, Depth).
match_expression(optional(_), _, _, _, _, Cursor, Cursor, none, [], Depth,
                 Depth).
match_expression(not(Expression), Rules, Text, Origin, Maximum, Cursor, Cursor,
                 absent, [], Depth, Depth) :-
    \+ match_expression(Expression, Rules, Text, Origin, Maximum, Cursor, _,
                        _, _, Depth, _).
match_expression(repeat(Minimum, MaximumCount, Expression), Rules, Text,
                 Origin, Maximum, Start, End, repeated(Values), Evidence,
                 Depth0, Depth) :-
    match_repetition(Expression, Rules, Text, Origin, Maximum, Minimum,
                     MaximumCount, 0, Start, End, Values, Evidence, Depth0,
                     Depth).
match_expression(field(Name, Expression), Rules, Text, Origin, Maximum, Start,
                 End, field(Name, Value), Evidence, Depth0, Depth) :-
    match_expression(Expression, Rules, Text, Origin, Maximum, Start, End,
                     Value, Evidence, Depth0, Depth).
match_expression(literal(Surface0, Semantic, presentation(Metadata)), _, Text,
                 Origin, _, cursor(CharacterStart, ByteStart),
                 cursor(CharacterEnd, ByteEnd), literal(Semantic), [Item],
                 Depth, Depth) :-
    text_string(Surface0, Surface),
    string_length(Surface, CharacterLength),
    sub_string(Text, CharacterStart, CharacterLength, _, Surface),
    CharacterEnd is CharacterStart + CharacterLength,
    string_utf8_bytes(Surface, ByteLength),
    ByteEnd is ByteStart + ByteLength,
    presentation(Metadata, Syntax, Description, Preference),
    Item = evidence(Semantic, span(ByteStart, ByteEnd),
                    [span(ByteStart, ByteEnd)], Surface, Syntax, Description,
                    Preference, Origin).
match_expression(
    terminal(text(codepoint(Set)), presentation(Metadata)), _, Text, Origin, _,
    cursor(CharacterStart, ByteStart), cursor(CharacterEnd, ByteEnd),
    codepoint(Code), [Item], Depth, Depth) :-
    StringIndex is CharacterStart + 1,
    string_code(StringIndex, Text, Code),
    codepoint_in(Set, Code),
    CharacterEnd is CharacterStart + 1,
    utf8_codepoint_bytes(Code, ByteLength),
    ByteEnd is ByteStart + ByteLength,
    string_codes(Surface, [Code]),
    presentation(Metadata, Syntax, Description, Preference),
    Item = evidence(codepoint(Code), span(ByteStart, ByteEnd),
                    [span(ByteStart, ByteEnd)], Surface, Syntax, Description,
                    Preference, Origin).

match_expressions([], _, _, _, _, Cursor, Cursor, [], [], Depth, Depth).
match_expressions([Expression|Expressions], Rules, Text, Origin, Maximum, Start,
                  End, [Value|Values], Evidence, Depth0, Depth) :-
    match_expression(Expression, Rules, Text, Origin, Maximum, Start, Middle,
                     Value, FirstEvidence, Depth0, Depth1),
    match_expressions(Expressions, Rules, Text, Origin, Maximum, Middle, End,
                      Values, RestEvidence, Depth1, Depth),
    append(FirstEvidence, RestEvidence, Evidence).

match_repetition(_, _, _, _, _, Minimum, _, Count, Cursor, Cursor, [], [],
                 Depth, Depth) :-
    Count >= Minimum.
match_repetition(Expression, Rules, Text, Origin, Maximum, Minimum,
                 MaximumCount, Count, Start, End, [Value|Values], Evidence,
                 Depth0, Depth) :-
    repetition_room(MaximumCount, Count),
    match_expression(Expression, Rules, Text, Origin, Maximum, Start, Middle,
                     Value, FirstEvidence, Depth0, Depth1),
    Middle \= Start,
    NextCount is Count + 1,
    match_repetition(Expression, Rules, Text, Origin, Maximum, Minimum,
                     MaximumCount, NextCount, Middle, End, Values,
                     RestEvidence, Depth1, Depth),
    append(FirstEvidence, RestEvidence, Evidence).

repetition_room(unbounded, _).
repetition_room(Maximum, Count) :- integer(Maximum), Count < Maximum.

expression_choice([Expression|_], Index, Index, Expression).
expression_choice([_|Expressions], Index0, Index, Expression) :-
    Index1 is Index0 + 1,
    expression_choice(Expressions, Index1, Index, Expression).

rule_expression(Name, [rule(Name, Expression)|_], Expression) :- !.
rule_expression(Name, [_|Rules], Expression) :-
    rule_expression(Name, Rules, Expression).

codepoint_in(any, _).
codepoint_in(chars(Characters0), Code) :-
    text_string(Characters0, Characters),
    string_code(_, Characters, Code).
codepoint_in(except(Characters0), Code) :-
    text_string(Characters0, Characters),
    \+ string_code(_, Characters, Code).
codepoint_in(range(Low, High), Code) :-
    integer(Low), integer(High), Low =< Code, Code =< High.
codepoint_in(union(Sets), Code) :- set_member(Set, Sets), codepoint_in(Set, Code).

set_member(Set, [Set|_]).
set_member(Set, [_|Sets]) :- set_member(Set, Sets).

presentation(Metadata, Syntax, Description, Preference) :-
    metadata_value(syntax, Metadata, text, Syntax),
    metadata_value(description, Metadata, grammar, Description),
    metadata_value(preference, Metadata, 0, Preference),
    atom(Syntax), ground(Description), number(Preference).

metadata_value(Name, [meta(Name, Value)|_], _, Value) :- !.
metadata_value(Name, [_|Metadata], Default, Value) :-
    metadata_value(Name, Metadata, Default, Value).
metadata_value(_, [], Default, Default).

cursor_span(cursor(_, ByteStart), cursor(_, ByteEnd), span(ByteStart, ByteEnd)).

string_utf8_bytes(String, Bytes) :-
    string_length(String, Characters),
    string_utf8_bytes(String, 1, Characters, 0, Bytes).

string_utf8_bytes(_, Index, Characters, Bytes, Bytes) :- Index > Characters, !.
string_utf8_bytes(String, Index, Characters, Bytes0, Bytes) :-
    string_code(Index, String, Code),
    utf8_codepoint_bytes(Code, Width),
    Bytes1 is Bytes0 + Width,
    Index1 is Index + 1,
    string_utf8_bytes(String, Index1, Characters, Bytes1, Bytes).

utf8_codepoint_bytes(Code, 1) :- Code =< 0x7f, !.
utf8_codepoint_bytes(Code, 2) :- Code =< 0x7ff, !.
utf8_codepoint_bytes(Code, 3) :- Code =< 0xffff, !.
utf8_codepoint_bytes(Code, 4) :- Code =< 0x10ffff.

project_highlights([], []).
project_highlights(
    [evidence(Semantic, _Span, PaintSpans, _Surface, Syntax, Description,
              _Preference, Origin)|Evidence], Highlights) :-
    paint_highlights(PaintSpans, Syntax, Semantic, Description, Origin, First),
    project_highlights(Evidence, Rest),
    append(First, Rest, Highlights).

paint_highlights([], _, _, _, _, []).
paint_highlights([Span|Spans], Syntax, Semantic, Description, Origin,
                 [highlight(Span, Syntax, Semantic, Origin)|Highlights]) :-
    paint_highlights(Spans, Syntax, Semantic, Description, Origin, Highlights).

requested_bindings([], _, []).
requested_bindings([Name|Names], Available, [binding(Name, Value)|Bindings]) :-
    given_value(Name, Available, Value),
    requested_bindings(Names, Available, Bindings).

given_value(Name, [binding(Name, Value)|_], Value).
given_value(Name, [_|Bindings], Value) :- given_value(Name, Bindings, Value).

limit_solutions(Candidates, Maximum, Solutions,
                [diagnostic(solution_limit(Maximum))]) :-
    take_prefix(Maximum, Candidates, Solutions, Rest),
    Rest = [_|_], !.
limit_solutions(Candidates, _, Candidates, []).

take_prefix(0, Values, [], Values) :- !.
take_prefix(_, [], [], []).
take_prefix(Count, [Value|Values], [Value|Prefix], Rest) :-
    Count > 0,
    Next is Count - 1,
    take_prefix(Next, Values, Prefix, Rest).

text_string(Text, Text) :- string(Text), !.
text_string(Text, String) :- atom_string(Text, String).

append([], Tail, Tail).
append([Head|Items], Tail, [Head|Result]) :- append(Items, Tail, Result).
