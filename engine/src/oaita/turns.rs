// Turn model — the on-disk grammar that IS the conversation state.
//
//   NNNN[-turnid[-from]].<flags>.<type>
//
//   NNNN    zero-padded grid (width 4, step 10) — alphabetical sort = order
//   turnid  [a-z0-9]+ stable id the model can reference in its replies
//   from    [A-Za-z0-9]+ session that posted this turn (cross-context writes
//           only; absent for own turns)
//   flags   subset of {p, i, c, b}:
//             p — partial / resumable (interrupted stream)
//             i — no turn-id header (suppress injected meta line)
//             c — tool call (assistant turn holding a {"tool","arguments"} envelope)
//             b — backtrack waypoint (NOT a finished answer; run continues past it)
//   type    one of system/developer/user/assistant/tool — the OpenAI role.
//
// One file = one turn; the file content is the RAW turn text. The header line
// `{"turn-id":"<id>"[,"from":"<sender>"]}\n` is INJECTED at send time from the
// filename and stripped from generated replies — files on disk stay raw, no
// hidden state, no JSON wrapper.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use regex::Regex;
use serde_json::json;
use std::sync::OnceLock;

use crate::oaita::ids;

pub const TURN_TYPES: &[&str] = &["system", "developer", "user", "assistant", "tool"];
pub const FLAG_CHARS: &str = "picb";
pub const NUM_WIDTH: usize = 4;
pub const NUM_STEP: u32 = 10;

#[derive(Debug, Clone)]
pub struct Turn {
    pub number: u32,
    pub slug: Option<String>,
    pub sender: Option<String>,
    pub flags: String,
    pub kind: String, // role: system|developer|user|assistant|tool
    pub path: PathBuf,
}

impl Turn {
    pub fn role(&self) -> &str {
        &self.kind
    }

    pub fn read(&self) -> io::Result<String> {
        fs::read_to_string(&self.path)
    }

    /// The OpenAI chat message — `{role, content}`. When `inject_id` is true
    /// and the turn has a slug and lacks the `i` flag, the content is prefixed
    /// with the synthesised `{"turn-id":"<id>"[,"from":"<sender>"]}\n` header
    /// so the model can reference turns by their id.
    pub fn message(&self, inject_id: bool) -> serde_json::Value {
        let raw = self.read().unwrap_or_default();
        let content = if inject_id && self.slug.is_some() && !self.flags.contains('i') {
            let mut hdr = serde_json::Map::new();
            hdr.insert("turn-id".into(), json!(self.slug.as_deref().unwrap()));
            if let Some(s) = &self.sender {
                hdr.insert("from".into(), json!(s));
            }
            let line = serde_json::Value::Object(hdr).to_string();
            format!("{line}\n{raw}")
        } else {
            raw
        };
        // Roles other than the OpenAI primitives map cleanly; developer is
        // OpenAI's newer system-equivalent — we keep it as-is.
        json!({"role": self.kind, "content": content})
    }
}

fn turn_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        let types = TURN_TYPES.join("|");
        let pat = format!(
            r"^(?P<num>\d+)(?:-(?P<turnid>[a-z0-9]+)(?:-(?P<sender>[A-Za-z0-9]+))?)?(?:\.(?P<flags>[{FLAG_CHARS}]+))?\.(?P<kind>{types})$",
        );
        Regex::new(&pat).expect("turn regex")
    })
}

pub fn parse_turn(path: &Path) -> Option<Turn> {
    let name = path.file_name()?.to_str()?;
    let caps = turn_regex().captures(name)?;
    let number = caps.name("num")?.as_str().parse().ok()?;
    Some(Turn {
        number,
        slug: caps.name("turnid").map(|m| m.as_str().to_string()),
        sender: caps.name("sender").map(|m| m.as_str().to_string()),
        flags: caps
            .name("flags")
            .map(|m| m.as_str().to_string())
            .unwrap_or_default(),
        kind: caps.name("kind").unwrap().as_str().to_string(),
        path: path.to_path_buf(),
    })
}

pub fn session_dir(name: &str) -> PathBuf {
    crate::paths::oaita_state_home().join(name)
}

pub fn load_turns(name: &str) -> Vec<Turn> {
    let folder = session_dir(name);
    let Ok(rd) = fs::read_dir(&folder) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if !p.is_file() {
                return None;
            }
            parse_turn(&p)
        })
        .collect();
    // Files are sorted ALPHABETICALLY by filename; zero-padding makes that
    // identical to numeric order, with stable tiebreak on full name so two
    // turns with the same number sort deterministically.
    entries.sort_by(|a, b| {
        let an = a.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let bn = b.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        an.cmp(bn)
    });
    entries
}

