:- module(grammar_store, [install_grammar/2, resolve_grammar/2]).

/** <module> Install-once immutable grammar handles

Handles cross the engine/client boundary; grammar trees do not. Installation
is a startup composition operation. Reinstalling an identical value is
idempotent, while changing the value behind an existing handle fails.
*/

:- dynamic installed_grammar/2.

install_grammar(Handle, Grammar) :-
    atom(Handle),
    ground(Grammar),
    ( installed_grammar(Handle, Existing)
    -> Existing =@= Grammar
    ;  assertz(installed_grammar(Handle, Grammar))
    ).

resolve_grammar(grammar_handle(Handle), Grammar) :-
    atom(Handle),
    installed_grammar(Handle, Grammar).
resolve_grammar(Grammar, Grammar) :-
    Grammar \= grammar_handle(_),
    ground(Grammar).
