:- module(test_relation_api, [run_relation_api_tests/0]).

:- use_module(relation_api).
:- use_module(grammar_store).

test_name(same_request_shape_parses_and_renders_foreign_grammar).
test_name(tear_completion_is_aggregated_from_parse_evidence).
test_name(context_queries_completions_and_dependencies_are_one_relation).
test_name(context_support_uses_the_same_given_wanted_envelope).
test_name(grammar_choice_and_projection_are_executable_data).
test_name(projection_template_is_bidirectional_and_can_append_values).
test_name(choice_namespaces_context_dependencies).
test_name(grammar_terminal_codecs_are_declarative_and_bidirectional).
test_name(opaque_handle_resolves_install_once_grammar).
test_name(solution_limit_is_enforced_and_reported).
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
        separator(" "),
        contexts([]))).

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

run_test(tear_completion_is_aggregated_from_parse_evidence) :-
    foreign_grammar(Grammar),
    limits(Limits),
    Tear = edit_tear(edit, span(0, 2), "he"),
    Request = request(
        Grammar,
        given([binding(source, source([Tear, end(2)], assist(edit)))]),
        want([arguments, status, completions]), observations([]), Limits),
    transform(Request, Reply),
    Reply = reply(
        [solution([binding(arguments, [hole(name, word)]),
                   binding(status, incomplete(edit(edit))),
                   binding(completions,
                           [completion(span(0, 2), "hello",
                                       [alternative(greeting, keyword,
                                                    greeting, 20)],
                                       20, 1)])],
                  20)], [], [], []).

run_test(solution_limit_is_enforced_and_reported) :-
    Ambiguous = sequence_grammar(
        [argument(arg(name, word, required, scalar))],
        terminals([
            terminal(word, identifier,
                     [surface(first, "same"), surface(second, "same")])
        ]), separator(" "), contexts([])),
    source("same", 0, Same),
    Request = request(
        Ambiguous,
        given([binding(source, source([Same, end(4)], exact))]),
        want([arguments]), observations([]), limits(1, 10, 65536)),
    transform(Request, Reply),
    Reply = reply([solution([binding(arguments, [_])], _)], [], [],
                  [diagnostic(solution_limit(1))]).

run_test(context_queries_completions_and_dependencies_are_one_relation) :-
    Grammar = sequence_grammar(
        [literal(open, "open", keyword, open, 20),
         argument(arg(name, word, required, scalar))],
        terminals([
            terminal(word, identifier,
                     [surface(word("wo"), "wo")])
        ]), separator(" "), contexts([context(name, one, object, root)])),
    source("open", 0, Open),
    source("wo", 5, Word),
    Tear = edit_tear(edit, span(5, 7), "wo"),
    Limits = limits(16, 256, 65536),
    ExactRequest = request(
        Grammar,
        given([binding(source, source([Open, Word, end(7)], exact))]),
        want([arguments]), observations([]), Limits),
    transform(ExactRequest, ExactReply),
    ExactReply = reply(_, [query(q(1), ask(one, object, name("wo")))],
                       [], []),
    Request0 = request(
        Grammar,
        given([binding(source,
                       source([Open, Tear, end(7)], assist(edit)))]),
        want([arguments, completions]), observations([]), Limits),
    transform(Request0, Reply0),
    Reply0 = reply(_, [query(q(1), Query)], [], []),
    Query = ask(all, object, prefix("wo")),
    Entries = [entry(object, 7, ["work"], object_id(7), [])],
    Observation = observed(q(1), Query, source(objects, 12),
                           some(all(Entries))),
    Request1 = request(
        Grammar,
        given([binding(source,
                       source([Open, Tear, end(7)], assist(edit)))]),
        want([completions]), observations([Observation]), Limits),
    transform(Request1, Reply1),
    Reply1 = reply(
        [solution([binding(completions,
                           [completion(span(5, 7), "work",
                                       [alternative(context(object, 7),
                                                    context_argument,
                                                    objects, 33)],
                                       33, 1)])], 33)],
        [query(q(1), Query)],
        [dependency(q(1), Query, some(all(Entries)))], []).

