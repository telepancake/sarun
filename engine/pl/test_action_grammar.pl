:- module(test_action_grammar, [run_action_grammar_tests/0]).

:- use_module(action_grammar).
:- use_module(relation_api).
:- discontiguous run_test/1.

% Core-only test runner: this same file runs in the package-free embedded image.
test_name(catalog_is_complete_and_valid).
test_name(wire_identities_are_explicit_unique_and_normalized).
test_name(representations_project_the_executable_forms).
test_name(neutral_source_parses_canonical_form).
test_name(action_identifier_has_one_mechanical_text_encoding).
test_name(argument_projection_is_wire_ready).
test_name(parsed_commands_relate_to_closed_action_requests).
test_name(action_request_cardinality_is_relational).
test_name(action_request_values_enforce_wire_bounds_and_types).
test_name(closed_override_requests_use_the_same_relation).
test_name(structured_specs_parse_render_and_materialize_relationally).
test_name(every_wire_handler_materializes_from_closed_source_values).
test_name(string_kinds_do_not_become_numbers).
test_name(array_wire_shape_is_preserved).
test_name(parse_render_roundtrip).
test_name(all_actions_roundtrip_through_shared_sequence_relation).
test_name(completion_is_projected_from_tear_parse_evidence).
test_name(tear_parse_checks_every_concrete_suffix_item).
test_name(application_surface_is_closed).
test_name(complete_action_language_is_an_immutable_grammar_value).
test_name(generic_action_relation_parses_and_renders_projection).
test_name(generic_action_relation_resolves_context_before_projection).
test_name(generic_action_relation_declares_dependent_context_graph).
test_name(generic_action_completion_survives_context_roundtrip).
test_name(generic_action_help_is_constant_branch_projection).

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

relation_limits(limits(256, 4096, 1048576)).

run_test(complete_action_language_is_an_immutable_grammar_value) :-
    once(action_relation_grammar(choice_grammar(Alternatives))),
    length(Alternatives, 108),
    list_has(alternative(kill, 50,
                         projection_grammar(sequence_grammar(_, _, _, _),
                                            [projection(command, _),
                                             projection(action_target,
                                                        constant(ui)),
                                             projection(help, _),
                                             projection(help_filter, _)])),
             Alternatives),
    list_has(alternative('oci.build', 50,
                         projection_grammar(sequence_grammar(_, _, _, _),
                                            [projection(command, _),
                                             projection(action_target,
                                                        constant(ui)),
                                             projection(help, _),
                                             projection(help_filter, _)])),
             Alternatives).

run_test(generic_action_help_is_constant_branch_projection) :-
    once(action_relation_grammar(Grammar)),
    relation_limits(Limits),
    transform(
        request(Grammar, given([binding(action_target, control)]),
                want([help]), observations([]), Limits),
        reply(Solutions, [], [], [])),
    length(Solutions, 5),
    list_has(solution([binding(help,
                               record("quit", "", "quit the engine"))], _),
             Solutions),
    transform(
        request(Grammar,
                given([binding(action_target, ui),
                       binding(help_filter, "mirror")]),
                want([help]), observations([]), Limits),
        reply(MirrorSolutions, [], [], [])),
    length(MirrorSolutions, 9).

run_test(generic_action_relation_parses_and_renders_projection) :-
    once(action_relation_grammar(Grammar)),
    items(["mirror", "resume", "7"], Items),
    relation_limits(Limits),
    transform(
        request(Grammar,
                given([binding(source, source(Items, exact))]),
                want([command, status]), observations([]), Limits),
        reply([solution([binding(command,
                                 command(mirror_resume, mirror_pause, ui,
                                         [integer(7), boolean(false)])),
                         binding(status, complete)], _)], [], [], [])),
    transform(
        request(Grammar,
                given([binding(command,
                               command(mirror_resume, mirror_pause, ui,
                                       [integer(7), boolean(false)]))]),
                want([source]), observations([]), Limits),
        reply([solution([binding(source, "mirror resume 7")], 79)],
              [], [], [])).

