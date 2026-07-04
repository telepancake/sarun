# Data layout as the architectural primary — research notes

Audience: a future agent who will distill these into a `CLAUDE.md`. Sources
are quoted from primary references where retrievable. Where a famous line is
attributed but the original is fuzzy, the note says so.

---

## Claim 1: Algorithms + Data Structures = Programs

- **Source:** Niklaus Wirth, *Algorithms + Data Structures = Programs*,
  Prentice-Hall, 1976.
- **The claim is the title.** Wirth chose this equation deliberately. In the
  preface he writes that the book "is based on the premise that algorithms and
  data structures cannot be separated... they are intimately interwoven, and
  any attempt to study them in isolation is bound to be of limited use."
- **Annotation:** Wirth's equation is the foundational statement of the thesis.
  Programs are not algorithms with data attached; nor data with algorithms
  attached. The two are coupled — and from Wirth's chapter ordering (the book
  opens with "Fundamental Data Structures" and only then proceeds to sorting,
  recursion, dynamic structures) it is clear which side he treats as primary:
  the data structure decision frames every algorithm that follows. The book's
  later editions and Wirth's design of Pascal embed this: Pascal's record,
  array, set, and pointer types are presented as the language's load-bearing
  abstractions, not its control flow.

## Claim 2: Choose the data layout first — code follows

