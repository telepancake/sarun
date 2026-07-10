//! Forward cold-frame construction for bulk import (depot SPEC §"Bulk
//! forward construction"), pinned end-to-end through the importer:
//!
//!   * EQUIVALENCE: the same dump imported through the forward path
//!     and through the prepend path (test knob) serves byte-identical
//!     revision texts and metadata — the read walk cannot tell the
//!     stores apart (on-disk frame boundaries may differ; a wrong
//!     refPrefix anchor would fail the zstd decode loudly, so the
//!     store verifying at all is itself the strong claim). Digested
//!     (SHA-1) served outputs must match too.
//!   * READ INSTRUMENTATION: the 2990906 contract holds on a
//!     forward-built store — head reads touch (1,0,0), an
//!     oldest-revision read walks each frame exactly once.
//!   * WRITE AMPLIFICATION, measured not asserted from thin air: depot
//!     bytes-written / final on-disk data bytes ≈ 1.0 for the forward
//!     path; the prepend path's factor is reported alongside (it
//!     rewrites the accumulator per batch and copies it again at each
//!     seal).
//!   * CRASH MID-CONSTRUCTION (separate test, child processes): a
//!     build aborted between cold frames leaves the chain invisible,
//!     the store reopens sound, and a re-import completes with a full
//!     round-trip — the orphan frames stay as dead cold bytes, exactly
//!     the documented trade.

mod common;

use std::io::{Cursor, Write as _};
use std::path::Path;

use sha1::{Digest as _, Sha1};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::Instance;

const PAGE_ID: u64 = 7;

/// Deterministic multi-revision page: successive revisions share most
/// lines (the refPrefix redundancy the design exists for) plus a salt
/// line each — regenerable for the round-trip checks. `lines` scales
/// the per-revision size (~36 bytes/line).
fn rev_text(r: usize, lines: usize) -> String {
    let mut s = String::with_capacity(lines * 40 + 64);
    for i in 0..lines {
        if i == r % lines {
            s.push_str(&format!("line {i:04} EDITED by revision {r:04}\n"));
        } else {
            s.push_str(&format!("line {i:04} stable shared corpus filler\n"));
        }
    }
    s.push_str(&format!("appended tail of revision {r:04}\n"));
    s
}

fn export_xml(n: usize, lines: usize) -> String {
    let mut revs = String::new();
    for r in 0..n {
        revs.push_str(&format!(
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
            text = rev_text(r, lines),
        ));
    }
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>fb</sitename><dbname>fb</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Forward Page</title><ns>0</ns><id>{PAGE_ID}</id>
{revs}  </page>
</mediawiki>"#
    )
}

/// One served revision: `(rev_id, ts_micros, text)`.
type Served = (u64, i64, Vec<u8>);

/// History newest-first plus a digest of the served stream — what
/// "reads must be byte-identical" means concretely.
fn served_history(inst: &Instance) -> (Vec<Served>, [u8; 20]) {
    let mut out = Vec::new();
    let mut hasher = Sha1::new();
    for entry in inst.page_history(PAGE_ID).unwrap() {
        let e = entry.unwrap();
        let text = (e.fetch_text)().unwrap();
        hasher.update(e.meta.rev_id.to_le_bytes());
        hasher.update(e.meta.sha1.as_bytes());
        hasher.update(&text);
        out.push((e.meta.rev_id, e.meta.ts.timestamp_micros(), text));
    }
    (out, hasher.finalize().into())
}

