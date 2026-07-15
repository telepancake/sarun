:- module(relation_api, [transform/2]).

:- use_module(grammar_engine).

/** <module> Single bounded grammar-engine boundary */

transform(
    request(Grammar, given(Given), want(Wanted), observations(Observations),
            Limits),
    Reply) :-
    valid_envelope(Given, Wanted, Observations, Limits),
    !,
    ( transform_relation(Grammar, Given, Wanted, Observations, Limits, Reply)
    -> true
    ;  Reply = reply([], [], [], [diagnostic(no_solution)])
    ).
transform(_, reply([], [], [], [diagnostic(invalid_request)])).

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
