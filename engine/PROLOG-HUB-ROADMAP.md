# Prolog hub implementation roadmap

This file is the persistent implementation contract for replacing sarun's
fragmented command and representation machinery with the mandatory embedded
Prolog relation. Keep it current while the migration is in progress. Do not
introduce compatibility modes, optional features, alternate authorities, or
fallback parsers.

## Non-negotiable architecture

SWI-Prolog is a mandatory part of the single static sarun engine on both
`aarch64-unknown-linux-musl` and `x86_64-unknown-linux-musl`.

The Prolog relation is the sole semantic authority for:

- canonical action identity and implementation/handler identity;
- typed and shaped argument schemas;
- control-socket/UI wire verbs and messages;
- mechanical textual encodings of the sole action identifier;
- keycode bindings and their UI context/gates;
- menu labels and action availability;
- description providers, help text, syntax classes, and preferences;
- parsing, normalization, conversion, rendering, highlighting, and completion.

Rust may own only neutral framing/transport, typed FFI values, UI display of
relation results, and execution handler bodies. A Rust match from a canonical
handler identity to executable code is an implementation boundary, not a
second semantic registry. It must contain no duplicated schemas, aliases,
descriptions, syntax, keys, menus, or rendering rules.

`ui.sock` is a direct Rust-process transport, not a Prolog RPC mechanism. Its
request/reply/event encoding must be compact binary rather than JSON. Sending,
receiving, framing, decoding the binary envelope, and executing an already
typed command stay entirely in Rust; Prolog is not invoked on that hot path,
just as it is not invoked merely to forward an IP packet. The relation is the
sole definition of how the typed wire representation corresponds to command
text, CLI, logging, help, and every other representation. Rust wire opcodes and
codecs must be generated/projected from that definition, not maintained as a
parallel hand-written catalog. Cut over to the binary protocol and delete the
JSON protocol rather than retaining a compatibility mode.

Persist and record canonical binary data without eagerly converting it to a
human representation. Logs should retain compact typed wire frames with only
the framing/provenance required to replay or select them; log views invoke the
relation on demand for the visible window. Packet capture follows the same
rule: preserve packet bytes and capture metadata, then decode, describe,
highlight, or complete only when a consumer asks to view or transform them.
This late-decoding rule keeps transport and recording fast while allowing rich
relational presentation to take the time it needs.

Use `tv/wire/wire.h` and `tv/trace/trace.h` as the concrete binary-format
precedent. The useful properties are: one shared atom codec is the only layer
that knows raw bytes; common scalar bytes encode in one byte; payloads through
55 bytes carry a one-byte inline length; longer payloads carry a bounded
little-endian length; nested atoms frame compound values without field names,
terminators, seeking, or back-patching; blobs remain arbitrary bytes and can be
viewed without copying; a leading version rejects incompatible streams; and
the streaming decoder accepts arbitrary read fragmentation while committing
state only after a complete valid frame. Stable numeric wire identities come
from the Prolog relation. Reuse this encoding and cross-language boundary
fixtures rather than introducing a generic serialization library. Apply tv's
per-stream delta-state pattern where append-only logs have repeated headers;
keep the request/reply frame grammar small and explicit.

The relation and FFI must be generic enough to host grammars for
packets/protocol stacks, patches and editing, nested highlighting, build
graphs, and brush syntax. UI actions are the first practical client used to
prove the architecture; they must not bake command-specific assumptions into
the FFI.

The staged Brush proving-ground and its no-delete-before-parity cutover gates
are specified in `BRUSH-RELATION-MIGRATION.md`. That document is part of this
roadmap: unchecked items there are required work under the same singular-hub,
ordinary-tear, explicit-context, and no-fallback rules.

SWI remains the singular relation engine after an explicit comparison with
egglog and egglog-experimental. Egglog's monotone bottom-up saturation and
cost-based extraction are attractive for persistent equivalence classes, but
the hub primarily needs demand-driven queries over partially bound terms,
query-scoped changing inputs, exact zero/one/all cardinalities, and successful
parse witnesses carrying explicit context observations. Do not introduce an
egglog side engine. Borrow its useful separation between generating all valid
alternatives and selecting a preferred representative through declarative
costs.

Generic binary grammars operate on bounded immutable blobs and borrowed byte
slices, not lists containing one Prolog integer per byte. Small native pure
primitives may provide bounds-checked length, slice, scalar-endian, checksum,
and compound-layout operations; the relation must still own field order,
length dependencies, validation, semantic types, and representation meaning.
Every representation relation declares and tests its supported mode matrix
(for example decode, encode/render, and validate); relational notation alone
does not prove termination when insufficient arguments are bound.

A tear is an explicit typed hole in the ordinary grammar, not input to a
second completion algorithm. Matching a terminal or contextual value against
a tear yields candidate evidence, and only a successful parse of the complete
surrounding input records that binding as a completion. This automatically
checks suffix viability and makes the completion carry the exact context
query graph and observations used by the successful parse. Recording means
returning pure evidence, never asserting state during backtracking.

This is a present acceptance criterion, not a future improvement. If any
consumer derives completions, highlighting, hints, syntax, normalization, or
rendering by independently reinterpreting the grammar, stop the migration and
replace that path with a projection of the ordinary relation before doing
further feature work. Do not describe an adjacent implementation as though it
already satisfies this contract.

Unchecked items in this roadmap are required implementation work. They must
not be relabelled as optional extensions, compatibility modes, or possible
future improvements in code, documentation, or status reports. Completion
means proving the singular relation is the authority and deleting the
superseded path; it does not mean leaving both paths present behind a choice.

## Grammar engine boundary

The hub must be three independently understandable layers, with dependencies
flowing in one direction:

1. The grammar engine owns neutral sources and sinks, bounded execution modes,
   variables/tears, evidence, ranking, context query graphs, dependency keys,
   relational composition, and generic transformation operations. It imports
   no sarun catalog and contains no grammar-name dispatch (`parse_cpp26`,
   `parse_old_german`, or an equivalent switch hidden in data).
2. A grammar declares only relations among representations: sequence, choice,
   repetition, fields, semantic values, constraints, embeddings, context
   requirements, and presentation metadata. It does not implement a parsing or
   printing traversal, completion algorithm, context scheduler, FFI codec, or
   consumer callback. Adding a grammar must not require editing the engine.
3. A client such as sarun selects/composes grammar values, supplies neutral
   input/output representations, and implements context providers through a
   bounded generic query API. It does not inspect grammar productions or
   reimplement any derived operation. Adding a grammar must not require adding
   a grammar-specific Rust method.

The uniform engine operation is relation transformation, not a collection of
parser entry points. Parse, validate, encode, decode, render, complete,
highlight, explain, and dependency discovery are mode/projection choices over
the same grammar value and evidence. Grammars may be values nested in other
grammars; PHP-like language switching and protocol-stack composition must use
ordinary composition rather than engine registration.

Necessary operational ugliness belongs inside the engine: termination/mode
checks, bounds, immutable blob slicing, tear bookkeeping, ambiguity/ranking,
context staging, caching/invalidation keys, exception containment, and FFI
marshalling. Grammar declarations and client calls must stay terse even when
those mechanisms are active.

### Engine/client contract before grammar syntax

Do not elaborate or sugar the grammar vocabulary until the engine/client
contract works end to end. `grammar_ir.pl` is frozen as a capability probe in
the meantime; its spelling is not a public language and sarun must not grow
dependencies on its current term shapes.

The engine has one public operation: bounded relation transformation. Its
conceptual request and reply are:

```
transform(
    GrammarHandle,
    GivenRepresentations,
    WantedProjections,
    ContextObservations,
    Limits,
    Reply)

Reply = solutions(Solutions, ContextQueries, DependencyKeys, Diagnostics)
```

The exact Rust and Prolog data types may refine this notation, but the
separation is mandatory:

- `GrammarHandle` is an opaque, engine-owned handle to an immutable grammar
  value. It may be content-addressed or established during engine startup. It
  is not a grammar-name switch, and Rust cannot inspect the grammar AST.
- Given representations are neutral typed values such as text/byte sources,
  semantic values, rendered values, tears, and prior context observations.
  Wanted projections name outputs, not algorithms. Parse and print differ by
  which representations are given and wanted, not by an operation enum.
- A solution contains generic representation bindings plus evidence and
  ranking. Application-specific typed values are decoded by generated adapters
  outside the engine boundary.
- Context queries are ordinary unresolved relational dependencies using the
  `empty`, `one`, and `all` cardinalities. A client executes them through a
  generic provider interface and resubmits observations; it never receives a
  grammar callback or decides how a query affects parsing.
- Dependency keys and diagnostics are returned uniformly for every grammar and
  projection. Completion and highlighting consume the same solution evidence;
  there is no adjacent completion/highlight request protocol.
- Limits cover input/output bytes, solutions, ambiguity, recursion, inference,
  context graph size, and primitive work. Bounds are data on every request,
  not hidden globals chosen by an application grammar.

The Rust transport across the SWI worker must be typed and structured. The
public API must not expose Prolog source strings, `format!("request(...)")`,
operation atoms, predicate names, or operation-specific response decoders.
Rust constructs SWI terms through a bounded generic value encoder (or an
equivalent compact typed envelope), and decodes one generic reply shape.
Ergonomic sarun methods such as `parse_command` may exist as thin generated or
typed adapters, but they must all call this one transformation and contain no
grammar semantics.

The grammar/engine side of the same boundary is equally strict. A grammar
provides an immutable grammar value. It does not provide terminal callbacks,
parsing predicates, printers, completion walkers, or context planners. The
engine interprets the value without importing the grammar module or switching
on its identity. Grammar composition resolves values/handles as data.

