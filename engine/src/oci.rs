// `sarun oci load <ref> [NAME]` — populate a chain of at-rest sarun boxes
// from an OCI image, one box per layer. The bottom (rootfs) box is
// `--no-parent` (no_host_fallback=1) so the stack is closed; each layer
// above chains its `parent_box_id` to the previous layer's box. The image
// config (env / cmd / entrypoint / workingdir / user) is stored as JSON in
// the TOP layer box's sqlar meta.
//
// Box naming: the base box defaults to `C<box_id>`; layer boxes default to
// `L<box_id>`. A user-supplied NAME replaces the base box's name (layers
// keep `L<box_id>`), so `sarun oci load alpine:3.20 alpine` gives a stack
// you can address as `alpine`, `alpine.L<id>`, etc., via the normal dotted
// display path.
//
// Scope (v1): public registries (anonymous), local oci-archive: and
// oci-layout: refs; gzip + zstd + uncompressed layers; PAX/GNU long names;
// AUFS-style whiteouts (`.wh.<name>`); per-entry uid/gid/xattrs/mtime.
// Out of scope (v1): private-registry auth, signatures, zstd:chunked
// streaming, opaque-dir markers (`.wh..wh..opq` — logged + skipped).

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use oci_client::Reference;
use oci_client::client::{Client, ClientConfig, ImageLayer};
use oci_client::manifest::{
    IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
};
use oci_client::secrets::RegistryAuth;

use crate::capture::BoxState;
use crate::paths;

const ZSTD_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
const DOCKER_LAYER_GZIP_MEDIA_TYPE: &str =
    "application/vnd.docker.image.rootfs.diff.tar.gzip";

/// CLI dispatch: `sarun oci <subverb> <args...>`.
pub fn cli_oci(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("usage: sarun oci load <ref> [NAME]");
        return 2;
    };
    match sub {
        "load" => cli_load(&args[1..]),
        "-h" | "--help" => {
            println!("usage: sarun oci load <ref> [NAME]");
            println!("  ref  e.g. alpine:3.20, ghcr.io/foo/bar:tag,");
            println!("       oci-archive:/path/to.tar, oci-layout:/path/to/dir");
            println!("  NAME optional name for the base (rootfs) box");
            0
        }
        other => {
            eprintln!("sarun oci: unknown subcommand '{other}' \
                       (try `sarun oci --help`)");
            2
        }
    }
}

fn cli_load(args: &[String]) -> i32 {
    let Some(reference) = args.first().cloned() else {
        eprintln!("usage: sarun oci load <ref> [NAME]");
        return 2;
    };
    let name = args.get(1).cloned();
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("sarun oci load: {e}");
        return 1;
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all().build() {
        Ok(r) => r,
        Err(e) => { eprintln!("sarun oci load: tokio init: {e}"); return 1; }
    };
    match rt.block_on(load(&reference, name)) {
        Ok(r) => {
            // Spell out both ends of the chain so the user can address the
            // image as either `<base>` (full merged view) or `<base>.L<n>`
            // (a specific layer). Single-layer images have base == top.
            println!("loaded image '{reference}': base box '{}' (id={}) → \
                      top box '{}' (id={}), {} layer(s)",
                     r.base_name, r.base_id, r.top_name, r.top_id, r.n_layers);
            0
        }
        Err(e) => {
            eprintln!("sarun oci load: {e:#}");
            1
        }
    }
}

struct LoadOutcome {
    base_id: i64,
    base_name: String,
    top_id: i64,
    top_name: String,
    n_layers: usize,
}

/// One pulled layer ready to ingest. media_type drives the decompressor.
struct PulledLayer {
    media_type: String,
    bytes: Vec<u8>,
    digest: String,
}

/// Result of fetching: layers in BOTTOM→TOP order plus the image config.
struct PulledImage {
    layers: Vec<PulledLayer>,
    config_json: String,
    reference: String,
}

async fn load(reference: &str, name: Option<String>) -> Result<LoadOutcome> {
    let image = fetch(reference).await?;
    install_chain(image, name)
}

