//! Title-lookup micro-bench on a ~100k-title synthetic store — the
//! before/after numbers for wiring the title dictionary. `#[ignore]`d:
//! run explicitly, in release, when you want the measurement:
//!
//!     cargo test -p wikimak-wikipedia --release --test title_bench -- --ignored --nocapture
//!
//! "Before" is measured HONESTLY on the same store: the exact SQL the
//! old read path issued (`title_intervals.normalized_title` full scans
//! — the table has no text index, by design) via a second read-only
//! connection. "After" is the shipping API (`page_id_by_title_at`,
//! `pages`).

mod common;

use std::io::{Cursor, Write};
use std::time::Instant;

use rusqlite::Connection;
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::Instance;

const N_TITLES: u64 = 100_000;
const SHARDS: u32 = 16;

fn build_store(tmp: &TempDir) -> Instance {
    let mut cfg = common::cfg(tmp.path().to_path_buf(), 2 * N_TITLES);
    cfg.title_shard_count = SHARDS;
    let inst = Instance::open(cfg).expect("open");

    let mut doc = Vec::with_capacity(64 << 20);
    doc.write_all(
        br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>benchwiki</dbname><base>http://x/</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
"#,
    )
    .unwrap();
    for i in 1..=N_TITLES {
        write!(
            doc,
            r#"  <page>
    <title>Synthetic Article {i:06} ({})</title><ns>0</ns><id>{i}</id>
    <revision>
      <id>{r}</id><timestamp>2020-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">body {i}</text><sha1>aa</sha1>
    </revision>
  </page>
"#,
            ["History", "Science", "Geography", "Music"][(i % 4) as usize],
            r = i + 1_000_000,
        )
        .unwrap();
    }
    doc.write_all(b"</mediawiki>").unwrap();

    let t = Instant::now();
    let mut stream = new_page_stream(Cursor::new(doc));
    inst.import(&mut stream).expect("import");
    inst.flush().expect("flush");
    println!("[bench] imported {N_TITLES} titles in {:.1?}", t.elapsed());
    inst
}

fn tau() -> i64 {
    chrono::DateTime::parse_from_rfc3339("2020-06-01T00:00:00Z")
        .unwrap()
        .timestamp_micros()
}

/// Median-ish: run `n` times, report (first, mean-of-rest) in µs.
fn time_us<F: FnMut() -> u64>(n: u32, mut f: F) -> (f64, f64) {
    let t0 = Instant::now();
    let mut sink = f();
    let first = t0.elapsed().as_secs_f64() * 1e6;
    let t1 = Instant::now();
    for _ in 1..n {
        sink = sink.wrapping_add(f());
    }
    std::hint::black_box(sink);
    let rest = t1.elapsed().as_secs_f64() * 1e6 / (n - 1).max(1) as f64;
    (first, rest)
}

#[test]
#[ignore = "measurement, not a gate — run with --ignored --nocapture in release"]
fn bench_title_lookup_and_substring_search() {
    let tmp = TempDir::new().unwrap();
    let inst = build_store(&tmp);
    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();

    let probe = |i: u64| format!(
        "Synthetic Article {i:06} ({})",
        ["History", "Science", "Geography", "Music"][(i % 4) as usize]
    );

    // ---- exact title lookup at τ -------------------------------------
    // BEFORE: the old read's exact SQL — a full scan of
    // title_intervals.normalized_title (no text index existed or
    // exists; that was the point of the dictionary).
    let mut k = 0u64;
    let (b_first, b_rest) = time_us(20, || {
        k += 4_999; // different title each run, defeat the page cache
        let key = probe(1 + (k % N_TITLES)).into_bytes();
        conn.query_row(
            "SELECT page_id FROM title_intervals
             WHERE normalized_title = ?1
               AND start_ts <= ?2 AND (end_ts IS NULL OR end_ts > ?2)
             ORDER BY start_ts DESC LIMIT 1",
            rusqlite::params![key, tau()],
            |r| r.get::<_, i64>(0),
        )
        .expect("old-path lookup") as u64
    });
    println!("[bench] exact lookup BEFORE (title_intervals scan): first {b_first:.0}µs, steady {b_rest:.0}µs");

    // AFTER: the shipping dictionary path (cold shard walk first, then
    // cached probes).
    let mut k = 1u64;
    let (a_first, a_rest) = time_us(2_000, || {
        k += 4_999;
        inst.page_id_by_title_at(&probe(1 + (k % N_TITLES)), Some(tau()))
            .expect("lookup")
            .expect("hit")
    });
    println!("[bench] exact lookup AFTER (dictionary): first(cold shard) {a_first:.0}µs, steady {a_rest:.2}µs");

    // ---- substring search --------------------------------------------
    let (sb_first, sb_rest) = time_us(5, || {
        // The old pages() body, verbatim: ordered full scan + Rust-side
        // lossy lowercase filter, stop at limit.
        let mut st = conn
            .prepare_cached(
                "SELECT page_id, normalized_title FROM title_intervals
                 WHERE end_ts IS NULL ORDER BY normalized_title",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?)))
            .unwrap();
        let needle = "article 0999".to_string();
        let mut out = Vec::new();
        for row in rows.flatten() {
            let title = String::from_utf8_lossy(&row.1).into_owned();
            if !title.to_lowercase().contains(&needle) {
                continue;
            }
            out.push((row.0, title));
            if out.len() >= 200 {
                break;
            }
        }
        out.len() as u64
    });
    println!("[bench] substring search BEFORE (sqlite scan+sort): first {sb_first:.0}µs, steady {sb_rest:.0}µs");

    let (sa_first, sa_rest) = time_us(20, || {
        inst.pages(Some("article 0999"), 200).expect("pages").len() as u64
    });
    println!("[bench] substring search AFTER (parallel shard scan): first {sa_first:.0}µs, steady {sa_rest:.0}µs");

    // Sanity: both paths agree on the answer set size.
    let new_hits = inst.pages(Some("article 0999"), 200).unwrap();
    assert_eq!(new_hits.len(), 10, "…0999 0..9 → ten hits");
}