run_test(context_support_uses_the_same_given_wanted_envelope) :-
    Query = ask(one, object, name("work")),
    Entry = entry(object, 7, ["work"], object_id(7), []),
    Snapshot = snapshot(source(objects, 12), [Entry]),
    Limits = limits(16, 256, 65536),
    transform(
        request(context_grammar,
                given([binding(query, Query), binding(snapshot, Snapshot)]),
                want([outcome]), observations([]), Limits),
        reply([solution([binding(outcome, some(one(Entry)))], 0)], [], [], [])),
    transform(
        request(context_grammar,
                given([binding(id, object_query), binding(query, Query),
                       binding(snapshot, Snapshot)]),
                want([observation]), observations([]), Limits),
        reply([solution([binding(observation, Observation)], 0)], [],
              [Dependency], [])),
    Observation = observed(object_query, Query, source(objects, 12),
                           some(one(Entry))),
    Dependency = dependency(object_query, Query, some(one(Entry))),
    transform(
        request(context_grammar, given([]), want([dependency_keys]),
                observations([Observation]), Limits),
        reply([solution([binding(dependency_keys, [Dependency])], 0)], [],
              [Dependency], [])).

run_test(grammar_choice_and_projection_are_executable_data) :-
    Terminals = terminals([
        terminal(word, identifier, [surface(word("world"), "world")])
    ]),
    Greeting = projection_grammar(
        sequence_grammar(
            [literal(hello, "hello", keyword, greeting, 20),
             argument(arg(name, word, required, scalar))],
            Terminals, separator(" "), contexts([])),
        [projection(semantic,
                    structure(message,
                              [constant(greeting), reference(arguments)]))]),
    Farewell = projection_grammar(
        sequence_grammar(
            [literal(goodbye, "goodbye", keyword, farewell, 20),
             argument(arg(name, word, required, scalar))],
            Terminals, separator(" "), contexts([])),
        [projection(semantic,
                    structure(message,
                              [constant(farewell), reference(arguments)]))]),
    Grammar = choice_grammar([
        alternative(greeting, 7, Greeting),
        alternative(farewell, 3, Farewell)
    ]),
    source("hello", 0, Hello),
    source("world", 6, World),
    limits(Limits),
    transform(
        request(Grammar,
                given([binding(source,
                               source([Hello, World, end(11)], exact))]),
                want([semantic]), observations([]), Limits),
        reply([solution([binding(semantic,
                                 message(greeting, [word("world")]))], 43)],
              [], [], [])),
    transform(
        request(Grammar,
                given([binding(semantic,
                               message(farewell, [word("world")]))]),
                want([source]), observations([]), Limits),
        reply([solution([binding(source, "goodbye world")], 3)], [], [], [])).

run_test(projection_template_is_bidirectional_and_can_append_values) :-
    Grammar = projection_grammar(
        sequence_grammar(
            [literal(resume, "resume", keyword, resume, 20),
             argument(arg(id, integer, required, scalar))],
            terminals([
                terminal(integer, integer,
                         [surface(integer(7), "7")])
            ]), separator(" "), contexts([])),
        [projection(semantic,
                    structure(command,
                              [constant(mirror_pause),
                               concatenate(reference(arguments),
                                           sequence([
                                               constant(boolean(false))
                                           ]))]))]),
    source("resume", 0, Resume),
    source("7", 7, Seven),
    limits(Limits),
    Semantic = command(mirror_pause, [integer(7), boolean(false)]),
    transform(
        request(Grammar,
                given([binding(source,
                               source([Resume, Seven, end(8)], exact))]),
                want([semantic]), observations([]), Limits),
        reply([solution([binding(semantic, Semantic)], 36)], [], [], [])),
    transform(
        request(Grammar, given([binding(semantic, Semantic)]), want([source]),
                observations([]), Limits),
        reply([solution([binding(source, "resume 7")], 0)], [], [], [])).

