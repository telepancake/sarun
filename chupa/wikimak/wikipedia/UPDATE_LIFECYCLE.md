# Wikipedia incremental-update lifecycle

This document is the authoritative lifecycle specification for applying
incremental Wikimedia content and page-action data to an installed Wikipedia
mirror. It is a normative companion to [LIFECYCLE.md](LIFECYCLE.md).

The job/run, installation, serving, media, cleanup, progress, invalid-state,
and testing rules in `LIFECYCLE.md` apply here without exception. In
particular:

- an interrupted, failed, or cancelled update is not an automatic retry
  request;
- a live process, marker filename, or line of stderr is not durable state;
- optional media and cleanup cannot retroactively fail an installed text
  generation;
- recovery is non-destructive and must not adopt foreign work.

The key words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## 1. Model

An incremental update is a transaction:

```text
installed base generation
    + immutable update plan
    + immutable sorted update tail
    + one immutable candidate per base range
    + one new title/frame/segment index
    = one new installed generation
```

The archive range files are immutable objects. Updating a range means creating
a new range object and selecting it in the candidate generation. It does not
mean editing the old range.

The title/frame/segment index is the generation selector. It names the exact
ordered range inventory that constitutes the generation. Atomically publishing
the complete new index is the reader-visible publication point. Before that
rename, new readers use the preserved base generation; after it, new readers
use the new generation. Readers already open may continue using their original
generation. The lifecycle records `Committed` only after validating that
published generation and durably writing the update commit receipt.

### 1.1 Performance contract

The state machine also fixes the asymptotic I/O behavior. Correctness recovery
MUST NOT be implemented by repeatedly rescanning immutable archive data.
In this section `U_c` and `U_r` are respectively the compressed and raw typed
sizes of the sorted update tail.

- discovery reads remote metadata only;
- materializing the sorted tail reads every selected source stream once and
  writes the authoritative tail once. The normal three-day-overlap update has
  about seven logical inputs (daily runs, at most two history partitions, and
  the manifest), so it merges directly. A stale mirror with more than 64
  logical inputs uses a bounded-fan-in hierarchy: for logical source count
  `S`, it adds `ceil(log_64(S)) - 1` complete intermediate
  tail-sized scratch write/read and recompression passes. Thus compressed I/O
  is `O(U_c * ceil(log_64(S)))` and record CPU is
  `O(U_r * ceil(log_64(S)))`, never a pass over the base archive. The first
  intermediate pass appears only once the mirror is roughly 61 daily runs
  behind; this explicit stale-mirror cost is preferred to retaining one open
  descriptor or one unbounded decoded frame for every daily run;
- applying the tail reads each affected base range once and writes its
  replacement once;
- an unchanged base range is neither decoded nor rewritten;
- within a touched range, unchanged base frames are copied as compressed
  payloads. Changed and newly introduced frames are compressed independently
  by the configured bounded CPU pool, then one output owner writes copied and
  recompressed frames in entity order. For raw tail record bytes `U_r` and raw
  typed bytes `B_H` in intersected base frames, coordinator merge/decode work
  is `O(U_r + B_H)`; compressed input I/O is the compressed tail plus the
  intersected frames, not the complete base archive;
  independent output-frame compression is distributed across the CPU pool.
  The queue retains at most one submitted/completed frame per worker plus the
  current indivisible entity. A queued copied frame retains a cloned source
  descriptor and offset, not a payload-sized memory buffer;
- the merge keeps a bounded number of source, base-range, candidate, and
  receipt descriptors open, independent of total source-part and range counts;
- memory use is bounded independently of wiki size except for the explicitly
  indivisible current entity, which a correct page-aligned frame cannot split;
- title projection uses bounded external runs and globally arbitrates current
  title ownership; page-range-local arbitration is not correct for moves or
  competing title claims;
- external-run merge fan-in is bounded, and an interrupted attempt's
  non-authoritative runs are reclaimed before the next explicit resume;
- committing reads the prepared index and small receipts/inventories only;
- restart inspects receipts and bounded structural metadata. It does not scan
  archive payloads or repeat discovery.

