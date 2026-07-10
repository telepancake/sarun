//! RO-attachment readout over ONE PINNED page revision
//! (ATTACH-CONVERGENCE.md chip 2).
//!
//! The attachment's name pins a revision (`wiki:<wiki>/<title>@r<rev>`),
//! and this readout serves exactly that: one leaf `<title>.txt` at the
//! readout root, bytes frozen at the pin — never "whatever is head by
//! the time someone reads". Residency is the pinned revision's text
//! alone; the decode is a newest-first early-stopping chain walk
//! ([`Instance::revision_text`]) that visits newer frames one at a
//! time and drops them.
//!
//! Locking: construction does no I/O. The first access opens the
//! instance read-side (SHARED flock, [`Instance::open_read`]) just
//! long enough to decode the pinned revision, then drops it — an
//! attached box never blocks `wikimak import`/`sync` on the root, and
//! a writer in flight makes the access a MISS that is retried on the
//! next access (never cached: only a real decode or a definitive
//! no-such-store outcome is). Writers keep the exclusive lock; the pin
//! keeps their new revisions invisible here.

use std::path::PathBuf;
use std::sync::Mutex;

use depot::variant::{Blob, Readout, ReadoutEntry, ReadoutKind};
use depot::{Attrs, Name};

use crate::instance::read_config;
use crate::{Error, Instance};

pub struct PageReadout {
    root: PathBuf,
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
    pub fn new(root: PathBuf, page_id: u64, title: Option<&str>, rev_id: u64) -> Self {
        let base: String = match title {
            Some(t) => t.chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect(),
            None => format!("page-{page_id}"),
        };
        PageReadout {
            root,
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
            match Instance::open_read(read_config(self.root.clone())) {
                Ok(inst) => {
                    // Decode errors and an absent page/revision are the
                    // same definitive miss to the overlay (never an
                    // error); the Instance (and its shared lock) drops
                    // right here.
                    *slot = Some(inst.revision_text(self.page_id, self.rev_id).ok().flatten());
                }
                // A writer holds the root (import/sync in flight): miss
                // NOW, resolve on a later access.
                Err(Error::InstanceLocked(_)) => return f(None),
                // No instance at that root / unreadable: definitive miss.
                Err(_) => *slot = Some(None),
            }
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
