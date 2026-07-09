//! The geometric delta stack (ASSEMBLY.md §4).
//!
//! Each written delta layer is pushed on a stack (bottom = older, top =
//! newer). After every push, the two topmost layers are merged repeatedly
//! **while the top is at least 70% of the size of the one below it**,
//! stopping when the top drops below that (or only one entry remains). This
//! keeps the stack to ~log(n) layers of geometrically increasing size, so a
//! read walks few layers and a merge amortizes to O(total bytes · small
//! factor) rather than remerging the whole history each commit.
//!
//! The stack is generic over the layer type `L`: the driver only needs each
//! layer's byte `size` and a `merge(lower, upper)` that returns the single
//! layer equivalent to applying `lower` then `upper`. The concrete merge is
//! `depot::stream::compose_stream` extended with the hole-annihilation rule
//! (§4); this module is only the stacking policy and is tested against a
//! model merge so the policy is proven independently of the byte codec.

/// A geometric merge stack. `entries[0]` is the oldest (bottom), the last
/// is the newest (top).
pub struct GeoStack<L> {
    entries: Vec<L>,
}

impl<L> Default for GeoStack<L> {
    fn default() -> Self {
        GeoStack { entries: Vec::new() }
    }
}

impl<L> GeoStack<L> {
    pub fn new() -> Self {
        GeoStack::default()
    }

    /// The layers, oldest first. A reader applies them in this order over
    /// the base full-state.
    pub fn layers(&self) -> &[L] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Push a freshly written delta layer, then compact: while the top layer
    /// is ≥70% of the size of the layer below it, pop the two topmost, merge
    /// them (lower then upper), and push the result — re-checking each time.
    ///
    /// `size` returns a layer's byte length; `merge(lower, upper)` collapses
    /// two adjacent layers. The 70% test uses integer math: `top ≥ 0.7·next`
    /// ⇔ `10·top ≥ 7·next`.
    pub fn push(
        &mut self,
        layer: L,
        size: impl Fn(&L) -> u64,
        merge: impl Fn(L, L) -> L,
    ) {
        self.entries.push(layer);
        while self.entries.len() >= 2 {
            let n = self.entries.len();
            let top = size(&self.entries[n - 1]);
            let next = size(&self.entries[n - 2]);
            if 10 * top < 7 * next {
                break; // top dropped below 70% of next — geometric gap reached
            }
            let upper = self.entries.pop().unwrap();
            let lower = self.entries.pop().unwrap();
            self.entries.push(merge(lower, upper));
        }
    }

    /// Fully collapse the stack to a single layer (the seal / re-shard path).
    /// Returns `None` for an empty stack.
    pub fn collapse(mut self, merge: impl Fn(L, L) -> L) -> Option<L> {
        let mut acc = self.entries.drain(..);
        let mut cur = acc.next()?;
        for next in acc {
            cur = merge(cur, next);
        }
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Model layer: a sequence of (order-tag, size) merged by concatenation,
    /// so we can assert both the byte-conservation and the geometric shape
    /// without any codec.
    #[derive(Clone, Debug, PartialEq)]
    struct Model {
        // The pushed-layer ids this model layer covers, in apply order.
        ids: Vec<u32>,
        size: u64,
    }
    fn size(m: &Model) -> u64 {
        m.size
    }
    fn merge(lower: Model, upper: Model) -> Model {
        // Lower then upper: ids concatenate in apply order; sizes add.
        let mut ids = lower.ids;
        ids.extend(upper.ids);
        Model { size: lower.size + upper.size, ids }
    }

    /// The stack must always preserve the total content: concatenating every
    /// layer's ids in stack order equals the ids pushed, in push order.
    #[test]
    fn preserves_apply_order_and_bytes() {
        let mut rng = 0x1234_5678u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut stack = GeoStack::new();
        let mut pushed: Vec<u32> = Vec::new();
        let mut total = 0u64;
        for id in 0..500u32 {
            let sz = 1 + next() % 4000; // varied sizes
            pushed.push(id);
            total += sz;
            stack.push(Model { ids: vec![id], size: sz }, size, merge);

            // Invariant: flattening stack ids (oldest→newest) == push order.
            let flat: Vec<u32> = stack.layers().iter().flat_map(|m| m.ids.clone()).collect();
            assert_eq!(flat, pushed, "apply order broken after {id}");
            let sum: u64 = stack.layers().iter().map(|m| m.size).sum();
            assert_eq!(sum, total, "bytes not conserved after {id}");
        }
        // Geometric shape: sizes strictly increase bottom→top is NOT required
        // (a fresh small top is allowed), but each non-top layer must be
        // >~ the next-smaller by the 70% rule after compaction settles.
        let sizes: Vec<u64> = stack.layers().iter().map(|m| m.size).collect();
        // The stack stays short (log-ish), never one-per-push.
        assert!(sizes.len() < 40, "stack not compacting: {} layers", sizes.len());
    }

    /// Equal-size pushes collapse aggressively: N equal layers keep the top
    /// below 70% only briefly, so the stack stays tiny.
    #[test]
    fn equal_sizes_stay_shallow() {
        let mut stack = GeoStack::new();
        for id in 0..1024u32 {
            stack.push(Model { ids: vec![id], size: 100 }, size, merge);
        }
        // With uniform sizes every push merges down to few entries.
        assert!(stack.len() <= 12, "expected shallow, got {}", stack.len());
        // Collapsing yields all ids in order.
        let all: Vec<u32> = (0..1024).collect();
        assert_eq!(stack.collapse(merge).unwrap().ids, all);
    }

    #[test]
    fn the_70_percent_boundary() {
        // A big layer then a small one that is <70% must NOT merge.
        let mut s = GeoStack::new();
        s.push(Model { ids: vec![0], size: 1000 }, size, merge);
        s.push(Model { ids: vec![1], size: 699 }, size, merge); // 699 < 0.7*1000
        assert_eq!(s.len(), 2, "small top must not merge");
        // Exactly 70% DOES merge (≥).
        let mut s2 = GeoStack::new();
        s2.push(Model { ids: vec![0], size: 1000 }, size, merge);
        s2.push(Model { ids: vec![1], size: 700 }, size, merge); // 700 == 0.7*1000
        assert_eq!(s2.len(), 1, "70% boundary must merge");
    }
}
