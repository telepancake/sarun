# Brush relation migration

This is the durable implementation plan for making the generic relation engine
the singular parser and representation hub for shell text used by standalone
Brush, box Brush, Kati recipes, embedded builtins, and editors. Keep the
existing Brush execution parser intact while building and measuring the new
relation. Do not delete the reference parser and then hope to recreate its
behavior.

The in-process editor work has its own durable implementation checklist in
`EDITOR-RELATION-MIGRATION.md`. Its shell analysis and Brush-interactive
analysis are the same required `sarun_brush` client, not parallel parsers.

## Current authority map

Brush currently has one strong execution parser and several adjacent partial
interpreters:

- `brush-parser` tokenizes shell source and applies its PEG grammar to produce
  `brush_parser::ast::Program`; `brush-core` executes that AST. This is the
  reference implementation during migration.
- `brush-interactive` validation calls `Shell::parse_string`, so incomplete
  versus complete input is based on the execution parser.
- interactive highlighting calls `tokenize_str_with_options`, then parses each
  word separately and heuristically classifies command position. It does not
  require the complete surrounding program to parse and does not reset all
  grammar state through an ordinary AST traversal.
- sarun's Reedline completion path now requires a host semantic provider and
  presents exact edits from `sarun_brush`; its former `COMP_WORDBREAKS` lookup
  call has been deleted. Brush-core still contains the old completion machinery
  for not-yet-migrated upstream/basic-backend consumers, but it is no longer an
  authority in standalone `sarun brush`.
- heredocs are recognized by the execution tokenizer and PEG grammar, including
  dynamic delimiters, `<<-`, quoted-delimiter expansion rules, and heredocs
  nested inside command substitutions. `IoHereDocument::location()` currently
  returns `None`, so the AST alone is not yet a complete highlighting source.
- `SourcePosition::index` is documented as a character index. Sarun's relation
  and Reedline spans are byte offsets. Every adapter must convert explicitly;
  treating these coordinate systems as interchangeable is a correctness bug.

The migration replaces the partial interpreters with projections of one
successful relation. It does not make them fallbacks. Until a consumer reaches
its cutover gate, the old consumer remains authoritative; after cutover, its
old algorithm is deleted.

## Intermediate acceptance target: interactive Brush

The next externally usable milestone is a standalone interactive `sarun brush`
whose entire editing experience is relation-owned. Before moving the execution
parser, its Reedline session must use one analysis of the current source and
cursor for:

- syntax highlighting, including nested substitutions and heredoc bodies;
- literal, variable, command, filesystem, builtin-argument, and sarun-domain
  completions with exact replacement spans;
- complete/incomplete/invalid validation, continuation behavior, diagnostics,
  and indentation;
- syntax/help hints and semantic descriptions;
- explicit context queries and dependency keys against the persistent Brush
  shell and sarun state.

This is a real authority cutover, not a demo or opt-in mode. At the cutover,
the old Brush-interactive tokenizer highlighter, `COMP_WORDBREAKS` completion
path, and parser-based validator are removed from the sarun interactive path;
there is no runtime fallback. Brush's existing AST parser may still execute the
accepted command text until the later execution-AST parity gate. Thus the
intermediate product has one parsing authority for the interactive editing
experience while retaining a separately scoped, measured execution adapter.

Acceptance requires PTY-level tests that type, edit, highlight, complete, and
submit commands in the actual `sarun brush -i` session on aarch64. Unit tests
of grammar terms or Reedline adapters alone do not satisfy this milestone.

### Consumer inventory (2026-07-16)

- sarun execution/provenance: `brush.rs` parses top-level box scripts,
  standalone/nested scripts, snooped shell scripts, and Kati recipe strings;
  it executes deliberately separated complete commands with `run_program`.
- sarun setup/test execution: `brush.rs` uses `run_string` for shell option
  setup, snooped prefixes, and builtin-boundary fixtures.
- interactive execution: `brush-interactive/interactive_shell.rs` executes
  accepted input and prompt commands with `run_string`.
- interactive validation: Reedline and basic backends call `parse_string`.
- interactive presentation: Reedline calls the independent
  `highlight_command` and `Shell::complete` paths described above.
- Brush-core internal execution: `shell/execution.rs` parses `run_string`,
  `commands.rs` parses command-bearing builtin input, and `shell/traps.rs`
  executes stored trap text. These are later execution cutovers, not hidden
  exceptions.