run_test(generic_action_relation_resolves_context_before_projection) :-
    once(action_relation_grammar(Grammar)),
    items(["kill", "C1"], Items),
    relation_limits(Limits),
    Query = ask(one, box, name("C1")),
    Request0 = request(
        Grammar, given([binding(source, source(Items, exact))]),
        want([command]), observations([]), Limits),
    transform(Request0,
              reply([solution([binding(command,
                                       command(kill, kill, ui,
                                               [string("C1")]))], _)],
                    [query(branch(kill, q(1)), Query)], [], [])),
    Entry = entry(box, 1, ["C1"], integer(1), []),
    Observation = observed(branch(kill, q(1)), Query, source(boxes, 9),
                           some(one(Entry))),
    Request1 = request(
        Grammar, given([binding(source, source(Items, exact))]),
        want([command]), observations([Observation]), Limits),
    transform(Request1,
              reply([solution([binding(command,
                                       command(kill, kill, ui,
                                               [integer(1)]))], _)],
                    [query(branch(kill, q(1)), Query)],
                    [dependency(branch(kill, q(1)), Query,
                                some(one(Entry)))], [])),
    once(action_request(command(kill, kill, ui, [integer(1)]),
                        action_request(kill, 59, [1]))).

run_test(generic_action_relation_declares_dependent_context_graph) :-
    once(action_relation_grammar(Grammar)),
    items(["writer", "id", "C1", "src/main.c"], Items),
    relation_limits(Limits),
    BoxQuery = ask(one, box, name("C1")),
    PathQuery = ask(one, path,
                    within(box(ref(branch(writer_id, q(1)))),
                           name("src/main.c"))),
    Graph = [query(branch(writer_id, q(1)), BoxQuery),
             query(branch(writer_id, q(2)), PathQuery)],
    transform(
        request(Grammar, given([binding(source, source(Items, exact))]),
                want([command]), observations([]), Limits),
        reply(_, Graph, [], [])),
    BoxEntry = entry(box, 1, ["C1"], integer(1), []),
    BoxObservation = observed(branch(writer_id, q(1)), BoxQuery,
                              source(boxes, 9), some(one(BoxEntry))),
    transform(
        request(context_grammar, given([binding(graph, Graph)]), want([ready]),
                observations([BoxObservation]), Limits),
        reply([solution([binding(ready,
                                 [query(branch(writer_id, q(2)),
                                        ask(one, path,
                                            within(box(integer(1)),
                                                   name("src/main.c"))))])],
                        0)], [], _, [])),
    PathEntry = entry(path, 8, ["src/main.c"], path("src/main.c"),
                      [within(box(integer(1)))]),
    PathObservation = observed(branch(writer_id, q(2)), PathQuery,
                               source(paths, 3), some(one(PathEntry))),
    transform(
        request(Grammar, given([binding(source, source(Items, exact))]),
                want([command]),
                observations([BoxObservation, PathObservation]), Limits),
        reply([solution([binding(command,
                                 command(writer_id, writer_id, ui,
                                         [integer(1),
                                          path("src/main.c")]))], _)],
              Graph, _, [])).

run_test(generic_action_completion_survives_context_roundtrip) :-
    once(action_relation_grammar(Grammar)),
    neutral("kill", 0, Kill),
    Items = [Kill, edit_tear(edit, span(5, 6), "C"), end(6)],
    relation_limits(Limits),
    Query = ask(all, box, prefix("C")),
    Request0 = request(
        Grammar, given([binding(source, source(Items, assist(edit)))]),
        want([command, completions]), observations([]), Limits),
    transform(Request0,
              reply(_, [query(branch(kill, q(1)), Query)], [], [])),
    Entries = [entry(box, 1, ["C1"], integer(1), [])],
    Observation = observed(branch(kill, q(1)), Query, source(boxes, 9),
                           some(all(Entries))),
    Request1 = request(
        Grammar, given([binding(source, source(Items, assist(edit)))]),
        want([command, completions]), observations([Observation]), Limits),
    transform(Request1, Reply),
    Reply = reply(
        [solution([binding(command,
                           command(kill, kill, ui,
                                   [hole(sid, string)])),
                   binding(completions, Completions)], 90)],
        [query(branch(kill, q(1)), Query)], _, []),
    Completions = [completion(span(5, 6), "C1",
                              [alternative(context(kill, box, 1), context_argument,
                                           boxes, 90)],
                              90, 1)].

