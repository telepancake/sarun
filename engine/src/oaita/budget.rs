// Per-session turn budget. Engine-owned, decremented automatically on
// every `api.proxy` conn (= one LLM call = one turn). The decrement
// walks UP the session's parent chain (sub-agent → parent → ... → root)
// so a single allocation at the root caps spend across the whole tree:
// 100 turns at top-level means 100 LLM calls TOTAL no matter how
// distributed between the parent and its sub-agents.
//
// The check is topmost-first: if ANY session in the chain has ≤ 0
// remaining, every box in its subtree gets a 503 from `api.proxy` —
// no upstream call is made. The driver sees the 503, exits cleanly,
// the box stays alive in intermediate state, the user can resume by
// granting more turns (`oaita run <session> --max-steps N` adds N
// to that root session's pool; a `follow_up` ask spawns a new
// sub-agent that still draws from the same root).
//
// Persistence: in-memory only for now. Lost across engine restarts;
// a future commit will mirror the counters into the engine's own
// state directory.
//
// Parent chain: derived from the sub-agent seed turn (0010-*.user)
// whose JSON header carries `from: "<parent-session>"`. Engine reads
// this once per session and caches; a session with no `from` (or no
// 0010 file) is treated as root.

use std::collections::HashMap;
use std::sync::Mutex;

/// Where the persisted budget lives — one JSON object keyed by session.
/// Lives next to the other engine state (sqlar files, sockets); on
/// process restart we reload from here so granted-but-unspent turns
/// don't get lost.
fn store_path() -> std::path::PathBuf {
    crate::paths::state_home().join("oaita-budgets.json")
}

fn load_persisted() -> HashMap<String, i64> {
    let p = store_path();
    let bytes = match std::fs::read(&p) { Ok(b) => b, Err(_) => return HashMap::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn persist(state: &HashMap<String, i64>) {
    let p = store_path();
    if let Some(parent) = p.parent() { let _ = std::fs::create_dir_all(parent); }
    let Ok(bytes) = serde_json::to_vec(state) else { return; };
    // Atomic rename so a partial write can't trash the file.
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, &bytes).is_err() { return; }
    let _ = std::fs::rename(&tmp, &p);
}

fn budget() -> &'static Mutex<HashMap<String, i64>> {
    static B: std::sync::OnceLock<Mutex<HashMap<String, i64>>> =
        std::sync::OnceLock::new();
    B.get_or_init(|| Mutex::new(load_persisted()))
}

fn parent_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static P: std::sync::OnceLock<Mutex<HashMap<String, Option<String>>>> =
        std::sync::OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

fn read_parent_from_seed(session: &str) -> Option<String> {
    let dir = crate::paths::oaita_state_home().join(session);
    let entries = std::fs::read_dir(&dir).ok()?;
    for e in entries.flatten() {
        let n = e.file_name();
        let n = n.to_string_lossy().into_owned();
        if !n.starts_with("0010-") || !n.ends_with(".user") {
            continue;
        }
        let content = std::fs::read_to_string(e.path()).ok()?;
        let first_line = content.lines().next()?;
        let v: serde_json::Value = serde_json::from_str(first_line).ok()?;
        return v.get("from").and_then(|f| f.as_str()).map(String::from);
    }
    None
}

fn parent_of(session: &str) -> Option<String> {
    {
        let c = parent_cache().lock().unwrap();
        if let Some(p) = c.get(session) {
            return p.clone();
        }
    }
    let p = read_parent_from_seed(session);
    parent_cache().lock().unwrap().insert(session.to_string(), p.clone());
    p
}

/// Walk `session` → root, returning the chain in order [session, parent, …, root].
fn chain_of(session: &str) -> Vec<String> {
    let mut chain = vec![session.to_string()];
    let mut cur = session.to_string();
    let mut seen = std::collections::HashSet::new();
    seen.insert(cur.clone());
    while let Some(p) = parent_of(&cur) {
        if !seen.insert(p.clone()) { break; }  // cycle guard
        chain.push(p.clone());
        cur = p;
    }
    chain
}

/// Grant `amount` more turns to `session`. Additive — resuming a
/// session via another `oaita run --max-steps N` extends its pool.
/// Persists the new total so engine restart doesn't lose it.
pub fn grant(session: &str, amount: i64) {
    let mut b = budget().lock().unwrap();
    *b.entry(session.to_string()).or_insert(0) += amount;
    persist(&b);
}

/// Decrement one turn from EVERY capped session in `session`'s parent
/// chain. A session is "capped" iff it has an entry in the pool — set
/// by an explicit `grant` (cli `--max-steps`, or an `ask`-level
/// `max_steps` arg). Sessions WITHOUT an entry are uncapped and pass
/// through transparently: they neither check nor decrement. That gives
/// the user's invariant: "ask-level caps default to unlimited; only
/// the parent's caps apply unless the model sets one explicitly".
///
/// Returns `Ok(())` on success; `Err(exhausted_session)` when ANY
/// capped session in the chain has ≤ 0 turns BEFORE the decrement —
/// names the topmost (root-most) such session, so the model gets the
/// most useful "where to grant more" hint.
pub fn take_chain(session: &str) -> Result<(), String> {
    let chain = chain_of(session);
    let mut b = budget().lock().unwrap();
    for s in chain.iter().rev() {
        if let Some(&v) = b.get(s) {
            if v <= 0 {
                return Err(s.clone());
            }
        }
    }
    for s in &chain {
        if let Some(v) = b.get_mut(s) {
            *v -= 1;
        }
    }
    persist(&b);
    Ok(())
}

/// Remaining turns at THIS session (no chain walk). For status views.
pub fn remaining(session: &str) -> i64 {
    budget().lock().unwrap().get(session).copied().unwrap_or(0)
}
