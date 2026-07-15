:- module(action_grammar,
          [ action/7,
            wire_handler/3,
            wire_request_fields/2,
            wire_protocol_version/1,
            wire_limit/2,
            wire_type/2,
            wire_enum/3,
            wire_variant/4,
            wire_request/6,
            wire_response/3,
            wire_mode/3,
            wire_event/3,
            wire_frame/7,
            valid_wire_type/1,
            valid_wire_fields/1,
            valid_transport_catalog/0,
            valid_action_catalog/0,
            representation/3,
            convert/4,
            valid_action/1,
            parse/2,
            parse/3,
            completions/3,
            highlights/2,
            render/2,
            catalog/2,
            context_plan/3,
            resolve_context_plan/3,
            context_completion_plan/3,
            resolve_context_completion/3,
            action_request/2,
            action_relation_grammar/1,
            application/3
          ]).

:- use_module(action_catalog).
:- use_module(context_relation).
:- use_module(grammar_engine).
:- use_module(transport_catalog).

/** <module> Relational parser and representation hub

The grammar consumes neutral lexer evidence. Rust supplies UTF-8 byte spans and
source surfaces; it does not classify command names or arguments. This module
relates those surfaces to canonical actions, typed wire values, syntax,
descriptions, completions, and rendered forms using the sole action definition
in `action_catalog.pl`.

Input ends in `end(BytePosition)`. Source tokens are represented as `unit/8`;
their incoming semantic/syntax/provider fields are deliberately ignored by the
command grammar. `edit_tear/3` marks the one source range completion may
replace. `source_tear/3` remains an explicit unrecognized hole and is never
silently repaired.

The normalized result is
`command(Action, Handler, Target, WireArguments)`. Rust therefore receives a
dispatch-ready value without consulting another registry.
*/

valid_action(Action) :-
    action(Action, Handler, Target, Notation, Description, Visibility,
           Preference),
    atom(Action),
    atom(Handler),
    valid_target(Target),
    string(Notation),
    string(Description),
    valid_visibility(Visibility),
    number(Preference),
    argument_schema(Action, Schema),
    valid_schema(Schema),
    valid_action_wire(Target, Handler),
    once(action_form(Action, _, _)).

valid_action_wire(local, Handler) :- \+ wire_handler(Handler, _, _).
valid_action_wire(ui, Handler) :- valid_wire_handler(Handler).
valid_action_wire(control, Handler) :- valid_wire_handler(Handler).

valid_wire_handler(Handler) :-
    wire_handler(Handler, Code, _),
    integer(Code),
    Code > 0,
    argument_schema(Handler, Schema),
    valid_schema(Schema).

% Deep schema closure and cross-row uniqueness are startup/catalog work, not
% work for each parse candidate.  Keeping this out of valid_action/1 prevents
% ordinary parsing from walking the entire action-result type graph.
valid_action_catalog :-
    findall(Action, action(Action, _, _, _, _, _, _), Actions),
    Actions \= [],
    all_unique_terms(Actions),
    findall(Words,
            ( action(Action, _, _, _, _, _, _), action_words(Action, Words) ),
            CommandNames),
    all_unique_terms(CommandNames),
    all_actions_valid(Actions),
    findall(Handler-Code, wire_handler(Handler, Code, _), HandlerCodes),
    HandlerCodes \= [],
    handler_codes_unique(HandlerCodes),
    all_wire_rows_valid.

all_actions_valid([]).
all_actions_valid([Action|Actions]) :-
    once(valid_action(Action)),
    all_actions_valid(Actions).

all_wire_rows_valid :-
    \+ (wire_handler(Handler, Code, ResultType),
        ( \+ atom(Handler)
        ; \+ (integer(Code), Code > 0)
        ; \+ valid_wire_type(ResultType)
        ; \+ (wire_request_fields(Handler, RequestFields),
               valid_wire_fields(RequestFields))
        ; \+ (action(Handler, Handler, Target, _, _, _, _),
               (Target = ui ; Target = control))
        )).

handler_codes_unique(Rows) :-
    handler_code_columns(Rows, Handlers, Codes),
    all_unique_terms(Handlers),
    all_unique_terms(Codes).

handler_code_columns([], [], []).
handler_code_columns([Handler-Code|Rows], [Handler|Handlers], [Code|Codes]) :-
    handler_code_columns(Rows, Handlers, Codes).

all_unique_terms(Terms) :-
    sort(Terms, Unique),
    length(Terms, Count),
    length(Unique, Count).

valid_target(ui).
valid_target(control).
valid_target(local).

valid_visibility(visible).
valid_visibility(internal).

valid_schema([]).
valid_schema([arg(Name, Kind, Cardinality, Shape)|Schema]) :-
    atom(Name),
    valid_kind(Kind),
    valid_cardinality(Cardinality),
    valid_shape(Cardinality, Shape),
    valid_schema(Schema).

valid_kind(boolean).
valid_kind(integer).
valid_kind(string).
valid_kind(path).
valid_kind(base64).
valid_kind(oci_spec).
valid_kind(api_spec).
valid_kind(spec).

valid_cardinality(required).
valid_cardinality(optional).
valid_cardinality(repeated).

valid_shape(required, scalar).
valid_shape(optional, scalar).
valid_shape(repeated, array).
valid_shape(repeated, spread).

%! action_form(?Action, ?Specs, ?Projection) is nondet.
%
% A form is a sequence of `literal/5` and `argument/1` specs. Projections
% relate source-form arguments to the handler's typed wire arguments. There is
% exactly one textual form per action: its words are mechanically decoded from
% its sole identifier rather than declared as additional names.

action_form(Action, Specs, Projection) :-
    action(Action, _, _, _, _, _, _),
    action_source_schema(Action, Schema),
    action_words(Action, Words),
    action_literal_specs(Words, Action, Literals),
    argument_projection(Action, Projection),
    schema_specs(Schema, ArgSpecs),
    append(Literals, ArgSpecs, Specs).

action_source_schema(mirror_pause,
                     [arg(id, integer, required, scalar)]) :- !.
action_source_schema(Action, Schema) :-
    argument_schema(Action, Schema).

argument_projection(mirror_pause, pause_true) :- !.
argument_projection(mirror_resume, resume_false) :- !.
argument_projection(_, identity).

%! action_relation_grammar(-Grammar) is det.
%
% Materialize the complete action language as one immutable executable grammar
% value. The engine interprets this data without importing this module. This is
% the cut-over value for the generic transformation API; the older predicates
% below remain only until their Rust consumers have migrated.

action_relation_grammar(choice_grammar(Alternatives)) :-
    findall(alternative(Action, Preference, Grammar),
            action_relation_alternative(Action, Preference, Grammar),
            Alternatives).

