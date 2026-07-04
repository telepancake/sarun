//! gitdepot — a git repository to/from depot straightedge.
//!
//! The second workload for the depot model (DEPOT-DESIGN.md §7 "git"):
//! a repo's history becomes a chain of **full-content canonical layers,
//! newest first** — each commit's tree is one layer (never a delta layer:
//! cross-version redundancy is the *compression's* job, per the tiered-VBF
//! design), refPrefix-chained in zstd exactly the way a VBF chain anchors
//! each frame on the next-newer record. Refs and commit metadata (author,
//! committer, message, parent edges) are **meta** — bookkeeping outside
//! the layers, per the round-trip fence: they round-trip, but they are
//! not tree data.
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
//! On-disk store: `<dir>/meta.json` + `<dir>/chain` (frames newest-first,
//! each `[u32 raw_len | u32 zstd_len | zstd bytes]`, frame 0 standalone,
//! frame i compressed with record i-1 as zstd refPrefix). git itself is
//! driven by shelling out — sarun custom — so this tool needs a `git`
//! binary and runs host-side.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use depot::codec;
use depot::{Attrs, BlobOp, Layer, Node, Presence};

pub mod chain;

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
    /// Raw `author`/`committer` header values and the message, hex-coded
    /// so arbitrary bytes survive JSON.
    pub author_hex: String,
    pub committer_hex: String,
    pub message_hex: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub refs: Vec<RefMeta>,
    /// Newest-first; index i corresponds to chain frame i.
    pub commits: Vec<CommitMeta>,
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
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        for oid in &uniq {
            writeln!(stdin, "{oid}")?;
        }
    } // drop closes stdin
    let out = child.wait_with_output()?;
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

fn parse_commit(raw: &[u8]) -> Result<CommitMeta> {
    let body_at = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .ok_or_else(|| Error::Git("commit: no header/body split".into()))?;
    let (headers, message) = (&raw[..body_at], &raw[body_at + 2..]);
    let mut parents = Vec::new();
    let (mut author, mut committer) = (None, None);
    for line in headers.split(|&b| b == b'\n') {
        let sp = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
        let (key, val) = (&line[..sp], &line[sp.min(line.len() - 1) + 1..]);
        match key {
            b"tree" => {}
            b"parent" => parents.push(String::from_utf8_lossy(val).into_owned()),
            b"author" => author = Some(val.to_vec()),
            b"committer" => committer = Some(val.to_vec()),
            other => {
                // gpgsig etc. would break SHA-exact re-import; refuse
                // loudly rather than round-trip approximately.
                return Err(Error::Unsupported(format!(
                    "commit header '{}' (signed commits are out of straightedge scope)",
                    String::from_utf8_lossy(other)
                )));
            }
        }
    }
    Ok(CommitMeta {
        sha: String::new(), // filled by caller
        parents,
        author_hex: hex::encode(author.ok_or_else(|| Error::Git("commit: no author".into()))?),
        committer_hex: hex::encode(
            committer.ok_or_else(|| Error::Git("commit: no committer".into()))?,
        ),
        message_hex: hex::encode(message),
    })
}

/// Size comparison across encodings of the same records (bytes).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SizeReport {
    /// Sum of raw canonical records.
    pub raw: u64,
    /// Each record zstd'd standalone.
    pub standalone: u64,
    /// The stored VBF-style refPrefix chain.
    pub ref_prefix_chain: u64,
    /// All records concatenated into one zstd stream (a solid bound —
    /// not seekable, listed for comparison only).
    pub solid: u64,
    pub commits: usize,
    pub zstd_level: i32,
}

pub struct ImportOutcome {
    pub meta: Meta,
    pub report: SizeReport,
}

