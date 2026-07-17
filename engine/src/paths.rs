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

/// Config file holding the API credentials (model, base_url, api_key). Lives
/// under XDG config home, separate from sarun's other settings so a user can
/// chmod 0600 it without dragging other config along.
pub fn oaita_config_path() -> PathBuf {
    config_home().join("oaita.toml")
}

/// `cosign.toml` — the OCI image-signature trust policy (key-based cosign
/// verification). Lives at `{config_home}/cosign.toml`. Read host-side in the
/// engine pull path; never enters a box.
pub fn cosign_config_path() -> PathBuf {
    config_home().join("cosign.toml")
}

/// Where `oaita local` keeps its model + CPU runtime. Deliberately OUTSIDE
/// sarun's own app dirs: the overlay self-hides data/config/state/runtime
/// homes from boxes, and this payload is downloaded INSIDE a box by default
/// — it must be a path a box can write (captured) and an apply lands on the
/// host at the same location the host-side server then reads.
pub fn oaita_local_dir() -> PathBuf {
    home("XDG_DATA_HOME", ".local/share").join("oaita-local")
}

/// `images.toml` — the UI's base-image catalog (the hierarchical picker's
/// groups + tags). Optional: when absent the picker uses a built-in curated
/// list of common distro bases.
pub fn images_config_path() -> PathBuf {
    config_home().join("images.toml")
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

/// The augmented CA bundle (host system bundle + engine's MITM CA root),
/// pre-written once at engine startup. The FUSE overlay shadows each of
/// CA_BUNDLE_TARGETS (engine/src/runner.rs) into `--api` boxes with this
/// file's bytes, so a box reading `/etc/ssl/certs/ca-certificates.crt`
/// (etc.) gets the engine-trusted bundle without any bwrap bind or
/// runner-written on-disk file.
pub fn api_box_ca_pem_path() -> PathBuf {
    runtime_home().join("api-box-ca.pem")
}

/// Synthetic /etc/resolv.conf for `--api` boxes — `nameserver <gw>\n`
/// where <gw> is the engine's per-box-stack gateway IP. Pre-written at
/// engine startup; shadowed in by the overlay.
pub fn api_box_resolv_conf_path() -> PathBuf {
    runtime_home().join("api-box-resolv.conf")
}

pub fn mnt_point() -> PathBuf {
    runtime_home().join("mnt")
}

/// Control socket of the private FUSE mount-owner namespace. Top-level FUSE
/// runners receive the owner user+mount namespace descriptors here before any
/// parser/runtime worker starts; it never carries filesystem operations.
pub fn fuse_broker_socket() -> PathBuf {
    runtime_home().join("fuse-broker.sock")
}

/// Per-run vhost-user socket used by the QEMU transport.  It lives beside the
/// box's other ephemeral state, never in the persistent sqlar/depot.  A named
/// helper also keeps QEMU launch code from inventing a second path convention.
pub fn virtiofs_socket(box_id: i64) -> PathBuf {
    live_home().join(box_id.to_string()).join("virtiofs.sock")
}

/// Resolver projected into QEMU appliances using QEMU's direct host network.
/// Like the target `/init`, this is engine presentation state rather than a
/// captured write in the box.
pub fn appliance_host_resolv_conf_path() -> PathBuf {
    runtime_home().join("appliance-host-resolv.conf")
}

pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [data_home(), config_home(), runtime_home(), state_home(),
              live_home(), mnt_point(), oaita_state_home()] {
        std::fs::create_dir_all(&d)?;
    }
    Ok(())
}
