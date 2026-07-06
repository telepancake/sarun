//! Throwaway health inspection of a gitdepot v2 TREES chain: per-record
//! byte anatomy (blob payload vs structure vs framing), op summaries,
//! aggregates, largest records. Usage: dump <store-path>
//!
//! NOT for commit; analysis scaffolding only.

use std::collections::BTreeMap;

use depot::{BlobOp, Node, Presence};
use gitdepot::store::{Store, TREES};

#[derive(Default, Clone)]
struct RecStats {
    total: usize,
    blob_payload: usize, // Set payload bytes only
    name_bytes: usize,   // child-name bytes (the path chains)
    attr_bytes: usize,   // attr key+value bytes
    other: usize,        // flags, varints, counts (codec overhead)
    nodes: usize,
    set_nodes: usize,
    remove_nodes: usize,
    tombstones: usize,
    keep_carriers: usize, // interior Keep nodes that only route to children
    attr_nodes: usize,
    opaque_nodes: usize,
    suspicious_identityish: usize, // Keep, no attrs, no children, live (should never exist)
    touched_paths: Vec<(String, String, usize, bool)>, // path, op, blob len, attrs?
}

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn walk(node: &Node, path: &str, s: &mut RecStats) {
    s.nodes += 1;
    s.other += 1; // flags byte
    if node.presence == Presence::Tombstone {
        s.tombstones += 1;
        s.touched_paths.push((path.to_string(), "tombstone".into(), 0, false));
        return;
    }
    let mut touched_here = false;
    match &node.blob {
        BlobOp::Set(b) => {
            s.set_nodes += 1;
            s.blob_payload += b.len();
            s.other += varint_len(b.len() as u64);
            s.touched_paths.push((
                path.to_string(),
                "set".into(),
                b.len(),
                node.attrs.is_some(),
            ));
            touched_here = true;
        }
        BlobOp::Remove => {
            s.remove_nodes += 1;
            s.touched_paths.push((path.to_string(), "remove-blob".into(), 0, node.attrs.is_some()));
            touched_here = true;
        }
        BlobOp::Keep => {}
    }
    if let Some(attrs) = &node.attrs {
        s.attr_nodes += 1;
        s.other += varint_len(attrs.len() as u64);
        for (k, v) in attrs {
            s.attr_bytes += k.len() + v.len();
            s.other += varint_len(k.len() as u64) + varint_len(v.len() as u64);
        }
        if !touched_here {
            s.touched_paths.push((path.to_string(), "attrs-only".into(), 0, true));
        }
    }
    if node.opaque {
        s.opaque_nodes += 1;
    }
    if matches!(node.blob, BlobOp::Keep) && node.attrs.is_none() && !node.opaque {
        if node.children.is_empty() {
            s.suspicious_identityish += 1;
        } else {
            s.keep_carriers += 1;
        }
    }
    s.other += varint_len(node.children.len() as u64);
    for (name, child) in &node.children {
        s.name_bytes += name.len();
        s.other += varint_len(name.len() as u64);
        let p = format!("{}/{}", path, String::from_utf8_lossy(name));
        walk(child, &p, s);
    }
}

