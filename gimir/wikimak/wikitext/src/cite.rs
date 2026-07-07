//! MediaWiki Cite extension (`<ref>` / `<references>`) — browsing plan
//! §3.5 ("implemented early (ubiquitous)"). Owns the per-page footnote
//! STATE (numbering, groups, named-ref reuse) and the id/label scheme;
//! the escaped HTML shells live in [`crate::html`]. Ref content is
//! rendered by the parser's inline path and handed in ALREADY escaped, so
//! nothing here emits raw box-produced bytes.
//!
//! State discipline (plan §3.6): [`CiteState`] is created per render and
//! held inside the parser's Ctx — it never crosses the lib.rs boundary.
//! Failures (empty ref, redefinition, missing `<references/>`) surface as
//! inline error boxes / list errors and are counted by the caller; they
//! never panic and never silently drop a citation.

use std::collections::BTreeMap;

use crate::html;

/// One footnote within a group. `content` is final, already-escaped HTML;
/// `None` means the name was used but never given a body → error in list.
#[derive(Default)]
pub(crate) struct Note {
    /// 1-based sequential number within its group.
    pub number: usize,
    pub name: Option<String>,
    pub content: Option<String>,
    /// Strip-item index of each inline use marker, in document order. Its
    /// length is the use count that decides single- vs multi-backlink.
    pub uses: Vec<usize>,
}

/// A citation group. `name` is empty for the default group; `<ref group=…>`
/// and `<references group=…>` open separate, independently-numbered groups.
#[derive(Default)]
pub(crate) struct Group {
    pub name: String,
    pub notes: Vec<Note>,
    /// note name → index into `notes` (first definition wins).
    pub by_name: BTreeMap<String, usize>,
    /// Strip-item index reserved by a `<references>` for this group. `None`
    /// after all refs are seen ⇒ the list is auto-appended with an error.
    pub references_marker: Option<usize>,
}

/// Per-render citation state. Groups are stored in first-seen order so the
/// auto-appended lists come out deterministically.
#[derive(Default)]
pub(crate) struct CiteState {
    pub groups: Vec<Group>,
    index: BTreeMap<String, usize>,
}

/// Outcome of recording one `<ref>` use, for the caller's miss accounting.
pub(crate) struct Recorded {
    /// True when a second content-bearing definition of a name was ignored
    /// (first definition wins) — the caller logs a miss.
    pub redefinition: bool,
}

impl CiteState {
    /// Index of the group named `name`, creating it if unseen.
    pub fn group_idx(&mut self, name: &str) -> usize {
        if let Some(&i) = self.index.get(name) {
            return i;
        }
        let i = self.groups.len();
        self.groups.push(Group {
            name: name.to_string(),
            ..Default::default()
        });
        self.index.insert(name.to_string(), i);
        i
    }

    /// Register one `<ref>` use in group `group_idx`. `name` `None` = an
    /// anonymous ref (always a fresh note). `content` = rendered HTML when
    /// the tag carried a body. `strip_idx` = the reserved inline marker the
    /// caller will backfill with the final `<sup>` once use counts are known.
    pub fn record_use(
        &mut self,
        group_idx: usize,
        name: Option<&str>,
        content: Option<String>,
        strip_idx: usize,
    ) -> Recorded {
        let g = &mut self.groups[group_idx];
        let mut redefinition = false;
        let note_i = match name {
            Some(n) => {
                if let Some(&i) = g.by_name.get(n) {
                    match (g.notes[i].content.is_some(), content) {
                        // First definition wins; a later body is a miss.
                        (true, Some(_)) => redefinition = true,
                        (false, Some(c)) => g.notes[i].content = Some(c),
                        _ => {}
                    }
                    i
                } else {
                    let i = g.notes.len();
                    g.notes.push(Note {
                        number: i + 1,
                        name: Some(n.to_string()),
                        content,
                        uses: Vec::new(),
                    });
                    g.by_name.insert(n.to_string(), i);
                    i
                }
            }
            None => {
                let i = g.notes.len();
                g.notes.push(Note {
                    number: i + 1,
                    name: None,
                    content,
                    uses: Vec::new(),
                });
                i
            }
        };
        g.notes[note_i].uses.push(strip_idx);
        Recorded { redefinition }
    }

    /// List-defined reference: fill a still-undefined, already-used named
    /// note from a `<references>…</references>` body. Never creates a note
    /// (an LDR entry for an unused name is inert). Returns true if it set.
    pub fn define(&mut self, group_idx: usize, name: &str, content: String) -> bool {
        let g = &mut self.groups[group_idx];
        if let Some(&i) = g.by_name.get(name) {
            if g.notes[i].content.is_none() {
                g.notes[i].content = Some(content);
                return true;
            }
        }
        false
    }
}

// ---- id / label scheme -----------------------------------------------

/// `id`/`href` anchor for a note's list entry (`cite_note-…`).
fn note_id(group: &str, number: usize) -> String {
    if group.is_empty() {
        format!("cite_note-{number}")
    } else {
        format!("cite_note-{group}-{number}")
    }
}

/// `id` of one inline use (`cite_ref-…`). A note with a single use gets an
/// unsuffixed id; a reused note suffixes the 0-based use index so every
/// back-link targets the right occurrence.
fn ref_id(group: &str, number: usize, use_index: usize, multi: bool) -> String {
    let base = if group.is_empty() {
        format!("cite_ref-{number}")
    } else {
        format!("cite_ref-{group}-{number}")
    };
    if multi {
        format!("{base}-{use_index}")
    } else {
        base
    }
}

/// Visible marker text: `[N]` in the default group, `[g N]` in group `g`.
fn inline_label(group: &str, number: usize) -> String {
    if group.is_empty() {
        format!("[{number}]")
    } else {
        format!("[{group} {number}]")
    }
}

/// Bijective base-26 back-link label: 0→a, 25→z, 26→aa, 27→ab, …
fn letter_label(i: usize) -> String {
    let mut n = i + 1;
    let mut s = Vec::new();
    while n > 0 {
        n -= 1;
        s.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
    }
    s.iter().rev().collect()
}

/// Final inline `<sup>` for use `use_index` of a note. `multi` = the note
/// has more than one use (drives the suffixed id / lettered back-links).
pub(crate) fn sup_html(group: &str, number: usize, use_index: usize, multi: bool) -> String {
    let rid = ref_id(group, number, use_index, multi);
    let nid = note_id(group, number);
    let label = inline_label(group, number);
    html::ref_marker(&rid, &nid, &label)
}

/// The whole `<ol class="references">` list for one group.
pub(crate) fn references_list_html(g: &Group) -> String {
    let mut items = String::new();
    for note in &g.notes {
        let nid = note_id(&g.name, note.number);
        let backlinks = if note.uses.len() <= 1 {
            html::ref_backlink_single(&ref_id(&g.name, note.number, 0, false))
        } else {
            let entries: Vec<(String, String)> = (0..note.uses.len())
                .map(|u| (ref_id(&g.name, note.number, u, true), letter_label(u)))
                .collect();
            html::ref_backlink_multi(&entries)
        };
        let content = note.content.clone().unwrap_or_else(|| {
            html::cite_error(&match &note.name {
                Some(n) => format!("the named reference \"{n}\" was used but never defined"),
                None => "this reference has no content".to_string(),
            })
        });
        items.push_str(&html::reference_item(&nid, &backlinks, &content));
    }
    html::references_wrap(&items)
}
