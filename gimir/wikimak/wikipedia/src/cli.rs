//! `wikimak` command line for portable Wikipedia archives.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::archive::MIRROR_FRAME_TARGET;

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

fn reset_mirror_scratch(archive: &Path) -> Result<PathBuf, String> {
    let scratch = mirror_scratch_path(archive);
    if scratch.exists() {
        remove_path(&scratch).map_err(|error| format!("{}: {error}", scratch.display()))?;
    }
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    Ok(scratch)
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
        (false, false) => {}
        (archive_backup, title_backup) => {
            if archive_backup {
                remove_path(&old_archive)
                    .map_err(|error| format!("{}: {error}", old_archive.display()))?;
            }
            if title_backup {
                std::fs::remove_file(&old_title)
                    .map_err(|error| format!("{}: {error}", old_title.display()))?;
            }
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
        std::fs::rename(&archive, destination)
            .map_err(|error| format!("{}: {error}", destination.display()))?;
        if let Err(error) = titles.persist(&title) {
            let cleanup = remove_path(destination);
            return match cleanup {
                Ok(()) => Err(format!("{}: {}", title.display(), error.error)),
                Err(cleanup) => Err(format!(
                    "{}: {}; could not remove incomplete {}: {cleanup}",
                    title.display(),
                    error.error,
                    destination.display()
                )),
            };
        }
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

    eprintln!("building title history index");
    let mut titles = tempfile::NamedTempFile::new_in(scratch)
        .map_err(|error| format!("{}: {error}", scratch.display()))?;
    let title_entries = crate::title_index::build(&archive, titles.path())
        .map_err(|error| error.to_string())?;
    titles
        .as_file_mut()
        .sync_all()
        .map_err(|error| format!("{}: {error}", titles.path().display()))?;

    eprintln!("installing completed archive and title index");
    persist_archive_pair(archive, titles, destination)?;
    eprintln!("{title_entries} title intervals");
    Ok(())
}

fn build_full(
    client: &reqwest::blocking::Client,
    dbname: &str,
    archive: &Path,
    scratch_parent: &Path,
) -> Result<(), String> {
    recover_interrupted_install(archive)?;
    std::fs::create_dir_all(scratch_parent)
        .map_err(|error| format!("{}: {error}", scratch_parent.display()))?;
    let scratch = tempfile::TempDir::new_in(scratch_parent)
        .map_err(|error| format!("{}: {error}", scratch_parent.display()))?;
    let build_root = tempfile::TempDir::new_in(scratch.path())
        .map_err(|error| format!("{}: {error}", scratch.path().display()))?;
    let built = build_root.path().join("archive.swdump");
    crate::build_direct_archive(
        client,
        &wikimak_mediawiki::Config::default(),
        dbname,
        &built,
        scratch.path(),
        |message| eprintln!("{message}"),
    )
    .map_err(|error| error.to_string())?;
    install_built_archive(built, archive, scratch.path())
}

fn cmd_fetch(dbname: &str, archive: &str) -> Result<(), String> {
    let archive = Path::new(archive);
    recover_interrupted_install(archive)?;
    let scratch_parent = reset_mirror_scratch(archive)?;
    let client = http_client()?;
    if !archive.exists() {
        return build_full(&client, dbname, archive, &scratch_parent);
    }
    std::fs::create_dir_all(&scratch_parent)
        .map_err(|error| format!("{}: {error}", scratch_parent.display()))?;
    let scratch = tempfile::TempDir::new_in(&scratch_parent)
        .map_err(|error| format!("{}: {error}", scratch_parent.display()))?;
    let partial = scratch.path().join("update.swdump");
    crate::build_update_archive(
        &client,
        &wikimak_mediawiki::Config::default(),
        dbname,
        archive,
        &partial,
        scratch.path(),
        3,
        MIRROR_FRAME_TARGET,
        mirror_compression(),
        |message| eprintln!("{message}"),
    )
    .map_err(|error| error.to_string())?;

    let update_marker = install_sidecar(archive, ".updating")?;
    if !update_marker.exists() {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&update_marker)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("{}: {error}", update_marker.display()))?;
        sync_parent(archive)?;
    }
    eprintln!("streaming current archive and updates through page-ID range files");
    let inputs = vec![archive.to_path_buf(), partial];
    let set = crate::archive_set::ArchiveSetReader::open(archive)
        .map_err(|error| error.to_string())?;
    let boundaries = set
        .segments()
        .iter()
        .filter_map(|segment| {
            segment.kind.map(|kind| crate::archive::EntityKey {
                kind,
                id: segment.last_id,
            })
        })
        .collect::<Vec<_>>();
    let merged = crate::archive_set::ArchiveSetOutput::replacing(
        archive,
        crate::archive_set::DEFAULT_RANGE_TARGET,
        set.segments(),
    )
    .map_err(|error| error.to_string())?;
    let (merged, frames, records) =
        crate::archive::merge_many_archives_reusing_ref_prefix_at_boundaries(
        archive,
        &inputs,
        merged,
        MIRROR_FRAME_TARGET,
        mirror_compression(),
        boundaries,
    )
    .map_err(|error| error.to_string())?;
    let completed = merged.finish().map_err(|error| error.to_string())?;
    completed
        .finish_replacement()
        .map_err(|error| error.to_string())?;

    eprintln!("rebuilding the single title and virtual-frame index");
    let mut titles = tempfile::NamedTempFile::new_in(scratch.path())
        .map_err(|error| format!("{}: {error}", scratch.path().display()))?;
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
    eprintln!("{records} records, {frames} frames, {title_entries} title intervals");
    Ok(())
}

