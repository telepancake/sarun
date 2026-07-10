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
//! A FRESH page (empty chain — the bulk-import common case) skips the
//! prepend cycle entirely: dumps arrive oldest-first, and the depot's
//! forward construction (`ChainBuilder`, SPEC §"Bulk forward
//! construction") writes each RAM-bound batch as one cold frame, once
//! (batch boundary = frame boundary; the batch's newest record is the
//! frame's refPrefix anchor and carries into the next batch), with the
//! tail landing as f0/f1 at the commit. History write amplification:
//! 1.0. Update mode (existing chain) keeps the prepend path.
//!
//! ## Per-page atomicity — and the RAM bound
//!
//! Per SPEC §"Crash-safety contract":
//!   1. `BEGIN IMMEDIATE` on sqlite.
//!   2. The page's revisions STREAM off the parser one at a time (a
//!      hot full-history page must never be resident whole); each new
//!      record is encoded as it arrives and lands on the chain in
//!      batch prepends (SPEC §"Prepend multiple records") bounded by
//!      the ingest RAM bound — one prepend for any page under the
//!      bound. The depot index flip is the depot's commit; if sqlite
//!      then rolls back, those frames are orphaned but unreferenced
//!      (sqlite owns the page-id↔chain-id story) — the dirty flag is
//!      already durable, so the next session's suspect-mode repair
//!      re-derives `revisions_seen` from the chain.
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
use wikimak_mediawiki::{
    verify_rev_sha1, Contributor, PageHeader, PageStream, Revision, RevisionStream,
};

use crate::error::Result;
use crate::instance::{ContributorMeta, ImportStats, Instance, InstanceInner, RevisionMeta};
use crate::revision::{
    encode_revision, FLAG_COMMENT_HIDDEN, FLAG_CONTRIBUTOR_HIDDEN, FLAG_SHA1_MISMATCH,
    FLAG_SUPPRESSED, FLAG_TEXT_HIDDEN,
};

/// RAM bound for the per-page ingest batch: the encoded revision
/// records resident between depot prepends. This bounds the
/// COLLECTION, not just the prepend — revisions are encoded as they
/// stream off the parser (one `Revision` resident at a time) and
/// flushed to the chain in bounded batches, oldest batch first, so a
/// full-history page of any size imports in ~this much memory.
/// Test-overridable via `WIKIMAK_TEST_INGEST_RAM` (bytes), like the
/// `GITDEPOT_TEST_*` knobs.
const INGEST_RAM_BOUND: u64 = 256 << 20;