- Brush-core completion: `shell/completion.rs` delegates to the independent
  token-based completion configuration in `completion.rs`.

## Target composition

The shell grammar is an immutable client grammar interpreted by the generic
engine. It relates at least these independent representations:

```
raw UTF-8/byte source + typed tears + local environment + external observations
    <-> shell text AST
    <-> concrete/evidence tree with byte spans
    <-> Brush execution AST              (sarun-owned glue relation/adapter)
    <-> rendered shell source
```

Highlight spans, completion candidates, diagnostics, indentation, incomplete
input, syntax help, and dependency keys are wanted projections of that same
relation. They are not grammar walkers in Reedline or Rust.

The Brush AST adapter is client glue, just like the existing action
TextAst/WireAst adapter. The grammar must not construct Rust-specific Brush AST
objects, and the engine must not acquire `parse_shell` cases. While both ASTs
exist, bounded structural conversion plus explicit client-owned reshapes is
allowed. If a mapping is genuinely relational, install it as immutable glue
grammar data.

## Context model

Shell-local bindings are relation state, not external queries:

- assignments and positional/special parameters;
- lexical/function scopes and function definitions;
- aliases and shell options that affect tokenization or parsing;
- command position, redirection state, and heredoc delimiter queues.

State transitions are pure and lexically scoped. A declaration/assignment
installs a typed local binding; subsequent references search the scope chain
and emit no context query when that binding resolves them. Closing a scope
drops non-escaping bindings. Escaping shell effects are returned as an explicit
state delta and may seed the next relation request, but their later use within
the request that created them is still local. An external query is emitted only
for information not derivable from the request's initial state and local
transitions, or for an explicit surrounding-context constraint such as an
ODR/conflict check. Thus the analogue of C parameters/locals never pollutes the
dependency trace, while an unresolved free name does.

Live or potentially expensive namespaces are explicit context queries:

- builtins and their typed argument grammars;
- functions and aliases supplied by the persistent Brush instance when they
  are not already present in the submitted local environment;
- executable names and PATH resolution;
- filesystem paths relative to the logical shell cwd;
- environment variables and programmable completion specifications;
- sarun boxes, paths, actions, mirrors, rules, and other application domains.

Each dependency uses the existing `ask(empty|one|all, Domain, Selector)`
algebra. A command-name tear can ask for all builtin/function/alias/executable
prefix matches; an exact executable can ask for one; a cheap command viability
check can ask whether the matching set is empty. Nested constructs retain their
own query identities and byte spans. Changing a variable, alias, builtin set,
cwd/PATH snapshot, or sarun snapshot invalidates only text whose returned
dependency keys changed.

Builtin argument syntax is grammar composition. After a shell command-position
match resolves to a builtin identity, the builtin's grammar parses its argument
region and supplies its completions/help/highlights. Kati embeds the same shell
grammar for recipe bodies; its Make grammar and the shell grammar keep distinct
ASTs and compose at the recipe representation boundary.

## Required generic engine capabilities

Do not implement these as Brush-specific engine branches:

- bounded raw multiline sources with byte spans, trivia, zero-length tears,
  and exact UTF-8 boundary validation;
- named recursive rules and rule references;
- sequence, choice, optional, bounded/unbounded repetition, fields, and AST
  construction with source ownership;
- longest/operator token choices and declarative precedence/associativity;
- threaded pure local state for scopes, command position, parser options, and
  dynamic delimiter queues;
- nested grammar embedding with span rebasing, including command substitution,
  arithmetic, parameter operators, and Kati recipe bodies;
- dynamic-delimiter regions for heredocs, with quoted delimiters controlling
  whether the body embeds the expansion grammar;
- successful-parse evidence for every concrete token, trivia region, tear,
  AST field, diagnostic, and context dependency;
- explicit complete/incomplete/invalid status without accepting a torn prefix
  whose concrete suffix cannot parse;
- bounded ambiguity, recursion, inference, source bytes, evidence, context
  nodes, primitive work, and output bytes on every request;
- caching keyed by immutable grammar content, source slices, local-state
  inputs, and context dependency outcomes, without hidden mutable Prolog facts.

## Work sequence

### Restart audit (2026-07-19)

The editor and Reedline completion paths prove the boundary is executable. The
composition recovery run completed the following fixed implementation order:

