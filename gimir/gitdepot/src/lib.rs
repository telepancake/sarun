//! gitdepot — a git repository to/from depot straightedge.
//!
//! The second workload for the depot model (DEPOT-DESIGN.md §7 "git"):
//! a repo's history becomes a chain of canonical tree layers walked
//! **newest first**: the head record is the newest tree's full layer;
//! every older record is a **reverse delta** — the layer that rebuilds
//! the older view from the next-newer one (full-content records per
//! commit make zero sense at scale — imagine linux.git). Frames are
//! refPrefix-chained in zstd the way a VBF chain anchors each frame on
//! the next-newer record; `import --report` prints the comparison
//! against the other encodings (full/delta × standalone/refPrefix, plus
//! the solid bound). Refs and commit metadata (author, committer,
//! message, parent edges) round-trip through their own chains/tables,
//! not through tree layers.
//!
//! Per the implicit-id rule, no git object id is stored in any layer:
//! blob/tree hashes are dropped on import and recomputed by git on
//! export (`git fast-import`). Commit ids ARE kept in meta — not as
//! layer data but as the round-trip *check*: an export is faithful iff
//! the regenerated commit SHAs match. The one exception inside a layer
//! is a gitlink (submodule pin, mode 160000): its commit id is source
//! data — a pointer into a repo we do not hold — and is stored as the
//! node's blob.
//!
//! On-disk store (ATTACH-CONVERGENCE.md chip 7 — THREE CHAINS +
//! STABLE INDICES; full layout/discipline in `store.rs`):
//!
//! * `<dir>/depot/` — one tiered wikimak-depot instance (f0/f1/cold,
//!   bounded prepend) holding four chains: TREES (reverse-delta tree
//!   layers, tip full at f0), COMMITS (one object per commit — sha,
//!   PARENT INDICES, tree index, author/committer/message — batched
//!   one chain record per ingest), REFLOG (every observed ref movement,
//!   deletions included, batched likewise), TAGS (one object per
//!   annotated tag — sha, PEELED target index (commit, or TREE for a
//!   tag at a tree: deduped to a commit's tree when the oids match —
//!   the linux v2.6.11-tree shape — else imported standalone), complete
//!   raw tag bytes as the export-fidelity payload; nested tag→tag
//!   chains stored inner-first). Tags peeling to a blob are refused
//!   with a named Unsupported — the only remaining unsupported ref
//!   shape (no known real-world need; revisit on evidence).
//! * `<dir>/meta.sqlite` (WAL) — kv (schema=6, label/url, the
//!   authoritative per-chain record counts), refs (CURRENT refs only:
//!   name → PEELED nullable commit_idx + tree_idx, nullable tag_idx for
//!   annotated tags — resolving/attaching by tag name yields the peeled
//!   commit; a tree tag has commit_idx NULL and attaches the tagged
//!   tree, pinned by the tag's own sha) — NOTHING else: sha → idx is an
//!   in-RAM map derived by one commits-chain walk per open handle
//!   (store.rs cost model), and tree dedup is parent-oid comparison
//!   plus an intra-ingest map, never persisted.
//!
//! Import/update discovery is O(changes), not O(tree × history): ONE
//! `git rev-list --parents` pass fixes the CHAIN LANDING ORDER (an
//! own linearization, `walk_order` — git's `--topo-order`/`--date-order`
//! both interleave diverged lines and blow the reverse-delta chain up
//! by orders of magnitude on merge-heavy history), then ONE
//! `git diff-tree --stdin` stream serves the first-parent deltas in
//! that order + ONE persistent `cat-file --batch` the raw commits and
//! changed blobs; per-commit views are built frontier-style (clone
//! first parent's view + apply_mut of the delta, refcounted by
//! remaining children). The CHAIN ENCODING is untouched: the same
//! records, reverse deltas, batching and anchoring land through
//! `store::Ingest` exactly as before.
//!
//! Records carry STABLE indices counted from the oldest end (record k =
//! newest-first frame N-1-k; prepends only grow N), so lineage lives in
//! the data and an upstream rewrite is just new records + repointed
//! refs — no non-fast-forward path, no re-import, no store retirement.
//! git itself is driven by shelling out — sarun custom — so this tool
//! needs a `git` binary and runs host-side.
//!
//! Mirroring keeps NO persistent clone: `<root>/repo.git` is a
//! KB-scale SHALLOW STUB (tip commit objects + tag chains + refs +
//! `shallow` boundary — see THE STUB CONTRACT at the stub section
//! below) rebuilt from the store after every run; tip snapshots are
//! materialized into it before each fetch and vanish with the re-pin.
//! First contact bootstraps through a LADDER of fetch rungs (tag waves
//! in natural-version order, then a converge fetch) whose records all
//! stage through ONE ingest — exactly one prepend per touched chain,
//! however many rungs the transport took (`mirror --whole` opts back
//! into a single-shot clone).

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use depot::codec;
use depot::{Attrs, BlobOp, Layer, Node};

mod cli;
pub use cli::cli_main;
mod geostack;
mod gitobj;
pub mod lanestore;
pub mod oidenc;
pub mod reslot;
pub mod layer;
pub mod lanes;
pub mod store;

pub use store::{commit_at, commit_count, label, resolve_ref, Resolved};

// ------------------------------------------------------------------ meta

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RefMeta {
    pub name: String,
    /// Commit id the ref points at (hex) — the PEELED commit for an
    /// annotated tag.
    pub sha: String,
    /// The annotated-tag object id when the ref is one; empty for
    /// branches and lightweight tags.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tag_sha: String,
    /// The peeled TREE oid when the tag peels to a tree (`sha` is then
    /// empty — there is no commit); empty for everything else.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tree_sha: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommitMeta {
    /// The original commit id (hex) — kept as the fidelity check for
    /// export, not referenced by any layer.
    pub sha: String,
    pub parents: Vec<String>,
    /// Raw `author`/`committer` header values and the message,
    /// hex-coded in RAM (the chain records store the raw bytes).
    pub author_hex: String,
    pub committer_hex: String,
    pub message_hex: String,
    /// Header keys beyond tree/parent/author/committer (gpgsig,
    /// mergetag, encoding …). Non-empty means fast-import cannot
    /// regenerate this commit SHA-exact — export refuses it; the raw
    /// object below preserves the data for a future exact exporter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_headers: Vec<String>,
    /// The complete raw commit object, hex — kept ONLY when
    /// extra_headers is non-empty (it is redundant otherwise).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_hex: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    /// Human name of the mirrored repository ("WHICH git?") — shown in
    /// listings and in attachment box names. `mirror` derives it from
    /// the URL; empty on stores imported before it existed.
    #[serde(default)]
    pub label: String,
    /// Where this mirror fetches from (empty for direct imports).
    #[serde(default)]
    pub url: String,
    pub refs: Vec<RefMeta>,
    /// Newest-first; index i corresponds to chain frame i.
    pub commits: Vec<CommitMeta>,
}

impl CommitMeta {
    /// First line of the commit message, lossily decoded.
    pub fn subject(&self) -> String {
        let msg = hex::decode(&self.message_hex).unwrap_or_default();
        let line = msg.split(|&b| b == b'\n').next().unwrap_or(&[]);
        String::from_utf8_lossy(line).into_owned()
    }
}

// ----------------------------------------------------------------- error

