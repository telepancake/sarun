# Sarun system model

This is the council's reconciled architecture map for the repository as of
2026-08-05. It is the working contract for the next engineering pass. It is
deliberately about ownership and observable transitions, rather than about
which source file happens to contain a function.

The council inspected the engine, the mirror drivers, the Wikipedia archive
and serving paths, the build tools, and the repository's test entry points.
The execution and data councils did their inventories independently and the
tables below record the agreement and the disagreements that remain. A cell
marked **open** is not an undocumented assumption; it is work that is not yet
safe to call complete.

## Council method

Three independent passes were requested: repository cartography, execution and
control, and archive/data/serving. The cartographer enumerated packages,
entrypoints, and persistent roots; the execution council traced process and
resource ownership; the data council traced source, construction, publication,
and serving. They reported code paths and missing tests before seeing one
another's conclusions. Reconciliation then assigned each artifact to one
owner and kept disagreements as explicit gaps.

The strongest common finding is that local state tables were mistaken for a
complete design. The run table and the archive installation table are each
reasonable in isolation, but their handoffs (spawn/stop/restart and scratch /
candidate / selector) were not total. This document therefore treats every
handoff as its own event-bearing boundary and refuses a “green” subsystem
claim until its inspector, transition function, and failure tests are wired
into production paths.

## Repository decomposition

| boundary | implementation | owns | does not own |
| --- | --- | --- | --- |
| engine instance | `engine/src/main.rs`, `control.rs`, `gateway.rs`, `sarunfs/`, `net/` | control socket, FUSE broker, box and service channels, engine shutdown, live process/resource registries | mirror build meaning or archive publication |
| UI client | `engine/src/ui.rs` (with the prototype test client) | user intent and projection of inspector output | durable job/build state, process truth, filesystem interpretation |
| mirror supervisor | `engine/src/mirrors.rs` | mirror registration, one `RunId`, admission, driver process group, stop/retry/delete requests, job projections | source parsing, archive format, generation installation |
| box lifecycle | `engine/src/box.rs`, `sarunfs/`, `appliance.rs`, `sud.rs` | registration, capture, transport, process tree, QEMU/SUD/direct backends | mirror job rows and archive generations |
| resource scheduling | `engine/src/slippool.rs`, `jobserver.rs`, `n2run.rs`, `katirun.rs`, brush | bounded slips, nested build workers, make/ninja execution and cancellation | ownership of a mirror generation |
| Wikipedia source/build | `gimir/wikimak/mediawiki/`, `wikipedia/src/{cli,direct,build_lifecycle,update_lifecycle}.rs` | discovery, fetching, parsing, typed plans, target materialization, assembly and update candidates | publication selector and running UI |
| archive publication | `wikipedia/src/installation_lifecycle.rs`, `generation.rs`, `archive_set.rs` | immutable generations, selector, install receipt, reader leases, cleanup | source downloads or optional media generation |
| archive serving | `wikipedia/src/serve.rs`, `archive_browse.rs`, `title_index.rs`, `frame_directory.rs` | page/revision/title lookup and HTTP responses from a selected generation | building or repairing that generation |
| auxiliary projections | `backrefs.rs`, `media/`, `.swrefs`, `.media` | explicitly requested relationship and image projections | text-generation publication (currently — this is a defect to resolve) |
| other archive drivers | `gimir/gitdepot/`, `ietf-mirror/`, `depot*`, `strpool/` | Git/IETF mirrors and reusable storage primitives | engine process supervision unless invoked through it |
| host tools/appliances | `tv/`, `viros/`, `cellulose/`, `prototype/`, scripts/vendor | target appliances, debugger/RouterOS artifacts, test harness and build inputs | Sarun's runtime authority |

The build workspace contains the engine binary (`sarun`) and the drivers
`wikimak`, `ietfmak`, and `gitdepot` (the drivers are multi-call entry points
in the engine release). The gimir workspace also contains the depot, stream,
cache, VBF, string-pool, MediaWiki, Wikipedia, wikitext, Scribunto, and media
crates. These are separate implementations even where names in old documents
make them look like one product.

The non-Cargo surfaces are part of the map too: `prototype/` is the engine
test/support harness, `scripts/` supplies vendor/build/code-generation steps,
`tv/` builds the native tracing/SUD helpers, `viros/` is the QEMU/RouterOS
lab, `cellulose/` is a browser experiment, and `sakar` is a separate network
interceptor. They do not own Sarun runtime state.

## Persistent artifact inventory

The following inventory is intentionally concrete. A path is not an authority
just because it exists; each entry names its owner and whether it may be
recreated.