1. Add grammar-neutral AST-node relation application with the local-state
   snapshot visible at that node.
2. Lower symbolic local references to virtual command text while preserving
   the physical source tear/replacement span; delete the whole-document
   builtin-grammar shortcut.
3. Prove the engine with a foreign supplied grammar, multiple command
   positions, state timing, direct/propagated tears, unknown commands, and
   UTF-8 spans before using a sarun builtin.
4. Make `find` expose the same declarative argument grammar used by execution;
   do not add `find` knowledge to Rust adapters or the generic engine.
5. Add an immutable Brush semantic snapshot and one resolved/dependency-keyed
   analysis service shared by editor and Reedline; route the first
   shell-variable and filesystem domains. The general composite-provider
   registry remains later provider-convergence work.

The result is now an executable checkpoint rather than a plan: supplied command
grammars run at every parsed command node with the local state visible there;
symbolic local values preserve physical edit spans; `find` owns the native
`-type`/`-xtype` catalog; and the editor/Reedline clients resolve immutable
Brush-variable and filesystem snapshots through one analysis loop. The
generated command grammar is installed once and passed to nested applications
by opaque handle.

The mandatory baseline includes the focused Prolog suites, the static Rust
relation/editor tests, and every native aarch64 PTY binary. Building test
binaries without executing them is not a gate. The production backward-
completion test, editor insertion test, and real editor PTY are green at this
checkpoint.

### Existing-parser adaptation checkpoint (active, 2026-07-19)

Do not grow the neutral `CommandSyntax` bootstrap into a second implementation
of Clap, find, Kati, coreutils, or any embedded operand language. The preferred
integration boundary is a registered parser relation behind an opaque grammar
handle:

```
rich source/argv + optional tear + observations
    <-> command-owned parser adapter
    <-> typed invocation AST + parse evidence + context queries
```

The same adapter has two projections, not two parsers. Exact execution parsing
returns the typed invocation consumed by the existing executor. Assist parsing
also records the successful parser path, expected terminals/value domains,
source spans, and suspended context requirements at the tear. The generic
relation engine composes those results with shell state, nested grammars, and
external observations; it never switches on command names or parser kinds.

A fake environment must be recording and suspending, not a no-op that returns
empty/default values. When parsing needs external information, it emits the
ordinary `ask(empty|one|all, Domain, Selector)` query and suspends that
candidate. After the relation resolves the observation, the parser adapter is
replayed deterministically. Locally defined values continue to resolve inside
the relation without external queries.

The input adapter must preserve cooked bytes, physical source spans, symbolic
values, and a typed zero-width tear. Existing parsers may keep their functional
core, but conversions to plain `String`, `OsString`, or `&str` must not erase
the provenance needed for evidence and completion. Parsing must be separated
from execution where an existing entry point currently performs I/O while
recognizing arguments.

Implementation order:

1. [x] Add the generic registered-parser request/result boundary and bounds;
       prove that the engine treats it as an ordinary grammar relation. The
       first context-free host adapter crosses the explicit query/observation
       protocol and records its implementation revision as a dependency on
       native aarch64. Context suspension by the adapter remains item 5.
