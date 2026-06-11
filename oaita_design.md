# oaita — design so far

CLI client for OpenAI-compatible chat APIs, depends on `sarun`.

**Built (on the branch, tested):**
- `oaita_fakeserver` / `oaita_fakeclient` / `test_oaita_fakeapi.py` — canned-response
  test rig (zero-dep server, openai-SDK client).

## Core model

- A session is a folder: `$XDG_STATE_HOME/oaita/<name>/`.
- <name> is `[A-Za-z0-9]+`
- One file = one turn. File content is the raw turn text. Turns are ordered alphabetically by file name.
- Filename `NNNN[-turnid].<flags>.<type>`:
  - `NNNN` zero-padded, automatic step 10 numbers so insert is easy;
  - `turnid` alphanumerical turn-id. Auto-assigned if missing. Unique across all sessions.
  - `<flags>` is additional meta about the turn.
    - `p` for partial (needs to be extended. For example, no EOS yet)
    - `i` for "no turn id header". suppresses turn id prefix processing described in next section.
  - `<type>` extension is the role (`system`/`user`/`assistant`/`tool`/…).
- Context is rebuilt from the files every step, without any additional hidden state.

## Turn-ids

- Purpose: let the model reference / edit its own context
- At send time each message's content is prefixed with
  `{"turn-id":"<id>"}\n`. Files stay raw; header is synthesized from the filename.
- Generated turns get a fresh unique id, 5 lowercase letters currently.
- If the model generates a turn-id header atop its reply (by following example it sees), strip it
- use the generated turn-id if it is unique and valid. otherwise, make new and replace generated turn-id in this turn's text with new one.

## Name stitching

- Use: skills, system prompts, sub-conversations.
- name `a.b.c` = prepend a then b in front of c; infer and write in **c** (the last
  segment). Composition, not hierarchy — order can differ each round (`c.d.a`).

## gen subcommand

- One model generation per `oaita gen`.
- the streamed reply is the answer (one turn).

## call subcommand

- evaluate tool call from the last turn.
- if tool call has produced reply, write it in a follow-up turn.
- reply can be deferred, such as when subagent is invoked in a "blocking" manner

## run subcommand

- this is "run to completion" loop.
- evaluate contexts repeatedly until they produce assistant turn
- tool calls are processed using call.
- if assistant turn causes tool reply to another context, it is written there.

## Tools

- run process in sarun sandbox. reports either completion (if short lived), or informs that process is running in background. background processes can be inspected, but completion automatically posts completion turn into the session that ran the process.
- inspect stuff. given path, shows structure of thing at that path (list of things in the thing). when applied repeatedly lets one build path that reaches into depths of stuff. thing can be directory, but also source or text file (tree-sitter), turn-id (objects referred by that turn), sarun instance, another context, sqlite database, and so on. if inspect is about to produce exceedingly long output, harness automatically makes sure it is summarized more and more, until it fits into reasonable number of entries. model can expand any of them using later inspect calls.
- read thing referred by a path obtained from inspect.
- replace contents of thing refered to by a path obtained from inspect. harness makes sure current content of the thing matches previous content that was returned by read in this session, and reports confilct error unless forced by the tool call to ignore. fake "/before" and "/after" paths can be used to insert element into sequeces before or after another element
- apply or discard changes made by process. sarun sandbox stays around after process completion and can be used to run follow up process, or inspected to look at changes it has made to files. these changes can be either applied (whole, per file, per hunk), or discarded.
- delete process. if running, terminates (including nested processes). optionally rollback up to provided turn-id and replace or augment contents of that turn with provided text. this allow long string of attempts to be collapsed by model into single clean invocation with an annotation.

## what is sarun (already made and working)

- processes run in sarun boxes, that are unprivileged bwrap+FUSE:
- copy-on-write filesystem overlay,
- per-process stdout/stderr capture with attribution,
- filesystem change tracking,
- nesting (box in box),
- apply or discard a box's accumulated changes.
