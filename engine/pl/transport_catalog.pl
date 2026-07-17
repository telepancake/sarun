:- module(transport_catalog,
          [ wire_protocol_version/1,
            wire_limit/2,
            wire_type/2,
            wire_enum/3,
            wire_variant/4,
            wire_request/6,
            wire_response/3,
            wire_mode/3,
            wire_stream/1,
            wire_event/3,
            wire_frame/7,
            valid_wire_type/1,
            valid_wire_fields/1,
            valid_transport_catalog/0
          ]).

:- discontiguous wire_type/2.
:- discontiguous wire_enum/3.
:- discontiguous wire_variant/4.

/** <module> Normalized direct-Rust transport relation

This is the semantic definition of sarun's non-action binary transport.  It is
not a serializer implementation and it is not a second command catalog.
Action requests use `action_catalog:wire_handler/3`; the facts below cover only
connection/lifecycle messages, replies, events, stream modes, mux frames, and
SCM_RIGHTS roles.

Every compound is positional.  Field names are relation metadata used for
conversion, generation, help, and inspection; field-name strings are never
sent.  Every variable-length leaf and collection names an explicit bound.
Numeric identities are facts, never declaration-order indexes.

The type algebra is deliberately small:

  * scalar atoms: `bool`, `u16`, `u32`, `u64`, `s32`, `s64`, `f64`;
  * the generated `action_success` sum used only by the reply envelope;
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
wire_enum(run_backend, qemu, 3).

wire_type(qemu_architecture, enum).
wire_enum(qemu_architecture, aarch64, 1).
wire_enum(qemu_architecture, x86_64, 2).

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
    field(virtiofs_socket, option(path))
])).

% Host runner <-> target /init over the named virtio-serial port.  This is a
% compact point-to-point lifecycle relation, not another command-line or JSON
% representation.
wire_type(appliance_command, record([
    field(command, list(os_string, 1, command_items)),
    field(cwd, option(path)),
    field(environment, environment),
    field(net_mode, net_mode)
])).
% A nested `run --qemu` does not start QEMU in the guest.  The guest sends this
% semantic request to its still-live host runner, which launches a second flat
% appliance through an engine connection authenticated by the outer box
% channel.  Environment is explicit because the nested caller's guest process
% context, not the host runner's environment, is inherited by the child.
wire_type(appliance_run_request, record([
    field(architecture, qemu_architecture),
    field(name, option(text(short_bytes))),
    field(capture_environment, bool),
    field(no_parent, bool),
    field(readonly_parent, bool),
    field(cwd, option(path)),
    field(net_mode, net_mode),
    field(brush, bool),
    field(command, list(os_string, 1, command_items)),
    field(environment, environment)
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

% Action result vocabulary.  These are semantic wire records, not a binary
% spelling of serde_json::Value.  Action handlers are related to them in the
% action catalog, so result shape is known before Rust code is generated.
wire_type(unit,       record([])).
wire_type(job_id,     alias(u64)).
wire_type(view_id,    alias(u64)).
wire_type(process_id, alias(u32)).
wire_type(file_mode,  alias(u32)).
wire_type(nanoseconds, alias(u64)).

wire_type(change_kind, enum).
wire_enum(change_kind, changed,    1).
wire_enum(change_kind, deleted,    2).
wire_enum(change_kind, symlink,    3).
wire_enum(change_kind, created,    4).
wire_enum(change_kind, modified,   5).
wire_enum(change_kind, xattr,      6).
wire_enum(change_kind, directory,  7).
wire_enum(change_kind, xattr_only, 8).

wire_type(path_kind, enum).
wire_enum(path_kind, missing,   1).
wire_enum(path_kind, file,      2).
wire_enum(path_kind, directory, 3).
wire_enum(path_kind, symlink,   4).
wire_enum(path_kind, special,   5).

wire_type(session_status, enum).
wire_enum(session_status, running,  1).
wire_enum(session_status, finished, 2).
wire_enum(session_status, failed,   3).
wire_enum(session_status, killed,   4).

wire_type(mirror_state, enum).
wire_enum(mirror_state, running,   1).
wire_enum(mirror_state, paused,    2).
wire_enum(mirror_state, pending,   3).
wire_enum(mirror_state, stopped,   4).
wire_enum(mirror_state, error,     5).
wire_enum(mirror_state, completed, 6).
wire_enum(mirror_state, scheduled, 7).

wire_type(oaita_status_kind, enum).
wire_enum(oaita_status_kind, none,     1).
wire_enum(oaita_status_kind, external, 2).
wire_enum(oaita_status_kind, local,    3).

wire_type(prompt_verdict, enum).
wire_enum(prompt_verdict, yes_once,   1).
wire_enum(prompt_verdict, no_once,    2).
wire_enum(prompt_verdict, allow_save, 3).
wire_enum(prompt_verdict, deny_save,  4).

wire_type(provenance_domain, enum).
wire_enum(provenance_domain, process,  1).
wire_enum(provenance_domain, pipeline, 2).
wire_enum(provenance_domain, edge,     3).

wire_type(view_kind, enum).
wire_enum(view_kind, changes,    1).
wire_enum(view_kind, processes,  2).
wire_enum(view_kind, outputs,    3).
wire_enum(view_kind, pipelines,  4).
wire_enum(view_kind, build_edges, 5).

wire_type(filter_join, enum).
wire_enum(filter_join, and, 1).
wire_enum(filter_join, or,  2).

wire_type(filter_kind, enum).
wire_enum(filter_kind, path,   1).
wire_enum(filter_kind, box,    2).
wire_enum(filter_kind, exe,    3).
wire_enum(filter_kind, cwd,    4).
wire_enum(filter_kind, arg,    5).
wire_enum(filter_kind, ids,    6).
wire_enum(filter_kind, err,    7).
wire_enum(filter_kind, cmd,    8).
wire_enum(filter_kind, target,  9).

wire_type(filter_clause, record([
    field(kind, filter_kind),
    field(pattern, text(text_bytes)),
    field(join, filter_join),
    field(negated, bool),
    field(enabled, bool)
])).
wire_type(filter_spec, alias(list(filter_clause, collection_items))).

wire_type(external_reference, record([
    field(kind, text(short_bytes)),
    field(store, path),
    field(reference, text(text_bytes)),
    field(revision, text(short_bytes)),
    field(prefix, path),
    field(name, text(text_bytes))
])).
wire_type(readonly_attachment, choice).
wire_variant(readonly_attachment, box, 1, [field(box, box_id)]).
wire_variant(readonly_attachment, external, 2,
             [field(reference, external_reference)]).

wire_type(oci_build_spec, record([
    field(context_tar_gz, bytes(blob_bytes)),
    field(dockerfile, bytes(blob_bytes)),
    field(tag, option(text(text_bytes))),
    field(net_mode, net_mode),
    field(build_arguments,
          map(text(text_bytes), text(text_bytes), environment_entries))
])).

wire_type(api_probe_spec, record([
    field(base_url, text(text_bytes)),
    field(model, text(text_bytes)),
    field(api_key, text(text_bytes))
])).

wire_type(failure_kind, enum).
wire_enum(failure_kind, edge,     1).
wire_enum(failure_kind, pipeline, 2).

wire_type(flow_row, record([
    field(frame, u64),
    field(time, timestamp),
    field(source, text(short_bytes)),
    field(destination, text(short_bytes)),
    field(sni, text(text_bytes)),
    field(host, text(text_bytes)),
    field(method, text(short_bytes)),
    field(uri, text(text_bytes)),
    field(status, text(short_bytes)),
    field(stream, option(u64))
])).

wire_type(packet_row, record([
    field(frame, u64),
    field(time, timestamp),
    field(source, text(short_bytes)),
    field(destination, text(short_bytes)),
    field(protocol, text(short_bytes)),
    field(length, u32),
    field(summary, text(text_bytes))
])).

wire_type(model_entry, record([
    field(name, text(text_bytes)),
    field(url, text(text_bytes)),
    field(note, text(text_bytes))
])).
wire_type(model_catalog, record([
    field(source, text(text_bytes)),
    field(models, list(model_entry, collection_items))
])).
wire_type(oaita_status, record([
    field(kind, oaita_status_kind),
    field(model, text(text_bytes)),
    field(endpoint, text(text_bytes)),
    field(serving, bool)
])).

wire_type(oci_build_result, record([
    field(code, exit_code),
    field(log, text(blob_bytes)),
    field(top, option(box_id))
])).
wire_type(oci_image, record([
    field(top, box_id),
    field(name, text(short_bytes)),
    field(reference, text(text_bytes)),
    field(digest, text(short_bytes))
])).
wire_type(oci_load_result, record([
    field(base, box_id),
    field(base_name, text(short_bytes)),
    field(top, box_id),
    field(top_name, text(short_bytes)),
    field(layer_count, u32),
    field(verified, bool)
])).
wire_type(oci_resolve_result, record([
    field(top, box_id),
    field(note, text(text_bytes))
])).

wire_type(network_prompt, record([
    field(id, u64),
    field(box, text(short_bytes)),
    field(host, text(text_bytes)),
    field(port, u16),
    field(scheme, text(short_bytes))
])).

wire_type(path_error, record([
    field(path, option(path)),
    field(message, text(text_bytes))
])).
wire_type(apply_result, record([
    field(applied, list(path, collection_items)),
    field(errors, list(path_error, collection_items))
])).
wire_type(discard_result, record([
    field(discarded, list(path, collection_items)),
    field(errors, list(path_error, collection_items))
])).
wire_type(action_mutation_result, record([
    field(box, box_id),
    field(count, u64),
    field(errors, list(path_error, collection_items))
])).

wire_type(change_row, record([
    field(path, path),
    field(kind, change_kind),
    field(size, u64)
])).
wire_type(change_decoration, record([
    field(kind, change_kind),
    field(is_text, bool),
    field(stale, bool)
])).
wire_type(file_group, record([
    field(name, text(short_bytes)),
    field(count, u64),
    field(paths, list(path, collection_items))
])).
wire_type(diff_line, record([
    field(style, text(short_bytes)),
    field(text, text(text_bytes))
])).
wire_type(diff_hunk, record([
    field(index, u32),
    field(lines, list(diff_line, collection_items))
])).
wire_type(file_diff, choice).
wire_variant(file_diff, text, 1, [
    field(hunks, list(diff_hunk, collection_items))
]).
wire_variant(file_diff, deleted, 2, []).
wire_variant(file_diff, symlink, 3, [
    field(kind, change_kind),
    field(target, bytes(path_bytes))
]).
wire_variant(file_diff, binary, 4, [
    field(kind, change_kind),
    field(content, bytes(blob_bytes)),
    field(content_before, option(bytes(blob_bytes)))
]).
wire_variant(file_diff, unavailable, 5, [
    field(message, text(text_bytes))
]).

wire_type(make_variable_row, record([
    field(id, row_id),
    field(name, os_string),
    field(location, os_string),
    field(value, os_string),
    field(make_directory, path),
    field(rhs, os_string),
    field(references, os_string),
    field(flags, text(short_bytes)),
    field(edge_output, option(path)),
    field(pipeline_uid, option(pipeline_id)),
    field(edge, option(row_id)),
    field(pipeline, option(row_id))
])).

wire_type(pipeline_context_item, record([
    field(id, row_id),
    field(command, text(text_bytes)),
    field(exit_code, option(exit_code))
])).
wire_type(pipeline_context, record([
    field(parent, option(pipeline_context_item)),
    field(children, list(pipeline_context_item, collection_items)),
    field(edge_output, option(path))
])).

wire_type(output_preview, record([
    field(id, row_id),
    field(time, timestamp),
    field(stream, echo_stream),
    field(length, u64),
    field(preview, text(text_bytes))
])).
wire_type(change_preview, record([
    field(path, path),
    field(kind, change_kind),
    field(size, u64),
    field(modified_at, s64),
    field(xattr_key, option(os_string)),
    field(xattr_length, option(u64))
])).
wire_type(process_preview, record([
    field(id, row_id),
    field(tgid, option(process_id)),
    field(executable, path),
    field(argv0, os_string)
])).
wire_type(pipeline_preview, record([
    field(id, row_id),
    field(command, text(text_bytes)),
    field(nested, bool)
])).
wire_type(edge_preview, record([
    field(id, row_id),
    field(output, option(path)),
    field(output_count, u32),
    field(command, option(text(text_bytes)))
])).
wire_type(failure_preview, record([
    field(kind, failure_kind),
    field(label, text(text_bytes)),
    field(code, exit_code),
    field(excerpt, text(text_bytes))
])).
wire_type(box_summary, record([
    field(outputs, list(output_preview, collection_items)),
    field(changes, list(change_preview, collection_items)),
    field(processes, list(process_preview, collection_items)),
    field(pipelines, list(pipeline_preview, collection_items)),
    field(edges, list(edge_preview, collection_items)),
    field(failures, list(failure_preview, collection_items)),
    field(has_make_variables, bool),
    field(has_sud_trace, bool),
    field(activity, list(activity_item, collection_items))
])).

wire_type(view_open_result, record([
    field(view, view_id),
    field(total, u64)
])).
wire_type(view_filter_result, record([field(total, u64)])).
wire_type(view_find_result, alias(option(u64))).

wire_type(api_log_row, record([
    field(id, row_id),
    field(time, timestamp),
    field(method, text(short_bytes)),
    field(path, text(text_bytes)),
    field(model, text(text_bytes)),
    field(status, u16),
    field(streaming, bool),
    field(request_length, u64),
    field(response_length, u64)
])).
wire_type(api_log_detail, record([
    field(summary, api_log_row),
    field(request, bytes(blob_bytes)),
    field(response, bytes(blob_bytes))
])).

wire_type(apply_copy_result, record([
    field(box, box_id),
    field(name, text(short_bytes)),
    field(applied, u64)
])).
wire_type(directory_entry, record([
    field(name, os_string),
    field(kind, path_kind)
])).
wire_type(box_created, record([
    field(box, box_id),
    field(root, path)
])).

wire_type(pipeline_row, record([
    field(id, row_id),
    field(time, timestamp),
    field(command, text(text_bytes)),
    field(record, option(pipeline_provenance)),
    field(pipeline, option(pipeline_id)),
    field(spawned_at, option(timestamp)),
    field(done_at, option(timestamp)),
    field(nested, bool),
    field(uid, option(pipeline_id)),
    field(parent_uid, option(pipeline_id)),
    field(exit_code, option(exit_code)),
    field(processes, list(row_id, collection_items))
])).
wire_type(pipeline_summary, record([
    field(id, row_id),
    field(time, timestamp),
    field(command, text(text_bytes)),
    field(record, option(pipeline_provenance)),
    field(pipeline, option(pipeline_id))
])).
wire_type(build_edge_row, record([
    field(id, row_id),
    field(time, timestamp),
    field(outputs, list(path, 1, collection_items)),
    field(inputs, list(path, collection_items)),
    field(command, option(text(text_bytes))),
    field(started_at, option(timestamp)),
    field(ended_at, option(timestamp)),
    field(exit_code, option(exit_code)),
    field(output_excerpt, option(text(text_bytes)))
])).
wire_type(free_result, record([
    field(reparented, list(box_id, collection_items))
])).

wire_type(process_row, record([
    field(id, row_id),
    field(tgid, option(process_id)),
    field(ppid, option(process_id)),
    field(parent, option(row_id)),
    field(executable, path),
    field(argv, list(os_string, command_items)),
    field(pipeline, option(row_id))
])).
wire_type(process_info, record([
    field(tgid, option(process_id)),
    field(ppid, option(process_id)),
    field(parent, option(row_id)),
    field(executable, path),
    field(argv, list(os_string, command_items))
])).
wire_type(process_subject, record([
    field(executable, path),
    field(cwd, path),
    field(argv, list(os_string, command_items))
])).
wire_type(writer_provenance, record([
    field(pid, option(process_id)),
    field(ppid, option(process_id)),
    field(executable, path),
    field(cwd, path),
    field(argv, list(os_string, command_items))
])).
wire_type(output_row, record([
    field(id, row_id),
    field(time, timestamp),
    field(process, option(row_id)),
    field(stream, echo_stream),
    field(length, u64)
])).
wire_type(output_detail, record([
    field(summary, output_row),
    field(content, bytes(blob_bytes))
])).

wire_type(checkout_result, record([
    field(revision, text(short_bytes)),
    field(files, u64),
    field(bytes, u64)
])).
wire_type(ietf_attachment_result, record([
    field(name, text(text_bytes)),
    field(revision, text(short_bytes))
])).
wire_type(wiki_attachment_result, record([
    field(name, text(text_bytes)),
    field(page, u64),
    field(title, text(text_bytes)),
    field(revision, u64)
])).
wire_type(mirror_job, record([
    field(id, job_id),
    field(kind, text(short_bytes)),
    field(source, text(text_bytes)),
    field(destination, path),
    field(interval_seconds, u64),
    field(paused, bool),
    field(last_start, option(s64)),
    field(last_end, option(s64)),
    field(last_exit, option(exit_code)),
    field(last_detail, text(text_bytes)),
    field(state, mirror_state),
    field(next_due, option(s64))
])).
wire_type(rotate_result, record([
    field(parent, box_id),
    field(child, box_id)
])).

wire_type(external_attachment, record([
    field(name, text(text_bytes)),
    field(kind, text(short_bytes)),
    field(revision, text(short_bytes)),
    field(error, option(text(text_bytes)))
])).
wire_type(box_session, record([
    field(box, box_id),
    field(name, text(short_bytes)),
    field(command, list(os_string, command_items)),
    field(shared_memory, path),
    field(live, bool),
    field(has_archive, bool),
    field(exit_code, option(exit_code)),
    field(run_pid, option(process_id)),
    field(parent, option(box_id)),
    field(parents, list(box_id, collection_items)),
    field(attachments, list(external_attachment, collection_items)),
    field(started_at, timestamp),
    field(status, session_status),
    field(upper, path),
    field(display_path, text(text_bytes))
])).

wire_type(structural_line, record([
    field(style, text(short_bytes)),
    field(text, text(text_bytes))
])).
wire_type(structural_diff, record([
    field(lines, list(structural_line, collection_items))
])).
wire_type(structural_quick, record([
    field(lines, list(structural_line, collection_items)),
    field(job, option(job_id))
])).

wire_type(stuck_thread, record([
    field(pid, process_id),
    field(tid, process_id),
    field(command, text(short_bytes)),
    field(state, text(short_bytes)),
    field(wait_channel, text(text_bytes)),
    field(syscall, text(short_bytes)),
    field(detail, text(text_bytes)),
    field(backtrace, list(text(text_bytes), collection_items))
])).
wire_type(stuck_report, record([
    field(runner, process_id),
    field(threads, list(stuck_thread, collection_items))
])).

wire_type(action_help_row, record([
    field(verb, text(short_bytes)),
    field(arguments, text(text_bytes)),
    field(description, text(text_bytes))
])).

wire_type(web_capture_row, record([
    field(id, row_id),
    field(time, timestamp),
    field(method, text(short_bytes)),
    field(url, text(text_bytes)),
    field(host, text(text_bytes)),
    field(status, u16),
    field(mime, text(text_bytes)),
    field(truncated, bool),
    field(request_length, u64),
    field(response_length, u64)
])).
wire_type(web_capture_detail, record([
    field(summary, web_capture_row),
    field(request_headers, bytes(blob_bytes)),
    field(response_headers, bytes(blob_bytes)),
    field(request_body, bytes(blob_bytes)),
    field(response_body, bytes(blob_bytes))
])).
wire_type(web_capture_body, record([
    field(mime, text(text_bytes)),
    field(body, bytes(blob_bytes))
])).

% TRACE is independently versioned and may acquire event kinds before this
% viewer does. Preserve an unknown numeric kind explicitly instead of making
% the typed display relation unable to represent bytes the recorder retained.
wire_type(sud_event_kind, choice).
wire_variant(sud_event_kind, exec,    1, []).
wire_variant(sud_event_kind, argv,    2, []).
wire_variant(sud_event_kind, env,     3, []).
wire_variant(sud_event_kind, open,    4, []).
wire_variant(sud_event_kind, cwd,     5, []).
wire_variant(sud_event_kind, stdout,  6, []).
wire_variant(sud_event_kind, stderr,  7, []).
wire_variant(sud_event_kind, exit,    8, []).
wire_variant(sud_event_kind, prof,    9, []).
wire_variant(sud_event_kind, unknown, 10, [field(code, s64)]).
wire_type(sud_event, record([
    field(time_ns, nanoseconds),
    field(kind, sud_event_kind),
    field(pid, process_id),
    field(tgid, process_id),
    field(ppid, process_id),
    field(extras, list(s64, collection_items)),
    field(text, text(text_bytes))
])).
wire_type(sud_trace_view, record([
    field(events, list(sud_event, collection_items)),
    field(truncated, bool)
])).

wire_type(rename_result, record([
    field(old_display_path, text(text_bytes)),
    field(name, text(short_bytes))
])).

wire_type(view_change_row, record([
    field(path, path),
    field(name, os_string),
    field(kind, change_kind),
    field(size, u64),
    field(depth, u32),
    field(connector, bool),
    field(xattr_for, option(path)),
    field(xattr_key, option(os_string))
])).
wire_type(view_process_row, record([
    field(id, row_id),
    field(tgid, option(process_id)),
    field(ppid, option(process_id)),
    field(executable, path),
    field(argv, list(os_string, command_items)),
    field(depth, u32),
    field(connector, bool)
])).
wire_type(view_output_row, record([
    field(output, output_row),
    field(executable, path),
    field(tgid, option(process_id))
])).
wire_type(view_window, choice).
wire_variant(view_window, changes, 1, [
    field(start, u64), field(total, u64),
    field(rows, list(view_change_row, collection_items))
]).
wire_variant(view_window, processes, 2, [
    field(start, u64), field(total, u64),
    field(rows, list(view_process_row, collection_items))
]).
wire_variant(view_window, outputs, 3, [
    field(start, u64), field(total, u64),
    field(rows, list(view_output_row, collection_items))
]).
wire_variant(view_window, pipelines, 4, [
    field(start, u64), field(total, u64),
    field(rows, list(pipeline_row, collection_items))
]).
wire_variant(view_window, build_edges, 5, [
    field(start, u64), field(total, u64),
    field(rows, list(build_edge_row, collection_items))
]).

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

% Request identities 1..131 belong to action_catalog:wire_handler/3. The
% transport-only namespace starts at 256 so the first request atom dispatches
% directly without another family tag.
% wire_request(Name, Code, Success, PositionalFields, FdSchema, Authority).
wire_request(subscribe,             256, mode(subscribe), [], [], public).
wire_request(register,              257, mode(box), [
    field(command, list(os_string, 1, command_items)),
    field(provenance, process_provenance),
    field(name, registration_name),
    field(backend, run_backend),
    field(architecture, option(qemu_architecture)),
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
% Reply-mode payload identities. An error always selects reply mode; a request
% never falls through to another success mode after an error.
wire_response(empty,        1, []).
wire_response(error,        2, [
    field(category, error_category),
    field(message, text(text_bytes))
]).
wire_response(recorded,     3, [field(count, u64)]).
wire_response(budget,       5, [field(remaining, s64)]).
wire_response(action,       6, [field(value, action_success)]).

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

% Frame streams are independent of first-compound connection modes.  Most
% engine streams share a name with their mode; `appliance` is the generated
% host-runner <-> guest-PID1 operation stream and is not an engine mode.
wire_stream(box).
wire_stream(pty).
wire_stream(service_accept).
wire_stream(appliance).

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

% Flat nested-QEMU operation channel.  `stream` identifies one guest caller;
% it never names an engine socket or a host descriptor.  The host runner owns
% the real child connection, QEMU process, virtio-fs endpoint, and pidfds.
wire_frame(appliance, nested_open, 1, guest_to_host, [
    field(stream, u64),
    field(request, appliance_run_request)
], [], stay).
wire_frame(appliance, nested_input, 2, guest_to_host, [
    field(stream, u64),
    field(data, bytes(stream_chunk_bytes))
], [], stay).
wire_frame(appliance, nested_input_eof, 3, guest_to_host, [
    field(stream, u64)
], [], stay).
wire_frame(appliance, nested_signal, 4, guest_to_host, [
    field(stream, u64),
    field(signal, s32)
], [], stay).
wire_frame(appliance, nested_output, 5, host_to_guest, [
    field(stream, u64),
    field(data, bytes(stream_chunk_bytes))
], [], stay).
wire_frame(appliance, nested_result, 6, host_to_guest, [
    field(stream, u64),
    field(code, exit_code)
], [], stay).
wire_frame(appliance, result, 7, guest_to_host, [
    field(code, exit_code)
], [], close).

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

valid_wire_type(Type) :-
    valid_type(Type, []).

valid_wire_fields(Fields) :-
    valid_fields(Fields, []).

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
valid_type(action_success, _).
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
        ( \+ wire_stream(Stream)
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
valid_direction(guest_to_host).
valid_direction(host_to_guest).

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
