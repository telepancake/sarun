:- module(test_brush_grammar, [run_brush_grammar_tests/0]).

:- use_module(brush_grammar).
:- use_module(relation_api).

test_name(grammar_is_valid_executable_data).
test_name(word_slice_parses_quotes_expansions_and_utf8).
test_name(program_slice_parses_commands_pipelines_and_lists).
test_name(tear_completion_is_an_ordinary_parse_with_concrete_suffix).
test_name(unterminated_word_has_no_complete_parse).
test_name(assignment_resolves_later_parameter_inside_relation).
test_name(assignment_rhs_resolves_before_definition).
test_name(unresolved_parameter_emits_external_query).
test_name(parameter_observation_resolves_and_records_dependency).
test_name(missing_unique_parameter_observation_fails_semantic_solution).
test_name(assignment_tear_survives_as_later_local_value).
test_name(later_find_type_use_constrains_assignment_tear).

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
    brush_relation_grammar(
        completion_union_grammar(
            enrichment_grammar(
                enrichment_grammar(
                    Syntax, [ast], ast_state_grammar(StateRules),
                    [steps, final_state, resolutions, delta]),
                [steps, final_state], _, [semantic_completions]),
            semantic_completions)),
    grammar_ir:valid_grammar(Syntax),
    ast_state_relation:valid_ast_state_rules(StateRules).

run_test(word_slice_parses_quotes_expansions_and_utf8) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("pré' literal 'x\\ y\"$name:$?$(echo hi)\"$((base + (2)))",
                                           exact, brush_test))]),
                want([ast, status, highlights]), observations([]),
                limits(32, 4096, 1048576)),
        reply([solution([binding(ast, node(shell_program, span(0, 54), _)),
                         binding(status, complete),
                         binding(highlights, Highlights)], 0)], [], [], [])),
    has_highlight(span(0, 1), word, codepoint(112), brush_test, Highlights),
    has_highlight(span(2, 4), word, codepoint(233), brush_test, Highlights),
    has_highlight(_, variable, _, brush_test, Highlights),
    has_highlight(_, arithmetic, _, brush_test, Highlights).

run_test(program_slice_parses_commands_pipelines_and_lists) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source(
                                   "echo \"hi $name\" | grep hi && printf done\nnext &",
                                   exact, brush_test))]),
                want([ast, status, highlights]), observations([]),
                limits(32, 4096, 1048576)),
        reply([solution([binding(ast,
                                 node(shell_program, span(0, 47), _)),
                         binding(status, complete),
                         binding(highlights, Highlights)], 0)], [], [], [])),
    has_highlight(_, variable, parameter_sigil, brush_test, Highlights),
    has_highlight(_, operator, pipe, brush_test, Highlights),
    has_highlight(_, operator, and_if, brush_test, Highlights),
    has_highlight(_, operator, newline, brush_test, Highlights),
    has_highlight(_, operator, background, brush_test, Highlights),
    has_highlight(_, trivia, codepoint(32), brush_test, Highlights).

run_test(tear_completion_is_an_ordinary_parse_with_concrete_suffix) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("echo hi)",
                                           assist(edit, span(0, 0)),
                                           brush_test))]),
                want([status, completions, highlights]), observations([]),
                limits(32, 4096, 1048576)),
        reply([solution([binding(status, incomplete(edit(edit))),
                         binding(completions,
                                 [completion(
                                      span(0, 0), "$(",
                                      [alternative(command_substitution_open,
                                                   delimiter, grammar, 0)],
                                      0, 1)]),
                         binding(highlights, Highlights)], 0)], [], [], [])),
    has_highlight(span(0, 1), command, codepoint(101), brush_test,
                  Highlights).

run_test(unterminated_word_has_no_complete_parse) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("before\"unterminated", exact,
                                           brush_test))]),
                want([ast]), observations([]), limits(32, 4096, 1048576)),
        reply([], [], [], [diagnostic(no_solution)])).

run_test(assignment_resolves_later_parameter_inside_relation) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("x=123; echo $x", exact,
                                           brush_test)),
                       binding(initial_state,
                               local_state([scope(root, [])], []))]),
                want([ast, highlights, resolutions, delta]), observations([]),
                limits(32, 4096, 1048576)),
        reply([solution(
                   [binding(ast, node(shell_program, span(0, 14), _)),
                    binding(highlights, Highlights),
                    binding(resolutions,
                            [resolved(
                                 node_ref(simple_parameter, span(12, 14)),
                                 local(local_binding(
                                     shell_variable, "x", text(["123"]),
                                     escaping)))]),
                    binding(delta,
                            [state_change(shell_variable, "x",
                                          text(["123"]))])], 0)],
              [], [], [])),
    has_highlight(span(1, 2), operator, assignment, brush_test, Highlights),
    has_highlight(span(12, 13), variable, parameter_sigil, brush_test,
                  Highlights).

run_test(assignment_rhs_resolves_before_definition) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("x=$x", exact, brush_test)),
                       binding(initial_state,
                               local_state([scope(root, [])], []))]),
                want([resolutions, delta]), observations([]),
                limits(32, 4096, 1048576)),
        reply([solution(
                   [binding(resolutions,
                            [resolved(
                                 node_ref(simple_parameter, span(2, 4)),
                                 external(ref(node_ref(simple_parameter,
                                                       span(2, 4))))) ]),
                    binding(delta,
                            [state_change(shell_variable, "x",
                                          text([reference(shell_variable,
                                                          "x")]))])], 0)],
              [query(node_ref(simple_parameter, span(2, 4)),
                     ask(one, shell_variable, name("x")))], [], [])).

