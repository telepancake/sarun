//! Readout over one pinned revision in a portable Wikipedia archive.

use std::path::PathBuf;
use std::sync::Mutex;

use depot::variant::{Blob, Readout, ReadoutEntry, ReadoutKind};
use depot::{Attrs, Name};

pub struct PageReadout {
    archive: PathBuf,
    page_id: u64,
    /// The pinned revision id — what the attachment's `@r<rev>` names.
    rev_id: u64,
    /// `<sanitized title>.txt` — the single leaf name.
    file_name: Vec<u8>,
    /// Outer `None` = not resolved yet (or a writer held the root:
    /// retry). Inner `None` = definitive miss (no store/page/revision).
    text: Mutex<Option<Option<Vec<u8>>>>,
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
            text: Mutex::new(None),
        }
    }

    /// Run `f` over the pinned text (`None` = miss), resolving it on
    /// first use. The closure form keeps the bytes behind the mutex —
    /// only `blob` pays for a copy.
    fn with_text<T>(&self, f: impl FnOnce(Option<&[u8]>) -> T) -> T {
        let mut slot = self.text.lock().expect("readout mutex poisoned");
        if slot.is_none() {
            *slot = Some(
                crate::archive_browse::ArchiveBrowseIndex::open_installed(&self.archive)
                    .and_then(|archive| archive.revision(self.page_id, self.rev_id))
                    .ok()
                    .flatten()
                    .and_then(|revision| revision.has_text.then_some(revision.text)),
            );
        }
        f(slot.as_ref().expect("just resolved").as_deref())
    }
}

impl Readout for PageReadout {
    fn entry(&self, at: &[&[u8]]) -> Option<ReadoutEntry> {
        self.with_text(|text| {
            let text = text?;
            match at {
                [] => Some(ReadoutEntry {
                    kind: ReadoutKind::Branch,
                    blob_len: None,
                    attrs: Attrs::new(),
                }),
                [name] if *name == self.file_name.as_slice() => Some(ReadoutEntry {
                    kind: ReadoutKind::Leaf,
                    blob_len: Some(text.len() as u64),
                    attrs: Attrs::new(),
                }),
                _ => None,
            }
        })
    }

    fn children(&self, at: &[&[u8]]) -> Vec<Name> {
        if at.is_empty() && self.with_text(|t| t.is_some()) {
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
