# Wikipedia mirror lifecycle

This document is the authoritative lifecycle specification for Wikipedia
initial imports and explicit full-snapshot replacements. It specifies what the
program means, independently of the current implementation.

The incremental-update lifecycle uses the same job, generation, installation,
media, and cleanup rules. Its discovery, tail, range-candidate, and generation
commit machines are specified in
[UPDATE_LIFECYCLE.md](UPDATE_LIFECYCLE.md).

The key words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## 1. Principles

1. A state is identified by typed, durable evidence. A filename, a process ID,
   a line of stderr, or the existence of a make target is not a state by
   itself.
2. There is one function that interprets the durable build tree. The CLI,
   scheduler, UI, Kati recipes, recovery, and cleanup MUST use that
   interpretation instead of independently guessing state.
3. Live telemetry enriches a durable state. It MUST NOT determine or change the
   durable state.
4. An interrupted, failed, or cancelled run is not a request to retry. Only a
   user request or an explicit retry policy creates another run.
5. A completed archive is not a completed generation. A generation becomes
   ready only after its archive, title/frame index, and generation receipt are
   mutually bound and durable.
6. An installed generation remains installed if optional media work or scratch
   cleanup fails.
7. Recovery MUST reject an impossible or foreign combination. It MUST NOT
   silently adopt it, bless it with a new identity, or delete potentially
   authoritative data.
8. State transitions that publish durable work MUST be atomic at their stated
   commit point. Data needed to recover the previous stable state MUST survive
   until the new commit point is durable.

## 2. Terms and identities

- **Job**: the engine-owned schedule and user-visible mirror registration.
- **Run**: one supervised execution of a job. Every run has a fresh `RunId`.
- **Plan**: the immutable selection of upstream snapshots, source parts, and
  encoding parameters for one full build. Its complete canonical encoding
  determines `PlanId`.
- **Target**: one independently materialized source part named by
  `(PlanId, TargetKind, TargetIndex)`.
- **Generation**: an archive and its required indexes built from exactly one
  plan. It has a `GenerationId`, which MUST bind the `PlanId`, archive identity,
  and index identity.
- **Attempt**: one process's work toward a target or assembly checkpoint. It
  has an `AttemptId`; a PID is telemetry attached to the attempt, not its
  identity.
- **Installed generation**: the archive/index pair currently selected for
  serving.

Identities MUST be stored inside typed receipts. They MUST NOT be inferred from
path names. Receipts have a schema version and are written atomically.

## 3. Scope and ownership

| Owner | Owns | Does not own |
|---|---|---|
| Engine | job record, schedule, `RunId`, child process group, run outcome | build-state interpretation |
| Build coordinator | destination-local build lock, plan, transition sequencing | schedule policy |
| Kati | dependency scheduling and bounded parallel execution | durable lifecycle state |
| Target worker | one target attempt, its checkpoint and telemetry | another target or global completion |
| Assembler | assembly checkpoint and candidate archive | installed generation |
| Index builder | candidate generation index | archive identity or installation |
| Installer | generation switch transaction | archive construction |
| Media worker | optional packed media state | validity of the installed text generation |
| Cleaner | explicitly enumerated non-authoritative remnants | installed or recoverable authoritative data |

There MUST be no second dispatcher, detached service hierarchy, or independent
liveness registry hidden inside a mirror build. Kati and Brush may execute the
work, but the engine remains the process owner.

## 4. Durable layout and the state inspector

The implementation MUST expose an operation equivalent to:

```rust
fn inspect_build(layout: &BuildLayout, expected: Option<PlanId>)
    -> Result<BuildState, InvalidBuildState>;
```

`inspect_build` is the only authority for interpreting the build tree. It MUST:

- validate every receipt before returning the state it represents;
- validate identity links between plan, targets, assembly, archive, and index;
- distinguish absent data from I/O failure and corrupt data;
- return `InvalidBuildState` for contradictory evidence;
- perform no mutation;
- never convert an error into “missing” with `unwrap_or(false)` or equivalent;
- never inspect free-form progress text;
- never use process liveness to decide durable state.

