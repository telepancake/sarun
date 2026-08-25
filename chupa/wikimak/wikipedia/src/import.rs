//! Import pipeline. Drains a `PageStream` into the depot + strpool +
//! sqlite under per-page atomic transactions.
//!
//! ## Chain prepend strategy
//!
//! Each revision becomes ONE record; the depot stores zstd frames the
//! wikipedia layer encodes (the depot is byte-opaque):
//!
//!   * f0 = the NEWEST revision's record, standalone zstd.
//!   * On updates, f1 is the mutable accumulator of older records
//!     concatenated newest-first, zstd with refPrefix anchored on f0's
//!     RECORD — successive revisions are ~99% identical, so the
//!     accumulator costs ~the delta per revision. Fresh imports bypass
//!     f1 and forward-build their history directly into cold.
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
//! A fresh page is collected page-at-a-time, sorted by immutable
//! revision id, and committed as exactly one f0 plus one cold history
//! frame (when history exists), never f1. An existing chain is the
//! dedup authority. Strictly newer ids take the prepend fast path;
//! an interleaved/older addition is streaming-merged with the chain
//! and installed by one atomic replacement index flip.
//!
//! ## Dedup
//!
//! Revision id is the identity key. Identical records deduplicate.
//! Different bytes for an existing id never replace archival content:
//! the incoming complete record is appended to the page's separate
//! correction lane and surfaced in import statistics/read APIs.

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

const TITLE_INTENT_BATCH: usize = 4096;

