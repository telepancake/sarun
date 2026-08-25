//! Bounded merge of two canonical per-page revision streams.
//!
//! Both inputs must already be strictly descending by revision id and
//! contain each revision id at most once. Revision id is the immutable
//! identity key; timestamp is record content, so a changed timestamp
//! must collide here rather than evade deduplication. The merge
//! retains one record from each input plus the returned record. This
//! module deliberately does not pretend to solve input preparation:
//! MediaWiki XML is collected page-at-a-time and sorted before this
//! merge.

use crate::error::{Error, Result};

const CORRECTION_MAGIC: [u8; 4] = *b"WCR1";
const CORRECTION_VERSION: u32 = 1;
const CORRECTION_HEADER: usize = 4 + 4 + 8 + 8 + 8;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RevisionKey {
    pub(crate) revision_id: u64,
}

struct Record {
    key: RevisionKey,
    bytes: Vec<u8>,
}

impl Record {
    fn decode(bytes: Vec<u8>) -> Result<Self> {
        Ok(Self {
            key: RevisionKey {
                revision_id: crate::revision::peek_rev_id(&bytes)?,
            },
            bytes,
        })
    }
}

/// One canonical record and, when the same immutable revision id had
/// different bytes in the incoming stream, the conflicting bytes that
/// a correction/event sink must preserve. Stored content wins the
/// canonical slot; the conflict is never silently overwritten.
#[derive(Debug)]
pub(crate) struct MergedRevision {
    pub(crate) record: Vec<u8>,
    pub(crate) conflicting_incoming: Option<Vec<u8>>,
    pub(crate) origin: MergeOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MergeOrigin {
    Stored,
    Incoming,
    Both,
}

/// Tagged record in the separate correction lane. `occurrence` is
/// monotonic within one page's correction chain. The complete incoming
/// revision record is retained; canonical archival content is never
/// replaced by it.
pub(crate) struct CorrectionRecord<'a> {
    pub(crate) revision_id: u64,
    pub(crate) occurrence: u64,
    pub(crate) incoming_record: &'a [u8],
}

pub(crate) fn encode_correction(
    revision_id: u64,
    occurrence: u64,
    incoming_record: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(CORRECTION_HEADER + incoming_record.len());
    out.extend_from_slice(&CORRECTION_MAGIC);
    out.extend_from_slice(&CORRECTION_VERSION.to_le_bytes());
    out.extend_from_slice(&revision_id.to_le_bytes());
    out.extend_from_slice(&occurrence.to_le_bytes());
    out.extend_from_slice(&(incoming_record.len() as u64).to_le_bytes());
    out.extend_from_slice(incoming_record);
    out
}

pub(crate) fn decode_correction(buf: &[u8]) -> Result<CorrectionRecord<'_>> {
    if buf.len() < CORRECTION_HEADER {
        return Err(Error::Codec("truncated correction record"));
    }
    if buf[..4] != CORRECTION_MAGIC {
        return Err(Error::Codec("unknown correction record tag"));
    }
    let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    if version != CORRECTION_VERSION {
        return Err(Error::Codec("unknown correction record version"));
    }
    let revision_id = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let occurrence = u64::from_le_bytes(buf[16..24].try_into().unwrap());
    let len = u64::from_le_bytes(buf[24..32].try_into().unwrap());
    let len = usize::try_from(len).map_err(|_| Error::Codec("correction record too large"))?;
    let end = CORRECTION_HEADER
        .checked_add(len)
        .ok_or(Error::Codec("correction record too large"))?;
    if end != buf.len() {
        return Err(Error::Codec("invalid correction record length"));
    }
    Ok(CorrectionRecord {
        revision_id,
        occurrence,
        incoming_record: &buf[CORRECTION_HEADER..end],
    })
}

