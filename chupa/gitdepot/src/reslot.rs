//! Per-path variant→slot assignment — the whole variant algebra for ONE
//! file path, with NO tree iteration, NO depot types, NO content bytes: it
//! moves around variant ids and lane bitmaps only, so it is a standalone,
//! exhaustively testable unit. The tree walk feeds each path's current
//! variant set in and turns the returned slot changes into frame deltas;
//! the content bytes for a changed slot are fetched by the caller by id.
//!
//! ## Slots
//!
//! A path holds an ordered set of **slots**, each a stable key that a
//! variant occupies across revisions. A variant is identified by its id (a
//! git blob oid, or oid+mode — opaque bytes here) and carries a lane
//! **bitmap**. The point of a slot is stability: when a variant is edited
//! in place its slot is reused, so the frame carries a content replacement
//! at an unchanged key (small reverse delta, good zstd ordering), and when
//! a lane merely joins/leaves a variant only that slot's bitmap moves.
//!
//! ## One reslot step
//!
//! Given the slots as they stand and the path's NEW variant set (id →
//! bitmap), [`Slots::reslot`] reconciles them and returns the slots that
//! changed. The matching, in order:
//!
//! 1. **By id.** A new variant whose id already occupies a slot stays in
//!    that slot; its bitmap is updated only if it moved. (Content unchanged.)
//! 2. **By bitmap similarity.** Each still-unmatched new variant is paired
//!    with the still-free old slot it shares the most lanes with (must share
//!    at least one) — the heuristic that this is an in-place edit of that
//!    slot's old content. The slot is reused: content and bitmap replaced.
//! 3. **New slots.** Any new variant with no match takes the lowest free
//!    slot key — a genuinely new variant.
//! 4. **Deletions.** Any old slot left unmatched is emptied.

use std::collections::{BTreeMap, BTreeSet};

/// A variant id — opaque bytes (a git blob oid, or oid+mode). Compared for
/// equality only; never interpreted here.
/// A lane membership bitmap (bit `l` set ⇒ lane `l` carries this variant).
pub type Bitmap = Vec<u8>;

/// What occupies a slot: a variant identity `V` and the lanes that carry it.
/// `V` is whatever the caller keys variants by — the encoder uses the
/// `(attrs, content)` pair so a slot's occupant directly yields the bytes
/// and mode for a `\0v`/`\0m` frame node; the tests use a byte-string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occupant<V> {
    pub id: V,
    pub bitmap: Bitmap,
}

/// One slot that changed in a reslot step. `before`/`after` are the
/// occupant on each side (`None` = empty): `None→Some` is a create,
/// `Some→None` a delete, `Some→Some` an edit (content and/or bitmap). The
/// caller turns this into a frame delta at the path — using `after` for a
/// forward store, `before` for a reverse one. Unchanged slots never appear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotChange<V> {
    pub slot: u32,
    pub before: Option<Occupant<V>>,
    pub after: Option<Occupant<V>>,
}

/// The persistent per-path slot state, held in RAM across the whole encode
/// and mutated one revision at a time. Tiny when `V` is an id; the encoder's
/// `V = (attrs, content)` is Arc-backed so its content shares storage.
#[derive(Clone, Debug)]
pub struct Slots<V> {
    /// slot key → occupant. Absent key = free slot. Sparse; freed keys are
    /// reused lowest-first so the key space stays compact.
    by_slot: BTreeMap<u32, Occupant<V>>,
}

impl<V> Default for Slots<V> {
    fn default() -> Self {
        Slots { by_slot: BTreeMap::new() }
    }
}

fn common_lanes(a: &[u8], b: &[u8]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x & y).count_ones()).sum()
}

impl<V: Ord + Clone> Slots<V> {
    pub fn is_empty(&self) -> bool {
        self.by_slot.is_empty()
    }

