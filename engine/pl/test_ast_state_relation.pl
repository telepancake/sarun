:- module(test_ast_state_relation, [run_ast_state_relation_tests/0]).

:- use_module(ast_state_relation).
:- use_module(local_state_relation).

test_name(fields_emit_ordered_state_steps_from_utf8_source).
test_name(before_and_after_emissions_wrap_child_nodes).

run_ast_state_relation_tests :-
    findall(Name, test_name(Name), Names),
    run_names(Names, 0, Passed),
    format('% ast_state_relation: ~d tests passed~n', [Passed]).

run_names([], Passed, Passed).
run_names([Name|Names], Passed0, Passed) :-
    format('% ast_state_relation:~w ... ', [Name]),
    ( once(run_test(Name))
    -> writeln(passed), Passed1 is Passed0 + 1
    ;  writeln('FAILED'), fail
    ),
    run_names(Names, Passed1, Passed).

run_test(fields_emit_ordered_state_steps_from_utf8_source) :-
    Ast = node(program, span(0, 21), sequence([
              node(declaration, span(0, 7),
                   field(name, span(4, 6), ignored)),
              node(use, span(8, 14), field(name, span(12, 14), ignored)),
              node(use, span(16, 21), field(name, span(20, 21), ignored))
          ])),
    Rules = [
        state_rule(node(declaration), [capture(name, field_text(name))],
                   before([define(c_symbol, slot(name), int, lexical, unique)]),
                   after([])),
        state_rule(node(use), [capture(name, field_text(name))],
                   before([use(node_identity, c_symbol, slot(name))]),
                   after([]))
    ],
    Source = text_source("let λ; use λ; use z", exact, fixture),
    derive_ast_state_steps(Rules, Ast, Source, Steps),
    Steps = [define(c_symbol, "λ", int, lexical, unique),
             use(node_ref(use, span(8, 14)), c_symbol, "λ"),
             use(node_ref(use, span(16, 21)), c_symbol, "z")],
    empty_local_state(Initial),
    run_state_steps(Steps, Initial, _, Resolutions, Queries, []),
    Resolutions = [resolved(node_ref(use, span(8, 14)), local(_)),
                   resolved(node_ref(use, span(16, 21)), external(_))],
    Queries = [query(node_ref(use, span(16, 21)),
                     ask(one, c_symbol, name("z")))].

run_test(before_and_after_emissions_wrap_child_nodes) :-
    Ast = node(function, span(0, 1),
               sequence([node(use, span(0, 1),
                                   field(name, span(0, 1), ignored))])),
    Rules = [
        state_rule(node(function), [], before([enter(function_scope)]),
                   after([leave(function_scope)])),
        state_rule(node(use), [capture(name, field_text(name))],
                   before([use(node_identity, symbol, slot(name))]), after([]))
    ],
    derive_ast_state_steps(Rules, Ast, "x",
                           [enter(function_scope),
                            use(node_ref(use, span(0, 1)), symbol, "x"),
                            leave(function_scope)]).
