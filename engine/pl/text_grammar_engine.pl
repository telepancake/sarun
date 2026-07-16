:- module(text_grammar_engine, [transform_text_grammar/6]).

:- use_module(grammar_ir).
:- use_module(evidence_projection).

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
    given_value(source, Given, Source),
    text_source_context(Source, Text, CharacterCount, Context, Status),
    SearchLimit is MaxSolutions + 1,
    findnsols(SearchLimit, Candidate,
              complete_text_candidate(Root, Rules, Text, CharacterCount,
                                      Context, Status, MaxEvidence, Candidate),
              Candidates0),
    Candidates0 = [_|_],
    text_candidate_completions(Candidates0, Completions),
    text_candidate_solutions(Candidates0, Wanted, Completions, Solutions0),
    limit_solutions(Solutions0, MaxSolutions, Solutions, Diagnostics).

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
executable_text_expression(extras(ExtraExpressions, Expression)) :-
    executable_text_expressions(ExtraExpressions),
    executable_text_expression(Expression).
executable_text_expression(lexical(Expression)) :-
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

text_source_context(text_source(Text, exact, Origin), Text, CharacterCount,
                    parse_context(source_context(Origin, no_tear), []),
                    complete) :-
    string(Text),
    ground(Origin),
    string_length(Text, CharacterCount).
text_source_context(
    text_source(Text, assist(EditId, span(ByteStart, ByteEnd)), Origin), Text,
    CharacterCount,
    parse_context(
        source_context(Origin,
                       tear(EditId, cursor(CharacterStart, ByteStart),
                            cursor(CharacterEnd, ByteEnd), Surface)),
        []),
    incomplete(edit(EditId))) :-
    string(Text),
    atom(EditId),
    ground(Origin),
    integer(ByteStart), integer(ByteEnd), 0 =< ByteStart, ByteStart =< ByteEnd,
    byte_character_cursor(Text, ByteStart, CharacterStart),
    byte_character_cursor(Text, ByteEnd, CharacterEnd),
    CharacterLength is CharacterEnd - CharacterStart,
    sub_string(Text, CharacterStart, CharacterLength, _, Surface),
    string_length(Text, CharacterCount).

byte_character_cursor(Text, TargetByte, Character) :-
    byte_character_cursor(Text, TargetByte, 1, 0, 0, Character).

byte_character_cursor(_, TargetByte, _, Character, TargetByte, Character) :- !.
byte_character_cursor(Text, TargetByte, Index, Character0, Byte0, Character) :-
    Byte0 < TargetByte,
    string_code(Index, Text, Code),
    utf8_codepoint_bytes(Code, Width),
    Byte1 is Byte0 + Width,
    Byte1 =< TargetByte,
    Index1 is Index + 1,
    Character1 is Character0 + 1,
    byte_character_cursor(Text, TargetByte, Index1, Character1, Byte1,
                          Character).

source_origin(parse_context(source_context(Origin, _), _), Origin).

initial_text_cursor(parse_context(source_context(_, no_tear), _),
                    cursor(0, 0, absent)).
initial_text_cursor(parse_context(source_context(_, tear(_, _, _, _)), _),
                    cursor(0, 0, unused)).

complete_text_cursor(parse_context(source_context(_, no_tear), _),
                     CharacterCount, ByteEnd,
                     cursor(CharacterCount, ByteEnd, absent)).
complete_text_cursor(parse_context(source_context(_, tear(_, _, _, _)), _),
                     CharacterCount,
                     ByteEnd, cursor(CharacterCount, ByteEnd, used)).

source_tear_at(
    parse_context(
        source_context(_,
                       tear(EditId, cursor(CharacterStart, ByteStart),
                            cursor(CharacterEnd, ByteEnd), Surface)),
        _),
    cursor(CharacterStart, ByteStart, unused), EditId,
    cursor(CharacterEnd, ByteEnd, used), Surface).

context_with_extras(parse_context(Source, Existing), Added,
                    parse_context(Source, Combined)) :-
    append(Added, Existing, Combined).

context_without_extras(parse_context(Source, _), parse_context(Source, [])).

context_extras(parse_context(_, Extras), Extras).

complete_text_candidate(Root, Rules, Text, CharacterCount, Context, Status,
                        Maximum, candidate(Available, Evidence)) :-
    rule_expression(Root, Rules, Expression),
    initial_text_cursor(Context, Start),
    match_expression(Expression, Rules, Text, Context, Maximum,
                     Start, End,
                     Value, Evidence, 0, _Depth),
    complete_text_cursor(Context, CharacterCount, ByteEnd, End),
    length(Evidence, EvidenceCount),
    EvidenceCount =< Maximum,
    Ast = node(Root, span(0, ByteEnd), Value),
    project_highlights(Evidence, Highlights),
    Available = [binding(ast, Ast),
                 binding(evidence, Evidence),
                 binding(highlights, Highlights),
                 binding(status, Status)].