Representation grammars remain independent even when sarun commonly uses
them together. A text grammar may relate source to `TextAst`, while a binary
grammar relates bytes to a distinct `WireAst`. Neither grammar imports or
names the other. The client supplies an immutable glue relation
`TextAst <-> WireAst` and installs a composed handle. Structural identity is
ordinary unification; renamed, defaulted, gathered, or semantically different
fields are explicit client bridge data. A client may first attempt a bounded
typed structural conversion in Rust when the ASTs line up, or install an
immutable glue relation when the mapping is relational or semantic. In both
cases the conversion belongs to the client: the engine must not acquire
action-, packet-, or language-specific AST adaptation, and the destination
grammar/generated type remains the authority that accepts or rejects the
resulting shape.

Context dependencies follow the representation that introduces them. A text
name can emit and resolve a context query into a semantic identity before the
bridge relates that identity to `WireAst`; the binary grammar does not learn
about the original spelling. Composition namespaces query identities, carries
evidence and dependency keys across bridges, and permits a bridge to relate
context-bearing values without executing or hiding a lookup. Rust may execute
generated direct codecs for an already closed wire AST on the hot path, but
ad-hoc Rust AST conversion is not a substitute for installed glue relation
data.

Boundary acceptance tests must prove all of the following before grammar
notation work resumes:

- a foreign test grammar is installed and transformed without importing
  `action_grammar` or changing engine code;
- the same generic request shape parses, renders, exposes a tear completion,
  projects highlights, and returns a context query/observation dependency;
- sarun actions use the generic transformation path while preserving the real
  command-modal and control-socket behavior tests;
- Rust has no `Operation::{Parse, Complete, Highlights, Render, ...}` dispatch,
  and Prolog has no corresponding `dispatch_application/3` table;
- the boundary contains no textual Prolog request construction and no
  action-specific term decoder;
- dependency tests enforce engine -> no grammar/client imports, grammar -> no
  engine traversal callbacks, and client -> no grammar-AST inspection.

Concrete boundary checkpoint: `relation_api:transform/2` now executes a
foreign immutable sequence grammar in both directions with the same envelope.
Rust has generic `RelationRequest`/`RelationReply` values and recursively
constructs/decodes SWI terms through the FLI on this path, so no textual Prolog
request or operation atom crosses it. Solution and evidence limits now
constrain foreign-grammar enumeration, and a
tear completion is aggregated from ordinary parse evidence through the same
request. The output-byte limit is validated at the Prolog envelope and bounded
by the Rust decoder using the request-specific ceiling; oversized or zero
limits fail before entering Prolog. The former action-request operation has
now also been removed; all calls into SWI use the generic structured envelope.
The foreign grammar now also declares contextual fields with an explicit
`empty`/`one`/`all` exact cardinality. Exact and torn source transformations
return stable query nodes; resubmitted observations produce completion matches
and dependency keys through the same reply, without a context-specific entry
point or client-side grammar inspection.
Rust's typed context-query, observation, readiness, and dependency methods now
adapt to `RelationRequest` and cross the structured FLI path. Their four
`Operation` variants and the matching `action_grammar:dispatch_application/3`
cases have been deleted. Existing graph/ref/cardinality behavior passes through
`context_grammar` by varying given and wanted bindings, providing the first
production consumer migrated off the old operation protocol.
The executable grammar value now composes alternatives with
`choice_grammar/1` and maps neutral representation bindings to semantic terms
with bidirectional data templates in `projection_grammar/2`. Choice branch keys
namespace context-query identities and route resubmitted observations without
client inspection. Templates cover constants, binding references, compound
terms, lists, and relational list concatenation; the latter proves that
sarun's resume/pause projection can append and remove its fixed boolean through
the same declaration. Foreign tests parse and render through nested
choice/projection/sequence values. The action catalog is now materialized as
this immutable value; duplicate legacy parsing predicates remain a deletion
target, while the request operation surface is gone.
Terminals can now contain engine-interpreted codec data rather than a terminal
predicate. `grammar_codec.pl` implements finite enumerations, typed integer and
text wrappers, codec choice, and a closed relational JSON shape vocabulary
(objects, arrays, tuples, nullable fields, and strings). A foreign grammar
test parses reordered JSON fields into a typed compound and renders canonical
compact JSON from the same declaration. The current OCI and API action
arguments are expressed with these codecs in the installed action grammar.
Their older duplicate parsing predicates remain to be deleted.
`action_grammar:action_relation_grammar/1` now materializes all 108 executable actions as
one immutable `choice_grammar` value. Every branch contains its mechanically
derived command words, source schema, declarative terminal codecs, context
descriptors, action preference, and a bidirectional template for the normalized
`command(Action, Handler, Target, CommandArguments)` text-AST representation. Generic
transformation tests parse and render `mirror_resume`, including its fixed
false normalized argument, without the terminal callback or action operation table.
The complete value is installed once behind the opaque `sarun_actions` handle
and production Rust consumes it without transporting or inspecting its tree.
Generic context staging now resolves successful `one` observations into the
corresponding neutral argument binding before semantic projection. Dependent
selectors are declared with argument references, become validated query-graph
edges, and are resolved only after the referenced observation exists. Choice
composition namespaces both node identities and nested `ref/1` terms, so the
graph remains valid outside a branch and routes back inside without client
inspection. Completion preferences from every successful choice branch are
merged and reranked globally. Action-level tests cover the reported `kill C1`
flow, its typed command-AST result, a two-stage box/path dependency, and context
completion returned from the ordinary torn parse.
`grammar_store.pl` now provides install-once immutable grammar handles. The
embedded startup composition materializes the complete action value once and
installs it as `grammar_handle(sarun_actions)`; `relation_api` resolves handles
generically and has no action-name branch. Reinstalling the identical value is
idempotent, changing a handle fails, and missing handles fail closed. A native
aarch64 structured-FLI test renders an action through the installed handle, so
Rust neither transports nor inspects the grammar tree.
Production Rust parsing, rendering, literal completion, and highlighting now
use `grammar_handle(sarun_actions)` through `RelationRequest` and the recursive
structured FLI. Input units, typed commands, and retained parse evidence are
constructed as neutral values rather than Prolog source strings. Highlighting
is requested as a projection of the successful parse evidence through the
same grammar, not recomputed by Rust. The four corresponding Rust `Operation`
variants and Prolog `dispatch_application/3` cases have been deleted. All 319
native aarch64 Rust tests (318 pass, one ignored browser integration test) and
the command-modal regressions pass after the cut-over. Context planning and
completion now also use the generic request and explicit branch-scoped
relation values for query identities. Concrete contextual fields remain part
of an assist parse, so dependent completion first resolves earlier `one`
queries and only then executes the torn field's `all` query. Failed context
observations suppress semantic solutions and therefore highlights; the client
does not filter evidence independently. Choice composition carries branch
provenance into contextual completion semantics. The old context-plan
predicates, four dispatch cases, Rust operations, textual encoders/decoders,
and binding adapter have been deleted. Action metadata is now constant
projection data on the same alternatives: all help and target-filtered help
vary only their given bindings, and substring filtering is a generic pure
projection template. The old help predicate, operation cases, and textual
decoder have been deleted.

Generic relation composition now separates representation grammars at the
engine boundary. `compose_grammar(Left, SharedBindings, Right)` joins two
immutable relations through explicitly named AST bindings in either direction;
`binding_grammar/1` is the neutral leaf used by glue relations. Component
context queries and dependency keys are namespaced as `left`/`right`, while a
successful observation binds the originating AST before the bridge runs.
Foreign tests prove both `source <-> TextAst <-> WireAst` and contextual-name
resolution before `TextAst <-> WireAst`, without sarun grammar imports.

Sarun now owns command-to-request adaptation in `action_bridge.rs`, outside
the text grammar, grammar engine, and generated binary codec. It attempts only
bounded structural option/list/record adaptation plus explicit client-owned
reshapes for the few intentionally different ASTs; the generated closed
`ActionRequest::from_relation` implementation is the accepting authority and
requires exactly one result. The old Prolog `action_request/2`, generic
`application/3` dispatch table, Rust operation variant, textual command
encoder, textual response parser, misleading `Prolog::action_request` facade,
and their obsolete tests have been deleted. The parser invokes the client
bridge directly after the text relation returns its AST.
Bridge tests cover optional and repeated fields, `ro_attach`, `view.open`, and
type-confusion rejection. The full static aarch64 suite passes (321 passed,
one ignored browser integration test). A bridge that needs genuinely
relational or context-bearing adaptation must be installed as glue grammar
data and composed through `compose_grammar/3`, not added to the engine.

Help presentation now follows the same client/engine boundary. `sarun verbs`
and `sarun --help` query the embedded relation in the invoking process and
format the same typed `ActionHelpRow` projection; neither needs a running
engine or touches ui.sock. The former `verbs` pseudo-action, opcode, generated
request/success variants, engine dispatcher, newline-JSON request, and legacy
JSON result projection have been deleted. Top-level help labels the generated
rows as the interactive action language so it does not pretend every action is
already a top-level argv form. Native CLI tests execute both help surfaces with
no engine, and the static aarch64 suite passes 322 tests with one ignored
browser integration test.

