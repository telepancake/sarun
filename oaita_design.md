# oaita — design so far

CLI client for OpenAI-compatible chat APIs, depends on `sarun`.

**Built (on the branch, tested):**
- `oaita_fakeserver` / `oaita_fakeclient` / `test_oaita_fakeapi.py` — canned-response
  test rig (zero-dep server, openai-SDK client). Expect-mode: `srv.expect(matcher,
  response)` rules answer by request MATCH (string = substring of last message, or
  callable), before the queue; responses may be callables (templating).
- `oaita` core: turn files (grammar below incl. flags + from), gen/call/run split,
  `add` (stdin → turn file), turn-id injection + header absorb, name stitching,
  `act` sub-agents with from-routing.
- Deterministic ids: `$OAITA_ID_SEED` makes each generated id a hash of its
  (session, NNNN) slot — same folder + seed = identical filenames. Tests pin
  exact trees; a replay scenario byte-compares two from-scratch runs.
- Send-time adapter: answered tool calls replay in NATIVE provider form
  (assistant.tool_calls + matching tool_call_id, content = id-header only);
  files keep the envelope. Kills the envelope-as-prose imitation wart.
- Tool registry (two-faced rows): `act`, `shell`, `inspect`, `delete`.
  - `shell` runs in the session's ONE persistent sarun box (OAITA-<SESSION>)
    behind an executor interface — SarunExecutor (sarun BOX -- sh -c …; changes
    via sarun BOX patch → per-file +/- summary), FakeExecutor in tests,
    executor-less → error result. CLI: --sarun / --no-sandbox.
  - `inspect` lists structure at a path (harness-native; directory entries,
    summary fuse past a cap). `delete` rolls back/collapses a context to one
    annotation turn, or drops a spent sub-agent session (+ discards its box).
- Sub-agents are now TOOL-CAPABLE: an `act` leaf is run-to-completion one depth
  deeper (it can shell/inspect/delete/delegate), capped at MAX_DEPTH (`act`
  returns "too deep" past the cap).
- Narration kept: a reply with prose AND tool calls now keeps the prose as its
  own clean assistant turn before the c-turns (running tallies survive instead
  of being overwritten by the envelope).
- `test_oaita.py` (42), `test_oaita_fakeapi.py` (9), `test_oaita_scenarios.py`
  (8 scripted scenarios incl. CLI-subprocess + fix-the-build shell arc), and
  `test_oaita_e2e.py` — the shell tool against a REAL bwrap+FUSE box (real
  headless UI, staged write, host untouched). E2E PASS.

## Core model

- A session is a folder: `$XDG_STATE_HOME/oaita/<name>/`.
- <name> is `[A-Za-z0-9]+`
- One file = one turn. File content is the raw turn text. Turns are ordered alphabetically by file name.
- Filename `NNNN[-turnid[-from]].<flags>.<type>`:
  - `NNNN` zero-padded, automatic step 10 numbers so insert is easy;
  - `turnid` turn-id, `[a-z0-9]+` (no dash — dash separates fields). Auto-assigned
    if missing. Unique across all sessions.
  - `from` optional sender: the session name that wrote this turn into the folder.
    Absent for turns from the session itself (own assistant output, operator input);
    present exactly when call/run machinery posts a turn cross-context. `from`
    implies a turnid (harness assigns one when posting).
  - `<flags>` is additional meta about the turn.
    - `p` for partial (needs to be extended. For example, no EOS yet). gen streams
      into a p-flagged target and drops the flag on clean completion, so an
      interrupt leaves a resumable partial; a CLEAN assistant tail is never
      rewritten (gen after it appends).
    - `i` for "no turn id header". suppresses turn id prefix processing described in next section.
    - `c` for "tool call": an assistant turn holding a `{"tool", "arguments"}`
      envelope, written by gen, evaluated by call.
  - `<type>` extension is the role (`system`/`user`/`assistant`/`tool`/…).
- Context is rebuilt from the files every step, without any additional hidden state.

## Turn attachments

- A turn may have a parallel dir `NNNN-turnid/` next to its file: files the user
  attached, items unpacked by `inspect` (e.g. a member pulled out of an archive —
  no virtual filesystem in the harness), full outputs of tool runs.
- Turn text carries the LLM-facing snippet; the attachment dir carries the full
  payload (also what a UI shows). This IS the tool-result split.
- Addressed through the turn-id: `inspect <turnid>` lists turn content +
  attachments; `read <turnid>/build.log`. Ids are globally unique, so no session
  prefix needed even across stitched contexts.
- Lifecycle is the turn's: slug assignment renames file and dir together;
  regenerate-in-place wipes the dir; deleting the turn deletes the dir.
- Attachments are NOT sent in context — the model pulls them via inspect/read.

## Turn-ids

- Purpose: let the model reference / edit its own context, and see who said what
- At send time each message's content is prefixed with
  `{"turn-id":"<id>"}\n` — plus `"from":"<name>"` when the filename carries a
  sender. Files stay raw; header is synthesized from the filename.
- Generated turns get a fresh unique id, 5 lowercase letters currently. Collisions
  are handled by probing against the set of known ids, so the short length is a
  readability choice, not a correctness limit.
