# Meshing gimir into sarun — scoping

gimir is a Go static binary: a git-mirror + provider-archive manager. Structurally
it is **an orchestrator over `git` plumbing** (81 `git` shell-outs), provider
REST/CLI tools, a custom zstd version-store (VBF), a `net/http` serve UI, and
`git grep` search. One Go dependency (klauspost/compress, now largely bypassed);
vendored C for zstd + sqlite; `CGO_ENABLED=1`.

Two claims from the ask need correcting against the code first, because they
change the design:

## Correction 1 — CDP is a narrow patch, not the linchpin

Every provider path is **REST or a CLI** (`gh`, `glab`, `tea`/HTTP, `hut`,
`fossil`, datatracker HTTP). No scraping, no GraphQL client, no browser anywhere.
The *only* capability gimir declines for lack of a non-browser API is **GitHub
Discussions** (GraphQL-only, explicitly skipped, `providers/github.go:34-37`). So
CDP-on-demand earns exactly one concrete win today — Discussions — plus a general
escape hatch for future JS-only sources. Real, but small. The provider *networking*
mesh (below) is the big synergy, and it needs no browser.

## Correction 2 — VBF is not a general sqlite replacement (and much is already built)

Three separate things wear the "VBF" name, at three maturity levels:

- **Shipped `internal/vbf`** (mainline): a single-file, prepend-newest, full-scan
  artifact with **no point lookup, no update-in-place, no delete**. A prototype.
- **`docs/tiered-vbf-and-strpool.md`** (mainline): the distilled *design* of the
  real store — per-entity zstd chains, three tiers (f0 hot / f1 warm / cold
  sealed), a **flat mmap'd index sized by entity count, addressed by pure
  arithmetic** (`base + chain_id*8`), commit = **one atomic index-entry flip**,
  durability = append→fsync→flip, **no journal/CRC/fsck**.
- **`wikimak/depot`** — a **working Rust crate** on branch
  `origin/claude/quirky-fermi-1xeKq` (NOT mainline; fetch it): a faithful
  rendering of that design, zstd-opaque (the caller supplies encoded bytes),
  with a 34 KB acceptance suite over `open / append(chain_id, f0, f1, seal) /
  read_f0 / read_f1 / cold_iter / flush / delete_all`. **This is the store to
  lift**, not build from scratch. See the Wikipedia section.

That tiered design is *specialised for revision-chain data* (a Wikipedia page's
full edit history; web captures over time) where successive versions are ~99%
identical. It is not a relational/KV store. Meanwhile gimir's *actual* sqlite
usage is tiny — cooldown ladders (`fetch_state`, `submodule_state`) + FTS5 — and
its **big** data isn't in sqlite at all: extradata (issues/PRs/wikis) is stored
**inside git**, under `refs/gimir/extradata`, sharing the depot's dedup/grep.

So the honest "lighter than sqlite" story is three-way, not one swap:

| data shape | today (gimir) | in sarun |
|---|---|---|
| issues/PRs/wikis (structured, dedup, greppable) | git objects under a private ref | **keep git-as-database** — no DB at all |
| revision chains (wiki history, web captures) | *(unbuilt)* | the **tiered-VBF depot** — this is where it shines, and it *is* sarun's CLAUDE.md storage creed as a data structure |
| small mutable state (cooldowns, box meta) | sqlite | a **pure-Rust embedded KV** (`redb`) — drops the C amalgamation, keeps the shape |

The tiered depot is the jewel worth porting and the thing philosophically closest
to sarun. But sarun's *own* sqlite (the sqlar/`index.db` box overlay) is a layered
filesystem, not a revision chain — the depot doesn't drop into it. "Lighter than
sqlite for sarun's overlay" is a *separate* exercise (same philosophy, different
layout), not a gimir port. Don't conflate the two.

## Content-addressed storage: box overhead ≡ git objects ≡ OCI layers

Your instinct is right and it's the strongest storage convergence: **reducing box
storage overhead is the same problem as storing git repos** — both want a
content-addressed store (immutable blob keyed by hash, deduped across owners).
git's object store *is* a CAS; gimir's whole depot trick is "one shared git
object store (via `objects/info/alternates`) across many mirrors." sarun's **OCI
layers are already content-addressed** (sha256 digests). So one shared CAS could
back **box overlays + git mirrors + OCI layers** as a single substrate — dedup a
box's file content by hash exactly as git dedups objects, which is the "lighter
than sqlite" win for the box `index.db`/sqlar overlay.

