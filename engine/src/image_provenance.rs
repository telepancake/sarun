//! Pure selected-image provenance resolution.
//!
//! The caller resolves the user-facing box selector and passes a normalized,
//! provider-root-relative path.  The selected file's own captured row is the
//! root of identity; a parent, attachment, or host file cannot satisfy it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::{Component, Path};

pub(crate) type BoxId = i64;
pub(crate) type RowId = i64;

/// Keep a corrupt or unexpectedly broad graph from turning one UI action into
/// unbounded output. Traversal continues only until this many distinct nodes.
pub(crate) const MAX_ANCESTRY_ARTIFACTS: usize = crate::generated_wire::LIMIT_COLLECTION_ITEMS;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapturedFileRecord {
    pub size: u64,
    pub regular: bool,
    pub first_writer: Option<RowId>,
    pub last_writer: Option<RowId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessRecord {
    pub row_id: RowId,
    pub parent_row_id: Option<RowId>,
    pub executable: String,
    pub cwd: String,
    pub argv: Vec<String>,
    pub pipeline_row_id: Option<RowId>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PipelineRecord {
    pub row_id: RowId,
    pub command: String,
    pub spawned_at: f64,
    pub completed_at: Option<f64>,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuildEdgeRecord {
    pub row_id: RowId,
    pub outputs: Vec<String>,
    pub inputs: Vec<String>,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BoxRecord {
    pub parent_box: Option<BoxId>,
    pub read_only_attachments: Vec<BoxId>,
}

/// Narrow read-only boundary implemented by the Sarun archive reader and by
/// unit-test fixtures. `captured_file` reads an own-layer row only.
pub(crate) trait ProvenanceCatalog {
    fn provider_chain(&self, source_box: BoxId) -> Result<Vec<BoxId>, ProvenanceError>;
    fn captured_file(
        &self,
        box_id: BoxId,
        relative_path: &str,
    ) -> Result<Option<CapturedFileRecord>, ProvenanceError>;
    fn process(
        &self,
        box_id: BoxId,
        row_id: RowId,
    ) -> Result<Option<ProcessRecord>, ProvenanceError>;
    fn pipeline(
        &self,
        box_id: BoxId,
        row_id: RowId,
    ) -> Result<Option<PipelineRecord>, ProvenanceError>;
    fn build_edges(&self, box_id: BoxId) -> Result<Vec<BuildEdgeRecord>, ProvenanceError>;
    fn box_record(&self, box_id: BoxId) -> Result<BoxRecord, ProvenanceError>;
}

struct EngineCatalog<'a> {
    overlay: &'a crate::overlay::Overlay,
}

fn catalog_error(context: &str, error: impl fmt::Display) -> ProvenanceError {
    ProvenanceError::Catalog(format!("{context}: {error}"))
}

fn wire_path(value: &[u8], context: &str) -> Result<String, ProvenanceError> {
    std::str::from_utf8(value)
        .map(str::to_owned)
        .map_err(|error| catalog_error(context, error))
}

impl ProvenanceCatalog for EngineCatalog<'_> {
    fn provider_chain(&self, source_box: BoxId) -> Result<Vec<BoxId>, ProvenanceError> {
        self.overlay
            .debug_provider_chain(source_box)
            .map_err(|error| catalog_error("provider chain", error))
    }

    fn captured_file(
        &self,
        box_id: BoxId,
        relative_path: &str,
    ) -> Result<Option<CapturedFileRecord>, ProvenanceError> {
        let Some(connection) = crate::discover::open_ro_for(box_id) else {
            return Ok(None);
        };
        let row = connection.query_row(
            "SELECT sz,mode,writer,last_writer FROM sqlar WHERE name=?1",
            [relative_path],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        );
        match row {
            Ok((size, mode, first_writer, last_writer)) => Ok(Some(CapturedFileRecord {
                size: u64::try_from(size)
                    .map_err(|_| ProvenanceError::Catalog("negative captured file size".into()))?,
                regular: mode & i64::from(libc::S_IFMT) == i64::from(libc::S_IFREG),
                first_writer,
                last_writer,
            })),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(catalog_error("captured file row", error)),
        }
    }

    fn process(
        &self,
        box_id: BoxId,
        row_id: RowId,
    ) -> Result<Option<ProcessRecord>, ProvenanceError> {
        let row_id = u64::try_from(row_id)
            .map_err(|_| ProvenanceError::Catalog("negative process row id".into()))?;
        let rows = crate::discover::processes_typed(box_id)
            .map_err(|error| catalog_error("process rows", error))?;
        let Some(row) = rows.into_iter().find(|row| row.id == row_id) else {
            return Ok(None);
        };
        let provenance = crate::discover::proc_prov_typed(box_id, row_id)
            .map_err(|error| catalog_error("process provenance", error))?
            .ok_or_else(|| {
                ProvenanceError::Catalog(format!("process row {row_id} has no provenance"))
            })?;
        Ok(Some(ProcessRecord {
            row_id: i64::try_from(row.id)
                .map_err(|_| ProvenanceError::Catalog("process row id exceeds i64".into()))?,
            parent_row_id: row
                .parent
                .map(|value| {
                    i64::try_from(value).map_err(|_| {
                        ProvenanceError::Catalog("process parent row id exceeds i64".into())
                    })
                })
                .transpose()?,
            executable: wire_path(row.executable.as_slice(), "process executable")?,
            cwd: wire_path(provenance.cwd.as_slice(), "process cwd")?,
            argv: row
                .argv
                .as_slice()
                .iter()
                .map(|word| wire_path(word.as_slice(), "process argument"))
                .collect::<Result<Vec<_>, _>>()?,
            pipeline_row_id: row
                .pipeline
                .map(|value| {
                    i64::try_from(value).map_err(|_| {
                        ProvenanceError::Catalog("process pipeline row id exceeds i64".into())
                    })
                })
                .transpose()?,
        }))
    }

    fn pipeline(
        &self,
        box_id: BoxId,
        row_id: RowId,
    ) -> Result<Option<PipelineRecord>, ProvenanceError> {
        let Some(connection) = crate::discover::open_ro_for(box_id) else {
            return Ok(None);
        };
        let row = connection.query_row(
            "SELECT cmd,spawn_ts,done_ts,exit_code FROM brushprov WHERE id=?1",
            [row_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, Option<f64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        );
        match row {
            Ok((command, spawned_at, completed_at, exit_code)) => Ok(Some(PipelineRecord {
                row_id,
                command,
                spawned_at,
                completed_at,
                exit_code: exit_code
                    .map(|value| {
                        i32::try_from(value).map_err(|_| {
                            ProvenanceError::Catalog("pipeline exit code exceeds i32".into())
                        })
                    })
                    .transpose()?,
            })),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            // Old capture databases may predate one of these additive
            // columns. Pipeline attribution is useful context, but its absence
            // must not erase exact file/build-edge provenance.
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("no such column") =>
            {
                Ok(None)
            }
            Err(error) => Err(catalog_error("pipeline row", error)),
        }
    }

    fn build_edges(&self, box_id: BoxId) -> Result<Vec<BuildEdgeRecord>, ProvenanceError> {
        crate::discover::build_edges_typed(box_id)
            .map_err(|error| catalog_error("build edges", error))?
            .into_iter()
            .map(|row| {
                Ok(BuildEdgeRecord {
                    row_id: i64::try_from(row.id).map_err(|_| {
                        ProvenanceError::Catalog("build edge row id exceeds i64".into())
                    })?,
                    outputs: row
                        .outputs
                        .as_slice()
                        .iter()
                        .map(|path| wire_path(path.as_slice(), "build output"))
                        .collect::<Result<Vec<_>, _>>()?,
                    inputs: row
                        .inputs
                        .as_slice()
                        .iter()
                        .map(|path| wire_path(path.as_slice(), "build input"))
                        .collect::<Result<Vec<_>, _>>()?,
                    command: row.command.map(|value| value.as_str().to_owned()),
                    exit_code: row.exit_code,
                })
            })
            .collect()
    }

    fn box_record(&self, box_id: BoxId) -> Result<BoxRecord, ProvenanceError> {
        let boxes = crate::discover::discover();
        let r#box = boxes.get(&box_id).ok_or_else(|| {
            ProvenanceError::Catalog(format!("build-context box {box_id} is unavailable"))
        })?;
        let read_only_attachments = r#box
            .meta
            .get("ro_attachments")
            .map(|stored| {
                serde_json::from_str::<Vec<crate::capture::RoAttachment>>(stored)
                    .map_err(|error| catalog_error("read-only attachment metadata", error))
            })
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .filter_map(|attachment| match attachment {
                crate::capture::RoAttachment::Box(id) => Some(id),
                crate::capture::RoAttachment::Ext(_) => None,
            })
            .collect();
        Ok(BoxRecord {
            parent_box: r#box.parent,
            read_only_attachments,
        })
    }
}

pub(crate) fn resolve_selected_image_from_engine(
    overlay: &crate::overlay::Overlay,
    source_box: BoxId,
    relative_path: &str,
) -> Result<SelectedImageProvenance, ProvenanceError> {
    resolve_selected_image(&EngineCatalog { overlay }, source_box, relative_path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedImageRecord {
    pub source_box: BoxId,
    pub relative_path: String,
    pub size: u64,
    pub first_writer: Option<RowId>,
    pub last_writer: Option<RowId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProducerEdge {
    /// Identical graph rows from repeated graph capture are one semantic edge.
    pub row_ids: Vec<RowId>,
    pub outputs: Vec<String>,
    pub inputs: Vec<String>,
    pub command: Option<String>,
    pub completed_successfully: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ArtifactKind {
    KernelSymbols,
    KernelBootImage,
    RootFilesystem,
    BuildTool,
    SourceInput,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct AncestryArtifact {
    pub provider_box: BoxId,
    pub relative_path: String,
    pub kind: ArtifactKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuildContextBox {
    pub box_id: BoxId,
    pub parent_box: Option<BoxId>,
    pub read_only_attachments: Vec<BoxId>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectedImageProvenance {
    pub selected: SelectedImageRecord,
    pub writer: Option<ProcessRecord>,
    pub pipeline: Option<PipelineRecord>,
    pub producer_edge: Option<ProducerEdge>,
    pub build_context: Vec<BuildContextBox>,
    pub artifacts: Vec<AncestryArtifact>,
    pub artifacts_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProvenanceError {
    UnsafePath(String),
    MissingSelectedFile {
        box_id: BoxId,
        path: String,
    },
    SelectedFileNotRegular {
        box_id: BoxId,
        path: String,
    },
    InvalidProviderChain {
        source_box: BoxId,
    },
    AmbiguousProducer {
        path: String,
        row_groups: Vec<Vec<RowId>>,
    },
    Catalog(String),
}

impl fmt::Display for ProvenanceError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath(path) => {
                write!(output, "selected image path is not normalized: {path:?}")
            }
            Self::MissingSelectedFile { box_id, path } => {
                write!(
                    output,
                    "box {box_id} has no own-layer captured file {path:?}"
                )
            }
            Self::SelectedFileNotRegular { box_id, path } => {
                write!(output, "box {box_id}:{path} is not a regular captured file")
            }
            Self::InvalidProviderChain { source_box } => {
                write!(
                    output,
                    "provider chain does not begin with source box {source_box}"
                )
            }
            Self::AmbiguousProducer { path, row_groups } => {
                write!(
                    output,
                    "{path:?} has distinct producing build edges {row_groups:?}"
                )
            }
            Self::Catalog(message) => write!(output, "captured provenance: {message}"),
        }
    }
}

impl std::error::Error for ProvenanceError {}

pub(crate) fn normalized_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Lexically place a build-graph path in the captured provider root. This does
/// not touch the host filesystem and rejects traversal above that root.
fn root_relative(cwd: &str, path: &str) -> Option<String> {
    let mut parts = Vec::new();
    if !path.starts_with('/') {
        append_components(&mut parts, cwd)?;
    }
    append_components(&mut parts, path)?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn append_components(parts: &mut Vec<String>, value: &str) -> Option<()> {
    for component in Path::new(value).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(())
}

type EdgeGroup = (Vec<RowId>, Vec<String>, Vec<String>, Option<String>, bool);

fn edge_groups<'a>(
    edges: impl IntoIterator<Item = &'a BuildEdgeRecord>,
    cwd: &str,
) -> Vec<EdgeGroup> {
    let mut groups: BTreeMap<(Vec<String>, Vec<String>, Option<String>), (Vec<RowId>, bool)> =
        BTreeMap::new();
    for edge in edges {
        let Some(outputs) = edge
            .outputs
            .iter()
            .map(|path| root_relative(cwd, path))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let Some(inputs) = edge
            .inputs
            .iter()
            .map(|path| root_relative(cwd, path))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let group = groups
            .entry((outputs, inputs, edge.command.clone()))
            .or_default();
        group.0.push(edge.row_id);
        group.1 |= edge.exit_code == Some(0);
    }
    groups
        .into_iter()
        .map(|((outputs, inputs, command), (row_ids, success))| {
            (row_ids, outputs, inputs, command, success)
        })
        .collect()
}

fn artifact_kind(path: &str) -> Option<ArtifactKind> {
    let name = Path::new(path).file_name()?.to_str()?.to_ascii_lowercase();
    if name == "vmlinux" || name.starts_with("vmlinux.") || name.ends_with(".vmlinux") {
        Some(ArtifactKind::KernelSymbols)
    } else if matches!(name.as_str(), "image" | "zimage" | "uimage" | "bzimage")
        || name.starts_with("vmlinuz")
        || name == "kernel.elf"
    {
        Some(ArtifactKind::KernelBootImage)
    } else if name.contains("rootfs")
        && [
            ".img",
            ".bin",
            ".cpio",
            ".cpio.gz",
            ".squashfs",
            ".ubifs",
            ".jffs2",
            ".tar",
            ".tar.gz",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        Some(ArtifactKind::RootFilesystem)
    } else {
        None
    }
}

fn pipeline_for_writer(
    catalog: &impl ProvenanceCatalog,
    source_box: BoxId,
    writer: &ProcessRecord,
) -> Result<Option<PipelineRecord>, ProvenanceError> {
    let mut current = Some(writer.clone());
    let mut seen = BTreeSet::new();
    for _ in 0..64 {
        let Some(process) = current else { break };
        if !seen.insert(process.row_id) {
            break;
        }
        if let Some(pipeline_id) = process.pipeline_row_id {
            if let Some(pipeline) = catalog.pipeline(source_box, pipeline_id)? {
                return Ok(Some(pipeline));
            }
        }
        current = match process.parent_row_id {
            Some(parent) => catalog.process(source_box, parent)?,
            None => None,
        };
    }
    Ok(None)
}

fn provider_for_path(
    catalog: &impl ProvenanceCatalog,
    chain: &[BoxId],
    path: &str,
) -> Result<Option<BoxId>, ProvenanceError> {
    for box_id in chain {
        if catalog
            .captured_file(*box_id, path)?
            .is_some_and(|record| record.regular)
        {
            return Ok(Some(*box_id));
        }
    }
    Ok(None)
}

fn add_artifact(
    artifacts: &mut BTreeSet<AncestryArtifact>,
    truncated: &mut bool,
    artifact: AncestryArtifact,
) {
    if artifacts.contains(&artifact) {
        return;
    }
    if artifacts.len() >= MAX_ANCESTRY_ARTIFACTS {
        *truncated = true;
    } else {
        artifacts.insert(artifact);
    }
}

/// Resolve writer/build ancestry for a normalized captured image identity.
///
/// Distinct build edges claiming the selected output are reported as an
/// ambiguity. Repeated identical rows are folded while preserving their row
/// IDs. An absent build graph is valid: writer and box context still resolve.
pub(crate) fn resolve_selected_image(
    catalog: &impl ProvenanceCatalog,
    source_box: BoxId,
    relative_path: &str,
) -> Result<SelectedImageProvenance, ProvenanceError> {
    if !normalized_relative_path(relative_path) {
        return Err(ProvenanceError::UnsafePath(relative_path.to_owned()));
    }
    let selected = catalog
        .captured_file(source_box, relative_path)?
        .ok_or_else(|| ProvenanceError::MissingSelectedFile {
            box_id: source_box,
            path: relative_path.to_owned(),
        })?;
    if !selected.regular {
        return Err(ProvenanceError::SelectedFileNotRegular {
            box_id: source_box,
            path: relative_path.to_owned(),
        });
    }
    let chain = catalog.provider_chain(source_box)?;
    if chain.first().copied() != Some(source_box) {
        return Err(ProvenanceError::InvalidProviderChain { source_box });
    }

    let writer = match selected.last_writer.or(selected.first_writer) {
        Some(row_id) => catalog.process(source_box, row_id)?,
        None => None,
    };
    let pipeline = match &writer {
        Some(writer) => pipeline_for_writer(catalog, source_box, writer)?,
        None => None,
    };
    let cwd = writer.as_ref().map_or("/", |writer| writer.cwd.as_str());
    let edges = catalog.build_edges(source_box)?;
    let matching = edges.iter().filter(|edge| {
        edge.outputs
            .iter()
            .filter_map(|path| root_relative(cwd, path))
            .any(|path| path == relative_path)
    });
    let mut groups = edge_groups(matching, cwd);
    if groups.len() > 1 {
        return Err(ProvenanceError::AmbiguousProducer {
            path: relative_path.to_owned(),
            row_groups: groups.into_iter().map(|group| group.0).collect(),
        });
    }
    let producer_edge = groups.pop().map(
        |(row_ids, outputs, inputs, command, completed_successfully)| ProducerEdge {
            row_ids,
            outputs,
            inputs,
            command,
            completed_successfully,
        },
    );

    let mut by_output: BTreeMap<String, Vec<&BuildEdgeRecord>> = BTreeMap::new();
    for edge in &edges {
        for output in edge
            .outputs
            .iter()
            .filter_map(|path| root_relative(cwd, path))
        {
            by_output.entry(output).or_default().push(edge);
        }
    }
    let mut pending: VecDeque<String> = producer_edge
        .iter()
        .flat_map(|edge| edge.inputs.iter().cloned())
        .collect();
    let mut visited = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    let mut artifacts_truncated = false;
    while let Some(path) = pending.pop_front() {
        if visited.len() >= MAX_ANCESTRY_ARTIFACTS {
            artifacts_truncated = true;
            break;
        }
        if !visited.insert(path.clone()) {
            continue;
        }
        if let Some(kind) = artifact_kind(&path) {
            let provider_box = provider_for_path(catalog, &chain, &path)?.unwrap_or(source_box);
            add_artifact(
                &mut artifacts,
                &mut artifacts_truncated,
                AncestryArtifact {
                    provider_box,
                    relative_path: path.clone(),
                    kind,
                },
            );
        }

        let producers = by_output.get(&path).cloned().unwrap_or_default();
        let dependency_groups = edge_groups(producers, cwd);
        if dependency_groups.len() == 1 {
            let (_, outputs, inputs, _, _) = &dependency_groups[0];
            for output in outputs {
                if let Some(file) = catalog.captured_file(source_box, output)? {
                    if let Some(writer_id) = file.last_writer.or(file.first_writer) {
                        if let Some(output_writer) = catalog.process(source_box, writer_id)? {
                            if let Some(tool_path) =
                                root_relative(&output_writer.cwd, &output_writer.executable)
                            {
                                if let Some(provider_box) =
                                    provider_for_path(catalog, &chain, &tool_path)?
                                {
                                    add_artifact(
                                        &mut artifacts,
                                        &mut artifacts_truncated,
                                        AncestryArtifact {
                                            provider_box,
                                            relative_path: tool_path,
                                            kind: ArtifactKind::BuildTool,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
            pending.extend(inputs.iter().cloned());
        } else {
            let provider_box = provider_for_path(catalog, &chain, &path)?.unwrap_or(source_box);
            add_artifact(
                &mut artifacts,
                &mut artifacts_truncated,
                AncestryArtifact {
                    provider_box,
                    relative_path: path,
                    kind: ArtifactKind::SourceInput,
                },
            );
        }
    }
    if let Some(writer) = &writer {
        if let Some(tool_path) = root_relative(&writer.cwd, &writer.executable) {
            if let Some(provider_box) = provider_for_path(catalog, &chain, &tool_path)? {
                add_artifact(
                    &mut artifacts,
                    &mut artifacts_truncated,
                    AncestryArtifact {
                        provider_box,
                        relative_path: tool_path,
                        kind: ArtifactKind::BuildTool,
                    },
                );
            }
        }
    }

    let mut build_context = Vec::with_capacity(chain.len());
    for box_id in chain {
        let record = catalog.box_record(box_id)?;
        build_context.push(BuildContextBox {
            box_id,
            parent_box: record.parent_box,
            read_only_attachments: record.read_only_attachments,
        });
    }
    Ok(SelectedImageProvenance {
        selected: SelectedImageRecord {
            source_box,
            relative_path: relative_path.to_owned(),
            size: selected.size,
            first_writer: selected.first_writer,
            last_writer: selected.last_writer,
        },
        writer,
        pipeline,
        producer_edge,
        build_context,
        artifacts: artifacts.into_iter().collect(),
        artifacts_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fixture {
        chain: Vec<BoxId>,
        files: BTreeMap<(BoxId, String), CapturedFileRecord>,
        processes: BTreeMap<(BoxId, RowId), ProcessRecord>,
        pipelines: BTreeMap<(BoxId, RowId), PipelineRecord>,
        edges: BTreeMap<BoxId, Vec<BuildEdgeRecord>>,
        boxes: BTreeMap<BoxId, BoxRecord>,
    }

    impl ProvenanceCatalog for Fixture {
        fn provider_chain(&self, _source_box: BoxId) -> Result<Vec<BoxId>, ProvenanceError> {
            Ok(self.chain.clone())
        }
        fn captured_file(
            &self,
            box_id: BoxId,
            relative_path: &str,
        ) -> Result<Option<CapturedFileRecord>, ProvenanceError> {
            Ok(self.files.get(&(box_id, relative_path.into())).cloned())
        }
        fn process(
            &self,
            box_id: BoxId,
            row_id: RowId,
        ) -> Result<Option<ProcessRecord>, ProvenanceError> {
            Ok(self.processes.get(&(box_id, row_id)).cloned())
        }
        fn pipeline(
            &self,
            box_id: BoxId,
            row_id: RowId,
        ) -> Result<Option<PipelineRecord>, ProvenanceError> {
            Ok(self.pipelines.get(&(box_id, row_id)).cloned())
        }
        fn build_edges(&self, box_id: BoxId) -> Result<Vec<BuildEdgeRecord>, ProvenanceError> {
            Ok(self.edges.get(&box_id).cloned().unwrap_or_default())
        }
        fn box_record(&self, box_id: BoxId) -> Result<BoxRecord, ProvenanceError> {
            Ok(self.boxes.get(&box_id).cloned().unwrap_or_default())
        }
    }

    fn file(writer: Option<RowId>, size: u64) -> CapturedFileRecord {
        CapturedFileRecord {
            size,
            regular: true,
            first_writer: writer,
            last_writer: writer,
        }
    }

    fn fixture() -> Fixture {
        let mut fixture = Fixture {
            chain: vec![30, 20, 10],
            ..Fixture::default()
        };
        fixture
            .files
            .insert((30, "work/out/fw.img".into()), file(Some(7), 4096));
        fixture
            .files
            .insert((30, "work/vmlinux".into()), file(Some(8), 2048));
        fixture
            .files
            .insert((30, "work/rootfs.squashfs".into()), file(Some(9), 1024));
        fixture
            .files
            .insert((20, "src/init/main.c".into()), file(None, 100));
        fixture
            .files
            .insert((10, "sdk/bin/ld".into()), file(None, 200));
        fixture.processes.insert(
            (30, 7),
            ProcessRecord {
                row_id: 7,
                parent_row_id: Some(6),
                executable: "/bin/sh".into(),
                cwd: "/work".into(),
                argv: vec!["sh".into(), "-c".into(), "assemble".into()],
                pipeline_row_id: None,
            },
        );
        fixture.processes.insert(
            (30, 6),
            ProcessRecord {
                row_id: 6,
                parent_row_id: None,
                executable: "/bin/brush".into(),
                cwd: "/work".into(),
                argv: vec!["brush".into()],
                pipeline_row_id: Some(3),
            },
        );
        fixture.processes.insert(
            (30, 8),
            ProcessRecord {
                row_id: 8,
                parent_row_id: None,
                executable: "/sdk/bin/ld".into(),
                cwd: "/work".into(),
                argv: vec!["ld".into()],
                pipeline_row_id: None,
            },
        );
        fixture.pipelines.insert(
            (30, 3),
            PipelineRecord {
                row_id: 3,
                command: "make image".into(),
                spawned_at: 1.0,
                completed_at: Some(2.0),
                exit_code: Some(0),
            },
        );
        fixture.edges.insert(
            30,
            vec![
                BuildEdgeRecord {
                    row_id: 40,
                    outputs: vec!["out/fw.img".into()],
                    inputs: vec!["vmlinux".into(), "rootfs.squashfs".into()],
                    command: Some("assemble".into()),
                    exit_code: Some(0),
                },
                BuildEdgeRecord {
                    row_id: 41,
                    outputs: vec!["vmlinux".into()],
                    inputs: vec!["../src/init/main.c".into()],
                    command: Some("/sdk/bin/ld".into()),
                    exit_code: Some(0),
                },
            ],
        );
        fixture.boxes.insert(
            30,
            BoxRecord {
                parent_box: None,
                read_only_attachments: vec![20, 10],
            },
        );
        fixture
    }

    #[test]
    fn selected_file_anchors_writer_graph_artifacts_and_boxes() {
        let fixture = fixture();
        let result = resolve_selected_image(&fixture, 30, "work/out/fw.img").unwrap();

        assert_eq!(result.selected.last_writer, Some(7));
        assert_eq!(result.writer.as_ref().unwrap().row_id, 7);
        assert_eq!(result.pipeline.as_ref().unwrap().command, "make image");
        assert_eq!(result.producer_edge.as_ref().unwrap().row_ids, vec![40]);
        for expected in [
            AncestryArtifact {
                provider_box: 30,
                relative_path: "work/vmlinux".into(),
                kind: ArtifactKind::KernelSymbols,
            },
            AncestryArtifact {
                provider_box: 30,
                relative_path: "work/rootfs.squashfs".into(),
                kind: ArtifactKind::RootFilesystem,
            },
            AncestryArtifact {
                provider_box: 20,
                relative_path: "src/init/main.c".into(),
                kind: ArtifactKind::SourceInput,
            },
            AncestryArtifact {
                provider_box: 10,
                relative_path: "sdk/bin/ld".into(),
                kind: ArtifactKind::BuildTool,
            },
        ] {
            assert!(result.artifacts.contains(&expected), "missing {expected:?}");
        }
        assert_eq!(result.build_context[0].read_only_attachments, vec![20, 10]);
        assert!(!result.artifacts_truncated);
    }

    #[test]
    fn parent_or_attachment_file_cannot_satisfy_selection() {
        let mut fixture = fixture();
        fixture.files.remove(&(30, "work/out/fw.img".into()));
        fixture
            .files
            .insert((20, "work/out/fw.img".into()), file(None, 1));
        assert!(matches!(
            resolve_selected_image(&fixture, 30, "work/out/fw.img"),
            Err(ProvenanceError::MissingSelectedFile { box_id: 30, .. })
        ));
    }

    #[test]
    fn non_regular_and_non_normalized_selection_are_rejected() {
        let mut fixture = fixture();
        fixture
            .files
            .get_mut(&(30, "work/out/fw.img".into()))
            .unwrap()
            .regular = false;
        assert!(matches!(
            resolve_selected_image(&fixture, 30, "work/out/fw.img"),
            Err(ProvenanceError::SelectedFileNotRegular { .. })
        ));
        for path in ["/work/out/fw.img", "work/../fw.img", ""] {
            assert!(matches!(
                resolve_selected_image(&fixture, 30, path),
                Err(ProvenanceError::UnsafePath(_))
            ));
        }
    }

    #[test]
    fn repeated_equal_edges_fold_but_distinct_producers_do_not() {
        let mut fixture = fixture();
        let original = fixture.edges[&30][0].clone();
        fixture.edges.get_mut(&30).unwrap().push(BuildEdgeRecord {
            row_id: 42,
            ..original
        });
        assert_eq!(
            resolve_selected_image(&fixture, 30, "work/out/fw.img")
                .unwrap()
                .producer_edge
                .unwrap()
                .row_ids,
            vec![40, 42]
        );
        fixture.edges.get_mut(&30).unwrap().push(BuildEdgeRecord {
            row_id: 43,
            outputs: vec!["out/fw.img".into()],
            inputs: vec!["different-input".into()],
            command: Some("other".into()),
            exit_code: Some(0),
        });
        assert!(matches!(
            resolve_selected_image(&fixture, 30, "work/out/fw.img"),
            Err(ProvenanceError::AmbiguousProducer { row_groups, .. })
                if row_groups == vec![vec![43], vec![40, 42]]
                    || row_groups == vec![vec![40, 42], vec![43]]
        ));
    }

    #[test]
    fn ancestry_output_is_finite_and_marks_truncation() {
        let mut fixture = fixture();
        let inputs = (0..MAX_ANCESTRY_ARTIFACTS + 20)
            .map(|index| format!("input-{index}.c"))
            .collect();
        fixture.edges.get_mut(&30).unwrap()[0].inputs = inputs;
        let result = resolve_selected_image(&fixture, 30, "work/out/fw.img").unwrap();
        assert_eq!(result.artifacts.len(), MAX_ANCESTRY_ARTIFACTS);
        assert!(result.artifacts_truncated);
    }
}