| artifact family | owner | kind | recovery rule |
| --- | --- | --- | --- |
| namespaced XDG `data`, `config`, `state`, `runtime` roots | engine instance | durable configuration/state or runtime | namespaced by `SLOPBOX_NS`; runtime sockets/mounts are disposable |
| `state/mirrors.db` (+ WAL) | mirror supervisor | authoritative jobs/runs | inspect transactionally; do not infer from child PIDs |
| `state/<box>.sqlar`, `state/live/<box>/`, blob/trace/flow trees | box lifecycle | authoritative captured box plus live construction | delete only through box-data transition; live process ownership must be settled first |
| `state/cache/blob`, `cache/tree` | engine cache | derived immutable cache | rebuildable after validation |
| Wikipedia destination selector `X.swtitle` | installation lifecycle | authoritative text-generation choice | atomically replace; readers lease selected generation |
| `X.generations/<id>/` and install receipts/pending selector | installation lifecycle | immutable candidate/committed data and recovery evidence | inspect before cleanup; orphan handling is open |
| Wikipedia `scratch/` plans, nodes, checkpoints, partials | Wikipedia build lifecycle | recoverable construction state | explicit abandon/reset; never silently adopt foreign identity |
| `X.swrefs`, `X.media` | auxiliary builders | derived sidecars | currently independent; must gain generation identity or explicit stale policy |
| IETF `.lock`, `meta.db`, archive frames | IETF driver | driver authority | driver-owned lock and typed update recovery |
| Git `repo.git`, `store/`, `meta.sqlite`, tree lanes, staging | Git driver/depot | driver authority plus construction scratch | lock, identity, and staged update commit must remain inside driver |
| `wikimak-depot` `format`, `index`, `f0`, `f1`, `cold` | depot primitive | archive storage primitive | consumers must not mistake counters/caches for authority |

Engine runtime sockets (`ui.sock`, FUSE broker, service sockets), API shadow
files, appliance resolvers, and OCI temporary directories are not durable
mirror state. Their cleanup belongs to the engine owner that created them.

## Entry-point map

All user-visible entry points must eventually name one owner and one request
event. The important paths are:

| entry point | dispatch | owner |
| --- | --- | --- |
| `sarun` / `sarun serve` / `sarun engine` | engine startup and UI/control socket | engine instance |
| `sarun run`, box actions, OCI verbs, oaita | control handlers and child/box machines | box/OCI/oaita owner |
| mirror UI/CLI actions | `mirrors.rs` -> embedded driver child | mirror supervisor, then driver |
| `wikimak` (`fetch`, full refresh, serve/readout) | Wikipedia driver CLI | Wikipedia build/publication/serving owner |
| `ietfmak` | IETF driver CLI | IETF driver |
| `gitdepot` | Git mirror/readout CLI | Git driver |
| `make`, `ninja`, brush/Kati | build executor paths | resource/build invocation owner |
| QEMU/SUD/appliance helpers | `viros`, `tv`, engine appliance paths | box/appliance owner |

Convenience symlinks and argv[0] dispatch do not create a second lifecycle;
they are alternate spellings of the same owner.

## Authorities and projections

There must be one owner for each item of durable truth. The current map is:

| durable evidence | authority | projections/consumers | status |
| --- | --- | --- | --- |
| engine control socket and live maps | engine instance | UI, control clients | live-only; crash recovery is limited |
| `jobs`/`runs` rows | mirror supervisor | scheduler, UI, CLI | durable, but split from the live owner map |
| `RunId` + process group | mirror supervisor | stop/recovery/telemetry | recovery now attempts incarnation-checked termination/reaping; macOS token remains open |
| `scratch/plan.json`, target receipts/checkpoints | build lifecycle | workers and assembly | construction evidence, not published data |
| archive segments + title projection under a generation | generation installer | serving and reader leases | candidate until selector commit |
| stable `.swtitle` selector | installation lifecycle | all production serving | sole text publication selector |
| `install.json` and pending selector | installation lifecycle | inspector/recovery | recovery evidence, never serving authority |
| progress projection | progress owner | UI/status | telemetry only; must not infer runtime state |
| `.swrefs` and `.media` | auxiliary builders | category/user/image routes | **open:** not generation-bound or publication-atomic |
| old Depot/SQLite `Instance` paths | legacy archive code | old tests and some serving paths | **open:** parallel semantics must be retired or explicitly isolated |