fn analyze(rec: &[u8]) -> RecStats {
    let layer = depot::codec::decode(rec).expect("record must decode");
    let mut s = RecStats { total: rec.len(), ..Default::default() };
    walk(&layer.root, "", &mut s);
    s
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump <store>");
    let st = Store::open(std::path::Path::new(&path)).expect("open store");
    let n_trees = st.count(TREES).expect("count");
    // tree_idx -> (commit idx, sha, message first line)
    let mut tree_commit: BTreeMap<u64, (u64, String, String)> = BTreeMap::new();
    let commits = st.commit_records().expect("commit records");
    for c in &commits {
        tree_commit.entry(c.tree_idx).or_insert_with(|| {
            let msg = String::from_utf8_lossy(&c.message);
            let line = msg.lines().next().unwrap_or("").to_string();
            (c.idx, c.sha.clone(), line)
        });
    }
    println!("store: {path}");
    println!("n_trees={n_trees} n_commits={}", commits.len());

    let mut all: Vec<(usize, RecStats)> = Vec::new(); // pos, stats
    st.walk_tree_views(None, &mut |pos, rec, _view| {
        all.push((pos, analyze(rec)));
    })
    .expect("walk");

    let n = all.len();
    assert_eq!(n as u64, n_trees, "walked record count != n_trees");

    // ---- sample of ~30 across the chain
    let stride = (n / 30).max(1);
    println!("\n== sample (~30 records across chain; pos 0 = newest = full head layer) ==");
    println!(
        "{:>5} {:>6} {:>8} {:>9} {:>7} {:>6} {:>6} {:>6} {:>5} {:>5}  commit",
        "pos", "tidx", "bytes", "blob", "names", "attrs", "other", "nodes", "set", "tomb"
    );
    for (pos, s) in all.iter().filter(|(p, _)| p % stride == 0 || *p == 1) {
        let tidx = n_trees - 1 - *pos as u64;
        let c = tree_commit
            .get(&tidx)
            .map(|(i, sha, m)| format!("c{} {} {}", i, &sha[..8], m))
            .unwrap_or_default();
        println!(
            "{:>5} {:>6} {:>8} {:>9} {:>7} {:>6} {:>6} {:>6} {:>5} {:>5}  {}",
            pos, tidx, s.total, s.blob_payload, s.name_bytes, s.attr_bytes, s.other,
            s.nodes, s.set_nodes, s.tombstones,
            c.chars().take(60).collect::<String>()
        );
    }

    // ---- 3 representative decodes (skip the pos-0 full layer)
    println!("\n== representative op summaries ==");
    for &pick in &[1usize, n / 2, n - 2] {
        let (pos, s) = &all[pick];
        let tidx = n_trees - 1 - *pos as u64;
        let c = tree_commit.get(&tidx);
        println!(
            "\n-- record pos {pos} (tree_idx {tidx}, {} bytes) commit: {}",
            s.total,
            c.map(|(i, sha, m)| format!("idx {} sha {} \"{}\"", i, sha, m))
                .unwrap_or_else(|| "?".into())
        );
        for (p, op, blen, attrs) in s.touched_paths.iter().take(25) {
            println!("   {p} -> {op} blob={blen}B attrs={attrs}");
        }
        if s.touched_paths.len() > 25 {
            println!("   ... {} more ops", s.touched_paths.len() - 25);
        }
        println!(
            "   nodes={} keep-carriers={} set={} tomb={} attr-nodes={} opaque={} identity-ish={}",
            s.nodes, s.keep_carriers, s.set_nodes, s.tombstones, s.attr_nodes,
            s.opaque_nodes, s.suspicious_identityish
        );
    }

    // ---- aggregates over ALL delta records (exclude pos 0: full layer by design)
    let deltas: Vec<&RecStats> = all.iter().filter(|(p, _)| *p != 0).map(|(_, s)| s).collect();
    let head = &all.iter().find(|(p, _)| *p == 0).unwrap().1;
    let tot: usize = deltas.iter().map(|s| s.total).sum();
    let blob: usize = deltas.iter().map(|s| s.blob_payload).sum();
    let names: usize = deltas.iter().map(|s| s.name_bytes).sum();
    let attrs: usize = deltas.iter().map(|s| s.attr_bytes).sum();
    let other: usize = deltas.iter().map(|s| s.other).sum();
    let framing = 4 * deltas.len(); // u32 length prefix per f1/cold entry
    let pct = |x: usize| 100.0 * x as f64 / tot.max(1) as f64;
    println!("\n== aggregate over {} delta records (head full layer excluded) ==", deltas.len());
    println!("head full layer: {} bytes ({} nodes, {} blob bytes)", head.total, head.nodes, head.blob_payload);
    println!("total raw delta bytes: {tot}");
    println!("  blob payload : {blob:>9} ({:5.1}%)", pct(blob));
    println!("  names (paths): {names:>9} ({:5.1}%)", pct(names));
    println!("  attrs        : {attrs:>9} ({:5.1}%)", pct(attrs));
    println!("  codec overhead (flags/varints): {other:>9} ({:5.1}%)", pct(other));
    println!("  u32 record framing (not in raw): {framing} bytes = {:.2}% of raw+framing", 100.0 * framing as f64 / (tot + framing) as f64);
    let agg = |f: fn(&RecStats) -> usize| deltas.iter().map(|s| f(s)).sum::<usize>();
    println!(
        "  nodes={} set={} remove={} tomb={} keep-carriers={} attr-nodes={} opaque={} identity-ish={}",
        agg(|s| s.nodes), agg(|s| s.set_nodes), agg(|s| s.remove_nodes), agg(|s| s.tombstones),
        agg(|s| s.keep_carriers), agg(|s| s.attr_nodes), agg(|s| s.opaque_nodes),
        agg(|s| s.suspicious_identityish)
    );

    // histogram
    let buckets = [64usize, 256, 1024, 4096, 16384, 65536, 262144, usize::MAX];
    let mut hist = vec![0usize; buckets.len()];
    let mut empties = 0;
    for s in &deltas {
        let i = buckets.iter().position(|&b| s.total <= b).unwrap();
        hist[i] += 1;
        if s.set_nodes + s.tombstones + s.remove_nodes + s.attr_nodes == 0 {
            empties += 1;
        }
    }
    println!("\n== size histogram (delta records) ==");
    let labels = ["<=64B", "<=256B", "<=1KB", "<=4KB", "<=16KB", "<=64KB", "<=256KB", ">256KB"];
    for (l, c) in labels.iter().zip(&hist) {
        println!("  {l:>8}: {c}");
    }
    println!("  no-op records (zero ops): {empties}");

    // 5 largest
    let mut idx: Vec<usize> = (0..all.len()).filter(|&i| all[i].0 != 0).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(all[i].1.total));
    println!("\n== 5 largest delta records ==");
    for &i in idx.iter().take(5) {
        let (pos, s) = &all[i];
        let tidx = n_trees - 1 - *pos as u64;
        let c = tree_commit
            .get(&tidx)
            .map(|(ci, sha, m)| format!("c{} {} \"{}\"", ci, &sha[..10], m))
            .unwrap_or_default();
        println!("  pos {pos} tree_idx {tidx}: {} bytes, {} set nodes — {}", s.total, s.set_nodes, c);
        for (p, op, blen, _) in s.touched_paths.iter().take(10) {
            println!("     {p} [{op} {blen}B]");
        }
        if s.touched_paths.len() > 10 {
            println!("     ... {} more", s.touched_paths.len() - 10);
        }
    }
}
