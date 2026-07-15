:- module(test_grammar_ir, [run_grammar_ir_tests/0]).

:- use_module(grammar_ir).

test_name(tree_sitter_shaped_grammar_is_data).
test_name(wireshark_shaped_grammar_is_data).
test_name(undeclared_rules_and_primitives_fail_closed).

run_grammar_ir_tests :-
    findall(Name, test_name(Name), Names),
    run_test_names(Names, 0, Passed),
    format('% grammar_ir: ~d tests passed~n', [Passed]).

run_test_names([], Passed, Passed).
run_test_names([Name|Names], Passed0, Passed) :-
    format('% grammar_ir:~w ... ', [Name]),
    catch(( once(run_test(Name))
          -> writeln(passed)
          ;  throw(test_failed(Name))
          ),
          Error,
          ( format('FAILED: ~w~n', [Error]), throw(Error) )),
    Passed1 is Passed0 + 1,
    run_test_names(Names, Passed1, Passed).

tree_sitter_fixture(
    grammar(source(text(utf8)), expression,
            [rule(identifier,
                  terminal(text(class(identifier)),
                           presentation([syntax-identifier]))),
             rule(comment,
                  terminal(text(regex("//[^\\n]*")),
                           presentation([syntax-comment]))),
             rule(expression,
                  extras([ref(comment)],
                         conflicts(
                             [expression],
                             choice([
                                 field(name, ref(identifier)),
                                 precedence(
                                     10, left,
                                     seq([field(left, ref(expression)),
                                          literal("+", plus,
                                                  presentation([syntax-operator])),
                                          field(right, ref(expression))])),
                                 terminal(
                                     primitive(template_close, []),
                                     presentation([syntax-delimiter])),
                                 embed(grammar_ref(php),
                                       delimited("<?php", "?>"))]))))],
            [primitive(template_close, 0, modes([out]), bounded(8))])).

wireshark_fixture(
    grammar(source(bytes), packet,
            [rule(packet,
                  seq([field(length,
                             terminal(bytes(uint(32, little)),
                                      presentation([syntax-length]))),
                       field(kind,
                             terminal(bytes(uint(8, big)),
                                      presentation([syntax-tag]))),
                       field(payload,
                             terminal(bytes(slice(value(length))),
                                      presentation([syntax-payload]))),
                       dispatch(value(kind),
                                [case(1, ref(text_payload)),
                                 case(2, ref(nested_payload))],
                                default(ref(opaque_payload))),
                       field(checksum,
                             terminal(bytes(uint(32, big)),
                                      presentation([syntax-checksum]))),
                       constraint(checksum(adler32, value(payload),
                                           value(checksum))),
                       context(stream_state,
                               ask(one, tcp_stream,
                                   within(value(flow), name(value(stream)))))])),
             rule(text_payload,
                  terminal(text(class(utf8)), presentation([syntax-text]))),
             rule(nested_payload,
                  embed(grammar_ref(nested_protocol), bounded(value(length)))),
             rule(opaque_payload,
                  terminal(bytes(rest), presentation([syntax-opaque])))],
            [])).

run_test(tree_sitter_shaped_grammar_is_data) :-
    tree_sitter_fixture(Grammar),
    valid_grammar(Grammar).

run_test(wireshark_shaped_grammar_is_data) :-
    wireshark_fixture(Grammar),
    valid_grammar(Grammar).

run_test(undeclared_rules_and_primitives_fail_closed) :-
    \+ valid_grammar(
           grammar(source(text(utf8)), root,
                   [rule(root, ref(missing))], [])),
    \+ valid_grammar(
           grammar(source(bytes), root,
                   [rule(root,
                         terminal(primitive(hidden_callback, []),
                                  presentation([])))], [])).