pub fn next_number(turns: &[Turn]) -> u32 {
    turns
        .iter()
        .map(|t| t.number)
        .max()
        .map(|m| m + NUM_STEP)
        .unwrap_or(NUM_STEP)
}

pub fn turn_filename(
    number: u32,
    kind: &str,
    slug: Option<&str>,
    sender: Option<&str>,
    flags: &str,
) -> String {
    if sender.is_some() && slug.is_none() {
        panic!("turn_filename: sender requires slug");
    }
    let mut stem = format!("{:0width$}", number, width = NUM_WIDTH);
    if let Some(s) = slug {
        stem.push('-');
        stem.push_str(s);
    }
    if let Some(f) = sender {
        stem.push('-');
        stem.push_str(f);
    }
    if !flags.is_empty() {
        stem.push('.');
        stem.push_str(flags);
    }
    format!("{stem}.{kind}")
}

/// A session name is letters+digits only — the `.` that stitches names in a
/// spec like `a.b.c` must be unambiguous, never part of a name.
pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "invalid session name {name:?} — letters/digits only"
        ));
    }
    Ok(())
}

/// Split a dot-stitched name spec into segments. The LAST segment is the
/// target (writes happen there); earlier segments are prepended as context.
/// Composition, not hierarchy — order may vary turn-to-turn.
pub fn parse_stitch(spec: &str) -> Result<Vec<String>, String> {
    let segs: Vec<&str> = spec.split('.').collect();
    for s in &segs {
        validate_session_name(s)?;
    }
    let mut seen = std::collections::HashSet::new();
    for s in &segs {
        if !seen.insert(*s) {
            return Err(format!("session {s:?} appears more than once in {spec:?}"));
        }
    }
    Ok(segs.into_iter().map(String::from).collect())
}

/// Walk a session's turns and assign slugs to any that lack one, renaming the
/// files on disk in place. Returns the updated turn list.
pub fn assign_slugs(
    turns: Vec<Turn>,
    existing: &mut std::collections::HashSet<String>,
) -> io::Result<Vec<Turn>> {
    let mut out = Vec::with_capacity(turns.len());
    for t in turns {
        if let Some(s) = &t.slug {
            existing.insert(s.clone());
            out.push(t);
        } else {
            let parent = t.path.parent().unwrap();
            let id = ids::new_turn_id(existing);
            existing.insert(id.clone());
            let new_name = turn_filename(t.number, &t.kind, Some(&id), None, &t.flags);
            let new_path = parent.join(new_name);
            fs::rename(&t.path, &new_path)?;
            out.push(Turn {
                slug: Some(id),
                path: new_path,
                ..t
            });
        }
    }
    Ok(out)
}

/// Append a NEW turn file with the given content; returns the path written.
pub fn append_turn(
    name: &str,
    kind: &str,
    content: &str,
    slug: Option<String>,
    sender: Option<String>,
    flags: &str,
    number: Option<u32>,
) -> io::Result<PathBuf> {
    validate_session_name(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let folder = session_dir(name);
    fs::create_dir_all(&folder)?;
    let turns = load_turns(name);
    let mut existing: std::collections::HashSet<String> =
        turns.iter().filter_map(|t| t.slug.clone()).collect();
    let slug = slug.or_else(|| Some(ids::new_turn_id(&existing)));
    if let Some(s) = &slug {
        existing.insert(s.clone());
    }
    let n = number.unwrap_or_else(|| next_number(&turns));
    let name_file = turn_filename(n, kind, slug.as_deref(), sender.as_deref(), flags);
    let path = folder.join(name_file);
    fs::write(&path, content)?;
    Ok(path)
}

/// Read all turns for a STITCHED context — every segment's turns, in order,
/// with slugs assigned and uniqueness checked across the whole context.
pub fn load_stitched(spec: &str) -> Result<Vec<Turn>, String> {
    let segs = parse_stitch(spec)?;
    let mut all = Vec::new();
    let mut slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for seg in &segs {
        // Ensure folder exists for later writes; loading the empty list is fine.
        let _ = fs::create_dir_all(session_dir(seg));
        let ts = load_turns(seg);
        let ts = assign_slugs(ts, &mut slugs).map_err(|e| format!("assign slugs in {seg}: {e}"))?;
        all.extend(ts);
    }
    // Cross-context turn-id uniqueness check.
    let mut seen: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for t in &all {
        if let Some(s) = &t.slug {
            if let Some(prev) = seen.get(s) {
                return Err(format!(
                    "turn-id collision {s:?} — used by both \
                    {prev:?} and {:?}",
                    t.path
                ));
            }
            seen.insert(s.clone(), t.path.clone());
        }
    }
    Ok(all)
}

/// Returns (segs, target_segment) for a stitch spec — the LAST segment is the
/// target (writes go there).
pub fn target_segment(spec: &str) -> Result<String, String> {
    Ok(parse_stitch(spec)?.into_iter().last().unwrap())
}

pub fn cmp_path_name(a: &Path, b: &Path) -> Ordering {
    a.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .cmp(b.file_name().and_then(|s| s.to_str()).unwrap_or(""))
}

// ── strip a model-emitted turn-id header off a generated reply ───────────────
fn emitted_id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(
        r#"(?is)^\s*\{\s*['"]turn[-_ ]?id['"]\s*:\s*['"](?P<id>[^'"]*)['"]\s*(?:,\s*['"]from['"]\s*:\s*['"][^'"]*['"]\s*)?\}[ \t]*(?:\r?\n|$)"#,
    ).expect("emitted id regex"))
}

