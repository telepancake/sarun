:- module(test_action_grammar, [run_action_grammar_tests/0]).

:- use_module(action_grammar).
:- discontiguous run_test/1.

% Core-only test runner: this same file runs in the package-free embedded image.
test_name(catalog_is_complete_and_valid).
test_name(neutral_source_parses_canonical_form).
test_name(shared_cli_form_uses_complete_schema).
test_name(alias_normalization_is_wire_ready).
test_name(string_kinds_do_not_become_numbers).
test_name(array_wire_shape_is_preserved).
test_name(parse_render_roundtrip).
test_name(completion_and_highlighting_derive_from_relation).
test_name(application_surface_is_closed).
test_name(context_plan_captures_box_dependency).
test_name(context_resolution_rewrites_wire_argument).
test_name(dependent_path_plan_references_box_query).
test_name(context_completion_uses_all_prefix_query).
test_name(context_completion_resolution_uses_entry_names).
test_name(dependent_context_completion_graph).

run_action_grammar_tests :-
    findall(Name, test_name(Name), Names),
    run_test_names(Names, 0, Passed),
    format('% action_grammar: ~d tests passed~n', [Passed]).

run_test_names([], Passed, Passed).
run_test_names([Name|Names], Passed0, Passed) :-
    format('% action_grammar:~w ... ', [Name]),
    catch(( once(run_test(Name))
          -> writeln(passed)
          ;  throw(test_failed(Name))
          ),
          Error,
          ( format('FAILED: ~w~n', [Error]), throw(Error) )),
    Passed1 is Passed0 + 1,
    run_test_names(Names, Passed1, Passed).

expect(Goal) :-
    ( call(Goal) -> true ; throw(expected_success(Goal)) ).

expect_equal(Actual, Expected) :-
    ( Actual == Expected -> true ; throw(expected(Expected, got(Actual))) ).

neutral(Surface, Start,
        unit(ignored, span(Start, Stop), [span(Start, Stop)], Surface,
             source, command_source, 0, rust)) :-
    string_length(Surface, Length),
    Stop is Start + Length.

items(Surfaces, Items) :- items(Surfaces, 0, Items).
items([], End, [end(End)]).
items([Surface|Surfaces], Start, [Unit|Items]) :-
    neutral(Surface, Start, Unit),
    string_length(Surface, Length),
    Next is Start + Length + 1,
    items(Surfaces, Next, Items).

parse_words(Words, Command) :-
    items(Words, Items),
    once(parse(Items, parse_result(Command, complete, _, _))).

run_test(catalog_is_complete_and_valid) :-
    findall(Action, action(Action, _, _, _, _, _, _), Actions),
    sort(Actions, Unique),
    length(Actions, 114),
    length(Unique, 114),
    expect(all_valid(Actions)).

all_valid([]).
all_valid([Action|Actions]) :-
    valid_action(Action),
    all_valid(Actions).

run_test(neutral_source_parses_canonical_form) :-
    parse_words(["review.map_ids", "12", "process", "3", "4", "edge"],
                command('review.map_ids', 'review.map_ids', ui,
                        [string("12"), string("process"),
                         array([integer(3), integer(4)]), string("edge")])).

run_test(shared_cli_form_uses_complete_schema) :-
    findall(Command, parse_words(["mirror", "run"], Command), Pending),
    expect_equal(Pending,
                 [command(mirror_run_pending, mirror_run_pending, ui, [])]),
    findall(Command, parse_words(["mirror", "run", "5"], Command), One),
    expect_equal(One,
                 [command(mirror_run, mirror_run, ui, [integer(5)])]).

run_test(alias_normalization_is_wire_ready) :-
    parse_words(["mirror", "pause", "5"],
                command(mirror_pause, mirror_pause, ui,
                        [integer(5), boolean(true)])),
    parse_words(["mirror", "resume", "5"],
                command(mirror_resume, mirror_pause, ui,
                        [integer(5), boolean(false)])).

run_test(string_kinds_do_not_become_numbers) :-
    parse_words(["rename", "7", "00042"],
                command(rename, rename, control,
                        [string("7"), string("00042")])).

run_test(array_wire_shape_is_preserved) :-
    parse_words(["review.apply", "7", "one", "two"],
                command('review.apply', 'review.apply', ui,
                        [string("7"), array([string("one"), string("two")])])).

