//! The variant tree shape (ASSEMBLY.md §2/§3): how a union of git trees maps
//! onto `depot::codec` node names, and the classification of a node name back
//! to a git entry. The single-pass layer iterator (yielding git-order
//! entries with content byte-ranges) builds on these.
//!
//! Naming, chosen so the container's ONE sort order — bytewise-ascending on
//! raw node names — reproduces git's `base_name_compare` exactly:
//!
//! - a **file** at segment `name`, version (slot) `k` → `name` + `0x00` +
//!   `varint(k)`. The `0x00` is git's synthetic file terminator.
//! - a **directory** `name` → `name` + `0x2F` (`/`). The `/` is git's
//!   synthetic directory terminator.
//! - **meta children** UNDER a file-variant node: `lanes` (blob = the lane
//!   bitmap) and at most one mode tag `x` (executable) / `l` (symlink) /
//!   `m` (gitlink). A plain file / directory carries no mode tag.
//!
//! Git segment names contain neither `0x00` nor `0x2F`, so the two synthetic
//! bytes are unambiguous classifiers.

/// The directory-terminator byte git compares as if appended to a tree name.
const DIR: u8 = b'/';
/// The file-terminator byte git compares as if appended to a blob name; also
/// our variant separator.
const NUL: u8 = 0;

/// Meta child names under a variant node.
pub const LANES: &[u8] = b"lanes";
pub const TAG_EXEC: &[u8] = b"x";
pub const TAG_SYMLINK: &[u8] = b"l";
pub const TAG_GITLINK: &[u8] = b"m";

/// A git file mode, reduced to the four cases the mode tags encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `100644` — a normal file (no tag).
    File,
    /// `100755` — executable (tag `x`).
    Exec,
    /// `120000` — symlink (tag `l`).
    Symlink,
    /// `160000` — gitlink / submodule (tag `m`).
    Gitlink,
}

impl Mode {
    /// The git octal mode bytes.
    pub fn octal(self) -> &'static [u8] {
        match self {
            Mode::File => b"100644",
            Mode::Exec => b"100755",
            Mode::Symlink => b"120000",
            Mode::Gitlink => b"160000",
        }
    }
    /// The mode of a git tree entry, or `None` if it is a directory (`40000`)
    /// — directories carry no variant/tag.
    pub fn from_octal(mode: &[u8]) -> Option<Mode> {
        match mode {
            b"100644" => Some(Mode::File),
            b"100755" => Some(Mode::Exec),
            b"120000" => Some(Mode::Symlink),
            b"160000" => Some(Mode::Gitlink),
            _ => None, // 40000 (dir) or anything unexpected
        }
    }
    /// The meta-child tag for this mode, if any (a plain file has none).
    pub fn tag(self) -> Option<&'static [u8]> {
        match self {
            Mode::File => None,
            Mode::Exec => Some(TAG_EXEC),
            Mode::Symlink => Some(TAG_SYMLINK),
            Mode::Gitlink => Some(TAG_GITLINK),
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn get_varint(bytes: &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            // Reject trailing bytes: the varint must consume the whole slice.
            return (i + 1 == bytes.len()).then_some(v);
        }
    }
    None
}

/// Container node name for a file variant: `name` + `0x00` + `varint(slot)`.
pub fn file_key(name: &[u8], slot: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(name.len() + 6);
    k.extend_from_slice(name);
    k.push(NUL);
    put_varint(&mut k, slot as u64);
    k
}

/// Container node name for a directory: `name` + `0x2F`.
pub fn dir_key(name: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(name.len() + 1);
    k.extend_from_slice(name);
    k.push(DIR);
    k
}