/// If `content` begins with a `{"turn-id":...}` header (with optional `from`),
/// strip it and return `(Some(id), body)`. Otherwise return `(None, content)`.
pub fn strip_emitted_turn_id(content: &str) -> (Option<String>, String) {
    let Some(m) = emitted_id_re().find(content) else {
        return (None, content.to_string());
    };
    let id = emitted_id_re()
        .captures(content)
        .unwrap()
        .name("id")
        .map(|m| m.as_str().to_string());
    (id, content[m.end()..].to_string())
}

/// Build the OpenAI `messages` array from a list of turns.
///
/// Normal turns go through `Turn::message(true)` (role + injected turn-id
/// header). The two special cases are tool-call exchanges:
///
///   * `c`-flagged assistant turns hold the envelope `{"tool":..., "arguments":...}`
///     on disk. The wire form is `{"role":"assistant", "tool_calls":[…]}` where
///     each call carries `id` (the file slug, which IS the OpenAI call_id),
///     `type:"function"`, and `function.{name,arguments-as-string}`. Consecutive
///     `c` turns COLLAPSE into one assistant message with N tool_calls — that's
///     how the wire originally delivered them (one gen.reply with parallel
///     calls = one assistant message, even though storage splits to one file
///     per call).
///
///   * tool-result turns immediately following a collapsed c-block PAIR
///     positionally to the calls and need `tool_call_id` set — without it,
///     strict providers (xiaomi/mimo, others) 400 with "tool_call_id is not
///     set". ds4-flash tolerates the missing field; mimo doesn't.
///
/// Header injection is suppressed on both forms: the c envelope is the
/// arguments JSON (a header would corrupt it), and the tool result content
/// already carries its own framing.
pub fn build_messages(turns: &[Turn]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < turns.len() {
        let t = &turns[i];
        // Collapse a run of consecutive c-flagged assistant turns into ONE
        // assistant message with N tool_calls, then pair the following tool
        // turns by position.
        if t.kind == "assistant" && t.flags.contains('c') {
            let mut tool_calls: Vec<serde_json::Value> = Vec::new();
            let mut call_ids: Vec<String> = Vec::new();
            let mut j = i;
            while j < turns.len() && turns[j].kind == "assistant" && turns[j].flags.contains('c') {
                let body = turns[j].read().unwrap_or_default();
                let env: serde_json::Value =
                    serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
                let name = env
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = env.get("arguments").cloned().unwrap_or(json!({}));
                // OpenAI wants arguments as a STRING (stringified JSON), not
                // an inline object — that's the convention strict providers
                // enforce.
                let args_str = match args {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                let call_id = turns[j]
                    .slug
                    .clone()
                    .unwrap_or_else(|| format!("call_orphan_{}", j));
                tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": args_str },
                }));
                call_ids.push(call_id);
                j += 1;
            }
            out.push(json!({
                "role": "assistant",
                "content": serde_json::Value::Null,
                "tool_calls": tool_calls,
            }));
            // Pair the next run of .tool turns to those calls positionally.
            // If there are more tool turns than calls, fan extras onto the
            // last call_id (degenerate but keeps the wire valid).
            let mut k: usize = 0;
            while j < turns.len() && turns[j].kind == "tool" {
                let raw = turns[j].read().unwrap_or_default();
                let id = call_ids
                    .get(k)
                    .or_else(|| call_ids.last())
                    .cloned()
                    .unwrap_or_default();
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": raw,
                }));
                k += 1;
                j += 1;
            }
            i = j;
            continue;
        }
        // Orphan tool turn (no preceding c assistant — can happen after a
        // backtrack that ate the call but spared the result). Demote to a
        // user message so the wire stays valid; the content is preserved.
        if t.kind == "tool" {
            out.push(json!({
                "role": "user",
                "content": t.read().unwrap_or_default(),
            }));
            i += 1;
            continue;
        }
        out.push(t.message(true));
        i += 1;
    }
    out
}
