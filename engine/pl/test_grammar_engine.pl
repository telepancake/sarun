:- module(test_grammar_engine, [run_grammar_engine_tests/0]).

:- use_module(grammar_engine).

/*
This is deliberately not an action_grammar test.  It is a small foreign
grammar proving that the sequence engine can parse, render, repeat, and expose
tear evidence without importing the sarun catalog or knowing its terminal
kinds.
*/

test_name(parse_foreign_sequence).
test_name(render_foreign_sequence).
test_name(repeat_foreign_field).
test_name(tear_uses_ordinary_relation_and_leaves_required_hole).
test_name(neutral_source_validation_is_engine_owned).
test_name(completion_projection_groups_ranks_and_keeps_ambiguity).
test_name(highlighting_is_evidence_projection).

run_grammar_engine_tests :-
    findall(Name, test_name(Name), Names),
    run_test_names(Names, 0, Passed),
    format('% grammar_engine: ~d tests passed~n', [Passed]).

run_test_names([], Passed, Passed).
run_test_names([Name|Names], Passed0, Passed) :-
    format('% grammar_engine:~w ... ', [Name]),
    catch(( once(run_test(Name))
          -> writeln(passed)
          ;  throw(test_failed(Name))
          ),
          Error,
          ( format('FAILED: ~w~n', [Error]), throw(Error) )),
    Passed1 is Passed0 + 1,
    run_test_names(Names, Passed1, Passed).

expect_equal(Actual, Expected) :-
    ( Actual == Expected -> true ; throw(expected(Expected, got(Actual))) ).

foreign_terminal(surface(word, word(Surface), Surface)) :- string(Surface).
foreign_terminal(syntax(word, foreign_word)).

foreign_specs([
    literal(greeting, "hello", foreign_keyword, greeting, 20),
    argument(arg(name, word, required, scalar))
]).

source(Surface, Start,
       unit(ignored, span(Start, Stop), [span(Start, Stop)], Surface,
            source, foreign_source, 3, foreign_test)) :-
    string_length(Surface, Length),
    Stop is Start + Length.

run_test(parse_foreign_sequence) :-
    foreign_specs(Specs),
    source("hello", 0, Hello),
    source("world", 6, World),
    relate_sequence(Specs, [Hello, World], exact,
                    test_grammar_engine:foreign_terminal,
                    Arguments, Evidence, EditCount),
    expect_equal(Arguments, [word("world")]),
    expect_equal(EditCount, 0),
    Evidence = [evidence(greeting, _, _, "hello", foreign_keyword, _, 23,
                         foreign_test),
                evidence(word("world"), _, _, "world", foreign_word, name,
                         13, foreign_test)].

run_test(render_foreign_sequence) :-
    foreign_specs(Specs),
    relate_sequence(Specs, Items, render,
                    test_grammar_engine:foreign_terminal,
                    [word("world")], Evidence, EditCount),
    expect_equal(Items, [rendered("hello"), rendered("world")]),
    expect_equal(Evidence, [rendered, rendered]),
    expect_equal(EditCount, 0).

run_test(repeat_foreign_field) :-
    Specs = [argument(arg(words, word, repeated, array))],
    source("one", 0, One),
    source("two", 4, Two),
    relate_sequence(Specs, [One, Two], exact,
                    test_grammar_engine:foreign_terminal,
                    [array([word("one"), word("two")])], Evidence, 0),
    Evidence = [evidence(word("one"), _, _, "one", foreign_word, words,
                         13, foreign_test),
                evidence(word("two"), _, _, "two", foreign_word, words,
                         13, foreign_test)].

run_test(tear_uses_ordinary_relation_and_leaves_required_hole) :-
    foreign_specs(Specs),
    Tear = edit_tear(cursor, span(0, 2), "he"),
    relate_sequence(Specs, [Tear], assist(cursor),
                    test_grammar_engine:foreign_terminal,
                    Arguments, Evidence, EditCount),
    expect_equal(Arguments, [hole(name, word)]),
    expect_equal(EditCount, 1),
    Evidence = [evidence(greeting, span(0, 2), [], "he", foreign_keyword,
                         greeting, 20,
                         tear(cursor, literal("hello")))],
    literal_completion_evidence(cursor, Evidence, span(0, 2), "hello",
                                greeting, foreign_keyword, greeting).

run_test(neutral_source_validation_is_engine_owned) :-
    source("hello", 0, Hello),
    neutral_input([Hello, end(5)], [Hello]),
    Invalid = unit(ignored, span(0, 2), [span(0, 3)], "hi",
                   source, foreign_source, 0, foreign_test),
    \+ neutral_input([Invalid, end(3)], _).

run_test(completion_projection_groups_ranks_and_keeps_ambiguity) :-
    Hello = completion_key(span(0, 2), "hello"),
    Help = completion_key(span(0, 2), "help"),
    Pairs = [
        Hello-(alternative(greeting, foreign_keyword, first)-10),
        Hello-(alternative(greeting, foreign_keyword, first)-20),
        Hello-(alternative(other, foreign_keyword, second)-15),
        Help-(alternative(help, foreign_keyword, help)-30)
    ],
    project_completions(Pairs, Completions),
    expect_equal(
        Completions,
        [completion(span(0, 2), "help",
                    [alternative(help, foreign_keyword, help, 30)], 30, 1),
         completion(span(0, 2), "hello",
                    [alternative(greeting, foreign_keyword, first, 20),
                     alternative(other, foreign_keyword, second, 15)],
                    20, 2)]).

run_test(highlighting_is_evidence_projection) :-
    Evidence = [evidence(word("hello"), span(0, 5),
                         [span(0, 2), span(3, 5)], "hello", foreign_word,
                         name, 10, foreign_test)],
    project_highlights(Evidence, Highlights),
    expect_equal(
        Highlights,
        [highlight(span(0, 2), foreign_word, word("hello"), foreign_test),
         highlight(span(3, 5), foreign_word, word("hello"), foreign_test)]).
