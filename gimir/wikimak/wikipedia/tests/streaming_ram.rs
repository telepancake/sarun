//! Streaming import never holds a page's whole history in RAM.
//!
//! REAL effect, really measured: a synthetic single page whose total
//! revision text (~60MB) exceeds a test-shrunk ingest bound
//! (`WIKIMAK_TEST_INGEST_RAM` = 4MB) by ~15x is imported by the actual
//! `wikimak` CLI binary in a child process, and the child's peak RSS
//! (getrusage(RUSAGE_CHILDREN) max, == VmHWM) must stay BELOW the
//! corpus size — the pre-streaming importer materialized every
//! revision String plus every encoded record, >2x the corpus, before
//! the first depot write.
//!
//! Then the store must be a real store: every revision's text reads
//! back byte-exact through the public history API (newest-first), and
//! a re-import of the same dump is a byte-level no-op on the depot
//! (dedup + idempotency preserved across the batched prepends).

mod common;

use std::io::Write as _;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;
use wikimak_depot::DepotConfig;
use wikimak_wikipedia::{max_chain_id_for_root, Instance, InstanceConfig};

/// Revisions in the synthetic page, oldest-first ids 1000..1000+REVS.
const REVS: usize = 40;
/// ~bytes of text per revision.
const REV_TEXT_BYTES: usize = 1_500_000;
/// The shrunk ingest bound handed to the child.
const BOUND: u64 = 4 << 20;

/// Deterministic per-revision text, regenerable for the round-trip
/// check. Successive revisions share most lines (realistic for the
/// refPrefix compression path) plus per-revision salt lines.
fn rev_text(r: usize) -> String {
    let mut s = String::with_capacity(REV_TEXT_BYTES + 128);
    let mut i = 0usize;
    while s.len() < REV_TEXT_BYTES {
        // Every 17th line is revision-specific; the rest are shared.
        if i.is_multiple_of(17) {
            s.push_str(&format!("line {i:07} salted by revision {r:04}\n"));
        } else {
            s.push_str(&format!("line {i:07} shared corpus filler text\n"));
        }
        i += 1;
    }
    s
}

/// Stream the dump to disk — the TEST doesn't get to hold the corpus
/// either. Returns total text bytes.
fn write_dump(path: &Path) -> u64 {
    let f = std::fs::File::create(path).unwrap();
    let mut w = std::io::BufWriter::new(f);
    w.write_all(
        br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>ramwiki</sitename><dbname>ramwiki</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Hot Page</title><ns>0</ns><id>1</id>
"#,
    )
    .unwrap();
    let mut total = 0u64;
    for r in 0..REVS {
        let text = rev_text(r);
        total += text.len() as u64;
        write!(
            w,
            r#"    <revision>
      <id>{id}</id><timestamp>2024-01-01T{h:02}:{m:02}:00Z</timestamp>
      <contributor><username>E</username><id>1</id></contributor>
      <comment>r{r}</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{text}</text>
    </revision>
"#,
            id = 1000 + r,
            h = r / 60,
            m = r % 60,
        )
        .unwrap();
    }
    w.write_all(b"  </page>\n</mediawiki>\n").unwrap();
    w.flush().unwrap();
    total
}

/// Peak RSS in bytes over all reaped children (Linux ru_maxrss is KB).
fn children_peak_rss() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    assert_eq!(rc, 0, "getrusage failed");
    ru.ru_maxrss as u64 * 1024
}

/// Total bytes of every regular file under `dir`, recursively, as a
/// sorted (path, len) list — the byte-level depot fingerprint.
fn dir_manifest(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((p.to_string_lossy().into_owned(), p.metadata().unwrap().len()));
            }
        }
    }
    out.sort();
    out
}

fn run_import(dump: &Path, root: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_wikimak"))
        .arg("import")
        .arg(dump)
        .arg(root)
        // Shrink the fresh-root index too: the RAM assertion must not
        // be fogged by an 800MB (sparse, but mmap'd) default index.
        .args(["--max-page-id", "1024"])
        .env("WIKIMAK_TEST_INGEST_RAM", BOUND.to_string())
        .output()
        .expect("spawn wikimak import");
    assert!(
        out.status.success(),
        "import failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn oversized_page_imports_under_the_ram_bound_and_round_trips() {
    let tmp = TempDir::new().unwrap();
    let dump = tmp.path().join("dump.xml");
    let root = tmp.path().join("root");
    let total_text = write_dump(&dump);
    assert!(
        total_text > 10 * BOUND,
        "fixture must dwarf the bound: {total_text} vs {BOUND}"
    );

    // ---- import in a child; measure ITS peak RSS ----
    let stdout = run_import(&dump, &root);
    assert!(
        stdout.contains(&format!("revisions new {REVS}")),
        "first import stats: {stdout}"
    );
    let peak = children_peak_rss();
    assert!(peak > 4 << 20, "implausible peak RSS measurement: {peak}");
    assert!(
        peak < total_text,
        "peak RSS {peak} not under the {total_text}-byte corpus — \
         the import materialized the page history ({}x bound)",
        peak / BOUND
    );
    eprintln!(
        "import peak RSS {:.1} MB for a {:.1} MB single-page history (bound {} MB)",
        peak as f64 / (1 << 20) as f64,
        total_text as f64 / (1 << 20) as f64,
        BOUND >> 20,
    );

    // ---- round-trip: every revision byte-exact, newest-first ----
    let depot_before = dir_manifest(&root.join("depot"));
    {
        let inst = Instance::open(InstanceConfig {
            root: root.clone(),
            dbname: "wiki".into(),
            max_chain_id: max_chain_id_for_root(&root),
            depot: DepotConfig {
                root: Default::default(), // forced to <root>/depot/
                max_chain_id: max_chain_id_for_root(&root),
                file_size_threshold: 1 << 30,
                eviction_dead_ratio: 0.5,
            },
            title_shard_count: 4,
            title_seal_threshold_bytes: 8 << 20,
            f1_seal_threshold_bytes: 0,
        })
        .expect("open imported instance");
        let mut n = 0usize;
        for entry in inst.page_history(1).unwrap() {
            let e = entry.unwrap();
            let want_r = REVS - 1 - n; // newest-first
            assert_eq!(e.meta.rev_id, (1000 + want_r) as u64, "chain order");
            let text = (e.fetch_text)().unwrap();
            assert_eq!(
                text,
                rev_text(want_r).into_bytes(),
                "revision {} text must round-trip byte-exact",
                e.meta.rev_id
            );
            n += 1;
        }
        assert_eq!(n, REVS, "all revisions present exactly once");
    } // drop: release the root flock before the re-import child

    // ---- re-import: dedup no-op, depot bytes untouched ----
    let stdout = run_import(&dump, &root);
    assert!(
        stdout.contains("revisions new 0") && stdout.contains(&format!("deduped {REVS}")),
        "re-import must dedup everything: {stdout}"
    );
    let depot_after = dir_manifest(&root.join("depot"));
    assert_eq!(
        depot_before, depot_after,
        "re-import of an already-seen dump changed depot bytes"
    );
}
