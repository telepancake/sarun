# Parent and agent work protocol

This document turns `AGENTS.md` into a practical operating method for an LLM
that delegates work. It is meant to be usable in other repositories after
substituting their local instructions and tools.

It is not a pipeline that mechanically produces good software. The parent
agent remains responsible for understanding the problem, choosing appropriate
depth, reconciling conflicting evidence, and judging the result. Delegation
increases the amount of evidence available; it does not transfer
accountability.

## 1. The parent owns the outcome

The parent agent must itself:

- read the applicable instruction files completely;
- understand the user's objective, authority, and constraints;
- inspect the baseline repository and external state;
- decide what may be delegated and what must remain centralized;
- reconcile agents' assumptions and designs;
- inspect every complete logical change and its production call paths;
- inspect generated or bulk artifacts through their generator, input/output
  manifest, reproducibility check, and representative output rather than
  pretending to review every generated line;
- run or reproduce the decisive checks, or mark the corresponding claim
  unverified with the exact blocker;
- control staging, commits, destructive actions, and external publication;
- state what remains unknown or unverified.

An agent report is testimony, not proof. “The agent says it passed” is not a
substitute for reading the relevant source, diff, model, and command output.
The parent may delegate command execution, but it must understand why the
command is relevant and verify the resulting artifact or result. A delegated
result records toolchain, platform, relevant environment, and input identity;
agent output alone cannot upgrade a blocked parent check to “complete.”

The parent must not delegate interpretation of repository instructions and
then rely on a summary. An agent can audit an interpretation, but the parent
must read the source instructions.

## 2. Open a work record

Before mutation, the parent creates a compact live work record in its plan or
working context. Do not commit a diary for routine work. If the work spans
sessions or establishes lasting architecture, put the durable contract and
rationale in the repository's design documentation.

The work record contains:

```text
objective:
user-visible acceptance:
authority and forbidden actions:
baseline commit/worktree/dirty files:
external processes and data in scope:
observed current behavior:
desired contract:
assumptions and fault model:
scale and resource budget:
affected ownership/change boundaries:
agent leases:
evidence required:
open questions and stop conditions:
```

For a repository-wide audit, add a parent-owned coverage ledger. Inventory
components, state owners, interfaces, build/runtime paths, and external
systems. Mark each `observed`, `modeled`, `implemented`, `evidenced`,
`out-of-scope(reason)`, or `open`. Completion means every inventory entry and
important edge is inspected or explicitly excluded; success in several
well-mapped corners says nothing about uninspected ones.

For each authoritative fact, the ledger also names every consumer and
projection—UI, CLI, scheduler, recovery, cleanup, serving, and build logic as
applicable. A target contract defines their common interpretation and evidence
that they agree. Do not claim that this consistency already exists merely
because the ledger calls for it.

Depth follows consequences, not ceremony:

- A local reversible change may need only a contract, scoped patch, focused
  test, and diff review.
- Persistent state, concurrency, background work, protocols, large data, or
  recovery normally require an explicit behavioral model, failure assumptions,
  and independent review.
- Destructive or externally published work additionally requires exact target
  inventory, authority, recovery analysis, and a final review immediately
  before acting.

The record changes when evidence changes. It is not a promise to preserve the
first design.

## 3. Delegate questions, not vague responsibility

Every delegated task is a bounded question with an output contract. Its prompt
must include:

- objective and relevant user requirements;
- baseline commit and worktree;
- files and external resources in scope;
- whether the task is read-only or may edit named paths;
- assumptions already established and questions still disputed;
- forbidden actions;
- evidence and report format expected;
- who owns integration and cleanup.

Useful roles include:

- **cartographer:** establishes current behavior, authority, artifacts,
  consumers, and contradictions;
- **modeler:** states the abstract contract, events or laws, assumptions,
  invariants, commit points, and costs;
- **designer:** compares concrete representations against that contract;
- **implementer:** changes only a leased implementation surface;
- **adversary:** searches for omitted events, counterexamples, scale failures,
  and unjustified claims;
- **verifier:** independently relates the implementation to the contract and
  executes the selected evidence.