#[cfg(test)]
struct PrepareGate {
    state: std::sync::Mutex<(usize, bool)>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
static PREPARE_GATE: std::sync::Mutex<Option<std::sync::Arc<PrepareGate>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn wait_at_prepare_gate() {
    let gate = PREPARE_GATE.lock().unwrap().clone();
    let Some(gate) = gate else { return };
    let mut state = gate.state.lock().unwrap();
    state.0 += 1;
    gate.changed.notify_all();
    while !state.1 {
        state = gate.changed.wait(state).unwrap();
    }
}

pub(crate) fn do_import<R: Read>(
    instance: &Instance,
    stream: &mut PageStream<R>,
) -> Result<ImportStats> {
    // The parser yields revisions one at a time, but this canonical merge
    // deliberately collects ONE page before sorting by immutable revision
    // id. Exceptionally huge pages therefore set the import RAM bound.
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
        // auto-grows to cover actual ids. Silently skipping would let
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

    let mut g = instance.inner.lock().expect("instance mutex poisoned");
    crate::instance::finish_title_slot_intent(&instance.root, &mut g)?;
    Ok(stats)
}

fn import_one_page<R: Read>(
    instance: &Instance,
    header: &PageHeader,
    stream: &mut RevisionStream<R>,
    stats: &mut ImportStats,
) -> Result<()> {
    let page_id = header.id as u64;
    let mut incoming = Vec::<Vec<u8>>::new();
    let mut earliest_ts = None;
    while let Some(revision) = stream.next_revision() {
        let revision = revision?;
        let ts = revision.timestamp.timestamp_micros();
        earliest_ts = Some(earliest_ts.map_or(ts, |old: i64| old.min(ts)));
        incoming.push(encode_new_revision(revision, stats));
    }
    import_encoded_page(
        instance,
        page_id,
        Some(&header.title),
        earliest_ts,
        incoming,
        stats,
    )
}

pub(crate) fn import_encoded_page(
    instance: &Instance,
    page_id: u64,
    title: Option<&str>,
    earliest_ts: Option<i64>,
    mut incoming: Vec<Vec<u8>>,
    stats: &mut ImportStats,
) -> Result<()> {
    if page_id >= wikimak_depot::CHAIN_ID_CEILING {
        return Err(crate::error::Error::PageIdOverflow {
            page_id,
            ceiling: wikimak_depot::CHAIN_ID_CEILING,
        });
    }
    incoming.sort_by(|a, b| revision_key(b).cmp(&revision_key(a)));
    let (incoming, conflicts) = dedup_incoming(incoming, stats);
    let likely_fresh = {
        let g = instance.inner.lock().expect("instance mutex poisoned");
        !g.depot.has_chain(page_id)?
    };
    let prepared_fresh = if likely_fresh {
        let dictionaries = crate::frames::DictionaryStore::open_existing(&instance.root);
        prepare_fresh_chain(&incoming, &dictionaries)?
    } else {
        None
    };
    #[cfg(test)]
    wait_at_prepare_gate();

    let mut g = instance.inner.lock().expect("instance mutex poisoned");
    for conflict in conflicts {
        if append_correction(
            &g,
            page_id,
            &conflict,
            instance.f1_seal_threshold_bytes,
        )? {
            stats.revision_conflicts += 1;
        }
    }
    let had_chain = g.depot.has_chain(page_id)?;
    let old_head_key = if had_chain && !incoming.is_empty() {
        let raw = crate::frames::decompress_head(
            &g.depot.read_f0(page_id)?,
            &g.revision_dictionaries,
            "revision",
        )?;
        Some(revision_key(&raw))
    } else {
        None
    };
    let dump_extends_head = incoming
        .first()
        .is_some_and(|record| old_head_key.is_none_or(|old| revision_key(record) > old));

    let (new_this_page, deduped) = if incoming.is_empty() {
        (0, 0)
    } else if !had_chain {
        let prepared = prepared_fresh.ok_or(crate::error::Error::Corrupt(
            "fresh-chain preparation missing",
        ))?;
        install_fresh_chain(&g, page_id, prepared)?;
        (incoming.len() as u64, 0)
    } else if incoming
        .last()
        .is_some_and(|record| revision_key(record) > old_head_key.expect("existing head"))
    {
        prepend_depot_frames(&g, page_id, &incoming, instance.f1_seal_threshold_bytes)?;
        (incoming.len() as u64, 0)
    } else {
        merge_existing_chain(
            &g,
            page_id,
            old_head_key.expect("existing head"),
            &incoming,
            instance.f1_seal_threshold_bytes,
            stats,
        )?
    };

    g.conn.execute("BEGIN IMMEDIATE", [])?;
    let outcome = (|| -> Result<usize> {
        title.map_or(Ok(0), |title| {
            ensure_current_title(
                &mut g,
                page_id,
                title.as_bytes(),
                instance.title_shard_count.load(std::sync::atomic::Ordering::Relaxed),
                earliest_ts,
                dump_extends_head,
            )
        })
    })();
    match outcome {
        Ok(added_intents) => {
            g.conn.execute("COMMIT", [])?;
            g.pending_title_intents += added_intents;
            if g.pending_title_intents >= TITLE_INTENT_BATCH {
                crate::instance::finish_title_slot_intent(&instance.root, &mut g)?;
            }
            stats.revisions_new += new_this_page;
            stats.revisions_deduped += deduped;
            stats.pages += 1;
            if let Some(title) = title {
                let normalized = crate::titles::normalize_title(title.as_bytes());
                let count = instance
                    .title_shard_count
                    .load(std::sync::atomic::Ordering::Relaxed);
                let sid = crate::titles::shard_for(&normalized, count);
                instance.maintain_title_shard(&mut g, sid)?;
            }
            Ok(())
        }
        Err(e) => {
            let _ = g.conn.execute("ROLLBACK", []);
            Err(e)
        }
    }
}

fn revision_key(record: &[u8]) -> u64 {
    crate::revision::peek_rev_id(record).expect("freshly encoded revision record")
}

fn dedup_incoming(
    incoming: Vec<Vec<u8>>,
    stats: &mut ImportStats,
) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut unique: Vec<Vec<u8>> = Vec::with_capacity(incoming.len());
    let mut conflicts = Vec::new();
    for record in incoming {
        if let Some(previous) = unique.last() {
            if revision_key(previous) == revision_key(&record) {
                if previous == &record {
                    stats.revisions_deduped += 1;
                } else {
                    conflicts.push(record);
                }
                continue;
            }
        }
        unique.push(record);
    }
    (unique, conflicts)
}

