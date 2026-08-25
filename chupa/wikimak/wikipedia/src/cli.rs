//! `wikimak` command line for portable Wikipedia archives.

use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use crate::archive::MIRROR_FRAME_TARGET;

#[path = "update_lifecycle.rs"]
mod update_lifecycle;

#[cfg(test)]
thread_local! {
    static UPDATE_TEST_FAILPOINTS: std::cell::RefCell<std::collections::BTreeSet<String>> =
        std::cell::RefCell::new(std::collections::BTreeSet::new());
}

#[cfg(test)]
fn arm_update_test_failpoint(name: &str) {
    UPDATE_TEST_FAILPOINTS.with(|failpoints| {
        failpoints.borrow_mut().insert(name.to_owned());
    });
}

#[cfg(test)]
fn clear_update_test_failpoints() {
    UPDATE_TEST_FAILPOINTS.with(|failpoints| failpoints.borrow_mut().clear());
}

#[cfg(test)]
fn update_test_failpoint(name: &str) -> Result<(), String> {
    let armed = UPDATE_TEST_FAILPOINTS.with(|failpoints| failpoints.borrow_mut().remove(name));
    if armed {
        Err(format!("test failpoint fired: {name}"))
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn update_test_failpoint(_name: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
static INCREMENTAL_BACKREF_PREPARATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
static BOOTSTRAP_BACKREF_PREPARATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
enum RangeIoEvent {
    /// Emitted only after the requested compressed tail span is fully
    /// resident in memory. It is a completion boundary, not a phase-start
    /// notification.
    TailRead { offset: u64, bytes: u64 },
    /// Emitted only after the complete old physical piece is resident in
    /// memory.
    BaseRead { bytes: u64 },
    /// Emitted after the replacement stream has finished and its archive-set
    /// pieces have been flushed and synced by `ArchiveSetOutput`.
    ReplacementWrite,
    /// Emitted only after this range's receipt is durable and its preserved
    /// base segment has been atomically replaced/reclaimed.  This is the
    /// inter-range handoff boundary; observing it is stronger than inferring
    /// progress from timestamps or path names.
    RangeDurableSwap {
        slot_index: usize,
        old_installed_reclaimed: bool,
    },
}

struct MirrorBuildLock {
    _lease: crate::direct::MirrorBuildWriterCleanupLease,
}

impl MirrorBuildLock {
    fn acquire(scratch: &Path) -> Result<Self, String> {
        crate::direct::acquire_mirror_build_writer_for_cli(scratch)
            .map(|lease| Self { _lease: lease })
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    format!("{}: another mirror build is already running", scratch.display())
                } else {
                    format!("{}: {error}", scratch.display())
                }
            })
    }
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    let operator = std::env::var("SARUN_WIKIMEDIA_CONTACT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("; operator: {value}"))
        .unwrap_or_default();
    reqwest::blocking::Client::builder()
        .user_agent(format!(
            "sarun-wikimak/{} (+https://github.com/telepancake/sarun{operator})",
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(24 * 3600))
        .build()
        .map_err(|error| error.to_string())
}

/// Resolve the dump origin once, before discovery.  Production defaults to
/// Wikimedia; operators and the shipped-binary acceptance test may point the
/// same discovery/import path at a compatible HTTPS or HTTP mirror.  Exact
/// source URLs are still frozen into the durable build plan before workers
/// start, so changing this variable cannot redirect an in-progress build.
fn wikimedia_config() -> Result<wikimak_mediawiki::Config, String> {
    let Some(value) = std::env::var_os("SARUN_WIKIMEDIA_BASE_URL") else {
        return Ok(wikimak_mediawiki::Config::default());
    };
    let value = value
        .into_string()
        .map_err(|_| "SARUN_WIKIMEDIA_BASE_URL must be valid UTF-8".to_owned())?;
    wikimedia_config_from(&value)
}

fn wikimedia_config_from(value: &str) -> Result<wikimak_mediawiki::Config, String> {
    let value = value.trim();
    let parsed = reqwest::Url::parse(value)
        .map_err(|error| format!("invalid SARUN_WIKIMEDIA_BASE_URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.cannot_be_a_base()
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(
            "SARUN_WIKIMEDIA_BASE_URL must be an http(s) origin or path without credentials, query, or fragment"
                .to_owned(),
        );
    }
    Ok(wikimak_mediawiki::Config {
        base_url: value.trim_end_matches('/').to_owned(),
    })
}

fn mirror_compression() -> crate::archive::CompressionSettings {
    crate::archive::CompressionSettings {
        level: 9,
        ..crate::archive::CompressionSettings::default()
    }
}

pub fn mirror_scratch_path(archive: &Path) -> PathBuf {
    let parent = archive
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = archive
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("mirror"));
    parent.join(".wikimak-scratch").join(name)
}

fn sync_directories(directories: &[&Path]) -> Result<(), String> {
    #[cfg(unix)]
    {
        let mut synced = Vec::new();
        for directory in directories {
            if synced.iter().any(|previous| previous == directory) {
                continue;
            }
            std::fs::File::open(directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| format!("{}: {error}", directory.display()))?;
            synced.push(*directory);
        }
    }
    Ok(())
}

fn sync_parent(destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_directories(&[parent])
}

fn ensure_directory_no_symlink(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(format!("cleanup quarantine is not a real directory: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(path)
            .map_err(|error| format!("{}: {error}", path.display())),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn real_directory_exists(path: &Path, description: &str) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(format!("{} is not a real directory: {}", description, path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

const SCRATCH_CLAIM_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
enum ScratchClaimState {
    Planned,
    Claimed,
    Foreign,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct ScratchClaimInventory {
    schema: u32,
    operation: String,
    source_name: String,
    expected_kind: String,
    expected_bytes: u64,
    expected_identity: Option<String>,
    claimed_kind: Option<String>,
    claimed_bytes: Option<u64>,
    claimed_identity: Option<String>,
    state: ScratchClaimState,
}

fn scratch_entry_identity(path: &Path) -> Result<(String, u64, Option<String>), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let kind = if metadata.file_type().is_file() {
        "file"
    } else if metadata.file_type().is_dir() {
        "directory"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "other"
    }
    .to_owned();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok((
            kind,
            metadata.len(),
            Some(format!(
                "unix:{}:{}:{}:{}",
                metadata.dev(),
                metadata.ino(),
                metadata.mtime(),
                metadata.mtime_nsec()
            )),
        ));
    }
    #[cfg(not(unix))]
    Ok((kind, metadata.len(), None))
}

fn persist_scratch_claim_inventory(
    path: &Path,
    inventory: &ScratchClaimInventory,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("scratch claim has no parent: {}", path.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    serde_json::to_writer(&mut temporary, inventory)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    temporary
        .write_all(b"\n")
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| format!("{}: {}", path.display(), error.error))?;
    sync_directories(&[parent])
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

const UPDATE_HARDLINK_CLEANUP_SCHEMA: u32 = 1;
const UPDATE_HARDLINK_CLEANUP_RECEIPT: &str = "hardlink-cleanup.json";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct UpdateHardlinkCleanupEntry {
    source: String,
    target: String,
    expected_kind: String,
    expected_bytes: u64,
    expected_identity: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct UpdateHardlinkCleanupReceipt {
    schema: u32,
    update_id: String,
    base_generation_id: String,
    new_generation_id: String,
    entries: Vec<UpdateHardlinkCleanupEntry>,
}

#[derive(Clone, Debug)]
struct CommittedUpdateHardlinkCleanup {
    receipt: UpdateHardlinkCleanupReceipt,
    selected_archive: PathBuf,
}

#[derive(Clone, Copy)]
enum UpdateCleanupMode<'a> {
    Conservative,
    Committed(&'a CommittedUpdateHardlinkCleanup),
}

fn update_hardlink_cleanup_path(root: &Path) -> PathBuf {
    root.join(UPDATE_HARDLINK_CLEANUP_RECEIPT)
}

fn safe_relative_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative_path = Path::new(relative);
    if relative.is_empty() || relative_path.is_absolute() {
        return Err(format!("cleanup receipt contains an unsafe relative path {relative:?}"));
    }
    let mut path = root.to_path_buf();
    for component in relative_path.components() {
        match component {
            Component::Normal(value) => path.push(value),
            _ => {
                return Err(format!(
                    "cleanup receipt contains an unsafe relative path {relative:?}"
                ));
            }
        }
    }
    Ok(path)
}

fn relative_update_path(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("{} is outside update root {}", path.display(), root.display()))?;
    if relative.components().any(|component| {
        !matches!(component, Component::Normal(_))
    }) {
        return Err(format!("{} is not a safe update-relative path", path.display()));
    }
    Ok(relative.to_string_lossy().into_owned())
}

fn same_proven_hardlink(left: &Path, right: &Path) -> Result<bool, String> {
    let (left_kind, left_bytes, left_identity) = scratch_entry_identity(left)?;
    let (right_kind, right_bytes, right_identity) = scratch_entry_identity(right)?;
    if left_kind != "file" || right_kind != "file" || left_bytes != right_bytes {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        Ok(left_identity.is_some() && left_identity == right_identity)
    }
    #[cfg(not(unix))]
    {
        let _ = (left_identity, right_identity);
        Ok(false)
    }
}

fn make_update_hardlink_cleanup_entry(
    update_root: &Path,
    source: &Path,
    target_relative: &str,
    selected_archive: &Path,
) -> Result<UpdateHardlinkCleanupEntry, String> {
    let target_name = target_relative
        .strip_prefix("archive.swdump/")
        .unwrap_or(target_relative);
    let target = safe_relative_path(selected_archive, target_name)?;
    let (expected_kind, expected_bytes, expected_identity) = scratch_entry_identity(source)?;
    if expected_kind != "file" || !same_proven_hardlink(source, &target)? {
        return Err(format!(
            "cannot prove generated update hardlink {} -> {}",
            source.display(),
            target.display()
        ));
    }
    Ok(UpdateHardlinkCleanupEntry {
        source: relative_update_path(update_root, source)?,
        target: target_relative.to_owned(),
        expected_kind,
        expected_bytes,
        expected_identity,
    })
}

fn selected_archive_segment_names(selected_archive: &Path) -> Result<Vec<String>, String> {
    crate::archive_set::ArchiveSetReader::open(selected_archive)
        .map_err(|error| error.to_string())
        .map(|archive| archive.segments().iter().map(|segment| segment.name.clone()).collect())
}

fn build_committed_update_hardlink_cleanup(
    archive: &Path,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<CommittedUpdateHardlinkCleanup, String> {
    let installed = installed_generation_id(archive)?;
    let commit = match update_lifecycle::inspect_update(paths, installed.as_str())
        .map_err(|error| error.to_string())?
    {
        update_lifecycle::UpdateState::Committed(commit) => commit,
        state => {
            return Err(format!(
                "update cleanup requires a committed update, observed {state:?}"
            ));
        }
    };
    if commit.new_generation_id != installed.as_str() {
        return Err(format!(
            "committed update names {}, but selector names {}",
            commit.new_generation_id,
            installed.as_str()
        ));
    }
    let (selected_archive, _) = crate::installation_lifecycle::selected_generation_paths(archive)?
        .ok_or_else(|| format!("{} has no selected generation", archive.display()))?;
    let selected_names = selected_archive_segment_names(&selected_archive)?;
    let mut entries = Vec::with_capacity(selected_names.len());
    for name in &selected_names {
        entries.push(make_update_hardlink_cleanup_entry(
            &paths.root,
            &paths.base_archive().join(name),
            &format!("archive.swdump/{name}"),
            &selected_archive,
        )?);
    }
    let ranges = update_lifecycle::read_receipt::<update_lifecycle::RangePlanReceipt>(
        &paths.range_plan(),
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| format!("{} has no range plan", paths.range_plan().display()))?;
    for slot in &ranges.slots {
        let receipt = update_lifecycle::read_receipt::<update_lifecycle::RangeCandidateReceipt>(
            &paths.range_receipt(slot.index),
        )
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{} has no range receipt", paths.range_receipt(slot.index).display()))?;
        if let update_lifecycle::RangeSelection::Replaced {
            segment_id,
            name,
            ..
        } = receipt.selection
        {
            if segment_id != slot.candidate_id || !selected_names.iter().any(|selected| selected == &name) {
                return Err(format!(
                    "range {} does not identify a selected replacement",
                    slot.index
                ));
            }
            entries.push(make_update_hardlink_cleanup_entry(
                &paths.root,
                &paths.range_object(&slot.candidate_id),
                &format!("archive.swdump/{name}"),
                &selected_archive,
            )?);
        }
    }
    Ok(CommittedUpdateHardlinkCleanup {
        receipt: UpdateHardlinkCleanupReceipt {
            schema: UPDATE_HARDLINK_CLEANUP_SCHEMA,
            update_id: commit.update_id,
            base_generation_id: commit.old_generation_id,
            new_generation_id: commit.new_generation_id,
            entries,
        },
        selected_archive,
    })
}

fn validate_update_hardlink_cleanup_receipt(
    archive: &Path,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<CommittedUpdateHardlinkCleanup, String> {
    let receipt: UpdateHardlinkCleanupReceipt = read_required_json(
        &update_hardlink_cleanup_path(&paths.root),
    )?;
    if receipt.schema != UPDATE_HARDLINK_CLEANUP_SCHEMA
        || receipt.update_id.is_empty()
        || receipt.base_generation_id.is_empty()
        || receipt.new_generation_id.is_empty()
        || receipt.entries.is_empty()
    {
        return Err(format!(
            "{} is not a valid committed hardlink cleanup receipt",
            update_hardlink_cleanup_path(&paths.root).display()
        ));
    }
    let commit = update_lifecycle::read_receipt::<update_lifecycle::CommitReceipt>(
        &paths.commit_receipt(),
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| format!("{} has no commit receipt", paths.commit_receipt().display()))?;
    if commit.schema != update_lifecycle::UPDATE_SCHEMA
        || receipt.update_id != commit.update_id
        || receipt.base_generation_id != commit.old_generation_id
        || receipt.new_generation_id != commit.new_generation_id
    {
        return Err(format!(
            "{} does not match the committed update",
            update_hardlink_cleanup_path(&paths.root).display()
        ));
    }
    let installed = installed_generation_id(archive)?;
    if installed.as_str() != receipt.new_generation_id {
        return Err(format!(
            "cleanup receipt names {}, but selector names {}",
            receipt.new_generation_id,
            installed.as_str()
        ));
    }
    let (selected_archive, _) = crate::installation_lifecycle::selected_generation_paths(archive)?
        .ok_or_else(|| format!("{} has no selected generation", archive.display()))?;
    let selected_names = selected_archive_segment_names(&selected_archive)?;
    let mut sources = Vec::with_capacity(receipt.entries.len());
    for entry in &receipt.entries {
        let range_source_is_generated = entry
            .source
            .strip_prefix("ranges/objects/")
            .and_then(|name| name.strip_suffix(".swdump-part"))
            .is_some_and(is_hex_id);
        if entry.expected_kind != "file"
            || entry.source.is_empty()
            || entry.target.is_empty()
            || !(entry.source.starts_with("base/archive.swdump/")
                || range_source_is_generated)
            || !entry.target.starts_with("archive.swdump/")
            || !selected_names.iter().any(|name| {
                entry.target == format!("archive.swdump/{name}")
            })
        {
            return Err(format!(
                "{} contains an unrecognized hardlink cleanup entry",
                update_hardlink_cleanup_path(&paths.root).display()
            ));
        }
        let source = safe_relative_path(&paths.root, &entry.source)?;
        let target_name = entry
            .target
            .strip_prefix("archive.swdump/")
            .ok_or_else(|| "cleanup target is outside the selected archive".to_owned())?;
        let target = safe_relative_path(&selected_archive, target_name)?;
        if !sources.iter().all(|found: &String| found != &entry.source) {
            return Err(format!(
                "{} contains a duplicate source entry",
                update_hardlink_cleanup_path(&paths.root).display()
            ));
        }
        sources.push(entry.source.clone());
        let (target_kind, target_bytes, target_identity) = scratch_entry_identity(&target)?;
        if target_kind != entry.expected_kind
            || target_bytes != entry.expected_bytes
            || target_identity != entry.expected_identity
        {
            return Err(format!(
                "selected target {} changed since cleanup proof",
                target.display()
            ));
        }
        if let Ok((source_kind, source_bytes, source_identity)) = scratch_entry_identity(&source) {
            if source_kind != entry.expected_kind
                || source_bytes != entry.expected_bytes
                || source_identity != entry.expected_identity
            {
                // The path is retained below as ambiguous. It is not deletion
                // authority merely because its name remains in the receipt.
            }
        }
    }
    Ok(CommittedUpdateHardlinkCleanup { receipt, selected_archive })
}

fn reap_committed_update_hardlinks(
    update_root: &Path,
    cleanup: &CommittedUpdateHardlinkCleanup,
) -> Result<(), String> {
    for entry in &cleanup.receipt.entries {
        let source = safe_relative_path(update_root, &entry.source)?;
        let target_name = entry
            .target
            .strip_prefix("archive.swdump/")
            .ok_or_else(|| "cleanup target is outside the selected archive".to_owned())?;
        let target = safe_relative_path(&cleanup.selected_archive, target_name)?;
        let source_metadata = match std::fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("inspect {}: {error}", source.display())),
        };
        if !source_metadata.file_type().is_file() {
            continue;
        }
        let (kind, bytes, identity) = scratch_entry_identity(&source)?;
        if kind != entry.expected_kind
            || bytes != entry.expected_bytes
            || identity != entry.expected_identity
            || !same_proven_hardlink(&source, &target)?
        {
            continue;
        }
        std::fs::remove_file(&source)
            .map_err(|error| format!("unlink validated update hardlink {}: {error}", source.display()))?;
        if let Some(parent) = source.parent() {
            sync_directory_path(parent)?;
        }
    }
    Ok(())
}

pub fn mirror_auxiliary_paths(archive: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = vec![mirror_scratch_path(archive)];
    paths.extend(crate::installation_lifecycle::auxiliary_paths(archive)?);
    Ok(paths)
}

pub fn mirror_has_installed_generation(archive: &Path) -> Result<bool, String> {
    crate::installation_lifecycle::serving_pair(archive).map(|pair| pair.is_some())
}

fn ensure_mirror_scratch(archive: &Path) -> Result<PathBuf, String> {
    let scratch = mirror_scratch_path(archive);
    match std::fs::symlink_metadata(&scratch) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "mirror scratch is not a real directory: {}",
                scratch.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&scratch)
                .map_err(|error| format!("{}: {error}", scratch.display()))?;
            let metadata = std::fs::symlink_metadata(&scratch)
                .map_err(|error| format!("{}: {error}", scratch.display()))?;
            if !metadata.file_type().is_dir() {
                return Err(format!(
                    "mirror scratch is not a real directory: {}",
                    scratch.display()
                ));
            }
        }
        Err(error) => return Err(format!("{}: {error}", scratch.display())),
    }
    Ok(scratch)
}

fn ensure_direct_tmpdir(scratch: &Path) -> Result<PathBuf, String> {
    // Import subprocesses are destination-owned work.  Always override the
    // ambient process setting so a caller cannot accidentally route large
    // request bodies or helper temporaries to an unrelated/system volume.
    let request_tmp = scratch.join("request-tmp");
    std::fs::create_dir_all(&request_tmp)
        .map_err(|error| format!("{}: {error}", request_tmp.display()))?;
    std::env::set_var("TMPDIR", &request_tmp);
    Ok(request_tmp)
}

fn require_absolute_archive(archive: &Path) -> Result<(), String> {
    if !archive.is_absolute() {
        return Err(format!(
            "Wikipedia mirror destination must be an absolute path: {}",
            archive.display()
        ));
    }
    if archive.file_name().is_none() {
        return Err(format!(
            "Wikipedia mirror destination must name an archive: {}",
            archive.display()
        ));
    }
    Ok(())
}

fn prepare_direct_archive(archive: &Path) -> Result<PathBuf, String> {
    require_absolute_archive(archive)?;
    let scratch = ensure_mirror_scratch(archive)?;
    ensure_direct_tmpdir(&scratch)?;
    Ok(scratch)
}

const PRESERVED_MIRROR_SCRATCH_ENTRIES: &[&str] = &[
    "build.lock",
    "input-cache",
    "robots-cache",
    "request-tmp",
    "updates",
];

const OWNED_MIRROR_SCRATCH_ENTRIES: &[&str] = &[
    "archive.generation.json",
    "archive.receipt.json",
    "archive.swdump",
    "archive.swframe",
    "archive.swtitle",
    "assembly.checkpoint.json",
    "assembly.partial",
    "make",
    "manifest.swdump",
    "progress.bin",
    "stage1.mk",
    "stage2.mk",
    "target-logs",
    "title-projection.receipt.json",
    "wikimak-tool",
    "plan.json",
];

fn is_decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_owned_progress_temporary(name: &str) -> bool {
    [".progress.", ".progress-run."].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(".tmp"))
            .is_some_and(is_decimal)
    })
}

fn is_owned_source_sidecar(name: &str) -> bool {
    ["content-", "history-"].iter().any(|prefix| {
        let Some(value) = name.strip_prefix(prefix) else {
            return false;
        };
        let Some((index, suffix)) = value.split_once('.') else {
            return false;
        };
        index.len() == 6
            && is_decimal(index)
            && matches!(suffix, "swdump" | "receipt.json" | "progress")
    })
}

fn is_owned_title_projection(name: &str) -> bool {
    name.strip_prefix("title-projection-")
        .and_then(|value| value.strip_suffix(".entries"))
        .is_some_and(is_hex_id)
}

fn quarantine_scratch_entry(scratch: &Path, entry: &Path, label: &str) -> Result<(), String> {
    let quarantine = scratch.join(".sarun-quarantine");
    ensure_directory_no_symlink(&quarantine)?;
    let (expected_kind, expected_bytes, expected_identity) = scratch_entry_identity(entry)?;
    let name = entry
        .file_name()
        .ok_or_else(|| format!("cannot name quarantine entry {}", entry.display()))?
        .to_string_lossy();
    let (destination, inventory_path) = (0_u32..1024)
        .find_map(|counter| {
            let candidate_name = format!("{label}-{name}-{}-{counter}", std::process::id());
            let candidate = quarantine.join(&candidate_name);
            let inventory = quarantine.join(format!("{candidate_name}.cleanup.json"));
            match (
                std::fs::symlink_metadata(&candidate),
                std::fs::symlink_metadata(&inventory),
            ) {
                (Err(destination_error), Err(inventory_error))
                    if destination_error.kind() == std::io::ErrorKind::NotFound
                        && inventory_error.kind() == std::io::ErrorKind::NotFound =>
                {
                    Some((candidate, inventory))
                }
                _ => None,
            }
        })
        .ok_or_else(|| format!("quarantine name exhausted for {}", entry.display()))?;
    let mut inventory = ScratchClaimInventory {
        schema: SCRATCH_CLAIM_SCHEMA,
        operation: destination
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        source_name: name.into_owned(),
        expected_kind,
        expected_bytes,
        expected_identity,
        claimed_kind: None,
        claimed_bytes: None,
        claimed_identity: None,
        state: ScratchClaimState::Planned,
    };
    // The inventory is durable before the no-replace claim, so a crash at
    // either side of the namespace mutation remains inspectable and
    // resumable. Fixed names without an ownership receipt are retained.
    persist_scratch_claim_inventory(&inventory_path, &inventory)?;
    if let Err(error) = crate::instance::rename_without_replacing(entry, &destination) {
        return Err(format!(
            "cannot quarantine unowned scratch entry {} -> {}: {error}",
            entry.display(),
            destination.display()
        ));
    }
    let (claimed_kind, claimed_bytes, claimed_identity) = scratch_entry_identity(&destination)?;
    inventory.claimed_kind = Some(claimed_kind.clone());
    inventory.claimed_bytes = Some(claimed_bytes);
    inventory.claimed_identity = claimed_identity.clone();
    inventory.state = if claimed_kind == inventory.expected_kind
        && claimed_bytes == inventory.expected_bytes
        && claimed_identity == inventory.expected_identity
    {
        ScratchClaimState::Claimed
    } else {
        ScratchClaimState::Foreign
    };
    persist_scratch_claim_inventory(&inventory_path, &inventory)?;
    let source_parent = entry
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let destination_parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // The quarantine directory may have been created above, while rename and
    // the claim receipt change both namespace entries. Sync all affected
    // directories, with duplicate paths collapsed by sync_directories.
    sync_directories(&[scratch, source_parent, destination_parent])
}

fn retire_nested_owned_entry(directory: &Path, entry: &Path, label: &str) -> Result<(), String> {
    std::fs::symlink_metadata(entry)
        .map_err(|error| format!("{}: {error}", entry.display()))?;
    let update_scratch = directory
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "updates"))
        .and_then(Path::parent);
    let scratch = update_scratch
        .or_else(|| directory.parent())
        .ok_or_else(|| format!("cannot identify scratch owner for {}", entry.display()))?;
    quarantine_scratch_entry(scratch, entry, label)
}

fn retire_complete_archive_set(scratch: &Path, archive: &Path) -> Result<(), String> {
    // The archive receipt currently records structural segment extents but
    // not per-segment immutable identities. A valid-looking archive is
    // therefore not enough authority to unlink its children. Claim the
    // complete namespace atomically and retain it for inspection/recovery.
    quarantine_scratch_entry(scratch, archive, "archive-owned")
}

fn clear_owned_nodes(
    scratch: &Path,
    plan: Option<&crate::direct::DirectBuildPlan>,
) -> Result<(), String> {
    let nodes = scratch.join("nodes");
    match std::fs::symlink_metadata(&nodes) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", nodes.display())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return quarantine_scratch_entry(scratch, &nodes, "nodes");
        }
        Ok(_) => {}
    }
    let Some(plan) = plan else {
        // Without a valid plan there is no authority to identify a node.
        // Quarantine the namespace rather than leaving stale node names in
        // the active build root or destroying their potentially useful data.
        return quarantine_scratch_entry(scratch, &nodes, "nodes");
    };
    let entries = std::fs::read_dir(&nodes)
        .map_err(|error| format!("{}: {error}", nodes.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", nodes.display()))?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let mut owned = None;
        for (kind, count) in [
            (
                crate::build_lifecycle::TargetKind::Content,
                plan.content_target_count(),
            ),
            (
                crate::build_lifecycle::TargetKind::History,
                plan.history_files.len(),
            ),
        ] {
            for index in 0..count {
                let Some(target) = plan.target_name(kind.as_str(), index) else {
                    continue;
                };
                if name == format!("{target}.done") {
                    owned = Some((kind, index, true));
                } else if name == format!("{target}.partial")
                    || (name.starts_with(&format!(".{target}.")) && name.ends_with(".partial"))
                {
                    owned = Some((kind, index, false));
                }
            }
        }
        match owned {
            Some((kind, index, true))
                if crate::build_lifecycle::validate_ready_target_for_cleanup(
                    scratch, plan, kind, index,
                )
                .is_ok() =>
            {
                crate::direct::retire_validated_target_directory(scratch, plan, kind, index)
                    .map_err(|error| error.to_string())?;
            }
            _ => quarantine_scratch_entry(scratch, &entry.path(), "node")?,
        }
    }
    match std::fs::remove_dir(&nodes) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", nodes.display())),
    }
    Ok(())
}

/// Remove only the fixed and plan-derived full-build artifacts owned by
/// wikimak.  `input-cache`, request/robots state, update roots, and unknown
/// entries are deliberately outside this allowlist and survive every call.
fn clear_owned_mirror_scratch(
    scratch: &Path,
    destination: Option<&Path>,
) -> Result<(), String> {
    if !real_directory_exists(scratch, "mirror scratch")? {
        return Ok(());
    }
    // Capture authority before deleting any fixed artifact.  Directory entry
    // order is unspecified; reading plan.json from inside node cleanup would
    // otherwise make cleanup depend on whether the plan happened to be seen
    // before or after nodes/.
    let plan = crate::direct::read_direct_build_plan(&scratch.join("plan.json")).ok();
    let entries = std::fs::read_dir(scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    for entry in entries {
        let name = entry.file_name();
        if PRESERVED_MIRROR_SCRATCH_ENTRIES
            .iter()
            .any(|preserved| name == *preserved)
        {
            continue;
        }
        if name == "nodes" {
            clear_owned_nodes(scratch, plan.as_ref())?;
            continue;
        }
        let name_text = name.to_string_lossy();
        let owned = OWNED_MIRROR_SCRATCH_ENTRIES
            .iter()
            .any(|owned| name == *owned)
            || is_owned_progress_temporary(&name_text)
            || is_owned_source_sidecar(&name_text)
            || is_owned_title_projection(&name_text);
        if owned {
            if destination.is_some_and(|destination| {
                crate::installation_lifecycle::candidate_cleanup_owns_path(
                    destination,
                    &entry.path(),
                )
                .unwrap_or_else(|error| {
                    eprintln!(
                        "cannot establish candidate cleanup ownership for {}: {error}",
                        entry.path().display()
                    );
                    true
                })
            }) {
                continue;
            }
            if name == "archive.swdump" {
                retire_complete_archive_set(scratch, &entry.path())?;
            } else {
                // A name allowlist is not immutable ownership evidence. Move
                // the candidate atomically and retain it unless a durable
                // file identity receipt authorizes a later unlink.
                quarantine_scratch_entry(scratch, &entry.path(), "owned-scratch")?;
            }
        }
    }
    sync_parent(&scratch.join("build.lock"))
}

fn clear_owned_children(
    directory: &Path,
    mut is_owned: impl FnMut(&str) -> bool,
) -> Result<(), String> {
    match std::fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", directory.display())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            let scratch = directory
                .ancestors()
                .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "updates"))
                .and_then(Path::parent)
                .or_else(|| directory.parent())
                .ok_or_else(|| format!("cannot identify scratch owner for {}", directory.display()))?;
            return quarantine_scratch_entry(scratch, directory, "owned-directory");
        }
        Ok(_) => {}
    }
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_owned(&name) {
            retire_nested_owned_entry(directory, &entry.path(), "owned-child")?;
        }
    }
    match std::fs::remove_dir(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", directory.display())),
    }
    Ok(())
}

fn is_hex_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_date_directory(name: &str, prefix: &str) -> bool {
    let Some(date) = name.strip_prefix(prefix) else {
        return false;
    };
    date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn clear_owned_update_work(work: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(work) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", work.display())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return retire_nested_owned_entry(
                work.parent().unwrap_or_else(|| Path::new(".")),
                work,
                "tail-merge-work",
            );
        }
        Ok(_) => {}
    }
    let entries = std::fs::read_dir(work)
        .map_err(|error| format!("{}: {error}", work.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", work.display()))?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "update-manifest.swdump" || is_owned_source_sidecar(&name) {
            retire_nested_owned_entry(work, &entry.path(), "owned-work-child")?;
        } else if is_date_directory(&name, "incremental-") {
            clear_owned_children(&entry.path(), |child| is_owned_source_sidecar(child))?;
        } else if name == "update-tail-merge-work" {
            if crate::direct::clear_update_tail_merge_workspace(&entry.path()).is_err() {
                retire_nested_owned_entry(work, &entry.path(), "tail-merge-work")?;
            }
        }
    }
    match std::fs::remove_dir(work) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", work.display())),
    }
    Ok(())
}

fn clear_owned_update_tail(root: &Path) -> Result<(), String> {
    let tail = root.join("tail");
    match std::fs::symlink_metadata(&tail) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", tail.display())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return retire_nested_owned_entry(root, &tail, "owned-tail");
        }
        Ok(_) => {}
    }
    let entries = std::fs::read_dir(&tail)
        .map_err(|error| format!("{}: {error}", tail.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", tail.display()))?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(
            name.as_str(),
            "records.swdump" | "receipt.json" | "frames.swframe"
        ) {
            retire_nested_owned_entry(&tail, &entry.path(), "owned-tail-child")?;
        } else if name == "work" {
            clear_owned_update_work(&entry.path())?;
        }
    }
    match std::fs::remove_dir(&tail) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", tail.display())),
    }
    Ok(())
}

fn update_scratch_for_root(root: &Path) -> Result<&Path, String> {
    root.parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "updates"))
        .and_then(Path::parent)
        .ok_or_else(|| format!("cannot identify scratch owner for update {}", root.display()))
}

fn committed_cleanup_entry_for_source<'a>(
    update_root: &Path,
    path: &Path,
    cleanup: &'a CommittedUpdateHardlinkCleanup,
) -> Result<Option<&'a UpdateHardlinkCleanupEntry>, String> {
    let relative = relative_update_path(update_root, path)?;
    Ok(cleanup
        .receipt
        .entries
        .iter()
        .find(|entry| entry.source == relative))
}

fn remove_or_quarantine_committed_hardlink(
    update_root: &Path,
    path: &Path,
    cleanup: &CommittedUpdateHardlinkCleanup,
    label: &str,
) -> Result<(), String> {
    let Some(entry) = committed_cleanup_entry_for_source(update_root, path, cleanup)? else {
        return quarantine_scratch_entry(update_scratch_for_root(update_root)?, path, label);
    };
    let target_name = entry
        .target
        .strip_prefix("archive.swdump/")
        .ok_or_else(|| "cleanup target is outside the selected archive".to_owned())?;
    let target = safe_relative_path(&cleanup.selected_archive, target_name)?;
    let exact = scratch_entry_identity(path).ok().is_some_and(
        |(kind, bytes, identity)| {
            kind == entry.expected_kind
                && bytes == entry.expected_bytes
                && identity == entry.expected_identity
                && same_proven_hardlink(path, &target).unwrap_or(false)
        },
    );
    if exact {
        std::fs::remove_file(path)
            .map_err(|error| format!("unlink validated update hardlink {}: {error}", path.display()))?;
        if let Some(parent) = path.parent() {
            sync_directory_path(parent)?;
        }
    } else {
        quarantine_scratch_entry(update_scratch_for_root(update_root)?, path, label)?;
    }
    Ok(())
}

fn clear_committed_base_archive(
    update_root: &Path,
    archive: &Path,
    cleanup: &CommittedUpdateHardlinkCleanup,
) -> Result<(), String> {
    match std::fs::symlink_metadata(archive) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", archive.display())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return quarantine_scratch_entry(
                update_scratch_for_root(update_root)?,
                archive,
                "owned-child",
            );
        }
        Ok(_) => {}
    }
    let entries = std::fs::read_dir(archive)
        .map_err(|error| format!("{}: {error}", archive.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", archive.display()))?;
    for entry in entries {
        let path = entry.path();
        let relative = relative_update_path(update_root, &path)?;
        if cleanup
            .receipt
            .entries
            .iter()
            .any(|claimed| claimed.source == relative)
        {
            remove_or_quarantine_committed_hardlink(update_root, &path, cleanup, "owned-child")?;
        }
        // A foreign or unrecognized child is deliberately left in place. The
        // enclosing update residue will be retained later without treating
        // its name as deletion authority.
    }
    match std::fs::remove_dir(archive) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", archive.display())),
    }
    Ok(())
}

fn clear_committed_range_objects(
    update_root: &Path,
    objects: &Path,
    cleanup: &CommittedUpdateHardlinkCleanup,
) -> Result<(), String> {
    match std::fs::symlink_metadata(objects) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", objects.display())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return quarantine_scratch_entry(
                update_scratch_for_root(update_root)?,
                objects,
                "owned-directory",
            );
        }
        Ok(_) => {}
    }
    let entries = std::fs::read_dir(objects)
        .map_err(|error| format!("{}: {error}", objects.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", objects.display()))?;
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = relative_update_path(update_root, &path)?;
        if cleanup
            .receipt
            .entries
            .iter()
            .any(|claimed| claimed.source == relative)
        {
            remove_or_quarantine_committed_hardlink(update_root, &path, cleanup, "owned-child")?;
        } else if name
            .strip_suffix(".swdump-part")
            .is_some_and(is_hex_id)
        {
            quarantine_scratch_entry(update_scratch_for_root(update_root)?, &path, "owned-child")?;
        }
    }
    match std::fs::remove_dir(objects) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", objects.display())),
    }
    Ok(())
}

fn clear_owned_update_base(
    root: &Path,
    mode: UpdateCleanupMode<'_>,
) -> Result<(), String> {
    if let UpdateCleanupMode::Committed(cleanup) = mode {
        let base = root.join("base");
        match std::fs::symlink_metadata(&base) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("{}: {error}", base.display())),
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return quarantine_scratch_entry(update_scratch_for_root(root)?, &base, "owned-base");
            }
            Ok(_) => {}
        }
        clear_committed_base_archive(root, &base.join("archive.swdump"), cleanup)?;
        for name in ["archive.swtitle", "receipt.json"] {
            let path = base.join(name);
            if std::fs::symlink_metadata(&path).is_ok() {
                quarantine_scratch_entry(update_scratch_for_root(root)?, &path, "owned-child")?;
            }
        }
        match std::fs::remove_dir(&base) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", base.display())),
        }
        return Ok(());
    }
    clear_owned_children(&root.join("base"), |name| {
        matches!(name, "archive.swdump" | "archive.swtitle" | "receipt.json")
    })
}

fn clear_owned_update_ranges(
    root: &Path,
    mode: UpdateCleanupMode<'_>,
) -> Result<(), String> {
    let ranges = root.join("ranges");
    match std::fs::symlink_metadata(&ranges) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", ranges.display())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return retire_nested_owned_entry(root, &ranges, "owned-ranges");
        }
        Ok(_) => {}
    }
    let plan = ranges.join("plan.json");
    if std::fs::symlink_metadata(&plan).is_ok() {
        let scratch = ranges
            .ancestors()
            .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "updates"))
            .and_then(Path::parent)
            .ok_or_else(|| format!("cannot identify scratch owner for {}", plan.display()))?;
        quarantine_scratch_entry(scratch, &plan, "owned-range-plan")?;
    }
    for entry in
        std::fs::read_dir(&ranges).map_err(|error| format!("{}: {error}", ranges.display()))?
    {
        let entry = entry.map_err(|error| format!("{}: {error}", ranges.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.strip_prefix(".building-").is_some_and(is_hex_id) {
            retire_nested_owned_entry(&ranges, &entry.path(), "range-building")?;
        }
    }
    for (directory, suffixes) in [
        ("receipts", &[".json"][..]),
        ("objects", &[".swdump-part"][..]),
        ("projections", &[".swdump", ".swdump-building"][..]),
        ("frame-directories", &[".swframe"][..]),
        ("base-frame-directories", &[".swframe"][..]),
    ] {
        let path = ranges.join(directory);
        if directory == "objects" {
            if let UpdateCleanupMode::Committed(cleanup) = mode {
                clear_committed_range_objects(root, &path, cleanup)?;
                continue;
            }
        }
        clear_owned_children(&path, |name| {
            suffixes
                .iter()
                .any(|suffix| name.strip_suffix(suffix).is_some_and(is_hex_id))
        })?;
    }
    match std::fs::remove_dir(&ranges) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", ranges.display())),
    }
    Ok(())
}

fn clear_owned_update_candidate(root: &Path) -> Result<(), String> {
    clear_owned_children(&root.join("candidate"), |name| {
        matches!(
            name,
            "archive.swdump"
                | "archive.swtitle"
                | "inventory.json"
                | "generation.json"
                | "title-projection-work"
                | ".archive-building"
        )
    })
}