    /// The current occupant of each slot, for reconstruction/inspection.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Occupant<V>)> {
        self.by_slot.iter().map(|(k, v)| (*k, v))
    }

    /// Directly place an occupant at a slot — used to reconstruct the slot
    /// state of a stored boundary union for an incremental update, where the
    /// slot keys must match what was originally assigned (read back from the
    /// stored frame, not re-derived).
    pub fn set(&mut self, slot: u32, occ: Occupant<V>) {
        self.by_slot.insert(slot, occ);
    }

    /// Lowest free slot key.
    fn free_slot(&self) -> u32 {
        let mut k = 0;
        while self.by_slot.contains_key(&k) {
            k += 1;
        }
        k
    }

    /// Reconcile the slots with the path's new variant set (`new_variants`:
    /// id → bitmap) and return the slots that changed. Mutates in place to
    /// the new assignment.
    pub fn reslot(&mut self, new_variants: &BTreeMap<V, Bitmap>) -> Vec<SlotChange<V>> {
        let mut changes = Vec::new();
        // Slots still up for grabs (start with all), and new variants still
        // needing a home.
        let mut free_old: BTreeSet<u32> = self.by_slot.keys().copied().collect();
        let mut pending: Vec<(&V, &Bitmap)> = Vec::new();

        // Pass 1 — id match: an id that already occupies a slot keeps it.
        for (id, bm) in new_variants {
            if let Some((&slot, _)) = self.by_slot.iter().find(|(_, occ)| &occ.id == id) {
                free_old.remove(&slot);
                let before = self.by_slot.get(&slot).cloned();
                if before.as_ref().map(|o| &o.bitmap) != Some(bm) {
                    let after = Occupant { id: id.clone(), bitmap: bm.clone() };
                    self.by_slot.insert(slot, after.clone());
                    changes.push(SlotChange { slot, before, after: Some(after) });
                }
            } else {
                pending.push((id, bm));
            }
        }

        // Pass 2 — similarity match: pair each unmatched new variant with the
        // free old slot it shares the most lanes with (≥1). Rank all
        // candidate pairs by shared-lane count and take them greedily so the
        // strongest edit-of pairing wins when several compete.
        let mut cand: Vec<(u32, usize, u32)> = Vec::new(); // (common, pending_idx, slot)
        for (pi, (_, bm)) in pending.iter().enumerate() {
            for &slot in &free_old {
                let c = common_lanes(bm, &self.by_slot[&slot].bitmap);
                if c > 0 {
                    cand.push((c, pi, slot));
                }
            }
        }
        cand.sort_by(|a, b| b.0.cmp(&a.0)); // most-common first
        let mut taken_pending = vec![false; pending.len()];
        for (_, pi, slot) in cand {
            if taken_pending[pi] || !free_old.contains(&slot) {
                continue;
            }
            taken_pending[pi] = true;
            free_old.remove(&slot);
            let (id, bm) = pending[pi];
            let before = self.by_slot.get(&slot).cloned();
            let after = Occupant { id: id.clone(), bitmap: bm.clone() };
            self.by_slot.insert(slot, after.clone());
            changes.push(SlotChange { slot, before, after: Some(after) });
        }

        // Pass 3 — new slots for variants that matched nothing.
        for (pi, (id, bm)) in pending.iter().enumerate() {
            if taken_pending[pi] {
                continue;
            }
            let slot = self.free_slot();
            let after = Occupant { id: (*id).clone(), bitmap: (*bm).clone() };
            self.by_slot.insert(slot, after.clone());
            changes.push(SlotChange { slot, before: None, after: Some(after) });
        }

        // Pass 4 — delete old slots nothing claimed.
        for slot in free_old {
            let before = self.by_slot.remove(&slot);
            changes.push(SlotChange { slot, before, after: None });
        }

        changes.sort_by_key(|c| c.slot);
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }
    /// Bitmap from a list of lane ids.
    fn bm(lanes: &[usize]) -> Bitmap {
        let mut b = Vec::new();
        for &l in lanes {
            let byte = l / 8;
            if b.len() <= byte {
                b.resize(byte + 1, 0);
            }
            b[byte] |= 1 << (l % 8);
        }
        b
    }
    fn variants(items: &[(&str, &[usize])]) -> BTreeMap<Vec<u8>, Bitmap> {
        items.iter().map(|(i, l)| (id(i), bm(l))).collect()
    }
    fn occ(slots: &Slots<Vec<u8>>) -> BTreeMap<u32, (Vec<u8>, Bitmap)> {
        slots.iter().map(|(k, o)| (k, (o.id.clone(), o.bitmap.clone()))).collect()
    }

    /// The user's worked example. Old slots: 0=aaaa, 1=bbbb, 2=ffff. New
    /// variants: bbbb, cccc, eeee, ffff. bbbb and ffff keep their slots by
    /// id; cccc shares lanes with aaaa's slot → reuses it (edit); eeee shares
    /// none → a fresh slot; aaaa itself is gone but its slot was taken by the
    /// edit, so no delete.
    #[test]
    fn worked_example_id_then_similarity() {
        let mut s = Slots::default();
        // Seed the three old slots (all lanes chosen so cccc overlaps aaaa).
        s.reslot(&variants(&[("aaaa", &[0, 1, 2]), ("bbbb", &[3]), ("ffff", &[4])]));
        assert_eq!(occ(&s).len(), 3);

        let changes = s.reslot(&variants(&[
            ("bbbb", &[3]),       // unchanged
            ("ffff", &[4]),       // unchanged
            ("cccc", &[0, 1, 5]), // shares lanes 0,1 with aaaa's slot 0 → edit
            ("eeee", &[6]),       // shares nothing → new slot
        ]));

        let m = occ(&s);
        // aaaa's slot 0 now holds cccc (the edit reused it).
        assert_eq!(m[&0].0, id("cccc"));
        assert_eq!(m[&1].0, id("bbbb"));
        assert_eq!(m[&2].0, id("ffff"));
        // eeee took the lowest free slot (3).
        assert_eq!(m[&3].0, id("eeee"));
        assert_eq!(m.len(), 4);

        // bbbb and ffff didn't move → not reported. cccc(slot0) edited,
        // eeee(slot3) created. No deletes.
        let touched: Vec<u32> = changes.iter().map(|c| c.slot).collect();
        assert_eq!(touched, vec![0, 3]);
        let c0 = changes.iter().find(|c| c.slot == 0).unwrap();
        assert_eq!(c0.before.as_ref().unwrap().id, id("aaaa"));
        assert_eq!(c0.after.as_ref().unwrap().id, id("cccc"));
        let c3 = changes.iter().find(|c| c.slot == 3).unwrap();
        assert!(c3.before.is_none() && c3.after.as_ref().unwrap().id == id("eeee"));
    }

    #[test]
    fn in_place_edit_keeps_slot_same_bitmap() {
        let mut s = Slots::default();
        s.reslot(&variants(&[("v1", &[0, 1, 2])]));
        let ch = s.reslot(&variants(&[("v2", &[0, 1, 2])])); // same lanes, new id
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].slot, 0);
        assert_eq!(ch[0].before.as_ref().unwrap().id, id("v1"));
        assert_eq!(ch[0].after.as_ref().unwrap().id, id("v2"));
        assert_eq!(ch[0].after.as_ref().unwrap().bitmap, bm(&[0, 1, 2]));
    }

    #[test]
    fn lane_joins_variant_is_bitmap_only_change() {
        let mut s = Slots::default();
        s.reslot(&variants(&[("shared", &[0, 1])]));
        let ch = s.reslot(&variants(&[("shared", &[0, 1, 2])])); // lane 2 joins
        assert_eq!(ch.len(), 1);
        // Same slot, same id — only the bitmap moved.
        assert_eq!(ch[0].before.as_ref().unwrap().id, id("shared"));
        assert_eq!(ch[0].after.as_ref().unwrap().id, id("shared"));
        assert_eq!(ch[0].before.as_ref().unwrap().bitmap, bm(&[0, 1]));
        assert_eq!(ch[0].after.as_ref().unwrap().bitmap, bm(&[0, 1, 2]));
    }

    #[test]
    fn unmatched_old_slot_is_deleted() {
        let mut s = Slots::default();
        s.reslot(&variants(&[("keep", &[0]), ("drop", &[9])]));
        // "drop" shares no lanes with anything new and its id is gone.
        let ch = s.reslot(&variants(&[("keep", &[0])]));
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].before.as_ref().unwrap().id, id("drop"));
        assert!(ch[0].after.is_none());
        assert_eq!(occ(&s).len(), 1);
    }

    #[test]
    fn identical_set_yields_no_changes() {
        let mut s = Slots::default();
        s.reslot(&variants(&[("a", &[0]), ("b", &[1, 2])]));
        let ch = s.reslot(&variants(&[("a", &[0]), ("b", &[1, 2])]));
        assert!(ch.is_empty(), "no-op reslot must emit nothing");
    }

    #[test]
    fn stronger_similarity_wins_the_slot() {
        // Two old slots; one new variant overlaps both but more with slot 1.
        let mut s = Slots::default();
        s.reslot(&variants(&[("x", &[0]), ("y", &[1, 2, 3])]));
        // New: "z" shares 3 lanes with y, 0 with x; "x" stays.
        let ch = s.reslot(&variants(&[("x", &[0]), ("z", &[1, 2, 3])]));
        // z should take y's slot (1), not x's.
        let m = occ(&s);
        assert_eq!(m[&0].0, id("x"));
        assert_eq!(m[&1].0, id("z"));
        assert_eq!(ch.iter().find(|c| c.slot == 1).unwrap().before.as_ref().unwrap().id, id("y"));
    }
}
