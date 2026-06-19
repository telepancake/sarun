// Per-box turn budget. Engine-owned, debited automatically on every
// `api.proxy` conn (= one LLM call = one turn). Each debit walks UP
// the box's parent_box_id chain (sub-agent → parent → … → root) so
// a single allocation at the root caps spend across the whole tree:
// 100 turns at top means 100 LLM calls TOTAL no matter how
// distributed between the parent and its sub-agents.
//
// The check is topmost-first: if ANY capped box in the chain has
// ≤ 0 remaining, every box in its subtree gets HTTP 503 from
// `api.proxy` — no upstream call is made. The driver sees the 503,
// exits cleanly, the box stays alive in intermediate state, the
// user can resume by granting more turns (`oaita run <session>
// --max-steps N` adds N to the box's meta counter; an `ask(follow_up,
// max_steps=N)` grants from the parent's pool to the existing
// sub-agent box).
//
// Persistence: the engine writes the counter into the box's own
// sqlar `meta` table under the key TURN_BUDGET_KEY. Surviving across
// engine restarts is automatic — the next time a box's chain is
// walked, the value is read from sqlar.
//
// "Capped" vs "uncapped" semantics. A box without a `turn_budget`
// meta entry is UNCAPPED — neither checked nor debited. That gives
// the user's invariant: ask-level caps default to unlimited (the
// child box has no entry, only the parent's chain caps it).
// `grant` writes the entry; before grant fires, the box is
// uncapped.

use rusqlite::params;

/// Sqlar meta key used to persist the per-box remaining-turn count.
const TURN_BUDGET_KEY: &str = "turn_budget";

/// Read the remaining-turn count for `box_id`. `None` means the box
/// has no entry (= uncapped). `Some(n)` means it's been granted at
/// some point and has `n` turns left. Live-or-at-rest aware.
pub fn remaining(state: &crate::control::State, box_id: i64) -> Option<i64> {
    let live = state.lock().unwrap().overlay.clone()
        .and_then(|o| o.live_box(box_id));
    if let Some(b) = live {
        return b.get_meta(TURN_BUDGET_KEY).and_then(|s| s.parse().ok());
    }
    // At-rest box: open the sqlar directly. Same pattern control.rs
    // uses elsewhere for at-rest meta writes (e.g. rename).
    let p = crate::paths::state_home().join(format!("{box_id}.sqlar"));
    let conn = rusqlite::Connection::open(&p).ok()?;
    conn.query_row("SELECT value FROM meta WHERE key=?1", [TURN_BUDGET_KEY],
                   |r| r.get::<_, String>(0)).ok()
        .and_then(|s| s.parse().ok())
}

fn write(state: &crate::control::State, box_id: i64, value: i64) {
    let live = state.lock().unwrap().overlay.clone()
        .and_then(|o| o.live_box(box_id));
    let s = value.to_string();
    if let Some(b) = live {
        b.set_meta(TURN_BUDGET_KEY, &s);
        return;
    }
    let p = crate::paths::state_home().join(format!("{box_id}.sqlar"));
    if let Ok(c) = rusqlite::Connection::open(&p) {
        let _ = c.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![TURN_BUDGET_KEY, s]);
    }
}

/// Walk the parent_box_id chain from `box_id` to the root, returning
/// [box_id, parent, …, root]. Uses the in-memory parent map maintained
/// by the live-box overlay; at-rest boxes fall back to a sqlar read.
fn chain_of(state: &crate::control::State, box_id: i64) -> Vec<i64> {
    let mut chain = vec![box_id];
    let mut cur = box_id;
    let mut seen = std::collections::HashSet::new();
    seen.insert(cur);
    for _ in 0..64 {
        let parent = parent_of(state, cur);
        let Some(p) = parent else { break; };
        if !seen.insert(p) { break; }
        chain.push(p);
        cur = p;
    }
    chain
}

fn parent_of(state: &crate::control::State, box_id: i64) -> Option<i64> {
    let live = state.lock().unwrap().overlay.clone()
        .and_then(|o| o.live_box(box_id));
    if let Some(b) = live {
        return b.parent();
    }
    let p = crate::paths::state_home().join(format!("{box_id}.sqlar"));
    let conn = rusqlite::Connection::open(&p).ok()?;
    conn.query_row("SELECT value FROM meta WHERE key='parent_box_id'", [],
                   |r| r.get::<_, String>(0)).ok()
        .and_then(|s| s.parse().ok())
}

/// Grant `amount` more turns to `box_id`. Additive — resuming via
/// another `oaita run --max-steps N` or `ask(follow_up, max_steps=N)`
/// extends the box's pool. Always writes the row, so the box becomes
/// "capped" from this call onward even if it wasn't before.
pub fn grant(state: &crate::control::State, box_id: i64, amount: i64) {
    let cur = remaining(state, box_id).unwrap_or(0);
    write(state, box_id, cur + amount);
}

/// Decrement one turn from every CAPPED box in `box_id`'s parent
/// chain. Returns `Ok(())` on success, `Err(exhausted_box_id)`
/// when any capped box in the chain has ≤ 0 turns — names the
/// topmost (root-most) such box.
pub fn take_chain(state: &crate::control::State, box_id: i64) -> Result<(), i64> {
    let chain = chain_of(state, box_id);
    // Topmost-first check so the model gets the "root-most empty"
    // hint, which is the one worth granting more to.
    for &b in chain.iter().rev() {
        if let Some(v) = remaining(state, b) {
            if v <= 0 { return Err(b); }
        }
    }
    for &b in &chain {
        if let Some(v) = remaining(state, b) {
            write(state, b, v - 1);
        }
    }
    Ok(())
}