These are roles, not permanent agent identities, and not all are needed for
every change. For consequential work, the implementer should not be the only
author of the model or the only verifier. Independent review matters because
an implementation and its tests can share the same misconception.

The parent owns shared interfaces, architecture boundaries, generated files,
manifests, and final integration unless it explicitly leases them. Do not ask
two agents to solve “the whole thing” and then choose the more confident
answer.

### Default structure for consequential work

1. The parent reads instructions, captures the baseline, and writes the work
   record.
2. Read-only agents examine separable concerns in parallel—for example current
   behavior, data/authority, failure/concurrency, and performance. Their scopes
   may overlap for independent observation, but their questions must differ.
3. The parent compares their evidence, resolves terminology and ownership, and
   writes one provisional contract. Disagreements remain explicit until
   resolved; majority vote is not resolution.
4. A designer and adversary examine alternatives and counterexamples. The
   parent selects a design and the evidence required to accept it.
5. One implementer owns each mutable surface. Implementation can be parallel
   only where leases and contracts are independent.
6. A verifier who did not author the change checks the contract-to-code
   correspondence and runs the decisive evidence.
7. The parent reads and integrates the combined diff, reruns decisive checks,
   inspects final state, and reports bounded claims and remaining uncertainty.

Collapse these steps for simple reversible work. Expand or repeat them when
new evidence changes the abstraction.

## 4. Make knowledge and claims inspectable

Agent reports separate six categories:

- **Observed:** source, path, command, output, or runtime fact.
- **Inferred:** conclusion drawn from observations, with assumptions.
- **Proposed:** desired semantics or design not yet implemented.
- **Implemented:** exact files and behavior changed.
- **Executed:** exact validation command, environment, and result.
- **Open:** unverified claim, conflict, skipped check, or remaining risk.

The categories may coexist. “Implemented” does not imply “executed”;
“executed” does not imply that the right property was tested.

Strong words require matching evidence:

| Claim | Minimum supporting evidence |
| --- | --- |
| fixed | prior failure or violated property, relevant change, and a regression/property check that distinguishes old from new behavior |
| complete | an explicit bounded inventory, exclusions and omissions, plus a reason that boundary is closed |
| atomic or crash-safe | publication/transaction contract, platform assumptions, interruption model, failpoint or equivalent evidence at named commit cuts, and restart inspection |
| resumable | persisted progress semantics plus interruption and replay without loss, duplication, or forbidden repeated work |
| thread-safe or race-free | synchronization argument plus stated actor/schedule bounds and systematic interleaving evidence where feasible; unmodeled OS actors remain open |
| bounded | a formula or invariant naming the bound and an instrumented check |
| faster | comparable workload, cache state, environment, counters, variance, and repeated measurement where relevant |
| model-checked | committed model/configuration, environment assumptions, abstraction mapping, checker version, bounds, properties, command, and successful result |
| proved | theorem, assumptions, proof artifact, checker, and the connection to the implementation |
| no user data affected | bounded before/after manifest, operation identity/lease, ownership classification, and explicit uninspected or concurrently mutable paths |

The parent challenges a consequential proposal with:

1. What observation would make this design wrong?
2. Which ordinary event, scale term, or external behavior is absent?
3. Where meaningful, can the claimed property be made to fail in a
   deliberately weakened model or implementation, leaving a retained expected
   failure? If not, why is that check inapplicable?
4. Does the evidence test the contract independently, or merely repeat the
   code's branch structure?
5. What exactly has not been checked?

Unexpected evidence reopens the design. A compatibility branch, retry, cache,
sidecar, or special status may be correct, but only after stating its semantic
need, owner, cost, and why it addresses the contract rather than concealing the
counterexample.

## 5. Mutation and review gates

Before an agent edits:

1. Capture `HEAD`, branch/worktree, `git status --porcelain=v2`, relevant
   tracked diff, and untracked or external artifacts relevant to the declared
   scope. Record timestamp and whether relevant writers remain live; this is a
   bounded observation, not a global snapshot of the machine.