list_has(Value, [Head|_]) :- Value = Head.
list_has(Value, [_|Tail]) :- list_has(Value, Tail).

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

parse_words_from_text(Text, Command) :-
    split_string(Text, " ", "", Words),
    parse_words(Words, Command).

run_test(catalog_is_complete_and_valid) :-
    expect(valid_action_catalog),
    findall(Action, action(Action, _, _, _, _, _, _), Actions),
    sort(Actions, Unique),
    length(Actions, 108),
    length(Unique, 108),
    expect(all_valid(Actions)).

run_test(wire_identities_are_explicit_unique_and_normalized) :-
    findall(Code-Handler-Result,
            wire_handler(Handler, Code, Result), Rows),
    length(Rows, 95),
    findall(Code, wire_handler(_, Code, _), Codes),
    sort(Codes, UniqueCodes),
    length(UniqueCodes, 95),
    findall(Handler, wire_handler(Handler, _, _), Handlers),
    sort(Handlers, UniqueHandlers),
    length(UniqueHandlers, 95),
    expect(all_wire_handlers_are_actions(Handlers)),
    expect(all_wire_results_are_concrete(Rows)),
    expect(all_wire_requests_are_concrete(Handlers)),
    once(representation(mirror_resume, wire, Wire)),
    expect_equal(
        Wire,
        wire(62, mirror_pause, ui,
             [field(id, job_id), field(paused, bool)],
             unit)),
    expect(\+ representation(mirror_browse, wire, _)).

run_test(representations_project_the_executable_forms) :-
    once(representation(mirror_resume, command,
                        command(["mirror", "resume"], resume_false))),
    once(representation(
        mirror_resume, syntax,
        syntax([literal(mirror, "mirror", command_namespace,
                        mirror_resume, 10),
                literal(resume, "resume", action_word,
                        mirror_resume, 20),
                argument(arg(id, integer, required, scalar))]))),
    once(convert(action, mirror_resume, wire,
                 wire(62, mirror_pause, ui,
                      [field(id, job_id), field(paused, bool)],
                      unit))),
    once(representation(
        mirror_resume, source_schema,
        schema([arg(id, integer, required, scalar)]))),
    once(convert(command, command(["mirror", "resume"], resume_false), help,
                 help("ID", "resume a mirror job"))),
    findall(Action, action(Action, _, _, _, _, _, _), Actions),
    expect(all_actions_have_singular_core_representations(Actions)).

all_actions_have_singular_core_representations([]).
all_actions_have_singular_core_representations([Action|Actions]) :-
    findall(Command, representation(Action, command, Command), [_]),
    findall(Help, representation(Action, help, Help), [_]),
    findall(Syntax, representation(Action, syntax, Syntax), [_]),
    all_actions_have_singular_core_representations(Actions).

all_wire_handlers_are_actions([]).
all_wire_handlers_are_actions([Handler|Handlers]) :-
    action(Handler, Handler, Target, _, _, _, _),
    ( Target = ui ; Target = control ),
    all_wire_handlers_are_actions(Handlers).

all_wire_results_are_concrete([]).
all_wire_results_are_concrete([_-_-Result|Rows]) :-
    valid_wire_type(Result),
    Result \= response,
    all_wire_results_are_concrete(Rows).

all_wire_requests_are_concrete([]).
all_wire_requests_are_concrete([Handler|Handlers]) :-
    findall(Fields, wire_request_fields(Handler, Fields), [Fields]),
    valid_wire_fields(Fields),
    all_wire_requests_are_concrete(Handlers).

