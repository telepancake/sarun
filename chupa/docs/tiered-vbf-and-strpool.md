# Tiered VBF depot + string→id pool — design

Source: extracted from `meta/reports/vbf-recovery.md`, the verified reconstruction
of the prior owner-authored design. This document states the design itself, not its
history; commit references live in the recovery report.

## 1. Purpose and scope

The substrate for **revision-chain data**: one entity that accumulates a long series
of successive, ~99%-identical versions — a Wikipedia page and its revision history is
the canonical case; web captures over time, IETF draft series, and post-edit
histories have the same shape.

Content-addressed storage (a git object per revision) does not hold up at this
shape: a few million pages × hundreds of revisions becomes hundreds of millions of
tiny near-duplicate objects, with index and pack overhead that dwarfs the payload —
observed as prohibitive on real attempts, not just argued. This design targets the
shape directly: per-entity chains of zstd frames tuned so that successive
near-identical versions cost almost nothing, the newest version reads in one small
decode, and old history stays compressed and out of the hot path.

Two pieces, usable independently:
- **The tiered chain depot** ("tiered VBF") — stores each entity's version chain.
- **strpool** — interns repeated strings to dense integer ids. It lets a
  string-keyed source (titles, URLs, draft names) address the depot's integer-keyed
  index, and removes repeated-metadata redundancy inside records.

## 2. Data model

- An **entity** maps to one **chain**, addressed by a dense integer `chain_id`.
- A chain holds **versions**, newest-first; each version is opaque record bytes (the
  depot does not parse them) plus the caller's metadata.
- Successive versions are assumed highly similar; the compression exploits that.
- Where a key is a string, strpool assigns it a dense `chain_id`.

## 3. The chain depot: three tiers

Each chain is split across three tiers, isolated because they have different read
frequency and write cadence:

| Tier | Holds | Written | Read |
|---|---|---|---|
| **f0** (hot) | the newest version alone, zstd-compressed against the chain's pretrained dictionary | replaced on every update | on every "show current" — the only tier a current-version read touches |
| **f1** (warm) | the accumulator: one refPrefix frame anchored on f0's record, absorbing each spilled-out old head | replaced on every update; highest write volume | only on history requests |
| **cold** | sealed former accumulators, immutable, chained newest→oldest | append-only, at seal time | only when walking deep history |

Flow: a new version becomes the new f0; the old f0 spills into f1; when f1 crosses a
size threshold it **seals** — its bytes move verbatim into a cold frame (no re-encode,
because its refPrefix anchor is unchanged) and a fresh f1 starts.

Each frame is refPrefix-compressed against the next-newer frame's relevant record: f1
against f0's record, the newest cold frame against f1's oldest record, and each older
cold frame against the next-newer cold frame's oldest record. Those anchors are fixed
at encode time, which is why sealing can move f1's bytes into cold unchanged.

## 4. On-disk layout

**Frame** (every frame, all tiers): a 20-byte header followed by the zstd payload.

```
[ u64 chain_id ][ u64 next_pointer ][ u32 zstd_len ][ zstd bytes ]
```

`next_pointer` packs `(u32 file_id, u32 offset)` and links the chain: f0 → f1 →
newest cold → … → older cold, with `(0,0)` marking the end. The pointer precedes the
zstd bytes so a reader chasing the chain never decodes past the frame it wants.

**Index**: a fixed-size, mmap'd flat array. Entry = `(file_id, offset)` pointing at
the chain's f0, addressed by `chain_id` arithmetic (`base + chain_id * 8`) —
no hash map, no B-tree. Sized by **entity count, not history depth**, so deep
histories do not grow it; about 480 MB for ~60M chains (8 bytes × chain count). Fixed-size entries
are load-bearing: they make the addressing pure arithmetic.

**Files**:
- `f0/file-NNNN`, `f1/file-NNNN`: directories of ~1 GiB append-only files.
- `cold/cold`: a single file per depot instance, so deleting a whole instance is one
  `unlink`.