2. Assign an exclusive lease for each file and mutable external resource.
3. Establish the intended contract and decisive validation.
4. For stateful work, identify authority, operation identity, commit point,
   interruption behavior, and cleanup ownership.

Within one coordinated parent session, the parent work record is the source
lease authority: it records owner, scope, grant/release, and current epoch.
Absent a grant means no edit. Agents recheck it before writing and handoff.
Across independent sessions, such a prose lease is only advisory; use separate
worktrees and enforce mutable external-resource ownership with an OS or
application lock, or serialize the work.

During implementation:

- Keep the change inside the lease and report newly discovered overlap.
- Do not format unrelated code, stage broad paths, reset another agent's work,
  or silently change external state.
- Send important discoveries to the parent promptly; do not leave them hidden
  in an agent's private reasoning.
- Pause and report when ownership is ambiguous, evidence contradicts the
  design, measured resource cost crosses the threshold stated in the work
  record, or the task requires new authority. Show predicted versus observed
  values; the parent revises the contract or authorizes a new design rather
  than silently abandoning or continuing the work.

After implementation, the parent:

1. Repeats the repository and external-state inventory.
2. Classifies every difference as pre-existing, intentional, generated, or
   unknown. Unknown changes stop integration.
3. Reads the complete logical diff and follows its production call paths.
4. Checks that obsolete experimental paths were removed where appropriate.
5. Runs focused evidence first, then broader checks proportional to risk.
6. Compares measurements with the prior cost model.
7. Requests an adversarial review for consequential behavior.
8. Records residual assumptions and unverified properties.

The external inventory is limited to the declared paths, processes, endpoints,
and owned manifests. Prefer bounded headers, receipts, and counters over
recursive scans of enormous archives. Anything relevant but unreadable or
outside that manifest remains explicitly unknown.

### Destructive operations

Use a two-phase procedure:

1. A read-only phase produces an exact manifest of targets, owners, artifact
   roles, replacement cost, and recovery options.
2. A mutation phase occurs only after the parent re-reads that manifest
   against current state and confirms authority. Obtain user approval when the
   requested authority does not already cover the exact action.

For high-consequence deletion, migration, publication, or process termination,
use an independent second review between the phases. Afterward, inventory the
result and report what changed and whether recovery remains possible.

The mutation phase must also prevent the manifest from going stale: quiesce or
lock concurrent writers, or use an identity/version compare immediately before
each mutation and abort on change. If the external system supplies no reliable
exclusion or compare-and-swap, describe the guarantee as best-effort and obtain
final user confirmation. When no second reviewer exists, the parent performs
an explicit adversarial pass and says that independent review was unavailable.

Names such as `scratch`, `tmp`, `cache`, `candidate`, and `old` confer no
deletion authority.

## 6. Pass knowledge without creating a second codebase

An agent handoff is concise but reproducible:

```text
objective and lease:
baseline commit/worktree/status:
observed facts with paths or commands:
inferences and assumptions:
design decisions or disagreements:
files edited/generated:
external resources/processes touched:
validation commands and results:
toolchain/platform/features/input-data identity:
skipped/failed evidence:
remaining risks and next owner:
final status and resource inventory:
commit/push status:
```

The parent rejects “fixed,” “looks good,” or “tests pass” without this
substance.

Use three knowledge lifetimes:

- **live coordination:** parent plan, agent messages, leases, process IDs;
- **handoff evidence:** report tied either to a commit or to an immutable
  baseline plus complete diff identity, with reproducible commands;
- **durable product theory:** contracts, format descriptions, models,
  rationale, and known limitations committed with the code they explain.

Do not commit raw agent transcripts, status diaries, large logs, or temporary
research. Promote only knowledge needed to understand, verify, operate, or
modify the product. When a log is decisive but unsuitable for the repository,
retain it in bounded external storage and record its path, checksum, owner, and
retention period.

Chat and plans can disappear through context compaction or interruption. Work
affecting long-running operations, destructive cleanup, architecture, or a
later session therefore leaves a small durable contract/decision/audit record
or a parent-owned external ledger. That record points to evidence; it does not
duplicate the transcript.