all_valid([]).
all_valid([Action|Actions]) :-
    valid_action(Action),
    all_valid(Actions).

run_test(neutral_source_parses_canonical_form) :-
    parse_words(["review", "map", "ids", "12", "process", "3", "4", "edge"],
                command('review.map_ids', 'review.map_ids', ui,
                        [string("12"), string("process"),
                         array([integer(3), integer(4)]), string("edge")])).

run_test(action_identifier_has_one_mechanical_text_encoding) :-
    findall(Words,
            representation(_, command, command(Words, _)),
            CommandNames),
    length(CommandNames, 108),
    sort(CommandNames, UniqueCommandNames),
    length(UniqueCommandNames, 108),
    parse_words(["mirror", "run", "pending"],
                command(mirror_run_pending, mirror_run_pending, ui, [])),
    parse_words(["mirror", "run", "5"],
                command(mirror_run, mirror_run, ui, [integer(5)])),
    expect(\+ parse_words(["mirror_run", "5"], _)),
    expect(\+ parse_words(["mirror", "ls"], _)),
    parse_words(["mirror", "jobs"],
                command(mirror_jobs, mirror_jobs, ui, [])).

run_test(argument_projection_is_wire_ready) :-
    parse_words(["mirror", "pause", "5"],
                command(mirror_pause, mirror_pause, ui,
                        [integer(5), boolean(true)])),
    parse_words(["mirror", "resume", "5"],
                command(mirror_resume, mirror_pause, ui,
                        [integer(5), boolean(false)])).

run_test(parsed_commands_relate_to_closed_action_requests) :-
    parse_words(["mirror", "resume", "5"], Resume),
    once(action_request(Resume, ResumeRequest)),
    expect_equal(ResumeRequest,
                 action_request(mirror_pause, 62, [5, false])),
    parse_words(["mirror", "add", "git", "source", "destination", "30"],
                Add),
    once(action_request(Add, AddRequest)),
    expect_equal(AddRequest,
                 action_request(mirror_add, 60,
                                ["git", "source", "destination", some(30)])),
    once(action_request(
        command('review.apply', 'review.apply', ui,
                [integer(7), array([path("one"), path("two")])]),
        ApplyRequest)),
    expect_equal(ApplyRequest,
                 action_request('review.apply', 14,
                                [7, ["one", "two"]])).

run_test(action_request_cardinality_is_relational) :-
    once(action_request(
        command('flows.detail', 'flows.detail', ui, [integer(9)]),
        FrameOnly)),
    expect_equal(FrameOnly,
                 action_request('flows.detail', 1, [none, 9])),
    once(action_request(
        command('flows.detail', 'flows.detail', ui,
                [integer(4), integer(9)]),
        BoxAndFrame)),
    expect_equal(BoxAndFrame,
                 action_request('flows.detail', 1, [some(4), 9])).

run_test(action_request_values_enforce_wire_bounds_and_types) :-
    expect(\+ action_request(
        command('view.window', 'view.window', ui,
                [integer(1), integer(-1), integer(2)]), _)),
    expect(\+ action_request(
        command(mirror_pause, mirror_pause, ui,
                [integer(1), string("false")]), _)),
    expect(\+ action_request(
        command('review.write_file', 'review.write_file', ui,
                [integer(7), path("file"), base64("not base64")]), _)),
    once(action_request(
        command('review.write_file', 'review.write_file', ui,
                [integer(7), path("file"), base64("eA==")]),
        action_request('review.write_file', 33,
                       [7, "file", base64("eA==")]))),
    once(action_request(
        command(rename, rename, control,
                [integer(7), string("ž")]),
        action_request(rename, 130, [7, "ž"]))).