- If the model generates a turn-id header atop its reply (by following example it sees), strip it
- use the generated turn-id if it is unique and valid. otherwise, make new and replace generated turn-id in this turn's text with new one.
- a model-emitted `from` is always stripped, never adopted: sender identity is
  harness-assigned provenance, not a label the model may choose.
- `from` lets the model target senders in its commands — e.g. follow-up to the
  context that posted a result, or address the context that started this subagent.

## Name stitching

- Use: skills, system prompts, sub-conversations.
- name `a.b.c` = prepend a then b in front of c; infer and write in **c** (the last
  segment). Composition, not hierarchy — order can differ each round (`c.d.a`).

## gen subcommand

- One model generation per `oaita gen`.
- the streamed reply is the answer (one turn).
- tool call(s) in the reply are PERSISTED as c-flagged turns, never evaluated;
  gen refuses to run while a call is unanswered (the prompt would mislead).

## call subcommand

- evaluate tool call from the last turn. ONE call per invocation; in the
  trailing call/result block, pairing is positional (k-th result ↔ k-th call).
- if tool call has produced reply, write it in a follow-up turn. The result
  turn carries from=<inner session>; the inner seed carries from=<outer> —
  both ends know whom to address.
- reply can be deferred, such as when subagent is invoked in a "blocking" manner

## run subcommand

- this is "run to completion" loop.
- evaluate contexts repeatedly until they produce assistant turn
- tool calls are processed using call.
- if assistant turn causes tool reply to another context, it is written there.

## add subcommand

- stdin → one turn file. Defaults: next grid number, generated turn-id, type
  user, session $OAITA_SESSION/'default'; all overridable (--type --id --from
  --flags --number). Unbuffered copy; written path on stdout.

## Tools

- run process in sarun sandbox. reports either completion (if short lived), or informs that process is running in background. background processes can be inspected, but completion automatically posts completion turn into the session that ran the process.
- inspect stuff. given path, shows structure of thing at that path (list of things in the thing). when applied repeatedly lets one build path that reaches into depths of stuff. thing can be directory, but also source or text file (tree-sitter), turn-id (objects referred by that turn), sarun instance, another context, sqlite database, and so on. if inspect is about to produce exceedingly long output, harness automatically makes sure it is summarized more and more, until it fits into reasonable number of entries. model can expand any of them using later inspect calls.
- read thing referred by a path obtained from inspect.
- replace contents of thing refered to by a path obtained from inspect. harness makes sure current content of the thing matches previous content that was returned by read in this session, and reports confilct error unless forced by the tool call to ignore. fake "/before" and "/after" paths can be used to insert element into sequeces before or after another element
- apply or discard changes made by process. sarun sandbox stays around after process completion and can be used to run follow up process, or inspected to look at changes it has made to files. these changes can be either applied (whole, per file, per hunk), or discarded.
- delete process. if running, terminates (including nested processes). optionally rollback up to provided turn-id and replace or augment contents of that turn with provided text. this allow long string of attempts to be collapsed by model into single clean invocation with an annotation.

## warts found while scripting scenarios

- ~~context replay is a narrative, not native tool format~~ — FIXED: send-time
  adapter (build_messages). Unanswered mid-history calls still replay as
  narrative (a dangling tool_calls would be rejected); only an edge via
  stitched prepends.
- **big tool params will bloat the envelope.** Fine for act's one-line requests;
  `replace(path, newcontent)` as JSON-in-a-turn is awful. Resolution: big params
  go in the call turn's ATTACHMENT dir, envelope references them. (Attachments
  pull double duty.)
- **positional pairing is fragile under hand-editing** — delete one result file
  and later pairings in the block shift. Accepted cost of files-as-database;
  recoverable pairing (explicit per-result reference) only if it bites.
- **rerolling a bad call = delete the c-turn file by hand.** gen's refusal is
  right, but a "discard call" gesture belongs with the delete tool.
- ~~leaf is tools=[]~~ — FIXED: sub-agents are tool-capable, run-to-completion
  one depth deeper, capped at MAX_DEPTH (act → "too deep").
- **spent sub-agent sessions leak.** Each `act` mints a persistent session
  folder (re-addressable for follow-ups), and nobody garbage-collects them — a
  fan-out over N files leaves N orphan sessions. `delete(session=…)` exists but
  the caller must remember to use it (scenario 9 deliberately doesn't, to show
  the leak). Open: lifecycle policy — auto-drop on parent settle? a sweep
  command? mark-and-keep only those a follow-up referenced?
- **run interleaves all streams on one stdout** (outer + every leaf). Cosmetic;
  the files are the record.
- **keep nondeterminism out of file contents** — the byte-replay scenario only
  holds because no timestamps/randomness ever land in a turn file. Invariant
  worth keeping on purpose.

## what is sarun (already made and working)

- processes run in sarun boxes, that are unprivileged bwrap+FUSE:
- copy-on-write filesystem overlay,
- per-process stdout/stderr capture with attribution,
- filesystem change tracking,
- nesting (box in box),
- apply or discard a box's accumulated changes.
- boxes run in the HOST network namespace (network gating moved to a separate
  `sakar` tool), so an in-box oaita reaches a localhost LLM endpoint directly.