/// Clear one update only through its known lifecycle namespaces.  The update
/// root is identified by the durable active selector, but unknown siblings
/// and unknown children remain untouched.  In particular, this never means
/// `remove_dir_all(root)`.
fn clear_owned_update_root(
    root: &Path,
    mode: UpdateCleanupMode<'_>,
) -> Result<(), String> {
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", root.display())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            let scratch = root
                .parent()
                .filter(|parent| parent.file_name().is_some_and(|name| name == "updates"))
                .and_then(Path::parent)
                .ok_or_else(|| format!("cannot identify scratch owner for {}", root.display()))?;
            return quarantine_scratch_entry(scratch, root, "update-root");
        }
        Ok(_) => {}
    }
    if let UpdateCleanupMode::Committed(cleanup) = mode {
        reap_committed_update_hardlinks(root, cleanup)?;
    }
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("{}: {error}", root.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", root.display()))?;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        match name.as_str() {
            "plan.json" | "source-plan.json" | "commit.json" => {
                retire_nested_owned_entry(root, &entry.path(), "owned-update-receipt")?;
            }
            UPDATE_HARDLINK_CLEANUP_RECEIPT => {}
            "tail" => clear_owned_update_tail(root)?,
            "base" => clear_owned_update_base(root, mode)?,
            "ranges" => clear_owned_update_ranges(root, mode)?,
            "candidate" => clear_owned_update_candidate(root)?,
            _ => {}
        }
    }
    if let UpdateCleanupMode::Committed(cleanup) = mode {
        let receipt_path = update_hardlink_cleanup_path(root);
        match std::fs::symlink_metadata(&receipt_path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                let found: UpdateHardlinkCleanupReceipt = read_required_json(&receipt_path)?;
                if found != cleanup.receipt {
                    return Err(format!(
                        "{} changed while committed hardlink cleanup was running",
                        receipt_path.display()
                    ));
                }
                std::fs::remove_file(&receipt_path)
                    .map_err(|error| format!("remove {}: {error}", receipt_path.display()))?;
                sync_directory_path(root)?;
            }
            Ok(_) => {
                return Err(format!(
                    "{} is not a regular cleanup receipt",
                    receipt_path.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect {}: {error}", receipt_path.display())),
        }
    }
    match std::fs::remove_dir(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            let scratch = root
                .parent()
                .filter(|parent| parent.file_name().is_some_and(|name| name == "updates"))
                .and_then(Path::parent)
                .ok_or_else(|| {
                    format!(
                        "cannot identify scratch owner for residual update {}",
                        root.display()
                    )
                })?;
            quarantine_scratch_entry(scratch, root, "update-residue")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", root.display())),
    }
    Ok(())
}

fn clear_owned_update_selector(scratch: &Path) -> Result<(), String> {
    let selector = update_selector_path(scratch);
    match std::fs::symlink_metadata(&selector) {
        Ok(_) => quarantine_scratch_entry(scratch, &selector, "update-selector"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", selector.display())),
    }
}

/// Abandon a malformed/foreign construction tree while holding its
/// destination-local build lock.  The lifecycle inspector decides whether
/// state is disposable; cleanup removes only the explicit full-build
/// allowlist and never touches the selected installed generation.
fn abandon_invalid_build(
    scratch: &Path,
    destination: &Path,
    state: &crate::build_lifecycle::InvalidBuildState,
) -> Result<(), String> {
    crate::build_lifecycle::transition_invalid_build(
        scratch,
        state,
        crate::build_lifecycle::InvalidBuildEvent::AbandonInvalidScratch,
    )
    .map_err(|error| error.to_string())?;
    eprintln!(
        "discarding invalid temporary build state at {}; installed generation preserved",
        scratch.display()
    );
    clear_owned_mirror_scratch(scratch, Some(destination))
}

fn inspect_build_for_start(
    scratch: &Path,
    destination: &Path,
) -> Result<crate::build_lifecycle::BuildState, String> {
    match crate::build_lifecycle::inspect_build(scratch, None) {
        Ok(state) => Ok(state),
        Err(error) => {
            let original = error.to_string();
            abandon_invalid_build(scratch, destination, &error)
                .map_err(|reset_error| format!("{original}; {reset_error}"))?;
            crate::build_lifecycle::inspect_build(scratch, None)
                .map_err(|reset_error| reset_error.to_string())
        }
    }
}

fn persist_json(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    serde_json::to_writer(&mut temporary, value)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    use std::io::Write;
    temporary
        .write_all(b"\n")
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| format!("{}: {}", path.display(), error.error))?;
    sync_parent(path)
}

fn persist_text(path: &Path, text: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    use std::io::Write;
    temporary
        .write_all(text.as_bytes())
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| format!("{}: {}", path.display(), error.error))?;
    sync_parent(path)
}

fn executable_is_standalone_wikimak(executable: &Path) -> bool {
    executable
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "wikimak")
}

fn prepare_build_tools(scratch: &Path) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let tool = scratch.join("wikimak-tool");
    if std::fs::symlink_metadata(&tool).is_ok() {
        std::fs::remove_file(&tool)
            .map_err(|error| format!("{}: {error}", tool.display()))?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&executable, &tool)
        .map_err(|error| format!("{}: {error}", tool.display()))?;
    if !executable_is_standalone_wikimak(&executable) {
        let make = scratch.join("make");
        if std::fs::symlink_metadata(&make).is_ok() {
            std::fs::remove_file(&make)
                .map_err(|error| format!("{}: {error}", make.display()))?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(executable, &make)
            .map_err(|error| format!("{}: {error}", make.display()))?;
    }
    Ok(())
}

fn build_tool_command() -> Result<&'static str, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Ok(if executable_is_standalone_wikimak(&executable) {
        "./wikimak-tool"
    } else {
        // The engine's brush shell exposes wikimak as an in-process builtin.
        // Keeping build-node/stage assembly in that same process gives Kati,
        // brush provenance, cancellation, and the Wikimedia gate one owner;
        // standalone wikimak binaries retain the small helper executable.
        "wikimak"
    })
}

fn recursive_make_command() -> Result<&'static str, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Ok(if executable_is_standalone_wikimak(&executable) {
        "$(MAKE)"
    } else {
        "./make"
    })
}

fn make_program() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Ok(if executable_is_standalone_wikimak(&executable) {
        PathBuf::from("make")
    } else {
        PathBuf::from("./make")
    })
}

fn build_node_targets(plan: &crate::direct::DirectBuildPlan) -> Vec<String> {
    (0..plan.content_target_count())
        .map(|index| {
            format!(
                "nodes/{}.done/receipt.json",
                plan.target_name("content", index)
                    .expect("content target index came from the plan")
            )
        })
        .chain(
            (0..plan.history_files.len())
                .map(|index| format!("nodes/history-{index:06}.done/receipt.json")),
        )
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StageOneGeometry {
    /// Maximum number of independent build-node recipes admitted by make.
    make_jobs: usize,
    /// Bzip2 decoder workers provisioned in each admitted recipe.
    bz2_workers: usize,
    /// Maximum number of those workers that may decode concurrently in this
    /// process.
    active_decode_budget: usize,
}

/// Allocate the stage-one CPU budget between independent target pipelines and
/// the bounded bzip2 pool inside each pipeline.
///
/// For `C >= 2` and `T > 0`, the cost model is `N = min(T, floor(C / 2))`
/// target recipes, where `C` is the configured Sarun CPU budget. Recipes run
/// by Sarun's embedded Kati share one process, so each reader provisions the
/// full `D = C - N` decode pool while a process-wide limiter admits at most
/// `D` simultaneous block decodes. An input-starved recipe therefore lends
/// idle decoder capacity to another recipe without allowing parser owners plus
/// active decoders to exceed `C`.
///
/// A standalone wikimak uses separate recipe processes and cannot share that
/// in-memory limiter. It retains the static per-recipe share
/// `B = floor(C / N) - 1`, preserving `N * (1 + B) <= C`. A one-CPU process
/// has an explicit minimum of one recipe and one decoder because both are
/// needed for progress. A zero-target makefile still receives `-j1` but has no
/// recipe. HTTP admission remains a separate, destination-wide policy.
fn stage_one_geometry(
    target_count: usize,
    cpu_budget: usize,
    shared_process: bool,
) -> StageOneGeometry {
    let cpu_budget = cpu_budget.max(1);
    let target_capacity = (cpu_budget / 2).max(1);
    let active_targets = target_count.min(target_capacity);
    let make_jobs = active_targets.max(1);
    let static_share = if active_targets == 0 {
        1
    } else if cpu_budget == 1 {
        1
    } else {
        (cpu_budget / active_targets).saturating_sub(1).max(1)
    };
    let shared_decode_budget = if active_targets == 0 || cpu_budget == 1 {
        1
    } else {
        cpu_budget.saturating_sub(active_targets).max(1)
    };
    let (bz2_workers, active_decode_budget) = if shared_process {
        (shared_decode_budget, shared_decode_budget)
    } else {
        (static_share, static_share)
    };
    StageOneGeometry {
        make_jobs,
        bz2_workers,
        active_decode_budget,
    }
}

fn write_stage_one_makefile(
    scratch: &Path,
    plan: &crate::direct::DirectBuildPlan,
) -> Result<(), String> {
    let tool = build_tool_command()?;
    let make = recursive_make_command()?;
    let cores = crate::direct::processing_parallelism();
    let geometry = stage_one_geometry(
        plan.target_count(),
        cores,
        !executable_is_standalone_wikimak(
            &std::env::current_exe().map_err(|error| error.to_string())?,
        ),
    );
    let targets = build_node_targets(plan);
    let mut makefile = String::from(".PHONY: all\n");
    makefile.push_str(&format!("all: stage2.mk\n\t@{make} -f stage2.mk -j1\n\n"));
    makefile.push_str("stage2.mk:");
    for target in &targets {
        makefile.push(' ');
        makefile.push_str(target);
    }
    makefile.push_str(&format!(
        "\n\t@{tool} build-stage2 . plan.json\n\n"
    ));
    for index in 0..plan.content_target_count() {
        let target = plan
            .target_name("content", index)
            .expect("content target index came from the plan");
        makefile.push_str(&format!(
            "nodes/{target}.done/receipt.json:\n\
             \t@{tool} build-node . plan.json content {index} {} {}\n\n",
            geometry.bz2_workers,
            geometry.active_decode_budget,
        ));
    }
    for index in 0..plan.history_files.len() {
        makefile.push_str(&format!(
            "nodes/history-{index:06}.done/receipt.json:\n\
             \t@{tool} build-node . plan.json history {index} {} {}\n\n",
            geometry.bz2_workers,
            geometry.active_decode_budget,
        ));
    }
    persist_text(&scratch.join("stage1.mk"), &makefile)
}

fn write_stage_two_makefile(
    scratch: &Path,
    plan: &crate::direct::DirectBuildPlan,
) -> Result<(), String> {
    let tool = build_tool_command()?;
    let mut makefile =
        String::from(".PHONY: all\nall: archive.generation.json\n\narchive.generation.json:");
    for target in build_node_targets(plan) {
        makefile.push(' ');
        makefile.push_str(&target);
    }
    makefile.push_str(&format!(
        "\n\t@{tool} build-assemble . plan.json\n"
    ));
    persist_text(&scratch.join("stage2.mk"), &makefile)
}

fn run_build_make(
    scratch: &Path,
    plan: &crate::direct::DirectBuildPlan,
) -> Result<(), String> {
    let log_directory = std::fs::canonicalize(scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?
        .join("target-logs");
    // The build is deliberately destination-local.  Besides the files we
    // create explicitly below, make and any recipe subprocesses may use the
    // platform's default temporary directory.  Point that directory at the
    // same external scratch tree so a tool added later cannot put an
    // unbounded intermediate on the system volume.
    let jobs = stage_one_geometry(
        plan.target_count(),
        crate::direct::processing_parallelism(),
        !executable_is_standalone_wikimak(
            &std::env::current_exe().map_err(|error| error.to_string())?,
        ),
    )
    .make_jobs;
    let tmpdir = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| scratch.to_path_buf());
    let status = std::process::Command::new(make_program()?)
        .current_dir(scratch)
        .env("BUMBA_TARGET_LOG_DIR", log_directory)
        .env("SARUN_WIKIMEDIA_ROBOTS_CACHE", scratch.join("robots-cache"))
        .env("TMPDIR", tmpdir)
        .args(["-f", "stage1.mk", &format!("-j{jobs}")])
        .status()
        .map_err(|error| format!("cannot start resumable build: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "resumable build stopped with {}",
            status
                .code()
                .map_or_else(|| "a signal".to_owned(), |code| format!("exit {code}"))
        ))
    }
}

fn install_built_archive(
    archive: PathBuf,
    destination: &Path,
) -> Result<crate::installation_lifecycle::InstallOutcome, String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;

    let projected = archive.with_extension("swtitle");
    if !projected.exists() {
        return Err("ready build generation has no projected title index".into());
    }
    eprintln!("using title history index produced during final merge");
    let index =
        crate::title_index::TitleIndex::open(&projected).map_err(|error| error.to_string())?;
    let title_entries = index.entry_count() as u64;

    eprintln!("installing completed archive and title index");
    let outcome =
        crate::installation_lifecycle::install(archive, projected, destination)?;
    if let Err(error) = schedule_backrefs_task(destination) {
        eprintln!(
            "text generation is installed; optional backref task could not be queued: {error}"
        );
    } else {
        eprintln!(
            "text generation is installed; optional category expansion is a resumable full-scan task: wikimak backrefs-task {}",
            destination.display()
        );
    }
    if outcome.cleanup_pending {
        if outcome.candidate_cleanup_pending {
            eprintln!(
                "installed generation is live; redundant candidate-link cleanup remains pending"
            );
        } else {
            eprintln!(
                "installed generation is live; previous-generation cleanup remains pending"
            );
        }
    }
    eprintln!("{title_entries} title intervals");
    Ok(outcome)
}

fn backrefs_path(destination: &Path) -> PathBuf {
    destination.with_extension("swrefs")
}

const BACKREFS_TASK_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct BackrefsTask {
    schema: u32,
    generation_id: String,
}

fn backrefs_task_path(destination: &Path) -> PathBuf {
    mirror_scratch_path(destination).join("backrefs.task.json")
}

fn pending_backrefs_path(destination: &Path, generation_id: &str) -> PathBuf {
    mirror_scratch_path(destination)
        .join(format!("backrefs-{generation_id}.swrefs.pending"))
}

fn is_backrefs_generation_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_backrefs_task(task: &BackrefsTask, generation_id: &str, path: &Path) -> Result<(), String> {
    if task.schema != BACKREFS_TASK_SCHEMA
        || !is_backrefs_generation_id(&task.generation_id)
        || task.generation_id != generation_id
    {
        return Err(format!(
            "{} is not a task for selected generation {}",
            path.display(),
            generation_id
        ));
    }
    Ok(())
}

fn read_backrefs_task(path: &Path) -> Result<BackrefsTask, String> {
    if !regular_file_exists(path)? {
        return Err(format!("backref task is missing: {}", path.display()));
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?)
        .map_err(|error| format!("decode {}: {error}", path.display()))
}

fn remove_published_backrefs_task(destination: &Path) -> Result<(), String> {
    let task_path = backrefs_task_path(destination);
    let scratch = mirror_scratch_path(destination);
    if !task_path.starts_with(&scratch) {
        return Err(format!(
            "backref task path escaped destination scratch: {}",
            task_path.display()
        ));
    }
    if !regular_file_exists(&task_path)? {
        return Ok(());
    }
    let task = read_backrefs_task(&task_path)?;
    if task.schema != BACKREFS_TASK_SCHEMA || !is_backrefs_generation_id(&task.generation_id) {
        return Err(format!(
            "refusing to remove unrecognized backref task {}",
            task_path.display()
        ));
    }
    std::fs::remove_file(&task_path)
        .map_err(|error| format!("remove obsolete backref task {}: {error}", task_path.display()))?;
    sync_parent(&task_path)
}

fn persist_json_without_replacing(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    serde_json::to_writer(&mut temporary, value)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    use std::io::Write;
    temporary
        .write_all(b"\n")
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    crate::instance::rename_without_replacing(temporary.path(), path).map_err(|error| {
        format!("publish {} without replacing an existing task: {error}", path.display())
    })?;
    sync_parent(path)
}

/// Record the expensive relation build after text publication. This is the
/// lifecycle commit for an optional capability; it does not scan the archive.
fn schedule_backrefs_task(destination: &Path) -> Result<(), String> {
    let Some((archive, title_index)) =
        crate::installation_lifecycle::selected_generation_paths(destination)?
    else {
        return Ok(());
    };
    let generation_id = archive
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("selected generation has no usable name: {}", archive.display()))?
        .to_owned();
    let output = backrefs_path(destination);
    let scratch = ensure_mirror_scratch(destination)?;
    let task_path = backrefs_task_path(destination);
    let task = BackrefsTask {
        schema: BACKREFS_TASK_SCHEMA,
        generation_id: generation_id.clone(),
    };
    if regular_file_exists(&output)?
        && crate::backrefs::BackrefIndex::open_for_title_index(&output, &title_index).is_ok()
    {
        return Ok(());
    }
    match std::fs::symlink_metadata(&task_path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let existing = read_backrefs_task(&task_path)?;
            if existing.schema != BACKREFS_TASK_SCHEMA
                || !is_backrefs_generation_id(&existing.generation_id)
            {
                return Err(format!(
                    "refusing to replace unrecognized backref task {}",
                    task_path.display()
                ));
            }
            // A task is an owned, small receipt. Replacing a validated old
            // generation receipt publishes the current generation obligation.
            if existing != task {
                persist_json(&task_path, &task)?;
            }
        }
        Ok(_) => {
            return Err(format!(
                "backref task {} is not a regular file",
                task_path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !task_path.starts_with(&scratch) {
                return Err(format!(
                    "backref task path escaped destination scratch: {}",
                    task_path.display()
                ));
            }
            persist_json_without_replacing(&task_path, &task)?;
        }
        Err(error) => return Err(format!("inspect {}: {error}", task_path.display())),
    }
    Ok(())
}

fn regular_file_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(format!(
            "{} is not a regular file; refusing to follow or replace it",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
}

fn build_backrefs_pending(
    archive: &Path,
    title_index: &Path,
    pending: &Path,
) -> Result<(), String> {
    let parent = pending
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;

    // The builder persists its result with a normal rename. Give it an empty,
    // unique destination namespace so it cannot clobber a file that appeared
    // at the durable pending name, then claim that name with no-replace.
    let staging = tempfile::TempDir::new_in(parent)
        .map_err(|error| format!("create backref staging under {}: {error}", parent.display()))?;
    let staged = staging.path().join("backrefs.swrefs");
    crate::backrefs::build(archive, title_index, &staged)
        .map_err(|error| format!("build {}: {error}", staged.display()))?;
    crate::backrefs::BackrefIndex::open_for_title_index(&staged, title_index)
        .map_err(|error| format!("validate {}: {error}", staged.display()))?;
    match crate::instance::rename_without_replacing(&staged, pending) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(format!(
                "durable backref pending artifact appeared during build: {}",
                pending.display()
            ))
        }
        Err(error) => Err(format!(
            "publish durable backref pending artifact {}: {error}",
            pending.display()
        )),
    }
}

/// Build and publish the optional relation index for the generation selected
/// by `destination`. The archive/title pair is immutable after selector
/// publication, so a retry can reuse a valid generation-specific pending
/// sidecar without rebuilding it. A malformed, non-regular, or otherwise
/// unvalidated existing final sidecar is never replaced.
fn ensure_backrefs_for_selected(destination: &Path) -> Result<(), String> {
    let Some((archive, title_index)) =
        crate::installation_lifecycle::selected_generation_paths(destination)?
    else {
        return Ok(());
    };
    let generation_id = archive
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("selected generation has no usable name: {}", archive.display()))?
        .to_owned();
    let output = backrefs_path(destination);
    let scratch = ensure_mirror_scratch(destination)?;

    if regular_file_exists(&output)? {
        if crate::backrefs::BackrefIndex::open_for_title_index(&output, &title_index).is_ok() {
            return Ok(());
        }
    }

    let pending = pending_backrefs_path(destination, &generation_id);
    if !pending.starts_with(&scratch) {
        return Err(format!(
            "backref pending path escaped destination scratch: {}",
            pending.display()
        ));
    }
    if regular_file_exists(&pending)? {
        crate::backrefs::BackrefIndex::open_for_title_index(&pending, &title_index)
            .map_err(|error| format!("validate pending backrefs {}: {error}", pending.display()))?;
    } else {
        eprintln!(
            "building optional backref index for generation {}",
            generation_id
        );
        build_backrefs_pending(&archive, &title_index, &pending)?;
    }

    // A structurally valid SWREFOBJ is a known wikimak sidecar, even when its
    // title fingerprint is stale. Anything that fails structural validation
    // remains in place as an unowned/foreign artifact.
    match regular_file_exists(&output)? {
        false => crate::instance::rename_without_replacing(&pending, &output).map_err(|error| {
            format!(
                "atomically publish backrefs {}: {error}",
                output.display()
            )
        })?,
        true => {
            if crate::backrefs::BackrefIndex::open_for_title_index(&output, &title_index).is_ok() {
                return Ok(());
            }
            crate::backrefs::BackrefIndex::open(&output).map_err(|error| {
                format!(
                    "refusing to replace unvalidated backref artifact {}: {error}",
                    output.display()
                )
            })?;
            std::fs::rename(&pending, &output).map_err(|error| {
                format!(
                    "atomically replace stale backrefs {}: {error}",
                    output.display()
                )
            })?;
        }
    }
    sync_parent(&pending)?;
    sync_parent(&output)
}

fn cmd_backrefs_task(destination: &str) -> Result<(), String> {
    let destination = Path::new(destination);
    require_absolute_archive(destination)?;
    let scratch = ensure_mirror_scratch(destination)?;
    ensure_direct_tmpdir(&scratch)?;
    let _lock = MirrorBuildLock::acquire(&scratch)?;
    schedule_backrefs_task(destination)?;
    let task_path = backrefs_task_path(destination);
    if !regular_file_exists(&task_path)? {
        println!(
            "backrefs task not needed; generation already has a current sidecar at {}",
            backrefs_path(destination).display()
        );
        return Ok(());
    }
    let task = read_backrefs_task(&task_path)?;
    let (archive, title_index) = crate::installation_lifecycle::selected_generation_paths(destination)?
        .ok_or_else(|| format!("{} has no committed generation", destination.display()))?;
    let selected_generation = archive
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("selected generation has no usable name: {}", archive.display()))?;
    validate_backrefs_task(&task, selected_generation, &task_path)?;
    if regular_file_exists(&backrefs_path(destination))?
        && crate::backrefs::BackrefIndex::open_for_title_index(
            &backrefs_path(destination),
            &title_index,
        )
        .is_ok()
    {
        std::fs::remove_file(&task_path)
            .map_err(|error| format!("complete backref task {}: {error}", task_path.display()))?;
        sync_parent(&task_path)?;
        println!(
            "backrefs task not needed; generation already has a current sidecar at {}",
            backrefs_path(destination).display()
        );
        return Ok(());
    }
    eprintln!(
        "backrefs task generation {selected_generation}: full rebuild; scans latest page bodies, all revisions/user attribution, external runs, and transitive relation sets; no incremental percentage is available"
    );
    ensure_backrefs_for_selected(destination)?;
    std::fs::remove_file(&task_path)
        .map_err(|error| format!("complete backref task {}: {error}", task_path.display()))?;
    sync_parent(&task_path)?;
    println!(
        "backrefs task complete for generation {selected_generation}; sidecar published at {}",
        backrefs_path(destination).display()
    );
    Ok(())
}

fn sync_directory_path(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn update_record_belongs_to_range(
    record: &crate::archive::Record,
    kind: crate::archive::EntityKind,
    upper_id: u64,
) -> bool {
    let entity = record.entity();
    entity.kind == kind && entity.id <= upper_id
}

fn read_required_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(
        &std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?,
    )
    .map_err(|error| format!("{}: {error}", path.display()))
}

fn update_selector_path(scratch: &Path) -> PathBuf {
    scratch.join("updates").join("active.json")
}

fn update_root(scratch: &Path, update_id: &str) -> PathBuf {
    scratch.join("updates").join(update_id)
}

fn lifecycle_plan(source: &crate::direct::UpdateSourcePlan) -> update_lifecycle::UpdatePlanReceipt {
    let compression: crate::archive::CompressionSettings = source.compression.into();
    update_lifecycle::UpdatePlanReceipt {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: source.source_plan_id.clone(),
        base_generation_id: source.base_generation_id.as_str().to_owned(),
        new_generation_id: source.generation_id.as_str().to_owned(),
        source_plan_id: source.source_plan_id.clone(),
        wiki_db: source.wiki_db.clone(),
        base_content_frontier: source.base_content_frontier.clone(),
        base_metadata_frontier: source.base_metadata_frontier.clone(),
        result_content_frontier: source.resulting_content_frontier.clone(),
        result_metadata_frontier: source.resulting_metadata_frontier.clone(),
        overlap_days: source.overlap_days,
        frame_target: source.frame_target,
        compression: update_lifecycle::CompressionReceipt::from(compression),
    }
}

fn load_active_update(
    scratch: &Path,
) -> Result<Option<(update_lifecycle::ActiveUpdate, update_lifecycle::UpdatePaths)>, String> {
    let selector = update_selector_path(scratch);
    let active = match std::fs::read(&selector) {
        Ok(bytes) => serde_json::from_slice::<update_lifecycle::ActiveUpdate>(&bytes)
            .map_err(|error| format!("{}: {error}", selector.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", selector.display())),
    };
    if active.schema != update_lifecycle::UPDATE_SCHEMA
        || active.update_id.is_empty()
        || active.base_generation_id.is_empty()
    {
        return Err(format!("{} is not a valid update selector", selector.display()));
    }
    Ok(Some((
        active.clone(),
        update_lifecycle::UpdatePaths::new(update_root(scratch, &active.update_id)),
    )))
}

pub(crate) fn active_update_progress_path(
    scratch: &Path,
) -> Result<Option<(String, PathBuf)>, String> {
    let Some((active, paths)) = load_active_update(scratch)? else {
        return Ok(None);
    };
    let mut components = Path::new(&active.update_id).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("active update identity is not one path component".into());
    }
    Ok(Some((active.update_id, paths.root.join("progress.bin"))))
}

/// Recover the destination and identify the fetch route before any serving
/// check. The caller holds the mirror writer lock; an active update may also
/// have a durable maintenance marker, so selector-only paths are intentional.
fn recover_fetch_entry(
    archive: &Path,
    scratch: &Path,
) -> Result<
    (
        Option<(PathBuf, PathBuf)>,
        Option<(update_lifecycle::ActiveUpdate, update_lifecycle::UpdatePaths)>,
    ),
    String,
> {
    let _ = crate::installation_lifecycle::recover(archive)?;
    let selected = crate::installation_lifecycle::selected_generation_paths(archive)?;
    let mut active_update = load_active_update(scratch)?;
    if let Some((_, paths)) = active_update.as_ref() {
        if update_hardlink_cleanup_path(&paths.root).is_file() {
            finish_update_cleanup(archive, scratch, paths)?;
            active_update = load_active_update(scratch)?;
        }
    }
    Ok((selected, active_update))
}

fn installed_generation_id(archive: &Path) -> Result<crate::generation::GenerationId, String> {
    let (_, selected_title) = crate::installation_lifecycle::selected_generation_paths(archive)?
        .ok_or_else(|| format!("{} has no installed generation", archive.display()))?;
    crate::title_index::TitleIndex::open(selected_title)
        .map(|titles| titles.generation_id().clone())
        .map_err(|error| error.to_string())
}

fn create_update_plan(
    client: &reqwest::blocking::Client,
    config: &wikimak_mediawiki::Config,
    dbname: &str,
    archive: &Path,
    scratch: &Path,
    overlap_days: u64,
    compression: crate::archive::CompressionSettings,
) -> Result<
    (
        update_lifecycle::ActiveUpdate,
        update_lifecycle::UpdatePaths,
        crate::direct::UpdateSourcePlan,
        crate::generation::GenerationIdentity,
    ),
    String,
> {
    let selected = crate::installation_lifecycle::serving_pair(archive)?
        .ok_or_else(|| format!("{} has no installed generation", archive.display()))?;
    let base = crate::generation::generation_identity(&selected.archive, &selected.title)
        .map_err(|error| error.to_string())?;
    if base.wiki_db != dbname {
        return Err(format!(
            "installed generation belongs to {}, not {dbname}",
            base.wiki_db
        ));
    }
    let source = crate::direct::discover_update_source_plan(
        client,
        config,
        &base,
        overlap_days,
        MIRROR_FRAME_TARGET,
        compression,
        &|message| eprintln!("{message}"),
    )
    .map_err(|error| error.to_string())?;
    let active = update_lifecycle::ActiveUpdate {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: source.source_plan_id.clone(),
        base_generation_id: base.generation_id.as_str().to_owned(),
    };
    let paths =
        update_lifecycle::UpdatePaths::new(update_root(scratch, &active.update_id));
    std::fs::create_dir_all(&paths.root)
        .map_err(|error| format!("{}: {error}", paths.root.display()))?;
    // Exact remote discovery is durable before the selector makes this plan
    // resumable.  Materialization never rediscovers.
    persist_json(&paths.source_plan(), &source)?;
    persist_json(&paths.plan(), &lifecycle_plan(&source))?;
    std::fs::create_dir_all(update_selector_path(scratch).parent().unwrap())
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    persist_json(&update_selector_path(scratch), &active)?;
    Ok((active, paths, source, base))
}

fn load_update_plan(
    active: &update_lifecycle::ActiveUpdate,
    paths: &update_lifecycle::UpdatePaths,
    dbname: &str,
) -> Result<crate::direct::UpdateSourcePlan, String> {
    let source: crate::direct::UpdateSourcePlan = read_required_json(&paths.source_plan())?;
    crate::direct::validate_update_source_plan(&source)
        .map_err(|error| error.to_string())?;
    let plan: update_lifecycle::UpdatePlanReceipt = read_required_json(&paths.plan())?;
    if source.source_plan_id != active.update_id
        || source.base_generation_id.as_str() != active.base_generation_id
        || source.wiki_db != dbname
        || plan != lifecycle_plan(&source)
    {
        return Err(format!(
            "{} does not match the active update selector",
            paths.plan().display()
        ));
    }
    Ok(source)
}

fn tail_id(
    source: &crate::direct::UpdateSourcePlan,
    stats: &crate::direct::UpdateArchiveStats,
) -> String {
    let mut identity = b"wikipedia-update-tail\0".to_vec();
    identity.extend_from_slice(source.source_plan_id.as_bytes());
    identity.extend_from_slice(&stats.output_bytes.to_le_bytes());
    identity.extend_from_slice(&stats.output_frames.to_le_bytes());
    identity.extend_from_slice(&stats.output_records.to_le_bytes());
    crate::generation::GenerationId::from_plan_bytes(&identity)
        .as_str()
        .to_owned()
}

fn ensure_update_tail(
    client: &reqwest::blocking::Client,
    source: &crate::direct::UpdateSourcePlan,
    paths: &update_lifecycle::UpdatePaths,
    run_id: Option<&str>,
) -> Result<update_lifecycle::TailReceipt, String> {
    if let Some(receipt) =
        update_lifecycle::read_receipt::<update_lifecycle::TailReceipt>(
            &paths.tail_receipt(),
        )
        .map_err(|error| error.to_string())?
    {
        return Ok(receipt);
    }
    std::fs::create_dir_all(paths.tail_archive().parent().unwrap())
        .map_err(|error| format!("{}: {error}", paths.root.display()))?;
    let tail_artifact_exists = match std::fs::symlink_metadata(paths.tail_archive()) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "inspect {} before tail recovery: {error}",
                paths.tail_archive().display()
            ));
        }
    };
    if tail_artifact_exists {
        return recover_unreceipted_tail(source, paths);
    }
    let work = paths.root.join("tail").join("work");
    std::fs::create_dir_all(&work)
        .map_err(|error| format!("{}: {error}", work.display()))?;
    let stats = crate::direct::build_update_archive_from_plan_for_run(
        client,
        source,
        paths.tail_archive(),
        &work,
        &paths.root,
        run_id,
        |message| eprintln!("{message}"),
    )
    .map_err(|error| error.to_string())?;
    let tail_id = tail_id(source, &stats);
    let identity = crate::generation::GenerationId::parse(&tail_id)
        .and_then(|identity| identity.to_bytes())
        .map_err(|error| error.to_string())?;
    let frame_directory = crate::frame_directory::write_from_archive(
        paths.tail_archive(),
        paths.tail_frame_directory(),
        identity,
    )
    .map_err(|error| error.to_string())?;
    if frame_directory.frames != stats.output_frames
        || frame_directory.records != stats.output_records
    {
        return Err("materialized update tail statistics disagree with its frame directory".into());
    }
    let receipt = update_lifecycle::TailReceipt {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: source.source_plan_id.clone(),
        base_generation_id: source.base_generation_id.as_str().to_owned(),
        source_plan_id: source.source_plan_id.clone(),
        tail_id,
        file_name: "records.swdump".into(),
        bytes: stats.output_bytes,
        frame_directory_name: "frames.swframe".into(),
        frame_directory_format: crate::frame_directory::FORMAT_VERSION,
        frame_directory_bytes: frame_directory.bytes,
        frames: frame_directory.frames,
        records: frame_directory.records,
        first_entity: frame_directory.first_entity.map(Into::into),
        last_entity: frame_directory.last_entity.map(Into::into),
        complete: true,
    };
    persist_json(&paths.tail_receipt(), &receipt)?;
    Ok(receipt)
}

