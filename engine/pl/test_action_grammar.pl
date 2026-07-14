:- module(test_action_grammar, [run_action_grammar_tests/0]).

:- use_module(action_grammar).

:- discontiguous run_test/1.

% This runner deliberately has no plunit or other package dependency. The same
% complete suite runs in the package-free embedded core and a full host SWI.

test_name(action_table_invariants).
test_name(parse_canonical_and_cli).
test_name(shared_cli_resolves_at_explicit_end).
test_name(explicit_end_and_tear_are_distinct).
test_name(source_tear_is_not_repairable).
test_name(reversed_source_span_rejected).
test_name(overlapping_and_reordered_source_rejected).
test_name(out_of_unit_paint_rejected).
test_name(reversed_and_overlapping_paint_rejected).
test_name(partial_identifier_completion).
test_name(completion_between_known_units).
test_name(visible_completion_merges_differing_descriptions).
test_name(completion_ranking).
test_name(highlighting_uses_real_evidence_only).
test_name(utf8_byte_spans_are_preserved).
test_name(render_lex_parse_roundtrip_all_actions).
test_name(invalid_trailing_input).
test_name(malformed_action_schema_is_excluded).
test_name(application_entry_is_closed_and_serialized).
test_name(registry_scale_regression).

run_action_grammar_tests :-
    findall(Name, test_name(Name), Names),
    run_test_names(Names, 0, Passed),
    write('% action_grammar: '),
    write(Passed),
    write(' tests passed'),
    nl.

run_test_names([], Passed, Passed).
run_test_names([Name|Names], Passed0, Passed) :-
    write('% action_grammar:'),
    write(Name),
    write(' ... '),
    catch(( once(run_test(Name))
          -> write('passed'), nl
          ;  throw(test_failed(Name))
          ),
          Error,
          ( write('FAILED: '), write(Error), nl, throw(Error) )),
    Passed1 is Passed0 + 1,
    run_test_names(Names, Passed1, Passed).

expect_true(Goal) :-
    ( call(Goal) -> true ; throw(expected_success(Goal)) ).

expect_false(Goal) :-
    ( call(Goal) -> throw(expected_failure(Goal)) ; true ).

expect_equal(Actual, Expected) :-
    ( Actual == Expected
    -> true
    ;  throw(expected(Expected, got(Actual)))
    ).

list_member(Item, [Head|_]) :- Item = Head.
list_member(Item, [_|Items]) :- list_member(Item, Items).

list_length([], 0).
list_length([_|Items], Length) :-
    list_length(Items, Rest),
    Length is Rest + 1.

all_valid_actions([]).
all_valid_actions([Name|Names]) :-
    expect_true(valid_action(Name)),
    all_valid_actions(Names).

unit(Semantic, Start, Stop, Surface, Syntax, Description,
     unit(Semantic, span(Start, Stop), [span(Start, Stop)], Surface, Syntax,
          Description, 2, lexer)).

mirror(Unit) :-
    unit(mirror, 0, 6, "mirror", command_namespace, mirror_namespace, Unit).

run(Start, Unit) :-
    Stop is Start + 3,
    unit(run, Start, Stop, "run", action_word, mirror_run_word, Unit).

integer_unit(Value, Start, Stop, Unit) :-
    number_string(Value, Surface),
    unit(integer(Value), Start, Stop, Surface, integer, job_id, Unit).

canonical(Action, Stop, Unit) :-
    atom_string(Action, Surface),
    unit(Action, 0, Stop, Surface, action_identifier, Action, Unit).

run_test(action_table_invariants) :-
    findall(Name, action(Name, _, _, _, _, _), Names),
    sort(Names, UniqueNames),
    expect_equal(Names,
                 [mirror_jobs, mirror_run, mirror_run_pending,
                  mirror_pause, mirror_rm]),
    expect_equal(UniqueNames,
                 [mirror_jobs, mirror_pause, mirror_rm,
                  mirror_run, mirror_run_pending]),
    all_valid_actions(Names).

