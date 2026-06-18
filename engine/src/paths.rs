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

pub fn mnt_point() -> PathBuf {
    runtime_home().join("mnt")
}

pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [data_home(), runtime_home(), state_home(), live_home(), mnt_point()] {
        std::fs::create_dir_all(&d)?;
    }
    Ok(())
}
