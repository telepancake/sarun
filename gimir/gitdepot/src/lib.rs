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
//!   bounded prepend) holding three chains: TREES (reverse-delta tree
//!   layers, tip full at f0), COMMITS (one object per commit — sha,
//!   PARENT INDICES, tree index, author/committer/message — batched
//!   one chain record per ingest), REFLOG (every observed ref movement,
//!   deletions included, batched likewise).
//! * `<dir>/meta.sqlite` (WAL) — kv (schema=3, label/url, the
//!   authoritative per-chain record counts), refs (CURRENT refs only:
//!   name → commit_idx + tree_idx) — NOTHING else: sha → idx is an
//!   in-RAM map derived by one commits-chain walk per open handle
//!   (store.rs cost model), and tree dedup is parent-oid comparison
//!   plus an intra-ingest map, never persisted.
//!
//! Records carry STABLE indices counted from the oldest end (record k =
//! newest-first frame N-1-k; prepends only grow N), so lineage lives in
//! the data and an upstream rewrite is just new records + repointed
//! refs — no non-fast-forward path, no re-import, no store retirement.
//! git itself is driven by shelling out — sarun custom — so this tool
//! needs a `git` binary and runs host-side.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use depot::codec;
use depot::{Attrs, BlobOp, Layer, Node};

pub mod readout;
pub mod store;

pub use store::{commit_at, commit_count, label, resolve_ref};

// ------------------------------------------------------------------ meta

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RefMeta {
    pub name: String,
    /// Commit id the ref points at (hex).
    pub sha: String,
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

// ---------------------------------------------------------------- import

struct TreeEntry {
    mode: String,
    oid: String,
    path: Vec<u8>,
}

fn ls_tree(repo: &Path, commit: &str) -> Result<Vec<TreeEntry>> {
    let out = git(repo, &["ls-tree", "-r", "-z", "--full-tree", commit])?;
    let mut entries = Vec::new();
    for rec in out.split(|&b| b == 0) {
        if rec.is_empty() {
            continue;
        }
        // "<mode> <type> <oid>\t<path>"
        let tab = rec
            .iter()
            .position(|&b| b == b'\t')
            .ok_or_else(|| Error::Git("ls-tree: no tab".into()))?;
        let head = std::str::from_utf8(&rec[..tab])
            .map_err(|_| Error::Git("ls-tree: non-utf8 header".into()))?;
        let mut it = head.split(' ');
        let (mode, _typ, oid) = (
            it.next().unwrap_or_default().to_string(),
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default().to_string(),
        );
        entries.push(TreeEntry { mode, oid, path: rec[tab + 1..].to_vec() });
    }
    Ok(entries)
}

/// Fetch every oid's content through ONE `git cat-file --batch` process.
/// The oid→bytes map is the import's internal dedup (equal blobs read
/// once) — the oids never reach a layer.
fn fetch_blobs(
    repo: &Path,
    oids: impl IntoIterator<Item = String>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let uniq: std::collections::BTreeSet<String> = oids.into_iter().collect();
    let mut map = BTreeMap::new();
    if uniq.is_empty() {
        return Ok(map);
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    // Write requests from a thread: the request stream can exceed the
    // pipe buffer (large trees) while git's replies fill the other
    // pipe — writing and reading from one thread deadlocks both sides
    // (observed: import wedged in anon_pipe_write on a real history).
    let mut stdin = child.stdin.take().expect("piped stdin");
    let reqs: Vec<String> = uniq.iter().cloned().collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for oid in &reqs {
            writeln!(stdin, "{oid}")?;
        }
        Ok(()) // drop closes stdin
    });
    let out = child.wait_with_output()?;
    writer.join().map_err(|_| Error::Git("cat-file writer panicked".into()))?
        .map_err(Error::Io)?;
    if !out.status.success() {
        return Err(Error::Git("cat-file --batch failed".into()));
    }
    let mut buf = &out.stdout[..];
    for oid in &uniq {
        let nl = buf
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| Error::Git("cat-file: truncated header".into()))?;
        let header = std::str::from_utf8(&buf[..nl])
            .map_err(|_| Error::Git("cat-file: bad header".into()))?;
        let mut it = header.split(' ');
        let (h_oid, _typ, size) = (
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
        );
        if h_oid != oid {
            return Err(Error::Git(format!("cat-file: expected {oid}, got {h_oid}")));
        }
        let size: usize = size
            .parse()
            .map_err(|_| Error::Git("cat-file: bad size".into()))?;
        let start = nl + 1;
        if buf.len() < start + size + 1 {
            return Err(Error::Git("cat-file: truncated body".into()));
        }
        map.insert(oid.clone(), buf[start..start + size].to_vec());
        buf = &buf[start + size + 1..]; // skip trailing \n
    }
    Ok(map)
}