fn recover_unreceipted_tail(
    source: &crate::direct::UpdateSourcePlan,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::TailReceipt, String> {
    let archive_metadata = std::fs::symlink_metadata(paths.tail_archive())
        .map_err(|error| format!("{}: {error}", paths.tail_archive().display()))?;
    if !archive_metadata.file_type().is_file() {
        return Err(format!(
            "{} is not a regular file; refusing to replace it",
            paths.tail_archive().display()
        ));
    }
    if !crate::archive::has_clean_completion_marker(paths.tail_archive())
        .map_err(|error| error.to_string())?
    {
        return Err(format!(
            "{} has no clean completion marker; refusing to replace it",
            paths.tail_archive().display()
        ));
    }

    let directory = crate::frame_directory::FrameDirectory::open(paths.tail_frame_directory())
        .map_err(|error| {
            format!(
                "{} is not a valid tail frame directory: {error}",
                paths.tail_frame_directory().display()
            )
        })?;
    let summary = directory.summary();
    let archive_bytes = archive_metadata.len();
    directory
        .require_archive_bounds(archive_bytes)
        .map_err(|error| {
            format!(
                "{} does not describe {}: {error}",
                paths.tail_frame_directory().display(),
                paths.tail_archive().display()
            )
        })?;
    let stats = crate::direct::UpdateArchiveStats {
        output_bytes: archive_bytes,
        output_frames: summary.frames,
        output_records: summary.records,
        ..Default::default()
    };
    let tail_id = tail_id(source, &stats);
    let identity = crate::generation::GenerationId::parse(&tail_id)
        .and_then(|identity| identity.to_bytes())
        .map_err(|error| error.to_string())?;
    directory
        .require_identity(identity)
        .map_err(|error| {
            format!(
                "{} does not match the recoverable tail identity: {error}",
                paths.tail_frame_directory().display()
            )
        })?;
    let receipt = update_lifecycle::TailReceipt {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: source.source_plan_id.clone(),
        base_generation_id: source.base_generation_id.as_str().to_owned(),
        source_plan_id: source.source_plan_id.clone(),
        tail_id,
        file_name: "records.swdump".into(),
        bytes: archive_bytes,
        frame_directory_name: "frames.swframe".into(),
        frame_directory_format: crate::frame_directory::FORMAT_VERSION,
        frame_directory_bytes: summary.bytes,
        frames: summary.frames,
        records: summary.records,
        first_entity: summary.first_entity.map(Into::into),
        last_entity: summary.last_entity.map(Into::into),
        complete: true,
    };
    persist_json(&paths.tail_receipt(), &receipt)?;
    Ok(receipt)
}

fn hard_link_file(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::hard_link(source, destination).map_err(|error| {
        format!(
            "cannot create destination-local immutable range link {} -> {}: {error}",
            destination.display(),
            source.display()
        )
    })
}

fn hard_link_archive(source: &Path, destination: &Path) -> Result<(), String> {
    if source.is_dir() {
        std::fs::create_dir(destination)
            .map_err(|error| format!("{}: {error}", destination.display()))?;
        for entry in
            std::fs::read_dir(source).map_err(|error| format!("{}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| format!("{}: {error}", source.display()))?;
            hard_link_file(&entry.path(), &destination.join(entry.file_name()))?;
        }
        sync_directory_path(destination)
    } else {
        hard_link_file(source, destination)?;
        sync_parent(destination)
    }
}

fn ensure_preserved_base(
    archive: &Path,
    source: &crate::direct::UpdateSourcePlan,
    base: &crate::generation::GenerationIdentity,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::PreservedBaseReceipt, String> {
    if let Some(receipt) =
        update_lifecycle::read_receipt::<update_lifecycle::PreservedBaseReceipt>(
            &paths.base_receipt(),
        )
        .map_err(|error| error.to_string())?
    {
        crate::generation::validate_generation(
            paths.base_archive(),
            paths.base_index(),
            &receipt.generation,
        )
        .map_err(|error| error.to_string())?;
        return Ok(receipt);
    }
    let base_archive = paths.base_archive();
    let base_root = base_archive.parent().unwrap();
    std::fs::create_dir_all(base_root)
        .map_err(|error| format!("{}: {error}", base_root.display()))?;
    for path in [paths.base_archive(), paths.base_index()] {
        if path.exists() {
            retire_nested_owned_entry(base_root, &path, "stale-base")?;
        }
    }
    let (selected_archive, selected_title) =
        crate::installation_lifecycle::selected_generation_paths(archive)?
            .ok_or_else(|| format!("{} has no installed generation", archive.display()))?;
    hard_link_archive(&selected_archive, &paths.base_archive())?;
    if let Err(error) = hard_link_file(&selected_title, &paths.base_index()) {
        let _ = remove_path(&paths.base_archive());
        return Err(error);
    }
    sync_directory_path(base_root)?;
    crate::generation::validate_generation(
        paths.base_archive(),
        paths.base_index(),
        base,
    )
    .map_err(|error| error.to_string())?;
    let receipt = update_lifecycle::PreservedBaseReceipt {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: source.source_plan_id.clone(),
        generation: base.clone(),
        archive_name: "archive.swdump".into(),
        index_name: "archive.swtitle".into(),
    };
    persist_json(&paths.base_receipt(), &receipt)?;
    Ok(receipt)
}

fn ensure_base_site_info(
    source: &crate::direct::UpdateSourcePlan,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::BaseSiteInfoCheckpoint, String> {
    let plan = lifecycle_plan(source);
    if let Some(checkpoint) = update_lifecycle::read_receipt::<
        update_lifecycle::BaseSiteInfoCheckpoint,
    >(&paths.base_site_info())
    .map_err(|error| error.to_string())?
    {
        update_lifecycle::validate_base_site_info(paths, &plan, &checkpoint)
            .map_err(|error| error.to_string())?;
        return Ok(checkpoint);
    }
    reconstruct_missing_base_site_info(source, paths)
}

fn reconstruct_missing_base_site_info(
    source: &crate::direct::UpdateSourcePlan,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::BaseSiteInfoCheckpoint, String> {
    let plan = lifecycle_plan(source);
    let preserved = update_lifecycle::read_receipt::<update_lifecycle::PreservedBaseReceipt>(
        &paths.base_receipt(),
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| {
        format!(
            "{}: cannot reconstruct SiteInfo without a preserved-base receipt",
            paths.base_receipt().display()
        )
    })?;
    update_lifecycle::validate_preserved_base(paths, &plan, &preserved)
        .map_err(|error| error.to_string())?;
    if preserved.generation.generation_id.as_str() != source.base_generation_id.as_str() {
        return Err(format!(
            "{}: preserved base generation {} disagrees with source plan base {}",
            paths.base_receipt().display(),
            preserved.generation.generation_id.as_str(),
            source.base_generation_id.as_str()
        ));
    }
    let titles = crate::title_index::TitleIndex::open(paths.base_index())
        .map_err(|error| error.to_string())?;
    let site_info = latest_site_info(&paths.base_archive(), &titles)?;
    let checkpoint = update_lifecycle::BaseSiteInfoCheckpoint::new(&plan, site_info);
    persist_json(&paths.base_site_info(), &checkpoint)?;
    Ok(checkpoint)
}

fn stable_update_object_id(domain: &[u8], fields: &impl serde::Serialize) -> Result<String, String> {
    let mut bytes = domain.to_vec();
    bytes.extend_from_slice(
        &serde_json::to_vec(fields).map_err(|error| format!("encode update identity: {error}"))?,
    );
    Ok(crate::generation::GenerationId::from_plan_bytes(&bytes)
        .as_str()
        .to_owned())
}

fn ensure_range_plan(
    source: &crate::direct::UpdateSourcePlan,
    tail: &update_lifecycle::TailReceipt,
    base: &crate::generation::GenerationIdentity,
    base_site_info: &update_lifecycle::BaseSiteInfoCheckpoint,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::RangePlanReceipt, String> {
    update_lifecycle::validate_base_site_info(
        paths,
        &lifecycle_plan(source),
        base_site_info,
    )
    .map_err(|error| error.to_string())?;
    if let Some(plan) =
        update_lifecycle::read_receipt::<update_lifecycle::RangePlanReceipt>(
            &paths.range_plan(),
        )
        .map_err(|error| error.to_string())?
    {
        return Ok(plan);
    }
    if !paths.base_archive().is_dir() {
        return Err(
            "incremental update requires the installed Wikipedia archive-set layout".into(),
        );
    }
    let archive = crate::archive_set::ArchiveSetReader::open(paths.base_archive())
        .map_err(|error| error.to_string())?;
    if archive.segments().len() != base.segments.len() {
        return Err("base generation segment inventory changed after preservation".into());
    }
    let mut slots = Vec::new();
    for (segment, identity) in archive.segments().iter().zip(&base.segments) {
        let Some(kind) = segment.kind else {
            continue;
        };
        let expected_role = match kind {
            crate::archive::EntityKind::Page => 1,
            crate::archive::EntityKind::User => 2,
            crate::archive::EntityKind::Global => 3,
        };
        if identity.role != expected_role
            || identity.first_id != segment.first_id
            || identity.last_id != segment.last_id
            || identity.virtual_start != segment.virtual_start
            || identity.bytes != segment.bytes
        {
            return Err("base archive paths do not match the preserved generation index".into());
        }
        let index = slots.len();
        let base_segment_id = stable_update_object_id(
            b"wikipedia-base-range\0",
            &(
                source.base_generation_id.as_str(),
                identity.role,
                identity.first_id,
                identity.last_id,
                identity.virtual_start,
                identity.bytes,
            ),
        )?;
        let candidate_id = stable_update_object_id(
            b"wikipedia-update-range\0",
            &(
                &source.source_plan_id,
                &tail.tail_id,
                index,
                &base_segment_id,
            ),
        )?;
        slots.push(update_lifecycle::RangeSlot {
            index,
            kind: kind as u8,
            first_id: segment.first_id,
            last_id: segment.last_id,
            base_segment_id,
            base_name: segment.name.clone(),
            base_bytes: segment.bytes,
            candidate_id,
        });
    }
    if slots.is_empty() {
        return Err("base archive has no entity range slots".into());
    }
    let plan = update_lifecycle::RangePlanReceipt {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: source.source_plan_id.clone(),
        base_generation_id: source.base_generation_id.as_str().to_owned(),
        tail_id: tail.tail_id.clone(),
        slots,
    };
    std::fs::create_dir_all(paths.range_plan().parent().unwrap())
        .map_err(|error| format!("{}: {error}", paths.range_plan().display()))?;
    persist_json(&paths.range_plan(), &plan)?;
    Ok(plan)
}

fn is_title_projection_record(record: &crate::archive::Record) -> bool {
    match record {
        crate::archive::Record::PageState { .. }
        | crate::archive::Record::SiteInfo { .. } => true,
        crate::archive::Record::PageAction { entity, .. } => {
            entity.kind == crate::archive::EntityKind::Page
        }
        _ => false,
    }
}

struct BufferedTailSlot {
    bytes: std::sync::Arc<[u8]>,
    span_start: u64,
    frame_start: usize,
    frame_end: usize,
    start_ordinal: u64,
    next_cursor: update_lifecycle::TailCursorReceipt,
    records: u64,
    first: Option<crate::archive::EntityKey>,
    last: Option<crate::archive::EntityKey>,
    entities: Vec<crate::archive::EntityKey>,
    title_records: u64,
}

impl BufferedTailSlot {
    fn physical_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn reader(
        &self,
        directory: std::sync::Arc<crate::frame_directory::FrameDirectory>,
        reference: Option<crate::archive::CompressionReference>,
    ) -> Result<crate::archive::ArchiveRecordReader, String> {
        let mut reader = crate::archive::ArchiveRecordReader::open_buffered_frame_directory(
            std::sync::Arc::clone(&self.bytes),
            self.span_start,
            directory,
            self.frame_start,
            self.frame_end,
            reference,
        )
        .map_err(|error| error.to_string())?;
        for _ in 0..self.start_ordinal {
            if reader
                .next_record()
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Err("tail cursor record ordinal lies beyond its buffered frame".into());
            }
        }
        Ok(reader)
    }

    fn range_recipe(
        &self,
        directory: std::sync::Arc<crate::frame_directory::FrameDirectory>,
        reference: Option<crate::archive::CompressionReference>,
        first_entity: crate::archive::EntityKey,
        last_entity: crate::archive::EntityKey,
    ) -> Result<Option<crate::archive::BufferedRecordRangeRecipe>, String> {
        crate::archive::BufferedRecordRangeRecipe::new(
            std::sync::Arc::clone(&self.bytes),
            self.span_start,
            directory,
            self.frame_start,
            self.frame_end,
            self.start_ordinal,
            reference,
            first_entity,
            last_entity,
        )
        .map_err(|error| error.to_string())
    }
}

fn update_range_input_bytes(
    slot: &update_lifecycle::RangeSlot,
    base_bytes: u64,
    tail_bytes: u64,
) -> Result<u64, String> {
    base_bytes.checked_add(tail_bytes).ok_or_else(|| {
        format!(
            "update range {} input buffer size overflows (base {base_bytes} bytes, tail {tail_bytes} bytes)",
            slot.index
        )
    })
}

fn read_exact_file_range(
    path: &Path,
    offset: u64,
    bytes: u64,
    event: RangeIoEvent,
    observe: &mut impl FnMut(RangeIoEvent),
) -> Result<std::sync::Arc<[u8]>, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let file_bytes = file
        .metadata()
        .map_err(|error| format!("{}: {error}", path.display()))?
        .len();
    let end = offset
        .checked_add(bytes)
        .ok_or_else(|| format!("{}: requested byte range overflows", path.display()))?;
    if end > file_bytes {
        return Err(format!(
            "{}: requested byte range {offset}..{end} exceeds file length {file_bytes}",
            path.display()
        ));
    }
    let length = usize::try_from(bytes).map_err(|_| {
        format!(
            "{}: requested in-memory byte range {bytes} exceeds this platform",
            path.display()
        )
    })?;
    let mut buffered = Vec::new();
    buffered.try_reserve_exact(length).map_err(|error| {
        format!(
            "{}: cannot reserve {bytes} bytes for the update input buffer: {error}",
            path.display()
        )
    })?;
    buffered.resize(length, 0);
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.read_exact(&mut buffered)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    observe(event);
    Ok(buffered.into())
}

fn buffer_tail_slot(
    paths: &update_lifecycle::UpdatePaths,
    directory: std::sync::Arc<crate::frame_directory::FrameDirectory>,
    reference: Option<crate::archive::CompressionReference>,
    cursor: &update_lifecycle::TailCursorReceipt,
    slot: &update_lifecycle::RangeSlot,
    kind: crate::archive::EntityKind,
    upper_id: u64,
    observe: &mut impl FnMut(RangeIoEvent),
) -> Result<Option<BufferedTailSlot>, String> {
    let Some(frame_offset) = cursor.frame_offset else {
        return Ok(None);
    };
    let frame_start = directory
        .index_of_offset(frame_offset)
        .ok_or_else(|| "range receipt points between update-tail frames".to_string())?;
    let upper = crate::archive::EntityKey { kind, id: upper_id };
    let first = directory
        .get(frame_start)
        .map_err(|error| error.to_string())?;
    if first.first_entity > upper {
        return Ok(None);
    }
    let mut frame_end = frame_start;
    while frame_end < directory.len() {
        let entry = directory
            .get(frame_end)
            .map_err(|error| error.to_string())?;
        if frame_end != frame_start && entry.first_entity > upper {
            break;
        }
        frame_end += 1;
    }
    let last = directory
        .get(frame_end - 1)
        .map_err(|error| error.to_string())?;
    let span_start = first
        .compressed_offset
        .checked_sub(64)
        .ok_or_else(|| "update-tail frame has no preceding header".to_string())?;
    let span_end = last
        .compressed_offset
        .checked_add(last.compressed_bytes)
        .ok_or_else(|| "update-tail frame span overflows".to_string())?;
    let span_bytes = span_end - span_start;
    let bytes = read_exact_file_range(
        &paths.tail_archive(),
        span_start,
        span_bytes,
        RangeIoEvent::TailRead {
            offset: span_start,
            bytes: span_bytes,
        },
        observe,
    )?;
    let mut buffered = BufferedTailSlot {
        bytes,
        span_start,
        frame_start,
        frame_end,
        start_ordinal: cursor.record_ordinal,
        next_cursor: cursor.clone(),
        records: 0,
        first: None,
        last: None,
        entities: Vec::new(),
        title_records: 0,
    };
    let mut reader = buffered.reader(directory.clone(), reference)?;
    loop {
        let Some(record) = reader.next_record().map_err(|error| error.to_string())? else {
            buffered.next_cursor = if frame_end < directory.len() {
                update_lifecycle::TailCursorReceipt {
                    frame_offset: Some(
                        directory
                            .get(frame_end)
                            .map_err(|error| error.to_string())?
                            .compressed_offset,
                    ),
                    record_ordinal: 0,
                }
            } else {
                update_lifecycle::TailCursorReceipt {
                    frame_offset: None,
                    record_ordinal: 0,
                }
            };
            break;
        };
        let entity = record.entity();
        if update_record_belongs_to_range(&record, kind, upper_id) {
            if entity.id < slot.first_id {
                return Err(format!(
                    "update tail cursor for slot {} precedes its first entity",
                    slot.index
                ));
            }
            buffered.first.get_or_insert(entity);
            buffered.last = Some(entity);
            if buffered.entities.last().is_some_and(|previous| *previous > entity) {
                return Err(format!(
                    "update tail for slot {} is not sorted by entity",
                    slot.index
                ));
            }
            if buffered.entities.last() != Some(&entity) {
                buffered.entities.push(entity);
            }
            buffered.records = buffered
                .records
                .checked_add(1)
                .ok_or_else(|| "update range record count overflows".to_string())?;
            if is_title_projection_record(&record) {
                buffered.title_records = buffered
                    .title_records
                    .checked_add(1)
                    .ok_or_else(|| "update title record count overflows".to_string())?;
            }
            continue;
        }
        let local_offset = reader
            .current_frame_offset()
            .ok_or_else(|| "pending tail record has no buffered source frame".to_string())?;
        buffered.next_cursor = update_lifecycle::TailCursorReceipt {
            frame_offset: Some(
                buffered
                    .span_start
                    .checked_add(local_offset)
                    .ok_or_else(|| "tail cursor offset overflows".to_string())?,
            ),
            record_ordinal: reader
                .current_frame_records_read()
                .checked_sub(1)
                .ok_or_else(|| "pending tail record has no frame ordinal".to_string())?,
        };
        break;
    }
    Ok(Some(buffered))
}

fn begin_title_projection(
    paths: &update_lifecycle::UpdatePaths,
    slot: &update_lifecycle::RangeSlot,
) -> Result<
    (
        PathBuf,
        crate::archive::ArchiveWriter<'static, std::fs::File>,
    ),
    String,
> {
    let final_path = paths.range_projection(&slot.candidate_id);
    std::fs::create_dir_all(final_path.parent().unwrap())
        .map_err(|error| format!("{}: {error}", final_path.display()))?;
    let building = final_path.with_extension("swdump-building");
    for path in [&building, &final_path] {
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
        }
    }
    let file = std::fs::File::create(&building)
        .map_err(|error| format!("{}: {error}", building.display()))?;
    let writer = crate::archive::ArchiveWriter::new(file, 1 << 20)
        .map_err(|error| error.to_string())?;
    Ok((building, writer))
}

fn finish_title_projection(
    paths: &update_lifecycle::UpdatePaths,
    slot: &update_lifecycle::RangeSlot,
    building: &Path,
    writer: crate::archive::ArchiveWriter<'static, std::fs::File>,
    records: u64,
) -> Result<(Option<String>, u64, u64), String> {
    let (file, _) = writer.finish().map_err(|error| error.to_string())?;
    file.sync_all()
        .map_err(|error| format!("{}: {error}", building.display()))?;
    drop(file);
    if records == 0 {
        std::fs::remove_file(building)
            .map_err(|error| format!("{}: {error}", building.display()))?;
        return Ok((None, 0, 0));
    }
    let final_path = paths.range_projection(&slot.candidate_id);
    std::fs::rename(building, &final_path)
        .map_err(|error| format!("{}: {error}", final_path.display()))?;
    sync_parent(&final_path)?;
    let bytes = std::fs::metadata(&final_path)
        .map_err(|error| format!("{}: {error}", final_path.display()))?
        .len();
    Ok((
        Some(format!("{}.swdump", slot.candidate_id)),
        bytes,
        records,
    ))
}

struct ProjectionDeltaSpool {
    accumulator: crate::backrefs::ProjectionDeltaAccumulator,
}

impl ProjectionDeltaSpool {
    fn new() -> Self {
        Self {
            accumulator: crate::backrefs::ProjectionDeltaAccumulator::new(),
        }
    }

    fn append(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.accumulator
            .absorb(&bytes)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn finish(self, output: &Path) -> Result<(u64, u64), String> {
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", output.display()))?;
        let building = output.with_extension("swrefdelta-building");
        let set_count = self.accumulator
            .write_to(&building)
            .map_err(|error| format!("{}: {error}", building.display()))?;
        let bytes = std::fs::metadata(&building)
            .map_err(|error| format!("{}: {error}", building.display()))?
            .len();
        std::fs::rename(&building, output)
            .map_err(|error| format!("{}: {error}", output.display()))?;
        sync_parent(output)?;
        Ok((bytes, set_count))
    }
}

fn write_buffered_title_projection(
    paths: &update_lifecycle::UpdatePaths,
    slot: &update_lifecycle::RangeSlot,
    buffered: &BufferedTailSlot,
    directory: std::sync::Arc<crate::frame_directory::FrameDirectory>,
    reference: Option<crate::archive::CompressionReference>,
    kind: crate::archive::EntityKind,
    upper_id: u64,
) -> Result<(Option<String>, u64, u64), String> {
    let (building, mut writer) = begin_title_projection(paths, slot)?;
    let mut reader = buffered.reader(directory, reference)?;
    let mut records = 0_u64;
    while let Some(record) = reader.next_record().map_err(|error| error.to_string())? {
        if !update_record_belongs_to_range(&record, kind, upper_id) {
            break;
        }
        if is_title_projection_record(&record) {
            writer.write(&record).map_err(|error| error.to_string())?;
            records = records
                .checked_add(1)
                .ok_or_else(|| "update title record count overflows".to_string())?;
        }
    }
    if records != buffered.title_records {
        return Err("buffered update title scan is not repeatable".into());
    }
    finish_title_projection(paths, slot, &building, writer, records)
}

#[derive(Default)]
struct SparseRangeMergeStats {
    output_frames: u64,
    output_records: u64,
    copied_frames: u64,
    copied_compressed_bytes: u64,
    decoded_frames: u64,
    decoded_compressed_bytes: u64,
    backref_delta_bytes: u64,
    backref_delta_sets: u64,
}

fn write_buffered_update_range<W: crate::archive::StreamingFrameOutput>(
    writer: &mut crate::archive::ParallelArchiveWriter<W>,
    recipe: crate::archive::BufferedRecordRangeRecipe,
    projection_factory: Option<&std::sync::Arc<dyn crate::archive::FrameMergeProjectionFactory>>,
    projection_deltas: Option<&mut ProjectionDeltaSpool>,
) -> Result<(), String> {
    let mut source = recipe.open().map_err(|error| error.to_string())?;
    let mut projection = projection_factory
        .map(|factory| factory.new_state().map_err(|error| error.to_string()))
        .transpose()?;
    while let Some(record) = crate::archive::RecordSource::next_record(&mut source)
        .map_err(|error| error.to_string())?
    {
        if let Some(projection) = projection.as_mut() {
            projection
                .observe(&record)
                .map_err(|error| error.to_string())?;
        }
        writer.write(&record).map_err(|error| error.to_string())?;
    }
    if let Some(projection) = projection {
        projection_deltas
            .ok_or_else(|| "projection spool is missing for a projected range".to_string())?
            .append(projection.finish().map_err(|error| error.to_string())?)?;
    }
    Ok(())
}

fn merge_sparse_update_range(
    base: std::sync::Arc<[u8]>,
    frames: &[crate::frame_directory::FrameDirectoryEntry],
    prefix: std::sync::Arc<[u8]>,
    updates: &BufferedTailSlot,
    update_directory: std::sync::Arc<crate::frame_directory::FrameDirectory>,
    update_reference: Option<crate::archive::CompressionReference>,
    output: crate::archive_set::ArchiveSetOutput,
    projection_factory: Option<std::sync::Arc<dyn crate::archive::FrameMergeProjectionFactory>>,
    projection_output: Option<&Path>,
    progress: &mut impl FnMut(u64),
    observe: &mut impl FnMut(RangeIoEvent),
) -> Result<
    (
        crate::archive_set::ArchiveSetOutput,
        SparseRangeMergeStats,
    ),
    String,
> {
    let output = output.collect_frame_directory_metadata();
    let workers = usize::try_from(crate::archive::streaming_compression_workers())
        .unwrap_or(usize::MAX);
    let mut writer = crate::archive::ParallelArchiveWriter::new_frame_transform(
        output,
        MIRROR_FRAME_TARGET,
        mirror_compression(),
        &prefix,
        workers,
    )
    .map_err(|error| error.to_string())?;
    let mut stats = SparseRangeMergeStats::default();
    let mut projection_deltas = projection_output.map(|_| ProjectionDeltaSpool::new());
    let first_update = *updates
        .entities
        .first()
        .ok_or_else(|| "sparse merge has no update entities".to_string())?;
    let last_update = *updates
        .entities
        .last()
        .ok_or_else(|| "sparse merge has no update entities".to_string())?;
    if updates.first != Some(first_update) || updates.last != Some(last_update) {
        return Err("sparse merge update entity index disagrees with its record bounds".into());
    }
    if first_update.kind != last_update.kind {
        return Err("sparse merge update range changes entity kind".into());
    }
    let mut update_position = 0_usize;

    for entry in frames.iter().copied() {
        if entry.first_entity.kind != first_update.kind
            || entry.last_entity.kind != first_update.kind
        {
            return Err("sparse merge base frame changes entity kind".into());
        }
        let before_end = update_position
            + updates.entities[update_position..]
                .partition_point(|entity| *entity < entry.first_entity);
        if before_end != update_position {
            let recipe = updates
                .range_recipe(
                    std::sync::Arc::clone(&update_directory),
                    update_reference.clone(),
                    updates.entities[update_position],
                    updates.entities[before_end - 1],
                )?
                .ok_or_else(|| {
                    "buffered update directory omits known entities before a base frame"
                        .to_string()
                })?;
                write_buffered_update_range(
                    &mut writer,
                    recipe,
                    projection_factory.as_ref(),
                    projection_deltas.as_mut(),
                )?;
            update_position = before_end;
        }
        let overlap_end = update_position
            + updates.entities[update_position..]
                .partition_point(|entity| *entity <= entry.last_entity);
        if overlap_end != update_position {
            let recipe = updates
                .range_recipe(
                    std::sync::Arc::clone(&update_directory),
                    update_reference.clone(),
                    updates.entities[update_position],
                    updates.entities[overlap_end - 1],
                )?
                .ok_or_else(|| {
                    "buffered update directory omits an entity known to intersect a base frame"
                        .to_string()
                })?;
            writer
                .append_buffered_merged_frame_with_projection(
                    &base,
                    0,
                    entry,
                    &prefix,
                    Some(recipe),
                    projection_factory.clone(),
                )
                .map_err(|error| error.to_string())?;
            stats.decoded_frames = stats.decoded_frames.saturating_add(1);
            stats.decoded_compressed_bytes = stats
                .decoded_compressed_bytes
                .saturating_add(entry.compressed_bytes);
            update_position = overlap_end;
        } else {
            let copied = writer
                .append_buffered_compressed_frame(&base, 0, entry)
                .map_err(|error| error.to_string())?;
            stats.copied_frames = stats.copied_frames.saturating_add(copied.frames);
            stats.copied_compressed_bytes = stats
                .copied_compressed_bytes
                .saturating_add(copied.compressed_bytes);
        }
        let ready = writer
            .collect_ready_output_records()
            .map_err(|error| error.to_string())?;
        while let Some((_, delta)) = writer.take_projected_delta() {
            if let Some(spool) = projection_deltas.as_mut() {
                spool.append(delta)?;
            }
        }
        progress(ready);
    }
    writer
        .drain_buffered_merged_frames_with_progress_and_projection(
            progress,
            &mut |delta| {
                if let Some(spool) = projection_deltas.as_mut() {
                    spool
                        .append(delta)
                        .map_err(|error| crate::archive::ArchiveError::Io(std::io::Error::other(error)))?;
                }
                Ok(())
            },
        )
        .map_err(|error| error.to_string())?;
    while let Some((_, delta)) = writer.take_projected_delta() {
        if let Some(spool) = projection_deltas.as_mut() {
            spool.append(delta)?;
        }
    }
    if update_position != updates.entities.len() {
        let recipe = updates
            .range_recipe(
                std::sync::Arc::clone(&update_directory),
                update_reference,
                updates.entities[update_position],
                last_update,
            )?
            .ok_or_else(|| {
                "buffered update directory omits known entities after the base frames"
                    .to_string()
            })?;
            write_buffered_update_range(
                &mut writer,
                recipe,
                projection_factory.as_ref(),
                projection_deltas.as_mut(),
            )?;
    }
    stats.output_records = writer
        .finish_output_records(progress)
        .map_err(|error| error.to_string())?;
    progress(stats.output_records);
    let (output, frames) = writer.finish().map_err(|error| error.to_string())?;
    observe(RangeIoEvent::ReplacementWrite);
    stats.output_frames = frames;
    if let (Some(spool), Some(path)) = (projection_deltas, projection_output) {
        (stats.backref_delta_bytes, stats.backref_delta_sets) = spool.finish(path)?;
    }
    Ok((output, stats))
}

fn apply_update_ranges(
    source: &crate::direct::UpdateSourcePlan,
    tail_receipt: &update_lifecycle::TailReceipt,
    range_plan: &update_lifecycle::RangePlanReceipt,
    base_site_info: &update_lifecycle::BaseSiteInfoCheckpoint,
    paths: &update_lifecycle::UpdatePaths,
    maintenance: &crate::installation_lifecycle::UpdateMaintenanceGuard,
) -> Result<(u64, u64), String> {
    apply_update_ranges_observing(
        source,
        tail_receipt,
        range_plan,
        base_site_info,
        paths,
        maintenance,
        &mut |_| {},
    )
}

fn apply_update_ranges_observing(
    source: &crate::direct::UpdateSourcePlan,
    tail_receipt: &update_lifecycle::TailReceipt,
    range_plan: &update_lifecycle::RangePlanReceipt,
    base_site_info: &update_lifecycle::BaseSiteInfoCheckpoint,
    paths: &update_lifecycle::UpdatePaths,
    maintenance: &crate::installation_lifecycle::UpdateMaintenanceGuard,
    observe: &mut impl FnMut(RangeIoEvent),
) -> Result<(u64, u64), String> {
    let identity = crate::generation::GenerationId::parse(&tail_receipt.tail_id)
        .and_then(|identity| identity.to_bytes())
        .map_err(|error| error.to_string())?;
    let all_frames = std::sync::Arc::new(
        crate::frame_directory::FrameDirectory::open_bound(
            paths.tail_frame_directory(),
            identity,
        )
        .map_err(|error| error.to_string())?,
    );
    let mut completed = 0;
    let mut previous_receipt = None;
    for slot in &range_plan.slots {
        let receipt =
            update_lifecycle::read_receipt::<update_lifecycle::RangeCandidateReceipt>(
                &paths.range_receipt(slot.index),
            )
            .map_err(|error| error.to_string())?;
        let Some(receipt) = receipt else {
            break;
        };
        let _ = retire_replaced_base_segment(maintenance, paths, slot, &receipt)?;
        previous_receipt = Some(receipt);
        completed += 1;
    }
    if range_plan.slots[completed..].iter().any(|slot| {
        paths.range_receipt(slot.index).exists()
    }) {
        return Err("range receipts contain a gap".into());
    }

    let mut cursor = previous_receipt
        .as_ref()
        .map(|receipt| receipt.tail_cursor.clone())
        .unwrap_or(update_lifecycle::TailCursorReceipt {
            frame_offset: all_frames.get(0).ok().map(|frame| frame.compressed_offset),
            record_ordinal: 0,
        });
    if cursor
        .frame_offset
        .is_some_and(|offset| all_frames.index_of_offset(offset).is_none())
    {
        return Err("range receipt points between update-tail frames".into());
    }
    let tail_reference = crate::archive::archive_compression_reference(paths.tail_archive())
        .map_err(|error| error.to_string())?;
    let prefix = crate::archive::archive_ref_prefix_part(
        paths
            .base_archive()
            .join("0000-reference.swdump-part"),
    )
    .map_err(|error| error.to_string())?;
    let mut total_frames = 0_u64;
    let mut total_records = 0_u64;

    for (index, slot) in range_plan.slots.iter().enumerate().skip(completed) {
        let kind =
            crate::archive::EntityKind::try_from(slot.kind).map_err(|error| error.to_string())?;
        let final_for_kind = range_plan
            .slots
            .get(index + 1)
            .is_none_or(|next| next.kind != slot.kind);
        let upper_id = if final_for_kind {
            u64::MAX
        } else {
            slot.last_id
        };
        let buffered_tail = buffer_tail_slot(
            paths,
            std::sync::Arc::clone(&all_frames),
            tail_reference.clone(),
            &cursor,
            slot,
            kind,
            upper_id,
            observe,
        )?;
        let touched = buffered_tail
            .as_ref()
            .is_some_and(|buffered| buffered.records != 0);
        let first_addition = buffered_tail.as_ref().and_then(|buffered| buffered.first);
        let last_addition = buffered_tail.as_ref().and_then(|buffered| buffered.last);
        let tail_bytes_read = buffered_tail
            .as_ref()
            .map_or(0, BufferedTailSlot::physical_bytes);
        let tail_cursor = buffered_tail
            .as_ref()
            .map(|buffered| buffered.next_cursor.clone())
            .unwrap_or_else(|| cursor.clone());
        let mut title_projection_name = None;
        let mut title_projection_bytes = 0;
        let mut title_projection_records = 0;
        let mut base_frame_bytes_copied = 0_u64;
        let mut base_frame_bytes_decoded = 0_u64;
        let mut backref_delta_name = None;
        let mut backref_delta_bytes = 0_u64;
        let mut backref_delta_records = 0_u64;
        let selection = if !touched {
            update_lifecycle::RangeSelection::Unchanged {
                segment_id: slot.base_segment_id.clone(),
                name: slot.base_name.clone(),
                bytes: slot.base_bytes,
            }
        } else {
            let buffered_tail = buffered_tail
                .as_ref()
                .expect("touched range has a buffered tail");
            let required_memory = update_range_input_bytes(
                slot,
                slot.base_bytes,
                buffered_tail.physical_bytes(),
            )?;
            let base = read_exact_file_range(
                &paths.base_archive().join(&slot.base_name),
                0,
                slot.base_bytes,
                RangeIoEvent::BaseRead {
                    bytes: slot.base_bytes,
                },
                observe,
            )?;
            let base_frames = crate::archive::buffered_data_segment_frames(&base)
                .map_err(|error| error.to_string())?;
            eprintln!(
                "update range {}/{}: preloaded {} bytes for {}",
                index + 1,
                range_plan.slots.len(),
                required_memory,
                slot.base_name,
            );
            let output = crate::archive_set::ArchiveSetOutput::new_in(&paths.root, u64::MAX)
                .map_err(|error| error.to_string())?;
            let projection_factory = (kind == crate::archive::EntityKind::Page).then(|| {
                std::sync::Arc::new(crate::backrefs::BackrefFrameMergeProjectionFactory::new(
                    &base_site_info.site_info(),
                )) as std::sync::Arc<dyn crate::archive::FrameMergeProjectionFactory>
            });
            let mut last_progress = std::time::Instant::now();
            let projection_output = (kind == crate::archive::EntityKind::Page)
                .then(|| paths.range_backref_delta(&slot.candidate_id));
            let (output, merge_stats) = merge_sparse_update_range(
                base,
                &base_frames,
                std::sync::Arc::clone(&prefix),
                buffered_tail,
                std::sync::Arc::clone(&all_frames),
                tail_reference.clone(),
                output,
                projection_factory,
                projection_output.as_deref(),
                &mut |records| {
                    if last_progress.elapsed() >= std::time::Duration::from_secs(2) {
                        eprintln!(
                            "update range {}/{}: {records} merged records",
                            index + 1,
                            range_plan.slots.len()
                        );
                        last_progress = std::time::Instant::now();
                    }
                },
                observe,
            )?;
            if kind == crate::archive::EntityKind::Page {
                backref_delta_name = Some(format!("{}.swrefdelta", slot.candidate_id));
                backref_delta_bytes = merge_stats.backref_delta_bytes;
                backref_delta_records = merge_stats.backref_delta_sets;
                update_test_failpoint("after-range-delta-before-receipt")?;
            }
            let records = merge_stats.output_records;
            base_frame_bytes_copied = merge_stats.copied_compressed_bytes;
            base_frame_bytes_decoded = merge_stats.decoded_compressed_bytes;
            eprintln!(
                "update range {}/{}: {} base frames copied ({} bytes), {} decoded ({} bytes)",
                index + 1,
                range_plan.slots.len(),
                merge_stats.copied_frames,
                merge_stats.copied_compressed_bytes,
                merge_stats.decoded_frames,
                merge_stats.decoded_compressed_bytes,
            );
            (
                title_projection_name,
                title_projection_bytes,
                title_projection_records,
            ) = write_buffered_title_projection(
                paths,
                slot,
                buffered_tail,
                std::sync::Arc::clone(&all_frames),
                tail_reference.clone(),
                kind,
                upper_id,
            )?;
            let completed_archive = output.finish().map_err(|error| error.to_string())?;
            let replacement = paths
                .root
                .join("ranges")
                .join(format!(".building-{}", slot.candidate_id));
            if replacement.exists() {
                retire_nested_owned_entry(
                    replacement.parent().unwrap(),
                    &replacement,
                    "stale-range-building",
                )?;
            }
            std::fs::create_dir_all(replacement.parent().unwrap())
                .map_err(|error| format!("{}: {error}", replacement.display()))?;
            let replacement_range = completed_archive
                .segments
                .iter()
                .find(|segment| segment.kind == Some(kind))
                .ok_or_else(|| "range replacement contains no entity segment".to_string())?
                .clone();
            let replacement_files = completed_archive
                .segments
                .iter()
                .map(|segment| segment.name.clone())
                .collect::<Vec<_>>();
            let frame_entries = completed_archive
                .frame_directory_entries_for(&replacement_range.name)
                .map_err(|error| error.to_string())?
                .to_vec();
            completed_archive
                .persist(&replacement)
                .map_err(|error| error.to_string())?;
            let object = paths.range_object(&slot.candidate_id);
            std::fs::create_dir_all(object.parent().unwrap())
                .map_err(|error| format!("{}: {error}", object.display()))?;
            if object.exists() {
                std::fs::remove_file(&object)
                    .map_err(|error| format!("{}: {error}", object.display()))?;
            }
            std::fs::rename(
                replacement.join(&replacement_range.name),
                &object,
            )
            .map_err(|error| format!("{}: {error}", object.display()))?;
            sync_directory_path(object.parent().unwrap())?;
            let object_metadata = std::fs::symlink_metadata(&object)
                .map_err(|error| format!("{}: {error}", object.display()))?;
            if !object_metadata.file_type().is_file()
                || object_metadata.len() != replacement_range.bytes
            {
                return Err(format!(
                    "{}: persisted replacement length/type disagrees with write-time segment metadata",
                    object.display()
                ));
            }
            let identity = crate::generation::GenerationId::parse(&slot.candidate_id)
                .and_then(|identity| identity.to_bytes())
                .map_err(|error| error.to_string())?;
            let frame_directory = crate::frame_directory::write_from_archive_entries(
                &frame_entries,
                replacement_range.bytes,
                paths.range_frame_directory(&slot.candidate_id),
                identity,
            )
            .map_err(|error| error.to_string())?;
            if frame_directory.records != records
                || frame_directory.frames != merge_stats.output_frames
            {
                return Err(
                    "range replacement counts disagree with its frame directory".into(),
                );
            }
            let first_entity = frame_directory
                .first_entity
                .ok_or_else(|| "range replacement frame directory is empty".to_string())?;
            let last_entity = frame_directory
                .last_entity
                .ok_or_else(|| "range replacement frame directory is empty".to_string())?;
            for name in replacement_files {
                let path = replacement.join(name);
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("{}: {error}", path.display())),
                }
            }
            match std::fs::remove_dir(&replacement) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    retire_nested_owned_entry(
                        replacement.parent().unwrap(),
                        &replacement,
                        "range-building-residue",
                    )?;
                }
                Err(error) => return Err(format!("{}: {error}", replacement.display())),
            }
            update_lifecycle::RangeSelection::Replaced {
                segment_id: slot.candidate_id.clone(),
                name: replacement_range.name,
                bytes: replacement_range.bytes,
                frames: frame_directory.frames,
                records,
                frame_directory_name: format!("{}.swframe", slot.candidate_id),
                frame_directory_format: crate::frame_directory::FORMAT_VERSION,
                frame_directory_bytes: frame_directory.bytes,
                first_entity: first_entity.into(),
                last_entity: last_entity.into(),
            }
        };
        let (base_bytes_read, candidate_bytes_written, frames, records) = match &selection {
            update_lifecycle::RangeSelection::Unchanged { .. } => (0, 0, 0, 0),
            update_lifecycle::RangeSelection::Replaced {
                bytes,
                frames,
                records,
                ..
            } => (
                base_frame_bytes_copied.saturating_add(base_frame_bytes_decoded),
                *bytes,
                *frames,
                *records,
            ),
        };
        let receipt = update_lifecycle::RangeCandidateReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: source.source_plan_id.clone(),
            base_generation_id: source.base_generation_id.as_str().to_owned(),
            tail_id: tail_receipt.tail_id.clone(),
            slot_index: slot.index,
            candidate_id: slot.candidate_id.clone(),
            kind: slot.kind,
            first_id: slot.first_id,
            last_id: slot.last_id,
            base_segment_id: slot.base_segment_id.clone(),
            selection,
            consumed_first: first_addition.map(|entity| update_lifecycle::EntityBound {
                kind: entity.kind as u8,
                id: entity.id,
            }),
            consumed_last: last_addition.map(|entity| update_lifecycle::EntityBound {
                kind: entity.kind as u8,
                id: entity.id,
            }),
            tail_bytes_read,
            base_bytes_read,
            base_frame_bytes_copied,
            base_frame_bytes_decoded,
            candidate_bytes_written,
            title_projection_name,
            title_projection_bytes,
            title_projection_records,
            backref_delta_name,
            backref_delta_bytes,
            backref_delta_records,
            tail_cursor: tail_cursor.clone(),
            complete: true,
        };
        std::fs::create_dir_all(paths.range_receipt(slot.index).parent().unwrap())
            .map_err(|error| format!("{}: {error}", paths.root.display()))?;
        persist_json(&paths.range_receipt(slot.index), &receipt)?;
        let old_installed_reclaimed = retire_replaced_base_segment(maintenance, paths, slot, &receipt)?;
        if matches!(&receipt.selection, update_lifecycle::RangeSelection::Replaced { .. }) {
            observe(RangeIoEvent::RangeDurableSwap {
                slot_index: slot.index,
                old_installed_reclaimed,
            });
        }
        total_frames = total_frames.saturating_add(frames);
        total_records = total_records.saturating_add(records);
        cursor = tail_cursor;
        eprintln!(
            "update range {}/{} durable · tail {} bytes · base {} bytes · candidate {} bytes",
            index + 1,
            range_plan.slots.len(),
            receipt.tail_bytes_read,
            receipt.base_bytes_read,
            receipt.candidate_bytes_written
        );
    }
    if cursor.frame_offset.is_some() {
        return Err("sorted update tail contains records outside the base range plan".into());
    }
    Ok((total_frames, total_records))
}

fn retire_replaced_base_segment(
    maintenance: &crate::installation_lifecycle::UpdateMaintenanceGuard,
    paths: &update_lifecycle::UpdatePaths,
    slot: &update_lifecycle::RangeSlot,
    receipt: &update_lifecycle::RangeCandidateReceipt,
) -> Result<bool, String> {
    let update_lifecycle::RangeSelection::Replaced { name, .. } = &receipt.selection else {
        return Ok(false);
    };
    let reclaimed = maintenance.replace_preserved_segment(
        &paths.base_archive().join(&slot.base_name),
        &slot.base_name,
        &paths.range_object(&slot.candidate_id),
        name,
    )?;
    eprintln!(
        "update range {}: old {}-byte HDD piece {}",
        slot.index + 1,
        slot.base_bytes,
        if reclaimed { "reclaimed" } else { "was already reclaimed" }
    );
    Ok(reclaimed)
}

fn read_range_receipts(
    plan: &update_lifecycle::RangePlanReceipt,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<Vec<update_lifecycle::RangeCandidateReceipt>, String> {
    plan.slots
        .iter()
        .map(|slot| {
            update_lifecycle::read_receipt(&paths.range_receipt(slot.index))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    format!(
                        "range slot {} has no durable candidate receipt",
                        slot.index
                    )
                })
        })
        .collect()
}

