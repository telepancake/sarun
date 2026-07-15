:- module(action_grammar,
          [ action/7,
            wire_handler/2,
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
            valid_transport_catalog/0,
            representation/3,
            convert/4,
            valid_action/1,
            parse/2,
            parse/3,
            completions/3,
            highlights/2,
            render/3,
            catalog/2,
            context_plan/3,
            resolve_context_plan/3,
            context_completion_plan/3,
            resolve_context_completion/3,
            application/3
          ]).

:- use_module(action_catalog).
:- use_module(context_relation).
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
    once(action_form(Action, verb, _, _)).

valid_action_wire(local, Handler) :- \+ wire_handler(Handler, _).
valid_action_wire(ui, Handler) :- valid_wire_handler(Handler).
valid_action_wire(control, Handler) :- valid_wire_handler(Handler).

valid_wire_handler(Handler) :-
    wire_handler(Handler, Code),
    integer(Code),
    Code > 0,
    argument_schema(Handler, Schema),
    valid_schema(Schema).

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
valid_kind(spec).

valid_cardinality(required).
valid_cardinality(optional).
valid_cardinality(repeated).

valid_shape(required, scalar).
valid_shape(optional, scalar).
valid_shape(repeated, array).
valid_shape(repeated, spread).

%! action_form(?Action, ?Style, ?Specs, ?Normalizer) is nondet.
%
% A form is a sequence of `literal/5` and `argument/1` specs. Normalizers
% relate source-form arguments to the handler's typed wire arguments.

action_form(Action, verb, Specs, Normalizer) :-
    action(Action, _, _, _, _, _, _),
    argument_schema(Action, Schema),
    atom_string(Action, Text),
    canonical_normalizer(Action, Normalizer),
    schema_specs(Schema, ArgSpecs),
    Specs = [literal(Action, Text, action_identifier, Action, 30)|ArgSpecs].
action_form(Action, cli, Specs, Normalizer) :-
    cli_form(Action, Words, Normalizer),
    cli_source_schema(Action, Normalizer, Schema),
    cli_literal_specs(Words, Action, Literals),
    schema_specs(Schema, ArgSpecs),
    append(Literals, ArgSpecs, Specs).

canonical_normalizer(mirror_resume, resume_false) :- !.
canonical_normalizer(_, identity).

cli_source_schema(mirror_pause, pause_true,
                  [arg(id, integer, required, scalar)]) :- !.
cli_source_schema(Action, _, Schema) :-
    argument_schema(Action, Schema).

schema_specs([], []).
schema_specs([Arg|Schema], [argument(Arg)|Specs]) :-
    schema_specs(Schema, Specs).

cli_literal_specs(Words, Action, Specs) :-
    cli_literal_specs(Words, Action, first, Specs).

cli_literal_specs([], _, _, []).
cli_literal_specs([Text|Words], Action, Position,
                  [literal(Semantic, Text, Syntax, Action, Preference)|Specs]) :-
    atom_string(Semantic, Text),
    cli_literal_metadata(Position, Syntax, Preference),
    cli_literal_specs(Words, Action, rest, Specs).

cli_literal_metadata(first, command_namespace, 10).
cli_literal_metadata(rest, action_word, 20).

%! representation(?Action, ?Kind, ?Value) is nondet.
%
% Every public projection is rooted in the normalized action facts and the
% same form relation used by parsing and rendering.  `syntax(Style)` exposes
% the exact executable spec rather than reconstructing usage text elsewhere.

representation(Action, action, Action) :-
    action(Action, _, _, _, _, _, _).
representation(Action, verb, verb(Text, Normalizer)) :-
    action_form(Action, verb,
                [literal(_, Text, _, _, _)|_], Normalizer).
representation(Action, cli, cli(Words, Normalizer)) :-
    action_form(Action, cli, Specs, Normalizer),
    form_literal_prefix(Specs, Words).
representation(Action, syntax(Style), syntax(Specs)) :-
    action_form(Action, Style, Specs, _).