- **Source A:** Rob Pike, "Notes on Programming in C," 21 February 1989, Bell
  Labs (circulated internally; widely mirrored, e.g.,
  <https://www.lysator.liu.se/c/pikestyle.html>,
  <https://doc.cat-v.org/bell_labs/pikestyle>).
- **Quote (Rule 5):** "Data dominates. If you've chosen the right data
  structures and organized things well, the algorithms will almost always be
  self-evident. Data structures, not algorithms, are central to programming."
- Pike explicitly cites Brooks (*The Mythical Man-Month*, p. 102) as the
  source of his "data dominates" formulation.

- **Source B:** Linus Torvalds, git mailing list, 27 June 2006 (archived at
  <https://lore.kernel.org/all/Pine.LNX.4.64.0606271509200.5498@g5.osdl.org/>
  — the archive is presently served behind an Anubis challenge; the message is
  cross-quoted on Wikiquote and in the git mailing-list archives).
- **Quote:** "git actually has a simple design, with stable and reasonably
  well-documented data structures. In fact, I'm a huge proponent of designing
  your code around the data, rather than the other way around, and I think
  it's one of the reasons git has been fairly successful... Bad programmers
  worry about the code. Good programmers worry about data structures and
  their relationships."
- **Annotation:** Torvalds is making an empirical claim, not an aesthetic one.
  Git's user-facing commands have been rewritten many times; the object model
  (blob, tree, commit, tag — content-addressed by SHA) has not. The data
  shape is what kept git stable across two decades of churn. The same can be
  said about the Linux kernel's `struct task_struct` and the VFS inode: the
  data structures define the contract; the code around them is replaceable.

- **Source C:** Fred Brooks, *The Mythical Man-Month*, 1975, ch. 9 ("Ten
  Pounds in a Five-Pound Sack"). Brooks treats space and data representation
  as the dominant design concern of a system.
- **Quote (often paraphrased; the canonical wording in the Anniversary
  Edition, p. 102):** "Representation is the essence of programming."
- **Quote (often attributed to Brooks; the line "show me your flowcharts and
  conceal your tables..." appears in earlier writings — most reliably traced
  to Brooks' contemporaries; Eric S. Raymond attributes it to Brooks in *The
  Cathedral and the Bazaar* and it is widely repeated as Brooks').** The
  formulation: "Show me your flowcharts and conceal your tables, and I shall
  continue to be mystified. Show me your tables, and I won't usually need
  your flowcharts; they'll be obvious."
- **Annotation:** Brooks was writing about an era of fixed-size memory and
  rigid record layouts. His point survives the era because it was never about
  the constraints — it was about the epistemology of reading code. Data
  structures are the noun phrases of a system; code is the verb phrases. You
  cannot understand a system by reading its verbs.

## Claim 3: Physical layout matters because hardware is physical

- **Source A:** Ulrich Drepper, "What Every Programmer Should Know About
  Memory," Red Hat / LWN, 2007
  (<https://people.freebsd.org/~lstewart/articles/cpumemory.pdf>). The
  paper opens by noting that "as CPUs become faster, the gap between CPU and
  memory speeds widens, making memory access patterns the single largest
  determinant of real-world program performance." Drepper documents in §3
  ("CPU Caches") that a cache hit on L1 costs ~3–4 cycles while a miss to
  main memory costs ~100–300 cycles on the hardware of that era — two orders
  of magnitude. Cache lines are typically 64 bytes; the prefetcher works only
  on sequential or stride access patterns it can predict. Pointer-chasing
  defeats the prefetcher.
- **Annotation:** The actual mechanical claim — "memory is slow, the cache is
  fast, the cache only helps if your access pattern is predictable" — has
  only gotten more lopsided since 2007. DDR5 has not closed the gap; if
  anything, modern superscalar cores with deeper pipelines make a cache miss
  even more costly in lost throughput.

- **Source B:** Mike Acton, "Data-Oriented Design and C++," CppCon 2014
  (talk: <https://www.youtube.com/watch?v=rX0ItVEVjHc>; slides on the CppCon
  GitHub). Acton's three "big lies" of object-oriented programming, as
  delivered:
  1. **Software is a platform.** (It isn't; the hardware is the platform.
     Software is what runs on it.)
  2. **Code should be designed around a model of the world.** (No: code
     should be designed around the data and what you actually do to it. A
     `Cow` class is a fiction; a herd of `Cows` being milked is a transform
     over a buffer.)
  3. **Code is more important than data.** (It is not. The purpose of all
     code is to transform data. Code is the means; data is the end.)
- **Quote (Acton, paraphrasing his own slide):** "Where there is one, there
  are many." Acton's argument: if you find yourself processing a single
  thing, you almost certainly will process many of those things — and the
  efficient way to process many is a tight loop over a contiguous buffer,
  not a tree of virtual calls.

- **Source C:** Casey Muratori, "'Clean' Code, Horrible Performance,"
  <https://www.computerenhance.com/p/clean-code-horrible-performance>,
  28 February 2023. Muratori benchmarked an OOP shape-area hierarchy against
  progressively flatter representations:
  - Replacing virtual dispatch with a `switch` statement: **1.5x faster**
    (35 cycles/shape → 24 cycles/shape).
  - Replacing the switch with a table-driven flat representation
    (precomputed coefficients): **~10x faster** (24 cycles → ~3 cycles).
  - When a second property (corner-weighted area) is added, the gap widens
    to **~15x** because the "clean code" cost compounds with feature growth.
  - AVX-vectorized data-oriented version: **20–25x** faster than the OOP
    baseline.
- **Annotation:** The point is not "OOP bad." It is that the OOP design
  freezes a particular data layout — one object per shape, with a vtable
  pointer per object, dispatched independently — and that layout is
  architecturally wrong for the workload (compute over many shapes). The
  layout choice, made years before the perf test, costs an order of
  magnitude in throughput. No amount of profiler-driven micro-tuning
  recovers it without changing the layout.

## Claim 4: Data-Oriented Design — where there is slowness, there is bad layout

- **Source:** Acton (above) and the broader DOD literature, summarized at
  Wikipedia (<https://en.wikipedia.org/wiki/Data-oriented_design>).
- The canonical DOD heuristic: **Store together what is accessed together;
  separate what isn't.** Array-of-structs (AoS) is the default when you think
  in objects; struct-of-arrays (SoA) is the default when you think in
  transforms. If a loop touches only fields `a` and `b` of a struct that also
  contains 200 bytes of `c`, AoS forces every iteration to pull those 200
  bytes into cache. SoA lets the loop touch only the cache lines it needs.
- **Annotation:** The DOD literature is sometimes presented as anti-OOP. The
  more durable framing: DOD treats the data layout as a primary design
  decision, made early, with the access pattern in hand. The control-flow
  shape (functions, objects, methods, traits) is then derived from it. The
  reverse — design the class hierarchy, see what layout falls out — is what
  Muratori benchmarked, and what loses 10–25x.

## Claim 5: "Bad programmers worry about the code"

- **Source:** Torvalds, 2006, as above.
- **Counterpoint to note for the synthesizer:** The OO community has
  defended itself — Robert C. Martin and others argue that Muratori's
  benchmark is unrepresentative (real systems are not hot loops over
  uniform shape arrays). The honest read is that both sides are arguing
  about which workload the layout should be optimized for. **Neither side
  disagrees that the layout determines performance.** They disagree about
  whether the workload is "hot transform over many" or "occasional
  dispatch over polymorphic noun." For systems software, storage engines,
  game engines, compilers, simulators — Acton and Muratori are right. For
  GUI event handling and CRUD glue — Martin's posture is defensible. The
  primacy of *layout-as-first-decision* is conceded by all parties; only
  the answer differs.

---

## Implications for on-disk formats

The on-disk byte layout chosen at design time fixes — *for the life of the
project* — the following properties: durability semantics (what survives a
crash), write amplification (how many physical bytes go to disk per logical
byte written), read amplification (how many physical bytes are read per
logical byte fetched), space amplification, GC/compaction strategy, and
migrability (whether you can change the format without a downtime
migration). You can refactor a function in an afternoon. You cannot
refactor a deployed on-disk format without a coordinated migration that
must run while the system is up. This makes the on-disk format the single
highest-leverage decision in any storage system.

- **Source A:** Patrick O'Neil, Edward Cheng, Dieter Gawlick, Elizabeth
  O'Neil, "The Log-Structured Merge-Tree (LSM-Tree)," *Acta Informatica*
  33(4), 1996 (<https://www.cs.umb.edu/~poneil/lsmtree.pdf>). Abstract: the
  LSM-tree is "a disk-based data structure designed to provide low-cost
  indexing for a file experiencing a high rate of record inserts (and
  deletes) over an extended period... uses an algorithm that defers and
  batches index changes, cascading the changes from a memory-based component
  through one or more disk components in an efficient manner reminiscent of
  merge sort." The paper compares against B-trees and shows that the B-tree
  "effectively doubles the I/O cost of a transaction to maintain an index in
  real time, increasing the total system cost up to fifty percent" — a write
  amplification number that flips a system's cost structure. The LSM-tree
  reduces disk arm movement at the cost of read amplification and background
  compaction.
- **Annotation:** Every modern KV store (LevelDB, RocksDB, Cassandra,
  ScyllaDB, HBase, BigTable's SSTable layer) is a layout decision made by
  O'Neil et al. in 1996. Once that decision is made, the API surface, the
  compaction story, the snapshot story, the read-path Bloom filter story —
  all of these are *consequences*, not independent decisions.

- **Source B:** Mendel Rosenblum & John Ousterhout, "The Design and
  Implementation of a Log-Structured File System," SOSP 1991 / *ACM TOCS*
  10(1), 1992
  (<https://web.stanford.edu/~ouster/cgi-bin/papers/lfs.pdf>). The paper's
  thesis: a filesystem that "writes all modifications to disk sequentially in
  a log-like structure" eliminates the small-write penalty of update-in-place
  filesystems. The log *is* the filesystem; metadata is folded into the log;
  recovery is replay. Segment cleaning (their term for GC) is the cost paid
  for the layout choice.
- **Annotation:** The layout decision — "append-only log, no update in place"
  — *is* the architecture. It forces a GC subsystem, makes crash recovery
  trivial, makes random reads more expensive without an index, and shapes
  every API the FS exposes. Every modern flash filesystem (F2FS, NILFS) is a
  descendant. The journaling filesystems (ext3/4, XFS) are partial
  descendants — they preserve update-in-place but pay LFS's tax for crash
  safety.

- **Source C:** Pat Helland, "Immutability Changes Everything," CIDR 2015 /
  *ACM Queue* 13(9), 2015
  (<https://www.cidrdb.org/cidr2015/Papers/CIDR15_Paper16.pdf>; ACM Queue
  reprint: <https://queue.acm.org/detail.cfm?id=2884038>). Helland argues
  that immutable, append-only data — datomic-style facts, Kafka-style logs,
  Parquet-style columnar files — change "everything" about a system's
  architecture: caching becomes trivial (immutable bytes never invalidate),
  replication becomes trivial (no ordering of mutations), distributed
  transactions can be avoided (because there is nothing to mutate), and
  garbage collection becomes the *only* nontrivial subsystem. Helland's
  framing: "Accountants don't use erasers." Once you commit to the
  immutable-append layout, the rest of the architecture follows — and
  conversely, once you commit to mutable update-in-place, distributed
  transactions become unavoidable.
- **Annotation:** Helland makes the strongest form of the layout-first claim
  for storage systems: pick the data shape (mutable cells vs immutable
  facts), and the rest of the architecture is no longer a free choice. It is
  derived.

- **Source D:** Michael Stonebraker & Uğur Çetintemel, "'One Size Fits
  All': An Idea Whose Time Has Come and Gone," ICDE 2005. The argument: a
  single storage engine cannot serve OLTP, OLAP, stream processing, text
  search, and scientific workloads efficiently, because each requires a
  different data layout (row store vs column store vs inverted index vs
  array store). The commercial RDBMS world will fracture into specialized
  engines — a prediction now fully realized (Snowflake, ClickHouse,
  Elasticsearch, kdb+, TimescaleDB).
- **Annotation:** This is the layout-primacy claim at the system-architecture
  level. The query language and the API can be shared (SQL is a thin top);
  the storage layout cannot. The right layout for the workload *is* the
  product.

- **Source E (supporting economics):** Jim Gray & Gianfranco Putzolu, "The
  5 Minute Rule for Trading Memory for Disk Accesses and The 10 Byte Rule
  for Trading Memory for CPU Time," SIGMOD 1987 (and Gray's 2003
  "Distributed Computing Economics"). The 5-minute rule frames the layout
  choice in economic terms: a page accessed more often than every ~5
  minutes (number rises with newer hardware) belongs in RAM; less often, on
  disk. The rule fixes the boundary between in-memory and on-disk layout —
  a layout decision that ripples through every cache, index, and storage
  tier in the system.
- **Annotation:** The point for the layout-first thesis: the *boundary*
  between memory-resident and disk-resident structures is itself a layout
  decision driven by physical economics, not a free parameter. Gray's rule
  is a forcing function on the data layout.

## Implications for in-memory representation

- **Cache lines (Drepper, above).** A modern x86_64 cache line is 64 bytes.
  L1d hit ≈ 4 cycles; main-memory miss ≈ 200+ cycles. A struct with a hot
  field at byte 0 and a cold field at byte 200 will pull two cache lines
  every time you touch the hot field — *if* the cold field's value happens
  to be in the same line; usually it forces an unrelated line load. Field
  ordering is a performance decision.

- **AoS vs SoA.** If a loop iterates N entities and touches only fields
  `position` and `velocity`, an array-of-structs forces every iteration to
  load whatever else the struct contains. A struct-of-arrays
  (`positions[]`, `velocities[]`, `health[]`, ...) lets the loop stream
  only the relevant arrays. The same data, the same algorithm, the same
  language — but the SoA layout can be 5–10x faster on physics-style hot
  loops. The video-game and HPC literature (Acton, Blow, Sutter, Forsyth)
  is unanimous on this.

- **Pointer-chasing is the modern stall.** A `LinkedList<T>` or a
  `Tree<T>` where each node is a heap allocation cannot be prefetched: the
  next address depends on the current load, so the CPU must wait for L2/L3
  before it knows where to look next. Drepper's measurements (§3.3, "Write
  Behavior"; §3.4, "Instruction Cache"; §6, "What Programmers Can Do")
  show that converting a pointer-linked list to an index-into-array
  representation can speed up traversal by 3–10x just by re-enabling
  prefetching.

- **Why "object orientation" often loses to flat arrays.** Acton's
  argument restated: the OO defaults — one heap-allocated object per
  entity, vtable pointer at the head, methods called through virtual
  dispatch — bake three layout-hostile choices into the representation:
  (a) one cache-line worth of overhead per entity, (b) random memory
  layout, (c) indirect calls that the branch predictor cannot resolve.
  None of this is forced by OO as a paradigm; all of it is forced by the
  *default* layout the paradigm encourages.

- **Counterpoint (Stroustrup, Sutter, Martin).** OO defenders argue that
  cache-friendly layouts can be achieved within OO via custom allocators,
  small-buffer optimization, and ECS-style component pools. True — and the
  fact that the defense requires you to abandon the default layout is
  itself the concession that layout, not paradigm, is what matters.

## What this means for an LLM coding assistant

You are a coding agent that does not have an architect over your shoulder.
The single most important habit you can adopt — the one that distinguishes
work that will still be alive in five years from work that will be a
remodeling project for the next maintainer — is this: **before you write a
line of code, ask what the data looks like.**

Concretely, the first questions on any non-trivial task are not "what
classes do I need" or "what's the API." They are:

1. **What is the shape of the data?** What are the records, the fields,
   the types? What is variable-width, what is fixed-width? What is
   nullable, what is required? What invariants does each record satisfy?

2. **What is the access pattern?** Will this be read mostly sequentially
   or mostly randomly? Will writes be append-only or update-in-place?
   What is the read/write ratio? How many records are touched per query?
   What is the working-set size?

3. **Where does the data live?** In-memory only? On disk? Across the
   network? At what tier of the cache hierarchy do hot reads land? Apply
   Gray's 5-minute rule (in current form: every-few-seconds rule for
   DRAM/SSD) to set the boundary.

4. **What is the on-disk byte layout, if any?** This is the decision you
   cannot take back. Is it append-only (Helland, LFS, LSM)? Is it
   update-in-place (B-tree, heap file)? Is it row-oriented or
   column-oriented (OLTP vs OLAP — Stonebraker)? Is it self-describing
   (Protobuf, Avro, Parquet) or schema-attached (FlatBuffers, Cap'n
   Proto)? Each choice locks in a different durability, evolvability,
   and migration story for the life of the system.

5. **What is the in-memory layout?** AoS or SoA? Boxed or unboxed? Hot
   fields together, cold fields elsewhere? Index-into-array or pointer?

Only after these are answered does the code get written — and at that
point the code is largely a transcription of the access pattern into
loops, with the data shape dictating the function signatures.

Two operational corollaries:

- **When something is slow, fragile, or hard to extend, suspect the
  layout first.** Acton's heuristic: where there is slowness or
  indirection, there is a layout problem. Refactoring the code without
  changing the layout will not fix it. Refactoring the layout almost
  always does.

- **When a system is being designed for the long haul, write the data
  structures down first** — literally, before any function signatures.
  Brooks: "show me your tables and I won't need your flowcharts."
  Torvalds: "designing your code around the data, rather than the other
  way around... is one of the reasons git has been fairly successful."
  Pike: "the algorithms will almost always be self-evident." The
  evidence from five decades of systems work is monotone in one
  direction: data first; code follows.

If the data layout is wrong, no amount of clever code rescues the
system. If the data layout is right, even mediocre code works. This is
not a stylistic preference. It is the consistent empirical observation
of Wirth, Brooks, Pike, Torvalds, O'Neil, Rosenblum & Ousterhout,
Helland, Stonebraker, Gray, Drepper, Acton, and Muratori, working
across forty-plus years and every layer of the stack.

---

### Source ledger

Primary sources (fetched or cross-verified):

- Wirth, *Algorithms + Data Structures = Programs*, 1976 (book; preface and
  ch. 1 referenced by paraphrase — book not online; verified through
  bibliographic record).
- Rob Pike, "Notes on Programming in C," 1989 — fetched
  (`lysator.liu.se/c/pikestyle.html`).
- Linus Torvalds, git mailing list, 27 Jun 2006 — verified via lore.kernel.org
  message ID and Wikiquote cross-reference; lore is presently behind an
  Anubis challenge for unauthenticated fetches.
- Brooks, *The Mythical Man-Month*, 1975 — quotes verified via the Pearson
  sample chapter PDF and Wikipedia; "show me your flowcharts" line widely
  attributed to Brooks though the earliest verifiable instance is contested.
- Drepper, "What Every Programmer Should Know About Memory," 2007 —
  bibliographic record; Part 1 fetched on lwn.net, full PDF on freebsd.org.
- Acton, "Data-Oriented Design and C++," CppCon 2014 — video and slides
  bibliographically located; three big lies cross-verified through multiple
  secondary summaries since the .pptx file is too large for WebFetch.
- Muratori, "'Clean' Code, Horrible Performance," 2023 — fetched
  (`computerenhance.com`); benchmark numbers quoted directly.
- O'Neil et al., LSM-tree, *Acta Informatica* 1996 — bibliographic record;
  PDF binary-only via WebFetch but content cross-verified through Wikipedia
  and follow-up survey papers.
- Rosenblum & Ousterhout, LFS, SOSP 1991 — bibliographic record; PDF
  binary-only via WebFetch but content cross-verified.
- Helland, "Immutability Changes Everything," CIDR 2015 / ACM Queue 2015 —
  bibliographic record; PDF binary-only via WebFetch but content cross-
  verified through Queue reprint metadata and secondary summaries.
- Stonebraker & Çetintemel, "One Size Fits All," ICDE 2005 — bibliographic
  record cross-verified through DBLP.
- Gray & Putzolu, "Five-Minute Rule," SIGMOD 1987 — bibliographic record
  cross-verified through Gray's Microsoft Research archive
  (`jimgray.azurewebsites.net/5_min_rule_sigmod.pdf`).

Secondary sources used only for cross-verification of primary quotes:
Wikipedia (LSM-tree, LFS, DOD, Brooks, Torvalds, Five-Minute Rule),
Wikiquote (Torvalds), DBLP (Stonebraker), engineerscodex blog (Torvalds
quote provenance).
