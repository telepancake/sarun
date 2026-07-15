:- module(transport_catalog,
          [ wire_protocol_version/1,
            wire_limit/2,
            wire_type/2,
            wire_enum/3,
            wire_variant/4,
            wire_request/6,
            wire_response/3,
            wire_mode/3,
            wire_event/3,
            wire_frame/7,
            valid_transport_catalog/0
          ]).

:- discontiguous wire_type/2.
:- discontiguous wire_enum/3.
:- discontiguous wire_variant/4.

/** <module> Normalized direct-Rust transport relation

This is the semantic definition of sarun's non-action binary transport.  It is
not a serializer implementation and it is not a second command catalog.
Action requests use `action_catalog:wire_handler/2`; the facts below cover only
connection/lifecycle messages, replies, events, stream modes, mux frames, and
SCM_RIGHTS roles.

Every compound is positional.  Field names are relation metadata used for
conversion, generation, help, and inspection; field-name strings are never
sent.  Every variable-length leaf and collection names an explicit bound.
Numeric identities are facts, never declaration-order indexes.

The type algebra is deliberately small:

  * scalar atoms: `bool`, `u16`, `u32`, `u64`, `s32`, `s64`, `f64`;
  * `text(Limit)`, `bytes(Limit)`, and `fixed_bytes(Length)`;
  * `option(Type)`, `list(Type, Limit)`, `list(Type, Min, Limit)`, and
    `map(Key, Value, Limit)`;
  * a named alias, record, enum, or tagged choice declared below;
  * `response`, the nested response compound selected by reply mode.

`option` and `bool` have fixed codec tags.  Every domain enum and tagged choice
has explicit stable case codes in `wire_enum/3` or `wire_variant/4`.
*/

wire_protocol_version(1).

wire_limit(frame_bytes,       16777216).
wire_limit(blob_bytes,        16777216).
wire_limit(text_bytes,         1048576).
wire_limit(path_bytes,         1048576).
wire_limit(stream_chunk_bytes, 1048576).
wire_limit(short_bytes,           4096).
wire_limit(collection_items,      65536).
wire_limit(command_items,         65536).
wire_limit(environment_entries,   32768).
wire_limit(error_items,            4096).
wire_limit(stage_items,            4096).

% Semantic scalar/leaf aliases. Unix paths, argv, and environment contents are
% bytes; no transport decoder is allowed to make them UTF-8 merely because the
% old JSON path did.
wire_type(box_id,          alias(u64)).
wire_type(row_id,          alias(u64)).
wire_type(pipeline_id,     alias(u64)).
wire_type(timestamp,       alias(f64)).
wire_type(exit_code,       alias(s32)).
wire_type(path,            alias(bytes(path_bytes))).
wire_type(os_string,       alias(bytes(text_bytes))).
wire_type(short_os_string, alias(bytes(short_bytes))).
wire_type(service_name,    alias(text(short_bytes))).
wire_type(box_selector,    alias(text(short_bytes))).
wire_type(ipv4,            alias(fixed_bytes(4))).
wire_type(owner_token,     alias(fixed_bytes(16))).
wire_type(environment,
          alias(map(short_os_string, os_string, environment_entries))).

wire_type(net_mode, enum).
wire_enum(net_mode, off,  1).
wire_enum(net_mode, host, 2).
wire_enum(net_mode, tap,  3).

wire_type(run_backend, enum).
wire_enum(run_backend, fuse, 1).
wire_enum(run_backend, sud,  2).

wire_type(error_category, enum).
wire_enum(error_category, invalid_request, 1).
wire_enum(error_category, not_found,       2).
wire_enum(error_category, conflict,        3).
wire_enum(error_category, unavailable,     4).
wire_enum(error_category, unauthorized,    5).
wire_enum(error_category, internal,        6).

wire_type(echo_stream, enum).
wire_enum(echo_stream, stdout, 1).
wire_enum(echo_stream, stderr, 2).

wire_type(edge_phase, enum).
wire_enum(edge_phase, rebuilt, 1).
wire_enum(edge_phase, started, 2).
wire_enum(edge_phase, done,    3).