run_test(parse_canonical_and_cli) :-
    canonical(mirror_run, 10, Verb),
    integer_unit(5, 11, 12, VerbId),
    once(parse([Verb, VerbId, end(12)], VerbResult)),
    VerbResult = parse_result(command(mirror_run, [job_id(5)]), complete,
                              VerbEvidence, 104),
    expect_equal(VerbEvidence,
                 [evidence(mirror_run, span(0, 10), [span(0, 10)],
                           "mirror_run", action_identifier, mirror_run,
                           2, lexer),
                  evidence(integer(5), span(11, 12), [span(11, 12)],
                           "5", integer, job_id, 2, lexer)]),
    mirror(Mirror),
    run(7, Run),
    integer_unit(5, 11, 12, CliId),
    findall(Command,
            parse([Mirror, Run, CliId, end(12)], exact,
                  parse_result(Command, complete, _, _)),
            Commands),
    expect_equal(Commands, [command(mirror_run, [job_id(5)])]),
    unit(ls, 7, 9, "ls", action_word, action_mirror_jobs, Ls),
    findall(JobsCommand,
            parse([Mirror, Ls, end(9)], exact,
                  parse_result(JobsCommand, complete, _, _)),
            JobsCommands),
    expect_equal(JobsCommands, [command(mirror_jobs, [])]).

run_test(shared_cli_resolves_at_explicit_end) :-
    mirror(Mirror),
    run(7, Run),
    findall(Command,
            parse([Mirror, Run, end(10)], exact,
                  parse_result(Command, complete, _, _)),
            Commands),
    expect_equal(Commands, [command(mirror_run_pending, [])]).

run_test(explicit_end_and_tear_are_distinct) :-
    mirror(Mirror),
    expect_false(parse([Mirror], _)),
    expect_false(parse([Mirror, end(6)], _)),
    completions([Mirror, end(6)], absent, AtEnd),
    expect_equal(AtEnd, []),
    completions([Mirror, edit_tear(gap, span(6, 6), ""), end(6)],
                gap, AtTear),
    expect_false(AtTear == []).

run_test(source_tear_is_not_repairable) :-
    Items = [source_tear(source, span(0, 4), "mirr"), end(4)],
    expect_false(parse(Items, assist(source), _)),
    completions(Items, source, Completions),
    expect_equal(Completions, []).

run_test(reversed_source_span_rejected) :-
    Unit = unit(mirror_jobs, span(11, 0), [], "mirror_jobs",
                action_identifier, action_mirror_jobs, 2, lexer),
    expect_false(parse([Unit, end(11)], _)),
    completions([edit_tear(edit, span(4, 2), "mirr"), end(4)],
                edit, Completions),
    expect_equal(Completions, []).

run_test(overlapping_and_reordered_source_rejected) :-
    mirror(Mirror),
    unit(run, 5, 8, "run", action_word, mirror_run_word, OverlapRun),
    expect_false(parse([Mirror, OverlapRun, end(8)], _)),
    UnitA = unit(mirror, span(7, 13), [], "mirror",
                 command_namespace, mirror_namespace, 2, lexer),
    UnitB = unit(run, span(0, 3), [], "run",
                 action_word, mirror_run_word, 2, lexer),
    expect_false(parse([UnitA, UnitB, end(13)], _)).

run_test(out_of_unit_paint_rejected) :-
    Unit = unit(mirror_jobs, span(0, 11), [span(10, 12)], "mirror_jobs",
                action_identifier, action_mirror_jobs, 2, lexer),
    expect_false(parse([Unit, end(12)], _)).

run_test(reversed_and_overlapping_paint_rejected) :-
    Reversed = unit(mirror_jobs, span(0, 11), [span(5, 4)], "mirror_jobs",
                    action_identifier, action_mirror_jobs, 2, lexer),
    Overlap = unit(mirror_jobs, span(0, 11),
                   [span(0, 7), span(6, 11)], "mirror_jobs",
                   action_identifier, action_mirror_jobs, 2, lexer),
    Reordered = unit(mirror_jobs, span(0, 11),
                     [span(7, 11), span(0, 6)], "mirror_jobs",
                     action_identifier, action_mirror_jobs, 2, lexer),
    expect_false(parse([Reversed, end(11)], _)),
    expect_false(parse([Overlap, end(11)], _)),
    expect_false(parse([Reordered, end(11)], _)).