Mutation is performed by explicit transition operations that accept the
required input state and return the next state. Rust enums and exhaustive
matching SHOULD make invalid calls unrepresentable.

The build lock grants exclusive mutation. It is not evidence of build phase.

## 5. Job and run machine

### 5.1 Stable job states

```text
Idle(NeverRun)
Idle(Succeeded)
Idle(Failed)
Idle(Cancelled)
Idle(Interrupted)
Starting(RunId)
Running(RunId, ProcessGroup)
Stopping(RunId, ProcessGroup, StopReason)
```

`paused` and `next_due` are scheduling attributes of an idle job. They are not
run outcomes. A running job may be marked “pause future runs”; that does not
implicitly cancel the current run.

The engine MUST durably create the `RunId` and `Starting` record before spawn.
Completion events MUST name the same `RunId`; a late event from an old child
cannot finish a newer run.

### 5.2 Transitions

| Current state | Event | Next state | Required effect |
|---|---|---|---|
| `Idle(NeverRun)` | initial run requested | `Starting(new)` | persist run before spawn |
| `Idle(Succeeded)` | scheduled time due | `Starting(new)` | normal scheduled maintenance |
| any `Idle` | explicit run/retry/resume | `Starting(new)` | user-authorized attempt |
| `Idle(Failed/Cancelled/Interrupted)` | scheduler tick | unchanged | surface attention; never auto-retry |
| `Starting` | spawn succeeds | `Running` | record process group |
| `Starting` | spawn fails | `Idle(Failed)` | record attributed diagnostic |
| `Starting` | cancel requested | `Stopping` | prevent or terminate spawn |
| `Running` | cancel requested | `Stopping` | signal owned process group |
| `Running` | process exits zero | `Idle(Succeeded)` | persist outcome and end time |
| `Running` | process exits nonzero | `Idle(Failed)` | persist exit and attributed diagnostic |
| `Running` | engine/owner disappears | `Idle(Interrupted)` on next inspection | preserve build checkpoints |
| `Stopping` | process group exits | `Idle(Cancelled)` | record explicit cancellation |
| any `Idle` | pause changed | same outcome | only change scheduling attribute |

An orderly engine shutdown interrupts active mirror runs unless the user
explicitly cancelled them. Engine startup MUST display those runs as
`Interrupted`; it MUST NOT relaunch them.

Signal escalation MUST remain scoped to the same owned `RunId` and process
group. It MUST NOT signal a numeric process group after ownership could have
been released and reused.

## 6. Plan and build machine

### 6.1 Stable build states

```text
Unplanned
Planned(Plan, TargetStates)
ReadyForAssembly(Plan, ReadyTargets)
Assembling(Plan, AssemblyCheckpoint)
Projecting(Plan, CompleteArchive)
Ready(ReadyGeneration)
```

Failure is a run outcome, not an alternate build phase. After failure,
`inspect_build` returns the last durable build state, which a later explicit
resume consumes.

### 6.2 Transitions

| Current state | Event | Next state | Commit point |
|---|---|---|---|
| `Unplanned` | discovery starts | unchanged | discovery is live telemetry |
| `Unplanned` | discovery succeeds | `Planned` | atomic, synced plan receipt |
| `Planned` | target committed | `Planned` with one more Ready target | atomic target-directory publish |
| `Planned` with all targets Ready | readiness evaluated | `ReadyForAssembly` | pure state projection |
| `ReadyForAssembly` | assembly starts/seals ranges | `Assembling` | first valid assembly receipt/checkpoint |
| `Assembling` | archive DONE becomes durable | `Projecting` | complete candidate archive identity durable |
| `Projecting` | index and generation receipt commit | `Ready` | atomic generation receipt after both artifacts |
| `Ready` | install requested | install machine | build remains recoverable until install commits |

