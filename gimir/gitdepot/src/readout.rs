//! RO-attachment readout over a gitdepot store
//! (ATTACH-CONVERGENCE.md chip 2).
//!
//! The tip is the TREES chain's f0 — a standalone full record — so
//! [`TipReadout`] decodes exactly one frame, on first access, and serves
//! plain [`ViewReadout`] semantics over it (nested under the attach
//! verb's `prefix`, so e.g. "src" serves the repo under /src).
//! [`TipReadout::for_commit`] resolves sha → commit index → tree index
//! through the bookkeeping, then walks the TREES chain from the head
//! down to that tree applying reverse deltas — O(distance from tip),
//! paid up front; serving is O(1) afterwards.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use depot::variant::{nest_view, view_at, view_entry, Blob, Readout, ReadoutEntry};
use depot::{Name, View};

use crate::{store, Result};

pub struct TipReadout {
    store: PathBuf,
    /// Slash-separated attach prefix, split into components.
    prefix: Vec<Name>,
    /// The served (prefix-nested) view; `None` = unreadable store, a
    /// readout miss. Populated lazily by `new`, eagerly by `for_commit`.
    view: OnceLock<Option<View>>,
}

fn split_prefix(prefix: &str) -> Vec<Name> {
    prefix.split('/').filter(|c| !c.is_empty()).map(|c| c.as_bytes().to_vec()).collect()
}

impl TipReadout {
    /// Lazy tip readout: nothing is read until the first access.
    pub fn new(store: &Path, prefix: &str) -> Self {
        TipReadout {
            store: store.to_path_buf(),
            prefix: split_prefix(prefix),
            view: OnceLock::new(),
        }
    }

    /// Readout of ONE commit, selected by exact sha — or, when the sha
    /// is an annotated TAG's (the tree-tag pin written by git_attach),
    /// the tag's peeled tree. Walks the trees chain down to that tree
    /// NOW; serving is O(1) afterwards. `Ok(None)` when the sha is
    /// neither a commit nor a tag in the store.
    pub fn for_commit(store: &Path, sha: &str, prefix: &str) -> Result<Option<Self>> {
        let st = store::Store::open(store)?;
        let tree_idx = match st.sha_to_idx(sha)? {
            Some(cidx) => st.commit_record_at(cidx)?.tree_idx,
            None => match st.tag_sha_to_idx(sha)? {
                Some(ti) => match st.tag_record_at(ti)?.target {
                    store::TagTarget::Tree(t) => t,
                    store::TagTarget::Commit(c) => st.commit_record_at(c)?.tree_idx,
                },
                None => return Ok(None),
            },
        };
        Ok(Some(Self::for_tree_open(&st, store, tree_idx, prefix)?))
    }

    /// Readout of one TREES record by stable index — the tree-tag
    /// serving path, trivial next to `for_commit`.
    pub fn for_tree(store: &Path, tree_idx: u64, prefix: &str) -> Result<Self> {
        let st = store::Store::open(store)?;
        Self::for_tree_open(&st, store, tree_idx, prefix)
    }

    fn for_tree_open(
        st: &store::Store,
        store: &Path,
        tree_idx: u64,
        prefix: &str,
    ) -> Result<Self> {
        let tree = st.tree_view(tree_idx)?;
        let prefix = split_prefix(prefix);
        let nested =
            nest_view(tree, &prefix.iter().map(|c| c.as_slice()).collect::<Vec<_>>());
        let view = OnceLock::new();
        view.set(Some(nested)).expect("fresh OnceLock");
        Ok(TipReadout { store: store.to_path_buf(), prefix, view })
    }

    fn view(&self) -> Option<&View> {
        self.view
            .get_or_init(|| {
                let st = store::Store::open(&self.store).ok()?;
                let tip = st.tree_views(Some(0)).ok()?.pop()?;
                let prefix: Vec<&[u8]> = self.prefix.iter().map(|c| c.as_slice()).collect();
                Some(nest_view(tip, &prefix))
            })
            .as_ref()
    }
}

impl Readout for TipReadout {
    fn entry(&self, at: &[&[u8]]) -> Option<ReadoutEntry> {
        view_at(self.view()?, at).map(view_entry)
    }

    fn children(&self, at: &[&[u8]]) -> Vec<Name> {
        match self.view().and_then(|v| view_at(v, at)) {
            Some(v) => v.children.keys().cloned().collect(),
            None => Vec::new(),
        }
    }

    fn blob(&self, at: &[&[u8]]) -> Option<Blob> {
        view_at(self.view()?, at)?.blob.as_deref().map(|b| Blob::Bytes(b.to_vec()))
    }
}