2. [x] Instrument the shared Brush/Clap command boundary once. The token-level
       probe calls the real `T::new`, retains the concrete suffix, records
       consumed/expected Clap argument identities, and tests option candidates
       by submitting them to that parser. Generic `edit` and `bind` fixtures
       are green with no command-name cases. Before production cutover its
       `CommandProbeInput` must grow from plain argv strings into the rich
       word/provenance representation described above; this checkpoint does
       not authorize flattening quoted, expanded, or symbolic shell words.
   [x] Route probe results through the opaque registered relation handle and
       ordinary completion evidence. The handle is required process glue, not
       an opt-in mode. `bind -m |` finite values and `edit ./t|` filesystem
       values come from the same typed parser probe; the latter visibly emits
       an ordinary scoped `ask(all, filesystem_path, prefix("./t"))`, replays
       only after its observation arrives, and records both parser-revision and
       filesystem dependencies in the final reply. A native aarch64 test covers
       the complete-but-still-completable matched-argument case. Replay input
       is the provenance-free `(query id, query, outcome)` dependency value;
       provider identity/revision remains in the outer trace and cannot alter
       parsing when the typed result is unchanged.
   [ ] Compose that handle at Brush command nodes after rich shell words cross
       the boundary. The current proof accepts only already-cooked simple
       words and deliberately rejects quoting and expansions rather than
       flattening their provenance. Same-word concrete suffixes are preserved:
       `bind -m em|-standard` offers only the prefix `emacs`, after the real
       typed parser accepts the rejoined `emacs-standard`; incompatible
       suffixes produce no completion.
   [x] Remove the engine's hard-coded nested-grammar `text_source` call shape.
       An AST/state application now supplies a unique list of named relation
       bindings, with `state_text_source(Expression)` as one generic
       materializer. Application bindings explicitly replace inherited names;
       the engine still knows neither Brush nor a command/parser kind. This is
       the API needed for the next `argv_source` materializer rather than an
       additional parsing mode.
   [ ] Capture `command_words` as separate spanned symbolic command/argument
       fields, make quote/escape cooking grammar-owned, and replace the
       temporary flat source binding with that `argv_source`.
       [x] The generic AST adapter can now project repeated named fields as
           `symbolic_word(Span, Fragments)`. Every literal, reference, tear,
           or opaque fragment retains its source identity, physical UTF-8 byte
           span, and grammar-declared attributes such as quote mode. References
           also carry their exact use identity. The remaining work is to make
           Brush declare the projection and replace the flat adapter input.
3. [ ] Cut those two builtins to execute the typed invocation returned by that
       same exact parser, then delete their generated `CommandSyntax` copies.
4. [ ] Adapt one simple uutils parser/executor split (`cat`), then one embedded
       operand grammar (`cut` or `chmod`) through the same boundary.
5. [x] Add generic recording/suspending context to a parser that needs it. A
       registered adapter's scoped query graph is ordinary relation output;
       observations are explicit typed replay input, repeated suspension is
       bounded by the consumer's stabilization loop, and stalled replay is a
       visible diagnostic rather than hidden provider access.
   [ ] Add Kati's `MakeInvocation` and its evaluated-target snapshot provider
       through the same protocol.
6. [ ] Introduce `FindPlan`/`FindExpr`, move parse-time I/O into explicit
       context/compile phases, and replace the temporary `-type` schema slice.

The generated `CommandSyntax` grammar remains a temporary viability client for
coverage not yet adapted. It is not the target universal command grammar and
must shrink as real parser adapters cross their exact-runtime gates.

### 0. Preserve and measure the reference

- [x] Inventory every `Shell::parse_string`, `run_string`, `run_program`,
      highlighter, completer, and validator consumer in sarun and vendored
      Brush; classify execution, validation, presentation, or provenance use.
- [x] Check in an initial aarch64-tested reference-status corpus for Bash/POSIX
      nested substitutions, variables, UTF-8, compounds, functions,
      arithmetic, pipelines, quoted/unquoted/multiple heredocs, and representative
      incomplete and invalid sources.
- [ ] Build a checked-in differential corpus covering POSIX and Bash modes,
      valid/invalid/incomplete input, UTF-8, comments/trivia, assignments,
      functions, compound commands, pipelines, redirections, expansions,
      substitutions, process substitution, and heredocs.
- [ ] Record normalized reference parse outcome and AST shape without treating
      Brush's incomplete source locations or display formatting as truth.
- [ ] Add user-facing interactive fixtures for cursor placement, replacement
      span, expected syntax class, and context-dependent candidate identity.

### 1. Prove recursive source parsing in the generic engine

- [x] Execute the first foreign raw UTF-8 grammar through the uniform relation:
      immutable named recursive rules, sequence/choice/optional/repetition,
      fields, literals, declarative codepoint sets, exact consumption, generic
      AST nodes, evidence/highlights, and UTF-8 byte spans. Unsupported IR
      constructs return an explicit mode diagnostic rather than `no_solution`.
- [x] Replace the flat pre-tokenized limitation with raw-source grammar values,
      recursive rule references, grammar-owned trivia, lexical regions, and
      byte-span evidence. Trivia consumption is deterministic at the nearest
      syntactic boundary rather than an exponential AST ambiguity.
- [x] Install the first Brush-owned immutable grammar behind an opaque handle
      and execute its shell-word slice through the generic engine: plain text,
      escapes, single/double quotes, named/braced/special parameters, nested
      command substitution, and grouped arithmetic. Declarative negative
      lookahead expresses lexical boundaries without shell-specific engine
      behavior. This is a relation client and remains shadow-only.
