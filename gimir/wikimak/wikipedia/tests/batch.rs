//! Batch-prepend equivalence (depot SPEC §"Prepend multiple records"):
//! a page import that lands N revisions as chunked batch prepends must
//! read back record-for-record identical to N single-revision imports,
//! sealing included.

mod common;

use std::io::Cursor;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::Instance;

/// One page (id 7), revisions 0..n, each a small edit of the last.
/// `upto` limits how many revisions the export carries.
fn export_xml(n: usize, upto: usize) -> String {
    let mut lines: Vec<String> = (0..40).map(|i| format!("line {i:03} stable")).collect();
    let mut revs = String::new();
    for r in 0..n {
        lines[r % 40] = format!("line edited by r{r}");
        lines.push(format!("appended by r{r}"));
        if r >= upto {
            continue;
        }
        let text = lines.join("\n");
        revs.push_str(&format!(
            r#"    <revision>
      <id>{id}</id>
      <timestamp>2024-01-01T00:{m:02}:{s:02}Z</timestamp>
      <contributor><username>E</username><id>1</id></contributor>
      <comment>r{r}</comment>
      <model>wikitext</model><format>text/x-wiki</format>
      <text bytes="{len}" xml:space="preserve">{text}</text>
    </revision>
"#,
            id = 1000 + r,
            m = (r / 60) % 60,
            s = r % 60,
            len = text.len(),
        ));
    }
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>T</sitename><dbname>t</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Batch</title><ns>0</ns><id>7</id>
{revs}  </page>
</mediawiki>"#
    )
}

fn history(inst: &Instance) -> Vec<(u64, Vec<u8>)> {
    inst.page_history(7)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.meta.rev_id, (e.fetch_text)().unwrap())
        })
        .collect()
}

#[test]
fn batch_import_equals_sequential_imports() {
    const N: usize = 30;
    // Tiny seal threshold so BOTH paths seal repeatedly mid-history.
    let mk = |tmp: &TempDir| {
        let mut cfg = common::cfg(tmp.path().to_path_buf(), 64);
        cfg.f1_seal_threshold_bytes = 2 * 1024;
        Instance::open(cfg).unwrap()
    };

    // Batch: all 30 revisions in one import (one page transaction).
    let tmp_a = TempDir::new().unwrap();
    let a = mk(&tmp_a);
    let mut stream = new_page_stream(Cursor::new(export_xml(N, N).into_bytes()));
    let stats = a.import(&mut stream).unwrap();
    assert_eq!(stats.revisions_new as usize, N);
    a.flush().unwrap();

    // Sequential: 30 imports, each adding exactly one new revision
    // (dedup skips the rest) — the single-record prepend path.
    let tmp_b = TempDir::new().unwrap();
    let b = mk(&tmp_b);
    for upto in 1..=N {
        let mut stream = new_page_stream(Cursor::new(export_xml(N, upto).into_bytes()));
        let stats = b.import(&mut stream).unwrap();
        assert_eq!(stats.revisions_new, 1, "import {upto} should add one revision");
    }
    b.flush().unwrap();

    // Record-for-record identical read-back, newest-first.
    let ha = history(&a);
    let hb = history(&b);
    assert_eq!(ha.len(), N);
    assert_eq!(ha, hb, "batch and sequential stores decode differently");

    // One batch = ONE prepend: sealing is decided BETWEEN prepends,
    // so a single batch import must NOT have split itself into cold
    // frames, however small the threshold.
    let cold = tmp_a.path().join("depot/cold/cold");
    assert_eq!(cold.metadata().map(|m| m.len()).unwrap_or(0), 0,
               "single-batch import sealed mid-batch — batches must never split");

    // The NEXT prepend sees the oversized old accumulator and seals it
    // whole: import one more revision.
    let mut stream = new_page_stream(Cursor::new(export_xml(N + 1, N + 1).into_bytes()));
    let stats = a.import(&mut stream).unwrap();
    assert_eq!(stats.revisions_new, 1);
    a.flush().unwrap();
    assert!(cold.metadata().map(|m| m.len()).unwrap_or(0) > 0,
            "oversized old accumulator not sealed at the next prepend");
    let ha2 = history(&a);
    assert_eq!(ha2.len(), N + 1, "post-seal read-back lost records");
}
