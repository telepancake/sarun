//! Publication of immutable, generation-addressed Wikipedia archives.
//!
//! The stable title index is the sole selector. Its embedded generation ID
//! names one immutable archive directory. Publication therefore has one
//! visibility boundary: atomically replacing the selector.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest;

const INSTALL_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PublishReceipt {
    schema: u32,
    publication_id: String,
    candidate_generation_id: String,
    selected_before_publish: Option<String>,
    cleanup_generation_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstallOutcome {
    pub(crate) cleanup_pending: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServingPair {
    pub(crate) archive: PathBuf,
    pub(crate) title: PathBuf,
}

pub(crate) fn generation_root(destination: &Path) -> PathBuf {
    destination.with_extension("generations")
}

fn selector_path(destination: &Path) -> PathBuf {
    destination.with_extension("swtitle")
}

fn receipt_path(destination: &Path) -> PathBuf {
    destination.with_extension("install.json")
}

fn generation_path(destination: &Path, generation_id: &str) -> Result<PathBuf, String> {
    if generation_id.len() != 64
        || !generation_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(format!("invalid generation ID {generation_id:?}"));
    }
    Ok(generation_root(destination).join(generation_id))
}

fn pending_selector_path(destination: &Path, generation_id: &str) -> Result<PathBuf, String> {
    Ok(generation_root(destination).join(format!("{generation_id}.swtitle.pending")))
}

fn path_exists(path: &Path) -> Result<bool, String> {
    path.try_exists()
        .map_err(|error| format!("inspect {}: {error}", path.display()))
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    Ok(())
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn selected_generation(destination: &Path) -> Result<Option<String>, String> {
    let selector = selector_path(destination);
    if !path_exists(&selector)? {
        return Ok(None);
    }
    crate::title_index::TitleIndex::open(&selector)
        .map(|titles| Some(titles.generation_id().as_str().to_owned()))
        .map_err(|error| format!("open selector {}: {error}", selector.display()))
}

fn publication_id(
    candidate_generation_id: &str,
    selected_before_publish: Option<&str>,
    cleanup_generation_ids: &[String],
) -> String {
    hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&(
            "wikipedia-generation-publication",
            candidate_generation_id,
            selected_before_publish,
            cleanup_generation_ids,
        ))
        .expect("publication identity is serializable"),
    ))
}

fn validate_receipt(receipt: &PublishReceipt, path: &Path) -> Result<(), String> {
    if receipt.schema != INSTALL_SCHEMA {
        return Err(format!(
            "{} has unsupported schema {}",
            path.display(),
            receipt.schema
        ));
    }
    generation_path(Path::new("mirror.swdump"), &receipt.candidate_generation_id)?;
    if let Some(selected) = receipt.selected_before_publish.as_deref() {
        generation_path(Path::new("mirror.swdump"), selected)?;
    }
    for generation in &receipt.cleanup_generation_ids {
        generation_path(Path::new("mirror.swdump"), generation)?;
    }
    if receipt.publication_id
        != publication_id(
            &receipt.candidate_generation_id,
            receipt.selected_before_publish.as_deref(),
            &receipt.cleanup_generation_ids,
        )
    {
        return Err(format!("{} has a foreign publication identity", path.display()));
    }
    Ok(())
}