- [x] Derive highlighting and literal tear completion from the same successful
      word parses. A zero-width tear records `$(` only when the ordinary parser
      can consume the concrete `echo hi)` suffix; tear state is linear, so it is
      consumed exactly once even inside repetition.
- [x] Represent grammar-declared symbolic terminal tears as ordinary ground AST
      and evidence values without consuming source bytes. This lets later
      semantic relations constrain a hole inside free text while the ordinary
      parser still proves the concrete prefix and suffix; terminals without
      symbolic tear metadata retain their finite parsing behavior.
- [ ] Differentially test UTF-8 byte offsets and render/parse round trips.

### 2. Shell program structure and dynamic regions

- [x] Establish the first program slice over the same Brush grammar value:
      whitespace-separated simple commands, pipelines, `&&`/`||`, `;`, newline,
      backgrounding, and operator/trivia highlighting. Lexical maximality keeps
      adjacent codepoints in one word while trivia separates command words.
- [ ] Add compound commands, functions, full assignment forms, redirections,
      and parser-option gates. The initial lists, pipelines, `&&`/`||`, and
      backgrounding slice is already covered above.
- [ ] Add heredoc delimiter queues, `<<-`, quoted delimiter semantics, multiple
      heredocs, and nested heredocs in command substitution.
- [ ] Prove highlighting and completion inside expandable heredoc bodies use
      embedded ordinary parsing; quoted heredoc bodies remain literal.
- [ ] Return exact incomplete/invalid diagnostics from the same relation.

### 3. Context and composed command grammars

- [x] Establish the generic pure scoped-state algebra independently of shell
      syntax: enter/leave, lexical versus escaping definitions, replace versus
      unique policy, nearest-scope lookup, explicit escaping deltas, and
      external queries only for unresolved uses or explicit requirements. The
      C-shaped `f/x/y/z` fixture and shell `x=123; use(x)` fixture pin that local
      resolution produces no context query.
- [x] Add a separately composable, declarative AST-to-state adapter relation.
      Client data selects named nodes and fields and emits generic state steps
      before or after child traversal; the engine has no grammar-name cases.
      Exact UTF-8 field text comes from parser-owned byte spans. A composed
      foreign `let λ; use λ; use z` fixture proves that only `z` escapes to an
      external query. The same boundary now carries the initial Brush rules
      described below.
- [x] Encode assignment-only shell variables and simple parameters as pure
      relation inputs/outputs. Escaping deltas, later local resolution,
      unresolved `one` queries, observations, and dependency keys are covered.
- [ ] Extend shell-local state to prefix assignments, functions, compound
      scopes, braced operators, special parameters, aliases, and parser-option
      effects.
- [x] Add a grammar-independent symbolic text constraint relation over scoped
      state. A later `reference(Domain, Name)` resolves through the local scope
      chain to an earlier source hole, and a finite typed value relation emits
      ordinary ranked completion evidence for every matching value. Prefix and
      suffix text constrain the replacement; cyclic, missing, and malformed
      bindings fail closed. Ordinary embedded command grammars still need to
      produce these generic constraints through their AST/value glue.
- [x] Add declarative AST-to-symbolic-text projection rules to the existing
      client glue boundary. Grammar data chooses source-text, child
      concatenation, or typed field reference per named node; the generic
      adapter preserves terminal holes and normalizes adjacent text. Brush
      assignments now store `text([...])` values, so a tear inside `A="|"`
      survives local resolution at a later `$A` use.
- [x] Compose an ordinary supplied command grammar at each parsed Brush command
      position, including propagation through locally resolved shell values.
      The removed `signature`/`following`/`positional` mini-language is not an
      acceptable implementation. The target fixture remains
      `A="|"; find . -type $A`: the later ordinary `find` grammar must constrain
      the earlier tear through unification, with no `find` branch in Rust or in
      the generic engine.
- [ ] Add explicit providers for aliases, functions, builtins, PATH commands,
      filesystem names, environment, and programmable completion specs.
- [x] Compose the first real builtin argument grammar from the same declarative
      definition that parses execution arguments. Every `Command` registration
      exposes its Clap command value; one generic sarun adapter emits only
      ordinary text grammar IR. `bind|`, `bind |`, and `bind -m |` therefore
      offer valid continuations through ordinary parse evidence in document
      analysis and native aarch64 interactive PTY tests. No command name is
      switched on and no argument-layout vocabulary exists in the engine.
