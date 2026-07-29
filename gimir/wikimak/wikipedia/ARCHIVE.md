# Portable Wikipedia event stream

This is a research/export format independent of the live mirror layout.
It is intended to make storage experiments ordinary ordered stream filters.

- Entity groups are ordered by kind (`page`, then `user`, then `global`) and
  then by entity ID.
- Within one entity ID, records are newest to oldest. Equal timestamps use a
  deterministic typed tie sequence.
- XML is parsed into typed revision records. Its serialization is never stored.
- MediaWiki History TSV rows are normalized immediately. Source column arrays
  and redundant current/analytical fields are never stored.
- A TSV revision row enriches the matching XML revision record. It is not a
  second copy of the revision metadata.
- Page and user actions use typed variants with an explicit `other(name)`
  escape for previously unseen event types.
- An unsupported TSV column count or entity class is a loud import error until
  the schema mapper is updated.
- Revision records contain metadata and optional wikitext, but not SHA-1.
- Page and user state records preserve current state once per entity.
- A manifest preserves the wiki, snapshot identifiers, and source filenames
  once per archive.
- Unknown future record kinds can be skipped using their payload length.

The fixed 24-byte file header is followed by 64-byte frame headers and zstd
frames. A writer checks actual emitted zstd bytes at every entity boundary and
starts a new frame when the target (4 MiB by default) has been reached. Page,
user, and global records are never mixed in one frame: changing entity kind
always starts a new frame. The header identifies that kind and the minimum and
maximum page ID or user ID in the frame. An entity is never split, so a single
exceptionally large page may produce a frame larger than the target.

Each independently streamed upstream dump part produces page-aligned frames.
Disjoint content segments are consolidated by copying their compressed frames
in page-range order. Parts whose page ranges overlap remain in one sequential
group. Typed metadata runs require an ordered merge because their entity ranges
overlap.

There is no whole-file checksum. A `DONE` header distinguishes a clean finish,
but every completely written preceding frame remains readable if a later frame
or the completion marker is truncated. Zstd structural validation is local to
each frame and never invalidates an earlier frame.

## Wire layout

All fixed-width integers are little-endian. `varint` is unsigned LEB128.

```text
[ magic "SWDUMP\0\0":8 | version:u32 | flags:u32 | frame_target:u64 ]

[ magic "FRM1":4 | header_len:u32
| first_entity_kind:u8 | last_entity_kind:u8 | reserved:6
| first_entity_id:u64 | last_entity_id:u64
| record_count:u64 | raw_bytes:u64 | compressed_bytes:u64 | reserved:8 ]

[ entity_kind:u8 | entity_id:varint | timestamp_micros:i64
| record_kind:u8 | payload_len:varint | payload:payload_len ] ...
```

Entity kinds are page `1`, user `2`, and global/unbound `3`. Record kinds are:

1. page state
2. revision
3. page action
4. user state
5. user action
6. manifest

A clean file ends with a 64-byte `DONE` header.

`wikimak archive-repack` is the first generic stream filter. It decodes records
in order and writes the same records using the requested compressed frame-size
target, zstd level, checksum, long-distance matching, window log, and target
compressed-block size. Frame boundaries remain entity-aligned.

`wikimak archive-merge` performs a canonical set union over any number of
archives and writes it through the same configurable compressor and framing
code as `archive-repack`. Records are externally sorted in bounded memory.
`--scratch-dir` places the bounded sort runs and hierarchical consolidation
passes on caller-selected storage.
Equal revision IDs are joined field by field; repeated actions are identified
from their typed event content rather than their source-row ordinal. Exact
records occur once. Consequently input order, grouping, and repetition do not
change the result.

`wikimak archive-build-update` reads the logical content date from a base
archive, fetches daily incremental content beginning three days before that
date, and emits a partial archive. The overlap makes late or repeated daily
runs harmless under merge. For partitioned MediaWiki History releases it
includes the newest completed partition and current partial partition; an
all-time wiki necessarily contributes its one all-time file.

## Normalized metadata

Page actions retain their event kind, log ID, performer, comment, historical
title/namespace, resulting deletion state, and a tie sequence. Current title is
stored once in page state. Counts, elapsed-time values, content-namespace
booleans, and other derivable analytical columns are discarded.

Revision annotations retain visibility, minor-edit state, content model and
format, identity-revert relations, before-page-creation state, and tags.
Revision SHA-1, duplicated contributor/comment/timestamp, text byte counts, and
page/user activity counters are discarded. A TSV-only revision is represented
as a typed text-absent revision shell; the ordered merge coalesces it with XML
by `(page_id, revision_id)`.

User actions retain their event kind, log ID, performer, comment, historical
subject state, account creation origin, and relevant timestamps. Current user
state is stored once per user. Current values repeated across old rows are not
retained.

Performer identity is a compact tuple of optional local and central IDs,
historical name/IP, and account class. Lists such as groups, blocks, bot
classifications, and revision tags are decoded into length-delimited string
arrays rather than retained in TSV escaping.
