//! The trait halves (DEPOT-DESIGN.md §5) and generic transfer.
//!
//! Deliberately minimal and PROVISIONAL: method signatures are pinned
//! only after the fs-layer workload's operation frequencies are measured
//! against the live overlay (§5). What is already fixed by the design:
//! the split itself — a stream variant cannot random-read or accept
//! out-of-order writes, so ingest and readout are separate capabilities —
//! and transfer as the composition `walk source → feed sink`, which is
//! what makes import/export fall out of the abstraction instead of being
//! per-variant code.

use crate::{Attrs, Layer, Name, View};

/// Accepts layers in canonical order. The stream variant is sink-only on
/// write; random-access variants implement both halves.
pub trait LayerSink {
    type Err: std::error::Error;
    fn put_layer(&mut self, layer: &Layer) -> Result<(), Self::Err>;
}

/// Yields layers in stored order.
pub trait LayerSource {
    type Err: std::error::Error;
    fn next_layer(&mut self) -> Result<Option<Layer>, Self::Err>;
}

/// Transfer failure: either side's error, kept distinguishable.
#[derive(Debug)]
pub enum TransferError<S, K> {
    Source(S),
    Sink(K),
}

impl<S: std::fmt::Display, K: std::fmt::Display> std::fmt::Display for TransferError<S, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::Source(e) => write!(f, "source: {e}"),
            TransferError::Sink(e) => write!(f, "sink: {e}"),
        }
    }
}

impl<S, K> std::error::Error for TransferError<S, K>
where
    S: std::error::Error,
    K: std::error::Error,
{
}

/// Depot-to-depot transfer: walk the source in order, feed the sink.
/// Returns the number of layers moved. Sharing is depot-internal (§1),
/// so nothing of the source's internal representation crosses; the sink
/// re-establishes its own.
pub fn transfer<Src: LayerSource, Snk: LayerSink>(
    src: &mut Src,
    dst: &mut Snk,
) -> Result<u64, TransferError<Src::Err, Snk::Err>> {
    let mut moved = 0u64;
    while let Some(layer) = src.next_layer().map_err(TransferError::Source)? {
        dst.put_layer(&layer).map_err(TransferError::Sink)?;
        moved += 1;
    }
    Ok(moved)
}

// ───────────────────────── readout (DEPOT-DESIGN.md §8) ─────────────────────

/// Shape of a node as seen by readout. `Branch` means "has children";
/// a branch may still carry a blob (the model's interior-blob superset
/// of git's tree/blob split), so blob presence is reported separately
/// on [`ReadoutEntry`], never inferred from the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadoutKind {
    Branch,
    Leaf,
}

/// One node's readout metadata. `blob_len` is the blob's byte length
/// when the node carries one (leaf or interior); attrs are the node's
/// source-provided attributes verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadoutEntry {
    pub kind: ReadoutKind,
    pub blob_len: Option<u64>,
    pub attrs: Attrs,
}

/// Blob content, bytes-or-backing-file: a store that already holds the
/// blob as a loose host file hands back its path so consumers stay
/// zero-copy (mmap/sendfile); everything else decodes to bytes. A
/// returned `File` must be immutable for the life of the readout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blob {
    Bytes(Vec<u8>),
    File(std::path::PathBuf),
}

/// The readout half of the depot trait (§8): everything an RO
/// attachment needs — entry, children, blob — over ONE resolved tree.
/// A location is a slice of name components from the root (`&[]` is
/// the root itself); names are opaque bytes, never a joined path (§1).
///
/// Misses are `None`/empty — readout serves a pinned, already-consistent
/// snapshot, so absence is data, not an error. Implementations may
/// decode lazily on first access but must be internally synchronized
/// (`Send + Sync`: overlay serving is concurrent).
pub trait Readout: Send + Sync {
    fn entry(&self, at: &[&[u8]]) -> Option<ReadoutEntry>;
    /// Direct children of the node at `at`, in the variant's canonical
    /// order (bytewise here). Empty for leaves and misses.
    fn children(&self, at: &[&[u8]]) -> Vec<Name>;
    fn blob(&self, at: &[&[u8]]) -> Option<Blob>;
}

/// Descend a resolved [`View`] by name components.
pub fn view_at<'a>(root: &'a View, at: &[&[u8]]) -> Option<&'a View> {
    let mut v = root;
    for name in at {
        v = v.children.get(*name)?;
    }
    Some(v)
}

/// [`ReadoutEntry`] for one view node. Canonical form guarantees no
/// empty nodes exist, so kind is decidable locally: children ⇒ branch.
pub fn view_entry(v: &View) -> ReadoutEntry {
    ReadoutEntry {
        kind: if v.children.is_empty() { ReadoutKind::Leaf } else { ReadoutKind::Branch },
        blob_len: v.blob.as_ref().map(|b| b.len() as u64),
        attrs: v.attrs.clone(),
    }
}

/// Wrap `view` under `prefix` components (outermost first), so a tree
/// serves nested — the attach verbs' `prefix` argument. The wrapper
/// branches carry no attrs; an empty prefix is the identity.
pub fn nest_view(view: View, prefix: &[&[u8]]) -> View {
    let mut v = view;
    for name in prefix.iter().rev() {
        let mut children = std::collections::BTreeMap::new();
        children.insert(name.to_vec(), v);
        v = View { blob: None, attrs: Attrs::new(), children };
    }
    v
}

