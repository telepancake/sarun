//! Import pipeline. Drains a `PageStream` into the depot + strpool +
//! sqlite under per-page atomic transactions.
//!
//! ## Chain prepend strategy
//!
//! Each revision becomes ONE record; the depot stores zstd frames the
//! wikipedia layer encodes (the depot is byte-opaque):
//!
//!   * f0 = the NEWEST revision's record, standalone zstd.
//!   * f1 = the older records concatenated newest-first, zstd with
//!     refPrefix anchored on f0's RECORD — successive revisions are
//!     ~99% identical, so the accumulator costs ~the delta per
//!     revision. Records are self-delimiting (codec fixed prefix + four
//!     varint-prefixed blobs), so the reader walks the decompressed
//!     payload sequentially.
//!   * When the decompressed accumulator would exceed the instance's
//!     `f1_seal_threshold_bytes`, the old f1 SEALS: its zstd bytes move
//!     verbatim into a cold frame (no re-encode — its anchor, the old
//!     f0 record, becomes the new f1's sole content, exactly the depot
//!     SPEC's invariant) and the new f1 restarts from that one record.
//!
//! This is the design the depot exists for; the previous
//! store-uncompressed scheme (no zstd, no seal) was the sabotage
//! documented in meta/reports/vbf-recovery.md §4.
//!
//! ## Per-page atomicity
//!
//! Per SPEC §"Crash-safety contract":
//!   1. `BEGIN IMMEDIATE` on sqlite.
//!   2. All of the page's new revisions land on the chain as ONE batch
//!      prepend (SPEC §"Prepend multiple records"). The depot index
//!      flip is the depot's commit; if sqlite then rolls back, those
//!      frames are orphaned but unreferenced (sqlite owns the
//!      page-id↔chain-id story).
//!   3. Append the title bytes to the strpool ONCE per (ns, normalized
//!      title) if not already present; record the resulting id.
//!   4. Insert sqlite rows: `revisions_seen`, `title_id_to_page` (if
//!      new title), `page_to_title_id`, `title_intervals` (one row per
//!      stable title), `siteinfo_snapshots` (once per import).
//!   5. Commit sqlite. The commit is the atomic boundary.
//!
//! ## Dedup
//!
//! A revision `(page_id, rev_id)` already present in `revisions_seen`
//! is skipped and counted toward `revisions_deduped`.

use std::io::Read;

use rusqlite::params;
use serde_json::json;
use wikimak_mediawiki::{site_info, verify_rev_sha1, Contributor, Page, PageStream, Revision};

use crate::error::Result;
use crate::instance::{ContributorMeta, ImportStats, Instance, InstanceInner, RevisionMeta};
use crate::revision::{
    encode_revision, FLAG_COMMENT_HIDDEN, FLAG_CONTRIBUTOR_HIDDEN, FLAG_SHA1_MISMATCH,
    FLAG_SUPPRESSED, FLAG_TEXT_HIDDEN,
};

pub(crate) fn do_import<R: Read>(
    instance: &Instance,
    stream: &mut PageStream<R>,
) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let mut siteinfo_captured = false;

    while let Some(page) = stream.next() {
        let page = page?;

        // Capture site_info once (PageStream parses it during the first
        // `next()` call). Best-effort: skipping on missing or insert
        // failure is fine — the table is not query-pinned by tests.
        if !siteinfo_captured {
            if let Some(si) = site_info(stream) {
                // Use a Mutex-guarded conn; capture once.
                let g = instance.inner.lock().expect("instance mutex poisoned");
                capture_siteinfo(&g.conn, si)?;
                siteinfo_captured = true;
            }
        }

        let page_id = page.id as u64;

        // Skip-policy on overflow: page never touches the depot or
        // sqlite. Matches PHASES §"page_id_overflow_errors_before_writes".
        if page_id >= instance.max_chain_id {
            continue;
        }

        import_one_page(instance, page, &mut stats)?;
    }

    Ok(stats)
}