text_candidate_solutions([], _, _, []).
text_candidate_solutions([candidate(Available0, _)|Candidates], Wanted,
                         Completions, [solution(Bindings, 0)|Solutions]) :-
    Available = [binding(completions, Completions)|Available0],
    requested_bindings(Wanted, Available, Bindings),
    text_candidate_solutions(Candidates, Wanted, Completions, Solutions).

text_candidate_completions(Candidates, Completions) :-
    findall(Pair, text_completion_pair(Candidates, Pair), Pairs),
    project_completions(Pairs, Completions).

text_completion_pair([candidate(_, Evidence)|_],
                     completion_key(Span, Text)-
                     (alternative(Semantic, Syntax, Description)-Preference)) :-
    tear_literal_evidence(Evidence, Span, Text, Semantic, Syntax, Description,
                          Preference).
text_completion_pair([_|Candidates], Pair) :-
    text_completion_pair(Candidates, Pair).

tear_literal_evidence(
    [evidence(Semantic, Span, _, _, Syntax, Description, Preference,
              tear(_, literal(Text)))|_],
    Span, Text, Semantic, Syntax, Description, Preference).
tear_literal_evidence([_|Evidence], Span, Text, Semantic, Syntax, Description,
                      Preference) :-
    tear_literal_evidence(Evidence, Span, Text, Semantic, Syntax, Description,
                          Preference).

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
match_expression(extras(ExtraExpressions, Expression), Rules, Text, Context,
                 Maximum, Start, End,
                 with_extras(Leading, Value, Trailing), Evidence, Depth0,
                 Depth) :-
    context_with_extras(Context, ExtraExpressions, InnerContext),
    context_without_extras(InnerContext, TriviaContext),
    match_trivia(ExtraExpressions, Rules, Text, TriviaContext, Maximum, Start,
                 BodyStart, Leading, LeadingEvidence, Depth0, Depth1),
    match_expression(Expression, Rules, Text, InnerContext, Maximum, BodyStart,
                     BodyEnd, Value, BodyEvidence, Depth1, Depth2),
    match_trivia(ExtraExpressions, Rules, Text, TriviaContext, Maximum, BodyEnd,
                 End, Trailing, TrailingEvidence, Depth2, Depth),
    append(LeadingEvidence, BodyEvidence, FirstEvidence),
    append(FirstEvidence, TrailingEvidence, Evidence).
match_expression(lexical(Expression), Rules, Text, Context, Maximum, Start, End,
                 lexical(Value), Evidence, Depth0, Depth) :-
    context_without_extras(Context, LexicalContext),
    match_expression(Expression, Rules, Text, LexicalContext, Maximum, Start,
                     End, Value, Evidence, Depth0, Depth).
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
match_expression(literal(Surface0, Semantic, presentation(Metadata)), _, _Text,
                 Context, _, Start, End, literal(Semantic), [Item], Depth,
                 Depth) :-
    source_tear_at(Context, Start, EditId, End, TearSurface),
    text_string(Surface0, Surface),
    string_length(TearSurface, TearLength),
    sub_string(Surface, 0, TearLength, _, TearSurface),
    cursor_span(Start, End, Span),
    presentation(Metadata, Syntax, Description, Preference),
    Item = evidence(Semantic, Span, [], TearSurface, Syntax, Description,
                    Preference, tear(EditId, literal(Surface))).
match_expression(literal(Surface0, Semantic, presentation(Metadata)), _, Text,
                 Context, _, cursor(CharacterStart, ByteStart, TearState),
                 cursor(CharacterEnd, ByteEnd, TearState), literal(Semantic), [Item],
                 Depth, Depth) :-
    text_string(Surface0, Surface),
    string_length(Surface, CharacterLength),
    sub_string(Text, CharacterStart, CharacterLength, _, Surface),
    CharacterEnd is CharacterStart + CharacterLength,
    string_utf8_bytes(Surface, ByteLength),
    ByteEnd is ByteStart + ByteLength,
    source_origin(Context, Origin),
    presentation(Metadata, Syntax, Description, Preference),
    Item = evidence(Semantic, span(ByteStart, ByteEnd),
                    [span(ByteStart, ByteEnd)], Surface, Syntax, Description,
                    Preference, Origin).
