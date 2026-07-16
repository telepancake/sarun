# Relation-backed built-in editor

## Outcome

Sarun has one reusable in-process text editor frontend, built on Ratatui and
`edtui`, which can be mounted in the main UI or run as the foreground `edit`
Brush builtin. Shell documents use `sarun_brush` as their sole analysis
authority for parsing, highlighting, completion, hints, diagnostics,
indentation, local state, context queries, and dependency keys. There is no
tree-sitter fallback for shell text and no editor-side shell tokenizer.

The builtin runs inside the same process and borrowed Brush shell context. It
can therefore supply shell variables, aliases, functions, builtins, options,
cwd, PATH, and completion configuration directly, then compose those immutable
snapshots with Sarun, Kati, filesystem, and future provider domains through the
same query protocol. The editor never switches on provider-specific domains.

## Existing implementation

- `src/editor.rs` owns `EditorPane`, the `edtui` vim-modal buffer, dirty and
  read-only state, saving bytes, and a debounced highlight cache.
- `compute()` and `lang_for_path()` send Bash and seven other file types to
  syntastica/tree-sitter. Highlights are whole-buffer results converted to
  character columns, with only a cursor-centred 400-row window injected into
  `edtui` each frame.
- `src/ui.rs` is currently the only host: it opens host and box bytes, routes
  keys, saves directly or over the control socket, and renders the pane.
- `box_builtins()` in `src/brush.rs` does not register an editor.
- The editor has no completion UI, hints, diagnostics, indentation projection,
  context-provider execution, dependency invalidation, or standalone terminal
  event loop.
- Brush-interactive still has separate Reedline highlighter/completer/validator
  algorithms. They are migration targets, not reusable analysis authorities.

## Required boundaries

### Document analysis

Expose an owned, relation-neutral Rust request/result over the generic
`Prolog::transform` boundary. `editor.rs` and Reedline must not construct or
decode Prolog terms.

The request carries the grammar handle, exact source, optional edit tear,
initial local state, external observations, and wanted projections. The result
carries AST/status, byte-span highlights, completions, hints, diagnostics,
indentation, context queries, dependency keys, final state, and delta. A single
request may ask for several projections; enrichment is not a second parse.

### Context providers

Use one domain-routed composite provider implementing the existing pure query
snapshot contract. Grammar data chooses domain, selector, and `empty`/`one`/`all`;
the editor only executes the returned graph and resubmits observations.

- Brush provider: borrowed shell variables, aliases, functions, builtins,
  options, cwd, PATH, and programmable completion state.
- Sarun provider: reusable state/socket provider extracted from the UI-specific
  `ContextProvider for App` implementation.
- Kati, filesystem, and later providers: register domains through the same
  composite without editor branches.

Every snapshot has a revision. Cached analysis is invalidated by returned
dependency keys, not by a global "context changed" flag.

### Editor frontend and persistence

Keep `edtui` and the bounded viewport highlight injection. Separate:

- analysis source, cache, completion/hint/diagnostic presentation;
- buffer persistence (`load`/`save`) for UI host files, UI box RPC, and the
  Brush builtin's logical filesystem;
- terminal host: existing UI pane or standalone foreground Ratatui loop.

Relation spans are UTF-8 byte offsets; `edtui` positions are row/character
columns. Conversion is explicit, checked, and shared by highlights,
replacement spans, cursor tears, and diagnostics.

## Work sequence

### 1. Production document-analysis API

- [x] Promote the `sarun_brush` grammar handle from a test helper to a
      production document-analysis client.
- [x] Add owned Rust request/result types for exact and assist analysis; hide
      the grammar handle, source terms, scoped-state terms, observation terms,
      and wanted projections behind this adapter.
- [x] Decode highlights, completions, status, queries, dependencies, and delta
      from one transformation reply while preserving ambiguous candidates.
- [ ] Extend that same result with the syntax AST and final state; keep domain
      and semantic values typed without exposing Prolog term construction to
      consumers.
- [ ] Add generic hint, indentation, and precise invalid/incomplete diagnostic
      projections to the grammar engine; do not synthesize them in the editor.
- [ ] Add exact/assist, UTF-8, local-state, successful observation, failed
      unique observation, and output-bound tests through embedded static SWI.

### 2. Relation-owned shell editing analysis

- [x] Add a required analysis provider to `EditorPane`; cursor positions are
      converted into relation edit tears. Revision-tagged asynchronous caching
      remains below.
- [x] Replace syntastica for `.sh`/`.bash` with `sarun_brush` results in one
      cut. Never run both and pick a winner.
- [x] Convert byte spans to `edtui` coordinates and completion edits back to
      bytes without splitting UTF-8.
- [x] Render relation-derived completions in an edtui popup and apply their
      exact replacement spans. The defining backward `find -type` completion
      now passes through the real editor buffer on static aarch64.
- [x] Feed finite builtin-argument grammar data from the builtin's actual Clap
      definition into the same analysis request. `bind -m |` completes and
      inserts canonical keymap values without editor, engine, or builtin-name
      branches; the editor remains a consumer of ordinary completion evidence.
- [x] Relate an identifier tear in a state `use` step to names visible at that
      exact lexical point. Local names unify immediately; the same step emits
      an explicit `all(Domain, prefix(Text))` provider query and can union its
      observations without a consumer-side identifier scanner.
- [ ] Render hints, diagnostics, and incomplete status in the existing editor
      chrome/widget once those generic projections exist; do not reinterpret
      shell grammar in the editor.