fn ensure_candidate_archive(
    plan: &update_lifecycle::RangePlanReceipt,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::CandidateInventoryReceipt, String> {
    if let Some(inventory) =
        update_lifecycle::read_receipt::<update_lifecycle::CandidateInventoryReceipt>(
            &paths.candidate_inventory(),
        )
        .map_err(|error| error.to_string())?
    {
        return Ok(inventory);
    }
    let receipts = read_range_receipts(plan, paths)?;
    let candidate_archive = paths.candidate_archive();
    let candidate_root = candidate_archive.parent().unwrap();
    std::fs::create_dir_all(candidate_root)
        .map_err(|error| format!("{}: {error}", candidate_root.display()))?;
    if paths.candidate_archive().exists() {
        retire_nested_owned_entry(
            candidate_root,
            &paths.candidate_archive(),
            "stale-candidate-archive",
        )?;
    }
    let building = candidate_root.join(".archive-building");
    if building.exists() {
        retire_nested_owned_entry(candidate_root, &building, "stale-candidate-building")?;
    }
    std::fs::create_dir(&building)
        .map_err(|error| format!("{}: {error}", building.display()))?;
    for name in [
        "0000-reference.swdump-part",
        "9999-complete.swdump-part",
    ] {
        hard_link_file(
            &paths.base_archive().join(name),
            &building.join(name),
        )?;
    }
    let mut selected = Vec::with_capacity(plan.slots.len());
    for (slot, receipt) in plan.slots.iter().zip(&receipts) {
        let (segment_id, name, bytes, source) = match &receipt.selection {
            update_lifecycle::RangeSelection::Unchanged {
                segment_id,
                name,
                bytes,
            } => (
                segment_id.clone(),
                name.clone(),
                *bytes,
                paths.base_archive().join(name),
            ),
            update_lifecycle::RangeSelection::Replaced {
                segment_id,
                name,
                bytes,
                ..
            } => (
                segment_id.clone(),
                name.clone(),
                *bytes,
                paths.range_object(&slot.candidate_id),
            ),
        };
        if building.join(&name).exists() {
            return Err(format!("candidate range filename collision: {name}"));
        }
        hard_link_file(&source, &building.join(&name))?;
        selected.push(update_lifecycle::SelectedSegment {
            slot_index: slot.index,
            segment_id,
            name,
            bytes,
        });
    }
    eprintln!(
        "linked {} candidate data pieces; syncing candidate directory",
        selected.len()
    );
    sync_directory_path(&building)?;
    crate::archive_set::ArchiveSetReader::open(&building)
        .map_err(|error| error.to_string())?;
    std::fs::rename(&building, paths.candidate_archive())
        .map_err(|error| format!("{}: {error}", paths.candidate_archive().display()))?;
    sync_directory_path(candidate_root)?;
    let inventory = update_lifecycle::CandidateInventoryReceipt {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: plan.update_id.clone(),
        base_generation_id: plan.base_generation_id.clone(),
        tail_id: plan.tail_id.clone(),
        segments: selected,
    };
    persist_json(&paths.candidate_inventory(), &inventory)?;
    Ok(inventory)
}

fn latest_site_info(
    archive: &Path,
    titles: &crate::title_index::TitleIndex,
) -> Result<crate::archive::SiteInfoRecord, String> {
    let indexed =
        crate::archive::IndexedArchiveSet::open(archive, titles).map_err(|error| error.to_string())?;
    let target = crate::archive::EntityKey {
        kind: crate::archive::EntityKind::Global,
        id: 1,
    };
    let mut left = 0;
    let mut right = titles.frame_count();
    while left < right {
        let middle = left + (right - left) / 2;
        if titles
            .frame(middle)
            .map_err(|error| error.to_string())?
            .info
            .last_entity
            < target
        {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    for position in left..titles.frame_count() {
        let frame = titles.frame(position).map_err(|error| error.to_string())?;
        if frame.info.first_entity > target {
            break;
        }
        let location = indexed.location(frame).map_err(|error| error.to_string())?;
        let mut input = indexed
            .open_file(&location)
            .map_err(|error| error.to_string())?;
        let mut latest = None;
        crate::archive::visit_frame_while_file(&mut input, &location, |record| {
            if let crate::archive::Record::SiteInfo {
                site_info,
                ..
            } = record
            {
                latest = Some(site_info);
                return Ok(false);
            }
            Ok(true)
        })
        .map_err(|error| error.to_string())?;
        if let Some(site_info) = latest {
            return Ok(site_info);
        }
    }
    Err("base generation has no siteinfo record".into())
}

fn projected_site_info(
    range_plan: &update_lifecycle::RangePlanReceipt,
    receipts: &[update_lifecycle::RangeCandidateReceipt],
    paths: &update_lifecycle::UpdatePaths,
    fallback: crate::archive::SiteInfoRecord,
) -> Result<crate::archive::SiteInfoRecord, String> {
    let mut latest = None;
    for (slot, receipt) in range_plan.slots.iter().zip(receipts) {
        if slot.kind != crate::archive::EntityKind::Global as u8
            || receipt.title_projection_records == 0
        {
            continue;
        }
        let mut reader =
            crate::archive::ArchiveRecordReader::open(paths.range_projection(&slot.candidate_id))
                .map_err(|error| error.to_string())?;
        while let Some(record) = reader.next_record().map_err(|error| error.to_string())? {
            if let crate::archive::Record::SiteInfo {
                timestamp_micros,
                site_info,
            } = record
            {
                if latest
                    .as_ref()
                    .is_none_or(|(current, _)| timestamp_micros > *current)
                {
                    latest = Some((timestamp_micros, site_info));
                }
            }
        }
    }
    Ok(latest.map_or(fallback, |(_, site_info)| site_info))
}

struct MergedTitleEntries<'a> {
    base: std::iter::Peekable<crate::title_index::TitleEntryIter<'a>>,
    updates: std::iter::Peekable<crate::title_projection::ExternalTitleEntryIter<'a>>,
}

impl<'a> MergedTitleEntries<'a> {
    fn new(
        base: crate::title_index::TitleEntryIter<'a>,
        updates: crate::title_projection::ExternalTitleEntryIter<'a>,
    ) -> Self {
        Self {
            base: base.peekable(),
            updates: updates.peekable(),
        }
    }
}

impl Iterator for MergedTitleEntries<'_> {
    type Item = crate::title_index::TitleIndexEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let next = match (self.base.peek(), self.updates.peek()) {
            (Some(base), Some(update)) => {
                let base_key = (base.coded_title, base.time);
                let update_key = (update.coded_title, update.time);
                match base_key.cmp(&update_key) {
                    std::cmp::Ordering::Less => self.base.next(),
                    std::cmp::Ordering::Greater => self.updates.next(),
                    std::cmp::Ordering::Equal => {
                        self.base.next();
                        self.updates.next()
                    }
                }
            }
            (Some(_), None) => self.base.next(),
            (None, Some(_)) => self.updates.next(),
            (None, None) => None,
        };
        next
    }
}

#[derive(Clone)]
struct FrameSelection {
    old_start: u64,
    old_end: u64,
    new_start: u64,
    replacement: Option<std::sync::Arc<crate::frame_directory::FrameDirectory>>,
}

struct ComposedFrameEntries<'a> {
    base: std::iter::Peekable<crate::title_index::FrameEntryIter<'a>>,
    selections: std::vec::IntoIter<FrameSelection>,
    active: Option<FrameSelection>,
    replacement_position: usize,
}

impl<'a> ComposedFrameEntries<'a> {
    fn new(
        base: crate::title_index::FrameEntryIter<'a>,
        selections: Vec<FrameSelection>,
    ) -> Self {
        Self {
            base: base.peekable(),
            selections: selections.into_iter(),
            active: None,
            replacement_position: 0,
        }
    }
}

impl Iterator for ComposedFrameEntries<'_> {
    type Item = crate::archive::Result<crate::title_index::FrameIndexEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.active.is_none() {
                self.active = self.selections.next();
                self.replacement_position = 0;
            }
            let Some(selection) = self.active.as_ref() else {
                return self.base.next();
            };
            if let Some(replacement) = selection.replacement.as_ref() {
                while let Some(frame) = self.base.peek() {
                    match frame {
                        Ok(frame) if frame.compressed_offset < selection.old_start => {
                            let next = self.base.next();
                            return next;
                        }
                        Ok(frame) if frame.compressed_offset < selection.old_end => {
                            let _ = self.base.next();
                        }
                        Err(_) => {
                            let next = self.base.next();
                            return next;
                        }
                        Ok(_) => break,
                    }
                }
                if self.replacement_position < replacement.len() {
                    let entry = match replacement.get(self.replacement_position) {
                        Ok(entry) => Ok(crate::title_index::FrameIndexEntry {
                            info: entry.frame_info(),
                            compressed_offset: selection.new_start + entry.compressed_offset,
                        }),
                        Err(error) => Err(error),
                    };
                    self.replacement_position += 1;
                    return Some(entry);
                }
                self.active = None;
                continue;
            }
            match self.base.peek() {
                Some(Ok(frame)) if frame.compressed_offset < selection.old_start => {
                    let next = self.base.next();
                    return next;
                }
                Some(Ok(frame)) if frame.compressed_offset < selection.old_end => {
                    let mut frame = self.base.next().expect("peeked frame");
                    if let Ok(frame) = &mut frame {
                        frame.compressed_offset = selection.new_start
                            + (frame.compressed_offset - selection.old_start);
                    }
                    return Some(frame);
                }
                Some(Err(_)) => {
                    let next = self.base.next();
                    return next;
                }
                Some(Ok(_)) | None => {
                    self.active = None;
                }
            }
        }
    }

}

fn frame_selections(
    base_titles: &crate::title_index::TitleIndex,
    plan: &update_lifecycle::RangePlanReceipt,
    receipts: &[update_lifecycle::RangeCandidateReceipt],
    candidate: &crate::archive_set::ArchiveSetReader,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<
    (
        Vec<FrameSelection>,
        Vec<crate::title_index::SegmentIndexEntry>,
    ),
    String,
> {
    let base_segments = base_titles
        .segment_entries()
        .collect::<crate::archive::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    let candidate_segments = candidate.segments();
    let mut output_segments = Vec::with_capacity(candidate_segments.len());
    for segment in candidate_segments {
        let role = match segment.kind {
            Some(crate::archive::EntityKind::Page) => 1,
            Some(crate::archive::EntityKind::User) => 2,
            Some(crate::archive::EntityKind::Global) => 3,
            None if segment.name.starts_with("0000-") => 0,
            None if segment.name.starts_with("9999-") => 4,
            None => return Err("candidate archive has an unknown control segment".into()),
        };
        output_segments.push(crate::title_index::SegmentIndexEntry {
            role,
            first_id: segment.first_id,
            last_id: segment.last_id,
            virtual_start: segment.virtual_start,
            bytes: segment.bytes,
        });
    }
    let mut selections = Vec::with_capacity(plan.slots.len());
    for (slot, receipt) in plan.slots.iter().zip(receipts) {
        let base_segment = base_segments
            .iter()
            .find(|segment| {
                segment.role == slot.kind
                    && segment.first_id == slot.first_id
                    && segment.last_id == slot.last_id
                    && segment.bytes == slot.base_bytes
            })
            .ok_or_else(|| format!("base index has no segment for slot {}", slot.index))?;
        let selected_name = match &receipt.selection {
            update_lifecycle::RangeSelection::Unchanged { name, .. }
            | update_lifecycle::RangeSelection::Replaced { name, .. } => name,
        };
        let candidate_segment = candidate_segments
            .iter()
            .find(|segment| segment.name == *selected_name)
            .ok_or_else(|| format!("candidate archive has no selected segment {selected_name}"))?;
        let replacement = match &receipt.selection {
            update_lifecycle::RangeSelection::Unchanged { .. } => None,
            update_lifecycle::RangeSelection::Replaced {
                segment_id,
                frames,
                ..
            } => {
                let identity = crate::generation::GenerationId::parse(segment_id)
                    .and_then(|identity| identity.to_bytes())
                    .map_err(|error| error.to_string())?;
                let directory = crate::frame_directory::FrameDirectory::open_bound(
                    paths.range_frame_directory(&slot.candidate_id),
                    identity,
                )
                .map_err(|error| error.to_string())?;
                if directory.len() as u64 != *frames {
                    return Err(format!(
                        "replacement frame directory for slot {} changed after validation",
                        slot.index
                    ));
                }
                Some(std::sync::Arc::new(directory))
            }
        };
        selections.push(FrameSelection {
            old_start: base_segment.virtual_start,
            old_end: base_segment.virtual_start + base_segment.bytes,
            new_start: candidate_segment.virtual_start,
            replacement,
        });
    }
    Ok((selections, output_segments))
}

fn read_range_backref_deltas(
    range_plan: &update_lifecycle::RangePlanReceipt,
    receipts: &[update_lifecycle::RangeCandidateReceipt],
    paths: &update_lifecycle::UpdatePaths,
) -> Result<Vec<PathBuf>, String> {
    if range_plan.slots.len() != receipts.len() {
        return Err("range backref receipt count does not match its plan".into());
    }
    let mut deltas = Vec::new();
    for (slot, receipt) in range_plan.slots.iter().zip(receipts) {
        let Some(name) = receipt.backref_delta_name.as_deref() else {
            continue;
        };
        let expected = format!("{}.swrefdelta", slot.candidate_id);
        if name != expected {
            return Err(format!(
                "range {} has a non-canonical backref delta name",
                slot.index
            ));
        }
        // inspect_update has already parsed the complete canonical stream. At
        // handoff, recheck the cheap identity fields so an intervening file
        // replacement cannot silently change its length or declared set count.
        // The rewrite reader then performs the one necessary full parse while
        // applying the delta.
        let path = paths.range_backref_delta(&slot.candidate_id);
        let bytes = std::fs::metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .len();
        if bytes != receipt.backref_delta_bytes {
            return Err(format!(
                "{}: durable backref delta length disagrees with its receipt",
                path.display()
            ));
        }
        let sets = crate::backrefs::projection_delta_file_declared_sets(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if sets != receipt.backref_delta_records {
            return Err(format!(
                "{}: durable backref delta set count disagrees with its receipt",
                path.display()
            ));
        }
        deltas.push(path);
    }
    Ok(deltas)
}

fn has_legacy_page_delta_gap(
    range_plan: &update_lifecycle::RangePlanReceipt,
    receipts: &[update_lifecycle::RangeCandidateReceipt],
) -> bool {
    range_plan
        .slots
        .iter()
        .zip(receipts)
        .any(|(slot, receipt)| {
            slot.kind == crate::archive::EntityKind::Page as u8
                && matches!(receipt.selection, update_lifecycle::RangeSelection::Replaced { .. })
                && receipt.backref_delta_name.is_none()
                && receipt.backref_delta_bytes == 0
                && receipt.backref_delta_records == 0
        })
}

fn ensure_prepared_backrefs(
    archive: &Path,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::PreparedGenerationReceipt, String> {
    let mut prepared = update_lifecycle::read_receipt::<
        update_lifecycle::PreparedGenerationReceipt,
    >(&paths.prepared_generation())
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "cannot prepare backrefs without a prepared-generation receipt".to_string())?;
    let legacy = prepared.backrefs_name.is_empty()
        && prepared.backrefs_bytes == 0
        && prepared.backrefs_records == 0;

    let installed = installed_generation_id(archive)?;
    let candidate_is_installed = installed.as_str() == prepared.generation_id;
    let (source_archive, source_title) = if candidate_is_installed {
        crate::installation_lifecycle::selected_generation_paths(archive)?
            .ok_or_else(|| "candidate is selected without published generation paths".to_string())?
    } else {
        (paths.candidate_archive(), paths.candidate_index())
    };

    let live = backrefs_path(archive);
    let candidate = paths.candidate_backrefs();

    if !legacy {
        let sidecar = if candidate_is_installed && regular_file_exists(&live)? {
            live.clone()
        } else {
            candidate.clone()
        };
        let index = crate::backrefs::BackrefIndex::open_for_title_index(&sidecar, &source_title)
            .map_err(|error| format!("validate {}: {error}", sidecar.display()))?;
        if !index.has_raw_postings() {
            return Err(format!(
                "{}: prepared backref sidecar lacks raw postings",
                sidecar.display()
            ));
        }
        if index.logical_count() != prepared.backrefs_records {
            return Err(format!(
                "{}: prepared backref logical-count metadata disagrees",
                sidecar.display()
            ));
        }
        return Ok(prepared);
    }

    let mut sidecar = None;
    if candidate_is_installed && regular_file_exists(&live)? {
        if let Ok(index) = crate::backrefs::BackrefIndex::open_for_title_index(&live, &source_title)
        {
            if index.has_raw_postings() {
                sidecar = Some(live.clone());
            }
        }
    }
    if sidecar.is_none() && regular_file_exists(&candidate)? {
        if let Ok(index) = crate::backrefs::BackrefIndex::open_for_title_index(&candidate, &source_title)
        {
            if index.has_raw_postings() {
                sidecar = Some(candidate.clone());
            } else {
                std::fs::remove_file(&candidate)
                    .map_err(|error| format!("{}: {error}", candidate.display()))?;
            }
        } else {
            std::fs::remove_file(&candidate)
                .map_err(|error| format!("{}: {error}", candidate.display()))?;
        }
    }
    if sidecar.is_none() {
        eprintln!("bootstrapping legacy prepared backrefs from the candidate archive");
        build_backrefs_pending(&source_archive, &source_title, &candidate)?;
        sidecar = Some(candidate.clone());
    }
    let sidecar = sidecar.expect("legacy backref sidecar is selected above");
    let legacy_live_only = candidate_is_installed
        && sidecar == live
        && !regular_file_exists(&candidate)?;
    let index = crate::backrefs::BackrefIndex::open_for_title_index(&sidecar, &source_title)
        .map_err(|error| format!("validate {}: {error}", sidecar.display()))?;
    if !index.has_raw_postings() {
        return Err(format!(
            "{}: recovered backref sidecar lacks raw postings",
            sidecar.display()
        ));
    }
    if legacy_live_only {
        return Ok(prepared);
    }
    prepared.backrefs_name = "backrefs.swrefs".into();
    prepared.backrefs_bytes = std::fs::metadata(&sidecar)
        .map_err(|error| format!("{}: {error}", sidecar.display()))?
        .len();
    prepared.backrefs_records = index.logical_count();
    persist_json(&paths.prepared_generation(), &prepared)?;
    Ok(prepared)
}

fn ensure_candidate_backrefs(
    archive: &Path,
    base_site_info: &update_lifecycle::BaseSiteInfoCheckpoint,
    candidate_site_info: &crate::archive::SiteInfoRecord,
    range_plan: &update_lifecycle::RangePlanReceipt,
    receipts: &[update_lifecycle::RangeCandidateReceipt],
    paths: &update_lifecycle::UpdatePaths,
) -> Result<(String, u64, u64), String> {
    let output = paths.candidate_backrefs();
    if regular_file_exists(&output)? {
        if let Ok(index) = crate::backrefs::BackrefIndex::open_for_title_index(
            &output,
            paths.candidate_index(),
        ) {
            if index.has_raw_postings() {
                let bytes = std::fs::metadata(&output)
                    .map_err(|error| format!("{}: {error}", output.display()))?
                    .len();
                return Ok((
                    "backrefs.swrefs".into(),
                    bytes,
                    index.logical_count(),
                ));
            }
        }
        std::fs::remove_file(&output)
            .map_err(|error| format!("{}: {error}", output.display()))?;
    }
    let base_sidecar = backrefs_path(archive);
    let base_index = paths.base_index();
    let legacy_page_delta_gap = has_legacy_page_delta_gap(range_plan, receipts);
    let can_incremental = if regular_file_exists(&base_sidecar)? {
        match crate::backrefs::BackrefIndex::open_for_title_index(&base_sidecar, &base_index) {
            Ok(index) => {
                index.has_raw_postings()
                    && !legacy_page_delta_gap
                    && base_site_info.site_info() == *candidate_site_info
            }
            Err(_) => false,
        }
    } else {
        false
    };
    if can_incremental {
        #[cfg(test)]
        INCREMENTAL_BACKREF_PREPARATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        eprintln!("preparing candidate backrefs from range XOR deltas");
        let deltas = read_range_backref_deltas(range_plan, receipts, paths)?;
        crate::backrefs::rewrite_backref_sidecar_with_deltas(
            &base_sidecar,
            &base_index,
            &deltas,
            &output,
            paths.candidate_index(),
            crate::backrefs::title_index_fingerprint(paths.candidate_index())
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("rewrite candidate backrefs: {error}"))?;
    } else {
        #[cfg(test)]
        BOOTSTRAP_BACKREF_PREPARATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        eprintln!("bootstrapping candidate backrefs from the candidate archive");
        build_backrefs_pending(&paths.candidate_archive(), &paths.candidate_index(), &output)?;
    }
    let index = crate::backrefs::BackrefIndex::open_for_title_index(&output, paths.candidate_index())
        .map_err(|error| format!("validate {}: {error}", output.display()))?;
    let bytes = std::fs::metadata(&output)
        .map_err(|error| format!("{}: {error}", output.display()))?
        .len();
    Ok(("backrefs.swrefs".into(), bytes, index.logical_count()))
}

fn ensure_candidate_index(
    archive: &Path,
    source: &crate::direct::UpdateSourcePlan,
    base_site_info: &update_lifecycle::BaseSiteInfoCheckpoint,
    range_plan: &update_lifecycle::RangePlanReceipt,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<(update_lifecycle::PreparedGenerationReceipt, u64), String> {
    update_lifecycle::validate_base_site_info(
        paths,
        &lifecycle_plan(source),
        base_site_info,
    )
    .map_err(|error| error.to_string())?;
    if let Some(receipt) =
        update_lifecycle::read_receipt::<update_lifecycle::PreparedGenerationReceipt>(
            &paths.prepared_generation(),
        )
        .map_err(|error| error.to_string())?
    {
        update_lifecycle::validate_prepared_generation(
            paths,
            &lifecycle_plan(source),
            &receipt,
            false,
        )
        .map_err(|error| error.to_string())?;
        return Ok((
            receipt,
            crate::title_index::TitleIndex::open(paths.candidate_index())
                .map_err(|error| error.to_string())?
                .entries(),
        ));
    }
    let receipts = read_range_receipts(range_plan, paths)?;
    let base_titles = crate::title_index::TitleIndex::open(paths.base_index())
        .map_err(|error| error.to_string())?;
    let site_info = projected_site_info(
        range_plan,
        &receipts,
        paths,
        base_site_info.site_info(),
    )?;
    let mut projection_inputs = Vec::new();
    for (slot, receipt) in range_plan.slots.iter().zip(&receipts) {
        if receipt.title_projection_records == 0 {
            continue;
        }
        projection_inputs.push((
            paths.range_projection(&slot.candidate_id),
            receipt.title_projection_records,
        ));
    }
    let projection_work = paths.root.join("candidate").join("title-projection-work");
    if projection_work.exists() {
        retire_nested_owned_entry(
            projection_work.parent().unwrap(),
            &projection_work,
            "stale-title-projection-work",
        )?;
    }
    std::fs::create_dir_all(&projection_work)
        .map_err(|error| format!("{}: {error}", projection_work.display()))?;
    let tail_titles = crate::title_projection::project_title_record_archives(
        projection_inputs,
        site_info.clone(),
        &projection_work,
        crate::title_projection::ProjectionLimits::default(),
    )
    .map_err(|error| error.to_string())?;
    let candidate_set = crate::archive_set::ArchiveSetReader::open(paths.candidate_archive())
        .map_err(|error| error.to_string())?;
    let (selections, segments) =
        frame_selections(&base_titles, range_plan, &receipts, &candidate_set, paths)?;
    if paths.candidate_index().exists() {
        std::fs::remove_file(paths.candidate_index())
            .map_err(|error| format!("{}: {error}", paths.candidate_index().display()))?;
    }
    crate::title_index::write_generation_index(
        paths.candidate_index(),
        &source.generation_id,
        MergedTitleEntries::new(base_titles.title_entries(), tail_titles.iter()),
        ComposedFrameEntries::new(base_titles.frame_entries(), selections),
        segments.into_iter().map(Ok),
    )
    .map_err(|error| error.to_string())?;
    drop(tail_titles);
    remove_path(&projection_work)
        .map_err(|error| format!("{}: {error}", projection_work.display()))?;
    let index = crate::title_index::TitleIndex::open(paths.candidate_index())
        .map_err(|error| error.to_string())?;
    if index.generation_id() != &source.generation_id {
        return Err("prepared index carries the wrong generation ID".into());
    }
    let (backrefs_name, backrefs_bytes, backrefs_records) = ensure_candidate_backrefs(
        archive,
        base_site_info,
        &site_info,
        range_plan,
        &receipts,
        paths,
    )?;
    update_test_failpoint("after-candidate-sidecar-before-prepared-receipt")?;
    let receipt = update_lifecycle::PreparedGenerationReceipt {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: source.source_plan_id.clone(),
        base_generation_id: source.base_generation_id.as_str().to_owned(),
        generation_id: source.generation_id.as_str().to_owned(),
        archive_name: "archive.swdump".into(),
        index_name: "archive.swtitle".into(),
        index_bytes: std::fs::metadata(paths.candidate_index())
            .map_err(|error| format!("{}: {error}", paths.candidate_index().display()))?
            .len(),
        backrefs_name,
        backrefs_bytes,
        backrefs_records,
    };
    persist_json(&paths.prepared_generation(), &receipt)?;
    Ok((receipt, index.entries()))
}

fn publish_update_backrefs(
    archive: &Path,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<(), String> {
    let installed_before = installed_generation_id(archive)?;
    let legacy_prepared = update_lifecycle::read_receipt::<
        update_lifecycle::PreparedGenerationReceipt,
    >(&paths.prepared_generation())
    .map_err(|error| error.to_string())?
    .is_some_and(|prepared| {
        prepared.backrefs_name.is_empty()
            && prepared.backrefs_bytes == 0
            && prepared.backrefs_records == 0
            && installed_before.as_str() == prepared.generation_id
    });
    let prepared = ensure_prepared_backrefs(archive, paths)?;
    let (_selected_archive, selected_title) = crate::installation_lifecycle::selected_generation_paths(archive)?
        .ok_or_else(|| "candidate archive is not selected while publishing backrefs".to_string())?;
    let selected_titles = crate::title_index::TitleIndex::open(&selected_title)
        .map_err(|error| error.to_string())?;
    if selected_titles.generation_id().as_str() != prepared.generation_id {
        return Err(format!(
            "selected title index generation {} disagrees with prepared generation {}",
            selected_titles.generation_id().as_str(),
            prepared.generation_id
        ));
    }
    if prepared.index_bytes != 0
        && std::fs::metadata(&selected_title)
            .map_err(|error| format!("{}: {error}", selected_title.display()))?
            .len()
            != prepared.index_bytes
    {
        return Err(format!(
            "{}: selected title index length disagrees with its prepared receipt",
            selected_title.display()
        ));
    }
    let live = backrefs_path(archive);
    if regular_file_exists(&live)? {
        if let Ok(index) = crate::backrefs::BackrefIndex::open_for_title_index(&live, &selected_title) {
            let bytes = std::fs::metadata(&live)
                .map_err(|error| format!("{}: {error}", live.display()))?
                .len();
            if index.has_raw_postings()
                && (legacy_prepared
                    || (bytes == prepared.backrefs_bytes
                        && index.logical_count() == prepared.backrefs_records))
            {
                let candidate = paths.candidate_backrefs();
                let candidate_is_published = regular_file_exists(&candidate)?
                    && same_proven_hardlink(&candidate, &live)?;
                if candidate_is_published || legacy_prepared {
                    remove_published_backrefs_task(archive)?;
                    return Ok(());
                }
            }
        }
    }
    let candidate = paths.candidate_backrefs();
    if !regular_file_exists(&candidate)? {
        return Err(format!(
            "{}: prepared candidate backrefs are missing; refusing live-only normal publication",
            candidate.display()
        ));
    }
    let candidate_bytes = std::fs::metadata(&candidate)
        .map_err(|error| format!("{}: {error}", candidate.display()))?
        .len();
    if candidate_bytes != prepared.backrefs_bytes {
        return Err(format!(
            "{}: candidate backref length disagrees with its prepared receipt",
            candidate.display()
        ));
    }
    let candidate_index = crate::backrefs::BackrefIndex::open_for_title_index(
        &candidate,
        &selected_title,
    )
    .map_err(|error| format!("validate {}: {error}", candidate.display()))?;
    if !candidate_index.has_raw_postings() {
        return Err(format!(
            "{}: candidate backref sidecar lacks raw postings",
            candidate.display()
        ));
    }
    if candidate_index.logical_count() != prepared.backrefs_records {
        return Err(format!(
            "{}: candidate backref logical count disagrees with its prepared receipt",
            candidate.display()
        ));
    }
    let live_parent = live
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = live_parent.join(format!(
        ".{}.{}.swrefs-install",
        live.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("backrefs"),
        prepared.update_id
    ));
    if staging.exists() {
        std::fs::remove_file(&staging)
            .map_err(|error| format!("{}: {error}", staging.display()))?;
    }
    std::fs::hard_link(&candidate, &staging).map_err(|error| {
        format!(
            "cannot stage candidate backrefs as a same-filesystem hardlink {} -> {}: {error}",
            staging.display(),
            candidate.display()
        )
    })?;
    std::fs::File::open(&staging)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("{}: {error}", staging.display()))?;
    crate::backrefs::BackrefIndex::open_for_title_index(&staging, &selected_title)
        .map_err(|error| format!("validate staged {}: {error}", staging.display()))?;
    std::fs::rename(&staging, &live)
        .map_err(|error| format!("publish {}: {error}", live.display()))?;
    sync_parent(&live)?;
    let published = crate::backrefs::BackrefIndex::open_for_title_index(&live, &selected_title)
        .map_err(|error| format!("validate published {}: {error}", live.display()))?;
    if !published.has_raw_postings() {
        return Err(format!(
            "{}: published backref sidecar lacks raw postings",
            live.display()
        ));
    }
    remove_published_backrefs_task(archive)
}

fn install_update_generation(
    archive: &Path,
    source: &crate::direct::UpdateSourcePlan,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<(), String> {
    let installed_id = installed_generation_id(archive)?;
    if installed_id != source.base_generation_id
        && installed_id != source.generation_id
    {
        return Err(format!(
            "installed index names generation {}, update expects base {} or candidate {}",
            installed_id.as_str(),
            source.base_generation_id.as_str(),
            source.generation_id.as_str()
        ));
    }
    ensure_prepared_backrefs(archive, paths)?;
    if installed_id == source.base_generation_id {
        // Range construction has already drained serving readers under the
        // maintenance lease, durably swapped each changed candidate piece,
        // and reclaimed that piece's previous installed link before advancing
        // to the next range. This final install retains the existing atomic
        // selector publication for the completed candidate generation.
        let outcome = crate::installation_lifecycle::install(
            paths.candidate_archive(),
            paths.candidate_index(),
            archive,
        )?;
        update_test_failpoint("after-selector-install-before-backrefs")?;
        if outcome.cleanup_pending {
            eprintln!(
                "new generation installed; previous generation cleanup is reader-deferred"
            );
        }
    }
    // The selector may already name the candidate after a crash between the
    // archive install and sidecar publication. Publication is idempotent and
    // must run on both paths while the maintenance guard is still held.
    publish_update_backrefs(archive, paths)?;
    validate_installed_update_generation(archive, source)
}

fn commit_update_generation(
    archive: &Path,
    source: &crate::direct::UpdateSourcePlan,
    paths: &update_lifecycle::UpdatePaths,
    maintenance: &mut Option<crate::installation_lifecycle::UpdateMaintenanceGuard>,
) -> Result<(), String> {
    publish_update_backrefs(archive, paths)?;
    update_test_failpoint("after-backrefs-before-commit")?;
    validate_installed_update_generation(archive, source)?;
    publish_update_commit(source, paths)?;
    maintenance
        .take()
        .ok_or_else(|| "update maintenance lease is absent at commit".to_string())?
        .finish()
}

fn validate_installed_update_generation(
    archive: &Path,
    source: &crate::direct::UpdateSourcePlan,
) -> Result<(), String> {
    let (selected_archive, selected_title) =
        crate::installation_lifecycle::selected_generation_paths(archive)?
            .ok_or_else(|| "published update has no selected generation".to_string())?;
    let observed = crate::generation::generation_identity(&selected_archive, &selected_title)
        .map_err(|error| error.to_string())?;
    if observed.generation_id != source.generation_id
        || observed.wiki_db != source.wiki_db
        || observed.content_frontier != source.resulting_content_frontier
        || observed.metadata_frontier != source.resulting_metadata_frontier
    {
        return Err("published update generation does not match its immutable source plan".into());
    }
    Ok(())
}

fn publish_update_commit(
    source: &crate::direct::UpdateSourcePlan,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::CommitReceipt, String> {
    let receipt = update_lifecycle::CommitReceipt {
        schema: update_lifecycle::UPDATE_SCHEMA,
        update_id: source.source_plan_id.clone(),
        old_generation_id: source.base_generation_id.as_str().to_owned(),
        new_generation_id: source.generation_id.as_str().to_owned(),
    };
    persist_json(&paths.commit_receipt(), &receipt)?;
    Ok(receipt)
}

fn finish_update_cleanup(
    archive: &Path,
    scratch: &Path,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<(), String> {
    let selector = update_selector_path(scratch);
    let hardlink_cleanup = if update_hardlink_cleanup_path(&paths.root).exists() {
        match validate_update_hardlink_cleanup_receipt(archive, paths) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                eprintln!(
                    "update committed; deferred hardlink cleanup of {}: {error}",
                    paths.root.display()
                );
                return Ok(());
            }
        }
    } else {
        let cleanup = match build_committed_update_hardlink_cleanup(archive, paths) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                eprintln!(
                    "update committed; deferred hardlink cleanup proof for {}: {error}",
                    paths.root.display()
                );
                return Ok(());
            }
        };
        if let Err(error) = persist_json(
            &update_hardlink_cleanup_path(&paths.root),
            &cleanup.receipt,
        ) {
            eprintln!(
                "update committed; deferred hardlink cleanup journal for {}: {error}",
                paths.root.display()
            );
            return Ok(());
        }
        cleanup
    };
    if let Err(error) = sync_parent(archive) {
        eprintln!("update committed; deferred directory sync: {error}");
    }
    if let Err(error) = clear_owned_update_root(
        &paths.root,
        UpdateCleanupMode::Committed(&hardlink_cleanup),
    ) {
        eprintln!(
            "update committed; deferred cleanup of {}: {error}",
            paths.root.display()
        );
        return Ok(());
    }
    if selector.exists() {
        if let Err(error) = std::fs::remove_file(&selector) {
            eprintln!(
                "update committed; deferred selector cleanup {}: {error}",
                selector.display()
            );
        } else if let Err(error) = sync_parent(&selector) {
            eprintln!("update committed; deferred selector directory sync: {error}");
        }
    }
    Ok(())
}

#[cfg(feature = "serve")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MediaUpdateArgs<'a> {
    run_id: Option<&'a str>,
    dbname: &'a str,
    archive: &'a str,
}

#[cfg(feature = "serve")]
fn parse_media_update_args<'a>(args: &'a [&'a str]) -> Result<MediaUpdateArgs<'a>, String> {
    let (run_id, dbname, archive) = match args {
        [dbname, archive] => (None, *dbname, *archive),
        ["--run-id", run_id, dbname, archive] => (Some(*run_id), *dbname, *archive),
        _ => {
            return Err(
                "usage: wikimak media-update [--run-id ID] <dbname> <archive.swdump>".into(),
            )
        }
    };
    if dbname.is_empty() || archive.is_empty() || run_id.is_some_and(str::is_empty) {
        return Err(
            "usage: wikimak media-update [--run-id ID] <dbname> <archive.swdump>".into(),
        );
    }
    Ok(MediaUpdateArgs {
        run_id,
        dbname,
        archive,
    })
}

#[cfg(feature = "serve")]
fn shared_media_update_target(archive: &Path) -> PathBuf {
    crate::shared_packed_media_path(archive)
}

/// Run the one shared-repository operation used by explicit media updates and
/// optional post-text acquisition.  In particular, the existence of the
/// repository is not a skip condition: the operation must inspect it and add
/// only missing image identities.
#[cfg(feature = "serve")]
fn with_shared_media_repository<T>(
    archive: &Path,
    label: &str,
    operation: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let repository = shared_media_update_target(archive);
    eprintln!(
        "media-update[{label}]: shared repository {}{}",
        repository.display(),
        if repository.exists() {
            " (incremental update)"
        } else {
            " (initial population)"
        }
    );
    operation(&repository)
}

/// Adapter seam for the media crate's incremental packer. Keeping the call
/// boundary here makes explicit and post-text acquisition use exactly the
/// same shared-repository operation.
#[cfg(feature = "serve")]
fn pack_missing_remote_into_shared_repository(
    source: &wikimak_media::remote::RemoteKiwixImageSource,
    repository: &Path,
) -> Result<wikimak_media::kiwix::KiwixPackStats, String> {
    source
        .import_missing(repository)
        .map_err(|error| error.to_string())
}

#[cfg(feature = "serve")]
fn pack_missing_local_into_shared_repository(
    source: &wikimak_media::KiwixImageSource,
    repository: &Path,
) -> Result<wikimak_media::kiwix::KiwixPackStats, String> {
    source
        .import_missing(repository)
        .map_err(|error| error.to_string())
}

#[cfg(feature = "serve")]
fn run_media_update(
    client: &reqwest::blocking::Client,
    dbname: &str,
    archive: &Path,
    run_id: Option<&str>,
    source_override: Option<std::ffi::OsString>,
) -> Result<(), String> {
    let label = run_id.unwrap_or("manual");
    let stats = with_shared_media_repository(archive, label, |repository| {
        let source = source_override.as_deref();
        if source.is_none_or(|value| value == std::ffi::OsStr::new("auto")) {
            let release = wikimak_media::remote::discover_latest(client, dbname)
                .map_err(|error| format!("discover official Kiwix all-maxi release: {error}"))?;
            eprintln!(
                "media-update[{label}]: source {} (ranged requests; ZIM is not saved)",
                release.name
            );
            let source = wikimak_media::remote::RemoteKiwixImageSource::open(
                client.clone(),
                release.url,
            )
            .map_err(|error| format!("open remote Kiwix source: {error}"))?;
            eprintln!(
                "media-update[{label}]: indexed {} image entries from {} source bytes",
                source.len(),
                source.file_size()
            );
            pack_missing_remote_into_shared_repository(&source, repository)
                .map_err(|error| format!("incremental remote media update: {error}"))
        } else {
            let source_path = PathBuf::from(source.unwrap());
            eprintln!(
                "media-update[{label}]: local ZIM source {}",
                source_path.display()
            );
            let source = wikimak_media::KiwixImageSource::open(&source_path)
                .map_err(|error| format!("open local Kiwix source: {error}"))?;
            eprintln!(
                "media-update[{label}]: indexed {} local image entries",
                source.len()
            );
            pack_missing_local_into_shared_repository(&source, repository)
                .map_err(|error| format!("incremental local media update: {error}"))
        }
    })?;
    eprintln!(
        "media-update[{label}]: complete; source entries {}, existing skipped {}, duplicate skipped {}, missing entries added {}, payload bytes {}, storages changed {}, ranged ZIM response-body bytes {}, range attempts {}, retries {}",
        stats.entries_seen,
        stats.entries_skipped_existing,
        stats.entries_skipped_duplicate,
        stats.entries_written,
        stats.bytes_written,
        stats.storages,
        stats.http_bytes,
        stats.http_requests,
        stats.http_retries
    );
    Ok(())
}

#[cfg(feature = "serve")]
fn cmd_media_update(args: MediaUpdateArgs<'_>) -> Result<(), String> {
    let archive = Path::new(args.archive);
    require_absolute_archive(archive)?;
    run_media_update(
        &http_client()?,
        args.dbname,
        archive,
        args.run_id,
        std::env::var_os("SARUN_KIWIX_SOURCE"),
    )
}

#[cfg(feature = "serve")]
fn pack_selected_media(
    client: &reqwest::blocking::Client,
    dbname: &str,
    archive: &Path,
) -> Result<(), String> {
    let Some(source) = std::env::var_os("SARUN_KIWIX_SOURCE") else {
        return Ok(());
    };
    run_media_update(client, dbname, archive, Some("post-text"), Some(source))
}

fn build_full(
    client: &reqwest::blocking::Client,
    config: &wikimak_mediawiki::Config,
    dbname: &str,
    archive: &Path,
    scratch: &Path,
    replace_plan: bool,
    run_id: Option<&str>,
) -> Result<(), String> {
    std::env::set_var("SARUN_MIRROR_DEST", archive);
    std::fs::create_dir_all(scratch).map_err(|error| format!("{}: {error}", scratch.display()))?;
    ensure_direct_tmpdir(scratch)?;
    // Fetch robots.txt once during discovery and leave the result in the
    // resumable build tree for every stage-one helper to consume.
    std::env::set_var("SARUN_WIKIMEDIA_ROBOTS_CACHE", scratch.join("robots-cache"));
    let _lock = MirrorBuildLock::acquire(scratch)?;
    if let Some(outcome) = crate::installation_lifecycle::recover(archive)? {
        if outcome.candidate_cleanup_pending {
            eprintln!("previous installed generation has redundant candidate-link cleanup pending");
        } else if outcome.cleanup_pending {
            eprintln!("previous installed generation cleanup remains pending");
        }
    }
    if replace_plan {
        clear_obsolete_active_update(scratch)?;
        clear_owned_mirror_scratch(scratch, Some(archive))?;
    }
    let inspected = inspect_build_for_start(scratch, archive)?;
    let plan = match inspected {
        crate::build_lifecycle::BuildState::Unplanned => {
            let plan =
                crate::direct::discover_direct_build_plan(client, config, dbname, &|message| {
                    eprintln!("{message}")
                })
                .map_err(|error| error.to_string())?;
            crate::build_lifecycle::commit_plan(scratch, &plan)
                .map_err(|error| error.to_string())?;
            plan
        }
        state => {
            let plan = state
                .plan()
                .expect("every non-unplanned state carries its plan")
                .clone();
            if plan.wiki_db != dbname {
                return Err(format!(
                    "{} belongs to {}, not {dbname}",
                    scratch.join("plan.json").display(),
                    plan.wiki_db,
                ));
            }
            eprintln!(
                "resuming snapshot {} from {} with {} source targets",
                plan.content_snapshot,
                state.phase(),
                plan.target_count(),
            );
            plan
        }
    };
    if let Some(run_id) = run_id {
        crate::progress_projection::begin_run(scratch, &plan, run_id)?;
    } else {
        crate::progress_projection::initialize(scratch, &plan)?;
    }
    if let Some(url) = plan.first_source_url() {
        // This is deliberately done by the importing process, before any
        // stage-one helpers start.  A resumed plan therefore cannot race
        // several workers into independently requesting robots.txt.
        wikimak_mediawiki::prepare_robots(client, url)
            .map_err(|error| error.to_string())?;
    }
    let progress_state = crate::build_lifecycle::inspect_build(scratch, Some(&plan.plan_id))
        .map_err(|error| error.to_string())?;
    let reusable = progress_state
        .targets()
        .iter()
        .filter(|target| matches!(target.state, crate::build_lifecycle::TargetState::Ready(_)))
        .inspect(|target| {
            crate::progress_projection::mark_target_completed(
                scratch,
                &plan,
                target.kind.as_str(),
                target.index,
            );
        })
        .count();
    if reusable != 0 {
        eprintln!(
            "resuming with {reusable}/{} source targets already durable",
            plan.target_count(),
        );
    }
    prepare_build_tools(scratch)?;
    write_stage_one_makefile(scratch, &plan)?;
    run_build_make(scratch, &plan)?;
    let ready = crate::build_lifecycle::inspect_build(scratch, Some(&plan.plan_id))
        .map_err(|error| error.to_string())?;
    if !matches!(ready, crate::build_lifecycle::BuildState::Ready { .. }) {
        return Err(format!(
            "resumable build stopped in durable state {}",
            ready.phase()
        ));
    }
    let built = scratch.join("archive.swdump");
    let install_outcome = install_built_archive(built, archive)?;
    #[cfg(feature = "serve")]
    if let Err(error) = pack_selected_media(client, dbname, archive) {
        eprintln!(
            "text generation is installed; optional media remains pending: {error}"
        );
    }
    if let Err(error) = clear_owned_mirror_scratch(scratch, Some(archive)) {
        eprintln!(
            "text generation is installed; scratch cleanup remains pending at {}: {error}",
            scratch.display()
        );
    } else if install_outcome.candidate_cleanup_pending {
        eprintln!(
            "text generation is installed; candidate links remain under installation cleanup authority"
        );
    }
    Ok(())
}

fn require_state_preserving_event(
    phase: update_lifecycle::UpdatePhase,
    event: update_lifecycle::UpdateEvent,
) -> Result<(), String> {
    match update_lifecycle::transition(phase, event) {
        update_lifecycle::TransitionDecision::NoOp => Ok(()),
        decision => Err(format!(
            "update lifecycle classified {event:?} in {phase:?} as {decision:?}"
        )),
    }
}

fn update_step<T>(
    phase: update_lifecycle::UpdatePhase,
    result: Result<T, String>,
) -> Result<T, String> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            require_state_preserving_event(
                phase,
                update_lifecycle::UpdateEvent::WorkerFailed,
            )?;
            Err(error)
        }
    }
}