#[derive(Debug)]
pub enum Error {
    Git(String),
    Io(std::io::Error),
    Codec(codec::DecodeError),
    Chain(String),
    Unsupported(String),
    Meta(String),
    /// Another process holds the per-root mirror lock.
    Locked(std::path::PathBuf),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Git(s) => write!(f, "git: {s}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Codec(e) => write!(f, "codec: {e}"),
            Error::Chain(s) => write!(f, "chain: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Meta(s) => write!(f, "meta: {s}"),
            Error::Locked(p) => write!(f, "another mirror run holds {}", p.display()),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<codec::DecodeError> for Error {
    fn from(e: codec::DecodeError) -> Self {
        Error::Codec(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ------------------------------------------------------------- git shell

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

fn git_str(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git(repo, args)?).into_owned())
}


/// Fetch every oid's raw content (any object type) through ONE
/// `git cat-file --batch` process. The oid→bytes map is the import's
/// internal dedup (equal blobs read once) — blob oids never reach a
/// layer; tag oids ride the same batch pipe.
fn fetch_blobs(
    repo: &Path,
    oids: impl IntoIterator<Item = String>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let uniq: std::collections::BTreeSet<String> = oids.into_iter().collect();
    let mut map = BTreeMap::new();
    if uniq.is_empty() {
        return Ok(map);
    }
    // Native object reads (crate::gitobj) — no cat-file subprocess.
    let mut st = crate::gitobj::ObjectStore::open(repo)?;
    for oid in uniq {
        let raw = st.get(&oid)?;
        map.insert(oid, raw);
    }
    Ok(map)
}

/// Returns the meta plus the commit's root tree oid (the `tree`
/// header) — the TREES dedup key.
fn parse_commit(raw: &[u8]) -> Result<(CommitMeta, String)> {
    let body_at = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .ok_or_else(|| Error::Git("commit: no header/body split".into()))?;
    let (headers, message) = (&raw[..body_at], &raw[body_at + 2..]);
    let mut parents = Vec::new();
    let mut tree_oid = None;
    let mut extra_headers = Vec::new();
    let (mut author, mut committer) = (None, None);
    for line in headers.split(|&b| b == b'\n') {
        let sp = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
        let (key, val) = (&line[..sp], &line[sp.min(line.len() - 1) + 1..]);
        if line.first() == Some(&b' ') {
            // Continuation line of a multi-line header (gpgsig PGP
            // block, mergetag body) — the whole raw object is already
            // preserved below; nothing to parse here.
            continue;
        }
        match key {
            b"tree" => tree_oid = Some(String::from_utf8_lossy(val).into_owned()),
            b"parent" => parents.push(String::from_utf8_lossy(val).into_owned()),
            b"author" => author = Some(val.to_vec()),
            b"committer" => committer = Some(val.to_vec()),
            other => {
                // gpgsig / mergetag / encoding … — data we cannot
                // regenerate through fast-import. Record the fact; the
                // RAW object is preserved so nothing is lost, and
                // export refuses these commits explicitly instead of
                // silently minting different SHAs.
                extra_headers.push(String::from_utf8_lossy(other).into_owned());
            }
        }
    }
    let cm = CommitMeta {
        sha: String::new(), // filled by caller
        parents,
        author_hex: hex::encode(author.ok_or_else(|| Error::Git("commit: no author".into()))?),
        committer_hex: hex::encode(
            committer.ok_or_else(|| Error::Git("commit: no committer".into()))?,
        ),
        message_hex: hex::encode(message),
        raw_hex: if extra_headers.is_empty() {
            String::new()
        } else {
            hex::encode(raw)
        },
        extra_headers,
    };
    Ok((cm, tree_oid.ok_or_else(|| Error::Git("commit: no tree header".into()))?))
}

/// Size comparison across encodings of the same history (bytes).
///
/// Two record families over the same commits, newest-first:
/// - **full**: every record is the commit's complete tree layer. Zero
///   sense as a rest form (imagine linux.git) — measured purely as the
///   baseline the delta encodings must beat.
/// - **delta**: record 0 is the newest full layer; record i>0 is
///   `diff(view[i-1], view[i])` — the layer that rebuilds the older view
///   from the next-newer one, walking the chain backward VBF-style.
///
/// Each family measured standalone-zstd-per-record and as a refPrefix
/// chain (frame i anchored on record i-1). The delta refPrefix chain is
/// what the store keeps.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SizeReport {
    pub commits: usize,
    pub zstd_level: i32,
    pub full_raw: u64,
    pub full_standalone: u64,
    pub full_ref_chain: u64,
    pub delta_raw: u64,
    pub delta_standalone: u64,
    /// Delta records refPrefix-anchored on the previous DELTA record.
    pub delta_ref_chain: u64,
    /// The stored form: delta records refPrefix-anchored on the previous
    /// commit's full VIEW bytes (recomputed by the decoder from the
    /// reconstructed view — bit-exact canonical encoding is load-bearing).
    pub view_ref_chain: u64,
    /// One zstd stream over the concatenated full records — the global
    /// redundancy bound (not seekable, comparison only).
    pub solid_full: u64,
}

pub struct ImportOutcome {
    /// The refs observed at import (the walk's per-commit metadata is
    /// NOT carried: hoarding a CommitMeta per commit cost ~17KB/commit
    /// of RSS at linux scale — read it back from the store when needed).
    pub refs: Vec<RefMeta>,
    /// Commits imported.
    pub new_commits: usize,
    /// Present only when the encoding comparison was requested
    /// (import_opts(report=true) / CLI `import --report`) — computing
    /// it recompresses the whole history five extra ways, so the
    /// mirror loop must never pay for it.
    pub report: Option<SizeReport>,
    /// Peak count of live frontier views during the walk (DAG-width
    /// instrumentation; each view is a full in-RAM tree).
    pub max_frontier: usize,
}

/// Refs (bookkeeping): branches + tags only. `refs/pull/*` and friends
/// are excluded — on public forges that forest is unbounded and
/// adversarial (spam PRs merging foreign megahistories).
fn collect_refs(repo: &Path) -> Result<Vec<RefMeta>> {
    let mut refs = Vec::new();
    // %(*objectname)/%(*objecttype) = the FULLY-peeled target (empty for
    // non-tag refs); refname last — it can't contain spaces.
    for line in git_str(
        repo,
        &["for-each-ref",
          "--format=%(objectname) %(objecttype) %(*objectname) %(*objecttype) %(refname)",
          "refs/heads", "refs/tags"],
    )?
    .lines()
    {
        let mut it = line.splitn(5, ' ');
        let (sha, typ, peeled, peeled_typ, name) = (
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
        );
        match typ {
            "commit" => refs.push(RefMeta {
                name: name.to_string(),
                sha: sha.to_string(),
                tag_sha: String::new(),
                tree_sha: String::new(),
            }),
            "tag" => {
                // %(*…) peels one level here (a nested tag shows
                // peeled type "tag") — finish the peel ourselves.
                let (peeled, peeled_typ) = if peeled_typ == "tag" {
                    let full = git_str(repo, &["rev-parse", &format!("{name}^{{}}")])?
                        .trim()
                        .to_string();
                    let typ = git_str(repo, &["cat-file", "-t", &full])?.trim().to_string();
                    (full, typ)
                } else {
                    (peeled.to_string(), peeled_typ.to_string())
                };
                match peeled_typ.as_str() {
                    "commit" => refs.push(RefMeta {
                        name: name.to_string(),
                        sha: peeled,
                        tag_sha: sha.to_string(),
                        tree_sha: String::new(),
                    }),
                    // Tag at a tree (linux v2.6.11-tree): no commit.
                    "tree" => refs.push(RefMeta {
                        name: name.to_string(),
                        sha: String::new(),
                        tag_sha: sha.to_string(),
                        tree_sha: peeled,
                    }),
                    _ => {
                        return Err(Error::Unsupported(format!(
                            "ref {name} is a tag that peels to a {peeled_typ} \
                             (only commit- and tree-target tags are supported)"
                        )));
                    }
                }
            }
            other => {
                return Err(Error::Unsupported(format!(
                    "ref {name} points at a {other}"
                )));
            }
        }
    }
    if refs.is_empty() {
        return Err(Error::Git("repository has no refs".into()));
    }
    Ok(refs)
}

/// `object`/`type` headers of a raw tag object.
pub(crate) fn parse_tag_target(sha: &str, raw: &[u8]) -> Result<(String, String)> {
    let (mut obj, mut typ) = (None, None);
    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            break; // header/message split
        }
        if let Some(v) = line.strip_prefix(b"object ".as_slice()) {
            obj = Some(String::from_utf8_lossy(v).into_owned());
        } else if let Some(v) = line.strip_prefix(b"type ".as_slice()) {
            typ = Some(String::from_utf8_lossy(v).into_owned());
        }
    }
    match (obj, typ) {
        (Some(o), Some(t)) => Ok((o, t)),
        _ => Err(Error::Git(format!("tag {sha}: no object/type header"))),
    }
}

/// The fully-peeled end of a tag chain, by oid.
enum TagPeelOid {
    Commit(String),
    Tree(String),
}

struct TagObj {
    sha: String,
    /// The FULLY-peeled end of the tag's chain.
    target: TagPeelOid,
    raw: Vec<u8>,
}

/// Every tag object reachable from the annotated-tag refs, nested
/// chains expanded, deduped, and ordered inner-first (an inner tag
/// always precedes the outer one whose raw names it — the chain
/// ingest/export order). Raw bytes come through one `cat-file --batch`
/// per nesting level (fetch_blobs).
fn collect_tag_objects(repo: &Path, refs: &[RefMeta]) -> Result<Vec<TagObj>> {
    let mut outers: Vec<String> = refs
        .iter()
        .filter(|r| !r.tag_sha.is_empty())
        .map(|r| r.tag_sha.clone())
        .collect();
    outers.sort();
    outers.dedup();
    // sha → (raw, target sha, target type), grown per nesting level.
    let mut objs: BTreeMap<String, (Vec<u8>, String, String)> = BTreeMap::new();
    let mut pending = outers.clone();
    while !pending.is_empty() {
        let mut next = Vec::new();
        for (sha, raw) in fetch_blobs(repo, pending)? {
            let (t_sha, t_typ) = parse_tag_target(&sha, &raw)?;
            if t_typ == "tag" && !objs.contains_key(&t_sha) {
                next.push(t_sha.clone());
            }
            objs.insert(sha, (raw, t_sha, t_typ));
        }
        pending = next;
    }
    let mut out = Vec::new();
    let mut emitted = std::collections::BTreeSet::new();
    for outer in &outers {
        let mut chain = vec![outer.clone()];
        let target = loop {
            let cur = chain.last().unwrap().clone();
            let (_, t_sha, t_typ) = &objs[&cur];
            match t_typ.as_str() {
                "tag" => chain.push(t_sha.clone()),
                "commit" => break TagPeelOid::Commit(t_sha.clone()),
                "tree" => break TagPeelOid::Tree(t_sha.clone()),
                // collect_refs already refused the ref by name; this
                // backstops direct callers.
                other => {
                    return Err(Error::Unsupported(format!(
                        "tag {cur} targets a {other} ({t_sha})"
                    )))
                }
            }
        };
        let peeled = |t: &TagPeelOid| match t {
            TagPeelOid::Commit(s) => TagPeelOid::Commit(s.clone()),
            TagPeelOid::Tree(s) => TagPeelOid::Tree(s.clone()),
        };
        for sha in chain.into_iter().rev() {
            if emitted.insert(sha.clone()) {
                out.push(TagObj {
                    raw: objs[&sha].0.clone(),
                    target: peeled(&target),
                    sha,
                });
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------- discovery
//
// Import/update walk the history O(changes), not O(tree × history):
// ONE `git rev-list --parents` pass computes the chain landing order
// (`walk_order` — the TREES size model), ONE `git diff-tree --stdin`
// stream fed that order yields every commit's changed paths (vs the
// FIRST parent — the frontier's base), and ONE persistent
// `git cat-file --batch` serves the raw commit objects and the
// changed blobs on demand. No per-commit subprocess, no re-piping of
// unchanged blobs.

/// One persistent `git cat-file --batch` child for a whole run. Strict
/// request-one/read-one interleaving: a single pending request never
/// outgrows the pipe buffer, so no writer thread is needed (contrast
/// fetch_blobs, which streams thousands of requests ahead).
/// Object reads by oid, NATIVE (crate::gitobj): loose zlib + pack/idx with
/// delta chains — no `cat-file` subprocess, no pipe. The name survives from
/// the cat-file era; the API is the same one object-at-a-time `get`.
pub(crate) struct CatFile {
    store: crate::gitobj::ObjectStore,
}

impl CatFile {
    pub(crate) fn new(repo: &Path) -> Result<CatFile> {
        Ok(CatFile { store: crate::gitobj::ObjectStore::open(repo)? })
    }

    /// The raw bytes of one object (any type) — a tree's binary entries or a
    /// blob's content, served by oid straight from the object store.
    pub(crate) fn get(&mut self, oid: &str) -> Result<Vec<u8>> {
        self.store.get(oid)
    }
}

/// The walked scope, from ONE `rev-list --parents` pass (same scope
/// as the change stream; minus `negations` for updates).
struct Dag {
    /// Per-sha child count within the scope (the frontier refcount —
    /// parents outside the streamed set included: those are the update
    /// path's boundary parents).
    counts: HashMap<String, u32>,
    /// The streamed set itself.
    streamed: std::collections::HashSet<String>,
    /// CHAIN LANDING ORDER (parents always before children): the order
    /// `ingest_stream` feeds to `git diff-tree --stdin` and therefore
    /// the order trees land in the TREES chain. Because each chain
    /// record is the reverse delta between chain-NEIGHBORING trees,
    /// this order decides record size — see `walk_order`.
    order: Vec<String>,
}

fn dag_scope(repo: &Path, negations: &[String]) -> Result<Dag> {
    let mut args: Vec<&str> = vec!["rev-list", "--parents", "--branches", "--tags"];
    if !negations.is_empty() {
        // A negation tip can have been pruned from the buffer's
        // object db; --ignore-missing keeps the walk going (known
        // commits that re-stream are skipped by the caller).
        args.push("--ignore-missing");
        args.push("--not");
        args.extend(negations.iter().map(String::as_str));
    }
    let out = git_str(repo, &args)?;
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut streamed = std::collections::HashSet::new();
    // (sha, parents) in rev-list output order — children before
    // parents, so the first zero-child sha of each history comes
    // first. Dropped after `walk_order`.
    let mut commits: Vec<(String, Vec<String>)> = Vec::new();
    for line in out.lines() {
        let mut it = line.split(' ');
        let sha = it.next().unwrap_or_default().to_string();
        streamed.insert(sha.clone());
        let parents: Vec<String> = it.map(str::to_string).collect();
        for p in &parents {
            *counts.entry(p.clone()).or_insert(0) += 1;
        }
        commits.push((sha, parents));
    }
    let order = walk_order(&commits, &streamed);
    Ok(Dag { counts, streamed, order })
}

/// The chain landing order — the size model of the TREES chain.
///
/// TREES records are reverse deltas between chain-NEIGHBORING trees,
/// so this order decides record size: every adjacency between trees
/// from diverged lines of history is paid as a record carrying their
/// full file-level divergence. git's own linearizations interleave
/// lines freely and measured catastrophic on merge-heavy history
/// (git.git, 85k commits: `--topo-order` and `--date-order` both put
/// ~1/3 of commits next to a diverged line at ~1.5MB a record — 94% of
/// all staged bytes, 15GB+ of staging, ENOSPC before finishing).
///
/// The order that restores "record ∝ what the commit touched" is a
/// SEGMENT-AT-ITS-FORK linearization, produced by one Kahn walk over
/// the scope with a LIFO ready stack:
///
/// * a commit becomes ready when all its in-scope parents are emitted;
/// * when a commit is emitted, its now-ready children are pushed
///   sorted by first-parent lineage length (`fpheight`) DESCENDING, so
///   the stack pops SHORT side lines first and the longest line — the
///   mainline continuation — LAST;
/// * the LIFO discipline then keeps every first-parent segment
///   contiguous and places it immediately after its fork: a topic that
///   forked at X lands right after X (its whole divergence from the
///   chain neighbor is the topic itself plus one mainline step), the
///   mainline resumes behind it, and the topic's eventual merge — made
///   ready by its mainline first parent much later — lands
///   first-parent-adjacent, costing exactly its own first-parent
///   change;
/// * unrelated ROOT histories (git.git's gitk/git-gui subtree sources,
///   `todo`) stay whole: their commits gate on nothing outside their
///   own component, so each component drains as ONE contiguous block
///   (shortest component first), costing two whole-tree adjacencies
///   total instead of one per subtree merge or timestamp interleave.
///
/// Parents always precede children (it is a Kahn order), which is the
/// ingest's only correctness requirement; everything else is purely a
/// size optimization. Measured on git.git (85k commits): git-ordered
/// walks headed past 50GB of raw staging (ENOSPC on a 15GB disk,
/// never finished); this order stages 11.4GB raw against an 8.0GB
/// irreducible floor (the sum of every commit's own first-parent
/// old-side blob bytes) and completes in minutes. The residue is real
/// divergence at genuinely era-crossing merges (git.git's cross-maint
/// security waves), not walk noise.
fn walk_order(
    commits: &[(String, Vec<String>)],
    streamed: &std::collections::HashSet<String>,
) -> Vec<String> {
    let n = commits.len();
    let idx: HashMap<&str, u32> =
        commits.iter().enumerate().map(|(i, (s, _))| (s.as_str(), i as u32)).collect();
    // In-scope parent count (readiness gate) and first-parent children.
    let mut pending: Vec<u32> = vec![0; n];
    let mut fp_child_of: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut children: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (i, (_, parents)) in commits.iter().enumerate() {
        let mut first = true;
        for p in parents {
            if let Some(&pi) = idx.get(p.as_str()) {
                pending[i] += 1;
                children[pi as usize].push(i as u32);
                if first {
                    fp_child_of[pi as usize].push(i as u32);
                }
            }
            first = false;
        }
    }
    // fpheight = longest first-parent-only chain hanging off a commit,
    // by DP over one plain Kahn pass (reverse topological order).
    let mut topo: Vec<u32> = Vec::with_capacity(n);
    {
        let mut deg = pending.clone();
        let mut queue: std::collections::VecDeque<u32> = (0..n as u32)
            .filter(|&i| deg[i as usize] == 0)
            .collect();
        while let Some(i) = queue.pop_front() {
            topo.push(i);
            for &c in &children[i as usize] {
                deg[c as usize] -= 1;
                if deg[c as usize] == 0 {
                    queue.push_back(c);
                }
            }
        }
    }
    let mut fpheight: Vec<u32> = vec![1; n];
    for &i in topo.iter().rev() {
        for &c in &fp_child_of[i as usize] {
            fpheight[i as usize] = fpheight[i as usize].max(1 + fpheight[c as usize]);
        }
    }
    // The ready stack. Roots seed it, longest lineage pushed first so
    // the shortest root component drains first and the primary history
    // comes out last (deterministic: ties break on scope index).
    let mut order = Vec::with_capacity(n);
    let mut stack: Vec<u32> = (0..n as u32).filter(|&i| pending[i as usize] == 0).collect();
    stack.sort_by_key(|&i| (std::cmp::Reverse(fpheight[i as usize]), i));
    let mut ready: Vec<u32> = Vec::new();
    while let Some(i) = stack.pop() {
        order.push(commits[i as usize].0.clone());
        ready.clear();
        for &c in &children[i as usize] {
            pending[c as usize] -= 1;
            if pending[c as usize] == 0 {
                ready.push(c);
            }
        }
        // Push longest lineage FIRST so the stack pops short side
        // lines before the mainline continuation (LIFO).
        ready.sort_by_key(|&c| (std::cmp::Reverse(fpheight[c as usize]), c));
        stack.extend(ready.iter().copied());
    }
    debug_assert_eq!(order.len(), streamed.len());
    let _ = streamed;
    order
}

/// The frontier: view-per-commit-with-unprocessed-children, refcounted
/// by remaining children. Views are persistent (Arc-shared subtrees;
/// clone is O(root fanout)), so a fork costs almost nothing and N live
/// views that diverged by k commits cost one tree + O(k·delta), not N
/// trees — the term that OOM'd wide-frontier imports.
/// A blob-oid MULTISET: count per git oid (hex bytes). A multiset, not a
/// set, because the same blob oid can sit at multiple paths, so a removal
/// (D/M of one path) must be refcounted and only drop the key when the last
/// path holding that oid goes. This rides the frontier exactly like the
/// views: a commit's multiset is its FIRST parent's multiset with the
/// commit's changes applied, cloned/moved at the SAME points the view is,
fn ingest_tags(
    repo: &Path,
    ingest: &mut store::Ingest,
    refs: &[RefMeta],
    ls: &crate::lanestore::LaneStore,
) -> Result<()> {
    for t in collect_tag_objects(repo, refs)? {
        if ingest.knows_tag(&t.sha)? {
            continue;
        }
        let peel = match &t.target {
            TagPeelOid::Commit(sha) => store::TagPeel::Commit(sha),
            // A tag at a TREE targets the tagged tree's revision in the
            // union — the tag-tree event the lane store appended (keyed by
            // the tree oid, so nested chains all resolve to it).
            TagPeelOid::Tree(tree) => {
                let rev = ls.rev_of(tree).ok_or_else(|| {
                    Error::Meta(format!(
                        "tagged tree {tree} (tag {}) has no revision in the union",
                        t.sha
                    ))
                })?;
                store::TagPeel::Tree(rev as u64)
            }
        };
        ingest.add_tag(&t.sha, peel, &t.raw)?;
    }
    Ok(())
}

/// Import a git repo into `store` (created; must not exist).
pub fn import(repo: &Path, store: &Path, level: i32) -> Result<ImportOutcome> {
    import_opts(repo, store, level, false)
}

/// `report`: also measure the alternative encodings (full/delta ×
/// standalone/refPrefix + solid bound) — the straightedge's comparison
/// harness, NOT part of storing.
/// Ingest commit metadata (+ its lane reference) for revisions `start..` of
/// the union store `ls`, in union-revision order so each commit's stable index
/// equals its revision. Trees are NOT stored here — a commit points at its tree
/// by `(rev = idx, lane)` into `ls`.
fn ingest_commits(
    repo: &Path,
    ingest: &mut store::Ingest,
    ls: &crate::lanestore::LaneStore,
    start: usize,
) -> Result<usize> {
    let mut cat = CatFile::new(repo)?;
    // `start` counts COMMITS (the chain ordinal), not revisions: tag-tree
    // revisions interleave in the union's revision axis but never land in
    // the COMMITS chain.
    let mut ordinal = 0usize;
    let mut added = 0usize;
    for rev in 0..ls.n_rev() {
        if ls.is_tag_rev(rev) {
            continue;
        }
        ordinal += 1;
        if ordinal <= start {
            continue;
        }
        let sha = ls.sha_at(rev).to_string();
        let raw = cat.get(&sha)?;
        let (mut cm, _tree_oid) = parse_commit(&raw)?;
        cm.sha = sha;
        ingest.add_commit(&cm, ls.lane_of(rev) as u64)?;
        added += 1;
    }
    Ok(added)
}

pub fn import_opts(repo: &Path, store: &Path, level: i32, _report: bool)
    -> Result<ImportOutcome>
{
    let refs = collect_refs(repo)?;
    // 1. Build the union-of-lanes tree store (§1/§2/§7) in `store/trees`.
    let mut st = store::Store::create(store)?;
    let ls = crate::lanestore::LaneStore::encode_repo_union(repo, &store.join("trees"), level)?;
    // 2. Ingest commit metadata + tags + refs, each commit referencing its
    //    `(rev, lane)` tree in the union.
    let mut ingest = store::Ingest::new(&mut st, level)?;
    let new_commits = ingest_commits(repo, &mut ingest, &ls, 0)?;
    ingest_tags(repo, &mut ingest, &refs, &ls)?;
    ingest.finish(&refs)?;
    Ok(ImportOutcome { refs, new_commits, report: None, max_frontier: 0 })
}

// ---------------------------------------------------------------- update

#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub new_commits: usize,
    pub total_commits: usize,
    pub refs: Vec<RefMeta>,
    /// `Depot::prepend` calls made by this update — instrumentation for
    /// the batch invariant (N new commits land as ONE prepend per
    /// touched chain). 0 on the import path.
    pub depot_prepends: u64,
}

/// Incrementally prepend the repo's NEW commits to an existing store
/// (MIRRORS.md phase 3). Cost is proportional to the new history plus
/// the accumulator tier — never the cold tier (the depot's bounded
/// prepend). There is no non-fast-forward case: any commit not yet in
/// the store is simply new records with fresh stable indices, and every
/// observed ref movement (rewrites and deletions included) is one
/// reflog record + a refs-table repoint. Old records keep their indices
/// forever.
pub fn update(repo: &Path, store: &Path, level: i32) -> Result<UpdateOutcome> {
    let refs = collect_refs(repo)?;
    let mut st = store::Store::open(store)?;
    let base_prepends = st.depot_prepends();
    let old_n = st.count(store::COMMITS)? as usize;

    // Incrementally update the union tree store — O(new): only the new
    // revisions' union deltas are folded onto the stored boundary (§11).
    let ls = crate::lanestore::LaneStore::update(repo, &store.join("trees"), level)?;
    let total = ls.n_rev();

    // Ingest ONLY the new commits (revisions `old_n..`), continuing the
    // COMMITS chain (idx == rev).
    let mut ingest = store::Ingest::new(&mut st, level)?;
    let k = ingest_commits(repo, &mut ingest, &ls, old_n)?;
    ingest_tags(repo, &mut ingest, &refs, &ls)?;
    ingest.finish(&refs)?;
    Ok(UpdateOutcome {
        new_commits: k,
        total_commits: total,
        refs,
        depot_prepends: st.depot_prepends() - base_prepends,
    })
}

// ------------------------------------------------- git object identity
//
// The stub is REGENERATED from the store (the one-copy story), so the
// store must be able to recompute git object ids host-side: assembled
// tip commits and materialized snapshot trees are asserted against the
// shas recorded at import. Never stored (the implicit-id rule).

pub(crate) fn git_obj_oid(typ: &str, body: &[u8]) -> String {
    use sha1::Digest as _;
    let mut h = sha1::Sha1::new();
    h.update(format!("{typ} {}\0", body.len()).as_bytes());
    h.update(body);
    hex::encode(h.finalize())
}

/// The git tree oid of a canonical view, bottom-up over assembled tree
/// objects. Entry order is git's: byte order with directory names
/// compared as `name/`; directory mode is `40000` (tree objects carry
/// no leading zero).
/// Public, mode-faithful git tree oid of a reconstructed view — the raw
/// `mode` attr bytes are serialized verbatim (so non-canonical historical
/// modes such as `100664` reproduce exactly). Exposed for verification
/// tooling that must not go through `layer::Mode` (whose canonical enum
/// cannot represent those modes).
pub fn git_tree_oid_of_view(view: &depot::View) -> Result<String> {
    view_tree_oid(view)
}

fn view_tree_oid(view: &depot::View) -> Result<String> {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // (sortkey, raw entry)
    for (name, child) in &view.children {
        let (mode, oid, is_dir) = match &child.blob {
            Some(content) => {
                let mode = child
                    .attrs
                    .get(&b"mode"[..])
                    .map(|m| String::from_utf8_lossy(m).into_owned())
                    .ok_or_else(|| Error::Meta("file node without mode attr".into()))?;
                let oid = if mode == "160000" {
                    // gitlink: the stored blob IS the pinned commit id.
                    String::from_utf8_lossy(content).into_owned()
                } else {
                    git_obj_oid("blob", content)
                };
                (mode, oid, false)
            }
            None => ("40000".to_string(), view_tree_oid(child)?, true),
        };
        let mut raw = mode.trim_start_matches('0').to_string().into_bytes();
        raw.push(b' ');
        raw.extend_from_slice(name);
        raw.push(0);
        raw.extend_from_slice(
            &hex::decode(&oid).map_err(|_| Error::Meta(format!("bad oid {oid}")))?,
        );
        let mut key = name.clone();
        if is_dir {
            key.push(b'/');
        }
        entries.push((key, raw));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut body = Vec::new();
    for (_, raw) in entries {
        body.extend_from_slice(&raw);
    }
    Ok(git_obj_oid("tree", &body))
}

/// Reassemble the raw commit object bytes for a stored record.
/// extra_headers commits carry the complete raw object instead — the
/// assembled form cannot reproduce them.
fn assemble_commit_raw(
    rec: &store::CommitRecord,
    parent_shas: &[String],
    tree_oid: &str,
) -> Vec<u8> {
    if !rec.raw.is_empty() {
        return rec.raw.clone();
    }
    let mut out = format!("tree {tree_oid}\n").into_bytes();
    for p in parent_shas {
        out.extend_from_slice(format!("parent {p}\n").as_bytes());
    }
    out.extend_from_slice(b"author ");
    out.extend_from_slice(&rec.author);
    out.extend_from_slice(b"\ncommitter ");
    out.extend_from_slice(&rec.committer);
    out.extend_from_slice(b"\n\n");
    out.extend_from_slice(&rec.message);
    out
}

/// Write one loose object, asserting the produced id equals the one
/// recorded at import (the stub-side fidelity check).
fn write_object(repo: &Path, typ: &str, bytes: &[u8], expect: &str) -> Result<()> {
    let got = git_stdin(
        repo,
        &["hash-object", "-t", typ, "-w", "--stdin", "--literally"],
        bytes,
    )?;
    if got != expect {
        return Err(Error::Meta(format!(
            "fidelity check failed: {typ} regenerated as {got}, imported as {expect}"
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------ stub
//
// THE STUB CONTRACT (stage-0 validated on file://, local-path and
// https/github transports; git 2.43):
//
// `<root>/repo.git` at rest is a KB-scale SHALLOW STUB, not a clone:
//   * objects: each ref tip's commit object + the annotated-tag object
//     chains the refs point through — nothing else (no trees, no
//     blobs, no history);
//   * refs: the upstream branches+tags verbatim;
//   * `shallow`: the set of peeled tip commit shas — git treats the
//     tips as shallow boundaries, so every local walk (negotiation
//     haves, rev-list, log) stops there instead of dying on missing
//     parents;
//   * config: remote.origin fetching `+refs/heads/*:refs/heads/*` and
//     `+refs/tags/*:refs/tags/*` (NOT `+refs/*:refs/*` like the old
//     full clone: on public forges refs/pull/* is unbounded, and the
//     import never read beyond heads+tags anyway).
//
// BEFORE every fetch the tips' FULL snapshots (trees AND blobs) are
// materialized into the stub from the store (`materialize_snapshots`).
// This is load-bearing three ways, all verified in stage 0:
//   * the server, seeing our shallow lines, assumes we have EXACTLY
//     the tip snapshots and nothing behind them — thin-pack delta
//     bases are then drawn from objects we really have, so index-pack
//     resolves on any transport (no promisor/lazy-fetch tricks);
//   * fetch's connectivity check walks the new tips' full closures
//     down to the boundary — tips' trees/blobs must exist;
//   * `git diff-tree --stdin` emits correct changed-path deltas for commits
//     whose first parent is a boundary tip only if the parent's tree
//     objects exist (git diffs against the parent tree, shallow or
//     not).
// Anything attached BEHIND a tip (e.g. a merge of a branch rooted at
// an old non-tip commit) is simply RESENT by the server — the shallow
// grafts cut the haves closure at the tips, so the server cannot
// assume we kept deeper history. Correctness is unaffected (known
// commits re-streaming are skipped, their views seeded from the
// store); the cost is refetched bytes proportional to how far behind
// the tips new history attaches.
//
// After a successful update the stub is REBUILT FRESH from the store
// (`build_stub_at` + rename) — no prune/repack dance, the fetched pack
// and the materialized snapshots simply vanish with the old directory.

/// Directory size in KiB (buffer-peak instrumentation).
fn dir_kb(dir: &Path) -> u64 {
    fn walk(d: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, acc);
                } else if let Ok(m) = e.metadata() {
                    *acc += m.len();
                }
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n / 1024
}

/// Free KiB on `path`'s filesystem (0 on any failure) — the
/// disk-headroom side of the rung instrumentation.
fn disk_avail_kb(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return 0;
    };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return 0;
    }
    (s.f_bavail as u64).saturating_mul(s.f_frsize as u64) / 1024
}

fn init_stub_dir(dir: &Path, url: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    git(dir, &["init", "-q", "--bare"])?;
    git(dir, &["config", "remote.origin.url", url])?;
    git(dir, &["config", "remote.origin.fetch", "+refs/heads/*:refs/heads/*"])?;
    git(dir, &["config", "--add", "remote.origin.fetch", "+refs/tags/*:refs/tags/*"])?;
    Ok(())
}

/// Write refs + the shallow boundary for an already-object-complete
/// stub directory. `refs` are the current refs (annotated tags point
/// at their tag object).
fn write_stub_refs(dir: &Path, refs: &[RefMeta]) -> Result<()> {
    let mut lines = String::new();
    let mut tips: Vec<&str> = Vec::new();
    for r in refs {
        let obj = if r.tag_sha.is_empty() { &r.sha } else { &r.tag_sha };
        lines.push_str(&format!("update {} {}\n", r.name, obj));
        if !r.sha.is_empty() {
            tips.push(&r.sha);
        }
    }
    git_stdin(dir, &["update-ref", "--stdin"], lines.as_bytes())?;
    tips.sort_unstable();
    tips.dedup();
    std::fs::write(dir.join("shallow"), tips.join("\n") + "\n")?;
    Ok(())
}

/// Build a fresh stub at `dir` from the store: tip commit objects are
/// REGENERATED (raw bytes assembled from the record — extra_headers
/// commits use their preserved raw object — with the tree oid
/// recomputed from the stored view) and sha-asserted; tag chains are
/// written from their stored raw bytes, inner-first.
fn build_stub_at(dir: &Path, st: &store::Store, url: &str) -> Result<()> {
    init_stub_dir(dir, url)?;
    let refs = st.refs_meta()?;
    // Distinct peeled tip commits + their records.
    let mut recs: BTreeMap<String, store::CommitRecord> = BTreeMap::new();
    for r in &refs {
        if !r.sha.is_empty() && !recs.contains_key(&r.sha) {
            let idx = st.sha_to_idx(&r.sha)?.ok_or_else(|| {
                Error::Meta(format!("ref {} target {} not in store", r.name, r.sha))
            })?;
            recs.insert(r.sha.clone(), st.commit_record_at(idx)?);
        }
    }
    // The views of every tip needing an assembled commit (raw-carrying
    // records skip it), reconstructed from the union store.
    let ls = st.union()?;
    let mut views: HashMap<String, depot::View> = HashMap::new();
    for c in recs.values().filter(|c| c.raw.is_empty()) {
        views.insert(c.sha.clone(), ls.tree_view_of_commit(&c.sha)?);
    }
    for (sha, rec) in &recs {
        let parents = rec
            .parent_idxs
            .iter()
            .map(|p| {
                st.idx_to_sha(*p)?
                    .ok_or_else(|| Error::Meta(format!("parent index {p} not in chain")))
            })
            .collect::<Result<Vec<_>>>()?;
        let tree_oid = match views.get(sha) {
            Some(v) => view_tree_oid(v)?,
            None => String::new(), // raw-carrying record: oid unused
        };
        write_object(dir, "commit", &assemble_commit_raw(rec, &parents, &tree_oid), sha)?;
    }
    // Tag chains, inner-first (raw bytes are the stored fidelity
    // payload; `object` lines name objects written above or earlier in
    // the chain — or, for tree tags, a tree that need not exist for
    // hash-object/update-ref).
    let mut written = std::collections::BTreeSet::new();
    for r in &refs {
        if r.tag_sha.is_empty() {
            continue;
        }
        let mut chain = Vec::new();
        let mut idx = st.tag_sha_to_idx(&r.tag_sha)?.ok_or_else(|| {
            Error::Meta(format!("tag object {} for ref {} not in store", r.tag_sha, r.name))
        })?;
        loop {
            let rec = st.tag_record_at(idx)?;
            let (obj, typ) = parse_tag_target(&rec.sha, &rec.raw)?;
            chain.push(rec);
            if typ != "tag" {
                break;
            }
            idx = st
                .tag_sha_to_idx(&obj)?
                .ok_or_else(|| Error::Chain(format!("inner tag {obj} not in chain")))?;
        }
        for rec in chain.iter().rev() {
            if written.insert(rec.sha.clone()) {
                write_object(dir, "tag", &rec.raw, &rec.sha)?;
            }
        }
    }
    write_stub_refs(dir, &refs)?;
    // Loose refs and objects cost a filesystem block EACH — a repo
    // with hundreds of tags would idle at MBs of block overhead. Two
    // files instead: pack-refs, and pack-objects over the EXPLICIT
    // loose-object list (repack would walk tip trees, which a stub
    // deliberately lacks).
    git(dir, &["pack-refs", "--all"])?;
    let mut oids = String::new();
    for d in std::fs::read_dir(dir.join("objects"))?.flatten() {
        let fan = d.file_name().to_string_lossy().into_owned();
        if fan.len() != 2 || !d.path().is_dir() {
            continue;
        }
        for f in std::fs::read_dir(d.path())?.flatten() {
            oids.push_str(&format!("{fan}{}\n", f.file_name().to_string_lossy()));
        }
    }
    if !oids.is_empty() {
        // Relative to the repo: `git -C` chdirs, so an absolute base
        // is wrong exactly when the caller's root path is relative.
        git_stdin(dir, &["pack-objects", "-q", "objects/pack/pack"], oids.as_bytes())?;
        git(dir, &["prune-packed", "-q"])?;
    }
    Ok(())
}

/// Materialize full snapshots (trees AND blobs) for `views` into
/// `repo` through ONE `git fast-import` run — the stub contract's
/// pre-fetch step. Blobs dedup through marks; each view lands as a
/// throwaway commit on a scratch ref (deleted after) whose tree is,
/// by construction, the tip's real tree.
/// Returns the pack files the run created (so a bootstrap can carry
/// snapshot packs across re-pins instead of rebuilding them).
fn materialize_snapshots<'a>(
    repo: &Path,
    views: impl IntoIterator<Item = &'a depot::View>,
) -> Result<Vec<String>> {
    let pack_dir = repo.join("objects/pack");
    let before: std::collections::BTreeSet<String> = list_dir(&pack_dir);
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        // unpackLimit=1: ALWAYS emit a pack — snapshot objects must be
        // a carriable pack file, never loose (re-pins carry packs by
        // name; fast-import explodes small packs to loose by default).
        .args(["-c", "fastimport.unpackLimit=1", "fast-import", "--quiet", "--done", "--force"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    // Stream straight into the child: the union of snapshots can be
    // checkout-sized and must not be assembled in RAM.
    let mut stdin = std::io::BufWriter::new(child.stdin.take().expect("piped stdin"));
    // Dedup by git blob oid, never by content: content keys copy every
    // distinct blob into the map (a union of linux-tip snapshots is GBs
    // — this walk once drove the driver to a 15.7G OOM).
    let mut blob_marks: HashMap<String, usize> = HashMap::new();
    let mut next_mark = 1usize;
    let mut wrote_any = false;
    for view in views {
        wrote_any = true;
        let mut files = Vec::new();
        walk_files(view, &mut Vec::new(), &mut files)?;
        let mut file_marks: Vec<usize> = Vec::with_capacity(files.len());
        for (_, mode, content) in &files {
            if mode == "160000" {
                file_marks.push(0);
                continue;
            }
            let oid = git_obj_oid("blob", content);
            if let Some(&m) = blob_marks.get(&oid) {
                file_marks.push(m);
                continue;
            }
            blob_marks.insert(oid, next_mark);
            file_marks.push(next_mark);
            stdin.write_all(
                format!("blob\nmark :{next_mark}\ndata {}\n", content.len()).as_bytes(),
            )?;
            stdin.write_all(content)?;
            stdin.write_all(b"\n")?;
            next_mark += 1;
        }
        stdin.write_all(
            b"commit refs/gitdepot/seed\ncommitter gitdepot <gitdepot@localhost> 0 +0000\ndata 0\ndeleteall\n",
        )?;
        for ((path, mode, content), m) in files.iter().zip(&file_marks) {
            if mode == "160000" {
                let sha = String::from_utf8_lossy(content);
                stdin.write_all(format!("M 160000 {sha} {}\n", quote_path(path)).as_bytes())?;
            } else {
                stdin.write_all(format!("M {mode} :{m} {}\n", quote_path(path)).as_bytes())?;
            }
        }
        stdin.write_all(b"\n")?;
    }
    stdin.write_all(b"done\n")?;
    drop(stdin);
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "fast-import (snapshots): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if wrote_any {
        git(repo, &["update-ref", "-d", "refs/gitdepot/seed"])?;
    }
    Ok(list_dir(&pack_dir).difference(&before).cloned().collect())
}

fn list_dir(dir: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Pre-fetch step of the update path: snapshots of ref trees — tip
/// commit trees AND tag@tree tagged trees (rev-list over the buffer
/// refs must be able to PARSE every peel target) — views
/// reconstructed from the store in one walk. `only` restricts to the
/// named refs (the moved-refs heuristic: new history almost always
/// attaches at a moved ref's old tip; the caller falls back to
/// everything + one retry when the fetch proves the heuristic wrong).
fn materialize_tip_snapshots(
    repo: &Path,
    st: &store::Store,
    only: Option<&std::collections::BTreeSet<String>>,
) -> Result<()> {
    let ls = st.union()?;
    let mut views: Vec<depot::View> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for r in st.refs_meta()? {
        if only.is_some_and(|f| !f.contains(&r.name)) {
            continue;
        }
        if r.sha.is_empty() {
            continue;
        }
        if seen.insert(r.sha.clone()) {
            views.push(ls.tree_view_of_commit(&r.sha)?);
        }
    }
    // A live boundary lane tip can be unreachable from the current refs
    // (a deleted branch), yet the incremental update reconstructs its tree
    // from the boundary — so it must be present in the buffer too. When
    // `only` is set (the moved-refs fast path) these are left out; the
    // caller falls back to the full pass on a boundary miss.
    if only.is_none() {
        for sha in crate::lanestore::LaneStore::boundary_tip_shas(&st.root().join("trees"))? {
            if seen.insert(sha.clone()) {
                views.push(ls.tree_view_of_commit(&sha)?);
            }
        }
    }
    materialize_snapshots(repo, views.iter())?;
    Ok(())
}

// ---------------------------------------------------------------- mirror

/// Buffer instrumentation for one bootstrap rung.
#[derive(Debug, Clone)]
pub struct RungStat {
    /// Refs fetched by this rung (0 = the final converge rung).
    pub refs: usize,
    pub new_commits: usize,
    /// Peak count of live frontier views during the rung's walk (each
    /// is a full in-RAM tree — the walk's dominant memory driver now
    /// that per-commit metadata is no longer hoarded).
    pub frontier_peak: usize,
    /// repo.git size right after the rung's fetch — the moment the
    /// buffer peaks (pack + snapshots + stub, before the re-pin).
    pub buffer_peak_kb: u64,
    /// Raw bytes still staged in the ingest (RAM + spill, all three
    /// stages) at the end of the rung — grows across rungs until the
    /// single landing at finish (the spill keeps it compressed on
    /// disk, not in RAM).
    pub staged_kb: u64,
    /// Store directory size at the end of the rung.
    pub store_kb: u64,
    /// Free space on the store's filesystem (statvfs; 0 if the call
    /// fails).
    pub disk_avail_kb: u64,
}

#[derive(Debug, Clone)]
pub struct MirrorOutcome {
    pub update: UpdateOutcome,
    /// Bootstrap rungs (empty for update ticks and --whole imports).
    pub rungs: Vec<RungStat>,
    /// repo.git size right after the update tick's fetch (0 for
    /// bootstrap/no-op runs) — the transient peak the stub contract
    /// trades the persistent clone for.
    pub buffer_peak_kb: u64,
}

#[derive(Debug, Clone)]
pub struct MirrorOpts {
    /// Drop repo.git entirely after the run (even the stub); the next
    /// run rebuilds it from the store.
    pub frugal: bool,
    /// First contact: single-shot `clone --mirror` + import instead of
    /// the laddered bootstrap.
    pub whole: bool,
    /// Tags per bootstrap rung.
    pub tag_wave: usize,
}

impl Default for MirrorOpts {
    fn default() -> Self {
        MirrorOpts { frugal: false, whole: false, tag_wave: 16 }
    }
}

/// "WHICH git?" — the repo's human name, from its URL's last path
/// segment (`.git` stripped): `https://host/o/hello-world.git` →
/// `hello-world`.
pub fn label_from_url(url: &str) -> String {
    let tail = url.trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(url);
    tail.strip_suffix(".git").unwrap_or(tail).to_string()
}

pub fn mirror(url: &str, root: &Path) -> Result<MirrorOutcome> {
    mirror_opts(url, root, MirrorOpts::default())
}

/// The fetch-and-update loop for one remote: keep `<root>/store` (the
/// ONLY authoritative copy) in sync with `url`, using `<root>/repo.git`
/// — the KB-scale shallow stub (contract above) — as the transient
/// fetch buffer. First contact bootstraps the store through the
/// laddered fetch (or one `clone --mirror` with `whole`); later calls
/// materialize tip snapshots, fetch the delta, run the incremental
/// `update`, and re-pin the stub. A rewritten remote is just an
/// update: new records + repointed refs — the mirror follows the
/// remote AND keeps every commit it ever held resolvable.
pub fn mirror_opts(url: &str, root: &Path, opts: MirrorOpts) -> Result<MirrorOutcome> {
    std::fs::create_dir_all(root)?;
    // One-run-per-root guard: exclusive flock on <root>/.lock, held for
    // the whole run, kernel-released on ANY exit (crash included) — two
    // schedulers can never drive git against the same buffer/store.
    let _lock = {
        use std::os::fd::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .create(true).truncate(false).write(true)
            .open(root.join(".lock"))?;
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(Error::Locked(root.to_path_buf()));
        }
        f
    };
    let repo = root.join("repo.git");
    let store = root.join("store");
    // A crashed bootstrap/import leaves an EMPTY store (nothing lands
    // before the final one-prepend-per-chain flush): wipe and restart
    // from zero — the staging log was scratch, there is no partial
    // store to resume.
    if store::store_exists(&store) && store::commit_count(&store)? == 0 {
        std::fs::remove_dir_all(&store)?;
        if repo.exists() {
            std::fs::remove_dir_all(&repo)?;
        }
    }
    let scratch = root.join("repo.git.new");
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)?;
    }
    let out = if !store::store_exists(&store) {
        if opts.whole {
            // Single-shot first contact: clone into scratch + rename —
            // git creates HEAD long before a clone completes, so the
            // rename makes repo.git's existence imply a COMPLETE clone.
            if repo.exists() {
                std::fs::remove_dir_all(&repo)?;
            }
            let c = Command::new("git")
                .args(["clone", "--quiet", "--mirror", url])
                .arg(&scratch)
                .output()?;
            if !c.status.success() {
                return Err(Error::Git(format!(
                    "clone --mirror {url}: {}",
                    String::from_utf8_lossy(&c.stderr).trim()
                )));
            }
            std::fs::rename(&scratch, &repo)?;
            let o = import(&repo, &store, 3)?;
            let n = o.new_commits;
            MirrorOutcome {
                update: UpdateOutcome {
                    new_commits: n,
                    total_commits: n,
                    refs: o.refs,
                    depot_prepends: 0,
                },
                rungs: Vec::new(),
                buffer_peak_kb: 0,
            }
        } else {
            let (update, rungs) = bootstrap(url, root, &store, 3, opts.tag_wave.max(1))?;
            MirrorOutcome { update, rungs, buffer_peak_kb: 0 }
        }
    } else {
        // No-op short-circuit: if the advertised refs equal the
        // store's, there is nothing to fetch and nothing to re-pin —
        // the tick costs one ls-remote.
        let advertised = ls_remote(url)?;
        {
            let st = store::Store::open(&store)?;
            if refs_in_sync(&advertised, &st.refs_meta()?) {
                let refs = st.refs_meta()?;
                let total = st.count(store::COMMITS)? as usize;
                drop(st);
                stamp_identity(&store, url)?;
                if opts.frugal && repo.exists() {
                    std::fs::remove_dir_all(&repo)?;
                }
                return Ok(MirrorOutcome {
                    update: UpdateOutcome {
                        new_commits: 0,
                        total_commits: total,
                        refs,
                        depot_prepends: 0,
                    },
                    rungs: Vec::new(),
                    buffer_peak_kb: 0,
                });
            }
        }
        // The stub is DERIVED: a missing/incomplete one (deleted by
        // `mirror rm`, dropped by --frugal, or a crashed rebuild) is
        // rebuilt from the store. The old export-based full reseed is
        // gone — a stub costs O(tips), never O(history).
        if !repo.join("HEAD").exists() {
            if repo.exists() {
                std::fs::remove_dir_all(&repo)?;
            }
            let st = store::Store::open(&store)?;
            build_stub_at(&scratch, &st, url)?;
            drop(st);
            std::fs::rename(&scratch, &repo)?;
        } else {
            // The remote can move between mirrors of the same root.
            git(&repo, &["config", "remote.origin.url", url])?;
        }
        {
            let st = store::Store::open(&store)?;
            // EVERY tip's snapshot, not just moved refs': the server
            // excludes each shallow have's snapshot from the pack
            // regardless of movement, so any unmaterialized tip is a
            // potential hole the walk trips over later (proven by the
            // rewrite tests: an amend resends the parent whose tree
            // shares subtrees with an unmoved tag's snapshot).
            materialize_tip_snapshots(&repo, &st, None)?;
        }
        git(&repo, &["fetch", "--quiet", "--prune", "origin"])?;
        let buffer_peak_kb = dir_kb(&repo);
        MirrorOutcome { update: update(&repo, &store, 3)?, rungs: Vec::new(), buffer_peak_kb }
    };
    stamp_identity(&store, url)?;
    // Re-pin: replace the buffer with a fresh stub built from the
    // store — packs and materialized snapshots vanish with the old
    // directory.
    {
        let st = store::Store::open(&store)?;
        build_stub_at(&scratch, &st, url)?;
    }
    if repo.exists() {
        std::fs::remove_dir_all(&repo)?;
    }
    std::fs::rename(&scratch, &repo)?;
    if opts.frugal {
        std::fs::remove_dir_all(&repo)?;
    }
    Ok(out)
}

/// Stamp identity ("WHICH git?") — listings and attachment names key
/// off it.
fn stamp_identity(store: &Path, url: &str) -> Result<()> {
    let (label, old_url) = store::identity(store)?;
    if label.is_empty() || old_url != url {
        store::set_identity(store, &label_from_url(url), url)?;
    }
    Ok(())
}

// ------------------------------------------------------------- bootstrap

/// One `git ls-remote` advertisement entry.
struct LsRef {
    obj: String,
    /// The `^{}` peel (annotated tags); the object itself otherwise.
    peeled: String,
}

/// `git ls-remote` refs: name → (object, peeled).
fn ls_remote(url: &str) -> Result<BTreeMap<String, LsRef>> {
    let out = Command::new("git").args(["ls-remote", "--", url]).output()?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "ls-remote {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut map: BTreeMap<String, LsRef> = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((sha, name)) = line.split_once('\t') else { continue };
        match name.strip_suffix("^{}") {
            Some(base) => {
                if let Some(r) = map.get_mut(base) {
                    r.peeled = sha.to_string();
                }
            }
            None => {
                map.insert(
                    name.to_string(),
                    LsRef { obj: sha.to_string(), peeled: sha.to_string() },
                );
            }
        }
    }
    Ok(map)
}

/// Advertised heads+tags == the store's current refs (the no-op-tick
/// test). Other namespaces (refs/pull/*, HEAD) are outside the mirror
/// scope on both sides.
fn refs_in_sync(advertised: &BTreeMap<String, LsRef>, stored: &[RefMeta]) -> bool {
    let scoped: BTreeMap<&str, &str> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/heads/") || n.starts_with("refs/tags/"))
        .map(|(n, r)| (n.as_str(), r.obj.as_str()))
        .collect();
    if scoped.len() != stored.len() {
        return false;
    }
    stored.iter().all(|r| {
        let obj = if r.tag_sha.is_empty() { &r.sha } else { &r.tag_sha };
        scoped.get(r.name.as_str()) == Some(&obj.as_str())
    })
}

/// Natural-version sort key: alternating (text, number) segments so
/// `v0.9 < v0.10 < v1.0`. Chronological ordering was rejected: peeled
/// committer dates are unknowable before fetching the tag objects, and
/// fetching them cheaply (a `--filter=tree:0` wave) poisons the object
/// store for later rungs (present-but-filtered wants make fetch skip
/// the closure). Ordering only affects BUFFERING, never correctness —
/// every rung's haves are all previously imported tips.
fn natural_key(name: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut chars = name.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut n = 0u64;
            while let Some(&d) = chars.peek() {
                let Some(v) = d.to_digit(10) else { break };
                n = n.saturating_mul(10).saturating_add(v as u64);
                chars.next();
            }
            out.push((std::mem::take(&mut text), n));
        } else {
            text.push(c);
            chars.next();
        }
    }
    if !text.is_empty() {
        out.push((text, 0));
    }
    out
}

/// First contact, laddered: fetch the history in rungs (waves of tags
/// in natural-version order, then a converge fetch of everything) so
/// the fetch buffer peaks at one rung, not the whole clone — while the
/// STORE ingest stays ONE turn: every rung's records stage through
/// the same `Ingest` (spilling to a compressed scratch log past the
/// RAM bound) and land as EXACTLY ONE prepend per touched chain at
/// `finish()` — f0 + one stream-composed f1, sealed to cold
/// immediately when it dwarfs the seal threshold. Rung boundaries are
/// invisible to the walk: boundary views come from the staged reverse
/// deltas (`Ingest::staged_views`), and the sha→tree-oid memo spans
/// rungs. Crash mid-ladder = restart from zero (the staging log is
/// scratch; nothing lands before finish).
///
/// Peak disk = one rung's pack + materialized tip snapshots + the
/// bounded compressed staging log + the stub — never an uncompressed
/// copy of the whole history (frames stream in and out of the codec).
fn bootstrap(
    url: &str,
    root: &Path,
    store_path: &Path,
    level: i32,
    tag_wave: usize,
) -> Result<(UpdateOutcome, Vec<RungStat>)> {
    let repo = root.join("repo.git");
    if repo.exists() {
        std::fs::remove_dir_all(&repo)?;
    }
    init_stub_dir(&repo, url)?;
    let advertised = ls_remote(url)?;
    // (tag name, peeled commit) in natural-version order.
    let mut pending: std::collections::VecDeque<(String, String)> = {
        let mut v: Vec<(String, String)> = advertised
            .iter()
            .filter_map(|(n, r)| {
                n.strip_prefix("refs/tags/")
                    .map(|t| (t.to_string(), r.peeled.clone()))
            })
            .collect();
        v.sort_by_key(|(t, _)| natural_key(t));
        v.into()
    };
    if pending.is_empty() {
        eprintln!("gitdepot: no tags upstream — bootstrap falls back to one full fetch rung");
    }

    let mut st = store::Store::create(store_path)?;
    let trees = store_path.join("trees");
    let mut ingest = store::Ingest::new(&mut st, level)?;
    // Commits already added to `ingest` (== union revisions folded so far).
    let mut ingested = 0usize;
    let mut known_all: std::collections::HashSet<String> = Default::default();
    let mut prev_tips: Vec<String> = Vec::new();
    // Tips that appeared since the last snapshot materialization.
    let mut fresh_tips: Vec<String> = Vec::new();
    // Snapshot pack files carried across re-pins.
    let mut snap_packs: Vec<String> = Vec::new();
    let mut total_new = 0usize;
    let mut rungs = Vec::new();

    let mut rung_no = 0usize;
    let mut done = false;
    while !done {
        // Rung selection. READY tags — peeled commits already imported
        // by earlier rungs — cost only their tag objects, so they ship
        // in bulk; this is what keeps a multi-namespace tag forest
        // (crate tags peeling into mid-history) from triggering the
        // shallow-cut resend (stage-0: the server cuts the haves
        // closure at our shallow tips, so a want attaching BEHIND a
        // tip refetches everything between). Otherwise the next
        // natural-order wave is a real rung. The converge fetch runs
        // once pending is drained.
        let ready: Vec<String> = pending
            .iter()
            .filter(|(_, peel)| known_all.contains(peel))
            .map(|(t, _)| t.clone())
            .collect();
        let specs: Option<Vec<String>> = if !ready.is_empty() {
            let take: Vec<String> = ready.into_iter().take(400).collect();
            pending.retain(|(t, _)| !take.contains(t));
            Some(take.iter().map(|t| format!("+refs/tags/{t}:refs/tags/{t}")).collect())
        } else if !pending.is_empty() {
            let take: Vec<(String, String)> =
                pending.drain(..tag_wave.min(pending.len())).collect();
            Some(
                take.iter()
                    .map(|(t, _)| format!("+refs/tags/{t}:refs/tags/{t}"))
                    .collect(),
            )
        } else {
            done = true;
            None
        };
        rung_no += 1;
        // Pre-fetch: the stub contract's snapshot materialization,
        // views from the union store (landed by the previous rung's
        // update). EVERY current tip must be covered before any fetch
        // (the server excludes each shallow have's snapshot
        // unconditionally), but snapshot PACKS survive the re-pins, so
        // each rung only builds the tips that appeared since the last.
        if !fresh_tips.is_empty() {
            let ls = crate::lanestore::LaneStore::open(&trees)?;
            let mut views = Vec::new();
            for sha in &fresh_tips {
                views.push(ls.tree_view_of_commit(sha)?);
            }
            snap_packs.extend(materialize_snapshots(&repo, views.iter())?);
            fresh_tips.clear();
        }
        let rung_refs = specs.as_ref().map_or(0, |s| s.len());
        match &specs {
            Some(specs) => {
                let mut args: Vec<&str> = vec!["fetch", "--quiet", "origin"];
                args.extend(specs.iter().map(String::as_str));
                git(&repo, &args)?;
            }
            None => {
                git(&repo, &["fetch", "--quiet", "--prune", "origin"])?;
            }
        }
        let buffer_peak_kb = dir_kb(&repo);
        // Fold this rung's new commits into the union tree store — O(new):
        // the boundary is reconstructed from the store, only the newly
        // fetched revisions are walked and encoded. Then ingest their
        // commit metadata into the COMMITS chain (idx == rev).
        let ls = if trees.exists() {
            crate::lanestore::LaneStore::update(&repo, &trees, level)?
        } else {
            crate::lanestore::LaneStore::encode_repo_union(&repo, &trees, level)?
        };
        let rung_new = ingest_commits(&repo, &mut ingest, &ls, ingested)?;
        ingested = ls.n_rev();
        // Refresh the imported-commit set for the next rung's READY-tag
        // selection.
        known_all = (0..ls.n_rev()).map(|r| ls.sha_at(r).to_string()).collect();
        let frontier_peak = 0usize;
        total_new += rung_new;
        let staged_kb = ingest.staged_bytes() / 1024;
        let store_kb = dir_kb(store_path);
        let avail_kb = disk_avail_kb(store_path);
        eprintln!(
            "gitdepot: bootstrap rung {rung_no}: {rung_refs} refs, {rung_new} new \
             commits, frontier-peak {frontier_peak}, buffer {buffer_peak_kb}K, \
             staged {staged_kb}K, store {store_kb}K, disk-avail {avail_kb}K"
        );
        let refs_now = collect_refs(&repo)?;
        let old_tips: std::collections::BTreeSet<&String> = prev_tips.iter().collect();
        let mut tips: Vec<String> = refs_now
            .iter()
            .filter(|r| !r.sha.is_empty())
            .map(|r| r.sha.clone())
            .collect();
        tips.sort_unstable();
        tips.dedup();
        fresh_tips = tips.iter().filter(|t| !old_tips.contains(t)).cloned().collect();
        prev_tips = tips;
        rungs.push(RungStat {
            refs: rung_refs,
            new_commits: rung_new,
            frontier_peak,
            buffer_peak_kb,
            staged_kb,
            store_kb,
            disk_avail_kb: avail_kb,
        });
        if !done {
            repin_buffer(&repo, url, &refs_now, &snap_packs)?;
        }
    }

    let refs = collect_refs(&repo)?;
    // Tag-tree revisions were appended by the last rung's union encode;
    // reopen the lane store to resolve their revision indices.
    let ls = crate::lanestore::LaneStore::open(&trees)?;
    ingest_tags(&repo, &mut ingest, &refs, &ls)?;
    ingest.finish(&refs)?;
    let total = st.count(store::COMMITS)? as usize;
    let prepends = st.depot_prepends();
    Ok((
        UpdateOutcome {
            new_commits: total_new,
            total_commits: total,
            refs,
            depot_prepends: prepends,
        },
        rungs,
    ))
}

/// Mid-ladder re-pin: shrink the buffer back to stub shape (tip
/// commits + tag chains + refs + shallow) from ITS OWN objects — the
/// store has nothing landed yet. Rebuild-fresh + rename; the rung's
/// pack vanishes with the old directory.
fn repin_buffer(repo: &Path, url: &str, refs: &[RefMeta], keep_packs: &[String]) -> Result<()> {
    let tags = collect_tag_objects(repo, refs)?;
    let tips: std::collections::BTreeSet<String> = refs
        .iter()
        .filter(|r| !r.sha.is_empty())
        .map(|r| r.sha.clone())
        .collect();
    let commit_raws = fetch_blobs(repo, tips.iter().cloned())?;
    // A tag@tree's peeled tree closure must SURVIVE the re-pin: the
    // final ingest_tags reads it, and a later fetch will never resend
    // it (the tag object being present satisfies the want). Rare and
    // one-tree-sized.
    let mut tree_objs: Vec<(String, String)> = Vec::new(); // (oid, type)
    for r in refs {
        if r.tree_sha.is_empty() {
            continue;
        }
        for line in git_str(repo, &["rev-list", "--objects", &r.tree_sha])?.lines() {
            let oid = line.split(' ').next().unwrap_or_default().to_string();
            if oid.is_empty() {
                continue;
            }
            let typ = git_str(repo, &["cat-file", "-t", &oid])?.trim().to_string();
            tree_objs.push((oid, typ));
        }
    }
    let scratch = repo.with_extension("git.repin");
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)?;
    }
    init_stub_dir(&scratch, url)?;
    for (sha, raw) in &commit_raws {
        write_object(&scratch, "commit", raw, sha)?;
    }
    for t in &tags {
        write_object(&scratch, "tag", &t.raw, &t.sha)?;
    }
    if !tree_objs.is_empty() {
        let raws = fetch_blobs(repo, tree_objs.iter().map(|(o, _)| o.clone()))?;
        for (oid, typ) in &tree_objs {
            write_object(&scratch, typ, &raws[oid], oid)?;
        }
    }
    write_stub_refs(&scratch, refs)?;
    // Snapshot packs ride along: the next rung's fetch still needs
    // every tip snapshot present (rebuilding them each rung would be
    // O(rungs × union-of-snapshots) fast-import work).
    std::fs::create_dir_all(scratch.join("objects/pack"))?;
    for name in keep_packs {
        let from = repo.join("objects/pack").join(name);
        if from.exists() {
            std::fs::copy(&from, scratch.join("objects/pack").join(name))?;
        }
    }
    std::fs::remove_dir_all(repo)?;
    std::fs::rename(&scratch, repo)?;
    Ok(())
}

