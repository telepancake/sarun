# Choosing data structures and layouts — methodology

`notes/02-data-layout.md` argues *why* the data layout is the architectural primary; this note is the *how* — the procedural complement that takes you from a domain requirement to a chosen representation. Sources quoted from primary references where retrievable; the ledger marks secondary attributions.

---

## The six steps

Done in order. They are not independent. Step 5 (choosing the structure) is mechanical once steps 1–4 are written down; if you find yourself at step 5 with steps 1–4 ill-defined, go back rather than guess. The order: (1) name every distinct piece of information the software holds; (2) describe the workflows that move and display it, and where it originates; (3) estimate magnitude and distribution; (4) derive the operations on the data; (5) match operations + magnitude to a representation; (6) recognise that the chosen shape *is* the contract — API, failure modes, concurrency, durability fall out of it. Skip a step and you are guessing at step 5; guess at step 5 and the rest of the system inherits the guess.

## Step 1: Conceptual data inventory

Before any function signature, before any class hierarchy, before any module boundary: write down every distinct piece of information the system will hold, transmit, or display. For each, three things.

- **What it is.** A textual name and a one-sentence description. Not a type. The textual sentence is the contract with the domain expert; the type comes later.
- **Possible values.** Type, cardinality, range, and value distribution. A `country_code` is not just `String` — it is one of ~250 ISO 3166 strings, distribution roughly proportional to internet traffic. A `transaction_amount` is not just `Decimal` — it is a non-negative value with a long-tailed distribution and a unit.
- **Relationships to other pieces.** Cardinality (1:1, 1:N, M:N), optionality (every order has a customer; not every customer has an order), and lifecycle dependence (does the child outlive the parent? Can it exist before the parent is created?).

The output is a written list of named items with their types and relationships. Not a diagram. Diagrams hide ambiguity behind boxes; prose forces you to either commit to a meaning or notice that you don't have one.

**Codd, 1970** ("A Relational Model of Data for Large Shared Data Banks," CACM 13(6)): the canonical answer to "what are the primitives of step 1?" — a *relation* is a set of tuples of named attributes; everything the system knows decomposes into relations. The data-independence claim is that the conceptual inventory is *separable* from the storage layout — a "declarative method for specifying data and queries" where users state what they want without specifying how the database stores it. Steps 1–4 are the declarative side; steps 5–6 are the storage side. Codd's contribution was the insistence that you do steps 1–4 *first*.

**Chen, 1976** ("The Entity-Relationship Model—Toward a Unified View of Data," *ACM TODS* 1(1):9–36) names the three primitives — *entity*, *relationship*, *attribute*. Direct quote (Wikipedia summary, preserving original wording): "The entity-relationship model adopts the more natural view that the real world consists of entities and relationships. It incorporates some of the important semantic information about the real world." Chen's load-bearing distinction for step 1: an entity's *identity* (what makes two records "the same thing") versus its *attributes* (the values attached to that identity).

**Kent, 1978** (*Data and Reality*, North-Holland): book-length treatment of the gap between domain understanding and data representation. The diagnosis: step 1 is harder than it looks — whether two records describe "the same thing" rarely has a single answer, and the answer often depends on the workflow (step 2).

