:- module(test_relation_api, [run_relation_api_tests/0]).

:- use_module(relation_api).

test_name(same_request_shape_parses_and_renders_foreign_grammar).
test_name(envelope_fails_closed).

run_relation_api_tests :-
    findall(Name, test_name(Name), Names),
    run_test_names(Names, 0, Passed),
    format('% relation_api: ~d tests passed~n', [Passed]).

run_test_names([], Passed, Passed).
run_test_names([Name|Names], Passed0, Passed) :-
    format('% relation_api:~w ... ', [Name]),
    catch(( once(run_test(Name))
          -> writeln(passed)
          ;  throw(test_failed(Name))
          ),
          Error,
          ( format('FAILED: ~w~n', [Error]), throw(Error) )),
    Passed1 is Passed0 + 1,
    run_test_names(Names, Passed1, Passed).

foreign_grammar(
    sequence_grammar(
        [literal(greeting, "hello", keyword, greeting, 20),
         argument(arg(name, word, required, scalar))],
        terminals([
            terminal(word, identifier,
                     [surface(word("world"), "world"),
                      surface(word("friend"), "friend")])
        ]),
        separator(" "))).

source(Surface, Start,
       unit(ignored, span(Start, Stop), [span(Start, Stop)], Surface,
            source, foreign_source, 3, foreign_test)) :-
    string_length(Surface, Length),
    Stop is Start + Length.

limits(limits(16, 256, 65536)).

run_test(same_request_shape_parses_and_renders_foreign_grammar) :-
    foreign_grammar(Grammar),
    limits(Limits),
    source("hello", 0, Hello),
    source("world", 6, World),
    ParseRequest = request(
        Grammar,
        given([binding(source, source([Hello, World, end(11)], exact))]),
        want([arguments, status, highlights]), observations([]), Limits),
    transform(ParseRequest, ParseReply),
    ParseReply = reply(
        [solution([binding(arguments, [word("world")]),
                   binding(status, complete),
                   binding(highlights,
                           [highlight(span(0, 5), keyword, greeting,
                                      foreign_test),
                            highlight(span(6, 11), identifier, word("world"),
                                      foreign_test)])],
                  36)], [], [], []),
    RenderRequest = request(
        Grammar, given([binding(arguments, [word("world")])]),
        want([source]), observations([]), Limits),
    transform(RenderRequest, RenderReply),
    RenderReply = reply(
        [solution([binding(source, "hello world")], 0)], [], [], []).

run_test(envelope_fails_closed) :-
    foreign_grammar(Grammar),
    transform(request(Grammar, given([]), want([]), observations([]),
                      limits(0, 0, 0)),
              reply([], [], [], [diagnostic(invalid_request)])).