run_test(parse_render_roundtrip) :-
    parse_words(["mirror", "resume", "5"], Command),
    render(Command, cli, "mirror resume 5"),
    render(Command, verb, "mirror_resume 5").

run_test(completion_and_highlighting_derive_from_relation) :-
    Items = [edit_tear(edit, span(0, 8), "mirror_r"), end(8)],
    completions(Items, edit, Completions),
    expect(member_completion("mirror_run", Completions)),
    items(["mirror", "run", "5"], ParseItems),
    once(parse(ParseItems, Candidate)),
    highlights(Candidate, Highlights),
    expect(Highlights \= []).

member_completion(Text, [completion(_, Text, _, _, _)|_]).
member_completion(Text, [_|Completions]) :- member_completion(Text, Completions).

run_test(application_surface_is_closed) :-
    application(parse, "request([end(0)],exact)", ParseOutput),
    expect_equal(ParseOutput, "ok([])"),
    application(shell, "request(halt)", ShellOutput),
    expect_equal(ShellOutput, "error(invalid_operation)").

run_test(context_plan_captures_box_dependency) :-
    items(["rename", "work", "new-name"], Items),
    once(context_plan(Items, exact,
                      plan(command(rename, rename, control,
                                   [string("work"), string("new-name")]),
                           Queries, Bindings, _, _))),
    expect_equal(Queries,
                 [query(q1, ask(one, box, name("work")))]),
    expect_equal(Bindings, [bind(q1, arg(1), entry_value)]).

run_test(context_resolution_rewrites_wire_argument) :-
    items(["rename", "work", "new-name"], Items),
    once(context_plan(Items, exact, Plan)),
    Plan = plan(_, [query(q1, Query)], _, _, _),
    Observations =
        [observed(q1, Query, source(boxes, 7),
                  some(one(entry(box, 5, ["work"], string("5"), []))))],
    resolve_context_plan(Plan, Observations, Command),
    expect_equal(Command,
                 command(rename, rename, control,
                         [string("5"), string("new-name")])).

run_test(dependent_path_plan_references_box_query) :-
    items(["writer_id", "work", "src/main.rs"], Items),
    once(context_plan(Items, exact,
                      plan(_, Queries, Bindings, _, _))),
    expect_equal(Queries,
                 [query(q1, ask(one, box, name("work"))),
                  query(q2, ask(one, path,
                                within(box(ref(q1)), name("src/main.rs"))))]),
    expect_equal(Bindings,
                 [bind(q1, arg(1), entry_value),
                  bind(q2, arg(2), entry_value)]).

run_test(context_completion_uses_all_prefix_query) :-
    neutral("rename", 0, Rename),
    Items = [Rename, edit_tear(edit, span(7, 9), "wo"), end(9)],
    once(context_completion_plan(Items, edit, Plan)),
    expect_equal(Plan,
                 completion_context(rename, span(7, 9), "wo",
                                    [query(q1, ask(all, box, prefix("wo")))],
                                    q1)).

run_test(context_completion_resolution_uses_entry_names) :-
    Plan = completion_context(rename, span(7, 9), "wo",
                              [query(q1, Query)], q1),
    Query = ask(all, box, prefix("wo")),
    Observations =
        [observed(q1, Query, source(boxes, 7),
                  some(all([entry(box, 5, ["5", "work"], string("5"), []),
                            entry(box, 9, ["9", "world"], string("9"), [])])))],
    resolve_context_completion(Plan, Observations, Completions),
    expect_equal(Completions,
                 [completion(span(7, 9), "work",
                             [alternative(context(rename, box, 5),
                                          context_argument, boxes, 50)], 50, 1),
                  completion(span(7, 9), "world",
                             [alternative(context(rename, box, 9),
                                          context_argument, boxes, 50)], 50, 2)]).

run_test(dependent_context_completion_graph) :-
    neutral("writer_id", 0, Writer),
    neutral("work", 10, Box),
    Items = [Writer, Box, edit_tear(edit, span(15, 18), "src"), end(18)],
    once(context_completion_plan(Items, edit, Plan)),
    expect_equal(
        Plan,
        completion_context(
            writer_id, span(15, 18), "src",
            [query(q1, ask(one, box, name("work"))),
             query(q2, ask(all, path,
                           within(box(ref(q1)), prefix("src"))))],
            q2)).
