// `oaita.toml` — the API credentials file. Lives at
// `{config_home}/oaita.toml` (XDG_CONFIG_HOME-respecting), separate from the
// rest of sarun's settings so a user can `chmod 0600` it without dragging
// other config along.
//
// Format (TOML):
//   model     = "llama3.1:8b"           # default model
//   base_url  = "http://127.0.0.1:8080/v1"
//   api_key   = "sk-..."                # may be empty for local endpoints
//
// All fields optional. The runtime falls back to env vars in this order:
//   model     ←  $OAITA_MODEL
//   base_url  ←  $OPENAI_BASE_URL
//   api_key   ←  $OPENAI_API_KEY

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
        let Ok(text) = fs::read_to_string(path) else { return Self::default(); };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("oaita: ignoring malformed {}: {e}", path.display());
            Self::default()
        })
    }

    /// Resolve (model, base_url, api_key) from config + env. Errors only if
    /// `model` is unset in BOTH (the only required field).
    ///
    /// In `--api` boxes the safety property — that the box never sees the
    /// host's api_key or its real upstream URL — is enforced at the FUSE /
    /// bwrap layer (the engine substitutes a safe `oaita.toml` over the
    /// box's view of the host config path). So this resolver stays pure
    /// config+env: it does NOT special-case the proxy.
    pub fn resolve(&self) -> Result<(String, String, String), String> {
        let model = self.model.clone()
            .or_else(|| std::env::var("OAITA_MODEL").ok())
            .ok_or_else(|| "no model set — put model = \"…\" in oaita.toml \
                            or set $OAITA_MODEL".to_string())?;
        let base_url = self.base_url.clone()
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let api_key = self.api_key.clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .unwrap_or_default();
        Ok((model, base_url, api_key))
    }
}