Unix argv is now a distinct neutral source representation rather than text
joined and tokenized a second time. Each OS argument becomes exactly one
bounded source unit, including embedded spaces and empty strings. The installed
action grammar declares the standalone `brush` action with a CLI-local target;
`sarun brush`, `sarun brush -c SCRIPT ...`, and `sarun brush SCRIPT ...` pass
through the ordinary action relation and then invoke the existing embedded
Brush shell without starting or connecting to the server. `mirror` ingress now
uses the same argv adapter. Generated help and `sarun verbs brush` see this
single declaration. Generic choice execution conservatively indexes branches
by their exposed first literal, and source-mode spread repetition consumes the
bounded input before assembling output lists; a foreign-grammar regression
proves exhaustive spread enumeration terminates. The real static aarch64
binary executes a quoted standalone Brush command without an engine, and the
full suite passes 324 tests with one ignored browser integration test.
Top-level action ingress no longer contains a `brush` parser branch: argv is
related generically, only a result declared `cli` enters the CLI-local handler
dispatcher, and the parsed handler identity is retained explicitly even for
non-wire actions. The remaining Rust match maps that identity to an executable
function and contains no name, schema, syntax, or argument parsing knowledge.

Two portability tests constrain the grammar IR before it is considered
generic:

- A Tree-sitter grammar translator should map named rules plus ordinary
  `seq`/`choice`/`repeat`/`optional`, lexical tokens, fields, precedence,
  associativity, conflicts, extras, inline/supertypes, and embedded languages
  into engine grammar values. Source spans, concrete and semantic trees,
  highlighting, tears, and completions then come from ordinary engine
  projections. External scanners are explicit bounded primitive relations,
  not permission to generate a grammar-specific engine entry point.
- A Wireshark dissector translator should map protocol fields/value tables,
  bounded cursor and slice operations, endian scalars, dependent lengths,
  conditional/choice fields, dispatch tables, checksums, nested sub-dissectors,
  reassembly/context requirements, expert diagnostics, and subtree spans into
  the same IR. Original packet bytes remain the source representation; decode,
  validate, tree display, filtering metadata, and re-encoding are projections.
  Arbitrary side-effecting C cannot be imported as grammar semantics: effects
  must be isolated as declared context queries or small pure bounded primitives.

These translators need not preserve the source implementation strategy. Their
output must be declarative grammar data, and adding the translated output must
not change the engine or Rust API. Representative translated fixtures—one
precedence/conflict/external-token language fragment and one
length/dispatch/checksum protocol fragment—are required architecture tests.

Current IR checkpoint: `grammar_ir.pl` defines and closes the shared vocabulary,
and independent Tree-sitter-shaped and Wireshark-shaped fixtures validate as
pure data. Undeclared rule references and undeclared primitive callbacks fail
closed. With the uniform engine/client boundary now proven, the first executable
raw-text subset has landed in the grammar-independent
`text_grammar_engine.pl`: named recursive rules, sequence, choice, optional,
repetition, fields, literals, declarative codepoint sets, exact consumption,
generic AST/evidence/highlight projections, and UTF-8 byte spans all execute
through `relation_api:transform/2`. A foreign balanced recursive grammar proves
that `λ` is matched character-safely while exposed as its exact two-byte span.
The first real client grammar, `brush_grammar.pl`, now declares a recursive
shell-word slice as immutable data and is installed behind the `sarun_brush`
handle. It added generic negative lookahead to the IR for unambiguous lexical
boundaries; neither the engine nor Rust contains a Brush parsing case.
Raw text sources now also carry zero- or nonzero-width edit tears through the
ordinary parser. Shared evidence projection records literal completions only
from whole-source witnesses, including concrete suffixes, and linear cursor
state guarantees that a tear is consumed exactly once.
The generic text engine now executes grammar-owned extras and lexical regions;
trivia is consumed deterministically at the nearest syntactic boundary. The
Brush client has consequently grown from an isolated word to its first program
slice: multiword commands, pipelines, boolean lists, sequencing, newlines, and
backgrounding all produce AST and highlight evidence through the same handle.
The engine also now has an executable pure scoped-state relation: lexical and
escaping definitions resolve nearest-scope uses internally, escaping effects
become explicit deltas, and only unresolved uses or declared surrounding
constraints become external context queries. This algebra is tested through
the uniform transform envelope. A separately composable declarative AST-state
adapter now selects parser-owned named nodes and exact UTF-8 field spans and
emits ordered state steps around child traversal. A foreign composed grammar
proves local `λ` resolution and an external `z` query without either syntax or
state engine knowing the other AST. The installed Brush handle now uses a
generic enrichment combinator to expose syntax and state projections together:
assignment-only commands emit their variable definition after RHS traversal,
later simple parameters resolve locally, and unresolved parameters become
explicit external queries. This is the first shell-state slice, not yet full
Brush assignment or scope semantics. The same state projection now stages
context observations: successful unique observations bind external values,
failed unique observations fail the semantic solution, and consumed outcomes
become stable dependency keys for selective invalidation.

The discarded command-signature mini-language (`signature`, `following`, and
`positional`) has been removed from the engine, Rust boundary, grammar, and
tests. Declarative builtin parsers now translate into the ordinary generic text
grammar IR (`grammar`, `rule`, `seq`, `repeat`, `choice`, `literal`, and
`terminal`). The engine has no command, flag, argument-layout, or Clap cases.
The first concrete client is `bind -m`: its canonical keymap values are literal
branches in an immutable grammar supplied through the normal request envelope.
Interactive Reedline consumes the same completion projection.

A tear at the start of a remaining sequence is now an ordinary parse witness:
the relation renders a bounded valid continuation through the same grammar and
records it as ordinary evidence. The invariant is explicit and tested: if a
full construct parses, a tested prefix with valid continuations must expose a
completion from that parse relation. Generic tests cover `bind|`, `bind |`, and
`bind -m |`; Rust and native aarch64 PTY tests cover both the flag and value
menus. Completion is not permitted to become a side algorithm that traverses
or reinterprets grammar data.

Enrichment grammars now namespace base and extension query identities so an
observation returns only to the relation that emitted it, and extensions
receive exactly their declared shared representations rather than every base
projection. Context is also an ordinary nested text-grammar expression. It
wraps the grammar that recognizes the surface and carries an explicit pure
`ask(Cardinality, Domain, Selector)` template. Exact text substitutes
`value(surface)` and issues the declared query; a tear mechanically changes a
unique-name request into `ask(all, Domain, prefix(Surface))`. The same parse
evidence, observation, and dependency key then produce contextual completions.

The generic Clap adapter maps path value hints to filesystem context domains;
it does not map command names. `edit PATH` therefore parses through the
ordinary builtin grammar, asks the logical-cwd filesystem provider, and
replaces `./t` with `./test1.sh`. Generic relation tests cover query,
observation, completion, failed exact uniqueness, and dependency recording.
Rust and native aarch64 PTY tests cover the real standalone Brush behavior.
Remaining immediate work is to add live Brush variables/functions/builtins/PATH
providers, embed supplied command grammars at every Brush command position,
and make one cached relation analysis own every Reedline and editor
presentation projection.

The next externally usable checkpoint is explicitly the complete interactive
`sarun brush` editing experience: relation-owned highlighting, completion,
validation, indentation, diagnostics, hints, and context dependencies through
one cached analysis. Brush's current AST parser may remain only as the later
execution adapter. The interactive cutover is singular and mandatory—there is
no opt-in provider or fallback to the adjacent Brush algorithms.
Validated IR constructs outside this initial mode matrix return
`unsupported_text_grammar` explicitly rather than looking like source with no
parse. Raw tears, trivia, rendering, state, embedding, constraints, precedence,
and byte grammars remain required before the portability fixtures become full
transformation tests.

Immediate extraction order:

- [x] Move relational sequence execution, tears, evidence, repetition, and
      rendering terminals into an engine module with no sarun imports; prove
      it with an independent foreign grammar test.
- [x] Move neutral source validation and completion/highlight evidence
      projection into the grammar-independent engine rather than action
      grammar helpers; prove span rejection, ambiguity retention, ranking, and
      paint projection with the foreign grammar test.
- [x] Replace operation-specific and action-specific Rust term decoders with a
      bounded generic transformation envelope plus generated typed projections
      at application boundaries.
- [x] Replace `action_grammar:application/3` and its dispatch table with the
      single generic transformation entry point; make context queries and
      evidence ordinary reply data.
- [x] Drive a foreign grammar and then sarun actions through that same boundary,
      preserving user-facing parse/render/completion/highlight/context tests.
- [ ] Replace the flat action-only spec vocabulary and terminal callback with
      immutable composable grammar values interpreted only by the engine.
- [ ] Move primitive text/value and bounded blob codecs into reusable grammar
      modules; eliminate action-specific structured JSON handling from the
      engine layer.
- [ ] After the boundary is proven, revise rather than merely sugar the grammar
      IR for sequence, choice, repetition, fields, constraints, references,
      embedding, precedence/conflicts/extras, and bounded byte layouts.
- [ ] Keep `sarun_actions` and transport/packet/patch/brush grammars as clients
      of the engine; enforce the dependency direction in tests and build inputs.

## Starting state (historical, not the current implementation)

The migration started with three competing authorities:

- `control::ui_verbs!` / `VERB_DOCS`: 97 wire handler names plus schemas/help;
- `registry::ACTIONS`: 69 explicit action records plus synthesized copies of
  missing `VERB_DOCS` rows;
- `pl/action_grammar.pl`: five duplicated mirror actions.

The normal pre-migration build excluded Prolog. Its optional path converted a
successful Prolog result back through `registry::find`, and Rust fallbacks
owned parse, completion, highlighting, and rendering for unsupported input.
The mirror CLI called `parse_words` directly and bypassed Prolog. Generated
registry help/key/menu projections were mostly test-only or dead while the UI
retained handwritten key and menu tables. The checklist and completion log
below, rather than this historical inventory, state the live migration status.

