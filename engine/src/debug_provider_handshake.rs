//! Private preparation exchange between the engine and the ViroS service.
//!
//! This protocol precedes the ordinary `DebugProviderStart` frame on the same
//! socket. It carries only exact Sarun box/file identities and returns exact
//! provider-child resource identities. After the engine validates those files
//! it commits the preparation and the socket becomes the existing RSP lane.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;

use crate::generated_wire::QemuArchitecture;
use crate::wire::{DecodeError, get_atom, put_atom, put_compound_payload, put_u64};

const VERSION: u64 = 2;
const MESSAGE_PREPARE: u64 = 1;
const MESSAGE_COMMIT: u64 = 2;
const MESSAGE_ABORT: u64 = 3;
const OUTCOME_PREPARED: u64 = 1;
const OUTCOME_REJECTED: u64 = 2;
const OUTCOME_COMMITTED: u64 = 3;
const OUTCOME_ABORTED: u64 = 4;
const SELECTION_IMAGE: u64 = 1;
const SELECTION_LINUX: u64 = 2;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const TOKEN_BYTES: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapturedArtifact {
    pub box_id: u64,
    pub path: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub record_id: String,
    pub roles: Vec<String>,
    pub architecture: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SelectedBoot {
    Image(CapturedArtifact),
    KernelInitramfs {
        kernel: CapturedArtifact,
        initramfs: CapturedArtifact,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrepareRequest {
    pub architecture: QemuArchitecture,
    pub selected: SelectedBoot,
    pub catalog: Vec<CapturedArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResourceIdentity {
    pub path: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedBoot {
    pub token: [u8; TOKEN_BYTES],
    pub kernel_manifest: ResourceIdentity,
    pub image_manifest: ResourceIdentity,
    pub kernel: ResourceIdentity,
    pub initramfs: ResourceIdentity,
    pub kernel_init: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Prepared(PreparedBoot),
    Rejected { code: String, detail: String },
    Committed([u8; TOKEN_BYTES]),
    Aborted([u8; TOKEN_BYTES]),
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn wire_error(error: DecodeError) -> io::Error {
    invalid(format!("invalid ViroS preparation value: {error:?}"))
}

fn atom(output: &mut Vec<u8>, payload: &[u8]) -> io::Result<()> {
    put_atom(output, payload).map_err(wire_error)
}

fn uint(value: u64) -> Vec<u8> {
    let mut encoded = Vec::new();
    put_u64(&mut encoded, value);
    encoded
}

fn compound(encoded_fields: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoded = Vec::new();
    put_compound_payload(&mut encoded, encoded_fields).map_err(wire_error)?;
    Ok(encoded)
}

fn push_text(output: &mut Vec<u8>, value: &str) -> io::Result<()> {
    atom(output, value.as_bytes())
}

fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.contains('\0') && value.len() <= maximum
}

fn validate_artifact(row: &CapturedArtifact) -> io::Result<()> {
    if !safe_relative(&row.path) || row.path.len() > MAX_PATH_BYTES {
        return Err(invalid("captured artifact path is invalid"));
    }
    if row.record_id.is_empty() || !valid_text(&row.record_id, MAX_TEXT_BYTES) {
        return Err(invalid("captured artifact record identity is invalid"));
    }
    if row.roles.len() > 64
        || row.roles.iter().any(|role| {
            !matches!(
                role.as_str(),
                "device-tree"
                    | "disk"
                    | "firmware"
                    | "initramfs"
                    | "kernel"
                    | "kernel-boot"
                    | "rootfs"
                    | "vmlinux"
            )
        })
    {
        return Err(invalid("captured artifact roles are invalid"));
    }
    if row.architecture.as_ref().is_some_and(|architecture| {
        !matches!(
            architecture.as_str(),
            "aarch64"
                | "arm"
                | "mmips"
                | "mipsbe"
                | "mips64"
                | "powerpc"
                | "tilegx"
                | "x86"
                | "x86_64"
        )
    }) {
        return Err(invalid("captured artifact architecture is invalid"));
    }
    Ok(())
}

fn encode_string_list(values: &[String]) -> io::Result<Vec<u8>> {
    let mut fields = uint(values.len() as u64);
    for value in values {
        push_text(&mut fields, value)?;
    }
    compound(&fields)
}

fn encode_artifact(row: &CapturedArtifact) -> io::Result<Vec<u8>> {
    validate_artifact(row)?;
    let mut fields = uint(row.box_id);
    push_text(&mut fields, &row.path)?;
    fields.extend_from_slice(&uint(row.size));
    atom(&mut fields, &row.sha256)?;
    push_text(&mut fields, &row.record_id)?;
    fields.extend_from_slice(&encode_string_list(&row.roles)?);
    let mut architecture = uint(u64::from(row.architecture.is_some()));
    if let Some(value) = &row.architecture {
        push_text(&mut architecture, value)?;
    }
    fields.extend_from_slice(&compound(&architecture)?);
    compound(&fields)
}

fn encode_artifact_list(rows: &[CapturedArtifact]) -> io::Result<Vec<u8>> {
    if rows.len() > 100_000 {
        return Err(invalid("captured artifact catalog is too large"));
    }
    let mut fields = uint(rows.len() as u64);
    for row in rows {
        fields.extend_from_slice(&encode_artifact(row)?);
    }
    compound(&fields)
}

fn profile_tag(architecture: QemuArchitecture) -> u64 {
    match architecture {
        QemuArchitecture::Aarch64 => 1,
        QemuArchitecture::X8664 => 2,
        QemuArchitecture::Arm => 3,
        QemuArchitecture::Mmips => 4,
    }
}

pub(crate) fn encode_prepare(request: &PrepareRequest) -> io::Result<Vec<u8>> {
    let mut selection = match &request.selected {
        SelectedBoot::Image(selected) => {
            let mut fields = uint(SELECTION_IMAGE);
            fields.extend_from_slice(&encode_artifact(selected)?);
            fields
        }
        SelectedBoot::KernelInitramfs { kernel, initramfs } => {
            let mut fields = uint(SELECTION_LINUX);
            fields.extend_from_slice(&encode_artifact(kernel)?);
            fields.extend_from_slice(&encode_artifact(initramfs)?);
            fields
        }
    };
    selection = compound(&selection)?;

    let mut fields = uint(MESSAGE_PREPARE);
    fields.extend_from_slice(&uint(profile_tag(request.architecture)));
    fields.extend_from_slice(&selection);
    fields.extend_from_slice(&encode_artifact_list(&request.catalog)?);
    // The prebuilt-kernel provider needs no executable tool bindings.
    fields.extend_from_slice(&compound(&uint(0))?);

    let mut frame = uint(VERSION);
    frame.extend_from_slice(&compound(&fields)?);
    if frame.len() > MAX_FRAME_BYTES {
        return Err(invalid("ViroS preparation request is too large"));
    }
    Ok(frame)
}

fn take<'a>(input: &mut &'a [u8], maximum: usize) -> io::Result<&'a [u8]> {
    get_atom(input, maximum).map_err(wire_error)
}

fn canonical_uint(payload: &[u8]) -> io::Result<u64> {
    if payload.len() > 8 || payload.last() == Some(&0) {
        return Err(invalid("non-canonical ViroS preparation integer"));
    }
    Ok(payload
        .iter()
        .enumerate()
        .fold(0u64, |value, (index, byte)| {
            value | (u64::from(*byte) << (index * 8))
        }))
}

fn text(payload: &[u8], label: &str, maximum: usize) -> io::Result<String> {
    if payload.len() > maximum || payload.contains(&0) {
        return Err(invalid(format!("{label} is invalid")));
    }
    std::str::from_utf8(payload)
        .map(str::to_owned)
        .map_err(|_| invalid(format!("{label} is not UTF-8")))
}

fn decode_resource(encoded: &[u8]) -> io::Result<ResourceIdentity> {
    let mut fields = encoded;
    let path = text(
        take(&mut fields, MAX_PATH_BYTES)?,
        "resource path",
        MAX_PATH_BYTES,
    )?;
    let size = canonical_uint(take(&mut fields, 8)?)?;
    let sha256: [u8; 32] = take(&mut fields, 32)?
        .try_into()
        .map_err(|_| invalid("resource digest is not SHA-256"))?;
    if !fields.is_empty() || !safe_relative(&path) {
        return Err(invalid("prepared resource identity is invalid"));
    }
    Ok(ResourceIdentity { path, size, sha256 })
}

fn decode_outcome_atom(encoded: &[u8]) -> io::Result<Outcome> {
    let mut outer = encoded;
    let mut fields = take(&mut outer, MAX_FRAME_BYTES)?;
    if !outer.is_empty() {
        return Err(invalid("provider outcome has trailing atoms"));
    }
    let tag = canonical_uint(take(&mut fields, 8)?)?;
    let outcome = match tag {
        OUTCOME_PREPARED => {
            let token: [u8; TOKEN_BYTES] = take(&mut fields, TOKEN_BYTES)?
                .try_into()
                .map_err(|_| invalid("preparation token has the wrong size"))?;
            let kernel_manifest = decode_resource(take(&mut fields, MAX_FRAME_BYTES)?)?;
            let image_manifest = decode_resource(take(&mut fields, MAX_FRAME_BYTES)?)?;
            let kernel = decode_resource(take(&mut fields, MAX_FRAME_BYTES)?)?;
            let initramfs = decode_resource(take(&mut fields, MAX_FRAME_BYTES)?)?;
            let kernel_init = text(
                take(&mut fields, MAX_PATH_BYTES)?,
                "kernel init",
                MAX_PATH_BYTES,
            )?;
            if !kernel_init.starts_with('/')
                || !safe_relative(&kernel_init[1..])
                || kernel_init.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
            {
                return Err(invalid("prepared kernel init is invalid"));
            }
            Outcome::Prepared(PreparedBoot {
                token,
                kernel_manifest,
                image_manifest,
                kernel,
                initramfs,
                kernel_init,
            })
        }
        OUTCOME_REJECTED => Outcome::Rejected {
            code: text(take(&mut fields, 64)?, "rejection code", 64)?,
            detail: text(
                take(&mut fields, MAX_TEXT_BYTES)?,
                "rejection detail",
                MAX_TEXT_BYTES,
            )?,
        },
        OUTCOME_COMMITTED | OUTCOME_ABORTED => {
            let token: [u8; TOKEN_BYTES] = take(&mut fields, TOKEN_BYTES)?
                .try_into()
                .map_err(|_| invalid("terminal token has the wrong size"))?;
            if tag == OUTCOME_COMMITTED {
                Outcome::Committed(token)
            } else {
                Outcome::Aborted(token)
            }
        }
        _ => return Err(invalid("unknown provider preparation outcome")),
    };
    if !fields.is_empty() {
        return Err(invalid("provider preparation outcome has trailing fields"));
    }
    Ok(outcome)
}

pub(crate) fn read_outcome(stream: &mut UnixStream) -> io::Result<Outcome> {
    let version: u64 = crate::socket_wire::read_atom(stream)?;
    if version != VERSION {
        return Err(invalid(format!(
            "unsupported ViroS preparation version {version}"
        )));
    }
    let encoded = crate::socket_wire::read_encoded_atom(stream, MAX_FRAME_BYTES)?;
    decode_outcome_atom(&encoded)
}

pub(crate) fn write_prepare(stream: &mut UnixStream, request: &PrepareRequest) -> io::Result<()> {
    stream.write_all(&encode_prepare(request)?)?;
    stream.flush()
}

pub(crate) fn write_decision(
    stream: &mut UnixStream,
    commit: bool,
    token: &[u8; TOKEN_BYTES],
) -> io::Result<()> {
    let mut fields = uint(if commit {
        MESSAGE_COMMIT
    } else {
        MESSAGE_ABORT
    });
    atom(&mut fields, token)?;
    let mut frame = uint(VERSION);
    frame.extend_from_slice(&compound(&fields)?);
    stream.write_all(&frame)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(path: &str) -> CapturedArtifact {
        CapturedArtifact {
            box_id: 7,
            path: path.into(),
            size: 3,
            sha256: [0x5a; 32],
            record_id: format!("box:7:{path}"),
            roles: vec!["firmware".into()],
            architecture: Some("mmips".into()),
        }
    }

    #[test]
    fn prepare_is_deterministic_and_contains_no_host_selector() {
        let selected = artifact("out/image.cpio");
        let request = PrepareRequest {
            architecture: QemuArchitecture::Mmips,
            selected: SelectedBoot::Image(selected.clone()),
            catalog: vec![selected],
        };
        let encoded = encode_prepare(&request).unwrap();
        assert_eq!(encoded, encode_prepare(&request).unwrap());
        let encoded_hex = encoded
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            encoded_hex,
            "02f9c20104f95d01f95a07ce6f75742f696d6167652e6370696f03e05a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5ad4626f783a373a6f75742f696d6167652e6370696fca01c86669726d77617265c701c56d6d697073f95d01f95a07ce6f75742f696d6167652e6370696f03e05a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5ad4626f783a373a6f75742f696d6167652e6370696fca01c86669726d77617265c701c56d6d697073c1c0"
        );
        assert!(
            !encoded
                .windows(b"/tmp".len())
                .any(|window| window == b"/tmp")
        );
    }

    #[test]
    fn prepared_outcome_decodes_exact_identities() {
        fn resource(path: &str, byte: u8) -> Vec<u8> {
            let mut fields = Vec::new();
            push_text(&mut fields, path).unwrap();
            fields.extend_from_slice(&uint(19));
            atom(&mut fields, &[byte; 32]).unwrap();
            compound(&fields).unwrap()
        }
        let mut fields = uint(OUTCOME_PREPARED);
        atom(&mut fields, &[9; TOKEN_BYTES]).unwrap();
        fields.extend(resource("bundle/kernel/bundle.json", 1));
        fields.extend(resource("bundle/image.json", 2));
        fields.extend(resource("bundle/kernel/kernel", 3));
        fields.extend(resource("bundle/rootfs.cpio", 4));
        push_text(&mut fields, "/init").unwrap();
        let encoded = compound(&fields).unwrap();
        let Outcome::Prepared(prepared) = decode_outcome_atom(&encoded).unwrap() else {
            panic!("wrong outcome");
        };
        assert_eq!(prepared.token, [9; TOKEN_BYTES]);
        assert_eq!(prepared.kernel.sha256, [3; 32]);
        assert_eq!(prepared.kernel_init, "/init");
    }
}