% Registration names are a real sum, not two optional fields whose combination
% has to be guessed. `host` may contain a dotted parent path; `nested` is a
% single relative segment and requires the pidfd-derived enclosing box.
wire_type(registration_name, choice).
wire_variant(registration_name, automatic, 1, []).
wire_variant(registration_name, host,      2,
             [field(selector, box_selector)]).
wire_variant(registration_name, nested,    3,
             [field(name, text(short_bytes))]).

% A budget target is likewise explicit. The broker case consumes the
% authenticated box identity attached to that connection; selector performs a
% normal relational box lookup.
wire_type(box_target, choice).
wire_variant(box_target, broker,   1, []).
wire_variant(box_target, selector, 2,
             [field(box, box_selector)]).

wire_type(process_provenance, record([
    field(tgid, u32),
    field(ppid, s32),
    field(executable, path),
    field(cwd, path),
    field(argv, list(os_string, 1, command_items)),
    field(environment, option(environment))
])).

wire_type(oci_runtime, record([
    field(environment, option(list(os_string, environment_entries))),
    field(cwd, option(path)),
    field(command, option(list(os_string, command_items))),
    field(entrypoint, option(list(os_string, command_items))),
    field(user, option(os_string))
])).

wire_type(sud_runtime, record([
    field(upper, path),
    field(lowers, list(path, collection_items)),
    field(inramfs_key, text(short_bytes))
])).

wire_type(register_reply, record([
    field(mount, path),
    field(shared_memory, path),
    field(dns, option(ipv4)),
    field(ca_bundle, option(bytes(blob_bytes))),
    field(owner, owner_token),
    field(box, box_id),
    field(name, text(short_bytes)),
    field(capture, bool),
    field(api, bool),
    field(no_host, bool),
    field(oci, option(oci_runtime)),
    field(sud, option(sud_runtime))
])).

wire_type(pipeline_stage, choice).
wire_variant(pipeline_stage, simple, 1, [
    field(words, list(os_string, stage_items)),
    field(redirects, u32)
]).
wire_variant(pipeline_stage, compound, 2, [
    field(redirects, u32),
    field(text, text(text_bytes))
]).
wire_variant(pipeline_stage, function, 3, [
    field(text, text(text_bytes))
]).
wire_variant(pipeline_stage, extended_test, 4, [
    field(text, text(text_bytes))
]).

wire_type(pipeline_provenance, record([
    field(command, text(text_bytes)),
    field(negated, bool),
    field(stages, list(pipeline_stage, 1, stage_items)),
    field(output_targets, list(path, collection_items)),
    field(uid, pipeline_id),
    field(parent_uid, pipeline_id),
    field(sequence, u64),
    field(spawned_at, timestamp),
    field(nested, bool),
    field(edge_output, option(path))
])).

wire_type(build_edge, record([
    field(outputs, list(path, 1, collection_items)),
    field(inputs, list(path, collection_items)),
    field(command, option(text(text_bytes)))
])).

wire_type(make_variable, record([
    field(name, os_string),
    field(location, os_string),
    field(value, os_string),
    field(make_directory, path),
    field(rhs, os_string),
    field(references, os_string),
    field(edge_output, option(path)),
    field(pipeline, pipeline_id),
    field(flags, text(short_bytes))
])).

wire_type(activity_item, record([
    field(description, text(text_bytes)),
    field(age_seconds, u64)
])).

% Start and done have different required fields; this prevents a decoder from
% accepting `done` without an exit code or silently ignoring a code on `start`.
wire_type(build_edge_transition, choice).
wire_variant(build_edge_transition, start, 1, [
    field(at, timestamp),
    field(output, option(path)),
    field(command, option(text(text_bytes)))
]).
wire_variant(build_edge_transition, done, 2, [
    field(at, timestamp),
    field(output, option(path)),
    field(command, option(text(text_bytes))),
    field(code, exit_code),
    field(excerpt, option(text(text_bytes)))
]).