/// Build the full-content layer for one commit's tree.
fn tree_layer(entries: &[TreeEntry], blobs: &BTreeMap<String, Vec<u8>>) -> Result<Layer> {
    let mut root = Node::keep();
    for e in entries {
        let content: Vec<u8> = if e.mode == "160000" {
            // gitlink: the pinned commit id IS the source data.
            e.oid.clone().into_bytes()
        } else {
            blobs
                .get(&e.oid)
                .ok_or_else(|| Error::Git(format!("missing blob {}", e.oid)))?
                .clone()
        };
        let mut node = &mut root;
        let mut segs = e.path.split(|&b| b == b'/').peekable();
        while let Some(seg) = segs.next() {
            if seg.is_empty() {
                return Err(Error::Unsupported("empty path segment".into()));
            }
            node = node.children.entry(seg.to_vec()).or_insert_with(Node::keep);
            if segs.peek().is_none() {
                node.blob = BlobOp::Set(content.clone());
                node.attrs = Some(Attrs::from([(b"mode".to_vec(), e.mode.clone().into_bytes())]));
            }
        }
    }
    Ok(Layer { root })
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
    pub meta: Meta,
    /// Present only when the encoding comparison was requested
    /// (import_opts(report=true) / CLI `import --report`) — computing
    /// it recompresses the whole history five extra ways, so the
    /// mirror loop must never pay for it.
    pub report: Option<SizeReport>,
}

/// Refs (bookkeeping): branches + tags only. `refs/pull/*` and friends
/// are excluded — on public forges that forest is unbounded and
/// adversarial (spam PRs merging foreign megahistories).
fn collect_refs(repo: &Path) -> Result<Vec<RefMeta>> {
    let mut refs = Vec::new();
    for line in git_str(
        repo,
        &["for-each-ref", "--format=%(objectname) %(objecttype) %(refname)",
          "refs/heads", "refs/tags"],
    )?
    .lines()
    {
        let mut it = line.splitn(3, ' ');
        let (sha, typ, name) = (
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
            it.next().unwrap_or_default(),
        );
        if typ != "commit" {
            return Err(Error::Unsupported(format!(
                "ref {name} points at a {typ} (annotated tags are out of scope)"
            )));
        }
        refs.push(RefMeta { name: name.to_string(), sha: sha.to_string() });
    }
    if refs.is_empty() {
        return Err(Error::Git("repository has no refs".into()));
    }
    Ok(refs)
}

/// All commits reachable from branches + tags, OLDEST-first (reversed
/// topo order: parents always precede children — the stable-index
/// assignment order). Same scope rule as `collect_refs`.
fn rev_list_oldest_first(repo: &Path) -> Result<Vec<String>> {
    let mut shas: Vec<String> =
        git_str(repo, &["rev-list", "--topo-order", "--branches", "--tags"])?
            .lines()
            .map(str::to_string)
            .collect();
    shas.reverse();
    Ok(shas)
}

/// Memoized commit-sha → root-tree-oid over the fetch buffer, for the
/// parent-tree dedup check (store::Ingest — no persistent oid index).
/// Parents outside the walked set (update path) cost one rev-parse;
/// the buffer always has them (ancestors of a fetched tip).
#[derive(Default)]
struct TreeOidMemo(BTreeMap<String, String>);

impl TreeOidMemo {
    fn insert(&mut self, sha: &str, tree_oid: &str) {
        self.0.insert(sha.to_string(), tree_oid.to_string());
    }