Do not merely revert the last two commits: the Rust registry detour began in
`480f80a`, mixed with unrelated useful IETF work. Repair forward and delete the
detour surgically.

## Representation model

Use normalized relation facts, not a monolithic Rust-shaped registry row.
Exact predicate names can evolve while implementing, but the model must cover:

- `action`: public identity, executable handler identity, target, visibility,
  description provider/text, and preference;
- `schema`: ordered named arguments with semantic kind, required/optional or
  repeated cardinality, and scalar/array wire shape;
- `form`: the sole action identifier mechanically encoded as command words,
  plus wire, key, and menu representations;
- `syntax`: literal/argument syntax class and description provider;
- `projection`: typed argument transformations such as `mirror_resume`
  supplying `false` to the shared `mirror_pause` wire handler;
- `context`: pane/gate predicates for key and menu actions.

One normalized text-AST result must carry enough information for client glue
to select the destination representation without another semantic registry:
action identity, handler identity, target, and typed command arguments. It is
not itself a binary-layout AST; the sarun bridge and generated closed wire type
own that boundary explicitly.

Typed values initially required by the UI surface:

- string/path/base64/spec values that remain strings even when numeric-looking;
- signed integers and non-negative identifiers where required;
- booleans;
- arrays for the protocol variadics that are arrays on the wire;
- flattened repetitions only where the actual wire contract requires them;
- bounded compound/map/byte values shared with non-command grammars.

Rust may split command input into neutral UTF-8 byte-spanned source tokens, but
must not classify action literals or argument semantics. Prolog turns neutral
source evidence into semantic evidence. This preserves exact UTF-8 spans while
keeping syntax knowledge in the hub.

## External semantic context

Object-bearing grammars need query-scoped context: box identities and aliases,
paths visible in a particular box, mirror names, process rows, rule names, and
similar live domains. The initial command grammar validated only argument shape
and primitive type. The present relation emits explicit query graphs, resolves
ordinary contextual arguments through `one`, and derives contextual completion
from `all` observations matched against the same typed tear. Remaining provider
and domain coverage is tracked in the checklist below.

Context queries are themselves values in the relation. The compact core
algebra is:

```prolog
ask(empty, Domain, Selector)   % is the matching set empty?
ask(one,   Domain, Selector)   % relate to exactly one entry; fail otherwise
ask(all,   Domain, Selector)   % relate to the canonical matching entry list
```

`Domain` carries the semantic type. `Selector` is structured relational data,
not an untyped callback name. Examples:

```prolog
ask(one, box, name("work"))
ask(empty, box, prefix("wo"))
ask(all, box, prefix("wo"))
ask(all, path, within(box(ref(box_query)), prefix("src/")))
ask(one, c(variable), within(scope(17), name(y)))
```

The cardinality is part of meaning, not merely a provider optimization. Exact
name resolution normally uses `one`; completion enumeration uses `all`; cheap
viability and negative constraints use `empty`. `one` has no relational
solution for zero or multiple distinct identities.

Queries form a pure dependency graph:

```prolog
query(box_query, ask(one, box, name("work"))).
query(path_query,
      ask(all, path, within(box(ref(box_query)), prefix("src/")))).
```

`ref(Id)` is a typed dependency on the successful value of an earlier query.
The graph must be acyclic and stable under canonical term serialization. This
supports dependent grammars without prematurely performing I/O or hiding
dependencies in provider code.

Lexical/local context remains ordinary relational state and emits no external
query. Thus a prior C declaration resolves `x` internally, while an unresolved
`y` emits `ask(one, c(variable), within(Scope, name(y)))`. Composition carries
local environments and external query graphs separately, so nesting grammars
does not turn all names into global lookups.

Rust executes only ready query nodes against engine state, sockets, or
filesystem snapshots. It returns typed entries with stable semantic identity,
display names/aliases, metadata, provider identity, and revision. It must not
decide which textual position uses which domain, selector, cardinality, or
dependency; those are outputs of the relation.

Every attempted query produces a stable observation for dependency tracking:

```prolog
observed(QueryId, CanonicalQuery, source(Provider, SnapshotRevision),
         some(Result)).
observed(QueryId, CanonicalQuery, source(Provider, SnapshotRevision), none).
```

`none` records relational failure, including a `one` query with zero or
multiple identities. Provider/revision is provenance and cache freshness, not
semantic equality. The dependency key is the canonical `(QueryId, Query,
Outcome)` projection. After a relevant provider change, rerun the affected
queries and compare dependency keys; the parse needs recomputation only when a
key changes. Unrelated changes, and relevant snapshots producing the same
outcome, therefore do not invalidate the text.

The parser protocol is staged but remains one relation:

1. Relating source plus local environment yields a query graph and suspended
   semantic alternatives.
2. Rust resolves ready nodes and returns observations in bounded batches.
3. Relating the same source, graph, and observations yields parse, completion,
   highlight, or render results together with the exact observations used.

Prefer this explicit graph/observation envelope over foreign-predicate
callbacks. It keeps the Prolog worker bounded and deterministic, avoids
reentrant Rust/I/O calls from SWI, exposes type and dependency information,
and makes contextual parsing and invalidation independently testable.

## Complete UI action inventory

The relation now covers all 108 executable actions: 90 UI actions sharing 89
implementation arms, five control actions, twelve UI-local actions, and the
standalone CLI-local `brush` action. Help is
a projection of those actions, not an additional executable action. Entries
which had historically existed only in `registry::ACTIONS` included:

- control messages: `sudtrace`, `apply`, `discard`, `rename`, `quit`;
- local actions: `mirror_browse`, `mirror_read`, `change_read`, `change_edit`,
  `rule_new`, `rule_delete`, `rule_edit`, `detach`, `refresh`, `filter`,
  `action_menu`, `toggle_mark`;
- argument/wire projection: `mirror_resume` -> handler `mirror_pause` with a
  wire boolean of `false`.

There is exactly one name per action. Command tokens are derived mechanically
from that atom (`mirror_run_pending` -> `mirror run pending`, `oci.load` ->
`oci load`); title/display casing and binary identities are other encodings of
the same identifier. There are no explicit command spellings or implicit
aliases. Argument and handler projections remain relational but cannot rename
an action.

Key migration must cover the actual `PANE_ACTION_KEYS`, `PANE_KEYS`, modal key
tables, and context menus—not the unused registry projections. Selection-derived
arguments remain UI state assembly; key-to-action meaning and availability
belong to the relation.

## Work sequence and durable status

### 1. Static runtime and architecture support — IN PROGRESS

- [x] Remove the Cargo `prolog` feature declaration.
- [x] Compile/link `prolog.rs` unconditionally.
- [x] Remove `engine-prolog` and make normal `engine` depend on `swipl`.
- [x] Parameterize the SWI pipeline for x86_64 and aarch64 musl.
- [x] Build the pinned static SWI archive successfully for aarch64 musl.
- [x] Keep compiled SWI/zlib cache identity independent of application grammar
      and catalog hashes; relation edits now require only resource repackaging.
- [x] Finish a native aarch64 Rust test build linked with the archive.
- [x] Build the normal optimized aarch64 release with `make engine`; verify it
      is fully static and runs its help entry point on the current host.
- [ ] Revalidate the x86_64 archive/build after the pipeline change.
- [x] Define and test native aarch64 runtime behavior: FUSE and paired QEMU run
      natively, while `--sud` remains an explicitly x86 Syscall User Dispatch
      transport. The aarch64 build, canonical core, both x86 wrapper ABIs, and
      live FUSE/QEMU equivalence are covered without an emulated SUD fallback.
- [ ] Copy third-party license notices beside every normal release artifact.

### 2. Generic typed FFI — NEXT

- [x] Replace the five-value Rust `Action` enum with owned canonical/handler
      identities returned by Prolog.
- [x] Replace `CommandArg::JobId` with generic typed values including arrays.
- [x] Add neutral source-token semantics; remove Rust `grammar_unit` command
      classification.
- [x] Replace the application operation vocabulary with one generic bounded
      relation transformation envelope.
- [x] Preserve the closed callable surface, request/output bounds, dedicated
      worker thread, inference limits, exception handling, and cleanup tests.
- [ ] Add catalog/representation query decoding for help, bindings, menus, and
      conversions.
- [x] Add generic bounded context-request and typed context-fact envelopes,
      using the `ask(empty|one|all, Domain, Selector)` algebra, query IDs,
      typed `ref/1` dependencies, provider identity, and snapshot revision.

### 3. Complete Prolog action relation

- [x] Add all currently inventoried action metadata to Prolog facts (Rust
      duplicates remain until their runtime consumers have migrated).
- [ ] Make those Prolog facts the sole
      definition site.
- [x] Implement schema derivation/validation and explicit overrides in Prolog.
- [x] Implement generic literal and typed argument matching over neutral source
      evidence.
- [x] Implement mechanical identifier encoding, typed argument projections,
      exact end-of-input resolution, visibility, targets, and handler identities.
- [x] Generalize parse/render/completion/highlight predicates over all actions.
- [x] Add n-way representation queries for command text, wire, key,
      menu, help/description, and syntax.
- [ ] Add the generic bounded blob/slice layout relation and native pure byte
      primitives; cover decode, encode, and validation modes with malformed,
      truncated, length-dependent, and checksum fixtures.
- [x] Make tear matching part of the ordinary terminal/argument matcher and
      derive completions by projecting successful full-parse tear evidence;
      remove the separate completion traversal once this path is complete.
- [ ] Add invariants proving unique public identities, valid handlers/targets,
      schema/form agreement, and complete handler coverage.
