// Turn-id generation: 5 lowercase ASCII letters. Probed against the existing
// set so the short length is a readability choice, not a correctness limit
// (26^5 = 11_881_376 — comfortably larger than any real session).

use rand::Rng;
use std::collections::HashSet;

const ID_LEN: usize = 5;

pub fn new_turn_id(existing: &HashSet<String>) -> String {
    let mut rng = rand::thread_rng();
    loop {
        let id: String = (0..ID_LEN)
            .map(|_| {
                let i = rng.gen_range(0..26);
                (b'a' + i) as char
            })
            .collect();
        if !existing.contains(&id) {
            return id;
        }
    }
}

/// Is `s` a valid turn-id slug we are willing to ADOPT from a model that
/// emitted a `{"turn-id":"..."}` header atop its reply? Same rules as the
/// filename grammar's turnid field — `[a-z0-9]+`, length-bounded.
pub fn is_adoptable_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}
