:- module(action_catalog,
          [ action/7,
            argument_schema/2,
            cli_form/3,
            key_binding/4,
            menu_label/2,
            argument_context/4,
            wire_handler/2,
            visible_action/1
          ]).

/** <module> The single semantic definition of sarun UI actions

`action/7` is data, not an execution registry. Rust owns handler bodies; this
relation owns what each handler means and every public representation of that
meaning. No Rust table may duplicate these schemas, descriptions, targets,
aliases, keys, menus, or forms.

  action(PublicIdentity, HandlerIdentity, Target, ArgumentNotation,
         Description, Visibility, Preference).

Argument notation is retained as a help representation and is relationally
decoded by `argument_schema/2`. A schema element is
`arg(Name, Kind, Cardinality, WireShape)`, where cardinality is `required`,
`optional`, or `repeated`, and wire shape is `scalar`, `array`, or `spread`.
*/

action(session_dicts, session_dicts, ui, "", "list every box with status metadata (live overridden for running boxes)", visible, 50).
action(display_path, display_path, ui, "SID", "human display path for a box", visible, 50).
action(resolve_box, resolve_box, ui, "NAME_OR_ID", "resolve a box name/id to its numeric id", visible, 50).
action(select, select, ui, "SID", "set the engine-side selected box", visible, 50).
action(processes, processes, ui, "SID", "captured process rows for a box", visible, 50).
action(outputs, outputs, ui, "SID", "decoded stdout/stderr transcript rows", visible, 50).
action(api_log, api_log, ui, "SID", "--api oaita proxy request log", visible, 50).
action(api_log_detail, api_log_detail, ui, "SID ROW", "full request/response detail of one api_log row", visible, 50).
action(webcap, webcap, ui, "SID", "web-capture summary rows (tap MITM archive)", visible, 50).
action(webcap_detail, webcap_detail, ui, "SID ROW", "full detail of one web capture", visible, 50).
action(webcap_body, webcap_body, ui, "SID ROW", "raw base64 response body of one capture", visible, 50).
action(brushprov, brushprov, ui, "SID", "brush semantic-provenance rows (pipelines)", visible, 50).
action(build_edges, build_edges, ui, "SID", "parsed ninja/make build-graph edges", visible, 50).
action(proc_pipeline, proc_pipeline, ui, "SID ROW", "the pipeline a process belongs to", visible, 50).
action(output_pipeline, output_pipeline, ui, "SID OUTPUT", "the pipeline an output row belongs to", visible, 50).
action(pipeline_procs, pipeline_procs, ui, "SID PIPELINE", "processes belonging to a pipeline", visible, 50).
action(output_detail, output_detail, ui, "SID OUTPUT", "one output row in full", visible, 50).
action(processes_live, processes_live, ui, "SID", "process snapshot when the box is live, else null", visible, 50).
action(proc_info, proc_info, ui, "SID ROW", "one process row in full", visible, 50).
action(proc_prov, proc_prov, ui, "SID ROW", "brush provenance for one process", visible, 50).
action(proc_roots, proc_roots, ui, "SID", "root processes of the captured tree", visible, 50).
action(process_env, process_env, ui, "SID ROW", "recorded environment of one process", visible, 50).
action(writer_id, writer_id, ui, "SID REL", "process that last wrote a path", visible, 50).
action(first_writer_id, first_writer_id, ui, "SID REL", "process that first wrote a path", visible, 50).
action(first_writer_prov, first_writer_prov, ui, "SID REL", "provenance of the first writer of a path", visible, 50).
action(stuck, stuck, ui, "SID", "live THREADS of a running box with wchan/syscall (wedge diagnosis)", visible, 50).
action(kill, kill, ui, "SID", "SIGTERM the box's runner", visible, 50).
action(dissolve, dissolve, ui, "SID", "remove a box, promoting its changes down into children", visible, 50).
action(apply_to_copy, apply_to_copy, ui, "SID", "apply a box's changes onto a COPY of its parent (parent untouched)", visible, 50).
action(ro_attach, ro_attach, ui, "SID [RO_ID|{kind,store,ref,rev,prefix,name}...]", "replace the box's read-only attachment list (ints = box ids, objects = external refs)", visible, 50).
action(git_checkout, git_checkout, ui, "SID STORE REF [DEST] [SUBPATH]", "check a commit out of a mirror store into the box's changes", visible, 50).
action(wiki_attach, wiki_attach, ui, "SID ROOT PAGE [PREFIX]", "attach a wikipedia mirror page as a read-only external reference pinned at its current head revision", visible, 50).
action(ietf_attach, ietf_attach, ui, "SID ROOT DRAFT [PREFIX]", "attach an IETF draft as a read-only external reference pinned at its current head revision", visible, 50).
action(mirror_jobs, mirror_jobs, ui, "", "list scheduled mirror-update jobs", visible, 90).
action(mirror_add, mirror_add, ui, "KIND SRC DEST [INTERVAL_SECS]", "add a scheduled mirror-update job", visible, 90).
action(mirror_run, mirror_run, ui, "ID", "force-run one mirror job now", visible, 100).
action(mirror_run_pending, mirror_run_pending, ui, "", "start every due unpaused mirror job", visible, 85).
action(mirror_pause, mirror_pause, ui, "ID PAUSED", "pause or resume a mirror job", visible, 80).
action(mirror_resume, mirror_pause, ui, "ID", "resume a mirror job", visible, 79).
action(mirror_rm, mirror_rm, ui, "ID", "remove a mirror job (git: drops the repo.git fetch buffer, keeps <dest>/store)", visible, 75).
action(rotate, rotate, ui, "SID", "promote a child box over its parent (both at rest)", visible, 50).
action(reload_rules, reload_rules, ui, "", "reload the file-rules from disk", visible, 50).
action(rescan, rescan, ui, "", "no-op; discovery is always fresh", visible, 50).
action(delete, delete, ui, "SID", "remove a box, promoting its changes down (alias of dissolve)", visible, 50).
action(open_files, open_files, ui, "", "compat stub; always []", internal, 10).
action(review_state, review_state, ui, "", "consolidation status + the selected box", internal, 10).
action(review_live, review_live, ui, "", "compat stub; always false", internal, 10).
action('review.session_changes', 'review.session_changes', ui, "SID", "changed files of a box", visible, 50).
action('review.hunks', 'review.hunks', ui, "SID REL", "unified-diff hunks for one changed file", visible, 50).
action('review.file_bytes', 'review.file_bytes', ui, "SID REL", "current bytes of one box path (captured write, else host), base64", visible, 50).
action('review.write_file', 'review.write_file', ui, "SID REL B64", "overwrite one box path's bytes (editor save) — captured like the box's own write, host untouched", visible, 50).
action('review.apply', 'review.apply', ui, "SID [PATHS...]", "apply a box's changes to the host", visible, 50).
action('review.discard', 'review.discard', ui, "SID [PATHS...]", "discard a box's changes", visible, 50).
action('review.file_groups', 'review.file_groups', ui, "SID", "named file-groups + how many of the box's changes each selects", visible, 50).
action('review.patch_text', 'review.patch_text', ui, "SID", "whole-box patch as base64", visible, 50).
action('review.change_mode', 'review.change_mode', ui, "SID REL", "current mode of one changed path", visible, 50).
action('review.decorate', 'review.decorate', ui, "SID REL", "kind/stale/is_text label for one change", visible, 50).
action('review.recent_changes', 'review.recent_changes', ui, "SID [LIMIT]", "newest-first slice of the change set", visible, 50).
action('review.box_summary', 'review.box_summary', ui, "SID [LIMIT]", "outputs/changes/procs/pipelines/edges bundle", visible, 50).
action('review.pipeline_context', 'review.pipeline_context', ui, "SID PROV_ID", "causal neighborhood of one pipeline", visible, 50).
action('review.makevars', 'review.makevars', ui, "SID [NAME_PAT] [VALUE_PAT] [LIMIT] [ANY]", "search recorded makefile variable assignments", visible, 50).
action('review.map_ids', 'review.map_ids', ui, "SID FROM [IDS...] TO", "translate provenance row ids across domains", internal, 10).
action('review.decorate_many', 'review.decorate_many', ui, "SID [RELS...]", "bulk decorate a window of changes", internal, 10).
action('review.apply_hunk', 'review.apply_hunk', ui, "SID REL HUNK_IX", "apply one hunk to the host", visible, 50).
action('review.discard_hunk', 'review.discard_hunk', ui, "SID REL HUNK_IX", "discard one hunk (revert it in the box)", visible, 50).
action('view.open', 'view.open', ui, "KIND SID [FILTER] [RUNNING_ONLY]", "open a server-side windowed view", internal, 10).
action('view.window', 'view.window', ui, "VIEW START SIZE", "read one window of an open view", internal, 10).
action('view.filter', 'view.filter', ui, "VIEW FILTER", "re-filter an open view", internal, 10).
action('view.find', 'view.find', ui, "VIEW ROW_ID", "locate a row id inside a view", internal, 10).
action('view.close', 'view.close', ui, "VIEW", "close a view", internal, 10).
action(consolidate_start, consolidate_start, ui, "", "compat no-op lifecycle poke", internal, 10).
action('review.invalidate_consolidation', 'review.invalidate_consolidation', ui, "", "compat no-op lifecycle poke", internal, 10).
action('review.invalidate_struct', 'review.invalidate_struct', ui, "", "compat no-op lifecycle poke", internal, 10).
action(ping, ping, ui, "", "liveness check; broadcasts a pong event", visible, 50).
action(box_new, box_new, ui, "[PARENT_SID]", "create an empty box and expose its mount", visible, 50).
action(struct_quick, struct_quick, ui, "SID REL", "quick structural diff of a binary change", visible, 50).
action('flows.list', 'flows.list', ui, "[SID]", "tshark-decoded HTTP/TLS flow rows for a box", visible, 50).
action('flows.detail', 'flows.detail', ui, "[SID] FRAME", "full tshark decode of one frame", visible, 50).
action('prompts.peek', 'prompts.peek', ui, "", "next pending network-permission prompt", visible, 50).
action('prompts.answer', 'prompts.answer', ui, "ID VERDICT", "answer a prompt (yes_once|no_once|allow_save|deny_save)", visible, 50).
action('prompts.ui_active', 'prompts.ui_active', ui, "BOOL", "mark the TUI prompt consumer active/inactive", internal, 10).
action('flows.packets', 'flows.packets', ui, "[SID] STREAM", "every frame of one TCP stream", visible, 50).
action(struct_finish, struct_finish, ui, "JOB", "collect a finished structural-diff job", internal, 10).
action(struct_cancel, struct_cancel, ui, "JOB", "cancel a structural-diff job", internal, 10).
action(box_drop, box_drop, ui, "SID", "unregister a box from the overlay (no reap)", visible, 50).
action(box_file_read, box_file_read, ui, "BOX PATH", "read a file from a box's merged view (base64)", visible, 50).
action(box_file_write, box_file_write, ui, "BOX PATH B64", "write a file into a box's layer (oaita agent tool: same refusal gate as the editor save, but MAY create new files)", visible, 50).
action(box_dir_list, box_dir_list, ui, "BOX PATH", "list a directory in a box's merged view", visible, 50).
action(box_path_kind, box_path_kind, ui, "BOX PATH", "file/dir/missing kind of a box path", internal, 10).
action('oci.load', 'oci.load', ui, "REFERENCE [NAME]", "pull + unpack an OCI image into at-rest boxes", visible, 50).
action('oci.images', 'oci.images', ui, "", "loaded OCI images (top box of each chain)", visible, 50).
action('svc.up', 'svc.up', ui, "NAME", "whether a svc.serve service is live", visible, 50).
action('oci.resolve', 'oci.resolve', ui, "REFERENCE", "resolve an image reference to its local top box", visible, 50).
action('oci.build', 'oci.build', ui, "SPEC", "run an in-box-shipped Dockerfile build host-side", visible, 50).
action('oaita.models', 'oaita.models', ui, "", "GGUF local-model catalog for the picker", visible, 50).
action('oaita.status', 'oaita.status', ui, "", "what the Api pane is wired to (external/local/none)", visible, 50).
action('oaita.probe', 'oaita.probe', ui, "SPEC", "1-token connection test of an external API config", visible, 50).
action(verbs, verbs, ui, "[FILTER]", "list every UI verb with its args and help", visible, 50).