struct PreparedFreshChain {
    f0: Vec<u8>,
    history: Option<Vec<u8>>,
}

fn prepare_fresh_chain(
    records: &[Vec<u8>],
    dictionaries: &crate::frames::DictionaryStore,
) -> Result<Option<PreparedFreshChain>> {
    let Some(head) = records.first() else {
        return Ok(None);
    };
    let history = if records.len() > 1 {
        let total = records[1..].iter().try_fold(0u64, |sum, record| {
            sum.checked_add(record.len() as u64)
                .ok_or(crate::error::Error::Corrupt("revision history size overflow"))
        })?;
        let mut encoder = wikimak_depot::FrameEncoder::new(total, Some(head), 3)
            .map_err(|_| crate::error::Error::Codec("zstd compress"))?;
        for record in &records[1..] {
            encoder
                .write(record)
                .map_err(|_| crate::error::Error::Codec("zstd compress"))?;
        }
        let history = encoder
            .finish()
            .map_err(|_| crate::error::Error::Codec("zstd compress"))?;
        Some(history)
    } else {
        None
    };
    let f0 = crate::frames::compress_head(head, dictionaries)?;
    Ok(Some(PreparedFreshChain { f0, history }))
}

fn install_fresh_chain(
    g: &InstanceInner,
    page_id: u64,
    prepared: PreparedFreshChain,
) -> Result<()> {
    let mut builder = g.depot.begin_chain(page_id)?;
    if let Some(history) = prepared.history {
        g.depot.append_history_frame(&mut builder, &history)?;
    }
    g.depot.finish_chain(builder, &prepared.f0, None)?;
    Ok(())
}

fn merge_existing_chain(
    g: &InstanceInner,
    page_id: u64,
    old_head_id: u64,
    incoming: &[Vec<u8>],
    seal_threshold: u64,
    stats: &mut ImportStats,
) -> Result<(u64, u64)> {
    use crate::revision_merge::{MergeOrigin, RevisionMerge, StoredRecords};

    let incoming_stream = || incoming.iter().cloned().map(Ok);
    let mut merge = RevisionMerge::new(
        StoredRecords::new(&g.depot, &g.revision_dictionaries, page_id),
        incoming_stream(),
    );
    let mut new_count = 0u64;
    let mut deduped = 0u64;
    let mut has_interleaved_new = false;
    let mut record_count = 0u64;
    let mut history_bytes = 0u64;
    while let Some(item) = merge.next()? {
        if record_count > 0 {
            history_bytes = history_bytes
                .checked_add(item.record.len() as u64)
                .ok_or(crate::error::Error::Corrupt(
                    "replacement history byte count overflow",
                ))?;
        }
        record_count += 1;
        match item.origin {
            MergeOrigin::Incoming => {
                new_count += 1;
                has_interleaved_new |= revision_key(&item.record) <= old_head_id;
            }
            MergeOrigin::Both => deduped += 1,
            MergeOrigin::Stored => {}
        }
        if let Some(conflict) = item.conflicting_incoming {
            if append_correction(g, page_id, &conflict, seal_threshold)? {
                stats.revision_conflicts += 1;
            }
        }
    }
    if new_count == 0 {
        return Ok((0, deduped));
    }
    if !has_interleaved_new {
        let prefix: Vec<Vec<u8>> = incoming
            .iter()
            .take_while(|record| revision_key(record) > old_head_id)
            .cloned()
            .collect();
        if prefix.len() as u64 != new_count {
            return Err(crate::error::Error::Corrupt(
                "newer revision prefix disagrees with merge",
            ));
        }
        prepend_depot_frames(g, page_id, &prefix, seal_threshold)?;
        return Ok((new_count, deduped));
    }

    let mut merge = RevisionMerge::new(
        StoredRecords::new(&g.depot, &g.revision_dictionaries, page_id),
        incoming_stream(),
    );
    let head = merge
        .next()?
        .ok_or(crate::error::Error::Corrupt("replacement merge produced no head"))?
        .record;
    let mut builder = g.depot.begin_replace_chain(page_id)?;
    if history_bytes > 0 {
        let mut encoder = wikimak_depot::FrameEncoder::new(history_bytes, Some(&head), 3)
            .map_err(|_| crate::error::Error::Codec("zstd compress"))?;
        while let Some(item) = merge.next()? {
            encoder
                .write(&item.record)
                .map_err(|_| crate::error::Error::Codec("zstd compress"))?;
        }
        let history = encoder
            .finish()
            .map_err(|_| crate::error::Error::Codec("zstd compress"))?;
        g.depot.append_history_frame(&mut builder, &history)?;
    }
    let f0 = crate::frames::compress_head(&head, &g.revision_dictionaries)?;
    g.depot.finish_chain(builder, &f0, None)?;
    Ok((new_count, deduped))
}