**Evans, 2003** (*Domain-Driven Design*, Addison-Wesley): steelmanned, three contributions. *Ubiquitous language* — the names in your inventory must be the names the domain expert uses, with the same meaning, or your model will be wrong in ways you cannot see from the code. *Bounded context* — step 1 only converges if you scope it to one workflow at a time; the same word ("meter," in Fowler's utility-company example) means three different things in three different departments, and forcing unification at step 1 produces incoherent models. *Entity vs value object* — Chen's identity/attribute split made operational: entities have identity that persists across attribute changes; value objects *are* their attributes. The book's wider OO tendencies (aggregates, repositories, factories) are orthogonal and can be ignored.

## Step 2: Workflows and origins

What does the user expect to happen? What sequences of actions does the software support? What is displayed, when, in what form? For each item in the step-1 inventory: where does it originate (user input, external feed, computed from others), and what is its read/write profile (read-mostly, write-mostly, write-once-read-many)?

**Jackson, 1975** (*Principles of Program Design*; *JSP*): program structure mirrors data structure, where "data structure" means the structure of the input and output streams the program processes — i.e., the workflow. JSP's procedure (Wikipedia summary, preserving the operational form): "analyze the data structures of the files that a program must read as input and produce as output, and then produce a program design based on those data structures." The corollary Jackson states directly: "requirement changes are usually minor tweaks to the existing structures... small changes to the inputs and outputs should translate into small changes to the program." If you derive structure from workflow, workflow changes cost workflow-sized refactors; if you don't, they cost system-sized ones.

**Brooks, 1975** (*Mythical Man-Month*, ch. 8): the data representation *is* the essence; the workflows distinguish essence from accident. "Show me your tables and I won't usually need your flowcharts" (attribution verified in `notes/02-data-layout.md`) — the flowchart is redundant because the workflow is implicit in the table shape.

**Kleppmann, 2017** (*Designing Data-Intensive Applications*, ch. 2): the modern treatment of workflow-to-model matching. The right data model — relational, document, graph, event log — depends on the operations the workload performs, not on the conceptual neatness of the model. The same step-1 inventory can map to several data models; step 2 selects among them.

**Evans steelmanned, 2003**: a bounded context is, operationally, a workflow scope. The same `Order` participates in the order-taking workflow (mutable, identity-bearing, customer-session-bound) and in the analytics workflow (immutable, time-stamped, reporting-period-bound). Treating these as the same data shape collapses two workflows into one and makes both worse.

Output of step 2: for each workflow, which step-1 items it reads, which it writes, in what order, with what latency requirement.

## Step 3: Magnitude and distribution

For each item in the inventory, an order-of-magnitude estimate: 10? 10^3? 10^6? 10^9? Distribution shape: uniform, Zipfian, bimodal, heavy-tailed? Working-set size vs long-tail size? Growth profile: bounded (countries: ~250 forever), linear in users (orders per customer), super-linear (edges in a social graph)?

**Newman, 2005** ("Power laws, Pareto distributions and Zipf's law," *Contemporary Physics* 46(5), arxiv:cond-mat/0412004). Direct quote: "When the probability of measuring a particular value of some quantity varies inversely as a power of that value, the quantity is said to follow a power law, also known variously as Zipf's law or the Pareto distribution." Newman's catalogue: "the distributions of the sizes of cities, earthquakes, solar flares, moon craters, wars and people's personal fortunes all appear to follow power laws"; "Power laws appear widely in physics, biology, earth and planetary sciences, economics and finance, computer science, demography and the social sciences." Consequence for step 5: a structure tuned for uniform-distribution behaviour on a power-law key set sees a small number of buckets attract most of the load. The right structure for power-law data *exploits* the skew (weighted LRU, top-K structures, sketches), not one that ignores it.

**Knuth, *TAOCP* Vol. 3** (*Sorting and Searching*, preface and ch. 6): the canonical treatment of choosing search structures by access pattern, key distribution, and storage hierarchy. Average-case behaviour on the *actual* distribution dominates worst-case asymptotic behaviour on hypothetical ones — *provided you know the distribution*. When you don't, the worst case is your specification.

**Kleppmann, 2017** (ch. 3): read/write ratio, working-set size, and access pattern discriminate among B-tree, LSM-tree, hash index, and columnar formats. A B-tree's update-in-place cost (random I/O, write amplification ~2x) is acceptable when reads dominate and the working set fits cache; LSM batched writes (sequential I/O, higher read amplification) dominate when writes are heavy.

Output of step 3: per step-1 item, a magnitude and a distribution shape. If you cannot estimate — "10 or 10^9, I don't know" — *ask the user*. Guessing wrong by three orders of magnitude is the same failure as guessing whether a feature is load-bearing.

## Step 4: Derive operations

From the workflows (step 2) and the data (step 1), derive the *set of operations* the software must perform, and frequency-weight each. The canonical list, drawn from relational algebra and extended: point lookup, range scan, prefix search, substring/textual search, join, aggregation, sort by non-key attribute, top-K, set membership (returns a bit, not a record), set intersection/union/difference.

**Codd, 1970**: this is the relational algebra, derived from set theory. The Wikipedia summary of Codd's enumeration: "project (eliminating some of the columns), restrict (eliminating some of the rows), union, difference, intersect, and product" — plus join, plus division. The deep claim is that *every* query expressible against a relational schema decomposes into these primitives, so step 4 reduces, for relational workloads, to "which primitives, in what proportion?"

**Kleppmann, 2017** (ch. 2): non-relational workloads add primitives the relational algebra does not name — graph traversal (nodes within K hops), document patch, event append, time-window aggregation. Same discipline: enumerate, weight.

**Bentley, 1986/2000** (*Programming Pearls*, Columns 1, 2, 3, 11, 13): every column begins with "what is the data, and what is the operation?" Column 1's bit-vector sort of seven million telephone numbers is forced by (a) the data are small integers, (b) the operation is sort, (c) the magnitude exceeds memory under a naive representation. Step 5 is mechanical once steps 1, 3, and 4 are written down. *Writing Efficient Programs* (1982) makes this explicit: each transformation is indexed by the operation it makes cheap.

Output of step 4: a frequency-weighted list of operations. If point lookup is 99% of traffic and full-collection sort is 0.01%, optimise the lookup and accept a linear scan for the sort. Inverted ratio, inverted structure.

## Step 5: Match operations + magnitude to structures

Now and only now do you pick a structure. The choice is mechanical given steps 3 and 4: each has a regime where it dominates and a regime where it is wrong.

- **Hash table.** Point lookup, set membership; O(1) expected. Wrong for range scan, prefix, ordered iteration; wrong on power-law keys with a generic hash (hot buckets attract cache contention).
- **Sorted array.** Point lookup (binary search), range scan (sequential, cache-friendly), range aggregation. Wrong under mutation (insert O(n)); right for static or batch-built data.
- **B-tree.** Point lookup, range scan, in-place update, ordered iteration. Canonical OLTP. Wrong when writes vastly exceed reads (write amplification) or when data is append-only.
- **LSM tree.** Write-heavy with point lookup and range scan; trades read amplification for sequential write I/O. Wrong when reads dominate and the working set spills cache.
- **Inverted index.** Substring/textual search, set intersection on doc IDs. Necessary above the linear-scan latency budget; overkill for an enum (~10^2 values).
- **Bloom filter.** Set membership where false positives are cheap and false negatives unacceptable. Pre-filters negative lookups against a disk set. Wrong as the only structure (use the underlying set).
- **Trie / radix tree.** Prefix search, longest-prefix match (IP routing, autocomplete). Wrong when keys are random — pays for structure it cannot exploit.
- **Skip list.** Ordered set with concurrent updates and probabilistic O(log n). Right when concurrency dominates B-tree's cache advantage.
- **Log-structured file.** Append-only writes, replay or background indexing on read. Right for event logs and immutable history; wrong for random updates without a separate index.
- **Columnar store.** Aggregation/scan over a subset of columns across many rows (OLAP). Wrong for point lookup of full rows.

**Sedgewick, *Algorithms* (4th ed.)** ch. 1 on cost models and *doubling experiments*: when asymptotic analysis is inconclusive (two structures are both O(log n); constants matter), measure on the actual data at the actual magnitude and observe the doubling ratio — does cost double when n doubles, or grow by a smaller factor? When the choice is close, measure.

**Stonebraker & Çetintemel, 2005** ("'One Size Fits All': An Idea Whose Time Has Come and Gone," ICDE): a single storage engine cannot serve OLTP, OLAP, stream processing, text search, and scientific workloads efficiently — each requires a different layout. Specialised engines dominate generic ones in their regime. If your workload spans regimes, decompose into workloads and pick per workload.

**The matching principle, stated as two failure modes.**

- *Trigram-search on 200 country names* is wasted code. A linear scan touches at most 200 cache-resident strings; the trigram index is more code, more memory, more failure modes, and slower on a working set this small. Structure chosen without sizing.
- *Linear scan on the Library of Congress catalog* — ~10^8 records — is wasted clock cycles. Requires an inverted index keyed by author and title with secondary indices on subject. Structure chosen without sizing.

Same failure on both ends.

**Norvig, 2007** ("How to Write a Spelling Corrector," norvig.com/spell-correct.html): worked example of step 5 done right. The representation is a `Counter` over a ~1.1M-word corpus, indexed by word, storing P(word). Direct quote: "We can estimate the probability of a word, P(word), by counting the number of times each word appears in a text file." Operations — point lookup of word probability, candidate generation within edit distance ≤2 — fall out trivially. On why the representation matters: "the answer is that P(c|w) is *already* conflating two factors, and it is easier to separate the two out and deal with them explicitly." The representation separates the factors; the operations follow. Twenty-one lines of Python, because steps 1–4 were taken seriously.

## Step 6: The shape IS the contract

Once the representation is chosen, everything else falls out: the API surface (you can only expose operations the representation supports cheaply), failure modes (a B-tree fails differently from an LSM-tree under power loss), the concurrency model (sorted-array stores cannot support concurrent insert without coarse locks; LSMs can), the durability story (append-only logs survive crashes trivially; update-in-place requires a journal).

**Brooks, 1975** (*Mythical Man-Month*, ch. 9), attribution verified in `notes/02-data-layout.md`: "Representation is the essence of programming." Ch. 9 ("Ten Pounds in a Five-Pound Sack") frames this under space-time constraints; the geometry survives the era. The per-record layout times the cardinality from step 3 sets the working-set size, which sets the cache hierarchy the system lives in.

**Pike, 1989** ("Notes on Programming in C," Rule 5), attribution verified in `notes/02-data-layout.md`: "Data dominates. If you've chosen the right data structures and organized things well, the algorithms will almost always be self-evident." The converse: if your algorithms are *not* self-evident — if every operation requires twisty code, helper structures, defensive checks — you chose the wrong structure at step 5. Go back.

**Helland, 2015** ("Immutability Changes Everything," CIDR / *ACM Queue* 13(9)): the immutable-append representation forces a GC subsystem, makes replication trivial, makes distributed transactions avoidable. The mutable-update-in-place representation makes none of these choices and forces all of them as separate decisions. "Accountants don't use erasers."

**Torvalds, 2006**: git's blob/tree/commit object model is the contract; the porcelain has been rewritten many times without breaking it. The structure outlives every other decision in the system.

## Common mistakes the methodology refuses

Each is a failure at the moment of generation. The methodology gives a specific point at which to refuse it.

- **Choosing a structure because it is familiar.** "Use a hash map" without checking step 3 or 4. Hash maps are wrong for ordered iteration, prefix search, range queries, and power-law keys with a generic hash. Familiarity is not a regime.
- **Asymptotic complexity without bounding the constant.** "It's O(log n)" with no n. At n=10 the constant dominates; at n=10^9 the asymptote dominates. Step 3 names which.
- **Worst-case tuning when the distribution is skewed.** Newman: most real distributions are not uniform. A worst-case structure may be a 100x pessimisation on the typical case.
- **Step 4 skipped.** Picking "an array" without knowing whether the workload needs ordered iteration, indexed access, or just append-and-iterate.
- **Step 3 skipped.** The trigram-on-country-names / linear-on-LoC failure. A choice without sizing is a guess.
- **Step 1 skipped.** Three weeks in you discover two records you thought were "the same thing" are not (Kent), or an attribute you treated as a value object has identity (Evans).
- **Treating the framework's default as the design.** Active Record's `User` is not a data design; it is the framework's default for a missing one, tuned for a hypothetical workload that may not be yours.
- **Optimising for an operation not on the step-4 list.** Pre-optimisation. "We might need range queries someday" is answered by "we will pick the right structure then," not by paying for a B-tree now.
- **Conflating data model with storage layout.** Codd's data independence says the conceptual schema (steps 1–4) is *separable* from the storage layout (5–6). Collapsing them prematurely is two decisions becoming one worse one.

## What this means for an LLM coding assistant

Before writing code for a non-trivial feature, write down steps 1–4 for it. Even tersely. Step 5 follows mechanically; if it doesn't, you skipped one of the earlier steps.

Default to the simplest representation that supports the listed operations within the listed magnitudes. Do not optimise for operations not on the list. Point lookup and append only? A flat append-only file with an in-memory hash is correct; a B-tree pays for ordered iteration you do not need. Point lookup and range scan? B-tree is correct; the hash forces a full scan for any range query. The minimum-viable structure is the one whose cheap-operation set matches your step-4 list, no more.

When unsure of a magnitude, **ask the user before picking a structure**. A guess off by three orders of magnitude is the same failure as guessing whether a load-bearing feature can be skipped. "Tens of rows or tens of millions?" has answers like "tens of thousands, fat tail" — and that answer flips step 5 from "linear scan" to "sorted index." You will not get this right by guessing.

The matching principle restated: trigram-on-200-country-names is wasted code; linear-on-the-Library-of-Congress is wasted clock cycles. Both fail at step 5 because step 3 was skipped. If you cannot name the regime, you cannot pick the structure.

Between "simple structure that handles 99% of operations cheaply" and "complex structure that handles 100% cheaply but also handles operations not on the list," prefer the simple one. The 100% option is pre-optimisation for an unstated requirement. Hoare/Dijkstra: the simplest structure that works is the one you can verify by inspection.

Between "specialised structure that nails the regime" and "generic structure that is fine everywhere," in a hot path prefer the specialised (Stonebraker); in a cold path prefer the generic (less code is less debt). The discriminator is whether the structure's cost matters.

If your algorithms are not self-evident, the structure is wrong — fix the structure, not the algorithm.

---

## Source ledger

Primary text fetched and quoted directly:

- **Newman, M.E.J.** (2005). "Power laws, Pareto distributions and Zipf's law." *Contemporary Physics* 46(5). arxiv:cond-mat/0412004. Direct quotes on power-law definitions and the catalogue of phenomena.
- **Norvig, Peter** (2007). "How to Write a Spelling Corrector." norvig.com/spell-correct.html. Direct quotes on corpus-based representation.
- **Chen, Peter** (1976). "The Entity-Relationship Model—Toward a Unified View of Data." *ACM TODS* 1(1):9–36. Citation verified; the "real world consists of entities and relationships" quote is from the Wikipedia summary, which preserves the original wording.
- **Codd, E.F.** (1970). "A Relational Model of Data for Large Shared Data Banks." *CACM* 13(6):377–387. Citation verified via multiple sources (ACM DL, Wikipedia, Penn mirror). The upenn.edu PDF was binary-only via WebFetch; relational-operation enumeration and data-independence claim reproduced from the Wikipedia summary.

Cited from secondary or summary sources (bibliographic record verified, primary PDF not fetched in this pass):

- **Brooks, F.P.** (1975, ann. ed. 1995). *The Mythical Man-Month*, ch. 9 ("Ten Pounds in a Five-Pound Sack"). "Representation is the essence" and "show me your tables" verified in `notes/02-data-layout.md`.
- **Brooks, F.P.** (1986). "No Silver Bullet." *IEEE Computer*. Essence/accident referenced.
- **Pike, Rob** (1989). "Notes on Programming in C," Rule 5. Fetched in `notes/02-data-layout.md`.
- **Torvalds, Linus** (2006). git mailing list, 27 Jun 2006. Cross-verified in `notes/02-data-layout.md`.
- **Helland, Pat** (2015). "Immutability Changes Everything." CIDR / *ACM Queue* 13(9). Cross-verified in `notes/02-data-layout.md`.
- **Stonebraker, M. & Çetintemel, U.** (2005). "'One Size Fits All'." ICDE. Cross-verified in `notes/02-data-layout.md`.
- **Jackson, Michael A.** (1975). *Principles of Program Design*; (1983) *Jackson System Development*. JSP procedure and corollary quotes reproduced from the Wikipedia JSP summary, which preserves the original phrasing.
- **Evans, Eric** (2003). *Domain-Driven Design*, Addison-Wesley. Ubiquitous language / bounded context / entity vs value object reproduced from the DDD Wikipedia article and Fowler's BoundedContext bliki.
- **Kleppmann, Martin** (2017). *Designing Data-Intensive Applications*, O'Reilly. Ch. 2 and Ch. 3 cited from common knowledge of the chapters.
- **Knuth, D.E.** *TAOCP* Vol. 1 §2; Vol. 3, preface and ch. 6. Cited for average-vs-worst-case framing and the search-structure taxonomy.
- **Sedgewick, R.** (2011). *Algorithms* (4th ed.). Ch. 1 (cost models, doubling experiments). Course page redirected; cited from common knowledge.
- **Bentley, Jon** (1986; 2nd ed. 2000). *Programming Pearls*; (1982) *Writing Efficient Programs*. "Begin with data and operation" methodology cited from common knowledge.
- **Kent, William** (1978). *Data and Reality*, North-Holland. Wikipedia page 404; cited from common knowledge and survey references.

Canonical primary text not verified in this pass: Codd 1970 (binary PDF), Brooks 1975/95, Kent 1978, Bentley 1986/2000, Knuth TAOCP, Sedgewick 2011, Evans 2003, Jackson 1975/83, Kleppmann 2017.