action(mirror_browse, mirror_browse, local, "", "browse wiki mirror in the browser", visible, 50).
action(mirror_read, mirror_read, local, "", "read a mirror in the document reader", visible, 50).
action(apply, apply, control, "SID", "apply a box's changes to the host", visible, 50).
action(discard, discard, control, "SID", "discard a box's changes", visible, 50).
action(rename, rename, control, "SID NEW", "rename a box", visible, 50).
action(change_read, change_read, local, "", "open the selected change in the document reader", visible, 50).
action(change_edit, change_edit, local, "", "open the selected change in the text editor", visible, 50).
action(rule_new, rule_new, local, "", "create a new file rule", visible, 50).
action(rule_delete, rule_delete, local, "", "delete the selected file rule", visible, 50).
action(rule_edit, rule_edit, local, "", "edit the selected file rule", visible, 50).
action(quit, quit, local, "", "quit the engine", visible, 50).
action(detach, detach, local, "", "detach (leaves the engine running)", visible, 50).
action(refresh, refresh, local, "", "refresh sessions, changes, and rules", visible, 50).
action(filter, filter, local, "", "filter the active pane", visible, 50).
action(action_menu, action_menu, local, "", "show the actions popup for the selected row", visible, 50).
action(toggle_mark, toggle_mark, local, "", "select/unselect row for batch operations", visible, 50).

