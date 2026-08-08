---------------------- MODULE GenerationPublication ----------------------

(***************************************************************************)
(* A bounded publication model.  Candidate construction is private; a      *)
(* reader can observe only the complete generation named by the selector.   *)
(***************************************************************************)

EXTENDS Naturals

CONSTANT Generations

VARIABLES selector, complete, candidate, candidateReceipt, candidateIndex,
          readerGeneration, readerOpen
vars == <<selector, complete, candidate, candidateReceipt, candidateIndex,
           readerGeneration, readerOpen>>

Init ==
    /\ selector = "g0"
    /\ complete = {"g0"}
    /\ candidate = "none"
    /\ candidateReceipt = FALSE
    /\ candidateIndex = FALSE
    /\ readerGeneration = "none"
    /\ readerOpen = FALSE

BeginCandidate ==
    /\ candidate = "none"
    /\ candidate' \in Generations
    /\ candidate' # selector
    /\ candidateReceipt' = FALSE
    /\ candidateIndex' = FALSE
    /\ UNCHANGED <<selector, complete, readerGeneration, readerOpen>>

WriteReceipt ==
    /\ candidate \in Generations
    /\ ~candidateReceipt
    /\ candidateReceipt' = TRUE
    /\ UNCHANGED <<selector, complete, candidate, candidateIndex,
                         readerGeneration, readerOpen>>

WriteIndex ==
    /\ candidate \in Generations
    /\ ~candidateIndex
    /\ candidateIndex' = TRUE
    /\ UNCHANGED <<selector, complete, candidate, candidateReceipt,
                         readerGeneration, readerOpen>>

Commit ==
    /\ candidate \in Generations
    /\ candidateReceipt
    /\ candidateIndex
    /\ selector' = candidate
    /\ complete' = complete \cup {candidate}
    /\ candidate' = "none"
    /\ candidateReceipt' = FALSE
    /\ candidateIndex' = FALSE
    /\ UNCHANGED <<readerGeneration, readerOpen>>

OpenReader ==
    /\ ~readerOpen
    /\ readerOpen' = TRUE
    /\ readerGeneration' = selector
    /\ UNCHANGED <<selector, complete, candidate, candidateReceipt, candidateIndex>>

CloseReader ==
    /\ readerOpen
    /\ readerOpen' = FALSE
    /\ readerGeneration' = "none"
    /\ UNCHANGED <<selector, complete, candidate, candidateReceipt, candidateIndex>>

AbandonCandidate ==
    /\ candidate \in Generations
    /\ candidate' = "none"
    /\ candidateReceipt' = FALSE
    /\ candidateIndex' = FALSE
    /\ UNCHANGED <<selector, complete, readerGeneration, readerOpen>>

Next ==
    \/ BeginCandidate
    \/ WriteReceipt
    \/ WriteIndex
    \/ Commit
    \/ OpenReader
    \/ CloseReader
    \/ AbandonCandidate

TypeOK ==
    /\ selector \in complete
    /\ complete \subseteq Generations
    /\ candidate \in (Generations \cup {"none"})
    /\ readerGeneration \in (Generations \cup {"none"})
    /\ candidate = "none" => /\ ~candidateReceipt /\ ~candidateIndex
    /\ readerOpen <=> readerGeneration # "none"
    /\ readerGeneration # "none" => readerGeneration \in complete

(* The selector never names a partially prepared candidate. *)
PublishedIsComplete ==
    selector \in complete

(* A reader owns a snapshot.  Candidate work cannot change it. *)
ReaderSnapshot ==
    readerOpen => readerGeneration \in Generations

Spec == Init /\ [][Next]_vars

=============================================================================
