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
        // Resolve the sha to a commit sha: either it is one directly, or
        // it is an annotated tag whose target is a commit.
        let commit_sha = if st.sha_to_idx(sha)?.is_some() {
            sha.to_string()
        } else {
            match st.tag_sha_to_idx(sha)? {
                Some(ti) => match st.tag_record_at(ti)?.target {
                    store::TagTarget::Commit(c) => st.commit_record_at(c)?.sha,
                    // Standalone tree targets are no longer served.
                    store::TagTarget::Tree(_) => return Ok(None),
                },
                None => return Ok(None),
            }
        };
        let tree = st.union()?.tree_view_of_commit(&commit_sha)?;
        Ok(Some(Self::from_tree(store, tree, prefix)))
    }

    fn from_tree(store: &Path, tree: View, prefix: &str) -> Self {
        let prefix = split_prefix(prefix);
        let nested =
            nest_view(tree, &prefix.iter().map(|c| c.as_slice()).collect::<Vec<_>>());
        let view = OnceLock::new();
        view.set(Some(nested)).expect("fresh OnceLock");
        TipReadout { store: store.to_path_buf(), prefix, view }
    }

    fn view(&self) -> Option<&View> {
        self.view
            .get_or_init(|| {
                let ls = store::Store::open(&self.store).ok()?.union().ok()?;
                let n = ls.n_rev();
                if n == 0 {
                    return None;
                }
                // The tip is the last revision in DAG order (HEAD of a
                // linear history).
                let tip = ls.tree_view_at(n - 1).ok()?;
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
