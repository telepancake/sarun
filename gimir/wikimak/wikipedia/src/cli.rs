//! `wikimak` command line for portable Wikipedia archives.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::archive::MIRROR_FRAME_TARGET;

struct MirrorBuildLock(std::fs::File);

#[derive(serde::Deserialize, serde::Serialize)]
struct UpdateCheckpointReceipt {
    schema: u32,
    wiki_db: String,
    checkpoint_key: String,
    overlap_days: u64,
    frame_target: usize,
    compression_level: i32,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct UpdateRangePlan {
    schema: u32,
    checkpoint_key: String,
    ranges: Vec<UpdateRange>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct UpdateRange {
    name: String,
    kind: u8,
    first_id: u64,
    last_id: u64,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct UpdateRangeReceipt {
    schema: u32,
    checkpoint_key: String,
    old_name: String,
    new_name: String,
    bytes: u64,
    frames: u64,
    records: u64,
}

impl MirrorBuildLock {
    fn acquire(scratch: &Path) -> Result<Self, String> {
        use std::os::fd::AsRawFd;
        std::fs::create_dir_all(scratch)
            .map_err(|error| format!("{}: {error}", scratch.display()))?;
        let path = scratch.join("build.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!(
                "{}: another mirror build is already running",
                scratch.display()
            ));
        }
        Ok(Self(file))
    }
}

impl Drop for MirrorBuildLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
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

fn install_sidecar(destination: &Path, suffix: &str) -> Result<PathBuf, String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut name = destination
        .file_name()
        .ok_or_else(|| format!("{} has no file name", destination.display()))?
        .to_os_string();
    name.push(suffix);
    Ok(parent.join(name))
}

fn sync_parent(destination: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

pub fn mirror_auxiliary_paths(archive: &Path) -> Result<Vec<PathBuf>, String> {
    let title = archive.with_extension("swtitle");
    Ok(vec![
        mirror_scratch_path(archive),
        install_sidecar(archive, ".installing")?,
        install_sidecar(archive, ".previous")?,
        install_sidecar(archive, ".updating")?,
        install_sidecar(&title, ".previous")?,
    ])
}

fn ensure_mirror_scratch(archive: &Path) -> Result<PathBuf, String> {
    let scratch = mirror_scratch_path(archive);
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    Ok(scratch)
}

fn clear_mirror_scratch(scratch: &Path) -> Result<(), String> {
    for entry in
        std::fs::read_dir(scratch).map_err(|error| format!("{}: {error}", scratch.display()))?
    {
        let entry = entry.map_err(|error| format!("{}: {error}", scratch.display()))?;
        if entry.file_name() == "build.lock" {
            continue;
        }
        remove_path(&entry.path())
            .map_err(|error| format!("{}: {error}", entry.path().display()))?;
    }
    sync_parent(&scratch.join("build.lock"))
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
                "nodes/{}.done",
                plan.target_name("content", index)
                    .expect("content target index came from the plan")
            )
        })
        .chain(
            (0..plan.history_files.len())
                .map(|index| format!("nodes/history-{index:06}.done")),
        )
        .collect()
}

fn write_stage_one_makefile(
    scratch: &Path,
    plan: &crate::direct::DirectBuildPlan,
) -> Result<(), String> {
    let tool = build_tool_command()?;
    let make = recursive_make_command()?;
    let cores = crate::direct::processing_parallelism();
    let outer_workers = plan.target_count().min(cores).min(3).max(1);
    let bz2_workers = (cores / outer_workers).max(1);
    let targets = build_node_targets(plan);
    let mut makefile = String::from(".PHONY: all\n");
    makefile.push_str("ifneq ($(wildcard archive.complete),)\nall:\n");
    makefile.push_str(&format!(
        "else\nall: stage2.mk\n\t@{make} -f stage2.mk -j1\n\n"
    ));
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
            "nodes/{target}.done:\n\
             \t@{tool} build-node . plan.json content {index} {bz2_workers}\n\n"
        ));
    }
    for index in 0..plan.history_files.len() {
        makefile.push_str(&format!(
            "nodes/history-{index:06}.done:\n\
             \t@{tool} build-node . plan.json history {index} {bz2_workers}\n\n"
        ));
    }
    makefile.push_str("endif\n");
    persist_text(&scratch.join("stage1.mk"), &makefile)
}