% Request identities 1..131 belong to action_catalog:wire_handler/2. The
% transport-only namespace starts at 256 so the first request atom dispatches
% directly without another family tag.
% wire_request(Name, Code, Success, PositionalFields, FdSchema, Authority).
wire_request(subscribe,             256, mode(subscribe), [], [], public).
wire_request(register,              257, mode(box), [
    field(command, list(os_string, 1, command_items)),
    field(provenance, process_provenance),
    field(name, registration_name),
    field(backend, run_backend),
    field(net_mode, net_mode),
    field(capture, bool),
    field(direct, bool),
    field(capture_environment, bool),
    field(brush, bool),
    field(api, bool),
    field(web_capture, bool),
    field(web_filter, bool),
    field(replay_from, option(box_id)),
    field(no_parent, bool),
    field(readonly_parent, bool)
], [
    fd(pidfd, required, always),
    fd(tap, required, when(net_mode, tap)),
    fd(sud_trace, required, when(backend, sud))
], pidfd_identity).
wire_request(brush_provenance,      258, reply(recorded), [
    field(records, list(pipeline_provenance, 1, collection_items))
], [fd(pidfd, required, always)], pidfd_box).
wire_request(brush_done,            259, reply(empty), [
    field(pipelines, list(pipeline_id, 1, collection_items)),
    field(code, exit_code),
    field(done_at, timestamp)
], [fd(pidfd, required, always)], pidfd_box).
wire_request(recipe_started,        260, reply(empty), [
    field(pipelines, list(pipeline_id, 1, collection_items)),
    field(started_at, timestamp)
], [fd(pidfd, required, always)], pidfd_box).
wire_request(build_graph,           261, reply(recorded), [
    field(edges, list(build_edge, 1, collection_items))
], [fd(pidfd, required, always)], pidfd_box).
wire_request(make_variables,        262, reply(empty), [
    field(rows, list(make_variable, 1, collection_items))
], [fd(pidfd, required, always)], pidfd_box).
wire_request(box_activity,          263, reply(empty), [
    field(items, list(activity_item, 1, collection_items))
], [fd(pidfd, required, always)], pidfd_box).
wire_request(build_edge_state,      264, reply(empty), [
    field(transition, build_edge_transition)
], [fd(pidfd, required, always)], pidfd_box).
wire_request(pty_spawn,             265, mode(pty), [
    field(argv, list(os_string, 1, command_items)),
    field(rows, u16),
    field(columns, u16),
    field(cwd, option(path)),
    field(environment, environment)
], [], public).
wire_request(api_proxy,             266, mode(raw_http), [], [], broker_box).
wire_request(service_declare,       267, reply(empty), [
    field(name, service_name),
    field(argv, list(os_string, 1, command_items)),
    field(net_mode, option(net_mode))
], [], broker_box).
wire_request(service_accept,        268, mode(service_accept), [
    field(name, service_name)
], [], broker_box).
wire_request(service_dial,          269, mode(raw_service), [
    field(name, service_name)
], [], public).
wire_request(budget_grant,          270, reply(budget), [
    field(target, box_target),
    field(amount, s64)
], [], target_box(target)).
wire_request(sud_ingest,            271, reply(sud_ingested), [
    field(box, box_selector)
], [], public).

% Reply-mode payload identities. An error always selects reply mode; a request
% never falls through to another success mode after an error.
wire_response(empty,        1, []).
wire_response(error,        2, [
    field(category, error_category),
    field(message, text(text_bytes))
]).
wire_response(recorded,     3, [field(count, u64)]).
wire_response(sud_ingested, 4, [
    field(count, u64),
    field(errors, list(text(text_bytes), error_items))
]).
wire_response(budget,       5, [field(remaining, s64)]).

% The first engine compound always selects one of these modes. Box mode carries
% the register result. Service-accept remains atom-framed until `paired`, then
% deliberately hands the remaining bytes to the raw service.
wire_mode(reply,          1, [field(response, response)]).
wire_mode(subscribe,      2, []).
wire_mode(box,            3, [field(registration, register_reply)]).
wire_mode(pty,            4, []).
wire_mode(raw_http,       5, []).
wire_mode(service_accept, 6, []).
wire_mode(raw_service,    7, []).