fn read_receipt(destination: &Path) -> Result<Option<PublishReceipt>, String> {
    let path = receipt_path(destination);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let receipt: PublishReceipt = serde_json::from_slice(&bytes)
                .map_err(|error| format!("decode {}: {error}", path.display()))?;
            validate_receipt(&receipt, &path)?;
            Ok(Some(receipt))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn persist_receipt(destination: &Path, receipt: &PublishReceipt) -> Result<(), String> {
    let path = receipt_path(destination);
    let mut temporary = tempfile::NamedTempFile::new_in(parent(&path))
        .map_err(|error| format!("create receipt beside {}: {error}", path.display()))?;
    serde_json::to_writer(&mut temporary, receipt)
        .map_err(|error| format!("encode {}: {error}", path.display()))?;
    temporary
        .write_all(b"\n")
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    temporary
        .persist(&path)
        .map_err(|error| format!("publish {}: {}", path.display(), error.error))?;
    sync_directory(parent(&path))
}

fn generation_identity(
    archive: &Path,
    title: &Path,
) -> Result<crate::generation::GenerationIdentity, String> {
    crate::generation::generation_identity(archive, title).map_err(|error| error.to_string())
}

fn populate_generation(
    candidate_archive: &Path,
    candidate_title: &Path,
    destination: &Path,
    generation_id: &str,
) -> Result<PathBuf, String> {
    let root = generation_root(destination);
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create {}: {error}", root.display()))?;
    let generation = generation_path(destination, generation_id)?;
    if path_exists(&generation)? {
        let identity = generation_identity(&generation, candidate_title)?;
        if identity.generation_id.as_str() != generation_id {
            return Err(format!(
                "{} does not contain generation {}",
                generation.display(),
                generation_id
            ));
        }
        return Ok(generation);
    }

    let archive = crate::archive_set::ArchiveSetReader::open(candidate_archive)
        .map_err(|error| format!("open candidate {}: {error}", candidate_archive.display()))?;
    let temporary = tempfile::Builder::new()
        .prefix(".generation-")
        .tempdir_in(&root)
        .map_err(|error| format!("stage generation in {}: {error}", root.display()))?;
    for segment in archive.segments() {
        let source = candidate_archive.join(&segment.name);
        let target = temporary.path().join(&segment.name);
        std::fs::File::open(&source)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("sync candidate segment {}: {error}", source.display()))?;
        std::fs::hard_link(&source, &target).map_err(|error| {
            format!(
                "hard-link candidate segment {} into destination-local generation: {error}",
                source.display()
            )
        })?;
    }
    sync_directory(temporary.path())?;
    let temporary_path = temporary.path().to_path_buf();
    std::fs::rename(&temporary_path, &generation)
        .map_err(|error| format!("publish generation {}: {error}", generation.display()))?;
    drop(temporary);
    sync_directory(&root)?;
    generation_identity(&generation, candidate_title)?;
    Ok(generation)
}

fn stage_selector(
    candidate_title: &Path,
    destination: &Path,
    generation_id: &str,
) -> Result<PathBuf, String> {
    let pending = pending_selector_path(destination, generation_id)?;
    if path_exists(&pending)? {
        let titles = crate::title_index::TitleIndex::open(&pending)
            .map_err(|error| format!("open pending selector {}: {error}", pending.display()))?;
        if titles.generation_id().as_str() != generation_id {
            return Err(format!(
                "{} names generation {}, expected {}",
                pending.display(),
                titles.generation_id().as_str(),
                generation_id
            ));
        }
        return Ok(pending);
    }
    if let Err(link_error) = std::fs::hard_link(candidate_title, &pending) {
        std::fs::copy(candidate_title, &pending).map_err(|copy_error| {
            format!(
                "stage selector {} (hard link: {link_error}; copy: {copy_error})",
                pending.display()
            )
        })?;
    }
    std::fs::File::open(&pending)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync pending selector {}: {error}", pending.display()))?;
    sync_directory(&generation_root(destination))?;
    Ok(pending)
}

fn cleanup_displaced(
    destination: &Path,
    receipt: &mut PublishReceipt,
) -> Result<bool, String> {
    let mut pending = Vec::new();
    for generation_id in &receipt.cleanup_generation_ids {
        if generation_id == &receipt.candidate_generation_id {
            continue;
        }
        let generation = generation_path(destination, generation_id)?;
        if !path_exists(&generation)? {
            continue;
        }
        match crate::archive::try_acquire_archive_cleanup_lease(&generation) {
            Ok(Some(_lease)) => {
                if let Err(error) = std::fs::remove_dir_all(&generation) {
                    eprintln!(
                        "generation {} is no longer selected; cleanup remains pending: {error}",
                        generation.display()
                    );
                    pending.push(generation_id.clone());
                }
            }
            Ok(None) => pending.push(generation_id.clone()),
            Err(error) => {
                eprintln!(
                    "generation {} is no longer selected; lease check remains pending: {error}",
                    generation.display()
                );
                pending.push(generation_id.clone());
            }
        }
    }
    receipt.cleanup_generation_ids = pending;
    if receipt.cleanup_generation_ids.is_empty() {
        match std::fs::remove_file(receipt_path(destination)) {
            Ok(()) => sync_directory(parent(destination))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                eprintln!(
                    "generation is selected; cleanup receipt removal remains pending: {error}"
                );
                return Ok(true);
            }
        }
        Ok(false)
    } else {
        persist_receipt(destination, receipt)?;
        Ok(true)
    }
}