action_relation_alternative(Action, Preference,
                            projection_grammar(Sequence, Projections)) :-
    action(Action, Handler, Target, _, _, _, Preference),
    action_form(Action, Specs, Projection),
    action_contexts(Action, Contexts),
    action_terminals(Terminals),
    Sequence = sequence_grammar(Specs, terminals(Terminals), separator(" "),
                                contexts(Contexts)),
    action_semantic_template(Action, Handler, Target, Projection, Template),
    Projections = [projection(command, Template)].

action_semantic_template(Action, Handler, Target, Projection,
                         structure(command,
                                   [constant(Action), constant(Handler),
                                    constant(Target), Arguments])) :-
    action_arguments_template(Projection, Arguments).

action_arguments_template(identity, reference(arguments)).
action_arguments_template(
    pause_true,
    concatenate(reference(arguments), sequence([constant(boolean(true))]))).
action_arguments_template(
    resume_false,
    concatenate(reference(arguments), sequence([constant(boolean(false))]))).

action_contexts(Action, Contexts) :-
    action_source_schema(Action, Schema),
    schema_contexts(Action, Schema, Contexts).

schema_contexts(_, [], []).
schema_contexts(Action, [arg(Name, _, _, _)|Schema], Contexts) :-
    ( argument_context(Action, Name, Domain, Scope)
    -> Contexts = [context(Name, one, Domain, Scope)|Rest]
    ;  Contexts = Rest
    ),
    schema_contexts(Action, Schema, Rest).

action_terminals([
    terminal(boolean, boolean,
             codec(enumeration([surface(boolean(true), "true"),
                                surface(boolean(false), "false")]))),
    terminal(integer, integer, codec(integer(integer))),
    terminal(string, string,
             codec(choice([text(string), integer(integer)]))),
    terminal(path, path, codec(text(path))),
    terminal(base64, base64, codec(text(base64))),
    terminal(oci_spec, structured_spec, codec(json(OciShape))),
    terminal(api_spec, structured_spec, codec(json(ApiShape))),
    terminal(spec, spec, codec(text(spec)))
]) :-
    oci_shape(OciShape),
    api_shape(ApiShape).

oci_shape(object(oci_spec,
                 [field("context_tar_gz", string),
                  field("dockerfile", string),
                  field("tag", nullable(string, none, some)),
                  field("net", string),
                  field("build_args",
                        array(tuple(pair, [string, string])))] )).

api_shape(object(api_spec,
                 [field("base_url", string),
                  field("model", string),
                  field("api_key", string)])).

schema_specs([], []).
schema_specs([Arg|Schema], [argument(Arg)|Specs]) :-
    schema_specs(Schema, Specs).

action_literal_specs([Text], Action,
                     [literal(Semantic, Text, action_identifier,
                              Action, 30)]) :-
    atom_string(Semantic, Text).
action_literal_specs(Words, Action, Specs) :-
    Words = [_,_|_],
    action_literal_specs(Words, Action, first, Specs).

action_literal_specs([], _, _, []).
action_literal_specs([Text|Words], Action, Position,
                  [literal(Semantic, Text, Syntax, Action, Preference)|Specs]) :-
    atom_string(Semantic, Text),
    action_literal_metadata(Position, Syntax, Preference),
    action_literal_specs(Words, Action, rest, Specs).

action_literal_metadata(first, command_namespace, 10).
action_literal_metadata(rest, action_word, 20).

action_words(Action, Words) :-
    atom_codes(Action, Codes),
    identifier_word_codes(Codes, WordCodes),
    word_strings(WordCodes, Words).

identifier_word_codes([], []) :- !.
identifier_word_codes(Codes, [Word|Words]) :-
    take_identifier_word(Codes, Word, Rest),
    Word \= [],
    identifier_word_codes(Rest, Words).

take_identifier_word([], [], []).
take_identifier_word([Code|Codes], [], Codes) :-
    identifier_separator(Code), !.
take_identifier_word([Code|Codes], [Code|Word], Rest) :-
    take_identifier_word(Codes, Word, Rest).