fn cmd_refresh_full(dbname: &str, archive: &str) -> Result<(), String> {
    let archive = Path::new(archive);
    build_full(
        &http_client()?,
        dbname,
        archive,
        &reset_mirror_scratch(archive)?,
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
fn cmd_serve(path: &str, addr: &str) -> Result<(), String> {
    let started = std::time::Instant::now();
    let path = PathBuf::from(path);
    if install_sidecar(&path, ".installing")?.exists() {
        return Err("Wikipedia archive generation switch is in progress".into());
    }
    if install_sidecar(&path, ".updating")?.exists() {
        return Err("Wikipedia range-file update is in progress".into());
    }
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
    crate::serve::serve_archive(
        std::sync::Arc::new(archive),
        addr.to_owned(),
        path.with_extension("media"),
    )
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

fn cmd_repack(args: &[&str]) -> Result<(), String> {
    let [input, output, frame_target, level, options @ ..] = args else {
        return Err(
            "repack wants <input> <output> <frame-bytes> <zstd-level> \
             [--dictionary-bytes N | --ref-prefix-bytes N --sample-bytes N]"
                .into(),
        );
    };
    let frame_target = positive_size(frame_target, "frame bytes")?;
    let compression = compression(level)?;
    enum Reference {
        None,
        Dictionary(usize),
        RefPrefix { bytes: usize, sample_bytes: usize },
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
        _ => return Err("unknown repack options".into()),
    };
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
                std::process::exit(1);
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
        ["serve", archive] => cmd_serve(archive, "127.0.0.1:8642"),
        #[cfg(feature = "serve")]
        ["serve", archive, addr] => cmd_serve(archive, addr),
        ["siteinfo", api_url, output] => cmd_siteinfo(api_url, output),
        ["title-index", archive, output] => cmd_title_index(archive, output),
        ["repack", arguments @ ..] => cmd_repack(arguments),
        ["merge", arguments @ ..] => cmd_merge(arguments),
        ["inspect", archive] => cmd_inspect(archive),
        _ => Err(
            "usage: wikimak discover <dbname>\n\
             \x20      wikimak fetch <dbname> <archive.swdump>\n\
             \x20      wikimak refresh-full <dbname> <archive.swdump>\n\
             \x20      wikimak serve <archive.swdump> [addr]\n\
             \x20      wikimak siteinfo <api-url> <output.swdump>\n\
             \x20      wikimak title-index <archive.swdump> <output.swtitle>\n\
             \x20      wikimak repack <input> <output> <frame-bytes> <zstd-level> [--dictionary-bytes N | --ref-prefix-bytes N --sample-bytes N]\n\
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
}
