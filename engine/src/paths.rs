// Instance path layout — mirrors the Python engine's rules exactly (XDG vars,
// $SLOPBOX_NS namespacing, same dirnames), so the two engines are drop-in
// replacements for each other behind the same socket path.

use std::env;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn app_dir() -> String {
    match env::var("SLOPBOX_NS") {
        Ok(ns) if !ns.is_empty() => format!("slopbox.{ns}"),
        _ => "slopbox".into(),
    }
}

fn home(var: &str, fallback: &str) -> PathBuf {
    match env::var(var) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/root".into())).join(fallback),
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
    for d in [
        data_home(),
        config_home(),
        runtime_home(),
        state_home(),
        live_home(),
        mnt_point(),
        oaita_state_home(),
    ] {
        std::fs::create_dir_all(&d)?;
    }
    Ok(())
}

/// Remove the host-visible mount left by sarun's pre-broker FUSE design.
///
/// This is deliberately not part of [`ensure_dirs`]. The caller must first
/// hold the instance lock, so recovery can never detach a running engine's
/// mount. Current broker-owned mounts live in a private mount namespace and
/// therefore do not appear in this process's mountinfo.
pub(crate) fn recover_legacy_host_visible_fuse_mount() -> std::io::Result<()> {
    let path = mnt_point();
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    if !detach_legacy_sarun_fuse_if_present(&mountinfo, &path, detach_with_fusermount)? {
        return Ok(());
    }

    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    if mountinfo_has_legacy_sarun_fuse(&mountinfo, &path) {
        return Err(std::io::Error::other(format!(
            "{}: fusermount reported success but legacy sarun-rs mount remains",
            path.display()
        )));
    }
    Ok(())
}

fn detach_legacy_sarun_fuse_if_present(
    mountinfo: &str,
    path: &Path,
    detach: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    if !mountinfo_has_legacy_sarun_fuse(mountinfo, path) {
        return Ok(false);
    }
    detach(path)?;
    Ok(true)
}

fn mountinfo_has_legacy_sarun_fuse(mountinfo: &str, path: &Path) -> bool {
    let Some(target) = mountinfo_path(path) else {
        return false;
    };
    mountinfo
        .lines()
        .any(|line| is_legacy_sarun_fuse_mount(line, &target))
}

fn is_legacy_sarun_fuse_mount(line: &str, target: &str) -> bool {
    let Some((mount, filesystem)) = line.split_once(" - ") else {
        return false;
    };
    let mut mount_fields = mount.split_whitespace();
    let Some(mount_point) = mount_fields.nth(4) else {
        return false;
    };
    if mount_point != target {
        return false;
    }
    let mut filesystem_fields = filesystem.split_whitespace();
    let fs_type = filesystem_fields.next().unwrap_or("");
    let source = filesystem_fields.next().unwrap_or("");
    (fs_type == "fuse" || fs_type.starts_with("fuse.")) && source == "sarun-rs"
}

fn mountinfo_path(path: &Path) -> Option<String> {
    let mut encoded = String::new();
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b' ' => encoded.push_str("\\040"),
            b'\t' => encoded.push_str("\\011"),
            b'\n' => encoded.push_str("\\012"),
            b'\\' => encoded.push_str("\\134"),
            0 => return None,
            value => encoded.push(value as char),
        }
    }
    Some(encoded)
}

fn detach_with_fusermount(path: &Path) -> std::io::Result<()> {
    for helper in ["fusermount3", "fusermount"] {
        match std::process::Command::new(helper)
            .arg("-u")
            .arg("-z")
            .arg(path)
            .output()
        {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                let detail = String::from_utf8_lossy(&output.stderr);
                return Err(std::io::Error::other(format!(
                    "{helper} -u -z {} failed with {}{}{}",
                    path.display(),
                    output.status,
                    if detail.trim().is_empty() { "" } else { ": " },
                    detail.trim()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("cannot run {helper} for {}: {error}", path.display()),
                ));
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "legacy sarun-rs FUSE mount found, but neither fusermount3 nor fusermount is installed",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_mount_detection_matches_exact_mountpoint_type_and_source() {
        let line = "123 45 0:99 / /run/user/1000/slopbox/mnt rw,nosuid,nodev - fuse sarun-rs rw,user_id=1000,group_id=1000";

        assert!(is_legacy_sarun_fuse_mount(
            line,
            "/run/user/1000/slopbox/mnt"
        ));
        assert!(!is_legacy_sarun_fuse_mount(
            line,
            "/var/home/me/.local/share/slopbox/mnt"
        ));
        assert!(!is_legacy_sarun_fuse_mount(
            "123 45 0:99 / /run/user/1000/slopbox/mnt rw - fuse other rw",
            "/run/user/1000/slopbox/mnt"
        ));
        assert!(!is_legacy_sarun_fuse_mount(
            "123 45 0:99 / /run/user/1000/slopbox/mnt rw - fuseevil sarun-rs rw",
            "/run/user/1000/slopbox/mnt"
        ));
        assert!(is_legacy_sarun_fuse_mount(
            "123 45 0:99 / /run/user/1000/slopbox/mnt rw - fuse.sarun-rs sarun-rs rw",
            "/run/user/1000/slopbox/mnt"
        ));
    }

    #[test]
    fn mountinfo_path_escapes_kernel_mountinfo_fields() {
        assert_eq!(
            mountinfo_path(Path::new("/run/user/1000/slop box/mnt")).unwrap(),
            "/run/user/1000/slop\\040box/mnt"
        );
    }

    #[test]
    fn mount_set_detection_handles_escaped_mountpoint() {
        let mountinfo = "123 45 0:99 / /run/user/1000/slop\\040box/mnt rw - fuse sarun-rs rw";
        assert!(mountinfo_has_legacy_sarun_fuse(
            mountinfo,
            Path::new("/run/user/1000/slop box/mnt")
        ));
    }

    #[test]
    fn recovery_detaches_only_the_exact_legacy_mount() {
        let path = Path::new("/run/user/1000/slopbox/mnt");
        let other = "123 45 0:99 / /run/user/1000/slopbox/mnt rw - fuse other rw";
        let mut called = false;
        assert!(
            !detach_legacy_sarun_fuse_if_present(other, path, |_| {
                called = true;
                Ok(())
            })
            .unwrap()
        );
        assert!(!called);

        let legacy = "123 45 0:99 / /run/user/1000/slopbox/mnt rw - fuse sarun-rs rw,user_id=1000";
        assert!(
            detach_legacy_sarun_fuse_if_present(legacy, path, |found| {
                called = true;
                assert_eq!(found, path);
                Ok(())
            })
            .unwrap()
        );
        assert!(called);
    }
}
