//! An oldest-revision read touches every frame of a deep chain — but
//! its peak RSS stays ~one-frame-sized, nowhere near the decompressed
//! history. REAL effect, really measured: the store (a ~48MB
//! single-page history sealed into many ~MB cold frames) is read by
//! the actual `wikimak text <root> <page> <asof>` CLI in a child
//! process, and the child's peak RSS (getrusage(RUSAGE_CHILDREN) max,
//! == VmHWM) must stay a small fraction of the corpus. The
//! pre-streaming reader (`collect_records`) materialized every
//! decompressed record before answering — ≥ the corpus, every read.

mod common;

use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::Instance;

/// Imports (2 revisions each) building one page's history. Sized so the
/// decompressed history (~96MB) dwarfs the reader's frame-sized working
/// set (~1-2MB frames plus zstd's decode window, ~8MB at these frame
/// sizes) — the assertion is that peak RSS does NOT scale with this.
const IMPORTS: u64 = 40;
/// ~bytes of text per revision.
const REV_TEXT_BYTES: usize = 1_200_000;

/// Deterministic per-revision text (regenerable for the byte-exact
/// check); successive revisions share most lines.
fn rev_text(rev: u64) -> String {
    let mut s = String::with_capacity(REV_TEXT_BYTES + 128);
    let mut i = 0usize;
    while s.len() < REV_TEXT_BYTES {
        if i % 13 == 0 {
            s.push_str(&format!("line {i:07} salted by revision {rev:04}\n"));
        } else {
            s.push_str(&format!("line {i:07} shared corpus filler text\n"));
        }
        i += 1;
    }
    s
}

fn ts_of(rev: u64) -> String {
    format!("2024-01-01T{:02}:{:02}:00Z", rev / 60, rev % 60)
}

fn doc_two_revs(first_rev: u64) -> String {
    let mut revs = String::new();
    for rev in first_rev..first_rev + 2 {
        revs.push_str(&format!(
            r#"<revision><id>{rev}</id><timestamp>{ts}</timestamp>
      <contributor><username>E</username><id>1</id></contributor>
      <comment>r{rev}</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{text}</text></revision>
"#,
            ts = ts_of(rev),
            text = rev_text(rev),
        ));
    }
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>rss</sitename><dbname>rss</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page><title>Hot Page</title><ns>0</ns><id>1</id>
{revs}  </page>
</mediawiki>"#
    )
}

/// Peak RSS in bytes over all reaped children (Linux ru_maxrss is KB).
fn children_peak_rss() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    assert_eq!(rc, 0, "getrusage failed");
    ru.ru_maxrss as u64 * 1024
}

fn read_text_child(root: &Path, page: u64, asof: Option<i64>) -> Vec<u8> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wikimak"));
    cmd.arg("text").arg(root).arg(page.to_string());
    if let Some(ts) = asof {
        cmd.arg(ts.to_string());
    }
    let out = cmd.output().expect("spawn wikimak text");
    assert!(
        out.status.success(),
        "wikimak text failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn oldest_revision_read_stays_frame_sized() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let mut corpus: u64 = 0;
    {
        // Build the deep store IN PROCESS (default 256 KiB seal → each
        // 2-revision import seals the previous accumulator to cold) so
        // the only child this test ever spawns is the reader.
        let inst = Instance::open(common::cfg(root.clone(), 1024)).unwrap();
        for i in 0..IMPORTS {
            let doc = doc_two_revs(1 + i * 2);
            corpus += (rev_text(1 + i * 2).len() + rev_text(2 + i * 2).len()) as u64;
            let mut s = new_page_stream(Cursor::new(doc.into_bytes()));
            inst.import(&mut s).expect("import");
        }
        inst.flush().expect("flush");
    } // drop: release the root flock before the child opens it

    let oldest_ts = chrono::DateTime::parse_from_rfc3339(&ts_of(1))
        .unwrap()
        .timestamp_micros();

    let text = read_text_child(&root, 1, Some(oldest_ts));
    assert_eq!(
        text,
        rev_text(1).into_bytes(),
        "oldest revision must round-trip byte-exact through the CLI"
    );

    let peak = children_peak_rss();
    assert!(peak > 1 << 20, "implausible peak RSS measurement: {peak}");
    assert!(
        peak < corpus / 3,
        "reader peak RSS {peak} not a small fraction of the {corpus}-byte \
         decompressed history — the read materialized the chain"
    );
    eprintln!(
        "oldest-revision read: peak RSS {:.1} MB over a {:.1} MB history",
        peak as f64 / (1 << 20) as f64,
        corpus as f64 / (1 << 20) as f64,
    );
}
