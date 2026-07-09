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
/// `visit_current`) is dead: the live encoder is `oidenc.rs` + `reslot.rs`.
/// Its only callers must live inside `#[cfg(test)]`; the moment production code
/// depends on it again, the two encoders have diverged silently (DESIGN §6 —
/// the freed-slot / most-lanes reslot rule lives only in `reslot.rs`).
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

#[test]
fn dead_layer_encoder_has_no_caller_outside_layer_rs() {
    // No production module may reference the parallel encoder family. The live
    // path imports only the §2 name codec + reconstruction helpers from layer.
    for rel in ["src/oidenc.rs", "src/reslot.rs", "src/lanestore.rs", "src/store.rs",
                "src/readout.rs", "src/lib.rs", "src/cli.rs", "src/lanes.rs"] {
        let src = read(rel);
        for (i, line) in src.lines().enumerate() {
            for name in DEAD_ENCODER_FAMILY {
                assert!(
                    !is_call_site(line, name),
                    "{rel}:{} references dead layer.rs encoder `{name}` — the live \
                     encoder is oidenc.rs+reslot.rs (DESIGN §6). If this is a real \
                     new dependency the dead family must be revived deliberately, \
                     not leaned on.\n  {}",
                    i + 1,
                    line.trim(),
                );
            }
        }
    }
}

#[test]
fn dead_layer_encoder_is_reached_only_from_within_layer_rs() {
    // Inside layer.rs the family calls itself (one entry point delegates to the
    // next) and is exercised by the `#[cfg(test)]` module — that is expected and
    // is the whole point of D4: it is *only* reached from there. The guard that
    // matters is the cross-file one above; here we just pin that the family is
    // still self-contained in layer.rs and every public name still exists, so a
    // silent rename can't quietly slip the guard.
    let src = read("src/layer.rs");
    for name in DEAD_ENCODER_FAMILY {
        assert!(
            src.contains(&format!("fn {name}")),
            "layer.rs no longer defines `{name}` — update DEAD_ENCODER_FAMILY so \
             the cross-file guard keeps matching the real symbols."
        );
    }
}

/// D1 — the reverse-delta-per-commit `TREES` chain was replaced by the union
/// (DESIGN §1: "the mirror IS the union"). The `treecheck` example was the last
/// fossil calling the removed `Store::tree_view(s)` / `CommitRecord::tree_idx`
/// reconstruction API; it must stay gone so the build cannot resurrect it.
#[test]
fn removed_trees_chain_reconstruction_api_stays_removed() {
    let store = read("src/store.rs");
    // The reverse-delta reconstruction *methods* the fossil depended on.
    for gone in ["fn tree_view", "fn tree_views"] {
        assert!(
            !store.contains(gone),
            "store.rs resurrects `{gone}` — the per-commit TREES reverse-delta \
             reconstruction removed with the union switch (DESIGN §1)."
        );
    }
    assert!(
        !Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/treecheck.rs").exists(),
        "examples/treecheck.rs is back — it is a fossil of the removed TREES chain."
    );
}