Several repository documents are historical evidence rather than current
authority: `AUDIT.md` contains the earlier engine audit and old test counts,
`engine/DESIGN.md` mixes current Rust behavior with prototype wording, and
older gimir scoping/specification notes describe the Depot path as if it were
the only Wikipedia store. This model and the code inspectors decide current
behavior. Updating a document does not justify keeping an obsolete code path.

The important distinction is construction evidence versus authority. A
malformed private plan is not a malformed installed generation. The inspector
must report the former without mutating it, and an explicit abandon/reset
transition must exist for it; ordinary retry must not wedge merely because a
temporary schema changed during development.

## Machines and orthogonal state

### Engine instance

The instance has an ownership state (`not-started`, `starting`, `serving`,
`stopping`, `stopped`, `failed`) and a separate child-resource inventory.
Socket binding, FUSE mount, gateway, supervisor, and UI attachment are
separate milestones. A shutdown request is not complete until the owner has
drained or terminated mirror drivers, boxes, service children, and transports.

### Mirror run

The durable run axes are:

```text
registration: active | deleting
scheduling: enabled | paused
runtime: idle | starting | running | stopping
outcome: never | succeeded | failed | cancelled | interrupted | invalid
```

`RunId` identifies the attempt and `process_group` identifies the child it
owns. The valid runtime transitions are:

```text
idle + explicit start       -> starting
idle + due scheduled start  -> starting
starting + spawn success   -> running
starting + spawn failure   -> idle + failed
starting/running + cancel  -> stopping(user)
starting/running + shutdown-> stopping(shutdown)
running + exit 0           -> idle + succeeded
running + exit != 0        -> idle + failed
stopping(user) + exit/restart -> idle + cancelled
stopping(shutdown) + exit/restart -> idle + interrupted
```

Every completion event carries the `RunId`; a late child cannot complete a
newer attempt. Pre-spawn ownership failures now terminalize the current run,
spawn-after-stop signals the durably recorded group even if the live owner map
has already been removed, and restart recovery attempts safe termination and
reaping before publishing an interrupted/failed outcome. The remaining gap is
an incarnation token strong enough for macOS PID/PGID reuse, plus failpoint
coverage for every spawn/stop/shutdown interleaving.

### Build construction

The construction axes are:

```text
source plan: absent | discovered | fetching | materialized | invalid
targets: absent | partial | complete | failed
assembly: absent | partial | complete | failed
candidate: absent | archive-ready | index-ready | receipted
```

Target receipts bind source identity and plan identity. Assembly consumes only
valid target receipts. Invalid or foreign construction evidence is reported by
the inspector. **Abandon invalid construction** is an explicit event: it
removes only scratch belonging to the selected destination/operation while
preserving an installed generation. Normal full fetch uses it for classified
malformed/foreign temporary evidence, and `wikimak reset` exposes the
deliberate cleanup transition. Any candidate-like
files inside that rejected operation are discarded with its scratch; they are
not adopted through a compatibility path. Ambiguous I/O and contradictory
evidence remain visible errors.

### Installation and serving

Publication is a one-way commit:

```text
candidate -> receipt durable -> selector atomically replaced -> committed
```

Readers opening before or during the replacement keep the old generation;
readers opening after it use the new selector. Cleanup is a separate lease
machine and may remove old generations only after the last reader lease ends.

The selector and generation are authoritative for text. A missing generation,
or an orphan generation/pending selector after a crash, is an inspector state
that needs a typed repair/abandon transition; it is not permission to guess.

### Auxiliary data

Backrefs and media currently have independent files and lifecycles. They can
therefore lag a committed text generation, survive mirror deletion, or be
silently ignored when malformed. Until they carry a generation identity and a
defined publication policy, their routes must expose “absent/stale/building”
distinctly from “valid empty.”

### Box and build-resource machines

Box registration is a transaction over overlay, process maps, transport, API,
network, and optional QEMU/SUD resources. The current code inserts these in a
sequence but has no single typed registration phase, so each failure after a
side effect requires an explicit cleanup proof. Resource scheduling has a
similar issue: host PID polling is not an incarnation-safe owner identity on
macOS, guest slips are not reaped while a box remains live, and in-process
brush/kati/n2 invocations share process-global environment/cache state.

## Event catalogue

The following events are the minimum common vocabulary. Each machine must
classify every event as a transition, typed rejection, explicit no-op, or
impossible by construction.