// ── reference resolution ────────────────────────────────────────────────────

async fn fetch(reference: &str) -> Result<PulledImage> {
    if let Some(path) = reference.strip_prefix("oci-archive:") {
        return fetch_oci_archive(Path::new(path));
    }
    if let Some(path) = reference.strip_prefix("oci-layout:") {
        return fetch_oci_layout(Path::new(path));
    }
    fetch_registry(reference).await
}

async fn fetch_registry(reference: &str) -> Result<PulledImage> {
    let r = Reference::from_str(reference)
        .with_context(|| format!("parse reference '{reference}'"))?;
    let client = Client::new(ClientConfig::default());
    // Accepted media types: docker + oci variants in gzip / uncompressed /
    // zstd. Passing them all so a server that has multiple variants doesn't
    // serve us a zstd we wouldn't decode.
    let accepted = vec![
        IMAGE_LAYER_GZIP_MEDIA_TYPE,
        IMAGE_LAYER_MEDIA_TYPE,
        ZSTD_LAYER_MEDIA_TYPE,
        DOCKER_LAYER_GZIP_MEDIA_TYPE,
    ];
    let img = client.pull(&r, &RegistryAuth::Anonymous, accepted).await
        .context("pull image")?;
    let mut layers = Vec::with_capacity(img.layers.len());
    for layer in img.layers {
        let ImageLayer { data, media_type, .. } = layer;
        let bytes = data.to_vec();
        let digest = digest_of(&bytes);
        layers.push(PulledLayer { media_type, bytes, digest });
    }
    let config_json = String::from_utf8(img.config.data.to_vec())
        .context("config is not utf-8")?;
    Ok(PulledImage {
        layers,
        config_json,
        reference: reference.to_string(),
    })
}

fn fetch_oci_layout(path: &Path) -> Result<PulledImage> {
    // OCI image-layout: a directory with index.json + oci-layout + blobs/sha256/.
    // For v1 take the FIRST manifest in index.json; multi-arch indexes pick
    // the first descriptor that points at a manifest (not another index).
    let index_path = path.join("index.json");
    let idx_bytes = std::fs::read(&index_path)
        .with_context(|| format!("read {}", index_path.display()))?;
    let idx: serde_json::Value = serde_json::from_slice(&idx_bytes)
        .context("parse index.json")?;
    let descs = idx.get("manifests").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("index.json has no manifests array"))?;
    // Multi-arch index: prefer a manifest descriptor matching the host's
    // architecture+os (e.g. amd64/linux). Falls back to the first descriptor
    // when none matches — better than failing, in case the index lacks
    // platform tags (single-arch index, or producer didn't annotate).
    let (host_arch, host_os) = host_platform();
    let manifest_digest = descs.iter()
        .find(|d| matches_platform(d, &host_arch, &host_os))
        .or_else(|| descs.first())
        .and_then(|d| d.get("digest").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow!("no manifest digest in index.json"))?;
    let manifest_bytes = read_blob_by_digest(path, manifest_digest)?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .context("parse manifest")?;
    let config_desc = manifest.get("config")
        .ok_or_else(|| anyhow!("manifest has no config"))?;
    let config_digest = config_desc.get("digest").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("config has no digest"))?;
    let config_bytes = read_blob_by_digest(path, config_digest)?;
    let config_json = String::from_utf8(config_bytes)
        .context("config is not utf-8")?;
    let layers_desc = manifest.get("layers").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("manifest has no layers"))?;
    let mut layers = Vec::with_capacity(layers_desc.len());
    for l in layers_desc {
        let digest = l.get("digest").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer has no digest"))?;
        let media_type = l.get("mediaType").and_then(|v| v.as_str())
            .unwrap_or(IMAGE_LAYER_GZIP_MEDIA_TYPE).to_string();
        let bytes = read_blob_by_digest(path, digest)?;
        layers.push(PulledLayer {
            media_type, bytes,
            digest: digest.to_string(),
        });
    }
    Ok(PulledImage {
        layers, config_json,
        reference: format!("oci-layout:{}", path.display()),
    })
}

