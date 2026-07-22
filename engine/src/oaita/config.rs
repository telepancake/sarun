// `oaita.toml` — the API credentials file. Lives at
// `{config_home}/oaita.toml` (XDG_CONFIG_HOME-respecting), separate from the
// rest of sarun's settings so a user can `chmod 0600` it without dragging
// other config along.
//
// Format (TOML):
//   model     = "llama3.1:8b"           # required
//   base_url  = "http://127.0.0.1:8080/v1"   # optional, defaults to OpenAI
//   api_key   = "sk-..."                # may be empty for local endpoints
//
// No env-var fallbacks: the only way to set these is the toml. Inside an
// `--api` box the engine FUSE-shadows this path with a safe variant
// (model name copied, base_url pointed at the in-engine proxy, api_key
// stripped) — the box never sees the host's real upstream credentials.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Config {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

impl Config {
    pub fn load() -> Self {
        Self::load_from(&crate::paths::oaita_config_path())
    }

    pub fn load_from(path: &PathBuf) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("oaita: ignoring malformed {}: {e}", path.display());
            Self::default()
        })
    }

    /// Resolve (model, base_url, api_key) from the toml. Errors only if
    /// `model` is unset (the only required field).
    pub fn resolve(&self) -> Result<(String, String, String), String> {
        let model = self
            .model
            .clone()
            .ok_or_else(|| "no model set — put model = \"…\" in oaita.toml".to_string())?;
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let api_key = self.api_key.clone().unwrap_or_default();
        Ok((model, base_url, api_key))
    }
}