/// On-disk DATA bytes of the depot: f0 + f1 + cold files. The sparse
/// index and the advisory sidecars are bookkeeping, not store data.
fn depot_data_bytes(root: &Path) -> u64 {
    let mut total = 0;
    for sub in ["f0", "f1", "cold"] {
        if let Ok(rd) = std::fs::read_dir(root.join("depot").join(sub)) {
            for e in rd.flatten() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// ONE test (not several) because it steers the import path via
/// process-global env vars; the crash test below only spawns children
/// with their env set explicitly, so the two can run concurrently.
#[test]
fn forward_equals_prepend_and_amplification_is_measured() {
    const N: usize = 80;
    // Small ingest bound -> many batches -> many cold frames; the seal
    // threshold sits below it so the PREPEND store seals per batch too.
    std::env::set_var("WIKIMAK_TEST_INGEST_RAM", "32768");
    let mk = |tmp: &TempDir| {
        let mut cfg = common::cfg(tmp.path().to_path_buf(), 1024);
        cfg.f1_seal_threshold_bytes = 16 * 1024;
        Instance::open(cfg).unwrap()
    };
    const LINES: usize = 64; // ~2.4 KB/revision
    let xml = export_xml(N, LINES);

    // ---- forward-built store (the fresh-page default path) ----
    let tmp_a = TempDir::new().unwrap();
    let a = mk(&tmp_a);
    let mut stream = new_page_stream(Cursor::new(xml.clone().into_bytes()));
    let stats = a.import(&mut stream).unwrap();
    assert_eq!(stats.revisions_new as usize, N);
    a.flush().unwrap();
    a.collect().unwrap();
    let written_a = a.depot_bytes_written();

    // The store is genuinely deep: count the cold frames by walking
    // the whole history once, META-ONLY (each `fetch_text` is its own
    // fresh early-stopping walk and would multiply the counters), and
    // watching the payload-read counters.
    let c0 = a.depot_read_counts();
    let walked = a.page_history(PAGE_ID).unwrap().inspect(|e| assert!(e.is_ok())).count();
    assert_eq!(walked, N);
    let c1 = a.depot_read_counts();
    let cold_frames = c1.cold - c0.cold;
    assert!(
        cold_frames >= 3,
        "fixture must forward-build several cold frames, got {cold_frames}"
    );
    assert_eq!((c1.f0 - c0.f0, c1.f1 - c0.f1), (1, 1), "one f0 + one f1 on the meta walk");
    let (hist_a, digest_a) = served_history(&a);

    // Round-trip against the generator, newest-first.
    assert_eq!(hist_a.len(), N);
    for (i, (rev_id, _ts, text)) in hist_a.iter().enumerate() {
        let want = N - 1 - i;
        assert_eq!(*rev_id, (1000 + want) as u64, "newest-first order");
        assert_eq!(text, &rev_text(want, LINES).into_bytes(), "revision {rev_id} text");
    }

    // (b) 2990906 read instrumentation on the forward-built store.
    let c0 = a.depot_read_counts();
    assert_eq!(a.page_head(PAGE_ID).unwrap().unwrap().rev_id, (1000 + N - 1) as u64);
    let c1 = a.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (1, 0, 0),
        "head read must touch only f0 on a forward-built store"
    );
    let tau = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 30)
        .unwrap()
        .and_utc()
        .timestamp_micros(); // rev 1000 (00:00) only
    let c0 = a.depot_read_counts();
    let text = a.page_text_at(PAGE_ID, Some(tau)).unwrap().unwrap();
    assert_eq!(text, rev_text(0, LINES).into_bytes());
    let c1 = a.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (1, 1, cold_frames),
        "oldest-revision read walks each frame exactly once"
    );

    // ---- prepend-built store of the same dump (test knob) ----
    std::env::set_var("WIKIMAK_TEST_FORCE_PREPEND", "1");
    let tmp_b = TempDir::new().unwrap();
    let b = mk(&tmp_b);
    let mut stream = new_page_stream(Cursor::new(xml.into_bytes()));
    let stats = b.import(&mut stream).unwrap();
    std::env::remove_var("WIKIMAK_TEST_FORCE_PREPEND");
    assert_eq!(stats.revisions_new as usize, N);
    b.flush().unwrap();
    b.collect().unwrap();
    let written_b = b.depot_bytes_written();

    // (a) EQUIVALENCE: reads are byte-identical, digests included.
    let (hist_b, digest_b) = served_history(&b);
    assert_eq!(hist_a, hist_b, "forward and prepend stores serve different bytes");
    assert_eq!(digest_a, digest_b, "served-output digests differ");

    // (d) WRITE AMPLIFICATION, measured: bytes the depot wrote divided
    // by the data bytes that ended on disk.
    let disk_a = depot_data_bytes(tmp_a.path());
    let disk_b = depot_data_bytes(tmp_b.path());
    let amp_a = written_a as f64 / disk_a as f64;
    let amp_b = written_b as f64 / disk_b as f64;
    eprintln!(
        "write amplification: forward {amp_a:.3} ({written_a} written / {disk_a} on disk), \
         prepend {amp_b:.3} ({written_b} written / {disk_b} on disk)"
    );
    assert!(
        amp_a < 1.05,
        "forward build must write each history byte ~once, measured {amp_a:.3}"
    );
    assert!(
        amp_b > amp_a + 0.3,
        "prepend path should measurably amplify vs forward: {amp_b:.3} vs {amp_a:.3}"
    );
    std::env::remove_var("WIKIMAK_TEST_INGEST_RAM");
}