    /// A parent of `cm` whose tree oid equals `tree_oid`, if any.
    fn same_tree_parent(
        &mut self,
        repo: &Path,
        cm: &CommitMeta,
        tree_oid: &str,
    ) -> Result<Option<String>> {
        for p in &cm.parents {
            if !self.0.contains_key(p) {
                let oid = git_str(repo, &["rev-parse", &format!("{p}^{{tree}}")])?;
                self.0.insert(p.clone(), oid.trim().to_string());
            }
            if self.0[p] == tree_oid {
                return Ok(Some(p.clone()));
            }
        }
        Ok(None)
    }
}

/// One commit's meta + resolved view.
fn commit_view(repo: &Path, sha: &str) -> Result<(CommitMeta, String, depot::View)> {
    let raw = git(repo, &["cat-file", "commit", sha])?;
    let (mut cm, tree_oid) = parse_commit(&raw)?;
    cm.sha = sha.to_string();
    let entries = ls_tree(repo, sha)?;
    let blobs = fetch_blobs(
        repo,
        entries
            .iter()
            .filter(|e| e.mode != "160000")
            .map(|e| e.oid.clone()),
    )?;
    let full = tree_layer(&entries, &blobs)?;
    let view = depot::apply(None, &full)
        .ok_or_else(|| Error::Unsupported(format!("commit {sha} has an empty tree")))?;
    Ok((cm, tree_oid, view))
}

/// Import a git repo into `store` (created; must not exist).
pub fn import(repo: &Path, store: &Path, level: i32) -> Result<ImportOutcome> {
    import_opts(repo, store, level, false)
}

/// `report`: also measure the alternative encodings (full/delta ×
/// standalone/refPrefix + solid bound) — the straightedge's comparison
/// harness, NOT part of storing.
pub fn import_opts(repo: &Path, store: &Path, level: i32, report: bool)
    -> Result<ImportOutcome>
{
    let refs = collect_refs(repo)?;
    let shas = rev_list_oldest_first(repo)?;

    let mut st = store::Store::create(store)?;
    let mut ingest = store::Ingest::new(&mut st, level)?;
    // Walk oldest → newest, one resolved view at a time. The full
    // record is derived from the VIEW via diff(None, view) — the same
    // function the decoder uses to recompute refPrefix anchors from
    // reconstructed views; bit-exactness of that anchor is load-bearing.
    let mut commits = Vec::with_capacity(shas.len());
    let mut rep = report.then(ReportAccum::default);
    let mut tree_oids = TreeOidMemo::default();
    for sha in &shas {
        let (cm, tree_oid, view) = commit_view(repo, sha)?;
        let same = tree_oids.same_tree_parent(repo, &cm, &tree_oid)?;
        tree_oids.insert(sha, &tree_oid);
        let full = codec::encode(&depot::diff(None, Some(&view)));
        if let Some(r) = rep.as_mut() {
            r.push(&view, full.clone());
        }
        ingest.add_commit(&cm, &tree_oid, same.as_deref(), &view, &full)?;
        commits.push(cm);
    }
    ingest.finish(&refs)?;
    commits.reverse(); // Meta stays newest-first.

    let report = match rep {
        Some(r) => Some(r.finish(level)?),
        None => None,
    };
    let meta = Meta { label: String::new(), url: String::new(), refs, commits };
    Ok(ImportOutcome { meta, report })
}

/// Report-only accumulator: rebuilds the v1 comparison record families
/// (newest-first full + delta records) from an oldest-first walk.
#[derive(Default)]
struct ReportAccum {
    fulls: Vec<Vec<u8>>,
    deltas: Vec<Vec<u8>>, // record for view i, pushed when view i+1 arrives
    prev: Option<depot::View>,
}

impl ReportAccum {
    fn push(&mut self, view: &depot::View, full: Vec<u8>) {
        if let Some(prev) = &self.prev {
            self.deltas
                .push(codec::encode(&depot::diff(Some(view), Some(prev))));
        }
        self.fulls.push(full);
        self.prev = Some(view.clone());
    }