% Subscription events are compact invalidations. Durable provenance, packet,
% output, and trace records are not copied into the event stream; viewers fetch
% and relationally decode the visible rows after receiving an identity/count.
wire_event(box_added,                1, [
    field(box, box_id),
    field(name, option(text(short_bytes))),
    field(parent, option(box_id))
]).
wire_event(box_removed,              2, [field(box, box_id)]).
wire_event(box_renamed,              3, [
    field(box, box_id),
    field(name, text(short_bytes))
]).
wire_event(overlay_changed,          4, [
    field(box, box_id),
    field(count, u32),
    field(latest_path, option(path))
]).
wire_event(process_added,            5, [
    field(box, box_id),
    field(count, u32)
]).
wire_event(brush_provenance_added,   6, [
    field(box, box_id),
    field(row, row_id)
]).
wire_event(build_graph_changed,      7, [
    field(box, box_id),
    field(phase, edge_phase)
]).
wire_event(api_log_added,            8, [field(box, box_id)]).
wire_event(web_capture_added,        9, [field(box, box_id)]).
wire_event(pong,                    10, []).

% wire_frame(Stream, Name, Code, Direction, Fields, Fds, Transition).
% Codes preserve the already-deployed tv-atom mux identities. Retired 10..12
% remain unused rather than being silently recycled.
wire_frame(box, echo,             2, engine_to_runner, [
    field(stream, echo_stream),
    field(data, bytes(stream_chunk_bytes))
], [], stay).
wire_frame(box, echo_done,        3, engine_to_runner, [], [], stay).
wire_frame(box, mute,             4, runner_to_engine, [], [
    fd(pidfd, required, always)
], stay).
wire_frame(box, unmute,           5, runner_to_engine, [], [], stay).
wire_frame(box, provenance,       6, runner_to_engine, [
    field(record, pipeline_provenance)
], [], stay).
wire_frame(box, open_connection, 13, runner_to_engine, [], [], stay).
wire_frame(box, connection,      14, engine_to_runner, [], [
    fd(connected_socket, required, always)
], stay).

wire_frame(pty, data,             7, bidirectional, [
    field(data, bytes(stream_chunk_bytes))
], [], stay).
wire_frame(pty, resize,           8, client_to_engine, [
    field(rows, u16),
    field(columns, u16)
], [], stay).
wire_frame(pty, eof,              9, engine_to_client, [], [], close).

wire_frame(service_accept, paired, 1, engine_to_service, [], [],
           handoff(raw_service)).

% --- Catalog validation ---------------------------------------------------

valid_transport_catalog :-
    findall(Version, wire_protocol_version(Version), [Version]),
    integer(Version), Version > 0,
    wire_response(error, _, _),
    all_limits_valid,
    all_types_valid,
    all_names_and_codes_unique,
    all_requests_valid,
    all_responses_valid,
    all_modes_valid,
    all_events_valid,
    all_frames_valid.

all_limits_valid :-
    findall(Name-Value, wire_limit(Name, Value), Rows),
    Rows \= [],
    pair_keys_values(Rows, Names, _),
    all_unique(Names),
    all_positive_limits(Rows).

all_positive_limits([]).
all_positive_limits([Name-Value|Rows]) :-
    atom(Name), integer(Value), Value > 0,
    all_positive_limits(Rows).

all_types_valid :-
    findall(Name, wire_type(Name, _), Names),
    all_unique(Names),
    all_type_names_valid(Names),
    findall(Type-Code, wire_enum(Type, _, Code), EnumCodes),
    unique_group_codes(EnumCodes),
    findall(Type-Case, wire_enum(Type, Case, _), EnumCases),
    unique_pairs(EnumCases),
    findall(Type-Code, wire_variant(Type, _, Code, _), VariantCodes),
    unique_group_codes(VariantCodes),
    findall(Type-Case, wire_variant(Type, Case, _, _), VariantCases),
    unique_pairs(VariantCases),
    all_enum_rows_valid,
    all_variant_rows_valid.

all_type_names_valid([]).
all_type_names_valid([Name|Names]) :-
    once(valid_named_type(Name, [])),
    all_type_names_valid(Names).

valid_named_type(Name, Seen) :-
    member_eq(Name, Seen), !.
valid_named_type(Name, Seen) :-
    wire_type(Name, Definition),
    valid_type_definition(Name, Definition, [Name|Seen]).