representation(Action, wire, wire(Code, Handler, Target, Schema)) :-
    action(Action, Handler, Target, _, _, _, _),
    wire_handler(Handler, Code),
    argument_schema(Handler, Schema).
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

%! parse(+Items, -Result) is nondet.

parse(Items, Result) :-
    parse(Items, exact, Result).

%! parse(+Items, +Mode, -Result) is nondet.

parse(Items, Mode,
      parse_result(command(Action, Handler, Target, WireArgs), Status,
                   Evidence, Preference)) :-
    input_body(Items, Body, End),
    valid_body(Body, End),
    valid_mode(Mode),
    action(Action, Handler, Target, _, _, _, ActionPreference),
    valid_action(Action),
    form_relation(Action, _Style, Body, Mode, Normalizer, SourceArgs,
                  Evidence, EditCount),
    normalize_args(Normalizer, SourceArgs, WireArgs),
    parse_status(Mode, EditCount, Status),
    evidence_preference(Evidence, ActionPreference, Preference).

valid_mode(exact).
valid_mode(assist(_)).

parse_status(exact, 0, complete).
parse_status(assist(EditId), 1, incomplete(edit(EditId))).

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

input_body([end(End)], [], End) :-
    integer(End),
    End >= 0.
input_body([Item|Items], [Item|Body], End) :-
    input_body(Items, Body, End).

valid_body(Body, End) :-
    valid_items(Body, 0, End).

valid_items([], PreviousStop, End) :-
    PreviousStop =< End.
valid_items([Item|Items], PreviousStop, End) :-
    item_span(Item, span(Start, Stop)),
    Start >= PreviousStop,
    valid_item(Item, End),
    valid_items(Items, Stop, End).

item_span(unit(_, Span, _, _, _, _, _, _), Span).
item_span(edit_tear(_, Span, _), Span).
item_span(source_tear(_, Span, _), Span).

valid_item(unit(_, span(Start, Stop), PaintSpans, Surface, _, _, Preference, _),
           End) :-
    valid_span(span(Start, Stop), End),
    proper_list(PaintSpans),
    valid_paint_spans(PaintSpans, Start, Start, Stop),
    text(Surface),
    number(Preference).
valid_item(edit_tear(_, Span, Surface), End) :-
    valid_span(Span, End),
    text(Surface).
valid_item(source_tear(_, Span, Surface), End) :-
    valid_span(Span, End),
    text(Surface).

valid_paint_spans([], _, _, _).
valid_paint_spans([span(Start, Stop)|Spans], PreviousStop,
                  OwnerStart, OwnerStop) :-
    integer(Start),
    integer(Stop),
    OwnerStart =< Start,
    PreviousStop =< Start,
    Start =< Stop,
    Stop =< OwnerStop,
    valid_paint_spans(Spans, Stop, OwnerStart, OwnerStop).

valid_span(span(Start, Stop), End) :-
    integer(Start),
    integer(Stop),
    0 =< Start,
    Start =< Stop,
    Stop =< End.

proper_list([]).
proper_list([_|Items]) :-
    proper_list(Items).

text(Text) :- string(Text), !.
text(Text) :- atom(Text).

% Keep the embedded application core-only. SWI's boot image provides append/1
% but not library(lists)' append/3.
append([], Tail, Tail).
append([Head|Items], Tail, [Head|Result]) :-
    append(Items, Tail, Result).

source_unit(unit(_, Span, PaintSpans, Surface, _, _, Preference, Origin),
            Span, PaintSpans, Surface, Preference, Origin).

% Singular execution relation for an action form.  Concrete source units,
% edit tears, and rendered surfaces differ only at the terminal relation;
% sequence, cardinality, normalization inputs, and end-of-form behavior are
% shared by parsing and rendering.
form_relation(Action, Style, Items, Mode, Normalizer, SourceArgs, Evidence,
              EditCount) :-
    action_form(Action, Style, Specs, Normalizer),
    relate_specs(Specs, Items, Mode, SourceArgs, Evidence, EditCount).

