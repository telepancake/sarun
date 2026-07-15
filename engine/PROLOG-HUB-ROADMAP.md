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

The relation and FFI must remain generic enough to host future grammars for
packets/protocol stacks, patches and editing, nested highlighting, build
graphs, and brush syntax. UI actions are the first complete practical client,
not a command-specific architecture baked into the FFI.

## Current bad state being replaced

There are three competing authorities:

- `control::ui_verbs!` / `VERB_DOCS`: 97 wire handler names plus schemas/help;
- `registry::ACTIONS`: 69 explicit action records plus synthesized copies of
  missing `VERB_DOCS` rows;
- `pl/action_grammar.pl`: five duplicated mirror actions.

The normal pre-migration build excludes Prolog. The optional path converts a
successful Prolog result back through `registry::find`, and Rust fallbacks own
parse, completion, highlighting, and rendering for unsupported input. The
mirror CLI calls `parse_words` directly and bypasses Prolog. Generated registry
help/key/menu projections are mostly test-only or dead while the UI retains
handwritten key and menu tables.

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
- future-extensible compound/map/byte values for non-command grammars.

Rust may split command input into neutral UTF-8 byte-spanned source tokens, but
must not classify action literals or argument semantics. Prolog turns neutral
source evidence into semantic evidence. This preserves exact UTF-8 spans while
keeping syntax knowledge in the hub.

## External semantic context

Object-bearing grammars need query-scoped context: box identities and aliases,
paths visible in a particular box, mirror names, process rows, rule names, and
similar live domains. The current command grammar has no such context access;
it validates argument shape and primitive type only. `KnownUnit` has provider
fields, but the command relation deliberately ignores their incoming semantic
classification, and completion currently proposes literals only.

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
- [ ] Add n-way representation queries for canonical verb, CLI, wire, key,
      menu, help/description, and syntax.
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
- [ ] Inventory every `ui.sock` request, reply, event, and stream frame and
      specify bounded binary framing plus typed scalar/array/value encodings.
- [ ] Give every wire action/message a stable binary identity in the relation;
      project/generate the Rust opcode and codec definitions from it.
- [ ] Replace JSON `ui.sock` request/reply transport with direct Rust binary
      encode/decode and handler dispatch, with no Prolog call in message
      delivery and no retained JSON compatibility mode.
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
- [ ] Remove stale parser design text that calls implemented work future or
      describes the Rust registry as phase one.
- [ ] Verify with `rg` that no alternate command authority or fallback remains.

### 6. Verification gates

- [x] Prolog core-only focused tests validate all 114 action rows plus neutral
      parsing, shared forms, normalization, strings, arrays, rendering,
      completion/highlighting, and the closed application surface.
- [ ] Expand them to exhaustive per-representation and per-action round trips.
- [ ] Parse/render round trips preserve typed wire arguments for every action.
- [ ] Canonical verb and all CLI forms parse with exact arity and full input.
- [ ] Numeric-looking strings, paths, base64, and specs remain strings.
- [ ] Optional and repeated arguments preserve exact wire array shape.
- [ ] Completion supports partial tokens and mid-token UTF-8 byte spans.
- [ ] Box/object/path completion is derived from revision-tagged context facts;
      dependent domains such as box paths use earlier parsed identities.
- [ ] Canonical observations compare equal exactly when a context change cannot
      affect the dependent parse; test successful and failed `one` queries.
- [ ] Query graphs reject cycles, dangling refs, type-mismatched refs, duplicate
      query IDs, noncanonical entries, and provider responses beyond bounds.
      Cycles, dangling refs, duplicate IDs, refs to non-`one` queries, duplicate
      entry identities, and noncanonical result ordering are covered now;
      explicit cross-domain ref typing and provider-specific bounds remain.
- [ ] Highlighting is derived only from successful grammar evidence.
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
- Recorded the direct binary `ui.sock` transport boundary: Prolog owns
  representation relationships and conversion, while already-typed
  request/reply delivery is a Rust-only hot path.

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
listed runtime consumer uses it. Future packet/patch/brush grammars then extend
the same generic relation/FFI rather than creating another parser subsystem.
