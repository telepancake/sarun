# Wikipedia mirror performance contract

This document models the asymptotic and pass-count costs of the current
Wikipedia mirror design. Performance is a design property, but no individual
counter is a substitute for design judgment. A sequential extra pass may be a
good trade for simpler recovery and bounded descriptors; a nominally one-pass
design may be worse if it retains enormous sidecars, adds fragile state, or
turns sequential I/O into random I/O.

The requirements below describe the selected design and its intended operating
envelope. A change may revise them when it compares total I/O, memory,
descriptor use, implementation complexity, recovery behavior, and the
expected workload at enwiki scale. What is not acceptable is an accidental or
unmeasured cost hidden in a passive path.

The key words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## 1. Quantities

- `D`: compressed upstream source bytes.
- `X`: typed intermediate archive bytes.
- `A`: installed archive bytes.
- `B`: uncompressed typed record bytes.
- `F`: final data-frame count, approximately `A / 128 KiB`.
- `R`: physical archive range-file count.
- `P`: page count.
- `S`: serialized title-history fact bytes.
- `C`: current-title candidate assignment bytes; `C <= 64P`.
- `U_c`: compressed sorted incremental-update archive bytes.
- `U_r`: raw typed bytes in that incremental-update stream.
- `N`: logical incremental-update source count.
- `H`: base-frame count whose entity intervals intersect an update.
- `A_H`: compressed payload bytes in those intersected base frames.
- `B_H`: raw typed bytes in those intersected base frames.
- `f = A_H / H`: mean compressed intersected-frame payload bytes.
- `A_a`: physical base range bytes containing those frames.
- `K`: configured final-frame compression workers.

The constants above describe data, not memory allocations. In particular,
`A`, `B`, `P`, and `F` MUST NOT determine resident memory linearly.

## 2. Initial import

Each upstream source byte MUST be downloaded once in a successful,
non-resumed attempt. A transport resume MAY repeat the server-selected overlap
but MUST retain and report the prior byte frontier.

Each source stream MUST be decompressed and parsed once into typed records.
Ordinary nonoverlapping content parts produce one typed target each. Inputs
that genuinely overlap MAY require an external merge pass; nonoverlap MUST NOT
pay that cost. One overlap group with `N` physical inputs uses a bounded
24-way merge hierarchy and therefore at most `ceil(log_24 N)` merge levels.
This hierarchy is confined to target materialization, the explicit `merge`
command, and the stale-update case described in section 5; ordinary full-build
assembly MUST NOT route all of its independent inputs through it.

Final assembly MUST:

1. read each typed source record once;
2. perform one coalescing ordered merge;
3. encode each final record once;
4. compress each final data frame once; and
5. write each final compressed byte once.

The reference prefix is sampled from newest text-bearing page revisions.
Repacking records written before the prefix exists is bounded by the configured
sample capacity, currently 150 MiB, independent of `A`.

Final frames are independent compression jobs. They MUST be submitted to an
ordered bounded pool rather than relying on zstd's internal job pool: a normal
128 KiB compressed frame is smaller than the internal job size and otherwise
uses only one core. Results MAY complete out of order but MUST be written in
record order.

Peak final-compression memory is:

```text
O(K * normal_raw_frame + largest_indivisible_entity + prefix)
```

There MUST NOT be an archive-sized raw buffer. A single page or other entity is
indivisible and may exceed the normal frame target; that is the only
data-dependent unbounded term.

## 3. Frame and title directories

The frame directory is a fixed-width stream over final frame headers. Building
it costs `O(F)` 64-byte header reads, constant working memory, and at most one
open data segment at a time. Index construction MUST consume that directory
without creating a second `Vec<FrameLocation>` proportional to `F`.

Title-history facts are externally sorted with run size `M` and merge fan-in
`Q`. If `p = ceil(log_Q(ceil(S/M)))`, their sequential scratch I/O is bounded
by:

```text
(3 + 2p)S
```