relate_specs([], [], _Mode, [], [], 0).
% Once an edit tear has been consumed by an enclosing call, the ordinary
% parser may end with an expected continuation.  The missing typed arguments
% remain explicit holes in the incomplete command; concrete input to the
% right of a tear is never skipped and must still parse normally.
relate_specs(Specs, [], assist(_), Args, [], 0) :-
    specs_require_input(Specs),
    missing_source_args(Specs, Args).
relate_specs([literal(Semantic, Text, Syntax, Description, LitPreference)|Specs],
            [Item|Items], Mode, Args,
            [EvidenceItem|Evidence], EditCount) :-
    match_literal_item(
        literal(Semantic, Text, Syntax, Description, LitPreference),
        Item, Mode, EvidenceItem, ItemEditCount),
    relate_specs(Specs, Items, Mode, Args, Evidence, RestCount),
    EditCount is RestCount + ItemEditCount.
relate_specs([argument(arg(Name, Kind, required, scalar))|Specs],
            [Item|Items], Mode, [Value|Args], [EvidenceItem|Evidence],
            EditCount) :-
    match_argument_item(Name, Kind, Item, Mode, Value, EvidenceItem,
                        ItemEditCount),
    relate_specs(Specs, Items, Mode, Args, Evidence, RestCount),
    EditCount is RestCount + ItemEditCount.
relate_specs([argument(arg(Name, Kind, optional, scalar))|Specs],
            [Item|Items], Mode, [Value|Args], [EvidenceItem|Evidence],
            EditCount) :-
    match_argument_item(Name, Kind, Item, Mode, Value, EvidenceItem,
                        ItemEditCount),
    relate_specs(Specs, Items, Mode, Args, Evidence, RestCount),
    EditCount is RestCount + ItemEditCount.
relate_specs([argument(arg(_, _, optional, scalar))|Specs], Items, Mode,
            Args, Evidence, EditCount) :-
    relate_specs(Specs, Items, Mode, Args, Evidence, EditCount).
relate_specs([argument(arg(Name, Kind, repeated, Shape))|Specs], Items0, Mode,
            Args, Evidence, EditCount) :-
    repeated_arguments(Shape, Values, Specs, RepeatedArgs),
    append(RepeatedArgs, RestArgs, Args),
    relate_repeated_items(Name, Kind, Values, Items0, Mode, RepeatedEvidence,
                          RepeatedEditCount, Items),
    relate_specs(Specs, Items, Mode, RestArgs, RestEvidence, RestEditCount),
    append(RepeatedEvidence, RestEvidence, Evidence),
    EditCount is RepeatedEditCount + RestEditCount.

match_literal_item(
    literal(Semantic, Text, Syntax, Description, LitPreference), Item, Mode,
    evidence(Semantic, Span, PaintSpans, Surface, Syntax, Description,
             Preference, Origin), 0) :-
    source_mode(Mode),
    source_unit(Item, Span, PaintSpans, Surface, SourcePreference, Origin),
    text_string(Surface, SurfaceString),
    SurfaceString = Text,
    Preference is SourcePreference + LitPreference.
match_literal_item(
    literal(Semantic, Text, Syntax, Description, LitPreference),
    edit_tear(EditId, Span, Surface), assist(EditId),
    evidence(Semantic, Span, [], Surface, Syntax, Description, LitPreference,
             tear(EditId, literal(Text))), 1) :-
    surface_prefix(Surface, Text).
match_literal_item(literal(_, Text, _, _, _), rendered(Text), render,
                   rendered, 0).

match_argument_item(Name, Kind, Item, Mode, Value,
                    evidence(Value, Span, PaintSpans, Surface, Syntax,
                             Name, Preference, Origin), 0) :-
    source_mode(Mode),
    source_unit(Item, Span, PaintSpans, Surface, SourcePreference, Origin),
    argument_surface(Kind, Value, Surface),
    kind_syntax(Kind, Syntax),
    Preference is SourcePreference + 10.
