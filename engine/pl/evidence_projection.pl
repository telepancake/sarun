:- module(evidence_projection,
          [ literal_completion_evidence/7,
            project_completions/2,
            project_highlights/2
          ]).

/** <module> Grammar-independent evidence projections */

literal_completion_evidence(
    EditId,
    [evidence(Semantic, Span, _, _, Syntax, Description, _,
              tear(EditId, literal(Text)))|_],
    Span, Text, Semantic, Syntax, Description).
literal_completion_evidence(EditId, [_|Evidence], Span, Text, Semantic, Syntax,
                            Description) :-
    literal_completion_evidence(EditId, Evidence, Span, Text, Semantic, Syntax,
                                Description).

%! project_completions(+Pairs, -Completions) is det.
%
% `Pairs` are `completion_key(Span, Surface)-(Alternative-Preference)` parse
% witnesses. Grouping identical visible edits, retaining semantic ambiguity,
% choosing best scores, deterministic ordering, and rank assignment are shared
% engine behavior for every source representation.

project_completions([], []).
project_completions(Pairs, Completions) :-
    keysort(Pairs, SortedPairs),
    group_visible_pairs(SortedPairs, Candidates),
    sort_candidates(Candidates, Sorted),
    rank_completions(Sorted, 1, Completions).

group_visible_pairs([], []).
group_visible_pairs([Visible-Value|Pairs],
                    [candidate(Visible, Alternatives, Preference)|Candidates]) :-
    take_visible_pairs(Pairs, Visible, [Value], Values, Rest),
    merge_alternatives(Values, Alternatives, Preference),
    group_visible_pairs(Rest, Candidates).

take_visible_pairs([Visible-Value|Pairs], Visible, Values0, Values, Rest) :- !,
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
                       Best0, Best, Rest) :- !,
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
pair_values([_-Value|Pairs], [Value|Values]) :- pair_values(Pairs, Values).

rank_completions([], _, []).
rank_completions([candidate(completion_key(Span, Text), Alternatives,
                            Preference)|Candidates], Rank,
                 [completion(Span, Text, Alternatives, Preference, Rank)|Rest]) :-
    NextRank is Rank + 1,
    rank_completions(Candidates, NextRank, Rest).

project_highlights(Evidence, Highlights) :-
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