An ordinary resume MUST use the existing valid plan. Selecting a newer full
snapshot is a separate explicit “replace plan” event. It MUST NOT alter the
installed generation and MUST NOT destroy the old build root until the user has
authorized replacement or the obsolete root has been proven non-authoritative.

An artifact without a plan receipt is not part of a new plan. Discovery MUST
NOT assign it to the newly discovered plan.

## 7. Target machine

### 7.1 Stable target states

```text
Missing
Partial(TargetCheckpoint)
Ready(TargetReceipt)
```

`Working(AttemptId, telemetry)` is a live overlay on `Missing` or `Partial`, not
a durable fourth state. A failed attempt leaves `Missing` or a valid `Partial`
plus an attributed diagnostic in the run.

A target receipt MUST bind:

- `PlanId`;
- exact target kind and index;
- source identity from the plan;
- archive identity and byte size;
- a clean archive completion marker;
- required siteinfo identity for the designated siteinfo target;
- materialization statistics.

### 7.2 Transitions

| Current state | Event | Next state | Rule |
|---|---|---|---|
| `Missing` | worker starts | live Working overlay | one owner only |
| `Partial` | worker resumes | live Working overlay | checkpoint identity must match |
| Working | valid checkpoint seals | `Partial` | atomic checkpoint publish |
| Working | target completes | `Ready` | payloads and receipt synced, then atomic directory publish |
| Working | failure/cancel/crash | `Partial` or `Missing` | retain only independently valid checkpoint |
| `Ready` | resume/worker invocation | unchanged | never refetch |
| any | plan replacement | outside this target machine | old tree remains separately identified until cleanup |

Restart MUST NOT delete a partial merely because its PID is dead. If partial
work is intentionally non-resumable, the specification for that target must
say so and cleanup must be an explicit transition.

No compatibility adoption of experimental grouped layouts is part of this
machine. Unsupported layouts are reported as such and may be removed only by
an explicit cleanup/migration action.

## 8. Assembly machine

### 8.1 Stable assembly states

```text
Absent
Partial {
    PlanId,
    compression_reference,
    last_sealed_entity,
    sealed_ranges
}
CompleteArchive {
    PlanId,
    archive_identity,
    segments
}
ReadyGeneration {
    GenerationId,
    PlanId,
    archive_identity,
    index_identity
}
```

An unsealed range file is never a checkpoint. It is attempt-local scratch and
may be discarded after verifying that all preceding sealed ranges are intact.

The candidate archive manifest MUST contain `PlanId`. A structurally complete
archive whose plan identity differs from the active plan is foreign. Recovery
MUST NOT rewrite a marker to adopt it.

The generation receipt is the only `ReadyGeneration` commit record. It MUST
bind the archive and index identities. A make target named
`archive.complete` MAY point at this receipt, but existence alone MUST NOT be
used as proof.

### 8.2 Transitions

| Current state | Event | Next state | Rule |
|---|---|---|---|
| `Absent` | assembly begins | `Partial` | persist plan-bound assembly identity |
| `Partial` | range seals | `Partial` | fsync file, atomic rename, sync directory |
| `Partial` | interrupted | unchanged | discard only unsealed tail |
| `Partial` | explicit resume | `Partial` | reuse immutable compression reference and sealed boundary |
| `Partial` | DONE seals | `CompleteArchive` | validate complete stream and plan identity |
| `CompleteArchive` | index build begins | unchanged | visible Projecting activity |
| `CompleteArchive` | index + receipt commit | `ReadyGeneration` | receipt written last |
| `ReadyGeneration` | source cleanup | unchanged | target inputs are now consumable |

Assembly MUST use a bounded number of open descriptors independent of source
count. A streaming N-way merge means bounded readers feeding one streaming
writer; it does not permit retaining every source descriptor for convenience.

If resume rereads records preceding the sealed boundary, progress MUST say so.
The title projection MUST cover the whole resulting archive, including records
before the resume boundary.

