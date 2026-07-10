//! RO-attachment readout over ONE PINNED draft revision
//! (ATTACH-CONVERGENCE.md chip 2).
//!
//! The attachment's name pins a revision (`ietf:<draft>@<rev>`), and
//! this readout serves exactly that: one leaf `<draft>-<rev>.txt` at
//! the readout root, bytes frozen at the pin — never "whatever is head
//! by the time someone reads". Residency is the pinned revision's text
//! alone; the decode walk visits newer records one frame at a time and
//! drops them ([`Mirror::revision`]).
//!
//! Locking: construction does no I/O. The first access opens the
//! mirror read-side (SHARED flock, [`Mirror::open_read`]) just long
//! enough to decode the pinned revision, then drops it — an attached
//! box never blocks `ietfmak update`, and an update in flight makes
//! the access a MISS that is retried on the next access (never cached:
//! only a real decode or a definitive no-such-store outcome is).
//! Updates keep the exclusive lock; the pin keeps them invisible here.

use std::path::PathBuf;
use std::sync::Mutex;

use depot::variant::{Blob, Readout, ReadoutEntry, ReadoutKind};
use depot::{Attrs, Name};

use crate::{Error, Mirror, MirrorConfig};

pub struct DraftReadout {
    root: PathBuf,
    draft: String,
    rev: String,
    /// `<draft>-<rev>.txt` — the single leaf name.
    file_name: Vec<u8>,
    /// Outer `None` = not resolved yet (or a writer held the root:
    /// retry). Inner `None` = definitive miss (no such store/draft/rev).
    text: Mutex<Option<Option<Vec<u8>>>>,
}

impl DraftReadout {
    /// Pure bookkeeping — the store is not touched until first access.
    pub fn new(root: PathBuf, draft: &str, rev: &str) -> Self {
        DraftReadout {
            root,
            draft: draft.to_string(),
            rev: rev.to_string(),
            file_name: format!("{draft}-{rev}.txt").into_bytes(),
            text: Mutex::new(None),
        }
    }

    /// Run `f` over the pinned text (`None` = miss), resolving it on
    /// first use. The closure form keeps the bytes behind the mutex —
    /// only `blob` pays for a copy.
    fn with_text<T>(&self, f: impl FnOnce(Option<&[u8]>) -> T) -> T {
        let mut slot = self.text.lock().expect("readout mutex poisoned");
        if slot.is_none() {
            match Mirror::open_read(MirrorConfig::new(self.root.clone())) {
                Ok(m) => {
                    // Decode errors and absent draft/rev are the same
                    // definitive miss to the overlay (never an error);
                    // the Mirror (and its shared lock) drops right here.
                    *slot = Some(m.revision(&self.draft, &self.rev).ok().flatten().map(|e| e.text));
                }
                // A writer holds the root (update in flight): miss NOW,
                // resolve on a later access.
                Err(Error::MirrorLocked(_)) => return f(None),
                // No mirror at that root / unreadable: definitive miss.
                Err(_) => *slot = Some(None),
            }
        }
        f(slot.as_ref().expect("just resolved").as_deref())
    }
}

impl Readout for DraftReadout {
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
