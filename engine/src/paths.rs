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