/// One row of `list_mirrors`.
#[derive(Debug, Clone)]
pub struct MirrorEntry {
    /// Directory under the mirrors root (`<root>/<dir>/store`).
    pub dir: String,
    pub label: String,
    pub url: String,
    pub commits: usize,
    pub refs: Vec<RefMeta>,
}

/// Scan a mirrors root for `<root>/*/store` bookkeeping — the answer to
/// "which repos do I have?". Point reads only (identity, count, refs) —
/// no commit-list materialization per store.
pub fn list_mirrors(root: &Path) -> Result<Vec<MirrorEntry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(root)?.flatten() {
        let store = e.path().join("store");
        if !store::store_exists(&store) {
            continue;
        }
        let (label, url) = store::identity(&store)?;
        out.push(MirrorEntry {
            dir: e.file_name().to_string_lossy().into_owned(),
            label: if label.is_empty() {
                e.file_name().to_string_lossy().into_owned()
            } else {
                label
            },
            url,
            commits: store::commit_count(&store)?,
            refs: store::refs(&store)?,
        });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
}

// ---------------------------------------------------------------- export

fn quote_path(p: &[u8]) -> String {
    let mut s = String::from("\"");
    for &b in p {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            0x20..=0x7e => s.push(b as char),
            other => s.push_str(&format!("\\{:03o}", other)),
        }
    }
    s.push('"');
    s
}