pub(crate) fn correction_record_len(buf: &[u8], start: usize) -> Result<usize> {
    if start.checked_add(CORRECTION_HEADER).is_none_or(|end| end > buf.len()) {
        return Err(Error::Codec("truncated correction record"));
    }
    let len = u64::from_le_bytes(buf[start + 24..start + 32].try_into().unwrap());
    let len = usize::try_from(len).map_err(|_| Error::Codec("correction record too large"))?;
    let total = CORRECTION_HEADER
        .checked_add(len)
        .ok_or(Error::Codec("correction record too large"))?;
    if start.checked_add(total).is_none_or(|end| end > buf.len()) {
        return Err(Error::Codec("truncated correction record"));
    }
    Ok(total)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OwnedCorrection {
    pub(crate) revision_id: u64,
    pub(crate) occurrence: u64,
    pub(crate) incoming_record: Vec<u8>,
}

pub(crate) fn read_corrections(
    depot: &wikimak_depot::Depot,
    page_id: u64,
) -> Result<Vec<OwnedCorrection>> {
    if !depot.has_chain(page_id)? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut anchor = None::<Vec<u8>>;
    let f0 = crate::frames::decompress(&depot.read_f0(page_id)?, None)?;
    consume_corrections(&f0, &mut out, &mut anchor)?;
    if let Some(f1) = depot.read_f1(page_id)? {
        let raw = crate::frames::decompress(&f1, anchor.as_deref())?;
        consume_corrections(&raw, &mut out, &mut anchor)?;
    }
    let mut cold = depot.cold_cursor(page_id)?;
    while let Some(frame) = depot.cold_next(&mut cold)? {
        let raw = crate::frames::decompress(&frame, anchor.as_deref())?;
        consume_corrections(&raw, &mut out, &mut anchor)?;
    }
    Ok(out)
}

fn consume_corrections(
    raw: &[u8],
    out: &mut Vec<OwnedCorrection>,
    anchor: &mut Option<Vec<u8>>,
) -> Result<()> {
    let mut pos = 0usize;
    while pos < raw.len() {
        let len = correction_record_len(raw, pos)?;
        let bytes = &raw[pos..pos + len];
        let event = decode_correction(bytes)?;
        out.push(OwnedCorrection {
            revision_id: event.revision_id,
            occurrence: event.occurrence,
            incoming_record: event.incoming_record.to_vec(),
        });
        *anchor = Some(bytes.to_vec());
        pos += len;
    }
    Ok(())
}

struct Checked<I> {
    input: I,
    previous: Option<RevisionKey>,
}

impl<I> Checked<I>
where
    I: Iterator<Item = Result<Vec<u8>>>,
{
    fn next(&mut self) -> Result<Option<Record>> {
        let Some(bytes) = self.input.next().transpose()? else {
            return Ok(None);
        };
        let record = Record::decode(bytes)?;
        if self.previous.is_some_and(|previous| record.key >= previous) {
            return Err(Error::Corrupt(
                "revision merge input is not strictly descending by revision id",
            ));
        }
        self.previous = Some(record.key);
        Ok(Some(record))
    }
}

/// Merge two already-canonical streams using constant record count.
///
/// Equal keys necessarily mean equal revision ids. Different bytes
/// produce an explicit conflict while preserving the stored bytes as
/// canonical. A changed timestamp changes the bytes but not the key,
/// and therefore enters this same explicit conflict path.
pub(crate) struct RevisionMerge<S, I> {
    stored: Checked<S>,
    incoming: Checked<I>,
    stored_head: Option<Record>,
    incoming_head: Option<Record>,
    started: bool,
}

impl<S, I> RevisionMerge<S, I>
where
    S: Iterator<Item = Result<Vec<u8>>>,
    I: Iterator<Item = Result<Vec<u8>>>,
{
    pub(crate) fn new(stored: S, incoming: I) -> Self {
        Self {
            stored: Checked { input: stored, previous: None },
            incoming: Checked { input: incoming, previous: None },
            stored_head: None,
            incoming_head: None,
            started: false,
        }
    }

    fn start(&mut self) -> Result<()> {
        if !self.started {
            self.stored_head = self.stored.next()?;
            self.incoming_head = self.incoming.next()?;
            self.started = true;
        }
        Ok(())
    }

    pub(crate) fn next(&mut self) -> Result<Option<MergedRevision>> {
        self.start()?;
        let take = match (&self.stored_head, &self.incoming_head) {
            (None, None) => return Ok(None),
            (Some(_), None) => 0,
            (None, Some(_)) => 1,
            (Some(stored), Some(incoming)) => match stored.key.cmp(&incoming.key) {
                std::cmp::Ordering::Greater => 0,
                std::cmp::Ordering::Less => 1,
                std::cmp::Ordering::Equal => 2,
            },
        };
        match take {
            0 => {
                let record = self.stored_head.take().expect("selected stored");
                self.stored_head = self.stored.next()?;
                Ok(Some(MergedRevision {
                    record: record.bytes,
                    conflicting_incoming: None,
                    origin: MergeOrigin::Stored,
                }))
            }
            1 => {
                let record = self.incoming_head.take().expect("selected incoming");
                self.incoming_head = self.incoming.next()?;
                Ok(Some(MergedRevision {
                    record: record.bytes,
                    conflicting_incoming: None,
                    origin: MergeOrigin::Incoming,
                }))
            }
            _ => {
                let stored = self.stored_head.take().expect("selected stored");
                let incoming = self.incoming_head.take().expect("selected incoming");
                self.stored_head = self.stored.next()?;
                self.incoming_head = self.incoming.next()?;
                let conflict =
                    if stored.bytes == incoming.bytes { None } else { Some(incoming.bytes) };
                Ok(Some(MergedRevision {
                    record: stored.bytes,
                    conflicting_incoming: conflict,
                    origin: MergeOrigin::Both,
                }))
            }
        }
    }
}

/// Owned-record adapter over the authoritative depot walk. The walk
/// itself retains only decoder state/current record; this copies only
/// the record handed to the merge head.
pub(crate) struct StoredRecords<'a> {
    depot: &'a wikimak_depot::Depot,
    dictionaries: &'a crate::frames::DictionaryStore,
    walk: crate::instance::WalkState,
    done: bool,
}

