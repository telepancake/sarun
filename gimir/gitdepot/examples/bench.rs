//! Sanity timings against a real store: ref resolution and the streaming
//! checkout of the tip (and optionally a deep commit) — the bounded-memory
//! read path. `cargo run --release --example bench -- <store> [deep_sha]`.

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let store = std::path::PathBuf::from(args.next().expect("store path"));
    let deep = args.next();

    let t = Instant::now();
    let sha = gitdepot::resolve_ref(&store, "main").unwrap()
        .or_else(|| gitdepot::resolve_ref(&store, "master").unwrap())
        .expect("main/master")
        .sha()
        .to_string();
    println!("resolve_ref            {:>10.3?}  ({})", t.elapsed(), &sha[..8]);

    let ls = gitdepot::store::Store::open(&store).unwrap().union().unwrap();

    let t = Instant::now();
    let (mut files, mut bytes) = (0u64, 0u64);
    ls.checkout_entries(&sha, b"", &mut |_path, _mode, content| {
        files += 1;
        bytes += content.len() as u64;
        Ok(())
    })
    .unwrap();
    println!("tip: stream checkout   {:>10.3?}  ({files} files, {bytes} bytes)", t.elapsed());

    if let Some(dsha) = deep {
        let t = Instant::now();
        let (mut files, mut bytes) = (0u64, 0u64);
        ls.checkout_entries(&dsha, b"", &mut |_path, _mode, content| {
            files += 1;
            bytes += content.len() as u64;
            Ok(())
        })
        .unwrap();
        println!("deep: stream checkout  {:>10.3?}  ({files} files, {bytes} bytes)", t.elapsed());
    }
}
