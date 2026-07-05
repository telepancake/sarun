//! RO-attachment readout over a gitdepot store
//! (ATTACH-CONVERGENCE.md chip 2).
//!
//! The tip is frame 0 — a standalone full record — so [`TipReadout`]
//! decodes exactly one frame, on first access, and serves plain
//! [`ViewReadout`] semantics over it (nested under the attach verb's
//! `prefix`, so e.g. "src" serves the repo under /src). Non-tip commits
//! have no random access: every frame is refPrefix-anchored on the
//! next-newer view, so [`TipReadout::for_commit`] pays the FULL chain
//! walk (`chain::read_store`) up front — O(history), the §4 stated
//! cost — and only then serves O(1).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use depot::variant::{nest_view, view_at, view_entry, Blob, Readout, ReadoutEntry};
use depot::{codec, Name, View};

use crate::{chain, Result};

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

    /// Readout of ONE non-tip commit, selected by exact sha. Decodes
    /// the whole chain down to it NOW (view-anchored frames admit no
    /// random access into history); serving is O(1) afterwards.
    /// `Ok(None)` when the sha is not in the chain.
    pub fn for_commit(store: &Path, sha: &str, prefix: &str) -> Result<Option<Self>> {
        let (meta, views) = chain::read_store(store)?;
        let Some(idx) = meta.commits.iter().position(|c| c.sha == sha) else {
            return Ok(None);
        };
        let prefix = split_prefix(prefix);
        let nested = nest_view(
            views.into_iter().nth(idx).expect("read_store aligns views with commits"),
            &prefix.iter().map(|c| c.as_slice()).collect::<Vec<_>>(),
        );
        let view = OnceLock::new();
        view.set(Some(nested)).expect("fresh OnceLock");
        Ok(Some(TipReadout { store: store.to_path_buf(), prefix, view }))
    }

    fn view(&self) -> Option<&View> {
        self.view
            .get_or_init(|| {
                let record = chain::read_head_record(&self.store).ok()?;
                let layer = codec::decode(&record).ok()?;
                let tip = depot::apply(None, &layer)?;
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
        view_at(self.view()?, at)?.blob.clone().map(Blob::Bytes)
    }
}
