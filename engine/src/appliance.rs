//! The deliberately narrow QEMU/Linux execution appliance.
//!
//! The filesystem remains `SarunFs`: QEMU only supplies an architecture and
//! a kernel boundary around the command.  Host and PID-1 exchange the command
//! and exit status over one generated binary relation value.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::generated_wire::{
    ApplianceCommand, ApplianceResult, OsString as WireOsString, QemuArchitecture,
};

const ROOT_TAG: &str = "sarun-root";
const CONTROL_PORT: &str = "sarun-control";

pub fn architecture_name(architecture: QemuArchitecture) -> &'static str {
    match architecture {
        QemuArchitecture::Aarch64 => "aarch64",
        QemuArchitecture::X8664 => "x86_64",
    }
}

fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into())).join(".cache")
        })
        .join("sarun/appliances/v1")
}

pub fn target_init(architecture: QemuArchitecture) -> PathBuf {
    cache_root()
        .join(architecture_name(architecture))
        .join("init")
}

fn target_kernel(architecture: QemuArchitecture) -> PathBuf {
    cache_root()
        .join(architecture_name(architecture))
        .join("kernel")
}

fn qemu_binary(architecture: QemuArchitecture) -> PathBuf {
    cache_root()
        .join(format!("host-{}", std::env::consts::ARCH))
        .join(format!("qemu-system-{}", architecture_name(architecture)))
}

fn qemu_args(
    architecture: QemuArchitecture,
    kernel: &Path,
    virtiofs_socket: &Path,
    control_socket: &Path,
    kvm: bool,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-nodefaults",
        "-no-user-config",
        "-no-reboot",
        "-display",
        "none",
        "-serial",
        "stdio",
        "-m",
        "256M",
        "-smp",
        "1",
        "-kernel",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(kernel.as_os_str().to_owned());
    let (machine, console, fs_device, serial_device) = match architecture {
        QemuArchitecture::Aarch64 => ("virt", "ttyAMA0", "vhost-user-fs-pci", "virtio-serial-pci"),
        QemuArchitecture::X8664 => (
            "microvm",
            "ttyS0",
            "vhost-user-fs-device",
            "virtio-serial-device",
        ),
    };
    args.extend(["-machine".into(), machine.into()]);
    args.extend(["-accel".into(), if kvm { "kvm" } else { "tcg" }.into()]);
    args.extend(["-cpu".into(), if kvm { "host" } else { "max" }.into()]);
    args.extend([
        "-append".into(),
        format!("console={console} root={ROOT_TAG} rootfstype=virtiofs rw init=/init panic=-1")
            .into(),
        "-chardev".into(),
        format!("socket,id=fs,path={}", virtiofs_socket.display()).into(),
        "-device".into(),
        format!("{fs_device},chardev=fs,tag={ROOT_TAG}").into(),
        "-chardev".into(),
        format!("socket,id=control,path={}", control_socket.display()).into(),
        "-device".into(),
        serial_device.into(),
        "-device".into(),
        format!("virtserialport,chardev=control,name={CONTROL_PORT}").into(),
    ]);
    args
}

