:- module(test_local_state_relation, [run_local_state_relation_tests/0]).

:- use_module(local_state_relation).

test_name(locals_resolve_without_external_queries).
test_name(escaping_assignment_is_local_and_returns_delta).
test_name(shadowing_and_scope_exit_are_lexical).
test_name(name_tear_unifies_locals_and_emits_rich_provider_query).
test_name(external_resolution_consumes_unique_observation).
test_name(failed_unique_observation_fails_resolution).
test_name(later_constraint_binds_hole_through_local_reference).
test_name(symbolic_constraints_fail_closed).

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
    run_state_steps(Steps, Initial, Final, Resolutions, Queries, Delta, _),
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
        [], [state_change(shell_variable, "x", integer(123))], _).

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
        [], [], _).

run_test(name_tear_unifies_locals_and_emits_rich_provider_query) :-
    Hole = hole(edit, span(9, 9), "", text(codepoint(identifier))),
    CompletionId = name_completion(read_name, "", Hole, ""),
    Query = ask(all, shell_variable, prefix("")),
    empty_local_state(Initial),
    run_state_steps(
        [define(shell_variable, "A", text(["value"]), escaping, replace),
         use(read_name, shell_variable, text_hole("", Hole, ""))],
        Initial, _, [resolved(read_name, incomplete(_))],
        [query(CompletionId, Query)], _,
        [completion_key(span(9, 9), "A")-
             (alternative(local(shell_variable, "A"), variable,
                          local_state)-80)]),
    Entry = entry(shell_variable, external_b, ["B"], text(["other"]), []),
    context_state_completion_pairs(
        [query(CompletionId, Query)],
        [observed(CompletionId, Query, source(environment, 4),
                  some(all([Entry])))],
        [completion_key(span(9, 9), "B")-
             (alternative(context(shell_variable, external_b), variable,
                          environment)-50)]).

run_test(external_resolution_consumes_unique_observation) :-
    Id = use_z,
    Query = ask(one, symbol, name("z")),
    Entry = entry(symbol, z_id, ["z"], integer(9), []),
    Resolutions = [resolved(Id, external(ref(Id)))],
    resolve_state_resolutions(
        Resolutions,
        [observed(Id, Query, source(symbols, 3), some(one(Entry)))],
        [resolved(Id, external(one(Entry)))]).

run_test(failed_unique_observation_fails_resolution) :-
    Id = use_z,
    \+ resolve_state_resolutions(
           [resolved(Id, external(ref(Id)))],
           [observed(Id, ask(one, symbol, name("z")),
                     source(symbols, 4), none)], _).

run_test(later_constraint_binds_hole_through_local_reference) :-
    Hole = hole(edit, span(3, 3), "", text(codepoint(any))),
    State = local_state(
        [scope(root,
               [local_binding(shell_variable, "A", text([Hole]), escaping)])],
        []),
    Values = [value("f", file, find_type, 30),
              value("d", directory, find_type, 30),
              value("l", symlink, find_type, 30)],
    Constraints = [text_constraint(
                       text([reference(shell_variable, "A")]),
                       one_of(Values), presentation(find_type_argument))],
    state_constraint_completion_pairs(Constraints, State, Pairs),
    Pairs = [completion_key(span(3, 3), "f")-
                 (alternative(file, find_type_argument, find_type)-30),
             completion_key(span(3, 3), "d")-
                 (alternative(directory, find_type_argument, find_type)-30),
             completion_key(span(3, 3), "l")-
                 (alternative(symlink, find_type_argument, find_type)-30)].

run_test(symbolic_constraints_fail_closed) :-
    empty_local_state(State),
    \+ state_constraint_completion_pairs(
           [text_constraint(text([hole(edit, span(2, 1), "", any)]),
                            one_of([]), presentation(invalid))],
           State, _).