When agents disagree, preserve both evidence sets and name the disputed
requirement, authority, or assumption. The parent resolves the disagreement
from source evidence and user intent; confidence or majority vote is not a
resolution method.

## 7. Prevent parallel sessions from interfering

Source isolation and resource isolation are separate problems.

### Agents sharing one worktree

All agents share the same files and Git index, but their observations are not
synchronized and can become stale immediately.

- The parent work record records one writer per file or generated artifact,
  including lease epoch and release. Without the coordinating parent, this is
  advisory rather than an enforceable lock; serialize work that cannot tolerate
  that limitation.
- Agents get non-overlapping edit leases. Shared manifests and integration
  files remain parent-owned.
- Git index mutations are serialized. Before staging or committing, the parent
  announces an integration fence, all agents stop writes, and the parent
  captures fresh status, stages explicit pathspecs, reviews the staged diff
  against leases, and commits. An index-lock failure is a serialization event,
  not a reason to remove the lock.
- Agents re-read status before editing and before handoff because another
  agent may have changed the baseline.
- A parent commit is announced to every active agent; they must re-check their
  assumptions before continuing.
- Builds and tests classify their side effects and use distinct temporary,
  database, socket, port, mount, state, and output paths unless a resource is
  deliberately shared under a tested lock. Completion includes descendant and
  mount cleanup, not just a test process exit.

If two tasks need the same implementation file, serialize them or move one to
a separate worktree. Textual conflict avoidance is not enough when both tasks
change the same invariant or interface.

### Independent sessions

Independent writable sessions should use separate Git worktrees created from
an explicit base commit and separate branches:

```text
git worktree add <private-path> -b agent/<session-id> <base-commit>
```

Each session records its base and uses session-specific build targets and
scratch roots. Integration is by reviewed commit/cherry-pick or semantic
reimplementation, never by copying an unknown working tree or blindly
resolving merge conflicts.

Worktrees share the Git object database, refs, configuration, hooks, remote
state, and common worktree metadata. Use unique branch/worktree leases, record
uncommitted changes omitted from the new worktree, and serialize fetch, rebase,
tag, maintenance/GC, and other ref- or object-changing commands. Push remains
separately authorized and records the exact remote and ref.

Git worktrees also do not isolate:

- external drives or archives;
- databases outside the worktree;
- ports, sockets, daemons, process groups, or network quotas;
- global tool, compiler, and download caches;
- devices, mounts, keychains, credentials, or remote services.

Each mutable shared resource therefore needs an exclusive lease containing
session/operation identity, owner, start time, scope, and cleanup authority.
A filename convention is not a lock. Use an operating-system or
application-level lock whose stale-owner rules are defined and tested. If no
reliable shared lease exists, serialize access.

Every agent-owned launch records command, PID/process group, an OS-native
incarnation token where available, working directory, relevant environment,
owned paths, descendant policy, and expected stop condition. Prefer an owned
supervisor handshake that closes the gap between spawn and registration.
Terminate only the matching owned incarnation; never kill by a broad name
pattern. If reliable identity is unavailable, report ownership as unknown and
do not promise safe automated termination.

Separate worktrees may share immutable or content-addressed caches only after
their concurrency semantics are known. Cargo target directories, registry/git
caches, Python/uv caches, Kati state, generated wire/vendor files, release
symlinks, and tool downloads each need a deliberate share/isolate decision and
disk budget. Prefer `--locked` and version probes where supported. Do not
multiply enormous build trees merely to satisfy a naming convention when the
tool's own locking safely permits sharing.

Network politeness, `Retry-After`, host quotas, credentials, and remote
write-capable endpoints are shared even across machines behind one account or
IP. Lease the host/request budget and mutation authority, not merely the local
download path. Devices, VMs, FUSE mounts, fixed gateway addresses, external
volumes, and system daemons require the same treatment.

Before integration, the parent compares each branch with its recorded base,
checks path and resource leases, reviews cross-agent assumptions, and runs the
combined evidence. Two individually correct patches can violate a shared
invariant when composed.

## 8. Build and validate models and executable evidence reproducibly

A formal artifact is part of the source, not an illustration. Put each model
under a stable directory such as:

```text
formal/<topic>/
    README.md
    Model.tla
    Model.cfg
    counterexamples/
```

The README states:

- question and user requirement being modeled;
- abstract state and permitted observations;
- environment, failure, ordering, and fairness assumptions;
- safety and liveness properties;
- finite bounds and excluded behavior;
- mapping from model actions/state to implementation operations/evidence;
- checker and exact pinned version;
- reproduction command and expected result;
- known gaps.

Generated checker state and bulk logs are not committed. Only minimized,
reviewed counterexample fixtures belong in `counterexamples/`; raw TLC/fuzz
state lives in ignored per-run storage with an owner, size bound, and retention
period. A useful counterexample is explained and retained as a model scenario
or implementation regression test.

### Candidate tool portfolio

This is a menu, not a checklist. Adopt a tool only for a concrete risk and
owning harness, with a bounded ordinary runtime and a maintenance/removal plan.
No row is mandatory when its corresponding risk is absent.

| Problem | Tool | What it supplies | What it does not supply |
| --- | --- | --- | --- |
| Concurrent/durable lifecycle protocol | [TLA+ with TLC](https://github.com/tlaplus/tlaplus); PlusCal only when it clarifies the algorithm | Exhaustive exploration of the finite reachable abstract configuration represented by the committed model, invariant and temporal-property counterexamples | Proof of Rust code, unbounded data, filesystem truth, or correct requirements; liveness remains conditional on fairness assumptions |
| Pure data laws and stateful APIs | Rust [Proptest](https://proptest-rs.github.io/proptest/), optionally `proptest-state-machine`, with an independent reference model | Generated examples, shrinking, replayable minimal traces | Exhaustiveness or a trustworthy oracle when model and implementation share logic |
| Small synchronization primitive | [Loom](https://github.com/tokio-rs/loom) behind a narrow, explicitly checked Loom/std abstraction | Systematic bounded interleavings under Loom's memory/concurrency model | OS processes, SQLite, signals, filesystems, networks, Tokio as a whole, or all schedules |
| Bounded pure Rust routine | [Kani](https://model-checking.github.io/kani/) proof harness isolated from unsupported async/FFI/OS dependencies | Bit-precise bounded model checking under harness assumptions and explicit unwind/data bounds | General termination or whole-application verification |
| Unsafe/representation code | [Miri](https://github.com/rust-lang/miri) on a pinned nightly, for isolated pure tests and documented supported targets/features | Undefined-behavior detection in selected executions under Miri's interpreter | Proof of soundness, full FFI/platform behavior, all inputs, seeds, or schedules |
| Parser/decoder boundary | [`cargo-fuzz`/libFuzzer](https://rust-fuzz.github.io/book/cargo-fuzz.html) with a retained corpus | High-volume malformed-input exploration and minimized crashes | Correctness or “no bugs” after a time budget |
| Durable failure cuts | A test-only injected `FaultPlan` seam for returned errors, plus separate child kill/reopen tests | Deterministic I/O failures and process interruption/restart at named transaction cuts | Real power-loss behavior on every filesystem; publication claims remain conditional on stated APFS/Linux semantics |
| Python reference/model tests | Pinned Hypothesis, Python interpreter, and dependency lock through the project's wrapper | Generated and stateful examples with replay | Proof, or reproducibility without the committed environment and retained minimized example |
| Resource/performance claim | Owned byte/request/pass/descriptor/memory counters plus Criterion or scenario harness and platform profiler | Comparison with the static cost model and attribution of work | Semantic correctness from elapsed time alone |
| Coverage audit | `cargo llvm-cov` or Python branch coverage | Locations not exercised by the selected tests | A correctness property or a meaningful universal percentage |

For Make/Kati behavior, use a small graph or lifecycle model only for the
scheduling property in question. Validate recipes with deterministic fixture
manifests, no-op/touch/rebuild cases, `-jN` repetitions, kill/resume tests, and
GNU Make/Kati comparisons where semantics are intended to agree. Target
existence is evidence, not authoritative lifecycle state.

### Sarun integration policy

Sarun currently pins Rust 1.96.0 and has repository-local bootstrap support,
but it has no project command for TLC, Proptest, Loom, Kani, Miri, or
`cargo-fuzz`. The presence of a transitive crate or `/usr/bin/java` path is not
an installed verification tool.

The first committed harness using a tool must add:

- appropriate pinning: Cargo manifest plus committed lock, Python interpreter
  plus dependency lock/hashes, jar/binary release plus SHA-256, dated Rust
  nightly plus components, or a probed system prerequisite;
- a repository-local wrapper or documented pinned toolchain;
- a discoverable Make target;
- the smallest representative artifact and a negative/counterexample sanity
  check;
- platform and dependency prerequisites;
- a statement of what is fast enough for ordinary checks and what is
  scheduled/opt-in.

The following target names are a convention and do not exist until introduced
with a real artifact or harness:

```text
make check-models       # parse models and run committed bounded TLC configs
make test-properties    # owning Rust/Python reference-model properties
make test-loom          # isolated Loom harnesses
make test-failpoints    # durable-boundary/restart harnesses
make miri               # selected pure/unsafe tests on a pinned nightly
make fuzz-regress       # deterministic replay of retained cases/corpus
```

For TLA+, pin `tla2tools.jar` and its checksum in repository tooling rather
than requiring a globally installed Toolbox. Pin or clearly probe a supported
JDK; this Mac currently has a `/usr/bin/java` stub but no Java runtime. The
offline check verifies the JDK major version and jar SHA, runs SANY parsing,
then TLC with explicit configuration, worker/time limits, and coverage used
only as reachability diagnostics. It does not silently download a runtime.
Demonstrate expected-positive behavior and a meaningful expected-negative
configuration or scenario, with explicit reachability witnesses for important
states/actions; TLC printing success alone is insufficient.

`cargo-fuzz` requires its own pinned cargo-fuzz version, dated nightly,
components, host prerequisites, and sanitized corpus with no user data. Keep
deterministic corpus replay separate from an explicitly time-budgeted
open-ended fuzz run. Minimize each crash into a normal committed regression
case.

For human-reviewed implementation correspondence, maintain:

```text
requirement
    -> abstract property
    -> model action/state
    -> implementation operation/evidence
    -> test, proof, or measured assertion
```

This traceability is not a mechanized refinement proof. TLC checks the
configured finite model's properties. Proptest and stateful reference tests
compare generated executable behavior. Loom checks configured bounded
interleavings. Fault injection and restart tests exercise selected durable
cuts. Platform integration tests validate selected filesystem, process, and
network assumptions. None can substitute for the others when the corresponding
risk exists.

Use the two existing locked Rust workspaces with explicit scope, for example
`cd gimir && cargo test --workspace --all-targets --locked` and
`cargo test --manifest-path engine/Cargo.toml --all-targets --locked`, through
the repository's pinned Rust wrapper. Python commands currently resolve inline
`uv --with` dependencies without a project lock; a Python model/property
harness must pin the interpreter and add a deliberate lock/hash set invoked in
locked mode rather than silently using whatever is latest.

## 9. Completion

Before saying the work is complete, the parent answers:

- What user behavior changed?
- Which contract or model defines the intended result?
- Which implementation paths realize it?
- What evidence was personally inspected or reproduced?
- What counterexample or adverse event was considered?
- What resource and scale behavior was predicted and observed?
- Which files, processes, data, and remote state changed?
- Which claims remain bounded, assumed, skipped, or unverified?
- Did parallel work compose without violating shared invariants?

Then the parent reviews the final diff, repository status, external resource
inventory, and commit contents. Push and other external publication remain
separate actions requiring explicit authority.

If a decisive check is blocked, the work may still be handed off with a
bounded status and exact blocker, but it is not labeled complete for the
property that check was meant to support.

The protocol has succeeded when it makes the reasoning and uncertainty
inspectable and leads to better decisions. Merely producing all of its named
documents has no value. Following it is not proof that the repository has been
fully mapped; only the bounded coverage ledger can say which parts were
actually inspected, and unlisted or open parts remain unknown.