/// Import a git repo into `store` (created; must not exist).
pub fn import(repo: &Path, store: &Path, level: i32) -> Result<ImportOutcome> {
    // Refs (bookkeeping).
    let mut refs = Vec::new();
    for line in git_str(
        repo,
        &["for-each-ref", "--format=%(objectname) %(objecttype) %(refname)"],
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

    // Commits, newest-first.
    let shas: Vec<String> = git_str(repo, &["rev-list", "--topo-order", "--all"])?
        .lines()
        .map(str::to_string)
        .collect();

    let mut commits = Vec::with_capacity(shas.len());
    let mut records: Vec<Vec<u8>> = Vec::with_capacity(shas.len());
    for sha in &shas {
        let raw = git(repo, &["cat-file", "commit", sha])?;
        let mut cm = parse_commit(&raw)?;
        cm.sha = sha.clone();
        commits.push(cm);

        let entries = ls_tree(repo, sha)?;
        let blobs = fetch_blobs(
            repo,
            entries
                .iter()
                .filter(|e| e.mode != "160000")
                .map(|e| e.oid.clone()),
        )?;
        records.push(codec::encode(&tree_layer(&entries, &blobs)?));
    }

    let meta = Meta { refs, commits };
    let report = chain::write_store(store, &meta, &records, level)?;
    Ok(ImportOutcome { meta, report })
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
    node: &'a Node,
    prefix: &mut Vec<u8>,
    out: &mut Vec<(Vec<u8>, String, &'a [u8])>,
) -> Result<()> {
    if let BlobOp::Set(content) = &node.blob {
        let mode = node
            .attrs
            .as_ref()
            .and_then(|a| a.get(&b"mode"[..]))
            .ok_or_else(|| Error::Meta("file node without mode attr".into()))?;
        out.push((
            prefix.clone(),
            String::from_utf8_lossy(mode).into_owned(),
            content,
        ));
    }
    for (name, child) in &node.children {
        if child.presence == Presence::Tombstone {
            return Err(Error::Meta("tombstone in a full-content layer".into()));
        }
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
    let (meta, records) = chain::read_store(store)?;
    std::fs::create_dir_all(repo)?;
    git(repo, &["init", "-q"])?;

    // Build the fast-import stream. Commits oldest-first; every commit is
    // a full manifest (deleteall + M for each file) — matching the
    // full-content layer model. Blobs are deduped through marks.
    let mut stream: Vec<u8> = Vec::new();
    let mut blob_marks: std::collections::HashMap<&[u8], usize> = Default::default();
    let mut next_mark = 1usize;
    let mut commit_marks: std::collections::HashMap<&str, usize> = Default::default();

    let layers: Vec<Layer> = records
        .iter()
        .map(|r| codec::decode(r).map_err(Error::from))
        .collect::<Result<_>>()?;

    for idx in (0..meta.commits.len()).rev() {
        let cm = &meta.commits[idx];
        let mut files = Vec::new();
        walk_files(&layers[idx].root, &mut Vec::new(), &mut files)?;

        for (_, mode, content) in &files {
            if mode == "160000" {
                continue; // gitlink: no blob object
            }
            if !blob_marks.contains_key(*content) {
                blob_marks.insert(content, next_mark);
                stream.extend_from_slice(format!("blob\nmark :{next_mark}\ndata {}\n", content.len()).as_bytes());
                stream.extend_from_slice(content);
                stream.push(b'\n');
                next_mark += 1;
            }
        }

        let mark = next_mark;
        next_mark += 1;
        commit_marks.insert(&cm.sha, mark);
        stream.extend_from_slice(format!("commit refs/gitdepot/import\nmark :{mark}\n").as_bytes());
        stream.extend_from_slice(b"author ");
        stream.extend_from_slice(&hex::decode(&cm.author_hex).map_err(|e| Error::Meta(e.to_string()))?);
        stream.extend_from_slice(b"\ncommitter ");
        stream.extend_from_slice(&hex::decode(&cm.committer_hex).map_err(|e| Error::Meta(e.to_string()))?);
        let msg = hex::decode(&cm.message_hex).map_err(|e| Error::Meta(e.to_string()))?;
        stream.extend_from_slice(format!("\ndata {}\n", msg.len()).as_bytes());
        stream.extend_from_slice(&msg);
        stream.push(b'\n');
        for (i, parent) in cm.parents.iter().enumerate() {
            let pmark = commit_marks
                .get(parent.as_str())
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

    for r in &meta.refs {
        let mark = commit_marks
            .get(r.sha.as_str())
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
    for r in &meta.refs {
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
