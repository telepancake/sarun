# Sarun formal design bootstrap

Status: bootstrap baseline observed on 2026-08-08. This document is a
working design and traceability baseline, not a verification report. It was
written after independent read-only cartography of the engine, Wikipedia
archive, and storage/runner packages. A subsystem is not considered complete
because it has an enum, a transition helper, or a passing happy-path test.

The existing [system model](SYSTEM_MODEL.md) is the detailed inventory and
gap ledger. The current Wikipedia lifecycle documents are normative where
they say so; older Depot sketches are not a second authority. This document
adds the missing repository-wide decomposition and the
refinement obligations that connect prose, abstract machines, code, and
evidence.

The compact machine-readable coverage ledger is
[`formal/coverage.tsv`](../../formal/coverage.tsv). It is intentionally a
source artifact, not generated output: changing a row requires deciding what
was observed, modeled, implemented, and evidenced.

## 1. Method

The design is built in five separate layers. We do not collapse them into one
large status enum or treat a test fixture as a specification.

1. **Observation.** Record actual entry points, durable files, process owners,
   resource owners, and consumers. Mark facts as observed, inferred, or open.
2. **Contract.** State what users and neighboring machines can rely on:
   identity, authority, commit point, visibility, rejection behavior, and
   resource envelope. Include interruption and malformed-input behavior.
3. **Abstract machine.** Define state dimensions, events, guards, effects, and
   invariants. Independent dimensions stay independent; a projection is not an
   authority. A typed rejection or explicit no-op is part of the machine.
4. **Refinement.** Map durable implementation evidence to abstract state and
   map every production event (including recovery and failure events) to an
   abstract transition. Where the mapping is not established, the ledger says
   `open`.
5. **Evidence.** Use table tests, generated reference-model traces,
   durable-boundary failpoints, restart tests, resource counters, and realistic
   workflows. Claims are limited to the evidence actually run on the stated
   toolchain and platform.

The parent agent owns synthesis and integration. Read-only maps precede
design; an implementation is leased to one owner; an independent verifier
challenges the result; the parent reviews the logical diff and production call
path. Unknowns are preserved rather than filled with a plausible story.

## 2. Product-state decomposition

These are product dimensions, not alternatives in one global state machine.
The authority column is the durable or live owner; projections may report the
state but cannot change it.

| machine | authority and identity | commit/linearization boundary | principal consumers | status |
| --- | --- | --- | --- | --- |
| Engine incarnation | instance lock + namespaced Unix socket; live incarnation has no persistent token | socket is published after lock/FUSE/listener setup (before later network/gateway/scheduler setup); shutdown after child/resource drain and socket removal | control clients, UI, gateway, runner registry | model seed identified; startup rollback and macOS incarnation proof open |
| Mirror registration | `mirrors.db` `jobs` row, `JobId`, destination ownership/delete mode | transactional add/pause/delete marker and final removal | scheduler, CLI, UI, gateway inventory | transitions and ownership tests exist; cross-machine shutdown admission open |
| Mirror run | `mirrors.db` `runs` row, fresh `RunId`, process group; `RUNNING` is ephemeral | insert `starting` before spawn; matching spawn/stop/exit transaction; restart recovery | scheduler, UI telemetry, driver child | pure transition/projection tests exist; spawn races, failpoints, and macOS PID incarnation open |
| Box/runner registration | durable captured box plus live `BoxId`/transport/process maps | `BoxAdded` only after all required resources are owned; EOF removes live registration | control, FUSE, QEMU/SUD/direct backends | local rollback tests exist; total side-effect matrix and macOS identity open |
| Wikipedia build construction | destination-local `PlanId`, target receipts/checkpoints, assembly receipt | synced plan/target receipt; archive/index/generation receipt before publication | Kati/Brush workers, build inspector, resume | inspector and lifecycle relations exist; cleanup ownership and some receipt cuts open |
| Wikipedia update | `UpdateId`, `BaseGenerationId`, `TailId`, range receipts | complete candidate index selector replacement | updater, installer, serving readers | normative update model and tests exist; cleanup-failure representation is incomplete |
| Installed archive | immutable generation directory + `.swtitle` selector, `GenerationId` | durable receipt followed by atomic selector replacement | HTTP serve, gateway, terminal reader, attach/readout | publication/lease code and tests exist; orphan/crash matrix open |
| Archive serving | opened generation snapshot and reader lease; no mutable state | reader binds one selected generation at open | `serve.rs`, `archive_gateway.rs`, `reader.rs` | shared archive reader exists; gateway/in-process equivalence and child lifecycle open |
| Media projection | Kiwix source/packed files and media store; no generation-bound receipt is evident | currently post-install pack attempt, not text publication | page renderer, HTTP media routes, UI | format/read tests exist; generation identity, stale state, deletion, and progress open |
| Backref projection | `.swrefs`/backref builder outputs | currently independent of text selector | category/user/link routes | format tests exist; publication binding/stale state open |
| Legacy Depot/SQLite | `Instance` root, depot index, `meta.db`, sync state | depot/title flush and SQLite transactions | attach/readout, old sync tests, some APIs | retained implementation is observable; relationship to portable authority is unresolved |
| Depot primitive | sparse index chain root; f0/f1/cold frame files; counters are advisory | index flip after frame writes; cold bytes are append-only history | Depot/VBF/stream readers and legacy Wikipedia | index authority is documented; orphan-byte recovery and open-file bounds open |
| IETF mirror | driver-owned lock/database/archive | driver update commit | gateway and attach | driver-local contract exists; engine supervision/refinement open |
| Git mirror/depot | repository/store/meta DB and staged update | driver commit/ref update | attach/checkout/readout | package tests exist; supervisor and cross-driver resource ownership open |
| Capture/blob state | per-box SQLite/sqlar plus loose blobs and RAM caches | SQLite transaction/WAL plus blob publication (durability assumptions differ) | box readout, FUSE, provenance/readers | authority split and crash cuts open |
| FUSE/QEMU/network | live broker/mount/guest/process/socket maps; per-box network stack | resource registration and reverse-order stop/reap | boxes, appliances, net tests | local protocol tests exist; complete crash/reclaim and macOS guest refinement open |
| Resource/build execution | jobserver/slips/Kati/N2/Brush plus guest/host process owners | admission and worker completion | mirror workers, box builds, scripts | unit coverage exists; identity, descriptor, and nested-environment bounds open |
| UI session/projections | terminal session and RPC attachment; never durable job truth | user intent RPC and projection refresh | UI panes/key registry, CLI | projection tests exist; disconnect/degraded telemetry and terminal traces open |

