# Portable Wikipedia event stream

Version 1 is a research/export format independent of the live mirror layout.
It is intended to make storage experiments ordinary ordered stream filters.

- Entity groups are ordered by kind (`page`, then `user`, then `global`) and
  then by entity ID.
- Within one entity ID, records are newest to oldest. Equal timestamps use a
  deterministic source-specific tie order.
- Revision records contain metadata and wikitext, but not SHA-1.
- A revision may carry its complete MediaWiki History visibility annotation.
- Page-action records preserve every imported denormalized-history column.
- User events are grouped by their subject user ID, not by the actor who
  performed the action. Their complete original source fields are retained so
  additions to the upstream denormalized schema are not silently discarded.
- A page-state record preserves the current title once per page.
- Unknown typed records can be skipped using their payload length.

The fixed 24-byte file header is followed by 48-byte frame headers and zstd
frames. A writer starts a new frame at the next page boundary after compressed
output reaches the target (4 MiB by default). Thus a page is never split, and a
single exceptionally large page may produce a frame larger than the target.
Each frame header stores its page range, record count, raw length, and
compressed length. Independent filters may process frames in parallel and
write their results in original frame order.

There is no whole-file checksum. A `DONE` header distinguishes a clean finish,
but every completely written preceding frame remains readable if a later frame
or the completion marker is truncated. Zstd's own structural validation is
local to each frame and never invalidates an earlier frame.

## Version 1 wire layout

All fixed-width integers are little-endian. `varint` is unsigned LEB128.

The file header is:

```text
[ magic "SWDUMP\0\0":8 | version:u32 | flags:u32 | frame_target:u64 ]
```

Each 64-byte frame header is:

```text
[ magic "FRM1":4 | header_len:u32
| first_entity_kind:u8 | last_entity_kind:u8 | reserved:6
| first_entity_id:u64 | last_entity_id:u64
| record_count:u64 | raw_bytes:u64 | compressed_bytes:u64 | reserved:8 ]
```

The following `compressed_bytes` bytes are one independent zstd frame. Its raw
stream contains:

```text
[ entity_kind:u8 | entity_id:varint | timestamp_micros:i64
| record_kind:u8 | payload_len:varint | payload:payload_len ] ...
```

Entity kinds are page `1`, user `2`, and global/unbound `3`. Record kinds are
page state `1`, revision `2`, selected legacy page action `3`, and complete
MediaWiki History source event `4`. Payload lengths make future record kinds
skippable. Revision payloads deliberately omit SHA-1.

A clean file ends with a 64-byte `DONE` header. It contains no counts or digest:
those would turn an independently recoverable frame prefix into an
all-or-nothing object.

## MediaWiki History user events

MediaWiki History mixes page, revision, and subject-user events in source
partition order. During reconciliation, user rows are accumulated in bounded
64-MiB runs, sorted, and merged into
`history-users-<snapshot>.swdump`. This immutable sidecar contains user groups
in ID order and newest-to-oldest events within each user. It preserves every
original TSV field (including fields unknown to the current renderer) and is
published in the same metadata transaction through `history_user_archive`.

The sidecar is not a SQLite event ledger. A full portable export appends its
user/global groups after all page groups. Superseded snapshot sidecars are
removed only after the new metadata transaction commits.