run_test(closed_override_requests_use_the_same_relation) :-
    once(action_request(
        command(ro_attach, ro_attach, ui,
                [integer(7), integer(2), integer(3)]),
        RoAttach)),
    expect_equal(RoAttach,
                 action_request(ro_attach, 84,
                                [7, [box(2), box(3)]])),
    once(action_request(
        command('view.open', 'view.open', ui,
                [string("changes"), integer(7),
                 string("path:src/main.rs"), boolean(false)]),
        ViewOpen)),
    expect_equal(
        ViewOpen,
        action_request('view.open', 38,
                       [changes, 7,
                        some([record(path, "src/main.rs", and,
                                     false, true)]),
                        false])),
    once(action_request(
        command('view.filter', 'view.filter', ui,
                [integer(9), string("none")]),
        action_request('view.filter', 36, [9, none]))).

run_test(structured_specs_parse_render_and_materialize_relationally) :-
    OciJson = "{\"build_args\":[[\"A\",\"one\"]],\"net\":\"tap\",\"tag\":null,\"dockerfile\":\"FROM\\u0020scratch\\n\",\"context_tar_gz\":\"eA==\"}",
    parse_words(["oci", "build", OciJson], OciCommand),
    expect_equal(
        OciCommand,
        command('oci.build', 'oci.build', ui,
                [oci_spec("eA==", "FROM scratch\n", none, "tap",
                          [pair("A", "one")]) ])),
    once(action_request(OciCommand, OciRequest)),
    expect_equal(
        OciRequest,
        action_request('oci.build', 7,
                       [record(base64("eA=="), "FROM scratch\n", none,
                               tap, [pair("A", "one")])])),
    render(OciCommand, OciRendered),
    parse_words_from_text(OciRendered, OciRoundtrip),
    expect_equal(OciRoundtrip, OciCommand),
    ApiJson = "{\"api_key\":\"secret\",\"model\":\"m\",\"base_url\":\"https://example.test/v1\"}",
    parse_words(["oaita", "probe", ApiJson], ApiCommand),
    once(action_request(ApiCommand,
        action_request('oaita.probe', 5,
                       [record("https://example.test/v1", "m", "secret")]))),
    expect(\+ parse_words(
        ["oaita", "probe",
         "{\"base_url\":\"x\",\"model\":\"m\",\"api_key\":\"k\",\"model\":\"duplicate\"}"],
        _)),
    expect(\+ action_request(
        command('oci.build', 'oci.build', ui,
                [oci_spec("bad", "FROM", none, "tap", [])]), _)),
    expect(\+ action_request(
        command('oci.build', 'oci.build', ui,
                [oci_spec("eA==", "FROM", none, "unknown", [])]), _)).

run_test(every_wire_handler_materializes_from_closed_source_values) :-
    findall(Handler, wire_handler(Handler, _, _), Handlers),
    expect(all_wire_handlers_materialize(Handlers)).

all_wire_handlers_materialize([]).
all_wire_handlers_materialize([Handler|Handlers]) :-
    sample_handler_command(Handler, Command),
    once(action_request(Command, _)),
    all_wire_handlers_materialize(Handlers).

sample_handler_command(ro_attach,
                       command(ro_attach, ro_attach, ui,
                               [integer(7), integer(8)])) :- !.
sample_handler_command('view.open',
                       command('view.open', 'view.open', ui,
                               [string("changes"), integer(7),
                                string("none"), boolean(true)])) :- !.
sample_handler_command('view.filter',
                       command('view.filter', 'view.filter', ui,
                               [integer(7), string("none")])) :- !.
sample_handler_command('oci.build',
                       command('oci.build', 'oci.build', ui,
                               [oci_spec("eA==", "FROM", none, "tap", [])])) :- !.
sample_handler_command('oaita.probe',
                       command('oaita.probe', 'oaita.probe', ui,
                               [api_spec("https://example.test/v1", "m", "k")])) :- !.
sample_handler_command(Handler,
                       command(Handler, Handler, Target, Arguments)) :-
    action(Handler, Handler, Target, _, _, _, _),
    action_catalog:argument_schema(Handler, Schema),
    action_catalog:wire_request_fields(Handler, Fields),
    sample_request_arguments(Schema, Fields, Arguments).