The legacy Wikipedia path is not silently declared equivalent to the portable
archive. `SPEC.md` describes an older Depot design while `sync.rs` implements
additional TSV behavior. Until a deliberate decision retires or bounds that
API, changes must name which authority they affect.

### Data and archive algebra

The portable archive is a typed record stream, not an untyped byte bag. Its
entity key is `(Page | User | Global, id)`, and the canonical order is entity
key ascending, timestamp descending, with state/action records ordered before
revisions/actions for the same entity. `SortedArchiveMerge` assumes each input
already obeys that order. Duplicate records are coalesced by identity; a
revision may fill missing visibility, text, or metadata from the other
occurrence, but conflicting complete text or contributor identity is a typed
conflict. Page and user actions have their own explicit max/union rules. The
merge operation is intended to be commutative, associative, and idempotent.
Existing examples in `archive.rs` cover a small set of these laws; generated
records, conflicts, unknown types, and malformed order are still open.

The direct import machine has a concrete resource contract that belongs in the
formal model. Stage one currently streams source bytes through decompression
and parsing into typed `.swdump` targets; compressed source files are not
retained by that normal path. Whether expensive downloaded inputs should be
retained is a separate ownership/recovery decision, not a consequence to hide
inside this observation. Completed target groups are held in an ordered
producer/consumer sequence and consumed once by assembly. History workers feed
two external sorters with an 8 GiB run target; with three outer workers this
can retain tens of GiB before spill. The sorted merge has a 64-input hard cap,
while another archive merge path uses bounded fan-in. These are observed
implementation facts, not yet validated resource proofs, and must be
represented by counters and scale tests before being advertised as the import
envelope. History
discovery currently records no advertised digest for its TSV partitions, so
the source-integrity assumption is different from content parts and must be
made explicit. The direct final merge also feeds history paths directly into
the 64-input reader; a wiki whose plan exceeds that fan-in is a concrete
scalability boundary, not an abstract model detail.

## 3. Cross-machine contracts

These relations are the first system-wide invariants. They are stronger than
individual local transition tables and are the reason the decomposition is
useful.

* **Incarnation safety:** at most one engine instance owns a namespace; a
  published socket names that instance. A socket replacement, restart, or
  incompatible peer is a typed rejection, not an implicit takeover.
* **Run ownership:** a run completion, cancellation, or recovery action must
  carry the same `RunId` and an ownership-safe process identity. A stale child
  cannot finish a newer run. A deleting job cannot admit a run; engine shutdown
  admits no new run.
* **Authority separation:** durable rows, receipts, selectors, and generation
  manifests determine state. PIDs, stderr tails, progress text, filenames, and
  UI selection are observations or projections only.