match_argument_item(Name, Kind, edit_tear(EditId, Span, Surface),
                    assist(EditId), hole(Name, Kind),
                    evidence(hole(Name, Kind), Span, [], Surface, Syntax,
                             Name, 10,
                             tear(EditId, argument(Name, Kind))), 1) :-
    kind_syntax(Kind, Syntax).
match_argument_item(_Name, Kind, rendered(Surface), render, Value, rendered,
                    0) :-
    argument_surface(Kind, Value, Surface).

source_mode(exact).
source_mode(assist(_)).

relate_repeated_items(_, _, [], Items, _Mode, [], 0, Items).
relate_repeated_items(Name, Kind, [Value|Values], [Item|Items0], Mode,
                      [Evidence|EvidenceItems], EditCount, Items) :-
    match_argument_item(Name, Kind, Item, Mode, Value, Evidence,
                        ItemEditCount),
    relate_repeated_items(Name, Kind, Values, Items0, Mode, EvidenceItems,
                          RestEditCount, Items),
    EditCount is ItemEditCount + RestEditCount.

specs_require_input([literal(_, _, _, _, _)|_]).
specs_require_input([argument(arg(_, _, required, scalar))|_]).
specs_require_input([_|Specs]) :-
    specs_require_input(Specs).

missing_source_args([], []).
missing_source_args([literal(_, _, _, _, _)|Specs], Args) :-
    missing_source_args(Specs, Args).
missing_source_args([argument(arg(Name, Kind, required, scalar))|Specs],
                    [hole(Name, Kind)|Args]) :-
    missing_source_args(Specs, Args).
missing_source_args([argument(arg(_, _, optional, scalar))|Specs], Args) :-
    missing_source_args(Specs, Args).
missing_source_args([argument(arg(_, _, repeated, Shape))|Specs], Args) :-
    repeated_arguments(Shape, [], Specs, RepeatedArgs),
    missing_source_args(Specs, RestArgs),
    append(RepeatedArgs, RestArgs, Args).

repeated_arguments(array, Values, _Specs, [array(Values)]) :-
    Values = [_|_].
repeated_arguments(array, [], Specs, [array([])]) :-
    specs_have_argument(Specs).
repeated_arguments(array, [], Specs, []) :-
    \+ specs_have_argument(Specs).
repeated_arguments(spread, Values, _Specs, Values).

specs_have_argument([argument(_)|_]) :- !.
specs_have_argument([_|Specs]) :-
    specs_have_argument(Specs).

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
argument_surface(path, string(Text), Surface) :-
    string_value_surface(Text, Surface).
argument_surface(base64, string(Text), Surface) :-
    string_value_surface(Text, Surface).
argument_surface(spec, string(Text), Surface) :-
    string_value_surface(Text, Surface).

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
kind_syntax(spec, spec).

evidence_preference([], Preference, Preference).
evidence_preference([evidence(_, _, _, _, _, _, ItemPreference, _)|Evidence],
                    Accumulator, Preference) :-
    Next is Accumulator + ItemPreference,
    evidence_preference(Evidence, Next, Preference).

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
    merge_completion_pairs(Pairs, Candidates),
    sort_candidates(Candidates, Sorted),
    rank_completions(Sorted, 1, Completions).

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
    merge_completion_pairs(Pairs, Candidates),
    sort_candidates(Candidates, Sorted),
    rank_completions(Sorted, 1, Completions).

completion_witness(
    Items, EditId, completion_key(Span, Text),
    alternative(Semantic, Syntax, Description), Preference) :-
    parse(Items, assist(EditId),
          parse_result(command(Action, _, _, _), incomplete(edit(EditId)),
                       Evidence, Preference)),
    action(Action, _, _, _, _, visible, _),
    tear_literal_evidence(EditId, Evidence, Span, Text, Semantic, Syntax,
                          Description).

