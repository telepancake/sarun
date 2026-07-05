//! RO-attachment readout over ONE draft series
//! (ATTACH-CONVERGENCE.md chip 2).
//!
//! Serves the shape the engine's `ietf_attach` verb serves today: every
//! mirrored revision as a leaf `<draft>-<rev>.txt` at the readout root
//! (a whole series is small and the history is the point of a drafts
//! mirror). Construction is pure bookkeeping; the chain decode happens
//! once, on first access, and is cached. Per-file lazy decode is not
//! available from this store: f1/cold frames are refPrefix-anchored on
//! the next-newer record, so decoding revision k requires decoding
//! everything newer anyway — one `history()` walk IS the lazy unit.

use std::sync::{Mutex, OnceLock};

use depot::variant::{Blob, Readout, ReadoutEntry, ReadoutKind};
use depot::{Attrs, Name};

use crate::Mirror;

pub struct DraftReadout {
    /// `Mirror` holds a rusqlite `Connection` (Send, not Sync); the
    /// mutex makes the one decode safe under concurrent readout.
    mirror: Mutex<Mirror>,
    draft: String,
    /// `(file name, text)` sorted by name; `None` until first access.
    /// Empty = no such draft (a miss, never an error).
    files: OnceLock<Vec<(Name, Vec<u8>)>>,
}

impl DraftReadout {
    pub fn new(mirror: Mirror, draft: &str) -> Self {
        DraftReadout { mirror: Mutex::new(mirror), draft: draft.to_string(), files: OnceLock::new() }
    }

    fn files(&self) -> &[(Name, Vec<u8>)] {
        self.files.get_or_init(|| {
            let m = self.mirror.lock().expect("mirror mutex poisoned");
            let mut files: Vec<(Name, Vec<u8>)> = m
                .history(&self.draft)
                .unwrap_or_default()
                .into_iter()
                .map(|e| (format!("{}-{}.txt", self.draft, e.rev).into_bytes(), e.text))
                .collect();
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        })
    }

    fn file(&self, name: &[u8]) -> Option<&(Name, Vec<u8>)> {
        let files = self.files();
        files.binary_search_by(|f| f.0.as_slice().cmp(name)).ok().map(|i| &files[i])
    }
}

impl Readout for DraftReadout {
    fn entry(&self, at: &[&[u8]]) -> Option<ReadoutEntry> {
        match at {
            [] if !self.files().is_empty() => Some(ReadoutEntry {
                kind: ReadoutKind::Branch,
                blob_len: None,
                attrs: Attrs::new(),
            }),
            [name] => self.file(name).map(|(_, text)| ReadoutEntry {
                kind: ReadoutKind::Leaf,
                blob_len: Some(text.len() as u64),
                attrs: Attrs::new(),
            }),
            _ => None,
        }
    }

    fn children(&self, at: &[&[u8]]) -> Vec<Name> {
        if at.is_empty() {
            self.files().iter().map(|(n, _)| n.clone()).collect()
        } else {
            Vec::new()
        }
    }

    fn blob(&self, at: &[&[u8]]) -> Option<Blob> {
        match at {
            [name] => self.file(name).map(|(_, text)| Blob::Bytes(text.clone())),
            _ => None,
        }
    }
}