run_test(choice_namespaces_context_dependencies) :-
    Grammar = choice_grammar([
        alternative(open_form, 0,
            sequence_grammar(
                [literal(open, "open", keyword, open, 20),
                 argument(arg(name, word, required, scalar))],
                terminals([
                    terminal(word, identifier, [surface(word("wo"), "wo")])
                ]), separator(" "),
                contexts([context(name, one, object, root)])))
    ]),
    source("open", 0, Open),
    source("wo", 5, Word),
    limits(Limits),
    Query = ask(one, object, name("wo")),
    transform(
        request(Grammar,
                given([binding(source,
                               source([Open, Word, end(7)], exact))]),
                want([arguments]), observations([]), Limits),
        reply(_, [query(branch(open_form, q(1)), Query)], [], [])),
    Entry = entry(object, 7, ["work"], object_id(7), []),
    Observation = observed(branch(open_form, q(1)), Query,
                           source(objects, 12), some(one(Entry))),
    transform(
        request(Grammar,
                given([binding(source,
                               source([Open, Word, end(7)], exact))]),
                want([arguments]), observations([Observation]), Limits),
        reply(_, [query(branch(open_form, q(1)), Query)],
              [dependency(branch(open_form, q(1)), Query, some(one(Entry)))],
              [])).

run_test(grammar_terminal_codecs_are_declarative_and_bidirectional) :-
    Shape = object(build,
                   [field("context", string),
                    field("tag", nullable(string, none, some)),
                    field("arguments", array(tuple(pair, [string, string])))]),
    Grammar = sequence_grammar(
        [literal(build, "build", keyword, build, 20),
         argument(arg(specification, build_spec, required, scalar))],
        terminals([
            terminal(build_spec, structured_spec, codec(json(Shape)))
        ]), separator(" "), contexts([])),
    Json = "{\"arguments\":[[\"CC\",\"clang\"]],\"tag\":null,\"context\":\"eA==\"}",
    source("build", 0, Build),
    source(Json, 6, Specification),
    string_length(Json, JsonLength),
    End is JsonLength + 6,
    limits(Limits),
    Semantic = build("eA==", none, [pair("CC", "clang")]),
    transform(
        request(Grammar,
                given([binding(source,
                               source([Build, Specification, end(End)],
                                      exact))]),
                want([arguments]), observations([]), Limits),
        reply([solution([binding(arguments, [Semantic])], _)], [], [], [])),
    transform(
        request(Grammar, given([binding(arguments, [Semantic])]),
                want([source]), observations([]), Limits),
        reply([solution([binding(source,
                                 "build {\"context\":\"eA==\",\"tag\":null,\"arguments\":[[\"CC\",\"clang\"]]}")],
                        0)], [], [], [])).

run_test(opaque_handle_resolves_install_once_grammar) :-
    foreign_grammar(Grammar),
    install_grammar(foreign_test, Grammar),
    install_grammar(foreign_test, Grammar),
    source("hello", 0, Hello),
    source("friend", 6, Friend),
    limits(Limits),
    transform(
        request(grammar_handle(foreign_test),
                given([binding(source,
                               source([Hello, Friend, end(12)], exact))]),
                want([arguments]), observations([]), Limits),
        reply([solution([binding(arguments, [word("friend")])], 36)],
              [], [], [])),
    Different = sequence_grammar([], terminals([]), separator(" "),
                                 contexts([])),
    \+ install_grammar(foreign_test, Different),
    transform(
        request(grammar_handle(missing), given([]), want([source]),
                observations([]), Limits),
        reply([], [], [], [diagnostic(invalid_request)])).

run_test(envelope_fails_closed) :-
    foreign_grammar(Grammar),
    transform(request(Grammar, given([]), want([]), observations([]),
                      limits(0, 0, 0)),
              reply([], [], [], [diagnostic(invalid_request)])).