impl<'a> StoredRecords<'a> {
    pub(crate) fn new(
        depot: &'a wikimak_depot::Depot,
        dictionaries: &'a crate::frames::DictionaryStore,
        page_id: u64,
    ) -> Self {
        Self {
            depot,
            dictionaries,
            walk: crate::instance::WalkState::new(page_id),
            done: false,
        }
    }
}

impl Iterator for StoredRecords<'_> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.walk.next_record(self.depot, self.dictionaries) {
            Ok(Some(record)) => Some(Ok(record.to_vec())),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::RevisionMerge;
    use crate::{ContributorMeta, RevisionMeta};

    fn record(id: u64, ts: i64, text: &[u8]) -> Vec<u8> {
        crate::revision::encode_revision(
            &RevisionMeta {
                rev_id: id,
                parent_id: id.saturating_sub(1),
                ts: Utc.timestamp_micros(ts).single().unwrap(),
                contributor: ContributorMeta::Hidden,
                comment: String::new(),
                sha1: String::new(),
                flags: 0,
                text_len: text.len() as u64,
            },
            text,
        )
    }

    fn ok(records: Vec<Vec<u8>>) -> impl Iterator<Item = crate::Result<Vec<u8>>> {
        records.into_iter().map(Ok)
    }

    #[test]
    fn canonical_merge_orders_and_deduplicates() {
        let stored = vec![record(5, 50, b"five"), record(3, 30, b"three")];
        let incoming =
            vec![record(6, 60, b"six"), record(5, 50, b"five"), record(4, 40, b"four")];
        let mut merge = RevisionMerge::new(ok(stored), ok(incoming));
        let mut ids = Vec::new();
        while let Some(item) = merge.next().unwrap() {
            assert!(item.conflicting_incoming.is_none());
            ids.push(crate::revision::peek_rev_id(&item.record).unwrap());
        }
        assert_eq!(ids, [6, 5, 4, 3]);
    }

    #[test]
    fn conflicting_immutable_content_is_returned_to_the_caller() {
        let stored = record(7, 70, b"archived original");
        let incoming = record(7, 70, b"later conflicting export");
        let mut merge = RevisionMerge::new(ok(vec![stored.clone()]), ok(vec![incoming.clone()]));
        let item = merge.next().unwrap().unwrap();
        assert_eq!(item.record, stored);
        assert_eq!(item.conflicting_incoming, Some(incoming));
        assert!(merge.next().unwrap().is_none());
    }

    #[test]
    fn rejects_noncanonical_input_instead_of_misordering() {
        let incoming = vec![record(1, 10, b"one"), record(2, 20, b"two")];
        let mut merge = RevisionMerge::new(ok(Vec::new()), ok(incoming));
        let err = merge.next().unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Corrupt("revision merge input is not strictly descending by revision id")
        ));
    }

    #[test]
    fn correction_record_is_tagged_and_self_delimiting() {
        let incoming = record(9, 90, b"conflicting bytes");
        let encoded = super::encode_correction(9, 3, &incoming);
        assert_eq!(super::correction_record_len(&encoded, 0).unwrap(), encoded.len());
        let decoded = super::decode_correction(&encoded).unwrap();
        assert_eq!(decoded.revision_id, 9);
        assert_eq!(decoded.occurrence, 3);
        assert_eq!(decoded.incoming_record, incoming);
        let mut truncated = encoded;
        truncated.pop();
        assert!(super::decode_correction(&truncated).is_err());
    }
}