run_test(unresolved_parameter_emits_external_query) :-
    brush_relation_grammar(Grammar),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("echo $z", exact, brush_test)),
                       binding(initial_state,
                               local_state([scope(root, [])], []))]),
                want([resolutions]), observations([]),
                limits(32, 4096, 1048576)),
        reply([solution([binding(
                   resolutions,
                   [resolved(node_ref(simple_parameter, span(5, 7)),
                             external(ref(node_ref(simple_parameter,
                                                   span(5, 7)))))])], 0)],
              [query(node_ref(simple_parameter, span(5, 7)),
                     ask(one, shell_variable, name("z")))], [], [])).

run_test(parameter_observation_resolves_and_records_dependency) :-
    brush_relation_grammar(Grammar),
    Id = node_ref(simple_parameter, span(5, 7)),
    Query = ask(one, shell_variable, name("z")),
    Entry = entry(shell_variable, variable_z, ["z"], shell_text("value"),
                  [exported]),
    Observation = observed(Id, Query, source(brush_variables, 7),
                           some(one(Entry))),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("echo $z", exact, brush_test)),
                       binding(initial_state,
                               local_state([scope(root, [])], []))]),
                want([resolutions]), observations([Observation]),
                limits(32, 4096, 1048576)),
        reply([solution([binding(resolutions,
                                 [resolved(Id, external(one(Entry)))])], 0)],
              [], [dependency(Id, Query, some(one(Entry)))], [])).

run_test(missing_unique_parameter_observation_fails_semantic_solution) :-
    brush_relation_grammar(Grammar),
    Id = node_ref(simple_parameter, span(5, 7)),
    Query = ask(one, shell_variable, name("z")),
    Observation = observed(Id, Query, source(brush_variables, 8), none),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("echo $z", exact, brush_test)),
                       binding(initial_state,
                               local_state([scope(root, [])], []))]),
                want([resolutions]), observations([Observation]),
                limits(32, 4096, 1048576)),
        reply([], [], [dependency(Id, Query, none)], [])).

run_test(assignment_tear_survives_as_later_local_value) :-
    brush_relation_grammar(Grammar),
    Codec = text(codepoint(except("\"\\$"))),
    Hole = hole(edit, span(3, 3), "", terminal(Codec)),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source("A=\"\"; echo $A",
                                           assist(edit, span(3, 3)),
                                           brush_test)),
                       binding(initial_state,
                               local_state([scope(root, [])], []))]),
                want([status, final_state, resolutions, completions]),
                observations([]), limits(32, 4096, 1048576)),
        reply(Solutions, [], [], [])),
    has_assignment_hole_solution(Hole, Solutions).

run_test(later_find_type_use_constrains_assignment_tear) :-
    brush_relation_grammar(Grammar),
    Hole = hole(edit, span(3, 3), "",
                terminal(text(codepoint(except("\"\\$"))))),
    transform(
        request(Grammar,
                given([binding(source,
                               text_source(
                                   "A=\"\"; find . -type $A",
                                   assist(edit, span(3, 3)), brush_test)),
                       binding(initial_state,
                               local_state([scope(root, [])], []))]),
                want([status, completions, delta]), observations([]),
                limits(32, 4096, 1048576)),
        reply(Solutions, [], [], [])),
    has_find_type_solution(Hole, Solutions, Completions),
    has_completion("D", door, Completions),
    has_completion("b", block_device, Completions),
    has_completion("c", character_device, Completions),
    has_completion("d", directory, Completions),
    has_completion("f", regular_file, Completions),
    has_completion("l", symbolic_link, Completions),
    has_completion("p", named_pipe, Completions),
    has_completion("s", socket, Completions),
    has_completion("$", literal_dollar, Completions).

has_assignment_hole_solution(
    Hole,
    [solution(
         [binding(status, incomplete(edit(edit))),
          binding(final_state,
                  local_state(
                      [scope(root,
                             [local_binding(shell_variable, "A", text([Hole]),
                                            escaping)])],
                      [state_change(shell_variable, "A", text([Hole]))])),
          binding(resolutions,
                  [resolved(
                       node_ref(simple_parameter, span(11, 13)),
                       local(local_binding(shell_variable, "A", text([Hole]),
                                           escaping)))]),
          binding(completions,
                  [completion(span(3, 3), "$",
                              [alternative(literal_dollar, string, grammar,
                                           0)],
                              0, 1)])], 0)|_]).
has_assignment_hole_solution(Hole, [_|Solutions]) :-
    has_assignment_hole_solution(Hole, Solutions).

has_find_type_solution(
    Hole,
    [solution([binding(status, incomplete(edit(edit))),
               binding(completions, Completions),
               binding(delta,
                       [state_change(shell_variable, "A", text([Hole]))])],
              0)|_],
    Completions).
has_find_type_solution(Hole, [_|Solutions], Completions) :-
    has_find_type_solution(Hole, Solutions, Completions).

has_completion(Text, Semantic,
               [completion(_, Text, Alternatives, _, _)|_]) :-
    has_alternative(Semantic, Alternatives), !.
has_completion(Text, Semantic, [_|Completions]) :-
    has_completion(Text, Semantic, Completions).

has_alternative(Semantic, [alternative(Semantic, _, _, _)|_]).
has_alternative(Semantic, [_|Alternatives]) :-
    has_alternative(Semantic, Alternatives).

has_highlight(Span, Syntax, Semantic, Origin,
              [highlight(Span, Syntax, Semantic, Origin)|_]).
has_highlight(Span, Syntax, Semantic, Origin, [_|Highlights]) :-
    has_highlight(Span, Syntax, Semantic, Origin, Highlights).