% Stable direct-Rust wire identities. These are explicit protocol facts: never
% derive them from declaration order. Public aliases relate through their
% normalized Handler identity, so mirror_resume uses mirror_pause's opcode and
% two-field wire schema. Local UI actions deliberately have no wire identity.
wire_handler('flows.detail', 1).
wire_handler('flows.list', 2).
wire_handler('flows.packets', 3).
wire_handler('oaita.models', 4).
wire_handler('oaita.probe', 5).
wire_handler('oaita.status', 6).
wire_handler('oci.build', 7).
wire_handler('oci.images', 8).
wire_handler('oci.load', 9).
wire_handler('oci.resolve', 10).
wire_handler('prompts.answer', 11).
wire_handler('prompts.peek', 12).
wire_handler('prompts.ui_active', 13).
wire_handler('review.apply', 14).
wire_handler('review.apply_hunk', 15).
wire_handler('review.box_summary', 16).
wire_handler('review.change_mode', 17).
wire_handler('review.decorate', 18).
wire_handler('review.decorate_many', 19).
wire_handler('review.discard', 20).
wire_handler('review.discard_hunk', 21).
wire_handler('review.file_bytes', 22).
wire_handler('review.file_groups', 23).
wire_handler('review.hunks', 24).
wire_handler('review.invalidate_consolidation', 25).
wire_handler('review.invalidate_struct', 26).
wire_handler('review.makevars', 27).
wire_handler('review.map_ids', 28).
wire_handler('review.patch_text', 29).
wire_handler('review.pipeline_context', 30).
wire_handler('review.recent_changes', 31).
wire_handler('review.session_changes', 32).
wire_handler('review.write_file', 33).
wire_handler('svc.up', 34).
wire_handler('view.close', 35).
wire_handler('view.filter', 36).
wire_handler('view.find', 37).
wire_handler('view.open', 38).
wire_handler('view.window', 39).
wire_handler(api_log, 40).
wire_handler(api_log_detail, 41).
wire_handler(apply_to_copy, 42).
wire_handler(box_dir_list, 43).
wire_handler(box_drop, 44).
wire_handler(box_file_read, 45).
wire_handler(box_file_write, 46).
wire_handler(box_new, 47).
wire_handler(box_path_kind, 48).
wire_handler(brushprov, 49).
wire_handler(build_edges, 50).
wire_handler(consolidate_start, 51).
wire_handler(delete, 52).
wire_handler(display_path, 53).
wire_handler(dissolve, 54).
wire_handler(first_writer_id, 55).
wire_handler(first_writer_prov, 56).
wire_handler(git_checkout, 57).
wire_handler(ietf_attach, 58).
wire_handler(kill, 59).
wire_handler(mirror_add, 60).
wire_handler(mirror_jobs, 61).
wire_handler(mirror_pause, 62).
wire_handler(mirror_rm, 63).
wire_handler(mirror_run, 64).
wire_handler(mirror_run_pending, 65).
wire_handler(open_files, 66).
wire_handler(output_detail, 67).
wire_handler(output_pipeline, 68).
wire_handler(outputs, 69).
wire_handler(ping, 70).
wire_handler(pipeline_procs, 71).
wire_handler(proc_info, 72).
wire_handler(proc_pipeline, 73).
wire_handler(proc_prov, 74).
wire_handler(proc_roots, 75).
wire_handler(process_env, 76).
wire_handler(processes, 77).
wire_handler(processes_live, 78).
wire_handler(reload_rules, 79).
wire_handler(rescan, 80).
wire_handler(resolve_box, 81).
wire_handler(review_live, 82).
wire_handler(review_state, 83).
wire_handler(ro_attach, 84).
wire_handler(rotate, 85).
wire_handler(select, 86).
wire_handler(session_dicts, 87).
wire_handler(struct_cancel, 88).
wire_handler(struct_finish, 89).
wire_handler(struct_quick, 90).
wire_handler(stuck, 91).
wire_handler(verbs, 92).
wire_handler(webcap, 93).
wire_handler(webcap_body, 94).
wire_handler(webcap_detail, 95).
wire_handler(wiki_attach, 96).
wire_handler(writer_id, 97).
wire_handler(apply, 128).
wire_handler(discard, 129).
wire_handler(rename, 130).