Telemetry for tail and range work MUST expose source bytes read, tail bytes
read, base-frame payload bytes copied without decoding, base-frame payload
bytes decoded, candidate bytes written, records, and frames.
Tests MUST assert the selected I/O envelope with instrumented readers and
writers: a direct normal-tail merge, the explicit bounded stale-tail
hierarchy, byte-copying disjoint frames, and decoding only intersected frames.
Elapsed-time tests are not a substitute for those invariants.

## 2. Identities

### 2.1 Base generation

`BaseGenerationId` is the installed generation's `GenerationId` from
`LIFECYCLE.md`. It binds:

- wiki database name;
- complete archive manifest/frontiers;
- compression reference identity;
- ordered archive segment identities;
- title/frame/segment index identity.

Content and metadata frontier strings alone are not a base identity. Two
different archives may legitimately carry the same frontier.

### 2.2 Update plan

An immutable `UpdatePlan` contains:

- schema version;
- `UpdateId`;
- `BaseGenerationId`;
- wiki database name;
- base content and metadata frontiers;
- overlap policy;
- exact discovered daily content runs and source parts;
- exact selected MediaWiki History snapshot and partitions;
- siteinfo source, if any;
- frame, compression, and range-selection settings;
- expected coverage and gap checks.

`UpdateId` is derived from the canonical complete plan with its ID field
blanked. Discovery results MUST be persisted before any source materialization.
Resuming an update uses the persisted plan; it does not rediscover and silently
change the source set.

### 2.3 Tail

`TailId` binds:

- `UpdateId`;
- exact source receipts;
- sorted record-stream identity;
- clean completion marker;
- minimum and maximum entity keys;
- frame and record counts.

The tail is an immutable, sorted archive of records not an instruction log.
Its merge semantics are commutative, associative, and idempotent by record
identity. Overlap is deliberate and does not require remembering which source
files have previously been applied.

### 2.4 Range slots and candidates

The base index defines an ordered vector of `RangeSlot`s. A slot binds:

- entity kind;
- lower and upper routing bounds;
- base segment identity;
- base segment filename as a storage detail.

For every slot, a candidate generation selects exactly one:

```text
Unchanged(BaseSegmentId)
Replaced(CandidateSegmentId)
```

A `RangeCandidateReceipt` binds:

- `UpdateId`, `BaseGenerationId`, and `TailId`;
- slot index and routing bounds;
- base segment identity;
- selected candidate segment identity;
- candidate entity bounds, byte size, frame count, and record count;
- clean completion evidence;
- the consumed tail-key interval.

Filename and byte length alone are not candidate identity.

### 2.5 New generation

`GenerationId` for the result binds:

- `BaseGenerationId`;
- `UpdateId` and `TailId`;
- complete ordered selected-segment inventory;
- new title/frame/segment index identity;
- resulting content and metadata frontiers.

The new index stores this identity. Publishing that index atomically commits the
generation.

## 3. Ownership and layout

The build coordinator owns one destination-local update lock. Only one
uncommitted update may mutate the candidate namespace for a mirror.

Each update has a distinct root:

```text
updates/<UpdateId>/
    plan.json
    tail/
    base/
    ranges/
    candidate-inventory.json
    index/
    telemetry/
    cleanup.json
```

Generic shared names such as `update.swdump`, `update-ranges.json`, and
`updated-ranges/000001.json` MUST NOT be reused by different update
transactions.

Ownership is:

| Owner | Owns |
|---|---|
| Coordinator | update lock, plan transition sequencing |
| Discovery | immutable update plan |
| Tail workers | source checkpoints and immutable source receipts |
| Tail assembler | immutable sorted tail and receipt |
| Range worker | one immutable range candidate and receipt |
| Candidate coordinator | selected-segment inventory |
| Index builder | candidate title/frame/segment index |
| Committer | atomic index publication and installed-generation receipt |
| Server | an opened immutable generation selected by its index |
| Cleaner | prior-generation snapshot and completed transaction remnants |