fn append_correction(
    g: &InstanceInner,
    page_id: u64,
    incoming: &[u8],
    seal_threshold: u64,
) -> Result<bool> {
    use crate::revision_merge::encode_correction;

    let revision_id = crate::revision::peek_rev_id(incoming)?;
    let corrections = crate::revision_merge::read_corrections(&g.corrections, page_id)?;
    if corrections
        .iter()
        .any(|event| event.revision_id == revision_id && event.incoming_record == incoming)
    {
        return Ok(false);
    }
    let max_occurrence = corrections.iter().map(|event| event.occurrence).max().unwrap_or(0);

    let event = encode_correction(revision_id, max_occurrence + 1, incoming);
    let new_f0 = crate::frames::compress(&event, None)?;
    if !g.corrections.has_chain(page_id)? {
        g.corrections.prepend(page_id, &new_f0, None, false)?;
        return Ok(true);
    }
    let old_f0 = crate::frames::decompress(&g.corrections.read_f0(page_id)?, None)?;
    let old_f1 = match g.corrections.read_f1(page_id)? {
        Some(frame) => Some(crate::frames::decompress(&frame, Some(&old_f0))?),
        None => None,
    };
    let (new_f1_raw, seal) =
        wikimak_depot::compose_f1(&[old_f0.as_slice()], old_f1.as_deref(), seal_threshold);
    let new_f1 = crate::frames::compress(&new_f1_raw, Some(&event))?;
    g.corrections.prepend(page_id, &new_f0, Some(&new_f1), seal)?;
    if new_f1_raw.len() as u64 > seal_threshold {
        g.corrections.seal_f1(page_id)?;
    }
    Ok(true)
}