fn write_stage_two_makefile(
    scratch: &Path,
    plan: &crate::direct::DirectBuildPlan,
) -> Result<(), String> {
    let tool = build_tool_command()?;
    let mut makefile = String::from(".PHONY: all\nall: archive.complete\n\narchive.complete:");
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
    let jobs = plan
        .target_count()
        .min(crate::direct::processing_parallelism())
        .min(3)
        .max(1);
    let status = std::process::Command::new(make_program()?)
        .current_dir(scratch)
        .env("SARUN_KATI_TARGET_LOG_DIR", log_directory)
        .env("SARUN_WIKIMEDIA_ROBOTS_CACHE", scratch.join("robots-cache"))
        .env("TMPDIR", scratch)
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

fn recover_interrupted_install(destination: &Path) -> Result<(), String> {
    let marker = install_sidecar(destination, ".installing")?;
    if !marker.exists() {
        return Ok(());
    }
    let title = destination.with_extension("swtitle");
    let old_archive = install_sidecar(destination, ".previous")?;
    let old_title = install_sidecar(&title, ".previous")?;
    match (old_archive.exists(), old_title.exists()) {
        (true, true) => {
            if destination.exists() {
                remove_path(destination)
                    .map_err(|error| format!("{}: {error}", destination.display()))?;
            }
            if title.exists() {
                std::fs::remove_file(&title)
                    .map_err(|error| format!("{}: {error}", title.display()))?;
            }
            std::fs::rename(&old_archive, destination)
                .map_err(|error| format!("{}: {error}", destination.display()))?;
            std::fs::rename(&old_title, &title)
                .map_err(|error| format!("{}: {error}", title.display()))?;
        }
        (false, false) => match (destination.exists(), title.exists()) {
            (true, false) => remove_path(destination)
                .map_err(|error| format!("{}: {error}", destination.display()))?,
            (false, true) => std::fs::remove_file(&title)
                .map_err(|error| format!("{}: {error}", title.display()))?,
            _ => {}
        },
        (true, false) => {
            if destination.exists() {
                remove_path(destination)
                    .map_err(|error| format!("{}: {error}", destination.display()))?;
            }
            std::fs::rename(&old_archive, destination)
                .map_err(|error| format!("{}: {error}", destination.display()))?;
        }
        (false, true) => {
            std::fs::remove_file(&old_title)
                .map_err(|error| format!("{}: {error}", old_title.display()))?;
        }
    }
    std::fs::remove_file(&marker)
        .map_err(|error| format!("{}: {error}", marker.display()))?;
    sync_parent(destination)
}

fn persist_archive_pair(
    archive: PathBuf,
    titles: tempfile::NamedTempFile,
    destination: &Path,
) -> Result<(), String> {
    let title = destination.with_extension("swtitle");
    if !destination.exists() && !title.exists() {
        let marker = install_sidecar(destination, ".installing")?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("{}: {error}", marker.display()))?;
        sync_parent(destination)?;
        let install = std::fs::rename(&archive, destination)
            .map_err(|error| format!("{}: {error}", destination.display()))
            .and_then(|_| {
                titles
                    .persist(&title)
                    .map_err(|error| format!("{}: {}", title.display(), error.error))
            })
            .and_then(|_| sync_parent(destination));
        if let Err(error) = install {
            recover_interrupted_install(destination)?;
            return Err(error);
        }
        std::fs::remove_file(&marker)
            .map_err(|error| format!("{}: {error}", marker.display()))?;
        return sync_parent(destination);
    }
    if !destination.exists() || !title.exists() {
        return Err(format!(
            "{} and {} must either both exist or both be absent",
            destination.display(),
            title.display()
        ));
    }

    let marker = install_sidecar(destination, ".installing")?;
    let old_archive = install_sidecar(destination, ".previous")?;
    let old_title = install_sidecar(&title, ".previous")?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("{}: {error}", marker.display()))?;
    std::fs::rename(destination, &old_archive)
        .map_err(|error| format!("{}: {error}", old_archive.display()))?;
    if let Err(error) = std::fs::hard_link(&title, &old_title) {
        let _ = std::fs::rename(&old_archive, destination);
        let _ = std::fs::remove_file(&marker);
        return Err(format!("{}: {error}", old_title.display()));
    }
    sync_parent(destination)?;

    let install = std::fs::rename(&archive, destination)
        .map_err(|error| format!("{}: {error}", destination.display()))
        .and_then(|_| {
            titles
                .persist(&title)
                .map_err(|error| format!("{}: {}", title.display(), error.error))
        })
        .and_then(|_| sync_parent(destination));
    if let Err(error) = install {
        recover_interrupted_install(destination)?;
        return Err(error);
    }

    remove_path(&old_archive)
        .map_err(|error| format!("{}: {error}", old_archive.display()))?;
    std::fs::remove_file(&old_title)
        .map_err(|error| format!("{}: {error}", old_title.display()))?;
    std::fs::remove_file(&marker)
        .map_err(|error| format!("{}: {error}", marker.display()))?;
    sync_parent(destination)
}

fn install_built_archive(
    archive: PathBuf,
    destination: &Path,
    scratch: &Path,
) -> Result<(), String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;

    let mut titles = tempfile::NamedTempFile::new_in(scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    let projected = archive.with_extension("swtitle");
    let title_entries = if projected.exists() {
        eprintln!("using title history index produced during final merge");
        let index = crate::title_index::TitleIndex::open(&projected)
            .map_err(|error| error.to_string())?;
        let entries = index.entry_count() as u64;
        let mut input = std::fs::File::open(&projected)
            .map_err(|error| format!("{}: {error}", projected.display()))?;
        std::io::copy(&mut input, titles.as_file_mut())
            .map_err(|error| format!("{}: {error}", projected.display()))?;
        entries
    } else {
        eprintln!("building title history index");
        crate::title_index::build(&archive, titles.path())
            .map_err(|error| error.to_string())?
    };
    titles
        .as_file_mut()
        .sync_all()
        .map_err(|error| format!("{}: {error}", titles.path().display()))?;

    eprintln!("installing completed archive and title index");
    persist_archive_pair(archive, titles, destination)?;
    eprintln!("{title_entries} title intervals");
    Ok(())
}

fn load_or_create_update_range_plan(
    archive: &Path,
    scratch: &Path,
    checkpoint_key: &str,
) -> Result<UpdateRangePlan, String> {
    let path = scratch.join("update-ranges.json");
    if path.exists() {
        let plan: UpdateRangePlan = serde_json::from_slice(
            &std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?,
        )
        .map_err(|error| format!("{}: {error}", path.display()))?;
        if plan.schema != 1 || plan.checkpoint_key != checkpoint_key || plan.ranges.is_empty() {
            return Err(format!(
                "{} does not describe this update generation",
                path.display()
            ));
        }
        return Ok(plan);
    }
    let set = crate::archive_set::ArchiveSetReader::open(archive)
        .map_err(|error| error.to_string())?;
    let ranges = set
        .segments()
        .iter()
        .filter_map(|segment| {
            segment.kind.map(|kind| UpdateRange {
                name: segment.name.clone(),
                kind: kind as u8,
                first_id: segment.first_id,
                last_id: segment.last_id,
            })
        })
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return Err("Wikipedia archive has no entity range files".into());
    }
    let plan = UpdateRangePlan {
        schema: 1,
        checkpoint_key: checkpoint_key.to_owned(),
        ranges,
    };
    persist_json(&path, &plan)?;
    Ok(plan)
}

fn update_range_receipt_path(scratch: &Path, index: usize) -> PathBuf {
    scratch
        .join("updated-ranges")
        .join(format!("{index:06}.json"))
}

fn read_update_range_receipt(
    archive: &Path,
    scratch: &Path,
    plan: &UpdateRangePlan,
    index: usize,
) -> Result<Option<UpdateRangeReceipt>, String> {
    let path = update_range_receipt_path(scratch, index);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let receipt: UpdateRangeReceipt =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    let range = &plan.ranges[index];
    let installed = archive.join(&receipt.new_name);
    if receipt.schema != 1
        || receipt.checkpoint_key != plan.checkpoint_key
        || receipt.old_name != range.name
        || std::fs::metadata(&installed)
            .map(|metadata| metadata.len())
            .ok()
            != Some(receipt.bytes)
    {
        return Err(format!(
            "{} does not match its installed range file",
            path.display()
        ));
    }
    Ok(Some(receipt))
}

