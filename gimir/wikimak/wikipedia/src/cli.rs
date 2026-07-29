//! `wikimak` command line for portable Wikipedia archives.

use std::io::BufReader;
use std::path::{Path, PathBuf};

const MIRROR_FRAME_TARGET: usize = 128 << 10;
const MIRROR_DICTIONARY_BYTES: usize = 800 << 10;

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

fn mirror_scratch(archive: &Path) -> PathBuf {
    archive
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(".wikimak-scratch")
}

fn install_archive(source: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;

    let input = std::fs::File::open(source)
        .map_err(|error| format!("{}: {error}", source.display()))?;
    let mut archive = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    let (_, stats) = crate::archive::repack_with_dictionary(
        BufReader::new(input),
        archive.as_file_mut(),
        MIRROR_FRAME_TARGET,
        mirror_compression(),
        MIRROR_DICTIONARY_BYTES,
    )
    .map_err(|error| error.to_string())?;
    archive
        .as_file()
        .sync_all()
        .map_err(|error| format!("{}: {error}", archive.path().display()))?;

    let title_path = destination.with_extension("swtitle");
    let mut titles = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    let title_entries = crate::title_index::build(archive.path(), titles.path())
        .map_err(|error| error.to_string())?;
    titles
        .as_file_mut()
        .sync_all()
        .map_err(|error| format!("{}: {error}", titles.path().display()))?;

    archive
        .persist(destination)
        .map_err(|error| format!("{}: {}", destination.display(), error.error))?;
    titles
        .persist(&title_path)
        .map_err(|error| format!("{}: {}", title_path.display(), error.error))?;
    println!(
        "{} records, {} frames, {} title intervals, {}-byte dictionary",
        stats.records, stats.output_frames, title_entries, stats.dictionary_bytes
    );
    Ok(())
}

fn build_full(
    client: &reqwest::blocking::Client,
    dbname: &str,
    archive: &Path,
    scratch_parent: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(scratch_parent)
        .map_err(|error| format!("{}: {error}", scratch_parent.display()))?;
    let scratch = tempfile::TempDir::new_in(scratch_parent)
        .map_err(|error| format!("{}: {error}", scratch_parent.display()))?;
    let raw = scratch.path().join("full.swdump");
    crate::build_direct_archive(
        client,
        &wikimak_mediawiki::Config::default(),
        dbname,
        &raw,
        scratch.path(),
        |message| eprintln!("{message}"),
    )
    .map_err(|error| error.to_string())?;
    install_archive(&raw, archive)
}

fn cmd_fetch(dbname: &str, archive: &str) -> Result<(), String> {
    let archive = Path::new(archive);
    let scratch_parent = mirror_scratch(archive);
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

    let merged = scratch.path().join("merged.swdump");
    let inputs = vec![archive.to_path_buf(), partial];
    let output = std::fs::File::create(&merged)
        .map_err(|error| format!("{}: {error}", merged.display()))?;
    crate::archive::merge_many_archives_with_compression_in(
        &inputs,
        output,
        MIRROR_FRAME_TARGET,
        mirror_compression(),
        scratch.path(),
    )
    .map_err(|error| error.to_string())?;
    install_archive(&merged, archive)
}

fn cmd_refresh_full(dbname: &str, archive: &str) -> Result<(), String> {
    let archive = Path::new(archive);
    build_full(
        &http_client()?,
        dbname,
        archive,
        &mirror_scratch(archive),
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

fn cmd_repack(args: &[&str]) -> Result<(), String> {
    let [input, output, frame_target, level, options @ ..] = args else {
        return Err(
            "repack wants <input> <output> <frame-bytes> <zstd-level> [--dictionary-bytes N]"
                .into(),
        );
    };
    let frame_target = positive_size(frame_target, "frame bytes")?;
    let compression = compression(level)?;
    let dictionary = match options {
        [] => None,
        ["--dictionary-bytes", bytes] => Some(positive_size(bytes, "dictionary bytes")?),
        _ => return Err("unknown repack options".into()),
    };
    let input_file =
        std::fs::File::open(input).map_err(|error| format!("{input}: {error}"))?;
    let output_path = Path::new(output);
    let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("{}: {error}", parent.display()))?;
    let result = match dictionary {
        Some(bytes) => crate::archive::repack_with_dictionary(
            BufReader::new(input_file),
            temporary.as_file_mut(),
            frame_target,
            compression,
            bytes,
        ),
        None => crate::archive::repack(
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
        "{} records, {} frames, dictionary {} bytes",
        stats.records, stats.output_frames, stats.dictionary_bytes
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
    let (_, frames, records) = crate::archive::merge_many_archives_with_compression_in(
        &input_paths,
        temporary.as_file_mut(),
        frame_target,
        compression(level)?,
        parent,
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

pub fn cli_main(args: &[String]) -> i32 {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
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
             \x20      wikimak repack <input> <output> <frame-bytes> <zstd-level> [--dictionary-bytes N]\n\
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