/// What a sibling node name denotes.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    /// A directory; `.0` is the git name (terminator stripped).
    Dir(&'a [u8]),
    /// A file variant; `.0` is the git name, `.1` the slot.
    File(&'a [u8], u32),
}

/// Classify a sibling node name. Returns `None` if it is neither a
/// `…/` directory nor a `…\0<varint>` file variant (not a valid entry name).
pub fn classify(name: &[u8]) -> Option<Kind<'_>> {
    if let Some((&last, head)) = name.split_last() {
        if last == DIR {
            return Some(Kind::Dir(head));
        }
    }
    // File variant: split at the FIRST 0x00 (git names contain no 0x00).
    let nul = name.iter().position(|&b| b == NUL)?;
    let slot = get_varint(&name[nul + 1..])?;
    Some(Kind::File(&name[..nul], u32::try_from(slot).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git's `base_name_compare` (read-cache.c): bytewise, with the byte past
    /// the shorter name synthesized as `/` for a dir and `0` for a file.
    fn base_name_compare(a: &[u8], a_dir: bool, b: &[u8], b_dir: bool) -> std::cmp::Ordering {
        let n = a.len().min(b.len());
        match a[..n].cmp(&b[..n]) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
        let c1 = a.get(n).copied().unwrap_or(if a_dir { DIR } else { 0 });
        let c2 = b.get(n).copied().unwrap_or(if b_dir { DIR } else { 0 });
        c1.cmp(&c2)
    }

    /// Our container key for an entry (dir or the slot-0 file variant — the
    /// slot suffix never participates in cross-name ordering).
    fn key(name: &[u8], is_dir: bool) -> Vec<u8> {
        if is_dir {
            dir_key(name)
        } else {
            file_key(name, 0)
        }
    }

    /// Over random entry sets, sorting by our container key must yield the
    /// SAME order as git's `base_name_compare`. Includes the file-vs-dir
    /// same-name case and prefix cases (`a`/`a.txt`/`a/`).
    #[test]
    fn container_order_matches_git() {
        let mut rng = 0x9e37_79b9u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let alpha = b"ab.-_/x"; // include '.' '-' '_' which bracket 0x2F, and force collisions
        let alpha: Vec<u8> = alpha.iter().copied().filter(|&c| c != b'/').collect();
        for _ in 0..2000 {
            // A handful of (name, is_dir) entries with deliberate collisions.
            let mut entries: Vec<(Vec<u8>, bool)> = Vec::new();
            let m = 2 + (next() % 5) as usize;
            for _ in 0..m {
                let len = 1 + (next() % 4) as usize;
                let name: Vec<u8> = (0..len).map(|_| alpha[(next() as usize) % alpha.len()]).collect();
                entries.push((name, next() % 2 == 0));
            }
            // git can't hold a file and dir of the same name in ONE tree, but
            // the union can — base_name_compare still defines a total order,
            // so compare our order against it directly.
            let mut git = entries.clone();
            git.sort_by(|(a, ad), (b, bd)| {
                base_name_compare(a, *ad, b, *bd).then(ad.cmp(bd))
            });
            let mut ours = entries.clone();
            ours.sort_by(|(a, ad), (b, bd)| {
                key(a, *ad).cmp(&key(b, *bd)).then(ad.cmp(bd))
            });
            assert_eq!(ours, git, "order mismatch");
        }
    }

    #[test]
    fn keys_and_classify_roundtrip() {
        for slot in [0u32, 1, 127, 128, 300, 70000] {
            let k = file_key(b"foo.rs", slot);
            assert_eq!(classify(&k), Some(Kind::File(b"foo.rs", slot)));
        }
        let d = dir_key(b"src");
        assert_eq!(classify(&d), Some(Kind::Dir(b"src")));
        // A file whose git name would be a prefix of another still splits at
        // the first NUL only.
        let k = file_key(b"a", 5);
        assert_eq!(classify(&k), Some(Kind::File(b"a", 5)));
    }

    #[test]
    fn file_before_dir_same_name() {
        // The case bare names got wrong: file `foo` must sort before dir `foo`.
        assert!(file_key(b"foo", 0) < dir_key(b"foo"));
        // And a file variant run stays entirely below the same-named dir.
        assert!(file_key(b"foo", 999) < dir_key(b"foo"));
    }
}