    fn finish(mut self, level: i32) -> Result<SizeReport> {
        self.fulls.reverse();
        let full_records = self.fulls;
        let mut delta_records = Vec::with_capacity(full_records.len());
        delta_records.push(full_records[0].clone());
        delta_records.extend(self.deltas.into_iter().rev());
        Ok(SizeReport {
            commits: delta_records.len(),
            zstd_level: level,
            full_raw: full_records.iter().map(|r| r.len() as u64).sum(),
            full_standalone: standalone_total(&full_records, level)?,
            full_ref_chain: chain_bytes(&full_records, level)?,
            delta_raw: delta_records.iter().map(|r| r.len() as u64).sum(),
            delta_standalone: standalone_total(&delta_records, level)?,
            delta_ref_chain: chain_bytes(&delta_records, level)?,
            view_ref_chain: view_chain_total(&delta_records, &full_records, level)?,
            solid_full: solid_total(&full_records, level)?,
        })
    }
}

fn standalone_total(records: &[Vec<u8>], level: i32) -> Result<u64> {
    let mut total = 0u64;
    for rec in records {
        total += store::compress(rec, None, level)?.len() as u64;
    }
    Ok(total)
}

fn chain_bytes(records: &[Vec<u8>], level: i32) -> Result<u64> {
    let mut total = 0u64;
    for (i, rec) in records.iter().enumerate() {
        let prefix = if i == 0 { None } else { Some(records[i - 1].as_slice()) };
        total += 8 + store::compress(rec, prefix, level)?.len() as u64;
    }
    Ok(total)
}

/// The v1 stored form (delta records anchored on the previous commit's
/// full VIEW bytes) — kept as a comparison line.
fn view_chain_total(
    delta_records: &[Vec<u8>],
    full_records: &[Vec<u8>],
    level: i32,
) -> Result<u64> {
    let mut total = 0u64;
    for (i, rec) in delta_records.iter().enumerate() {
        let prefix = if i == 0 { None } else { Some(full_records[i - 1].as_slice()) };
        total += 8 + store::compress(rec, prefix, level)?.len() as u64;
    }
    Ok(total)
}

fn solid_total(records: &[Vec<u8>], level: i32) -> Result<u64> {
    let mut concat = Vec::new();
    for rec in records {
        concat.extend_from_slice(rec);
    }
    Ok(store::compress(&concat, None, level)?.len() as u64)
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
    let shas = rev_list_oldest_first(repo)?;

    let mut st = store::Store::open(store)?;
    let base_prepends = st.depot_prepends();
    let mut ingest = store::Ingest::new(&mut st, level)?;
    let mut k = 0usize;
    let mut tree_oids = TreeOidMemo::default();
    for sha in &shas {
        if ingest.knows_sha(sha)? {
            continue;
        }
        let (cm, tree_oid, view) = commit_view(repo, sha)?;
        let same = tree_oids.same_tree_parent(repo, &cm, &tree_oid)?;
        tree_oids.insert(sha, &tree_oid);
        let full = codec::encode(&depot::diff(None, Some(&view)));
        ingest.add_commit(&cm, &tree_oid, same.as_deref(), &view, &full)?;
        k += 1;
    }
    ingest.finish(&refs)?;
    Ok(UpdateOutcome {
        new_commits: k,
        total_commits: st.count(store::COMMITS)? as usize,
        refs,
        depot_prepends: st.depot_prepends() - base_prepends,
    })
}

// ---------------------------------------------------------------- mirror

#[derive(Debug, Clone)]
pub struct MirrorOutcome {
    pub update: UpdateOutcome,
}

/// The fetch-and-update loop for one remote: keep `<root>/repo.git` (a
/// bare mirror clone) in sync with `url`, and `<root>/store` in sync
/// with the clone. First call clones + imports; later calls fetch +
/// incrementally `update`. A rewritten remote is just an update: new
/// records + repointed refs — the mirror follows the remote AND keeps
/// every commit it ever held resolvable.
///
/// Fetching is host-side for now (MIRRORS.md: the move into a tap box
/// is mechanical later).
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
    mirror_opts(url, root, false)
}

