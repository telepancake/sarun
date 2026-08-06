# Programming practice for Sarun

This file is guidance for coding agents working on Sarun. It is not a design
substitute, a compliance checklist, or evidence that the program is correct.
Its purpose is to make careful reasoning part of ordinary implementation.

Programming here means developing and maintaining an accurate theory of the
problem: what users are trying to accomplish, what the program means, which
facts it owns, how its parts cooperate, and why the chosen implementation
satisfies the important constraints. Code, tests, diagrams, types, and prose
are different ways of expressing or checking pieces of that theory. None is a
badge that makes the theory sound.

## Conduct

Humility, care, and diligence are engineering requirements.

- Inspect the actual repository, running processes, storage, and user data
  before reasoning from names or assumptions.
- Distinguish observed facts, inferences, requirements, design decisions, and
  unresolved questions. Do not silently promote one into another.
- Prefer reversible investigation while the situation is uncertain. Before an
  irreversible operation, resolve its exact targets, ownership, value,
  recoverability, and the user's authority for the operation.
- Treat surprising evidence as a reason to revisit the model, not as an
  inconvenience to patch around.
- Explain uncertainty and mistakes directly. Do not rename a failure as a
  feature, compatibility policy, or user choice.

For cleanup, recovery, import, or storage work, read
`docs/engineering/POSTMORTEM-2026-08-06.md` before acting. It records a case
where reusable, expensive inputs were destroyed because an agent acted before
understanding the artifacts.

## Understand the problem before choosing a mechanism

Begin with the user-visible work and its constraints:

- the outcomes users need and actions they may take;
- current behavior, including accidental and contradictory behavior;
- safety properties, progress requirements, and acceptable failure behavior;
- scale, latency, throughput, memory, storage, network, and descriptor budgets;
- environmental assumptions: filesystem, process, network, upstream protocol,
  concurrency, and crash model;
- non-goals and questions still being explored.

Record the rationale for consequential decisions: the question, relevant
evidence, alternatives considered, decision, assumptions, and consequences.
Keep it short enough to remain useful and update it when evidence changes.
Documentation written after the fact should present the current defensible
rationale and identify reconstructed history as such, rather than inventing a
tidy fictional chronology.

`docs/architecture/SYSTEM_MODEL.md` is currently an architecture inventory and
gap register. It is useful evidence about the repository, but it is not a
formal specification and must not be presented as one. Use it as a starting
map for cross-cutting work, then verify its claims against the current code and
runtime.

Keep the levels of the argument distinct:

- a **requirement** describes a user need or environmental constraint;
- an **abstract specification** describes allowed observable behavior without
  committing to a representation;
- **semantics** gives precise meaning to the terms and behaviors in a
  specification or program;
- an **implementation** supplies concrete state and operations;
- **verification** asks whether implementation behavior refines the
  specification under stated assumptions;
- **validation** asks whether the specification actually addresses the user's
  need.

Internal implementation steps may be invisible at the abstract level.
Conversely, an omitted filesystem, process, or network behavior can invalidate
an otherwise sound proof. Verification does not replace validation.

## Decompose by knowledge and change

A useful module hides a design decision or body of knowledge likely to change.
Do not decompose merely by source file, process stage, UI screen, or status
label.

For each important fact, determine:

- its abstract meaning;
- who may create or change it;
- what durable or live evidence represents it;
- which interfaces expose it without leaking representation decisions;
- which other facts must agree with it;
- how it evolves, is published, or ceases to be authoritative.

One canonical meaning and clear write authority are often valuable. They do
not imply that there must be exactly one reader function, one process, one
database, or one projection. Several implementations are sound when their
relationship to the same abstract fact is defined and checked.

Classify artifacts along independent dimensions rather than by directory:
ownership, authority, recoverability and replacement cost, release status, and
whether an artifact is input, resumable work, or installed output. Categories
can overlap: a private build tree may contain irreplaceable user-owned input.

Cleanup and compatibility policy follow from those facts. “Scratch” does not
mean worthless. Unreleased obsolete output may usually be discarded; released
data needs an explicit evolution policy; valuable inputs survive until their
replacement or deletion is deliberately authorized.

## Choose a reasoning method that fits the question

No single formalism is the right scaffold for the whole program. Select the
simplest model demonstrably sufficient for the contract and risk, and refine
it when evidence exposes an omission.