// ---------------------------------------------------------------------------
// (c) Crash mid-construction: the abort knob kills the real CLI binary
// between cold frames. Reopen must be sound with the half-built chain
// invisible; a re-import completes and round-trips.
// ---------------------------------------------------------------------------

/// The CLI's batch bound is max(WIKIMAK_TEST_INGEST_RAM, 256K default
/// seal threshold), so the crash fixture needs revisions fat enough
/// that 60 of them span several 256K batches: ~24 KB each, ~1.4 MB total.
const CRASH_LINES: usize = 640;

fn write_dump(path: &Path, n: usize) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    f.write_all(export_xml(n, CRASH_LINES).as_bytes()).unwrap();
    f.flush().unwrap();
}

fn run_import(dump: &Path, root: &Path, abort_after: Option<u64>) -> std::process::Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_wikimak"));
    cmd.arg("import")
        .arg(dump)
        .arg(root)
        .args(["--max-page-id", "1024"])
        // Explicit env: the equivalence test above mutates the parent's
        // env concurrently, so children pin every knob themselves.
        .env("WIKIMAK_TEST_INGEST_RAM", "8192")
        .env_remove("WIKIMAK_TEST_FORCE_PREPEND");
    match abort_after {
        Some(n) => cmd.env("WIKIMAK_TEST_ABORT_AFTER_COLD_FRAMES", n.to_string()),
        None => cmd.env_remove("WIKIMAK_TEST_ABORT_AFTER_COLD_FRAMES"),
    };
    cmd.output().expect("spawn wikimak import")
}

#[test]
fn crash_mid_construction_reopens_sound_and_reimports() {
    const N: usize = 60;
    let tmp = TempDir::new().unwrap();
    let dump = tmp.path().join("dump.xml");
    let root = tmp.path().join("root");
    write_dump(&dump, N);

    // Child 1 aborts after the 2nd cold frame, BEFORE the index flip.
    let out = run_import(&dump, &root, Some(2));
    assert!(
        !out.status.success(),
        "abort knob must kill the import: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let cold_file = root.join("depot/cold/cold");
    let orphan_bytes = cold_file.metadata().map(|m| m.len()).unwrap_or(0);
    assert!(orphan_bytes > 0, "the aborted build must have written cold frames");

    // Reopen is sound and the half-built chain is INVISIBLE. The CLI
    // child created the store (4 title shards persisted); derive its
    // count instead of asserting the test default 1.
    {
        let mut cfg = common::cfg(root.clone(), 1024);
        cfg.title_shard_count = 0;
        let inst = Instance::open(cfg).expect("reopen after abort");
        assert!(inst.page_head(PAGE_ID).unwrap().is_none(), "orphans must not surface");
        assert_eq!(inst.page_history(PAGE_ID).unwrap().count(), 0);
    } // drop: release the root flock for the re-import child

    // Child 2 re-imports clean (the dirty-flag machinery re-derives
    // nothing here — the chain is still empty — so all revisions are
    // new again) and the store round-trips fully.
    let out = run_import(&dump, &root, None);
    assert!(
        out.status.success(),
        "re-import failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&format!("revisions new {N}")), "re-import stats: {stdout}");

    let mut cfg = common::cfg(root.clone(), 1024);
    cfg.title_shard_count = 0; // CLI-built store: derive its persisted count
    let inst = Instance::open(cfg).unwrap();
    let mut n = 0usize;
    for entry in inst.page_history(PAGE_ID).unwrap() {
        let e = entry.unwrap();
        let want = N - 1 - n;
        assert_eq!(e.meta.rev_id, (1000 + want) as u64);
        assert_eq!((e.fetch_text)().unwrap(), rev_text(want, CRASH_LINES).into_bytes());
        n += 1;
    }
    assert_eq!(n, N, "every revision present exactly once after the re-import");

    // The orphan frames are still parked in the cold file (dead bytes
    // reclaimed only by instance delete) — the accepted trade, pinned
    // so a future "fix" that compacts cold shows up loudly.
    let cold_after = cold_file.metadata().unwrap().len();
    assert!(
        cold_after > orphan_bytes,
        "re-import appended past the orphans ({cold_after} vs {orphan_bytes})"
    );
}
