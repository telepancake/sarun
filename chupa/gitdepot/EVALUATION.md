# gitdepot — EVALUATION (Phase 5)

Measured on the real repositories in `/home/user/eval-repos/` (`git.git`,
`sqlite.git`, `ripgrep.git`, `jq.git`) and this checkout (`/home/user/sarun`),
driving the shipped static-musl binary
(`target/x86_64-unknown-linux-musl/release/gitdepot`) through its CLI. Every
number is read back out of the stored data on disk; nothing is taken from an
in-RAM structure or a git-side re-derivation counted as "free".

## What is measured, and what isn't

Per `VALIDATION.md`, the CLI reaches exactly **one** engine: the shipping
one-git-tree-per-commit store (`lib.rs`/`store.rs` → `wikimak_depot::Depot`, four
chains TREES/COMMITS/REFLOG/TAGS, newest tree stored full + older trees as
reverse deltas). The union-of-lanes encoding the design bets on for size
(§1/§2 — a file's versions across branches made adjacent for zstd) is **not on
this path**. So these are the numbers of a *reverse-delta chain of whole trees*.
That is the whole story of where it wins and loses.

Two store sizes are reported, because they differ a lot:

- **shipped on-disk** — `du -sb` of the store exactly as `import` leaves it. The
  depot is an append log with dead-byte accounting; a single-batch import seals
  the old f1 accumulator **verbatim into a cold frame** but does **not** truncate
  its bytes from the sub-threshold f1 file, and the CLI never runs the
  session-end compaction (`Depot::collect`). So the shipped store carries the
  tree corpus ~twice.
- **live (compacted)** — `du -sb` after one `Depot::collect` (run here via a
  15-line example, `gitdepot/examples/compact.rs`). This is the real corpus and
  the fair thing to compare against a fresh git pack (which has no dead bytes).

**git baseline**: a fresh bare repo with the *same refs* (`refs/heads` +
`refs/tags`), objects fetched, then `git repack -adf` at two windows — `--window=10`
(git's default `gc` pack, what a user actually has) and `--window=50 --depth=50`
(git's aggressive minimum). `-adf` (force delta recompute) is essential: a plain
`git repack -ad` reuses the clone's thin-pack deltas and overstates git by up to
2.7× (git.git: 314 MB reused vs 143/116 MB recomputed).

## 1. SHA-exact reconstruction — PASS on 4 corpora, **FAILS on git.git**

Reconstruction was checked two independent ways: `examples/treecheck.rs`
rebuilds each tree from the stored reverse-delta chain and hashes it to a git
tree oid **in-process**, compared to `git rev-parse <sha>^{tree}`; and `export`
rebuilds commits via `git fast-import` and checks each regenerated ref sha
against the imported one (its built-in fidelity check).

| corpus | commits checked | tree-oid exact | result |
|--------|----------------:|---------------:|--------|
| jq              |  1 977 |  1 977 | **PASS** (treecheck, 0 mismatch) |
| ripgrep         |  2 278 |  2 278 | **PASS** (treecheck, 0 mismatch) |
| sarun           |  1 220 |  1 220 | **PASS** (treecheck, 0 mismatch) |
| sqlite (6 000-commit `master` prefix) | 6 000 | 6 000 | **PASS** (export fidelity + ref sha == source) |
| **git.git** (6 660-commit prefix) | 6 660 | 6 562 | **FAIL — 98 wrong** |

The git.git failure is real, deterministic, and reproducible:

- The 98 bad commits are a **contiguous block — git's earliest ten days**
  (`e83c5163` "Initial revision of git", 2005-04-07 → `9fec8b26`, 2005-04-16),
  all flat all-blob trees, the deepest tail of the reverse-delta chain.
- Their reconstructed tree oids **do not exist as git objects at all** (e.g.
  root commit `e83c5163`: git's real tree is `2b5bfdf7`, the store reconstructs
  `bab06b64` — "not a tree object"). So the reconstructed tree *bytes* are wrong,
  not merely mis-indexed.
- **`export`'s own fidelity check independently catches it**: the exported
  `refs/heads/main` regenerates as `40a3e087…` vs the imported `e893f7ad…`
  (`rc=1`, "fidelity check failed"). Zero signed commits in the slice, so this is
  not the signed-commit abort below — it is a genuine reconstruction defect.
- It is **not an alternates/subset artifact**: re-importing the same slice as a
  self-contained clone (objects copied, no `alternates`) reproduces the *identical*
  wrong sha `40a3e087…`. Normal `import` of real git.git history mis-encodes it.

So the design's headline property — SHA-exact round-trip — **holds on 4 of 5 real
corpora and fails on the 5th**, on its earliest history, through the ordinary
import path. It is close, but not the unconditional guarantee the design claims.

**Coverage gaps around identity** (each a hard abort, per §11 "aborting on
unsupported input is a temporary state"):

- `export` refuses the first **gpg-signed commit** — so it cannot verify jq (354
  signed), ripgrep (917), sarun (18) or full git.git (70) at the commit level at
  all (treecheck, being tree-level, still works).
- `export` builds the whole `fast-import` stream in RAM and was **OOM-killed at
  ~12 GB** on full sqlite (36 306 commits); `treecheck` materialises every tree
  view at once and was **OOM-killed** on full sqlite too. There is currently **no
  way to verify identity of a large real repo** through the shipped tools — the
  readout is O(corpus) in memory, not O(1).
- `import` **aborts the entire repo** on `refs/tags/junio-gpg-pub`, a tag that
  peels to a **blob** (Junio's GPG key): "only commit- and tree-target tags are
  supported". The git.git rows below are with that one ref filtered out.

## 2. Stored size vs git's packfile

Sizes MiB. `git pack` shown at both windows (`repack -adf`). Ratio = gitdepot ÷ git.

| repo    | commits | heads | tags | live | shipped | git w10 (default) | git w50 (aggr) | **live ÷ w50** | shipped ÷ w50 |
|---------|--------:|------:|-----:|-----:|--------:|------------------:|---------------:|---------------:|--------------:|
| ripgrep |   2 278 |    16 |  260 |  2.13 |   3.18 |             3.36 |          3.18 |       **0.67** |         1.00 |
| jq      |   1 977 |    19 |   19 |  3.34 |   6.00 |             4.66 |          4.52 |       **0.74** |         1.33 |
| sarun   |   1 220 |     ~6 |  ~6 | 10.97 |  13.92 |            12.61 |         12.30 |       **0.89** |         1.13 |
| git     |  84 508 |     7 | 1007 | 192.6 |  358.1 |            136.0 |         110.9 |       **1.74** |         3.23 |
| sqlite  |  36 306 | 1 707 |  388 | 268.3 |  523.0 |            129.4 |          88.65 |       **3.03** |         5.90 |

Plainly:

- **Wins on small, few-lane repos — but only the compacted corpus.** ripgrep
  −33 %, jq −26 %, sarun −11 % against git's *aggressive* pack (and a little more
  against its default). The adjacent-revision whole-tree + zstd-solid chain
  genuinely beats git's object delta graph at this scale. The win exists **only**
  for `live`; the **shipped on-disk store is 0–33 % larger** than git because it
  never sheds the dead f1.
- **Loses at scale, worst with many branches.** git.git (7 heads, but 84 508
  commits and a wide topic-branch frontier) is **1.74× / 1.42×** git's pack
  (w50 / w10); sqlite (**1 707 concurrently-live branches**) is **3.03× / 2.07×**.
  Shipped-as-is: 3.2× and 5.9× git.
- **The shipped on-disk store never beats git, anywhere** — the missing
  `collect()` costs 1.1–1.8× on small repos and ~2× on the big ones.

sqlite is the design thesis in the negative: 1 707 live lanes is exactly the case
the union encoding exists to compress, and exactly the case the shipping
one-tree-per-commit chain handles worst — the linearised walk interleaves
unrelated branches, so chain-neighbour tree deltas are large, while git's global
delta window pays no such penalty. Where the union is unimplemented, the size
claim inverts.

## 3. Memory during import

Peak RSS = the binary's own `VmHWM` (stderr). Imports run **without** `--report`
(that flag hoards every full record in RAM — 4.3 GB raw for jq alone — and is a
benchmarking harness, not the store path).

| repo    | commits | max frontier | peak RSS |
|---------|--------:|-------------:|---------:|
| ripgrep |   2 278 |            4 |  274 MiB |
| sarun   |   1 220 |            5 |  297 MiB |
| jq      |   1 977 |            7 |  300 MiB |
| git     |  84 508 |          504 | 1.13 GiB |
| sqlite  |  36 306 |          380 | 1.96 GiB |

Memory tracks the **frontier width** (concurrently-live views), not commit count:
84 508 git.git commits at frontier 504 cost *less* than 36 306 sqlite commits at
frontier 380, because sqlite's trees are larger and its 1 707 branches hold a
wide frontier live longer. Few-lane repos sit on a ~300 MiB floor (process + git
subprocess buffers) regardless of history length. The persistent Arc-shared
frontier keeps this bounded (no OOM even at 84 508 commits), but "a couple of GB
for a mid-size many-branch repo" is the honest import cost, and it scales with
peak concurrency.

## 4. Time and size growth with number of commits

Growth on git.git `master`'s first-parent history (a **few-lane** slice — the
regime gitdepot is built for), each depth imported into a fresh store. Its git
reference is a naive `pack-objects --stdout` (default window, no repack
delta-reuse/sort — git's cheap streaming pack, looser than the repacked numbers
in §2), so use this curve for gitdepot's own trend, not as a competitiveness
verdict. Sizes MB (÷1e6), time wall-seconds.

| first-parent depth | reachable | import s | peak RSS | live MB | naive git pack MB | live ÷ pack |
|-------------------:|----------:|---------:|---------:|--------:|------------------:|------------:|
|                500 |       548 |     0.6 |   22 MiB |    0.35 |             0.62 |        0.57 |
|              1 000 |     1 121 |     0.4 |   47 MiB |    0.67 |             1.24 |        0.54 |
|              2 000 |     3 819 |     1.5 |  255 MiB |    1.96 |             4.61 |        0.42 |
|              4 000 |     8 620 |     5.1 |  334 MiB |    4.48 |            10.05 |        0.45 |
|              8 000 |    18 110 |    13.2 |  353 MiB |   10.66 |            22.67 |        0.47 |
|             16 000 |    47 859 |    54.3 |  602 MiB |   50.62 |            87.54 |        0.58 |
|             24 000 |    80 902 |   145.7 | 1.03 GiB |  191.26 |           302.73 |        0.63 |

- **Size stays ~linear while few-lane** and holds a steady 0.42–0.58× of the
  naive git pack up to ~48 k commits, then **turns superlinear**: 47 859 → 80 902
  reachable (1.7×) but live 50.6 → 191 MB (3.8×). Depth 24 000 first-parent pulls
  the topic-branch merge era (`seen`/`next`) into scope — the frontier widens and
  the reverse-delta chain bloats, the same effect as sqlite.
- **Time grows superlinearly** throughout: 8 620 → 80 902 reachable (~9.4×) takes
  5.1 s → 145.7 s (~29×), roughly O(commits^1.5). The cost is per-commit diff +
  reverse-delta + zstd, amplified as the frontier deepens.
- **The contrast is the punchline.** The *same* git.git as few-lane first-parent
  history keeps gitdepot ~2× smaller than a naive pack; as the whole ref graph
  (frontier 504) it is 1.7× *larger* than a repacked one. **Concurrency, not
  commit count, decides win vs loss** — and concurrency is exactly what the
  unimplemented union encoding is meant to absorb.

## Verdict — where it wins, where it loses, by how much

- **Correctness:** SHA-exact on jq, ripgrep, sarun and a 6 000-commit sqlite
  slice (0 mismatches); **fails on git.git** — 98 of the earliest commits
  reconstruct to non-existent tree oids, caught by both probes and reproducible
  on a plain self-contained import. Identity is *nearly* there but not the
  unconditional guarantee claimed, and the verification tooling itself can't
  reach large repos (OOM) or signed commits (abort).
- **Size:** wins only in its niche — small, few-branch repos, and **only after a
  compaction step the CLI never performs** — by 11–33 % under git's best pack.
  Everywhere else it loses: **1.74×** (git.git) to **3.03×** (sqlite) on the live
  corpus, **3.2–5.9×** on the shipped-as-is store, against git's aggressive pack
  (1.42×/2.07× against git's default pack).
- **Memory/time:** import memory is bounded but frontier-driven (0.3 GB few-lane,
  ~2 GB for a many-branch mid-size repo); import time is superlinear in commits.
- **The deficit is concentrated in two already-designed-but-unshipped pieces:**
  (1) session-end `collect()` (absent → shipped store ~doubles); (2) the
  union-of-lanes encoding (§1/§2, absent → many-branch repos blow up). Plus one
  outright **reconstruction bug** on git.git's earliest history that breaks the
  SHA-exact guarantee. The wins prove the base reverse-delta chain is sound; the
  losses and the failure are all in the un-shipped or unfinished parts.