Kati may schedule tail and range targets, but it does not own or infer update
state.

## 4. Authoritative inspector

The implementation MUST expose an operation equivalent to:

```rust
fn inspect_update(
    installed: &InstalledGeneration,
    root: &UpdateRoot,
) -> Result<UpdateState, InvalidUpdateState>;
```

It is the only authority for interpreting update artifacts. It uses the shared
generation and receipt validators from `LIFECYCLE.md`.

The inspector MUST:

- validate the installed generation before choosing it as a base;
- bind every artifact to one `UpdateId` and `BaseGenerationId`;
- validate the tail before accepting any range receipt;
- validate candidate contents, not only names or lengths;
- validate that every range slot is represented exactly once;
- distinguish candidate construction from generation commit;
- identify a committed update even if marker/cleanup files remain;
- distinguish a committed prior update with cleanup pending from an active
  update based on it;
- perform no mutation;
- return a typed invalid-state diagnostic instead of guessing.

## 5. Stable update states

```text
NoUpdate

Planned {
    plan,
    tail_targets
}

TailReady {
    plan,
    tail
}

BasePreserved {
    plan,
    tail,
    preserved_base
}

ApplyingRanges {
    plan,
    tail,
    preserved_base,
    slots
}

CandidateComplete {
    plan,
    tail,
    preserved_base,
    selected_segments
}

IndexReady {
    plan,
    tail,
    preserved_base,
    selected_segments,
    candidate_index
}

Installed {
    new_generation
}

Committed {
    old_generation,
    new_generation,
    cleanup
}
```

`Discovering`, `WorkingTailTarget`, `BuildingRange`, `BuildingIndex`, and
`Cleaning` are live activity overlays. They are not additional durable states.

Failure is a run outcome. After failure or interruption, `inspect_update`
returns the last stable update state.

### 5.1 Closed event alphabet

The implementation defines one closed `UpdateEvent` enum and one pure,
exhaustive transition function. Its publication events are plan, tail,
preserved base, range plan, non-final/final range receipt, inventory, index,
generation installation, commit receipt, and cleanup publication.

Operational events are equally part of the machine: discovery failure,
source-gap detection, worker failure, cancellation, process crash, explicit
resume/retry, duplicate/stale/foreign receipts, interrupted installation,
failed cleanup, an already-open reader observation, and a new reader opening
either the base or candidate generation. Every phase/event pair is classified
as advancing, an idempotent no-op, rejected, or impossible. A table test covers
the complete cross product. Execution uses the same transition decisions for
resume, worker failure, and serving selection; the table is not a second
descriptive model.

## 6. Discovery and tail machine

### 6.1 Discovery policy

Discovery starts from the verified base generation's frontiers.

Daily content discovery MUST include the configured overlap. If the first daily
run after the base content frontier is not the next day, the update fails with
“full refresh required”; it MUST NOT leap over the gap.

MediaWiki History/page-action data is part of the normal update. The plan
selects the required new snapshot partitions according to the data-source
policy. It MUST NOT omit page actions merely because revision-content updates
are available.

The plan records the exact sources selected. A server-side dump listing that
changes later does not alter a resumed update.

### 6.2 Tail target states

Tail source targets use the target machine in `LIFECYCLE.md`:

```text
Missing
Partial(SourceCheckpoint)
Ready(SourceReceipt)
```

Each source receipt binds its source entry from `UpdatePlan`. A completed
source is never fetched again during resume.

### 6.3 Tail transitions

| Current state | Event | Next state | Commit |
|---|---|---|---|
| `NoUpdate` | explicit/scheduled update requested | live Discovering | no durable change |
| `NoUpdate` | discovery succeeds | `Planned` | atomic plan receipt |
| `Planned` | source target commits | `Planned` | atomic source receipt |
| `Planned`, all sources Ready | tail assembly starts | unchanged | live activity |
| `Planned`, all sources Ready | sorted tail commits | `TailReady` | tail receipt written last |
| `Planned` | failure/cancel/interruption | same durable state | explicit resume required |
| `TailReady` | resume | unchanged | never rediscover or rebuild tail |

