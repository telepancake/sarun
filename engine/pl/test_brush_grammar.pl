:- module(test_brush_grammar, [run_brush_grammar_tests/0]).

:- use_module(brush_grammar).
:- use_module(relation_api).

test_name(grammar_is_valid_executable_data).
test_name(word_slice_parses_quotes_expansions_and_utf8).
test_name(unterminated_word_has_no_complete_parse).

run_brush_grammar_tests :-
    findall(Name, test_name(Name), Names),
    run_tests(Names, 0, Passed),
    length(Names, Passed),
    format('% brush_grammar: ~d tests passed~n', [Passed]).

run_tests([], Passed, Passed).
run_tests([Name|Names], Passed0, Passed) :-
    format('% brush_grammar:~w ... ', [Name]),
    ( once(run_test(Name))
    -> writeln(passed), Passed1 is Passed0 + 1
    ;  writeln('FAILED'), fail
    ),
    run_tests(Names, Passed1, Passed).

run_test(grammar_is_valid_executable_data) :-
    brush_relation_grammar(Grammar),
    grammar_ir:valid_grammar(Grammar).

run_test(word_slice_parses_quotes_expansions_and_utf8) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("pré' literal 'x\\ y\"$name:$?$(echo hi)\"$((base + (2)))",
                                           exact, brush_test))]),
                want([ast, status, highlights]), observations([]),
                limits(32, 4096, 1048576)),
        reply([solution([binding(ast, node(shell_word, span(0, 54), _)),
                         binding(status, complete),
                         binding(highlights, Highlights)], 0)], [], [], [])),
    has_highlight(span(0, 1), word, codepoint(112), brush_test, Highlights),
    has_highlight(span(2, 4), word, codepoint(233), brush_test, Highlights),
    has_highlight(_, variable, _, brush_test, Highlights),
    has_highlight(_, arithmetic, _, brush_test, Highlights).

run_test(unterminated_word_has_no_complete_parse) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("before\"unterminated", exact,
                                           brush_test))]),
                want([ast]), observations([]), limits(32, 4096, 1048576)),
        reply([], [], [], [diagnostic(no_solution)])).

has_highlight(Span, Syntax, Semantic, Origin,
              [highlight(Span, Syntax, Semantic, Origin)|_]).
has_highlight(Span, Syntax, Semantic, Origin, [_|Highlights]) :-
    has_highlight(Span, Syntax, Semantic, Origin, Highlights).
