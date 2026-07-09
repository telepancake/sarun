//! Executable design-conformance guards.
//!
//! DESIGN.md prohibitions are laundered by the usual acceptance proxy
//! (plausible + SHA-exact + tests green): a violation stays green because the
//! green thing is not the shipped thing. These tests turn a prohibition into a
//! negative assertion a plausible SHA-exact implementation cannot satisfy — the
//! reward here is *finding* the forbidden construct, not approving it.
//!
//! Source-scanning (not behavioural) on purpose: the defects are structural
//! (dead code kept green by its own tests, fossil APIs), so the assertion has to
//! be over the source text itself.

use std::path::Path;

fn read(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// A line that is neither a `fn NAME` definition, a use/import, nor a comment,
/// but that mentions `NAME(` or `NAME::` — i.e. an actual call/reference site.
fn is_call_site(line: &str, name: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with("*") {
        return false;
    }
    if t.contains(&format!("fn {name}")) || t.starts_with("use ") {
        return false;
    }
    t.contains(&format!("{name}(")) || t.contains(&format!("{name}::"))
}

/// D4 — the parallel union/delta encoder in `layer.rs`
/// (`encode_union`/`delta_multi_lane*`/`encode_lane`/`delta_single_lane`/
/// `visit_current`) was a second, DEAD implementation of the whole encode/delta
/// path, kept green only by its own unit tests while diverging from DESIGN §6
/// (the freed-slot / most-lanes reslot rule lives only in `reslot.rs`). It has
/// been removed; the live encoder is `oidenc.rs` + `reslot.rs`. These names must
/// never reappear as functions — a reintroduction is a second encoder drifting
/// out of sync with the shipped one, which is exactly what D4 was.
const DEAD_ENCODER_FAMILY: &[&str] = &[
    "encode_lane",
    "delta_single_lane",
    "encode_union",
    "encode_union_oid",
    "delta_multi_lane",
    "delta_multi_lane_stacked",
    "delta_multi_lane_stacked_oid",
    "visit_current",
];

/// Every gitdepot source file (the removed encoder must be gone from all of
/// them, not merely uncalled).
const SRC_FILES: &[&str] = &[
    "src/layer.rs", "src/oidenc.rs", "src/reslot.rs", "src/lanestore.rs",
    "src/store.rs", "src/readout.rs", "src/lib.rs", "src/cli.rs", "src/lanes.rs",
];

#[test]
fn dead_layer_encoder_family_is_removed_not_defined_anywhere() {
    // The strongest form of the D4 fix: not "dead but present", but gone. No
    // source file may DEFINE any of the family — the live path is oidenc+reslot.
    for rel in SRC_FILES {
        let src = read(rel);
        for name in DEAD_ENCODER_FAMILY {
            assert!(
                !src.contains(&format!("fn {name}")),
                "{rel} re-defines removed encoder `{name}` — the DESIGN §6 encoder \
                 is oidenc.rs+reslot.rs; a second one drifts out of sync (D4)."
            );
        }
    }
}

#[test]
fn dead_layer_encoder_family_has_no_caller() {
    // Belt-and-braces: even a call to one of these names (e.g. via a re-added
    // `pub use`) is forbidden across the whole crate.
    for rel in SRC_FILES {
        let src = read(rel);
        for (i, line) in src.lines().enumerate() {
            for name in DEAD_ENCODER_FAMILY {
                assert!(
                    !is_call_site(line, name),
                    "{rel}:{} references removed encoder `{name}` — the live encoder \
                     is oidenc.rs+reslot.rs (DESIGN §6).\n  {}",
                    i + 1,
                    line.trim(),
                );
            }
        }
    }
}

/// D1 — the reverse-delta-per-commit `TREES` chain was replaced by the union
/// (DESIGN §1: "the mirror IS the union"). Trees live in the LaneStore union
/// depot under `<store>/trees/`; `store.rs` owns only COMMITS/REFLOG/TAGS. The
/// dead chain, its constant, its phantom count, and the reconstruction fossils
/// must all stay gone so the build cannot resurrect the replaced architecture.
#[test]
fn removed_trees_chain_stays_removed() {
    let store = read("src/store.rs");
    // The dead chain-0 constant and its never-updated count key.
    for gone in ["pub const TREES", "\"n_trees\"", "n_trees"] {
        assert!(
            !store.contains(gone),
            "store.rs resurrects the dead TREES chain (`{gone}`) — tree state is \
             the union under <store>/trees/, not a per-commit chain (DESIGN §1)."
        );
    }
    // The reverse-delta reconstruction *methods* the fossil examples depended on.
    for gone in ["fn tree_view", "fn tree_views"] {
        assert!(
            !store.contains(gone),
            "store.rs resurrects `{gone}` — the per-commit TREES reverse-delta \
             reconstruction removed with the union switch (DESIGN §1)."
        );
    }
    // The three-chain depot: exactly COMMITS/REFLOG/TAGS, no reserved 4th slot.
    assert!(
        store.contains("const MAX_CHAIN_ID: u64 = 3"),
        "store.rs no longer bounds the depot to 3 chains — a 4th reserved slot is \
         the dead TREES chain (DESIGN §1)."
    );
    for fossil in ["examples/treecheck.rs", "examples/verify.rs", "examples/dump.rs"] {
        assert!(
            !Path::new(env!("CARGO_MANIFEST_DIR")).join(fossil).exists(),
            "{fossil} is back — it is a fossil of the removed TREES chain."
        );
    }
}