fn ensure_current_title(
    g: &mut InstanceInner,
    page_id: u64,
    title: &[u8],
    title_shard_count: u32,
    earliest_ts: Option<i64>,
    dump_extends_head: bool,
) -> Result<usize> {
    let Some(start_micros) = earliest_ts else {
        return Ok(0);
    };
    let start = u32::try_from(start_micros.div_euclid(1_000_000))
        .map_err(|_| crate::error::Error::Corrupt("title timestamp outside u32 seconds"))?;
    let normalized = crate::titles::normalize_title(title);
    let mut ids = crate::titles::lookup_ids(&g.titles, title_shard_count, &normalized)?;
    let title_id = match ids.pop() {
        Some(id) => id,
        None => {
            let shard = crate::titles::shard_for(&normalized, title_shard_count);
            g.titles.append(shard, &normalized)?
        }
    };
    let page_id: u32 =
        page_id.try_into().map_err(|_| crate::error::Error::Corrupt("page id exceeds u32"))?;
    let current_title = crate::instance::effective_page_title_id(g, page_id)?;
    if current_title != Some(title_id) && current_title.is_some() && !dump_extends_head {
        return Ok(0);
    }
    let mut changes = Vec::with_capacity(2);
    if let Some(old_title_id) = current_title {
        if old_title_id != title_id {
            changes.push((old_title_id, crate::title_slots::TitleBinding::unbound(start)));
        }
    }
    let since = crate::instance::effective_title_binding(g, title_id)?
        .filter(|binding| binding.page_id == page_id)
        .map_or(start, |binding| binding.valid_since.min(start));
    changes.push((
        title_id,
        crate::title_slots::TitleBinding::bound(page_id, since)?,
    ));
    let mut added = 0;
    for (title_id, binding) in changes {
        added += g.conn.execute(
            "INSERT OR IGNORE INTO title_slot_intent(title_id,page_id,valid_since)
             VALUES(?1,?2,?3)",
            rusqlite::params![title_id as i64, binding.page_id, binding.valid_since],
        )?;
        g.conn.execute(
            "UPDATE title_slot_intent SET page_id=?2,valid_since=?3 WHERE title_id=?1",
            rusqlite::params![title_id as i64, binding.page_id, binding.valid_since],
        )?;
    }
    Ok(added)
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
            crate::frames::decompress_head(
                &frame,
                &g.revision_dictionaries,
                "revision",
            )?,
        ),
        None => {
            // Empty chain: seed with the OLDEST record (the depot
            // forbids f1 on a chain's first prepend), then absorb the
            // rest as one batch.
            let (seed, rest) = records_newest_first.split_last().expect("non-empty batch");
            g.depot
                .prepend(
                    chain_id,
                    &crate::frames::compress_head(seed, &g.revision_dictionaries)?,
                    None,
                    false,
                )?;
            if rest.is_empty() {
                return Ok(());
            }
            (&rest[0], &rest[1..], seed.clone())
        }
    };
    let old_f1_raw = match g.depot.read_f1(chain_id)? {
        Some(f1_frame) => crate::frames::decompress_history(&f1_frame, &prev_record)?,
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
    let new_f0 = crate::frames::compress_head(head, &g.revision_dictionaries)?;
    let new_f1 = crate::frames::compress_history(&new_f1_raw, head)?;
    g.depot.prepend(chain_id, &new_f0, Some(&new_f1), seal)?;
    Ok(())
}

