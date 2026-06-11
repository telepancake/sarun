# oaita — design so far

CLI client for OpenAI-compatible chat APIs. Uses existing providers; does **not**
run its own inference server (so it can't touch the chat template or decoding,
and must use native function-calling). Tailored to run on/with `sarun`.

This is a working note, not a spec. Edit freely.

---

## Status

**Built (on the branch, tested):**
- `oaita_fakeserver` / `oaita_fakeclient` / `test_oaita_fakeapi.py` — canned-response
  test rig (zero-dep server, openai-SDK client).
- `oaita` core: turn files, `gen`, turn-id injection + header absorb, name
  stitching, one-step tool-call loop, `act` recursive sub-agents (leaf is a plain
  `gen` for now — no real tools).
- `test_oaita.py` (33 tests).

**Designed, not built:**
- `shell` tool + sarun integration (executor interface).
- `inspect` / `read` / `replace` / `insert` / `apply` / `reject`.
- declarative tool registry (name → command → run-location).
- self-extending parser (LLM writes a span-tree decoder).
- merge-as-review.

**Deferred ("next season"):** real MCP, auto retry-until-success, harness context
compaction.

---

## Core model

- A session is a folder: `$XDG_STATE_HOME/oaita/<name>/`.
- One file = one turn. File content is the raw turn text — no JSON, no headers.
- Filename `NNNN[-slug].<type>`:
  - `NNNN` zero-padded, step 10 (BASIC line numbers); alphabetical sort == order.
  - `slug` optional; it **is** the turn-id.
  - `<type>` = extension = OpenAI role (`system`/`user`/`assistant`/`tool`/…).
- No DB, no hidden state. Context is rebuilt from the files every step.
- Files that don't match the grammar are ignored.

## Turn-ids

- The slug is the turn-id. At send time each message's content is prefixed with
  `{"turn-id":"<id>"}\n`. Files stay raw; header is synthesized from the filename.
- Generated turns get a fresh unique id (5 lowercase letters). Hand-authored slugs
  are kept verbatim.
- If the model echoes a turn-id header atop its reply, strip it (keep the file
  raw); on an appended turn, adopt a valid+unique id the model chose.
- Purpose: let the model reference / edit its own context (delete stale, fix
  wrong) — via the structural-edit tools pointed at the session folder. Experiment.

## Name stitching

- Names are `[A-Za-z0-9]+` only, so `.` is an unambiguous separator.
- `a.b.c` = prepend a then b in front of c; infer and write in **c** (the last
  segment). Composition, not hierarchy — order can differ next round (`c.d.a`).
- Use: skills, system prompts, sub-conversations.
- All referenced sessions are loaded + slug-assigned; turn-ids unique across the
  whole stitched context; only the last segment is written.

## gen = one step

- One model generation per `oaita gen`.
- No tool call → the streamed reply is the answer (one turn).
- Tool call(s) → evaluate each, persist results, then **stop**. No auto-continue.
  The caller drives the loop by running `gen` again (the result is in the files).
- Streamed to disk per-delta (interrupt leaves resumable partial text).

---

## Tools — native function-calling (forced)

- Run-to-reply (parse shell out of prose) does **not** work: benchmaxxed models
  emit tool-call tokens, the provider template lifts them into `tool_calls`, and
  we don't own the template/decoder. So: tools via the API `tools` param.
- v1 set: `shell`, `act`, `inspect`/`read`/`replace`(+`insert`), `apply`/`reject`.

### Tool registry

Each tool is a row, two-faced:
- **outward:** the function schema the provider needs (derive from oaita's own
  argparse subcommand → one source of truth).
- **inward:** how a received `tool_call` expands to a command (params → argv;
  the one big-content param → **stdin**, so `replace` never fights shell quoting)
  + a **run-location** meta.

Run-location:
- **in-box** (default): runs against the current sarun box; output+changes
  captured. → `shell`, `inspect`, `read`, `replace`, `act` (act = `oaita <ctx>`
  with a nest+depth flag).
