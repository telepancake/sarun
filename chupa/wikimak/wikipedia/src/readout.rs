//! Readout over one pinned revision in a portable Wikipedia archive.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use depot::variant::{Blob, Readout, ReadoutEntry, ReadoutKind};
use depot::{Attrs, Name};

struct ResolvedRevision {
    index: usize,
    summary: crate::archive_browse::PageRevisionSummary,
}

struct ReadoutState {
    /// Opening the selected generation once pins this readout to it across a
    /// later selector publication.
    archive: Option<Arc<crate::archive_browse::ArchiveBrowseIndex>>,
    /// Outer `None` means metadata has not been resolved. Inner `None` is a
    /// definitive missing page/revision or a revision without retained text.
    revision: Option<Option<ResolvedRevision>>,
    /// Outer `None` means text has not been read yet. This keeps `entry` and
    /// `children` metadata-only; the full pinned text is read for `blob`.
    text: Option<Option<Arc<[u8]>>>,
}

pub struct PageReadout {
    archive: PathBuf,
    page_id: u64,
    /// The pinned revision id — what the attachment's `@r<rev>` names.
    rev_id: u64,
    /// `<sanitized title>.txt` — the single leaf name.
    file_name: Vec<u8>,
    /// Failed filesystem/decode operations leave the corresponding outer
    /// slot unresolved so a later access can recover. Definitive misses are
    /// cached only after metadata or text was successfully inspected.
    state: Mutex<ReadoutState>,
}

impl PageReadout {
    /// Pure bookkeeping — the store is not touched until first access.
    /// `title` names the leaf (the verb resolves titles; ids are the
    /// plumbing): `/` and NUL are name separators/terminators in
    /// consumers, so they are replaced, matching `wiki_attach`.
    pub fn new(archive: PathBuf, page_id: u64, title: Option<&str>, rev_id: u64) -> Self {
        let base: String = match title {
            Some(t) => t.chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect(),
            None => format!("page-{page_id}"),
        };
        PageReadout {
            archive,
            page_id,
            rev_id,
            file_name: format!("{base}.txt").into_bytes(),
            state: Mutex::new(ReadoutState {
                archive: None,
                revision: None,
                text: None,
            }),
        }
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut ReadoutState) -> T) -> T {
        let mut state = self.state.lock().expect("readout mutex poisoned");
        f(&mut state)
    }

    fn ensure_archive(state: &mut ReadoutState, path: &std::path::Path) -> bool {
        if state.archive.is_some() {
            return true;
        }
        match crate::archive_browse::ArchiveBrowseIndex::open_installed(path) {
            Ok(archive) => {
                state.archive = Some(Arc::new(archive));
                true
            }
            Err(_) => false,
        }
    }

    /// Run `f` over metadata without loading the revision text. The closure
    /// form keeps the generation lease and summary behind the mutex.
    fn with_revision<T>(&self, f: impl FnOnce(Option<&ResolvedRevision>) -> T) -> T {
        self.with_state(|state| {
            if state.revision.is_none()
                && Self::ensure_archive(state, &self.archive)
            {
                let archive = state.archive.as_ref().expect("archive just opened");
                match archive.revision_metadata(self.page_id, self.rev_id, usize::MAX) {
                    Ok(revision) => {
                        state.revision = Some(revision.map(|(index, summary)| {
                            ResolvedRevision { index, summary }
                        }));
                    }
                    Err(_) => {
                        // A missing selector, transient I/O failure, or
                        // publication race is not evidence of a definitive
                        // miss. Leave metadata unresolved for retry.
                    }
                }
            }
            f(state.revision.as_ref().and_then(|revision| revision.as_ref()))
        })
    }

    /// Run `f` over the pinned text, resolving it only when needed. The
    /// archive cursor returns shared bytes; `blob` performs the one copy
    /// required by depot's current `Blob::Bytes(Vec<u8>)` boundary.
    fn with_text<T>(&self, f: impl FnOnce(Option<&[u8]>) -> T) -> T {
        self.with_state(|state| {
            if state.text.is_none() {
                if state.revision.is_none()
                    && Self::ensure_archive(state, &self.archive)
                {
                    let archive = state.archive.as_ref().expect("archive just opened");
                    match archive.revision_metadata(self.page_id, self.rev_id, usize::MAX) {
                        Ok(revision) => {
                            state.revision = Some(revision.map(|(index, summary)| {
                                ResolvedRevision { index, summary }
                            }));
                        }
                        Err(_) => {
                            // Retry metadata resolution on the next access.
                        }
                    }
                }
                if let Some(Some(revision)) = state.revision.as_ref() {
                    if !revision.summary.has_text {
                        state.text = Some(None);
                    } else if let Some(archive) = state.archive.as_ref() {
                        match archive.revision_text_at_index(
                            self.page_id,
                            revision.index,
                            0,
                            usize::MAX,
                        ) {
                            Ok(text) => state.text = Some(text),
                            Err(_) => {
                                // Keep the text unresolved so transient read
                                // failures remain recoverable.
                            }
                        }
                    }
                } else if state.revision.is_some() {
                    state.text = Some(None);
                }
            }
            f(state.text.as_ref().and_then(|text| text.as_deref()))
        })
    }
}

impl Readout for PageReadout {
    fn entry(&self, at: &[&[u8]]) -> Option<ReadoutEntry> {
        self.with_revision(|revision| {
            let revision = revision?;
            if !revision.summary.has_text {
                return None;
            }
            match at {
                [] => Some(ReadoutEntry {
                    kind: ReadoutKind::Branch,
                    blob_len: None,
                    attrs: Attrs::new(),
                }),
                [name] if *name == self.file_name.as_slice() => Some(ReadoutEntry {
                    kind: ReadoutKind::Leaf,
                    blob_len: Some(revision.summary.text_len),
                    attrs: Attrs::new(),
                }),
                _ => None,
            }
        })
    }

    fn children(&self, at: &[&[u8]]) -> Vec<Name> {
        if at.is_empty()
            && self.with_revision(|revision| {
                revision.is_some_and(|revision| revision.summary.has_text)
            })
        {
            vec![self.file_name.clone()]
        } else {
            Vec::new()
        }
    }

    fn blob(&self, at: &[&[u8]]) -> Option<Blob> {
        match at {
            [name] if *name == self.file_name.as_slice() => {
                self.with_text(|t| t.map(|b| Blob::Bytes(b.to_vec())))
            }
            _ => None,
        }
    }
}