run_test(partial_identifier_completion) :-
    Items = [edit_tear(edit, span(0, 8), "mirror_j"), end(8)],
    completions(Items, edit, Completions),
    expect_equal(
        Completions,
        [completion(span(0, 8), "mirror_jobs",
                    [alternative(mirror_jobs, action_identifier,
                                 action_mirror_jobs, 120)],
                    120, 1)]),
    once(parse(Items, assist(edit), Result)),
    expect_equal(Result,
                 parse_result(command(mirror_jobs, []),
                              incomplete(edit(edit)), [], 90)).

run_test(completion_between_known_units) :-
    mirror(Mirror),
    integer_unit(5, 8, 9, Id),
    Items = [Mirror, edit_tear(gap, span(7, 7), ""), Id, end(9)],
    completions(Items, gap, Completions),
    expect_equal(
        Completions,
        [completion(span(7, 7), "run",
                    [alternative(run, action_word, mirror_run_word, 120)],
                    120, 1),
         completion(span(7, 7), "pause",
                    [alternative(pause, action_word,
                                 action_mirror_pause, 100)],
                    100, 2),
         completion(span(7, 7), "rm",
                    [alternative(rm, action_word, action_mirror_rm, 95)],
                    95, 3)]),
    findall(Command,
            parse(Items, assist(gap),
                  parse_result(Command, incomplete(edit(gap)), _, _)),
            Commands),
    expect_equal(Commands,
                 [command(mirror_run, [job_id(5)]),
                  command(mirror_pause, [job_id(5)]),
                  command(mirror_rm, [job_id(5)])]).

run_test(visible_completion_merges_differing_descriptions) :-
    mirror(Mirror),
    Items = [Mirror, edit_tear(action, span(7, 8), "r"), end(8)],
    completions(Items, action, Completions),
    expect_equal(
        Completions,
        [completion(span(7, 8), "run",
                    [alternative(run, action_word,
                                 action_mirror_run_pending, 105),
                     alternative(run, action_word, mirror_run_word, 120)],
                    120, 1),
         completion(span(7, 8), "rm",
                    [alternative(rm, action_word, action_mirror_rm, 95)],
                    95, 2)]).

run_test(completion_ranking) :-
    mirror(Mirror),
    Items = [Mirror, edit_tear(action, span(7, 7), ""), end(7)],
    completions(Items, action, Completions),
    completion_visible_summary(Completions, Summary),
    expect_equal(Summary,
                 ["run"-120-1, "ls"-110-2,
                  "pause"-100-3, "rm"-95-4]).

completion_visible_summary([], []).
completion_visible_summary([completion(_, Text, _, Preference, Rank)|Items],
                           [Text-Preference-Rank|Summary]) :-
    completion_visible_summary(Items, Summary).

run_test(highlighting_uses_real_evidence_only) :-
    Mirror = unit(mirror, span(0, 6), [span(0, 2), span(2, 6)], "mirror",
                  command_namespace, mirror_namespace, 2, lexer(namespace)),
    Id = unit(integer(5), span(8, 9), [span(8, 9)], "5", integer,
              job_id, 2, lexer(number)),
    Items = [Mirror, edit_tear(gap, span(7, 7), ""), Id, end(9)],
    once(parse(Items, assist(gap), Result)),
    highlights(Result, Highlights),
    expect_equal(
        Highlights,
        [highlight(span(0, 2), command_namespace, mirror, lexer(namespace)),
         highlight(span(2, 6), command_namespace, mirror, lexer(namespace)),
         highlight(span(8, 9), integer, integer(5), lexer(number))]),
    expect_false(list_member(highlight(span(7, 7), _, _, _), Highlights)).

run_test(utf8_byte_spans_are_preserved) :-
    Unit = unit(mirror_jobs, span(0, 12), [span(8, 10)], "mirror_jöbs",
                action_identifier, action_mirror_jobs, 2, lexer(utf8_bytes)),
    once(parse([Unit, end(12)], Result)),
    highlights(Result, Highlights),
    expect_equal(Highlights,
                 [highlight(span(8, 10), action_identifier, mirror_jobs,
                            lexer(utf8_bytes))]).