% Explicit shell forms. `Normalizer` relates parsed source arguments to the
% handler's wire arguments. Shared paths are intentional and resolved by the
% complete schema at end-of-input.
cli_form(mirror_jobs, ["mirror", "ls"], identity).
cli_form(mirror_add, ["mirror", "add"], identity).
cli_form(mirror_run, ["mirror", "run"], identity).
cli_form(mirror_run_pending, ["mirror", "run"], identity).
cli_form(mirror_pause, ["mirror", "pause"], pause_true).
cli_form(mirror_resume, ["mirror", "resume"], resume_false).
cli_form(mirror_rm, ["mirror", "rm"], identity).
cli_form(wiki_attach, ["attach", "wiki"], identity).
cli_form(ietf_attach, ["attach", "ietf"], identity).
cli_form(git_checkout, ["checkout"], identity).
cli_form('oci.load', ["oci", "load"], identity).
cli_form('oci.build', ["oci", "build"], identity).

% Key and menu meaning lives here. Context is an atom naming the UI context;
% `any` is global. More modal/navigation bindings are added as their actual UI
% dispatch paths are migrated, rather than preserving the dead Rust projection.
key_binding(mirror_run, char(r), 'Mirrors', 80).
key_binding(mirror_run_pending, char('R'), 'Mirrors', 80).
key_binding(mirror_pause, char(space), 'Mirrors', 80).
key_binding(mirror_rm, char('D'), 'Mirrors', 80).
key_binding(mirror_browse, char(b), 'Mirrors', 80).
key_binding(mirror_read, char('V'), 'Mirrors', 80).
key_binding(apply, char(a), any, 50).
key_binding(discard, char(x), any, 50).
key_binding(kill, char('K'), any, 50).
key_binding(dissolve, char('D'), any, 50).
key_binding(rename, char(r), 'Sessions', 80).
key_binding('review.apply_hunk', char(a), 'Hunks', 80).
key_binding('review.discard_hunk', char(x), 'Hunks', 80).
key_binding(change_read, char('V'), 'Changes', 80).
key_binding(change_edit, char('E'), 'Changes', 80).
key_binding(rule_new, char(n), 'Rules', 80).
key_binding(rule_delete, char(d), 'Rules', 80).
key_binding(quit, char(q), any, 50).
key_binding(detach, char(d), any, 40).
key_binding(refresh, char('R'), any, 40).
key_binding(filter, char('/'), any, 50).
key_binding(action_menu, char(m), any, 50).
key_binding(toggle_mark, char(space), any, 40).

