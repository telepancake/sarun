:- module(wire_codegen, [emit_manifest/0]).

:- use_module(action_grammar).

/** <module> Canonical build-time projection of the transport relation

This emits a deliberately trivial tab-separated stream whose cells are
canonical Prolog terms.  It is an interchange surface for the Rust source
generator, not another schema: every row is projected directly from the
validated central relation.
*/

emit_manifest :-
    valid_transport_catalog,
    valid_action_catalog,
    forall(wire_protocol_version(Version), emit(version, [Version])),
    forall(wire_limit(Name, Value), emit(limit, [Name, Value])),
    forall(wire_type(Name, Definition), emit(type, [Name, Definition])),
    forall(wire_enum(Type, Case, Code), emit(enum, [Type, Case, Code])),
    forall(wire_variant(Type, Case, Code, Fields),
           emit(variant, [Type, Case, Code, Fields])),
    forall((wire_handler(Handler, Code, Result),
            wire_request_fields(Handler, Fields)),
           emit(action, [Handler, Code, Fields, Result])),
    forall(wire_request(Name, Code, Success, Fields, Fds, Authority),
           emit(request, [Name, Code, Success, Fields, Fds, Authority])),
    forall(wire_response(Name, Code, Fields),
           emit(response, [Name, Code, Fields])),
    forall(wire_mode(Name, Code, Fields),
           emit(mode, [Name, Code, Fields])),
    forall(wire_event(Name, Code, Fields),
           emit(event, [Name, Code, Fields])),
    forall(wire_frame(Stream, Name, Code, Direction, Fields, Fds, Transition),
           emit(frame,
                [Stream, Name, Code, Direction, Fields, Fds, Transition])).

emit(Category, Terms) :-
    write(Category),
    emit_terms(Terms),
    nl.

emit_terms([]).
emit_terms([Term|Terms]) :-
    put_code(9),
    write_canonical(Term),
    emit_terms(Terms).