| Question | Useful model | Typical evidence |
| --- | --- | --- |
| Does a local operation establish a condition? | Preconditions, postconditions, frame conditions, loop invariants and variants | Review, assertions, proof for critical code, focused tests |
| Does a concrete representation mean the right abstract value? | Abstract data type, representation invariant, abstraction function | Invariant checks, equivalence with a simple reference implementation |
| Does an algorithm obey algebraic laws? | Equations and laws such as ordering, idempotence, associativity, or round-trip identity | Property-based and differential tests |
| Is a parser or persistent format well defined? | Grammar/schema, canonical encoding rules, compatibility and corruption model | Round-trip, malformed-input, fuzz, and cross-version tests |
| Can events, retries, crashes, or actors interact incorrectly? | Transition system with temporal properties and explicit environment assumptions | Model exploration where warranted, generated traces, concurrency and restart tests |
| Is publication durable and observable atomically? | Transaction protocol, linearization point, storage guarantees, interruption model | Failpoints, reopen tests, platform-specific integration tests |
| Is a design fast and bounded at real scale? | Workload and cost model for bytes, passes, memory, descriptors, requests, synchronization, and contention | Instrumented tests and representative benchmarks |
| Can a user understand and control the work? | User task model, information hierarchy, action availability, feedback and cancellation semantics | Interaction tests and observation of real workflows |

These methods complement one another. A proof establishes only its stated
property under its axioms and preconditions and can say nothing about a
mistaken requirement. A model checker exhaustively explores the bounded model
and configuration supplied to it, not the Rust implementation, unless their
correspondence is also established. Property tests sample cases produced by
their generators unless they deliberately enumerate a finite closed domain.
Integration tests and benchmarks observe particular platforms and workloads.
State exactly what each result supports and what it does not.

Use a stronger tool when the risk and structure justify it. TLA+/PlusCal and a
model checker may be appropriate for a bounded concurrent lifecycle; a simple
equational reference model may be better for merge logic; assertions and
ordinary tests may be sufficient for a local adapter. Tool choice follows the
question, not fashion.

## What a state-machine claim requires

A state machine is a semantic transition system, not an enum.

For a reactive subsystem, its model should identify:

- abstract state variables and their meaning;
- initial states;
- actions or events, their guards, and their next-state relation;
- safety properties that must always hold;
- relevant progress or liveness properties and the conditions under which
  they must eventually hold;
- environment, failure, ordering, and fairness assumptions;
- when it is claimed to describe running code, the abstraction or refinement
  relation connecting implementation evidence to model state.

The state space may be finite or infinite, deterministic or nondeterministic,
and represented by equations, relations, tables, diagrams, executable code,
or a specification language. Closed enums and exhaustive matching can be
excellent implementation techniques, but they neither define the semantics
nor demonstrate correctness. Conversely, a system represented by integers,
sets, files, or predicates may have a precise transition semantics without an
enum anywhere.

Do not force data transformations, parsers, static structures, or numerical
algorithms into lifecycle state machines when their natural contracts are
different. For genuinely concurrent or durable behavior, do not flatten
independent facts into one status string merely to make a small diagram.

## Move from model to implementation by refinement

The design task is to choose a concrete representation and algorithm whose
behavior refines the chosen abstract contract.

Compare plausible alternatives using the whole problem:

- correctness and clarity of the abstraction;
- ordinary and failure-path complexity;
- resource costs at the intended scale;
- observability and recovery;
- change boundaries and future evolution;
- dependency and platform assumptions.

Prefer the least mechanism that satisfies the semantic and operational
contract well. Extra passes, indexes, caches, databases, sidecars, hashes,
background processes, and special cases may be required by that contract or
justified by a concrete benefit; make their costs explicit and acceptable for
the workload. A “one pass,” “constant time,” “type safe,” or “formally
modeled” label is not itself a benefit.

Keep authority and commit points semantic. For example, a publication contract
may require old-or-new visibility and durable binding among several artifacts.
Atomic rename and directory synchronization may implement that contract on a
particular filesystem; a database or object store requires different
reasoning. State the platform guarantees rather than universalizing one
mechanism.

While exploring, isolate experiments and measure them. Once a direction is
rejected, remove its implementation, switches, and mechanism-specific tests
unless they support a released artifact or are compact evidence needed to
explain the decision. Do not turn files created during unreleased
experimentation into a permanent compatibility burden.

## Build independent evidence

Verification begins from the contract, not from the current code. Tests
derived solely from implementation branches can reproduce the same mistake,
though implementation-oriented regression tests remain useful when tied back
to the contract.

Use evidence appropriate to the risks:

- examples for important user workflows and boundary values;
- a simple independent oracle or reference implementation where possible;
- property and metamorphic tests for broad data spaces;
- generated command sequences for stateful APIs;
- adversarial schedules for concurrency and duplicate, late, or reordered
  events;
- failpoints and reopen tests at meaningful transaction cuts;
- malformed and partial input tests for external data;
- real end-to-end workflows for wiring and presentation;
- counters that assert resource behavior, not only elapsed time.