fn fetch_oci_archive(path: &Path) -> Result<PulledImage> {
    // OCI image-archive: a tar of an oci-layout. Extract to a temp dir, then
    // fetch_oci_layout from there. Cheap given image sizes (an alpine layer
    // is ~3 MB; the archive itself is what the user already had on disk).
    let tmp = tempdir_for_archive()?;
    let file = File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut ar = tar::Archive::new(file);
    ar.unpack(&tmp).context("untar oci-archive")?;
    fetch_oci_layout(&tmp)
}

fn tempdir_for_archive() -> Result<PathBuf> {
    let base = paths::runtime_home().join("oci-archive-tmp");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

/// Host platform tuple in OCI naming. The OS is always "linux" on this
/// engine; the arch maps Rust's target_arch to OCI's name (amd64/arm64/etc.).
fn host_platform() -> (String, String) {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        "powerpc64" => "ppc64le",
        "riscv64" => "riscv64",
        "s390x" => "s390x",
        other => other,
    };
    (arch.to_string(), "linux".to_string())
}

/// Does an index manifest descriptor's platform field match the host?
/// A descriptor without a platform field is treated as a NON-match (so the
/// host-arch entry, if present, is preferred). The first-fallback caller
/// handles the no-match-but-some-entry case.
fn matches_platform(desc: &serde_json::Value, arch: &str, os: &str) -> bool {
    let Some(p) = desc.get("platform") else { return false; };
    p.get("architecture").and_then(|v| v.as_str()) == Some(arch)
        && p.get("os").and_then(|v| v.as_str()) == Some(os)
}

fn read_blob_by_digest(layout: &Path, digest: &str) -> Result<Vec<u8>> {
    // digest is "sha256:<hex>"; the blob lives at blobs/sha256/<hex>.
    let (algo, hex) = digest.split_once(':')
        .ok_or_else(|| anyhow!("malformed digest '{digest}'"))?;
    let p = layout.join("blobs").join(algo).join(hex);
    std::fs::read(&p).with_context(|| format!("read blob {}", p.display()))
}

fn digest_of(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    // We don't need a cryptographic digest — sqlar just records what the
    // registry/manifest told us the digest was. For the registry-pull path we
    // get the bytes after `pull()` strips them from the descriptor; we record
    // a stable but non-crypto fingerprint so users can correlate sqlars to
    // layers without us pulling in a sha2 crate just for this. Real digest
    // verification happens inside oci-client.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.len().hash(&mut h);
    if bytes.len() >= 64 {
        bytes[..32].hash(&mut h);
        bytes[bytes.len() - 32..].hash(&mut h);
    } else {
        bytes.hash(&mut h);
    }
    format!("fp:{:016x}", h.finish())
}

// ── box chain construction ──────────────────────────────────────────────────

/// Mint the next box id. Looks at the highest existing at-rest sqlar id
/// under state_home. (The engine, when running, allocates from
/// max(at_rest, live)+1; here we're at-rest-only so it's just at_rest+1.)
fn next_box_id() -> Result<i64> {
    let sh = paths::state_home();
    let mut max: i64 = 0;
    if let Ok(rd) = std::fs::read_dir(&sh) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("sqlar") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if let Ok(n) = stem.parse::<i64>() {
                    if n > max { max = n; }
                }
            }
        }
    }
    Ok(max + 1)
}