Source targets and the manifest MUST survive until `ReadyGeneration`. A
complete archive without an index can be resumed at Projecting without
redownloading. A complete archive from another plan cannot.

## 9. Installation machine

The installed archive and title/frame index form one logical generation even
if represented by sibling paths. Serving MUST open only a mutually matching
pair.

### 9.1 Stable transaction states

```text
NoInstallTransaction
Prepared { new_generation, old_generation? }
OldGenerationPreserved { new_generation, old_generation }
NewArchivePlaced { new_generation, old_generation? }
NewPairPlaced { new_generation, old_generation? }
Committed { new_generation }
```

The transaction receipt MUST store old and new `GenerationId`s and its phase.
Artifact identity, not path presence alone, resolves recovery.

### 9.2 Transitions and recovery

| Current state | Event | Next state | Recovery policy |
|---|---|---|---|
| `NoInstallTransaction` | install Ready generation | `Prepared` | old generation still served |
| `Prepared`, replacement | preserve old pair | `OldGenerationPreserved` | rollback to old on interruption |
| `Prepared`/`OldGenerationPreserved` | place new archive | `NewArchivePlaced` | rollback if matching new index absent |
| `NewArchivePlaced` | place matching new index | `NewPairPlaced` | roll forward after pair validation |
| `NewPairPlaced` | persist installed-generation receipt | `Committed` | new pair is authoritative |
| `Committed` | remove old pair/transaction remnants | `NoInstallTransaction` | cleanup failure is non-fatal |

For a first install, rollback before `NewPairPlaced` restores the candidate to
the build tree or leaves no installed generation. For replacement, rollback
restores the old matching pair. Once the complete matching new pair is durable,
recovery rolls forward rather than discarding it.

While a transaction is before `Committed`, serving either uses the explicitly
preserved old generation or refuses if no old generation exists. It MUST NOT
guess from whichever sibling paths happen to exist.

## 10. Media and cleanup

Media is an independent post-install machine:

```text
Disabled
Pending(GenerationId)
Running(GenerationId)
Ready(GenerationId)
Failed(GenerationId, Diagnostic)
```

Scratch cleanup is likewise independent:

```text
NotNeeded
Pending
Running
Failed(Diagnostic)
Done
```

A text generation that reached installation `Committed` remains successfully
installed in every media and cleanup state. The UI may report:

```text
installed; media failed
installed; cleanup pending
installed; cleanup failed
```

It MUST NOT report the mirror generation itself as failed, and the next normal
mirror run MUST NOT reinterpret post-install work as an incremental update or
new initial import.

Cleanup MUST operate from an explicit ownership manifest or an exact typed
layout. It MUST never recursively delete an ambiguous root. Cancelling a run
preserves valid checkpoints; deleting mirror data is a separate, explicit,
destructive action.

## 11. Progress projection

Progress is a pure projection:

```rust
fn project_progress(
    job: &JobLifecycle,
    build: &BuildState,
    live: Option<&RunTelemetry>,
) -> MirrorProgress;
```

Required user-visible phases include:

- discovering sources;
- waiting for explicit resume after interruption/failure/cancellation;
- validating plan-bound target receipts;
- fetching/parsing a named target;
- ready for assembly;
- inventorying assembly inputs;
- sampling compression context;
- distilling compression context;
- replaying bootstrap;
- merging, with durable resume boundary;
- projecting title/frame index;
- ready to install;
- switching generation;
- installed;
- optional media work;
- optional cleanup.

Telemetry MUST be keyed by `RunId`, `AttemptId`, and target/assembly identity.
Stale telemetry may be displayed as historical diagnostics but cannot make a
state “running.” Free-form stderr is diagnostic text only. Progress code MUST
NOT parse words such as “failed” or “finished” to determine state.

For every stable state, the UI, CLI, and logs MUST show the same phase. A
process that is reading an archive to build an index must say “building index,”
not “running,” “fetching,” or “assembling.”