fn import_one_page(instance: &Instance, page: Page, stats: &mut ImportStats) -> Result<()> {
    let page_id = page.id as u64;

    let mut g = instance.inner.lock().expect("instance mutex poisoned");

    // Dirty fence (once per session, durable BEFORE any import write):
    // between here and the next flush, revisions_seen commits may be
    // durable while their depot frames are not — a power loss in that
    // window is what the flag records, and what suspect-mode repairs.
    if !g.dirty_stamped {
        g.conn.execute(
            "INSERT OR REPLACE INTO instance_flags(key, value) VALUES('dirty', 1)",
            [],
        )?;
        g.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        g.dirty_stamped = true;
    }
    // Suspect-mode repair: the previous session died dirty, so this
    // page's revisions_seen rows may reference frames that never became
    // durable. Re-derive the rows from the CHAIN (the depot is the data
    // fence; bookkeeping must never be ahead of it) once per page.
    if instance.suspect && !g.repaired.contains(&page_id) {
        let actual: Vec<i64> = crate::instance::collect_records(&g.depot, page_id)?
            .iter()
            .map(|rec| decode_rev_id(rec))
            .collect::<Result<_>>()?;
        g.conn.execute(
            "DELETE FROM revisions_seen WHERE page_id = ?1",
            params![page_id as i64],
        )?;
        for rev_id in actual {
            g.conn.execute(
                "INSERT OR IGNORE INTO revisions_seen(page_id, rev_id) VALUES(?1, ?2)",
                params![page_id as i64, rev_id],
            )?;
        }
        g.repaired.insert(page_id);
    }
    let g = &*g;

    // Begin the per-page transaction.
    g.conn.execute("BEGIN IMMEDIATE", [])?;
    let outcome = (|| -> Result<bool> {
        // Title bookkeeping. Tests assert ONE title_intervals row per
        // stable-title page (PHASES W6 revisits rename history). For
        // this phase: insert the page's title ONCE on first appearance.
        let ns_i = page.namespace as i64;
        let normalized = page.title.trim().as_bytes().to_vec();
        ensure_title(&g, page_id, ns_i, &normalized, instance.title_shard_count)?;

        // Collect the page's NEW revisions in source order (oldest →
        // newest); skip those already in revisions_seen. Source order
        // isn't strictly timestamp-ordered in the wild, but every test
        // fixture has it so. All N records then land as ONE batch
        // prepend (depot SPEC §"Prepend multiple records") — one f0
        // swap, one f1 re-encode, one seal check per page, not per
        // revision.
        let mut new_this_page = 0u64;
        let mut new_records: Vec<Vec<u8>> = Vec::new();
        for rev in &page.revisions {
            let rev_id = rev.id as u64;
            if revision_seen(&g.conn, page_id, rev_id)? {
                stats.revisions_deduped += 1;
                continue;
            }

            let (meta, text_bytes) = build_revision_record(rev, stats);
            new_records.push(encode_revision(&meta, &text_bytes));

            g.conn.execute(
                "INSERT INTO revisions_seen(page_id, rev_id) VALUES(?1, ?2)",
                params![page_id as i64, rev_id as i64],
            )?;
            new_this_page += 1;
        }
        if !new_records.is_empty() {
            new_records.reverse(); // the chain wants newest-first
            prepend_depot_frames(&g, page_id, &new_records,
                                 instance.f1_seal_threshold_bytes)?;
        }

        stats.revisions_new += new_this_page;
        // Pages counter: bump even when the page was wholly deduped —
        // it WAS observed in the stream. Tests don't pin this case but
        // the "pages" semantic is "pages seen this run".
        stats.pages += 1;
        Ok(true)
    })();

    match outcome {
        Ok(_) => {
            g.conn.execute("COMMIT", [])?;
            Ok(())
        }
        Err(e) => {
            // Rollback sqlite; depot frames already prepended are
            // orphaned (dead bytes), per SPEC's per-page atomicity
            // contract.
            let _ = g.conn.execute("ROLLBACK", []);
            Err(e)
        }
    }
}