valid_type_definition(_, alias(Type), Seen) :- valid_type(Type, Seen).
valid_type_definition(_, record(Fields), Seen) :- valid_fields(Fields, Seen).
valid_type_definition(Name, enum, _) :- wire_enum(Name, _, _).
valid_type_definition(Name, choice, Seen) :-
    wire_variant(Name, _, _, _),
    all_variants_of(Name, Seen).

all_variants_of(Type, Seen) :-
    findall(Fields, wire_variant(Type, _, _, Fields), FieldLists),
    all_field_lists_valid(FieldLists, Seen).

all_field_lists_valid([], _).
all_field_lists_valid([Fields|FieldLists], Seen) :-
    valid_fields(Fields, Seen),
    all_field_lists_valid(FieldLists, Seen).

valid_type(bool, _).
valid_type(u16, _).
valid_type(u32, _).
valid_type(u64, _).
valid_type(s32, _).
valid_type(s64, _).
valid_type(f64, _).
valid_type(response, _).
valid_type(text(Limit), _) :- wire_limit(Limit, _).
valid_type(bytes(Limit), _) :- wire_limit(Limit, _).
valid_type(fixed_bytes(Length), _) :- integer(Length), Length > 0.
valid_type(option(Type), Seen) :- valid_type(Type, Seen).
valid_type(list(Type, Limit), Seen) :-
    wire_limit(Limit, _), valid_type(Type, Seen).
valid_type(list(Type, Minimum, Limit), Seen) :-
    integer(Minimum), Minimum > 0,
    wire_limit(Limit, Maximum), Minimum =< Maximum,
    valid_type(Type, Seen).
valid_type(map(Key, Value, Limit), Seen) :-
    wire_limit(Limit, _), valid_type(Key, Seen), valid_type(Value, Seen).
valid_type(Name, Seen) :- atom(Name), valid_named_type(Name, Seen).

valid_fields(Fields, Seen) :-
    field_names(Fields, Names),
    all_unique(Names),
    all_fields_valid(Fields, Seen).

field_names([], []).
field_names([field(Name, _)|Fields], [Name|Names]) :-
    field_names(Fields, Names).

all_fields_valid([], _).
all_fields_valid([field(Name, Type)|Fields], Seen) :-
    atom(Name), valid_type(Type, Seen),
    all_fields_valid(Fields, Seen).

all_enum_rows_valid :-
    \+ (wire_enum(Type, Case, Code),
        ( \+ wire_type(Type, enum)
        ; \+ atom(Case)
        ; \+ (integer(Code), Code > 0)
        )).

all_variant_rows_valid :-
    \+ (wire_variant(Type, Case, Code, Fields),
        ( \+ wire_type(Type, choice)
        ; \+ atom(Case)
        ; \+ (integer(Code), Code > 0)
        ; \+ valid_fields(Fields, [Type])
        )).

all_names_and_codes_unique :-
    findall(Name-Code, wire_request(Name, Code, _, _, _, _), Requests),
    unique_keys_and_values(Requests),
    findall(Name-Code, wire_response(Name, Code, _), Responses),
    unique_keys_and_values(Responses),
    findall(Name-Code, wire_mode(Name, Code, _), Modes),
    unique_keys_and_values(Modes),
    findall(Name-Code, wire_event(Name, Code, _), Events),
    unique_keys_and_values(Events),
    findall(Stream-Name-Code,
            wire_frame(Stream, Name, Code, _, _, _, _), Frames),
    unique_frame_names_and_codes(Frames).

all_requests_valid :-
    \+ (wire_request(Name, Code, Success, Fields, Fds, Authority),
        ( \+ atom(Name)
        ; \+ (integer(Code), Code >= 256)
        ; \+ valid_success(Success)
        ; \+ valid_fields(Fields, [])
        ; \+ valid_fds(Fds, Fields)
        ; \+ valid_authority(Authority, Fields)
        )).

valid_success(reply(Name)) :- wire_response(Name, _, _).
valid_success(mode(Name)) :- wire_mode(Name, _, _), Name \= reply.

valid_authority(public, _).
valid_authority(broker_box, _).
valid_authority(pidfd_identity, _).
valid_authority(pidfd_box, _).
valid_authority(target_box(Field), Fields) :-
    member_field_type(Field, box_target, Fields).

