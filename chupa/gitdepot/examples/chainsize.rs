//! Throwaway: per-chain LIVE compressed bytes (f0 + f1 + cold frames)
//! of a gitdepot store's depot, format-agnostic. Usage: chainsize <store>
//! Chain-size audit tool (live f0/f1/cold bytes per chain).

fn main() {
    let store = std::path::PathBuf::from(std::env::args().nth(1).expect("store path"));
    let depot = wikimak_depot::Depot::open(wikimak_depot::DepotConfig {
        root: store.join("depot"),
        max_chain_id: 3,
        file_size_threshold: 4 << 20,
        eviction_dead_ratio: 0.5,
    })
    .expect("open depot");
    let mut total = 0u64;
    for (id, name) in [(0u64, "trees"), (1, "commits"), (2, "reflog")] {
        let f0 = depot.read_f0(id).map(|b| b.len() as u64).unwrap_or(0);
        let f1 = depot
            .read_f1(id)
            .expect("read f1")
            .map(|b| b.len() as u64)
            .unwrap_or(0);
        let mut cold = 0u64;
        let mut ncold = 0u32;
        for c in depot.cold_iter(id).expect("cold iter") {
            cold += c.expect("cold frame").len() as u64;
            ncold += 1;
        }
        println!(
            "{name:8} f0={f0:>9}  f1={f1:>9}  cold={cold:>9} ({ncold} frames)  total={:>10}",
            f0 + f1 + cold
        );
        total += f0 + f1 + cold;
    }
    println!("all      {total}");
}