- **out-of-box** (manager level): operates on the box itself → `apply` / `reject`
  / `patch` as `sarun …` against a box id.

**Invariant:** arbitrary script only ever runs in-box. `shell` is arbitrary →
in-box. out-of-box rows are fixed, parameterized templates, no free-form
passthrough. Never *arbitrary × out-of-box*.

### act (sub-agents)

- `act` runs `oaita` on a stitched sub-context inside a nested sarun box. Returns
  (answer, change-summary) — same shape as `shell`.
- Each call is a persistent, addressable sub-context; `follow_up` continues it.
- Sub-agents are folders of turn files → fully inspectable (this is the answer to
  pi's "black-box sub-agents can't be debugged" objection).
- Depth-capped ("too deep") to stop infinite delegation; enforced by oaita seeing
  its own nesting (depth counter in env, or sarun box depth). Flavour text flatter
  / less encouraging than top-level `act`.

---

## sarun (sandbox substrate)

`shell`/`act` run in sarun boxes. sarun = unprivileged bwrap+FUSE:
- copy-on-write filesystem overlay,
- per-process stdout/stderr capture with attribution,
- filesystem change tracking,
- network intercept/approval,
- nesting (box in box),
- apply or discard a box's accumulated changes.

### Box lifecycle

- A box N persists after its call returns (so it can be a `follow_up` target).
- Subboxes spawned during the call are discarded on return.

### Change model

- `shell`/`act` run in a box. Result is one of:
  - **output only** → the output **is** the tool result (big output read via the
    same `inspect` mechanism). Subbox discarded; no follow-up needed.
  - **file changes** → the model resolves them with `apply` (merge up) or `reject`
    (discard). That choice drives the leaf loop.
- `replace` (structural edit) changes files **directly** — no box, no apply/reject.

### sarun: edge vs cost

- Edge: CoW overlay, per-process capture, apply/discard-as-a-unit, nesting,
  network intercept. pi.dev has none of this in core; its ecosystem (Gondolin
  micro-VM, oh-my-pi worktrees) retrofits weaker versions.
- Cost: Linux-only, bwrap/FUSE fragility, ~25s first-run build, single-host,
  separate system to maintain.
- Conscious fork: Linux-native moat vs portability. Fine while target is local
  Linux; a hard blocker the day macOS/Windows shows up.

---

## Structural editing — `inspect` / `read` / `replace`

- Scope: **regular files you edit** (working tree). Virtual/dynamic things
  (`/proc`, `/sys`, logs, command output) go through `shell` (capture, not edit).
- Pieces = tree-sitter nodes as **byte spans**. Address by structural **path**
  (`foo.py › class A › bar`).
- `inspect` = structure only (paths/kinds/spans). Huge files fall back to a
  **summary** ("15086 functions") — a degenerate-case fuse, not navigation; to
  dig, use `shell` (jq/sed). No drill-down tool.
- `read(path)` = the node's bytes. `replace(path, new)` = byte splice.
  `insert before|after`. `delete` = replace with `""`.

### Lens via tree-sitter

- `get` = parse + index spans. `put` = splice.
- Round-trip is free: unchanged bytes are never re-serialized (the 99% you didn't
  touch is the original `src[…]`). That's why it's a lens for free — vs Augeas,
  whose hard part is reconstructing formatting on `put`. Keep the view **concrete**
  (spans); abstracting it (normalize/reorder) reintroduces the real lens work.
- Residue of real work: trivia/separator discipline in the gaps between nodes
  (does `delete` eat the trailing `,`/newline; where does `insert` land vs
  indentation). Same thing an Augeas lens encodes, much smaller.

### No model-carried hash

- Earlier idea (content-hash anchor the model echoes back): **dropped.**
- The harness already has what `read` returned (it's in context). Any staleness
  check is harness-side bookkeeping over its own read-log. Making the model repeat
  a SHA1 is the password anti-pattern — hurts perf, noise tokens, verbatim
  reproduction is exactly what models fail at.
- Cheap locate comes from path addressing, not hashes.
- cf. JSON Patch `test` op: a precondition checked by the applier, not carried by
  the author. Same idea, harness-side.

### Self-extending parser

- Supported formats → builtin decode.
- Unsupported → an LLM loop writes a detector + a `bytes → labeled span-tree`
  decoder; harness registers it and reuses it (pay LLM once, deterministic after).
  Learned decoders are a growing, shareable code library.
- Acceptance gate: spans in-bounds, non-overlapping per level, leaves (gaps
  included) concatenate back to input (`concat(spans) == input`). That's the lens
  minus the serializer — tractable for an LLM and checkable by the harness.
- Prefer wiring an existing parser (tree-sitter grammar, format lib) over novel.
- Runs in a sarun box (it's LLM-authored code over arbitrary data).
- Works for text/structured-text (splice). **Binary** (length-prefixed, offset
  tables, compressed) needs a real encoder, not splicing — out of the free regime.

---

## Merge (parallel agents)

- **No mechanical auto-merge.** "No conflict" ≠ "correct" (semantic conflicts;
  PR review exists even with zero file-level conflicts). Evidence agrees (Brun et
  al. on semantic conflicts; structured-merge studies improve detection, not
  correctness).
- Merge = the **same caller-driven, throwaway-box, apply/reject loop as a single
  call**, with N change-sets as input. A reconciling agent (or a human) reviews
  all results, applies the good pieces, discards the rest, iterates. Literally
  just another oaita context whose subject is the boxes.
- Structural/AST diff (difftastic / mergiraf / spork / GumTree lineage) is a
  **review aid** — "here's what each agent changed, node by node, where they
  overlap" — never an auto-combiner.

---

## Deferred ("next season")

- **Real MCP** tool servers at the leaf.
- **Auto retry-until-success** (the loop where a leaf retries a command until it
  works).
- **Context compaction** — collapse failed attempts to the successful run.
  - If "prefill" means *the harness rebuilds the compacted context and sends a
    leaner messages array* → fully portable; our file model makes it easy (edit
    the folder / send less). Use this.
  - If it means the provider *assistant-prefill* feature → that's being withdrawn
    (Anthropic 4.6). Don't depend on it.

---

## Prior art to crib

- **Augeas + lenses** (bidirectional, lossless round-trip) — the model to steal
  for `inspect`/`replace`. "LLM writes a decoder" = "writes a lens."
- **JSON Pointer (6901) + JSON Patch (6902)** — path syntax + op set; `test` op =
  harness-side precondition (not a model hash).
- **Plan 9 / 9P, procfs/sysfs** — navigable tree as a universal interface (sarun
  is FUSE; the structural view could literally be a mount). Lineage, not a plan.
- **LSP `documentSymbol` + tree-sitter** — don't reinvent code structure.
- **XPath / jq / CSS selectors** — path conventions.
- **pi.dev** (Mario Zechner, ~61k★, MIT): same minimal-core + external-provider +
  function-calling shape. Loot: LLM compaction with a cumulative file-op trail;
  branch summarization on navigation; tool-result split (LLM-facing vs UI-facing
  payload); overflow→bigger-model→compact cascade; progressive disclosure of
  capability docs (relevant when MCP lands).

---

## Open questions

- gen is a one-step primitive; an autonomous "agent" needs an external driver
  loop (re-run gen until done). Deferred, but it's table stakes for real use.
  pi's RPC (JSONL events over stdio) is a clean driver pattern to copy.
- Confirm the compaction "prefill" sense (harness rebuild vs provider feature).
- Cross-conversation inner-id collision (sub-agent sessions are global by id).
- Tool-result split (LLM-facing vs UI-facing) — adopt early, cheap.
