:- module(test_local_state_relation, [run_local_state_relation_tests/0]).

:- use_module(local_state_relation).

test_name(locals_resolve_without_external_queries).
test_name(escaping_assignment_is_local_and_returns_delta).
test_name(shadowing_and_scope_exit_are_lexical).

run_local_state_relation_tests :-
    findall(Name, test_name(Name), Names),
    run_names(Names, 0, Passed),
    format('% local_state_relation: ~d tests passed~n', [Passed]).

run_names([], Passed, Passed).
run_names([Name|Names], Passed0, Passed) :-
    format('% local_state_relation:~w ... ', [Name]),
    ( once(run_test(Name))
    -> writeln(passed), Passed1 is Passed0 + 1
    ;  writeln('FAILED'), fail
    ),
    run_names(Names, Passed1, Passed).

run_test(locals_resolve_without_external_queries) :-
    empty_local_state(Initial),
    Steps = [require(f_odr, empty, c_external_symbol, name("f")),
             define(c_symbol, "f", function(int, [int]), escaping, unique),
             enter(function_f),
             define(c_symbol, "x", int, lexical, unique),
             define(c_symbol, "y", int, lexical, unique),
             use(use_x, c_symbol, "x"),
             use(use_y, c_symbol, "y"),
             use(use_z, c_symbol, "z"),
             leave(function_f)],
    run_state_steps(Steps, Initial, Final, Resolutions, Queries, Delta),
    Queries = [query(f_odr,
                     ask(empty, c_external_symbol, name("f"))),
               query(use_z, ask(one, c_symbol, name("z")))],
    Resolutions = [resolved(use_x,
                            local(local_binding(c_symbol, "x", int,
                                                lexical))),
                   resolved(use_y,
                            local(local_binding(c_symbol, "y", int,
                                                lexical))),
                   resolved(use_z, external(ref(use_z)))],
    Delta = [state_change(c_symbol, "f", function(int, [int]))],
    Final = local_state(
        [scope(root,
               [local_binding(c_symbol, "f", function(int, [int]),
                              escaping)])],
        Delta).

run_test(escaping_assignment_is_local_and_returns_delta) :-
    empty_local_state(Initial),
    run_state_steps(
        [define(shell_variable, "x", integer(123), escaping, replace),
         use(read_x, shell_variable, "x")],
        Initial, _Final,
        [resolved(read_x,
                  local(local_binding(shell_variable, "x", integer(123),
                                      escaping)))],
        [], [state_change(shell_variable, "x", integer(123))]).

run_test(shadowing_and_scope_exit_are_lexical) :-
    empty_local_state(Initial),
    run_state_steps(
        [define(name, "x", outer, lexical, unique),
         enter(inner),
         define(name, "x", inner, lexical, unique),
         use(inner_x, name, "x"),
         leave(inner),
         use(outer_x, name, "x")],
        Initial, _Final,
        [resolved(inner_x,
                  local(local_binding(name, "x", inner, lexical))),
         resolved(outer_x,
                  local(local_binding(name, "x", outer, lexical)))],
        [], []).
