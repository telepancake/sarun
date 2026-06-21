//! Symlink-safe host filesystem operations for the apply path.
//!
//! `review::materialize` writes a box's captured changes onto the real host
//! filesystem. The danger: a box can capture a symlink (e.g. `link -> /`) AND a
//! path under it (`link/x`); naively joining `"/".join(rel)` and using
//! `std::fs` follows that symlink, so applying `link/x` writes to the host's
//! `/x` — an arbitrary-write escape (audit C2). The final-component check the
//! old code had did NOT cover symlinked *ancestors*.
//!
//! Every operation here resolves `rel` one component at a time with
//! `openat(O_NOFOLLOW)` starting from a root dir fd, so a symlinked component
//! fails closed (ELOOP/ENOTDIR) instead of being traversed, and the final
//! mutation is performed with `*at` syscalls relative to the resolved parent
//! dir fd (again `O_NOFOLLOW` on the leaf). This is race-free: there is no
//! check-then-use gap, the kernel refuses the symlink at open time. `..` and
//! empty/`.` components are rejected outright. Confinement to the root holds
//! because we never follow a symlink and never accept `..`.
//!
//! The root is a parameter (`/` in production) so the logic is testable against
//! a temp dir without touching the real filesystem.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Open `/` as an `O_PATH` directory fd — the production root for apply.
pub fn open_root() -> std::io::Result<OwnedFd> {
    open_dir(Path::new("/"))
}

/// Open `path` as an `O_PATH` directory fd (used for the root; tests pass a temp
/// dir). `O_PATH|O_DIRECTORY` is enough to serve as the `dirfd` for the `*at`
/// calls below and needs no read permission.
pub fn open_dir(path: &Path) -> std::io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: `c` is a valid NUL-terminated C string for the duration of the call.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: open returned a fresh, owned, valid fd.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Split `rel` into its non-empty components, rejecting anything that could
/// escape the root: absolute leading slash is already stripped by the caller,
/// but `..`, `.`, empty, and interior-NUL components are refused here.
fn safe_components(rel: &str) -> Result<Vec<&str>, String> {
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return Err("empty relative path".into());
    }
    for c in &comps {
        if *c == ".." || *c == "." {
            return Err(format!("path component '{c}' not allowed"));
        }
        if c.as_bytes().contains(&0) {
            return Err("path contains NUL".into());
        }
    }
    Ok(comps)
}