tear_literal_evidence(
    EditId,
    [evidence(Semantic, Span, _, _, Syntax, Description, _,
              tear(EditId, literal(Text)))|_],
    Span, Text, Semantic, Syntax, Description).
tear_literal_evidence(EditId, [_|Evidence], Span, Text, Semantic, Syntax,
                      Description) :-
    tear_literal_evidence(EditId, Evidence, Span, Text, Semantic, Syntax,
                          Description).

surface_prefix(Surface, Text) :-
    text_string(Surface, SurfaceString),
    text_string(Text, TextString),
    sub_string(TextString, 0, _, _, SurfaceString).

text_string(Text, Text) :- string(Text), !.
text_string(Text, String) :- atom_string(Text, String).

merge_completion_pairs([], []).
merge_completion_pairs(Pairs, Candidates) :-
    keysort(Pairs, Sorted),
    group_visible_pairs(Sorted, Candidates).

group_visible_pairs([], []).
group_visible_pairs([Visible-Value|Pairs],
                    [candidate(Visible, Alternatives, Preference)|Candidates]) :-
    take_visible_pairs(Pairs, Visible, [Value], Values, Rest),
    merge_alternatives(Values, Alternatives, Preference),
    group_visible_pairs(Rest, Candidates).

take_visible_pairs([Visible-Value|Pairs], Visible, Values0, Values, Rest) :-
    !,
    take_visible_pairs(Pairs, Visible, [Value|Values0], Values, Rest).
take_visible_pairs(Pairs, _Visible, Values, Values, Pairs).

merge_alternatives(Values, Alternatives, Preference) :-
    alternative_pairs(Values, Pairs),
    keysort(Pairs, Sorted),
    group_alternative_pairs(Sorted, Alternatives, Preference).

alternative_pairs([], []).
alternative_pairs([Alternative-Preference|Values],
                  [Alternative-Preference|Pairs]) :-
    alternative_pairs(Values, Pairs).

group_alternative_pairs([Alternative-Value|Pairs],
                        [Merged|Alternatives], Preference) :-
    take_alternative_pairs(Pairs, Alternative, Value, Best, Rest),
    Alternative = alternative(Semantic, Syntax, Description),
    Merged = alternative(Semantic, Syntax, Description, Best),
    group_alternative_pairs_rest(Rest, Alternatives, RestPreference),
    max_number(Best, RestPreference, Preference).

group_alternative_pairs_rest([], [], -1.0Inf).
group_alternative_pairs_rest([Alternative-Value|Pairs],
                             [Merged|Alternatives], Preference) :-
    take_alternative_pairs(Pairs, Alternative, Value, Best, Rest),
    Alternative = alternative(Semantic, Syntax, Description),
    Merged = alternative(Semantic, Syntax, Description, Best),
    group_alternative_pairs_rest(Rest, Alternatives, RestPreference),
    max_number(Best, RestPreference, Preference).

take_alternative_pairs([Alternative-Value|Pairs], Alternative,
                       Best0, Best, Rest) :-
    !,
    max_number(Best0, Value, Best1),
    take_alternative_pairs(Pairs, Alternative, Best1, Best, Rest).
take_alternative_pairs(Pairs, _Alternative, Best, Best, Pairs).

max_number(A, B, A) :- A >= B, !.
max_number(_A, B, B).

sort_candidates(Candidates, Sorted) :-
    candidate_rank_pairs(Candidates, Pairs),
    keysort(Pairs, RankedPairs),
    pair_values(RankedPairs, Sorted).

candidate_rank_pairs([], []).
candidate_rank_pairs([Candidate|Candidates],
                     [rank_key(Negative, Visible)-Candidate|Pairs]) :-
    Candidate = candidate(Visible, _Alternatives, Preference),
    Negative is -Preference,
    candidate_rank_pairs(Candidates, Pairs).

pair_values([], []).
pair_values([_-Value|Pairs], [Value|Values]) :-
    pair_values(Pairs, Values).

