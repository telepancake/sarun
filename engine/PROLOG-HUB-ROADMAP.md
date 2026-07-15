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
- shell command-line and canonical verb forms;
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
- `form`: canonical verb, shell CLI, wire, key, and menu representations;
- `syntax`: literal/argument syntax class and description provider;
- `normalization`: aliases and injected values such as mirror pause/resume;
- `context`: pane/gate predicates for key and menu actions.

One normalized command result must carry enough information for Rust to
dispatch without looking up semantic metadata elsewhere: action identity,
handler identity, target, and typed wire-ready arguments.

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

The migration must cover all 97 names emitted by `control::ui_verbs!`, not just
the five mirror actions. It must also cover local and top-level control actions
currently declared only in `registry::ACTIONS`:

- control messages: `apply`, `discard`, `rename`;
- local actions: `mirror_browse`, `mirror_read`, `change_read`, `change_edit`,
  `rule_new`, `rule_delete`, `rule_edit`, `quit`, `detach`, `refresh`, `filter`,
  `action_menu`, `toggle_mark`;
- alias/normalization action: `mirror_resume` -> handler `mirror_pause` with a
  wire boolean of `false`.

CLI forms currently requiring explicit relation facts include:

- `mirror ls`, `mirror add`, shared `mirror run`, `mirror pause`,
  `mirror resume`, `mirror rm`;
- `attach wiki`, `attach ietf`, `checkout`;
- `oci load`, `oci build`.

All actions also receive their canonical verb form. Shared CLI forms must be
resolved relationally by complete schemas and end-of-input, not heuristic word
classification.

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
- [ ] Decide and test native aarch64 runtime behavior where the separate SUD
      wrapper backend is x86-specific; do not claim full `make engine` support
      until this is explicit and working.
- [ ] Copy third-party license notices beside every normal release artifact.

### 2. Generic typed FFI — NEXT

- [x] Replace the five-value Rust `Action` enum with owned canonical/handler
      identities returned by Prolog.
- [x] Replace `CommandArg::JobId` with generic typed values including arrays.
- [x] Add neutral source-token semantics; remove Rust `grammar_unit` command
      classification.
- [ ] Make the application operation vocabulary extensible by grammar/domain,
      rather than hard-coding a mirror-command-only Rust API.
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
- [x] Implement aliases, injected arguments, shared CLI paths, exact
      end-of-input resolution, visibility, targets, and handler identities.
- [x] Generalize parse/render/completion/highlight predicates over all actions.
- [x] Add n-way representation queries for canonical verb, CLI, wire, key,
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
- [x] Resolve successful `one` observations into wire-ready command arguments,
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
- [x] Route the mirror CLI through the same relation (its `parse_words` call is
      now only neutral framing into the mandatory parser).
- [ ] Route every other command ingress through the same relation.
- [x] Inventory every `ui.sock` request, reply, event, and stream frame and
      specify bounded binary framing plus typed scalar/array/value encodings.
      The durable cutover contract is `UI-SOCK-BINARY.md`.
- [x] Give every wire action a stable binary identity in the relation. Alias
      actions normalize to their handler's identity and schema; local actions
      have no invented wire form.
- [x] Give every wire action a concrete binary request-field schema in the
      relation and relate parsed/context-resolved values to that schema.
      Source/parser categories such as `integer`, `path`, `base64`, and
      especially `spec` are representations to convert from, not binary field
      types. Structured requests such as OCI build, API probe, view filters,
      and read-only attachments must be closed records/choices; no generic
      JSON-shaped request payload may survive.
- [ ] Give every wire action handler a concrete typed result schema in the
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
- [ ] Delete `registry::ACTIONS`, supplemental action synthesis, schema/type
      inference, CLI maps, target/alias switches, and dead projections.
- [ ] Delete `registry.rs` once no semantic responsibility remains.
- [ ] Strip name/schema/help metadata out of `ui_verbs!`; retain only handler
      implementation dispatch or replace it with handler functions.
- [ ] Delete `VERB_DOCS`; implement the `verbs` response from relation data.
- [ ] Delete duplicate help sections, dead `registry_menu_items`, and registry
      synchronization tests.
- [x] Remove stale parser design text that calls implemented work future or
      describes the Rust registry as phase one.
- [ ] Verify with `rg` that no alternate command authority or fallback remains.

### 6. Verification gates

- [x] Prolog core-only focused tests validate all 114 action rows plus neutral
      parsing, shared forms, normalization, strings, arrays, rendering,
      completion/highlighting, and the closed application surface.
- [x] Expand them to exhaustive per-representation and per-action round trips.
- [x] Parse/render round trips preserve typed wire arguments for every action.
- [x] Canonical verb and all CLI forms parse with exact arity and full input.
- [ ] Numeric-looking strings, paths, base64, and specs remain strings.
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
- Added a 114-action Prolog catalog covering the 97 UI wire verbs plus
  control/local/alias actions, with schemas, targets, visibility, descriptions,
  canonical forms, explicit CLI forms, and normalization.
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
  them back into the relation for wire-ready resolution. The UI box provider
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
  action handlers in the relation. Wire projection occurs after alias
  normalization, so (for example) `mirror_resume` projects the actual
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
  form relation. Canonical verb, CLI, syntax, wire, help, key, and menu values
  now project from the normalized facts and executable specs. Exhaustive tests
  render and reparse minimal and fully populated canonical forms for all 108
  actions and every explicit CLI form, covering optional/repeated shapes.
- Added the normalized non-action transport relation: 16 requests, 5 response
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
  aliases and contextual identities.
- Replaced the two unconstrained `SPEC` terminals with action-specific JSON
  source relations for OCI builds and API probes. Parsing and canonical
  rendering are pure and core-only; object order is irrelevant, while unknown
  or duplicate fields fail. The parsed terms relate to bounded
  `oci_build_spec` and `api_probe_spec` records, so JSON is only a textual
  representation and never survives as the binary request value. Embedded
  aarch64 SWI tests parse, cross the typed FFI, decode base64 into bounded
  bytes, and materialize the generated request.

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
