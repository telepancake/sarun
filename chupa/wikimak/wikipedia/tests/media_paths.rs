#![cfg(feature = "serve")]

use std::path::{Path, PathBuf};

use wikimak_media::MediaStorageWriter;
use wikimak_wikipedia::{
    packed_media_directory_is_valid, resolve_packed_media_path, shared_packed_media_path,
};

fn make_packed_repository(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let data = root.join("media-jpg-0000.data");
    let hashes = root.join("media-jpg-0000.hashes");
    let offsets = root.join("media-jpg-0000.offsets");
    let mut writer = MediaStorageWriter::create(&data, &hashes, &offsets).unwrap();
    writer.append("Example.jpg", b"image").unwrap();
    writer.finish().unwrap();
    root.to_path_buf()
}

#[test]
fn sibling_mirrors_share_one_wikimedia_repository_path() {
    let root = Path::new("/Volumes/Elements/library");
    assert_eq!(
        shared_packed_media_path(&root.join("lvwiki.swdump")),
        root.join("wikimedia.media")
    );
    assert_eq!(
        shared_packed_media_path(&root.join("ruwiki.swdump")),
        root.join("wikimedia.media")
    );
}

#[test]
fn packed_store_validation_rejects_blob_cache_shape() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("lvwiki.media");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("Example.jpg"), b"404").unwrap();
    std::fs::write(cache.join("media-jpg-0000.data"), b"404").unwrap();
    assert!(!packed_media_directory_is_valid(&cache));
}

#[test]
fn explicit_override_wins_and_automatic_selection_prefers_shared_then_legacy() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("lvwiki.swdump");
    let shared = make_packed_repository(&shared_packed_media_path(&archive));
    let legacy = make_packed_repository(&archive.with_extension("media"));
    let explicit = temp.path().join("operator-selected.media");

    assert!(packed_media_directory_is_valid(&shared));
    assert!(packed_media_directory_is_valid(&legacy));
    assert_eq!(
        resolve_packed_media_path(&archive, None),
        Some(shared.clone())
    );
    assert_eq!(
        resolve_packed_media_path(&archive, Some(&explicit)),
        Some(explicit)
    );

    std::fs::remove_dir_all(&shared).unwrap();
    assert_eq!(
        resolve_packed_media_path(&archive, None),
        Some(legacy)
    );
}