fn sync_directory_path(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn one_range_archive(
    archive: &Path,
    range_name: &str,
    scratch: &Path,
) -> Result<tempfile::TempDir, String> {
    let temporary =
        tempfile::tempdir_in(scratch).map_err(|error| format!("{}: {error}", scratch.display()))?;
    for name in [
        "0000-reference.swdump-part",
        range_name,
        "9999-complete.swdump-part",
    ] {
        std::fs::hard_link(archive.join(name), temporary.path().join(name))
            .map_err(|error| format!("{}: {error}", archive.join(name).display()))?;
    }
    crate::archive_set::ArchiveSetReader::open(temporary.path())
        .map_err(|error| error.to_string())?;
    Ok(temporary)
}

fn update_record_belongs_to_range(
    record: &crate::archive::Record,
    kind: crate::archive::EntityKind,
    upper_id: u64,
) -> bool {
    let entity = record.entity();
    entity.kind == kind && entity.id <= upper_id
}

struct UpdateRangeSource<'a> {
    tail: &'a mut crate::archive::ArchiveRecordReader,
    pending: &'a mut Option<crate::archive::Record>,
    kind: crate::archive::EntityKind,
    upper_id: u64,
    records: &'a mut u64,
}

impl crate::archive::RecordSource for UpdateRangeSource<'_> {
    fn next_record(
        &mut self,
    ) -> crate::archive::Result<Option<crate::archive::Record>> {
        let Some(record) = self.pending.take() else {
            return Ok(None);
        };
        if !update_record_belongs_to_range(&record, self.kind, self.upper_id) {
            *self.pending = Some(record);
            return Ok(None);
        }
        *self.pending = self.tail.next_record()?;
        *self.records = self
            .records
            .checked_add(1)
            .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
        Ok(Some(record))
    }
}

fn skip_update_range(
    tail: &mut crate::archive::ArchiveRecordReader,
    pending: &mut Option<crate::archive::Record>,
    kind: crate::archive::EntityKind,
    upper_id: u64,
) -> Result<u64, String> {
    let mut records = 0_u64;
    while pending
        .as_ref()
        .is_some_and(|record| update_record_belongs_to_range(record, kind, upper_id))
    {
        *pending = tail.next_record().map_err(|error| error.to_string())?;
        records = records.saturating_add(1);
    }
    Ok(records)
}