The tail assembler streams ready sources into one sorted stream. It may use
bounded external runs, but temporary runs are not receipts. Consumed source
objects may be deleted only after `TailReady` is durable and only if the tail
receipt contains everything needed to validate and resume the transaction.

## 7. Base-preservation machine

Before any path selected by the installed index is replaced or removed, the
entire base generation MUST remain openable by identity.

For split archives this MAY be a hard-linked immutable snapshot of the base
segment inventory and its index. For a single-file archive it MAY be a hard
link to the complete base archive and index. In either case a
`PreservedBaseReceipt` binds the snapshot to `BaseGenerationId`.

| Current state | Event | Next state | Rule |
|---|---|---|---|
| `TailReady` | preserve base begins | unchanged | live activity |
| `TailReady` | preserved receipt commits | `BasePreserved` | archive/index pair validated and directory synced |
| `TailReady` | failure/crash | `TailReady` | incomplete snapshot is non-authoritative |
| `BasePreserved` | resume | unchanged | reuse exact preserved base |

Path-pair presence is not a receipt. A half-created snapshot is neither
silently deleted nor accepted by state inspection. A transition repair may
remove attempt-local links after proving the installed base is still intact.

Serving continues to use the installed base before range application, so base
preservation may be delayed until immediately before the first candidate path
is published.

## 8. Range-candidate machine

### 8.1 Routing

Tail records are routed monotonically by `(EntityKind, entity_id)` into the
base index's range slots. The final slot of an entity kind has an open upper
bound so newly allocated IDs belong to it.

The complete tail is consumed exactly once as an ordered logical stream during
a run. A terminal range receipt records the next tail position as a frame
offset and record ordinal. Resume seeks to that frame and re-decodes at most
that one frame; it does not replay completed tail frames or serialize the
frame remainder into a second archive.

### 8.2 Per-slot states

```text
Pending
Unchanged(RangeCandidateReceipt)
CandidateReady(RangeCandidateReceipt)
```

Both terminal states have receipts. “Unchanged” explicitly selects the base
segment; it is not inferred from absence.

### 8.3 Transitions

| Current slot | Event | Next slot | Commit |
|---|---|---|---|
| `Pending` | tail interval is empty | `Unchanged` | receipt selecting base segment |
| `Pending` | build candidate starts | unchanged + live overlay | immutable base + bounded tail source |
| `Pending` | candidate completes | `CandidateReady` | candidate synced, validated, then receipt |
| `Pending` | failure/cancel/crash | `Pending` | incomplete candidate remains attempt-local |
| terminal slot | resume | unchanged | validate and skip exact recorded tail interval |

Candidate publication MUST NOT overwrite a base segment before the candidate
receipt is durable. A candidate may be stored in the final archive directory
only under a collision-free immutable object name. The selected inventory,
not directory enumeration, determines membership.

Removing an obsolete base pathname before generation commit is permitted only
after the preserved base receipt proves that the old generation remains fully
openable. It does not install the candidate generation; the old index remains
the installed selector.

The candidate builder merges one base range and the routed tail interval using
the archive's immutable compression reference. Because frames are
entity-aligned, base frames whose entity bounds do not intersect a tail entity
are copied in compressed form. Only intersecting frames are decoded, merged,
and recompressed. Record-level merge semantics provide idempotence. Recovery
MUST nevertheless use receipts rather than relying on repeated rewriting as
the normal transaction mechanism.

## 9. Candidate inventory and index

When every slot has a valid terminal receipt, the coordinator constructs one
ordered `CandidateInventory`. It contains exactly one selected segment per
slot plus the reference and completion segments.

The inventory MUST prove:

- every base slot is represented once;
- entity kinds and ranges remain strictly ordered and non-overlapping;
- the final slot of each kind includes all tail records for that kind;
- no tail record lies outside the inventory;
- all selected segment objects exist and validate;
- every selected object belongs to this base/update transaction;
- the immutable compression reference is unchanged;
- the resulting manifest frontiers equal the update plan's result.