- [ ] Extend composed builtin grammars beyond enum-after-flag to positional
      values, repetitions, and mutually dependent arguments while retaining
      the same definition and relation path. The first contextual positional is
      complete: `edit` declares `PATH` once, the generic adapter emits an
      ordinary nested context expression, and the tear relation consumes a
      typed logical-cwd snapshot. Non-path values and cross-argument
      dependencies remain.
- [ ] Compose sarun's action and object domains in shell argument positions.
- [ ] Prove dependency-key stability and selective invalidation when cwd,
      PATH, variables, builtins, or sarun snapshots change.

### 4. Shadow integration without execution risk

- [ ] Run the relation in a test/debug shadow harness beside Brush parsing and
      report structured mismatches; never silently choose whichever succeeds.
- [ ] Adapt relation `ShellTextAst` to Brush AST and compare normalized trees
      on the differential corpus and real captured Kati/Brush commands.
- [ ] Measure latency, inference, allocation, evidence size, and cache behavior
      on interactive edits and large scripts on aarch64.
- [ ] Fuzz malformed, truncated, deeply nested, and adversarial sources under
      all request bounds.

### 5. Consumer cutovers

- [ ] Cut Reedline highlighting to relation evidence, then delete the old
      tokenizer/word-piece highlighter.
- [x] Cut sarun's Reedline completion to ordinary tear evidence and delete its
      call into `COMP_WORDBREAKS` tokenization/lookup. The backend constructor
      requires a neutral semantic provider and has no optional provider or
      fallback. A native aarch64 PTY proves `bind -m |` shows the canonical
      builtin-definition values in an actual standalone `sarun brush` session;
      another proves `bind|` exposes valid ` -m VALUE` continuations, and a
      third proves `edit ./t|` consumes a logical-cwd context observation and
      offers `./test1.sh`. Live variables, functions, builtins, and PATH-command
      observations remain below.
- [ ] Cut validation/indentation/diagnostics after complete/incomplete/invalid
      parity is proven.
- [ ] Cut provenance AST consumption, standalone Brush, box Brush, sourced
      files, nested shells, and Kati recipes one consumer at a time.
- [ ] At each cutover, use exactly one authority. No runtime fallback between
      Brush parsing and relation parsing is permitted.
- [ ] Remove the old parser dependency only after every execution and
      presentation consumer has crossed its gate and the full differential,
      integration, fuzz, static aarch64, and x86_64 suites pass.

### 6. Interactive Brush acceptance gate — INTERMEDIATE TARGET

- [ ] Expose one required, relation-neutral analysis-provider interface at the
      sarun/Brush-interactive boundary. Sarun supplies it; Reedline does not
      import Prolog types or reinterpret shell syntax. The completion slice is
      now required and neutral; broaden this same boundary to the other
      projections rather than adding parallel provider interfaces.
- [ ] Make one cached analysis result feed highlighting, completion,
      validation, indentation, diagnostics, and hints for a buffer revision.
- [ ] Supply pure snapshots/observations from the persistent Brush shell and
      sarun state, and invalidate by returned dependency keys.
- [ ] Delete the old highlighter/completer/validator authority from sarun's
      Reedline construction in the same commit that installs the relation
      provider. No optional constructor, feature toggle, or fallback remains.
- [ ] Pass checked-in PTY fixtures for nested syntax, UTF-8 edits, variables,
      command/PATH and filesystem completion, builtin grammar, heredocs,
      incomplete input, invalid input, and a submitted command that executes.

## First acceptance fixtures

The initial corpus must include at least:

```sh
name=world; printf '%s\n' "hello ${name:-nobody}"
printf '%s\n' "$(printf '%s' "$name")" | sed 's/world/WORLD/'
for x in one two; do printf '<%s>\n' "$x"; done
cat <<EOF
expanded $name and $(printf nested)
EOF
cat <<'EOF'
literal $name and $(printf not-executed)
EOF
value=$((base + 2 * step))
sarun verbs mir
```

For each cursor-bearing variant, the fixture records source bytes, cursor byte
offset, parser mode, local context, external observations, wanted projections,
and expected semantic identities—not merely inserted strings or colors.
