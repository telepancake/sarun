//! Sanity timings against a real store: tip readout, deep readout,
//! ref resolution. `cargo run --release --example bench -- <store>
//! [deep_sha]`.

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let store = std::path::PathBuf::from(args.next().expect("store path"));
    let deep = args.next();

    let t = Instant::now();
    let (sha, _idx) = gitdepot::resolve_ref(&store, "main").unwrap()
        .or_else(|| gitdepot::resolve_ref(&store, "master").unwrap())
        .expect("main/master");
    println!("resolve_ref            {:>10.3?}  ({})", t.elapsed(), &sha[..8]);

    let t = Instant::now();
    let ro = gitdepot::readout::TipReadout::for_commit(&store, &sha, "")
        .unwrap().expect("tip");
    // Force the lazy decode + touch one entry.
    use depot::variant::Readout as _;
    let n_root = ro.children(&[]).len();
    println!("tip: decode+ls root    {:>10.3?}  ({n_root} root entries)", t.elapsed());

    let t = Instant::now();
    let mut files = 0u64; let mut bytes = 0u64;
    let mut stack: Vec<Vec<Vec<u8>>> = vec![vec![]];
    while let Some(at) = stack.pop() {
        let comps: Vec<&[u8]> = at.iter().map(|c| c.as_slice()).collect();
        for name in ro.children(&comps) {
            let mut child = at.clone(); child.push(name.clone());
            let ccomps: Vec<&[u8]> = child.iter().map(|c| c.as_slice()).collect();
            match ro.entry(&ccomps) {
                Some(e) if matches!(e.kind, depot::variant::ReadoutKind::Branch) =>
                    stack.push(child),
                Some(e) => { files += 1; bytes += e.blob_len.unwrap_or(0); }
                None => {}
            }
        }
    }
    println!("tip: full tree walk    {:>10.3?}  ({files} files, {bytes} bytes)", t.elapsed());

    if let Some(dsha) = deep {
        let t = Instant::now();
        let dro = gitdepot::readout::TipReadout::for_commit(&store, &dsha, "")
            .unwrap().expect("deep commit");
        let n = dro.children(&[]).len();
        println!("deep: decode+ls root   {:>10.3?}  ({n} root entries)", t.elapsed());
    }
}