fn acquire_update_maintenance(
    archive: &Path,
    source: &crate::direct::UpdateSourcePlan,
) -> Result<crate::installation_lifecycle::UpdateMaintenanceGuard, String> {
    crate::installation_lifecycle::begin_update_maintenance(
        archive,
        source.base_generation_id.as_str(),
        source.generation_id.as_str(),
        &source.source_plan_id,
    )
    .map_err(|error| error.to_string())
}

fn cmd_fetch(dbname: &str, archive: &str, run_id: Option<&str>) -> Result<(), String> {
    let archive = Path::new(archive);
    require_absolute_archive(archive)?;
    std::env::set_var("SARUN_MIRROR_DEST", archive);
    let scratch = ensure_mirror_scratch(archive)?;
    // Install destination-local request coordination before constructing or
    // using any discovery/network client.
    ensure_direct_tmpdir(&scratch)?;
    let client = http_client()?;
    let config = wikimedia_config()?;
    let _lock = MirrorBuildLock::acquire(&scratch)?;
    let (selected, active_update) = recover_fetch_entry(archive, &scratch)?;
    if selected.is_none() && active_update.is_none() {
        drop(_lock);
        return build_full(&client, &config, dbname, archive, &scratch, false, run_id);
    }
    std::env::set_var("SARUN_WIKIMEDIA_ROBOTS_CACHE", scratch.join("robots-cache"));
    let overlap_days = 3;
    let compression = mirror_compression();

    let mut resumed = match active_update {
        Some((active, paths)) => match load_update_plan(&active, &paths, dbname) {
            Ok(source) => {
                let base = if let Some(receipt) = update_lifecycle::read_receipt::<
                    update_lifecycle::PreservedBaseReceipt,
                >(&paths.base_receipt())
                .map_err(|error| error.to_string())?
                {
                    receipt.generation
                } else {
                    let (selected_archive, selected_title) =
                        crate::installation_lifecycle::selected_generation_paths(archive)?
                        .ok_or_else(|| {
                            format!("{} has no installed generation", archive.display())
                        })?;
                    crate::generation::generation_identity(&selected_archive, &selected_title)
                        .map_err(|error| error.to_string())?
                };
                Some((active, paths, source, base))
            }
            Err(error) => {
                abandon_invalid_update(&scratch, Some(&paths), error)?;
                None
            }
        },
        None => None,
    };
    let (_active, mut paths, mut source, mut base, resuming) =
        if let Some((active, paths, source, base)) = resumed.take() {
            (active, paths, source, base, true)
        } else {
            let (active, paths, source, base) = create_update_plan(
                &client,
                &config,
                dbname,
                archive,
                &scratch,
                overlap_days,
                compression,
            )?;
            (active, paths, source, base, false)
        };

    if resuming {
        let installed_id = installed_generation_id(archive)?;
        match update_lifecycle::inspect_update(&paths, installed_id.as_str()) {
            Ok(state) => require_state_preserving_event(
                state.phase(),
                update_lifecycle::UpdateEvent::ResumeRequested,
            )?,
            Err(error) if error.path == paths.base_site_info() => {
                let missing = match std::fs::symlink_metadata(paths.base_site_info()) {
                    Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => true,
                    Ok(_) => false,
                    Err(io_error) => {
                        return Err(format!(
                            "{}: cannot inspect missing-checkpoint recovery boundary: {io_error}",
                            paths.base_site_info().display()
                        ));
                    }
                };
                if !missing {
                    return Err(error.to_string());
                }
                reconstruct_missing_base_site_info(&source, &paths)?;
                let repaired = update_lifecycle::inspect_update(&paths, installed_id.as_str())
                    .map_err(|reinspect| reinspect.to_string())?;
                require_state_preserving_event(
                    repaired.phase(),
                    update_lifecycle::UpdateEvent::ResumeRequested,
                )?;
            }
            Err(error) => {
                abandon_invalid_update(&scratch, Some(&paths), error)?;
                let replacement = create_update_plan(
                    &client,
                    &config,
                    dbname,
                    archive,
                    &scratch,
                    overlap_days,
                    compression,
                )?;
                paths = replacement.1;
                source = replacement.2;
                base = replacement.3;
            }
        }
    }
    let mut maintenance = None;
    loop {
        let installed_id = installed_generation_id(archive)?;
        let state = update_lifecycle::inspect_update(&paths, installed_id.as_str())
            .map_err(|error| error.to_string())?;
        let phase = state.phase();
        let action = state.next_action();
        if matches!(
            action,
                update_lifecycle::UpdateAction::PublishRange
                | update_lifecycle::UpdateAction::PublishInventory
                | update_lifecycle::UpdateAction::PublishIndex
                | update_lifecycle::UpdateAction::InstallGeneration
                | update_lifecycle::UpdateAction::PublishCommit
        ) && maintenance.is_none()
        {
            maintenance = Some(acquire_update_maintenance(archive, &source)?);
        }
        match (action, state) {
            (
                update_lifecycle::UpdateAction::PublishTail,
                update_lifecycle::UpdateState::Planned(_),
            ) => {
                update_step(
                    phase,
                    ensure_update_tail(&client, &source, &paths, run_id),
                )?;
            }
            (
                update_lifecycle::UpdateAction::PreserveBase,
                update_lifecycle::UpdateState::TailReady(_, _),
            ) => {
                update_step(
                    phase,
                    ensure_preserved_base(archive, &source, &base, &paths),
                )?;
            }
            (
                update_lifecycle::UpdateAction::PublishBaseSiteInfo,
                update_lifecycle::UpdateState::BasePreserved(_, _, _),
            ) => {
                update_step(phase, ensure_base_site_info(&source, &paths))?;
            }
            (
                update_lifecycle::UpdateAction::PublishRangePlan,
                update_lifecycle::UpdateState::BaseSiteInfoReady {
                    tail,
                    preserved_base: preserved,
                    site_info,
                    ..
                },
            ) => {
                update_step(
                    phase,
                    ensure_range_plan(
                        &source,
                        &tail,
                        &preserved.generation,
                        &site_info,
                        &paths,
                    ),
                )?;
            }
            (
                update_lifecycle::UpdateAction::PublishRange,
                update_lifecycle::UpdateState::ApplyingRanges {
                    tail,
                    ranges,
                    site_info,
                    completed,
                    ..
                },
            ) => {
                eprintln!(
                    "applying update ranges from durable slot {}/{}",
                    completed + 1,
                    ranges.slots.len()
                );
                update_step(
                    phase,
                    apply_update_ranges(
                        &source,
                        &tail,
                        &ranges,
                        &site_info,
                        &paths,
                        maintenance
                            .as_ref()
                            .ok_or_else(|| "update maintenance lease is absent".to_string())?,
                    ),
                )?;
            }
            (
                update_lifecycle::UpdateAction::PublishInventory,
                update_lifecycle::UpdateState::ApplyingRanges { ranges, .. },
            ) => {
                eprintln!(
                    "all {} range candidates are durable; assembling inventory",
                    ranges.slots.len()
                );
                let inventory =
                    update_step(phase, ensure_candidate_archive(&ranges, &paths))?;
                eprintln!(
                    "candidate inventory durable with {} data pieces",
                    inventory.segments.len()
                );
            }
            (
                update_lifecycle::UpdateAction::PublishIndex,
                update_lifecycle::UpdateState::CandidateComplete {
                    site_info,
                    ranges,
                    ..
                },
            ) => {
                eprintln!(
                    "assembling title/frame index from durable piece metadata"
                );
                let (_, title_entries) = update_step(
                    phase,
                    ensure_candidate_index(
                        archive,
                        &source,
                        &site_info,
                        &ranges,
                        &paths,
                    ),
                )?;
                eprintln!("prepared {title_entries} title intervals");
            }
            (
                update_lifecycle::UpdateAction::InstallGeneration,
                update_lifecycle::UpdateState::IndexReady { .. },
            ) => {
                update_step(
                    phase,
                    install_update_generation(
                        archive,
                        &source,
                        &paths,
                    ),
                )?;
            }
            (
                update_lifecycle::UpdateAction::PublishCommit,
                update_lifecycle::UpdateState::Installed { .. },
            ) => {
                update_step(
                    phase,
                    commit_update_generation(archive, &source, &paths, &mut maintenance),
                )?;
                eprintln!(
                    "committed Wikipedia generation {}",
                    source.generation_id.as_str()
                );
            }
            (
                update_lifecycle::UpdateAction::Cleanup,
                update_lifecycle::UpdateState::Committed(_),
            ) => {
                if crate::installation_lifecycle::update_maintenance_active(archive)? {
                    acquire_update_maintenance(archive, &source)?.finish()?;
                }
                update_step(
                    phase,
                    finish_update_cleanup(
                        archive,
                        &scratch,
                        &paths,
                    ),
                )?;
                #[cfg(feature = "serve")]
                if let Err(error) = pack_selected_media(&client, dbname, archive) {
                    eprintln!("text update committed; optional media update failed: {error}");
                }
                return Ok(());
            }
            (action, state) => {
                return Err(format!(
                    "update action {action:?} does not match inspected state {state:?}"
                ));
            }
        }
    }
}

fn cmd_refresh_full(dbname: &str, archive: &str, run_id: Option<&str>) -> Result<(), String> {
    let archive = Path::new(archive);
    require_absolute_archive(archive)?;
    let scratch = ensure_mirror_scratch(archive)?;
    ensure_direct_tmpdir(&scratch)?;
    build_full(
        &http_client()?,
        &wikimedia_config()?,
        dbname,
        archive,
        &scratch,
        true,
        run_id,
    )
}

fn abandon_invalid_update(
    scratch: &Path,
    paths: Option<&update_lifecycle::UpdatePaths>,
    detail: impl std::fmt::Display,
) -> Result<(), String> {
    eprintln!(
        "discarding invalid temporary update state at {}; installed generation preserved ({detail})",
        scratch.display()
    );
    if let Some(paths) = paths {
        clear_owned_update_root(&paths.root, UpdateCleanupMode::Conservative)?;
    }
    clear_owned_update_selector(scratch)
}

fn clear_obsolete_active_update(scratch: &Path) -> Result<(), String> {
    match load_active_update(scratch) {
        Ok(Some((_active, paths))) => {
            clear_owned_update_root(&paths.root, UpdateCleanupMode::Conservative)?;
            clear_owned_update_selector(scratch)?;
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!(
                "discarding invalid temporary update selector at {}; preserving unrecognized update roots ({error})",
                update_selector_path(scratch).display()
            );
            clear_owned_update_selector(scratch)?;
        }
    }
    Ok(())
}

/// Explicitly abandon an invalid destination-local build/update tree.  This
/// is intentionally narrower than `refresh-full`: valid resumable work and
/// committed installed generations are left untouched. Invalid private
/// update output is discarded rather than adopted by a compatibility path.
fn cmd_reset(dbname: &str, archive: &str) -> Result<(), String> {
    let archive = Path::new(archive);
    let scratch = prepare_direct_archive(archive)?;
    let _lock = MirrorBuildLock::acquire(&scratch)?;
    // Validate the publication selector before touching scratch.  A malformed
    // selector means the installed generation itself needs repair, not a
    // construction reset.
    let _ = crate::installation_lifecycle::serving_pair(archive)?;

    match crate::build_lifecycle::inspect_build(&scratch, None) {
        Ok(crate::build_lifecycle::BuildState::Unplanned) => {}
        Ok(state) => {
            return Err(format!(
                "{} contains valid {} state; use fetch/resume or refresh-full",
                scratch.display(),
                state.phase()
            ));
        }
        Err(error) => abandon_invalid_build(&scratch, archive, &error)?,
    }

    match load_active_update(&scratch) {
        Ok(None) => {}
        Ok(Some((active, paths))) => {
            let installed = installed_generation_id(archive)?;
            match update_lifecycle::inspect_update(&paths, installed.as_str()) {
                Ok(state) => {
                    return Err(format!(
                        "update {} is in valid {:?} state; use fetch/resume",
                        active.update_id,
                        state.phase()
                    ));
                }
                Err(error) => {
                    eprintln!(
                        "discarding invalid temporary update state at {}; installed generation preserved ({error})",
                        paths.root.display()
                    );
                    clear_owned_update_root(&paths.root, UpdateCleanupMode::Conservative)?;
                    clear_owned_update_selector(&scratch)?;
                }
            }
        }
        Err(error) => {
            eprintln!(
                "discarding invalid temporary update selector at {}; installed generation preserved ({error})",
                update_selector_path(&scratch).display()
            );
            clear_owned_update_selector(&scratch)?;
        }
    }
    eprintln!("reset complete for {dbname} at {}", scratch.display());
    Ok(())
}

fn cmd_discover(dbname: &str) -> Result<(), String> {
    let run = wikimak_mediawiki::discover(&http_client()?, dbname)
        .map_err(|error| error.to_string())?;
    println!("run {} ({:?}), {} parts", run.date, run.source, run.parts.len());
    for part in run.parts {
        println!("{}\t{} bytes", part.filename, part.size_bytes);
    }
    Ok(())
}

#[cfg(feature = "serve")]
fn cmd_serve(path: &str, addr: &str, packed_media: Option<&str>) -> Result<(), String> {
    let mirror_path = PathBuf::from(path);
    let media_root = mirror_path.with_extension("media");
    crate::serve::serve_archive(
        mirror_path,
        addr.to_owned(),
        media_root,
        None,
        packed_media.map(PathBuf::from),
    )
}

#[cfg(feature = "serve")]
fn cmd_kiwix_pack(zim: &str, output: &str) -> Result<(), String> {
    let source = wikimak_media::KiwixImageSource::open(zim)
        .map_err(|error| error.to_string())?;
    eprintln!(
        "wikimak kiwix-pack: indexed {} image entries from {}",
        source.len(),
        zim,
    );
    let stats = source
        .pack(output)
        .map_err(|error| error.to_string())?;
    println!(
        "{} entries written, {} bytes in {} storages",
        stats.entries_written, stats.bytes_written, stats.storages
    );
    Ok(())
}

fn cmd_siteinfo(api_url: &str, output: &str) -> Result<(), String> {
    crate::siteinfo::fetch_siteinfo_archive(&http_client()?, api_url, output)
        .map_err(|error| error.to_string())
}

fn cmd_title_index(archive: &str, output: &str, generation_id: &str) -> Result<(), String> {
    let generation_id =
        crate::generation::GenerationId::parse(generation_id).map_err(|error| error.to_string())?;
    let entries = crate::title_index::build(archive, output, &generation_id)
        .map_err(|error| error.to_string())?;
    println!("{entries} title intervals");
    Ok(())
}

fn cmd_backrefs(archive: &str, titles: &str, output: &str) -> Result<(), String> {
    let stats = crate::backrefs::build(archive, titles, output)
        .map_err(|error| error.to_string())?;
    let bytes = std::fs::metadata(output)
        .map_err(|error| format!("{output}: {error}"))?
        .len();
    println!(
        "{} sets, {} source pages, {} static edges, {} unresolved static edges, {} unresolved dynamic targets, {} redirects, {} users with edits, {} user-page memberships, {} bytes",
        stats.sets,
        stats.source_pages,
        stats.extracted_static_edges,
        stats.unresolved_static_edges,
        stats.unresolved_dynamic_targets,
        stats.redirect_pages,
        stats.users_with_edits,
        stats.user_page_memberships,
        bytes,
    );
    Ok(())
}

fn compression(level: &str) -> Result<crate::archive::CompressionSettings, String> {
    Ok(crate::archive::CompressionSettings {
        level: level
            .parse()
            .map_err(|error| format!("zstd level: {error}"))?,
        ..crate::archive::CompressionSettings::default()
    })
}

fn positive_size(value: &str, name: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|error| format!("{name}: {error}"))
        .and_then(|value| {
            if value == 0 {
                Err(format!("{name} must be positive"))
            } else {
                Ok(value)
            }
        })
}

enum ArchiveInput {
    File(std::fs::File),
    Set(crate::archive_set::ArchiveSetReader),
}

impl std::io::Read for ArchiveInput {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File(input) => input.read(bytes),
            Self::Set(input) => input.read(bytes),
        }
    }
}

impl std::io::Seek for ArchiveInput {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        match self {
            Self::File(input) => input.seek(position),
            Self::Set(input) => input.seek(position),
        }
    }
}

fn cmd_raw_repack(
    input: &str,
    output: &str,
    frame_target: usize,
    compression: crate::archive::CompressionSettings,
    raw_input: bool,
) -> Result<(), String> {
    let output_path = Path::new(output);
    let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    let (records, frames) = if raw_input {
        let source =
            std::fs::File::open(input).map_err(|error| format!("{input}: {error}"))?;
        let (_, frames, records) = crate::archive::import_raw_record_stream(
            BufReader::new(source),
            temporary.as_file_mut(),
            frame_target,
            compression,
        )
        .map_err(|error| error.to_string())?;
        (records, frames)
    } else {
        let records =
            crate::archive::export_raw_record_stream(input, temporary.as_file_mut())
                .map_err(|error| error.to_string())?;
        (records, 0)
    };
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temporary
        .persist(output_path)
        .map_err(|error| format!("{output}: {}", error.error))?;
    if raw_input {
        println!("{records} raw records, {frames} archive frames");
    } else {
        println!("{records} raw records");
    }
    Ok(())
}

fn cmd_repack(args: &[&str]) -> Result<(), String> {
    let [input, output, frame_target, level, options @ ..] = args else {
        return Err(
            "repack wants <input> <output> <frame-bytes> <zstd-level> \
             [--dictionary-bytes N | --ref-prefix-bytes N --sample-bytes N | \
              --raw-output | --raw-input]"
                .into(),
        );
    };
    let frame_target = positive_size(frame_target, "frame bytes")?;
    let compression = compression(level)?;
    #[derive(Clone, Copy)]
    enum Reference {
        None,
        Dictionary(usize),
        RefPrefix { bytes: usize, sample_bytes: usize },
        RawOutput,
        RawInput,
    }
    let reference = match options {
        [] => Reference::None,
        ["--dictionary-bytes", bytes] => {
            Reference::Dictionary(positive_size(bytes, "dictionary bytes")?)
        }
        ["--ref-prefix-bytes", bytes, "--sample-bytes", sample_bytes]
        | ["--sample-bytes", sample_bytes, "--ref-prefix-bytes", bytes] => {
            Reference::RefPrefix {
                bytes: positive_size(bytes, "reference-prefix bytes")?,
                sample_bytes: positive_size(sample_bytes, "sample bytes")?,
            }
        }
        ["--raw-output"] => Reference::RawOutput,
        ["--raw-input"] => Reference::RawInput,
        _ => return Err("unknown repack options".into()),
    };
    if matches!(reference, Reference::RawOutput | Reference::RawInput) {
        return cmd_raw_repack(
            input,
            output,
            frame_target,
            compression,
            matches!(reference, Reference::RawInput),
        );
    }
    let input_path = Path::new(input);
    let input_file = if input_path.is_dir() {
        ArchiveInput::Set(
            crate::archive_set::ArchiveSetReader::open(input_path)
                .map_err(|error| format!("{input}: {error}"))?,
        )
    } else {
        ArchiveInput::File(
            std::fs::File::open(input).map_err(|error| format!("{input}: {error}"))?,
        )
    };
    let output_path = Path::new(output);
    let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    let result = match reference {
        Reference::Dictionary(bytes) => crate::archive::repack_with_dictionary(
            BufReader::new(input_file),
            temporary.as_file_mut(),
            frame_target,
            compression,
            bytes,
        ),
        Reference::RefPrefix {
            bytes,
            sample_bytes,
        } => crate::archive::repack_with_ref_prefix(
            BufReader::new(input_file),
            temporary.as_file_mut(),
            frame_target,
            compression,
            sample_bytes,
            bytes,
        ),
        Reference::None => crate::archive::repack(
            BufReader::new(input_file),
            temporary.as_file_mut(),
            frame_target,
            compression,
        ),
        Reference::RawOutput | Reference::RawInput => unreachable!("returned above"),
    };
    let (_, stats) = result.map_err(|error| error.to_string())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temporary
        .persist(output_path)
        .map_err(|error| format!("{output}: {}", error.error))?;
    println!(
        "{} records, {} frames, dictionary {} bytes, refPrefix {} bytes from {} sample bytes",
        stats.records,
        stats.output_frames,
        stats.dictionary_bytes,
        stats.ref_prefix_bytes,
        stats.sample_bytes,
    );
    Ok(())
}

fn cmd_merge(args: &[&str]) -> Result<(), String> {
    let [output, frame_target, level, inputs @ ..] = args else {
        return Err("merge wants <output> <frame-bytes> <zstd-level> <input>...".into());
    };
    if inputs.is_empty() {
        return Err("merge needs at least one input".into());
    }
    let frame_target = positive_size(frame_target, "frame bytes")?;
    let input_paths = inputs.iter().map(PathBuf::from).collect::<Vec<_>>();
    let output_path = Path::new(output);
    let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    let (_, frames, records) = crate::archive::merge_many_archives_with_compression(
        &input_paths,
        temporary.as_file_mut(),
        frame_target,
        compression(level)?,
    )
    .map_err(|error| error.to_string())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temporary
        .persist(output_path)
        .map_err(|error| format!("{output}: {}", error.error))?;
    println!("{records} records, {frames} frames");
    Ok(())
}

fn cmd_inspect(path: &str) -> Result<(), String> {
    let (frame_target, frames, complete) =
        crate::archive::index_file(path).map_err(|error| error.to_string())?;
    if !complete {
        return Err("archive has no clean completion marker".into());
    }
    let records = frames.iter().map(|frame| frame.info.records).sum::<u64>();
    let compressed = frames
        .iter()
        .map(|frame| frame.info.compressed_bytes)
        .sum::<u64>();
    println!(
        "{records} records, {} frames, {compressed} compressed bytes, {frame_target}-byte frame target",
        frames.len(),
    );
    Ok(())
}

#[cfg(feature = "serve")]
fn cmd_verify(destination: &str) -> Result<(), String> {
    let archive = crate::archive_browse::ArchiveBrowseIndex::open_installed(destination)
        .map_err(|error| format!("open installed archive {destination}: {error}"))?;
    let report = archive.verify_deterministic_sample().map_err(|error| {
        format!(
            "verify installed archive {destination} generation {}: {error}",
            archive.generation_id().as_str()
        )
    })?;
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|error| format!("encode verify report: {error}"))?
    );
    Ok(())
}

fn cmd_build_node(args: &[&str]) -> Result<(), String> {
    let [root, plan, kind, index, bz2_workers, active_decode_budget] = args else {
        return Err(
            "build-node wants <root> <plan.json> <content|history> <index> <bz2-workers> <active-decode-budget>"
                .into(),
        );
    };
    let root = Path::new(root);
    let plan = crate::direct::read_direct_build_plan(&root.join(plan))
        .map_err(|error| error.to_string())?;
    let index = index
        .parse::<usize>()
        .map_err(|error| format!("target index: {error}"))?;
    let bz2_workers = positive_size(bz2_workers, "bzip2 workers")?;
    let active_decode_budget =
        positive_size(active_decode_budget, "active bzip2 decode budget")?;
    wikimak_mediawiki::configure_active_decode_budget(active_decode_budget)
        .map_err(|error| format!("configure active bzip2 decode budget: {error}"))?;
    crate::direct::materialize_direct_build_node(
        &http_client()?,
        root,
        &plan,
        kind,
        index,
        bz2_workers,
        &|message| eprintln!("[{kind}-{index:06}] {message}"),
    )
    .map_err(|error| format!("[{kind}-{index:06}] {error}"))
}

fn cmd_build_stage_two(root: &str, plan: &str) -> Result<(), String> {
    let root = Path::new(root);
    let plan = crate::direct::read_direct_build_plan(&root.join(plan))
        .map_err(|error| error.to_string())?;
    write_stage_two_makefile(root, &plan)
}

fn cmd_build_assemble(root: &str, plan: &str) -> Result<(), String> {
    let root = Path::new(root);
    let plan = crate::direct::read_direct_build_plan(&root.join(plan))
        .map_err(|error| error.to_string())?;
    crate::direct::assemble_direct_build(root, &plan, &|message| eprintln!("{message}"))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn arm_parent_watchdog() {
    let Some(expected) = std::env::var("SARUN_MIRROR_PARENT_PID")
        .ok()
        .and_then(|value| value.parse::<libc::pid_t>().ok())
    else {
        return;
    };
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if unsafe { libc::getppid() } != expected {
                eprintln!("wikimak: supervising sarun engine exited; stopping mirror job");
                unsafe {
                    let group = libc::getpgrp();
                    if group == libc::getpid() {
                        libc::kill(-group, libc::SIGTERM);
                    }
                    libc::_exit(1);
                }
            }
        }
    });
}

const WIKIMAK_USAGE: &str = "usage: wikimak discover <dbname>\n\
     \x20      wikimak fetch <dbname> <archive.swdump>\n\
     \x20      wikimak refresh-full <dbname> <archive.swdump>\n\
     \x20      wikimak reset <dbname> <archive.swdump>\n\
     \x20      wikimak media-update [--run-id ID] <dbname> <archive.swdump>\n\
     \x20      wikimak serve <archive.swdump> [addr] [--packed-media <directory>]\n\
     \x20      wikimak kiwix-pack <source.zim> <output-directory>\n\
     \x20      wikimak siteinfo <api-url> <output.swdump>\n\
     \x20      wikimak title-index <archive.swdump> <output.swtitle> <generation-id>\n\
     \x20      wikimak backrefs <archive.swdump> <titles.swtitle> <output.swrefs>\n\
     \x20      wikimak backrefs-task <installed-archive.swdump>\n\
     \x20      wikimak repack <input> <output> <frame-bytes> <zstd-level> [--dictionary-bytes N | --ref-prefix-bytes N --sample-bytes N | --raw-output | --raw-input]\n\
     \x20      wikimak merge <output> <frame-bytes> <zstd-level> <input>...\n\
     \x20      wikimak inspect <archive.swdump>\n\
     \x20      wikimak verify <installed-archive>";