- [x] Implement the pure `empty|one|all` evaluator, canonical typed matching,
      observation/dependency-key projection, graph validation, and staged
      `ref/1` readiness relation.
- [ ] Add contextual argument domains, dependent provider requests, exact/alias
      resolution, relational `one` failure, and contextual completions.
- [x] Add initial contextual domains for box identifiers and box-relative paths;
      structural plans now carry `query/2` nodes plus explicit AST
      `bind(QueryId,arg(Index),entry_value)` flows.
- [x] Resolve successful `one` observations into typed command-AST arguments,
      derive `all`/`prefix` queries for contextual completion, and derive path
      queries containing `ref/1` dependencies on earlier box arguments.
- [x] Execute dependent completion graphs in readiness order and return all
      observations to Prolog for ranked completion rendering; the initial UI
      path provider supplies changed paths tagged by their resolved box value.
- [ ] Back the path domain with the complete box-visible path index rather than
      only recorded changed paths, without moving prefix/cardinality semantics
      into the provider.
- [ ] Add local lexical environments that resolve internal bindings without
      external queries and compose independently from external query graphs.

### 4. Mandatory Rust integration

- [x] Remove every `cfg(feature = "prolog")` and negative counterpart.
- [x] Remove `BackendStatus::{Disabled,Used,Unsupported,Error}` and the notion
      of a selectable backend.
- [x] Make parse failure/no-solution a normal typed parse result and runtime
      failure a visible hard error; never invoke a second parser.
- [x] Route command prompt parse/render/highlight/completion only through the
      relation.
- [x] Route the mirror CLI through the same relation using the argv source
      adapter, preserving OS argument boundaries without joining/re-tokenizing.
- [x] Route standalone Brush through the same relation as a CLI-local action;
      prove `-c`, script arguments containing spaces, and empty argv entries do
      not require a running server.
- [ ] Route every other command ingress through the same relation.
- [x] Inventory every `ui.sock` request, reply, event, and stream frame and
      specify bounded binary framing plus typed scalar/array/value encodings.
      The durable cutover contract is `UI-SOCK-BINARY.md`.
- [x] Give every wire action a stable binary identity in the relation. Actions
      may project to a shared handler identity and schema; local actions
      have no invented wire form.
- [x] Give every wire action a concrete binary request-field schema in the
      relation and relate parsed/context-resolved values to that schema.
      Source/parser categories such as `integer`, `path`, `base64`, and
      especially `spec` are representations to convert from, not binary field
      types. Structured requests such as OCI build, API probe, view filters,
      and read-only attachments must be closed records/choices; no generic
      JSON-shaped request payload may survive.
- [x] Give every wire action handler a concrete typed result schema in the
      relation. Do not preserve the JSON object model as a generic recursive
      binary value or let Rust result construction remain the schema authority.
- [x] Give every non-action request, reply, event, stream mode, mux frame, sum
      variant, and SCM_RIGHTS role a stable binary identity and bounded schema
      in the relation. Ordinary actions do not receive duplicate transport
      request identities.
- [x] Project/generate the Rust opcode and codec definitions from the complete
      transport relation and prove every generated request has exactly one
      handler.
- [ ] Replace JSON `ui.sock` request/reply transport with direct Rust binary
      encode/decode and handler dispatch, with no Prolog call in message
      delivery and no retained JSON compatibility mode.
- [x] Consolidate the Rust implementation of `tv/wire/wire.h`; TRACE and the
      box/PTY/echo/FD-broker mux now share it, and the old four-byte big-endian
      mux framing has been removed process-wide.
- [ ] Exercise relation-based conversion only at representation boundaries
      (command line, command prompt, logging, diagnostics, help), producing or
      consuming the same typed binary-wire value used by direct transport.
- [ ] Store logs as compact binary wire frames plus minimal provenance; remove
      eager JSON/text log encoding and decode only requested display windows.
- [ ] Apply the same late-decoding contract to packet data: retain original
      bytes and metadata, and run relational packet decoding/rendering only for
      explicit views or conversions.
- [ ] Add direct binary round-trip, malformed-frame/bounds, request/reply, and
      runner-to-server integration tests; verify the socket hot path no longer
      serializes or parses `serde_json::Value` frames, and recording paths do
      not eagerly invoke Prolog or human-oriented rendering.
- [ ] Query help, menus, and binding data from the relation and cache immutable
      projections if needed.
- [ ] Route key events through relation-derived action identities while keeping
      execution and selection-argument assembly in Rust.

### 5. Delete competing authorities

- [x] Delete Rust parse/completion/highlight/render implementations and
      compatibility wrappers.
- [x] Delete `registry::ACTIONS`, supplemental action synthesis, schema/type
      inference, CLI maps, target/alias switches, and dead projections.
- [x] Delete `registry.rs` once no semantic responsibility remains.
- [x] Strip name/schema/help metadata out of `ui_verbs!`; retain only handler
      implementation dispatch or replace it with handler functions.
- [x] Delete `VERB_DOCS`; implement local `verbs` presentation from relation
      data without an engine request.
- [x] Delete duplicate help sections, dead `registry_menu_items`, and registry
      synchronization tests.
- [x] Remove stale parser design text that calls implemented work future or
      describes the Rust registry as phase one.
- [ ] Verify with `rg` that no alternate command authority or fallback remains.

### 6. Verification gates

- [x] Prolog core-only focused tests validate all 108 action rows plus neutral
      parsing, identifier encoding, argument projection, strings, arrays, rendering,
      completion/highlighting, and the generic transformation surface.
- [x] Expand them to exhaustive per-representation and per-action round trips.
- [x] Parse/render round trips preserve typed wire arguments for every action.
- [x] Every mechanically encoded command parses with exact arity and full input.
- [ ] Numeric-looking textual values remain text; byte paths and decoded
      base64 bodies remain bytes without numeric coercion or lossy conversion.
- [x] Optional and repeated arguments preserve exact wire array shape.
- [ ] Completion supports partial tokens and mid-token UTF-8 byte spans.
- [x] Completion candidates are exactly the tear bindings used by successful
      whole-input parses, including suffix viability, ranking evidence, and
      contextual dependency observations.
- [x] Box/object/path completion is derived from revision-tagged context facts;
      dependent domains such as box paths use earlier parsed identities.
- [x] Canonical observations compare equal exactly when a context change cannot
      affect the dependent parse; provenance/revision is excluded while query
      identity and successful/failed outcomes are included. Pure and embedded
      aarch64 tests cover the projection and revision-only refresh.
- [ ] Query graphs reject cycles, dangling refs, type-mismatched refs, duplicate
      query IDs, noncanonical entries, and provider responses beyond bounds.
      Cycles, dangling refs, duplicate IDs, refs to non-`one` queries, duplicate
      entry identities, and noncanonical result ordering are covered now;
      explicit cross-domain ref typing and provider-specific bounds remain.
- [x] Highlighting is derived only from successful grammar evidence, including
      successful external context observations for contextual arguments.
- [ ] Help/menu/key projections exactly cover intended visible actions.
- [ ] Every relation handler resolves to a real Rust execution handler, and
      every public handler has one relation definition.
- [x] Mandatory aarch64 musl build and focused Rust tests pass on the current
      machine; full suite is 282 passed, 1 ignored, 2 unrelated failures caused
      by unavailable `CLONE_NEWNET` changing the expected PTY network choice.
- [ ] x86_64 musl cross-build and focused tests pass.
- [ ] Full engine Rust suite passes apart from explicitly documented,
      independently reproduced environment limitations.
- [ ] Existing Python/e2e command, UI, and mirror suites pass against the
      mandatory-Prolog release binary.

## Work completed in this recovery session

- Audited the competing authorities and all parser/registry consumers.
- Confirmed 97 control/UI wire names, 69 explicit registry rows, and only five
  Prolog action facts.
- Confirmed registry help/key/menu projections are largely test-only/dead and
  the mirror CLI bypasses Prolog.
- Made embedded SWI unconditional in Cargo/module/build configuration.
- Made normal `make engine` depend on the SWI artifact and removed the opt-in
  engine variant.
- Parameterized SWI artifact generation and build.rs lookup for aarch64 and
  x86_64 musl.
- Successfully produced and validated the pinned aarch64 static SWI-Prolog and
  zlib artifact set on the current aarch64 host.
- Added the complete Prolog action catalog with schemas, targets, visibility,
  descriptions, one mechanically encoded name per action, and typed argument
  projections.
- Replaced the five-action grammar with a generic relation over neutral source
  units and typed normalized `command/4` results.
- Generalized the Rust FFI result types to owned identities and recursive typed
  values, and made the main parse ingress call Prolog unconditionally.
- Removed the selectable-backend result/status types, Rust completion and
  highlighting fallbacks, Rust render fallback, and obsolete parse wrappers.
- Built the normal fully static optimized aarch64 release with `make engine`
  and ran it successfully on the current aarch64 host.
- Added and embedded `context_relation.pl`, a pure query algebra with semantic
  cardinalities, typed selectors and entries, stable observations, dependency
  graphs, cycle/dangling-ref validation, and staged reference substitution.
- Added generic Rust relation values and typed context envelopes; embedded
  aarch64 tests round-trip a box lookup and make a dependent path query ready.
- Exposed contextual command/completion plans and command resolution through
  the bounded embedded FFI, including aarch64-native boundary tests.
- Made command parsing execute the relation-emitted query graph in dependency
  order through an explicit provider, retain the exact observations, and feed
  them back into the relation for typed command-AST resolution. The UI box provider
  supplies revision-tagged identities, names, display paths, and typed values.