Transitions:

| Current state | Event | Next state | Commit |
|---|---|---|---|
| `ApplyingRanges`, all slots terminal | inventory persists | `CandidateComplete` | atomic inventory receipt |
| `CandidateComplete` | index build starts | unchanged + BuildingIndex | old generation still selected |
| `CandidateComplete` | candidate index persists in update root | `IndexReady` | complete index validated against inventory |
| `IndexReady` | resume | unchanged | never rebuild unless explicitly discarded |

The candidate index contains title history, frame locations, ordered segment
directory, `GenerationId`, `BaseGenerationId`, and `UpdateId`. It is built
against the candidate inventory, never by scanning an ambiguous directory that
also contains unselected objects.

## 10. Generation commit

The final archive objects MUST all be durable before commit. The base
generation remains served until the new index is ready.

Commit consists of:

1. verify that installed `GenerationId` still equals `BaseGenerationId`;
2. make every selected candidate segment reachable at its final immutable
   object path;
3. sync the archive directory;
4. atomically publish the new title/frame/segment index at the installed index
   path;
5. sync its parent directory;
6. atomically publish the commit receipt bound to the observed installed
   generation.

Step 4 is the reader-visible generation switch. Before it, new readers select
the old generation. After it, new readers select the new generation. Step 6
records the stable `Committed` lifecycle state. A crash between them is the
explicit `Installed` state and resumes by publishing only the missing commit
receipt. An update marker may aid human diagnostics but cannot redefine
either point.

| Current state | Event | Next state | Rule |
|---|---|---|---|
| `IndexReady` | commit begins | unchanged + live activity | base still authoritative |
| `IndexReady` | index publish succeeds | `Installed` | new generation authoritative |
| `IndexReady` | crash before index publish | `IndexReady` | resume commit |
| `Installed` | commit receipt persists | `Committed` | installation transaction durably acknowledged |
| `Installed` | crash/resume | `Installed` | validate the selected physical generation; do not republish archive or index |
| `Committed` | crash before marker/remnant cleanup | `Committed` | roll forward; never reapply tail |

The new index MUST NOT become visible before all segment paths it names are
durable. Old segment objects MUST NOT be removed until no committed or
preserved generation names them.

## 11. Serving during update

Serving chooses a generation, not a directory:

- before commit, new readers open `BaseGenerationId`;
- after commit, new readers open the new `GenerationId`;
- a reader that already opened the base generation may finish using its held
  files and index;
- an invalid update transaction never makes a valid installed base
  unservable.

While candidate pathnames are being published, new readers MUST use the
preserved base receipt or an equivalent generation handle. They MUST NOT decide
this solely from the existence of `.updating`.

The server verifies that the opened archive objects match the chosen index.
It cannot combine the base index with candidate ranges or the new index with
missing ranges.

Update progress and serving are independent. Stopping the UI or updater does
not invalidate already opened readers; stopping the engine terminates the
owned updater according to `LIFECYCLE.md`.

## 12. Restart, cancellation, and a second update

### 12.1 Restart and cancellation

After interruption, cancellation, or failure:

1. the engine records the corresponding run outcome;
2. no automatic retry occurs;
3. `inspect_update` returns the last durable update state;
4. serving uses the last committed generation;
5. explicit Resume continues the same `UpdateId`;
6. explicit Abandon removes only objects exclusively owned by that uncommitted
   update after validating that the base generation is intact.

Cancel preserves valid source, tail, range, inventory, and index receipts.

### 12.2 Second request while an update is uncommitted

There may be only one uncommitted update for an installed base. A new update
request when `Planned` through `IndexReady` already exists MUST offer or perform
one of these explicit actions:

- resume the same `UpdateId`;
- abandon it, then discover a new plan from the still-installed base.

It MUST NOT overwrite the existing update root, mix newly discovered sources
into its tail, or compute identity from a partially mutated candidate
directory.

### 12.3 Second update after commit

