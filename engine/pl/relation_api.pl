:- module(relation_api, [transform/2]).

:- use_module(grammar_engine).
:- use_module(grammar_store).

/** <module> Single bounded grammar-engine boundary */

% Whole-document grammar composition is deliberately bounded, but the bound
% must cover syntax, state, and projection relations in one request.  The Brush
% backward-completion acceptance case takes just over 100,000 inferences; keep
% substantial headroom here while request byte/depth/evidence limits provide
% the tighter resource bounds.
relation_inference_limit(5000000).

transform(Request, Reply) :-
    relation_inference_limit(InferenceLimit),
    call_with_inference_limit(transform_request(Request, Candidate),
                              InferenceLimit,
                              LimitResult),
    ( LimitResult == inference_limit_exceeded
    -> Reply = reply([], [], [], [diagnostic(inference_limit_exceeded)])
    ;  Reply = Candidate
    ).

transform_request(
    request(GrammarReference, given(Given), want(Wanted), observations(Observations),
            Limits),
    Reply) :-
    valid_envelope(Given, Wanted, Observations, Limits),
    resolve_grammar(GrammarReference, Grammar),
    !,
    ( transform_relation(Grammar, Given, Wanted, Observations, Limits, Reply)
    -> true
    ;  Reply = reply([], [], [], [diagnostic(no_solution)])
    ).
transform_request(_, reply([], [], [], [diagnostic(invalid_request)])).

valid_envelope(Given, Wanted, Observations,
               limits(MaxSolutions, MaxEvidence, MaxOutputBytes)) :-
    proper_list(Given),
    proper_list(Wanted),
    Wanted = [_|_],
    proper_atom_list(Wanted),
    all_unique(Wanted),
    proper_list(Observations),
    integer(MaxSolutions), MaxSolutions > 0, MaxSolutions =< 1024,
    integer(MaxEvidence), MaxEvidence >= 0, MaxEvidence =< 65536,
    integer(MaxOutputBytes), MaxOutputBytes > 0,
    MaxOutputBytes =< 16777216.

proper_atom_list([]).
proper_atom_list([Value|Values]) :-
    atom(Value),
    proper_atom_list(Values).

all_unique(Values) :-
    sort(Values, Unique),
    length(Values, Count),
    length(Unique, Count).

proper_list([]).
proper_list([_|Values]) :- proper_list(Values).