- Made contextual completion execute the relation's explicit `all` query and
  feed the observation back into Prolog, which selects matching entry names
  and produces the ordinary ranked completion representation. Live UI box
  names now complete without Rust interpreting selectors or argument kinds.
- Generalized completion plans to query graphs with an explicit target node;
  dependent path completion resolves its preceding box `one` query before the
  `all` path query, and the UI path source returns typed containment facts.
- Unified invocation parsing and highlighting on the same resolved contextual
  plan; zero/ambiguous `one` results now leave contextual command text
  unhighlighted instead of presenting a merely structural parse as valid.
- Exposed provenance-free dependency-key projection through the embedded
  relation; a provider revision change with the same query outcome compares
  equal, while a changed outcome invalidates the dependent parse.
- Inventoried every `ui.sock` connection family and wrote the binary cutover
  contract. Consolidated the pre-existing TRACE atom code into one bounded
  tv-compatible Rust primitive and cut the box/PTY mux over to compound atoms.
- Assigned explicit, order-independent numeric identities to all 95 live wire
  action handlers in the relation. Wire projection occurs after typed argument
  projection, so (for example) `mirror_resume` projects the actual
  `mirror_pause` handler and its two-argument schema, while local-only actions
  do not pretend to be transport messages. Relation invariants prove identity
  uniqueness and complete UI/control handler coverage.
- Recorded the direct binary `ui.sock` transport boundary: Prolog owns
  representation relationships and conversion, while already-typed
  request/reply delivery is a Rust-only hot path.
- Replaced the separate literal and contextual completion grammar walkers with
  ordinary `parse(..., assist(EditId), ...)` witnesses. Literal and argument
  tears now flow through the same terminal/argument matcher as concrete units;
  concrete suffix input must parse in full, while absent continuation arguments
  remain explicit typed holes that cannot cross the execution boundary.
- Made contextual tear binding a projection of the same staged context
  relation: `all`/prefix observations are rebound to exact `one`/name witnesses,
  dependent refs are resolved from the parse's own observations, ambiguous
  aliases are rejected, and candidates retain the successful parse preference.
  Deleted `split_literal`, `match_known_prefix`, `match_known_item`,
  `viable_suffix`, `split_context_argument`, and the separate context-prefix
  grammar traversal.
- Replaced the independent `render_specs`/minimum-arity/splitting algorithm
  with the same `form_relation` and `relate_specs` sequence relation used by
  parsing. Parse units, edit tears, and rendered surfaces now differ only in
  terminal mode clauses; required, optional, repeated-array, repeated-spread,
  normalization, and end-of-form behavior execute once. Removed the old render
  traversal and its argument-counting helpers.
- Moved `representation/3` and `convert/4` into the hub beside the executable
  form relation. Command text, syntax, wire, help, key, and menu values project
  from the normalized facts and executable specs. Exhaustive tests render and
  reparse minimal and fully populated forms for all 108 actions.
- Added the normalized non-action transport relation: 16 requests, 6 response
  payloads, 7 connection modes, 10 compact event invalidations, and 11
  stream-frame identities, plus bounded records/enums/tagged choices and exact
  conditional descriptor roles. Action and transport-only request namespaces
  are disjoint; select/apply/discard/rename/patch/sudtrace/quit remain actions
  rather than acquiring duplicate message identities. Unix argv, paths, and
  environments remain bytes instead of inheriting JSON's UTF-8 restriction.
- Rejected a generic recursive binary `Value` for action replies: that would
  only reproduce JSON's schema-less object tree with smaller tags. Concrete
  per-handler result schemas remain an explicit prerequisite for generated
  action reply codecs.
- Made result type inseparable from action opcode by replacing the two-column
  wire-handler fact with `wire_handler(Handler, Code, ResultType)`. All 95 live
  handlers now name a bounded concrete success type, including typed view
  variants and raw byte bodies rather than base64 wrappers. This declaration
  checkpoint does not complete the migration gate: generated Rust result
  values must still replace the current JSON construction and be checked
  against these schemas.
- Replaced the misleading action-wire projection of parser categories with
  concrete semantic request fields for all 95 handlers. Box/process/view/job
  identities, byte paths and blobs, cardinalities, prompt verdicts, provenance
  domains, view filters, read-only attachment choices, OCI build specs, and API
  probe specs are now bounded wire types. There is no default mapping for
  `spec`, so a newly added structured action fails catalog validation until it
  defines a closed type. The gate remains open until parsed/context-resolved
  values are related into these records and Rust uses the generated codecs.
- Added the bounded Rust value primitives generated codecs build on: explicit
  compound wrapping, exact scalar range checks, UTF-8-only text, arbitrary
  bytes, fixed bytes, min/max lists, bounded maps, and tagged options. Their
  decoders reject invalid tags, duplicate map keys, count violations, malformed
  text, and trailing fields; they share the existing tv-compatible atom code.
- Deleted seven Python-era wire actions whose implementations were explicit
  compatibility no-ops and which had no Rust UI caller: `rescan`, `open_files`,
  `review_state`, `review_live`, `consolidate_start`, and the two consolidation
  invalidation pokes. Their stable codes remain retired rather than being
  recycled into unrelated meanings.
- Routed transport facts, types, enum cases, and tagged variants through the
  same `representation/3` and `convert/4` hub, embedded the catalog in the
  static SWI resource, and added closure/uniqueness/bounds/projection tests.
  Subscription events now describe compact invalidations; durable provenance
  and trace bodies are fetched and decoded only at a view boundary.
- Added a build-time projection which validates the central relation and emits
  concrete bounded Rust structs, enums, identities, and tv-compatible codecs;
  it does not emit a generic schema interpreter or recursive value tree. All
  95 action request variants map directly to one generated handler identity,
  and their typed success variants share the same stable opcode. Non-action
  requests, responses, connection modes, events, and stream frames are emitted
  from their respective relation sums.
- Made the projection fail closed when stale: the generated file records exact
  hashes for every contributing Prolog and generator source, `build.rs` checks
  those hashes on direct Cargo builds, `make engine` regenerates after the
  pinned host SWI artifact, and `--check` verifies byte-for-byte freshness.
  Generated aarch64-musl tests instantiate and round-trip every action request,
  every action success, every named type/case, and every transport/frame
  variant; 12 exhaustive suites pass, including identity uniqueness and
  malformed unknown-code/trailing-field rejection.
- Related every fully parsed and context-resolved wire command to its concrete
  generated `ActionRequest`. The relation enforces source cardinality, numeric
  and UTF-8 byte bounds, enum membership, decoded base64 size, and closed
  record/choice structure before Rust materializes the generated variant.
  Exhaustive pure tests construct a request for all 95 handlers, including
  shared-handler projections and contextual identities.
- Replaced the two unconstrained `SPEC` terminals with action-specific JSON
  source relations for OCI builds and API probes. Parsing and canonical
  rendering are pure and core-only; object order is irrelevant, while unknown
  or duplicate fields fail. The parsed terms relate to bounded
  `oci_build_spec` and `api_probe_spec` records, so JSON is only a textual
  representation and never survives as the binary request value. Embedded
  aarch64 SWI tests parse, cross the typed FFI, decode base64 into bounded
  bytes, and materialize the generated request.
- Made concrete request materialization mandatory in the live parser:
  every resolved UI/control invocation contains the generated
  `ActionRequest`, projections must agree on the generated handler identity, and a
  missing request is a hard parse error rather than a fallback to source
  arguments. Local UI actions are the explicit non-wire sum case. The existing
  JSON argument array remains only as the active transport projection to be
  deleted by the binary socket cutover; parsed UI commands already carry the
  generated request instead of requiring transport code to reinterpret parser
  values.
- Added the direct socket request envelope over the disjoint generated action
  and transport opcode namespaces, and made the ordinary reply mode carry the
  closed generated `ActionSuccess` sum. Added exact streaming atom I/O which
  validates the protocol version and bounds before allocation without reading
  into a subsequent raw-stream handoff. Fragmented request, reply, event,
  malformed-length, and wrong-version tests pass on aarch64; the listener and
  clients still require the single coordinated cutover, so that gate remains
  unchecked.
- Deleted the 1,500-line Rust action registry, its supplemental action/schema
  synthesis, dead menu projection, synchronization tests, and duplicate help
  section. `ui_verbs!` now retains implementation dispatch only. The `verbs`
  action and F1 help both query a bounded typed help projection from the
  embedded relation; 108 total actions and the exact 91-action UI subset are
  checked in core-only and aarch64 embedded tests.
- Moved `sudtrace`, whole-box `apply`/`discard`, `rename`, and `quit` control
  execution onto generated `ActionRequest -> ActionSuccess` values. TRACE is
  retained in its compact native form and decoded on demand into a bounded
  typed view; independently versioned unknown TRACE event kinds are preserved
  explicitly. Apply/discard now construct bounded typed path/error results.
  The active newline-JSON listener projects those typed results only at its
  outer boundary; it has not yet been replaced, so the binary socket and
  all-handler result gates remain unchecked. The cleaned aarch64-musl suite
  passes 298 tests with one ignored, and all 42 pure relation tests pass.
- Moved all five `view.*` operations onto generated request/result values and
  removed their JSON result constructors. The materialized registry now
  retains a closed sum of generated change/process/output/pipeline/build-edge
  rows; filtering and window slicing operate directly on those types, and the
  active JSON listener projects them only at its outer boundary. Tests cover
  every concrete window variant. Handler-driven validation also found and
  fixed a schema error: filter kinds are the rule engine's actual `cmd` and
  `err` vocabulary, not the previously guessed `command` and `error`. It also
  fixed lost xattr keys in xattr-only view rows. The aarch64-musl suite now
  passes 300 tests with one ignored; all pure relation and generated-code
  consistency checks pass.
