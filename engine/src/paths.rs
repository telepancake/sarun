// Instance path layout — mirrors the Python engine's rules exactly (XDG vars,
// $SLOPBOX_NS namespacing, same dirnames), so the two engines are drop-in
// replacements for each other behind the same socket path.

use std::env;
use std::path::PathBuf;

fn app_dir() -> String {
    match env::var("SLOPBOX_NS") {
        Ok(ns) if !ns.is_empty() => format!("slopbox.{ns}"),
        _ => "slopbox".into(),
    }
}

fn home(var: &str, fallback: &str) -> PathBuf {
    match env::var(var) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/root".into()))
            .join(fallback),
    }
}

pub fn data_home() -> PathBuf {
    home("XDG_DATA_HOME", ".local/share").join(app_dir())
}

pub fn config_home() -> PathBuf {
    home("XDG_CONFIG_HOME", ".config").join(app_dir())
}

/// User-edited list of host paths the engine should shadow with itself
/// when a -b box runs, redirecting them to the brush-sh shim. One glob
/// per line in the config file ({config_home}/shadow_sh.glob); blank
/// lines and lines starting with `#` are ignored. Missing file falls
/// back to the historical defaults ({/bin,/usr/bin}/{sh,bash,dash}).
pub fn shadow_sh_glob_path() -> PathBuf {
    config_home().join("shadow_sh.glob")
}

/// Same for the embedded-make redirect. Pattern file lives at
/// {config_home}/shadow_make.glob; default {/bin,/usr/bin}/{make,gmake}.
pub fn shadow_make_glob_path() -> PathBuf {
    config_home().join("shadow_make.glob")
}

/// Same for the embedded-ninja redirect; default {/bin,/usr/bin}/ninja.
pub fn shadow_ninja_glob_path() -> PathBuf {
    config_home().join("shadow_ninja.glob")
}

pub fn runtime_home() -> PathBuf {
    match env::var("XDG_RUNTIME_DIR") {
        Ok(v) if !v.is_empty() => PathBuf::from(v).join(app_dir()),
        _ => data_home(),
    }
}

pub fn state_home() -> PathBuf {
    home("XDG_STATE_HOME", ".local/state").join(app_dir())
}

pub fn live_home() -> PathBuf {
    state_home().join("live")
}

pub fn sock_path() -> PathBuf {
    runtime_home().join("ui.sock")
}

/// Per-engine API proxy socket. An `--api` box has THIS bind-mounted at
/// `/run/sarun/api.sock` inside, and oaita talks plain HTTP/1.1 over it; the
/// engine proxies the call upstream after injecting the auth header from
/// `oaita.toml` and logging the request/response into the box's `api_log`
/// sqlar table.
pub fn api_sock_path() -> PathBuf {
    runtime_home().join("api.sock")
}

/// Config file holding the API credentials (model, base_url, api_key). Lives
/// under XDG config home, separate from sarun's other settings so a user can
/// chmod 0600 it without dragging other config along.
pub fn oaita_config_path() -> PathBuf {
    config_home().join("oaita.toml")
}

/// Where oaita sessions live: one folder per session under here. Mirrors the
/// Python prototype's `$XDG_STATE_HOME/oaita/<name>/` layout but lives under
/// sarun's own state root so removing sarun's state removes oaita's too.
pub fn oaita_state_home() -> PathBuf {
    state_home().join("oaita")
}

/// Where the engine writes the SAFE-FOR-BOX oaita.toml. The FUSE overlay
/// substitutes this file's bytes/attrs over the box's view of the host
/// oaita.toml whenever the box was launched with `--api`. The substitution
/// keeps the api_key + real base_url off the box's filesystem entirely
/// (a `cat` of the path returns the safe content; the host bytes are
/// never reachable from the box).
pub fn api_box_oaita_toml_path() -> PathBuf {
    runtime_home().join("api-box-oaita.toml")
}

pub fn mnt_point() -> PathBuf {
    runtime_home().join("mnt")
}

pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [data_home(), config_home(), runtime_home(), state_home(),
              live_home(), mnt_point(), oaita_state_home()] {
        std::fs::create_dir_all(&d)?;
    }
    Ok(())
}