fn install_chain(image: PulledImage, base_name: Option<String>)
    -> Result<LoadOutcome>
{
    if image.layers.is_empty() {
        bail!("image has no layers");
    }
    let mut prev: Option<i64> = None;
    let mut base_id: i64 = 0;
    let mut base_name_out = String::new();
    let mut top_id: i64 = 0;
    let mut top_name = String::new();
    let n = image.layers.len();
    let mut next_id = next_box_id()?;
    let actual_base_name = base_name.clone();
    for (i, layer) in image.layers.into_iter().enumerate() {
        let id = next_id;
        next_id += 1;
        let is_base = i == 0;
        let is_top = i == n - 1;
        let name = if is_base {
            actual_base_name.clone().unwrap_or_else(|| format!("C{id}"))
        } else {
            format!("L{id}")
        };
        let b = BoxState::create(id)
            .with_context(|| format!("create sqlar for box {id}"))?;
        b.set_meta("name", &name);
        b.set_meta("oci_reference", &image.reference);
        b.set_meta("oci_layer_digest", &layer.digest);
        b.set_meta("oci_layer_index", &i.to_string());
        if is_base {
            b.set_no_host_fallback(true);
            b.set_meta("no_host_fallback", "1");
        }
        if let Some(p) = prev {
            b.set_parent(Some(p));
            b.set_meta("parent_box_id", &p.to_string());
        }
        if is_top {
            // Stash the image config on the TOP layer so a future runner can
            // pick up env/cmd/entrypoint/workdir/user. Storing the verbatim
            // JSON keeps us compatible with oci_spec::image::ImageConfiguration
            // (parse on demand) without committing to a schema here.
            b.set_meta("oci_config", &image.config_json);
        }
        ingest_layer(&b, &layer.bytes, &layer.media_type)
            .with_context(|| format!("ingest layer {i} into box {id} ({name})"))?;
        prev = Some(id);
        if is_base {
            base_id = id;
            base_name_out = name.clone();
        }
        top_id = id;
        top_name = name;
    }
    Ok(LoadOutcome {
        base_id, base_name: base_name_out,
        top_id, top_name,
        n_layers: n,
    })
}

// ── per-layer tar entry loop ────────────────────────────────────────────────