match_expression(
    terminal(text(codepoint(Set)), presentation(Metadata)), _, Text, Origin, _,
    cursor(CharacterStart, ByteStart, TearState),
    cursor(CharacterEnd, ByteEnd, TearState),
    codepoint(Code), [Item], Depth, Depth) :-
    StringIndex is CharacterStart + 1,
    string_code(StringIndex, Text, Code),
    codepoint_in(Set, Code),
    CharacterEnd is CharacterStart + 1,
    utf8_codepoint_bytes(Code, ByteLength),
    ByteEnd is ByteStart + ByteLength,
    string_codes(Surface, [Code]),
    source_origin(Origin, EvidenceOrigin),
    presentation(Metadata, Syntax, Description, Preference),
    Item = evidence(codepoint(Code), span(ByteStart, ByteEnd),
                    [span(ByteStart, ByteEnd)], Surface, Syntax, Description,
                    Preference, EvidenceOrigin).

match_expressions([], _, _, _, _, Cursor, Cursor, [], [], Depth, Depth).
match_expressions([Expression], Rules, Text, Context, Maximum, Start, End,
                  [Value], Evidence, Depth0, Depth) :-
    match_expression(Expression, Rules, Text, Context, Maximum, Start, End,
                     Value, Evidence, Depth0, Depth).
match_expressions([Expression, Next|Expressions], Rules, Text, Context, Maximum,
                  Start, End, [ValueWithTrivia|Values], Evidence, Depth0,
                  Depth) :-
    match_expression(Expression, Rules, Text, Context, Maximum, Start, Middle0,
                     Value, FirstEvidence, Depth0, Depth1),
    match_active_trivia(Context, Rules, Text, Maximum, Middle0, Middle, Trivia,
                        TriviaEvidence, Depth1, Depth2),
    attach_trivia(Value, Trivia, ValueWithTrivia),
    match_expressions([Next|Expressions], Rules, Text, Context, Maximum, Middle,
                      End, Values, RestEvidence, Depth2, Depth),
    append(FirstEvidence, TriviaEvidence, BeforeRest),
    append(BeforeRest, RestEvidence, Evidence).

match_active_trivia(Context, Rules, Text, Maximum, Start, End, Values, Evidence,
                    Depth0, Depth) :-
    context_extras(Context, Extras),
    context_without_extras(Context, TriviaContext),
    match_trivia(Extras, Rules, Text, TriviaContext, Maximum, Start, End, Values,
                 Evidence, Depth0, Depth).

match_trivia(Expressions, Rules, Text, Context, Maximum, Start, End,
             [Value|Values], Evidence, Depth0, Depth) :-
    expression_choice(Expressions, 1, _, Expression),
    match_expression(Expression, Rules, Text, Context, Maximum, Start, Middle,
                     Value, FirstEvidence, Depth0, Depth1),
    Middle \= Start,
    !,
    match_trivia(Expressions, Rules, Text, Context, Maximum, Middle, End,
                 Values, RestEvidence, Depth1, Depth),
    append(FirstEvidence, RestEvidence, Evidence).
match_trivia(_, _, _, _, _, Cursor, Cursor, [], [], Depth, Depth).

attach_trivia(Value, [], Value).
attach_trivia(Value, Trivia, with_trailing_trivia(Value, Trivia)) :-
    Trivia = [_|_].

match_repetition(_, _, _, _, _, Minimum, _, Count, Cursor, Cursor, [], [],
                 Depth, Depth) :-
    Count >= Minimum.
match_repetition(Expression, Rules, Text, Origin, Maximum, Minimum,
                 MaximumCount, Count, Start, End,
                 [ValueWithTrivia|Values], Evidence,
                 Depth0, Depth) :-
    repetition_room(MaximumCount, Count),
    match_expression(Expression, Rules, Text, Origin, Maximum, Start, Middle0,
                     Value, FirstEvidence, Depth0, Depth1),
    Middle0 \= Start,
    repetition_gap(Origin, Rules, Text, Maximum, Middle0, Middle, Trivia,
                   TriviaEvidence, Depth1, Depth2),
    NextCount is Count + 1,
    match_repetition(Expression, Rules, Text, Origin, Maximum, Minimum,
                     MaximumCount, NextCount, Middle, End, Values,
                     RestEvidence, Depth2, Depth),
    gap_has_following_value(Trivia, Values),
    attach_trivia(Value, Trivia, ValueWithTrivia),
    append(FirstEvidence, TriviaEvidence, BeforeRest),
    append(BeforeRest, RestEvidence, Evidence).

repetition_gap(_, _, _, _, Cursor, Cursor, [], [], Depth, Depth).
repetition_gap(Context, Rules, Text, Maximum, Start, End, Trivia, Evidence,
               Depth0, Depth) :-
    match_active_trivia(Context, Rules, Text, Maximum, Start, End, Trivia,
                        Evidence, Depth0, Depth),
    Trivia = [_|_].

gap_has_following_value([], _).
gap_has_following_value([_|_], [_|_]).

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

cursor_span(cursor(_, ByteStart, _), cursor(_, ByteEnd, _),
            span(ByteStart, ByteEnd)).

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