Once the first update is `Committed`, a second update uses the new
`GenerationId` as its base and receives a distinct `UpdateId` and root. Pending
cleanup from the first update may proceed independently.

A crash, media failure, or cleanup failure after the first commit MUST NOT make
the second request reuse its old tail, old range plan, old receipts, or old
base snapshot. Generic scratch names are forbidden specifically to prevent
this collision.

## 13. Cleanup and media

After commit, cleanup may remove:

- source checkpoints subsumed by the tail;
- the immutable tail after no resume/verification policy requires it;
- unselected or superseded candidate objects;
- old range objects not referenced by any committed/open/preserved generation;
- the preserved base snapshot after reader/reference ownership permits;
- transaction telemetry and make logs.

Cleanup uses an ownership manifest keyed by `UpdateId`. Failure is represented
as `Committed { cleanup: Failed(...) }`; it does not change the installed
generation outcome.

Optional Kiwix/media work starts from the committed `GenerationId` and follows
the independent media machine in `LIFECYCLE.md`. Media failure does not retain
or resurrect the update transaction and cannot cause the next run to reapply
the tail.

## 14. Progress projection

The shared progress projector combines:

```text
JobLifecycle
InstalledGeneration
UpdateState
live telemetry for the owned `RunId` and update/target identity
MediaState
CleanupState
```

Required phases include:

- discovering updates from base generation `<id>`;
- persisting update plan;
- fetching/parsing named daily or history source;
- assembling sorted tail;
- preserving base generation;
- applying range `n/N`;
- reusing validated range candidate `n/N`;
- validating candidate inventory;
- building title/frame/segment index;
- committing generation;
- installed generation `<id>`;
- installed; cleanup pending/failed;
- installed; media pending/failed;
- interrupted/failed/cancelled; explicit resume required.

Progress reports base, update, tail, and candidate generation IDs in diagnostic
detail. It MUST NOT infer update phase from `.updating`, generic scratch files,
PID-bearing directory names, or prose. It MUST NOT report “running” without the
specific owned activity.

## 15. Invariants

1. Exactly one generation index is installed at a time.
2. Every installed index names exactly one complete, ordered immutable segment
   inventory.
3. Every active update names exactly one verified base generation.
4. Every tail and range candidate names exactly one update.
5. A base generation never changes in place.
6. A range candidate never changes after its receipt commits.
7. Each range slot has exactly one selected segment in a candidate generation.
8. Old and new generations may share immutable segments.
9. The new generation is invisible before the index commit point.
10. The base remains servable until the new generation commits.
11. Commit is idempotent; recovery after commit never reapplies the tail.
12. Resume is idempotent; committed source/tail/range/index targets are never
    rebuilt without an explicit discard.
13. Daily overlap cannot create duplicate logical records.
14. Gaps after the base frontier require explicit full refresh.
15. Update, media, and cleanup outcomes are independent.
16. Failure, interruption, and cancellation never imply retry.

## 16. Invalid combinations

Inspection MUST reject at least:

- update artifacts without an immutable plan;
- a plan whose `BaseGenerationId` is not the base it records;
- a plan discovered from an archive/index pair that do not match;
- a tail receipt for another plan or base;
- a complete-looking tail without a matching receipt;
- a source receipt not named by the plan;
- a preserved base archive and index with different generation IDs;
- a range receipt for another base, update, tail, or slot;
- a range receipt whose candidate is absent, mutable, incomplete, or corrupt;
- two terminal receipts for one slot;
- a missing slot or unconsumed tail interval;
- candidate ranges that overlap, reverse, mix entity kinds, or leave an entity
  outside routing coverage;
- a candidate inventory built from directory enumeration rather than receipts;
- a candidate index whose inventory or generation identity does not match;
- a committed index that names a missing segment;
- a base segment removed before a preserved base can serve it;
- two uncommitted update roots claiming the same mirror;
- an uncommitted update whose base is no longer the installed generation;
- remnants of a committed update mistaken for an active update;
- a second update reusing the first update's generic scratch receipts;
- a marker claiming update activity without the plan and base identity needed
  to interpret it.

