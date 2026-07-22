//! Box-owned debugger-resource selection for Sarun QEMU registrations.
//!
//! The resolver deliberately has no host-path, environment, or command-line
//! input.  Its catalog is the narrow interface the engine's Overlay and
//! declared-service registry need to implement.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};

pub(crate) const DEBUG_SERVICE_NAME: &str = "viros-debug";
pub const KERNEL_BUNDLE_FORMAT: &str = "viros-kernel-bundle-v1";
pub const IMAGE_BUNDLE_FORMAT: &str = "viros-image-bundle-v1";

pub type BoxId = i64;

fn architecture_name(architecture: crate::generated_wire::QemuArchitecture) -> &'static str {
    match architecture {
        crate::generated_wire::QemuArchitecture::Aarch64 => "aarch64",
        crate::generated_wire::QemuArchitecture::X8664 => "x86_64",
        crate::generated_wire::QemuArchitecture::Arm => "arm",
        crate::generated_wire::QemuArchitecture::Mmips => "mmips",
    }
}

/// The only engine-facing operations used during selection.
///
/// `regular_captured_paths` is an own-layer snapshot, not a merged-root walk.
/// It must omit directories, symlinks, whiteouts, and holes. `provider_chain`
/// must use Overlay lookup order: current box, its box RO attachments, parent,
/// that parent's box RO attachments, and so on. External attachments are not
/// returned because they have no Sarun box identity.
trait DebugResourceCatalog {
    fn provider_chain(&self, consumer_box: BoxId) -> Result<Vec<BoxId>, ResolveError>;
    fn regular_captured_paths(&self, provider_box: BoxId) -> Result<Vec<String>, ResolveError>;
    fn box_read_file(
        &self,
        provider_box: BoxId,
        relative_path: &str,
    ) -> Result<Vec<u8>, ResolveError>;
    fn declared_service_boxes(&self, service_name: &str) -> Result<Vec<BoxId>, ResolveError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactDescriptor {
    /// UTF-8, provider-root-relative path suitable for Overlay::box_read_file.
    pub path: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugEntrypoints {
    pub callgate: ArtifactDescriptor,
    pub gdb_loader: ArtifactDescriptor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugKernelArtifacts {
    pub vmlinux: ArtifactDescriptor,
    pub boot_image: ArtifactDescriptor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedKernelBundle {
    pub provider_box: BoxId,
    pub manifest_path: String,
    pub architecture: crate::generated_wire::QemuArchitecture,
    pub entrypoints: DebugEntrypoints,
    pub kernel: DebugKernelArtifacts,
    pub artifacts: Vec<ArtifactDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDeclaredService {
    pub name: &'static str,
    pub provider_box: BoxId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDebugResources {
    pub service: ResolvedDeclaredService,
    pub bundle: ResolvedKernelBundle,
    /// Present when the selected named build box publishes a complete guest
    /// image. Absence retains the ordinary Sarun appliance rootfs.
    pub image: Option<ResolvedImageBundle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedUserspaceExecutable {
    pub guest_path: String,
    pub build_id: String,
    pub runtime_sha256: [u8; 32],
    pub runtime_size: u64,
    pub debug_elf: ArtifactDescriptor,
    pub elf_class: u8,
    pub elf_machine: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedImageBundle {
    pub provider_box: BoxId,
    pub manifest_path: String,
    pub architecture: crate::generated_wire::QemuArchitecture,
    pub profile: String,
    pub init: String,
    pub initramfs: ArtifactDescriptor,
    pub kernel_manifest: ArtifactDescriptor,
    pub executables: Vec<ResolvedUserspaceExecutable>,
    pub artifacts: Vec<ArtifactDescriptor>,
}

struct EngineCatalog<'a> {
    overlay: &'a crate::overlay::Overlay,
}

impl DebugResourceCatalog for EngineCatalog<'_> {
    fn provider_chain(&self, consumer_box: BoxId) -> Result<Vec<BoxId>, ResolveError> {
        self.overlay
            .debug_provider_chain(consumer_box)
            .map_err(ResolveError::Catalog)
    }

    fn regular_captured_paths(&self, provider_box: BoxId) -> Result<Vec<String>, ResolveError> {
        self.overlay
            .box_of(provider_box)
            .map(|provider| provider.regular_captured_paths_snapshot())
            .ok_or_else(|| {
                ResolveError::Catalog(format!("debug provider box {provider_box} is unavailable"))
            })
    }

    fn box_read_file(
        &self,
        provider_box: BoxId,
        relative_path: &str,
    ) -> Result<Vec<u8>, ResolveError> {
        self.overlay
            .box_read_file(provider_box, relative_path)
            .map_err(|error| {
                ResolveError::Catalog(format!("read box {provider_box}:{relative_path}: {error}"))
            })
    }

    fn declared_service_boxes(&self, service_name: &str) -> Result<Vec<BoxId>, ResolveError> {
        Ok(crate::discover::discover()
            .into_iter()
            .filter_map(|(box_id, provider)| {
                (provider.meta.get("svc_provide").map(String::as_str) == Some(service_name))
                    .then_some(box_id)
            })
            .collect())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveError {
    Catalog(String),
    NoService {
        name: &'static str,
    },
    AmbiguousService {
        name: &'static str,
        boxes: Vec<BoxId>,
    },
    NoBundle {
        architecture: &'static str,
    },
    AmbiguousBundle {
        architecture: &'static str,
        candidates: Vec<(BoxId, String)>,
    },
    AmbiguousImage {
        architecture: &'static str,
        candidates: Vec<(BoxId, String)>,
    },
    InvalidBundle {
        provider_box: BoxId,
        manifest_path: String,
        reason: String,
    },
    InvalidImage {
        provider_box: BoxId,
        manifest_path: String,
        reason: String,
    },
}

impl ResolveError {
    fn invalid(provider_box: BoxId, manifest_path: &str, reason: impl Into<String>) -> Self {
        Self::InvalidBundle {
            provider_box,
            manifest_path: manifest_path.to_owned(),
            reason: reason.into(),
        }
    }

    fn invalid_image(provider_box: BoxId, manifest_path: &str, reason: impl Into<String>) -> Self {
        Self::InvalidImage {
            provider_box,
            manifest_path: manifest_path.to_owned(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(message) => write!(output, "debug resource catalog: {message}"),
            Self::NoService { name } => write!(output, "no box declares service {name:?}"),
            Self::AmbiguousService { name, boxes } => {
                write!(output, "service {name:?} is declared by boxes {boxes:?}")
            }
            Self::NoBundle { architecture } => {
                write!(
                    output,
                    "no {KERNEL_BUNDLE_FORMAT} bundle for {architecture}"
                )
            }
            Self::AmbiguousBundle {
                architecture,
                candidates,
            } => {
                write!(
                    output,
                    "multiple bundles for {architecture}: {candidates:?}"
                )
            }
            Self::AmbiguousImage {
                architecture,
                candidates,
            } => write!(output, "multiple images for {architecture}: {candidates:?}"),
            Self::InvalidBundle {
                provider_box,
                manifest_path,
                reason,
            } => {
                write!(
                    output,
                    "invalid bundle box {provider_box}:{manifest_path}: {reason}"
                )
            }
            Self::InvalidImage {
                provider_box,
                manifest_path,
                reason,
            } => write!(
                output,
                "invalid image box {provider_box}:{manifest_path}: {reason}"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

#[derive(Deserialize)]
struct RawBundle {
    format: String,
    architecture: String,
    kernel: RawKernel,
    entrypoints: RawEntrypoints,
    artifacts: Vec<RawArtifact>,
}

#[derive(Deserialize)]
struct RawKernel {
    vmlinux: String,
    vmlinux_sha256: String,
    boot_image: String,
    boot_image_sha256: String,
}

#[derive(Deserialize)]
struct RawEntrypoints {
    callgate: String,
    gdb_loader: String,
}

#[derive(Deserialize)]
struct RawArtifact {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct RawImageBundle {
    format: String,
    architecture: String,
    boot: RawImageBoot,
    userspace: RawUserspace,
    artifacts: Vec<RawArtifact>,
}

#[derive(Deserialize)]
struct RawImageBoot {
    profile: String,
    kernel_bundle: String,
    initramfs: String,
    init: String,
}

#[derive(Deserialize)]
struct RawUserspace {
    executables: Vec<RawExecutable>,
}

#[derive(Deserialize)]
struct RawExecutable {
    guest_path: String,
    build_id: String,
    runtime_sha256: String,
    runtime_size: u64,
    debug_elf: String,
    debug_sha256: String,
    debug_size: u64,
    elf_class: u8,
    elf_machine: u16,
    source_view: String,
}

/// Resolve the unique declared viros service and unique architecture-matching
/// kernel bundle visible through a QEMU box's captured-layer chain.
pub(crate) fn resolve_debug_resources(
    overlay: &crate::overlay::Overlay,
    consumer_box: BoxId,
    architecture: crate::generated_wire::QemuArchitecture,
) -> Result<ResolvedDebugResources, ResolveError> {
    resolve_from_catalog(&EngineCatalog { overlay }, consumer_box, architecture)
}

/// Validate debugger resources produced at two exact paths in one provider
/// child box.  This is the selected-image counterpart of
/// [`resolve_debug_resources`]: the provider has already returned the paths,
/// so this entry point never searches a provider chain or interprets a file
/// name as a declaration.
pub(crate) fn resolve_exact_debug_resources(
    overlay: &crate::overlay::Overlay,
    provider_box: BoxId,
    kernel_manifest_path: &str,
    image_manifest_path: &str,
    architecture: crate::generated_wire::QemuArchitecture,
) -> Result<ResolvedDebugResources, ResolveError> {
    resolve_exact_from_catalog(
        &EngineCatalog { overlay },
        provider_box,
        kernel_manifest_path,
        image_manifest_path,
        architecture,
    )
}

/// Re-read one already-selected artifact through its owning box and verify
/// that its immutable descriptor still matches before descriptor handoff.
pub(crate) fn read_resolved_artifact(
    overlay: &crate::overlay::Overlay,
    provider_box: BoxId,
    artifact: &ArtifactDescriptor,
) -> Result<Vec<u8>, ResolveError> {
    let bytes = overlay
        .box_read_file(provider_box, &artifact.path)
        .map_err(|error| {
            ResolveError::Catalog(format!(
                "read box {provider_box}:{}: {error}",
                artifact.path
            ))
        })?;
    if bytes.len() as u64 != artifact.size {
        return Err(ResolveError::Catalog(format!(
            "box {provider_box}:{} changed size after resolution",
            artifact.path
        )));
    }
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != artifact.sha256 {
        return Err(ResolveError::Catalog(format!(
            "box {provider_box}:{} changed content after resolution",
            artifact.path
        )));
    }
    Ok(bytes)
}

fn resolve_from_catalog(
    catalog: &impl DebugResourceCatalog,
    consumer_box: BoxId,
    architecture: crate::generated_wire::QemuArchitecture,
) -> Result<ResolvedDebugResources, ResolveError> {
    let service_box = unique_service_box(catalog)?;
    let chain = catalog.provider_chain(consumer_box)?;
    let mut seen_boxes = BTreeSet::new();
    let mut candidates = Vec::new();
    let mut image_candidates = Vec::new();

    for provider_box in chain {
        if !seen_boxes.insert(provider_box) {
            continue;
        }
        let mut paths = catalog.regular_captured_paths(provider_box)?;
        paths.sort();
        paths.dedup();
        for manifest_path in paths.into_iter().filter(|path| is_bundle_name(path)) {
            let bytes = catalog.box_read_file(provider_box, &manifest_path)?;
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                // A file named bundle.json is not ours unless it identifies
                // the viros format. Arbitrary project manifests stay inert.
                continue;
            };
            if value.get("format").and_then(serde_json::Value::as_str) != Some(KERNEL_BUNDLE_FORMAT)
            {
                continue;
            }
            let claimed_architecture = value
                .get("architecture")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ResolveError::invalid(
                        provider_box,
                        &manifest_path,
                        "architecture must be a string",
                    )
                })?;
            if claimed_architecture != architecture_name(architecture) {
                continue;
            }
            candidates.push(validate_bundle(
                catalog,
                provider_box,
                &manifest_path,
                architecture,
                &bytes,
            )?);
        }
        let mut image_paths = catalog.regular_captured_paths(provider_box)?;
        image_paths.sort();
        image_paths.dedup();
        for manifest_path in image_paths.into_iter().filter(|path| is_image_name(path)) {
            let bytes = catalog.box_read_file(provider_box, &manifest_path)?;
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if value.get("format").and_then(serde_json::Value::as_str) != Some(IMAGE_BUNDLE_FORMAT)
            {
                continue;
            }
            if value
                .get("architecture")
                .and_then(serde_json::Value::as_str)
                != Some(architecture_name(architecture))
            {
                continue;
            }
            image_candidates.push(validate_image_bundle(
                catalog,
                provider_box,
                &manifest_path,
                architecture,
                &bytes,
            )?);
        }
    }

    let bundle = match candidates.len() {
        0 => {
            return Err(ResolveError::NoBundle {
                architecture: architecture_name(architecture),
            });
        }
        1 => candidates.pop().expect("one candidate"),
        _ => {
            return Err(ResolveError::AmbiguousBundle {
                architecture: architecture_name(architecture),
                candidates: candidates
                    .iter()
                    .map(|candidate| (candidate.provider_box, candidate.manifest_path.clone()))
                    .collect(),
            });
        }
    };
    let image = match image_candidates.len() {
        0 => None,
        1 => image_candidates.pop(),
        _ => {
            return Err(ResolveError::AmbiguousImage {
                architecture: architecture_name(architecture),
                candidates: image_candidates
                    .iter()
                    .map(|candidate| (candidate.provider_box, candidate.manifest_path.clone()))
                    .collect(),
            });
        }
    };
    if let Some(image) = &image {
        if image.provider_box != bundle.provider_box
            || image.kernel_manifest.path != bundle.manifest_path
        {
            return Err(ResolveError::invalid_image(
                image.provider_box,
                &image.manifest_path,
                "boot.kernel_bundle does not identify the selected kernel bundle",
            ));
        }
    }
    Ok(ResolvedDebugResources {
        service: ResolvedDeclaredService {
            name: DEBUG_SERVICE_NAME,
            provider_box: service_box,
        },
        bundle,
        image,
    })
}

fn resolve_exact_from_catalog(
    catalog: &impl DebugResourceCatalog,
    provider_box: BoxId,
    kernel_manifest_path: &str,
    image_manifest_path: &str,
    architecture: crate::generated_wire::QemuArchitecture,
) -> Result<ResolvedDebugResources, ResolveError> {
    if !safe_relative(kernel_manifest_path) || !safe_relative(image_manifest_path) {
        return Err(ResolveError::Catalog(
            "provider returned an unsafe debugger manifest path".into(),
        ));
    }
    let own_paths: BTreeSet<_> = catalog
        .regular_captured_paths(provider_box)?
        .into_iter()
        .collect();
    for (kind, path) in [
        ("kernel", kernel_manifest_path),
        ("image", image_manifest_path),
    ] {
        if !own_paths.contains(path) {
            return Err(ResolveError::Catalog(format!(
                "provider box {provider_box} did not capture its returned {kind} manifest {path:?}",
            )));
        }
    }

    let kernel_bytes = catalog.box_read_file(provider_box, kernel_manifest_path)?;
    let bundle = validate_bundle(
        catalog,
        provider_box,
        kernel_manifest_path,
        architecture,
        &kernel_bytes,
    )?;
    let image_bytes = catalog.box_read_file(provider_box, image_manifest_path)?;
    let image = validate_image_bundle(
        catalog,
        provider_box,
        image_manifest_path,
        architecture,
        &image_bytes,
    )?;
    if image.kernel_manifest.path != bundle.manifest_path {
        return Err(ResolveError::invalid_image(
            provider_box,
            image_manifest_path,
            "boot.kernel_bundle does not identify the returned kernel manifest",
        ));
    }

    Ok(ResolvedDebugResources {
        service: ResolvedDeclaredService {
            name: DEBUG_SERVICE_NAME,
            provider_box: unique_service_box(catalog)?,
        },
        bundle,
        image: Some(image),
    })
}

fn unique_service_box(catalog: &impl DebugResourceCatalog) -> Result<BoxId, ResolveError> {
    let mut boxes = catalog.declared_service_boxes(DEBUG_SERVICE_NAME)?;
    boxes.sort_unstable();
    boxes.dedup();
    match boxes.as_slice() {
        [] => Err(ResolveError::NoService {
            name: DEBUG_SERVICE_NAME,
        }),
        [provider] => Ok(*provider),
        _ => Err(ResolveError::AmbiguousService {
            name: DEBUG_SERVICE_NAME,
            boxes,
        }),
    }
}

fn is_bundle_name(path: &str) -> bool {
    safe_relative(path)
        && Path::new(path).file_name().and_then(|name| name.to_str()) == Some("bundle.json")
}

fn is_image_name(path: &str) -> bool {
    safe_relative(path)
        && Path::new(path).file_name().and_then(|name| name.to_str()) == Some("image.json")
}

fn safe_guest_absolute(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains('\0')
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn safe_kernel_init(path: &str) -> bool {
    safe_guest_absolute(path)
        && path
            .bytes()
            .all(|byte| !byte.is_ascii_whitespace() && !byte.is_ascii_control())
}

fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn rooted_artifact_path(manifest_path: &str, artifact_path: &str) -> Option<String> {
    if !safe_relative(manifest_path) || !safe_relative(artifact_path) {
        return None;
    }
    let parent = Path::new(manifest_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let joined = parent.join(artifact_path);
    joined
        .to_str()
        .filter(|path| safe_relative(path))
        .map(str::to_owned)
}

fn parse_hash(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut result = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).ok()?;
        result[index] = u8::from_str_radix(text, 16).ok()?;
    }
    Some(result)
}

fn validate_bundle(
    catalog: &impl DebugResourceCatalog,
    provider_box: BoxId,
    manifest_path: &str,
    architecture: crate::generated_wire::QemuArchitecture,
    bytes: &[u8],
) -> Result<ResolvedKernelBundle, ResolveError> {
    let raw: RawBundle = serde_json::from_slice(bytes).map_err(|error| {
        ResolveError::invalid(
            provider_box,
            manifest_path,
            format!("manifest shape: {error}"),
        )
    })?;
    if raw.format != KERNEL_BUNDLE_FORMAT || raw.architecture != architecture_name(architecture) {
        return Err(ResolveError::invalid(
            provider_box,
            manifest_path,
            "format or architecture changed during validation",
        ));
    }

    let mut artifacts = Vec::with_capacity(raw.artifacts.len());
    let mut by_manifest_path = BTreeMap::new();
    for artifact in raw.artifacts {
        let rooted = rooted_artifact_path(manifest_path, &artifact.path).ok_or_else(|| {
            ResolveError::invalid(
                provider_box,
                manifest_path,
                format!("unsafe artifact path {:?}", artifact.path),
            )
        })?;
        let sha256 = parse_hash(&artifact.sha256).ok_or_else(|| {
            ResolveError::invalid(
                provider_box,
                manifest_path,
                format!("artifact {:?} has an invalid SHA-256", artifact.path),
            )
        })?;
        if by_manifest_path.contains_key(&artifact.path) {
            return Err(ResolveError::invalid(
                provider_box,
                manifest_path,
                format!("duplicate artifact path {:?}", artifact.path),
            ));
        }
        let content = catalog
            .box_read_file(provider_box, &rooted)
            .map_err(|error| {
                ResolveError::invalid(
                    provider_box,
                    manifest_path,
                    format!("cannot read artifact {:?}: {error}", artifact.path),
                )
            })?;
        if content.len() as u64 != artifact.size {
            return Err(ResolveError::invalid(
                provider_box,
                manifest_path,
                format!(
                    "artifact {:?} size is {}, expected {}",
                    artifact.path,
                    content.len(),
                    artifact.size,
                ),
            ));
        }
        let actual: [u8; 32] = Sha256::digest(&content).into();
        if actual != sha256 {
            return Err(ResolveError::invalid(
                provider_box,
                manifest_path,
                format!("artifact {:?} SHA-256 mismatch", artifact.path),
            ));
        }
        let descriptor = ArtifactDescriptor {
            path: rooted,
            size: artifact.size,
            sha256,
        };
        by_manifest_path.insert(artifact.path, descriptor.clone());
        artifacts.push(descriptor);
    }

    let descriptor = |kind: &str, relative: &str| {
        if !safe_relative(relative) {
            return Err(ResolveError::invalid(
                provider_box,
                manifest_path,
                format!("unsafe {kind} path {relative:?}"),
            ));
        }
        by_manifest_path.get(relative).cloned().ok_or_else(|| {
            ResolveError::invalid(
                provider_box,
                manifest_path,
                format!("{kind} path {relative:?} is absent from artifacts"),
            )
        })
    };

    let vmlinux = descriptor("kernel.vmlinux", &raw.kernel.vmlinux)?;
    let boot_image = descriptor("kernel.boot_image", &raw.kernel.boot_image)?;
    let expected_vmlinux = parse_hash(&raw.kernel.vmlinux_sha256).ok_or_else(|| {
        ResolveError::invalid(provider_box, manifest_path, "invalid kernel.vmlinux_sha256")
    })?;
    let expected_boot = parse_hash(&raw.kernel.boot_image_sha256).ok_or_else(|| {
        ResolveError::invalid(
            provider_box,
            manifest_path,
            "invalid kernel.boot_image_sha256",
        )
    })?;
    if vmlinux.sha256 != expected_vmlinux || boot_image.sha256 != expected_boot {
        return Err(ResolveError::invalid(
            provider_box,
            manifest_path,
            "kernel hashes disagree with their artifact rows",
        ));
    }

    Ok(ResolvedKernelBundle {
        provider_box,
        manifest_path: manifest_path.to_owned(),
        architecture,
        entrypoints: DebugEntrypoints {
            callgate: descriptor("entrypoints.callgate", &raw.entrypoints.callgate)?,
            gdb_loader: descriptor("entrypoints.gdb_loader", &raw.entrypoints.gdb_loader)?,
        },
        kernel: DebugKernelArtifacts {
            vmlinux,
            boot_image,
        },
        artifacts,
    })
}

fn image_profile(architecture: crate::generated_wire::QemuArchitecture) -> &'static str {
    match architecture {
        crate::generated_wire::QemuArchitecture::Aarch64 => "virt-initramfs-aarch64-v1",
        crate::generated_wire::QemuArchitecture::X8664 => "microvm-initramfs-x86_64-v1",
        crate::generated_wire::QemuArchitecture::Arm => "virt-initramfs-arm-v1",
        crate::generated_wire::QemuArchitecture::Mmips => "malta-initramfs-mipsel-v1",
    }
}

pub(crate) fn wire_image_profile(
    architecture: crate::generated_wire::QemuArchitecture,
) -> crate::generated_wire::DebugImageProfile {
    match architecture {
        crate::generated_wire::QemuArchitecture::Aarch64 => {
            crate::generated_wire::DebugImageProfile::VirtInitramfsAarch64V1
        }
        crate::generated_wire::QemuArchitecture::X8664 => {
            crate::generated_wire::DebugImageProfile::MicrovmInitramfsX8664V1
        }
        crate::generated_wire::QemuArchitecture::Arm => {
            crate::generated_wire::DebugImageProfile::VirtInitramfsArmV1
        }
        crate::generated_wire::QemuArchitecture::Mmips => {
            crate::generated_wire::DebugImageProfile::MaltaInitramfsMipselV1
        }
    }
}

pub(crate) fn wire_image_catalog(
    image: &ResolvedImageBundle,
) -> Result<crate::generated_wire::DebugImageCatalog, String> {
    use crate::generated_wire::{DebugExecutable, DebugImageCatalog, DebugSourceView};
    use crate::wire::{BoundedBytes, BoundedVec, FixedBytes};

    let path = |kind: &str, value: &str| {
        BoundedBytes::new(value.as_bytes().to_vec())
            .map_err(|error| format!("{kind} exceeds protocol bound: {error:?}"))
    };
    let executables = image
        .executables
        .iter()
        .map(|executable| {
            Ok(DebugExecutable {
                guest_path: path("guest executable path", &executable.guest_path)?,
                build_id: BoundedBytes::new(executable.build_id.as_bytes().to_vec())
                    .map_err(|error| format!("build ID exceeds protocol bound: {error:?}"))?,
                runtime_sha256: FixedBytes(executable.runtime_sha256),
                runtime_size: executable.runtime_size,
                debug_elf: path("debug ELF path", &executable.debug_elf.path)?,
                debug_sha256: FixedBytes(executable.debug_elf.sha256),
                debug_size: executable.debug_elf.size,
                elf_class: u16::from(executable.elf_class),
                elf_machine: executable.elf_machine,
                source_view: DebugSourceView::ProviderRoot,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(DebugImageCatalog {
        manifest: path("image manifest path", &image.manifest_path)?,
        profile: wire_image_profile(image.architecture),
        init: path("image init path", &image.init)?,
        executables: BoundedVec::new(executables)
            .map_err(|error| format!("executable catalog exceeds protocol bound: {error:?}"))?,
    })
}

fn image_elf_identity(architecture: crate::generated_wire::QemuArchitecture) -> (u8, u16) {
    match architecture {
        crate::generated_wire::QemuArchitecture::Aarch64 => (64, 183),
        crate::generated_wire::QemuArchitecture::X8664 => (64, 62),
        crate::generated_wire::QemuArchitecture::Arm => (32, 40),
        crate::generated_wire::QemuArchitecture::Mmips => (32, 8),
    }
}

fn valid_build_id(value: &str) -> bool {
    (8..=128).contains(&value.len())
        && value.len() % 2 == 0
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_image_bundle(
    catalog: &impl DebugResourceCatalog,
    provider_box: BoxId,
    manifest_path: &str,
    architecture: crate::generated_wire::QemuArchitecture,
    bytes: &[u8],
) -> Result<ResolvedImageBundle, ResolveError> {
    let raw: RawImageBundle = serde_json::from_slice(bytes).map_err(|error| {
        ResolveError::invalid_image(
            provider_box,
            manifest_path,
            format!("manifest shape: {error}"),
        )
    })?;
    if raw.format != IMAGE_BUNDLE_FORMAT || raw.architecture != architecture_name(architecture) {
        return Err(ResolveError::invalid_image(
            provider_box,
            manifest_path,
            "format or architecture changed during validation",
        ));
    }
    if raw.boot.profile != image_profile(architecture) {
        return Err(ResolveError::invalid_image(
            provider_box,
            manifest_path,
            format!("unsupported boot profile {:?}", raw.boot.profile),
        ));
    }
    if !safe_kernel_init(&raw.boot.init) {
        return Err(ResolveError::invalid_image(
            provider_box,
            manifest_path,
            format!("unsafe boot init path {:?}", raw.boot.init),
        ));
    }

    let mut artifacts = Vec::with_capacity(raw.artifacts.len());
    let mut by_manifest_path = BTreeMap::new();
    for artifact in raw.artifacts {
        let rooted = rooted_artifact_path(manifest_path, &artifact.path).ok_or_else(|| {
            ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("unsafe artifact path {:?}", artifact.path),
            )
        })?;
        let sha256 = parse_hash(&artifact.sha256).ok_or_else(|| {
            ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("artifact {:?} has an invalid SHA-256", artifact.path),
            )
        })?;
        if by_manifest_path.contains_key(&artifact.path) {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("duplicate artifact path {:?}", artifact.path),
            ));
        }
        let content = catalog
            .box_read_file(provider_box, &rooted)
            .map_err(|error| {
                ResolveError::invalid_image(
                    provider_box,
                    manifest_path,
                    format!("cannot read artifact {:?}: {error}", artifact.path),
                )
            })?;
        if content.len() as u64 != artifact.size
            || <[u8; 32]>::from(Sha256::digest(&content)) != sha256
        {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("artifact {:?} size or SHA-256 mismatch", artifact.path),
            ));
        }
        let descriptor = ArtifactDescriptor {
            path: rooted,
            size: artifact.size,
            sha256,
        };
        by_manifest_path.insert(artifact.path, descriptor.clone());
        artifacts.push(descriptor);
    }
    let descriptor = |kind: &str, relative: &str| {
        if !safe_relative(relative) {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("unsafe {kind} path {relative:?}"),
            ));
        }
        by_manifest_path.get(relative).cloned().ok_or_else(|| {
            ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("{kind} path {relative:?} is absent from artifacts"),
            )
        })
    };
    let kernel_manifest = descriptor("boot.kernel_bundle", &raw.boot.kernel_bundle)?;
    let initramfs = descriptor("boot.initramfs", &raw.boot.initramfs)?;
    let (expected_class, expected_machine) = image_elf_identity(architecture);
    let mut guest_paths = BTreeSet::new();
    let mut build_ids: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    let mut executables = Vec::with_capacity(raw.userspace.executables.len());
    for executable in raw.userspace.executables {
        if !safe_guest_absolute(&executable.guest_path)
            || !guest_paths.insert(executable.guest_path.clone())
        {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!(
                    "unsafe or duplicate guest executable {:?}",
                    executable.guest_path
                ),
            ));
        }
        if !valid_build_id(&executable.build_id) {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("invalid build ID {:?}", executable.build_id),
            ));
        }
        if executable.source_view != "provider-root" {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                "userspace source_view must be provider-root",
            ));
        }
        if executable.elf_class != expected_class || executable.elf_machine != expected_machine {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("{} has the wrong ELF identity", executable.guest_path),
            ));
        }
        let runtime_sha256 = parse_hash(&executable.runtime_sha256).ok_or_else(|| {
            ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("{} has an invalid runtime SHA-256", executable.guest_path),
            )
        })?;
        if executable.runtime_size == 0 {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("{} has an invalid runtime size", executable.guest_path),
            ));
        }
        let debug_elf = descriptor("userspace.debug_elf", &executable.debug_elf)?;
        let expected_debug = parse_hash(&executable.debug_sha256).ok_or_else(|| {
            ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!("{} has an invalid debug SHA-256", executable.guest_path),
            )
        })?;
        if debug_elf.sha256 != expected_debug {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!(
                    "{} debug ELF hash disagrees with its artifact",
                    executable.guest_path
                ),
            ));
        }
        if executable.debug_size == 0 || debug_elf.size != executable.debug_size {
            return Err(ResolveError::invalid_image(
                provider_box,
                manifest_path,
                format!(
                    "{} debug ELF size disagrees with its artifact",
                    executable.guest_path
                ),
            ));
        }
        if let Some(prior) = build_ids.insert(executable.build_id.clone(), debug_elf.sha256) {
            if prior != debug_elf.sha256 {
                return Err(ResolveError::invalid_image(
                    provider_box,
                    manifest_path,
                    format!(
                        "build ID {} identifies different debugger ELFs",
                        executable.build_id
                    ),
                ));
            }
        }
        executables.push(ResolvedUserspaceExecutable {
            guest_path: executable.guest_path,
            build_id: executable.build_id,
            runtime_sha256,
            runtime_size: executable.runtime_size,
            debug_elf,
            elf_class: executable.elf_class,
            elf_machine: executable.elf_machine,
        });
    }
    executables.sort_by(|left, right| left.guest_path.cmp(&right.guest_path));
    Ok(ResolvedImageBundle {
        provider_box,
        manifest_path: manifest_path.to_owned(),
        architecture,
        profile: raw.boot.profile,
        init: raw.boot.init,
        initramfs,
        kernel_manifest,
        executables,
        artifacts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated_wire::QemuArchitecture;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fixture {
        chain: Vec<BoxId>,
        paths: HashMap<BoxId, Vec<String>>,
        files: HashMap<(BoxId, String), Vec<u8>>,
        services: Vec<BoxId>,
        reads: RefCell<Vec<(BoxId, String)>>,
    }

    impl DebugResourceCatalog for Fixture {
        fn provider_chain(&self, _consumer_box: BoxId) -> Result<Vec<BoxId>, ResolveError> {
            Ok(self.chain.clone())
        }

        fn regular_captured_paths(&self, provider_box: BoxId) -> Result<Vec<String>, ResolveError> {
            Ok(self.paths.get(&provider_box).cloned().unwrap_or_default())
        }

        fn box_read_file(
            &self,
            provider_box: BoxId,
            relative_path: &str,
        ) -> Result<Vec<u8>, ResolveError> {
            self.reads
                .borrow_mut()
                .push((provider_box, relative_path.to_owned()));
            self.files
                .get(&(provider_box, relative_path.to_owned()))
                .cloned()
                .ok_or_else(|| {
                    ResolveError::Catalog(format!(
                        "missing fixture file {provider_box}:{relative_path}",
                    ))
                })
        }

        fn declared_service_boxes(&self, _service_name: &str) -> Result<Vec<BoxId>, ResolveError> {
            Ok(self.services.clone())
        }
    }

    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn add_bundle(fixture: &mut Fixture, provider: BoxId, directory: &str, architecture: &str) {
        let values = [
            ("vmlinux", b"symbols".as_slice()),
            ("kernel", b"boot-image".as_slice()),
            ("callgate.json", b"{}".as_slice()),
            ("vmlinux-gdb.py", b"# loader".as_slice()),
        ];
        let artifacts: Vec<_> = values
            .iter()
            .map(|(path, bytes)| {
                json!({
                    "path": path,
                    "size": bytes.len(),
                    "sha256": hash(bytes),
                })
            })
            .collect();
        let document = json!({
            "format": KERNEL_BUNDLE_FORMAT,
            "architecture": architecture,
            "kernel": {
                "vmlinux": "vmlinux",
                "vmlinux_sha256": hash(b"symbols"),
                "boot_image": "kernel",
                "boot_image_sha256": hash(b"boot-image"),
            },
            "entrypoints": {
                "callgate": "callgate.json",
                "gdb_loader": "vmlinux-gdb.py",
            },
            "artifacts": artifacts,
        });
        let manifest = if directory.is_empty() {
            "bundle.json".to_owned()
        } else {
            format!("{directory}/bundle.json")
        };
        fixture
            .paths
            .entry(provider)
            .or_default()
            .push(manifest.clone());
        fixture
            .files
            .insert((provider, manifest), serde_json::to_vec(&document).unwrap());
        for (path, bytes) in values {
            let rooted = if directory.is_empty() {
                path.to_owned()
            } else {
                format!("{directory}/{path}")
            };
            fixture.files.insert((provider, rooted), bytes.to_vec());
        }
    }

    fn add_image(fixture: &mut Fixture, provider: BoxId, directory: &str, architecture: &str) {
        let kernel_directory = format!("{directory}/kernel");
        add_bundle(fixture, provider, &kernel_directory, architecture);
        let kernel_manifest_path = format!("{kernel_directory}/bundle.json");
        let kernel_manifest = fixture
            .files
            .get(&(provider, kernel_manifest_path))
            .unwrap()
            .clone();
        let initramfs = b"newc-root".as_slice();
        let debug_elf = b"debug-elf".as_slice();
        let build_id = "0123456789abcdef";
        let (profile, class, machine) = match architecture {
            "aarch64" => ("virt-initramfs-aarch64-v1", 64, 183),
            "x86_64" => ("microvm-initramfs-x86_64-v1", 64, 62),
            "arm" => ("virt-initramfs-arm-v1", 32, 40),
            "mmips" => ("malta-initramfs-mipsel-v1", 32, 8),
            other => panic!("unsupported fixture architecture {other}"),
        };
        let values = [
            ("kernel/bundle.json", kernel_manifest.as_slice()),
            ("rootfs.cpio", initramfs),
            ("symbols/0123456789abcdef.elf", debug_elf),
        ];
        let artifacts: Vec<_> = values
            .iter()
            .map(|(path, bytes)| {
                json!({
                    "path": path,
                    "size": bytes.len(),
                    "sha256": hash(bytes),
                })
            })
            .collect();
        let document = json!({
            "format": IMAGE_BUNDLE_FORMAT,
            "architecture": architecture,
            "boot": {
                "profile": profile,
                "kernel_bundle": "kernel/bundle.json",
                "initramfs": "rootfs.cpio",
                "init": "/sbin/init",
            },
            "userspace": {"executables": [{
                "guest_path": "/usr/sbin/quagga",
                "build_id": build_id,
                "runtime_sha256": hash(b"runtime-elf"),
                "runtime_size": 11,
                "debug_elf": "symbols/0123456789abcdef.elf",
                "debug_sha256": hash(debug_elf),
                "debug_size": debug_elf.len(),
                "elf_class": class,
                "elf_machine": machine,
                "source_view": "provider-root",
            }]},
            "artifacts": artifacts,
        });
        let manifest_path = format!("{directory}/image.json");
        fixture
            .paths
            .entry(provider)
            .or_default()
            .push(manifest_path.clone());
        fixture.files.insert(
            (provider, manifest_path),
            serde_json::to_vec(&document).unwrap(),
        );
        for (path, bytes) in values {
            fixture
                .files
                .insert((provider, format!("{directory}/{path}")), bytes.to_vec());
        }
    }

    fn fixture() -> Fixture {
        Fixture {
            chain: vec![10, 20],
            services: vec![90],
            ..Fixture::default()
        }
    }

    #[test]
    fn selects_matching_architecture_and_returns_box_owned_descriptors() {
        let mut fixture = fixture();
        add_bundle(&mut fixture, 10, "debug/arm", "aarch64");
        add_bundle(&mut fixture, 20, "debug/x86", "x86_64");

        let resolved = resolve_from_catalog(&fixture, 10, QemuArchitecture::X8664).unwrap();

        assert_eq!(resolved.service.provider_box, 90);
        assert_eq!(resolved.service.name, "viros-debug");
        assert_eq!(resolved.bundle.provider_box, 20);
        assert_eq!(resolved.bundle.manifest_path, "debug/x86/bundle.json");
        assert_eq!(resolved.bundle.kernel.boot_image.path, "debug/x86/kernel");
        assert_eq!(
            resolved.bundle.entrypoints.gdb_loader.path,
            "debug/x86/vmlinux-gdb.py"
        );
        assert!(
            fixture
                .reads
                .borrow()
                .iter()
                .all(|(provider, _)| *provider == 10 || *provider == 20)
        );
    }

    #[test]
    fn selects_named_image_and_exact_userspace_identity() {
        let mut fixture = fixture();
        add_image(&mut fixture, 10, "openwrt", "aarch64");

        let resolved = resolve_from_catalog(&fixture, 10, QemuArchitecture::Aarch64).unwrap();
        let image = resolved.image.expect("named image");
        assert_eq!(image.provider_box, 10);
        assert_eq!(image.profile, "virt-initramfs-aarch64-v1");
        assert_eq!(image.initramfs.path, "openwrt/rootfs.cpio");
        assert_eq!(image.kernel_manifest.path, resolved.bundle.manifest_path);
        assert_eq!(image.executables.len(), 1);
        assert_eq!(image.executables[0].guest_path, "/usr/sbin/quagga");
        assert_eq!(image.executables[0].build_id, "0123456789abcdef");
        assert_eq!(
            image.executables[0].debug_elf.path,
            "openwrt/symbols/0123456789abcdef.elf"
        );
        let catalog = wire_image_catalog(&image).unwrap();
        assert_eq!(
            catalog.profile,
            crate::generated_wire::DebugImageProfile::VirtInitramfsAarch64V1
        );
        assert_eq!(catalog.executables.as_slice().len(), 1);
        assert_eq!(
            catalog.executables.as_slice()[0].source_view,
            crate::generated_wire::DebugSourceView::ProviderRoot
        );
        assert_eq!(
            catalog.executables.as_slice()[0].debug_elf.as_slice(),
            b"openwrt/symbols/0123456789abcdef.elf"
        );
        assert_eq!(
            catalog.executables.as_slice()[0].debug_sha256.0,
            image.executables[0].debug_elf.sha256
        );
        assert_eq!(
            catalog.executables.as_slice()[0].debug_size,
            image.executables[0].debug_elf.size
        );
    }

    #[test]
    fn validates_provider_generated_manifests_only_at_returned_paths() {
        let mut fixture = fixture();
        add_image(&mut fixture, 77, "derived/session", "x86_64");
        // An unrelated matching publication elsewhere must not participate in
        // exact selected-image validation.
        add_image(&mut fixture, 10, "old/publication", "x86_64");

        let resolved = resolve_exact_from_catalog(
            &fixture,
            77,
            "derived/session/kernel/bundle.json",
            "derived/session/image.json",
            QemuArchitecture::X8664,
        )
        .unwrap();

        assert_eq!(resolved.bundle.provider_box, 77);
        assert_eq!(
            resolved.bundle.manifest_path,
            "derived/session/kernel/bundle.json"
        );
        assert_eq!(
            resolved.image.unwrap().manifest_path,
            "derived/session/image.json"
        );
    }

    #[test]
    fn exact_validation_requires_returned_manifests_in_the_child_own_layer() {
        let mut fixture = fixture();
        add_image(&mut fixture, 77, "derived/session", "aarch64");
        fixture
            .paths
            .get_mut(&77)
            .unwrap()
            .retain(|path| path != "derived/session/image.json");

        let error = resolve_exact_from_catalog(
            &fixture,
            77,
            "derived/session/kernel/bundle.json",
            "derived/session/image.json",
            QemuArchitecture::Aarch64,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ResolveError::Catalog(message) if message.contains("did not capture")
        ));
    }

    #[test]
    fn armv7_and_mmips_use_exact_32_bit_image_identities() {
        let cases = [
            (
                "arm",
                QemuArchitecture::Arm,
                "virt-initramfs-arm-v1",
                crate::generated_wire::DebugImageProfile::VirtInitramfsArmV1,
                40,
            ),
            (
                "mmips",
                QemuArchitecture::Mmips,
                "malta-initramfs-mipsel-v1",
                crate::generated_wire::DebugImageProfile::MaltaInitramfsMipselV1,
                8,
            ),
        ];
        for (name, architecture, profile, wire_profile, machine) in cases {
            let mut fixture = fixture();
            add_image(&mut fixture, 10, "openwrt", name);
            let resolved = resolve_from_catalog(&fixture, 10, architecture).unwrap();
            let image = resolved.image.expect("named image");
            assert_eq!(image.profile, profile);
            assert_eq!(image.executables[0].elf_class, 32);
            assert_eq!(image.executables[0].elf_machine, machine);
            assert_eq!(wire_image_catalog(&image).unwrap().profile, wire_profile);
        }
    }

    #[test]
    fn image_debug_elf_hash_mismatch_is_rejected() {
        let mut fixture = fixture();
        add_image(&mut fixture, 10, "openwrt", "x86_64");
        let manifest_key = (10, "openwrt/image.json".to_owned());
        let mut document: serde_json::Value =
            serde_json::from_slice(fixture.files.get(&manifest_key).unwrap()).unwrap();
        document["userspace"]["executables"][0]["debug_sha256"] = json!("00".repeat(32));
        fixture
            .files
            .insert(manifest_key, serde_json::to_vec(&document).unwrap());

        let error = resolve_from_catalog(&fixture, 10, QemuArchitecture::X8664).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::InvalidImage { reason, .. }
                if reason.contains("debug ELF hash disagrees")
        ));
    }

    #[test]
    fn image_debug_elf_size_mismatch_is_rejected() {
        let mut fixture = fixture();
        add_image(&mut fixture, 10, "openwrt", "x86_64");
        let manifest_key = (10, "openwrt/image.json".to_owned());
        let mut document: serde_json::Value =
            serde_json::from_slice(fixture.files.get(&manifest_key).unwrap()).unwrap();
        document["userspace"]["executables"][0]["debug_size"] = json!(1);
        fixture
            .files
            .insert(manifest_key, serde_json::to_vec(&document).unwrap());

        let error = resolve_from_catalog(&fixture, 10, QemuArchitecture::X8664).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::InvalidImage { reason, .. }
                if reason.contains("debug ELF size disagrees")
        ));
    }

    #[test]
    fn image_cannot_silently_select_a_different_kernel_bundle() {
        let mut fixture = fixture();
        add_image(&mut fixture, 10, "openwrt", "aarch64");
        let manifest_key = (10, "openwrt/image.json".to_owned());
        let mut document: serde_json::Value =
            serde_json::from_slice(fixture.files.get(&manifest_key).unwrap()).unwrap();
        document["boot"]["kernel_bundle"] = json!("other/bundle.json");
        fixture
            .files
            .insert(manifest_key, serde_json::to_vec(&document).unwrap());

        let error = resolve_from_catalog(&fixture, 10, QemuArchitecture::Aarch64).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::InvalidImage { reason, .. }
                if reason.contains("absent from artifacts")
        ));
    }

    #[test]
    fn rejects_ambiguous_matching_bundles_without_precedence_guess() {
        let mut fixture = fixture();
        add_bundle(&mut fixture, 10, "one", "aarch64");
        add_bundle(&mut fixture, 20, "two", "aarch64");

        let error = resolve_from_catalog(&fixture, 10, QemuArchitecture::Aarch64).unwrap_err();
        assert_eq!(
            error,
            ResolveError::AmbiguousBundle {
                architecture: "aarch64",
                candidates: vec![
                    (10, "one/bundle.json".into()),
                    (20, "two/bundle.json".into())
                ],
            }
        );
    }

    #[test]
    fn rejects_ambiguous_declared_service_provider() {
        let mut fixture = fixture();
        fixture.services = vec![90, 91];

        let error = resolve_from_catalog(&fixture, 10, QemuArchitecture::Aarch64).unwrap_err();
        assert_eq!(
            error,
            ResolveError::AmbiguousService {
                name: DEBUG_SERVICE_NAME,
                boxes: vec![90, 91],
            }
        );
    }

    #[test]
    fn rejects_artifact_hash_mismatch() {
        let mut fixture = fixture();
        add_bundle(&mut fixture, 10, "debug", "aarch64");
        fixture
            .files
            .insert((10, "debug/kernel".into()), b"changed!!!".to_vec());

        let error = resolve_from_catalog(&fixture, 10, QemuArchitecture::Aarch64).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::InvalidBundle { reason, .. } if reason.contains("SHA-256 mismatch")
        ));
    }

    #[test]
    fn rejects_artifact_path_traversal_before_reading_it() {
        let mut fixture = fixture();
        add_bundle(&mut fixture, 10, "debug", "aarch64");
        let manifest_key = (10, "debug/bundle.json".to_owned());
        let mut document: serde_json::Value =
            serde_json::from_slice(fixture.files.get(&manifest_key).unwrap()).unwrap();
        document["artifacts"][0]["path"] = json!("../vmlinux");
        fixture
            .files
            .insert(manifest_key, serde_json::to_vec(&document).unwrap());
        fixture.reads.borrow_mut().clear();

        let error = resolve_from_catalog(&fixture, 10, QemuArchitecture::Aarch64).unwrap_err();
        assert!(
            matches!(error, ResolveError::InvalidBundle { reason, .. } if reason.contains("unsafe artifact path"))
        );
        assert!(
            !fixture
                .reads
                .borrow()
                .iter()
                .any(|(_, path)| path.contains(".."))
        );
    }

    #[test]
    fn ignores_unrelated_bundle_json_files() {
        let mut fixture = fixture();
        fixture
            .paths
            .entry(10)
            .or_default()
            .push("project/bundle.json".into());
        fixture.files.insert(
            (10, "project/bundle.json".into()),
            br#"{"format":"some-project-v1"}"#.to_vec(),
        );
        add_bundle(&mut fixture, 20, "debug", "x86_64");

        let resolved = resolve_from_catalog(&fixture, 10, QemuArchitecture::X8664).unwrap();
        assert_eq!(resolved.bundle.provider_box, 20);
    }
}
