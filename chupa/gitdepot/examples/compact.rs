//! Throwaway (Phase 5 eval): open a gitdepot store's depot and run
//! session-end compaction (`Depot::collect`), reclaiming dead bytes the
//! shipped importer leaves in sub-threshold current write files. Usage:
//! compact <store>. Afterward `du` reports the live compacted on-disk size.

fn main() {
    let store = std::path::PathBuf::from(std::env::args().nth(1).expect("store path"));
    let depot = wikimak_depot::Depot::open(wikimak_depot::DepotConfig {
        root: store.join("depot"),
        max_chain_id: 3,
        file_size_threshold: 4 << 20,
        eviction_dead_ratio: 0.5,
    })
    .expect("open depot");
    depot.collect().expect("collect");
    depot.flush().expect("flush");
    eprintln!("compacted {}", store.display());
}