all_responses_valid :-
    \+ (wire_response(Name, Code, Fields),
        ( \+ atom(Name)
        ; \+ (integer(Code), Code > 0)
        ; \+ valid_fields(Fields, [])
        )).

all_modes_valid :-
    \+ (wire_mode(Name, Code, Fields),
        ( \+ atom(Name)
        ; \+ (integer(Code), Code > 0)
        ; \+ valid_fields(Fields, [])
        )).

all_events_valid :-
    \+ (wire_event(Name, Code, Fields),
        ( \+ atom(Name)
        ; \+ (integer(Code), Code > 0)
        ; \+ valid_fields(Fields, [])
        )).

all_frames_valid :-
    \+ (wire_frame(Stream, Name, Code, Direction, Fields, Fds, Transition),
        ( \+ wire_mode(Stream, _, _)
        ; \+ atom(Name)
        ; \+ (integer(Code), Code > 0)
        ; \+ valid_direction(Direction)
        ; \+ valid_fields(Fields, [])
        ; \+ valid_fds(Fds, Fields)
        ; \+ valid_transition(Transition)
        )).

valid_direction(engine_to_runner).
valid_direction(runner_to_engine).
valid_direction(bidirectional).
valid_direction(client_to_engine).
valid_direction(engine_to_client).
valid_direction(engine_to_service).

valid_transition(stay).
valid_transition(close).
valid_transition(handoff(Mode)) :- wire_mode(Mode, _, _).

valid_fds(Fds, Fields) :-
    fd_names(Fds, Names),
    all_unique(Names),
    all_fds_valid(Fds, Fields).

fd_names([], []).
fd_names([fd(Name, _, _)|Fds], [Name|Names]) :- fd_names(Fds, Names).

all_fds_valid([], _).
all_fds_valid([fd(Name, required, Condition)|Fds], Fields) :-
    atom(Name), valid_fd_condition(Condition, Fields),
    all_fds_valid(Fds, Fields).

valid_fd_condition(always, _).
valid_fd_condition(when(Field, Value), Fields) :-
    member_field_type(Field, Type, Fields),
    valid_condition_value(Type, Value).

valid_condition_value(bool, true).
valid_condition_value(bool, false).
valid_condition_value(Type, Value) :- wire_enum(Type, Value, _).

member_field(Name, [field(Name0, _)|_]) :- Name == Name0, !.
member_field(Name, [_|Fields]) :- member_field(Name, Fields).

member_field_type(Name, Type, [field(Name0, Type0)|_]) :-
    Name == Name0, Type = Type0, !.
member_field_type(Name, Type, [_|Fields]) :-
    member_field_type(Name, Type, Fields).

member_eq(Value, [Head|_]) :- Value == Head, !.
member_eq(Value, [_|Values]) :- member_eq(Value, Values).

all_unique([]).
all_unique([Value|Values]) :-
    \+ member_eq(Value, Values),
    all_unique(Values).

unique_pairs(Pairs) :- all_unique(Pairs).

unique_keys_and_values(Pairs) :-
    pair_keys_values(Pairs, Keys, Values),
    all_unique(Keys), all_unique(Values).

pair_keys_values([], [], []).
pair_keys_values([Key-Value|Pairs], [Key|Keys], [Value|Values]) :-
    pair_keys_values(Pairs, Keys, Values).

unique_group_codes([]).
unique_group_codes([Type-Code|Rows]) :-
    \+ member_eq(Type-Code, Rows),
    unique_group_codes(Rows).

unique_frame_names_and_codes([]).
unique_frame_names_and_codes([Stream-Name-Code|Frames]) :-
    \+ frame_has_name(Stream, Name, Frames),
    \+ frame_has_code(Stream, Code, Frames),
    unique_frame_names_and_codes(Frames).

frame_has_name(Stream, Name, [S-N-_|_]) :- Stream == S, Name == N, !.
frame_has_name(Stream, Name, [_|Frames]) :- frame_has_name(Stream, Name, Frames).

frame_has_code(Stream, Code, [S-_-C|_]) :- Stream == S, Code == C, !.
frame_has_code(Stream, Code, [_|Frames]) :- frame_has_code(Stream, Code, Frames).