sample_request_arguments([], [], []).
sample_request_arguments(
    [arg(Name, Kind, required, scalar)|Schema],
    [field(Name, Type)|Fields], [Value|Arguments]) :-
    sample_wire_source(Kind, Type, Value),
    sample_request_arguments(Schema, Fields, Arguments).
sample_request_arguments(
    [arg(Name, Kind, optional, scalar)|Schema],
    [field(Name, option(Type))|Fields], [Value|Arguments]) :-
    sample_wire_source(Kind, Type, Value),
    sample_request_arguments(Schema, Fields, Arguments).
sample_request_arguments(
    [arg(Name, Kind, repeated, array)|Schema],
    [field(Name, list(Type, _))|Fields], [array([Value])|Arguments]) :-
    sample_wire_source(Kind, Type, Value),
    sample_request_arguments(Schema, Fields, Arguments).
sample_request_arguments(
    [arg(Name, Kind, repeated, spread)],
    [field(Name, list(Type, _))], [Value]) :-
    sample_wire_source(Kind, Type, Value).

sample_wire_source(Kind, Type, Value) :-
    transport_catalog:wire_type(Type, alias(Alias)), !,
    sample_wire_source(Kind, Alias, Value).
sample_wire_source(_, u16, integer(7)) :- !.
sample_wire_source(_, u32, integer(7)) :- !.
sample_wire_source(_, u64, integer(7)) :- !.
sample_wire_source(_, s32, integer(7)) :- !.
sample_wire_source(_, s64, integer(7)) :- !.
sample_wire_source(_, bool, boolean(true)) :- !.
sample_wire_source(_, text(_), string("text")) :- !.
sample_wire_source(path, bytes(_), path("path/to/file")) :- !.
sample_wire_source(base64, bytes(_), base64("eA==")) :- !.
sample_wire_source(_, Type, string(Text)) :-
    transport_catalog:wire_type(Type, enum),
    once(transport_catalog:wire_enum(Type, Case, _)),
    atom_string(Case, Text).

run_test(string_kinds_do_not_become_numbers) :-
    parse_words(["rename", "7", "00042"],
                command(rename, rename, control,
                        [string("7"), string("00042")])).

run_test(array_wire_shape_is_preserved) :-
    parse_words(["review", "apply", "7", "one", "two"],
                command('review.apply', 'review.apply', ui,
                        [string("7"), array([path("one"), path("two")])])).

run_test(parse_render_roundtrip) :-
    parse_words(["mirror", "resume", "5"], Command),
    render(Command, "mirror resume 5").

run_test(all_actions_roundtrip_through_shared_sequence_relation) :-
    findall(Action, action(Action, _, _, _, _, _, _), Actions),
    expect(all_action_forms_roundtrip(Actions)).

all_action_forms_roundtrip([]).
all_action_forms_roundtrip([Action|Actions]) :-
    form_roundtrips(Action, minimal),
    form_roundtrips(Action, full),
    all_action_forms_roundtrip(Actions).

form_roundtrips(Action, Population) :-
    once(action_grammar:action_form(Action, Specs, _)),
    spec_surfaces(Specs, Population, Surfaces),
    items(Surfaces, SourceItems),
    once(( parse(SourceItems, parse_result(Command, complete, _, _)),
           Command = command(Action, _, _, _)
         )),
    once(render(Command, Rendered)),
    split_string(Rendered, " ", "", RenderedSurfaces),
    items(RenderedSurfaces, RenderedItems),
    once(parse(RenderedItems,
               parse_result(Command, complete, _, _))).

spec_surfaces([], _, []).
spec_surfaces([literal(_, Text, _, _, _)|Specs], Population,
              [Text|Surfaces]) :-
    spec_surfaces(Specs, Population, Surfaces).