run_test(render_lex_parse_roundtrip_all_actions) :-
    Commands = [command(mirror_jobs, []),
                command(mirror_run, [job_id(5)]),
                command(mirror_run_pending, []),
                command(mirror_pause, [job_id(7)]),
                command(mirror_rm, [job_id(8)])],
    roundtrip_commands(Commands).

roundtrip_commands([]).
roundtrip_commands([Command|Commands]) :-
    roundtrip_styles([verb, cli], Command),
    roundtrip_commands(Commands).

roundtrip_styles([], _Command).
roundtrip_styles([Style|Styles], Command) :-
    render(Command, Style, Text),
    lex_rendered(Text, Items),
    findall(Parsed,
            parse(Items, exact, parse_result(Parsed, complete, _, _)),
            ParsedCommands),
    expect_equal(ParsedCommands, [Command]),
    roundtrip_styles(Styles, Command).

% Minimal lexer used by the roundtrip test. It consumes the rendered bytes,
% computes spans from the actual text, and resolves literals from action facts.
lex_rendered(Text, Items) :-
    string_codes(Text, Codes),
    lex_codes(Codes, 0, Items).

lex_codes([], Position, [end(Position)]).
lex_codes(Codes, Start, [Unit|Items]) :-
    take_word(Codes, WordCodes, Rest),
    WordCodes = [_|_],
    ascii_codes(WordCodes),
    list_length(WordCodes, Width),
    Stop is Start + Width,
    string_codes(Surface, WordCodes),
    lexeme_unit(Surface, Start, Stop, Unit),
    lex_rest(Rest, Stop, Items).

lex_rest([], Stop, [end(Stop)]).
lex_rest([32|Codes], Stop, Items) :-
    Next is Stop + 1,
    lex_codes(Codes, Next, Items).

take_word([], [], []).
take_word([32|Codes], [], [32|Codes]).
take_word([Code|Codes], [Code|Word], Rest) :-
    Code =\= 32,
    take_word(Codes, Word, Rest).

ascii_codes([]).
ascii_codes([Code|Codes]) :-
    Code >= 0,
    Code =< 127,
    ascii_codes(Codes).

lexeme_unit(Surface, Start, Stop,
            unit(integer(Value), span(Start, Stop), [span(Start, Stop)],
                 Surface, integer, job_id, 2, render_lexer)) :-
    catch(number_string(Value, Surface), _, fail),
    integer(Value),
    Value >= 0,
    !.
lexeme_unit(Surface, Start, Stop,
            unit(Semantic, span(Start, Stop), [span(Start, Stop)], Surface,
                 Syntax, Description, Preference, render_lexer)) :-
    findall(signature(Semantic0, Syntax0, Description0, Preference0),
            lexical_literal(Surface, Semantic0, Syntax0,
                            Description0, Preference0),
            Signatures0),
    sort(Signatures0, Signatures),
    Signatures = [signature(Semantic, Syntax, Description, Preference)|_].

lexical_literal(Surface, Semantic, Syntax, Description, Preference) :-
    action(Name, Schema, Verb, Cli, ActionDescription, ActionPreference),
    action_grammar:valid_action_fact(Name, Schema, Verb, Cli,
                                     ActionDescription, ActionPreference),
    ( literal_in_form(Verb, Surface, Semantic, Syntax, Description, Preference)
    ; literal_in_form(Cli, Surface, Semantic, Syntax, Description, Preference)
    ).

literal_in_form([literal(Semantic, Surface, Syntax, Description, Preference)|_],
                Surface, Semantic, Syntax, Description, Preference).
literal_in_form([_|Specs], Surface, Semantic, Syntax, Description, Preference) :-
    literal_in_form(Specs, Surface, Semantic, Syntax, Description, Preference).

run_test(invalid_trailing_input) :-
    canonical(mirror_jobs, 11, Jobs),
    integer_unit(5, 12, 13, Trailing),
    expect_false(parse([Jobs, Trailing, end(13)], _)).

