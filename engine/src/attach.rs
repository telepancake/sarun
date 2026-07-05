//! External RO attachments (commit 2 of gimir/notes/attach-
//! implementation-plan.md): the live-side object behind a
//! `RoAttachment::Ext` bookkeeping row. Wraps a mirror store's
//! `depot::variant::Readout` with everything the overlay needs to
//! serve it as a chain link: lazy open (registration does NO I/O — a
//! moved/deleted store must never brick box hydration; the error
//! surfaces at first use and in the session dict), prefix nesting
//! (attach verbs serve stores under a subdirectory), and a per-rel
//! memo (sound within one hydration: the readout decodes once behind
//! a OnceLock, so what it serves cannot change underneath the memo).
//!
//! Pinning honesty: `git` is pinned by content (full sha —
//! re-lookup finds byte-identical trees or fails naming the sha).
//! `wiki`/`ietf` record the head rev seen at attach time for identity
//! and display, but their adapters serve the store's CURRENT head at
//! first-use decode — true serve-at-rev needs read-at-rev adapters
//! (adapters v2 chip). Within a hydration the OnceLock freezes what
//! is served either way.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use depot_model::variant::{Blob, Readout, ReadoutKind};

use crate::capture::ExtRef;

/// What the overlay needs per resolved name: enough for getattr
/// WITHOUT touching blob bytes (laziness: `ls -lR` must not decode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtEntry {
    pub dir: bool,
    pub size: u64,
    pub mode: u32,
}

pub struct ExtAttachment {
    pub ext: ExtRef,
    /// Prefix as components (empty = attach at box root).
    prefix: Vec<Vec<u8>>,
    readout: OnceLock<Result<Arc<dyn Readout>, String>>,
    entries: RwLock<HashMap<String, Option<ExtEntry>>>,
}

impl ExtAttachment {
    pub fn new(ext: ExtRef) -> Self {
        let prefix = ext.prefix.split('/')
            .filter(|c| !c.is_empty())
            .map(|c| c.as_bytes().to_vec())
            .collect();
        Self { ext, prefix, readout: OnceLock::new(),
               entries: RwLock::new(HashMap::new()) }
    }

    /// The open error, if opening was attempted and failed — for the
    /// session dict's `attachments` rows. Never triggers an open.
    pub fn error(&self) -> Option<String> {
        match self.readout.get() {
            Some(Err(e)) => Some(e.clone()),
            _ => None,
        }
    }

    fn open(&self) -> Result<&Arc<dyn Readout>, &String> {
        self.readout.get_or_init(|| open_readout(&self.ext)).as_ref()
    }

    /// `rel` ('/'-separated, "" = root) → components under the prefix.
    /// None = outside the prefix (the fast path that keeps unrelated
    /// FUSE traffic away from the store entirely).
    fn strip<'a>(&self, rel: &'a str) -> Option<Vec<&'a [u8]>> {
        let comps: Vec<&[u8]> = rel.split('/')
            .filter(|c| !c.is_empty())
            .map(str::as_bytes)
            .collect();
        if comps.len() < self.prefix.len() {
            // An ancestor OF the prefix: a synthetic directory (the
            // attachment provides it so the subtree is reachable) iff
            // it lies on the prefix chain.
            return if self.prefix.iter().take(comps.len())
                .zip(&comps).all(|(p, c)| p.as_slice() == *c)
            { Some(vec![]) } else { None };
        }
        let (head, tail) = comps.split_at(self.prefix.len());
        if self.prefix.iter().zip(head).all(|(p, c)| p.as_slice() == *c) {
            Some(tail.to_vec())
        } else {
            None
        }
    }

    fn on_prefix_chain(&self, rel: &str) -> bool {
        let comps: Vec<&[u8]> = rel.split('/')
            .filter(|c| !c.is_empty()).map(str::as_bytes).collect();
        comps.len() < self.prefix.len()
            && self.prefix.iter().take(comps.len())
                .zip(&comps).all(|(p, c)| p.as_slice() == *c)
    }

    pub fn entry(&self, rel: &str) -> Option<ExtEntry> {
        if let Some(hit) = self.entries.read().unwrap().get(rel) {
            return *hit;
        }
        let val = self.entry_uncached(rel);
        self.entries.write().unwrap().insert(rel.to_string(), val);
        val
    }

    fn entry_uncached(&self, rel: &str) -> Option<ExtEntry> {
        if self.on_prefix_chain(rel) {
            return Some(ExtEntry { dir: true, size: 0, mode: 0o40755 });
        }
        let at = self.strip(rel)?;
        let ro = self.open().ok()?;
        let e = ro.entry(&at)?;
        let dir = matches!(e.kind, ReadoutKind::Branch);
        let mode = e.attrs.get(b"mode".as_slice())
            .and_then(|m| u32::from_str_radix(
                std::str::from_utf8(m).ok()?, 8).ok())
            .unwrap_or(if dir { 0o40755 } else { 0o100644 });
        Some(ExtEntry { dir, size: e.blob_len.unwrap_or(0), mode })
    }

    /// Child names at `rel` (leaf names only, not paths).
    pub fn children(&self, rel: &str) -> Vec<String> {
        if self.on_prefix_chain(rel) {
            let depth = rel.split('/').filter(|c| !c.is_empty()).count();
            return vec![String::from_utf8_lossy(&self.prefix[depth])
                            .into_owned()];
        }
        let Some(at) = self.strip(rel) else { return vec![] };
        let Ok(ro) = self.open() else { return vec![] };
        ro.children(&at).into_iter()
            .map(|n| String::from_utf8_lossy(&n).into_owned())
            .collect()
    }

    pub fn blob(&self, rel: &str) -> Option<Blob> {
        let at = self.strip(rel)?;
        self.open().ok()?.blob(&at)
    }
}

