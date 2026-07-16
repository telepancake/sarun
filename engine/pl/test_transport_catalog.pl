:- module(test_transport_catalog, [run_transport_catalog_tests/0]).

:- use_module(action_grammar).
:- discontiguous run_test/1.

test_name(catalog_is_closed_bounded_and_valid).
test_name(namespaces_are_explicit_unique_and_disjoint).
test_name(actions_are_not_duplicated_as_transport_requests).
test_name(register_schema_captures_conditional_fd_roles).
test_name(unix_values_remain_bytes).
test_name(events_are_compact_late_decode_invalidations).
test_name(stream_frames_cover_modes_directions_and_fd_handoffs).
test_name(all_transport_facts_project_through_central_relation).

run_transport_catalog_tests :-
    findall(Name, test_name(Name), Names),
    run_test_names(Names, 0, Passed),
    format('% transport_catalog: ~d tests passed~n', [Passed]).

run_test_names([], Passed, Passed).
run_test_names([Name|Names], Passed0, Passed) :-
    format('% transport_catalog:~w ... ', [Name]),
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

run_test(catalog_is_closed_bounded_and_valid) :-
    expect(valid_transport_catalog),
    findall(Name, wire_request(Name, _, _, _, _, _), Requests),
    findall(Name, wire_response(Name, _, _), Responses),
    findall(Name, wire_mode(Name, _, _), Modes),
    findall(Name, wire_event(Name, _, _), Events),
    findall(Stream-Name, wire_frame(Stream, Name, _, _, _, _, _), Frames),
    length(Requests, 16),
    length(Responses, 6),
    length(Modes, 7),
    length(Events, 10),
    length(Frames, 11),
    expect(wire_limit(frame_bytes, 16777216)),
    expect(wire_response(action, 6, [field(value, action_success)])).

run_test(namespaces_are_explicit_unique_and_disjoint) :-
    expect(wire_protocol_version(1)),
    findall(Code, wire_handler(_, Code, _), ActionCodes),
    findall(Code, wire_request(_, Code, _, _, _, _), RequestCodes),
    expect(all_less_than(ActionCodes, 256)),
    expect(all_at_least(RequestCodes, 256)),
    expect(no_shared_code(ActionCodes, RequestCodes)).

all_less_than([], _).
all_less_than([Value|Values], Bound) :-
    Value < Bound,
    all_less_than(Values, Bound).

all_at_least([], _).
all_at_least([Value|Values], Bound) :-
    Value >= Bound,
    all_at_least(Values, Bound).

no_shared_code([], _).
no_shared_code([Code|Codes], Other) :-
    \+ member_eq(Code, Other),
    no_shared_code(Codes, Other).

member_eq(Value, [Head|_]) :- Value == Head, !.
member_eq(Value, [_|Values]) :- member_eq(Value, Values).

run_test(actions_are_not_duplicated_as_transport_requests) :-
    expect(\+ wire_request(select, _, _, _, _, _)),
    expect(\+ wire_request(apply, _, _, _, _, _)),
    expect(\+ wire_request(discard, _, _, _, _, _)),
    expect(\+ wire_request(rename, _, _, _, _, _)),
    expect(\+ wire_request(patch, _, _, _, _, _)),
    expect(\+ wire_request(sudtrace, _, _, _, _, _)),
    expect(\+ wire_request(shutdown, _, _, _, _, _)),
    once(representation(sudtrace, wire,
                        wire(98, sudtrace, control,
                             [field(sid, box_id)],
                             sud_trace_view))),
    once(representation(quit, wire,
                        wire(131, quit, control, [], unit))).

run_test(register_schema_captures_conditional_fd_roles) :-
    once(wire_request(register, 257, mode(box), Fields, Fds,
                      pidfd_identity)),
    expect(member_eq(field(name, registration_name), Fields)),
    expect(member_eq(field(backend, run_backend), Fields)),
    expect(member_eq(field(architecture, option(qemu_architecture)), Fields)),
    expect(member_eq(field(net_mode, net_mode), Fields)),
    expect(\+ member_field(want_rerun, Fields)),
    expect_equal(Fds,
                 [fd(pidfd, required, always),
                  fd(tap, required, when(net_mode, tap)),
                  fd(sud_trace, required, when(backend, sud))]),
    once(wire_mode(box, 3, [field(registration, register_reply)])).

member_field(Name, [field(Seen, _)|_]) :- Name == Seen, !.
member_field(Name, [_|Fields]) :- member_field(Name, Fields).

run_test(unix_values_remain_bytes) :-
    once(wire_type(path, alias(bytes(path_bytes)))),
    once(wire_type(os_string, alias(bytes(text_bytes)))),
    once(wire_type(environment,
                   alias(map(short_os_string, os_string,
                             environment_entries)))),
    once(wire_type(process_provenance, record(Provenance))),
    expect(member_eq(field(executable, path), Provenance)),
    expect(member_eq(field(argv, list(os_string, 1, command_items)), Provenance)).

run_test(events_are_compact_late_decode_invalidations) :-
    once(wire_event(brush_provenance_added, 6,
                    [field(box, box_id), field(row, row_id)])),
    once(wire_event(build_graph_changed, 7,
                    [field(box, box_id), field(phase, edge_phase)])),
    once(wire_event(overlay_changed, 4,
                    [field(box, box_id), field(count, u32),
                     field(latest_path, option(path))])).

run_test(stream_frames_cover_modes_directions_and_fd_handoffs) :-
    once(wire_frame(box, mute, 4, runner_to_engine, [],
                    [fd(pidfd, required, always)], stay)),
    once(wire_frame(box, connection, 14, engine_to_runner, [],
                    [fd(connected_socket, required, always)], stay)),
    once(wire_frame(pty, resize, 8, client_to_engine,
                    [field(rows, u16), field(columns, u16)], [], stay)),
    once(wire_frame(service_accept, paired, 1, engine_to_service, [], [],
                    handoff(raw_service))).

run_test(all_transport_facts_project_through_central_relation) :-
    expect(all_requests_project),
    expect(all_responses_project),
    expect(all_modes_project),
    expect(all_events_project),
    expect(all_frames_project),
    expect(all_types_project),
    once(convert(wire,
                 request(270, reply(budget),
                         [field(target, box_target), field(amount, s64)],
                         [], target_box(target)),
                 wire,
                 request(270, reply(budget),
                         [field(target, box_target), field(amount, s64)],
                         [], target_box(target)))).

all_requests_project :-
    \+ (wire_request(Name, Code, Success, Fields, Fds, Authority),
        \+ representation(transport(request, Name), wire,
                          request(Code, Success, Fields, Fds, Authority))).

all_responses_project :-
    \+ (wire_response(Name, Code, Fields),
        \+ representation(transport(response, Name), wire,
                          response(Code, Fields))).

all_modes_project :-
    \+ (wire_mode(Name, Code, Fields),
        \+ representation(transport(mode, Name), wire,
                          mode(Code, Fields))).

all_events_project :-
    \+ (wire_event(Name, Code, Fields),
        \+ representation(transport(event, Name), wire,
                          event(Code, Fields))).

all_frames_project :-
    \+ (wire_frame(Stream, Name, Code, Direction, Fields, Fds, Transition),
        \+ representation(transport(frame(Stream), Name), wire,
                          frame(Code, Direction, Fields, Fds, Transition))).

all_types_project :-
    \+ (wire_type(Name, Definition),
        \+ representation(transport(type, Name), schema, Definition)).