run_test(malformed_action_schema_is_excluded) :-
    Fact = action(broken_action, [job_id],
                  [literal(broken_action, "broken", action_identifier,
                           broken_description, 10)],
                  [literal(broken, "broken", action_word,
                           broken_description, 10)],
                  broken_description, 10),
    assertz(action_grammar:Fact),
    catch(check_malformed_action, Error,
          ( retractall(action_grammar:action(broken_action, _, _, _, _, _)),
            throw(Error) )),
    retractall(action_grammar:action(broken_action, _, _, _, _, _)).

check_malformed_action :-
    expect_false(valid_action(broken_action)),
    Items = [edit_tear(edit, span(0, 3), "bro"), end(3)],
    completions(Items, edit, Completions),
    expect_equal(Completions, []).

run_test(application_entry_is_closed_and_serialized) :-
    application(render, "request(command(mirror_jobs,[]),cli)", RenderOutput),
    term_string(RenderResponse, RenderOutput),
    expect_equal(RenderResponse, ok("mirror ls")),
    Items = [edit_tear(edit, span(0, 8), "mirror_j"), end(8)],
    term_string(request(Items, edit), CompleteInput),
    application(complete, CompleteInput, CompleteOutput),
    term_string(ok(Completions), CompleteOutput),
    expect_equal(
        Completions,
        [completion(span(0, 8), "mirror_jobs",
                    [alternative(mirror_jobs, action_identifier,
                                 action_mirror_jobs, 120)], 120, 1)]),
    lex_rendered("mirror run 5", ParseItems),
    term_string(request(ParseItems, exact), ParseInput),
    application(parse, ParseInput, ParseOutput),
    term_string(ParseResponse, ParseOutput),
    ParseResponse =
        ok([parse_result(command(mirror_run, [job_id(5)]),
                         complete, _, _)]),
    application(eval, "request(halt)", ClosedOutput),
    term_string(ClosedResponse, ClosedOutput),
    expect_equal(ClosedResponse, error(unknown_operation)),
    application(parse, "request(_)", VariableOutput),
    term_string(VariableResponse, VariableOutput),
    expect_equal(VariableResponse, error(invalid_request)),
    Result = parse_result(command(mirror_jobs, []), complete,
                          [evidence(mirror_jobs, span(0, 11), [span(0, 11)],
                                    "mirror_jobs", action_identifier,
                                    action_mirror_jobs, 2, lexer)], 92),
    term_string(request(Result), HighlightInput),
    application(highlights, HighlightInput, HighlightOutput),
    term_string(HighlightResponse, HighlightOutput),
    expect_equal(HighlightResponse,
                 ok([highlight(span(0, 11), action_identifier,
                               mirror_jobs, lexer)])).

run_test(registry_scale_regression) :-
    add_scale_actions(0, 512),
    catch(check_scale_actions, Error,
          ( remove_scale_actions, throw(Error) )),
    remove_scale_actions.

add_scale_actions(Index, Count) :-
    ( Index >= Count
    -> true
    ;  number_string(Index, Digits),
       string_concat("scale_action_", Digits, Surface),
       atom_string(Name, Surface),
       Fact = action(Name, [],
                     [literal(Name, Surface, action_identifier,
                              scale_description, 1)],
                     [literal(Name, Surface, action_identifier,
                              scale_description, 1)],
                     scale_description, 1),
       assertz(action_grammar:Fact),
       Next is Index + 1,
       add_scale_actions(Next, Count)
    ).

remove_scale_actions :-
    retractall(action_grammar:action(_, _, _, _, scale_description, _)).

check_scale_actions :-
    Items = [edit_tear(scale, span(0, 0), ""), end(0)],
    completions(Items, scale, Completions),
    list_length(Completions, Count),
    % Five canonical surfaces, one shared "mirror" CLI surface, and 512
    % synthetic registry actions.
    expect_equal(Count, 518),
    expect_true(list_member(completion(span(0, 0), "scale_action_511",
                                       _, 2, _), Completions)).