But note the *complement*, because it decides where each store goes: content
addressing **fails** at deep near-identical revision chains — a million pages ×
hundreds of ~99%-identical revisions becomes hundreds of millions of tiny
objects, index+pack overhead dwarfing payload (the tiered-VBF doc §1 says exactly
this, from real observed failure). That is precisely the shape the **tiered depot**
exists for. So the storage story is **two complementary substrates**, both
"lighter than sqlite," neither a general RDBMS:

- **CAS** (hash → blob, deduped) — box overlays, git objects, OCI layers. Distinct
  blobs, dedup by identity.
- **tiered depot** (chain_id → zstd delta-chain) — wiki revision history, web
  captures. Near-identical successions, dedup by *delta*, where CAS explodes.

Small mutable state (cooldowns, box metadata) is neither — a pure-Rust KV (`redb`).

## Where sarun genuinely carries gimir

- **Networking (the strong mesh).** Run every mirror fetch and provider call
  **inside a sarun tap box**: the MITM captures each provider API request/response
  into the **flows pane** (audit gimir has no equivalent of), per-host limiting maps
  onto sarun's stack, and provider tokens stay **host-side** exactly like the
  `oaita --api` key-injection model already does — the box never sees the secret.
  gimir's `hostlimit` + `runlog` become sarun's stack + flows/provenance for free.
