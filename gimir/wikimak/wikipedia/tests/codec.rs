//! Per-revision binary codec tests. SPEC §"Per-revision storage in
//! the depot" + PHASES §"Per-revision record codec". These pin the wire
//! format byte-for-byte; the implementer's encoder MUST produce these
//! exact bytes for the given inputs.

use chrono::{TimeZone, Utc};
use wikimak_wikipedia::{
    ContributorMeta, RevisionMeta, FLAG_SHA1_MISMATCH, FLAG_SUPPRESSED, FLAG_TEXT_HIDDEN,
    KIND_ANONYMOUS, KIND_HIDDEN, KIND_NAMED, REVISION_SCHEMA_VERSION,
};

// Re-exports of the codec helpers; tests pin their public availability.
use wikimak_wikipedia::revision::{
    decode_revision, decode_varint, encode_revision, encode_varint,
};

// ---------------------------------------------------------------------------
// revision_codec_round_trip_basic
//
// Encode + decode → every field byte-identical.
// ---------------------------------------------------------------------------

#[test]
fn revision_codec_round_trip_basic() {
    let meta = RevisionMeta {
        rev_id: 12345,
        parent_id: 12344,
        ts: Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap(),
        contributor: ContributorMeta::Named {
            username: "Alice".to_string(),
            user_id: 10,
        },
        comment: "first edit".to_string(),
        sha1: "abcdefghijklmnopqrstuvwxyz01234".to_string(),
        flags: 0,
        text_len: 11,
    };
    let text = b"hello world";

    let bytes = encode_revision(&meta, text);
    let (got_meta, got_text) = decode_revision(&bytes).expect("decode");
    assert_eq!(got_meta, meta, "metadata round-trips");
    assert_eq!(got_text, text, "text round-trips");
}

// ---------------------------------------------------------------------------
// revision_codec_schema_version_prefix
//
// First 4 bytes (little-endian u32) MUST equal `REVISION_SCHEMA_VERSION`.
// ---------------------------------------------------------------------------

#[test]
fn revision_codec_schema_version_prefix() {
    let meta = RevisionMeta {
        rev_id: 1,
        parent_id: 0,
        ts: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        contributor: ContributorMeta::Hidden,
        comment: String::new(),
        sha1: String::new(),
        flags: 0,
        text_len: 0,
    };
    let bytes = encode_revision(&meta, b"");
    assert!(bytes.len() >= 4);
    let ver = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    assert_eq!(ver, REVISION_SCHEMA_VERSION);
    assert_eq!(ver, 1, "schema version starts at 1");
}

// ---------------------------------------------------------------------------
// revision_codec_flag_bits
//
// Flags layout pinned: TEXT_HIDDEN (0x01) | SUPPRESSED (0x08) |
// SHA1_MISMATCH (0x10) → 0x19. Read bytes 4..8 as the LE u32 flags.
// ---------------------------------------------------------------------------

#[test]
fn revision_codec_flag_bits() {
    let meta = RevisionMeta {
        rev_id: 1,
        parent_id: 0,
        ts: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        contributor: ContributorMeta::Hidden,
        comment: String::new(),
        sha1: String::new(),
        flags: FLAG_TEXT_HIDDEN | FLAG_SUPPRESSED | FLAG_SHA1_MISMATCH,
        text_len: 0,
    };
    let bytes = encode_revision(&meta, b"");
    assert!(bytes.len() >= 8);
    let flags = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    assert_eq!(flags, 0x01 | 0x08 | 0x10);
    assert_eq!(flags, 0x19, "bitwise OR of the three flag bits");
}

// ---------------------------------------------------------------------------
// revision_codec_contributor_variants
//
// kind byte position: after 4(ver)+4(flags)+8(rev_id)+8(parent_id)+
// 8(ts)+8(user_id) = 40 bytes in.
// ---------------------------------------------------------------------------

const CONTRIBUTOR_KIND_OFFSET: usize = 4 + 4 + 8 + 8 + 8 + 8;

#[test]
fn revision_codec_contributor_anonymous_kind() {
    let meta = RevisionMeta {
        rev_id: 1,
        parent_id: 0,
        ts: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        contributor: ContributorMeta::Anonymous {
            ip: "10.0.0.1".to_string(),
        },
        comment: String::new(),
        sha1: String::new(),
        flags: 0,
        text_len: 0,
    };
    let bytes = encode_revision(&meta, b"");
    assert_eq!(bytes[CONTRIBUTOR_KIND_OFFSET], KIND_ANONYMOUS);
    assert_eq!(bytes[CONTRIBUTOR_KIND_OFFSET], 0);
    // contributor_user_id must be 0 for Anonymous (bytes 32..40).
    let user_id = u64::from_le_bytes([
        bytes[32], bytes[33], bytes[34], bytes[35], bytes[36], bytes[37], bytes[38], bytes[39],
    ]);
    assert_eq!(user_id, 0, "Anonymous → contributor_user_id is 0");
}