| class | events |
| --- | --- |
| user intent | create, inspect, start, retry, resume, pause scheduling, cancel, stop engine, abandon scratch, replace full mirror, delete registration, delete data, attach, detach, browse, update |
| admission | due, paused, capacity granted, capacity unavailable, duplicate destination |
| child/process | spawn success/failure, progress, clean exit, non-zero exit, signal exit, pipe EOF, panic, stale completion, PID reuse |
| filesystem | create, short write, fsync success/failure, rename, missing artifact, malformed receipt, foreign identity, contradictory artifacts, no space, descriptor exhaustion |
| network/source | discovery change, missing part, timeout, disconnect, 429/Retry-After, 4xx/5xx, malformed UTF-8/XML/TSV, robots/siteinfo result |
| engine | startup, attach, UI disconnect, crash, restart, orderly shutdown, incompatible socket replacement |
| serving | reader before/during/after publication, selector missing, generation missing, sidecar stale, request for historical revision |
| time | retry deadline, stale heartbeat, watchdog timeout, lease expiry |

The current implementation has tests for many happy-path enum transitions but
not for the cross-product of these events with interruption boundaries.

## Transition closure ledger

This is the first executable-design ledger, not a list of UI labels. Each row
names the durable fact, the event guard, the side effect, and the test or gap
that makes the transition reviewable.

| machine | state + event | guard | result and commit point | coverage / open work |
| --- | --- | --- | --- | --- |
| engine | `not-started + start` | socket/runtime names are free | bind/register owned resources, then publish `serving` | startup tests; partial-start cleanup matrix open |
| engine | `serving + stop` | caller owns instance | stop accepting work, drain/terminate children, remove runtime resources, then `stopped` | shutdown tests; QEMU/SUD/guest drain open |
| mirror run | `idle + start` | registration active and no newer live run | insert `starting` RunId before spawn | transition tests pass |
| mirror run | `starting + spawn failure` | current RunId still newest | terminalize `failed` (or requested cancel outcome) in the same durable owner | pre-spawn regression test passes |
| mirror run | `starting/running + cancel` | current RunId matches caller | persist `stopping`, signal its group, await matching exit | spawn-after-stop test; all interleavings open |
| mirror run | active row + engine restart | persisted group is safe to signal | terminate/reap group, then terminalize row; diagnostics do not stop other rows | restart regression test passes; macOS incarnation token open |
| build | `unplanned + fetch` | destination lock held | write plan identity before source work | full-build tests; source failure matrix open |
| build | invalid scratch + normal fetch | inspector classifies malformed/foreign | remove the rejected operation's entire scratch, recreate plan; installed selector untouched | reported wedge regression passes |
| build | invalid/ambiguous scratch + reset | explicit reset; contradictory evidence still refuses | clear classified private scratch; never adopt it | reset tests pass; receipt-boundary failpoints open |
| update | `planned + worker success` | plan/source/base identities match | publish tail receipt atomically | update phase tests |
| update | invalid active update + fetch | selector is classified malformed/foreign | discard the rejected update's private output and create a new update plan | invalid-update discard regression passes |
| publication | candidate + commit | archive, title index, receipts bind one GenerationId | durable receipt, selector replacement, directory sync | selector tests; crash-window matrix open |
| publication | selector/generation mismatch + inspect | no repair authority inferred | report invalid state; never serve guessed data | inspector exists; repair transition open |
| serving | reader + publication | lease acquired on selected generation | serve old or new complete generation, never a partial tree | generation tests; sidecar generation binding open |
| auxiliary | media/backrefs + text commit | projection identity matches text generation | publish together or expose `stale/building/absent` | current files are independent; binding design open |
| box/resource | registration + side-effect failure | phase owns each inserted resource | rollback in reverse order, then durable failure | existing tests are local; total rollback matrix open |

The ledger is deliberately small enough to review. A subsystem is not
“specified” merely because its happy-path row exists: every open cell needs a
failure-injection test or an explicit decision that makes the event impossible.

## Reconciled gap list

These are the gaps that both domain councils found or that one found at a
boundary the other does not own:

1. **Run ownership loss (partly fixed in this pass):** pre-spawn database
   errors now terminalize the current `RunId`; restart recovery attempts safe
   process-group termination/reaping before terminalizing every row and keeps
   scheduling after per-row diagnostics; spawn-after-stop signals directly
   from its durable group identity. Remaining work is a macOS incarnation
   token stronger than a PID/PGID plus failpoint coverage for every shutdown
   interleaving.
2. **Invalid construction recovery (fixed for the reported wedge):** malformed
   full-build and update scratch now have a typed abandon transition. Normal
   fetch automatically uses it for temporary malformed/foreign evidence and
   discards the rejected operation's private output; `wikimak reset` is
   available for deliberate cleanup. I/O and contradictory states remain
   visible errors.
