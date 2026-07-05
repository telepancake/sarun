//! The anti-sabotage suite (DEPOT-DESIGN.md §9, vbf-recovery.md §4): the
//! compression discipline IS the architecture, and it is verified here
//! by MEASURED ON-DISK SIZE against a real multi-revision page — the
//! test the sabotaged encoder never had to face. A green byte-payload
//! suite over an uncompressed store cannot pass this file.

mod common;

use std::io::Cursor;
use std::path::Path;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{Instance, InstanceConfig};

/// Deterministic xorshift for barely-compressible page text.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// A ~40 KiB article body: pseudo-random hex lines — low internal
/// compressibility, so standalone-per-revision storage cannot shrink it
/// and only CROSS-revision refPrefix can.
fn base_text(rng: &mut Rng) -> Vec<String> {
    (0..700)
        .map(|i| format!("line {i:04}: {:016x}{:016x}{:016x}", rng.next(), rng.next(), rng.next()))
        .collect()
}

/// Build a MediaWiki export: ONE page, `n` revisions, each a small edit
/// of the previous (one line replaced, one appended) — the ~99%-identical
/// succession the tiered design exists for.
fn export_xml(n: usize) -> (String, Vec<String>) {
    let mut rng = Rng(0x5eed);
    let mut lines = base_text(&mut rng);
    let mut revs = String::new();
    let mut texts = Vec::new();
    for r in 0..n {
        let at = (rng.next() as usize) % lines.len();
        lines[at] = format!("line {at:04}: EDITED r{r} {:016x}", rng.next());
        lines.push(format!("appended by r{r}"));
        let text = lines.join("\n");
        revs.push_str(&format!(
            r#"    <revision>
      <id>{id}</id>{parent}
      <timestamp>2024-01-01T{h:02}:{m:02}:{s:02}Z</timestamp>
      <contributor><username>E</username><id>1</id></contributor>
      <comment>r{r}</comment>
      <model>wikitext</model><format>text/x-wiki</format>
      <text bytes="{len}" xml:space="preserve">{text}</text>
    </revision>
"#,
            id = 1000 + r,
            parent = if r == 0 { String::new() }
                     else { format!("\n      <parentid>{}</parentid>", 999 + r) },
            h = r / 3600, m = (r / 60) % 60, s = r % 60,
            len = text.len(),
        ));
        texts.push(text);
    }
    let xml = format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>T</sitename><dbname>t</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Big</title><ns>0</ns><id>7</id>
{revs}  </page>
</mediawiki>"#
    );
    (xml, texts)
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            total += if path.is_dir() { dir_size(&path) }
                     else { path.metadata().map(|m| m.len()).unwrap_or(0) };
        }
    }
    total
}

#[test]
fn multi_revision_page_compresses_and_seals() {
    const N: usize = 120;
    let (xml, texts) = export_xml(N);
    let raw_total: u64 = texts.iter().map(|t| t.len() as u64).sum();
    assert!(raw_total > 4 << 20, "fixture should be multi-MB raw");

    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(InstanceConfig {
        root: tmp.path().to_path_buf(),
        dbname: "t".into(),
        max_chain_id: 1024,
        depot: wikimak_depot::DepotConfig {
            root: std::path::PathBuf::new(), // forced to <root>/depot/
            max_chain_id: 1024,
            file_size_threshold: 512 * 1024, // small: exercise file rolls
            eviction_dead_ratio: 0.5,
        },
        title_shard_count: 1,
        title_seal_threshold_bytes: 1 << 20,
        // Small seal threshold: the ~40 KiB spilled head crosses it every
        // few revisions, so COLD FRAMES MUST FORM in this test.
        f1_seal_threshold_bytes: 128 * 1024,
    })
    .unwrap();

    let mut stream = new_page_stream(Cursor::new(xml.into_bytes()));
    let stats = inst.import(&mut stream).unwrap();
    assert_eq!(stats.revisions_new as usize, N);
    inst.flush().unwrap(); // also runs depot eviction

    // ── fidelity: every revision reads back exactly, newest-first ──────
    assert_eq!(inst.page_head_text(7).unwrap().unwrap(),
               texts.last().unwrap().as_bytes());
    let hist: Vec<_> = inst.page_history(7).unwrap().collect();
    assert_eq!(hist.len(), N);
    for (i, entry) in hist.into_iter().enumerate() {
        let e = entry.unwrap();
        let want = &texts[N - 1 - i];
        assert_eq!(e.meta.rev_id as usize, 1000 + (N - 1 - i));
        assert_eq!((e.fetch_text)().unwrap(), want.as_bytes(),
                   "revision {i} newest-first text mismatch");
    }

    // ── sealing actually happened: the cold file holds frames ──────────
    let cold = tmp.path().join("depot").join("cold").join("cold");
    let cold_size = cold.metadata().map(|m| m.len()).unwrap_or(0);
    assert!(cold_size > 0, "no cold frames — sealing never fired");

    // ── THE measurement: on-disk depot ≪ raw input ─────────────────────
    // Live bytes are ~one full head + a bounded accumulator + per-rev
    // deltas; eviction (dead ratio 0.5) bounds orphan slack at ~2x live.
    // The sabotaged scheme stored ~raw_total plus a QUADRATIC f1 — it
    // could not come near this bound.
    let disk = dir_size(&tmp.path().join("depot"));
    eprintln!("raw {raw_total} B, depot on disk {disk} B, cold {cold_size} B \
               ({}x compression)", raw_total / disk.max(1));
    assert!(
        disk * 8 < raw_total,
        "depot on disk ({disk}) not <1/8 of raw input ({raw_total}) — \
         the compression discipline is not rendered"
    );
}