/// Insert title pool entry + meta.db rows on first sighting of a
/// (ns, normalized_title) pair. Subsequent calls are no-ops for the
/// pool but DO insert `title_intervals` and `page_to_title_id` if not
/// already present for this page.
fn ensure_title(
    g: &InstanceInner,
    page_id: u64,
    ns: i64,
    normalized: &[u8],
    title_shard_count: u32,
) -> Result<()> {
    // Look up an existing title_id for this (ns, normalized_title).
    let existing: Option<i64> = g
        .conn
        .query_row(
            "SELECT title_id FROM title_id_to_page
             WHERE ns = ?1 AND normalized_title = ?2",
            params![ns, normalized],
            |r| r.get(0),
        )
        .ok();

    let title_id = match existing {
        Some(id) => id as u64,
        None => {
            // Pick a shard: simple modulo on a stable hash. For
            // shard_count=1 (test default) this is always shard 0.
            let shard_id = if title_shard_count == 0 {
                0
            } else {
                (fnv1a(normalized) % title_shard_count as u64) as u32
            };
            let id = g.titles.append(shard_id, normalized)?;
            g.conn.execute(
                "INSERT INTO title_id_to_page(title_id, ns, normalized_title)
                 VALUES(?1, ?2, ?3)",
                params![id as i64, ns, normalized],
            )?;
            id
        }
    };

    // Idempotent inserts for the page→title side.
    g.conn.execute(
        "INSERT OR IGNORE INTO page_to_title_id(page_id, title_id)
         VALUES(?1, ?2)",
        params![page_id as i64, title_id as i64],
    )?;

    // Title-intervals row: one per (page_id, start_ts). For this phase
    // we use start_ts = 0 and end_ts NULL — a stable title yields one
    // open-ended interval. INSERT OR IGNORE makes re-import a no-op.
    g.conn.execute(
        "INSERT OR IGNORE INTO title_intervals
            (page_id, ns, normalized_title, start_ts, end_ts)
         VALUES(?1, ?2, ?3, 0, NULL)",
        params![page_id as i64, ns, normalized],
    )?;
    Ok(())
}