When using a model or specification, maintain traceability: which requirement
became which property, which model action corresponds to which implementation
operation, and which test or proof checks that correspondence. Report a claim
as specified, implemented, tested, model-checked, proved, or still unverified;
do not blur those categories.

Several mirror lifecycle components have executable transition tables,
receipt inspectors, local state/event matrix tests, and identity/publication
tests. Depot algebra has deterministic seeded law tests; parser and integration
fixtures cover selected workflows. The repository does not have systematic
durable-boundary failpoints, generated reference-model command sequences,
cross-machine lifecycle coverage, a project fuzzing harness, repository-wide
formal verification, or a checked refinement from a system specification to
the implementation. Do not claim otherwise. If a formal model is introduced,
commit the model, checker configuration, stated bounds and assumptions,
reproduction command, and counterexample/regression tests.

## Predict performance as part of design

Before running a large job, derive its expected work from the algorithm and
workload. Name the terms that matter: input and output bytes, repeated passes,
compression, sorting, random I/O, retained memory, queue bounds, descriptors,
requests, retries, synchronization, and contention. Evaluate them at the
largest intended scale and under degraded conditions.

Then measure to validate or revise the model. A benchmark cannot explain an
unexpected result by itself; profiling and counters should locate where time
and resources go. Unexplained work, long periods without observable progress,
and large divergence from the prediction require investigation of both the
design model and the environment before the result is accepted.

Model how the program interacts with virtual memory, filesystem caches, the
network stack, and the scheduler. Add application-level bounds where an
invariant, failure containment, admission policy, or measured bottleneck
requires them, and verify that those bounds do not defeat useful system
resource management.

## A proportionate working loop

For a low-risk local change this may take minutes; work with substantial
state, concurrency, durability, scale, or failure risk may require a written
model and independent review. The loop is a prompt for judgment, not an
algorithm that mechanically produces a good design.

1. Observe the real system and inventory relevant state and ownership.
2. State the desired contract, assumptions, scale, and unresolved questions.
3. Choose the abstraction and reasoning method suited to those questions.
4. Compare designs and predict correctness, recovery, and resource behavior.
5. Implement the smallest coherent vertical change, preserving unrelated work.
6. Check the implementation against the abstraction using independent
   evidence.
7. Review the result as a whole: user workflow, failure behavior, performance,
   obsolete paths, and remaining uncertainty.

If implementation reveals that the abstraction was wrong, revise the model
and rationale. Do not accumulate exceptions to protect a mistaken model.

## Repository discipline

- Examine committed, uncommitted, ignored when relevant, and external runtime
  state before editing or cleaning. Preserve unrelated user work.
- Do not run a formatter over unrelated code. Remove formatting-only churn
  from the proposed change.
- Read before deleting. Inventory exact artifacts and distinguish valuable
  inputs from regenerable intermediates; inspect actual producer and consumer
  code rather than trusting names.
- Keep errors attributable to the operation, resource, and owner that produced
  them. User-visible progress should answer what is happening and whether it
  is advancing without exposing packet-level noise.
- Run focused checks first, then broader tests in proportion to risk. Review
  the diff and the actual workflow, not only the test exit status.
- Commit only the reviewed logical change. Do not commit generated
  experiments, archives, credentials, or user data. Push only with explicit
  authorization.

## Foundations

This guidance draws on, rather than replaces:

- Peter Naur, [*Programming as Theory Building*
  (1985)](https://pages.cs.wisc.edu/~remzi/Naur.pdf).
- D. L. Parnas, [*On the Criteria To Be Used in Decomposing Systems into
  Modules* (1972)](https://doi.org/10.1145/361598.361623).
- D. L. Parnas and P. C. Clements, [*A Rational Design Process: How and Why to
  Fake It*
  (1986)](https://www.cs.tufts.edu/comp/40-2011f/readings/fake-it.pdf).
- C. A. R. Hoare, [*An Axiomatic Basis for Computer Programming*
  (1969)](https://doi.org/10.1145/363235.363259).
- E. W. Dijkstra, [*On the Role of Scientific Thought*
  (EWD447, 1974)](https://www.cs.utexas.edu/~EWD/transcriptions/EWD04xx/EWD447.html)
  and *A Discipline of Programming* (1976).
- Leslie Lamport, [*Specifying Systems*
  (2002)](https://lamport.azurewebsites.net/tla/book.html).
- Koen Claessen and John Hughes, [*QuickCheck: A Lightweight Tool for Random
  Testing of Haskell Programs*
  (2000)](https://doi.org/10.1145/351240.351266).

The lesson is not to imitate their notation. It is to separate concerns,
choose useful abstractions, make claims precise, retain design rationale, and
match evidence to the property being claimed.
