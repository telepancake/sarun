//! The model catalog behind the UI's local-model picker. Its contents do
//! NOT come from hardcoded recommendations (those go stale the day they're
//! written): the live source is a HuggingFace query for currently-popular
//! GGUF instruct models, resolved to a concrete Q4 file each. A user config
//! file overrides/augments it, and a small built-in list is the offline
//! fallback — explicitly labelled as a possibly-outdated snapshot so it
//! never masquerades as current.

use serde_json::Value;

/// One pickable model: a ready-to-download Q4 GGUF with its size.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub name: String,
    pub url: String,
    pub note: String,
}

const HF_LIST: &str = "https://huggingface.co/api/models\
    ?filter=gguf&pipeline_tag=text-generation\
    &sort=downloads&direction=-1&limit=60";

/// Repo id looks like an instruct/chat model (vs a base, embedding, vision…).
fn instruct_like(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    let bad = [
        "embed", "rerank", "vision", "-vl", "audio", "ocr", "tts", "whisper", "base", "reward",
    ];
    if bad.iter().any(|b| l.contains(b)) {
        return false;
    }
    [
        "instruct", "-it", "-it-", "chat", "thinking", "coder", "-r1", "reason",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// The catalog + a one-line note on where it came from. Order: user config,
/// then the live HF list, then (only if both are empty) the offline fallback.
pub fn catalog() -> (Vec<ModelEntry>, String) {
    let mut out = Vec::new();
    let mut sources = Vec::new();

    let cfg = config_models();
    if !cfg.is_empty() {
        sources.push("config".to_string());
    }
    out.extend(cfg);

    match crate::oaita::client::block_on(fetch_hf()) {
        Ok(live) if !live.is_empty() => {
            sources.push(format!("HuggingFace ({} live)", live.len()));
            // de-dup by url against config entries
            for e in live {
                if !out.iter().any(|o| o.url == e.url) {
                    out.push(e);
                }
            }
        }
        _ => {}
    }

    if out.is_empty() {
        sources.push("offline snapshot (Jan 2026 — may be outdated)".into());
        out = fallback();
    }
    (out, sources.join(" · "))
}

/// `{config_home}/oaita-models.toml` — `[[model]] name / url / note`.
fn config_models() -> Vec<ModelEntry> {
    let p = crate::paths::config_home().join("oaita-models.toml");
    let Ok(text) = std::fs::read_to_string(&p) else {
        return vec![];
    };
    let Ok(v) = text.parse::<toml::Value>() else {
        return vec![];
    };
    v.get("model")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(ModelEntry {
                        name: m.get("name")?.as_str()?.to_string(),
                        url: m.get("url")?.as_str()?.to_string(),
                        note: m
                            .get("note")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn fetch_hf() -> anyhow::Result<Vec<ModelEntry>> {
    let client = reqwest::Client::builder()
        .user_agent("sarun-oaita")
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let list: Vec<Value> = client
        .get(HF_LIST)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut out = Vec::new();
    for m in &list {
        if out.len() >= 10 {
            break;
        }
        let Some(id) = m.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !instruct_like(id) {
            continue;
        }
        if let Some((file, size)) = resolve_q4(&client, id).await {
            out.push(ModelEntry {
                name: id.to_string(),
                url: format!("https://huggingface.co/{id}/resolve/main/{file}"),
                note: format!("{} · Q4 · {} GiB", short_id(id), size / (1 << 30)),
            });
        }
    }
    Ok(out)
}

/// Pick a single-file Q4 GGUF (prefer Q4_K_M) from a repo's file tree, with
/// its size. Skips multi-part shards (too big / awkward to serve).
async fn resolve_q4(client: &reqwest::Client, id: &str) -> Option<(String, u64)> {
    let url = format!("https://huggingface.co/api/models/{id}/tree/main");
    let tree: Vec<Value> = client
        .get(&url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    let path_of = |f: &Value| {
        f.get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    };
    let ggufs: Vec<&Value> = tree
        .iter()
        .filter(|f| path_of(f).to_ascii_lowercase().ends_with(".gguf"))
        .filter(|f| !path_of(f).contains("-of-")) // no shards
        .collect();
    let pick = ggufs
        .iter()
        .find(|f| path_of(f).to_ascii_lowercase().contains("q4_k_m"))
        .or_else(|| {
            ggufs
                .iter()
                .find(|f| path_of(f).to_ascii_lowercase().contains("q4"))
        })
        .copied()?;
    let path = pick.get("path").and_then(Value::as_str)?.to_string();
    let size = pick.get("size").and_then(Value::as_u64).unwrap_or(0);
    Some((path, size))
}

/// The short model name from a `owner/repo` id.
fn short_id(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// Offline fallback — a Jan-2026 snapshot, LABELLED as such (see catalog()).
/// Not a recommendation of "current best"; just something that resolves when
/// HuggingFace is unreachable.
fn fallback() -> Vec<ModelEntry> {
    let m = |repo: &str, note: &str| ModelEntry {
        name: repo.to_string(),
        url: format!(
            "https://huggingface.co/unsloth/{repo}-GGUF/\
                      resolve/main/{repo}-Q4_K_M.gguf"
        ),
        note: format!("Q4 · {note} · offline snapshot"),
    };
    vec![
        m("Qwen3-1.7B", "~1.0 GiB · light"),
        m("Qwen3-4B", "~2.4 GiB · solid local agent"),
        m("Qwen3-8B", "~4.7 GiB · stronger"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruct_filter_keeps_chat_drops_embeddings() {
        assert!(instruct_like("Qwen/Qwen3-4B-Instruct-GGUF"));
        assert!(instruct_like("unsloth/gemma-4-12B-it-GGUF"));
        assert!(instruct_like("x/DeepSeek-R1-Distill-Qwen-7B-GGUF"));
        assert!(!instruct_like("x/bge-m3-embed-GGUF"));
        assert!(!instruct_like("x/Qwen2-VL-7B-GGUF"));
        assert!(!instruct_like("x/Llama-3-8B-base-GGUF"));
    }

    #[test]
    fn fallback_is_labelled_and_resolves_to_gguf_urls() {
        let f = fallback();
        assert!(!f.is_empty());
        for e in &f {
            assert!(e.url.ends_with(".gguf"), "{}", e.url);
            assert!(e.note.contains("offline snapshot"));
        }
    }
}