Invalid-state handling is non-destructive. Diagnostics name expected and
observed IDs and exact paths. Repair or abandonment is explicit.

## 17. Required transition tests

### 17.1 Pure transition matrix

Test every `(UpdateState, Event)` pair, including:

- scheduled request;
- explicit request;
- explicit resume;
- explicit abandon;
- discovery success, gap, regression, and failure;
- source commit and source failure;
- tail commit and tail validation failure;
- base-preservation commit and failure;
- unchanged range, candidate range, and candidate failure;
- candidate inventory commit;
- index commit;
- generation commit conflict because installed base changed;
- cancel, process failure, engine interruption, and restart;
- cleanup and media outcomes;
- second request before and after commit.

Unsupported pairs return a typed error or explicit no-op.

### 17.2 Discovery and tail failpoints

Inject failure before and after:

- reading/verifying the base index and manifest;
- every upstream discovery request;
- plan temporary write, file fsync, rename, and directory fsync;
- each source checkpoint and receipt commit;
- creation and consumption of external sort runs;
- tail write, completion marker, fsync, receipt write, rename, and directory
  fsync.

After restart, assert the exact state and that a persisted plan's source set
does not change.

### 17.3 Base-preservation failpoints

For both split and single-file archives, fail before and after every:

- hard link or immutable-object reference;
- archive snapshot validation;
- index link/copy;
- preserved-base receipt write;
- file and directory fsync.

At each point, either the installed base remains directly servable or the
complete receipt-bound preserved base does. A half pair is never selected.

### 17.4 Per-range failpoints

For every slot type—unchanged, changed with same bounds, changed with expanded
final bounds—fail before and after:

- opening base segment;
- routing/skipping its tail interval;
- candidate temporary creation;
- every frame/range seal;
- candidate DONE;
- candidate validation;
- immutable candidate publication;
- candidate receipt write;
- obsolete pathname removal;
- directory fsync.

Restart then resumes the same `UpdateId`, consumes each logical tail interval
once, and produces the same candidate identity as an uninterrupted run.

Tests MUST cover a crash with both old and candidate paths present, a receipt
without cleanup, cleanup without marker removal, and a candidate publication
without a receipt.

### 17.5 Inventory, index, and commit failpoints

Fail before and after:

- final slot receipt;
- inventory validation and receipt;
- every index section write;
- index file fsync;
- candidate-index receipt;
- selected segment publication;
- archive-directory fsync;
- installed-index atomic rename;
- installed-generation receipt;
- transaction marker cleanup.

Before the index rename, new readers open the base. After it, new readers open
the new generation. No failpoint may expose a mixed pair.

### 17.6 Second-update matrix

For every first-update state:

```text
Planned
TailReady
BasePreserved
ApplyingRanges
CandidateComplete
IndexReady
Committed(cleanup pending)
Committed(cleanup failed)
```

issue:

- scheduler tick;
- explicit Resume;
- explicit Abandon;
- explicit New Update.

Assert that uncommitted work is never mixed with a new plan, committed work is
never reapplied, and the second committed update uses the first new
`GenerationId` as its base.

### 17.7 Serving concurrency tests

Hold readers open across every range publication and the index commit:

- old readers continue seeing a complete old generation;
- precommit new readers see the old generation;
- postcommit new readers see the new generation;
- no reader sees duplicate, missing, or overlapping ranges;
- server restart at every failpoint chooses the generation dictated by the
  authoritative index/transaction state.

### 17.8 Property and scale tests

Generated plans, tails, partitions, interruptions, and resumes must preserve:

- merge commutativity, associativity, and idempotence;
- strict entity ordering;
- exact candidate-slot coverage;
- equality with a one-shot merge;
- bounded descriptors and memory independent of range/source count;
- destination-local scratch;
- monotonically increasing committed work within one update;
- no cross-base or cross-update adoption.

The matrix includes empty tails, updates touching one range, all ranges, new
maximum page/user IDs, title moves, deletions, suppressions represented as
metadata, history-only updates, content-only updates, and overlapping daily
runs.