identifier_separator(0'.).
identifier_separator(0'_).

word_strings([], []).
word_strings([Codes|CodeWords], [Word|Words]) :-
    string_codes(Word, Codes),
    word_strings(CodeWords, Words).

%! representation(?Action, ?Kind, ?Value) is nondet.
%
% Every public projection is rooted in the normalized action facts and the
% same form relation used by parsing and rendering. `syntax` exposes
% the exact executable spec rather than reconstructing usage text elsewhere.

representation(Action, action, Action) :-
    action(Action, _, _, _, _, _, _).
representation(Action, command, command(Words, Projection)) :-
    action_form(Action, Specs, Projection),
    form_literal_prefix(Specs, Words).
representation(Action, syntax, syntax(Specs)) :-
    action_form(Action, Specs, _).
representation(Action, source_schema, schema(Schema)) :-
    action(Action, _, _, _, _, _, _),
    action_source_schema(Action, Schema).
representation(Action, wire,
               wire(Code, Handler, Target, RequestFields, ResultType)) :-
    action(Action, Handler, Target, _, _, _, _),
    wire_handler(Handler, Code, ResultType),
    wire_request_fields(Handler, RequestFields).
representation(Action, help, help(Notation, Description)) :-
    action(Action, _, _, Notation, Description, _, _).
representation(Action, key, key(Key, Context, Preference)) :-
    key_binding(Action, Key, Context, Preference).
representation(Action, menu, menu(Label)) :-
    menu_label(Action, Label).
representation(transport(request, Name), wire,
               request(Code, Success, Fields, Fds, Authority)) :-
    wire_request(Name, Code, Success, Fields, Fds, Authority).
representation(transport(response, Name), wire,
               response(Code, Fields)) :-
    wire_response(Name, Code, Fields).
representation(transport(mode, Name), wire,
               mode(Code, Fields)) :-
    wire_mode(Name, Code, Fields).
representation(transport(event, Name), wire,
               event(Code, Fields)) :-
    wire_event(Name, Code, Fields).
representation(transport(frame(Stream), Name), wire,
               frame(Code, Direction, Fields, Fds, Transition)) :-
    wire_frame(Stream, Name, Code, Direction, Fields, Fds, Transition).
representation(transport(type, Name), schema, Definition) :-
    wire_type(Name, Definition).
representation(transport(enum(Type), Case), wire, enum(Code)) :-
    wire_enum(Type, Case, Code).
representation(transport(variant(Type), Case), wire,
               variant(Code, Fields)) :-
    wire_variant(Type, Case, Code, Fields).

form_literal_prefix([], []).
form_literal_prefix([argument(_)|_], []).
form_literal_prefix([literal(_, Text, _, _, _)|Specs], [Text|Words]) :-
    form_literal_prefix(Specs, Words).

convert(FromKind, From, ToKind, To) :-
    representation(Action, FromKind, From),
    representation(Action, ToKind, To).

%! action_request(+Command, -Request) is semidet.
%
% Relate a fully parsed and context-resolved command to its concrete binary
% request representation. The result is positional because field names are
% schema metadata and are not bytes on the wire. Rust only materializes this
% already-typed value as the generated enum variant; it does not reinterpret
% parser kinds or cardinalities.

action_request(command(_Action, Handler, Target, Arguments),
               action_request(Handler, Code, Values)) :-
    wire_target(Target),
    wire_handler(Handler, Code, _ResultType),
    \+ action_catalog:wire_request_override(Handler, _),
    argument_schema(Handler, Schema),
    wire_request_fields(Handler, Fields),
    request_field_values(Schema, Fields, Arguments, Values).
action_request(command(_Action, ro_attach, Target,
                       [BoxSource|AttachmentSources]),
               action_request(ro_attach, Code, [Box, Attachments])) :-
    wire_target(Target),
    wire_handler(ro_attach, Code, _),
    wire_source_value(box_id, BoxSource, Box),
    readonly_attachment_sources(AttachmentSources, Attachments).
action_request(command(_Action, 'view.open', Target, Arguments),
               action_request('view.open', Code,
                              [Kind, Box, Filter, RunningOnly])) :-
    wire_target(Target),
    wire_handler('view.open', Code, _),
    view_open_sources(Arguments, Kind, Box, Filter, RunningOnly).
action_request(command(_Action, 'view.filter', Target,
                       [ViewSource, FilterSource]),
               action_request('view.filter', Code, [View, Filter])) :-
    wire_target(Target),
    wire_handler('view.filter', Code, _),
    wire_source_value(view_id, ViewSource, View),
    filter_source_value(FilterSource, Filter).
action_request(command(_Action, 'oci.build', Target,
                       [oci_spec(Context, Dockerfile, TagSource, NetSource,
                                 BuildArgumentSources)]),
               action_request('oci.build', Code,
                              [record(ContextBytes, Dockerfile, Tag, Net,
                                      BuildArguments)])) :-
    wire_target(Target),
    wire_handler('oci.build', Code, _),
    wire_source_value(bytes(blob_bytes), base64(Context), ContextBytes),
    bounded_source_text(blob_bytes, Dockerfile),
    optional_source_text(TagSource, Tag),
    wire_source_value(net_mode, string(NetSource), Net),
    build_argument_values(BuildArgumentSources, BuildArguments).
action_request(command(_Action, 'oaita.probe', Target,
                       [api_spec(BaseUrl, Model, ApiKey)]),
               action_request('oaita.probe', Code,
                              [record(BaseUrl, Model, ApiKey)])) :-
    wire_target(Target),
    wire_handler('oaita.probe', Code, _),
    bounded_source_text(text_bytes, BaseUrl),
    bounded_source_text(text_bytes, Model),
    bounded_source_text(text_bytes, ApiKey).

wire_target(ui).
wire_target(control).

readonly_attachment_sources([], []).
readonly_attachment_sources([Source|Sources], [box(Box)|Attachments]) :-
    wire_source_value(box_id, Source, Box),
    readonly_attachment_sources(Sources, Attachments).

optional_source_text(none, none).
optional_source_text(some(Value), some(Value)) :-
    bounded_source_text(text_bytes, Value).

build_argument_values(Sources, Values) :-
    wire_limit(environment_entries, Maximum),
    length(Sources, Count),
    Count =< Maximum,
    build_argument_values(Sources, [], Values).

build_argument_values([], _, []).
build_argument_values([pair(Key, Value)|Sources], Seen,
                      [pair(Key, Value)|Values]) :-
    bounded_source_text(text_bytes, Key),
    bounded_source_text(text_bytes, Value),
    text_key_absent(Key, Seen),
    build_argument_values(Sources, [Key|Seen], Values).

text_key_absent(_, []).
text_key_absent(Key, [Seen|Keys]) :-
    Key \= Seen,
    text_key_absent(Key, Keys).

view_open_sources([KindSource, BoxSource], Kind, Box, none, true) :-
    wire_source_value(view_kind, KindSource, Kind),
    wire_source_value(box_id, BoxSource, Box).
view_open_sources([KindSource, BoxSource, Third], Kind, Box, Filter,
                  RunningOnly) :-
    wire_source_value(view_kind, KindSource, Kind),
    wire_source_value(box_id, BoxSource, Box),
    ( filter_source_value(Third, Filter), RunningOnly = true
    ; wire_source_value(bool, Third, RunningOnly), Filter = none
    ).
view_open_sources([KindSource, BoxSource, FilterSource, RunningSource],
                  Kind, Box, Filter, RunningOnly) :-
    wire_source_value(view_kind, KindSource, Kind),
    wire_source_value(box_id, BoxSource, Box),
    filter_source_value(FilterSource, Filter),
    wire_source_value(bool, RunningSource, RunningOnly).

% The command-text filter representation is deliberately small and typed:
% `none` disables filtering; `KIND:PATTERN` is one enabled conjunctive clause.
% UI code constructs the same closed filter_spec record directly and does not
% send this textual spelling over the Rust-to-Rust socket.
filter_source_value(string("none"), none) :- !.
filter_source_value(string(""), none) :- !.
filter_source_value(string(Text), some([record(Kind, Pattern, and,
                                             false, true)])) :-
    sub_string(Text, Before, 1, After, ":"),
    sub_string(Text, 0, Before, _, KindText),
    PatternStart is Before + 1,
    sub_string(Text, PatternStart, After, 0, Pattern),
    wire_source_value(filter_kind, string(KindText), Kind),
    bounded_source_text(text_bytes, Pattern).

request_field_values([], [], [], []).
request_field_values(
    [arg(Name, _Kind, required, scalar)|Schema],
    [field(Name, Type)|Fields], [Argument|Arguments], [Value|Values]) :-
    wire_source_value(Type, Argument, Value),
    request_field_values(Schema, Fields, Arguments, Values).
request_field_values(
    [arg(Name, _Kind, optional, scalar)|Schema],
    [field(Name, option(Type))|Fields], [Argument|Arguments],
    [some(Value)|Values]) :-
    wire_source_value(Type, Argument, Value),
    request_field_values(Schema, Fields, Arguments, Values).
request_field_values(
    [arg(Name, _Kind, optional, scalar)|Schema],
    [field(Name, option(_Type))|Fields], Arguments, [none|Values]) :-
    request_field_values(Schema, Fields, Arguments, Values).
request_field_values(
    [arg(Name, _Kind, repeated, array)|Schema],
    [field(Name, list(Type, Limit))|Fields],
    [array(Arguments0)|Arguments], [List|Values]) :-
    wire_source_list(Type, Limit, Arguments0, List),
    request_field_values(Schema, Fields, Arguments, Values).
request_field_values(
    [arg(Name, _Kind, repeated, array)|Schema],
    [field(Name, list(_Type, _Limit))|Fields], Arguments, [[]|Values]) :-
    request_field_values(Schema, Fields, Arguments, Values).
request_field_values(
    [arg(Name, _Kind, repeated, spread)],
    [field(Name, list(Type, Limit))], Arguments, [List]) :-
    wire_source_list(Type, Limit, Arguments, List).

wire_source_list(Type, Limit, Arguments, Values) :-
    wire_limit(Limit, Maximum),
    length(Arguments, Count),
    Count =< Maximum,
    wire_source_values(Type, Arguments, Values).

wire_source_values(_, [], []).
wire_source_values(Type, [Argument|Arguments], [Value|Values]) :-
    wire_source_value(Type, Argument, Value),
    wire_source_values(Type, Arguments, Values).

wire_source_value(Type, Source, Value) :-
    wire_type(Type, alias(Alias)),
    !,
    wire_source_value(Alias, Source, Value).
wire_source_value(u16, integer(Value), Value) :-
    integer(Value), Value >= 0, Value =< 65535.
wire_source_value(u32, integer(Value), Value) :-
    integer(Value), Value >= 0, Value =< 4294967295.
wire_source_value(u64, integer(Value), Value) :-
    integer(Value), Value >= 0.
wire_source_value(s32, integer(Value), Value) :-
    integer(Value), Value >= -2147483648, Value =< 2147483647.
wire_source_value(s64, integer(Value), Value) :- integer(Value).
wire_source_value(bool, boolean(Value), Value) :-
    ( Value = true ; Value = false ).
wire_source_value(text(Limit), string(Value), Value) :-
    bounded_source_text(Limit, Value).
wire_source_value(bytes(Limit), path(Value), Value) :-
    bounded_source_text(Limit, Value).
wire_source_value(bytes(Limit), base64(Value), base64(Value)) :-
    base64_decoded_bytes(Value, Bytes),
    wire_limit(Limit, Maximum),
    Bytes =< Maximum.
wire_source_value(Type, string(Text), Case) :-
    wire_type(Type, enum),
    atom_string(Case, Text),
    wire_enum(Type, Case, _).

bounded_source_text(Limit, Value) :-
    string(Value),
    wire_limit(Limit, Maximum),
    string_bytes(Value, Bytes),
    Bytes =< Maximum.

base64_decoded_bytes(Value, Bytes) :-
    string(Value),
    string_codes(Value, Codes),
    base64_codes_bytes(Codes, Bytes).

base64_codes_bytes([], 0).
base64_codes_bytes([A, B, 61, 61], 1) :-
    base64_code(A), base64_code(B), !.
base64_codes_bytes([A, B, C, 61], 2) :-
    base64_code(A), base64_code(B), base64_code(C), !.
base64_codes_bytes([A, B, C, D|Codes], Bytes) :-
    base64_code(A), base64_code(B), base64_code(C), base64_code(D),
    base64_codes_bytes(Codes, Rest),
    Bytes is Rest + 3.

base64_code(Code) :- Code >= 0'A, Code =< 0'Z, !.
base64_code(Code) :- Code >= 0'a, Code =< 0'z, !.
base64_code(Code) :- Code >= 0'0, Code =< 0'9, !.
base64_code(0'+).
base64_code(0'/).

% SWI's string_length/2 counts Unicode code points. Wire text and byte bounds
% are byte bounds, so measure the canonical UTF-8 representation explicitly.
string_bytes(Value, Bytes) :-
    string_codes(Value, Codes),
    utf8_codes_bytes(Codes, 0, Bytes).

utf8_codes_bytes([], Bytes, Bytes).
utf8_codes_bytes([Code|Codes], Bytes0, Bytes) :-
    utf8_code_bytes(Code, Width),
    Bytes1 is Bytes0 + Width,
    utf8_codes_bytes(Codes, Bytes1, Bytes).

utf8_code_bytes(Code, 1) :- Code >= 0, Code =< 127, !.
utf8_code_bytes(Code, 2) :- Code =< 2047, !.
utf8_code_bytes(Code, 3) :- Code =< 65535, !.
utf8_code_bytes(Code, 4) :- Code =< 1114111.

%! parse(+Items, -Result) is nondet.

parse(Items, Result) :-
    parse(Items, exact, Result).

%! parse(+Items, +Mode, -Result) is nondet.

parse(Items, Mode,
      parse_result(command(Action, Handler, Target, WireArgs), Status,
                   Evidence, Preference)) :-
    neutral_input(Items, Body),
    valid_relation_mode(Mode),
    action(Action, Handler, Target, _, _, _, ActionPreference),
    valid_action(Action),
    form_relation(Action, Body, Mode, Projection, SourceArgs,
                  Evidence, EditCount),
    normalize_args(Projection, SourceArgs, WireArgs),
    relation_status(Mode, EditCount, Status),
    evidence_preference(Evidence, ActionPreference, Preference).

normalize_args(identity, Args, Args).
normalize_args(pause_true, Args, WireArgs) :-
    append(Args, [boolean(true)], WireArgs).
normalize_args(resume_false, Args, WireArgs) :-
    append(Args, [boolean(false)], WireArgs).

denormalize_args(identity, Args, Args).
denormalize_args(pause_true, WireArgs, Args) :-
    append(Args, [boolean(true)], WireArgs).
denormalize_args(resume_false, WireArgs, Args) :-
    append(Args, [boolean(false)], WireArgs).

text(Text) :- string(Text), !.
text(Text) :- atom(Text).

% Keep the embedded application core-only. SWI's boot image provides append/1
% but not library(lists)' append/3.
append([], Tail, Tail).
append([Head|Items], Tail, [Head|Result]) :-
    append(Items, Tail, Result).

% Singular execution relation for an action form.  Concrete source units,
% edit tears, and rendered surfaces differ only at the terminal relation;
% sequence, cardinality, normalization inputs, and end-of-form behavior are
% shared by parsing and rendering.
form_relation(Action, Items, Mode, Projection, SourceArgs, Evidence,
              EditCount) :-
    action_form(Action, Specs, Projection),
    relate_sequence(Specs, Items, Mode, action_grammar:terminal_relation,
                    SourceArgs, Evidence, EditCount).

terminal_relation(surface(Kind, Value, Surface)) :-
    argument_surface(Kind, Value, Surface).
terminal_relation(syntax(Kind, Syntax)) :-
    kind_syntax(Kind, Syntax).

argument_surface(boolean, boolean(true), Surface) :-
    canonical_text_surface("true", Surface).
argument_surface(boolean, boolean(false), Surface) :-
    canonical_text_surface("false", Surface).
argument_surface(integer, integer(Value), Text) :-
    ( integer(Value)
    -> number_string(Value, Text)
    ;  text_string(Text, String),
       number_string(Value, String),
       integer(Value)
    ).
argument_surface(string, string(Text), Surface) :-
    string_value_surface(Text, Surface).
argument_surface(string, integer(Value), Surface) :-
    argument_surface(integer, integer(Value), Surface).
argument_surface(path, path(Text), Surface) :-
    string_value_surface(Text, Surface).
argument_surface(base64, base64(Text), Surface) :-
    string_value_surface(Text, Surface).
argument_surface(oci_spec, Spec, Surface) :-
    structured_json_surface(oci_spec_json, Spec, Surface).
argument_surface(api_spec, Spec, Surface) :-
    structured_json_surface(api_spec_json, Spec, Surface).
argument_surface(spec, spec(Text), Surface) :-
    string_value_surface(Text, Surface).

% Structured source arguments use JSON only as a textual representation.  The
% ordinary terminal relation parses it here into a closed action-specific term;
% generic JSON never crosses the request boundary.  Rendering emits compact
% ASCII-only JSON so the neutral whitespace framer keeps it as one source item.
structured_json_surface(Relation, Value, Surface) :-
    ground(Surface),
    !,
    text_string(Surface, SurfaceString),
    string_codes(SurfaceString, Codes),
    phrase(json_document(Json), Codes),
    call(Relation, Value, Json).
structured_json_surface(Relation, Value, Surface) :-
    ground(Value),
    call(Relation, Value, Json),
    json_render_value(Json, Codes),
    string_codes(Surface, Codes).

oci_spec_json(
    oci_spec(Context, Dockerfile, Tag, Net, BuildArguments),
    json_object(Pairs)) :-
    ground(Pairs),
    !,
    json_take("context_tar_gz", Pairs, json_string(Context), Pairs1),
    json_take("dockerfile", Pairs1, json_string(Dockerfile), Pairs2),
    json_take("tag", Pairs2, TagJson, Pairs3),
    json_optional_string(TagJson, Tag),
    json_take("net", Pairs3, json_string(Net), Pairs4),
    json_take("build_args", Pairs4, json_array(BuildJson), []),
    json_build_arguments(BuildJson, BuildArguments).
oci_spec_json(
    oci_spec(Context, Dockerfile, Tag, Net, BuildArguments),
    json_object([
        "context_tar_gz"-json_string(Context),
        "dockerfile"-json_string(Dockerfile),
        "tag"-TagJson,
        "net"-json_string(Net),
        "build_args"-json_array(BuildJson)
    ])) :-
    json_optional_string(TagJson, Tag),
    json_build_arguments(BuildJson, BuildArguments).

api_spec_json(api_spec(BaseUrl, Model, ApiKey), json_object(Pairs)) :-
    ground(Pairs),
    !,
    json_take("base_url", Pairs, json_string(BaseUrl), Pairs1),
    json_take("model", Pairs1, json_string(Model), Pairs2),
    json_take("api_key", Pairs2, json_string(ApiKey), []).
api_spec_json(api_spec(BaseUrl, Model, ApiKey),
              json_object([
                  "base_url"-json_string(BaseUrl),
                  "model"-json_string(Model),
                  "api_key"-json_string(ApiKey)
              ])).

json_take(Name, [Name-Value|Pairs], Value, Pairs) :- !.
json_take(Name, [Pair|Pairs], Value, [Pair|Rest]) :-
    json_take(Name, Pairs, Value, Rest).

json_optional_string(json_null, none).
json_optional_string(json_string(Value), some(Value)).

json_build_arguments([], []).
json_build_arguments(
    [json_array([json_string(Key), json_string(Value)])|Json],
    [pair(Key, Value)|Arguments]) :-
    json_build_arguments(Json, Arguments).

json_document(Value) --> json_space, json_value(Value), json_space.

json_value(json_string(Value)) --> json_string(Value).
json_value(json_object(Pairs)) --> [123], json_space,
                                  json_object_members(Pairs), json_space, [125].
json_value(json_array(Values)) --> [91], json_space,
                                  json_array_members(Values), json_space, [93].
json_value(json_null) --> [110, 117, 108, 108].
json_value(json_true) --> [116, 114, 117, 101].
json_value(json_false) --> [102, 97, 108, 115, 101].

json_object_members([]) --> [].
json_object_members([Key-Value|Pairs]) -->
    json_string(Key), json_space, [58], json_space, json_value(Value),
    json_object_members_tail(Pairs).

json_object_members_tail([]) --> [].
json_object_members_tail([Key-Value|Pairs]) -->
    json_space, [44], json_space, json_string(Key), json_space, [58],
    json_space, json_value(Value), json_object_members_tail(Pairs).

json_array_members([]) --> [].
json_array_members([Value|Values]) -->
    json_value(Value), json_array_members_tail(Values).

json_array_members_tail([]) --> [].
json_array_members_tail([Value|Values]) -->
    json_space, [44], json_space, json_value(Value),
    json_array_members_tail(Values).

json_string(Value) --> [34], json_string_codes(Codes), [34],
                       { string_codes(Value, Codes) }.

json_string_codes([]) --> [].
json_string_codes([Code|Codes]) -->
    json_string_code(Code),
    json_string_codes(Codes).

json_string_code(Code) --> [92], json_escape(Code), !.
json_string_code(Code) --> [Code],
    { Code >= 32, Code =\= 34, Code =\= 92 }.

json_escape(34) --> [34].
json_escape(92) --> [92].
json_escape(47) --> [47].
json_escape(8) --> [98].
json_escape(12) --> [102].
json_escape(10) --> [110].
json_escape(13) --> [114].
json_escape(9) --> [116].
json_escape(Code) --> [117], json_hex_quad(High),
                      json_unicode_tail(High, Code).

json_unicode_tail(High, Code) -->
    { High >= 55296, High =< 56319 },
    !,
    [92, 117], json_hex_quad(Low),
    { Low >= 56320, Low =< 57343,
      Code is 65536 + ((High - 55296) * 1024) + Low - 56320
    }.
json_unicode_tail(Code, Code) -->
    { ( Code < 55296 ; Code > 57343 ) }.

json_hex_quad(Value) -->
    json_hex_digit(A), json_hex_digit(B),
    json_hex_digit(C), json_hex_digit(D),
    { Value is A * 4096 + B * 256 + C * 16 + D }.

json_hex_digit(Value) --> [Code], { json_hex_code(Code, Value) }.

json_hex_code(Code, Value) :-
    Code >= 48, Code =< 57, !, Value is Code - 48.
json_hex_code(Code, Value) :-
    Code >= 65, Code =< 70, !, Value is Code - 55.
json_hex_code(Code, Value) :-
    Code >= 97, Code =< 102, Value is Code - 87.

json_space --> [Code], { json_space_code(Code) }, !, json_space.
json_space --> [].

json_space_code(32).
json_space_code(9).
json_space_code(10).
json_space_code(13).

json_render_value(json_string(Value), [34|Codes]) :-
    string_codes(Value, StringCodes),
    json_render_string_codes(StringCodes, Escaped),
    append(Escaped, [34], Codes).
json_render_value(json_object(Pairs), Codes) :-
    json_render_object_members(Pairs, Members),
    append([123|Members], [125], Codes).
json_render_value(json_array(Values), Codes) :-
    json_render_array_members(Values, Members),
    append([91|Members], [93], Codes).
json_render_value(json_null, [110, 117, 108, 108]).
json_render_value(json_true, [116, 114, 117, 101]).
json_render_value(json_false, [102, 97, 108, 115, 101]).

json_render_object_members([], []).
json_render_object_members([Key-Value|Pairs], Codes) :-
    json_render_value(json_string(Key), KeyCodes),
    json_render_value(Value, ValueCodes),
    append(KeyCodes, [58|ValueCodes], Field),
    json_render_object_tail(Pairs, Tail),
    append(Field, Tail, Codes).

json_render_object_tail([], []).
json_render_object_tail(Pairs, [44|Codes]) :-
    json_render_object_members(Pairs, Codes).

json_render_array_members([], []).
json_render_array_members([Value|Values], Codes) :-
    json_render_value(Value, ValueCodes),
    json_render_array_tail(Values, Tail),
    append(ValueCodes, Tail, Codes).

json_render_array_tail([], []).
json_render_array_tail(Values, [44|Codes]) :-
    json_render_array_members(Values, Codes).

json_render_string_codes([], []).
json_render_string_codes([Code|Codes], Escaped) :-
    json_render_string_code(Code, Head),
    json_render_string_codes(Codes, Tail),
    append(Head, Tail, Escaped).

json_render_string_code(34, [92, 34]) :- !.
json_render_string_code(92, [92, 92]) :- !.
json_render_string_code(8, [92, 98]) :- !.
json_render_string_code(9, [92, 116]) :- !.
json_render_string_code(10, [92, 110]) :- !.
json_render_string_code(12, [92, 102]) :- !.
json_render_string_code(13, [92, 114]) :- !.
json_render_string_code(Code, [Code]) :-
    Code >= 33, Code =< 126, !.
json_render_string_code(Code, Escaped) :-
    Code >= 0, Code =< 65535, !,
    json_render_unicode_escape(Code, Escaped).
json_render_string_code(Code, Escaped) :-
    Code =< 1114111,
    Plane is Code - 65536,
    High is 55296 + Plane // 1024,
    Low is 56320 + Plane mod 1024,
    json_render_unicode_escape(High, HighCodes),
    json_render_unicode_escape(Low, LowCodes),
    append(HighCodes, LowCodes, Escaped).

json_render_unicode_escape(Code, [92, 117, A, B, C, D]) :-
    AValue is (Code // 4096) mod 16,
    BValue is (Code // 256) mod 16,
    CValue is (Code // 16) mod 16,
    DValue is Code mod 16,
    json_render_hex_code(AValue, A),
    json_render_hex_code(BValue, B),
    json_render_hex_code(CValue, C),
    json_render_hex_code(DValue, D).

json_render_hex_code(Value, Code) :-
    Value < 10, !, Code is Value + 48.
json_render_hex_code(Value, Code) :- Code is Value + 87.

string_value_surface(Text, Surface) :-
    ( text(Text)
    -> text_string(Text, Surface)
    ;  text_string(Surface, Text)
    ).

canonical_text_surface(Text, Surface) :-
    ( ground(Surface)
    -> text_string(Surface, Text)
    ;  Surface = Text
    ).

kind_syntax(boolean, boolean).
kind_syntax(integer, integer).
kind_syntax(string, string).
kind_syntax(path, path).
kind_syntax(base64, base64).
kind_syntax(oci_spec, structured_spec).
kind_syntax(api_spec, structured_spec).
kind_syntax(spec, spec).

%! context_plan(+Items, +Mode, -Plan) is nondet.
%
% Relate a structural parse to its explicit external dependencies. Primitive
% wire parsing does not perform the lookup; the plan records what must be true.

context_plan(Items, Mode,
             plan(Command, Queries, Bindings, Evidence, Preference)) :-
    relation_plan(Items, Mode,
                  plan(Command, Queries, Bindings, Evidence, Preference), _).

relation_plan(Items, Mode,
              plan(Command, Queries, Bindings, Evidence, Preference),
              CompletionTargets) :-
    parse(Items, Mode,
          parse_result(Command, Status, Evidence, Preference)),
    Command = command(Action, _, _, _),
    context_queries(Action, Mode, Evidence, 1, 1, [], Queries, Bindings,
                    CompletionTargets),
    valid_query_graph(Queries),
    plan_status_matches(Mode, Status).

plan_status_matches(exact, complete).
plan_status_matches(assist(Id), incomplete(edit(Id))).

context_queries(_, _, [], _, _, _, [], [], []).
context_queries(Action, Mode, [Evidence|EvidenceItems], QueryIndex, ArgIndex,
                KnownArguments, Queries, Bindings, CompletionTargets) :-
    Evidence = evidence(_, Span, _, Surface, _, Description, _, Origin),
    ( argument_name(Action, Description)
    -> ( argument_context(Action, Description, Domain, Scope)
       -> query_id(QueryIndex, Id),
          context_argument_dependency(
              Mode, Origin, Description, Domain, Scope, KnownArguments,
              Surface, Span, Id, ArgIndex, Query, Binding, Target),
          Queries = [query(Id, Query)|RestQueries],
          optional_head(Binding, Bindings, RestBindings),
          optional_head(Target, CompletionTargets, RestTargets),
          NextQuery is QueryIndex + 1,
          NextKnown = [Description-Id|KnownArguments]
       ;  Queries = RestQueries,
          Bindings = RestBindings,
          CompletionTargets = RestTargets,
          NextQuery = QueryIndex,
          NextKnown = KnownArguments
       ),
       NextArg is ArgIndex + 1
    ;  Queries = RestQueries,
       Bindings = RestBindings,
       CompletionTargets = RestTargets,
       NextQuery = QueryIndex,
       NextArg = ArgIndex,
       NextKnown = KnownArguments
    ),
    context_queries(Action, Mode, EvidenceItems, NextQuery, NextArg,
                    NextKnown, RestQueries, RestBindings, RestTargets).

context_argument_dependency(
    assist(EditId), tear(EditId, argument(Name, Kind)), Name, Domain, Scope,
    Known, Surface, Span, Id, _ArgIndex, ask(all, Domain, Selector), none,
    some(completion_target(EditId, Id, Span, Surface, Domain, Kind))) :-
    !,
    context_completion_selector(Scope, Known, Surface, Selector).
context_argument_dependency(
    _Mode, _Origin, _Name, Domain, Scope, Known, Surface, _Span, Id, ArgIndex,
    ask(one, Domain, Selector), some(bind(Id, arg(ArgIndex), entry_value)),
    none) :-
    context_selector(Scope, Known, Surface, Selector).

optional_head(none, Tail, Tail).
optional_head(some(Head), [Head|Tail], Tail).

argument_name(Action, Name) :-
    argument_schema(Action, Schema),
    schema_has_name(Schema, Name).

schema_has_name([arg(Name, _, _, _)|_], Name).
schema_has_name([_|Schema], Name) :- schema_has_name(Schema, Name).

context_selector(root, _, Surface, name(Surface)).
context_selector(within(Template), Known, Surface,
                 within(Resolved, name(Surface))) :-
    resolve_argument_refs(Template, Known, Resolved).

resolve_argument_refs(argument(Name), Known, ref(Id)) :-
    list_pair(Name, Id, Known), !.
resolve_argument_refs(Term, _, Term) :- atomic(Term), !.
resolve_argument_refs(Term, Known, Resolved) :-
    Term =.. [Functor|Args],
    resolve_argument_ref_list(Args, Known, ResolvedArgs),
    Resolved =.. [Functor|ResolvedArgs].

resolve_argument_ref_list([], _, []).
resolve_argument_ref_list([Arg|Args], Known, [Resolved|ResolvedArgs]) :-
    resolve_argument_refs(Arg, Known, Resolved),
    resolve_argument_ref_list(Args, Known, ResolvedArgs).

list_pair(Name, Id, [Name-Id|_]).
list_pair(Name, Id, [_|Pairs]) :- list_pair(Name, Id, Pairs).

resolve_context_plan(plan(command(Action, Handler, Target, Args0), Queries,
                          Bindings, _Evidence, _Preference),
                     Observations,
                     command(Action, Handler, Target, Args)) :-
    resolve_bindings(Bindings, Queries, Observations, Args0, Args).

resolve_bindings([], _, _, Args, Args).
resolve_bindings([bind(Id, arg(Index), entry_value)|Bindings], Queries,
                 Observations, Args0, Args) :-
    list_query(Id, Query, Queries),
    list_observation(Id, Query, Observations, Value),
    replace_nth(Index, Args0, Value, Args1),
    resolve_bindings(Bindings, Queries, Observations, Args1, Args).

list_query(Id, Query, [query(Id, Query)|_]).
list_query(Id, Query, [_|Queries]) :- list_query(Id, Query, Queries).

list_observation(Id, Query,
                 [observed(Id, Query, _,
                           some(one(entry(_, _, _, Value, _))))|_], Value).
list_observation(Id, Query, [_|Observations], Value) :-
    list_observation(Id, Query, Observations, Value).

replace_nth(1, [_|Values], Value, [Value|Values]) :- !.
replace_nth(Index, [Head|Values], Value, [Head|Result]) :-
    Index > 1, Next is Index - 1,
    replace_nth(Next, Values, Value, Result).

context_completion_plan(
    Items, EditId,
    completion_context(Action, Span, Surface, Queries, TargetId,
                       Preference)) :-
    relation_plan(
        Items, assist(EditId),
        plan(command(Action, _, _, _), Queries, _Bindings, _Evidence,
             Preference),
        Targets),
    action(Action, _, _, _, _, visible, _),
    list_completion_target(EditId, Targets, TargetId, Span, Surface).

list_completion_target(
    EditId,
    [completion_target(EditId, TargetId, Span, Surface, _Domain, _Kind)|_],
    TargetId, Span, Surface).
list_completion_target(EditId, [_|Targets], TargetId, Span, Surface) :-
    list_completion_target(EditId, Targets, TargetId, Span, Surface).

context_completion_selector(root, _, Surface, prefix(Surface)).
context_completion_selector(within(Template), Known, Surface,
                            within(Resolved, prefix(Surface))) :-
    resolve_argument_refs(Template, Known, Resolved).

resolve_context_completion(
    completion_context(Action, Span, Surface, Queries, TargetId, Preference),
    Observations, Completions) :-
    list_query(TargetId, Query, Queries),
    context_all_observation(TargetId, Query, Observations, Source, Entries),
    Source = source(Provider, _),
    resolve_query_refs(Query, Observations, ResolvedQuery),
    findall(completion_key(Span, Name)-
                (alternative(context(Action, Domain, Identity),
                             context_argument, Provider)-Preference),
            ( context_tear_match(
                  ResolvedQuery, snapshot(Source, Entries), Surface, Name,
                  _ExactQuery,
                  entry(Domain, Identity, _Names, _Value, _Attributes))
            ),
            Pairs),
    project_completions(Pairs, Completions).

context_all_observation(
    Id, Query,
    [observed(Id, Query, Source, some(all(Entries)))|_], Source, Entries).
context_all_observation(Id, Query, [_|Observations], Source, Entries) :-
    context_all_observation(Id, Query, Observations, Source, Entries).

query_id(1, q1).
query_id(2, q2).
query_id(3, q3).
query_id(4, q4).
query_id(5, q5).
query_id(6, q6).
query_id(7, q7).
query_id(8, q8).

%! completions(+Items, +EditTearId, -Completions) is det.

completions(Items, EditId, Completions) :-
    findall(Visible-(Alternative-Preference),
            completion_witness(Items, EditId, Visible, Alternative,
                               Preference),
            Pairs),
    project_completions(Pairs, Completions).

completion_witness(
    Items, EditId, completion_key(Span, Text),
    alternative(Semantic, Syntax, Description), Preference) :-
    parse(Items, assist(EditId),
          parse_result(command(Action, _, _, _), incomplete(edit(EditId)),
                       Evidence, Preference)),
    action(Action, _, _, _, _, visible, _),
    literal_completion_evidence(EditId, Evidence, Span, Text, Semantic,
                                Syntax, Description).

text_string(Text, Text) :- string(Text), !.
text_string(Text, String) :- atom_string(Text, String).

%! highlights(+ParseResult, -Highlights) is det.

highlights(parse_result(_Command, _Status, Evidence, _Preference), Highlights) :-
    project_highlights(Evidence, Highlights).

%! render(+Command, -Text) is semidet.

render(command(Action, Handler, Target, WireArgs), Text) :-
    action(Action, Handler, Target, _, _, _, _),
    denormalize_args(Projection, WireArgs, SourceArgs),
    form_relation(Action, RenderedItems, render, Projection, SourceArgs,
                  _Evidence, 0),
    rendered_parts(RenderedItems, Parts),
    join_parts(Parts, Text).

rendered_parts([], []).
rendered_parts([rendered(Text)|Items], [Text|Parts]) :-
    rendered_parts(Items, Parts).

join_parts([], "").
join_parts([Part], Part).
join_parts([Part|Parts], Text) :-
    Parts = [_|_],
    join_parts(Parts, Rest),
    string_concat(Part, " ", Prefix),
    string_concat(Prefix, Rest, Text).

%! catalog(+Visibility, -Rows) is det.

catalog(Visibility, Rows) :-
    findall(action_info(Action, Handler, Target, Schema, Notation,
                        Description, RowVisibility, Preference,
                        Representations),
            ( action(Action, Handler, Target, Notation, Description,
                     RowVisibility, Preference),
              visibility_matches(Visibility, RowVisibility),
              argument_schema(Action, Schema),
              findall(representation(Kind, Value),
                      representation(Action, Kind, Value),
                      Representations)
            ),
            Rows).

%! action_help(+Target, -Rows) is det.
%
% Closed help projection used by runtime consumers. Keeping this operation
% narrow avoids shipping executable syntax terms merely to display the three
% related fields.

action_help(Target, Rows) :-
    action_help(Target, "", Rows).

%! action_help(+Target, +Filter, -Rows) is det.
%
% Filtering is part of the relation projection, rather than a consumer-side
% reinterpretation of action metadata. The empty filter projects every row.

action_help(Target, Filter, Rows) :-
    findall(record(CommandText, Notation, Description),
            ( action(Action, _, RowTarget, Notation, Description, _, _),
              help_target_matches(Target, RowTarget),
              action_words(Action, Words),
              join_parts(Words, CommandText),
              help_filter_matches(Filter, Action, CommandText, Description)
            ),
            Rows).

help_target_matches(all, _).
help_target_matches(Target, Target) :- Target \== all.

help_filter_matches("", _, _, _).
help_filter_matches(Filter, Action, CommandText, Description) :-
    Filter \== "",
    atom_string(Action, ActionText),
    ( sub_string(ActionText, _, _, _, Filter)
    ; sub_string(CommandText, _, _, _, Filter)
    ; sub_string(Description, _, _, _, Filter)
    ), !.

visibility_matches(all, _).
visibility_matches(visible, visible).
visibility_matches(internal, internal).

%! application(+Operation, +InputString, -OutputString) is det.

application(Operation, InputString, OutputString) :-
    ( atom(Operation)
    -> application_atom(Operation, InputString, Response)
    ;  Response = error(invalid_operation)
    ),
    term_string(Response, OutputString,
                [quoted(true), ignore_ops(true), numbervars(true)]).

application_atom(Operation, InputString, Response) :-
    ( decode_request(InputString, Request)
    -> dispatch_application(Operation, Request, Response)
    ;  Response = error(invalid_request)
    ).

decode_request(InputString, Request) :-
    string(InputString),
    catch(term_string(Request, InputString, [syntax_errors(error)]), _, fail),
    ground(Request).

dispatch_application(parse, request(Items, Mode), ok(Results)) :-
    !, findall(Result, parse(Items, Mode, Result), Results).
dispatch_application(complete, request(Items, EditId), ok(Completions)) :-
    !, completions(Items, EditId, Completions).
dispatch_application(highlights, request(Result), ok(Highlights)) :-
    !, ( highlights(Result, Highlights) -> true ; Highlights = [] ).
dispatch_application(render, request(Command), Response) :-
    !, ( render(Command, Text)
       -> Response = ok(Text)
       ;  Response = error(no_solution)
       ).
dispatch_application(catalog, request(Visibility), ok(Rows)) :-
    !, catalog(Visibility, Rows).
dispatch_application(action_help, request(Target), Response) :-
    !, ( ( Target == all ; Target == ui ; Target == control ; Target == local )
       -> action_help(Target, Rows), Response = ok(Rows)
       ;  Response = error(invalid_request)
       ).
dispatch_application(action_help, request(Target, Filter), Response) :-
    !, ( ( Target == all ; Target == ui ; Target == control ; Target == local ),
         string(Filter)
       -> action_help(Target, Filter, Rows), Response = ok(Rows)
       ;  Response = error(invalid_request)
       ).
dispatch_application(convert, request(FromKind, From, ToKind), ok(Results)) :-
    !, findall(To, convert(FromKind, From, ToKind, To), Results).
dispatch_application(context_plan, request(Items, Mode), ok(Plans)) :-
    !, findall(Plan, context_plan(Items, Mode, Plan), Plans).
dispatch_application(context_resolve, request(Plan, Observations), Response) :-
    !, ( resolve_context_plan(Plan, Observations, Command)
       -> Response = ok(Command)
       ;  Response = error(no_solution)
       ).
dispatch_application(context_completion, request(Items, EditId), ok(Plans)) :-
    !, findall(Plan, context_completion_plan(Items, EditId, Plan), Plans).
dispatch_application(context_completion_resolve,
                     request(Plan, Observations), ok(Completions)) :-
    !, ( resolve_context_completion(Plan, Observations, Completions)
       -> true
       ;  Completions = []
       ).
dispatch_application(action_request, request(Command), Response) :-
    !, ( action_request(Command, Request)
       -> Response = ok(Request)
       ;  Response = error(no_solution)
       ).
dispatch_application(parse, _, error(invalid_request)) :- !.
dispatch_application(complete, _, error(invalid_request)) :- !.
dispatch_application(highlights, _, error(invalid_request)) :- !.
dispatch_application(render, _, error(invalid_request)) :- !.
dispatch_application(catalog, _, error(invalid_request)) :- !.
dispatch_application(action_help, _, error(invalid_request)) :- !.
dispatch_application(convert, _, error(invalid_request)) :- !.
dispatch_application(context_plan, _, error(invalid_request)) :- !.
dispatch_application(context_resolve, _, error(invalid_request)) :- !.
dispatch_application(context_completion, _, error(invalid_request)) :- !.
dispatch_application(context_completion_resolve, _, error(invalid_request)) :- !.
dispatch_application(action_request, _, error(invalid_request)) :- !.
dispatch_application(_, _, error(invalid_operation)).
