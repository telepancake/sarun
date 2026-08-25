# Full-build progress projection

This projection is live telemetry for a maturing subsystem. It is not durable
build state and cannot make a job running, failed, or ready.

## Ownership and identity

- The discovery coordinator creates `progress.bin` after `plan.json` commits.
- Each source worker owns one fixed source slot.
- The target publisher owns one fixed completion slot per durable target and
  writes it only after target rename and directory fsync.
- The assembly process owns the assembly slot.
- Every header and slot carries the canonical `PlanId`. Live source/assembly
  slots also carry the engine `RunId`. Foreign-plan and superseded-run writers
  are rejected.
- The engine is the only UI consumer. Job runtime/outcome comes from the run
  lifecycle inspector, never this file.

The file has a checksummed 4096-byte header and two checksummed 4096-byte banks
per slot. A writer replaces the older bank at its fixed offset. A reader picks
the newest valid bank, so a torn write invalidates only that bank and cannot
poison another slot.

## Events and projection transitions

| Current observation | Event | Projection result |
| --- | --- | --- |
| empty source slot | source update | record attempt and current structured counters |
| same attempt | duplicate/newer update | take monotonic counters and newest phase/item |
| earlier attempt retained | retry update | add network-attempt counters; retain maximum logical source/work counters |
| empty completion slot | target publish committed | record completed target and planned source bytes |
| completed target | duplicate completion | idempotent no-op value |
| any slot | foreign-plan update | typed rejection; no write |
| any bank | torn/corrupt write | ignore that bank; inspect the other bank |
| empty/old assembly slot | assembly observation | newest structured assembly counters |

Plan replacement initializes a new file atomically. Beginning another run of
the same plan atomically replaces only the header `RunId`, retaining completion
and cumulative network counters. Old source/assembly rows immediately stop
being eligible as live rows, and their writers cannot update the new run.
Telemetry can remain after a failed/interrupted run for diagnosis, but the
engine suppresses active rows/rate/quiet projections unless the authoritative
run lifecycle is live.

## Refresh cost

For `S` planned source fragments and `T` durable source targets:

```text
file bytes = 4096 + 8192 * (S + T + 1)
file opens = 1
bytes read = file bytes
peak decoded rows/slots = O(S + T)
directory reads = 0
receipt/index/archive opens = 0
hashing = bounded per 4096-byte occupied bank
```

At 2,000 source fragments and 500 targets the projection is 20,488,192 bytes.
For the 457-target enwiki planning envelope, with one source slot per target,
the fixed file/read is 7,499,776 bytes (7.15 MiB); every extra split source
adds exactly 8,192 bytes. A resume rewrites this bounded file once to commit
the new `RunId`; passive refresh remains one full sequential read. This trades
a few MiB of predictable read bandwidth for torn-slot isolation and zero
metadata/receipt/archive traversal.
Its cost depends only on planned slots, never compressed or uncompressed
Wikipedia corpus bytes. Worker updates read the fixed header and their two
banks, then write one 4096-byte bank; distinct source workers write disjoint
offsets.