- Removed `serde_json::Value` from the materialized view implementation
  completely. Change/process/output/pipeline/build-edge database readers now
  decode once into the generated relation types; process ancestry uses typed
  `ProcessInfo`, and only the explicit still-active JSON listener projection
  renders those values back to the old UI spelling. Moved `processes`,
  `processes_live`, `outputs`, `brushprov`, and `build_edges` execution onto
  generated request/success variants and deleted their former JSON-producing
  database functions. Nested stored pipeline JSON is normalized at its durable
  representation boundary and has focused old/current record tests. The
  aarch64-musl suite passes 302 tests with one ignored.
- Moved `proc_pipeline`, `output_pipeline`, `pipeline_procs`, `output_detail`,
  `proc_info`, `proc_prov`, `proc_roots`, and `process_env` execution onto the
  generated request/success variants. Their database readers now construct
  closed `PipelineSummary`, `OutputDetail`, `ProcessInfo`, `ProcessSubject`,
  row-id list, and bounded byte-map values; malformed stored JSON is an error
  rather than silently becoming null or empty. Missing historical linkage
  columns have an explicit no-row meaning. Deleted the eight JSON-producing
  database handlers; base64 and JSON spelling now occur only in the temporary
  outer listener projection. Focused conversion/projection tests and the full
  native aarch64-musl suite pass: 304 tests passed and one existing browser
  e2e test remained ignored.
- Moved `api_log`, `api_log_detail`, `webcap`, `webcap_detail`, and
  `webcap_body` onto generated request/success variants and deleted their
  JSON-producing database handlers. Summary reads construct bounded typed
  rows without touching bodies; detail reads retain bounded bytes, and web
  Content-Encoding is decoded only when detail/body is requested. Base64 and
  lossy text conversion are now confined to the temporary outer JSON listener
  projection. Stored booleans, numeric ranges, body bounds, and database errors
  fail closed. Focused projection tests and the full native aarch64-musl suite
  pass: 305 tests passed and one existing browser e2e test remained ignored.
- Moved `writer_id`, `first_writer_id`, and `first_writer_prov` onto generated
  request/success variants. Archive lookups now consume the bounded byte-path
  request, construct optional row identities or closed `WriterProvenance`, and
  surface malformed IDs, argv, and non-UTF-8 archive-path mismatches instead of
  silently returning null. Deleted the three JSON database handlers; their
  old spelling exists only at the temporary listener boundary. Focused tests
  and the full native aarch64-musl suite pass: 306 tests passed and one existing
  browser e2e test remained ignored.
- Moved `display_path`, `resolve_box`, `select`, `ping`, `reload_rules`, and
  `verbs` onto generated request/success variants. Help filtering now executes
  inside the relation's help projection; Rust no longer reinterprets names and
  descriptions with its own substring algorithm. The direct dispatcher
  produces bounded help rows and only the temporary JSON listener spells them
  as objects. All 42 pure relation tests and the generated-source freshness
  check pass; the full native aarch64-musl suite remains at 306 passed with one
  existing browser e2e test ignored.
- Moved `session_dicts` onto the generated request/success variant and deleted
  its mutable JSON row constructor. Discovery now constructs bounded
  `BoxSession` values directly, keeps shared-memory and upper paths as raw OS
  bytes, strictly decodes heterogeneous read-only attachment metadata, and
  merges live process/error state before the temporary listener projection.
  A focused closed-row test was added; the full native aarch64-musl suite passes
  307 tests with one existing browser e2e test ignored.
- Moved `box_new`, `box_drop`, `box_file_read`, `box_file_write`,
  `box_dir_list`, and `box_path_kind` onto generated request/success variants.
  File bodies remain bounded bytes through execution and base64 is confined to
  the temporary JSON projection. Box creation now considers both discovered and
  live IDs and validates an optional parent. Relative-path validation rejects
  absolute paths, traversal, NULs, and the current overlay index's unsupported
  non-UTF-8 keys explicitly. The latter is a recorded remaining defect: the
  wire retains arbitrary bytes, but the overlay's `String` key model must still
  be replaced before byte-path support is complete. The full native
  aarch64-musl suite passes 308 tests with one existing browser e2e test ignored.
- Moved `delete`, `dissolve`, `apply_to_copy`, and `kill` onto generated
  request/success variants. The duplicate JSON-returning lifecycle helpers are
  gone: delete/dissolve share one typed `FreeResult`, parent-copy returns a
  bounded `ApplyCopyResult`, and kill returns unit. Copy allocation now also
  considers live overlay IDs. Existing parent-copy, copy-down, and dissolve UI
  integration tests pass in the full native aarch64-musl suite: 308 passed and
  one existing browser e2e test ignored.
- Moved `rotate` onto its generated request/success variant. The archive-layer
  export, two-layer rewrite, parent swap, and mirror refresh now form a typed
  `Result<RotateResult, String>` implementation with no JSON construction;
  legacy field spelling occurs only at the temporary listener boundary. The
  full native aarch64-musl suite passes 308 tests with one existing browser
  e2e test ignored.
- Moved `stuck` onto its generated request/success variant and deleted the
  schema-less JSON diagnostic construction. `/proc` process/thread discovery,
  descriptor-peer joins, syscall descriptions, and merged backtraces now
  construct bounded `StuckThread` rows directly; blocked-first sorting uses
  their typed state and identities. Only the temporary listener projects the
  result to the old `procs` object array. The full native aarch64-musl suite
  passes 308 tests with one existing browser e2e test ignored.
- Moved `prompts.peek`, `prompts.answer`, and `prompts.ui_active` onto generated
  request/success variants. Pending prompts now materialize directly as bounded
  `NetworkPrompt` records; verdict execution consumes the generated
  `PromptVerdict` enum. Deleted the now-unused handwritten verdict string
  parser, leaving old object spelling only in the temporary listener
  projection. The full native aarch64-musl suite passes 308 tests with one
  existing browser e2e test ignored.
- Moved `flows.list`, `flows.detail`, and `flows.packets` onto generated
  request/success variants. Packet capture still stays in canonical pcapng and
  keylog files; on-demand tshark view decoding now returns bounded generated
  `FlowRow` and `PacketRow` values directly. Deleted their `to_json` methods and
  the parallel flow-argument helper; JSON spelling remains only at the
  temporary listener projection. A focused tshark-row materialization test and
  the full native aarch64-musl suite pass: 308 tests passed and one existing
  browser e2e test remained ignored.
- Moved `mirror_jobs`, `mirror_add`, `mirror_run`, `mirror_run_pending`,
  `mirror_pause`, and `mirror_rm` onto generated request/success variants.
  Stored scheduler rows now materialize as bounded `MirrorJob` values with a
  closed `MirrorState`; invalid numeric ranges, unknown derived states,
  non-UTF-8 scheduler destinations, and malformed database rows fail closed.
  The temporary listener projects the old object spelling only after typed
  execution. The full native aarch64-musl suite passes 308 tests with one
  existing browser e2e test ignored.
- Moved `struct_quick`, `struct_finish`, and `struct_cancel` onto generated
  request/success variants. The structural-diff job registry now retains
  bounded `StructuralLine` values rather than schema-less JSON, and both quick
  and sandboxed finish paths return the closed generated result records.
  Relative path validation and job identity validation happen before the
  implementation; the temporary listener alone projects line pairs. The full
  native aarch64-musl suite passes 308 tests with one existing browser e2e test
  ignored.
- Moved `oaita.models`, `oaita.status`, `oaita.probe`, and `svc.up` onto
  generated request/success variants. Catalog entries, status kind, endpoint,
  probe specification/result, and service name are now bounded closed values;
  the network probe executes directly in Rust and JSON is confined to its
  temporary request/result projection. The full native aarch64-musl suite
  passes 308 tests with one existing browser e2e test ignored.
- Moved `oci.load`, `oci.images`, `oci.resolve`, and `oci.build` onto generated
  request/success variants. The engine build implementation now consumes raw
  bounded compressed-context and Dockerfile bytes instead of a JSON object and
  base64, and returns `OciBuildResult` directly. Its worker handoff is a checked
  decimal box identity rather than a private JSON protocol. Image inventory
  metadata and all numeric ranges fail closed; only the temporary listener
  performs the old base64/object projection. The full native aarch64-musl suite
  passes 308 tests with one existing browser e2e test ignored.
- Moved `ro_attach`, `wiki_attach`, `ietf_attach`, and `git_checkout` onto
  generated request/success variants. Generic attachments now consume the
  closed box/external-reference sum and validate the full bounded list before
  replacing it. Wiki and IETF pinning validate their result records before
  mutating state. Git checkout returns `CheckoutResult`, rejects traversal and
  paths or symlink targets the current UTF-8 overlay index cannot represent,
  and no longer performs lossy conversion or constructs JSON. The temporary
  listener alone translates its legacy rows. The full native aarch64-musl
  suite passes 308 tests with one existing browser e2e test ignored.
- Moved `review.file_bytes`, `review.write_file`, `review.patch_text`,
  `review.change_mode`, `review.session_changes`, and `review.file_groups`
  onto generated request/success variants. File and patch content stay bounded
  bytes until the temporary listener; session rows and file-group membership
  materialize as closed generated records. Removed the JSON session-row
  constructor, JSON write wrappers, and the depot reader that silently erased
  SQLite errors; internal change-path consumers now reuse the typed rows. The
  full native aarch64-musl suite passes 308 tests with one existing browser e2e
  test ignored.