fn ingest_layer(b: &BoxState, blob: &[u8], media_type: &str) -> Result<()> {
    let reader = decompressor(blob, media_type)?;
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    // Phony writer row id: at-rest ingest has no process attribution.
    let writer = 0i64;
    // Tar producers are NOT required to emit a Directory entry for every
    // parent dir before the contents inside it (mtree-style implicit dirs).
    // Without an explicit Dir row, FUSE's lookup of `foo/bar` returns ENOENT
    // even though `foo/bar/baz` lives in this layer — so `ls /foo/bar` would
    // fail. We track which dir rels we've already materialized and create a
    // mode-0o755 placeholder for any ancestor missing one as we walk entries.
    let mut ensured_dirs: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let ensure_ancestors = |b: &BoxState, rel: &str,
                            ensured: &mut std::collections::HashSet<String>| {
        let mut acc = String::new();
        for comp in rel.split('/') {
            if acc.is_empty() { acc.push_str(comp); }
            else { acc.push('/'); acc.push_str(comp); }
            // The leaf itself isn't an ancestor — only stop one shy.
            if acc == rel { break; }
            if ensured.insert(acc.clone()) {
                // Don't clobber a non-Dir row a real entry will set later.
                if !matches!(b.entry(&acc),
                    Some(crate::capture::Entry::Dir { .. })) {
                    b.set_dir(&acc, 0o755, 0);
                }
            }
        }
    };
    for entry in ar.entries().context("read tar entries")? {
        let mut e = entry.context("tar entry")?;
        // Use Entry::path / link_name, NOT header().path() — the Entry-level
        // accessors honor PAX `path=` and GNU `L` long-name extensions, so
        // names past the 100/155-byte USTAR limit aren't silently truncated.
        let raw_path = match e.path() {
            Ok(p) => p.into_owned(),
            Err(_) => continue,
        };
        // Safety: refuse absolute paths or any `..` traversal.
        if raw_path.is_absolute()
            || raw_path.components().any(|c| matches!(c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)))
        {
            continue;
        }
        let rel_string = raw_path.to_string_lossy().to_string();
        let rel = rel_string.trim_end_matches('/');
        if rel.is_empty() {
            continue;
        }
        let basename = raw_path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // ── AUFS/OCI whiteout convention ──
        //   .wh..wh..opq  → opaque-dir marker (mask the lower at this dir's
        //                   parent). The PARENT of the marker is the dir
        //                   being opacified; we set its sqlar.opaque=1 so
        //                   overlay's resolve()/scan_dir honor it.
        //   .wh.<NAME>    → tombstone for sibling NAME at parent(path).
        if basename == ".wh..wh..opq" {
            let parent_rel = raw_path.parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            // An opaque marker at the layer root (.wh..wh..opq directly at
            // the tar's top) sets parent_rel to "" — the BOX ROOT, which IS
            // a valid opaque target. set_opaque writes a sqlar row with
            // name="" and the overlay's has_opaque_ancestor walks down to it.
            b.set_opaque(&parent_rel, writer);
            continue;
        }
        if let Some(orig) = basename.strip_prefix(".wh.") {
            let parent_rel = raw_path.parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let target = if parent_rel.is_empty() { orig.to_string() }
                         else { format!("{parent_rel}/{orig}") };
            b.set_whiteout(&target, writer);
            continue;
        }
        // Snapshot header fields into locals so the &header borrow ends
        // BEFORE we touch &mut e (pax_extensions / body read).
        let (mode, mtime_ns, uid, gid, ent_type, dev_major, dev_minor) = {
            let h = e.header();
            (
                (h.mode().unwrap_or(0o644)) & 0o7777,
                (h.mtime().unwrap_or(0) as i64).saturating_mul(1_000_000_000),
                h.uid().unwrap_or(0) as u32,
                h.gid().unwrap_or(0) as u32,
                h.entry_type(),
                h.device_major().ok().flatten().unwrap_or(0),
                h.device_minor().ok().flatten().unwrap_or(0),
            )
        };
        // PAX extensions BEFORE the body read consumes them.
        let xattrs = read_pax_xattrs(&mut e);
        // Materialise any missing ancestor dirs before we touch the leaf.
        ensure_ancestors(b, rel, &mut ensured_dirs);
        match ent_type {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let full_mode = 0o100000 | mode;
                let rowid = b.ensure_file_row(rel, full_mode, writer);
                let bp = crate::capture::blob_path(b.id, rowid);
                if let Some(p) = bp.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let mut out = File::create(&bp)
                    .with_context(|| format!("create blob {}", bp.display()))?;
                let sz = std::io::copy(&mut e, &mut out)
                    .with_context(|| format!("write blob {}", bp.display()))?;
                drop(out);
                let _ = std::fs::set_permissions(&bp,
                    std::fs::Permissions::from_mode(mode));
                b.finalize_file(rel, sz as i64, mtime_ns, writer);
            }
            tar::EntryType::Directory => {
                b.set_dir(rel, mode, writer);
            }
            tar::EntryType::Symlink => {
                let tgt = e.link_name()
                    .ok().flatten()
                    .map(|p| p.into_owned())
                    .unwrap_or_default();
                b.set_symlink(rel, &tgt, writer);
            }
            tar::EntryType::Link => {
                // Hardlink: tar carries a link_name pointing at an earlier
                // entry in this SAME archive. Approximate as a fresh row with
                // the source bytes copied in — the existing FUSE link() does
                // the same approximation (nlink stays 1, but the second name
                // works for the box's processes; matches the Python engine's
                // _link_overlay). Mode comes from the SOURCE: tar Link entries
                // commonly have mode=0 in the header because the inode metadata
                // is supposed to be shared with the target.
                if let Some(src_path) = e.link_name().ok().flatten() {
                    let src_rel = src_path.to_string_lossy()
                        .trim_end_matches('/')
                        .to_string();
                    if let Some((src_rowid, src_mode)) = lookup_file(b, &src_rel) {
                        let src_blob = crate::capture::blob_path(b.id, src_rowid);
                        let new_rowid = b.ensure_file_row(rel, src_mode, writer);
                        let new_blob = crate::capture::blob_path(b.id, new_rowid);
                        if let Some(p) = new_blob.parent() {
                            let _ = std::fs::create_dir_all(p);
                        }
                        std::fs::copy(&src_blob, &new_blob)
                            .with_context(|| format!("hardlink {rel} ← {src_rel}"))?;
                        let sz = std::fs::metadata(&new_blob)?.len() as i64;
                        b.finalize_file(rel, sz, mtime_ns, writer);
                    }
                    // If the source isn't recorded yet (entries out of order),
                    // we skip — uncommon enough that v1 lets it slide. A
                    // second pass would catch it; not worth the complexity here.
                }
            }
            tar::EntryType::Fifo | tar::EntryType::Char | tar::EntryType::Block => {
                let kind = match ent_type {
                    tar::EntryType::Fifo => libc::S_IFIFO,
                    tar::EntryType::Char => libc::S_IFCHR,
                    tar::EntryType::Block => libc::S_IFBLK,
                    _ => unreachable!(),
                };
                let rdev = ((dev_major as u64) << 8) | (dev_minor as u64);
                b.set_special(rel, kind | mode, rdev, writer);
            }
            // tar internal types (XHeader, XGlobalHeader, GNULongName,
            // GNULongLink, GNUSparse, GNULongLink, etc.) are consumed by the
            // tar crate itself — Entry::path()/link_name() already honor them.
            _ => continue,
        }
        if uid != 0 || gid != 0 {
            b.set_owner(rel, uid, gid);
        }
        for (key, val) in xattrs {
            b.set_xattr(rel, &key, &val);
        }
    }
    Ok(())
}