/// `frugal`: drop the fetch buffer after a successful update, leaving
/// the store as the single on-disk copy. The next run re-seeds the
/// buffer from the store (one export) before fetching.
pub fn mirror_opts(url: &str, root: &Path, frugal: bool) -> Result<MirrorOutcome> {
    std::fs::create_dir_all(root)?;
    let repo = root.join("repo.git");
    let store = root.join("store");
    if repo.join("HEAD").exists() {
        git(&repo, &["remote", "update", "--prune"])?;
    } else if store::store_exists(&store) {
        // The store is the ONLY authoritative copy; repo.git is a
        // transient fetch buffer, reconstructible because export is
        // SHA-exact. Re-seed it from the store (bare, wired like
        // clone --mirror) and fetch just the delta — a deleted (or
        // frugally dropped) buffer costs one export, never a re-clone.
        std::fs::create_dir_all(&repo)?;
        git(&repo, &["init", "-q", "--bare"])?;
        export(&store, &repo)?;
        git(&repo, &["config", "remote.origin.url", url])?;
        git(&repo, &["config", "remote.origin.fetch", "+refs/*:refs/*"])?;
        git(&repo, &["config", "remote.origin.mirror", "true"])?;
        git(&repo, &["remote", "update", "--prune"])?;
    } else {
        let out = Command::new("git")
            .args(["clone", "--quiet", "--mirror", url])
            .arg(&repo)
            .output()?;
        if !out.status.success() {
            return Err(Error::Git(format!(
                "clone --mirror {url}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }
    let out = if !store::store_exists(&store) {
        let o = import(&repo, &store, 3)?;
        let n = o.meta.commits.len();
        MirrorOutcome {
            update: UpdateOutcome {
                new_commits: n,
                total_commits: n,
                refs: o.meta.refs,
                depot_prepends: 0,
            },
        }
    } else {
        MirrorOutcome { update: update(&repo, &store, 3)? }
    };
    // Stamp identity ("WHICH git?") — listings and attachment names key
    // off it.
    let (label, old_url) = store::identity(&store)?;
    if label.is_empty() || old_url != url {
        store::set_identity(&store, &label_from_url(url), url)?;
    }
    if frugal {
        std::fs::remove_dir_all(&repo)?;
    }
    Ok(out)
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

/// Export a store into a fresh git repository at `repo` (must not be an
/// existing repo; `git init` is run there). Returns the regenerated tip
/// shas by ref name; fails if any regenerated commit id differs from the
/// one recorded at import (the fidelity check).
pub fn export(store: &Path, repo: &Path) -> Result<Vec<RefMeta>> {
    // Everything walks: all commit records plus every tree view,
    // reconstructed newest→oldest (the stated O(history) export cost).
    let st = store::Store::open(store)?;
    let recs = st.commit_records()?; // oldest-first (position = index)
    let views_nf = st.tree_views(None)?; // newest-first
    let n_trees = st.count(store::TREES)? as usize;
    if views_nf.len() != n_trees {
        return Err(Error::Chain(format!(
            "{} tree frames but n_trees = {n_trees}",
            views_nf.len()
        )));
    }
    let view_of = |tree_idx: u64| -> Result<&depot::View> {
        views_nf
            .get(n_trees - 1 - tree_idx as usize)
            .ok_or_else(|| Error::Chain(format!("no tree at index {tree_idx}")))
    };
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
        walk_files(view_of(cm.tree_idx)?, &mut Vec::new(), &mut files)?;

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
    // Drop the scratch ref; verify SHA fidelity per real ref.
    let _ = git(repo, &["update-ref", "-d", "refs/gitdepot/import"]);
    let mut result = Vec::new();
    for r in &refs {
        let got = git_str(repo, &["rev-parse", &r.name])?.trim().to_string();
        if got != r.sha {
            return Err(Error::Meta(format!(
                "fidelity check failed: {} regenerated as {got}, imported as {}",
                r.name, r.sha
            )));
        }
        result.push(RefMeta { name: r.name.clone(), sha: got });
    }
    Ok(result)
}
