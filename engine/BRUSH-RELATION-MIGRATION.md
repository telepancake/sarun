# Brush relation migration

This is the durable implementation plan for making the generic relation engine
the singular parser and representation hub for shell text used by standalone
Brush, box Brush, Kati recipes, embedded builtins, and editors. Keep the
existing Brush execution parser intact while building and measuring the new
relation. Do not delete the reference parser and then hope to recreate its
behavior.

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
- completion separately splits input with `COMP_WORDBREAKS`, treats the first
  resulting token as the command, and runs programmable/basic lookup logic.
  This is not evidence projected from an ordinary parse.
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

### 0. Preserve and measure the reference — NEXT

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
- [ ] Replace the current flat pre-tokenized sequence limitation with raw-source
      grammar values, recursive rule references, trivia, and byte-span evidence.
- [ ] Implement a small shell-word slice: plain text, escapes, single/double
      quotes, parameter expansion, command substitution, and arithmetic.
- [ ] Derive highlighting and a tear completion from successful parses of that
      slice, including concrete text to the right of the cursor.
- [ ] Differentially test UTF-8 byte offsets and render/parse round trips.

### 2. Shell program structure and dynamic regions

- [ ] Add lists, pipelines, `&&`/`||`, backgrounding, compound commands,
      functions, assignments, redirections, and parser-option gates.
- [ ] Add heredoc delimiter queues, `<<-`, quoted delimiter semantics, multiple
      heredocs, and nested heredocs in command substitution.
- [ ] Prove highlighting and completion inside expandable heredoc bodies use
      embedded ordinary parsing; quoted heredoc bodies remain literal.
- [ ] Return exact incomplete/invalid diagnostics from the same relation.

### 3. Context and composed command grammars

- [ ] Encode shell-local variables/scopes as pure relation inputs and outputs.
- [ ] Add explicit providers for aliases, functions, builtins, PATH commands,
      filesystem names, environment, and programmable completion specs.
- [ ] Compose builtin argument grammars after command resolution; start with a
      small representative set containing options, enums, paths, repetitions,
      and mutually dependent arguments.
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
- [ ] Cut Reedline completion to tear evidence plus explicit context queries,
      then delete `COMP_WORDBREAKS` completion tokenization and lookup routing.
- [ ] Cut validation/indentation/diagnostics after complete/incomplete/invalid
      parity is proven.
- [ ] Cut provenance AST consumption, standalone Brush, box Brush, sourced
      files, nested shells, and Kati recipes one consumer at a time.
- [ ] At each cutover, use exactly one authority. No runtime fallback between
      Brush parsing and relation parsing is permitted.
- [ ] Remove the old parser dependency only after every execution and
      presentation consumer has crossed its gate and the full differential,
      integration, fuzz, static aarch64, and x86_64 suites pass.

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