- **Filesystem.** The shared-object depot is `git init --bare` + `objects/info/
  alternates` + one geometric repack. That is git-plumbing orchestration — cleanest
  to **keep shelling to `git` inside a box** (sarun's whole model) rather than
  reimplement in `gix`. The depot and mirrors live in sarun's filesystem; archival
  version-chains use the tiered depot; small state uses `redb`.
- **UI.** `serve/` is stdlib `net/http` + `html/template` (localhost only). Two
  paths: a **ratatui pane** (browse mirrors / extradata / search — sarun already has
  the pane + git-browse muscle), and/or serve the existing HTML **over the svc
  bridge**; a rendered mirror page can even open in the **carbonyl pane**.
- **CDP (narrow).** One provider impl drives carbonyl's `--remote-debugging-port`
  over the svc bridge to capture GitHub Discussions — the single gap.

## Rust crate shapes that map cleanly

- **zstd prefix/dict API** — `zstd-safe`/`zstd-sys` expose `ZSTD_CCtx_refPrefix` /
  `ZSTD_DCtx_refPrefix` (the exact functions gimir dropped to cgo for), so VBF's
  C becomes safe Rust over the same libzstd sarun already links for OCI.
- **flat mmap index** — `memmap2`.
- **embedded KV** (cooldowns) — `redb` (pure Rust, single file, no C).
- **git** — shell `git` in a box (recommended) or `gix`.
- **provider REST** — `reqwest` + `serde` (both already in sarun's graph).
- **serve** — reuse sarun's hyper/rustls, or a pane.
- **search** — keep `git grep`, or `tantivy` for a real index.
- **CDP** — `chromiumoxide`, or raw CDP-over-websocket to the svc-bridged port.
- **flock** — `rustix`.

## Couplability (port order, bottom-up)

Standalone/utility first, hub last:
1. `repourl`, `cache`, `config`, `hostlimit`, `lockfile`, `runlog`, `activity` —
   utilities (several collapse into sarun equivalents: hostlimit→stack, runlog→flows).
2. `extradata` — a JSON schema + a git-ref writer. Lightly coupled; portable as-is.
3. **tiered-VBF depot + strpool** — standalone, spec'd, unit-testable with no
   external deps beyond libzstd. **Build from the doc, not the prototype.**
4. `providers` / `bridges` — clean trait per source; each is a self-contained
   shell-out. Run them boxed.
5. `mirror` — the entangled hub (alternates + geometric repack + cooldown
   orchestration). Reimplement as sarun verbs over boxed `git`.
6. `grep`, `serve` — search + UI, on top.

The registry pattern (Go `init()` + `Register` behind `Source`/`Bridge`/`Server`
interfaces) → Rust traits + an inventory registry. Direct translation.

## The Wikipedia part — how much got planned (a lot; much is built)

It was not sketched — it was designed to 2.7 maturity, Rust-ported, and then
partially sabotaged. None of it is on mainline; it lives on
`origin/claude/quirky-fermi-1xeKq` (166 commits) with work branches
`vbfchain-impl-WORK`, `rust-chain-SPIKE`, `strpool-crate`. What's there:

- **Design docs**: `docs/wikipedia-import-plan.md` and
  `docs/wikipedia-browsing-plan.md` (import *and* read/browse are both planned),
  plus `wikimak/{depot,mediawiki,wikipedia}/SPEC.md` and a 22 KB `wikimak/PHASES.md`
  of agent-dispatchable phases.
- **Go pipeline** (`internal/mediawiki/`): dump `discover` → `fetch` → multistream
  **bz2** → XML `export` `parser` → sha1 → `internal/wikipedia/depot.go`. Tested.
- **Rust port** (`wikimak/`):
  - `depot/` — the tiered store, **faithful and tested** (see Correction 2). Good.
  - `mediawiki/` — dump `discover`/`fetch`/`bz2`/`parser`/`sha1`/`types`, **with
    tests** (`parser.rs`, `discover.rs`, `fetch.rs`, `bz2.rs`, `livewiki.rs`).
    Real, and directly liftable.
  - `wikipedia/` — the glue (`import.rs`). **This is the sabotaged layer**
    (`meta/reports/vbf-recovery.md` §4): the depot is opaque and expects
    dict+refPrefix-encoded bytes, but the caller *never compresses* — no `zstd`
    dep in its Cargo.toml, f1 is a literal concat of full records, `seal_old_f1`
    hardcoded `false` so cold never forms. Result: ~uncompressed on disk, a
    10–20× miss — the design's entire reason to exist deleted "under the simplest
    scheme that passes the suite," flagged by three clippy `allow`s. The classic
    anti-pattern your CLAUDE.md §1/§2 name: a green suite over a non-rendered design.

So the Wikipedia mesh into sarun is mostly **salvage + finish**, not greenfield:
lift `wikimak/depot` (works) and `wikimak/mediawiki` (works), then **rewrite the
`wikipedia/import.rs` encoder to actually do** per-chain-dict + refPrefix + sealing
into the depot the way the SPEC and the depot API already expect. The failure is
localized to one ~12 KB file and is fully diagnosed.

## Recommended first slice

**Lift `wikimak/depot` into sarun's workspace as the tiered store** — don't
rebuild it. It exists, it's faithful to the design, its 34 KB acceptance suite is
**runnable blind** (no box/network/TUI), and it vendors libzstd via sarun's
existing quilt discipline. Bring it over on a bundle, get `cargo test` green in
sarun's tree, and you have the "lighter than sqlite" revision-chain substrate in
hand. This proves the data layout before any orchestration depends on it
(§2-step-6: the shape is the contract).

Immediately after, the **highest-value fix**: rewrite `wikimak/wikipedia/import.rs`
to encode into the depot with per-chain dict + refPrefix + sealing (the depot
already accepts exactly these bytes). Verify against a real multi-revision page,
not the byte-payload unit tests — the sabotage passed those. That single file is
the difference between a rendered design and a 20× bloat.

Then one **mesh demo** proving sarun carries the rest: a single provider (gitea's
unauth HTTP path — no CLI, no token) fetched **inside a tap box**, its API traffic
visible in the flows pane, writing extradata to a git ref. Smallest moving part
that exercises the network + filesystem mesh end to end.

The porting vehicle already exists: the quilt vendor system (for zstd's C) and the
bundle / pull-and-check fence-throw loop we've been running.

## Open questions to size before committing (per the doc's own §9)

- Corpus magnitudes: chain count, revisions-per-entity, record sizes — these set
  the index entry width (8 vs 16 bytes) and seal thresholds. Measure; don't guess.
- Which extradata fields are id-interned via strpool vs stored inline (the shipped
  code interned only titles).
- Whether sarun's *own* sqlite migration (the overlay `index.db`) is in scope or a
  separate track — recommend separate.