/// Complete a receipted selector publication and its independent old-generation cleanup.
pub(crate) fn recover(destination: &Path) -> Result<Option<InstallOutcome>, String> {
    let Some(mut receipt) = read_receipt(destination)? else {
        return Ok(None);
    };
    let selected = selected_generation(destination)?;
    if selected.as_deref() != Some(&receipt.candidate_generation_id) {
        if selected.as_deref() != receipt.selected_before_publish.as_deref() {
            return Err(format!(
                "selector changed outside publication {}",
                receipt.publication_id
            ));
        }
        let generation =
            generation_path(destination, &receipt.candidate_generation_id)?;
        let pending =
            pending_selector_path(destination, &receipt.candidate_generation_id)?;
        if !path_exists(&generation)? || !path_exists(&pending)? {
            return Err(
                "publication receipt lacks its immutable generation or pending selector".into(),
            );
        }
        let identity = generation_identity(&generation, &pending)?;
        if identity.generation_id.as_str() != receipt.candidate_generation_id {
            return Err("pending selector and immutable generation disagree".into());
        }
        let selector = selector_path(destination);
        std::fs::rename(&pending, &selector)
            .map_err(|error| format!("publish selector {}: {error}", selector.display()))?;
        sync_directory(parent(&selector))?;
    }
    let cleanup_pending = cleanup_displaced(destination, &mut receipt)?;
    Ok(Some(InstallOutcome { cleanup_pending }))
}