/// Per-kind store open + readout construction. Each arm re-validates
/// the pin where the store can: git re-resolves the sha (content-
/// addressed — byte-identical or a named failure).
fn open_readout(ext: &ExtRef) -> Result<Arc<dyn Readout>, String> {
    match ext.kind.as_str() {
        "git" => {
            let ro = gitdepot::readout::TipReadout::for_commit(
                    std::path::Path::new(&ext.store), &ext.rev, "")
                .map_err(|e| format!("git store {}: {e}", ext.store))?
                .ok_or_else(|| format!(
                    "commit {} no longer in {} (history rewritten?) — \
                     re-attach to pin the new tree", ext.rev, ext.store))?;
            Ok(Arc::new(ro))
        }
        "wiki" => {
            let inst = open_wiki_instance(&ext.store)?;
            let page: u64 = ext.refname.parse().map_err(|_|
                format!("wiki ref {:?} is not a page id", ext.refname))?;
            let title = ext.name.split(':').nth(1)
                .and_then(|s| s.rsplit_once('@').map(|(t, _)| t.to_string()));
            Ok(Arc::new(wikimak_wikipedia::readout::PageHeadReadout::new(
                inst, page, title.as_deref())))
        }
        "ietf" => {
            let m = ietf_mirror::Mirror::open(
                    ietf_mirror::MirrorConfig::new(ext.store.clone().into()))
                .map_err(|e| format!("ietf store {}: {e}", ext.store))?;
            Ok(Arc::new(ietf_mirror::readout::DraftReadout::new(
                m, &ext.refname)))
        }
        other => Err(format!("unknown attachment kind {other:?}")),
    }
}

/// Same sizing defaults as the wikimak driver CLI (and control.rs's
/// read-side open — dedupe when the attach verbs move here).
fn open_wiki_instance(root: &str)
    -> Result<wikimak_wikipedia::Instance, String>
{
    wikimak_wikipedia::Instance::open(wikimak_wikipedia::InstanceConfig {
        root: std::path::PathBuf::from(root),
        dbname: "wiki".into(),
        max_chain_id: 4_000_000,
        depot: wikimak_depot::DepotConfig {
            root: Default::default(),
            max_chain_id: 4_000_000,
            file_size_threshold: 1 << 30,
            eviction_dead_ratio: 0.5,
        },
        title_shard_count: 4,
        title_seal_threshold_bytes: 8 << 20,
        f1_seal_threshold_bytes: 0,
    }).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(kind: &str, prefix: &str) -> ExtAttachment {
        ExtAttachment::new(ExtRef {
            kind: kind.into(), store: "/nonexistent".into(),
            refname: "main".into(), rev: "abc".into(),
            prefix: prefix.into(), name: "t".into(),
        })
    }

    // A broken/missing store must degrade to misses (resolve falls
    // through; §8 independence keeps the box valid), never panic or
    // block hydration.
    #[test]
    fn missing_store_is_a_miss_not_an_error() {
        let a = ext("git", "sdk");
        assert_eq!(a.entry("sdk/README"), None);
        assert!(a.children("sdk").is_empty());
        assert!(a.blob("sdk/README").is_none());
        assert!(a.error().unwrap().contains("/nonexistent"));
    }

    // The prefix chain is synthesized without opening the store: the
    // subtree must be reachable by directory walk, and unrelated rels
    // must not touch the store at all.
    #[test]
    fn prefix_chain_synthesized_without_open() {
        let a = ext("git", "deep/sdk");
        assert_eq!(a.entry(""), Some(ExtEntry { dir: true, size: 0, mode: 0o40755 }));
        assert_eq!(a.entry("deep"), Some(ExtEntry { dir: true, size: 0, mode: 0o40755 }));
        assert_eq!(a.children(""), vec!["deep".to_string()]);
        assert_eq!(a.children("deep"), vec!["sdk".to_string()]);
        assert_eq!(a.entry("elsewhere"), None);
        assert_eq!(a.entry("deep/other"), None);
        // None of the above opened the store: no error recorded.
        assert!(a.error().is_none());
    }

    #[test]
    fn unknown_kind_reports() {
        let a = ext("zip", "");
        assert_eq!(a.entry("x"), None);
        assert!(a.error().unwrap().contains("unknown attachment kind"));
    }
}
