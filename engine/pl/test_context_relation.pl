:- module(test_context_relation, [run_context_relation_tests/0]).

:- use_module(context_relation).
:- discontiguous run_test/1.

test_name(cardinalities_are_semantic).
test_name(unique_fails_for_zero_and_ambiguity).
test_name(selectors_are_typed_and_composable).
test_name(tear_matches_are_exact_unique_parse_witnesses).
test_name(observations_have_stable_dependency_keys).
test_name(dependent_queries_become_ready_in_order).
test_name(invalid_graphs_fail_closed).

run_context_relation_tests :-
    findall(Name, test_name(Name), Names),
    run_names(Names, 0, Passed),
    format('% context_relation: ~d tests passed~n', [Passed]).

run_names([], Passed, Passed).
run_names([Name|Names], Passed0, Passed) :-
    format('% context_relation:~w ... ', [Name]),
    catch(( once(run_test(Name)) -> writeln(passed) ; throw(test_failed(Name))),
          Error, (format('FAILED: ~w~n', [Error]), throw(Error))),
    Passed1 is Passed0 + 1,
    run_names(Names, Passed1, Passed).

expect_equal(Actual, Expected) :-
    ( Actual == Expected -> true ; throw(expected(Expected, got(Actual))) ).

snapshot(snapshot(source(boxes, 7),
                  [entry(box, 2, ["work", "w"], box_id(2), [running]),
                   entry(box, 9, ["worker"], box_id(9), [stopped]),
                   entry(path, p1, ["src/main.rs"], path("src/main.rs"),
                         [box(box_id(2)), kind(file)]),
                   entry(path, p2, ["src/lib.rs"], path("src/lib.rs"),
                         [box(box_id(2)), kind(file)])])).

run_test(cardinalities_are_semantic) :-
    snapshot(Snapshot),
    context_query(ask(empty, box, prefix("no")), Snapshot, empty(true)),
    context_query(ask(empty, box, prefix("wo")), Snapshot, empty(false)),
    context_query(ask(one, box, name("work")), Snapshot,
                  one(entry(box, 2, ["work", "w"], box_id(2), [running]))),
    context_query(ask(all, box, prefix("wo")), Snapshot, all(Entries)),
    length(Entries, 2).

run_test(unique_fails_for_zero_and_ambiguity) :-
    snapshot(Snapshot),
    \+ context_query(ask(one, box, name("missing")), Snapshot, _),
    \+ context_query(ask(one, box, prefix("wo")), Snapshot, _).

run_test(selectors_are_typed_and_composable) :-
    snapshot(Snapshot),
    context_query(ask(all, path,
                      within(box(box_id(2)), and(prefix("src/"),
                                                 where(kind(file))))),
                  Snapshot, all(Paths)),
    length(Paths, 2),
    context_query(ask(empty, box, where(kind(file))), Snapshot, empty(true)).

run_test(tear_matches_are_exact_unique_parse_witnesses) :-
    snapshot(Snapshot),
    once(context_tear_match(
        ask(all, path, within(box(box_id(2)), prefix("src/m"))),
        Snapshot, "src/m", "src/main.rs", ExactQuery,
        entry(path, p1, _, _, _))),
    expect_equal(
        ExactQuery,
        ask(one, path, within(box(box_id(2)), name("src/main.rs")))),
    Ambiguous = snapshot(
        source(boxes, 8),
        [entry(box, 2, ["same"], box_id(2), []),
         entry(box, 9, ["same"], box_id(9), [])]),
    \+ context_tear_match(ask(all, box, prefix("sa")), Ambiguous, "sa",
                           _, _, _).

run_test(observations_have_stable_dependency_keys) :-
    snapshot(Snapshot),
    Query = ask(one, box, name("missing")),
    observe_query(box_query, Query, Snapshot, Observation),
    expect_equal(Observation,
                 observed(box_query, Query, source(boxes, 7), none)),
    dependency_key(Observation, Key),
    expect_equal(Key, dependency(box_query, Query, none)).

run_test(dependent_queries_become_ready_in_order) :-
    Graph = [query(box_query, ask(one, box, name("work"))),
             query(path_query,
                   ask(all, path,
                       within(box(ref(box_query)), prefix("src/"))))],
    valid_query_graph(Graph),
    ready_queries(Graph, [], [query(box_query, ask(one, box, name("work")))]),
    BoxObservation =
        observed(box_query, ask(one, box, name("work")), source(boxes, 7),
                 some(one(entry(box, 2, ["work"], box_id(2), [])))),
    stage_context(
        Graph, [BoxObservation],
        [query(path_query,
               ask(all, path,
                   within(box(box_id(2)), prefix("src/"))))],
        [dependency(box_query, ask(one, box, name("work")),
                    some(one(entry(box, 2, ["work"], box_id(2), []))))]).

run_test(invalid_graphs_fail_closed) :-
    \+ valid_query_graph([query(a, ask(one, box, name("x"))),
                          query(a, ask(one, box, name("y")))]),
    \+ valid_query_graph([query(a, ask(one, box, name(ref(missing))))]),
    \+ valid_query_graph([query(a, ask(one, box, name(ref(b)))),
                          query(b, ask(one, box, name(ref(a))))]),
    \+ valid_query_graph([query(a, ask(all, box, prefix("x"))),
                          query(b, ask(one, path, where(box(ref(a)))))]).