/// Install a validated candidate without moving or invalidating its build artifacts.
pub(crate) fn install(
    candidate_archive: PathBuf,
    candidate_title: PathBuf,
    destination: &Path,
) -> Result<InstallOutcome, String> {
    std::fs::create_dir_all(parent(destination))
        .map_err(|error| format!("create {}: {error}", parent(destination).display()))?;

    let previous_cleanup = if let Some(outcome) = recover(destination)? {
        if outcome.cleanup_pending {
            read_receipt(destination)?
                .map(|receipt| receipt.cleanup_generation_ids)
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let identity = generation_identity(&candidate_archive, &candidate_title)?;
    let candidate_generation_id = identity.generation_id.as_str().to_owned();
    let selected_before_publish = selected_generation(destination)?;
    if selected_before_publish.as_deref() == Some(&candidate_generation_id) {
        return Ok(InstallOutcome {
            cleanup_pending: !previous_cleanup.is_empty(),
        });
    }

    populate_generation(
        &candidate_archive,
        &candidate_title,
        destination,
        &candidate_generation_id,
    )?;
    stage_selector(&candidate_title, destination, &candidate_generation_id)?;

    let mut cleanup_generation_ids = previous_cleanup;
    if let Some(previous) = selected_before_publish.as_ref() {
        if previous != &candidate_generation_id
            && !cleanup_generation_ids.iter().any(|value| value == previous)
        {
            cleanup_generation_ids.push(previous.clone());
        }
    }
    cleanup_generation_ids.sort();
    cleanup_generation_ids.dedup();
    let receipt = PublishReceipt {
        schema: INSTALL_SCHEMA,
        publication_id: publication_id(
            &candidate_generation_id,
            selected_before_publish.as_deref(),
            &cleanup_generation_ids,
        ),
        candidate_generation_id,
        selected_before_publish,
        cleanup_generation_ids,
    };
    persist_receipt(destination, &receipt)?;
    recover(destination)?
        .ok_or_else(|| "publication receipt disappeared before selector commit".into())
}

/// Select one immutable archive without scanning the generation directory.
pub(crate) fn serving_pair(destination: &Path) -> Result<Option<ServingPair>, String> {
    let Some(generation_id) = selected_generation(destination)? else {
        return Ok(None);
    };
    let archive = generation_path(destination, &generation_id)?;
    if !path_exists(&archive)? {
        return Err(format!(
            "selector names unavailable generation {}",
            archive.display()
        ));
    }
    Ok(Some(ServingPair {
        archive,
        title: selector_path(destination),
    }))
}

/// Retry a read-only open only when atomic selector replacement changed the selection.
pub(crate) fn with_serving_pair<T>(
    destination: &Path,
    mut open: impl FnMut(&ServingPair) -> Result<T, String>,
) -> Result<T, String> {
    let first = serving_pair(destination)?
        .ok_or_else(|| format!("{} has no committed generation", destination.display()))?;
    match open(&first) {
        Ok(value) => Ok(value),
        Err(first_error) => {
            let second = serving_pair(destination)?
                .ok_or_else(|| format!("{} has no committed generation", destination.display()))?;
            if second == first {
                Err(first_error)
            } else {
                open(&second)
            }
        }
    }
}

/// All persistent paths owned by one logical Wikipedia mirror.
pub(crate) fn auxiliary_paths(destination: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(vec![
        selector_path(destination),
        generation_root(destination),
        receipt_path(destination),
        destination.with_extension("swrefs"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{
        ArchiveWriter, CompressionSettings, ManifestRecord, Record, SiteInfoRecord,
    };

    fn generation(parent: &Path, name: &str) -> (PathBuf, PathBuf, String) {
        let archive = parent.join(format!("{name}.swdump"));
        let title = archive.with_extension("swtitle");
        let output = crate::archive_set::ArchiveSetOutput::new_in(parent, 1 << 20).unwrap();
        let mut writer = ArchiveWriter::with_ref_prefix(
            output,
            128,
            CompressionSettings::default(),
            b"generation fixture reference",
        )
        .unwrap();
        writer
            .write(&Record::Manifest {
                timestamp_micros: 1,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: name.into(),
                    metadata_snapshot: name.into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 1,
                site_info: SiteInfoRecord {
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
        let id = crate::generation::GenerationId::from_plan_bytes(name.as_bytes());
        crate::title_index::build(&archive, &title, &id).unwrap();
        (archive, title, id.as_str().to_owned())
    }

    #[test]
    fn selector_is_the_only_visibility_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(first_id.as_str())
        );

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        populate_generation(&second_archive, &second_title, &destination, &second_id).unwrap();
        let pending = stage_selector(&second_title, &destination, &second_id).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(first_id.as_str()),
            "generation population and pending selector are invisible"
        );
        let cleanup = vec![first_id.clone()];
        let receipt = PublishReceipt {
            schema: INSTALL_SCHEMA,
            publication_id: publication_id(&second_id, Some(&first_id), &cleanup),
            candidate_generation_id: second_id.clone(),
            selected_before_publish: Some(first_id),
            cleanup_generation_ids: cleanup,
        };
        persist_receipt(&destination, &receipt).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            receipt.selected_before_publish.as_deref(),
            "durable intent is not publication"
        );
        std::fs::rename(pending, selector_path(&destination)).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(second_id.as_str()),
            "one selector rename publishes the complete generation"
        );
    }

    #[test]
    fn recovery_commits_receipted_candidate_but_preserves_unreceipted_candidate() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();

        let (orphan_archive, orphan_title, orphan_id) = generation(temporary.path(), "orphan");
        let orphan =
            populate_generation(&orphan_archive, &orphan_title, &destination, &orphan_id).unwrap();

        let (next_archive, next_title, next_id) = generation(temporary.path(), "next");
        populate_generation(&next_archive, &next_title, &destination, &next_id).unwrap();
        stage_selector(&next_title, &destination, &next_id).unwrap();
        let cleanup = vec![first_id.clone()];
        let receipt = PublishReceipt {
            schema: INSTALL_SCHEMA,
            publication_id: publication_id(&next_id, Some(&first_id), &cleanup),
            candidate_generation_id: next_id.clone(),
            selected_before_publish: Some(first_id),
            cleanup_generation_ids: cleanup,
        };
        persist_receipt(&destination, &receipt).unwrap();

        recover(&destination).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(next_id.as_str())
        );
        assert!(
            orphan.exists(),
            "cleanup must not infer that every unselected generation is disposable"
        );
    }

    #[test]
    fn old_generation_cleanup_waits_for_reader_lease() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();
        let selected = serving_pair(&destination).unwrap().unwrap();
        let titles = crate::title_index::TitleIndex::open(&selected.title).unwrap();
        let reader = crate::archive::IndexedArchiveSet::open(&selected.archive, &titles).unwrap();

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        let outcome = install(second_archive, second_title, &destination).unwrap();
        assert!(outcome.cleanup_pending);
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(second_id.as_str())
        );
        assert!(generation_path(&destination, &first_id).unwrap().exists());

        drop(reader);
        drop(titles);
        assert!(!recover(&destination).unwrap().unwrap().cleanup_pending);
        assert!(!generation_path(&destination, &first_id).unwrap().exists());
    }

    #[test]
    fn selector_open_retries_only_after_selection_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, _) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();
        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        let mut calls = 0;
        let opened = with_serving_pair(&destination, |pair| {
            calls += 1;
            if calls == 1 {
                install(second_archive.clone(), second_title.clone(), &destination).unwrap();
                Err("old generation lost its cleanup race".into())
            } else {
                Ok(pair.archive.clone())
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(opened, generation_path(&destination, &second_id).unwrap());
    }
}