3. **Publication orphan recovery:** crash windows around generation rename,
   pending selector, receipt, selector replacement, and directory fsync are not
   all inventoried or tested.
4. **Auxiliary publication:** media/backrefs are not generation-bound; deletion
   omits media; stale or invalid sidecars are not represented distinctly.
5. **Two archive semantics:** direct archive history, all-pages, as-of site
   configuration, and old Depot paths do not have one pinned contract.
6. **Registration/resource rollback:** box/QEMU/SUD setup and service startup
   lack a total phase/cleanup model; UI shutdown and engine replacement have
   untested ownership races.
7. **Build executor isolation:** nested make/ninja/brush invocations share
   process-global cwd, flags, environment, or caches; PID/slip identity is
   weaker on macOS and for guest processes.

## Current coverage matrix

This is the comparison ledger the councils used. “Existing” means a test was
found; it does not mean the machine is complete.

| machine | existing executable coverage | missing closure |
| --- | --- | --- |
| mirror run projection | `transition_run` matrix, pre-spawn terminalization, process-group recovery, cancel/stale/restart tests in `engine/src/mirrors.rs` | macOS incarnation token, failpoints for every spawn/stop/shutdown interleaving, panic/pipe lifetime |
| engine startup/shutdown | control/UI unit tests and `prototype/test_*_rs.py` socket workflows | partial startup, incompatible replacement, orderly drain of every child/resource |
| box registration | FUSE/backend/QEMU/SUD prototype suites | rollback after each insertion, guest EOF/worker death, concurrent shutdown |
| slips/jobserver/build executors | `slippool` unit tests, Kati corpus, integration builds | PID incarnation, guest slip death, nested concurrent make/cache/cwd isolation |
| full-build inspector | target/event tests plus malformed-plan abandon/discard tests in `build_lifecycle.rs` and `cli.rs` | failpoints for every receipt boundary; contradictory evidence is intentionally a typed refusal |
| update inspector | update phase matrix, receipt identity tests, and invalid-update preservation test | publication crash windows, orphan candidate repair |
| generation installation | selector/generation/reader-lease tests | pending selector/orphan generation matrix, selector-unchanged missing generation |
| direct serving | `serve_e2e`, archive/title/frame tests | history parity, all-pages indexed path, as-of siteinfo/sidecar semantics |
| backrefs/media | format/unit tests and selected render fixtures | generation binding, stale/invalid distinction, deletion cleanup, image resolution |
| Git/IETF/depot | crate equivalence/update/mirror tests | supervisor crash/restart and cross-driver resource ownership |

The missing column is the immediate source of implementation tasks. When a
row is closed, add the failpoint or workflow test and remove the corresponding
gap from the model; do not merely append a “fixed” paragraph elsewhere.

## Performance envelope

Construction may deliberately make multiple sequential passes when they buy a
bounded descriptor set, simple recovery, or a required index. Passive serving
must not decode or hash the archive. The current intended envelopes are:

```text
initial import: each source and sorted intermediate read once; final archive
                and projected index written once
update:         selected tail and affected ranges read once; unchanged ranges
                neither decoded nor rewritten
startup/UI:     jobs, receipts, selectors, indexes, bounded headers only
serving:        one indexed frame/page lookup for a requested revision
```

The implementation must instrument these claims at scale: bytes opened and
read, bytes rewritten, passes, peak retained buffers, open descriptors, fsyncs,
and work performed by inspectors and UI refresh. A small elapsed-time test is
not evidence that a full archive scan is acceptable.

## Test model and completion gate

For every machine, tests are derived from the map rather than from the current
keys or happy paths:

1. table-test every state/event cell, including typed no-op and rejection;
2. run generated event sequences against a pure reference model;
3. inject failures before and after every write, fsync, rename, receipt, and
   process-registration boundary, then restart and inspect;
4. deliver duplicate, reordered, stale, and late child events;
5. exercise start/cancel/retry/delete/shutdown/publication concurrently;
6. assert byte-pass, memory, descriptor, and fsync bounds with instrumented
   readers/writers;
7. run at least one realistic workflow for each production path.

The repository is not yet at the final gate. The following are true today:

- engine and Wikipedia unit/integration suites pass at the last audit;
- the selector/generation path is the production text authority;
- the council has identified the missing machines and transition classes;
- the gaps above still need executable transition/failpoint tests and, where
  required, implementation.

We may claim a machine complete only when its inspector and transition API are
the production path, the listed tests pass, and remaining deviations are
explicitly recorded here. This document is therefore a map and a work queue,
not a claim that the whole repository is already specified.