fn revision_seen(conn: &rusqlite::Connection, page_id: u64, rev_id: u64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM revisions_seen WHERE page_id = ?1 AND rev_id = ?2",
        params![page_id as i64, rev_id as i64],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Build the RevisionMeta + raw text bytes for one mediawiki Revision.
/// Updates `stats.sha1_*` counters as a side effect. Sets the
/// SHA1_MISMATCH flag when the stored sha1 cannot be matched to the
/// text by any newline-fudge variant.
fn build_revision_record(rev: &Revision, stats: &mut ImportStats) -> (RevisionMeta, Vec<u8>) {
    let mut flags: u32 = 0;
    if rev.text_hidden {
        flags |= FLAG_TEXT_HIDDEN;
    }
    if rev.comment_hidden {
        flags |= FLAG_COMMENT_HIDDEN;
    }
    if rev.contributor_hidden {
        flags |= FLAG_CONTRIBUTOR_HIDDEN;
    }
    if rev.suppressed {
        flags |= FLAG_SUPPRESSED;
    }

    // SHA1 counters. We can only verify if we actually have the text.
    if !rev.text_hidden && !rev.sha1.is_empty() {
        let (matched, _normalized, tried) = verify_rev_sha1(&rev.text, &rev.sha1);
        if matched {
            if tried.is_empty() {
                stats.sha1_ok += 1;
            } else {
                stats.sha1_fudged += 1;
            }
        } else {
            stats.sha1_mismatch += 1;
            flags |= FLAG_SHA1_MISMATCH;
        }
    }

    let contributor = match &rev.contributor {
        Contributor::Anonymous { ip } => ContributorMeta::Anonymous { ip: ip.clone() },
        Contributor::Named { username, user_id } => ContributorMeta::Named {
            username: username.clone(),
            user_id: *user_id as u64,
        },
        Contributor::Hidden => ContributorMeta::Hidden,
    };

    let text_bytes: Vec<u8> = if rev.text_hidden {
        Vec::new()
    } else {
        rev.text.as_bytes().to_vec()
    };
    let text_len = text_bytes.len() as u64;

    let meta = RevisionMeta {
        rev_id: rev.id as u64,
        parent_id: rev.parent_id.unwrap_or(0) as u64,
        ts: rev.timestamp,
        contributor,
        comment: rev.comment.clone(),
        sha1: rev.sha1.clone(),
        flags,
        text_len,
    };
    (meta, text_bytes)
}

/// Prepend one or more revision records (NEWEST-first) to the depot
/// chain for `chain_id` as ONE prepend — the normative multi-record
/// composition (depot SPEC §"Prepend multiple records", exposed as
/// `wikimak_depot::compose_f1`). Revision records stand alone, so the
/// old head demotes into the accumulator verbatim. See the module doc
/// for the f0/f1/seal strategy.
pub(crate) fn prepend_depot_frames(
    g: &InstanceInner,
    chain_id: u64,
    records_newest_first: &[Vec<u8>],
    seal_threshold: u64,
) -> Result<()> {
    // NEVER chunk at the seal threshold: one batch = one prepend =
    // one f1 re-encode regardless of size; splitting only churns dead
    // head frames. Sealing is decided BETWEEN prepends against the OLD
    // accumulator (compose_f1). Chunking survives solely as a RAM
    // bound for pathological batches.
    const INGEST_RAM_BOUND: u64 = 256 << 20;
    let sizes: Vec<usize> = records_newest_first.iter().map(|r| r.len()).collect();
    let chunks = wikimak_depot::chunk_newest_first(&sizes, INGEST_RAM_BOUND.max(seal_threshold));
    if chunks.len() > 1 {
        for range in chunks {
            prepend_depot_frames(g, chain_id, &records_newest_first[range], seal_threshold)?;
        }
        return Ok(());
    }
    // Is this the first prepend on the chain?
    let prev_f0 = match g.depot.read_f0(chain_id) {
        Ok(b) => Some(b),
        Err(wikimak_depot::Error::NoFrame) => None,
        Err(e) => return Err(e.into()),
    };

    let (head, older, prev_record) = match prev_f0 {
        Some(frame) => (
            &records_newest_first[0],
            &records_newest_first[1..],
            crate::frames::decompress(&frame, None)?,
        ),
        None => {
            // Empty chain: seed with the OLDEST record (the depot
            // forbids f1 on a chain's first prepend), then absorb the
            // rest as one batch.
            let (seed, rest) = records_newest_first.split_last().expect("non-empty batch");
            g.depot
                .prepend(chain_id, &crate::frames::compress(seed, None)?, None, false)?;
            if rest.is_empty() {
                return Ok(());
            }
            (&rest[0], &rest[1..], seed.clone())
        }
    };
    let old_f1_raw = match g.depot.read_f1(chain_id)? {
        Some(f1_frame) => crate::frames::decompress(&f1_frame, Some(&prev_record))?,
        None => Vec::new(),
    };
    // Accumulator entries newest-first: the older new records, then the
    // demoted old head (verbatim — its zstd f0 frame is orphaned).
    let mut entries: Vec<&[u8]> = older.iter().map(|r| r.as_slice()).collect();
    entries.push(&prev_record);
    let (new_f1_raw, seal) = wikimak_depot::compose_f1(
        &entries,
        if old_f1_raw.is_empty() { None } else { Some(&old_f1_raw) },
        seal_threshold,
    );
    let new_f0 = crate::frames::compress(head, None)?;
    let new_f1 = crate::frames::compress(&new_f1_raw, Some(head))?;
    g.depot.prepend(chain_id, &new_f0, Some(&new_f1), seal)?;
    Ok(())
}

/// The rev id of one encoded revision record (suspect-mode repair).
fn decode_rev_id(rec: &[u8]) -> Result<i64> {
    let (meta, _text) = crate::revision::decode_revision(rec)?;
    Ok(meta.rev_id as i64)
}

fn capture_siteinfo(conn: &rusqlite::Connection, si: &wikimak_mediawiki::SiteInfo) -> Result<()> {
    let captured_at = chrono::Utc::now().timestamp_micros();
    let payload = json!({
        "site_name": si.site_name,
        "db_name": si.db_name,
        "base": si.base,
        "generator": si.generator,
        "case": si.case,
    });
    // serde_json::to_vec on a flat object of String fields cannot fail
    // (no custom Serialize, no non-UTF-8 keys); unwrap is fine.
    let bytes = serde_json::to_vec(&payload).expect("siteinfo json");
    // PRIMARY KEY on captured_at; OR IGNORE so a re-import doesn't
    // collide on the rare same-microsecond reopen.
    conn.execute(
        "INSERT OR IGNORE INTO siteinfo_snapshots(captured_at, json) VALUES(?1, ?2)",
        params![captured_at, bytes],
    )?;
    Ok(())
}

/// FNV-1a 64-bit. Used solely to pick a strpool shard deterministically
/// from the normalized title bytes — never persisted, never read back.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