menu_label(mirror_run, "Force-run this job").
menu_label(mirror_run_pending, "Run all pending jobs").
menu_label(mirror_pause, "Pause/Resume this job").
menu_label(mirror_rm, "Delete this job").
menu_label(mirror_browse, "Browse this wiki").
menu_label(mirror_read, "Read in document reader").
menu_label(apply, "Apply ALL changes to host").
menu_label(discard, "Discard ALL changes").
menu_label(kill, "Kill (SIGTERM)").
menu_label(dissolve, "Delete box (changes promoted down)").
menu_label(rename, "Rename box").
menu_label(stuck, "Diagnose stuck (wchan/syscall)").
menu_label(apply_to_copy, "Apply changes to a COPY of the parent").
menu_label(rotate, "Rotate: promote child over parent").
menu_label('review.apply_hunk', "Apply this hunk").
menu_label('review.discard_hunk', "Discard this hunk").
menu_label(rule_new, "New rule").
menu_label(rule_delete, "Delete rule").
menu_label(rule_edit, "Edit rule").

% Semantic context is independent from wire encoding. Box references remain
% protocol strings after resolution, but their source text denotes a live box.
argument_context(_, sid, box, root).
argument_context(_, parent_sid, box, root).
argument_context(_, box, box, root).
argument_context(_, rel, path, within(box(argument(sid)))).
argument_context(_, path, path, within(box(argument(box)))).

