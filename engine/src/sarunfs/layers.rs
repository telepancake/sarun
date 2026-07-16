//! Pure merged-layer resolution for [`SarunFs`](crate::sarunfs::SarunFs).
//!
//! Storage ownership and attachment hydration stay outside this module.  A
//! caller supplies the already-bounded chain and whether the host backing path
//! exists; this module applies overlay precedence, whiteouts, holes, opacity,
//! rebasing, attachment semantics, and synthetic landing pads exactly once.

use std::path::PathBuf;
use std::sync::Arc;

use crate::capture::{BoxState, Entry};
use crate::depot::BoxDepot;

pub(crate) enum Layer {
    Absent,
    UpperFile {
        owner: i64,
        rowid: i64,
        mode: u32,
    },
    UpperDir {
        mode: u32,
        mtime_ns: i64,
    },
    UpperSymlink {
        target: PathBuf,
    },
    UpperSpecial {
        mode: u32,
        rdev: u64,
    },
    ExtFile {
        att: Arc<crate::attach::ExtAttachment>,
        rel: String,
        size: u64,
        mode: u32,
    },
    Lower,
}

pub(crate) enum ChainLink {
    Box(Arc<BoxState>),
    Ext(Arc<crate::attach::ExtAttachment>),
}

pub(crate) fn has_rebased_ancestor(box_state: &BoxState, rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let mut parent = std::path::Path::new(rel).parent();
    while let Some(ancestor) = parent {
        let text = ancestor.to_string_lossy();
        if matches!(
            box_state.entry(&text),
            Some(Entry::Dir { rebased: true, .. })
        ) {
            return true;
        }
        if text.is_empty() {
            break;
        }
        parent = ancestor.parent();
    }
    false
}

pub(crate) fn has_opaque_ancestor(box_state: &BoxState, rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let mut parent = std::path::Path::new(rel).parent();
    while let Some(ancestor) = parent {
        let text = ancestor.to_string_lossy();
        if box_state.is_opaque(&text) {
            return true;
        }
        if text.is_empty() {
            break;
        }
        parent = ancestor.parent();
    }
    false
}

pub(crate) fn own_layer(box_state: &BoxState, rel: &str, lower_exists: bool) -> Layer {
    match box_state.entry(rel) {
        Some(Entry::Whiteout) => Layer::Absent,
        Some(Entry::File { rowid, mode }) => Layer::UpperFile {
            owner: box_state.id,
            rowid,
            mode,
        },
        Some(Entry::Dir { mode, mtime_ns, .. }) => Layer::UpperDir { mode, mtime_ns },
        Some(Entry::Symlink { target }) => Layer::UpperSymlink { target },
        Some(Entry::Special { mode, rdev }) => Layer::UpperSpecial { mode, rdev },
        Some(Entry::Hole) | None if lower_exists => Layer::Lower,
        Some(Entry::Hole) | None => Layer::Absent,
    }
}

pub(crate) fn chain_has_children(chain: &[ChainLink], rel: &str) -> bool {
    chain.iter().any(|link| match link {
        ChainLink::Box(box_state) => {
            let (_, present, _) = box_state.children_of(rel);
            !present.is_empty()
        }
        ChainLink::Ext(attachment) => !attachment.children(rel).is_empty(),
    })
}

fn synthetic_landing(rel: &str) -> bool {
    matches!(rel, "proc" | "dev" | "sys" | "tmp")
}