fn replace_single_archive(
    archive: &Path,
    update: &Path,
    scratch: &Path,
) -> Result<(u64, u64), String> {
    let serving_snapshot = scratch.join("serving-generation.swdump");
    if serving_snapshot.exists() {
        use std::os::unix::fs::MetadataExt;
        let archive_metadata = std::fs::metadata(archive)
            .map_err(|error| format!("{}: {error}", archive.display()))?;
        let snapshot_metadata = std::fs::metadata(&serving_snapshot)
            .map_err(|error| format!("{}: {error}", serving_snapshot.display()))?;
        if archive_metadata.dev() != snapshot_metadata.dev()
            || archive_metadata.ino() != snapshot_metadata.ino()
        {
            let (_, frames, complete) =
                crate::archive::index_file(archive).map_err(|error| error.to_string())?;
            if !complete {
                return Err(
                    "installed single-file update has no completion marker".into(),
                );
            }
            let records = frames.iter().try_fold(0_u64, |total, frame| {
                total
                    .checked_add(frame.info.records)
                    .ok_or_else(|| "updated archive record count overflow".to_string())
            })?;
            eprintln!(
                "reusing the already installed single-file replacement after interruption"
            );
            return Ok((frames.len() as u64, records));
        }
    }
    let prefix = crate::archive::archive_ref_prefix(archive)
        .map_err(|error| error.to_string())?;
    let tail = crate::archive::ArchiveRecordReader::open(update)
        .map_err(|error| error.to_string())?;
    let temporary = tempfile::NamedTempFile::new_in(scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    let output = temporary
        .reopen()
        .map_err(|error| format!("{}: {error}", temporary.path().display()))?;
    let mut last_progress = std::time::Instant::now();
    let (output, frames, records) =
        crate::archive::merge_archive_with_sorted_source_and_ref_prefix(
            &prefix,
            archive,
            Box::new(tail),
            output,
            MIRROR_FRAME_TARGET,
            mirror_compression(),
            |records| {
                if last_progress.elapsed() >= std::time::Duration::from_secs(2) {
                    eprintln!("single-file update: merged {records} records");
                    last_progress = std::time::Instant::now();
                }
            },
        )
        .map_err(|error| error.to_string())?;
    output
        .sync_all()
        .map_err(|error| format!("{}: {error}", temporary.path().display()))?;
    drop(output);
    temporary
        .persist(archive)
        .map_err(|error| format!("{}: {}", archive.display(), error.error))?;
    sync_parent(archive)?;
    Ok((frames, records))
}

fn replace_archive_ranges(
    archive: &Path,
    update: &Path,
    scratch: &Path,
    checkpoint_key: &str,
) -> Result<(u64, u64), String> {
    if !archive.is_dir() {
        eprintln!("streaming the update through the mirror's single piece file");
        return replace_single_archive(archive, update, scratch);
    }
    let plan = load_or_create_update_range_plan(archive, scratch, checkpoint_key)?;
    std::fs::create_dir_all(scratch.join("updated-ranges"))
        .map_err(|error| format!("{}: {error}", scratch.display()))?;

    // Existing readers retain Arc<File> handles for every range. Renaming a
    // completed replacement therefore switches future generations without
    // disturbing a reader that was open when the update began.
    let prefix = crate::archive::archive_ref_prefix(archive)
        .map_err(|error| error.to_string())?;
    let mut tail = crate::archive::ArchiveRecordReader::open(update)
        .map_err(|error| error.to_string())?;
    let mut pending = tail.next_record().map_err(|error| error.to_string())?;
    let mut total_frames = 0_u64;
    let mut total_records = 0_u64;

    for (index, range) in plan.ranges.iter().enumerate() {
        let kind = crate::archive::EntityKind::try_from(range.kind)
            .map_err(|error| error.to_string())?;
        let final_for_kind = plan
            .ranges
            .get(index + 1)
            .is_none_or(|next| next.kind != range.kind);
        let upper_id = if final_for_kind {
            u64::MAX
        } else {
            range.last_id
        };
        if let Some(receipt) =
            read_update_range_receipt(archive, scratch, &plan, index)?
        {
            skip_update_range(&mut tail, &mut pending, kind, upper_id)?;
            if receipt.old_name != receipt.new_name {
                match std::fs::remove_file(archive.join(&receipt.old_name)) {
                    Ok(()) => sync_directory_path(archive)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "{}: {error}",
                            archive.join(&receipt.old_name).display()
                        ))
                    }
                }
            }
            total_frames = total_frames.saturating_add(receipt.frames);
            total_records = total_records.saturating_add(receipt.records);
            eprintln!(
                "reusing updated range {}/{} {}",
                index + 1,
                plan.ranges.len(),
                receipt.new_name
            );
            continue;
        }

        if pending.as_ref().is_some_and(|record| record.entity().kind < kind) {
            return Err("sorted update stream contains an entity before its archive range".into());
        }
        let touched = pending
            .as_ref()
            .is_some_and(|record| update_record_belongs_to_range(record, kind, upper_id));
        if !touched {
            let bytes = std::fs::metadata(archive.join(&range.name))
                .map_err(|error| format!("{}: {error}", archive.join(&range.name).display()))?
                .len();
            persist_json(
                &update_range_receipt_path(scratch, index),
                &UpdateRangeReceipt {
                    schema: 1,
                    checkpoint_key: plan.checkpoint_key.clone(),
                    old_name: range.name.clone(),
                    new_name: range.name.clone(),
                    bytes,
                    frames: 0,
                    records: 0,
                },
            )?;
            eprintln!(
                "keeping unchanged range {}/{} {}",
                index + 1,
                plan.ranges.len(),
                range.name
            );
            continue;
        }
        eprintln!(
            "streaming update into range {}/{} {}",
            index + 1,
            plan.ranges.len(),
            range.name
        );
        let base = one_range_archive(archive, &range.name, scratch)?;
        let output = crate::archive_set::ArchiveSetOutput::new_in(scratch, u64::MAX)
            .map_err(|error| error.to_string())?;
        let mut additions = 0_u64;
        let mut last_progress = std::time::Instant::now();
        let (output, frames, records) =
            crate::archive::merge_archive_with_sorted_source_and_ref_prefix(
                &prefix,
                base.path(),
                Box::new(UpdateRangeSource {
                    tail: &mut tail,
                    pending: &mut pending,
                    kind,
                    upper_id,
                    records: &mut additions,
                }),
                output,
                MIRROR_FRAME_TARGET,
                mirror_compression(),
                |records| {
                    if last_progress.elapsed() >= std::time::Duration::from_secs(2) {
                        eprintln!(
                            "range {}/{}: merged {records} records",
                            index + 1,
                            plan.ranges.len()
                        );
                        last_progress = std::time::Instant::now();
                    }
                },
            )
            .map_err(|error| error.to_string())?;
        let completed = output.finish().map_err(|error| error.to_string())?;
        let replacement = scratch.join(format!(".updated-range-{index:06}"));
        if replacement.exists() {
            remove_path(&replacement)
                .map_err(|error| format!("{}: {error}", replacement.display()))?;
        }
        completed
            .persist(&replacement)
            .map_err(|error| error.to_string())?;
        let replacement_set = crate::archive_set::ArchiveSetReader::open(&replacement)
            .map_err(|error| error.to_string())?;
        let replacement_range = replacement_set
            .segments()
            .iter()
            .find(|segment| segment.kind == Some(kind))
            .ok_or_else(|| "range replacement contains no entity data".to_string())?
            .clone();
        let new_name = replacement_range.name;
        let new_bytes = replacement_range.bytes;
        let installed = archive.join(&new_name);
        std::fs::rename(replacement.join(&new_name), &installed)
            .map_err(|error| format!("{}: {error}", installed.display()))?;
        sync_directory_path(archive)?;
        let receipt = UpdateRangeReceipt {
            schema: 1,
            checkpoint_key: plan.checkpoint_key.clone(),
            old_name: range.name.clone(),
            new_name: new_name.clone(),
            bytes: new_bytes,
            frames,
            records,
        };
        persist_json(&update_range_receipt_path(scratch, index), &receipt)?;
        if new_name != range.name {
            std::fs::remove_file(archive.join(&range.name))
                .map_err(|error| format!("{}: {error}", archive.join(&range.name).display()))?;
            sync_directory_path(archive)?;
        }
        remove_path(&replacement)
            .map_err(|error| format!("{}: {error}", replacement.display()))?;
        total_frames = total_frames.saturating_add(frames);
        total_records = total_records.saturating_add(records);
        eprintln!(
            "installed range {}/{} {} after merging {additions} update records",
            index + 1,
            plan.ranges.len(),
            new_name
        );
    }
    if let Some(record) = pending {
        return Err(format!(
            "sorted update record {:?} is outside the archive range plan",
            record.entity()
        ));
    }
    Ok((total_frames, total_records))
}

