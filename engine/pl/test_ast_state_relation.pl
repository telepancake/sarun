:- module(test_ast_state_relation, [run_ast_state_relation_tests/0]).

:- use_module(ast_state_relation).
:- use_module(local_state_relation).

test_name(fields_emit_ordered_state_steps_from_utf8_source).
test_name(before_and_after_emissions_wrap_child_nodes).
test_name(symbolic_text_projection_is_declarative_ast_glue).
test_name(symbolic_words_keep_fragment_origins_and_utf8_spans).
test_name(source_assist_tear_splits_successful_literal_with_suffix).

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
    run_state_steps(Steps, Initial, _, Resolutions, Queries, [], _),
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

run_test(symbolic_text_projection_is_declarative_ast_glue) :-
    Hole = hole(edit, span(5, 5), "", terminal(text(codepoint(any)))),
    Ast = node(assignment, span(0, 6),
               sequence([
                   field(name, span(0, 1), ignored),
                   field(value, span(2, 6),
                         sequence([
                             node(raw_text, span(2, 3), codepoint(120)),
                             node(parameter, span(3, 5),
                                  field(name, span(4, 5), ignored)),
                             Hole,
                             node(raw_text, span(5, 6), codepoint(121))]))
               ])),
    TextRules = [text_rule(node(raw_text), source),
                 text_rule(node(parameter),
                           reference(variable, field_text(name)))],
    Rules = [state_rule(
                 node(assignment),
                 [capture(name, field_text(name)),
                  capture(value,
                          field_symbolic_text(value, TextRules))],
                 before([]),
                 after([define(variable, slot(name), slot(value),
                               escaping, replace)]))],
    derive_ast_state_steps(
        Rules, Ast, text_source("A=x$By", assist(edit, span(5, 5)), fixture),
        [define(variable, "A",
                text(["x", reference(variable, "B"), Hole, "y"]),
                escaping, replace)]).

run_test(symbolic_words_keep_fragment_origins_and_utf8_spans) :-
    Hole = hole(edit, span(12, 12), "", any),
    Ast = node(command_words, span(0, 13),
               sequence([
                   field(command, span(0, 4),
                         node(word, span(0, 4),
                              node(raw_text, span(0, 4), ignored))),
                   field(argument, span(5, 7),
                         node(word, span(5, 7),
                              node(raw_text, span(5, 7), ignored))),
                   field(argument, span(7, 9),
                         node(word, span(7, 9),
                              node(parameter, span(7, 9),
                                   field(name, span(8, 9), ignored)))),
                   field(argument, span(10, 13),
                         node(word, span(10, 13),
                              node(quoted, span(10, 13),
                                   sequence([
                                       node(raw_text, span(11, 12), ignored),
                                       Hole
                                   ]))))
               ])),
    FragmentRules = [
        fragment_rule(node(raw_text), emit(source, literal, [])),
        fragment_rule(node(quoted), descend([quote(double)])),
        fragment_rule(node(parameter),
                      emit_reference(variable, field_text(name), []))],
    Rules = [state_rule(
                 node(command_words),
                 [capture(command,
                          field_symbolic_word(
                              command, FragmentRules, [quote(unquoted)])),
                  capture(arguments,
                          fields_symbolic_words(
                              argument, FragmentRules, [quote(unquoted)]))],
                 before([]),
                 after([argv(slot(command), slot(arguments))]))],
    derive_ast_state_steps(
        Rules, Ast,
        text_source("tool α$B \"x\"", assist(edit, span(12, 12)), fixture),
        [argv(symbolic_word(
                  span(0, 4),
                  [fragment(literal,
                            origin(fixture, span(0, 4), [quote(unquoted)]),
                            utf8("tool"))]),
              [symbolic_word(
                   span(5, 7),
                   [fragment(literal,
                             origin(fixture, span(5, 7), [quote(unquoted)]),
                             utf8("α"))]),
               symbolic_word(
                   span(7, 9),
                   [fragment(reference,
                             origin(fixture, span(7, 9), [quote(unquoted)]),
                             state_ref(node_ref(parameter, span(7, 9)),
                                       variable, "B"))]),
               symbolic_word(
                   span(10, 13),
                   [fragment(literal,
                             origin(fixture, span(11, 12), [quote(double)]),
                             utf8("x")),
                    fragment(tear,
                             origin(fixture, span(12, 12), [quote(double)]),
                             edit_tear(edit, "", any))])])]).

run_test(source_assist_tear_splits_successful_literal_with_suffix) :-
    Ast = node(command_words, span(0, 5),
               field(argument, span(0, 5),
                     node(raw_text, span(0, 5), ignored))),
    Rules = [state_rule(
                 node(command_words),
                 [capture(words,
                          fields_symbolic_words(
                              argument,
                              [fragment_rule(
                                   node(raw_text),
                                   emit(source, literal, []))],
                              [quote(unquoted)]))],
                 before([]), after([words(slot(words))]))],
    derive_ast_state_steps(
        Rules,
        Ast,
        text_source("emλx", assist(edit, span(2, 2)), fixture),
        [words([
             symbolic_word(
                 span(0, 5),
                 [fragment(literal,
                           origin(fixture, span(0, 2), [quote(unquoted)]),
                           utf8("em")),
                  fragment(tear,
                           origin(fixture, span(2, 2), source_edit),
                           edit_tear(edit, "", source_edit)),
                  fragment(literal,
                           origin(fixture, span(2, 5), [quote(unquoted)]),
                           utf8("λx"))])])]).