Current-title candidates are encoded as 32-byte assignments, externally sorted
by `(page_index, rank)`, and sequentially joined with the 8-byte page-ID stream
to write the 40-byte page table. This phase MUST perform zero per-page or
per-candidate seeks. Its additional sort I/O obeys the same formula with `C`
in place of `S`.

The final 16-byte title entries have the same external-sort bound. All run
merges MUST have bounded fan-in; a source, run, frame, or physical range MUST
NOT imply one simultaneously retained file descriptor.

## 4. Resume

Committed source targets MUST be reused without network transfer or parsing.
Assembly checkpoints MUST bind:

- the last sealed entity range;
- the corresponding source cursors;
- the title-projection prefix; and
- the immutable reference prefix.

Resume may replay the current unsealed bounded frame and current projection
run. It MUST NOT read source records from entity zero, rebuild a completed
projection prefix, or redownload a committed target.

## 5. Incremental update

Daily sources are exposed as lazy sequential groups. Merge fan-in is the number
of logical groups, not the number of physical dump files. The normal
three-day-overlap update has about seven logical inputs and merges directly.
When `N > 64` (roughly more than 61 daily runs behind after history and
manifest inputs), a bounded 64-way hierarchy adds
`ceil(log_64(N)) - 1` full intermediate tail write/read and recompression
passes. Its sequential I/O is `O(U_c * ceil(log_64(N)))` and its record
decode/recompression CPU is `O(U_r * ceil(log_64(N)))`; it never rereads the
base, and open descriptors remain bounded. This explicit stale-mirror cost is
preferred to retaining one descriptor or one potentially oversized decoded
frame per daily run.

The ordered update merge classifies base frames by entity interval:

- a frame disjoint from `U` is copied as its validated header and compressed
  payload; it incurs zero record decode and zero recompression;
- a frame intersecting `U` is decoded once, coalesced with its bounded update
  slice, and recompressed once;
- new tail entities are encoded and compressed once.

Thus update compressed-input I/O is `O(U_c + A_H)` and record merge/decode
CPU is `O(U_r + B_H)`, not `O(A)`.
Changed and new output frames are independent jobs in the same bounded ordered
compression pool used by initial assembly. Precompressed copied frames occupy
positions in that output sequence as `(descriptor, offset)` pairs, not
payload-sized buffers. One output owner writes both kinds in entity order;
queued frame state is bounded by `K` plus the current indivisible entity.
Physical publication
may rewrite `A_a` because range files are immutable, but MUST NOT decode or
recompress unchanged frames in those ranges.

The current lvwiki archive has 5,152 frames and 543,093,470 compressed payload
bytes, so observed mean `f` is 105.4 KiB. A tail distributed across 1,000 frame
ranges therefore makes about 103 MiB of compressed base input eligible for
decode/recompression even if `U` itself is only tens of MiB; serial
output-frame compression is not an acceptable enwiki-scale design.

Peak update storage before durable publication is:

```text
old installed A + sorted U_c + replacement A_a + bounded sort/index scratch
```

Old installed bytes remain authoritative until the replacement generation and
its index are durable. Scratch and displaced generations are cleaned only by
the lifecycle rules.

## 6. Serving and publication

Serving maps the title/frame index and loads the reference prefix. A range-set
reader retains one shared-locked archive-directory descriptor and an
eight-entry segment-file LRU. Its descriptor bound is:

```text
1 root + 8 cached segments + active request files + fixed control files
```

It MUST NOT retain one descriptor per physical range.

Segment files are opened relative to the retained immutable generation
directory descriptor. Publication atomically replaces only the stable index
selector; generation directories are never renamed out from under readers.
Cleanup obtains an exclusive nonblocking lease on an explicitly displaced
generation directory; if any reader retains a shared lease, deletion is
deferred.

Passive startup MUST NOT scan archive records. It may validate fixed headers,
map indexes, and load the reference prefix. Page lookup decodes only the
selected frame; history/current lookups share the same newest-revision-not-
newer-than-time path.

## 7. Required regression checks

Tests MUST cover:

- ordered output from more independent compression jobs than workers;
- no entity split across parallel-compressed frames;
- whole-set frame-directory offsets across segment boundaries;
- zero random page-table writes in title projection;
- bounded open descriptors with more ranges than the process limit;
- a reader opened before selector replacement lazily opening an unread segment
  from its retained old generation directory;
- cleanup deferral while that reader exists and eligibility after it drops;
- update copying disjoint compressed frames byte-for-byte; and
- update decoding/recompressing only intersecting frames.

Large-wiki estimates MUST be derived from this model before a run. Measurements
then validate constants and expose violated assumptions; they are not a
substitute for the model. The estimates are used to choose among designs, not
to reward code for mechanically minimizing one row of the table.

## 8. Enwiki-shaped planning evaluation

The following is a conservative capacity-planning envelope, not an observation
or an ETA. It deliberately does not assert a compression ratio. Before an
actual run, discovery replaces `D` and the source count, while a small typed
sample replaces the provisional `X`, `A`, `S`, and normal raw-frame values.

| Quantity | Planning assumption |
|---|---:|
| upstream compressed `D` | 25 TiB |
| typed intermediates `X` | 30 TiB |
| installed archive `A` | 10 TiB |
| pages `P` | 70 million |
| title facts `S` | 100 GiB |
| final title entries `I` | 100 million |
| physical range target | 128 GiB |
| compression workers `K` | 10 |
| normal raw bytes per final frame | 1 MiB |
| largest indivisible entity | unknown; measured and reported separately |

These assumptions evaluate as follows:

| Resource or pass | Evaluated envelope |
|---|---:|
| upstream requests | discovered source count + bounded metadata requests |
| network body bytes | 25 TiB, excluding attributed transport-resume overlap |
| source decompression | 25 TiB compressed input, exactly one pass |
| typed intermediate writes | 30 TiB |
| final assembly reads | 30 TiB, exactly one pass |
| final archive writes | 10 TiB, exactly one pass |
| final frames `F` | 83,886,080 |
| fixed frame directory | 5.00 GiB + 128 bytes |
| physical ranges `R` | 80 |
| page-ID stream | 0.52 GiB |
| 40-byte page table | 2.61 GiB |
| maximum 32-byte candidate stream `C` | 4.17 GiB |
| candidate-sort sequential I/O | at most 29.2 GiB |
| title-fact sort levels | 3 at 64 MiB runs and fan-in 32 |
| title-fact sequential I/O | at most 900 GiB |
| 16-byte final title entries | 1.49 GiB |
| final-entry sort sequential I/O | at most 7.45 GiB |
| prefix-distillation peak | about 316 MiB + allocator/workspace overhead |
| final compression + projection peak | about 92 MiB + largest entity + sorter workspace overhead |
| final merge descriptors | at most 64 logical sources + fixed output/control descriptors |
| serve descriptors, no active request | 1 root + 8 cached segments + fixed index/control descriptors |

The 1 MiB normal raw-frame assumption affects memory only. If sampling reports
2 MiB instead, the ten-worker raw-job term grows from 10 MiB to 20 MiB; archive
pass counts and disk bounds do not change. The largest entity remains explicit
because no correct frame writer may split it merely to satisfy a memory guess.

For an enwiki-shaped update whose page IDs are broadly distributed, it is
reasonable to expect nearly every one of the 80 physical ranges to be touched.
The best physical case is a pure new-ID tail: zero existing range bytes
rewritten. A one-range update reads and replaces about 128 GiB physically. The
worst distributed case reads and replaces 10 TiB, and may temporarily require
the old 10 TiB plus up to 10 TiB of replacements before publication. Even in
that worst physical case, disjoint frames are copied compressed; record decode
and recompression remain proportional to `B_H + U_r`, not 10 TiB; compressed
input I/O remains proportional to `A_H + U_c`.

Against these assumptions, allocating from `F`, opening `R` files
simultaneously, scanning `A` during ordinary startup, replaying `X` on resume,
or recompressing the disjoint portion of a touched update range is a serious
regression unless a concrete benefit justifies the evaluated cost. The table
exists to make that tradeoff visible before execution, not to make one metric
the design objective.
