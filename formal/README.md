# Sarun formal design

This directory is the executable part of Sarun's design bootstrap. It is not a
claim that the repository is already verified. The repository-wide ownership,
authority, transition, and evidence map is in
[`docs/architecture/FORMAL_DESIGN.md`](../docs/architecture/FORMAL_DESIGN.md);
the current `LIFECYCLE.md`, `UPDATE_LIFECYCLE.md`, `ARCHIVE.md`, and
`PERFORMANCE.md` documents state normative Wikipedia contracts where they say
so. Older Depot sketches and `SPEC.md` are evidence to reconcile, not an
additional authority.

The distinction between the artifacts is intentional:

* the architecture document says what each subsystem owns, which durable facts
  are authoritative, and which events it must accept, reject, or treat as a
  no-op;
* the TLA+ modules state small, bounded transition systems and invariants for
  the highest-risk handoffs;
* Rust transition functions and integration tests are the implementation and
  refinement evidence. They do not become a specification merely by using an
  enum.

## Model catalogue

| model | production boundary | current status |
| --- | --- | --- |
| `mirror_run/MirrorRun.tla` | engine mirror registration, scheduling, run identity, child completion | bounded abstract model; TLC execution is the first gate |
| `generation_publication/GenerationPublication.tla` | candidate generation, selector commit, reader snapshot | bounded abstract model; TLC execution is the first gate |

The publication model permits rebuilding an already-known generation identity,
but does not yet model the implementation's idempotent-install fast path,
crash/reopen behavior, or generation-content validation; receipt and index are
abstract booleans only.

The models deliberately omit telemetry, filenames, PIDs, progress text, and
compression details. Those are implementation mechanisms unless they carry a
durable identity or affect a stated resource bound. The models do include the
incarnation/run identity and publication commit points because those are
semantic facts.

`MirrorRun.tla` uses the active `RunId` as an abstract process-group token;
the concrete refinement must prove that the recorded process group is bound
to that run and is safe against macOS PID reuse. Its stale-completion action is
currently a placeholder no-op, not evidence that the concrete child event
path is correct. The omitted due-time, deleting-registration, process-loss,
spawn-race, and shutdown-admission events are listed in the architecture
ledger.

## Running the models

Run `make check-models`. It never downloads a runtime. The command requires a
user-provided TLC distribution and Java runtime, either through `TLC_JAR` and
`JAVA` (a relative `TLC_JAR` is resolved from the repository root), or through
the documented local tool path. It runs SANY syntax checking,
then TLC with the checked-in small bounds and explicit invariant names. Missing
prerequisites are an actionable failure, not a silent skip.

The checked-in configurations are deliberately small enough for a quick
smoke check. They are not evidence about enwiki-scale behaviour. A model
change must include the new/removed invariant names and a short explanation of
which implementation tests refine it. A model that only passes because an
event or fault is omitted is incomplete; add the event to the model or record
the omission in the architecture coverage ledger.

## Refinement obligations

For each model, the implementation must eventually provide:

1. a mapping from durable implementation evidence to the abstract state;
2. an event mapping for every production transition and every typed rejection;
3. a test that exercises duplicate, stale, reordered, interrupted, and failed
   events at the boundary;
4. a resource-bound measurement where the model assumes bounded descriptors,
   memory, or passes; and
5. an explicit gap entry when production has a behavior not represented by the
   model.

Until those obligations are met, a green TLC run means only that the bounded
abstract machine is internally consistent.