## 5. Operations

- **append(chain_id, new_f0, new_f1, seal_old_f1)** — the caller pre-encodes the new
  frames. On a seal, the old f1's bytes are first copied verbatim to the cold file and
  fsync'd. Then write the new f1, then the new f0, then **flip the one index entry** —
  that flip is the atomic commit. Finally bump the in-memory dead-byte counter on the
  files that held the old f0/f1.
- **read current** — `index[chain_id]` → f0 → one decode.
- **read history** — walk f0 → f1 → cold via `next_pointer`, decoding each.
- **delete instance** — remove its `cold/cold` file (and its f0/f1 files).
- **GC / eviction** — when a file's `dead / len` exceeds `eviction_dead_ratio`, walk
  it, copy live frames to the tier's current tail, patch the single referrer (the
  index for an f0, the owning f0's header for an f1), fsync, unlink. No allocator, no
  free list.

## 6. Durability

append → fsync → flip, and nothing else. No journal, CRC, magic number, fsck, or
tombstone bitmap. Crash contract: a flushed write is durable; an unflushed write
carries no promise. Integrity of the byte stream is the storage/transport layer's
job. The robustness is in the layout — an append that has not been committed by the
index flip is simply not visible — not in recovery scaffolding on top of it.

## 7. strpool: string → dense id

A shard is one file:

```
[ zero+ zstd frames ][ plaintext tail ][ 8-byte footer ]
footer = { u32 tail_len, u32 entry_count }
```

- Each frame's decompressed payload, and the plaintext tail, are concatenated
  **null-terminated** byte strings. `tail_start = file_size - 8 - tail_len`.
- **Ids are positional**: a string's local id is its insertion ordinal, never stored
  on disk — recomputed by counting separators on iteration. The global id is
  `(local << shard_bits) | shard_id`; the shard is chosen by `hash(normalized) %
  shard_count`.
- **No built-in reverse (bytes → id) index**. A caller that needs dedup-on-insert
  supplies it (e.g. a SQLite side table).
- **Seal**: when the plaintext tail exceeds a threshold, compress it into one frame
  and rewrite `frames || new_frame || footer{tail_len: 0, entry_count}` via
  tmp-file + rename + fsync. `entry_count` is preserved, so **ids are stable across
  seals**.

Uses: turn string keys into the dense `chain_id` the depot index needs; intern
repeated metadata strings (contributor, comment, …) so they are stored once and
referenced by id.

## 8. Why this shape

- **f0 solitary + per-chain dict** → the common operation, reading the current
  version, is one small decode; no multi-revision block to scan.
- **f1 refPrefix + seal-verbatim** → an update re-encodes only f0 and f1; cold frames
  are never touched, and sealing is a byte copy.
- **flat index sized by entity count** → the index stays bounded no matter how deep
  any single history grows.
- **cold = one file per instance** → instance deletion is O(1).
- The compression discipline — per-chain trained dict, refPrefix chaining, sealing —
  **is** the architecture. A depot whose on-disk size matches its uncompressed input
  has not rendered this design, whatever its tests say.

## 9. Open decisions (size against the real corpus; do not guess)

- **Metadata interning** — the prior intent was to intern contributor names, edit
  comments, and timestamps through strpool; the shipped code interned only titles and
  stored the rest inline. Decide which fields are id-referenced vs inline.
- **Index entry width** — the design uses 8 bytes `(u32 file_id, u32 offset)`, which
  caps a tier file at 4 GiB and the file count at 2³². Widen to 16 only if a depot's
  scale would breach those caps.
- **Seal threshold and dictionary training** — f1 seal size, and how/when the
  per-chain dictionary is trained, want tuning against measured revision sizes and
  similarity, not assumed constants.
- **Integer endianness on disk** — fix it explicitly if the artifact must move
  between architectures.
- **Corpus magnitudes** — chain count, revisions-per-entity distribution, and record
  sizes drive §4 sizing; measure them on the target corpus before committing widths
  and thresholds.