- Moved `review.hunks`, `review.decorate`, and `review.decorate_many` onto
  generated request/success variants. Text hunks now materialize as bounded
  `DiffHunk`/`DiffLine` records and non-text cases as the closed `FileDiff`
  sum, retaining binary and symlink data as bytes. Invalid UTF-8 is classified
  as binary instead of lossily reinterpreted as text. Whole-patch rendering
  walks the same typed diff, and the old JSON hunk/decorating algorithms and
  review-side base64 helper are gone. The full native aarch64-musl suite passes
  308 tests with one existing browser e2e test ignored.
- Moved `review.apply`, `review.discard`, `review.apply_hunk`, and
  `review.discard_hunk` onto generated request/success variants. An empty
  bounded path list explicitly selects the whole change set; otherwise all
  paths are validated before mutation. Whole-change and hunk mutations share
  the running-box guard, hunk writes return ordinary typed errors, archive and
  pool cleanup errors are no longer discarded, and invalid UTF-8 cannot enter
  the text-hunk algorithm through lossy conversion. Removed all four JSON
  implementation wrappers; only the temporary listener projects their legacy
  reply spellings. The full native aarch64-musl suite passes 308 tests with one
  existing browser e2e test ignored.
- Moved `review.recent_changes`, `review.box_summary`,
  `review.pipeline_context`, `review.makevars`, and `review.map_ids` onto
  generated request/success variants. Summary previews, provenance context,
  variable rows, and mapped identities now materialize directly as their
  bounded closed records; live activity is added to the same `BoxSummary`
  before projection. Removed the final JSON-producing review implementation,
  made archive and database iteration errors propagate, and made
  `dispatch_action` exhaustive over every generated `ActionRequest`, so a new
  generated action cannot compile without a handler. Every wire action now has
  a concrete generated request and success path. The full native
  aarch64-musl suite passes 308 tests with one existing browser e2e test
  ignored.
- Replaced every event producer's open JSON object with the generated bounded
  `SubscriptionEvent` sum, including coalesced overlay/process invalidations,
  box lifecycle, build/provenance, API/web capture, and pong. Detailed records
  are no longer copied into invalidation events. The temporary newline listener
  is now only an outer projection of these typed events and will be deleted by
  the coordinated binary listener/client cutover. The full native
  aarch64-musl suite passes 308 tests with one existing browser e2e test
  ignored.
- Began the transport-ingestion side of the atomic cutover by making pipeline
  completion, recipe attribution fixup, and build-edge transitions consume the
  generated `PipelineId`/`ExitCode`/`BuildEdgeTransition` values directly.
  Pidfd-to-box resolution is shared, range and UTF-8 constraints fail before
  storage mutation, and edge-state broadcasts reuse the typed transition. The
  temporary newline listener only constructs these values at its boundary.
  The full native aarch64-musl suite passes 308 tests with one existing browser
  e2e test ignored.
- Moved nested provenance, complete build graphs, make-variable batches, and
  live activity ingestion onto generated `PipelineProvenance`, `BuildEdge`,
  `MakeVariable`, and `ActivityItem` values. Every batch is validated before
  mutation, byte strings fail explicitly where the SQLite schema requires
  text, numeric identities are range-checked, and the old listener's lossy
  `filter_map`/default-empty behavior is gone. The temporary newline listener
  only constructs the closed values at its boundary. The full native
  aarch64-musl suite passes 308 tests with one existing browser e2e test
  ignored.
- Made registration's root-process recording consume the generated
  `ProcessProvenance` record. Executable/cwd/argv/environment bytes and process
  identities are now bounded and validated before insertion; the capture layer
  no longer extracts an open JSON object with default-empty and unchecked-cast
  behavior. The temporary listener constructs that record once at its boundary
  until the full `Register` request cutover. The full native aarch64-musl suite
  passes 308 tests with one existing browser e2e test ignored.
- Detached registration's network setup from the JSON envelope: it now
  consumes the generated `NetMode` plus explicit capture/filter/replay fields,
  and the obsolete untyped replay-as-of side field no longer enters the hot
  path. Replay identities are range-checked before setup. The full native
  aarch64-musl suite passes 308 tests with one existing browser e2e test
  ignored.
- Moved the complete registration input path onto generated
  `TransportRequest::Register` fields. Name authority is the closed
  automatic/host/nested choice; backend and network mode are enums; command,
  process provenance, flags, and replay identity are bounded before the
  registration implementation runs. The temporary newline listener now has
  one strict conversion at its boundary rather than JSON reads throughout
  registration. The full native aarch64-musl suite passes 308 tests with one
  existing browser e2e test ignored.
- Moved registration output onto the generated `RegisterReply`, `OciRuntime`,
  and `SudRuntime` records. Paths and process-facing strings remain bytes,
  DNS and owner identity have fixed binary widths, collections retain their
  distinct schema bounds, malformed stored OCI metadata now fails explicitly,
  and the implementation no longer assembles or mutates a JSON reply. The
  temporary newline listener projects the old spelling only at its boundary;
  that projection has direct coverage until the runner and listener switch to
  `ConnectionMode::Box`. The native aarch64-musl build and registration tests
  pass; the checkpoint's full suite has 310 passing tests with one existing
  browser e2e test ignored.
- Unified nested provenance, pipeline completion, recipe attribution, build
  graph, make-variable, activity, and build-edge-state requests behind one
  typed recording dispatcher. Each path now consumes its generated
  `TransportRequest` variant and returns `TransportResponse::Empty` or
  `Recorded`; the temporary listener projects that closed response only after
  execution. This removes the seven independently assembled JSON success
  replies and makes the eventual binary request path call the same dispatcher.
  The full native aarch64-musl suite passes 311 tests with one existing browser
  e2e test ignored.
- Extended that dispatcher to every non-stream transport reply: budget grants
  now consume the generated broker-or-selector `BoxTarget`, and the former SUD
  post-exit ingest path consumed `TransportRequest::SudIngest` and materialized
  bounded `TransportResponse::SudIngested` errors. The old listener only
  converts its input and projects the response spelling. That ingest path was
  later deleted with the sweep during the shared-SarunFs cutover. Reply-mode
  transport is therefore ready for direct binary framing; register,
  subscriptions, PTY, API, and service requests remain connection-mode
  handoffs rather than ordinary replies.
- Moved PTY handoff onto `TransportRequest::PtySpawn`: argv is a non-empty
  bounded byte vector, dimensions are checked `u16` values, cwd remains an OS
  byte path, and environment is the generated bounded byte map. The PTY
  implementation no longer filters malformed JSON items or silently defaults
  invalid values; it consumes the closed request before acknowledging and
  entering the raw PTY frame mux. The temporary listener performs the sole
  legacy conversion until `ConnectionMode::Pty` is written directly.
- Moved all service control onto generated transport variants. Declarations
  enter the common reply dispatcher as bounded `ServiceDeclare` values and
  fail when they lack a live declaring box; argv and network mode are validated
  before metadata mutation. Park and dial handoffs consume `ServiceAccept` and
  `ServiceDial`, including the shared service-name validation, before the
  socket becomes a parked accept slot or a raw spliced stream. Their remaining
  JSON acknowledgements are solely the temporary listener's mode projection.
- Fixed the command-modal completion composition failure exposed by
  `kill <TAB> C1`. Parsing and context resolution had already produced the
  correct typed `Kill` request, but the preview unconditionally requested the
  obsolete secondary command form and reported its ordinary no-solution result
  as a parser-backend failure. The secondary form has now been deleted entirely:
  each action atom has one mechanically derived textual encoding. Parser
  composition coverage applies a contextual
  completion, reparses it, checks the typed request, highlights it, and renders
  it; a UI-level regression reproduces the exact `kill ` to `C1` workflow. The
  core Prolog suite additionally round-trips minimal and full forms
  for all 108 actions. Native aarch64 verification passes all 42 Prolog tests
  and the full Rust suite (313 passed, 1 browser E2E ignored).
- Enforced the one-action/one-name invariant after the preview bug exposed the
  leftover dual-form model. Deleted `cli_form/3`, both `verb`/`cli` grammar
  branches, canonical-form selection, and Rust's `RenderForm` mode. The one
  action atom now mechanically produces command tokens by splitting identifier
  separators; catalog validation proves those token sequences are unique.
  Parsing, completion, highlighting, help, and rendering all consume that same
  form. `mirror_pause`/`mirror_resume` boolean injection remains solely an
  argument-to-wire projection and cannot rename either action.
- Added real command-modal key-path coverage after `: quit` exposed the gap
  between component tests and user behavior. Control submission no longer
  refreshes sessions/Changes after a failed request, and successful `quit`
  performs its one typed control send then exits the TUI without querying the
  engine it just stopped. The regression drives colon, character, Tab/selection,
  and Enter events through the actual modal dispatcher; a temporary Unix socket
  replies to quit and disappears before any accidental refresh, while the kill
  path asserts the rendered `kill C1` / `kill 1` preview and absence of parser
  diagnostics. Native aarch64 verification passes 315 tests with the browser
  E2E test ignored.

## Stop conditions

The feature is not complete merely because a subset works. Do not stop with:

- a feature flag;
- a Rust fallback;
- two action catalogs kept in sync by tests;
- generated projections that are not the runtime path;
- only mirror actions in Prolog;
- wire parsing, CLI parsing, key dispatch, or help bypassing the relation;
- JSON or Prolog-mediated `ui.sock` message delivery;
- an x86-only normal build.

Completion means the UI action surface has one Prolog definition and every
listed runtime consumer uses it. Packet, patch, and brush grammars extend the
same generic relation/FFI rather than creating another parser subsystem.
