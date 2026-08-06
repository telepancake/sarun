# Working method for Sarun

This file is repository-wide instruction for coding agents. Read and apply it
before changing Sarun.

Sarun is allowed to be exploratory while a subsystem is young. Experiments may
be replaced, formats may change, and failed approaches should be deleted. Once
a subsystem starts carrying valuable data, running unattended, publishing
state, or accumulating recovery behavior, stop extending it by local patches.
Map it, specify it, and make the specification executable.

The objective is not merely to fix the reported example. It is to know the
state space well enough that an overlooked class of ordinary events is
unlikely.

This is a reasoning method, not a catalogue of forbidden implementation
shapes. A hash, an extra pass, a database, a cache, or a sidecar may be the
right design when it buys enough correctness, simplicity, or performance for
its cost. Conversely, satisfying a mechanical metric does not make a design
good: do not create elaborate indexes or retained state merely to claim
"one pass," "constant time," or another attractive bound. Start from the
required behavior and expected scale, compare the complete alternatives, and
record why the chosen tradeoff is reasonable.

## 1. Decide whether the work is exploration or engineering

For repository-wide or cross-process work, start with
`docs/architecture/SYSTEM_MODEL.md`. It is the reconciled inventory and gap
register; update it when ownership, artifacts, or transition coverage changes.

Classify the affected subsystem before editing:

- **Experimental:** the question is still “which representation or algorithm
  should exist?” Keep experiments isolated, measurable, and easy to delete.
- **Maturing:** real workflows exist but state, ownership, or performance is
  still implicit. Stop adding recovery conditions and perform the mapping
  process below.
- **Specified:** authoritative state machines, invariants, commit points, and
  cost bounds exist. New behavior must first fit those models.

Signals that force the maturing process include resumability, background
processes, user-visible progress, destructive actions, persistent identities,
multiple cooperating processes, crash recovery, atomic publication, or data
large enough that an accidental extra pass matters.

Do not preserve experimental compatibility paths for unreleased artifacts.
Delete superseded mechanisms, tests, switches, and formats.

## 2. Map authority before control flow

Divide the subsystem by ownership, not by source file. For each independently
owned machine, record:

- the owner allowed to mutate it;
- durable authoritative evidence;
- live telemetry;
- identities binding evidence to an operation;
- consumers that project or display its state;
- resources it owns and may clean up.

Examples of separate machines are job scheduling, process supervision, source
planning, target materialization, assembly, index construction, installation,
serving, optional media, and cleanup.

Inventory every current source of “truth”: database columns, in-memory maps,
PIDs, lock files, marker files, receipts, directory names, build targets,
indexes, progress snapshots, and stderr. Classify each as authority, derived
projection, telemetry, or obsolete guess.

There must be one read-only inspector for each durable machine. UI, CLI,
scheduler, build graph, recovery, and cleanup consume that inspector rather
than independently interpreting files or nullable fields.

## 3. Separate observation from desired semantics

First document what the current program actually does, including contradictory
or accidental behavior. Separately define what the program should mean.

Do not turn an existing filename, environment variable, error string, or
workaround into a specification merely because code already depends on it.
Current files made during development are not legacy formats.

When independent audits are useful, assign machines to separate agents so
shared assumptions do not immediately contaminate every map. Reconcile their
boundaries centrally: an identity or commit point must have exactly one owner.

## 4. Derive states from orthogonal facts

Do not create one loose status string whose variants mix unrelated dimensions.
Separate axes such as:

```text
scheduling: enabled | paused
last outcome: never-run | succeeded | failed | cancelled | interrupted
runtime: idle | starting | running | stopping
publication: absent | candidate | committed
cleanup: clean | pending | failed
```

Define UI state, scheduler eligibility, and available commands as projections
of those facts. A scheduling attribute must not hide a failed outcome. Live
telemetry must not turn an invalid durable state into “running.”

Use closed enums and exhaustive matching where practical. Invalid combinations
should be unrepresentable or rejected with a typed diagnostic.

## 5. Enumerate events systematically

List events independently of the commands that happen to exist today.
Consider at least:

- user intent: create, inspect, run, retry, resume, pause future work, cancel
  current work, abandon, replace, update, attach, detach, delete registration,
  delete data;
- scheduler and admission: due, capacity granted, capacity unavailable;
- child process: spawn succeeds, spawn fails, progress, clean exit, error exit,
  signal exit, stale completion;
- engine lifecycle: startup, orderly shutdown, crash, restart;
- filesystem: temporary creation, short write, fsync success/failure, rename,
  missing artifact, malformed receipt, foreign receipt, contradictory
  artifacts, no space;
- upstream/network: discovery changes, gap, timeout, disconnect, 429 with
  Retry-After, malformed input, unavailable part;
- time: retry deadline, timeout, stale heartbeat;
- serving: reader opens before, during, or after generation publication.

Also derive user actions from the resource lifecycle, not from current keys or
menus. Distinguish pause from cancel, resume from retry, registration deletion
from data deletion, and incremental update from full replacement.

## 6. Complete the state/event table

For every machine, create a table whose cells contain:

```text
current durable state
event
guard or precondition
next durable state
required side effects
commit point
user-visible result
cost
```

Every state/event pair is a defined transition, an explicit no-op, a typed
rejection, or impossible by construction. An empty cell is an unanswered
design question.

Late and duplicated events must be included. Completion events name their
RunId or AttemptId; an old child cannot complete a newer run. A failed,
cancelled, or interrupted attempt is not an implicit request to retry.

## 7. State invariants and commit points

Write invariants before transition implementation. Typical invariants include:

- at most one live owner per job or mutable build;
- identities are stored in typed receipts, never inferred from pathnames;
- no artifact is adopted across PlanId, RunId, AttemptId, UpdateId, or
  GenerationId;
- a generation is not ready until archive, index, and receipt are mutually
  bound;
- before the publication commit, new readers see the old generation; after it,
  they see the new generation;
- open readers keep a complete generation across publication;
- optional media and cleanup cannot retroactively fail committed text data;
- authoritative inputs survive until the output commit is durable;
- invalid installed state is non-destructive; classified malformed or foreign
  private scratch may be discarded when the current design cannot consume it.

Name the exact publication operation—normally a destination-local atomic
rename followed by directory fsync. “The files are mostly there” is not a
commit point.

## 8. Walk every interruption boundary

For each durable transition, reason about interruption:

- before the first write;
- during data construction;
- before and after file fsync;
- before and after receipt write;
- before and after rename;
- before and after directory fsync;
- before and after source deletion;
- before and after process spawn and outcome persistence.

After each boundary, the read-only inspector must return one exact valid state
or one exact invalid-state diagnostic without mutation. Recovery is an
explicit transition from that state. It must not guess, silently bless foreign
work, or delete ambiguous data. A classified malformed/foreign temporary tree
whose format is not part of the current product may instead be explicitly
discarded; do not add an adoption or compatibility path merely to retain it.

## 9. Predict performance before running

Every transition touching substantial data must state a static cost model:

- network bytes and request count;
- compressed and uncompressed bytes read;
- bytes written and rewritten;
- compression/decompression passes;
- sort volume and number of consolidation passes;
- peak retained memory by named buffer or queue;
- maximum open descriptors;
- fsync and database operations;
- work performed by passive inspection and UI refresh.

Evaluate the formula at enwiki scale, not only on a tiny fixture. Treat
`O(total archive bytes)` work in restart, update, installation, serving
startup, or progress reporting as a design decision requiring an explicit
benefit and scale evaluation. It is usually inappropriate on a passive path,
but may be entirely reasonable for an explicit construction, conversion,
integrity audit, or repair.

Useful reference envelopes for the present Wikipedia design are:

```text
initial import:
    each source read/decompressed once
    sorted intermediates read once
    final archive written once
    index projected during the stream

incremental update:
    selected tail sources read once
    each affected base range read once
    each replacement range written once
    unchanged ranges neither decoded nor rewritten
    index composed from base index plus update metadata

passive startup/UI:
    job rows + receipts + indexes + bounded headers
    no archive decoding, hashing, or recursive directory scans
```

These are not universal axioms. For example, a bounded external merge may
reasonably add sequential passes to obtain simple recovery and a safe
descriptor bound. Compare that cost with the complexity, memory, and failure
surface of a nominally one-pass alternative. Prefer eliminating work when it
also simplifies the design; do not move the same work into huge sidecars or
opaque retained state to make a counter look better.

Generation identity should normally be assigned and bound to structural
inventory, with content digests accumulated while bytes already pass through
the writer. Rereading an entire archive solely to name or recover it needs a
separate, concrete integrity benefit; an explicitly requested integrity audit
is a different operation. Telemetry should be updated at owned
write/seal/remove boundaries rather than estimated by repeatedly walking large
trees.

Compression ratios, kernel caching, and constants still require benchmarks.
Measurements validate the predicted envelope; they do not substitute for an
algorithmic cost model.

## 10. Make the model the implementation

Implement:

- one pure transition function per machine;
- one read-only durable-state inspector;
- typed receipts with schema and operation identities;
- explicit mutation functions accepting the required input state;
- one shared progress projection;
- atomic helpers for receipt and publication commit points.

Do not leave a typed model unused beside the old implementation. Wire every
consumer to it, delete the old inference path, and keep the build warning-free.
Do not add fallback paths for artifacts created only during development.

Environment variables, marker files, free-form stderr, PIDs, and build-target
existence are not substitute state machines.

## 11. Derive tests mechanically

Tests come from the map:

- table tests for every `(state, event)` pair;
- reference-model tests over generated event sequences;
- failpoints before and after every durable boundary;
- restart from every resulting filesystem state;
- duplicate, reordered, and stale events;
- concurrency tests for start/cancel/delete/shutdown/publication races;
- property tests for ordering, merge idempotence, identity separation, and
  old-or-new serving visibility;
- scale tests with instrumented readers/writers/filesystems that assert byte
  passes, memory bounds, descriptor bounds, and fsync counts.

Tiny elapsed-time benchmarks are insufficient: a bad full scan can look fast
on a fixture. Assert which inputs were opened and how many bytes crossed each
boundary.

Then run realistic workflows and compare observed counters with the static
model. Unexpected work is a design defect until explained.

## 12. Completion and handoff

A maturing subsystem is complete only when:

1. machines, events, ownership, identities, invariants, and costs are written;
2. one inspector and transition API are active in production paths;
3. old inference, fallback, and compatibility paths are removed;
4. state/event, crash, concurrency, and scale tests pass;
5. UI and CLI expose the same state and available actions;
6. a real workflow demonstrates predicted behavior;
7. remaining deviations are explicitly documented design decisions.

Do not call partial enforcement “complete.” State which machines are enforced
and which still deviate.

## Repository discipline

- Examine committed and uncommitted changes before editing; preserve unrelated
  user work.
- Do not run a formatter across unrelated code. Revert formatting-only churn.
- Prefer simple formats and one code path.
- Do not hide errors from ownership-changing database or filesystem
  operations.
- Do not commit generated experiments, temporary archives, or user data.
- Run focused tests first, then the broad relevant suite and a release build.
- Commit only the reviewed logical change. Push only with the user’s explicit
  authorization.