visible_action(Action) :-
    action(Action, _, _, _, _, visible, _).

argument_schema(Action, Schema) :-
    action(Action, _, _, Notation, _, _, _),
    notation_schema(Notation, Schema).

notation_schema("", []) :- !.
notation_schema(Notation, Schema) :-
    split_string(Notation, " ", "", Tokens),
    notation_tokens(Tokens, Schema).

notation_tokens([], []).
notation_tokens([Token|Tokens], [Arg|Args]) :-
    notation_argument(Token, Arg),
    notation_tokens(Tokens, Args).

notation_argument(Token, arg(Name, Kind, Cardinality, Shape)) :-
    optional_token(Token, Core0, Optional),
    repeated_token(Core0, Core1, Repeated),
    first_alternative(Core1, NameString),
    string_lower(NameString, Lower),
    atom_string(Name, Lower),
    argument_kind(NameString, Kind),
    argument_cardinality(Optional, Repeated, Cardinality),
    argument_shape(NameString, Repeated, Shape).

optional_token(Token, Core, true) :-
    sub_string(Token, 0, 1, _, "["),
    sub_string(Token, _, 1, 0, "]"),
    !,
    sub_string(Token, 1, _, 1, Core).
optional_token(Token, Token, false).

repeated_token(Token, Core, true) :-
    sub_string(Token, _, 3, 0, "..."),
    !,
    sub_string(Token, 0, _, 3, Core).
repeated_token(Token, Token, false).

first_alternative(Token, Name) :-
    ( sub_string(Token, Before, 1, _, "|")
    -> sub_string(Token, 0, Before, _, Name)
    ;  Name = Token
    ).

argument_cardinality(_, true, repeated) :- !.
argument_cardinality(true, false, optional) :- !.
argument_cardinality(false, false, required).

argument_shape(Name, true, array) :-
    ( Name = "PATHS" ; Name = "RELS" ; Name = "IDS" ),
    !.
argument_shape(_, true, spread) :- !.
argument_shape(_, false, scalar).

argument_kind(Name, boolean) :-
    ( Name = "BOOL" ; Name = "PAUSED" ; Name = "RUNNING_ONLY" ; Name = "ANY" ),
    !.
argument_kind(Name, integer) :-
    ( Name = "ID" ; Name = "ROW" ; Name = "OUTPUT" ; Name = "FRAME"
    ; Name = "STREAM" ; Name = "VIEW" ; Name = "START" ; Name = "SIZE"
    ; Name = "LIMIT" ; Name = "JOB" ; Name = "HUNK_IX" ; Name = "PROV_ID"
    ; Name = "AMOUNT" ; Name = "ROW_ID" ; Name = "RO_ID" ; Name = "PIPELINE"
    ; Name = "INTERVAL_SECS" ; Name = "IDS"
    ),
    !.
argument_kind(Name, path) :-
    ( Name = "PATH" ; Name = "PATHS" ; Name = "REL" ; Name = "RELS"
    ; Name = "DEST" ; Name = "ROOT" ; Name = "SUBPATH"
    ),
    !.
argument_kind("B64", base64) :- !.
argument_kind("SPEC", spec) :- !.
argument_kind(_, string).