## 12. Invalid combinations

`inspect_build` or install inspection MUST reject at least:

- build artifacts with no plan receipt;
- a plan receipt whose canonical identity does not match `PlanId`;
- a target receipt for another plan, kind, index, or source;
- a target receipt whose payload or required siteinfo is absent;
- a completed archive whose manifest names another plan;
- a generation receipt without both matching archive and index;
- archive and index carrying different `GenerationId`s;
- an assembly checkpoint with reordered, overlapping, or foreign ranges;
- simultaneous competing candidate generations without an explicit
  transaction that names both;
- an install transaction whose observed old/new artifacts match neither its
  recorded phase nor an adjacent recoverable phase;
- a final archive without a matching index and no active install transaction;
- previous-generation backups with no transaction receipt;
- live telemetry naming a different run or attempt than the engine owns.

Invalid-state handling is non-destructive. The program reports exact paths,
expected identities, and observed identities, then waits for an explicit
repair or deletion action.

## 13. Kati integration

The generated graph describes dependencies:

```text
plan -> independent targets -> assembly -> index/generation receipt -> install
```

Kati target files MAY be lightweight projections of typed receipts. Before
declaring a target up to date, its recipe or graph generator MUST validate that
receipt through the shared state code. A wildcard or path-existence check is
not sufficient.

Stage generation MUST use `inspect_build`; it MUST NOT implement another
recovery policy. Build-node and assembly builtins receive typed plan/target
identities and refuse foreign work.

## 14. Required tests

### 14.1 Exhaustive transition tests

Each enum's `(state, event)` matrix MUST be tested. Every unsupported pair must
return a typed error or explicit no-op. Tests must cover:

- scheduler tick, explicit run, explicit retry/resume, pause, cancel, child
  exit, spawn failure, engine shutdown, and engine restart;
- discovery success/failure and explicit plan replacement;
- every target state and worker outcome;
- every assembly state, range seal, completion, projection, and cleanup;
- every installation state and recovery decision;
- every media and cleanup outcome;
- progress projection for every stable state.

### 14.2 Crash tests

Use deterministic failpoints around every durable operation:

- temporary creation;
- data write;
- file fsync;
- receipt write and fsync;
- rename;
- directory fsync;
- source deletion;
- process spawn and outcome persistence.

After each injected crash:

1. reopen using the same inspector used in production;
2. assert one exact valid state or one exact invalid-state diagnostic;
3. perform the specified recovery/resume event;
4. assert no already committed target is fetched again;
5. assert no authoritative input was deleted early;
6. assert the eventual generation is identical to an uninterrupted build.

Installation crash tests cover every boundary in the table, for both first
install and replacement.

### 14.3 Model and property tests

A small pure reference model MUST describe the state transitions. Generated
event sequences are applied to both model and filesystem implementation.
Properties include:

- at most one live run and one mutating build owner per job;
- monotonically increasing durable work within a plan;
- no cross-plan adoption;
- no mixed archive/index generation;
- no automatic retry after failure, interruption, or cancellation;
- serving observes either the old committed generation or the new committed
  generation, never a mixed pair;
- media and cleanup cannot alter installed-generation success;
- progress phase equals the inspected lifecycle phase.

### 14.4 Scale assertions

Tests or instrumentation MUST assert:

- open descriptor count is bounded independently of source count;
- worker count obeys the configured global budget;
- scratch paths remain destination-local;
- cancellation terminates all owned children and no others;
- progress continues during long reads, compression, projection, fsync, and
  recovery without inventing a different phase.

## 15. Maturity gate

Wikipedia initial import is lifecycle-mature only when:

1. the implementation represents these states and transitions explicitly;
2. all consumers use the shared inspectors and progress projection;
3. ad-hoc compatibility and fallback paths not justified by a released format
   are removed;
4. the exhaustive and crash matrices pass;
5. every remaining deviation from this document is written down as a deliberate
   specification change, not introduced as a local recovery condition.
