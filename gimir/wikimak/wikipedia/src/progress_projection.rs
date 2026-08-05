//! Bounded, plan-bound live progress for a full Wikipedia build.
//!
//! This file is telemetry, never lifecycle authority.  The durable build
//! inspector owns target and assembly state; writers only project observations
//! at their owned source, target-publication, and assembly boundaries.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::direct::{
    DirectBuildPlan, LiveTargetProgress, MirrorBuildProgress, MirrorTargetProgress,
};

const MAGIC: &[u8; 8] = b"SWPROG01";
const BANK_MAGIC: &[u8; 8] = b"SWPSLOT1";
const SCHEMA: u32 = 1;
const HEADER_BYTES: usize = 4096;
const BANK_BYTES: usize = 4096;
const SLOT_BYTES: usize = BANK_BYTES * 2;
const BANK_PREFIX: usize = 8 + 8 + 4 + 32;
const BANK_TRAILER: usize = 8;
const MAX_TEXT: usize = 512;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Header {
    schema: u32,
    plan_id: String,
    snapshot: String,
    targets_total: u64,
    source_bytes_total: u64,
    source_slots: u32,
    completion_slots: u32,
    slot_count: u32,
    #[serde(default)]
    active_run_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind")]
enum SlotValue {
    Source(SourceValue),
    Completion(CompletionValue),
    Assembly(AssemblyValue),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SourceValue {
    plan_id: String,
    #[serde(default)]
    run_id: Option<String>,
    target_index: u32,
    attempt_started_at_micros: u64,
    updated_at_micros: u64,
    heartbeat_at_micros: u64,
    phase_started_at_micros: u64,
    row: MirrorTargetProgress,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CompletionValue {
    plan_id: String,
    target_index: u32,
    source_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AssemblyValue {
    pub(crate) plan_id: String,
    #[serde(default)]
    pub(crate) run_id: Option<String>,
    pub(crate) phase: String,
    pub(crate) input_bytes: u64,
    pub(crate) input_bytes_total: u64,
    pub(crate) output_bytes: u64,
    pub(crate) records: u64,
    pub(crate) current_entity_id: u64,
    pub(crate) bytes_per_second: u64,
    pub(crate) started_at_micros: u64,
    pub(crate) updated_at_micros: u64,
    pub(crate) cpu_user_micros: u64,
    pub(crate) cpu_system_micros: u64,
    pub(crate) peak_rss_bytes: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SourceWriter {
    path: PathBuf,
    plan_id: String,
    slot: u32,
    target_index: u32,
    run_id: Option<String>,
}

pub(crate) fn path(root: &Path) -> PathBuf {
    root.join("progress.bin")
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn source_slot_count(plan: &DirectBuildPlan) -> usize {
    plan.content_groups.iter().map(Vec::len).sum::<usize>() + plan.history_files.len()
}

fn header_for(plan: &DirectBuildPlan) -> Result<Header, String> {
    let source_slots = source_slot_count(plan);
    let completion_slots = plan.target_count();
    let slot_count = source_slots
        .checked_add(completion_slots)
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| "progress slot count overflow".to_owned())?;
    Ok(Header {
        schema: SCHEMA,
        plan_id: plan.plan_id.clone(),
        snapshot: plan.content_snapshot.clone(),
        targets_total: plan.target_count() as u64,
        source_bytes_total: plan.source_bytes(),
        source_slots: u32::try_from(source_slots)
            .map_err(|_| "too many progress source slots".to_owned())?,
        completion_slots: u32::try_from(completion_slots)
            .map_err(|_| "too many progress completion slots".to_owned())?,
        slot_count: u32::try_from(slot_count)
            .map_err(|_| "too many progress slots".to_owned())?,
        active_run_id: None,
    })
}

fn encode_header(header: &Header) -> Result<[u8; HEADER_BYTES], String> {
    let payload =
        serde_json::to_vec(header).map_err(|_| "cannot encode progress header".to_owned())?;
    if payload.len() > HEADER_BYTES - 8 - 4 - 32 {
        return Err("progress header exceeds fixed bound".into());
    }
    let mut bytes = [0_u8; HEADER_BYTES];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes[12..44].copy_from_slice(&Sha256::digest(&payload));
    bytes[44..44 + payload.len()].copy_from_slice(&payload);
    Ok(bytes)
}

fn decode_header(bytes: &[u8]) -> Result<Header, String> {
    if bytes.len() < HEADER_BYTES || &bytes[..8] != MAGIC {
        return Err("invalid progress header".into());
    }
    let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if length > HEADER_BYTES - 44 {
        return Err("invalid progress header length".into());
    }
    let payload = &bytes[44..44 + length];
    if bytes[12..44] != Sha256::digest(payload)[..] {
        return Err("progress header checksum mismatch".into());
    }
    let header: Header =
        serde_json::from_slice(payload).map_err(|_| "invalid progress header payload")?;
    if header.schema != SCHEMA
        || header.slot_count
            != header
                .source_slots
                .saturating_add(header.completion_slots)
                .saturating_add(1)
    {
        return Err("unsupported progress header".into());
    }
    Ok(header)
}

pub(crate) fn initialize(root: &Path, plan: &DirectBuildPlan) -> Result<(), String> {
    let header = header_for(plan)?;
    let destination = path(root);
    if let Ok(mut existing) = File::open(&destination) {
        let mut bytes = vec![0_u8; HEADER_BYTES];
        if existing.read_exact(&mut bytes).is_ok()
            && decode_header(&bytes).is_ok_and(|value| {
                value.plan_id == header.plan_id && value.slot_count == header.slot_count
            })
        {
            return Ok(());
        }
    }
    let bytes = encode_header(&header)?;
    let temporary = root.join(format!(".progress.{}.tmp", std::process::id()));
    let mut output = File::create(&temporary).map_err(|error| error.to_string())?;
    output
        .set_len(
            (HEADER_BYTES as u64)
                .saturating_add(u64::from(header.slot_count).saturating_mul(SLOT_BYTES as u64)),
        )
        .map_err(|error| error.to_string())?;
    output.write_all(&bytes).map_err(|error| error.to_string())?;
    output.sync_all().map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &destination).map_err(|error| error.to_string())?;
    if let Ok(directory) = File::open(root) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn replace_header(path: &Path, mut update: impl FnMut(&mut Header)) -> Result<(), String> {
    let mut bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let mut header = decode_header(&bytes)?;
    let expected_len = HEADER_BYTES
        .checked_add(header.slot_count as usize * SLOT_BYTES)
        .ok_or_else(|| "progress file length overflow".to_owned())?;
    if bytes.len() != expected_len {
        return Err("truncated progress file".into());
    }
    update(&mut header);
    bytes[..HEADER_BYTES].copy_from_slice(&encode_header(&header)?);
    let parent = path
        .parent()
        .ok_or_else(|| "progress file has no parent".to_owned())?;
    let temporary = parent.join(format!(".progress-run.{}.tmp", std::process::id()));
    let mut output = File::create(&temporary).map_err(|error| error.to_string())?;
    output.write_all(&bytes).map_err(|error| error.to_string())?;
    output.sync_all().map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

pub(crate) fn begin_run(root: &Path, plan: &DirectBuildPlan, run_id: &str) -> Result<(), String> {
    if run_id
        .parse::<i64>()
        .ok()
        .filter(|run_id| *run_id > 0)
        .is_none()
    {
        return Err("progress RunId must be a positive engine identity".into());
    }
    initialize(root, plan)?;
    replace_header(&path(root), |header| {
        if header.plan_id == plan.plan_id {
            header.active_run_id = Some(run_id.to_owned());
        }
    })?;
    let mut file = File::open(path(root)).map_err(|error| error.to_string())?;
    let mut bytes = vec![0_u8; HEADER_BYTES];
    file.read_exact(&mut bytes).map_err(|error| error.to_string())?;
    let header = decode_header(&bytes)?;
    if header.plan_id != plan.plan_id || header.active_run_id.as_deref() != Some(run_id) {
        return Err("progress run ownership was not committed".into());
    }
    Ok(())
}

fn source_position(
    plan: &DirectBuildPlan,
    target: &str,
    part: &str,
) -> Option<(u32, u32)> {
    let mut slot = 0_usize;
    for (target_index, group) in plan.content_groups.iter().enumerate() {
        for (source_index, source) in group.iter().enumerate() {
            let visible = if group.len() == 1 {
                format!("content-{target_index:06}")
            } else {
                format!("content-{target_index:06}-source-{source_index:06}")
            };
            if visible == target && source.filename == part {
                return Some((slot as u32, target_index as u32));
            }
            slot += 1;
        }
    }
    let content_targets = plan.content_target_count();
    for (history_index, source) in plan.history_files.iter().enumerate() {
        if target == format!("history-{history_index:06}") && source.part.filename == part {
            return Some((slot as u32, (content_targets + history_index) as u32));
        }
        slot += 1;
    }
    None
}

pub(crate) fn source_writer(
    root: &Path,
    target: &str,
    part: &str,
) -> Result<SourceWriter, String> {
    let plan = crate::direct::read_direct_build_plan(&root.join("plan.json"))
        .map_err(|error| error.to_string())?;
    let (slot, target_index) = source_position(&plan, target, part)
        .ok_or_else(|| "source is outside progress plan".to_owned())?;
    let projection = path(root);
    let mut file = File::open(&projection).map_err(|error| error.to_string())?;
    let mut bytes = vec![0_u8; HEADER_BYTES];
    file.read_exact(&mut bytes).map_err(|error| error.to_string())?;
    let header = decode_header(&bytes)?;
    if header.plan_id != plan.plan_id || slot >= header.source_slots {
        return Err("foreign progress plan".into());
    }
    Ok(SourceWriter {
        path: projection,
        plan_id: plan.plan_id,
        slot,
        target_index,
        run_id: header.active_run_id,
    })
}

pub(crate) fn current_run_id(root: &Path, expected_plan_id: &str) -> Option<String> {
    let mut file = File::open(path(root)).ok()?;
    let mut bytes = vec![0_u8; HEADER_BYTES];
    file.read_exact(&mut bytes).ok()?;
    let header = decode_header(&bytes).ok()?;
    (header.plan_id == expected_plan_id)
        .then_some(header.active_run_id)
        .flatten()
}

fn clip(value: &str) -> String {
    value.chars().take(MAX_TEXT).collect()
}

impl SourceWriter {
    pub(crate) fn write(&self, value: &LiveTargetProgress) {
        let observed = now_micros();
        let elapsed = observed.saturating_sub(value.started_at_micros);
        let quiet = observed.saturating_sub(value.updated_at_micros) / 1_000_000;
        let heartbeat = if value.heartbeat_at_micros == 0 {
            u64::MAX
        } else {
            observed.saturating_sub(value.heartbeat_at_micros) / 1_000_000
        };
        let phase_seconds = if value.phase_started_at_micros == 0 {
            0
        } else {
            observed.saturating_sub(value.phase_started_at_micros) / 1_000_000
        };
        let incoming = SourceValue {
            plan_id: self.plan_id.clone(),
            run_id: self.run_id.clone(),
            target_index: self.target_index,
            attempt_started_at_micros: value.started_at_micros,
            updated_at_micros: value.updated_at_micros,
            heartbeat_at_micros: value.heartbeat_at_micros,
            phase_started_at_micros: value.phase_started_at_micros,
            row: MirrorTargetProgress {
                target: value.target.clone(),
                kind: if value.target.starts_with("history-") {
                    "history".into()
                } else {
                    "content".into()
                },
                phase: clip(&value.phase),
                source: clip(&value.part),
                source_bytes_read: value.source_bytes_read,
                source_bytes_total: value.source_bytes_total,
                decoded_bytes: value.decoded_bytes,
                bytes_per_second: if elapsed == 0 {
                    0
                } else {
                    value.source_bytes_read.saturating_mul(1_000_000) / elapsed
                },
                pages: value.pages,
                records: value.revisions,
                text_bytes: value.text_bytes,
                current_page: value.current_page,
                current_title: clip(&value.current_title),
                quiet_seconds: quiet,
                heartbeat_seconds: heartbeat,
                phase_seconds,
                fetch_attempts: value.fetch_attempts,
                fetch_bytes_received: value.fetch_bytes_received,
                fetch_rate_limit_responses: value.fetch_rate_limit_responses,
                fetch_client_error_responses: value.fetch_client_error_responses,
                fetch_server_error_responses: value.fetch_server_error_responses,
                fetch_transport_errors: value.fetch_transport_errors,
                cpu_user_micros: value.cpu_user_micros,
                cpu_system_micros: value.cpu_system_micros,
                peak_rss_bytes: value.peak_rss_bytes,
            },
        };
        let _ = update_slot(
            &self.path,
            &self.plan_id,
            self.run_id.as_deref(),
            self.slot,
            |previous| {
                let previous = match previous {
                    Some(SlotValue::Source(value)) => Some(value),
                    _ => None,
                };
                SlotValue::Source(merge_source(previous, incoming))
            },
        );
    }
}

fn merge_source(previous: Option<SourceValue>, mut incoming: SourceValue) -> SourceValue {
    let Some(previous) = previous.filter(|value| value.plan_id == incoming.plan_id) else {
        return incoming;
    };
    let new_attempt = previous.attempt_started_at_micros != incoming.attempt_started_at_micros;
    let merge_network = |old: u64, new: u64| {
        if new_attempt {
            old.saturating_add(new)
        } else {
            old.max(new)
        }
    };
    incoming.row.source_bytes_read = previous
        .row
        .source_bytes_read
        .max(incoming.row.source_bytes_read);
    incoming.row.pages = previous.row.pages.max(incoming.row.pages);
    incoming.row.records = previous.row.records.max(incoming.row.records);
    incoming.row.text_bytes = previous.row.text_bytes.max(incoming.row.text_bytes);
    incoming.row.decoded_bytes = previous.row.decoded_bytes.max(incoming.row.decoded_bytes);
    incoming.row.fetch_attempts =
        merge_network(previous.row.fetch_attempts, incoming.row.fetch_attempts);
    incoming.row.fetch_bytes_received = merge_network(
        previous.row.fetch_bytes_received,
        incoming.row.fetch_bytes_received,
    );
    incoming.row.fetch_rate_limit_responses = merge_network(
        previous.row.fetch_rate_limit_responses,
        incoming.row.fetch_rate_limit_responses,
    );
    incoming.row.fetch_client_error_responses = merge_network(
        previous.row.fetch_client_error_responses,
        incoming.row.fetch_client_error_responses,
    );
    incoming.row.fetch_server_error_responses = merge_network(
        previous.row.fetch_server_error_responses,
        incoming.row.fetch_server_error_responses,
    );
    incoming.row.fetch_transport_errors = merge_network(
        previous.row.fetch_transport_errors,
        incoming.row.fetch_transport_errors,
    );
    incoming.row.cpu_user_micros =
        merge_network(previous.row.cpu_user_micros, incoming.row.cpu_user_micros);
    incoming.row.cpu_system_micros = merge_network(
        previous.row.cpu_system_micros,
        incoming.row.cpu_system_micros,
    );
    incoming.row.peak_rss_bytes = previous
        .row
        .peak_rss_bytes
        .max(incoming.row.peak_rss_bytes);
    incoming
}

fn target_ordinal(plan: &DirectBuildPlan, kind: &str, index: usize) -> Option<usize> {
    match kind {
        "content" if index < plan.content_target_count() => Some(index),
        "history" if index < plan.history_files.len() => {
            Some(plan.content_target_count() + index)
        }
        _ => None,
    }
}

pub(crate) fn mark_target_completed(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: &str,
    index: usize,
) {
    let Ok(header) = header_for(plan) else {
        return;
    };
    let Some(target_index) = target_ordinal(plan, kind, index) else {
        return;
    };
    let slot = header.source_slots.saturating_add(target_index as u32);
    let source_bytes = plan.target_source_bytes(kind, index);
    let value = CompletionValue {
        plan_id: plan.plan_id.clone(),
        target_index: target_index as u32,
        source_bytes,
    };
    let _ = update_slot(&path(root), &plan.plan_id, None, slot, |_| {
        SlotValue::Completion(value)
    });
}

pub(crate) fn write_assembly(root: &Path, value: AssemblyValue) {
    let Ok(mut file) = File::open(path(root)) else {
        return;
    };
    let mut bytes = vec![0_u8; HEADER_BYTES];
    if file.read_exact(&mut bytes).is_err() {
        return;
    }
    let Ok(header) = decode_header(&bytes) else {
        return;
    };
    if header.plan_id != value.plan_id || header.active_run_id != value.run_id {
        return;
    }
    let slot = header.slot_count - 1;
    let plan_id = value.plan_id.clone();
    let run_id = value.run_id.clone();
    let _ = update_slot(
        &path(root),
        &plan_id,
        run_id.as_deref(),
        slot,
        |_| SlotValue::Assembly(value),
    );
}

fn bank_offset(slot: u32, bank: usize) -> u64 {
    HEADER_BYTES as u64
        + u64::from(slot) * SLOT_BYTES as u64
        + (bank * BANK_BYTES) as u64
}

fn decode_bank(bytes: &[u8]) -> Option<(u64, SlotValue)> {
    if bytes.len() != BANK_BYTES || &bytes[..8] != BANK_MAGIC {
        return None;
    }
    let sequence = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let length = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;
    if length > BANK_BYTES - BANK_PREFIX - BANK_TRAILER {
        return None;
    }
    let trailer =
        u64::from_le_bytes(bytes[BANK_BYTES - 8..].try_into().ok()?);
    if trailer != sequence {
        return None;
    }
    let payload = &bytes[BANK_PREFIX..BANK_PREFIX + length];
    if bytes[20..52] != Sha256::digest(payload)[..] {
        return None;
    }
    serde_json::from_slice(payload).ok().map(|value| (sequence, value))
}

fn encode_bank(sequence: u64, value: &SlotValue) -> Result<[u8; BANK_BYTES], String> {
    let payload =
        serde_json::to_vec(value).map_err(|_| "cannot encode progress slot".to_owned())?;
    if payload.len() > BANK_BYTES - BANK_PREFIX - BANK_TRAILER {
        return Err("progress slot exceeds fixed bound".into());
    }
    let mut bytes = [0_u8; BANK_BYTES];
    bytes[..8].copy_from_slice(BANK_MAGIC);
    bytes[8..16].copy_from_slice(&sequence.to_le_bytes());
    bytes[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes[20..52].copy_from_slice(&Sha256::digest(&payload));
    bytes[BANK_PREFIX..BANK_PREFIX + payload.len()].copy_from_slice(&payload);
    bytes[BANK_BYTES - 8..].copy_from_slice(&sequence.to_le_bytes());
    Ok(bytes)
}

fn update_slot(
    path: &Path,
    expected_plan_id: &str,
    expected_run_id: Option<&str>,
    slot: u32,
    update: impl FnOnce(Option<SlotValue>) -> SlotValue,
) -> Result<(), String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    let mut header_bytes = vec![0_u8; HEADER_BYTES];
    file.read_exact_at(&mut header_bytes, 0)
        .map_err(|error| error.to_string())?;
    let header = decode_header(&header_bytes)?;
    if header.plan_id != expected_plan_id || slot >= header.slot_count {
        return Err("foreign progress plan".into());
    }
    if expected_run_id.is_some()
        && header.active_run_id.as_deref() != expected_run_id
    {
        return Err("stale progress run".into());
    }
    let mut banks = [[0_u8; BANK_BYTES]; 2];
    for (index, bank) in banks.iter_mut().enumerate() {
        file.read_exact_at(bank, bank_offset(slot, index))
            .map_err(|error| error.to_string())?;
    }
    let decoded = [decode_bank(&banks[0]), decode_bank(&banks[1])];
    let current = decoded
        .iter()
        .flatten()
        .max_by_key(|(sequence, _)| *sequence);
    let sequence = current
        .map_or(1, |(sequence, _)| sequence.saturating_add(1));
    let next_bank = match (&decoded[0], &decoded[1]) {
        (Some((left, _)), Some((right, _))) => usize::from(left > right),
        (Some(_), None) => 1,
        _ => 0,
    };
    let value = update(current.map(|(_, value)| value.clone()));
    let bytes = encode_bank(sequence, &value)?;
    file.write_all_at(&bytes, bank_offset(slot, next_bank))
        .map_err(|error| error.to_string())
}

fn read_projection(
    path: &Path,
    expected_plan_id: Option<&str>,
    expected_run_id: Option<&str>,
) -> Result<Option<MirrorBuildProgress>, String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| error.to_string())?;
    let header = decode_header(&bytes)?;
    if expected_plan_id.is_some_and(|expected| expected != header.plan_id) {
        return Err("foreign progress plan".into());
    }
    let expected_len = HEADER_BYTES
        .checked_add(header.slot_count as usize * SLOT_BYTES)
        .ok_or_else(|| "progress file length overflow".to_owned())?;
    if bytes.len() != expected_len {
        return Err("truncated progress file".into());
    }
    let mut values = Vec::with_capacity(header.slot_count as usize);
    for slot in 0..header.slot_count {
        let offset = HEADER_BYTES + slot as usize * SLOT_BYTES;
        let banks = [
            decode_bank(&bytes[offset..offset + BANK_BYTES]),
            decode_bank(&bytes[offset + BANK_BYTES..offset + SLOT_BYTES]),
        ];
        let value = banks
            .into_iter()
            .flatten()
            .max_by_key(|(sequence, _)| *sequence)
            .map(|(_, value)| value)
            .filter(|value| match value {
                SlotValue::Source(value) => value.plan_id == header.plan_id,
                SlotValue::Completion(value) => value.plan_id == header.plan_id,
                SlotValue::Assembly(value) => value.plan_id == header.plan_id,
            });
        values.push(value);
    }
    Ok(Some(project(header, values, expected_run_id)))
}

fn project(
    header: Header,
    values: Vec<Option<SlotValue>>,
    expected_run_id: Option<&str>,
) -> MirrorBuildProgress {
    let observed = now_micros();
    let mut completed = vec![None; header.completion_slots as usize];
    for value in values.iter().flatten() {
        if let SlotValue::Completion(value) = value {
            if let Some(slot) = completed.get_mut(value.target_index as usize) {
                *slot = Some(value.source_bytes);
            }
        }
    }
    let targets_completed = completed.iter().filter(|value| value.is_some()).count() as u64;
    let completed_bytes = completed.iter().flatten().copied().sum::<u64>();
    let run_matches = |run_id: Option<&str>| {
        run_id == expected_run_id && header.active_run_id.as_deref() == expected_run_id
    };
    let assembly = values.iter().flatten().find_map(|value| match value {
        SlotValue::Assembly(value) if run_matches(value.run_id.as_deref()) => Some(value),
        _ => None,
    });
    let mut rows = Vec::new();
    let mut active = Vec::new();
    let mut source_in_progress = 0_u64;
    let mut fetch_attempts = 0_u64;
    let mut fetch_bytes_received = 0_u64;
    let mut fetch_rate_limit_responses = 0_u64;
    let mut fetch_client_error_responses = 0_u64;
    let mut fetch_server_error_responses = 0_u64;
    let mut fetch_transport_errors = 0_u64;
    for value in values.iter().flatten() {
        let SlotValue::Source(value) = value else {
            continue;
        };
        fetch_attempts = fetch_attempts.saturating_add(value.row.fetch_attempts);
        fetch_bytes_received =
            fetch_bytes_received.saturating_add(value.row.fetch_bytes_received);
        fetch_rate_limit_responses = fetch_rate_limit_responses
            .saturating_add(value.row.fetch_rate_limit_responses);
        fetch_client_error_responses = fetch_client_error_responses
            .saturating_add(value.row.fetch_client_error_responses);
        fetch_server_error_responses = fetch_server_error_responses
            .saturating_add(value.row.fetch_server_error_responses);
        fetch_transport_errors =
            fetch_transport_errors.saturating_add(value.row.fetch_transport_errors);
        if !run_matches(value.run_id.as_deref()) {
            continue;
        }
        if completed
            .get(value.target_index as usize)
            .is_some_and(Option::is_some)
        {
            continue;
        }
        let mut row = value.row.clone();
        row.quiet_seconds = observed.saturating_sub(value.updated_at_micros) / 1_000_000;
        row.heartbeat_seconds = if value.heartbeat_at_micros == 0 {
            u64::MAX
        } else {
            observed.saturating_sub(value.heartbeat_at_micros) / 1_000_000
        };
        row.phase_seconds = if value.phase_started_at_micros == 0 {
            0
        } else {
            observed.saturating_sub(value.phase_started_at_micros) / 1_000_000
        };
        source_in_progress = source_in_progress.saturating_add(row.source_bytes_read);
        active.push(format!(
            "{} · {} · {} / {} source",
            row.target, row.phase, row.source_bytes_read, row.source_bytes_total
        ));
        rows.push(row);
    }
    let (phase, target_progress, targets_active, active_rate, active_quiet) =
        if let Some(value) = assembly {
            let quiet = observed.saturating_sub(value.updated_at_micros) / 1_000_000;
            let row = MirrorTargetProgress {
                target: "assembly".into(),
                kind: "assembly".into(),
                phase: value.phase.clone(),
                source: format!("{} typed targets", header.targets_total),
                source_bytes_read: value.input_bytes,
                source_bytes_total: value.input_bytes_total,
                decoded_bytes: value.output_bytes,
                bytes_per_second: value.bytes_per_second,
                pages: value.current_entity_id,
                records: value.records,
                current_page: value.current_entity_id,
                quiet_seconds: quiet,
                heartbeat_seconds: quiet,
                phase_seconds: observed.saturating_sub(value.started_at_micros) / 1_000_000,
                cpu_user_micros: value.cpu_user_micros,
                cpu_system_micros: value.cpu_system_micros,
                peak_rss_bytes: value.peak_rss_bytes,
                ..Default::default()
            };
            (
                format!("assembling · {}", value.phase),
                vec![row],
                vec![format!("assembly · {}", value.phase)],
                Some(value.bytes_per_second),
                Some(quiet),
            )
        } else {
            rows.sort_by(|left, right| left.target.cmp(&right.target));
            active.sort();
            let rate = rows
                .iter()
                .filter(|row| row.phase != "finished")
                .map(|row| row.bytes_per_second)
                .sum();
            let quiet = rows
                .iter()
                .filter(|row| row.phase != "finished")
                .map(|row| row.quiet_seconds)
                .max();
            (
                if targets_completed == header.targets_total {
                    "source targets complete".into()
                } else {
                    "materializing source targets".into()
                },
                rows,
                active,
                quiet.map(|_| rate),
                quiet,
            )
        };
    MirrorBuildProgress {
        phase,
        targets_total: header.targets_total,
        targets_completed,
        target_progress,
        targets_active,
        source_bytes_total: header.source_bytes_total,
        source_bytes_completed: completed_bytes.saturating_add(source_in_progress),
        active_source_bytes_per_second: active_rate,
        active_quiet_seconds: active_quiet,
        fetch_attempts,
        fetch_bytes_received,
        fetch_rate_limit_responses,
        fetch_client_error_responses,
        fetch_server_error_responses,
        fetch_transport_errors,
        snapshot: header.snapshot,
    }
}

pub fn mirror_build_progress(archive: impl AsRef<Path>) -> Option<MirrorBuildProgress> {
    let root = crate::cli::mirror_scratch_path(archive.as_ref());
    read_projection(&path(&root), None, None).ok().flatten()
}

pub fn mirror_build_progress_for_run(
    archive: impl AsRef<Path>,
    run_id: &str,
) -> Option<MirrorBuildProgress> {
    let root = crate::cli::mirror_scratch_path(archive.as_ref());
    read_projection(&path(&root), None, Some(run_id))
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct::{canonical_direct_plan_id, PlannedHistoryFile, PlannedPart};

    fn plan() -> DirectBuildPlan {
        let part = |name: &str| PlannedPart {
            url: format!("https://example.invalid/{name}"),
            filename: name.into(),
            size_bytes: 100,
            sha256: None,
            sha1: None,
            md5: None,
        };
        let mut plan = DirectBuildPlan {
            schema: 1,
            plan_id: String::new(),
            wiki_db: "testwiki".into(),
            content_snapshot: "2026-08-01".into(),
            metadata_snapshot: "2026-08".into(),
            observed_at_micros: 0,
            frame_target: 1,
            range_target: 1,
            compression_level: 1,
            ref_prefix_sample_bytes: 2,
            ref_prefix_bytes: 1,
            content_groups: vec![vec![part("a.xml"), part("b.xml")]],
            history_files: vec![PlannedHistoryFile {
                partition: "p".into(),
                part: part("h.tsv"),
            }],
        };
        plan.plan_id = canonical_direct_plan_id(&plan).unwrap();
        plan
    }

    fn write_plan(root: &Path, plan: &DirectBuildPlan) {
        std::fs::write(root.join("plan.json"), serde_json::to_vec(plan).unwrap()).unwrap();
        initialize(root, plan).unwrap();
    }

    fn live(target: &str, part: &str, attempt: u64, bytes: u64) -> LiveTargetProgress {
        LiveTargetProgress {
            target: target.into(),
            part: part.into(),
            phase: "parsing".into(),
            source_bytes_read: bytes,
            source_bytes_total: 100,
            started_at_micros: attempt,
            updated_at_micros: now_micros(),
            heartbeat_at_micros: now_micros(),
            fetch_attempts: 1,
            fetch_bytes_received: bytes,
            ..Default::default()
        }
    }

    #[test]
    fn concurrent_distinct_slot_writers_preserve_both() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        write_plan(root.path(), &plan);
        let left = source_writer(root.path(), "content-000000-source-000000", "a.xml").unwrap();
        let right = source_writer(root.path(), "content-000000-source-000001", "b.xml").unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| left.write(&live("content-000000-source-000000", "a.xml", 1, 40)));
            scope.spawn(|| right.write(&live("content-000000-source-000001", "b.xml", 2, 60)));
        });
        let progress = read_projection(&path(root.path()), Some(&plan.plan_id), None)
            .unwrap()
            .unwrap();
        assert_eq!(progress.target_progress.len(), 2);
        assert_eq!(progress.source_bytes_completed, 100);
    }

    #[test]
    fn corrupt_bank_does_not_poison_other_slots() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        write_plan(root.path(), &plan);
        let left = source_writer(root.path(), "content-000000-source-000000", "a.xml").unwrap();
        let right = source_writer(root.path(), "content-000000-source-000001", "b.xml").unwrap();
        left.write(&live("content-000000-source-000000", "a.xml", 1, 40));
        right.write(&live("content-000000-source-000001", "b.xml", 2, 60));
        let file = OpenOptions::new().write(true).open(path(root.path())).unwrap();
        file.write_all_at(b"torn", bank_offset(0, 0)).unwrap();
        let progress = read_projection(&path(root.path()), Some(&plan.plan_id), None)
            .unwrap()
            .unwrap();
        assert!(progress.target_progress.iter().any(|row| row.source == "b.xml"));
    }

    #[test]
    fn foreign_plan_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        write_plan(root.path(), &plan);
        assert!(read_projection(&path(root.path()), Some("foreign-plan"), None).is_err());
    }

    #[test]
    fn retry_network_counters_and_logical_bytes_are_monotonic() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        write_plan(root.path(), &plan);
        let writer =
            source_writer(root.path(), "content-000000-source-000000", "a.xml").unwrap();
        writer.write(&live("content-000000-source-000000", "a.xml", 1, 80));
        writer.write(&live("content-000000-source-000000", "a.xml", 2, 20));
        let progress = read_projection(&path(root.path()), Some(&plan.plan_id), None)
            .unwrap()
            .unwrap();
        assert_eq!(progress.source_bytes_completed, 80);
        assert_eq!(progress.fetch_attempts, 2);
        assert_eq!(progress.fetch_bytes_received, 100);
    }

    #[test]
    fn completed_target_total_survives_resume_and_late_retry_telemetry() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        write_plan(root.path(), &plan);
        mark_target_completed(root.path(), &plan, "content", 0);
        initialize(root.path(), &plan).unwrap();
        let writer =
            source_writer(root.path(), "content-000000-source-000000", "a.xml").unwrap();
        writer.write(&live("content-000000-source-000000", "a.xml", 9, 1));
        let progress = read_projection(&path(root.path()), Some(&plan.plan_id), None)
            .unwrap()
            .unwrap();
        assert_eq!(progress.targets_completed, 1);
        assert_eq!(progress.source_bytes_completed, 200);
        assert!(progress.target_progress.iter().all(|row| row.kind != "content"));
    }

    #[test]
    fn new_resume_run_rejects_old_parsing_and_assembly_as_active() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        write_plan(root.path(), &plan);
        begin_run(root.path(), &plan, "1").unwrap();
        let old =
            source_writer(root.path(), "content-000000-source-000000", "a.xml").unwrap();
        old.write(&live("content-000000-source-000000", "a.xml", 1, 40));
        let old_assembly = AssemblyValue {
            plan_id: plan.plan_id.clone(),
            run_id: Some("1".into()),
            phase: "old assembly".into(),
            input_bytes: 1,
            input_bytes_total: 2,
            output_bytes: 0,
            records: 0,
            current_entity_id: 0,
            bytes_per_second: 0,
            started_at_micros: now_micros(),
            updated_at_micros: now_micros(),
            cpu_user_micros: 0,
            cpu_system_micros: 0,
            peak_rss_bytes: 0,
        };
        write_assembly(root.path(), old_assembly.clone());
        assert_eq!(
            read_projection(&path(root.path()), Some(&plan.plan_id), Some("1"))
                .unwrap()
                .unwrap()
                .target_progress
                .len(),
            1,
            "current run prefers its assembly row",
        );

        begin_run(root.path(), &plan, "2").unwrap();
        old.write(&live("content-000000-source-000000", "a.xml", 1, 80));
        write_assembly(root.path(), old_assembly);
        let resumed =
            read_projection(&path(root.path()), Some(&plan.plan_id), Some("2"))
                .unwrap()
                .unwrap();
        assert!(resumed.target_progress.is_empty());
        assert!(resumed.targets_active.is_empty());
        assert_eq!(resumed.fetch_attempts, 1);
        assert_eq!(resumed.fetch_bytes_received, 40);
    }

    #[test]
    fn assembly_keeps_structured_cpu_rss_and_current_item() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        write_plan(root.path(), &plan);
        write_assembly(
            root.path(),
            AssemblyValue {
                plan_id: plan.plan_id.clone(),
                run_id: None,
                phase: "replaying bootstrap 50/100 records".into(),
                input_bytes: 300,
                input_bytes_total: 1000,
                output_bytes: 200,
                records: 50,
                current_entity_id: 42,
                bytes_per_second: 70,
                started_at_micros: now_micros().saturating_sub(1_000_000),
                updated_at_micros: now_micros(),
                cpu_user_micros: 700_000,
                cpu_system_micros: 100_000,
                peak_rss_bytes: 1234,
            },
        );
        let progress = read_projection(&path(root.path()), Some(&plan.plan_id), None)
            .unwrap()
            .unwrap();
        assert!(progress.phase.starts_with("assembling · replaying bootstrap"));
        assert_eq!(progress.target_progress.len(), 1);
        let row = &progress.target_progress[0];
        assert_eq!(row.current_page, 42);
        assert_eq!(row.records, 50);
        assert_eq!(row.cpu_user_micros, 700_000);
        assert_eq!(row.peak_rss_bytes, 1234);
    }

    #[test]
    fn refresh_reads_projection_without_plan_or_build_directories() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        write_plan(root.path(), &plan);
        let writer =
            source_writer(root.path(), "content-000000-source-000000", "a.xml").unwrap();
        writer.write(&live("content-000000-source-000000", "a.xml", 1, 40));
        std::fs::remove_file(root.path().join("plan.json")).unwrap();
        let progress = read_projection(&path(root.path()), Some(&plan.plan_id), None)
            .unwrap()
            .unwrap();
        assert_eq!(progress.source_bytes_completed, 40);
    }

    #[test]
    fn writer_rejects_projection_owned_by_another_plan() {
        let root = tempfile::tempdir().unwrap();
        let first = plan();
        write_plan(root.path(), &first);
        let mut second = first.clone();
        second.content_snapshot = "2026-09-01".into();
        second.plan_id = canonical_direct_plan_id(&second).unwrap();
        std::fs::write(
            root.path().join("plan.json"),
            serde_json::to_vec(&second).unwrap(),
        )
        .unwrap();
        assert!(
            source_writer(root.path(), "content-000000-source-000000", "a.xml").is_err()
        );
    }
}