pub fn cli_main(args: &[String]) -> i32 {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    if matches!(args.as_slice(), ["-h"] | ["--help"]) {
        println!("{WIKIMAK_USAGE}");
        return 0;
    }
    if matches!(
        args.as_slice(),
        ["fetch" | "refresh-full", _, _]
            | ["fetch" | "refresh-full", "--run-id", _, _, _]
    ) {
        arm_parent_watchdog();
    }
    #[cfg(feature = "serve")]
    if matches!(
        args.as_slice(),
        ["media-update", _, _] | ["media-update", "--run-id", _, _, _]
    ) {
        arm_parent_watchdog();
    }
    let result = match args.as_slice() {
        ["discover", dbname] => cmd_discover(dbname),
        ["fetch", dbname, archive] => cmd_fetch(dbname, archive, None),
        ["refresh-full", dbname, archive] => cmd_refresh_full(dbname, archive, None),
        ["reset", dbname, archive] => cmd_reset(dbname, archive),
        ["fetch", "--run-id", run_id, dbname, archive] => cmd_fetch(dbname, archive, Some(run_id)),
        ["refresh-full", "--run-id", run_id, dbname, archive] => {
            cmd_refresh_full(dbname, archive, Some(run_id))
        }
        #[cfg(feature = "serve")]
        ["media-update", arguments @ ..] => {
            parse_media_update_args(arguments).and_then(cmd_media_update)
        }
        #[cfg(feature = "serve")]
        ["serve", archive] => cmd_serve(archive, "127.0.0.1:8642", None),
        #[cfg(feature = "serve")]
        ["serve", archive, "--packed-media", packed] => {
            cmd_serve(archive, "127.0.0.1:8642", Some(packed))
        }
        #[cfg(feature = "serve")]
        ["serve", archive, addr, "--packed-media", packed] => {
            cmd_serve(archive, addr, Some(packed))
        }
        #[cfg(feature = "serve")]
        ["serve", archive, addr] => cmd_serve(archive, addr, None),
        #[cfg(feature = "serve")]
        ["kiwix-pack", zim, output] => cmd_kiwix_pack(zim, output),
        ["siteinfo", api_url, output] => cmd_siteinfo(api_url, output),
        ["title-index", archive, output, generation_id] => {
            cmd_title_index(archive, output, generation_id)
        }
        ["backrefs", archive, titles, output] => cmd_backrefs(archive, titles, output),
        ["backrefs-task", destination] => cmd_backrefs_task(destination),
        ["repack", arguments @ ..] => cmd_repack(arguments),
        ["merge", arguments @ ..] => cmd_merge(arguments),
        ["inspect", archive] => cmd_inspect(archive),
        #[cfg(feature = "serve")]
        ["verify", destination] => cmd_verify(destination),
        ["build-node", arguments @ ..] => cmd_build_node(arguments),
        ["build-stage2", root, plan] => cmd_build_stage_two(root, plan),
        ["build-assemble", root, plan] => cmd_build_assemble(root, plan),
        _ => Err(WIKIMAK_USAGE.into()),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("wikimak: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static DIRECT_TMPDIR_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn legacy_replaced_page_receipt_is_resumable_but_forces_full_backref_bootstrap() {
        let slot = update_lifecycle::RangeSlot {
            index: 0,
            kind: crate::archive::EntityKind::Page as u8,
            first_id: 1,
            last_id: 10,
            base_segment_id: "base".into(),
            base_name: "pages.swdump-part".into(),
            base_bytes: 42,
            candidate_id: "candidate".into(),
        };
        let receipt = update_lifecycle::RangeCandidateReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: "update".into(),
            base_generation_id: "base-generation".into(),
            tail_id: "tail".into(),
            slot_index: 0,
            candidate_id: "candidate".into(),
            kind: slot.kind,
            first_id: slot.first_id,
            last_id: slot.last_id,
            base_segment_id: slot.base_segment_id.clone(),
            selection: update_lifecycle::RangeSelection::Replaced {
                segment_id: "candidate".into(),
                name: "pages.swdump-part".into(),
                bytes: 43,
                frames: 1,
                records: 1,
                frame_directory_name: "candidate.swframe".into(),
                frame_directory_format: crate::frame_directory::FORMAT_VERSION,
                frame_directory_bytes: 0,
                first_entity: update_lifecycle::EntityBound { kind: slot.kind, id: 1 },
                last_entity: update_lifecycle::EntityBound { kind: slot.kind, id: 1 },
            },
            consumed_first: None,
            consumed_last: None,
            tail_bytes_read: 0,
            base_bytes_read: 0,
            base_frame_bytes_copied: 0,
            base_frame_bytes_decoded: 0,
            candidate_bytes_written: 43,
            title_projection_name: None,
            title_projection_bytes: 0,
            title_projection_records: 0,
            backref_delta_name: None,
            backref_delta_bytes: 0,
            backref_delta_records: 0,
            tail_cursor: update_lifecycle::TailCursorReceipt {
                frame_offset: None,
                record_ordinal: 0,
            },
            complete: true,
        };
        let paths = update_lifecycle::UpdatePaths::new("/Volumes/Elements/legacy-resume-test");
        let mut legacy_json = serde_json::to_value(&receipt).unwrap();
        let legacy_object = legacy_json.as_object_mut().unwrap();
        legacy_object.remove("backref_delta_name");
        legacy_object.remove("backref_delta_bytes");
        legacy_object.remove("backref_delta_records");
        let receipt: update_lifecycle::RangeCandidateReceipt =
            serde_json::from_value(legacy_json).unwrap();
        assert!(update_lifecycle::validate_range_backref_delta(&paths, &slot, &receipt).is_ok());
        assert!(has_legacy_page_delta_gap(
            &update_lifecycle::RangePlanReceipt {
                schema: update_lifecycle::UPDATE_SCHEMA,
                update_id: "update".into(),
                base_generation_id: "base-generation".into(),
                tail_id: "tail".into(),
                slots: vec![slot],
            },
            &[receipt],
        ));
    }

    #[test]
    fn named_range_backref_delta_receipt_requires_exact_strict_artifact() {
        let root = tempfile::tempdir().unwrap();
        let paths = update_lifecycle::UpdatePaths::new(root.path());
        let slot = update_lifecycle::RangeSlot {
            index: 0,
            kind: crate::archive::EntityKind::Page as u8,
            first_id: 1,
            last_id: 10,
            base_segment_id: "base".into(),
            base_name: "pages.swdump-part".into(),
            base_bytes: 42,
            candidate_id: "candidate".into(),
        };
        let mut receipt = update_lifecycle::RangeCandidateReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: "update".into(),
            base_generation_id: "base-generation".into(),
            tail_id: "tail".into(),
            slot_index: 0,
            candidate_id: "candidate".into(),
            kind: slot.kind,
            first_id: slot.first_id,
            last_id: slot.last_id,
            base_segment_id: slot.base_segment_id.clone(),
            selection: update_lifecycle::RangeSelection::Replaced {
                segment_id: "candidate".into(),
                name: "pages.swdump-part".into(),
                bytes: 43,
                frames: 1,
                records: 1,
                frame_directory_name: "candidate.swframe".into(),
                frame_directory_format: crate::frame_directory::FORMAT_VERSION,
                frame_directory_bytes: 0,
                first_entity: update_lifecycle::EntityBound {
                    kind: slot.kind,
                    id: 1,
                },
                last_entity: update_lifecycle::EntityBound {
                    kind: slot.kind,
                    id: 1,
                },
            },
            consumed_first: None,
            consumed_last: None,
            tail_bytes_read: 0,
            base_bytes_read: 0,
            base_frame_bytes_copied: 0,
            base_frame_bytes_decoded: 0,
            candidate_bytes_written: 43,
            title_projection_name: None,
            title_projection_bytes: 0,
            title_projection_records: 0,
            backref_delta_name: Some("candidate.swrefdelta".into()),
            backref_delta_bytes: 0,
            backref_delta_records: 0,
            tail_cursor: update_lifecycle::TailCursorReceipt {
                frame_offset: None,
                record_ordinal: 0,
            },
            complete: true,
        };
        let delta = crate::backrefs::combine_projection_deltas(&[]).unwrap();
        assert!(!delta.is_empty());
        receipt.backref_delta_bytes = delta.len() as u64;
        let path = paths.range_backref_delta(&slot.candidate_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &delta).unwrap();
        assert!(update_lifecycle::validate_range_backref_delta(&paths, &slot, &receipt).is_ok());
        let range_plan = update_lifecycle::RangePlanReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: "update".into(),
            base_generation_id: "base-generation".into(),
            tail_id: "tail".into(),
            slots: vec![slot.clone()],
        };
        assert!(read_range_backref_deltas(&range_plan, &[receipt.clone()], &paths).is_ok());
        let mut replaced_header = delta.clone();
        replaced_header[12..20].copy_from_slice(&1_u64.to_le_bytes());
        std::fs::write(&path, replaced_header).unwrap();
        assert!(read_range_backref_deltas(&range_plan, &[receipt.clone()], &paths).is_err());
        std::fs::write(&path, &delta).unwrap();

        std::fs::remove_file(&path).unwrap();
        assert!(update_lifecycle::validate_range_backref_delta(&paths, &slot, &receipt).is_err());

        let truncated = &delta[..delta.len() - 1];
        std::fs::write(&path, truncated).unwrap();
        assert!(update_lifecycle::validate_range_backref_delta(&paths, &slot, &receipt).is_err());

        let mut malformed = delta.clone();
        malformed[0] ^= 1;
        std::fs::write(&path, malformed).unwrap();
        assert!(update_lifecycle::validate_range_backref_delta(&paths, &slot, &receipt).is_err());

        let mut trailing = delta.clone();
        trailing.push(0);
        receipt.backref_delta_bytes = trailing.len() as u64;
        std::fs::write(&path, trailing).unwrap();
        assert!(update_lifecycle::validate_range_backref_delta(&paths, &slot, &receipt).is_err());

        receipt.backref_delta_bytes = delta.len() as u64 + 1;
        std::fs::write(&path, &delta).unwrap();
        assert!(update_lifecycle::validate_range_backref_delta(&paths, &slot, &receipt).is_err());
    }

    #[test]
    fn wikimedia_origin_override_is_normalized_and_bounded() {
        assert_eq!(
            wikimedia_config_from(" http://127.0.0.1:8123/dumps/ ")
                .unwrap()
                .base_url,
            "http://127.0.0.1:8123/dumps"
        );
        for invalid in [
            "file:///Volumes/Elements/dumps",
            "https://user:secret@example.invalid/dumps",
            "https://example.invalid/dumps?branch=latest",
            "https://example.invalid/dumps#latest",
            "not a URL",
        ] {
            assert!(
                wikimedia_config_from(invalid).is_err(),
                "accepted invalid origin {invalid:?}"
            );
        }
    }

    #[test]
    fn wikimak_help_is_successful_without_starting_work() {
        assert_eq!(cli_main(&["--help".to_owned()]), 0);
    }

    #[test]
    fn initial_install_queues_backrefs_without_blocking_text_publication() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("build");
        std::fs::create_dir(&scratch).unwrap();
        let destination = root.path().join("installed/ruwiki.swdump");
        let (archive, _, _) = completed_candidate(&scratch);

        install_built_archive(archive, &destination).unwrap();

        let (_, title_index) = crate::installation_lifecycle::selected_generation_paths(&destination)
            .unwrap()
            .unwrap();
        let sidecar = backrefs_path(&destination);
        assert!(!sidecar.exists(), "large relation build is not on install's critical path");
        assert!(backrefs_task_path(&destination).is_file());
        cmd_backrefs_task(destination.to_str().unwrap()).unwrap();
        assert!(sidecar.is_file());
        crate::backrefs::BackrefIndex::open_for_title_index(&sidecar, &title_index).unwrap();
        assert!(!backrefs_task_path(&destination).exists());
    }

    #[test]
    fn incremental_install_replaces_valid_stale_backrefs() {
        let root = tempfile::tempdir().unwrap();
        let first_scratch = root.path().join("first");
        let second_scratch = root.path().join("second");
        std::fs::create_dir(&first_scratch).unwrap();
        std::fs::create_dir(&second_scratch).unwrap();
        let destination = root.path().join("installed/ruwiki.swdump");
        let (first_archive, first_title, _) = completed_candidate(&first_scratch);
        crate::installation_lifecycle::install(first_archive, first_title, &destination).unwrap();
        schedule_backrefs_task(&destination).unwrap();
        cmd_backrefs_task(destination.to_str().unwrap()).unwrap();
        let old_sidecar = std::fs::read(backrefs_path(&destination)).unwrap();

        let (second_archive, second_title, second_id) =
            completed_candidate_with_seed(&second_scratch, b"backrefs incremental replacement");
        crate::installation_lifecycle::install(second_archive, second_title, &destination).unwrap();
        schedule_backrefs_task(&destination).unwrap();

        let (_, selected_title) = crate::installation_lifecycle::selected_generation_paths(&destination)
            .unwrap()
            .unwrap();
        let sidecar = backrefs_path(&destination);
        assert!(
            crate::backrefs::BackrefIndex::open_for_title_index(&sidecar, &selected_title).is_err(),
            "the old sidecar is ignored until the explicit post-publication task runs"
        );
        cmd_backrefs_task(destination.to_str().unwrap()).unwrap();
        crate::backrefs::BackrefIndex::open_for_title_index(&sidecar, &selected_title).unwrap();
        assert_ne!(std::fs::read(&sidecar).unwrap(), old_sidecar);
        assert_eq!(
            crate::title_index::TitleIndex::open(&selected_title)
                .unwrap()
                .generation_id()
                .as_str(),
            second_id
        );
    }

    #[test]
    fn published_candidate_sidecar_removes_only_owned_stale_full_scan_task() {
        let (root, destination, paths, prepared) = prepared_sidecar_fixture(false);
        let task_path = backrefs_task_path(&destination);
        persist_json(
            &task_path,
            &BackrefsTask {
                schema: BACKREFS_TASK_SCHEMA,
                generation_id: crate::generation::GenerationId::from_plan_bytes(b"stale-task")
                    .as_str()
                    .into(),
            },
        )
        .unwrap();

        publish_update_backrefs(&destination, &paths).unwrap();

        assert!(!task_path.exists());
        let live = backrefs_path(&destination);
        assert!(same_proven_hardlink(&paths.candidate_backrefs(), &live).unwrap());
        assert_eq!(prepared.backrefs_records, crate::backrefs::BackrefIndex::open(
            &live
        ).unwrap().logical_count());
        drop(root);
    }

    #[test]
    fn normal_live_fast_path_requires_candidate_hardlink_identity() {
        let (_root, destination, paths, _prepared) = prepared_sidecar_fixture(true);
        let candidate = paths.candidate_backrefs();
        let live = backrefs_path(&destination);
        assert!(!same_proven_hardlink(&candidate, &live).unwrap());

        publish_update_backrefs(&destination, &paths).unwrap();

        assert!(same_proven_hardlink(&candidate, &live).unwrap());
    }

    #[test]
    fn legacy_installed_selector_can_recover_from_live_raw_sidecar_only() {
        let (_root, destination, paths, prepared) = prepared_sidecar_fixture(true);
        std::fs::remove_file(paths.candidate_backrefs()).unwrap();
        let mut legacy = prepared;
        legacy.backrefs_name.clear();
        legacy.backrefs_bytes = 0;
        legacy.backrefs_records = 0;
        persist_json(&paths.prepared_generation(), &legacy).unwrap();

        publish_update_backrefs(&destination, &paths).unwrap();

        assert!(backrefs_path(&destination).is_file());
        assert!(!paths.candidate_backrefs().exists());
    }

    #[test]
    fn rawless_candidate_sidecar_is_bootstrapped_before_preparation_returns() {
        let root = elements_tempdir("sarun-wikimak-rawless-candidate-");
        let input = root.path().join("input");
        std::fs::create_dir(&input).unwrap();
        let (archive, title, _) = completed_candidate(&input);
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("update"));
        std::fs::create_dir_all(paths.candidate_archive().parent().unwrap()).unwrap();
        hard_link_archive(&archive, &paths.candidate_archive()).unwrap();
        hard_link_file(&title, &paths.candidate_index()).unwrap();
        let source = test_update_source_plan(crate::generation::GenerationId::from_plan_bytes(
            b"rawless-base",
        ));
        let plan = lifecycle_plan(&source);
        let site_info = crate::archive::SiteInfoRecord {
            site_name: "Test".into(),
            db_name: "testwiki".into(),
            base: String::new(),
            generator: String::new(),
            case: "first-letter".into(),
            language: "en".into(),
            rtl: false,
            server: String::new(),
            script_path: String::new(),
            namespaces: Vec::new(),
            interwiki: Vec::new(),
            magic_words: Vec::new(),
        };
        std::fs::create_dir_all(paths.candidate_backrefs().parent().unwrap()).unwrap();
        crate::backrefs::write_test_user_only_sidecar(
            paths.candidate_backrefs(),
            paths.candidate_index(),
        )
        .unwrap();
        let before = BOOTSTRAP_BACKREF_PREPARATIONS.load(std::sync::atomic::Ordering::SeqCst);
        let (name, _, _) = ensure_candidate_backrefs(
            Path::new("/Volumes/Elements/sarun-user-validation-20260812/rawless-base"),
            &update_lifecycle::BaseSiteInfoCheckpoint::new(&plan, site_info.clone()),
            &site_info,
            &update_lifecycle::RangePlanReceipt {
                schema: update_lifecycle::UPDATE_SCHEMA,
                update_id: source.source_plan_id.clone(),
                base_generation_id: source.base_generation_id.as_str().into(),
                tail_id: String::new(),
                slots: Vec::new(),
            },
            &[],
            &paths,
        )
        .unwrap();

        assert_eq!(name, "backrefs.swrefs");
        assert!(crate::backrefs::BackrefIndex::open_for_title_index(
            paths.candidate_backrefs(),
            paths.candidate_index(),
        )
        .unwrap()
        .has_raw_postings());
        assert_eq!(
            BOOTSTRAP_BACKREF_PREPARATIONS.load(std::sync::atomic::Ordering::SeqCst),
            before + 1
        );
    }

    #[test]
    fn retry_claims_a_durable_pending_backref_without_rebuilding() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("build");
        std::fs::create_dir(&scratch).unwrap();
        let destination = root.path().join("installed/ruwiki.swdump");
        let (archive, title, _) = completed_candidate(&scratch);
        crate::installation_lifecycle::install(archive, title, &destination).unwrap();
        schedule_backrefs_task(&destination).unwrap();
        let (selected_archive, selected_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let generation_id = crate::title_index::TitleIndex::open(&selected_title)
            .unwrap()
            .generation_id()
            .as_str()
            .to_owned();
        let pending = pending_backrefs_path(&destination, &generation_id);
        build_backrefs_pending(&selected_archive, &selected_title, &pending).unwrap();

        cmd_backrefs_task(destination.to_str().unwrap()).unwrap();
        cmd_backrefs_task(destination.to_str().unwrap()).unwrap();
        assert!(backrefs_path(&destination).is_file());
        crate::backrefs::BackrefIndex::open_for_title_index(
            backrefs_path(&destination),
            selected_title,
        )
        .unwrap();
    }

    #[test]
    fn unvalidated_foreign_backrefs_are_not_replaced() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("build");
        std::fs::create_dir(&scratch).unwrap();
        let destination = root.path().join("installed/ruwiki.swdump");
        let (archive, title, _) = completed_candidate(&scratch);
        crate::installation_lifecycle::install(archive, title, &destination).unwrap();
        let sidecar = backrefs_path(&destination);
        std::fs::write(&sidecar, b"foreign sidecar").unwrap();

        schedule_backrefs_task(&destination).unwrap();
        let error = cmd_backrefs_task(destination.to_str().unwrap()).unwrap_err();

        assert!(error.contains("unvalidated backref artifact"));
        assert_eq!(std::fs::read(sidecar).unwrap(), b"foreign sidecar");
        assert!(backrefs_task_path(&destination).exists());
    }

    #[cfg(feature = "serve")]
    #[test]
    fn media_update_arguments_accept_optional_run_id() {
        assert_eq!(
            parse_media_update_args(&["lvwiki", "/Volumes/Elements/lvwiki.swdump"]),
            Ok(MediaUpdateArgs {
                run_id: None,
                dbname: "lvwiki",
                archive: "/Volumes/Elements/lvwiki.swdump",
            })
        );
        assert_eq!(
            parse_media_update_args(&[
                "--run-id",
                "run-42",
                "ruwiki",
                "/Volumes/Elements/ruwiki.swdump"
            ]),
            Ok(MediaUpdateArgs {
                run_id: Some("run-42"),
                dbname: "ruwiki",
                archive: "/Volumes/Elements/ruwiki.swdump",
            })
        );
        assert!(parse_media_update_args(&["lvwiki"]).is_err());
        assert!(parse_media_update_args(&["--run-id", "", "lvwiki", "x"]).is_err());
    }

    #[cfg(feature = "serve")]
    #[test]
    fn existing_shared_repository_is_passed_to_incremental_operation() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("lvwiki.swdump");
        let repository = shared_media_update_target(&archive);
        std::fs::create_dir_all(&repository).unwrap();
        let mut observed = None;
        with_shared_media_repository(&archive, "test", |path| {
            observed = Some(path.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(observed.as_deref(), Some(repository.as_path()));
    }

    #[test]
    fn wikipedia_mirror_compression_remains_level_nine() {
        assert_eq!(mirror_compression().level, 9);
    }

    #[test]
    fn stage_one_geometry_is_bounded_for_small_and_large_inputs() {
        let cases = [
            (0, 1, 1, 1, 1),
            (1, 1, 1, 1, 1),
            (2, 1, 1, 1, 1),
            (3, 2, 1, 1, 1),
            (3, 3, 1, 2, 2),
            (2, 10, 2, 8, 8),
            (3, 10, 3, 7, 7),
            (10, 10, 5, 5, 5),
            (usize::MAX, 10, 5, 5, 5),
            (1, usize::MAX, 1, usize::MAX - 1, usize::MAX - 1),
            (10, usize::MAX, 10, usize::MAX - 10, usize::MAX - 10),
        ];
        for (target_count, cpu_budget, make_jobs, bz2_workers, active_decode_budget) in cases {
            let geometry = stage_one_geometry(target_count, cpu_budget, true);
            assert_eq!(geometry.make_jobs, make_jobs);
            assert_eq!(geometry.bz2_workers, bz2_workers);
            assert_eq!(geometry.active_decode_budget, active_decode_budget);
            let normalized_cpu_budget = cpu_budget.max(1);
            let active_targets = target_count.min((normalized_cpu_budget / 2).max(1));
            if cpu_budget >= 2 && active_targets != 0 {
                assert!(
                    active_targets.saturating_add(geometry.active_decode_budget)
                        <= normalized_cpu_budget,
                    "parser plus active decoder geometry oversubscribes target_count={target_count}, cpu_budget={cpu_budget}"
                );
            }
        }
    }

    #[test]
    fn ten_cpu_stage_one_fans_out_locally_without_changing_http_admission() {
        let geometry = stage_one_geometry(10, 10, true);
        assert_eq!(geometry.make_jobs, 5);
        assert_eq!(geometry.bz2_workers, 5);
        assert_eq!(geometry.active_decode_budget, 5);
        assert!(geometry.make_jobs > 3);
        assert_eq!(geometry.make_jobs + geometry.active_decode_budget, 10);
        // Stage-one only controls make recipes and their decoder budgets. The
        // three-body Wikimedia admission limit is owned by mediawiki and is
        // intentionally not an input to this geometry.
    }

    #[test]
    fn standalone_stage_one_keeps_a_static_per_process_share() {
        let geometry = stage_one_geometry(10, 10, false);
        assert_eq!(geometry.make_jobs, 5);
        assert_eq!(geometry.bz2_workers, 1);
        assert_eq!(geometry.active_decode_budget, 1);
        assert_eq!(geometry.make_jobs * (1 + geometry.bz2_workers), 10);
    }

    fn elements_tempdir(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in("/Volumes/Elements/sarun-user-validation-20260812/tmp")
            .unwrap()
    }

    fn test_update_source_plan(
        base_generation_id: crate::generation::GenerationId,
    ) -> crate::direct::UpdateSourcePlan {
        use sha2::Digest;

        let mut source = crate::direct::UpdateSourcePlan {
            schema: 1,
            source_plan_id: String::new(),
            generation_id: crate::generation::GenerationId::from_plan_bytes(&[]),
            base_generation_id,
            wiki_db: "testwiki".into(),
            base_content_frontier: "2024-01-01".into(),
            base_metadata_frontier: "2024-01-01".into(),
            overlap_days: 1,
            frame_target: 128,
            compression: crate::archive::CompressionSettings::default().into(),
            content_runs: Vec::new(),
            history_snapshot: "2024-01-02".into(),
            history_files: Vec::new(),
            resulting_content_frontier: "2024-01-02".into(),
            resulting_metadata_frontier: "2024-01-02".into(),
        };
        let mut canonical = source.clone();
        canonical.generation_id = crate::generation::GenerationId::from_plan_bytes(&[]);
        let bytes = serde_json::to_vec(&canonical).unwrap();
        source.source_plan_id = hex::encode(sha2::Sha256::digest(bytes));
        let mut generation_identity = b"wikipedia-update-generation\0".to_vec();
        generation_identity.extend_from_slice(source.base_generation_id.as_str().as_bytes());
        generation_identity.push(0);
        generation_identity.extend_from_slice(source.source_plan_id.as_bytes());
        source.generation_id =
            crate::generation::GenerationId::from_plan_bytes(&generation_identity);
        crate::direct::validate_update_source_plan(&source).unwrap();
        source
    }

    fn test_generation(
        root: &Path,
        name: &str,
        generation_id: &crate::generation::GenerationId,
        content_frontier: &str,
        metadata_frontier: &str,
    ) -> (PathBuf, PathBuf) {
        let archive = root.join(format!("{name}.swdump"));
        let title = archive.with_extension("swtitle");
        let output = crate::archive_set::ArchiveSetOutput::new_in(root, 1 << 20).unwrap();
        let mut writer = crate::archive::ArchiveWriter::with_ref_prefix(
            output,
            128,
            crate::archive::CompressionSettings::default(),
            b"lifecycle test reference",
        )
        .unwrap();
        writer
            .write(&crate::archive::Record::PageState {
                page_id: 1,
                timestamp_micros: 1,
                title: "Main Page".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        writer
            .write(&crate::archive::Record::Manifest {
                timestamp_micros: 1,
                manifest: crate::archive::ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: content_frontier.into(),
                    metadata_snapshot: metadata_frontier.into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&crate::archive::Record::SiteInfo {
                timestamp_micros: 1,
                site_info: crate::archive::SiteInfoRecord {
                    site_name: "Lifecycle test".into(),
                    db_name: "testwiki".into(),
                    base: "https://example.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: "https://example.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (output, _) = writer.finish().unwrap();
        output.finish().unwrap().persist(&archive).unwrap();
        crate::title_index::build(&archive, &title, generation_id).unwrap();
        (archive, title)
    }

    fn write_complete_unreceipted_tail(
        source: &crate::direct::UpdateSourcePlan,
        paths: &update_lifecycle::UpdatePaths,
    ) -> Vec<u8> {
        std::fs::create_dir_all(paths.tail_archive().parent().unwrap()).unwrap();
        let mut writer = crate::archive::ArchiveWriter::new(
            std::fs::File::create(paths.tail_archive()).unwrap(),
            128,
        )
        .unwrap();
        writer
            .write(&crate::archive::Record::PageState {
                page_id: 1,
                timestamp_micros: 2,
                title: "Updated Main Page".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        let (file, frames) = writer.finish().unwrap();
        file.sync_all().unwrap();
        drop(file);
        let bytes = std::fs::metadata(paths.tail_archive()).unwrap().len();
        let stats = crate::direct::UpdateArchiveStats {
            output_bytes: bytes,
            output_frames: frames,
            output_records: 1,
            ..Default::default()
        };
        let tail_id = tail_id(source, &stats);
        crate::frame_directory::write_from_archive(
            paths.tail_archive(),
            paths.tail_frame_directory(),
            crate::generation::GenerationId::parse(&tail_id)
                .unwrap()
                .to_bytes()
                .unwrap(),
        )
        .unwrap();
        std::fs::read(paths.tail_archive()).unwrap()
    }

    #[test]
    fn unreceipted_complete_tail_reconstructs_receipt_without_rewriting_archive() {
        let root = elements_tempdir("sarun-wikimak-tail-");
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("updates/tail"));
        let source = test_update_source_plan(
            crate::generation::GenerationId::from_plan_bytes(b"tail-base"),
        );
        let before = write_complete_unreceipted_tail(&source, &paths);

        let receipt = ensure_update_tail(&http_client().unwrap(), &source, &paths, None).unwrap();

        assert_eq!(std::fs::read(paths.tail_archive()).unwrap(), before);
        assert!(receipt.complete);
        assert_eq!(receipt.bytes, before.len() as u64);
        assert!(paths.tail_receipt().is_file());
    }

    #[test]
    fn unreceipted_incomplete_tail_is_retained_and_update_fails_without_mutation() {
        let root = elements_tempdir("sarun-wikimak-tail-invalid-");
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("updates/tail"));
        let source = test_update_source_plan(
            crate::generation::GenerationId::from_plan_bytes(b"tail-base-invalid"),
        );
        std::fs::create_dir_all(paths.tail_archive().parent().unwrap()).unwrap();
        std::fs::write(paths.tail_archive(), b"partial tail").unwrap();
        let before = std::fs::read(paths.tail_archive()).unwrap();

        let error =
            ensure_update_tail(&http_client().unwrap(), &source, &paths, None).unwrap_err();

        assert!(error.contains("refusing to replace it"));
        assert_eq!(std::fs::read(paths.tail_archive()).unwrap(), before);
        assert!(!paths.tail_receipt().exists());
    }

    #[test]
    fn fetch_entry_recovers_update_marker_before_serving_check() {
        let root = elements_tempdir("sarun-wikimak-fetch-recovery-");
        let destination = root.path().join("wiki.swdump");
        let base_id = crate::generation::GenerationId::from_plan_bytes(b"fetch-base");
        let (base_archive, base_title) =
            test_generation(root.path(), "base", &base_id, "2024-01-01", "2024-01-01");
        crate::installation_lifecycle::install(base_archive, base_title, &destination).unwrap();
        let (selected_archive, selected_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let base = crate::generation::generation_identity(&selected_archive, &selected_title)
            .unwrap();
        let source = test_update_source_plan(base.generation_id.clone());
        let scratch = ensure_mirror_scratch(&destination).unwrap();
        let paths =
            update_lifecycle::UpdatePaths::new(update_root(&scratch, &source.source_plan_id));
        std::fs::create_dir_all(&paths.root).unwrap();
        persist_json(&paths.source_plan(), &source).unwrap();
        persist_json(&paths.plan(), &lifecycle_plan(&source)).unwrap();
        persist_json(
            &update_selector_path(&scratch),
            &update_lifecycle::ActiveUpdate {
                schema: update_lifecycle::UPDATE_SCHEMA,
                update_id: source.source_plan_id.clone(),
                base_generation_id: source.base_generation_id.as_str().into(),
            },
        )
        .unwrap();
        let marker = crate::installation_lifecycle::begin_update_maintenance(
            &destination,
            source.base_generation_id.as_str(),
            source.generation_id.as_str(),
            &source.source_plan_id,
        )
        .unwrap();
        drop(marker);

        let _lock = MirrorBuildLock::acquire(&scratch).unwrap();
        let (selected, active) = recover_fetch_entry(&destination, &scratch).unwrap();

        assert!(selected.is_some());
        assert_eq!(active.unwrap().0.update_id, source.source_plan_id);
        assert!(crate::installation_lifecycle::serving_pair(&destination).is_err());
    }

    #[test]
    fn legacy_partial_update_reconstructs_missing_site_info_only_for_matching_base() {
        let root = elements_tempdir("sarun-wikimak-legacy-site-info-");
        let base_id = crate::generation::GenerationId::from_plan_bytes(b"legacy-site-info-base");
        let (base_archive, base_title) =
            test_generation(root.path(), "base", &base_id, "2024-01-01", "2024-01-01");
        let destination = root.path().join("installed/wiki.swdump");
        crate::installation_lifecycle::install(base_archive, base_title, &destination).unwrap();
        let (selected_archive, selected_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let base = crate::generation::generation_identity(&selected_archive, &selected_title).unwrap();
        let source = test_update_source_plan(base.generation_id.clone());
        let scratch = ensure_mirror_scratch(&destination).unwrap();
        let paths = update_lifecycle::UpdatePaths::new(update_root(
            &scratch,
            &source.source_plan_id,
        ));
        std::fs::create_dir_all(&paths.root).unwrap();
        persist_json(&paths.source_plan(), &source).unwrap();
        persist_json(&paths.plan(), &lifecycle_plan(&source)).unwrap();
        ensure_preserved_base(&destination, &source, &base, &paths).unwrap();
        let tail = write_complete_unreceipted_tail(&source, &paths);
        let tail_receipt = ensure_update_tail(&http_client().unwrap(), &source, &paths, None).unwrap();
        assert!(!tail.is_empty());
        let site_info = ensure_base_site_info(&source, &paths).unwrap();
        let range_plan = ensure_range_plan(
            &source,
            &tail_receipt,
            &base,
            &site_info,
            &paths,
        )
        .unwrap();
        assert!(!range_plan.slots.is_empty());
        std::fs::remove_file(paths.base_site_info()).unwrap();

        let error = update_lifecycle::inspect_update(&paths, base.generation_id.as_str())
            .unwrap_err();
        assert_eq!(error.path, paths.base_site_info());
        assert!(!paths.base_site_info().exists());

        let repaired = ensure_base_site_info(&source, &paths).unwrap();
        assert_eq!(repaired.site_info().db_name, "testwiki");
        assert!(matches!(
            update_lifecycle::inspect_update(&paths, base.generation_id.as_str()).unwrap(),
            update_lifecycle::UpdateState::ApplyingRanges { completed: 0, .. }
        ));

        std::fs::remove_file(paths.base_site_info()).unwrap();
        let wrong_source = test_update_source_plan(crate::generation::GenerationId::from_plan_bytes(
            b"wrong-legacy-site-info-base",
        ));
        let wrong = reconstruct_missing_base_site_info(&wrong_source, &paths).unwrap_err();
        assert!(wrong.contains("generation") || wrong.contains("base"));
        assert!(!paths.base_site_info().exists());
    }

    fn completed_candidate(scratch: &Path) -> (PathBuf, PathBuf, String) {
        completed_candidate_with_seed(scratch, b"pending candidate cleanup")
    }

    fn completed_candidate_with_seed(
        scratch: &Path,
        generation_seed: &[u8],
    ) -> (PathBuf, PathBuf, String) {
        let archive = scratch.join("archive.swdump");
        let title = archive.with_extension("swtitle");
        let output = crate::archive_set::ArchiveSetOutput::new_in(scratch, 1 << 20).unwrap();
        let mut writer = crate::archive::ArchiveWriter::with_ref_prefix(
            output,
            128,
            crate::archive::CompressionSettings::default(),
            b"candidate cleanup fixture reference",
        )
        .unwrap();
        writer
            .write(&crate::archive::Record::Manifest {
                timestamp_micros: 1,
                manifest: crate::archive::ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "candidate".into(),
                    metadata_snapshot: "candidate".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&crate::archive::Record::SiteInfo {
                timestamp_micros: 1,
                site_info: crate::archive::SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "testwiki".into(),
                    base: "https://example.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: "https://example.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (output, _) = writer.finish().unwrap();
        output.finish().unwrap().persist(&archive).unwrap();
        let generation_id = crate::generation::GenerationId::from_plan_bytes(generation_seed);
        crate::title_index::build(&archive, &title, &generation_id).unwrap();
        (archive, title, generation_id.as_str().to_owned())
    }

    fn prepared_sidecar_fixture(
        copy_live: bool,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        update_lifecycle::UpdatePaths,
        update_lifecycle::PreparedGenerationReceipt,
    ) {
        let root = elements_tempdir("sarun-wikimak-prepared-sidecar-");
        let input = root.path().join("input");
        std::fs::create_dir(&input).unwrap();
        let (candidate_archive, candidate_title, _) = completed_candidate(&input);
        let destination = root.path().join("installed/wiki.swdump");
        crate::installation_lifecycle::install(
            candidate_archive,
            candidate_title,
            &destination,
        )
        .unwrap();
        let (selected_archive, selected_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("update"));
        std::fs::create_dir_all(paths.candidate_backrefs().parent().unwrap()).unwrap();
        crate::backrefs::build(
            &selected_archive,
            &selected_title,
            paths.candidate_backrefs(),
        )
        .unwrap();
        if copy_live {
            std::fs::copy(paths.candidate_backrefs(), backrefs_path(&destination)).unwrap();
        }
        let index = crate::backrefs::BackrefIndex::open_for_title_index(
            paths.candidate_backrefs(),
            &selected_title,
        )
        .unwrap();
        let prepared = update_lifecycle::PreparedGenerationReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: crate::generation::GenerationId::from_plan_bytes(b"prepared-update")
                .as_str()
                .into(),
            base_generation_id: crate::generation::GenerationId::from_plan_bytes(b"prepared-base")
                .as_str()
                .into(),
            generation_id: crate::title_index::TitleIndex::open(&selected_title)
                .unwrap()
                .generation_id()
                .as_str()
                .into(),
            archive_name: "archive.swdump".into(),
            index_name: "archive.swtitle".into(),
            index_bytes: std::fs::metadata(&selected_title).unwrap().len(),
            backrefs_name: "backrefs.swrefs".into(),
            backrefs_bytes: std::fs::metadata(paths.candidate_backrefs()).unwrap().len(),
            backrefs_records: index.logical_count(),
        };
        persist_json(&paths.prepared_generation(), &prepared).unwrap();
        ensure_mirror_scratch(&destination).unwrap();
        (root, destination, paths, prepared)
    }

    #[test]
    fn real_sparse_page_update_uses_incremental_backrefs_and_matches_full_bootstrap() {
        use crate::archive::{
            ArchiveRecordReader, ArchiveWriter, CompressionSettings, ManifestRecord, Record,
            SiteInfoRecord, StreamingArchiveWriter,
        };

        let root = elements_tempdir("sarun-wikimak-incremental-backrefs-");
        let base_input = root.path().join("base-input");
        std::fs::create_dir(&base_input).unwrap();
        let base_archive = base_input.join("archive.swdump");
        let base_title = base_input.join("archive.swtitle");
        let base_id = crate::generation::GenerationId::from_plan_bytes(b"incremental-backrefs-base");
        let base_output = crate::archive_set::ArchiveSetOutput::new_in(&base_input, 1 << 20).unwrap();
        let mut base_writer = StreamingArchiveWriter::new(
            base_output,
            1,
            CompressionSettings::default(),
            b"incremental-backrefs-reference",
            1,
        )
        .unwrap();
        base_writer
            .write(&Record::PageState {
                page_id: 1,
                timestamp_micros: 100,
                title: "One".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        base_writer.write(&sparse_revision(1, 10, 90, b"{{Old}}".to_vec())).unwrap();
        base_writer
            .write(&Record::PageState {
                page_id: 100,
                timestamp_micros: 100,
                title: "Far page".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        base_writer
            .write(&sparse_revision(100, 1000, 90, b"{{Far}}".to_vec()))
            .unwrap();
        base_writer
            .write(&Record::Manifest {
                timestamp_micros: 1,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "2024-01-01".into(),
                    metadata_snapshot: "2024-01-01".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        base_writer
            .write(&Record::SiteInfo {
                timestamp_micros: 1,
                site_info: SiteInfoRecord {
                    site_name: "Incremental backrefs".into(),
                    db_name: "testwiki".into(),
                    base: "https://example.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: "https://example.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (base_output, _) = base_writer.finish().unwrap();
        base_output.finish().unwrap().persist(&base_archive).unwrap();
        crate::title_index::build(&base_archive, &base_title, &base_id).unwrap();

        let destination = root.path().join("installed/wiki.swdump");
        crate::installation_lifecycle::install(base_archive, base_title, &destination).unwrap();
        let (selected_archive, selected_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let base = crate::generation::generation_identity(&selected_archive, &selected_title).unwrap();
        crate::backrefs::build(&selected_archive, &selected_title, backrefs_path(&destination)).unwrap();
        let source = test_update_source_plan(base.generation_id.clone());
        let scratch = ensure_mirror_scratch(&destination).unwrap();
        let paths = update_lifecycle::UpdatePaths::new(update_root(&scratch, &source.source_plan_id));
        std::fs::create_dir_all(&paths.root).unwrap();
        persist_json(&paths.source_plan(), &source).unwrap();
        persist_json(&paths.plan(), &lifecycle_plan(&source)).unwrap();
        ensure_preserved_base(&destination, &source, &base, &paths).unwrap();
        let base_site_info = ensure_base_site_info(&source, &paths).unwrap();

        let tail_path = paths.tail_archive();
        std::fs::create_dir_all(tail_path.parent().unwrap()).unwrap();
        let mut tail_writer = ArchiveWriter::new(std::fs::File::create(&tail_path).unwrap(), 1).unwrap();
        tail_writer
            .write(&Record::PageState {
                page_id: 1,
                timestamp_micros: 200,
                title: "One moved".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        tail_writer.write(&sparse_revision(1, 20, 190, b"{{New}}".to_vec())).unwrap();
        tail_writer
            .write(&Record::PageState {
                page_id: 2,
                timestamp_micros: 200,
                title: "Two".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        tail_writer
            .write(&sparse_revision(2, 30, 190, b"#REDIRECT [[One moved]]".to_vec()))
            .unwrap();
        tail_writer
            .write(&Record::PageAction {
                entity: crate::archive::EntityKey {
                    kind: crate::archive::EntityKind::Page,
                    id: 100,
                },
                timestamp_micros: 200,
                action: crate::archive::PageActionRecord {
                    log_id: Some(1),
                    tie_sequence: 0,
                    kind: crate::archive::PageActionKind::Delete,
                    performer: crate::archive::PerformerRecord {
                        local_user_id: None,
                        central_user_id: None,
                        historical_name: None,
                        account_class: crate::archive::AccountClass::Unknown,
                    },
                    comment: String::new(),
                    title_at_event: "Far page".into(),
                    namespace_at_event: Some(0),
                    resulting_deleted: None,
                },
            })
            .unwrap();
        let (tail_file, tail_frames) = tail_writer.finish().unwrap();
        tail_file.sync_all().unwrap();
        drop(tail_file);
        let tail_id = crate::generation::GenerationId::from_plan_bytes(b"incremental-backrefs-tail");
        let tail_directory = crate::frame_directory::write_from_archive(
            &tail_path,
            paths.tail_frame_directory(),
            tail_id.to_bytes().unwrap(),
        )
        .unwrap();
        let tail_receipt = update_lifecycle::TailReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: source.source_plan_id.clone(),
            base_generation_id: source.base_generation_id.as_str().into(),
            source_plan_id: source.source_plan_id.clone(),
            tail_id: tail_id.as_str().into(),
            file_name: "records.swdump".into(),
            bytes: std::fs::metadata(&tail_path).unwrap().len(),
            frame_directory_name: "frames.swframe".into(),
            frame_directory_format: crate::frame_directory::FORMAT_VERSION,
            frame_directory_bytes: tail_directory.bytes,
            frames: tail_frames,
            records: tail_directory.records,
            first_entity: tail_directory.first_entity.map(Into::into),
            last_entity: tail_directory.last_entity.map(Into::into),
            complete: true,
        };
        persist_json(&paths.tail_receipt(), &tail_receipt).unwrap();
        let range_plan = ensure_range_plan(
            &source,
            &tail_receipt,
            &base,
            &base_site_info,
            &paths,
        )
        .unwrap();
        let maintenance = crate::installation_lifecycle::begin_update_maintenance(
            &destination,
            source.base_generation_id.as_str(),
            source.generation_id.as_str(),
            &source.source_plan_id,
        )
        .unwrap();
        let directory_reads_before = crate::frame_directory::test_archive_segment_directory_reads();
        arm_update_test_failpoint("after-range-delta-before-receipt");
        assert!(apply_update_ranges_observing(
            &source,
            &tail_receipt,
            &range_plan,
            &base_site_info,
            &paths,
            &maintenance,
            &mut |_| {},
        )
        .is_err());
        clear_update_test_failpoints();
        let page_slot = range_plan
            .slots
            .iter()
            .find(|slot| slot.kind == crate::archive::EntityKind::Page as u8)
            .unwrap();
        assert!(paths.range_backref_delta(&page_slot.candidate_id).is_file());
        assert!(!paths.range_receipt(page_slot.index).exists());
        let mut events = Vec::new();
        apply_update_ranges_observing(
            &source,
            &tail_receipt,
            &range_plan,
            &base_site_info,
            &paths,
            &maintenance,
            &mut |event| events.push(event),
        )
        .unwrap();
        drop(maintenance);
        assert!(events.iter().any(|event| matches!(event, RangeIoEvent::TailRead { .. })));
        assert_eq!(
            crate::frame_directory::test_archive_segment_directory_reads(),
            directory_reads_before,
            "range update must retain write-time segment metadata"
        );

        let receipts = read_range_receipts(&range_plan, &paths).unwrap();
        let deltas = read_range_backref_deltas(&range_plan, &receipts, &paths).unwrap();
        assert!(!deltas.is_empty());
        let page_receipt = receipts
            .iter()
            .find(|receipt| receipt.kind == crate::archive::EntityKind::Page as u8)
            .unwrap();
        assert!(page_receipt.backref_delta_name.is_some());
        assert!(page_receipt.backref_delta_bytes > 0);

        ensure_candidate_archive(&range_plan, &paths).unwrap();
        let incremental_before =
            INCREMENTAL_BACKREF_PREPARATIONS.load(std::sync::atomic::Ordering::SeqCst);
        let bootstrap_before =
            BOOTSTRAP_BACKREF_PREPARATIONS.load(std::sync::atomic::Ordering::SeqCst);
        arm_update_test_failpoint("after-candidate-sidecar-before-prepared-receipt");
        assert!(ensure_candidate_index(
            &destination,
            &source,
            &base_site_info,
            &range_plan,
            &paths,
        )
        .is_err());
        clear_update_test_failpoints();
        assert!(paths.candidate_backrefs().is_file());
        assert!(!paths.prepared_generation().exists());
        let (prepared, _) = ensure_candidate_index(
            &destination,
            &source,
            &base_site_info,
            &range_plan,
            &paths,
        )
        .unwrap();
        assert_eq!(
            INCREMENTAL_BACKREF_PREPARATIONS.load(std::sync::atomic::Ordering::SeqCst),
            incremental_before + 1,
            "candidate preparation must choose the XOR path"
        );
        assert_eq!(
            BOOTSTRAP_BACKREF_PREPARATIONS.load(std::sync::atomic::Ordering::SeqCst),
            bootstrap_before,
            "candidate preparation must not silently fall back to an archive scan"
        );

        let mut candidate_reader = ArchiveRecordReader::open(paths.candidate_archive()).unwrap();
        let mut candidate_page_ids = Vec::new();
        while let Some(record) = candidate_reader.next_record().unwrap() {
            if let Record::PageState { page_id, .. } = record {
                candidate_page_ids.push(page_id);
            }
        }
        assert!(candidate_page_ids.contains(&2), "candidate-only PageState was lost");

        let full_sidecar = root.path().join("full-bootstrap.swrefs");
        crate::backrefs::build(
            paths.candidate_archive(),
            paths.candidate_index(),
            &full_sidecar,
        )
        .unwrap();
        let incremental = crate::backrefs::BackrefIndex::open_for_title_index(
            paths.candidate_backrefs(),
            paths.candidate_index(),
        )
        .unwrap();
        let full = crate::backrefs::BackrefIndex::open_for_title_index(
            &full_sidecar,
            paths.candidate_index(),
        )
        .unwrap();
        assert!(incremental.has_raw_postings());
        assert!(full.has_raw_postings());
        let incremental_sets = incremental.logical_sets_for_test().unwrap();
        assert_eq!(incremental_sets, full.logical_sets_for_test().unwrap());
        assert!(incremental_sets.iter().all(|(key, members)| {
            !matches!(
                key.class,
                crate::backrefs::SetClass::RawNonTopologyUnconditional
                    | crate::backrefs::SetClass::RawNonTopologyPossible
                    | crate::backrefs::SetClass::RawTopologyUnconditional
                    | crate::backrefs::SetClass::RawTopologyPossible
                    | crate::backrefs::SetClass::RawEmittedUnconditional
                    | crate::backrefs::SetClass::RawEmittedPossible
                    | crate::backrefs::SetClass::RawRedirectTarget
            ) || !members.contains(&100)
        }), "later action-only Delete must remove page 100 from raw backrefs");
        assert_eq!(incremental.logical_count(), prepared.backrefs_records);
    }

    #[test]
    fn selector_and_sidecar_commit_cuts_resume_under_maintenance() {
        let root = elements_tempdir("sarun-wikimak-commit-cuts-");
        let base_id = crate::generation::GenerationId::from_plan_bytes(b"commit-cuts-base");
        let (base_archive, base_title) =
            test_generation(root.path(), "base", &base_id, "2024-01-01", "2024-01-01");
        let destination = root.path().join("installed/wiki.swdump");
        crate::installation_lifecycle::install(base_archive, base_title, &destination).unwrap();
        let (selected_archive, selected_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let base = crate::generation::generation_identity(&selected_archive, &selected_title).unwrap();
        let mut source = test_update_source_plan(base.generation_id.clone());
        source.generation_id = crate::generation::GenerationId::from_plan_bytes(b"commit-cuts-candidate");
        let (candidate_archive, candidate_title) = test_generation(
            root.path(),
            "candidate",
            &source.generation_id,
            "2024-01-02",
            "2024-01-02",
        );
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("update"));
        std::fs::create_dir_all(paths.candidate_archive().parent().unwrap()).unwrap();
        hard_link_archive(&candidate_archive, &paths.candidate_archive()).unwrap();
        hard_link_file(&candidate_title, &paths.candidate_index()).unwrap();
        crate::backrefs::build(
            paths.candidate_archive(),
            paths.candidate_index(),
            paths.candidate_backrefs(),
        )
        .unwrap();
        let candidate_backrefs = crate::backrefs::BackrefIndex::open_for_title_index(
            paths.candidate_backrefs(),
            paths.candidate_index(),
        )
        .unwrap();
        let prepared = update_lifecycle::PreparedGenerationReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: source.source_plan_id.clone(),
            base_generation_id: source.base_generation_id.as_str().into(),
            generation_id: source.generation_id.as_str().into(),
            archive_name: "archive.swdump".into(),
            index_name: "archive.swtitle".into(),
            index_bytes: std::fs::metadata(paths.candidate_index()).unwrap().len(),
            backrefs_name: "backrefs.swrefs".into(),
            backrefs_bytes: std::fs::metadata(paths.candidate_backrefs()).unwrap().len(),
            backrefs_records: candidate_backrefs.logical_count(),
        };
        persist_json(&paths.prepared_generation(), &prepared).unwrap();
        let maintenance = crate::installation_lifecycle::begin_update_maintenance(
            &destination,
            source.base_generation_id.as_str(),
            source.generation_id.as_str(),
            &source.source_plan_id,
        )
        .unwrap();
        arm_update_test_failpoint("after-selector-install-before-backrefs");
        assert!(install_update_generation(&destination, &source, &paths).is_err());
        clear_update_test_failpoints();
        assert_eq!(installed_generation_id(&destination).unwrap(), source.generation_id);
        assert!(crate::installation_lifecycle::update_maintenance_active(&destination).unwrap());
        assert!(!backrefs_path(&destination).exists());

        let mut maintenance = Some(maintenance);
        arm_update_test_failpoint("after-backrefs-before-commit");
        assert!(commit_update_generation(&destination, &source, &paths, &mut maintenance).is_err());
        clear_update_test_failpoints();
        assert!(maintenance.is_some());
        assert!(backrefs_path(&destination).is_file());
        assert!(crate::installation_lifecycle::update_maintenance_active(&destination).unwrap());
        assert!(!paths.commit_receipt().exists());

        commit_update_generation(&destination, &source, &paths, &mut maintenance).unwrap();
        assert!(maintenance.is_none());
        assert!(paths.commit_receipt().is_file());
        assert!(!crate::installation_lifecycle::update_maintenance_active(&destination).unwrap());
    }

    struct SparseRangeFixture {
        stats: SparseRangeMergeStats,
        base_frame_bytes: Vec<u64>,
        events: Vec<RangeIoEvent>,
        base_still_exists: bool,
        base_bytes: std::sync::Arc<[u8]>,
        base_frames: Vec<crate::frame_directory::FrameDirectoryEntry>,
        output_bytes: Vec<u8>,
        output_frames: Vec<crate::frame_directory::FrameDirectoryEntry>,
        persisted_frames: Vec<crate::frame_directory::FrameDirectoryEntry>,
        output_records: Vec<crate::archive::Record>,
    }

    fn sparse_revision(
        page_id: u64,
        revision_id: u64,
        timestamp_micros: i64,
        text: Vec<u8>,
    ) -> crate::archive::Record {
        use chrono::TimeZone;

        crate::archive::Record::Revision {
            page_id,
            revision: crate::archive::RevisionRecord {
                meta: crate::RevisionMeta {
                    rev_id: revision_id,
                    parent_id: revision_id.saturating_sub(1),
                    ts: chrono::Utc
                        .timestamp_micros(timestamp_micros)
                        .single()
                        .unwrap(),
                    contributor: crate::ContributorMeta::Named {
                        username: "Sparse merge tester".into(),
                        user_id: 1,
                    },
                    comment: format!("revision {revision_id}"),
                    sha1: String::new(),
                    flags: 0,
                    text_len: text.len() as u64,
                },
                has_text: true,
                text,
                visibility: None,
                history: None,
            },
        }
    }

    fn sparse_frame_bytes(
        bytes: &[u8],
        entry: crate::frame_directory::FrameDirectoryEntry,
    ) -> &[u8] {
        const FRAME_HEADER_BYTES: usize = 64;
        let payload = entry.compressed_offset as usize;
        let end = payload + entry.compressed_bytes as usize;
        &bytes[payload - FRAME_HEADER_BYTES..end]
    }

    fn sparse_range_fixture_records(
        base_records: Vec<crate::archive::Record>,
        update_records: Vec<crate::archive::Record>,
    ) -> SparseRangeFixture {
        sparse_range_fixture_records_at_ordinal(base_records, update_records, 0)
    }

    fn sparse_range_fixture_records_at_ordinal(
        base_records: Vec<crate::archive::Record>,
        update_records: Vec<crate::archive::Record>,
        start_ordinal: u64,
    ) -> SparseRangeFixture {
        let root = tempfile::tempdir().unwrap();
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("update"));
        std::fs::create_dir_all(paths.base_archive().parent().unwrap()).unwrap();
        let prefix = vec![b'p'; 4096];
        let output = crate::archive_set::ArchiveSetOutput::new_in(
            paths.base_archive().parent().unwrap(),
            1 << 20,
        )
        .unwrap();
        let mut writer = crate::archive::StreamingArchiveWriter::new(
            output,
            1,
            crate::archive::CompressionSettings::default(),
            &prefix,
            1,
        )
        .unwrap();
        for record in &base_records {
            writer.write(record).unwrap();
        }
        let (output, _) = writer.finish().unwrap();
        output
            .finish()
            .unwrap()
            .persist(paths.base_archive())
            .unwrap();
        let set =
            crate::archive_set::ArchiveSetReader::open(paths.base_archive()).unwrap();
        let segment = set
            .segments()
            .iter()
            .find(|segment| {
                segment.kind == Some(crate::archive::EntityKind::Page)
            })
            .unwrap()
            .clone();
        let slot = update_lifecycle::RangeSlot {
            index: 0,
            kind: crate::archive::EntityKind::Page as u8,
            first_id: segment.first_id,
            last_id: u64::MAX,
            base_segment_id: crate::generation::GenerationId::from_plan_bytes(
                b"sparse-base",
            )
            .as_str()
            .into(),
            base_name: segment.name,
            base_bytes: segment.bytes,
            candidate_id: crate::generation::GenerationId::from_plan_bytes(
                b"sparse-candidate",
            )
            .as_str()
            .into(),
        };
        std::fs::create_dir_all(paths.tail_archive().parent().unwrap()).unwrap();
        let tail_path = paths.tail_archive();
        let mut tail_writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&tail_path).unwrap(), 1)
                .unwrap();
        for record in &update_records {
            tail_writer.write(record).unwrap();
        }
        tail_writer.finish().unwrap();
        let tail_id = crate::generation::GenerationId::from_plan_bytes(b"sparse-tail");
        crate::frame_directory::write_from_archive(
            &tail_path,
            paths.tail_frame_directory(),
            tail_id.to_bytes().unwrap(),
        )
        .unwrap();
        let tail_directory = std::sync::Arc::new(
            crate::frame_directory::FrameDirectory::open_bound(
                paths.tail_frame_directory(),
                tail_id.to_bytes().unwrap(),
            )
            .unwrap(),
        );
        let tail_reference =
            crate::archive::archive_compression_reference(&tail_path).unwrap();
        let cursor = update_lifecycle::TailCursorReceipt {
            frame_offset: Some(tail_directory.get(0).unwrap().compressed_offset),
            record_ordinal: start_ordinal,
        };
        let mut events = Vec::new();
        let buffered_tail = buffer_tail_slot(
            &paths,
            std::sync::Arc::clone(&tail_directory),
            tail_reference.clone(),
            &cursor,
            &slot,
            crate::archive::EntityKind::Page,
            u64::MAX,
            &mut |event| events.push(event),
        )
        .unwrap()
        .unwrap();
        update_range_input_bytes(
            &slot,
            slot.base_bytes,
            buffered_tail.physical_bytes(),
        )
        .unwrap();
        let base = read_exact_file_range(
            &paths.base_archive().join(&slot.base_name),
            0,
            slot.base_bytes,
            RangeIoEvent::BaseRead {
                bytes: slot.base_bytes,
            },
            &mut |event| events.push(event),
        )
        .unwrap();
        let base_frames = crate::archive::buffered_data_segment_frames(&base).unwrap();
        let base_frame_bytes = base_frames
            .iter()
            .map(|entry| entry.compressed_bytes)
            .collect::<Vec<_>>();
        let output =
            crate::archive_set::ArchiveSetOutput::new_in(root.path(), 1 << 20)
                .unwrap();
        let (output, stats) = merge_sparse_update_range(
            std::sync::Arc::clone(&base),
            &base_frames,
            prefix.into(),
            &buffered_tail,
            std::sync::Arc::clone(&tail_directory),
            tail_reference,
            output,
            None,
            None,
            &mut |_| {},
            &mut |event| events.push(event),
        )
        .unwrap();
        let completed = output.finish().unwrap();
        let output_segment = completed
            .segments
            .iter()
            .find(|segment| segment.kind == Some(crate::archive::EntityKind::Page))
            .unwrap()
            .clone();
        let output_frames = completed
            .frame_directory_entries_for(&output_segment.name)
            .unwrap()
            .to_vec();
        let output_archive = root.path().join("replacement.swdump");
        completed.persist(&output_archive).unwrap();
        let output_bytes = std::fs::read(output_archive.join(&output_segment.name)).unwrap();
        let directory_identity = [0x5a; 32];
        let directory_path = root.path().join("replacement.swframe");
        crate::frame_directory::write_from_archive_entries(
            &output_frames,
            output_segment.bytes,
            &directory_path,
            directory_identity,
        )
        .unwrap();
        let persisted_directory = crate::frame_directory::FrameDirectory::open_bound(
            &directory_path,
            directory_identity,
        )
        .unwrap();
        let persisted_frames = (0..persisted_directory.len())
            .map(|index| persisted_directory.get(index).unwrap())
            .collect::<Vec<_>>();
        let mut output_reader =
            crate::archive::ArchiveRecordReader::open(&output_archive).unwrap();
        let mut output_records = Vec::new();
        while let Some(record) = output_reader.next_record().unwrap() {
            output_records.push(record);
        }
        SparseRangeFixture {
            stats,
            base_frame_bytes,
            events,
            base_still_exists: paths.base_archive().join(&slot.base_name).exists(),
            base_bytes: base,
            base_frames,
            output_bytes,
            output_frames,
            persisted_frames,
            output_records,
        }
    }

    fn sparse_range_fixture(update_page_id: u64) -> SparseRangeFixture {
        let base_records = (1..=5_u64)
            .map(|page_id| crate::archive::Record::PageState {
                page_id,
                timestamp_micros: 100,
                title: format!("Base page {page_id} {}", "x".repeat(256)),
                namespace: None,
                deleted: false,
            })
            .collect();
        let update_records = vec![crate::archive::Record::PageState {
            page_id: update_page_id,
            timestamp_micros: 200,
            title: format!("Updated page {update_page_id}"),
            namespace: None,
            deleted: false,
        }];
        sparse_range_fixture_records(base_records, update_records)
    }

    #[test]
    fn sparse_range_raw_copies_every_unaffected_frame() {
        let fixture = sparse_range_fixture(6);
        assert_eq!(fixture.stats.decoded_frames, 0);
        assert_eq!(fixture.stats.decoded_compressed_bytes, 0);
        assert_eq!(fixture.stats.copied_frames, 5);
        assert_eq!(
            fixture.stats.copied_compressed_bytes,
            fixture.base_frame_bytes.iter().copied().sum::<u64>(),
        );
    }

    #[test]
    fn sparse_range_decodes_only_the_intersecting_entity_frame() {
        let fixture = sparse_range_fixture(3);
        assert_eq!(fixture.stats.decoded_frames, 1);
        assert_eq!(fixture.stats.decoded_compressed_bytes, fixture.base_frame_bytes[2]);
        assert_eq!(fixture.stats.copied_frames, 4);
        assert_eq!(
            fixture.stats.copied_compressed_bytes,
            fixture.base_frame_bytes
                .iter()
                .enumerate()
                .filter_map(|(index, bytes)| (index != 2).then_some(bytes))
                .copied()
                .sum::<u64>(),
        );
    }

    #[test]
    fn sparse_range_distant_updates_decode_only_exact_intersections() {
        let base_records = (1..=7_u64)
            .map(|page_id| crate::archive::Record::PageState {
                page_id,
                timestamp_micros: 100,
                title: format!("Base page {page_id} {}", "x".repeat(256)),
                namespace: None,
                deleted: false,
            })
            .collect();
        let update_records = [2_u64, 6]
            .into_iter()
            .map(|page_id| crate::archive::Record::PageState {
                page_id,
                timestamp_micros: 200,
                title: format!("Updated page {page_id}"),
                namespace: None,
                deleted: false,
            })
            .collect();
        let fixture = sparse_range_fixture_records(base_records, update_records);

        assert_eq!(fixture.stats.decoded_frames, 2);
        assert_eq!(
            fixture.stats.decoded_compressed_bytes,
            fixture.base_frame_bytes[1] + fixture.base_frame_bytes[5],
        );
        assert_eq!(fixture.stats.copied_frames, 5);
        assert_eq!(
            fixture.stats.copied_compressed_bytes,
            fixture
                .base_frame_bytes
                .iter()
                .enumerate()
                .filter_map(|(index, bytes)| (![1, 5].contains(&index)).then_some(bytes))
                .copied()
                .sum::<u64>(),
        );
        for page_id in [1_u64, 3, 4, 5, 7] {
            let base = fixture
                .base_frames
                .iter()
                .copied()
                .find(|entry| entry.first_entity.id == page_id)
                .unwrap();
            let output = fixture
                .output_frames
                .iter()
                .copied()
                .find(|entry| entry.first_entity.id == page_id)
                .unwrap();
            assert_eq!(base.frame_info(), output.frame_info());
            assert_eq!(
                sparse_frame_bytes(&fixture.base_bytes, base),
                sparse_frame_bytes(&fixture.output_bytes, output),
            );
        }
    }

    #[test]
    fn sparse_range_nonzero_tail_ordinal_has_no_duplicate_or_omitted_updates() {
        let base_records = (1..=7_u64)
            .map(|page_id| crate::archive::Record::PageState {
                page_id,
                timestamp_micros: 100,
                title: format!("Base page {page_id}"),
                namespace: None,
                deleted: false,
            })
            .collect();
        let already_consumed = sparse_revision(2, 21, 300, b"already consumed".to_vec());
        let page_two_update = sparse_revision(2, 22, 200, b"page two update".to_vec());
        let page_six_update = sparse_revision(6, 61, 200, b"page six update".to_vec());
        let fixture = sparse_range_fixture_records_at_ordinal(
            base_records,
            vec![
                already_consumed.clone(),
                page_two_update.clone(),
                page_six_update.clone(),
            ],
            1,
        );

        assert_eq!(fixture.stats.decoded_frames, 2);
        assert_eq!(fixture.stats.copied_frames, 5);
        assert_eq!(
            fixture
                .output_records
                .iter()
                .filter(|record| **record == already_consumed)
                .count(),
            0,
        );
        assert_eq!(
            fixture
                .output_records
                .iter()
                .filter(|record| **record == page_two_update)
                .count(),
            1,
        );
        assert_eq!(
            fixture
                .output_records
                .iter()
                .filter(|record| **record == page_six_update)
                .count(),
            1,
        );
        assert_eq!(fixture.output_records.len(), 9);
        for page_id in [1_u64, 3, 4, 5, 7] {
            let base = fixture
                .base_frames
                .iter()
                .copied()
                .find(|entry| entry.first_entity.id == page_id)
                .unwrap();
            let output = fixture
                .output_frames
                .iter()
                .copied()
                .find(|entry| entry.first_entity.id == page_id)
                .unwrap();
            assert_eq!(base.frame_info(), output.frame_info());
            assert_eq!(
                sparse_frame_bytes(&fixture.base_bytes, base),
                sparse_frame_bytes(&fixture.output_bytes, output),
            );
        }
    }

    #[test]
    fn sparse_range_archive_set_persists_copied_frames_and_directory_metadata() {
        let fixture = sparse_range_fixture(3);
        assert_eq!(fixture.output_frames, fixture.persisted_frames);
        assert_eq!(
            fixture
                .output_frames
                .iter()
                .map(|entry| entry.first_entity.id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5],
        );
        for page_id in [2_u64, 4] {
            let base = fixture
                .base_frames
                .iter()
                .copied()
                .find(|entry| entry.first_entity.id == page_id)
                .unwrap();
            let output = fixture
                .output_frames
                .iter()
                .copied()
                .find(|entry| entry.first_entity.id == page_id)
                .unwrap();
            assert_eq!(base.frame_info(), output.frame_info());
            assert_eq!(
                sparse_frame_bytes(&fixture.base_bytes, base),
                sparse_frame_bytes(&fixture.output_bytes, output),
            );
        }
        let changed = fixture
            .output_frames
            .iter()
            .find(|entry| entry.first_entity.id == 3)
            .unwrap();
        assert_eq!(changed.first_entity, changed.last_entity);
        assert_eq!(fixture.persisted_frames[2], *changed);
    }

    #[test]
    fn sparse_range_giant_changed_page_streams_as_one_frame_between_copies() {
        let page_one = crate::archive::Record::PageState {
            page_id: 1,
            timestamp_micros: 100,
            title: "Page one".into(),
            namespace: None,
            deleted: false,
        };
        let base_page_two = sparse_revision(2, 20, 100, b"old page two text".to_vec());
        let page_three = crate::archive::Record::PageState {
            page_id: 3,
            timestamp_micros: 100,
            title: "Page three".into(),
            namespace: None,
            deleted: false,
        };
        let giant_update = sparse_revision(2, 21, 200, vec![b'g'; 2 << 20]);
        let fixture = sparse_range_fixture_records(
            vec![page_one.clone(), base_page_two.clone(), page_three.clone()],
            vec![giant_update.clone()],
        );

        assert_eq!(fixture.stats.decoded_frames, 1);
        assert_eq!(fixture.stats.copied_frames, 2);
        assert_eq!(fixture.output_frames, fixture.persisted_frames);
        assert_eq!(fixture.output_frames.len(), 3);
        let page_two_frames = fixture
            .output_frames
            .iter()
            .filter(|entry| {
                entry.first_entity.id <= 2 && entry.last_entity.id >= 2
            })
            .collect::<Vec<_>>();
        assert_eq!(page_two_frames.len(), 1);
        let page_two_frame = page_two_frames[0];
        assert_eq!(page_two_frame.first_entity.id, 2);
        assert_eq!(page_two_frame.last_entity.id, 2);
        assert_eq!(page_two_frame.records, 2);
        assert!(page_two_frame.raw_bytes > (1 << 20));
        let output_keys = fixture
            .output_records
            .iter()
            .map(|record| (record.entity().id, record.timestamp_micros()))
            .collect::<Vec<_>>();
        assert_eq!(output_keys, [(1, 100), (2, 200), (2, 100), (3, 100)]);
        assert_eq!(fixture.output_records[0], page_one);
        assert_eq!(fixture.output_records[2], base_page_two);
        assert_eq!(fixture.output_records[3], page_three);
        let crate::archive::Record::Revision { page_id, revision } =
            &fixture.output_records[1]
        else {
            panic!("giant changed record did not round-trip as a revision");
        };
        assert_eq!(*page_id, 2);
        assert_eq!(revision.meta.rev_id, 21);
        assert_eq!(revision.meta.ts.timestamp_micros(), 200);
        assert_eq!(revision.meta.text_len, 2 << 20);
        assert_eq!(revision.text.len(), 2 << 20);
        assert!(revision.text.iter().all(|byte| *byte == b'g'));
        for page_id in [1_u64, 3] {
            let base = fixture
                .base_frames
                .iter()
                .copied()
                .find(|entry| entry.first_entity.id == page_id)
                .unwrap();
            let output = fixture
                .output_frames
                .iter()
                .copied()
                .find(|entry| entry.first_entity.id == page_id)
                .unwrap();
            assert_eq!(base.frame_info(), output.frame_info());
            assert_eq!(
                sparse_frame_bytes(&fixture.base_bytes, base),
                sparse_frame_bytes(&fixture.output_bytes, output),
            );
        }
    }

    #[test]
    fn hdd_range_merge_reads_tail_and_base_before_replacement_write() {
        let fixture = sparse_range_fixture(3);
        assert_eq!(fixture.events.len(), 3, "unexpected HDD phase event: {:?}", fixture.events);
        assert!(matches!(&fixture.events[0], RangeIoEvent::TailRead { .. }));
        assert!(matches!(&fixture.events[1], RangeIoEvent::BaseRead { .. }));
        assert_eq!(&fixture.events[2], &RangeIoEvent::ReplacementWrite);
        assert!(fixture.base_still_exists, "range construction must not reclaim the published base");
        let write = fixture.events
            .iter()
            .position(|event| event == &RangeIoEvent::ReplacementWrite)
            .unwrap();
        assert!(fixture.events[write + 1..].iter().all(|event| !matches!(
            event,
            RangeIoEvent::TailRead { .. } | RangeIoEvent::BaseRead { .. }
        )));
    }

    #[test]
    fn sparse_update_replaces_changed_revision_identity_without_splitting_page() {
        use chrono::TimeZone;
        use crate::archive::{
            ArchiveRecordReader, ArchiveWriter, CompressionSettings, ManifestRecord, Record,
            RevisionRecord, SiteInfoRecord,
        };
        use crate::{ContributorMeta, RevisionMeta};

        let root = tempfile::Builder::new()
            .prefix("sarun-wikimak-revision-replacement-")
            .tempdir_in("/Volumes/Elements/sarun-supervision-20260814")
            .unwrap();
        let candidate_archive = root.path().join("base.swdump");
        let candidate_title = root.path().join("base.swtitle");
        let base_id = crate::generation::GenerationId::from_plan_bytes(
            b"same-revision-identity-base",
        );
        let base_output =
            crate::archive_set::ArchiveSetOutput::new_in(root.path(), 1 << 20).unwrap();
        let mut base_writer = ArchiveWriter::with_ref_prefix(
            base_output,
            1,
            CompressionSettings::default(),
            b"same-revision-identity-reference",
        )
        .unwrap();
        base_writer
            .write(&Record::PageState {
                page_id: 7,
                timestamp_micros: 300,
                title: "Corrected page".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        base_writer
            .write(&Record::Revision {
                page_id: 7,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: 20,
                        parent_id: 19,
                        ts: chrono::Utc.timestamp_micros(200).single().unwrap(),
                        contributor: ContributorMeta::Named {
                            username: "Editor".into(),
                            user_id: 42,
                        },
                        comment: "stale base metadata".into(),
                        sha1: "stale-base-sha1".into(),
                        flags: 1,
                        text_len: b"stale base text".len() as u64,
                    },
                    has_text: true,
                    text: b"stale base text".to_vec(),
                    visibility: None,
                    history: None,
                },
            })
            .unwrap();
        base_writer
            .write(&Record::Revision {
                page_id: 7,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: 19,
                        parent_id: 18,
                        ts: chrono::Utc.timestamp_micros(100).single().unwrap(),
                        contributor: ContributorMeta::Named {
                            username: "Editor".into(),
                            user_id: 42,
                        },
                        comment: "text unavailable in base".into(),
                        sha1: String::new(),
                        flags: 0,
                        text_len: 0,
                    },
                    has_text: false,
                    text: Vec::new(),
                    visibility: None,
                    history: None,
                },
            })
            .unwrap();
        base_writer
            .write(&Record::Manifest {
                timestamp_micros: 1,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "2024-01-01".into(),
                    metadata_snapshot: "2024-01-01".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        base_writer
            .write(&Record::SiteInfo {
                timestamp_micros: 1,
                site_info: SiteInfoRecord {
                    site_name: "Revision replacement test".into(),
                    db_name: "testwiki".into(),
                    base: "https://example.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: "https://example.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (base_output, _) = base_writer.finish().unwrap();
        base_output
            .finish()
            .unwrap()
            .persist(&candidate_archive)
            .unwrap();
        crate::title_index::build(&candidate_archive, &candidate_title, &base_id).unwrap();

        let destination = root.path().join("installed/wiki.swdump");
        crate::installation_lifecycle::install(
            candidate_archive,
            candidate_title,
            &destination,
        )
        .unwrap();
        let (selected_archive, selected_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let base_identity =
            crate::generation::generation_identity(&selected_archive, &selected_title).unwrap();
        let source = test_update_source_plan(base_identity.generation_id.clone());
        let scratch = ensure_mirror_scratch(&destination).unwrap();
        let paths = update_lifecycle::UpdatePaths::new(update_root(
            &scratch,
            &source.source_plan_id,
        ));
        ensure_preserved_base(&destination, &source, &base_identity, &paths).unwrap();
        let base_site_info = ensure_base_site_info(&source, &paths).unwrap();

        let corrected = RevisionRecord {
            meta: RevisionMeta {
                rev_id: 20,
                parent_id: 19,
                ts: chrono::Utc.timestamp_micros(200).single().unwrap(),
                contributor: ContributorMeta::Named {
                    username: "Editor".into(),
                    user_id: 42,
                },
                comment: "corrected update metadata".into(),
                sha1: "corrected-update-sha1".into(),
                flags: 2,
                text_len: b"corrected tail text".len() as u64,
            },
            has_text: true,
            text: b"corrected tail text".to_vec(),
            visibility: None,
            history: None,
        };
        let populated = RevisionRecord {
            meta: RevisionMeta {
                rev_id: 19,
                parent_id: 18,
                ts: chrono::Utc.timestamp_micros(100).single().unwrap(),
                contributor: ContributorMeta::Named {
                    username: "Editor".into(),
                    user_id: 42,
                },
                comment: "text recovered by update".into(),
                sha1: "recovered-update-sha1".into(),
                flags: 4,
                text_len: b"recovered tail text".len() as u64,
            },
            has_text: true,
            text: b"recovered tail text".to_vec(),
            visibility: None,
            history: None,
        };
        let mut expected_corrected = corrected.clone();
        expected_corrected.meta.sha1.clear();
        let mut expected_populated = populated.clone();
        expected_populated.meta.sha1.clear();
        let tail_archive = paths.tail_archive();
        std::fs::create_dir_all(tail_archive.parent().unwrap()).unwrap();
        let mut tail_writer =
            ArchiveWriter::new(std::fs::File::create(&tail_archive).unwrap(), 1).unwrap();
        tail_writer
            .write(&Record::Revision {
                page_id: 7,
                revision: corrected.clone(),
            })
            .unwrap();
        tail_writer
            .write(&Record::Revision {
                page_id: 7,
                revision: populated.clone(),
            })
            .unwrap();
        let (tail_file, tail_frames) = tail_writer.finish().unwrap();
        tail_file.sync_all().unwrap();
        drop(tail_file);
        let tail_id =
            crate::generation::GenerationId::from_plan_bytes(b"same-revision-identity-tail");
        let tail_directory = crate::frame_directory::write_from_archive(
            &tail_archive,
            paths.tail_frame_directory(),
            tail_id.to_bytes().unwrap(),
        )
        .unwrap();
        let tail_receipt = update_lifecycle::TailReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: source.source_plan_id.clone(),
            base_generation_id: source.base_generation_id.as_str().into(),
            source_plan_id: source.source_plan_id.clone(),
            tail_id: tail_id.as_str().into(),
            file_name: "records.swdump".into(),
            bytes: std::fs::metadata(&tail_archive).unwrap().len(),
            frame_directory_name: "frames.swframe".into(),
            frame_directory_format: crate::frame_directory::FORMAT_VERSION,
            frame_directory_bytes: tail_directory.bytes,
            frames: tail_frames,
            records: tail_directory.records,
            first_entity: tail_directory.first_entity.map(Into::into),
            last_entity: tail_directory.last_entity.map(Into::into),
            complete: true,
        };
        persist_json(&paths.tail_receipt(), &tail_receipt).unwrap();
        let range_plan = ensure_range_plan(
            &source,
            &tail_receipt,
            &base_identity,
            &base_site_info,
            &paths,
        )
        .unwrap();
        let page_slot = range_plan
            .slots
            .iter()
            .find(|slot| slot.kind == crate::archive::EntityKind::Page as u8)
            .unwrap()
            .clone();

        let maintenance = crate::installation_lifecycle::begin_update_maintenance(
            &destination,
            source.base_generation_id.as_str(),
            source.generation_id.as_str(),
            &source.source_plan_id,
        )
        .unwrap();
        let (_, replacement_records) = apply_update_ranges(
            &source,
            &tail_receipt,
            &range_plan,
            &base_site_info,
            &paths,
            &maintenance,
        )
        .unwrap();
        assert_eq!(replacement_records, 3);
        drop(maintenance);

        let receipt = update_lifecycle::read_receipt::<
            update_lifecycle::RangeCandidateReceipt,
        >(&paths.range_receipt(page_slot.index))
        .unwrap()
        .unwrap();
        assert!(matches!(
            receipt.selection,
            update_lifecycle::RangeSelection::Replaced { .. }
        ));
        let directory = crate::frame_directory::FrameDirectory::open_bound(
            paths.range_frame_directory(&page_slot.candidate_id),
            crate::generation::GenerationId::parse(&page_slot.candidate_id)
                .unwrap()
                .to_bytes()
                .unwrap(),
        )
        .unwrap();
        let page = crate::archive::EntityKey {
            kind: crate::archive::EntityKind::Page,
            id: 7,
        };
        let containing_frames = directory
            .iter()
            .map(Result::unwrap)
            .filter(|entry| entry.first_entity <= page && page <= entry.last_entity)
            .collect::<Vec<_>>();
        assert_eq!(
            containing_frames.len(),
            1,
            "one page's records must not span replacement frames"
        );
        assert_eq!(containing_frames[0].first_entity, page);
        assert_eq!(containing_frames[0].last_entity, page);

        let mut reader = ArchiveRecordReader::open(paths.base_archive()).unwrap();
        let mut revisions = Vec::new();
        while let Some(record) = reader.next_record().unwrap() {
            if let Record::Revision {
                page_id: 7,
                revision,
            } = record
            {
                revisions.push(revision);
            }
        }
        assert_eq!(
            revisions.len(),
            2,
            "same revision identities must not leave duplicate records"
        );
        assert_eq!(
            revisions
                .iter()
                .map(|revision| revision.meta.rev_id)
                .collect::<Vec<_>>(),
            vec![20, 19]
        );
        let [reopened_corrected, reopened_populated] = revisions.as_slice() else {
            unreachable!()
        };
        assert_eq!(reopened_corrected, &expected_corrected);
        assert_eq!(reopened_populated, &expected_populated);
    }

    #[test]
    fn synthetic_cmd_fetch_restarts_after_unequal_size_range_replacement() {
        use crate::archive::{
            ArchiveWriter, CompressionSettings, ManifestRecord, Record, SiteInfoRecord,
        };

        const CHILD_DESTINATION: &str = "WIKIMAK_SYNTHETIC_FETCH_DESTINATION";
        if let Some(destination) = std::env::var_os(CHILD_DESTINATION) {
            cmd_fetch("testwiki", Path::new(&destination).to_str().unwrap(), None).unwrap();
            return;
        }

        let root = elements_tempdir("sarun-wikimak-unequal-range-");
        let candidate_archive = root.path().join("base.swdump");
        let candidate_title = root.path().join("base.swtitle");
        let base_id = crate::generation::GenerationId::from_plan_bytes(b"two-range-base");
        let base_output = crate::archive_set::ArchiveSetOutput::new_in(root.path(), 1).unwrap();
        let mut base_writer = ArchiveWriter::with_ref_prefix(
            base_output,
            1,
            CompressionSettings::default(),
            b"two-range-reference",
        )
        .unwrap();
        for (page_id, title) in [(1, "base one"), (2, "base two")] {
            base_writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: 100,
                    title: title.into(),
                    namespace: None,
                    deleted: false,
                })
                .unwrap();
        }
        base_writer
            .write(&Record::Manifest {
                timestamp_micros: 1,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "2024-01-01".into(),
                    metadata_snapshot: "2024-01-01".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        base_writer
            .write(&Record::SiteInfo {
                timestamp_micros: 1,
                site_info: SiteInfoRecord {
                    site_name: "Two range test".into(),
                    db_name: "testwiki".into(),
                    base: "https://example.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: "https://example.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (base_output, _) = base_writer.finish().unwrap();
        base_output
            .finish()
            .unwrap()
            .persist(&candidate_archive)
            .unwrap();
        crate::title_index::build(&candidate_archive, &candidate_title, &base_id).unwrap();

        let destination = root.path().join("installed/wiki.swdump");
        crate::installation_lifecycle::install(
            candidate_archive,
            candidate_title,
            &destination,
        )
        .unwrap();
        let selected = crate::installation_lifecycle::serving_pair(&destination)
            .unwrap()
            .unwrap();
        let selected_archive = selected.archive.clone();
        let selected_title = selected.title.clone();
        let base_identity =
            crate::generation::generation_identity(&selected_archive, &selected_title).unwrap();
        drop(selected);
        let source = test_update_source_plan(base_identity.generation_id.clone());
        let scratch = ensure_mirror_scratch(&destination).unwrap();
        let paths = update_lifecycle::UpdatePaths::new(update_root(
            &scratch,
            &source.source_plan_id,
        ));
        ensure_preserved_base(&destination, &source, &base_identity, &paths).unwrap();
        let base_site_info = ensure_base_site_info(&source, &paths).unwrap();
        assert_eq!(base_site_info.site_info().site_name, "Two range test");
        assert!(paths.base_site_info().is_file());
        persist_json(&paths.source_plan(), &source).unwrap();
        persist_json(&paths.plan(), &lifecycle_plan(&source)).unwrap();

        let base_set = crate::archive_set::ArchiveSetReader::open(paths.base_archive()).unwrap();
        let data_segments = base_set
            .segments()
            .iter()
            .filter(|segment| segment.kind.is_some())
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            data_segments
                .iter()
                .filter(|segment| segment.kind == Some(crate::archive::EntityKind::Page))
                .count(),
            2,
            "fixture must create two page pieces"
        );
        let slots = data_segments
            .iter()
            .enumerate()
            .map(|(index, segment)| update_lifecycle::RangeSlot {
                index,
                kind: segment.kind.unwrap() as u8,
                first_id: segment.first_id,
                last_id: segment.last_id,
                base_segment_id: crate::generation::GenerationId::from_plan_bytes(
                    format!("base-segment-{index}").as_bytes(),
                )
                .as_str()
                .into(),
                base_name: segment.name.clone(),
                base_bytes: segment.bytes,
                candidate_id: crate::generation::GenerationId::from_plan_bytes(
                    format!("candidate-segment-{index}").as_bytes(),
                )
                .as_str()
                .into(),
            })
            .collect::<Vec<_>>();
        let range_plan = update_lifecycle::RangePlanReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: source.source_plan_id.clone(),
            base_generation_id: source.base_generation_id.as_str().into(),
            tail_id: String::new(),
            slots,
        };

        let tail_archive = paths.tail_archive();
        std::fs::create_dir_all(tail_archive.parent().unwrap()).unwrap();
        let mut tail_writer = ArchiveWriter::new(std::fs::File::create(&tail_archive).unwrap(), 1)
            .unwrap();
        for (page_id, title) in [
            (1, "updated one with a deliberately longer physical replacement".repeat(64)),
            (2, "updated two with another deliberately longer physical replacement".repeat(64)),
        ] {
            tail_writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: 200,
                    title,
                    namespace: None,
                    deleted: false,
                })
                .unwrap();
        }
        tail_writer
            .write(&Record::Manifest {
                timestamp_micros: 200,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "2024-01-02".into(),
                    metadata_snapshot: "2024-01-02".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        let (tail_file, tail_frames) = tail_writer.finish().unwrap();
        tail_file.sync_all().unwrap();
        drop(tail_file);
        let tail_id = crate::generation::GenerationId::from_plan_bytes(b"two-range-tail");
        let tail_directory = crate::frame_directory::write_from_archive(
            &tail_archive,
            paths.tail_frame_directory(),
            tail_id.to_bytes().unwrap(),
        )
        .unwrap();
        let tail_receipt = update_lifecycle::TailReceipt {
            schema: update_lifecycle::UPDATE_SCHEMA,
            update_id: source.source_plan_id.clone(),
            base_generation_id: source.base_generation_id.as_str().into(),
            source_plan_id: source.source_plan_id.clone(),
            tail_id: tail_id.as_str().into(),
            file_name: "records.swdump".into(),
            bytes: std::fs::metadata(&tail_archive).unwrap().len(),
            frame_directory_name: "frames.swframe".into(),
            frame_directory_format: crate::frame_directory::FORMAT_VERSION,
            frame_directory_bytes: tail_directory.bytes,
            frames: tail_frames,
            records: tail_directory.records,
            first_entity: tail_directory.first_entity.map(Into::into),
            last_entity: tail_directory.last_entity.map(Into::into),
            complete: true,
        };
        let range_plan = update_lifecycle::RangePlanReceipt {
            tail_id: tail_id.as_str().into(),
            ..range_plan
        };
        persist_json(&paths.tail_receipt(), &tail_receipt).unwrap();
        std::fs::create_dir_all(paths.range_plan().parent().unwrap()).unwrap();
        persist_json(&paths.range_plan(), &range_plan).unwrap();

        let installed_generation = selected_archive;
        let old_base = range_plan
            .slots
            .iter()
            .map(|slot| paths.base_archive().join(&slot.base_name))
            .collect::<Vec<_>>();
        let old_installed = range_plan
            .slots
            .iter()
            .map(|slot| installed_generation.join(&slot.base_name))
            .collect::<Vec<_>>();
        let old_base_bytes = old_base
            .iter()
            .map(std::fs::read)
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        for (base, installed) in old_base.iter().zip(&old_installed) {
            assert!(base.exists());
            assert!(installed.exists());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            for (base, installed) in old_base.iter().zip(&old_installed) {
                assert_eq!(
                    (std::fs::metadata(base).unwrap().dev(), std::fs::metadata(base).unwrap().ino()),
                    (std::fs::metadata(installed).unwrap().dev(), std::fs::metadata(installed).unwrap().ino()),
                    "preserved base and installed piece must initially be hardlinks"
                );
            }
        }

        let maintenance = crate::installation_lifecycle::begin_update_maintenance(
            &destination,
            source.base_generation_id.as_str(),
            source.generation_id.as_str(),
            &source.source_plan_id,
        )
        .unwrap();
        let segment_directory_reads_before =
            crate::frame_directory::test_archive_segment_directory_reads();
        let mut events = Vec::new();
        let result = apply_update_ranges_observing(
            &source,
            &tail_receipt,
            &range_plan,
            &base_site_info,
            &paths,
            &maintenance,
            &mut |event| {
                if let RangeIoEvent::RangeDurableSwap {
                    slot_index,
                    old_installed_reclaimed,
                } = &event
                {
                    assert!(*old_installed_reclaimed);
                    assert!(*slot_index < range_plan.slots.len());
                    assert!(
                        !old_installed[*slot_index].exists(),
                        "installed old piece must be reclaimed at the durable handoff"
                    );
                    assert!(old_base[*slot_index].exists());
                    assert_ne!(
                        std::fs::read(&old_base[*slot_index]).unwrap(),
                        old_base_bytes[*slot_index],
                        "preserved path must name the replacement after the handoff"
                    );
                    if *slot_index == 0 {
                        assert_eq!(
                            ensure_base_site_info(&source, &paths).unwrap(),
                            base_site_info,
                            "restart after the first durable replacement must reuse the checkpoint"
                        );
                        assert!(matches!(
                            update_lifecycle::inspect_update(
                                &paths,
                                source.base_generation_id.as_str(),
                            )
                            .unwrap(),
                            update_lifecycle::UpdateState::ApplyingRanges {
                                completed: 1,
                                ..
                            }
                        ));
                        assert!(old_installed[1].exists());
                        assert_eq!(
                            std::fs::read(&old_base[1]).unwrap(),
                            old_base_bytes[1],
                            "range 2 old piece must remain until its own replacement"
                        );
                    }
                }
                events.push(event);
            },
        )
        .unwrap();
        assert_eq!(
            crate::frame_directory::test_archive_segment_directory_reads(),
            segment_directory_reads_before,
            "update range directory construction must use write-time metadata, not reread the replacement segment"
        );
        let receipts = read_range_receipts(&range_plan, &paths).unwrap();
        let replaced_slots = receipts
            .iter()
            .filter(|receipt| {
                matches!(
                    &receipt.selection,
                    update_lifecycle::RangeSelection::Replaced { .. }
                )
            })
            .map(|receipt| receipt.slot_index)
            .collect::<Vec<_>>();
        let unchanged_slots = receipts
            .iter()
            .filter(|receipt| {
                matches!(
                    &receipt.selection,
                    update_lifecycle::RangeSelection::Unchanged { .. }
                )
            })
            .map(|receipt| receipt.slot_index)
            .collect::<Vec<_>>();
        assert_eq!(unchanged_slots.len(), 1);
        let unchanged_site_info = &range_plan.slots[unchanged_slots[0]];
        assert_eq!(
            (
                unchanged_site_info.kind,
                unchanged_site_info.first_id,
                unchanged_site_info.last_id,
            ),
            (crate::archive::EntityKind::Global as u8, 1, 1),
            "the only unrewritten range must be the untouched SiteInfo record"
        );
        assert_eq!(
            result.1,
            receipts
                .iter()
                .filter_map(|receipt| match &receipt.selection {
                    update_lifecycle::RangeSelection::Replaced { records, .. } => Some(*records),
                    update_lifecycle::RangeSelection::Unchanged { .. } => None,
                })
                .sum::<u64>(),
            "range application reports records emitted into replacements"
        );
        assert_eq!(
            result.1,
            6,
            "four PageState records and two Manifest records are rewritten; the untouched SiteInfo range is selected without re-emission"
        );
        assert_eq!(
            events.len(),
            replaced_slots.len() * 4,
            "each replaced range has four semantic events: {events:?}"
        );
        for (slot_index, chunk) in replaced_slots.iter().copied().zip(events.chunks_exact(4)) {
            assert!(matches!(&chunk[0], RangeIoEvent::TailRead { .. }));
            assert!(matches!(&chunk[1], RangeIoEvent::BaseRead { .. }));
            assert_eq!(&chunk[2], &RangeIoEvent::ReplacementWrite);
            assert_eq!(
                &chunk[3],
                &RangeIoEvent::RangeDurableSwap {
                    slot_index,
                    old_installed_reclaimed: true,
                }
            );
            assert!(
                !chunk[3..]
                    .iter()
                    .any(|event| matches!(event, RangeIoEvent::TailRead { .. } | RangeIoEvent::BaseRead { .. })),
                "a range must not be pre-read after its replacement write"
            );
        }
        assert!(
            matches!(&events[4], RangeIoEvent::TailRead { .. }),
            "range 2 pre-read must be the event after range 1 durable swap"
        );
        for (slot_index, installed) in old_installed.iter().enumerate() {
            if replaced_slots.contains(&slot_index) {
                assert!(
                    !installed.exists(),
                    "replaced installed range {slot_index} must be reclaimed"
                );
            } else {
                assert!(
                    installed.exists(),
                    "unchanged installed range {slot_index} must remain selected"
                );
                assert_eq!(
                    std::fs::read(&old_base[slot_index]).unwrap(),
                    old_base_bytes[slot_index],
                    "unchanged preserved range {slot_index} must retain its bytes"
                );
            }
        }

        assert_eq!(receipts.len(), range_plan.slots.len());
        for (slot, receipt) in range_plan.slots.iter().zip(&receipts) {
            assert!(receipt.complete);
            assert!(paths.range_receipt(slot.index).is_file());
            match &receipt.selection {
                update_lifecycle::RangeSelection::Replaced { bytes, .. } => {
                    assert!(paths.range_object(&slot.candidate_id).is_file());
                    assert!(paths.range_frame_directory(&slot.candidate_id).is_file());
                    assert_eq!(
                        paths.range_projection(&slot.candidate_id).is_file(),
                        receipt.title_projection_records != 0,
                    );
                    assert!(receipt.candidate_bytes_written > 0);
                    assert_ne!(
                        *bytes,
                        slot.base_bytes,
                        "regression requires replacement pieces with changed physical sizes"
                    );
                }
                update_lifecycle::RangeSelection::Unchanged { name, bytes, .. } => {
                    assert_eq!(slot.index, unchanged_slots[0]);
                    assert_eq!(name, &slot.base_name);
                    assert_eq!(*bytes, slot.base_bytes);
                    assert!(!paths.range_object(&slot.candidate_id).exists());
                    assert!(!paths.range_frame_directory(&slot.candidate_id).exists());
                    assert!(!paths.range_projection(&slot.candidate_id).exists());
                    assert_eq!(receipt.candidate_bytes_written, 0);
                }
            }
        }

        // This is the layout consumed by the production cleanup proof: after
        // a durable replacement, the selected name is present in the
        // preserved base and the candidate object remains as another hardlink
        // until committed cleanup reaps it.  The child below resumes the real
        // cmd_fetch path without a synthetic hardlink-cleanup receipt, so it
        // exercises build_committed_update_hardlink_cleanup itself.
        for (slot, receipt) in range_plan.slots.iter().zip(&receipts) {
            let update_lifecycle::RangeSelection::Replaced { name, .. } = &receipt.selection
            else {
                continue;
            };
            let base_segment = paths.base_archive().join(name);
            let range_object = paths.range_object(&slot.candidate_id);
            assert!(
                base_segment.is_file(),
                "replaced selected segment must be present in preserved base: {}",
                base_segment.display()
            );
            assert!(
                range_object.is_file(),
                "replaced candidate object must remain until committed cleanup: {}",
                range_object.display()
            );
            #[cfg(unix)]
            assert!(same_proven_hardlink(&base_segment, &range_object).unwrap());
        }

        ensure_candidate_archive(&range_plan, &paths).unwrap();
        assert!(!update_hardlink_cleanup_path(&paths.root).exists());
        persist_json(
            &update_selector_path(&scratch),
            &update_lifecycle::ActiveUpdate {
                schema: update_lifecycle::UPDATE_SCHEMA,
                update_id: source.source_plan_id.clone(),
                base_generation_id: source.base_generation_id.as_str().into(),
            },
        )
        .unwrap();
        drop(maintenance);

        let mut candidate_reader =
            crate::archive::ArchiveRecordReader::open(paths.candidate_archive()).unwrap();
        let mut page_states = Vec::new();
        let mut manifests = Vec::new();
        let mut site_infos = Vec::new();
        while let Some(record) = candidate_reader.next_record().unwrap() {
            match record {
                Record::PageState {
                    page_id,
                    timestamp_micros,
                    title,
                    ..
                } => page_states.push((page_id, timestamp_micros, title)),
                Record::Manifest {
                    timestamp_micros,
                    manifest,
                } => manifests.push((timestamp_micros, manifest.content_snapshot)),
                Record::SiteInfo {
                    timestamp_micros,
                    site_info,
                } => site_infos.push((timestamp_micros, site_info.site_name)),
                other => panic!("unexpected candidate record: {other:?}"),
            }
        }
        assert_eq!(
            page_states,
            [
                (
                    1,
                    200,
                    "updated one with a deliberately longer physical replacement".repeat(64),
                ),
                (1, 100, "base one".into()),
                (
                    2,
                    200,
                    "updated two with another deliberately longer physical replacement".repeat(64),
                ),
                (2, 100, "base two".into()),
            ]
        );
        assert_eq!(
            manifests,
            [(200, "2024-01-02".into()), (1, "2024-01-01".into())]
        );
        assert_eq!(site_infos, [(1, "Two range test".into())]);
        drop(candidate_reader);

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("cli::tests::synthetic_cmd_fetch_restarts_after_unequal_size_range_replacement")
            .arg("--nocapture")
            .env(CHILD_DESTINATION, &destination)
            .status()
            .unwrap();
        assert!(status.success());

        assert!(!paths.root.exists(), "committed update state should be cleaned");
        let (published_archive, published_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let candidate_titles = crate::title_index::TitleIndex::open(&published_title).unwrap();
        assert_eq!(candidate_titles.generation_id(), &source.generation_id);
        let indexed = crate::archive::IndexedArchiveSet::open(
            &published_archive,
            &candidate_titles,
        )
        .unwrap();
        drop(indexed);
        assert_eq!(
            latest_site_info(&published_archive, &candidate_titles)
                .unwrap()
                .site_name,
            "Two range test"
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_update_cleanup_reclaims_replaced_piece_after_two_updates() {
        use std::os::unix::fs::MetadataExt;

        fn metadata_fingerprint(path: &Path) -> (u64, u64, u64, i64, i64) {
            let metadata = std::fs::symlink_metadata(path).unwrap();
            (
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.mtime(),
                metadata.mtime_nsec(),
            )
        }

        fn inode_stats(path: &Path, device: u64, inode: u64) -> (u64, u64) {
            let Ok(metadata) = std::fs::symlink_metadata(path) else {
                return (0, 0);
            };
            if metadata.file_type().is_dir() {
                return std::fs::read_dir(path)
                    .unwrap()
                    .map(|entry| inode_stats(&entry.unwrap().path(), device, inode))
                    .fold((0, 0), |(links, blocks), (found_links, found_blocks)| {
                        (links + found_links, blocks + found_blocks)
                    });
            }
            if metadata.file_type().is_file()
                && metadata.dev() == device
                && metadata.ino() == inode
            {
                (metadata.nlink(), metadata.blocks())
            } else {
                (0, 0)
            }
        }

        fn contains_selected_hardlink(
            path: &Path,
            selected: &[(u64, u64)],
        ) -> bool {
            let Ok(metadata) = std::fs::symlink_metadata(path) else {
                return false;
            };
            if metadata.file_type().is_dir() {
                return std::fs::read_dir(path)
                    .unwrap()
                    .any(|entry| contains_selected_hardlink(&entry.unwrap().path(), selected));
            }
            metadata.file_type().is_file()
                && selected
                    .iter()
                    .any(|(device, inode)| metadata.dev() == *device && metadata.ino() == *inode)
        }

        fn find_named(path: &Path, wanted: &str) -> Option<PathBuf> {
            let metadata = std::fs::symlink_metadata(path).ok()?;
            if path.file_name().is_some_and(|name| name == wanted) {
                return Some(path.to_owned());
            }
            if !metadata.file_type().is_dir() {
                return None;
            }
            std::fs::read_dir(path)
                .ok()?
                .filter_map(Result::ok)
                .find_map(|entry| find_named(&entry.path(), wanted))
        }

        fn stage_synthetic_committed_update(
            destination: &Path,
            scratch: &Path,
            old_generation_id: &str,
            candidate_archive: PathBuf,
            candidate_title: PathBuf,
            update_tag: &str,
        ) -> (PathBuf, (u64, u64), (u64, u64, u64, i64, i64)) {
            let candidate_identity = crate::generation::generation_identity(
                &candidate_archive,
                &candidate_title,
            )
            .unwrap();
            let update_id = crate::generation::GenerationId::from_plan_bytes(
                format!("synthetic-cleanup-{update_tag}").as_bytes(),
            );
            crate::installation_lifecycle::install(
                candidate_archive,
                candidate_title,
                destination,
            )
            .unwrap();
            let (selected_archive, selected_title) = crate::installation_lifecycle::selected_generation_paths(
                destination,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                crate::generation::generation_identity(
                    &selected_archive,
                    &selected_title,
                )
                .unwrap()
                .generation_id,
                candidate_identity.generation_id
            );
            let selected_set = crate::archive_set::ArchiveSetReader::open(&selected_archive).unwrap();
            let replaced_name = selected_set
                .segments()
                .iter()
                .find(|segment| segment.kind.is_some())
                .unwrap()
                .name
                .clone();
            drop(selected_set);

            let paths = update_lifecycle::UpdatePaths::new(update_root(
                scratch,
                update_id.as_str(),
            ));
            std::fs::create_dir_all(paths.base_archive()).unwrap();
            std::fs::create_dir_all(paths.range_object("").parent().unwrap()).unwrap();
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&selected_archive).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                let source = paths.base_archive().join(&name);
                hard_link_file(&entry.path(), &source).unwrap();
                let (_, bytes, identity) = scratch_entry_identity(&source).unwrap();
                entries.push(UpdateHardlinkCleanupEntry {
                    source: relative_update_path(&paths.root, &source).unwrap(),
                    target: format!("archive.swdump/{name}"),
                    expected_kind: "file".into(),
                    expected_bytes: bytes,
                    expected_identity: identity,
                });
            }
            let candidate_id = crate::generation::GenerationId::from_plan_bytes(
                format!("{update_tag}-range").as_bytes(),
            );
            let range_object = paths.range_object(candidate_id.as_str());
            hard_link_file(
                &selected_archive.join(&replaced_name),
                &range_object,
            )
            .unwrap();
            let (_, bytes, identity) = scratch_entry_identity(&range_object).unwrap();
            entries.push(UpdateHardlinkCleanupEntry {
                source: relative_update_path(&paths.root, &range_object).unwrap(),
                target: format!("archive.swdump/{replaced_name}"),
                expected_kind: "file".into(),
                expected_bytes: bytes,
                expected_identity: identity,
            });

            std::fs::write(
                paths.base_archive().join(format!("foreign-sentinel-{update_tag}")),
                b"foreign update data",
            )
            .unwrap();
            let sentinel_metadata = metadata_fingerprint(
                &paths
                    .base_archive()
                    .join(format!("foreign-sentinel-{update_tag}")),
            );
            let receipt = UpdateHardlinkCleanupReceipt {
                schema: UPDATE_HARDLINK_CLEANUP_SCHEMA,
                update_id: update_id.as_str().into(),
                base_generation_id: old_generation_id.into(),
                new_generation_id: candidate_identity.generation_id.as_str().into(),
                entries,
            };
            persist_json(&paths.commit_receipt(), &update_lifecycle::CommitReceipt {
                schema: update_lifecycle::UPDATE_SCHEMA,
                update_id: update_id.as_str().into(),
                old_generation_id: old_generation_id.into(),
                new_generation_id: candidate_identity.generation_id.as_str().into(),
            })
            .unwrap();
            persist_json(&update_hardlink_cleanup_path(&paths.root), &receipt).unwrap();
            persist_json(
                &update_selector_path(scratch),
                &update_lifecycle::ActiveUpdate {
                    schema: update_lifecycle::UPDATE_SCHEMA,
                    update_id: update_id.as_str().into(),
                    base_generation_id: old_generation_id.into(),
                },
            )
            .unwrap();
            let target = selected_archive.join(&replaced_name);
            let target_identity = metadata_fingerprint(&target);
            finish_update_cleanup(destination, scratch, &paths).unwrap();
            (
                target,
                (target_identity.0, target_identity.1),
                sentinel_metadata,
            )
        }

        let root = elements_tempdir("sarun-wikimak-hardlink-cleanup-");
        let destination = root.path().join("installed/wiki.swdump");
        let scratch = ensure_mirror_scratch(&destination).unwrap();
        let quarantine = scratch.join(".sarun-quarantine");
        std::fs::create_dir_all(&quarantine).unwrap();
        let preexisting = quarantine.join("pre-existing-entry");
        std::fs::write(&preexisting, b"pre-existing quarantine data").unwrap();
        let preexisting_metadata = metadata_fingerprint(&preexisting);

        let generation_zero = crate::generation::GenerationId::from_plan_bytes(b"cleanup-gen-0");
        let (archive_zero, title_zero) = test_generation(
            root.path(),
            "cleanup-zero",
            &generation_zero,
            "2024-01-01",
            "2024-01-01",
        );
        crate::installation_lifecycle::install(archive_zero, title_zero, &destination).unwrap();

        let generation_one = crate::generation::GenerationId::from_plan_bytes(b"cleanup-gen-1");
        let (archive_one, title_one) = test_generation(
            root.path(),
            "cleanup-one",
            &generation_one,
            "2024-01-02",
            "2024-01-02",
        );
        let (first_piece, first_inode, first_sentinel_metadata) = stage_synthetic_committed_update(
            &destination,
            &scratch,
            generation_zero.as_str(),
            archive_one,
            title_one,
            "one",
        );
        let first_metadata = std::fs::metadata(&first_piece).unwrap();
        assert_eq!(
            (first_metadata.dev(), first_metadata.ino()),
            first_inode,
            "the selected piece must remain after its own cleanup"
        );
        assert_eq!(first_metadata.nlink(), 1);
        let first_quarantine = scratch.join(".sarun-quarantine");
        let (selected_one, selected_one_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let mut selected_one_inodes = std::fs::read_dir(&selected_one)
            .unwrap()
            .map(|entry| {
                let metadata = std::fs::metadata(entry.unwrap().path()).unwrap();
                (metadata.dev(), metadata.ino())
            })
            .collect::<Vec<_>>();
        let title_metadata = std::fs::metadata(&selected_one_title).unwrap();
        selected_one_inodes.push((title_metadata.dev(), title_metadata.ino()));
        assert!(!contains_selected_hardlink(&first_quarantine, &selected_one_inodes));

        let generation_two = crate::generation::GenerationId::from_plan_bytes(b"cleanup-gen-2");
        let (archive_two, title_two) = test_generation(
            root.path(),
            "cleanup-two",
            &generation_two,
            "2024-01-03",
            "2024-01-03",
        );
        let (second_piece, second_inode, _) = stage_synthetic_committed_update(
            &destination,
            &scratch,
            generation_one.as_str(),
            archive_two,
            title_two,
            "two",
        );
        assert_eq!(
            inode_stats(root.path(), first_inode.0, first_inode.1),
            (0, 0),
            "the first update's replaced inode must have no remaining links or blocks after the second retirement"
        );
        let (selected_two, selected_two_title) =
            crate::installation_lifecycle::selected_generation_paths(&destination)
                .unwrap()
                .unwrap();
        let mut selected_two_inodes = std::fs::read_dir(&selected_two)
            .unwrap()
            .map(|entry| {
                let metadata = std::fs::metadata(entry.unwrap().path()).unwrap();
                (metadata.dev(), metadata.ino())
            })
            .collect::<Vec<_>>();
        let title_metadata = std::fs::metadata(&selected_two_title).unwrap();
        selected_two_inodes.push((title_metadata.dev(), title_metadata.ino()));
        assert!(!contains_selected_hardlink(&quarantine, &selected_two_inodes));
        assert_eq!(
            metadata_fingerprint(&preexisting),
            preexisting_metadata,
            "pre-existing quarantine data must retain its metadata"
        );
        assert_eq!(
            std::fs::read(&preexisting).unwrap(),
            b"pre-existing quarantine data"
        );
        let sentinel = find_named(&quarantine, "foreign-sentinel-one").unwrap();
        assert_eq!(metadata_fingerprint(&sentinel), first_sentinel_metadata);
        assert_eq!(
            metadata_fingerprint(&sentinel).2,
            b"foreign update data".len() as u64
        );
        assert_eq!(std::fs::read(sentinel).unwrap(), b"foreign update data");
        let second_metadata = std::fs::metadata(second_piece).unwrap();
        assert_eq!(
            (second_metadata.dev(), second_metadata.ino()),
            second_inode,
            "the selected piece from the second update must remain live"
        );
        assert_eq!(second_metadata.nlink(), 1);
    }

    #[test]
    fn hdd_range_memory_accounting_has_no_policy_threshold_and_checks_overflow() {
        let slot = update_lifecycle::RangeSlot {
            index: 7,
            kind: crate::archive::EntityKind::Page as u8,
            first_id: 1,
            last_id: 2,
            base_segment_id: "base".into(),
            base_name: "1000-pages.swdump-part".into(),
            base_bytes: 1 << 30,
            candidate_id: "candidate".into(),
        };
        assert_eq!(
            update_range_input_bytes(&slot, 2 << 30, 2 << 30).unwrap(),
            4 << 30,
            "valid piece geometry is admitted independently of an arbitrary byte threshold",
        );
        let error = update_range_input_bytes(
            &slot,
            u64::MAX,
            1,
        )
        .unwrap_err();
        assert!(error.contains("update range 7"));
        assert!(error.contains("input buffer size overflows"));
    }

    #[test]
    fn raw_repack_options_are_explicit_and_exclusive() {
        for options in [
            vec!["--raw-input", "--dictionary-bytes", "1024"],
            vec!["--raw-output", "--ref-prefix-bytes", "1024", "--sample-bytes", "2048"],
            vec!["--raw-input", "--raw-output"],
        ] {
            let mut args = vec!["input", "output", "128", "1"];
            args.extend(options);
            assert_eq!(cmd_repack(&args).unwrap_err(), "unknown repack options");
        }
    }

    #[test]
    fn start_discards_only_malformed_temporary_plan() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plan.json"), b"old plan").unwrap();
        std::fs::write(root.path().join("foreign-sentinel"), b"keep me").unwrap();
        std::fs::create_dir_all(root.path().join("input-cache")).unwrap();
        std::fs::write(root.path().join("input-cache/source.bz2"), b"cached").unwrap();
        std::fs::create_dir_all(root.path().join("nodes/content-000000.done")).unwrap();
        std::fs::write(
            root.path().join("nodes/content-000000.done/stale.bin"),
            b"stale node",
        )
        .unwrap();
        assert!(matches!(
            inspect_build_for_start(root.path(), &root.path().join("wiki.swdump")).unwrap(),
            crate::build_lifecycle::BuildState::Unplanned
        ));
        assert!(!root.path().join("plan.json").exists());
        assert!(!root.path().join("nodes").exists());
        assert!(root.path().join(".sarun-quarantine").is_dir());
        assert!(std::fs::read_dir(root.path().join(".sarun-quarantine"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .join("content-000000.done/stale.bin")
                .exists()));
        assert!(root.path().join("foreign-sentinel").exists());
        assert!(root.path().join("input-cache/source.bz2").exists());

        let mut replacement = crate::direct::DirectBuildPlan {
            schema: 1,
            plan_id: String::new(),
            wiki_db: "retrywiki".into(),
            content_snapshot: "2024-01-01".into(),
            metadata_snapshot: "2024-01".into(),
            observed_at_micros: 1,
            frame_target: MIRROR_FRAME_TARGET,
            range_target: crate::archive_set::DEFAULT_RANGE_TARGET,
            compression_level: 1,
            ref_prefix_sample_bytes: crate::archive::MIRROR_REF_PREFIX_SAMPLE_BYTES,
            ref_prefix_bytes: crate::archive::MIRROR_REF_PREFIX_BYTES,
            content_groups: vec![vec![crate::direct::PlannedPart {
                url: "https://example.invalid/content.xml".into(),
                filename: "content.xml".into(),
                size_bytes: 0,
                sha256: None,
                sha1: None,
                md5: None,
            }]],
            history_files: Vec::new(),
        };
        replacement.plan_id = crate::direct::canonical_direct_plan_id(&replacement).unwrap();
        assert!(matches!(
            crate::build_lifecycle::commit_plan(root.path(), &replacement).unwrap(),
            crate::build_lifecycle::BuildState::Planned { .. }
        ));
    }

    #[test]
    fn start_discards_invalid_tree_with_unusable_candidate() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plan.json"), b"old plan").unwrap();
        std::fs::write(root.path().join("archive.swdump"), b"candidate").unwrap();
        std::fs::write(root.path().join("foreign-sentinel"), b"keep me").unwrap();
        std::fs::create_dir_all(root.path().join("input-cache")).unwrap();
        std::fs::write(root.path().join("input-cache/source.bz2"), b"cached").unwrap();
        assert!(matches!(
            inspect_build_for_start(root.path(), &root.path().join("wiki.swdump")).unwrap(),
            crate::build_lifecycle::BuildState::Unplanned
        ));
        assert!(!root.path().join("plan.json").exists());
        assert!(!root.path().join("archive.swdump").exists());
        assert!(root.path().join("foreign-sentinel").exists());
        assert!(root.path().join("input-cache/source.bz2").exists());
    }

    #[test]
    fn invalid_update_reset_discards_unusable_candidate() {
        let root = tempfile::tempdir().unwrap();
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("updates/u1"));
        std::fs::create_dir_all(paths.candidate_inventory().parent().unwrap()).unwrap();
        std::fs::write(paths.candidate_inventory(), b"candidate").unwrap();
        std::fs::write(paths.root.join("foreign-sentinel"), b"keep me").unwrap();
        std::fs::create_dir_all(root.path().join("input-cache")).unwrap();
        std::fs::write(root.path().join("input-cache/source.bz2"), b"cached").unwrap();
        std::fs::write(root.path().join("foreign-root-sentinel"), b"keep me").unwrap();
        abandon_invalid_update(root.path(), Some(&paths), "malformed update receipt").unwrap();
        assert!(!paths.candidate_inventory().exists());
        assert!(!paths.root.exists());
        assert!(
            std::fs::read_dir(root.path().join(".sarun-quarantine"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .any(|path| {
                    std::fs::read(path.join("foreign-sentinel"))
                        .is_ok_and(|bytes| bytes == b"keep me")
                }),
            "invalid update reset must preserve foreign bytes in quarantine"
        );
        assert!(root.path().join("input-cache/source.bz2").exists());
        assert!(root.path().join("foreign-root-sentinel").exists());
    }

    #[test]
    fn refresh_cleanup_discards_owned_active_update_but_preserves_foreign_roots() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path();
        let active_root = update_root(scratch, "u1");
        std::fs::create_dir_all(active_root.join("candidate")).unwrap();
        std::fs::write(active_root.join("candidate/inventory.json"), b"owned").unwrap();
        let nested_owned_name = active_root.join("candidate/archive.swdump/foreign/nested");
        std::fs::create_dir_all(&nested_owned_name).unwrap();
        std::fs::write(nested_owned_name.join("sentinel"), b"keep nested").unwrap();
        std::fs::write(active_root.join("foreign-sentinel"), b"keep me").unwrap();
        let foreign_root = update_root(scratch, "foreign");
        std::fs::create_dir_all(&foreign_root).unwrap();
        std::fs::write(foreign_root.join("sentinel"), b"keep me").unwrap();
        persist_json(
            &update_selector_path(scratch),
            &update_lifecycle::ActiveUpdate {
                schema: update_lifecycle::UPDATE_SCHEMA,
                update_id: "u1".into(),
                base_generation_id: "base".into(),
            },
        )
        .unwrap();

        clear_obsolete_active_update(scratch).unwrap();

        assert!(!active_root.join("candidate/inventory.json").exists());
        assert!(!active_root.exists());
        let quarantined = std::fs::read_dir(root.path().join(".sarun-quarantine"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert!(
            quarantined.iter().any(|path| path.join("foreign-sentinel").exists()),
            "foreign update-root data must survive cleanup"
        );
        assert!(
            quarantined
                .iter()
                .any(|path| path.join("foreign/nested/sentinel").exists()),
            "foreign data nested under an owned staging name must survive cleanup"
        );
        assert!(foreign_root.join("sentinel").exists());
        assert!(!update_selector_path(scratch).exists());
    }

    #[test]
    fn scratch_cleanup_quarantines_unverified_fixed_names() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path();
        std::fs::write(scratch.join("archive.swdump"), b"owned archive").unwrap();
        std::fs::write(scratch.join("archive.swframe"), b"owned frame directory").unwrap();
        std::fs::write(
            scratch.join("title-projection-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.entries"),
            b"owned projection",
        )
        .unwrap();
        std::fs::create_dir_all(scratch.join("input-cache")).unwrap();
        std::fs::write(scratch.join("input-cache/source.bz2"), b"cached").unwrap();
        std::fs::write(scratch.join("foreign-sentinel"), b"keep me").unwrap();

        clear_owned_mirror_scratch(scratch, None).unwrap();

        assert!(!scratch.join("archive.swdump").exists());
        assert!(!scratch.join("archive.swframe").exists());
        assert!(!scratch
            .join("title-projection-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.entries")
            .exists());
        assert!(scratch.join("input-cache/source.bz2").exists());
        assert!(scratch.join("foreign-sentinel").exists());
    }

    #[test]
    fn scratch_cleanup_preserves_candidate_under_install_cleanup_authority() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let destination = root.path().join("installed/wiki.swdump");
        let (archive, title, generation_id) = completed_candidate(&scratch);
        std::fs::create_dir_all(crate::installation_lifecycle::generation_root(&destination))
            .unwrap();
        let claim_collision = crate::installation_lifecycle::generation_root(&destination)
            .join(format!(".candidate-archive-{generation_id}.pending"));
        std::fs::write(&claim_collision, b"foreign claim collision").unwrap();
        std::fs::create_dir(scratch.join("input-cache")).unwrap();
        std::fs::write(scratch.join("input-cache/source.bz2"), b"cached source").unwrap();
        std::fs::write(scratch.join("foreign-sentinel"), b"unknown").unwrap();

        let outcome = crate::installation_lifecycle::install(
            archive.clone(),
            title.clone(),
            &destination,
        )
        .unwrap();
        assert!(outcome.cleanup_pending);
        assert!(outcome.candidate_cleanup_pending);
        assert!(archive.is_dir());
        assert!(!title.exists(), "independent title cleanup still completed");

        clear_owned_mirror_scratch(&scratch, Some(&destination)).unwrap();

        assert!(archive.is_dir(), "generic cleanup must leave the receipted candidate in place");
        assert_eq!(std::fs::read(&claim_collision).unwrap(), b"foreign claim collision");
        assert_eq!(
            std::fs::read(scratch.join("input-cache/source.bz2")).unwrap(),
            b"cached source"
        );
        assert_eq!(std::fs::read(scratch.join("foreign-sentinel")).unwrap(), b"unknown");
        assert!(crate::installation_lifecycle::candidate_cleanup_owns_path(
            &destination,
            &archive,
        )
        .unwrap());
    }

    #[test]
    fn target_log_same_name_same_size_replacement_survives_quarantine() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path();
        let logs = scratch.join("target-logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("content-000000.log"), b"old-log").unwrap();
        let replacement = scratch.join("replacement.log");
        std::fs::write(&replacement, b"new-log").unwrap();
        std::fs::rename(&replacement, logs.join("content-000000.log")).unwrap();

        clear_owned_mirror_scratch(scratch, None).unwrap();

        assert!(!logs.exists());
        assert!(
            std::fs::read_dir(scratch.join(".sarun-quarantine"))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .any(|entry| {
                    std::fs::read(entry.path().join("content-000000.log"))
                        .is_ok_and(|bytes| bytes == b"new-log")
                }),
            "same-name replacement must survive in destination-local quarantine"
        );
    }

    #[test]
    fn reset_same_name_candidate_survives_destination_local_quarantine() {
        let root = tempfile::tempdir().unwrap();
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("updates/u1"));
        std::fs::create_dir_all(paths.candidate_inventory().parent().unwrap()).unwrap();
        std::fs::write(paths.candidate_inventory(), b"old-data").unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"new-data").unwrap();
        std::fs::rename(&replacement, paths.candidate_inventory()).unwrap();

        abandon_invalid_update(root.path(), Some(&paths), "malformed update receipt").unwrap();

        assert!(!paths.candidate_inventory().exists());
        assert!(
            std::fs::read_dir(root.path().join(".sarun-quarantine"))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .any(|entry| {
                    std::fs::read(entry.path())
                        .is_ok_and(|bytes| bytes == b"new-data")
                }),
            "reset must preserve the replacement candidate in quarantine"
        );
    }

    #[test]
    fn engine_cleanup_lease_is_nonblocking_and_does_not_create_state() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("scratch");
        let no_state = crate::direct::try_acquire_mirror_build_writer_cleanup_lease(&scratch)
            .unwrap()
            .unwrap();
        drop(no_state);
        assert!(!scratch.exists());

        let build = MirrorBuildLock::acquire(&scratch).unwrap();
        assert!(crate::direct::try_acquire_mirror_build_writer_cleanup_lease(&scratch)
            .unwrap()
            .is_none());
        drop(build);
        assert!(crate::direct::try_acquire_mirror_build_writer_cleanup_lease(&scratch)
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn writer_lock_rejects_symlinked_scratch_and_lock_leaf() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("wiki.swdump");
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let scratch = mirror_scratch_path(&archive);
        std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        symlink(&outside, &scratch).unwrap();
        assert!(ensure_mirror_scratch(&archive).unwrap_err().contains("not a real directory"));
        std::fs::remove_file(&scratch).unwrap();

        std::fs::create_dir(&scratch).unwrap();
        let outside_lock = outside.join("foreign-lock");
        std::fs::write(&outside_lock, b"foreign").unwrap();
        symlink(&outside_lock, scratch.join("build.lock")).unwrap();
        assert!(MirrorBuildLock::acquire(&scratch).is_err());
        assert_eq!(std::fs::read(outside_lock).unwrap(), b"foreign");
    }

    #[test]
    fn direct_tmpdir_is_destination_local_and_overrides_ambient_setting() {
        let _guard = DIRECT_TMPDIR_ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("mirror.swdump");
        let scratch = ensure_mirror_scratch(&archive).unwrap();
        let previous = std::env::var_os("TMPDIR");

        std::env::remove_var("TMPDIR");
        let request_tmp = ensure_direct_tmpdir(&scratch).unwrap();
        assert_eq!(request_tmp, scratch.join("request-tmp"));
        assert!(request_tmp.starts_with(&scratch));
        assert_eq!(
            std::env::var_os("TMPDIR"),
            Some(request_tmp.clone().into_os_string())
        );

        let explicit = root.path().join("explicit-tmp");
        std::fs::create_dir_all(&explicit).unwrap();
        std::env::set_var("TMPDIR", &explicit);
        assert_eq!(ensure_direct_tmpdir(&scratch).unwrap(), request_tmp);
        assert_eq!(
            std::env::var_os("TMPDIR"),
            Some(request_tmp.into_os_string())
        );

        match previous {
            Some(value) => std::env::set_var("TMPDIR", value),
            None => std::env::remove_var("TMPDIR"),
        }
    }

    #[test]
    fn direct_commands_require_an_absolute_archive_destination() {
        let error = require_absolute_archive(Path::new("relative/mirror.swdump")).unwrap_err();
        assert!(error.contains("absolute path"), "{error}");
        require_absolute_archive(Path::new("/owned-volume/mirror.swdump")).unwrap();
    }

    #[test]
    fn reset_rejects_relative_destination_before_creating_scratch() {
        let error = prepare_direct_archive(Path::new("relative/mirror.swdump")).unwrap_err();
        assert!(error.contains("absolute path"), "{error}");
    }

    #[test]
    fn reset_preparation_forces_destination_local_tmpdir() {
        let _guard = DIRECT_TMPDIR_ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("mirror.swdump");
        let previous = std::env::var_os("TMPDIR");

        let ambient = root.path().join("ambient-tmp");
        std::fs::create_dir_all(&ambient).unwrap();
        std::env::set_var("TMPDIR", &ambient);
        let scratch = prepare_direct_archive(&archive).unwrap();
        let request_tmp = PathBuf::from(std::env::var_os("TMPDIR").unwrap());

        assert_eq!(scratch.join("request-tmp"), request_tmp);
        assert!(request_tmp.starts_with(&scratch));

        match previous {
            Some(value) => std::env::set_var("TMPDIR", value),
            None => std::env::remove_var("TMPDIR"),
        }
    }

}