fn walk_files<'a>(
    view: &'a depot::View,
    prefix: &mut Vec<u8>,
    out: &mut Vec<(Vec<u8>, String, &'a [u8])>,
) -> Result<()> {
    if let Some(content) = &view.blob {
        let mode = view
            .attrs
            .get(&b"mode"[..])
            .ok_or_else(|| Error::Meta("file node without mode attr".into()))?;
        out.push((
            prefix.clone(),
            String::from_utf8_lossy(mode).into_owned(),
            content,
        ));
    }
    for (name, child) in &view.children {
        let len = prefix.len();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(name);
        walk_files(child, prefix, out)?;
        prefix.truncate(len);
    }
    Ok(())
}


/// Feed `input` to a git command and return trimmed stdout.
fn git_stdin(repo: &Path, args: &[&str], input: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(input)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Export a store into a fresh git repository at `repo` (must not be an
/// existing repo; `git init` is run there). Returns the regenerated tip
/// shas by ref name; fails if any regenerated commit id differs from the
/// one recorded at import (the fidelity check).
// ------------------------------------------------------- union tree store
//
// The design's §1/§2 path made reachable from the CLI: the revision-indexed
// UNION of the live lanes' git trees, encoded in the §2 variant tree
// (`layer.rs`) and stored newest-full / older-reverse (§7) — driven straight
// off git by `lanestore::encode_repo_union`. A commit resolves to `(revision,
// lane)`; a tree reconstructs by folding reverse deltas and extracting the
// commit's lane. This replaces the one-tree-per-commit model with the union of
// live lanes as the tree payload.

/// Summary of a union-store build.
pub struct UnionOutcome {
    pub n_rev: usize,
    pub n_lanes: usize,
    pub on_disk: u64,
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => total += dir_size(&e.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// Build the union-of-lanes tree store for `repo` at `store` (§1/§2/§7).
pub fn union_import(repo: &Path, store: &Path, level: i32) -> Result<UnionOutcome> {
    let s = crate::lanestore::LaneStore::encode_repo_union(repo, store, level)?;
    let n_rev = s.n_rev();
    let n_lanes = (0..n_rev).map(|r| s.lane_of(r)).max().map(|m| m as usize + 1).unwrap_or(0);
    Ok(UnionOutcome { n_rev, n_lanes, on_disk: dir_size(store) })
}

/// Incrementally update the union store at `store` from `repo` (§11) — folds
/// only the new commits' union deltas onto the stored boundary, O(new).
pub fn union_update(repo: &Path, store: &Path, level: i32) -> Result<UnionOutcome> {
    let s = crate::lanestore::LaneStore::update(repo, store, level)?;
    let n_rev = s.n_rev();
    let n_lanes = (0..n_rev).map(|r| s.lane_of(r)).max().map(|m| m as usize + 1).unwrap_or(0);
    Ok(UnionOutcome { n_rev, n_lanes, on_disk: dir_size(store) })
}

/// Reopen the union store at `store` and check that every `stride`-th commit's
/// tree reconstructs SHA-exact from the stored union bytes, against `repo` as
/// the oracle. Returns `(checked, mismatches)`.
pub fn union_verify(repo: &Path, store: &Path, stride: usize) -> Result<(usize, usize)> {
    let s = crate::lanestore::LaneStore::open(store)?;
    let mut real: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in git_str(repo, &["log", "--format=%H %T", "--branches", "--tags"])?.lines() {
        if let Some((h, t)) = line.split_once(' ') {
            real.insert(h.to_string(), t.to_string());
        }
    }
    let stride = stride.max(1);
    let (mut checked, mut bad) = (0usize, 0usize);
    for rev in (0..s.n_rev()).step_by(stride) {
        let sha = s.sha_at(rev).to_string();
        let got = s.tree_oid_at(rev)?;
        if real.get(&sha).map(|w| w.as_str()) != Some(got.as_str()) {
            bad += 1;
        }
        checked += 1;
    }
    Ok((checked, bad))
}

/// Write revision `rev`'s tree INTO `repo` as real git objects — blobs via
/// `hash-object -w`, directories bottom-up via `mktree` — returning the root
/// tree oid. Used for tag-at-tree export (rare, one tree), where fast-import
/// has no commit to carry the tree.
fn materialize_tree(repo: &Path, ls: &crate::lanestore::LaneStore, rev: usize) -> Result<String> {
    use std::collections::BTreeMap;
    // dir path -> entries (git-order sort key, mode, type, oid, name)
    let mut dirs: BTreeMap<Vec<u8>, Vec<(Vec<u8>, String, &'static str, String, Vec<u8>)>> =
        BTreeMap::new();
    dirs.insert(Vec::new(), Vec::new());
    let mut err: Option<Error> = None;
    ls.checkout_entries_at(rev, b"", &mut |path, mode, content| {
        let (dir, name) = match path.iter().rposition(|&b| b == b'/') {
            Some(i) => (path[..i].to_vec(), path[i + 1..].to_vec()),
            None => (Vec::new(), path.to_vec()),
        };
        // Ancestor dir rows so empty intermediate levels exist.
        let mut at = Vec::new();
        for seg in dir.split(|&b| b == b'/').filter(|s| !s.is_empty()) {
            if !at.is_empty() {
                at.push(b'/');
            }
            at.extend_from_slice(seg);
            dirs.entry(at.clone()).or_default();
        }
        let octal = String::from_utf8_lossy(&mode.octal()).into_owned();
        let (typ, oid) = if octal == "160000" {
            ("commit", String::from_utf8_lossy(content).into_owned())
        } else {
            let out = match run_stdin(
                repo,
                &["hash-object", "-w", "--stdin"],
                content,
            ) {
                Ok(o) => o,
                Err(e) => {
                    err = Some(e);
                    return Ok(());
                }
            };
            ("blob", out.trim().to_string())
        };
        let key = name.clone(); // files sort by bare name
        dirs.entry(dir).or_default().push((key, octal, typ, oid, name));
        Ok(())
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    // Bottom-up: deepest directories first (longer paths cannot be parents
    // of shorter ones; equal-length paths are independent).
    let mut order: Vec<Vec<u8>> = dirs.keys().cloned().collect();
    order.sort_by_key(|d| std::cmp::Reverse(d.len()));
    let mut oid_of_dir: BTreeMap<Vec<u8>, String> = BTreeMap::new();
    for d in order {
        let mut ents = dirs.remove(&d).unwrap_or_default();
        // This dir's SUBDIR entries (already resolved).
        let children: Vec<Vec<u8>> = oid_of_dir
            .keys()
            .filter(|c| {
                if d.is_empty() {
                    !c.contains(&b'/')
                } else {
                    c.len() > d.len()
                        && c.starts_with(&d)
                        && c[d.len()] == b'/'
                        && !c[d.len() + 1..].contains(&b'/')
                }
            })
            .cloned()
            .collect();
        for c in children {
            let name = if d.is_empty() { c.clone() } else { c[d.len() + 1..].to_vec() };
            let oid = oid_of_dir[&c].clone();
            // Directories sort with a trailing '/' in git tree order.
            let mut key = name.clone();
            key.push(b'/');
            ents.push((key, "040000".into(), "tree", oid, name));
        }
        ents.sort_by(|a, b| a.0.cmp(&b.0));
        let mut input = Vec::new();
        for (_k, mode, typ, oid, name) in &ents {
            input.extend_from_slice(format!("{mode} {typ} {oid}\t").as_bytes());
            input.extend_from_slice(name);
            input.push(b'\n');
        }
        let out = run_stdin(repo, &["mktree"], &input)?;
        oid_of_dir.insert(d, out.trim().to_string());
    }
    Ok(oid_of_dir.remove(&Vec::new()).unwrap_or_default())
}

/// Run a git command in `repo` feeding `input` on stdin; returns stdout.
fn run_stdin(repo: &Path, args: &[&str], input: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(input)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn export(store: &Path, repo: &Path) -> Result<Vec<RefMeta>> {
    // Everything walks: all commit records plus every tree view,
    // reconstructed newest→oldest (the stated O(history) export cost).
    let st = store::Store::open(store)?;
    let recs = st.commit_records()?; // oldest-first (position = index)
    // The tree for a commit is its lane of the union at that commit's
    // revision (sha-keyed — tag-tree revisions interleave with commits).
    let ls = st.union()?;
    let refs = st.refs_meta()?;
    std::fs::create_dir_all(repo)?;
    // Reinit is a no-op on an existing repo and preserves bareness —
    // mirror() pre-inits --bare to seed its fetch buffer through here.
    git(repo, &["init", "-q"])?;

    // Build the fast-import stream. Commits oldest-first; every commit is
    // a full manifest (deleteall + M for each file) from its resolved
    // view. Blobs are deduped through marks.
    let mut stream: Vec<u8> = Vec::new();
    let mut blob_marks: std::collections::HashMap<Vec<u8>, usize> = Default::default();
    let mut next_mark = 1usize;
    // Commit marks by STABLE INDEX (parents are indices in the data).
    let mut commit_marks: std::collections::HashMap<u64, usize> = Default::default();

    for cm in &recs {
        if !cm.extra_headers.is_empty() {
            return Err(Error::Unsupported(format!(
                "commit {} carries {:?} — SHA-exact export of signed/extended \
                 commits is not implemented (raw object is preserved in meta)",
                cm.sha, cm.extra_headers
            )));
        }
        let mut files = Vec::new();
        // Sha-keyed: revisions are ref-tree EVENTS (commits + tag-trees),
        // so a commit's COMMITS index no longer equals its revision.
        let view = ls.tree_view_of_commit(&cm.sha)?;
        walk_files(&view, &mut Vec::new(), &mut files)?;

        for (_, mode, content) in &files {
            if mode == "160000" {
                continue; // gitlink: no blob object
            }
            if !blob_marks.contains_key(*content) {
                blob_marks.insert(content.to_vec(), next_mark);
                stream.extend_from_slice(format!("blob\nmark :{next_mark}\ndata {}\n", content.len()).as_bytes());
                stream.extend_from_slice(content);
                stream.push(b'\n');
                next_mark += 1;
            }
        }

        let mark = next_mark;
        next_mark += 1;
        commit_marks.insert(cm.idx, mark);
        stream.extend_from_slice(format!("commit refs/gitdepot/import\nmark :{mark}\n").as_bytes());
        stream.extend_from_slice(b"author ");
        stream.extend_from_slice(&cm.author);
        stream.extend_from_slice(b"\ncommitter ");
        stream.extend_from_slice(&cm.committer);
        stream.extend_from_slice(format!("\ndata {}\n", cm.message.len()).as_bytes());
        stream.extend_from_slice(&cm.message);
        stream.push(b'\n');
        for (i, parent) in cm.parent_idxs.iter().enumerate() {
            let pmark = commit_marks
                .get(parent)
                .ok_or_else(|| Error::Meta(format!("parent {parent} not in store")))?;
            let verb = if i == 0 { "from" } else { "merge" };
            stream.extend_from_slice(format!("{verb} :{pmark}\n").as_bytes());
        }
        stream.extend_from_slice(b"deleteall\n");
        for (path, mode, content) in &files {
            if mode == "160000" {
                let sha = String::from_utf8_lossy(content);
                stream.extend_from_slice(
                    format!("M 160000 {sha} {}\n", quote_path(path)).as_bytes(),
                );
            } else {
                let m = blob_marks[*content];
                stream.extend_from_slice(format!("M {mode} :{m} {}\n", quote_path(path)).as_bytes());
            }
        }
        stream.push(b'\n');
    }

    let sha_to_idx: std::collections::HashMap<&str, u64> =
        recs.iter().map(|r| (r.sha.as_str(), r.idx)).collect();
    for r in &refs {
        if !r.tag_sha.is_empty() {
            continue; // annotated tag: written after fast-import, below
        }
        let mark = sha_to_idx
            .get(r.sha.as_str())
            .and_then(|i| commit_marks.get(i))
            .ok_or_else(|| Error::Meta(format!("ref {} target {} not in store", r.name, r.sha)))?;
        stream.extend_from_slice(format!("reset {}\nfrom :{mark}\n\n", r.name).as_bytes());
    }
    stream.extend_from_slice(b"done\n");

    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["fast-import", "--quiet", "--done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(&stream)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "fast-import: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    // Drop the scratch ref; then rebuild the tag objects.
    let _ = git(repo, &["update-ref", "-d", "refs/gitdepot/import"]);
    // Stored raw tag bytes are valid as-is: the `object <sha>` line
    // names a commit (or inner tag) fast-import regenerated SHA-exact.
    // Oldest-first = inner-before-outer for nested chains. --literally
    // skips git's format lint (historical tags predate it); fidelity is
    // OUR check — the produced id must equal the imported one.
    for t in &st.tag_records()? {
        if let store::TagTarget::Tree(rev) = t.target {
            // A tag at a TREE: materialize the tagged tree from its union
            // revision (blobs + mktree bottom-up) and check it regenerates
            // the tag's recorded target oid exactly. A tree that equals
            // some commit's tree is re-written idempotently.
            let (target_oid, typ) = parse_tag_target(&t.sha, &t.raw)?;
            if typ == "tree" {
                let got = materialize_tree(repo, &ls, rev as usize)?;
                if got != target_oid {
                    return Err(Error::Meta(format!(
                        "fidelity check failed: tagged tree regenerated as {got}, \
                         imported as {target_oid} (tag {})",
                        t.sha
                    )));
                }
            }
        }
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["hash-object", "-t", "tag", "-w", "--stdin", "--literally"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().expect("piped stdin").write_all(&t.raw)?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(Error::Git(format!(
                "hash-object tag {}: {}",
                t.sha,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if got != t.sha {
            return Err(Error::Meta(format!(
                "fidelity check failed: tag object regenerated as {got}, imported as {}",
                t.sha
            )));
        }
    }
    // Verify SHA fidelity per real ref (annotated tag refs point at
    // their tag object; the rest at their commit).
    let mut result = Vec::new();
    for r in &refs {
        if !r.tag_sha.is_empty() {
            git(repo, &["update-ref", &r.name, &r.tag_sha])?;
        }
        let expected = if r.tag_sha.is_empty() { &r.sha } else { &r.tag_sha };
        let got = git_str(repo, &["rev-parse", &r.name])?.trim().to_string();
        if got != *expected {
            return Err(Error::Meta(format!(
                "fidelity check failed: {} regenerated as {got}, imported as {expected}",
                r.name
            )));
        }
        result.push(r.clone());
    }
    Ok(result)
}