* **Construction/publication separation:** a candidate may be incomplete or
  abandoned without changing the installed selector. A reader sees either a
  complete old generation or a complete new generation, never a candidate
  directory assembled in place.
* **Identity closure:** plan, target, tail, range, archive, index, media, and
  backref artifacts must either share the intended generation/update identity
  or be explicitly reported as stale/absent. Names and lengths alone do not
  establish identity.
* **Non-destructive recovery:** malformed or foreign construction evidence is
  not silently adopted. Cleanup is limited to artifacts proven owned by the
  selected operation and requires an explicit transition when it can remove
  user data. Unknown files are preserved and reported.
* **Reader snapshot:** a reader holds a lease on the generation selected when
  it opened. Publication and cleanup cannot invalidate that snapshot.
* **Progress attribution:** every diagnostic and counter has an owning
  `(JobId, RunId, target/phase)` or is clearly aggregate. Losing a subscriber
  degrades observability; it does not alter the durable state.
* **Resource bounds:** worker count, open descriptors, retained decoded data,
  passes, network attempts, and scratch bytes are explicit contracts. A
  streaming implementation is not automatically I/O-efficient, and an
  elapsed-time result does not prove a pass or memory bound.

## 4. Event and fault vocabulary

Each machine must classify each applicable event as a transition, typed
rejection, explicit no-op, or impossible by construction. The common event
alphabet is:

```text
user: create, inspect, start, retry, resume, pause, cancel, stop, update,
      replace-full, abandon-scratch, delete-registration, delete-data, browse
admission: due, paused, capacity-granted, capacity-unavailable, duplicate-root
process: spawn-ok, spawn-failed, progress, clean-exit, nonzero-exit, signal,
         pipe-eof, panic, stale-completion, pid-reuse, watchdog-timeout
filesystem: create, short-write, fsync-ok, fsync-failed, rename, missing,
            malformed-receipt, foreign-identity, contradictory, no-space,
            descriptor-exhaustion
source: discovery-change, timeout, disconnect, retry-after, 4xx, 5xx,
        malformed-UTF8/XML/TSV, robots/siteinfo result
engine: startup, attach, UI-disconnect, crash, restart, orderly-shutdown,
        incompatible-socket
publication/serving: receipt-ready, selector-commit, reader-open/close,
                     selector-missing, generation-missing, sidecar-stale
```

The fault model includes interruption before/after every database commit,
write, `fsync`, rename, receipt, process registration, process spawn, signal,
and selector replacement. It also includes duplicate/reordered/late child
events, process disappearance, partial source transfers, malformed records,
no-space, descriptor exhaustion, and lost UI subscribers.

## 5. Initial executable models

The first bounded models are in [`formal/`](../../formal/):

* `mirror_run/MirrorRun.tla` models the durable run identity, scheduler
  admission, spawn handoff, stop reasons, restart interruption, and stale
  completion no-op. It intentionally does **not** yet model due timestamps,
  deleting registration, process-group disappearance, spawn transaction races,
  or the macOS incarnation token; those are explicit gaps, not hidden
  assumptions.
* `generation_publication/GenerationPublication.tla` models private candidate
  construction, receipt/index prerequisites, atomic selector commit, reader
  snapshots, and abandon. It treats receipt/index as abstract booleans and
  does not yet model crash/reopen or generation-content validation.

`make check-models` is the first formal gate. It performs SANY/TLC checking
only when a user supplies a pinned TLC jar and Java runtime; it never downloads
one. The checked-in bounds are smoke bounds, not scale evidence. The exact
toolchain, model version, invariants, and any skipped evidence must be recorded
with a change.

The next model seeds are deliberately separate machines:

* **Depot:** `Prepared(root) -> append immutable cold/f1/f0 -> publish index
  root -> Visible`; collect is a separate relocation transaction. Readers
  resolve only the published root, and counters never define reachability.
* **IETF:** `Locked -> discover -> append archive -> commit metadata rows ->
  Visible`, with explicit 404/missing and 304/no-change paths. A crash after
  append and before metadata commit currently leaves an unmodeled orphan.
* **Box:** persistent `Discovered(sqlar/meta)` and live `Registered(handles)`
  are independent axes; registration publishes all live resources only after
  setup, while runner EOF tears them down without deleting captured state.
* **Git:** legacy reverse-delta and union storage are separate machines, not
  states of one “git depot” model; each needs its own object/index/commit
  authority before any refinement claim.

The intended portfolio is deliberately mixed:

| question | tool/technique | first target |
| --- | --- | --- |
| bounded concurrent durable protocol | TLA+/TLC | run + selector models |
| stateful data laws and merge idempotence | Rust reference model + property testing | archive/update tail merge |
| small synchronization interleavings | Loom | supervisor gates and reader leases |
| bounded pure parser/codec obligations | Kani where available | receipt/index codecs |
| executed unsafe paths | Miri where available | selected storage/read paths |
| hostile parser/decoder input | `cargo fuzz` | XML/TSV/raw archive/frame readers |
| crash cuts | test-only failpoints + child kill/reopen | build/update/install boundaries |
| resource envelope | counters, descriptor audits, profilers | import/update/serve |

No such external formal tools are installed in the current environment. That
absence is recorded rather than worked around with an unpinned download or a
claim of verification.

## 6. Traceability and closure ledger

The following is the minimum evidence required before a machine can be called
closed:

1. a pure inspector and transition API are the production path;
2. every durable fact has one authority and every consumer is listed;
3. generated traces compare implementation transitions with an independent
   reference model, including duplicate/stale/reordered events;
4. failure injection covers every stated commit boundary and restart resumes
   from receipts without guessing;
5. resource counters establish the stated pass, memory, descriptor, network,
   and scratch bounds at a representative input;
6. at least one realistic end-to-end workflow exercises each production
   adapter; and
7. unresolved behavior is an `open` row, not a prose footnote.

Current open rows from the cartography are:

| boundary | concrete counterexample or missing proof | next evidence |
| --- | --- | --- |
| engine startup | failure after lock/FUSE/listener/gateway/scheduler setup has no total rollback matrix | failpoints + restart/replace tests |
| macOS process ownership | PID/PGID fallback is liveness, not incarnation proof | platform token/parent identity design + adversarial test |
| mirror run | DB/spawn/live-map stop race and child disappearance | generated traces + durable-boundary failpoints |
| gateway | ephemeral port reservation TOCTOU; child lifecycle lacks E2E tests and generation binding | owned listener/child model + lifecycle test |
| UI | subscriber failure can be silent; stderr retains only a short tail | degraded projection contract + terminal trace tests |
| full-build cleanup | `inspect_build_for_start` may abandon foreign/unsupported scratch through broad `clear_mirror_scratch`; this conflicts with the non-destructive lifecycle contract | ownership manifest, explicit abandon, preservation test |
| update cleanup | `Cleaned`/cleanup failure is in the relation/docs but not durably represented by `UpdateState`; cleanup errors can become invisible after selector commit | cleanup receipt/state and crash tests |
| publication | pending selector/orphan generation/crash windows are not fully tested | failpoints around receipt, rename, fsync, selector |
| media/backrefs | sidecars are optional and filename-discovered, not evidently generation-bound | shared identity/receipt and stale-state tests |
| history-source scale | history partitions have no advertised digest and the direct final merge has a 64-input cap | explicit source-integrity policy and bounded fan-in plan/test |
| portable vs Depot | old `Instance`/SQLite authority remains alongside portable `.swtitle` | explicit scope decision and adapter/refinement contract |
| Depot/IETF publication | frame/archive bytes are written before index/SQLite publication; orphan bytes and index flips lack crash/reopen evidence | immutable-frame model + failpoints + space/reclamation accounting |
| box capture | SQLite rows, loose blobs, and RAM caches have different durability assumptions (`synchronous=OFF` is visible in code) | blob-before-reference contract + restart/failpoint tests |
| Gitdepot | legacy reverse-delta path and union path are separately exposed and their designs disagree | two explicit models and a scope/refinement decision |
| FUSE/QEMU/network | broker, mount, guest, process, and socket facts are independently live; macOS has a different network boundary | resource ownership graph + crash/reclaim tests |
| box/resources | registration side effects require a total reverse-order rollback proof | cross-product failpoint matrix |
| network/build resources | ownership, fairness, descriptor limits, and nested environment isolation are partly incidental | instrumented resource model and stress tests |

The cleanup row is intentionally a counterexample: existing tests that expect
foreign scratch to be deleted encode current implementation policy, not proof
of the user-safety contract. The next change must reconcile code, tests, and
prose instead of adding another exception.

## 7. Work sequence

The next engineering passes should proceed in this order:

1. reconcile the cleanup contract and add an ownership manifest without
   deleting unknown artifacts;
2. add model-to-code trace tests for the mirror run and selector publication,
   then add durable failpoints;
3. settle engine incarnation and gateway child ownership on macOS;
4. bind media/backrefs to generation identity or explicitly expose their stale
   state;
5. decide the portable/Depot authority boundary and mark legacy APIs;
6. add resource counters and representative scale checks; and
7. only then extend the models to update ranges, boxes, IETF, Git, and the
   complete engine shutdown graph.

Each pass updates this ledger and the relevant domain specification. No agent
may claim “the system is formally designed” while an inspected boundary is
absent from the ledger.