pub(crate) fn resolve(
    box_id: i64,
    rel: &str,
    origin_is_api: bool,
    chain: &[ChainLink],
    lower_exists: bool,
) -> Layer {
    if rel == "tmp" && origin_is_api {
        return Layer::UpperSymlink {
            target: crate::paths::oaita_state_home()
                .join(".tmp")
                .join(box_id.to_string()),
        };
    }

    let mut no_host = false;
    for link in chain {
        let box_state = match link {
            ChainLink::Ext(attachment) => match attachment.entry(rel) {
                Some(entry) if entry.dir => {
                    return Layer::UpperDir {
                        mode: entry.mode,
                        mtime_ns: 0,
                    };
                }
                Some(entry) => {
                    return Layer::ExtFile {
                        att: attachment.clone(),
                        rel: rel.to_owned(),
                        size: entry.size,
                        mode: entry.mode,
                    };
                }
                None => continue,
            },
            ChainLink::Box(box_state) => box_state,
        };
        if box_state.no_host_fallback() {
            no_host = true;
        }
        match box_state.entry(rel) {
            Some(Entry::Whiteout) => return Layer::Absent,
            Some(Entry::File { rowid, mode }) => {
                return Layer::UpperFile {
                    owner: box_state.id,
                    rowid,
                    mode,
                };
            }
            Some(Entry::Dir { mode, mtime_ns, .. }) => {
                return Layer::UpperDir { mode, mtime_ns };
            }
            Some(Entry::Symlink { target }) => return Layer::UpperSymlink { target },
            Some(Entry::Special { mode, rdev }) => return Layer::UpperSpecial { mode, rdev },
            Some(Entry::Hole) => break,
            None if has_opaque_ancestor(box_state, rel) => return Layer::Absent,
            None if has_rebased_ancestor(box_state, rel) => break,
            None => {}
        }
    }

    let landing = synthetic_landing(rel) && !chain_has_children(chain, rel);
    if no_host {
        if landing {
            Layer::UpperDir {
                mode: 0o0555,
                mtime_ns: 0,
            }
        } else {
            Layer::Absent
        }
    } else if lower_exists {
        Layer::Lower
    } else if landing {
        Layer::UpperDir {
            mode: 0o0555,
            mtime_ns: 0,
        }
    } else {
        Layer::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_absent(layer: Layer) -> bool {
        matches!(layer, Layer::Absent)
    }

    fn is_lower(layer: Layer) -> bool {
        matches!(layer, Layer::Lower)
    }

    #[test]
    fn merged_resolution_has_one_transport_independent_precedence_model() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "sarun-layers-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: every state-home-dependent test using BoxState is serialized
        // by TEST_STATE_HOME_LOCK.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp) };
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let upper = Arc::new(BoxState::create(9811).unwrap());
        let lower = Arc::new(BoxState::create(9812).unwrap());
        lower.ensure_file_row("shared", 0o100640, 0);
        lower.ensure_file_row("opaque/child", 0o100644, 0);
        lower.ensure_file_row("rebased/child", 0o100644, 0);
        let chain = [ChainLink::Box(upper.clone()), ChainLink::Box(lower.clone())];

        // The first concrete entry in the chain wins.
        match resolve(upper.id, "shared", false, &chain, true) {
            Layer::UpperFile { owner, mode, .. } => {
                assert_eq!(owner, lower.id);
                assert_eq!(mode, 0o100640);
            }
            _ => panic!("lower box entry did not win over the host backing"),
        }

        // A whiteout is terminal and masks both recorded and host layers.
        upper.set_whiteout("shared", 0);
        assert!(is_absent(resolve(upper.id, "shared", false, &chain, true)));

        // A hole skips the rest of the recorded chain and exposes the backdrop.
        upper
            .kinds
            .write()
            .unwrap()
            .insert("shared".into(), Entry::Hole);
        assert!(is_lower(resolve(upper.id, "shared", false, &chain, true)));

        // Opaque ancestors erase both recorded lower layers and the backdrop.
        upper.set_opaque("opaque", 0);
        assert!(is_absent(resolve(
            upper.id,
            "opaque/child",
            false,
            &chain,
            true
        )));

        // A rebase anchor erases recorded parents but deliberately retains the
        // live host backdrop.
        upper.kinds.write().unwrap().insert(
            "rebased".into(),
            Entry::Dir {
                mode: 0o040755,
                mtime_ns: 0,
                opaque: false,
                rebased: true,
            },
        );
        assert!(is_lower(resolve(
            upper.id,
            "rebased/child",
            false,
            &chain,
            true
        )));

        // no_host_fallback suppresses the backdrop, while the synthetic root
        // mountpoints still exist. API boxes replace tmp with their private
        // state directory at the same resolver boundary.
        upper.set_no_host_fallback(true);
        assert!(is_absent(resolve(
            upper.id,
            "host-only",
            false,
            &chain,
            true
        )));
        assert!(matches!(
            resolve(upper.id, "proc", false, &chain, false),
            Layer::UpperDir { mode: 0o0555, .. }
        ));
        match resolve(upper.id, "tmp", true, &chain, false) {
            Layer::UpperSymlink { target } => {
                assert!(target.ends_with(upper.id.to_string()));
            }
            _ => panic!("API tmp was not projected as a private symlink"),
        }

        drop(chain);
        drop(lower);
        drop(upper);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn own_layer_and_child_detection_do_not_need_a_transport() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "sarun-own-layer-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp) };
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let box_state = Arc::new(BoxState::create(9821).unwrap());
        box_state.set_dir("dir", 0o755, 0);
        box_state.ensure_file_row("dir/file", 0o100600, 0);
        let chain = [ChainLink::Box(box_state.clone())];

        assert!(chain_has_children(&chain, "dir"));
        assert!(!chain_has_children(&chain, "missing"));
        assert!(matches!(
            own_layer(&box_state, "dir/file", false),
            Layer::UpperFile { mode: 0o100600, .. }
        ));
        assert!(is_lower(own_layer(&box_state, "absent", true)));
        assert!(is_absent(own_layer(&box_state, "absent", false)));

        drop(chain);
        drop(box_state);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
