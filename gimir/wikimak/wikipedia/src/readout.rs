//! RO-attachment readout over ONE page's head revision
//! (ATTACH-CONVERGENCE.md chip 2).
//!
//! Serves the shape the engine's `wiki_attach` verb serves today: the
//! page's head text as a single leaf `<title>.txt` (title sanitized,
//! `page-<id>` fallback) at the readout root. Construction is pure
//! bookkeeping — the frame decode (`page_head` + `page_head_text`, one
//! standalone f0 decode) happens on first access and is cached; a
//! missing page is a readout miss, never an error.

use std::sync::OnceLock;

use depot::variant::{Blob, Readout, ReadoutEntry, ReadoutKind};
use depot::{Attrs, Name};

use crate::instance::Instance;

pub struct PageHeadReadout {
    instance: Instance,
    page_id: u64,
    /// `<sanitized title>.txt` — the single leaf name.
    file_name: Vec<u8>,
    /// Head text, decoded once; `None` = no such page.
    text: OnceLock<Option<Vec<u8>>>,
}

impl PageHeadReadout {
    /// `title` names the leaf (the verb resolves titles; ids are the
    /// plumbing). `/` and NUL are name separators/terminators in
    /// consumers, so they are replaced, matching `wiki_attach`.
    pub fn new(instance: Instance, page_id: u64, title: Option<&str>) -> Self {
        let base: String = match title {
            Some(t) => t.chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect(),
            None => format!("page-{page_id}"),
        };
        PageHeadReadout {
            instance,
            page_id,
            file_name: format!("{base}.txt").into_bytes(),
            text: OnceLock::new(),
        }
    }

    fn head_text(&self) -> Option<&[u8]> {
        self.text
            .get_or_init(|| self.instance.page_head_text(self.page_id).ok().flatten())
            .as_deref()
    }
}

impl Readout for PageHeadReadout {
    fn entry(&self, at: &[&[u8]]) -> Option<ReadoutEntry> {
        let text = self.head_text()?;
        match at {
            [] => Some(ReadoutEntry { kind: ReadoutKind::Branch, blob_len: None, attrs: Attrs::new() }),
            [name] if *name == self.file_name.as_slice() => Some(ReadoutEntry {
                kind: ReadoutKind::Leaf,
                blob_len: Some(text.len() as u64),
                attrs: Attrs::new(),
            }),
            _ => None,
        }
    }

    fn children(&self, at: &[&[u8]]) -> Vec<Name> {
        if at.is_empty() && self.head_text().is_some() {
            vec![self.file_name.clone()]
        } else {
            Vec::new()
        }
    }

    fn blob(&self, at: &[&[u8]]) -> Option<Blob> {
        match at {
            [name] if *name == self.file_name.as_slice() => {
                Some(Blob::Bytes(self.head_text()?.to_vec()))
            }
            _ => None,
        }
    }
}
