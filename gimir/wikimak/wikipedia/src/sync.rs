//! Discover → fetch → import loop (MIRRORS.md phase 1).
//!
//! `sync` pulls the newest complete dump run for a dbname and imports
//! every part not already recorded in `parts_seen`, streaming: HTTP body
//! → checksum verifier → bz2 decoder → page stream → depot. Nothing
//! touches disk except the instance itself.
//!
//! Ordering of the durability handshake per part:
//!   1. import the whole part (per-page atomic inside);
//!   2. `Instance::flush` — pages durable;
//!   3. `mark_part_seen` — only now is the part skippable.
//!
//! A crash between 2 and 3 re-imports the part; `revisions_seen` dedup
//! makes that a cheap no-op, never a correctness problem.

use std::io::Read;

use reqwest::blocking::Client;
use wikimak_mediawiki::{discover_with, fetch, new_page_stream, Config, Run};

use crate::error::Result;
use crate::instance::{ImportStats, Instance};

/// Counters from one [`sync`] pass.
#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    pub parts_total: u64,
    pub parts_fetched: u64,
    pub parts_skipped: u64,
    pub import: ImportStats,
}

fn add_import(into: &mut ImportStats, s: &ImportStats) {
    into.pages += s.pages;
    into.revisions_new += s.revisions_new;
    into.revisions_deduped += s.revisions_deduped;
    into.sha1_ok += s.sha1_ok;
    into.sha1_fudged += s.sha1_fudged;
    into.sha1_mismatch += s.sha1_mismatch;
}

/// Discover the newest complete run for `dbname` and import its unseen
/// parts into `inst`. Returns the run picked and the counters.
///
/// `progress` is called before each part with `(filename, fetched|skipped)`;
/// pass `|_, _| ()` for silence.
pub fn sync(
    inst: &Instance,
    client: &Client,
    cfg: &Config,
    dbname: &str,
    mut progress: impl FnMut(&str, bool),
) -> Result<(Run, SyncStats)> {
    let run = discover_with(client, cfg, dbname)?;
    let mut stats = SyncStats {
        parts_total: run.parts.len() as u64,
        ..Default::default()
    };
    for part in &run.parts {
        if inst.part_seen(&part.filename)? {
            stats.parts_skipped += 1;
            progress(&part.filename, false);
            continue;
        }
        progress(&part.filename, true);
        let reader = fetch(client, part)?;
        let boxed: Box<dyn Read + Send> = if part.filename.ends_with(".bz2") {
            Box::new(wikimak_mediawiki::bz2::new_bz2_reader(
                reader,
                wikimak_mediawiki::bz2::Bz2Options { workers: 0 },
            ))
        } else {
            Box::new(reader)
        };
        let mut stream = new_page_stream(boxed);
        let s = inst.import(&mut stream)?;
        // The parser stops at `</mediawiki>`; drain the source to EOF so
        // the VerifyingReader's on-EOF checksum actually fires. A
        // mismatch surfaces here, BEFORE the watermark below.
        std::io::copy(&mut stream.into_inner(), &mut std::io::sink())
            .map_err(crate::error::Error::Io)?;
        add_import(&mut stats.import, &s);
        inst.flush()?;
        inst.mark_part_seen(&part.filename, part.sha256.as_deref())?;
        stats.parts_fetched += 1;
    }
    // Sync session over: reclaim the churn slack (dead superseded heads)
    // parked in the depot's current write files. Once per sync, not per
    // part — mid-session the slack is what keeps prepends cheap.
    if stats.parts_fetched > 0 {
        inst.collect()?;
    }
    Ok((run, stats))
}