fn wait_for_control(listener: &UnixListener, child: &mut Child) -> io::Result<UnixStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "QEMU exited before its control port connected: {status}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "QEMU appliance did not connect its control port within 30 seconds",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Boot one command and return its guest exit status.
pub fn run(
    architecture: QemuArchitecture,
    box_id: u64,
    virtiofs_socket: &Path,
    command: &ApplianceCommand,
) -> io::Result<i32> {
    let qemu = qemu_binary(architecture);
    let kernel = target_kernel(architecture);
    for (kind, path) in [("QEMU", &qemu), ("kernel", &kernel)] {
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{kind} appliance artifact {} is missing; run scripts/build-appliances.sh all",
                    path.display()
                ),
            ));
        }
    }
    let control_socket = crate::paths::appliance_control_socket(
        i64::try_from(box_id).map_err(|_| io::Error::other("box id exceeds engine range"))?,
    );
    if let Some(parent) = control_socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&control_socket);
    let listener = UnixListener::bind(&control_socket)?;
    let kvm =
        Path::new("/dev/kvm").exists() && architecture_name(architecture) == std::env::consts::ARCH;
    let args = qemu_args(architecture, &kernel, virtiofs_socket, &control_socket, kvm);
    let mut child = Command::new(&qemu)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let result = (|| {
        let mut stream = wait_for_control(&listener, &mut child)?;
        crate::socket_wire::write_versioned(&mut stream, command)?;
        let result: ApplianceResult = crate::socket_wire::read_versioned(&mut stream)?;
        Ok(result.code)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = std::fs::remove_file(control_socket);
    result
}

pub fn wire_command(command: &[String], cwd: Option<&str>) -> Result<ApplianceCommand, String> {
    let command = command
        .iter()
        .map(|value| {
            crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                .map_err(|error| format!("command argument exceeds relation bound: {error:?}"))
        })
        .collect::<Result<Vec<WireOsString>, _>>()?;
    let command = crate::wire::BoundedVec::new(command)
        .map_err(|error| format!("command size violates relation bound: {error:?}"))?;
    let cwd = cwd
        .map(|value| {
            crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                .map_err(|error| format!("cwd exceeds relation bound: {error:?}"))
        })
        .transpose()?;
    let environment = std::env::vars_os()
        .map(|(key, value)| {
            let key = crate::wire::BoundedBytes::new(key.as_bytes().to_vec())
                .map_err(|error| format!("environment name exceeds relation bound: {error:?}"))?;
            let value = crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                .map_err(|error| format!("environment value exceeds relation bound: {error:?}"))?;
            Ok((key, value))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let environment = crate::wire::BoundedMap::new(environment)
        .map_err(|error| format!("environment size violates relation bound: {error:?}"))?;
    Ok(ApplianceCommand {
        command,
        cwd,
        environment,
    })
}

static GUEST_CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward_guest_signal(signal: i32) {
    let child = GUEST_CHILD.load(std::sync::atomic::Ordering::Relaxed);
    if child > 0 {
        unsafe {
            libc::kill(-child, signal);
        }
    }
}

fn mount_one(source: &str, target: &str, kind: &str, flags: libc::c_ulong) -> io::Result<()> {
    std::fs::create_dir_all(target)?;
    let source = std::ffi::CString::new(source).unwrap();
    let target = std::ffi::CString::new(target).unwrap();
    let kind = std::ffi::CString::new(kind).unwrap();
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EBUSY) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

fn execute_guest(command: &ApplianceCommand) -> io::Result<i32> {
    let words: Vec<OsString> = command
        .command
        .as_slice()
        .iter()
        .map(|value| OsString::from_vec(value.as_slice().to_vec()))
        .collect();
    let mut process = Command::new(&words[0]);
    process.args(&words[1..]).env_clear();
    for (key, value) in command.environment.as_map() {
        process.env(
            OsStr::from_bytes(key.as_slice()),
            OsStr::from_bytes(value.as_slice()),
        );
    }
    if let Some(cwd) = &command.cwd {
        process.current_dir(OsStr::from_bytes(cwd.as_slice()));
    }
    unsafe {
        process.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    let child = process.spawn()?;
    let root = child.id() as i32;
    GUEST_CHILD.store(root, std::sync::atomic::Ordering::SeqCst);
    let mut root_status = None;
    while root_status.is_none() {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if pid == root {
            root_status = Some(status);
        }
    }
    GUEST_CHILD.store(0, std::sync::atomic::Ordering::SeqCst);
    let status = root_status.unwrap();
    Ok(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    })
}

fn find_control_port() -> io::Result<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(entries) = std::fs::read_dir("/sys/class/virtio-ports") {
            for entry in entries.flatten() {
                let name = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
                if name.trim() == CONTROL_PORT {
                    return Ok(Path::new("/dev").join(entry.file_name()));
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "virtio control port did not appear within 30 seconds",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// PID-1 entry. Called before Prolog or ordinary CLI initialization.
pub fn init_main() -> i32 {
    if let Err(error) = mount_one("devtmpfs", "/dev", "devtmpfs", 0) {
        eprintln!("sarun init: mount /dev: {error}");
    }
    for (source, target, kind) in [("proc", "/proc", "proc"), ("sysfs", "/sys", "sysfs")] {
        if let Err(error) = mount_one(source, target, kind, 0) {
            eprintln!("sarun init: mount {target}: {error}");
        }
    }
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        unsafe {
            libc::signal(
                signal,
                forward_guest_signal as *const () as libc::sighandler_t,
            );
        }
    }
    let outcome = (|| -> io::Result<i32> {
        let port = find_control_port()?;
        let mut stream = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&port)?;
        let command: ApplianceCommand = crate::socket_wire::read_versioned(&mut stream)?;
        let code = execute_guest(&command)?;
        crate::socket_wire::write_versioned(&mut stream, &ApplianceResult { code })?;
        Ok(code)
    })();
    let code = match outcome {
        Ok(code) => code,
        Err(error) => {
            eprintln!("sarun init: {error}");
            127
        }
    };
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_arguments_keep_architecture_specific_devices_at_the_edge() {
        let a = qemu_args(
            QemuArchitecture::Aarch64,
            Path::new("K"),
            Path::new("F"),
            Path::new("C"),
            false,
        );
        let a = a
            .iter()
            .map(|v| v.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(a.contains("-machine virt"));
        assert!(a.contains("vhost-user-fs-pci"));
        assert!(a.contains("console=ttyAMA0"));
        let x = qemu_args(
            QemuArchitecture::X8664,
            Path::new("K"),
            Path::new("F"),
            Path::new("C"),
            false,
        );
        let x = x
            .iter()
            .map(|v| v.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(x.contains("-machine microvm"));
        assert!(x.contains("vhost-user-fs-device"));
        assert!(x.contains("console=ttyS0"));
    }

    #[test]
    fn appliance_command_uses_the_generated_versioned_codec() {
        let value = ApplianceCommand {
            command: crate::wire::BoundedVec::new(vec![
                crate::wire::BoundedBytes::new(b"/bin/true".to_vec()).unwrap(),
            ])
            .unwrap(),
            cwd: None,
            environment: crate::wire::BoundedMap::new(BTreeMap::new()).unwrap(),
        };
        let mut bytes = Vec::new();
        crate::socket_wire::write_versioned(&mut bytes, &value).unwrap();
        let decoded: ApplianceCommand =
            crate::socket_wire::read_versioned(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded, value);
    }
}