fn capture_siteinfo(conn: &rusqlite::Connection, si: &wikimak_mediawiki::SiteInfo) -> Result<()> {
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

    // The export header is a minimal bootstrap, not an upstream-timed
    // siteinfo history source: it carries no effective timestamp and is
    // repeated byte-for-byte in every multipart dump. Keep exactly one
    // bootstrap snapshot. Rich API siteinfo refreshes belong to their
    // own changed-content/versioned path with an actual observation
    // time; reimporting an old dump must not fabricate a new present-day
    // configuration event.
    let already_bootstrapped: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM siteinfo_snapshots LIMIT 1)",
        [],
        |row| row.get(0),
    )?;
    if already_bootstrapped {
        return Ok(());
    }

    let captured_at = chrono::Utc::now().timestamp_micros();
    conn.execute(
        "INSERT INTO siteinfo_snapshots(captured_at, json) VALUES(?1, ?2)",
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

#[cfg(test)]
mod concurrency_tests {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;
    use wikimak_mediawiki::new_page_stream;

    use super::*;

    fn xml(page: u64) -> Vec<u8> {
        let text = "parallel fresh-page compression ".repeat(20_000);
        format!(
            "<mediawiki xmlns=\"http://www.mediawiki.org/xml/export-0.11/\" version=\"0.11\">\
             <siteinfo><sitename>P</sitename><dbname>p</dbname><base>x</base><generator>g</generator>\
             <case>first-letter</case><namespaces><namespace key=\"0\" case=\"first-letter\"/>\
             </namespaces></siteinfo><page><title>Page {page}</title><ns>0</ns><id>{page}</id>\
             <revision><id>{}</id><timestamp>2024-01-01T00:00:00Z</timestamp>\
             <contributor><username>E</username><id>1</id></contributor>\
             <text xml:space=\"preserve\">{text}</text></revision></page></mediawiki>",
            1000 + page
        )
        .into_bytes()
    }

    fn many_pages_xml(pages: u64) -> Vec<u8> {
        let mut body = String::new();
        for page in 1..=pages {
            body.push_str(&format!(
                "<page><title>Batch {page}</title><ns>0</ns><id>{page}</id>\
                 <revision><id>{}</id><timestamp>2024-01-01T00:00:00Z</timestamp>\
                 <contributor><username>E</username><id>1</id></contributor>\
                 <text xml:space=\"preserve\">text {page}</text></revision></page>",
                2000 + page
            ));
        }
        format!(
            "<mediawiki xmlns=\"http://www.mediawiki.org/xml/export-0.11/\" version=\"0.11\">\
             <siteinfo><sitename>P</sitename><dbname>p</dbname><base>x</base><generator>g</generator>\
             <case>first-letter</case><namespaces><namespace key=\"0\" case=\"first-letter\"/>\
             </namespaces></siteinfo>{body}</mediawiki>"
        )
        .into_bytes()
    }

    #[test]
    fn two_page_streams_prepare_before_either_enters_final_install() {
        let tmp = TempDir::new().unwrap();
        let instance = crate::Instance::open(crate::InstanceConfig {
            root: tmp.path().to_path_buf(),
            dbname: "parallel".into(),
            max_chain_id: 16,
            depot: wikimak_depot::DepotConfig {
                root: std::path::PathBuf::new(),
                max_chain_id: 16,
                file_size_threshold: 8 << 20,
                eviction_dead_ratio: 0.5,
            },
            title_shard_count: 1,
            title_seal_threshold_bytes: 8 << 20,
            f1_seal_threshold_bytes: 1 << 20,
        })
        .unwrap();
        let gate = Arc::new(PrepareGate {
            state: std::sync::Mutex::new((0, false)),
            changed: std::sync::Condvar::new(),
        });
        *PREPARE_GATE.lock().unwrap() = Some(Arc::clone(&gate));

        let both_prepared = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                let mut stream = new_page_stream(Cursor::new(xml(1)));
                instance.import(&mut stream)
            });
            let b = scope.spawn(|| {
                let mut stream = new_page_stream(Cursor::new(xml(2)));
                instance.import(&mut stream)
            });

            let mut state = gate.state.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while state.0 < 2 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (next, _) = gate.changed.wait_timeout(state, remaining).unwrap();
                state = next;
            }
            let both_prepared = state.0 == 2;
            state.1 = true;
            gate.changed.notify_all();
            drop(state);

            a.join().unwrap().unwrap();
            b.join().unwrap().unwrap();
            both_prepared
        });
        *PREPARE_GATE.lock().unwrap() = None;
        assert!(both_prepared, "page-local parsing/compression serialized behind Instance");
        assert!(instance.page_head(1).unwrap().is_some());
        assert!(instance.page_head(2).unwrap().is_some());
    }

    #[test]
    fn title_slot_files_are_applied_once_per_import_not_once_per_page() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let tmp = TempDir::new().unwrap();
        let instance = crate::Instance::open(crate::instance::read_config(
            tmp.path().to_path_buf(),
        ))
        .unwrap();
        let count = Arc::new(AtomicU64::new(0));
        crate::title_slots::set_apply_counter(Some((
            tmp.path().to_path_buf(),
            Arc::clone(&count),
        )));
        let mut stream = new_page_stream(Cursor::new(many_pages_xml(32)));
        instance.import(&mut stream).unwrap();
        crate::title_slots::set_apply_counter(None);

        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(instance.page_current_title(1).unwrap().is_some());
        assert!(instance.page_current_title(32).unwrap().is_some());
    }
}