rank_completions([], _, []).
rank_completions([candidate(completion_key(Span, Text), Alternatives,
                            Preference)|Candidates], Rank,
                 [completion(Span, Text, Alternatives, Preference, Rank)|Rest]) :-
    NextRank is Rank + 1,
    rank_completions(Candidates, NextRank, Rest).

%! highlights(+ParseResult, -Highlights) is det.

highlights(parse_result(_Command, _Status, Evidence, _Preference), Highlights) :-
    evidence_highlights(Evidence, Highlights, []).

evidence_highlights([], Highlights, Highlights).
evidence_highlights([evidence(Semantic, _Span, PaintSpans, _Surface, Syntax,
                              _Description, _Preference, Origin)|Evidence],
                    Highlights0, Highlights) :-
    paint_highlights(PaintSpans, Syntax, Semantic, Origin,
                     Highlights0, Highlights1),
    evidence_highlights(Evidence, Highlights1, Highlights).

paint_highlights([], _Syntax, _Semantic, _Origin, Highlights, Highlights).
paint_highlights([PaintSpan|PaintSpans], Syntax, Semantic, Origin,
                 [highlight(PaintSpan, Syntax, Semantic, Origin)|Highlights0],
                 Highlights) :-
    paint_highlights(PaintSpans, Syntax, Semantic, Origin,
                     Highlights0, Highlights).

%! render(+Command, +Style, -Text) is semidet.

render(command(Action, Handler, Target, WireArgs), Style, Text) :-
    action(Action, Handler, Target, _, _, _, _),
    denormalize_args(Normalizer, WireArgs, SourceArgs),
    form_relation(Action, Style, RenderedItems, render, Normalizer, SourceArgs,
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
dispatch_application(render, request(Command, Style), Response) :-
    !, ( render(Command, Style, Text)
       -> Response = ok(Text)
       ;  Response = error(no_solution)
       ).
dispatch_application(catalog, request(Visibility), ok(Rows)) :-
    !, catalog(Visibility, Rows).
dispatch_application(convert, request(FromKind, From, ToKind), ok(Results)) :-
    !, findall(To, convert(FromKind, From, ToKind, To), Results).
dispatch_application(context_query, request(Query, Snapshot), ok(Outcome)) :-
    !, ( context_query(Query, Snapshot, Result)
       -> Outcome = some(Result)
       ;  Outcome = none
       ).
dispatch_application(context_observe, request(Id, Query, Snapshot), ok(Observation)) :-
    !, observe_query(Id, Query, Snapshot, Observation).
dispatch_application(context_ready, request(Graph, Observations), ok(Ready)) :-
    !, ready_queries(Graph, Observations, Ready).
dispatch_application(context_dependencies, request(Observations), ok(Keys)) :-
    !, findall(Key,
               ( context_observation(Observations, Observation),
                 dependency_key(Observation, Key)
               ),
               Keys).
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
dispatch_application(parse, _, error(invalid_request)) :- !.
dispatch_application(complete, _, error(invalid_request)) :- !.
dispatch_application(highlights, _, error(invalid_request)) :- !.
dispatch_application(render, _, error(invalid_request)) :- !.
dispatch_application(catalog, _, error(invalid_request)) :- !.
dispatch_application(convert, _, error(invalid_request)) :- !.
dispatch_application(context_query, _, error(invalid_request)) :- !.
dispatch_application(context_observe, _, error(invalid_request)) :- !.
dispatch_application(context_ready, _, error(invalid_request)) :- !.
dispatch_application(context_dependencies, _, error(invalid_request)) :- !.
dispatch_application(context_plan, _, error(invalid_request)) :- !.
dispatch_application(context_resolve, _, error(invalid_request)) :- !.
dispatch_application(context_completion, _, error(invalid_request)) :- !.
dispatch_application(context_completion_resolve, _, error(invalid_request)) :- !.
dispatch_application(_, _, error(invalid_operation)).

context_observation([Observation|_], Observation).
context_observation([_|Observations], Observation) :-
    context_observation(Observations, Observation).