spec_surfaces([argument(arg(_, Kind, required, scalar))|Specs], Population,
              [Surface|Surfaces]) :-
    sample_surface(Kind, Surface),
    spec_surfaces(Specs, Population, Surfaces).
spec_surfaces([argument(arg(_, _, optional, scalar))|Specs], minimal,
              Surfaces) :-
    spec_surfaces(Specs, minimal, Surfaces).
spec_surfaces([argument(arg(_, Kind, optional, scalar))|Specs], full,
              [Surface|Surfaces]) :-
    sample_surface(Kind, Surface),
    spec_surfaces(Specs, full, Surfaces).
spec_surfaces([argument(arg(_, _, repeated, _))|Specs], minimal, Surfaces) :-
    spec_surfaces(Specs, minimal, Surfaces).
spec_surfaces([argument(arg(_, Kind, repeated, _))|Specs], full,
              [First, Second|Surfaces]) :-
    sample_surfaces(Kind, First, Second),
    spec_surfaces(Specs, full, Surfaces).

sample_surface(boolean, "true").
sample_surface(integer, "7").
sample_surface(string, "text").
sample_surface(path, "path/to/file").
sample_surface(base64, "eA==").
sample_surface(oci_spec, "{\"context_tar_gz\":\"eA==\",\"dockerfile\":\"FROM\",\"tag\":null,\"net\":\"tap\",\"build_args\":[]}").
sample_surface(api_spec, "{\"base_url\":\"https://example.test/v1\",\"model\":\"m\",\"api_key\":\"k\"}").
sample_surface(spec, "kind=value").

sample_surfaces(integer, "7", "8") :- !.
sample_surfaces(Kind, First, Second) :-
    sample_surface(Kind, First),
    sample_surface(Kind, Second).

run_test(completion_is_projected_from_tear_parse_evidence) :-
    neutral("mirror", 0, Mirror),
    Items = [Mirror, edit_tear(edit, span(7, 8), "r"), end(8)],
    once(parse(
        Items, assist(edit),
        parse_result(
            command(mirror_run, mirror_run, ui, [hole(id, integer)]),
            incomplete(edit(edit)), Evidence, _))),
    expect(tear_literal(Evidence, edit, "run")),
    completions(Items, edit, Completions),
    expect(member_completion("run", Completions)),
    items(["mirror", "run", "5"], ParseItems),
    once(parse(ParseItems, Candidate)),
    highlights(Candidate, Highlights),
    expect(Highlights \= []).

run_test(tear_parse_checks_every_concrete_suffix_item) :-
    neutral("mirror", 0, Mirror),
    neutral("5", 9, Five),
    Valid = [Mirror, edit_tear(edit, span(7, 8), "r"), Five, end(10)],
    completions(Valid, edit, ValidCompletions),
    expect(member_completion("run", ValidCompletions)),
    neutral("not-an-integer", 9, Bad),
    Invalid = [Mirror, edit_tear(edit, span(7, 8), "r"), Bad, end(23)],
    completions(Invalid, edit, InvalidCompletions),
    expect(\+ member_completion("run", InvalidCompletions)),
    neutral("extra", 11, Extra),
    Trailing = [Mirror, edit_tear(edit, span(7, 8), "r"), Five, Extra,
                end(16)],
    completions(Trailing, edit, TrailingCompletions),
    expect(\+ member_completion("run", TrailingCompletions)).

tear_literal([evidence(_, _, _, _, _, _, _,
                       tear(EditId, literal(Text)))|_], EditId, Text).
tear_literal([_|Evidence], EditId, Text) :-
    tear_literal(Evidence, EditId, Text).

member_completion(Text, [completion(_, Text, _, _, _)|_]).
member_completion(Text, [_|Completions]) :- member_completion(Text, Completions).

run_test(application_surface_is_closed) :-
    application(parse, "request([end(0)],exact)", ParseOutput),
    expect_equal(ParseOutput, "error(invalid_operation)"),
    application(shell, "request(halt)", ShellOutput),
    expect_equal(ShellOutput, "error(invalid_operation)").