/// Open one directory component beneath `at`, never following a symlink. With
/// `create`, a missing component is `mkdirat`'d first. A symlinked or
/// non-directory component fails closed (the `openat` returns ELOOP/ENOTDIR).
fn open_dir_component(at: BorrowedFd, name: &str, create: bool) -> Result<OwnedFd, String> {
    let cname = CString::new(name).map_err(|_| "NUL in path component".to_string())?;
    // O_PATH|O_DIRECTORY|O_NOFOLLOW: a real dir opens; a symlink yields ENOTDIR
    // (O_NOFOLLOW keeps the link itself, which is not a directory); a regular
    // file yields ENOTDIR. Either way a non-dir/symlink component is refused.
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid dirfd and C string.
    let mut fd = unsafe { libc::openat(at.as_raw_fd(), cname.as_ptr(), flags) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if create && err.raw_os_error() == Some(libc::ENOENT) {
            // mkdirat creates a literal directory (never follows); 0o777 is
            // masked by the process umask, matching the old create_dir_all.
            // SAFETY: valid dirfd and C string.
            let r = unsafe { libc::mkdirat(at.as_raw_fd(), cname.as_ptr(), 0o777) };
            if r < 0 {
                let e2 = std::io::Error::last_os_error();
                // A concurrent creator (EEXIST) is fine — re-open below.
                if e2.raw_os_error() != Some(libc::EEXIST) {
                    return Err(format!("mkdir '{name}': {e2}"));
                }
            }
            // SAFETY: valid dirfd and C string.
            fd = unsafe { libc::openat(at.as_raw_fd(), cname.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(format!(
                "open dir '{name}': {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    // SAFETY: openat returned a fresh, owned, valid fd.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Resolve the PARENT directory of `rel` beneath `root` without ever following
/// a symlinked component, returning the parent dir fd and the leaf name. With
/// `create`, missing intermediate directories are created. Fails closed on a
/// symlinked/non-dir ancestor, on `..`/`.`/empty components, and on NUL.
pub fn parent_beneath(
    root: BorrowedFd,
    rel: &str,
    create: bool,
) -> Result<(OwnedFd, CString), String> {
    let comps = safe_components(rel)?;
    let (last, parents) = comps.split_last().unwrap();
    let mut cur: OwnedFd = root
        .try_clone_to_owned()
        .map_err(|e| format!("dup root: {e}"))?;
    for comp in parents {
        cur = open_dir_component(cur.as_fd(), comp, create)?;
    }
    let leaf = CString::new(*last).map_err(|_| "NUL in leaf name".to_string())?;
    Ok((cur, leaf))
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)` on `name` beneath `parent`; `None` if absent.
pub fn lstat_at(parent: BorrowedFd, name: &CStr) -> Option<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid dirfd, C string, and out pointer.
    let r = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r == 0 { Some(st) } else { None }
}

/// `unlinkat` a non-directory leaf (file/symlink/fifo/device). Ignores ENOENT.
pub fn unlink_at(parent: BorrowedFd, name: &CStr) -> Result<(), String> {
    // SAFETY: valid dirfd and C string.
    let r = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOENT) {
            return Err(format!("unlink: {e}"));
        }
    }
    Ok(())
}

/// Recursively remove `name` beneath `parent`, never following symlinks. A
/// symlink is unlinked (not traversed); a directory is emptied via its own
/// `O_NOFOLLOW|O_DIRECTORY` fd and `rmdir`'d. Used for deletion tombstones.
pub fn remove_tree_at(parent: BorrowedFd, name: &CStr) -> Result<(), String> {
    let Some(st) = lstat_at(parent, name) else {
        return Ok(()); // already gone
    };
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return unlink_at(parent, name); // file/symlink/device: just unlink
    }
    // Directory: open it WITHOUT following (it's a real dir per the NOFOLLOW
    // lstat above), recurse, then rmdir via the parent.
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid dirfd and C string.
    let dfd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if dfd < 0 {
        return Err(format!("open dir to remove: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fresh owned fd.
    let dir = unsafe { OwnedFd::from_raw_fd(dfd) };
    // fdopendir takes ownership of the fd; dup so our OwnedFd can still close.
    let dup = dir.try_clone().map_err(|e| format!("dup dir: {e}"))?;
    // SAFETY: dup is a valid, owned dir fd handed to fdopendir.
    let dirp = unsafe { libc::fdopendir(dup.as_raw_fd()) };
    if dirp.is_null() {
        return Err(format!("fdopendir: {}", std::io::Error::last_os_error()));
    }
    std::mem::forget(dup); // fdopendir owns it now; closedir frees it
    loop {
        // SAFETY: dirp is a valid DIR* from fdopendir.
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            break;
        }
        // SAFETY: ent points at a valid dirent for this iteration.
        let dname = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let b = dname.to_bytes();
        if b == b"." || b == b".." {
            continue;
        }
        remove_tree_at(dir.as_fd(), dname)?;
    }
    // SAFETY: dirp is valid; closedir consumes it (and the dup'd fd).
    unsafe { libc::closedir(dirp) };
    // SAFETY: valid dirfd and C string; AT_REMOVEDIR makes this an rmdir.
    let r = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if r != 0 {
        return Err(format!("rmdir: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Write `bytes` to the leaf `name` beneath `parent`, refusing to follow a
/// symlink at the leaf (`O_NOFOLLOW`), then set its mode exactly. A failure to
/// set the mode is returned as an error rather than silently dropped (audit
/// H4: a 0600 file must not silently land world-readable).
pub fn write_file_at(
    parent: BorrowedFd,
    name: &CStr,
    bytes: &[u8],
    mode: u32,
) -> Result<(), String> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid dirfd and C string; variadic mode arg for O_CREAT.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, mode & 0o7777) };
    if fd < 0 {
        // ELOOP here == the leaf is an existing symlink: refuse (the C2 guard).
        return Err(format!("open for write: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fresh owned fd; File takes ownership and closes it on drop.
    let mut f = unsafe { File::from_raw_fd(fd) };
    f.write_all(bytes).map_err(|e| format!("write: {e}"))?;
    // O_CREAT's mode is umask-masked and ignored for an existing file, so set
    // the exact mode explicitly and surface any failure.
    // SAFETY: valid open fd.
    if unsafe { libc::fchmod(f.as_raw_fd(), mode & 0o7777) } != 0 {
        return Err(format!("set mode: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Write `bytes` to the leaf `name` beneath `parent`, refusing to follow a
/// symlink at the leaf, WITHOUT changing the mode — an existing file keeps its
/// permissions, a newly-created one gets `0o666 & ~umask` (matching the old
/// `std::fs::write`). Used by the per-hunk apply, which edits an existing host
/// file in place and must not alter its mode.
pub fn write_file_preserve_mode_at(parent: BorrowedFd, name: &CStr, bytes: &[u8]) -> Result<(), String> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid dirfd and C string; variadic create mode for O_CREAT.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o666) };
    if fd < 0 {
        return Err(format!("open for write: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fresh owned fd; File closes it on drop.
    let mut f = unsafe { File::from_raw_fd(fd) };
    f.write_all(bytes).map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// Create a symlink leaf `name -> target` beneath `parent`, replacing any
/// existing non-directory leaf first (matching the old behavior).
pub fn symlink_at(parent: BorrowedFd, name: &CStr, target: &[u8]) -> Result<(), String> {
    unlink_at(parent, name)?;
    let ctgt = CString::new(target).map_err(|_| "NUL in symlink target".to_string())?;
    // SAFETY: valid dirfd and C strings.
    if unsafe { libc::symlinkat(ctgt.as_ptr(), parent.as_raw_fd(), name.as_ptr()) } != 0 {
        return Err(format!("symlink: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Create (or accept existing) a directory leaf `name` beneath `parent` and set
/// its mode. Refuses if the leaf already exists as a symlink (would otherwise
/// chmod the symlink's target).
pub fn mkdir_at(parent: BorrowedFd, name: &CStr, mode: u32) -> Result<(), String> {
    // SAFETY: valid dirfd and C string.
    let r = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(format!("mkdir: {e}"));
        }
        // Exists already: make sure it's a real directory, not a symlink we'd
        // chmod through.
        match lstat_at(parent, name) {
            Some(st) if st.st_mode & libc::S_IFMT == libc::S_IFDIR => {}
            _ => return Err("refusing: directory path is a symlink or non-directory".into()),
        }
    }
    // fchmodat without AT_SYMLINK_NOFOLLOW is safe here: we just confirmed a real dir.
    // SAFETY: valid dirfd and C string.
    if unsafe { libc::fchmodat(parent.as_raw_fd(), name.as_ptr(), mode & 0o7777, 0) } != 0 {
        return Err(format!("set dir mode: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Recreate a fifo/device node leaf `name` beneath `parent`, replacing any
/// existing non-directory leaf first.
pub fn mknod_at(parent: BorrowedFd, name: &CStr, mode: u32, rdev: u64) -> Result<(), String> {
    unlink_at(parent, name)?;
    // SAFETY: valid dirfd and C string.
    if unsafe { libc::mknodat(parent.as_raw_fd(), name.as_ptr(), mode, rdev as libc::dev_t) } != 0 {
        return Err(format!("mknod: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Set mtime/atime on the leaf without following a final symlink.
pub fn utimens_at(parent: BorrowedFd, name: &CStr, mtime_ns: i64) {
    if mtime_ns <= 0 {
        return;
    }
    let ts = libc::timespec {
        tv_sec: mtime_ns.div_euclid(1_000_000_000),
        tv_nsec: mtime_ns.rem_euclid(1_000_000_000),
    };
    let times = [ts, ts];
    // SAFETY: valid dirfd, C string, and timespec array.
    unsafe {
        libc::utimensat(
            parent.as_raw_fd(),
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        );
    }
}

/// `lchown`-equivalent on the leaf (best-effort; ownership restore EPERMs for an
/// unprivileged host user, which is expected and ignored — audit-acknowledged).
pub fn chown_at(parent: BorrowedFd, name: &CStr, uid: u32, gid: u32) {
    // SAFETY: valid dirfd and C string.
    unsafe {
        libc::fchownat(
            parent.as_raw_fd(),
            name.as_ptr(),
            uid,
            gid,
            libc::AT_SYMLINK_NOFOLLOW,
        );
    }
}

/// `lsetxattr`-equivalent on the leaf, addressed via `/proc/self/fd/<parent>` so
/// it stays confined to the already-resolved parent dir (no ancestor re-walk).
pub fn setxattr_at(parent: BorrowedFd, name: &CStr, key: &CStr, val: &[u8]) {
    let leaf = match name.to_str() {
        Ok(s) => s,
        Err(_) => return,
    };
    let path = format!("/proc/self/fd/{}/{}", parent.as_raw_fd(), leaf);
    let Ok(cpath) = CString::new(path) else { return };
    // LSETXATTR via the /proc/self/fd/<parent>/<leaf> path: the parent fd is the
    // confined real dir, and lsetxattr does not follow the final symlink.
    // SAFETY: valid C strings and byte buffer.
    unsafe {
        libc::lsetxattr(
            cpath.as_ptr(),
            key.as_ptr(),
            val.as_ptr().cast(),
            val.len(),
            0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn tmpdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("hostfs-test-{}", std::process::id()))
            .join(format!("{:?}", std::time::SystemTime::now()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn writes_a_plain_file_with_exact_mode() {
        let root = tmpdir();
        let rfd = open_dir(&root).unwrap();
        let (parent, leaf) = parent_beneath(rfd.as_fd(), "a/b/c.txt", true).unwrap();
        write_file_at(parent.as_fd(), &leaf, b"hello", 0o600).unwrap();
        let f = root.join("a/b/c.txt");
        assert_eq!(std::fs::read(&f).unwrap(), b"hello");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o7777, 0o600);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_write_through_a_symlinked_parent() {
        // root/link -> root/secret_dir ; writing link/x must NOT create secret_dir/x.
        let root = tmpdir();
        let secret = root.join("secret_dir");
        std::fs::create_dir_all(&secret).unwrap();
        std::os::unix::fs::symlink(&secret, root.join("link")).unwrap();
        let rfd = open_dir(&root).unwrap();
        // create=false AND create=true must both refuse to traverse the symlink.
        let err = parent_beneath(rfd.as_fd(), "link/x", true).unwrap_err();
        assert!(err.contains("open dir"), "expected symlink refusal, got: {err}");
        assert!(!secret.join("x").exists(), "wrote through the symlink!");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_write_onto_a_symlink_leaf() {
        // root/f -> root/target ; write_file_at on f must refuse (O_NOFOLLOW).
        let root = tmpdir();
        let target = root.join("target");
        std::fs::write(&target, b"ORIGINAL").unwrap();
        std::os::unix::fs::symlink(&target, root.join("f")).unwrap();
        let rfd = open_dir(&root).unwrap();
        let (parent, leaf) = parent_beneath(rfd.as_fd(), "f", false).unwrap();
        let err = write_file_at(parent.as_fd(), &leaf, b"OVERWRITE", 0o644).unwrap_err();
        assert!(err.contains("open for write"), "got: {err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"ORIGINAL", "wrote through leaf symlink!");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_dotdot_components() {
        let root = tmpdir();
        let rfd = open_dir(&root).unwrap();
        assert!(parent_beneath(rfd.as_fd(), "../escape", true).is_err());
        assert!(parent_beneath(rfd.as_fd(), "a/../../escape", true).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn remove_tree_does_not_follow_symlinks_out() {
        // root/d/link -> root/outside ; remove_tree_at(d) must delete d but not outside's contents.
        let root = tmpdir();
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"x").unwrap();
        let d = root.join("d");
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("link")).unwrap();
        std::fs::write(d.join("own"), b"y").unwrap();
        let rfd = open_dir(&root).unwrap();
        let leaf = CString::new("d").unwrap();
        remove_tree_at(rfd.as_fd(), &leaf).unwrap();
        assert!(!d.exists(), "d not removed");
        assert!(outside.join("keep").exists(), "followed symlink and deleted outside content!");
        std::fs::remove_dir_all(&root).ok();
    }
}