fn ingest_ram_bound() -> u64 {
    std::env::var("WIKIMAK_TEST_INGEST_RAM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(INGEST_RAM_BOUND)
}

/// Test knob: route FRESH chains through the update prepend path
/// instead of forward construction, so tests can build both stores
/// from one dump and compare (forward_build.rs equivalence +
/// write-amplification). Never set outside tests.
fn force_prepend() -> bool {
    std::env::var("WIKIMAK_TEST_FORCE_PREPEND").is_ok_and(|v| v == "1")
}

/// Test knob: `std::process::abort()` once a forward build has
/// appended this many history frames — the crash-mid-construction
/// fixture (forward_build.rs). Never set outside tests.
fn abort_after_history_frames() -> Option<u64> {
    std::env::var("WIKIMAK_TEST_ABORT_AFTER_COLD_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
}

pub(crate) fn do_import<R: Read>(
    instance: &Instance,
    stream: &mut PageStream<R>,
) -> Result<ImportStats> {
    // Consume via the streaming core: pages yield a header, then
    // revisions ONE AT A TIME — a hot full-history page (~10^6
    // revisions, ~10^11 text bytes) must never be resident whole.
    let stream = stream.revisions_mut();
    let mut stats = ImportStats::default();
    let mut siteinfo_captured = false;

    while let Some(header) = stream.next_page() {
        let header = header?;

        let page_id = header.id as u64;

        // Reject-policy on overflow (PHASES §"page_id_overflow_errors_
        // before_writes"): a page id at/above the depot's 2^40 sanity
        // ceiling is a LOUD import error BEFORE any write for that
        // page — checked even before the once-per-import siteinfo
        // capture so a first-page overflow leaves meta.db untouched.
        // Ids below the ceiling never overflow: the depot's index
        // auto-grows (sparse) to cover them, so `--max-page-id` is only
        // a fresh-index size hint. Silently skipping instead would let
        // the part watermark land over a lossy import. Pages already
        // committed this run stay (per-page atomicity); the run fails,
        // so no part is ever marked seen.
        if page_id >= wikimak_depot::CHAIN_ID_CEILING {
            return Err(crate::error::Error::PageIdOverflow {
                page_id,
                ceiling: wikimak_depot::CHAIN_ID_CEILING,
            });
        }

        // Capture site_info once (parsed during the first `next_page()`
        // call). Best-effort: skipping on missing or insert failure is
        // fine — the table is not query-pinned by tests.
        if !siteinfo_captured {
            if let Some(si) = stream.site_info() {
                // Use a Mutex-guarded conn; capture once.
                let g = instance.inner.lock().expect("instance mutex poisoned");
                capture_siteinfo(&g.conn, si)?;
                siteinfo_captured = true;
            }
        }

        import_one_page(instance, &header, stream, &mut stats)?;
    }

    Ok(stats)
}

fn import_one_page<R: Read>(
    instance: &Instance,
    header: &PageHeader,
    stream: &mut RevisionStream<R>,
    stats: &mut ImportStats,
) -> Result<()> {
    let page_id = header.id as u64;

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
    // Streaming: the chain is walked one frame at a time, each record
    // peeked for (rev_id, ts) and inserted as it goes by — a hot page's
    // decompressed history is never resident during repair either.
    if instance.suspect && !g.repaired.contains(&page_id) {
        let inner = &mut *g;
        inner.conn.execute(
            "DELETE FROM revisions_seen WHERE page_id = ?1",
            params![page_id as i64],
        )?;
        let mut insert = inner.conn.prepare_cached(
            "INSERT OR IGNORE INTO revisions_seen(page_id, rev_id, ts) VALUES(?1, ?2, ?3)",
        )?;
        let mut walk = crate::instance::WalkState::new(page_id);
        while let Some(rec) = walk.next_record(&inner.depot)? {
            insert.execute(params![
                page_id as i64,
                crate::revision::peek_rev_id(rec)? as i64,
                crate::revision::peek_ts(rec)?,
            ])?;
        }
        drop(insert);
        g.repaired.insert(page_id);
    }
    let mut guard = g;
    let g = &*guard;

    // Begin the per-page transaction.
    g.conn.execute("BEGIN IMMEDIATE", [])?;
    let outcome = (|| -> Result<bool> {
        // Stream the page's revisions in source order (oldest →
        // newest, one resident at a time); skip those already in
        // revisions_seen. Source order isn't strictly
        // timestamp-ordered in the wild, but every test fixture has it
        // so. New records land on the chain in batches bounded by the
        // ingest RAM bound, down one of two paths:
        //
        //   * FRESH chain (the bulk-import common case): forward
        //     construction (depot SPEC §"Bulk forward construction").
        //     Dumps are oldest-first and the chain format's links
        //     point newer→older, so each full batch becomes ONE cold
        //     frame written ONCE — batch boundary = frame boundary,
        //     the batch's newest record excluded and carried as the
        //     next frame's oldest record (it is the frame's refPrefix
        //     anchor, exactly what the newest-first read walk decodes
        //     against). The final partial batch (plus the carry)
        //     lands as f0/f1 at `finish_chain`, whose index flip is
        //     the commit — until then everything is an invisible
        //     orphan. Write amplification for history: 1.0.
        //
        //   * EXISTING chain (update mode): the prepend path,
        //     untouched — one prepend per bound-sized batch (depot
        //     SPEC §"Prepend multiple records": one f0 swap, one f1
        //     re-encode, one seal check), oldest batch first, exactly
        //     the partition `wikimak_depot::chunk_newest_first`
        //     computes.
        //
        // On a mid-page error the sqlite transaction rolls back but
        // frames already on the chain (prepend path) stay — the same
        // chain-ahead-of-bookkeeping state a crash leaves, healed by
        // the same machinery: the dirty flag is already stamped, so
        // the next session opens suspect and re-derives revisions_seen
        // from the chain before trusting it. On the forward path the
        // unfinished build stays invisible (index never flipped) and a
        // re-import simply builds again; the orphan cold bytes die
        // with the instance.
        let batch_bound = ingest_ram_bound().max(instance.f1_seal_threshold_bytes);
        let mut batch: Vec<Vec<u8>> = Vec::new(); // oldest-first
        let mut batch_bytes: u64 = 0;
        let mut new_this_page = 0u64;
        // Fresh-vs-update fork (one index peek). The test knob forces
        // the prepend path so suites can build both stores and compare.
        let mut builder = if !force_prepend() && !g.depot.has_chain(page_id)? {
            Some(g.depot.begin_chain(page_id)?)
        } else {
            None
        };
        // Forward path: the previous batch's newest record, excluded
        // from its frame — the anchor it was compressed against, and
        // the oldest record of the NEXT frame (or of the f0/f1 head).
        let mut carry: Option<Vec<u8>> = None;

        // Earliest revision timestamp for THIS dump's copy of the page
        // — the real start of a title interval (browsing plan §2
        // wayback contract). Over ALL revisions in hand (not just the
        // new ones) so an idempotent reimport recomputes the SAME
        // value. `None` (no revisions) leaves the interval logic a
        // no-op.
        let mut earliest_ts: Option<i64> = None;
        // Does this dump carry the page FORWARD — is its newest
        // revision (by timestamp; `>=` so ties resolve to the LAST
        // maximal, matching the old `max_by_key` scan) one we had not
        // already stored? Each revision's `seen` is checked before its
        // own insert, so this matches the old pre-scan against the
        // pre-import state.
        let mut newest: Option<(i64, bool)> = None;

        while let Some(rev) = stream.next_revision() {
            let rev = rev?;
            let rev_id = rev.id as u64;
            let ts = rev.timestamp.timestamp_micros();
            earliest_ts = Some(earliest_ts.map_or(ts, |e| e.min(ts)));

            let seen = revision_seen(&g.conn, page_id, rev_id)?;
            if newest.is_none_or(|(m, _)| ts >= m) {
                newest = Some((ts, !seen));
            }
            if seen {
                stats.revisions_deduped += 1;
                continue;
            }

            let record = encode_new_revision(rev, stats);
            // `ts` rides along so reads resolve "newest revision ≤ τ"
            // in sqlite instead of decoding the chain (instance.rs
            // `revision_query`).
            g.conn
                .prepare_cached(
                    "INSERT INTO revisions_seen(page_id, rev_id, ts) VALUES(?1, ?2, ?3)",
                )?
                .execute(params![page_id as i64, rev_id as i64, ts])?;
            new_this_page += 1;

            // Flush BEFORE the record that would overflow the bound
            // (a single oversized record still travels alone) — the
            // same greedy oldest-first partition as chunk_newest_first.
            if !batch.is_empty() && batch_bytes + record.len() as u64 > batch_bound {
                match builder.as_mut() {
                    Some(b) => forward_flush(g, b, &mut carry, &mut batch)?,
                    None => {
                        batch.reverse(); // the chain wants newest-first
                        prepend_depot_frames(g, page_id, &batch, instance.f1_seal_threshold_bytes)?;
                        batch.clear();
                    }
                }
                batch_bytes = 0;
            }
            batch_bytes += record.len() as u64;
            batch.push(record);
        }
        match builder.take() {
            Some(b) => forward_finish(g, b, carry, batch)?,
            None => {
                if !batch.is_empty() {
                    batch.reverse(); // the chain wants newest-first
                    prepend_depot_frames(g, page_id, &batch, instance.f1_seal_threshold_bytes)?;
                }
            }
        }

        // Title bookkeeping: title pool + reverse index, and the
        // rename-aware title-interval bookkeeping (a moved page closes
        // its open interval and opens a new one — browsing plan §2).
        // After the revision loop (its inputs are streamed aggregates)
        // but inside the same transaction — the commit stays atomic.
        let dump_extends_head = newest.is_some_and(|(_, head_is_new)| head_is_new);
        ensure_title(
            g,
            page_id,
            header.namespace as i64,
            header.title.trim().as_bytes(),
            instance.title_shard_count,
            earliest_ts,
            dump_extends_head,
        )?;

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
            // contract. The chain is now AHEAD of the rows for this
            // page — flag it so reads this session distrust the rows
            // and scan the chain (the dirty flag already routes the
            // NEXT session through suspect-mode repair).
            let _ = g.conn.execute("ROLLBACK", []);
            guard.import_errored = true;
            Err(e)
        }
    }
}

/// Insert title pool entry + meta.db rows for a `(ns, normalized_title)`
/// pair, and maintain the page's `title_intervals` (the wayback title
/// time-travel index, browsing plan §2).
///
/// Interval discipline (`earliest_ts` = the earliest revision timestamp of
/// this dump's copy of the page). The dump only ever states a page's
/// CURRENT title; the rename INSTANT is not in the XML export (that lives
/// in the `mediawiki_history` TSV, import plan §2.4 / W5 — plan §2 "title
/// history is approximate"). So this bookkeeping does what a dump can
/// support and no more:
///
///   * First sighting of the page → open ONE interval `[earliest_ts, ∞)`
///     (real start, NOT 0 — so `exists_at` is false before the page's first
///     revision).
///   * SAME title still open → idempotent, EXCEPT a later dump that
///     backfills an EARLIER revision (a full-history dump split across parts
///     imported out of order) lowers the interval's start to `earliest_ts`,
///     keeping `exists_at` honest.
///   * DIFFERENT title whose earliest revision is strictly LATER than the
///     open interval's start → an INCREMENTAL move: an adds-changes dump
///     (W6) carries only the post-move revisions, so `earliest_ts` IS the
///     handoff. CLOSE the old interval there and OPEN the new one — the one
///     rename shape a dump can date. Old title stops resolving at the move.
///   * DIFFERENT title, earliest NOT later → a FULL-HISTORY re-export under
///     the page's current (post-move) title: it re-lists every revision from
///     the first, so the move cannot be dated. Adopt the new title as
///     authoritative by RETITLING the open interval in place (single-valued;
///     the prior title keeps no interval and stops resolving at τ). Real
///     per-instant rename history awaits the TSV.
fn ensure_title(
    g: &InstanceInner,
    page_id: u64,
    ns: i64,
    normalized: &[u8],
    title_shard_count: u32,
    earliest_ts: Option<i64>,
    dump_extends_head: bool,
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

    // A page with no revisions in this dump has no anchor for an interval;
    // leave the interval table untouched.
    let Some(start) = earliest_ts else {
        return Ok(());
    };

    // The page's current OPEN interval (end_ts IS NULL), if any. A legacy
    // start_ts=0 row is also open and matches here — its title is compared
    // like any other, so a rename off a pre-interval import still works.
    let open: Option<(i64, Vec<u8>)> = g
        .conn
        .query_row(
            "SELECT start_ts, normalized_title FROM title_intervals
             WHERE page_id = ?1 AND end_ts IS NULL
             ORDER BY start_ts DESC LIMIT 1",
            params![page_id as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    match open {
        None => {
            // First interval for this page. Real start = earliest revision.
            // INSERT OR IGNORE guards the (page_id, start_ts) PK against a
            // same-instant re-run.
            g.conn.execute(
                "INSERT OR IGNORE INTO title_intervals
                    (page_id, ns, normalized_title, start_ts, end_ts)
                 VALUES(?1, ?2, ?3, ?4, NULL)",
                params![page_id as i64, ns, normalized, start],
            )?;
        }
        Some((open_start, open_title)) => {
            if open_title == normalized {
                // Same title. Backfill only: a later dump may supply an
                // EARLIER revision (history split across parts, imported out
                // of order) — lower the start so exists_at stays correct.
                // Otherwise a true no-op (idempotent reimport).
                if start < open_start {
                    g.conn.execute(
                        "UPDATE title_intervals SET start_ts = ?1
                         WHERE page_id = ?2 AND start_ts = ?3 AND end_ts IS NULL",
                        params![start, page_id as i64, open_start],
                    )?;
                }
            } else if !dump_extends_head {
                // A DIFFERENT title but this dump adds no new head — a
                // re-run of an older slice (crash-resume, idempotent
                // reimport) under a title the page has already moved past.
                // Leave every interval alone.
            } else if start > open_start {
                // Incremental move (adds-changes / W6): a fresh dump whose
                // revisions begin strictly after the open interval, so
                // earliest_ts IS the handoff. Close the old interval there
                // and open the new one — the one datable rename shape.
                g.conn.execute(
                    "UPDATE title_intervals SET end_ts = ?1
                     WHERE page_id = ?2 AND start_ts = ?3 AND end_ts IS NULL",
                    params![start, page_id as i64, open_start],
                )?;
                g.conn.execute(
                    "INSERT OR IGNORE INTO title_intervals
                        (page_id, ns, normalized_title, start_ts, end_ts)
                     VALUES(?1, ?2, ?3, ?4, NULL)",
                    params![page_id as i64, ns, normalized, start],
                )?;
            } else {
                // A fresh full-history re-export under a new (post-move)
                // title: it re-lists every revision from the first, so the
                // move instant is not in the dump. Adopt the new title as
                // authoritative — retitle the open interval in place, keeping
                // it single-valued (the prior title then has no interval and
                // stops resolving at τ). Backfill the start too.
                let new_start = start.min(open_start);
                g.conn.execute(
                    "UPDATE title_intervals
                        SET normalized_title = ?1, ns = ?2, start_ts = ?3
                     WHERE page_id = ?4 AND start_ts = ?5 AND end_ts IS NULL",
                    params![normalized, ns, new_start, page_id as i64, open_start],
                )?;
            }
        }
    }
    Ok(())
}

fn revision_seen(conn: &rusqlite::Connection, page_id: u64, rev_id: u64) -> Result<bool> {
    // prepare_cached: this runs once per revision of every page — a
    // fresh prepare per call was measurable parse overhead at scale.
    let n: i64 = conn
        .prepare_cached("SELECT COUNT(*) FROM revisions_seen WHERE page_id = ?1 AND rev_id = ?2")?
        .query_row(params![page_id as i64, rev_id as i64], |r| r.get(0))?;
    Ok(n > 0)
}

/// Encode one NEW mediawiki Revision into its depot record. Consumes
/// the revision: the meta strings (contributor, comment, sha1) MOVE
/// into the codec input and the text is passed as a slice — no clone
/// and no full-text copy besides the one into the record itself.
/// Updates `stats.sha1_*` counters as a side effect; sets the
/// SHA1_MISMATCH flag when the stored sha1 cannot be matched to the
/// text by any newline-fudge variant.
fn encode_new_revision(rev: Revision, stats: &mut ImportStats) -> Vec<u8> {
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

    let contributor = match rev.contributor {
        Contributor::Anonymous { ip } => ContributorMeta::Anonymous { ip },
        Contributor::Named { username, user_id } => ContributorMeta::Named {
            username,
            user_id: user_id as u64,
        },
        Contributor::Hidden => ContributorMeta::Hidden,
    };

    let text: &[u8] = if rev.text_hidden {
        &[]
    } else {
        rev.text.as_bytes()
    };

    let meta = RevisionMeta {
        rev_id: rev.id as u64,
        parent_id: rev.parent_id.unwrap_or(0) as u64,
        ts: rev.timestamp,
        contributor,
        comment: rev.comment,
        sha1: rev.sha1,
        flags,
        text_len: text.len() as u64,
    };
    encode_revision(&meta, text)
}

/// Forward-construction batch flush (fresh chains only): turn the full
/// oldest-first `batch` into ONE cold frame written ONCE. The batch's
/// NEWEST record is excluded — it is the frame's refPrefix anchor and
/// becomes the next frame's oldest record (`carry`), reproducing the
/// read walk's invariant (each cold frame decodes against the oldest
/// record of the next-newer frame) in dump order. The frame holds, in
/// newest-first record order: the batch minus its newest, then the
/// incoming carry. A wrong anchor here would fail the read-back's zstd
/// decode loudly — the equivalence test's real teeth.
fn forward_flush(
    g: &InstanceInner,
    b: &mut wikimak_depot::ChainBuilder,
    carry: &mut Option<Vec<u8>>,
    batch: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let newest = batch.pop().expect("forward_flush wants a non-empty batch");
    // A single-record batch with no carry has nothing to frame: the
    // record just becomes the carry (a lone oversized record travels
    // to the NEXT frame as its oldest entry, or into the head).
    if !batch.is_empty() || carry.is_some() {
        let mut raw =
            Vec::with_capacity(batch.iter().map(Vec::len).sum::<usize>()
                + carry.as_ref().map_or(0, |c| c.len()));
        // Newest-first; each drained record is freed as it is copied.
        for rec in batch.drain(..).rev() {
            raw.extend_from_slice(&rec);
        }
        if let Some(c) = carry.take() {
            raw.extend_from_slice(&c);
        }
        let zstd = crate::frames::compress(&raw, Some(&newest))?;
        g.depot.append_history_frame(b, &zstd)?;
        if abort_after_history_frames().is_some_and(|n| b.frames_written() >= n) {
            // Crash-mid-construction test knob: die BETWEEN frames,
            // before the index flip — the build must stay invisible.
            std::process::abort();
        }
    }
    *carry = Some(newest);
    Ok(())
}

/// Forward-construction commit: the final partial batch plus the carry
/// are the chain HEAD — f0 = the newest record standalone, f1 = the
/// rest (newest-first, refPrefix-anchored on f0's record, its oldest
/// entry being the carry that anchors the newest cold frame). The
/// depot's `finish_chain` index flip is the atomic commit. No new
/// records at all ⇒ nothing was ever written; the builder just drops.
fn forward_finish(
    g: &InstanceInner,
    b: wikimak_depot::ChainBuilder,
    carry: Option<Vec<u8>>,
    mut batch: Vec<Vec<u8>>,
) -> Result<()> {
    batch.reverse(); // newest-first
    if let Some(c) = carry {
        batch.push(c);
    }
    let Some((head, older)) = batch.split_first() else {
        return Ok(());
    };
    let f0 = crate::frames::compress(head, None)?;
    let f1 = if older.is_empty() {
        None
    } else {
        let mut raw = Vec::with_capacity(older.iter().map(Vec::len).sum());
        for rec in older {
            raw.extend_from_slice(rec);
        }
        Some(crate::frames::compress(&raw, Some(head))?)
    };
    g.depot.finish_chain(b, &f0, f1.as_deref())?;
    Ok(())
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
    // bound for pathological batches — the streaming import loop
    // already flushes at this same bound (same greedy oldest-first
    // partition), so for import this is a no-op invariant guard.
    let sizes: Vec<usize> = records_newest_first.iter().map(|r| r.len()).collect();
    let chunks = wikimak_depot::chunk_newest_first(&sizes, ingest_ram_bound().max(seal_threshold));
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

fn capture_siteinfo(conn: &rusqlite::Connection, si: &wikimak_mediawiki::SiteInfo) -> Result<()> {
    let captured_at = chrono::Utc::now().timestamp_micros();
    // Per-namespace JSON (browsing plan §2 / §7 siteinfo). Keys are
    // ADDITIVE: the asof read API tolerates snapshots written before a key
    // existed. The dump's `<namespace>` gives one localized name + the
    // key; we record it as `localized` and fill `canonical` from the fixed
    // MediaWiki canonical-namespace map (real, not fabricated — the CANON
    // is a name, and the only ALIAS derived downstream is the dump's own
    // localized name). `aliases` stays empty because the export header
    // carries none (namespacealiases live only in the API's siteinfo).
    let namespaces: Vec<_> = si
        .namespaces
        .values()
        .map(|n| {
            let canonical = canonical_namespace_name(n.id).unwrap_or(n.name.as_str());
            json!({
                "id": n.id,
                "canonical": canonical,
                "localized": n.name,
                "case": n.case,
                "aliases": n.aliases,
            })
        })
        .collect();
    let payload = json!({
        "site_name": si.site_name,
        "db_name": si.db_name,
        "base": si.base,
        "generator": si.generator,
        "case": si.case,
        "namespaces": namespaces,
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
    // Interwiki map for this snapshot. Export dumps carry none, so this is
    // normally a no-op and asof falls back to the built-in seed; when a
    // richer source (API/sitematrix) fills `si.interwiki`, its prefixes
    // persist here keyed to the same `captured_at`. `is_local` is written
    // FALSE unconditionally: MediaWiki's own same-farm `local` flag is a
    // different notion from "mirrored by us", and we mirror nothing here
    // (never a local link for a foreign wiki — import plan §3 constraint).
    for iw in &si.interwiki {
        if iw.prefix.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO interwiki_map(captured_at, prefix, url, is_local)
             VALUES(?1, ?2, ?3, 0)",
            params![captured_at, iw.prefix, iw.url],
        )?;
    }
    Ok(())
}

/// Canonical (content-language-independent) MediaWiki name for a core
/// namespace id, or `None` for a wiki-specific / extension namespace. These
/// are fixed built-ins (Manual:Namespace), the same set every MediaWiki
/// accepts as an English prefix regardless of content language — so a
/// title's localized prefix AND its canonical prefix both resolve.
fn canonical_namespace_name(id: i32) -> Option<&'static str> {
    Some(match id {
        -2 => "Media",
        -1 => "Special",
        0 => "",
        1 => "Talk",
        2 => "User",
        3 => "User talk",
        4 => "Project",
        5 => "Project talk",
        6 => "File",
        7 => "File talk",
        8 => "MediaWiki",
        9 => "MediaWiki talk",
        10 => "Template",
        11 => "Template talk",
        12 => "Help",
        13 => "Help talk",
        14 => "Category",
        15 => "Category talk",
        _ => return None,
    })
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
