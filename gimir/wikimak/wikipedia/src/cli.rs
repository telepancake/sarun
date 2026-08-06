//! `wikimak` command line for portable Wikipedia archives.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::archive::MIRROR_FRAME_TARGET;

#[path = "update_lifecycle.rs"]
mod update_lifecycle;

struct MirrorBuildLock(std::fs::File);

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
    let mut paths = vec![mirror_scratch_path(archive)];
    paths.extend(crate::installation_lifecycle::auxiliary_paths(archive)?);
    Ok(paths)
}

pub fn mirror_has_installed_generation(archive: &Path) -> Result<bool, String> {
    crate::installation_lifecycle::serving_pair(archive).map(|pair| pair.is_some())
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

/// Abandon a malformed/foreign construction tree while holding its
/// destination-local build lock.  The lifecycle inspector decides whether
/// state is disposable; this function removes that operation's entire
/// private scratch tree and never touches the selected installed generation.
fn abandon_invalid_build(
    scratch: &Path,
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
    clear_mirror_scratch(scratch)
}

fn inspect_build_for_start(
    scratch: &Path,
) -> Result<crate::build_lifecycle::BuildState, String> {
    match crate::build_lifecycle::inspect_build(scratch, None) {
        Ok(state) => Ok(state),
        Err(error) => {
            let original = error.to_string();
            abandon_invalid_build(scratch, &error)
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
             \t@{tool} build-node . plan.json content {index} {bz2_workers}\n\n"
        ));
    }
    for index in 0..plan.history_files.len() {
        makefile.push_str(&format!(
            "nodes/history-{index:06}.done/receipt.json:\n\
             \t@{tool} build-node . plan.json history {index} {bz2_workers}\n\n"
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

fn install_built_archive(
    archive: PathBuf,
    destination: &Path,
) -> Result<(), String> {
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
    if outcome.cleanup_pending {
        eprintln!(
            "installed generation is live; previous generation cleanup waits for active readers"
        );
    }
    eprintln!("{title_entries} title intervals");
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

fn installed_generation_id(archive: &Path) -> Result<crate::generation::GenerationId, String> {
    let selected = crate::installation_lifecycle::serving_pair(archive)?
        .ok_or_else(|| format!("{} has no installed generation", archive.display()))?;
    crate::title_index::TitleIndex::open(selected.title)
        .map(|titles| titles.generation_id().clone())
        .map_err(|error| error.to_string())
}

fn create_update_plan(
    client: &reqwest::blocking::Client,
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
        &wikimak_mediawiki::Config::default(),
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
    if paths.tail_archive().exists() {
        std::fs::remove_file(paths.tail_archive())
            .map_err(|error| format!("{}: {error}", paths.tail_archive().display()))?;
    }
    let work = paths.root.join("tail").join("work");
    std::fs::create_dir_all(&work)
        .map_err(|error| format!("{}: {error}", work.display()))?;
    let stats = crate::direct::build_update_archive_from_plan(
        client,
        source,
        paths.tail_archive(),
        &work,
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
            remove_path(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        }
    }
    let selected = crate::installation_lifecycle::serving_pair(archive)?
        .ok_or_else(|| format!("{} has no installed generation", archive.display()))?;
    hard_link_archive(&selected.archive, &paths.base_archive())?;
    if let Err(error) = hard_link_file(&selected.title, &paths.base_index()) {
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
    paths: &update_lifecycle::UpdatePaths,
) -> Result<update_lifecycle::RangePlanReceipt, String> {
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

struct LifecycleRangeSource<'a> {
    tail: &'a mut crate::archive::ArchiveRecordReader,
    pending: &'a mut Option<crate::archive::Record>,
    kind: crate::archive::EntityKind,
    upper_id: u64,
    additions: &'a mut u64,
    first_addition: &'a mut Option<crate::archive::EntityKey>,
    last_addition: &'a mut Option<crate::archive::EntityKey>,
    title_writer: &'a mut crate::archive::ArchiveWriter<'static, std::fs::File>,
    title_records: &'a mut u64,
}

impl LifecycleRangeSource<'_> {
    fn peek_entity(
        &mut self,
    ) -> crate::archive::Result<Option<crate::archive::EntityKey>> {
        if self.pending.is_none() {
            *self.pending = self.tail.next_record()?;
        }
        Ok(self.pending.as_ref().map(crate::archive::Record::entity))
    }

    fn restore(&mut self, record: crate::archive::Record) -> crate::archive::Result<()> {
        if self.pending.replace(record).is_some() {
            return Err(crate::archive::ArchiveError::Invalid(
                "range source already has a pending record",
            ));
        }
        Ok(())
    }
}

impl crate::archive::RecordSource for LifecycleRangeSource<'_> {
    fn next_record(&mut self) -> crate::archive::Result<Option<crate::archive::Record>> {
        let record = match self.pending.take() {
            Some(record) => record,
            None => match self.tail.next_record()? {
                Some(record) => record,
                None => return Ok(None),
            },
        };
        if !update_record_belongs_to_range(&record, self.kind, self.upper_id) {
            *self.pending = Some(record);
            return Ok(None);
        }
        if is_title_projection_record(&record) {
            self.title_writer.write(&record)?;
            *self.title_records = self
                .title_records
                .checked_add(1)
                .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
        }
        let entity = record.entity();
        self.first_addition.get_or_insert(entity);
        *self.last_addition = Some(entity);
        *self.additions = self
            .additions
            .checked_add(1)
            .ok_or(crate::archive::ArchiveError::FieldTooLarge)?;
        Ok(Some(record))
    }
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

#[derive(Default)]
struct SparseRangeMergeStats {
    output_frames: u64,
    output_records: u64,
    copied_frames: u64,
    copied_compressed_bytes: u64,
    decoded_frames: u64,
    decoded_compressed_bytes: u64,
}

fn base_range_frame_directory(
    paths: &update_lifecycle::UpdatePaths,
    slot: &update_lifecycle::RangeSlot,
) -> Result<std::sync::Arc<crate::frame_directory::FrameDirectory>, String> {
    let path = paths.base_range_frame_directory(&slot.base_segment_id);
    let identity = crate::generation::GenerationId::parse(&slot.base_segment_id)
        .and_then(|identity| identity.to_bytes())
        .map_err(|error| error.to_string())?;
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|error| format!("{}: {error}", path.display()))?;
        crate::frame_directory::write_from_archive_segment(
            paths.base_archive().join(&slot.base_name),
            &path,
            identity,
        )
        .map_err(|error| error.to_string())?;
    }
    let directory = crate::frame_directory::FrameDirectory::open_bound(
        &path,
        identity,
    )
    .map_err(|error| error.to_string())?;
    directory
        .require_archive_bounds(slot.base_bytes)
        .map_err(|error| error.to_string())?;
    Ok(std::sync::Arc::new(directory))
}

fn merge_sparse_update_range(
    paths: &update_lifecycle::UpdatePaths,
    slot: &update_lifecycle::RangeSlot,
    prefix: &[u8],
    updates: &mut LifecycleRangeSource<'_>,
    output: crate::archive_set::ArchiveSetOutput,
    progress: &mut impl FnMut(u64),
) -> Result<
    (
        crate::archive_set::ArchiveSetOutput,
        SparseRangeMergeStats,
    ),
    String,
> {
    let directory = base_range_frame_directory(paths, slot)?;
    let mut base = std::fs::File::open(
        paths.base_archive().join(&slot.base_name),
    )
    .map_err(|error| error.to_string())?;
    let workers = usize::try_from(crate::archive::streaming_compression_workers())
        .unwrap_or(usize::MAX);
    let mut writer = crate::archive::ParallelArchiveWriter::new(
        output,
        MIRROR_FRAME_TARGET,
        mirror_compression(),
        prefix,
        workers,
    )
    .map_err(|error| error.to_string())?;
    let prefix: std::sync::Arc<[u8]> = std::sync::Arc::from(prefix);
    let mut stats = SparseRangeMergeStats::default();

    for position in 0..directory.len() {
        let entry = directory
            .get(position)
            .map_err(|error| error.to_string())?;
        while updates
            .peek_entity()
            .map_err(|error| error.to_string())?
            .is_some_and(|entity| entity < entry.first_entity)
        {
            let record = crate::archive::RecordSource::next_record(updates)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    "range source ended before its peeked record".to_string()
                })?;
            writer.write(&record).map_err(|error| error.to_string())?;
            stats.output_records = stats.output_records.saturating_add(1);
            progress(stats.output_records);
        }
        let intersects = updates
            .peek_entity()
            .map_err(|error| error.to_string())?
            .is_some_and(|entity| entity <= entry.last_entity);
        if intersects {
            let mut pending = None;
            let records = crate::archive::merge_frame_with_sorted_source(
                &mut writer,
                &base,
                entry,
                std::sync::Arc::clone(&prefix),
                updates,
                &mut pending,
            )
            .map_err(|error| error.to_string())?;
            if let Some(record) = pending {
                updates.restore(record).map_err(|error| error.to_string())?;
            }
            stats.decoded_frames = stats.decoded_frames.saturating_add(1);
            stats.decoded_compressed_bytes = stats
                .decoded_compressed_bytes
                .saturating_add(entry.compressed_bytes);
            stats.output_records = stats.output_records.saturating_add(records);
            progress(stats.output_records);
        } else {
            let copied = writer
                .append_compressed_frame(&mut base, entry)
                .map_err(|error| error.to_string())?;
            stats.copied_frames = stats.copied_frames.saturating_add(copied.frames);
            stats.copied_compressed_bytes = stats
                .copied_compressed_bytes
                .saturating_add(copied.compressed_bytes);
            stats.output_records = stats.output_records.saturating_add(copied.records);
            progress(stats.output_records);
        }
    }
    while let Some(record) = crate::archive::RecordSource::next_record(updates)
        .map_err(|error| error.to_string())?
    {
        writer.write(&record).map_err(|error| error.to_string())?;
        stats.output_records = stats.output_records.saturating_add(1);
        progress(stats.output_records);
    }
    let (output, frames) = writer.finish().map_err(|error| error.to_string())?;
    stats.output_frames = frames;
    Ok((output, stats))
}

fn apply_update_ranges(
    source: &crate::direct::UpdateSourcePlan,
    tail_receipt: &update_lifecycle::TailReceipt,
    range_plan: &update_lifecycle::RangePlanReceipt,
    paths: &update_lifecycle::UpdatePaths,
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
        previous_receipt = Some(receipt);
        completed += 1;
    }
    if range_plan.slots[completed..].iter().any(|slot| {
        paths.range_receipt(slot.index).exists()
    }) {
        return Err("range receipts contain a gap".into());
    }

    let cursor = previous_receipt
        .as_ref()
        .map(|receipt| receipt.tail_cursor.clone())
        .unwrap_or(update_lifecycle::TailCursorReceipt {
            frame_offset: all_frames.get(0).ok().map(|frame| frame.compressed_offset),
            record_ordinal: 0,
        });
    let remaining_start = match cursor.frame_offset {
        Some(offset) => all_frames.lower_bound_offset(offset),
        None if completed == 0 => 0,
        None => all_frames.len(),
    };
    if cursor
        .frame_offset
        .is_some_and(|offset| all_frames.index_of_offset(offset).is_none())
    {
        return Err("range receipt points between update-tail frames".into());
    }
    let mut tail = crate::archive::ArchiveRecordReader::open_frame_directory(
        paths.tail_archive(),
        std::sync::Arc::clone(&all_frames),
        remaining_start,
    )
    .map_err(|error| error.to_string())?;
    for _ in 0..cursor.record_ordinal {
        if tail
            .next_record()
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Err("tail cursor record ordinal lies beyond end of frame".into());
        }
    }
    let mut pending = None;
    let prefix = crate::archive::archive_ref_prefix_part(
        paths
            .base_archive()
            .join("0000-reference.swdump-part"),
    )
    .map_err(|error| error.to_string())?;
    let mut total_frames = 0_u64;
    let mut total_records = 0_u64;
    let mut frame_start = remaining_start;

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
        if pending.is_none() {
            pending = tail.next_record().map_err(|error| error.to_string())?;
        }
        let touched = pending
            .as_ref()
            .is_some_and(|record| update_record_belongs_to_range(record, kind, upper_id));
        let mut additions = 0_u64;
        let mut first_addition = None;
        let mut last_addition = None;
        let mut title_projection_name = None;
        let mut title_projection_bytes = 0;
        let mut title_projection_records = 0;
        let mut base_frame_bytes_copied = 0_u64;
        let mut base_frame_bytes_decoded = 0_u64;
        let selection = if !touched {
            update_lifecycle::RangeSelection::Unchanged {
                segment_id: slot.base_segment_id.clone(),
                name: slot.base_name.clone(),
                bytes: slot.base_bytes,
            }
        } else {
            eprintln!(
                "update range {}/{}: streaming {}",
                index + 1,
                range_plan.slots.len(),
                slot.base_name
            );
            let (title_building, mut title_writer) =
                begin_title_projection(paths, slot)?;
            let output = crate::archive_set::ArchiveSetOutput::new_in(&paths.root, u64::MAX)
                .map_err(|error| error.to_string())?;
            let mut last_progress = std::time::Instant::now();
            let mut range_source = LifecycleRangeSource {
                tail: &mut tail,
                pending: &mut pending,
                kind,
                upper_id,
                additions: &mut additions,
                first_addition: &mut first_addition,
                last_addition: &mut last_addition,
                title_writer: &mut title_writer,
                title_records: &mut title_projection_records,
            };
            let (output, merge_stats) = merge_sparse_update_range(
                paths,
                slot,
                &prefix,
                &mut range_source,
                output,
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
            )?;
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
            ) = finish_title_projection(
                paths,
                slot,
                &title_building,
                title_writer,
                title_projection_records,
            )?;
            let completed_archive = output.finish().map_err(|error| error.to_string())?;
            let replacement = paths
                .root
                .join("ranges")
                .join(format!(".building-{}", slot.candidate_id));
            if replacement.exists() {
                remove_path(&replacement)
                    .map_err(|error| format!("{}: {error}", replacement.display()))?;
            }
            std::fs::create_dir_all(replacement.parent().unwrap())
                .map_err(|error| format!("{}: {error}", replacement.display()))?;
            completed_archive
                .persist(&replacement)
                .map_err(|error| error.to_string())?;
            let replacement_set = crate::archive_set::ArchiveSetReader::open(&replacement)
                .map_err(|error| error.to_string())?;
            let replacement_range = replacement_set
                .segments()
                .iter()
                .find(|segment| segment.kind == Some(kind))
                .ok_or_else(|| "range replacement contains no entity segment".to_string())?
                .clone();
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
            let identity = crate::generation::GenerationId::parse(&slot.candidate_id)
                .and_then(|identity| identity.to_bytes())
                .map_err(|error| error.to_string())?;
            let frame_directory = crate::frame_directory::write_from_archive_segment(
                &object,
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
            remove_path(&replacement)
                .map_err(|error| format!("{}: {error}", replacement.display()))?;
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

        let tail_cursor = if pending.is_some() {
            let frame_offset = tail
                .current_frame_offset()
                .ok_or_else(|| "pending tail record has no source frame".to_string())?;
            let records_read = tail.current_frame_records_read();
            let record_ordinal = records_read
                .checked_sub(1)
                .ok_or_else(|| "pending tail record has no frame ordinal".to_string())?;
            update_lifecycle::TailCursorReceipt {
                frame_offset: Some(frame_offset),
                record_ordinal,
            }
        } else {
            update_lifecycle::TailCursorReceipt {
                frame_offset: None,
                record_ordinal: 0,
            }
        };
        let frame_end = all_frames.len() - tail.remaining_frame_count();
        let mut tail_bytes_read = 0_u64;
        for position in frame_start..frame_end {
            tail_bytes_read = tail_bytes_read
                .checked_add(
                    all_frames
                        .get(position)
                        .map_err(|error| error.to_string())?
                        .compressed_bytes,
                )
                .ok_or_else(|| "tail byte telemetry overflow".to_string())?;
        }
        frame_start = frame_end;
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
            tail_cursor,
            complete: true,
        };
        std::fs::create_dir_all(paths.range_receipt(slot.index).parent().unwrap())
            .map_err(|error| format!("{}: {error}", paths.root.display()))?;
        persist_json(&paths.range_receipt(slot.index), &receipt)?;
        total_frames = total_frames.saturating_add(frames);
        total_records = total_records.saturating_add(records);
        eprintln!(
            "update range {}/{} durable · tail {} bytes · base {} bytes · candidate {} bytes",
            index + 1,
            range_plan.slots.len(),
            receipt.tail_bytes_read,
            receipt.base_bytes_read,
            receipt.candidate_bytes_written
        );
    }
    if pending.is_some()
        || tail
            .next_record()
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Err("sorted update tail contains records outside the base range plan".into());
    }
    Ok((total_frames, total_records))
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
        remove_path(&paths.candidate_archive())
            .map_err(|error| format!("{}: {error}", paths.candidate_archive().display()))?;
    }
    let building = candidate_root.join(".archive-building");
    if building.exists() {
        remove_path(&building).map_err(|error| format!("{}: {error}", building.display()))?;
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

fn ensure_candidate_index(
    source: &crate::direct::UpdateSourcePlan,
    range_plan: &update_lifecycle::RangePlanReceipt,
    paths: &update_lifecycle::UpdatePaths,
) -> Result<(update_lifecycle::PreparedGenerationReceipt, u64), String> {
    if let Some(receipt) =
        update_lifecycle::read_receipt::<update_lifecycle::PreparedGenerationReceipt>(
            &paths.prepared_generation(),
        )
        .map_err(|error| error.to_string())?
    {
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
    let base_site_info = latest_site_info(&paths.base_archive(), &base_titles)?;
    let site_info = projected_site_info(range_plan, &receipts, paths, base_site_info)?;
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
        remove_path(&projection_work)
            .map_err(|error| format!("{}: {error}", projection_work.display()))?;
    }
    std::fs::create_dir_all(&projection_work)
        .map_err(|error| format!("{}: {error}", projection_work.display()))?;
    let tail_titles = crate::title_projection::project_title_record_archives(
        projection_inputs,
        site_info,
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
    };
    persist_json(&paths.prepared_generation(), &receipt)?;
    Ok((receipt, index.entries()))
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
    if installed_id == source.base_generation_id {
        let outcome = crate::installation_lifecycle::install(
            paths.candidate_archive(),
            paths.candidate_index(),
            archive,
        )?;
        if outcome.cleanup_pending {
            eprintln!(
                "new generation installed; previous generation cleanup is reader-deferred"
            );
        }
    }
    validate_installed_update_generation(archive, source)
}

fn validate_installed_update_generation(
    archive: &Path,
    source: &crate::direct::UpdateSourcePlan,
) -> Result<(), String> {
    let selected = crate::installation_lifecycle::serving_pair(archive)?
        .ok_or_else(|| "published update has no selected generation".to_string())?;
    let observed = crate::generation::generation_identity(&selected.archive, &selected.title)
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
    if selector.exists() {
        if let Err(error) = std::fs::remove_file(&selector) {
            eprintln!(
                "update committed; deferred selector cleanup {}: {error}",
                selector.display()
            );
        }
    }
    if let Err(error) = sync_parent(archive) {
        eprintln!("update committed; deferred directory sync: {error}");
    }
    if !selector.exists() {
        if let Err(error) = remove_path(&paths.root) {
            eprintln!(
                "update committed; deferred cleanup of {}: {error}",
                paths.root.display()
            );
        }
    }
    Ok(())
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
    run_id: Option<&str>,
) -> Result<(), String> {
    std::env::set_var("SARUN_MIRROR_DEST", archive);
    std::fs::create_dir_all(scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    // Fetch robots.txt once during discovery and leave the result in the
    // resumable build tree for every stage-one helper to consume.
    std::env::set_var("SARUN_WIKIMEDIA_ROBOTS_CACHE", scratch.join("robots-cache"));
    let _lock = MirrorBuildLock::acquire(scratch)?;
    if crate::installation_lifecycle::recover(archive)?
        .is_some_and(|outcome| outcome.cleanup_pending)
    {
        eprintln!("previous installed generation remains reader-leased; cleanup is pending");
    }
    if replace_plan {
        clear_mirror_scratch(scratch)?;
    }
    let inspected = inspect_build_for_start(scratch)?;
    let plan = match inspected {
        crate::build_lifecycle::BuildState::Unplanned => {
            let plan = crate::direct::discover_direct_build_plan(
                client,
                &wikimak_mediawiki::Config::default(),
                dbname,
                &|message| eprintln!("{message}"),
            )
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
    install_built_archive(built, archive)?;
    #[cfg(feature = "serve")]
    if let Err(error) = pack_selected_media(client, dbname, archive) {
        eprintln!(
            "text generation is installed; optional media remains pending: {error}"
        );
    }
    if let Err(error) = remove_path(scratch) {
        eprintln!(
            "text generation is installed; scratch cleanup remains pending at {}: {error}",
            scratch.display()
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

fn cmd_fetch(dbname: &str, archive: &str, run_id: Option<&str>) -> Result<(), String> {
    std::env::set_var("SARUN_MIRROR_DEST", archive);
    let archive = Path::new(archive);
    let client = http_client()?;
    if crate::installation_lifecycle::serving_pair(archive)?.is_none() {
        return build_full(
            &client,
            dbname,
            archive,
            &ensure_mirror_scratch(archive)?,
            false,
            run_id,
        );
    }
    let scratch = ensure_mirror_scratch(archive)?;
    let _lock = MirrorBuildLock::acquire(&scratch)?;
    let _ = crate::installation_lifecycle::recover(archive)?;
    std::env::set_var("SARUN_WIKIMEDIA_ROBOTS_CACHE", scratch.join("robots-cache"));
    let overlap_days = 3;
    let compression = mirror_compression();

    let mut resumed = match load_active_update(&scratch) {
        Ok(Some((active, paths))) => match load_update_plan(&active, &paths, dbname) {
            Ok(source) => {
                let base = if let Some(receipt) = update_lifecycle::read_receipt::<
                    update_lifecycle::PreservedBaseReceipt,
                >(&paths.base_receipt())
                .map_err(|error| error.to_string())?
                {
                    receipt.generation
                } else {
                    let selected = crate::installation_lifecycle::serving_pair(archive)?
                        .ok_or_else(|| {
                            format!("{} has no installed generation", archive.display())
                        })?;
                    crate::generation::generation_identity(&selected.archive, &selected.title)
                        .map_err(|error| error.to_string())?
                };
                Some((active, paths, source, base))
            }
            Err(error) => {
                abandon_invalid_update(&scratch, error)?;
                None
            }
        },
        Ok(None) => None,
        Err(error) => {
            abandon_invalid_update(&scratch, error)?;
            None
        }
    };
    let (_active, mut paths, mut source, mut base, resuming) =
        if let Some((active, paths, source, base)) = resumed.take() {
            (active, paths, source, base, true)
        } else {
            let (active, paths, source, base) = create_update_plan(
                &client,
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
            Err(error) => {
                abandon_invalid_update(&scratch, error)?;
                let replacement = create_update_plan(
                    &client,
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
    loop {
        let installed_id = installed_generation_id(archive)?;
        let state = update_lifecycle::inspect_update(&paths, installed_id.as_str())
            .map_err(|error| error.to_string())?;
        let phase = state.phase();
        let action = state.next_action();
        match (action, state) {
            (
                update_lifecycle::UpdateAction::PublishTail,
                update_lifecycle::UpdateState::Planned(_),
            ) => {
                update_step(
                    phase,
                    ensure_update_tail(&client, &source, &paths),
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
                update_lifecycle::UpdateAction::PublishRangePlan,
                update_lifecycle::UpdateState::BasePreserved(_, tail, preserved),
            ) => {
                update_step(
                    phase,
                    ensure_range_plan(
                        &source,
                        &tail,
                        &preserved.generation,
                        &paths,
                    ),
                )?;
            }
            (
                update_lifecycle::UpdateAction::PublishRange,
                update_lifecycle::UpdateState::ApplyingRanges {
                    tail,
                    ranges,
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
                    apply_update_ranges(&source, &tail, &ranges, &paths),
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
                update_step(phase, ensure_candidate_archive(&ranges, &paths))?;
            }
            (
                update_lifecycle::UpdateAction::PublishIndex,
                update_lifecycle::UpdateState::CandidateComplete { ranges, .. },
            ) => {
                let (_, title_entries) = update_step(
                    phase,
                    ensure_candidate_index(&source, &ranges, &paths),
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
                    validate_installed_update_generation(archive, &source),
                )?;
                update_step(phase, publish_update_commit(&source, &paths))?;
                eprintln!(
                    "committed Wikipedia generation {}",
                    source.generation_id.as_str()
                );
            }
            (
                update_lifecycle::UpdateAction::Cleanup,
                update_lifecycle::UpdateState::Committed(_),
            ) => {
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
    build_full(
        &http_client()?,
        dbname,
        archive,
        &ensure_mirror_scratch(archive)?,
        true,
        run_id,
    )
}

fn abandon_invalid_update(
    scratch: &Path,
    detail: impl std::fmt::Display,
) -> Result<(), String> {
    eprintln!(
        "discarding invalid temporary update state at {}; installed generation preserved ({detail})",
        scratch.display()
    );
    clear_mirror_scratch(scratch)
}

/// Explicitly abandon an invalid destination-local build/update tree.  This
/// is intentionally narrower than `refresh-full`: valid resumable work and
/// committed installed generations are left untouched. Invalid private
/// update output is discarded rather than adopted by a compatibility path.
fn cmd_reset(dbname: &str, archive: &str) -> Result<(), String> {
    let archive = Path::new(archive);
    let scratch = ensure_mirror_scratch(archive)?;
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
        Err(error) => abandon_invalid_build(&scratch, &error)?,
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
                    clear_mirror_scratch(&scratch)?;
                }
            }
        }
        Err(error) => {
            eprintln!(
                "discarding invalid temporary update selector at {}; installed generation preserved ({error})",
                update_selector_path(&scratch).display()
            );
            clear_mirror_scratch(&scratch)?;
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
    let started = std::time::Instant::now();
    let mirror_path = PathBuf::from(path);
    let archive = crate::installation_lifecycle::with_serving_pair(
        &mirror_path,
        |selected| {
            crate::archive_browse::ArchiveBrowseIndex::open(
                &selected.archive,
                &selected.title,
            )
            .map_err(|error| error.to_string())
        },
    )?;
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
    if matches!(
        args.as_slice(),
        ["fetch" | "refresh-full", _, _]
            | ["fetch" | "refresh-full", "--run-id", _, _, _]
    ) {
        arm_parent_watchdog();
    }
    let result = match args.as_slice() {
        ["discover", dbname] => cmd_discover(dbname),
        ["fetch", dbname, archive] => cmd_fetch(dbname, archive, None),
        ["refresh-full", dbname, archive] => cmd_refresh_full(dbname, archive, None),
        ["reset", dbname, archive] => cmd_reset(dbname, archive),
        ["fetch", "--run-id", run_id, dbname, archive] => {
            cmd_fetch(dbname, archive, Some(run_id))
        }
        ["refresh-full", "--run-id", run_id, dbname, archive] => {
            cmd_refresh_full(dbname, archive, Some(run_id))
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
             \x20      wikimak reset <dbname> <archive.swdump>\n\
             \x20      wikimak serve <archive.swdump> [addr] [--packed-media <directory>]\n\
             \x20      wikimak kiwix-pack <source.zim> <output-directory>\n\
             \x20      wikimak siteinfo <api-url> <output.swdump>\n\
             \x20      wikimak title-index <archive.swdump> <output.swtitle> <generation-id>\n\
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
    use super::*;

    fn sparse_range_fixture(
        update_page_id: u64,
    ) -> (SparseRangeMergeStats, Vec<u64>) {
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
        for page_id in 1..=5_u64 {
            writer
                .write(&crate::archive::Record::PageState {
                    page_id,
                    timestamp_micros: 100,
                    title: format!("Base page {page_id} {}", "x".repeat(256)),
                    namespace: None,
                    deleted: false,
                })
                .unwrap();
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
        let base_directory = base_range_frame_directory(&paths, &slot).unwrap();
        let base_frame_bytes = (0..base_directory.len())
            .map(|position| {
                base_directory.get(position).unwrap().compressed_bytes
            })
            .collect::<Vec<_>>();

        let tail_path = root.path().join("tail.swdump");
        let mut tail_writer =
            crate::archive::ArchiveWriter::new(std::fs::File::create(&tail_path).unwrap(), 1)
                .unwrap();
        tail_writer
            .write(&crate::archive::Record::PageState {
                page_id: update_page_id,
                timestamp_micros: 200,
                title: format!("Updated page {update_page_id}"),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        tail_writer.finish().unwrap();
        let mut tail = crate::archive::ArchiveRecordReader::open(&tail_path).unwrap();
        let mut pending = None;
        let title_path = root.path().join("titles.swdump");
        let mut title_writer =
            crate::archive::ArchiveWriter::new(
                std::fs::File::create(&title_path).unwrap(),
                1024,
            )
            .unwrap();
        let mut additions = 0;
        let mut first = None;
        let mut last = None;
        let mut title_records = 0;
        let output =
            crate::archive_set::ArchiveSetOutput::new_in(root.path(), 1 << 20)
                .unwrap();
        let (output, stats) = {
            let mut source = LifecycleRangeSource {
                tail: &mut tail,
                pending: &mut pending,
                kind: crate::archive::EntityKind::Page,
                upper_id: u64::MAX,
                additions: &mut additions,
                first_addition: &mut first,
                last_addition: &mut last,
                title_writer: &mut title_writer,
                title_records: &mut title_records,
            };
            merge_sparse_update_range(
                &paths,
                &slot,
                &prefix,
                &mut source,
                output,
                &mut |_| {},
            )
            .unwrap()
        };
        output.finish().unwrap();
        title_writer.finish().unwrap();
        (stats, base_frame_bytes)
    }

    #[test]
    fn sparse_range_raw_copies_every_unaffected_frame() {
        let (stats, base_frame_bytes) = sparse_range_fixture(6);
        assert_eq!(stats.decoded_frames, 0);
        assert_eq!(stats.decoded_compressed_bytes, 0);
        assert_eq!(stats.copied_frames, 5);
        assert_eq!(
            stats.copied_compressed_bytes,
            base_frame_bytes.iter().copied().sum::<u64>(),
        );
    }

    #[test]
    fn sparse_range_decodes_only_the_intersecting_entity_frame() {
        let (stats, base_frame_bytes) = sparse_range_fixture(3);
        assert_eq!(stats.decoded_frames, 1);
        assert_eq!(stats.decoded_compressed_bytes, base_frame_bytes[2]);
        assert_eq!(stats.copied_frames, 4);
        assert_eq!(
            stats.copied_compressed_bytes,
            base_frame_bytes
                .iter()
                .enumerate()
                .filter_map(|(index, bytes)| (index != 2).then_some(bytes))
                .copied()
                .sum::<u64>(),
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
    fn start_discards_only_malformed_temporary_plan() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plan.json"), b"old plan").unwrap();
        assert!(matches!(
            inspect_build_for_start(root.path()).unwrap(),
            crate::build_lifecycle::BuildState::Unplanned
        ));
        assert!(!root.path().join("plan.json").exists());
    }

    #[test]
    fn start_discards_invalid_tree_with_unusable_candidate() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plan.json"), b"old plan").unwrap();
        std::fs::write(root.path().join("archive.swdump"), b"candidate").unwrap();
        assert!(matches!(
            inspect_build_for_start(root.path()).unwrap(),
            crate::build_lifecycle::BuildState::Unplanned
        ));
        assert!(!root.path().join("plan.json").exists());
        assert!(!root.path().join("archive.swdump").exists());
    }

    #[test]
    fn invalid_update_reset_discards_unusable_candidate() {
        let root = tempfile::tempdir().unwrap();
        let paths = update_lifecycle::UpdatePaths::new(root.path().join("updates/u1"));
        std::fs::create_dir_all(paths.candidate_inventory().parent().unwrap()).unwrap();
        std::fs::write(paths.candidate_inventory(), b"candidate").unwrap();
        abandon_invalid_update(root.path(), "malformed update receipt").unwrap();
        assert!(!paths.candidate_inventory().exists());
    }

}