#[test]
fn revision_codec_contributor_named_kind() {
    let meta = RevisionMeta {
        rev_id: 1,
        parent_id: 0,
        ts: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        contributor: ContributorMeta::Named {
            username: "Bob".to_string(),
            user_id: 99,
        },
        comment: String::new(),
        sha1: String::new(),
        flags: 0,
        text_len: 0,
    };
    let bytes = encode_revision(&meta, b"");
    assert_eq!(bytes[CONTRIBUTOR_KIND_OFFSET], KIND_NAMED);
    assert_eq!(bytes[CONTRIBUTOR_KIND_OFFSET], 1);
    let user_id = u64::from_le_bytes([
        bytes[32], bytes[33], bytes[34], bytes[35], bytes[36], bytes[37], bytes[38], bytes[39],
    ]);
    assert_eq!(user_id, 99);
}

#[test]
fn revision_codec_contributor_hidden_kind() {
    let meta = RevisionMeta {
        rev_id: 1,
        parent_id: 0,
        ts: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        contributor: ContributorMeta::Hidden,
        comment: String::new(),
        sha1: String::new(),
        flags: 0,
        text_len: 0,
    };
    let bytes = encode_revision(&meta, b"");
    assert_eq!(bytes[CONTRIBUTOR_KIND_OFFSET], KIND_HIDDEN);
    assert_eq!(bytes[CONTRIBUTOR_KIND_OFFSET], 2);
    // For Hidden, contributor_len byte (varint) at offset 41 must be 0
    // (empty bytes).
    let contrib_len_byte = bytes[CONTRIBUTOR_KIND_OFFSET + 1];
    assert_eq!(contrib_len_byte, 0, "Hidden → contributor_bytes is empty");
}

// ---------------------------------------------------------------------------
// revision_codec_varint_leb128
//
// A text longer than 127 bytes must use a multi-byte LEB128 length
// prefix (not a fixed u32). Pick text_len = 200 → LEB128 encodes as
// 2 bytes: 0xC8 0x01.
// ---------------------------------------------------------------------------

#[test]
fn revision_codec_varint_leb128_basic() {
    // Round-trip the helper functions standalone.
    let mut out = Vec::new();
    encode_varint(0, &mut out);
    assert_eq!(out, vec![0x00]);

    let mut out = Vec::new();
    encode_varint(127, &mut out);
    assert_eq!(out, vec![0x7f]);

    let mut out = Vec::new();
    encode_varint(128, &mut out);
    assert_eq!(out, vec![0x80, 0x01], "128 = continuation byte + 1");

    let mut out = Vec::new();
    encode_varint(200, &mut out);
    assert_eq!(out, vec![0xc8, 0x01], "200 = 0xC8 0x01 in LEB128");

    let mut out = Vec::new();
    encode_varint(300, &mut out);
    assert_eq!(out, vec![0xac, 0x02], "300 = 0xAC 0x02 in LEB128");

    // Round-trip via decode_varint.
    for v in [0u64, 1, 127, 128, 200, 300, 1_000_000, u32::MAX as u64] {
        let mut buf = Vec::new();
        encode_varint(v, &mut buf);
        let (got, n) = decode_varint(&buf, 0).expect("decode");
        assert_eq!(got, v);
        assert_eq!(n, buf.len());
    }
}

// ---------------------------------------------------------------------------
// revision_codec_large_text_multi_byte_varint
//
// Pin: a text of length 200 in a real encoded record produces a
// multi-byte varint length prefix, not a fixed u32. We don't know the
// exact offset of the text varint (depends on comment/sha1/contributor
// lengths), so we assert: the encoded record contains the bytes
// `[0xC8, 0x01]` followed by 200 'A' bytes.
// ---------------------------------------------------------------------------

#[test]
fn revision_codec_large_text_multi_byte_varint() {
    let text: Vec<u8> = vec![b'A'; 200];
    let meta = RevisionMeta {
        rev_id: 1,
        parent_id: 0,
        ts: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        contributor: ContributorMeta::Hidden,
        comment: String::new(),
        sha1: String::new(),
        flags: 0,
        text_len: 200,
    };
    let bytes = encode_revision(&meta, &text);

    // Find the marker: 0xC8 0x01 followed by 200 'A's.
    let mut needle = vec![0xc8u8, 0x01u8];
    needle.extend(std::iter::repeat(b'A').take(200));
    let found = bytes.windows(needle.len()).any(|w| w == needle);
    assert!(
        found,
        "encoded record must contain LEB128 prefix [0xC8, 0x01] then 200 'A' bytes"
    );
}