fn ensure_update_serving_snapshot(archive: &Path, scratch: &Path) -> Result<(), String> {
    let snapshot = scratch.join("serving-generation.swdump");
    let snapshot_titles = snapshot.with_extension("swtitle");
    if snapshot.exists() || snapshot_titles.exists() {
        if !snapshot.exists() || !snapshot_titles.exists() {
            if snapshot.exists() {
                remove_path(&snapshot)
                    .map_err(|error| format!("{}: {error}", snapshot.display()))?;
            }
            if snapshot_titles.exists() {
                std::fs::remove_file(&snapshot_titles)
                    .map_err(|error| format!("{}: {error}", snapshot_titles.display()))?;
            }
        } else {
            let (_, _, complete) =
                crate::archive::index_file(&snapshot).map_err(|error| error.to_string())?;
            if !complete {
                return Err("update serving snapshot is incomplete".into());
            }
            let titles = crate::title_index::TitleIndex::open(&snapshot_titles)
                .map_err(|error| error.to_string())?;
            crate::archive::IndexedArchiveSet::open(&snapshot, &titles)
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
    }
    if !archive.is_dir() {
        std::fs::hard_link(archive, &snapshot)
            .map_err(|error| format!("{}: {error}", snapshot.display()))?;
        if let Err(error) =
            std::fs::hard_link(archive.with_extension("swtitle"), &snapshot_titles)
        {
            let _ = std::fs::remove_file(&snapshot);
            return Err(format!("{}: {error}", snapshot_titles.display()));
        }
        return sync_directory_path(scratch);
    }
    let temporary =
        tempfile::tempdir_in(scratch).map_err(|error| format!("{}: {error}", scratch.display()))?;
    for entry in
        std::fs::read_dir(archive).map_err(|error| format!("{}: {error}", archive.display()))?
    {
        let entry = entry.map_err(|error| format!("{}: {error}", archive.display()))?;
        let name = entry.file_name();
        std::fs::hard_link(entry.path(), temporary.path().join(name))
            .map_err(|error| format!("{}: {error}", entry.path().display()))?;
    }
    crate::archive_set::ArchiveSetReader::open(temporary.path())
        .map_err(|error| error.to_string())?;
    std::fs::hard_link(archive.with_extension("swtitle"), &snapshot_titles)
        .map_err(|error| format!("{}: {error}", snapshot_titles.display()))?;
    #[allow(deprecated)]
    let temporary = temporary.into_path();
    std::fs::rename(&temporary, &snapshot)
        .map_err(|error| format!("{}: {error}", snapshot.display()))?;
    sync_directory_path(scratch)
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
    let output = archive.with_extension("media");
    if output.exists() {
        eprintln!(
            "selected Kiwix media already exists at {}; keeping it",
            output.display()
        );
        return Ok(());
    }
    let stats = if source == "auto" {
        let release = wikimak_media::remote::discover_latest(client, dbname)
            .map_err(|error| format!("discover Kiwix image source: {error}"))?;
        eprintln!(
            "selected Kiwix source: {} (ranged import; no ZIM file is saved)",
            release.name
        );
        let source = wikimak_media::remote::RemoteKiwixImageSource::open(
            client.clone(),
            release.url,
        )
        .map_err(|error| format!("open remote Kiwix source: {error}"))?;
        eprintln!(
            "indexed {} image entries from {} bytes",
            source.len(),
            source.file_size()
        );
        source
            .pack(&output)
            .map_err(|error| format!("pack selected Kiwix media: {error}"))?
    } else {
        let source = PathBuf::from(source);
        eprintln!(
            "packing selected Kiwix media {} -> {}",
            source.display(),
            output.display()
        );
        let source = wikimak_media::KiwixImageSource::open(&source)
            .map_err(|error| format!("open selected Kiwix source: {error}"))?;
        source
            .pack(&output)
            .map_err(|error| format!("pack selected Kiwix media: {error}"))?
    };
    eprintln!(
        "packed {} image entries ({} bytes, {} storages)",
        stats.entries_written, stats.bytes_written, stats.storages
    );
    Ok(())
}

fn build_full(
    client: &reqwest::blocking::Client,
    dbname: &str,
    archive: &Path,
    scratch: &Path,
    replace_plan: bool,
) -> Result<(), String> {
    std::env::set_var("SARUN_MIRROR_DEST", archive);
    recover_interrupted_install(archive)?;
    std::fs::create_dir_all(scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    // Fetch robots.txt once during discovery and leave the result in the
    // resumable build tree for every stage-one helper to consume.
    std::env::set_var("SARUN_WIKIMEDIA_ROBOTS_CACHE", scratch.join("robots-cache"));
    let _lock = MirrorBuildLock::acquire(scratch)?;
    if replace_plan {
        clear_mirror_scratch(scratch)?;
    }
    let plan_path = scratch.join("plan.json");
    let plan = if plan_path.exists() {
        let plan = crate::direct::read_direct_build_plan(&plan_path)
            .map_err(|error| error.to_string())?;
        if plan.wiki_db != dbname {
            return Err(format!(
                "{} belongs to {}, not {dbname}",
                plan_path.display(),
                plan.wiki_db,
            ));
        }
        eprintln!(
            "resuming snapshot {} with {} source targets",
            plan.content_snapshot,
            plan.target_count(),
        );
        plan
    } else {
        let plan = crate::direct::discover_direct_build_plan(
            client,
            &wikimak_mediawiki::Config::default(),
            dbname,
            &|message| eprintln!("{message}"),
        )
        .map_err(|error| error.to_string())?;
        persist_json(&plan_path, &plan)?;
        plan
    };
    if let Some(url) = plan.first_source_url() {
        // This is deliberately done by the importing process, before any
        // stage-one helpers start.  A resumed plan therefore cannot race
        // several workers into independently requesting robots.txt.
        wikimak_mediawiki::prepare_robots(client, url)
            .map_err(|error| error.to_string())?;
    }
    let reusable = crate::direct::prune_invalid_build_nodes_observing(
        scratch,
        &plan,
        &|message| eprintln!("{message}"),
    )
    .map_err(|error| error.to_string())?;
    crate::direct::recover_direct_build_completion(scratch, &plan)
        .map_err(|error| error.to_string())?;
    if reusable != 0 {
        eprintln!(
            "resuming with {reusable}/{} source targets already durable",
            plan.target_count(),
        );
    }
    prepare_build_tools(scratch)?;
    write_stage_one_makefile(scratch, &plan)?;
    run_build_make(scratch, &plan)?;
    let built = scratch.join("archive.swdump");
    crate::archive_set::ArchiveSetReader::open(&built)
        .map_err(|error| error.to_string())?;
    if !scratch.join("archive.complete").exists() {
        return Err("resumable build stopped without a complete archive".into());
    }
    install_built_archive(built, archive, scratch)?;
    #[cfg(feature = "serve")]
    pack_selected_media(client, dbname, archive)?;
    remove_path(scratch).map_err(|error| format!("{}: {error}", scratch.display()))
}

fn cmd_fetch(dbname: &str, archive: &str) -> Result<(), String> {
    std::env::set_var("SARUN_MIRROR_DEST", archive);
    let archive = Path::new(archive);
    recover_interrupted_install(archive)?;
    let client = http_client()?;
    if !archive.exists() {
        return build_full(
            &client,
            dbname,
            archive,
            &ensure_mirror_scratch(archive)?,
            false,
        );
    }
    let scratch = ensure_mirror_scratch(archive)?;
    let _lock = MirrorBuildLock::acquire(&scratch)?;
    // The update discovery pass owns the one robots.txt fetch for this
    // resumable import.  Its child workers inherit this artifact path.
    std::env::set_var("SARUN_WIKIMEDIA_ROBOTS_CACHE", scratch.join("robots-cache"));
    let partial = scratch.join("update.swdump");
    let receipt_path = scratch.join("update.receipt.json");
    let update_marker = install_sidecar(archive, ".updating")?;
    let overlap_days = 3;
    let compression = mirror_compression();
    let expected_checkpoint_key = (!update_marker.exists())
        .then(|| {
            crate::direct::update_checkpoint_key(
                archive,
                dbname,
                overlap_days,
                MIRROR_FRAME_TARGET,
                compression,
            )
        })
        .transpose()
        .map_err(|error| error.to_string())?;
    let receipt = std::fs::read(&receipt_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<UpdateCheckpointReceipt>(&bytes).ok());
    let receipt_matches = receipt.as_ref().is_some_and(|receipt| {
        receipt.schema == 1
            && receipt.wiki_db == dbname
            && receipt.overlap_days == overlap_days
            && receipt.frame_target == MIRROR_FRAME_TARGET
            && receipt.compression_level == compression.level
            && expected_checkpoint_key
                .as_ref()
                .is_none_or(|expected| receipt.checkpoint_key == *expected)
    });
    let partial_complete = partial
        .exists()
        .then(|| crate::archive::index_file(&partial))
        .transpose()
        .map_err(|error| error.to_string())?
        .is_some_and(|(_, _, complete)| complete)
        && receipt_matches;
    if (partial.exists() || receipt_path.exists()) && !partial_complete {
        if update_marker.exists() {
            return Err(format!(
                "{} exists but its update checkpoint is missing or belongs to another build",
                update_marker.display()
            ));
        }
        if partial.exists() {
            std::fs::remove_file(&partial)
                .map_err(|error| format!("{}: {error}", partial.display()))?;
        }
        if receipt_path.exists() {
            std::fs::remove_file(&receipt_path)
                .map_err(|error| format!("{}: {error}", receipt_path.display()))?;
        }
    }
    if update_marker.exists() && !partial_complete {
        return Err(format!(
            "{} exists but its durable update stream is missing; refusing to continue from a \
             possibly mixed range generation",
            update_marker.display()
        ));
    }
    if partial_complete {
        eprintln!("reusing durable sorted update stream");
    } else {
        crate::build_update_archive(
            &client,
            &wikimak_mediawiki::Config::default(),
            dbname,
            archive,
            &partial,
            &scratch,
            overlap_days,
            MIRROR_FRAME_TARGET,
            compression,
            |message| eprintln!("{message}"),
        )
        .map_err(|error| error.to_string())?;
        persist_json(
            &receipt_path,
            &UpdateCheckpointReceipt {
                schema: 1,
                wiki_db: dbname.to_owned(),
                checkpoint_key: expected_checkpoint_key
                    .clone()
                    .expect("computed before update mutation"),
                overlap_days,
                frame_target: MIRROR_FRAME_TARGET,
                compression_level: compression.level,
            },
        )?;
    }
    let checkpoint_key = receipt
        .as_ref()
        .filter(|_| partial_complete)
        .map(|receipt| receipt.checkpoint_key.clone())
        .or(expected_checkpoint_key)
        .ok_or_else(|| "update checkpoint identity is unavailable".to_string())?;

    let serving_snapshot = scratch.join("serving-generation.swdump");
    if update_marker.exists()
        && (!serving_snapshot.exists()
            || !serving_snapshot.with_extension("swtitle").exists())
    {
        return Err(
            "update marker exists without its preserved serving generation".into(),
        );
    }
    ensure_update_serving_snapshot(archive, &scratch)?;
    if !update_marker.exists() {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&update_marker)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("{}: {error}", update_marker.display()))?;
        sync_parent(archive)?;
    }
    if archive.is_dir() {
        eprintln!("merging the sorted update into one durable page-ID range at a time");
    } else {
        eprintln!("merging the sorted update into the mirror's one piece file");
    }
    let (frames, records) =
        replace_archive_ranges(archive, &partial, &scratch, &checkpoint_key)?;

    eprintln!("rebuilding the single title and virtual-frame index");
    let mut titles = tempfile::NamedTempFile::new_in(&scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    let title_entries = crate::title_index::build(archive, titles.path())
        .map_err(|error| error.to_string())?;
    titles
        .as_file_mut()
        .sync_all()
        .map_err(|error| format!("{}: {error}", titles.path().display()))?;

    titles
        .persist(archive.with_extension("swtitle"))
        .map_err(|error| error.error.to_string())?;
    std::fs::remove_file(&update_marker)
        .map_err(|error| format!("{}: {error}", update_marker.display()))?;
    sync_parent(archive)?;
    #[cfg(feature = "serve")]
    pack_selected_media(&client, dbname, archive)?;
    eprintln!("{records} records, {frames} frames, {title_entries} title intervals");
    remove_path(&scratch).map_err(|error| format!("{}: {error}", scratch.display()))
}

fn cmd_refresh_full(dbname: &str, archive: &str) -> Result<(), String> {
    let archive = Path::new(archive);
    build_full(
        &http_client()?,
        dbname,
        archive,
        &ensure_mirror_scratch(archive)?,
        true,
    )
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
    let started = std::time::Instant::now();
    let mirror_path = PathBuf::from(path);
    if install_sidecar(&mirror_path, ".installing")?.exists() {
        return Err("Wikipedia archive generation switch is in progress".into());
    }
    let path = if install_sidecar(&mirror_path, ".updating")?.exists() {
        let snapshot = mirror_scratch_path(&mirror_path).join("serving-generation.swdump");
        if !snapshot.exists() || !snapshot.with_extension("swtitle").exists() {
            return Err("Wikipedia update lacks a readable preserved generation".into());
        }
        eprintln!("wikimak serve: update active; opening preserved generation");
        snapshot
    } else {
        mirror_path.clone()
    };
    let archive = crate::archive_browse::ArchiveBrowseIndex::open(
        &path,
        path.with_extension("swtitle"),
    )
    .map_err(|error| error.to_string())?;
    eprintln!(
        "wikimak serve: opened {} title intervals and {} frames in {:.3}s",
        archive.title_count(),
        archive.frame_count(),
        started.elapsed().as_secs_f64(),
    );
    let media_root = mirror_path.with_extension("media");
    let packed_media = packed_media
        .map(PathBuf::from)
        .or_else(|| has_packed_media(&media_root).then_some(media_root.clone()));
    crate::serve::serve_archive(
        std::sync::Arc::new(archive),
        addr.to_owned(),
        media_root,
        None,
        packed_media,
    )
}

#[cfg(feature = "serve")]
fn has_packed_media(path: &Path) -> bool {
    std::fs::read_dir(path).ok().is_some_and(|entries| {
        entries.flatten().any(|entry| {
            let path = entry.path();
            path.extension().and_then(|value| value.to_str()) == Some("data")
                && path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.starts_with("media-"))
        })
    })
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

fn cmd_title_index(archive: &str, output: &str) -> Result<(), String> {
    let entries = crate::title_index::build(archive, output)
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

fn cmd_build_node(args: &[&str]) -> Result<(), String> {
    let [root, plan, kind, index, bz2_workers] = args else {
        return Err(
            "build-node wants <root> <plan.json> <content|history> <index> <bz2-workers>"
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

pub fn cli_main(args: &[String]) -> i32 {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    if matches!(args.as_slice(), ["fetch" | "refresh-full", _, _]) {
        arm_parent_watchdog();
    }
    let result = match args.as_slice() {
        ["discover", dbname] => cmd_discover(dbname),
        ["fetch", dbname, archive] => cmd_fetch(dbname, archive),
        ["refresh-full", dbname, archive] => cmd_refresh_full(dbname, archive),
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
        ["title-index", archive, output] => cmd_title_index(archive, output),
        ["backrefs", archive, titles, output] => cmd_backrefs(archive, titles, output),
        ["repack", arguments @ ..] => cmd_repack(arguments),
        ["merge", arguments @ ..] => cmd_merge(arguments),
        ["inspect", archive] => cmd_inspect(archive),
        ["build-node", arguments @ ..] => cmd_build_node(arguments),
        ["build-stage2", root, plan] => cmd_build_stage_two(root, plan),
        ["build-assemble", root, plan] => cmd_build_assemble(root, plan),
        _ => Err(
            "usage: wikimak discover <dbname>\n\
             \x20      wikimak fetch <dbname> <archive.swdump>\n\
             \x20      wikimak refresh-full <dbname> <archive.swdump>\n\
             \x20      wikimak serve <archive.swdump> [addr] [--packed-media <directory>]\n\
             \x20      wikimak kiwix-pack <source.zim> <output-directory>\n\
             \x20      wikimak siteinfo <api-url> <output.swdump>\n\
             \x20      wikimak title-index <archive.swdump> <output.swtitle>\n\
             \x20      wikimak backrefs <archive.swdump> <titles.swtitle> <output.swrefs>\n\
             \x20      wikimak repack <input> <output> <frame-bytes> <zstd-level> [--dictionary-bytes N | --ref-prefix-bytes N --sample-bytes N | --raw-output | --raw-input]\n\
             \x20      wikimak merge <output> <frame-bytes> <zstd-level> <input>...\n\
             \x20      wikimak inspect <archive.swdump>"
                .into(),
        ),
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
    use std::io::Write;

    use super::*;

    fn candidate(parent: &Path, bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new_in(parent).unwrap();
        file.write_all(bytes).unwrap();
        file.as_file().sync_all().unwrap();
        file
    }

    fn archive_candidate(parent: &Path, bytes: &[u8]) -> PathBuf {
        #[allow(deprecated)]
        let path = tempfile::tempdir_in(parent).unwrap().into_path();
        std::fs::write(path.join("payload"), bytes).unwrap();
        path
    }

    fn page_state(page_id: u64, timestamp_micros: i64) -> crate::archive::Record {
        crate::archive::Record::PageState {
            page_id,
            timestamp_micros,
            title: format!("Page {page_id}"),
            namespace: None,
            deleted: false,
        }
    }

    fn manifest(timestamp_micros: i64, snapshot: &str) -> crate::archive::Record {
        crate::archive::Record::Manifest {
            timestamp_micros,
            manifest: crate::archive::ManifestRecord {
                wiki_db: "testwiki".into(),
                content_snapshot: snapshot.into(),
                metadata_snapshot: "2024-06".into(),
                source_files: Vec::new(),
            },
        }
    }

    fn site_info(timestamp_micros: i64) -> crate::archive::Record {
        crate::archive::Record::SiteInfo {
            timestamp_micros,
            site_info: crate::archive::SiteInfoRecord {
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
            },
        }
    }

    #[test]
    fn single_file_mirror_update_reuses_the_same_tail_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("testwiki.swdump");
        let prefix = vec![b'x'; 256];
        let mut writer = crate::archive::ArchiveWriter::with_ref_prefix(
            std::fs::File::create(&archive).unwrap(),
            1,
            crate::archive::CompressionSettings {
                level: 1,
                ..Default::default()
            },
            &prefix,
        )
        .unwrap();
        for record in [
            page_state(1, 100),
            page_state(2, 100),
            manifest(100, "2024-06-01"),
            site_info(100),
        ] {
            writer.write(&record).unwrap();
        }
        writer.finish().unwrap().0.sync_all().unwrap();
        crate::title_index::build(&archive, archive.with_extension("swtitle"))
            .unwrap();

        let update = directory.path().join("update.swdump");
        let mut writer = crate::archive::ArchiveWriter::new(
            std::fs::File::create(&update).unwrap(),
            1,
        )
        .unwrap();
        for record in [
            page_state(2, 200),
            page_state(3, 200),
            manifest(200, "2024-06-02"),
        ] {
            writer.write(&record).unwrap();
        }
        writer.finish().unwrap().0.sync_all().unwrap();

        let scratch = directory.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        ensure_update_serving_snapshot(&archive, &scratch).unwrap();
        replace_archive_ranges(&archive, &update, &scratch, "one")
            .unwrap();
        use std::os::unix::fs::MetadataExt;
        let installed_inode = std::fs::metadata(&archive).unwrap().ino();
        replace_archive_ranges(&archive, &update, &scratch, "one")
            .unwrap();
        assert_eq!(
            std::fs::metadata(&archive).unwrap().ino(),
            installed_inode,
            "resume rewrote an already installed single-file generation"
        );

        let page_threes = |path: &Path| {
            let mut reader = crate::archive::ArchiveRecordReader::open(path).unwrap();
            let mut count = 0;
            while let Some(record) = reader.next_record().unwrap() {
                if record.entity()
                    == (crate::archive::EntityKey {
                        kind: crate::archive::EntityKind::Page,
                        id: 3,
                    })
                {
                    count += 1;
                }
            }
            count
        };
        assert_eq!(page_threes(&archive), 1);
        assert_eq!(
            page_threes(&scratch.join("serving-generation.swdump")),
            0
        );
    }

    #[test]
    fn update_ranges_are_individually_durable_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("testwiki.swdump");
        let output =
            crate::archive_set::ArchiveSetOutput::new_in(directory.path(), 1).unwrap();
        let prefix = vec![b'x'; 256];
        let mut writer = crate::archive::ArchiveWriter::with_ref_prefix(
            output,
            1,
            crate::archive::CompressionSettings {
                level: 1,
                ..Default::default()
            },
            &prefix,
        )
        .unwrap();
        for record in [
            page_state(1, 100),
            page_state(2, 100),
            manifest(100, "2024-06-01"),
            site_info(100),
        ] {
            writer.write(&record).unwrap();
        }
        let (output, _) = writer.finish().unwrap();
        output
            .finish()
            .unwrap()
            .persist(&archive)
            .unwrap();

        let update = directory.path().join("update.swdump");
        let mut writer = crate::archive::ArchiveWriter::new(
            std::fs::File::create(&update).unwrap(),
            1,
        )
        .unwrap();
        for record in [
            page_state(2, 200),
            page_state(3, 200),
            manifest(200, "2024-06-02"),
        ] {
            writer.write(&record).unwrap();
        }
        writer.finish().unwrap();
        let scratch = directory.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        crate::title_index::build(&archive, archive.with_extension("swtitle")).unwrap();
        ensure_update_serving_snapshot(&archive, &scratch).unwrap();
        let snapshot = scratch.join("serving-generation.swdump");

        let first = replace_archive_ranges(&archive, &update, &scratch, "checkpoint").unwrap();
        let second = replace_archive_ranges(&archive, &update, &scratch, "checkpoint").unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_dir(scratch.join("updated-ranges"))
                .unwrap()
                .count(),
            4
        );

        let mut records = crate::archive::ArchiveRecordReader::open(&archive).unwrap();
        let mut page_three = 0;
        while let Some(record) = records.next_record().unwrap() {
            if record.entity()
                == (crate::archive::EntityKey {
                    kind: crate::archive::EntityKind::Page,
                    id: 3,
                })
            {
                page_three += 1;
            }
        }
        assert_eq!(page_three, 1);
        let mut old_records = crate::archive::ArchiveRecordReader::open(&snapshot).unwrap();
        while let Some(record) = old_records.next_record().unwrap() {
            assert_ne!(record.entity().id, 3);
        }
        assert!(
            crate::archive_set::ArchiveSetReader::open(&archive).is_ok(),
            "range replacement left obsolete or overlapping files"
        );
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
    fn archive_pair_install_replaces_both_completed_files() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("wiki.swdump");
        let titles = archive.with_extension("swtitle");
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(archive.join("payload"), b"old archive").unwrap();
        std::fs::write(&titles, b"old titles").unwrap();

        persist_archive_pair(
            archive_candidate(temporary.path(), b"new archive"),
            candidate(temporary.path(), b"new titles"),
            &archive,
        )
        .unwrap();

        assert_eq!(std::fs::read(archive.join("payload")).unwrap(), b"new archive");
        assert_eq!(std::fs::read(&titles).unwrap(), b"new titles");
        assert!(!install_sidecar(&archive, ".installing").unwrap().exists());
        assert!(!install_sidecar(&archive, ".previous").unwrap().exists());
        assert!(!install_sidecar(&titles, ".previous").unwrap().exists());
    }

    #[test]
    fn interrupted_pair_install_rolls_back_one_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("wiki.swdump");
        let titles = archive.with_extension("swtitle");
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(archive.join("payload"), b"old archive").unwrap();
        std::fs::write(&titles, b"old titles").unwrap();
        let marker = install_sidecar(&archive, ".installing").unwrap();
        let old_archive = install_sidecar(&archive, ".previous").unwrap();
        let old_titles = install_sidecar(&titles, ".previous").unwrap();
        std::fs::write(&marker, b"").unwrap();
        std::fs::rename(&archive, &old_archive).unwrap();
        std::fs::hard_link(&titles, &old_titles).unwrap();
        std::fs::rename(
            archive_candidate(temporary.path(), b"new archive"),
            &archive,
        )
        .unwrap();
        candidate(temporary.path(), b"new titles")
            .persist(&titles)
            .unwrap();

        recover_interrupted_install(&archive).unwrap();

        assert_eq!(std::fs::read(archive.join("payload")).unwrap(), b"old archive");
        assert_eq!(std::fs::read(&titles).unwrap(), b"old titles");
        assert!(!marker.exists());
    }

    #[test]
    fn interruption_between_archive_backup_and_title_backup_restores_archive() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("wiki.swdump");
        let titles = archive.with_extension("swtitle");
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(archive.join("payload"), b"old archive").unwrap();
        std::fs::write(&titles, b"old titles").unwrap();
        let marker = install_sidecar(&archive, ".installing").unwrap();
        let old_archive = install_sidecar(&archive, ".previous").unwrap();
        std::fs::write(&marker, b"").unwrap();
        std::fs::rename(&archive, &old_archive).unwrap();

        recover_interrupted_install(&archive).unwrap();

        assert_eq!(std::fs::read(archive.join("payload")).unwrap(), b"old archive");
        assert_eq!(std::fs::read(&titles).unwrap(), b"old titles");
        assert!(!marker.exists());
        assert!(!old_archive.exists());
    }

    #[test]
    fn interrupted_first_install_removes_an_unpaired_archive() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("wiki.swdump");
        let marker = install_sidecar(&archive, ".installing").unwrap();
        std::fs::write(&marker, b"").unwrap();
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(archive.join("payload"), b"incomplete").unwrap();

        recover_interrupted_install(&archive).unwrap();

        assert!(!archive.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn interrupted_first_install_keeps_a_complete_pair() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("wiki.swdump");
        let title = archive.with_extension("swtitle");
        let marker = install_sidecar(&archive, ".installing").unwrap();
        std::fs::write(&marker, b"").unwrap();
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(archive.join("payload"), b"complete").unwrap();
        std::fs::write(&title, b"complete titles").unwrap();

        recover_interrupted_install(&archive).unwrap();

        assert!(archive.exists());
        assert!(title.exists());
        assert!(!marker.exists());
    }
}