/// Readout over a resolved snapshot: any store that can produce a
/// [`View`] gets the trait for free. Blobs are in-memory in a `View`,
/// so `blob` always answers `Blob::Bytes`.
pub struct ViewReadout {
    view: View,
}

impl ViewReadout {
    pub fn new(view: View) -> Self {
        ViewReadout { view }
    }

    /// The view served under `prefix` (see [`nest_view`]).
    pub fn nested(view: View, prefix: &[&[u8]]) -> Self {
        ViewReadout { view: nest_view(view, prefix) }
    }

    pub fn view(&self) -> &View {
        &self.view
    }
}

impl Readout for ViewReadout {
    fn entry(&self, at: &[&[u8]]) -> Option<ReadoutEntry> {
        view_at(&self.view, at).map(view_entry)
    }

    fn children(&self, at: &[&[u8]]) -> Vec<Name> {
        match view_at(&self.view, at) {
            Some(v) => v.children.keys().cloned().collect(),
            None => Vec::new(),
        }
    }

    fn blob(&self, at: &[&[u8]]) -> Option<Blob> {
        view_at(&self.view, at)?.blob.clone().map(Blob::Bytes)
    }
}

#[cfg(test)]
mod readout_tests {
    use super::*;

    fn leaf(bytes: &[u8]) -> View {
        View { blob: Some(bytes.to_vec()), attrs: Attrs::new(), children: Default::default() }
    }

    /// root ─ src ─ {a.txt, b.txt}   (src carries an interior blob + attr)
    ///      └ zzz (leaf)
    fn sample() -> View {
        let mut src_attrs = Attrs::new();
        src_attrs.insert(b"mode".to_vec(), b"0755".to_vec());
        let mut src_children = std::collections::BTreeMap::new();
        src_children.insert(b"b.txt".to_vec(), leaf(b"bee"));
        src_children.insert(b"a.txt".to_vec(), leaf(b"aye"));
        let src = View {
            blob: Some(b"interior".to_vec()),
            attrs: src_attrs,
            children: src_children,
        };
        let mut root_children = std::collections::BTreeMap::new();
        root_children.insert(b"src".to_vec(), src);
        root_children.insert(b"zzz".to_vec(), leaf(b"z"));
        View { blob: None, attrs: Attrs::new(), children: root_children }
    }

    #[test]
    fn entry_children_blob() {
        let r = ViewReadout::new(sample());
        // Root: branch, no blob.
        let root = r.entry(&[]).unwrap();
        assert_eq!(root.kind, ReadoutKind::Branch);
        assert_eq!(root.blob_len, None);
        assert_eq!(r.children(&[]), vec![b"src".to_vec(), b"zzz".to_vec()]);
        // Interior node with a blob stays a Branch and reports the blob.
        let src = r.entry(&[b"src"]).unwrap();
        assert_eq!(src.kind, ReadoutKind::Branch);
        assert_eq!(src.blob_len, Some(8));
        assert_eq!(src.attrs.get(b"mode".as_slice()), Some(&b"0755".to_vec()));
        assert_eq!(r.blob(&[b"src"]), Some(Blob::Bytes(b"interior".to_vec())));
        // Children come back in canonical (bytewise) order.
        assert_eq!(r.children(&[b"src"]), vec![b"a.txt".to_vec(), b"b.txt".to_vec()]);
        // Leaf.
        let a = r.entry(&[b"src", b"a.txt"]).unwrap();
        assert_eq!(a.kind, ReadoutKind::Leaf);
        assert_eq!(a.blob_len, Some(3));
        assert_eq!(r.blob(&[b"src", b"a.txt"]), Some(Blob::Bytes(b"aye".to_vec())));
        assert!(r.children(&[b"src", b"a.txt"]).is_empty());
    }

    #[test]
    fn misses() {
        let r = ViewReadout::new(sample());
        assert_eq!(r.entry(&[b"nope"]), None);
        assert_eq!(r.entry(&[b"src", b"a.txt", b"deeper"]), None);
        assert!(r.children(&[b"nope"]).is_empty());
        assert_eq!(r.blob(&[b"nope"]), None);
        // Present but blob-less: entry hits, blob misses.
        assert!(r.entry(&[]).is_some());
        assert_eq!(r.blob(&[]), None);
    }

    #[test]
    fn nested_prefix() {
        let r = ViewReadout::nested(sample(), &[b"deps", b"lib"]);
        assert_eq!(r.children(&[]), vec![b"deps".to_vec()]);
        assert_eq!(r.entry(&[b"deps"]).unwrap().kind, ReadoutKind::Branch);
        assert_eq!(
            r.blob(&[b"deps", b"lib", b"src", b"b.txt"]),
            Some(Blob::Bytes(b"bee".to_vec()))
        );
        // The un-nested location no longer resolves.
        assert_eq!(r.entry(&[b"src"]), None);
        // Empty prefix is the identity.
        let id = ViewReadout::nested(sample(), &[]);
        assert_eq!(id.blob(&[b"zzz"]), Some(Blob::Bytes(b"z".to_vec())));
    }
}