/// PAX `SCHILY.xattr.*` entries — important for distroless / SELinux-labeled
/// images and easy to silently drop. Returns (key without prefix, raw bytes).
fn read_pax_xattrs<R: Read>(e: &mut tar::Entry<'_, R>) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let Ok(Some(exts)) = e.pax_extensions() else { return out; };
    for ext in exts.flatten() {
        let Ok(key) = ext.key() else { continue; };
        if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
            out.push((name.to_string(), ext.value_bytes().to_vec()));
        }
    }
    out
}

/// (rowid, full mode) of a regular file already ingested into `b`. Used by
/// the hardlink path — sources are always earlier entries in the same tar,
/// and tar Link entries don't carry mode so we copy it from the source.
fn lookup_file(b: &BoxState, rel: &str) -> Option<(i64, u32)> {
    match b.entry(rel) {
        Some(crate::capture::Entry::File { rowid, mode }) => Some((rowid, mode)),
        _ => None,
    }
}

// ── decompression ───────────────────────────────────────────────────────────

fn decompressor<'a>(blob: &'a [u8], media_type: &str)
    -> Result<Box<dyn Read + 'a>>
{
    // Dispatch on media type first, with a magic-byte safety net for the
    // small but real case of a manifest mis-labelling the layer (some early
    // BuildKit releases shipped zstd bytes under the gzip media type).
    let actual = sniff(blob).unwrap_or_else(|| classify(media_type));
    match actual {
        Comp::Gzip => Ok(Box::new(flate2::read::GzDecoder::new(blob))),
        Comp::Zstd => {
            let d = ruzstd::StreamingDecoder::new(blob)
                .map_err(|e| anyhow!("zstd init: {e}"))?;
            Ok(Box::new(d))
        }
        Comp::None => Ok(Box::new(blob)),
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum Comp { Gzip, Zstd, None }

fn classify(media_type: &str) -> Comp {
    if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") {
        Comp::Gzip
    } else if media_type.ends_with("+zstd") || media_type.ends_with("+zstd+chunked") {
        // zstd:chunked is a containers-storage extension; the blob itself is
        // still a valid zstd stream, so plain zstd decoding is correct (just
        // doesn't take advantage of the chunked-TOC fast path — v1 is fine).
        Comp::Zstd
    } else {
        Comp::None
    }
}

fn sniff(blob: &[u8]) -> Option<Comp> {
    if blob.len() >= 4 {
        if blob[0] == 0x1f && blob[1] == 0x8b {
            return Some(Comp::Gzip);
        }
        if &blob[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
            return Some(Comp::Zstd);
        }
    }
    None
}