- [x] Debounce and generation-tag whole-buffer highlight analysis; discard
      stale revision/hash results. Relation work runs off the render thread and
      reaches the existing dedicated Prolog worker. Explicit Tab completion is
      still a foreground request; provider observation rounds will need the
      same asynchronous scheduler.
- [x] Keep tree-sitter only for non-shell languages until each has a relation
      grammar/import path. Structural-locator uses are independently scoped.

### 3. Reusable standalone editor host

- [x] Extract a small Ratatui event loop around the same `EditorPane` used by
      `ui.rs`; do not duplicate editor behavior.
- [ ] Introduce persistence interfaces for UI host files, UI box RPC, and direct
      logical-filesystem builtin access. The first standalone host-file
      load/create/save path is executable; the interface extraction remains.
- [x] Add an RAII terminal guard which restores raw mode, cursor, mouse state,
      and alternate screen on ordinary errors and unwinding.
- [x] Use the controlling terminal rather than redirected builtin stdout.
      Refuse non-TTY, background, and pipeline-stage invocation visibly.
- [x] Give the standalone terminal a frameless presentation and render only on
      input, resize, or outstanding analysis ticks. Once analysis is quiescent
      it emits no periodic terminal writes that destroy native text selection.

### 4. Foreground Brush builtin

- [x] Register one canonical `edit` builtin in `box_builtins()` so standalone
      Brush, box Brush, nested shells, and recipe shells share the same path.
- [x] Resolve relative operands from `context.shell.working_dir()`, never the
      engine process cwd.
- [ ] Snapshot Brush semantic context directly from the borrowed shell before
      analysis. The shell is paused while its foreground editor runs.
- [x] Save through the builtin's direct logical filesystem and return a
      meaningful shell status. Redirection does not steal the TUI terminal;
      non-interactive, background, and pipeline uses fail before terminal setup.

### 5. Provider composition

- [ ] Implement domain routing and revisioned snapshot composition.
- [ ] Supply Brush variables/aliases/functions/builtins/options/cwd/PATH first.
- [ ] Extract Sarun context provision from `App`, then add Kati and filesystem
      domains without changing editor code.
- [ ] Cache only against source/cursor/local state plus the dependency outcomes
      actually returned by the relation.

### 6. Consumer convergence

- [x] Cut the main UI's Bash editor path to the same analysis client and remove
      syntastica Bash selection.
- [ ] Cut Brush-interactive highlighting, validation, hints, and indentation to
      the same provider, then delete the adjacent algorithms. Completion has
      crossed this boundary already: Reedline requires and directly presents
      `sarun_brush` completion edits, with no old-completer fallback.
- [ ] Reuse the editor analysis for Kati recipes, heredocs, and embedded shell
      regions through grammar composition rather than language-name branches.

## Acceptance gate

The defining bidirectional completion case is:

```sh
A=""; find . -type $A
```

With the cursor tear inside `A="|"`, ordinary whole-document parsing must use
the later `find -type $A` occurrence to constrain the earlier assignment and
offer the valid `find -type` values (for example `f`, `d`, and `l`). This is not
an editor heuristic, a forward-only shell completion pass, or a `find` branch
in Rust: it is a completion projection from the same relation that parses the
document and relates the assignment, expansion, command signature, and tear.

The engine-side production API, shared `EditorPane`, and foreground Brush
`edit` builtin now pass this exact case. The checked-in native aarch64 PTY test
starts `sarun brush`, launches `edit`, opens the relation completion popup,
selects `f`, saves exactly `A="f"; find . -type $A`, exits the editor, proves the
alternate screen was restored by returning to the Brush prompt, and exits the
shell successfully. Unit tests separately pin the byte replacement span,
absence of external queries, UTF-8 coordinate conversion, stale-result
rejection, and the full finite completion domain.

The second native acceptance case is the ordinary forward local-name use:

```sh
#!/bin/bash
A=""
find . -type $
```

With the tear after `$`, the parser's `simple_parameter` node emits the same
state `use` relation as a complete `$A`. That relation sees the earlier local
definition and offers `A`; it also records an `all shell_variable prefix("")`
context query for provider-backed names. The aarch64 PTY test accepts `A`,
saves the exact three-line `$A` result, verifies that the standalone host did
not render the UI pane's full-screen frame, and returns to Brush cleanly.

The remaining acceptance work is the broader context/provider and shell
grammar matrix below, not another editor parsing implementation.

Native aarch64 PTY tests must start standalone `sarun brush`, invoke `edit` as
a real foreground builtin, edit and save a UTF-8 shell file, and prove:

- highlighting is returned by the ordinary `sarun_brush` parse;
- assignment state affects later variable highlighting/hints/completion;
- a tear completion has the correct byte replacement span and reparses;
- Brush and at least one external Sarun/Kati/filesystem provider contribute
  candidates through explicit queries and dependency keys;
- invalid/incomplete text, heredoc/embedded regions, and cursor movement update
  the same analysis rather than a side tokenizer;
- save/exit and every error path restore the terminal;
- pipeline, background, redirected, and non-TTY uses refuse cleanly;
- the UI pane and builtin render the same analysis results;
- no shell tree-sitter fallback or legacy Brush-interactive parser is reachable.

Existing widget and UI tests remain useful but do not satisfy this gate. Add
focused unit tests for byte/character conversion and stale-analysis rejection,
then PTY integration tests for the user-visible behavior above.
