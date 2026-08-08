------------------------------ MODULE MirrorRun ------------------------------

(***************************************************************************)
(* A bounded model of the engine-owned mirror run boundary.                *)
(*                                                                        *)
(* This is intentionally smaller than engine/src/mirrors.rs.  It models  *)
(* the durable run identity, the process-group handoff, scheduler policy, *)
(* and the distinction between cancellation and engine interruption.       *)
(***************************************************************************)

EXTENDS Naturals

CONSTANT MaxRunId

Phases == {"idle", "starting", "running", "stopping"}
Outcomes == {"never", "succeeded", "failed", "cancelled", "interrupted"}
StopReasons == {"user", "shutdown"}

VARIABLES phase, outcome, activeRun, nextRun, processGroup, stopReason, paused
vars == <<phase, outcome, activeRun, nextRun, processGroup, stopReason, paused>>

Init ==
    /\ phase = "idle"
    /\ outcome = "never"
    /\ activeRun = 0
    /\ nextRun = 1
    /\ processGroup = 0
    /\ stopReason = ""
    /\ paused = FALSE

Start ==
    /\ phase = "idle"
    /\ nextRun <= MaxRunId
    /\ activeRun' = nextRun
    /\ nextRun' = nextRun + 1
    /\ phase' = "starting"
    /\ processGroup' = 0
    /\ stopReason' = ""
    /\ UNCHANGED <<outcome, paused>>

ScheduledStart ==
    /\ phase = "idle"
    /\ ~paused
    /\ outcome \in {"never", "succeeded"}
    /\ nextRun <= MaxRunId
    /\ activeRun' = nextRun
    /\ nextRun' = nextRun + 1
    /\ phase' = "starting"
    /\ processGroup' = 0
    /\ stopReason' = ""
    /\ UNCHANGED <<outcome, paused>>

SpawnSucceeded ==
    /\ phase = "starting"
    /\ phase' = "running"
    /\ processGroup' = activeRun
    /\ UNCHANGED <<outcome, activeRun, nextRun, stopReason, paused>>

SpawnFailed ==
    /\ phase = "starting"
    /\ phase' = "idle"
    /\ outcome' = "failed"
    /\ activeRun' = 0
    /\ processGroup' = 0
    /\ stopReason' = ""
    /\ UNCHANGED <<nextRun, paused>>

Cancel ==
    /\ phase \in {"starting", "running"}
    /\ phase' = "stopping"
    /\ stopReason' = "user"
    /\ processGroup' = IF phase = "running" THEN processGroup ELSE 0
    /\ UNCHANGED <<outcome, activeRun, nextRun, paused>>

Shutdown ==
    /\ phase \in {"starting", "running"}
    /\ phase' = "stopping"
    /\ stopReason' = "shutdown"
    /\ processGroup' = IF phase = "running" THEN processGroup ELSE 0
    /\ UNCHANGED <<outcome, activeRun, nextRun, paused>>

ExitZero ==
    /\ phase = "running"
    /\ phase' = "idle"
    /\ outcome' = "succeeded"
    /\ activeRun' = 0
    /\ processGroup' = 0
    /\ stopReason' = ""
    /\ UNCHANGED <<nextRun, paused>>

ExitFailure ==
    /\ phase = "running"
    /\ phase' = "idle"
    /\ outcome' = "failed"
    /\ activeRun' = 0
    /\ processGroup' = 0
    /\ stopReason' = ""
    /\ UNCHANGED <<nextRun, paused>>

ExitAfterStop ==
    /\ phase = "stopping"
    /\ phase' = "idle"
    /\ outcome' = IF stopReason = "user" THEN "cancelled" ELSE "interrupted"
    /\ activeRun' = 0
    /\ processGroup' = 0
    /\ stopReason' = ""
    /\ UNCHANGED <<nextRun, paused>>

RestartActive ==
    /\ phase \in {"starting", "running"}
    /\ phase' = "idle"
    /\ outcome' = "interrupted"
    /\ activeRun' = 0
    /\ processGroup' = 0
    /\ stopReason' = ""
    /\ UNCHANGED <<nextRun, paused>>

RestartStopping ==
    /\ phase = "stopping"
    /\ phase' = "idle"
    /\ outcome' = IF stopReason = "user" THEN "cancelled" ELSE "interrupted"
    /\ activeRun' = 0
    /\ processGroup' = 0
    /\ stopReason' = ""
    /\ UNCHANGED <<nextRun, paused>>

PauseChanged ==
    /\ paused' = ~paused
    /\ UNCHANGED <<phase, outcome, activeRun, nextRun, processGroup, stopReason>>

(* A completion from another RunId is an explicitly modeled no-op.  It must
   not finish, cancel, or signal the current run. *)
StaleCompletion ==
    /\ phase \in {"running", "stopping"}
    /\ \E id \in 1..MaxRunId: id # activeRun
    /\ UNCHANGED vars

Next ==
    \/ Start
    \/ ScheduledStart
    \/ SpawnSucceeded
    \/ SpawnFailed
    \/ Cancel
    \/ Shutdown
    \/ ExitZero
    \/ ExitFailure
    \/ ExitAfterStop
    \/ RestartActive
    \/ RestartStopping
    \/ PauseChanged
    \/ StaleCompletion

TypeOK ==
    /\ phase \in Phases
    /\ outcome \in Outcomes
    /\ activeRun \in 0..MaxRunId
    /\ nextRun \in 1..(MaxRunId + 1)
    /\ processGroup \in 0..MaxRunId
    /\ stopReason \in {"", "user", "shutdown"}
    /\ paused \in BOOLEAN
    /\ (phase = "stopping") => stopReason # ""
    /\ (phase # "stopping") => stopReason = ""
    /\ (phase = "running") => processGroup = activeRun

ActiveIdentity ==
    (phase # "idle") => /\ activeRun > 0
                         /\ activeRun < nextRun

NoStaleProcessGroup ==
    (phase = "idle") => processGroup = 0

Spec == Init /\ [][Next]_vars

=============================================================================
