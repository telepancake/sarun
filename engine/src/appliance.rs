//! The deliberately narrow QEMU/Linux execution appliance.
//!
//! The filesystem remains `SarunFs`: QEMU only supplies an architecture and
//! a kernel boundary around the command.  Host and PID-1 exchange the command
//! and exit status over one generated binary relation value.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::debug_session::DebugSessionResources;
use crate::generated_wire::{
    ApplianceCommand, ApplianceFrame, ApplianceRunRequest, DebugMode, NetMode,
    OsString as WireOsString, QemuArchitecture, ShortOsString,
};

const ROOT_TAG: &str = "sarun-root";
const CONTROL_PORT: &str = "sarun-control";
pub const NESTED_BROKER: &str = "sarun-appliance-run";

const MAX_APPLIANCE_CPUS: usize = 16;

/// A child which cannot outlive an early return from its owning operation.
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("managed child already reaped")
    }

    fn reap(mut self) {
        if let Some(mut child) = self.child.take() {
            if !wait_until(&mut child, Duration::from_secs(5)) {
                terminate(&mut child);
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate(&mut child);
        }
    }
}

fn wait_until(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn terminate(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    if !wait_until(child, Duration::from_secs(3)) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn appliance_resources() -> (usize, usize) {
    // available_parallelism() observes the process's affinity/cgroup limit, so
    // the appliance follows the CPU budget Sarun was actually given rather
    // than the physical-machine count.  Enough RAM per vCPU matters here: ten
    // simultaneously live clang processes cannot make useful progress in the
    // old fixed 256 MiB guest even when the paired kernel itself is tiny.
    let cpus = std::thread::available_parallelism()
        .map_or(1, |count| count.get())
        .clamp(1, MAX_APPLIANCE_CPUS);
    let memory_mib = (256 + cpus * 128).clamp(512, 4096);
    (cpus, memory_mib)
}

pub fn architecture_name(architecture: QemuArchitecture) -> &'static str {
    match architecture {
        QemuArchitecture::Aarch64 => "aarch64",
        QemuArchitecture::X8664 => "x86_64",
        QemuArchitecture::Arm => "arm",
        QemuArchitecture::Mmips => "mmips",
    }
}

fn cache_root() -> PathBuf {
    if let Some(root) = std::env::var_os("SARUN_APPLIANCE_ROOT") {
        return PathBuf::from(root);
    }
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
    let executable_architecture = match architecture {
        QemuArchitecture::Mmips => "mipsel",
        other => architecture_name(other),
    };
    cache_root()
        .join(format!("host-{}", std::env::consts::ARCH))
        .join(format!("qemu-system-{executable_architecture}"))
}

fn host_supports_compat32(architecture: QemuArchitecture) -> bool {
    match architecture {
        // IA32 execution is architectural on the x86-64 KVM hosts Sarun
        // supports; the paired kernel still has to enable IA32_EMULATION.
        QemuArchitecture::X8664 => true,
        // AArch32 EL0 is optional in ARMv8 and absent on some modern CPUs.
        // Never select an accelerator that would make the paired appliance
        // lose its promised 32-bit process ABI. `lscpu` derives this from the
        // host kernel/CPU and is forced to stable English output; uncertainty
        // conservatively retains TCG's known-compatible `max` CPU.
        QemuArchitecture::Aarch64 => Command::new("lscpu")
            .env("LC_ALL", "C")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .is_some_and(|output| {
                output
                    .lines()
                    .any(|line| line.starts_with("CPU op-mode(s):") && line.contains("32-bit"))
            }),
        QemuArchitecture::Arm | QemuArchitecture::Mmips => false,
    }
}

fn qemu_args(
    architecture: QemuArchitecture,
    kernel: &OsStr,
    virtiofs_fd: RawFd,
    control_fd: RawFd,
    data_dir: &Path,
    kvm: bool,
    net_mode: NetMode,
    network_fd: Option<RawFd>,
    debug_fd: Option<RawFd>,
) -> Vec<OsString> {
    let (cpus, memory_mib) = appliance_resources();
    let memory = format!("{memory_mib}M");
    let mut args: Vec<OsString> = [
        "-nodefaults",
        "-no-user-config",
        "-no-reboot",
        "-display",
        "none",
        "-serial",
        "stdio",
        "-m",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.extend([
        memory.clone().into(),
        "-object".into(),
        format!("memory-backend-memfd,id=mem,size={memory},share=on").into(),
        "-numa".into(),
        "node,memdev=mem".into(),
        "-smp".into(),
        cpus.to_string().into(),
        "-kernel".into(),
    ]);
    args.push(kernel.to_owned());
    let (machine, console, fs_device, serial_device, shutdown) = match architecture {
        QemuArchitecture::Aarch64 => (
            "virt",
            "ttyAMA0",
            "vhost-user-fs-pci",
            "virtio-serial-pci",
            "",
        ),
        QemuArchitecture::X8664 => (
            // With ACPI enabled, microvm does not add its virtio-mmio
            // transports to the kernel command line.  The appliance kernel
            // deliberately has no ACPI device-discovery path, so make the
            // transport description explicit and leave unused PC/RTC
            // hardware out of the paired machine.
            "microvm,acpi=off,pcie=off,rtc=off",
            "ttyS0",
            "vhost-user-fs-device",
            "virtio-serial-device",
            " reboot=t",
        ),
        QemuArchitecture::Arm | QemuArchitecture::Mmips => {
            unreachable!("named image architectures use qemu_image_args")
        }
    };
    args.extend(["-machine".into(), machine.into()]);
    if architecture == QemuArchitecture::X8664 {
        args.extend(["-L".into(), data_dir.as_os_str().to_owned()]);
    }
    args.extend([
        "-accel".into(),
        if kvm { "kvm" } else { "tcg,thread=multi" }.into(),
    ]);
    args.extend(["-cpu".into(), if kvm { "host" } else { "max" }.into()]);
    args.extend([
        "-append".into(),
        format!(
            "console={console} root={ROOT_TAG} rootfstype=virtiofs rw init=/init panic=-1{shutdown}"
        )
        .into(),
        "-chardev".into(),
        format!("socket,id=fs,fd={virtiofs_fd}").into(),
        "-device".into(),
        format!("{fs_device},chardev=fs,tag={ROOT_TAG}").into(),
        "-chardev".into(),
        format!("socket,id=control,fd={control_fd}").into(),
        "-device".into(),
        serial_device.into(),
        "-device".into(),
        format!("virtserialport,chardev=control,name={CONTROL_PORT}").into(),
    ]);
    if net_mode != NetMode::Off {
        let backend = match net_mode {
            NetMode::Off => unreachable!(),
            NetMode::Tap => format!(
                "dgram,id=network,local.type=fd,local.str={}",
                network_fd.expect("networked appliance requires its packet socket")
            ),
            NetMode::Host => "user,id=network".to_string(),
        };
        let device = match architecture {
            // The appliance boots a built-in kernel driver and carries no PXE
            // firmware; suppress the otherwise requested efi-virtio.rom.
            QemuArchitecture::Aarch64 => "virtio-net-pci,rombar=0,romfile=",
            QemuArchitecture::X8664 => "virtio-net-device",
            QemuArchitecture::Arm | QemuArchitecture::Mmips => {
                unreachable!("named image architectures use qemu_image_args")
            }
        };
        args.extend([
            "-netdev".into(),
            backend.into(),
            "-device".into(),
            format!("{device},netdev=network,mac=02:73:72:6e:00:02").into(),
        ]);
    }
    if let Some(fd) = debug_fd {
        args.extend([
            "-chardev".into(),
            format!("socket,id=viros-rsp,fd={fd}").into(),
            "-gdb".into(),
            "chardev:viros-rsp".into(),
        ]);
    }
    args
}

fn qemu_image_args(
    architecture: QemuArchitecture,
    profile: crate::generated_wire::DebugImageProfile,
    kernel: &OsStr,
    initramfs: &OsStr,
    init: &str,
    data_dir: &Path,
    kvm: bool,
    debug_fd: RawFd,
) -> io::Result<Vec<OsString>> {
    use crate::generated_wire::DebugImageProfile;
    let (machine, console, shutdown, fixed_cpu, fixed_resources) = match (architecture, profile) {
        (QemuArchitecture::Aarch64, DebugImageProfile::VirtInitramfsAarch64V1) => {
            ("virt", "ttyAMA0", "", None, None)
        }
        (QemuArchitecture::X8664, DebugImageProfile::MicrovmInitramfsX8664V1) => (
            "microvm,acpi=off,pcie=off,rtc=off",
            "ttyS0",
            " reboot=t",
            None,
            None,
        ),
        (QemuArchitecture::Arm, DebugImageProfile::VirtInitramfsArmV1) => {
            ("virt", "ttyAMA0", "", Some("cortex-a15"), Some((1, 512)))
        }
        (QemuArchitecture::Mmips, DebugImageProfile::MaltaInitramfsMipselV1) => {
            // `mmips` is ViroS's little-endian MIPS32 image identity. This
            // particular profile is the standard Malta/24Kc contract used by
            // OpenWrt Malta builds; RouterBoard kernels must publish and use
            // their separate ViroS hardware profile instead.
            ("malta", "ttyS0,115200", "", Some("24Kc"), Some((1, 256)))
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "debug image profile does not match QEMU architecture",
            ));
        }
    };
    let (cpus, memory_mib) = fixed_resources.unwrap_or_else(appliance_resources);
    let memory = format!("{memory_mib}M");
    let mut args = [
        "-nodefaults",
        "-no-user-config",
        "-no-reboot",
        "-display",
        "none",
        "-serial",
        "stdio",
        "-m",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    args.push(memory.clone().into());
    // The ordinary 64-bit profiles use Sarun's shared memfd topology. The
    // fixed 32-bit firmware profiles use the boards' simple RAM model; Malta
    // does not advertise a NUMA memory backend contract.
    if fixed_resources.is_none() {
        args.extend([
            "-object".into(),
            format!("memory-backend-memfd,id=mem,size={memory},share=on").into(),
            "-numa".into(),
            "node,memdev=mem".into(),
        ]);
    }
    args.extend([
        "-smp".into(),
        cpus.to_string().into(),
        "-kernel".into(),
        kernel.to_owned(),
        "-initrd".into(),
        initramfs.to_owned(),
        "-machine".into(),
        machine.into(),
    ]);
    if architecture == QemuArchitecture::X8664 {
        args.extend(["-L".into(), data_dir.as_os_str().to_owned()]);
    }
    args.extend([
        "-accel".into(),
        if kvm { "kvm" } else { "tcg,thread=multi" }.into(),
        "-cpu".into(),
        fixed_cpu.unwrap_or(if kvm { "host" } else { "max" }).into(),
        "-append".into(),
        format!("console={console} rdinit={init} panic=-1{shutdown}").into(),
        "-chardev".into(),
        format!("socket,id=viros-rsp,fd={debug_fd}").into(),
        "-gdb".into(),
        "chardev:viros-rsp".into(),
        "-S".into(),
    ]);
    Ok(args)
}

fn run_opaque_image(
    architecture: QemuArchitecture,
    qemu: &Path,
    resources: DebugSessionResources,
    box_channel: UnixStream,
) -> io::Result<i32> {
    let (kernel, rsp, image) = resources.into_parts();
    let (initramfs, profile, init) = image
        .expect("opaque image launch requires an image descriptor")
        .into_parts();
    let inherit = |fd: RawFd| {
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD, 3) };
        if duplicated < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
        }
    };
    let qemu_kernel = inherit(kernel.as_raw_fd())?;
    let qemu_initramfs = inherit(initramfs.as_raw_fd())?;
    let qemu_rsp = inherit(rsp.as_raw_fd())?;
    let matching_host = architecture_name(architecture) == std::env::consts::ARCH;
    let kvm_device = matching_host
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok();
    let kvm = kvm_device && host_supports_compat32(architecture);
    let data_dir = qemu
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("share/qemu");
    let args = qemu_image_args(
        architecture,
        profile,
        &OsString::from(format!("/proc/self/fd/{}", qemu_kernel.as_raw_fd())),
        &OsString::from(format!("/proc/self/fd/{}", qemu_initramfs.as_raw_fd())),
        &init,
        &data_dir,
        kvm,
        qemu_rsp.as_raw_fd(),
    )?;
    eprintln!(
        "sarun-engine: qemu {} opaque image accelerator {}",
        architecture_name(architecture),
        if kvm { "kvm" } else { "tcg" },
    );
    let mut child = ChildGuard::new(
        Command::new(qemu)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    drop(qemu_kernel);
    drop(qemu_initramfs);
    drop(qemu_rsp);
    let channel = ApplianceBoxChannel::new(box_channel)?;
    channel.await_debug_start()?;
    let status = child.child_mut().wait()?;
    channel.close_writer();
    child.reap();
    use std::os::unix::process::ExitStatusExt;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
}

/// Raw Ethernet packet lane between QEMU and the engine's existing smoltcp
/// stack. SOCK_DGRAM preserves exactly one Ethernet frame per operation, just
/// like the TAP fd consumed by `StackRuntime`, without creating another
/// filesystem or network-policy implementation.
pub fn packet_socket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_DGRAM | libc::SOCK_NONBLOCK,
            0,
            fds.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Boot one command and return its guest exit status.
pub fn run(
    architecture: QemuArchitecture,
    virtiofs: OwnedFd,
    command: &ApplianceCommand,
    network: Option<OwnedFd>,
    box_channel: UnixStream,
    debug: Option<DebugSessionResources>,
) -> io::Result<i32> {
    let qemu = qemu_binary(architecture);
    let data_dir = qemu
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("share/qemu");
    if !qemu.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "QEMU appliance artifact {} is missing; run scripts/build-appliances.sh all",
                qemu.display()
            ),
        ));
    }
    if debug
        .as_ref()
        .is_some_and(DebugSessionResources::has_guest_image)
    {
        return run_opaque_image(
            architecture,
            &qemu,
            debug.expect("guest image was checked above"),
            box_channel,
        );
    }
    let stock_kernel = debug.is_none().then(|| target_kernel(architecture));
    if let Some(kernel) = &stock_kernel {
        if !kernel.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "kernel appliance artifact {} is missing; run scripts/build-appliances.sh all",
                    kernel.display()
                ),
            ));
        }
    }
    let (mut control, qemu_control) = UnixStream::pair()?;
    let inherit = |fd: RawFd| {
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD, 3) };
        if duplicated < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
        }
    };
    let qemu_virtiofs = inherit(virtiofs.as_raw_fd())?;
    let qemu_control = inherit(qemu_control.as_raw_fd())?;
    let qemu_network = network
        .as_ref()
        .map(|fd| inherit(fd.as_raw_fd()))
        .transpose()?;
    let debug_enabled = debug.is_some();
    let (qemu_debug_kernel, qemu_rsp) = match debug {
        Some(resources) => {
            let (kernel, rsp, image) = resources.into_parts();
            debug_assert!(image.is_none());
            (
                Some(inherit(kernel.as_raw_fd())?),
                Some(inherit(rsp.as_raw_fd())?),
            )
        }
        None => (None, None),
    };
    let kernel_argument = qemu_debug_kernel
        .as_ref()
        .map(|kernel| OsString::from(format!("/proc/self/fd/{}", kernel.as_raw_fd())))
        .unwrap_or_else(|| {
            stock_kernel
                .as_ref()
                .expect("stock kernel selected without debug resources")
                .as_os_str()
                .to_owned()
        });
    let matching_host = architecture_name(architecture) == std::env::consts::ARCH;
    let kvm_device = matching_host
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok();
    let kvm = kvm_device && host_supports_compat32(architecture);
    if kvm_device && !kvm {
        eprintln!(
            "sarun-engine: qemu {} retaining tcg because host KVM lacks the 32-bit process ABI",
            architecture_name(architecture),
        );
    }
    let args = qemu_args(
        architecture,
        &kernel_argument,
        qemu_virtiofs.as_raw_fd(),
        qemu_control.as_raw_fd(),
        &data_dir,
        kvm,
        command.net_mode,
        qemu_network.as_ref().map(AsRawFd::as_raw_fd),
        qemu_rsp.as_ref().map(AsRawFd::as_raw_fd),
    );
    eprintln!(
        "sarun-engine: qemu {} accelerator {}",
        architecture_name(architecture),
        if kvm { "kvm" } else { "tcg" },
    );
    let child = Command::new(&qemu)
        .args(&args)
        .stdin(Stdio::piped())
        // User stdout returns through the recorded virtio-fs sink. The serial
        // console is an implementation diagnostic and must not corrupt a
        // pipeline consuming Sarun's stdout.
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut child = ChildGuard::new(child);
    let mut qemu_input = child
        .child_mut()
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("QEMU stdin pipe was not created"))?;
    let input_ready = Arc::new((Mutex::new(false), Condvar::new()));
    let input_gate = input_ready.clone();
    std::thread::spawn(move || {
        let (ready, changed) = &*input_gate;
        let mut ready = ready.lock().unwrap();
        while !*ready {
            ready = changed.wait(ready).unwrap();
        }
        drop(ready);
        let _ = io::copy(&mut io::stdin().lock(), &mut qemu_input);
    });
    drop(qemu_virtiofs);
    drop(qemu_control);
    drop(qemu_network);
    drop(qemu_debug_kernel);
    drop(qemu_rsp);
    let control_writer = Arc::new(Mutex::new(control.try_clone()?));
    let box_channel = ApplianceBoxChannel::new(box_channel)?;
    let nested = Arc::new(Mutex::new(HashMap::<u64, NestedChild>::new()));
    let mut result = (|| {
        crate::socket_wire::write_versioned(&mut control, command)?;
        loop {
            match crate::socket_wire::read_atom::<_, ApplianceFrame>(&mut control)? {
                ApplianceFrame::NestedOpen { stream, request } => {
                    spawn_nested_appliance(
                        stream,
                        request,
                        box_channel.clone(),
                        control_writer.clone(),
                        nested.clone(),
                    );
                }
                ApplianceFrame::NestedInput { stream, data } => {
                    if let Some(input) = nested
                        .lock()
                        .unwrap()
                        .get_mut(&stream)
                        .and_then(|child| child.input.as_mut())
                    {
                        let _ = input.write_all(data.as_slice());
                        let _ = input.flush();
                    }
                }
                ApplianceFrame::NestedInputEof { stream } => {
                    if let Some(child) = nested.lock().unwrap().get_mut(&stream) {
                        child.input.take();
                    }
                }
                ApplianceFrame::NestedSignal { stream, signal } => {
                    if let Some(child) = nested.lock().unwrap().get(&stream) {
                        unsafe { libc::kill(-child.pid, signal) };
                    }
                }
                ApplianceFrame::Process { event } => {
                    forward_guest_process(&box_channel, event)?;
                }
                ApplianceFrame::DebugPrepared => {
                    if !debug_enabled {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "guest entered a debug barrier in a normal session",
                        ));
                    }
                    // Keep PID 1 at its pre-command barrier until the engine
                    // has observed the per-session facade service.  The
                    // DebugStart atom is then queued on the guest control
                    // port while QEMU remains stopped; GDB's first continue
                    // makes it visible to PID 1 and starts the command.
                    box_channel.await_debug_start()?;
                    let mut writer = control_writer.lock().unwrap();
                    crate::socket_wire::write_atom(&mut *writer, &ApplianceFrame::DebugStart)?;
                    writer.flush()?;
                }
                ApplianceFrame::Ready => {
                    let (ready, changed) = &*input_ready;
                    *ready.lock().unwrap() = true;
                    changed.notify_all();
                }
                ApplianceFrame::Result { code } => break Ok(code),
                _ => {
                    break Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "guest sent a host-to-guest appliance frame",
                    ));
                }
            }
        }
    })();
    signal_nested_children(&nested, libc::SIGTERM);
    if !wait_nested_children(&nested, Duration::from_secs(10)) {
        signal_nested_children(&nested, libc::SIGKILL);
        if !wait_nested_children(&nested, Duration::from_secs(10)) && result.is_ok() {
            result = Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "flat child appliance did not terminate",
            ));
        }
    }
    if result.is_err() {
        let _ = child.child_mut().kill();
    }
    child.reap();
    box_channel.close_writer();
    result
}

struct NestedChild {
    pid: i32,
    input: Option<ChildStdin>,
}

struct BoxChannelState {
    connections: VecDeque<UnixStream>,
    debug_start: bool,
    closed: bool,
}

/// Host endpoint of one QEMU box channel. One reader owns all engine-to-runner
/// frames so live output and SCM_RIGHTS replies cannot race each other. Writers
/// may concurrently forward guest provenance or request flat nested launches;
/// the socket write mutex preserves complete generated atoms.
struct ApplianceBoxChannel {
    writer: Mutex<UnixStream>,
    responses: Arc<(Mutex<BoxChannelState>, Condvar)>,
}

impl ApplianceBoxChannel {
    fn new(reader: UnixStream) -> io::Result<Arc<Self>> {
        let writer = reader.try_clone()?;
        let responses = Arc::new((
            Mutex::new(BoxChannelState {
                connections: VecDeque::new(),
                debug_start: false,
                closed: false,
            }),
            Condvar::new(),
        ));
        let channel = Arc::new(Self {
            writer: Mutex::new(writer),
            responses: responses.clone(),
        });
        std::thread::spawn(move || {
            let mut buffered = Vec::new();
            let mut chunk = [0u8; 65536];
            loop {
                let mut received_fd = None;
                let count = crate::runner::recv_box_frame_bytes(
                    reader.as_raw_fd(),
                    &mut chunk,
                    &mut received_fd,
                );
                if count <= 0 {
                    if let Some(fd) = received_fd {
                        unsafe { libc::close(fd) };
                    }
                    break;
                }
                buffered.extend_from_slice(&chunk[..count as usize]);
                let (frames, used) = crate::frames::decode(&buffered);
                for (kind, payload) in frames {
                    match kind {
                        crate::frames::FRAME_ECHO if !payload.is_empty() => {
                            let result = if payload[0] == 1 {
                                let mut output = io::stderr().lock();
                                output
                                    .write_all(&payload[1..])
                                    .and_then(|()| output.flush())
                            } else {
                                let mut output = io::stdout().lock();
                                output
                                    .write_all(&payload[1..])
                                    .and_then(|()| output.flush())
                            };
                            if result.is_err() {
                                break;
                            }
                        }
                        crate::frames::FRAME_CONN => {
                            if let Some(fd) = received_fd.take() {
                                let connection = unsafe { UnixStream::from_raw_fd(fd) };
                                let (state, changed) = &*responses;
                                state.lock().unwrap().connections.push_back(connection);
                                changed.notify_one();
                            }
                        }
                        crate::frames::FRAME_ECHO_DONE => {}
                        crate::frames::FRAME_DEBUG_START => {
                            let (state, changed) = &*responses;
                            state.lock().unwrap().debug_start = true;
                            changed.notify_all();
                        }
                        _ => {}
                    }
                }
                if let Some(fd) = received_fd {
                    unsafe { libc::close(fd) };
                }
                buffered.drain(..used);
            }
            let (state, changed) = &*responses;
            state.lock().unwrap().closed = true;
            changed.notify_all();
        });
        Ok(channel)
    }

    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn request_connection(&self) -> io::Result<UnixStream> {
        self.send(&crate::frames::encode(crate::frames::FRAME_OPEN_CONN, &[]))?;
        let (state, changed) = &*self.responses;
        let mut state = state.lock().unwrap();
        loop {
            if let Some(connection) = state.connections.pop_front() {
                return Ok(connection);
            }
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "box channel closed while opening nested engine connection",
                ));
            }
            state = changed.wait(state).unwrap();
        }
    }

    fn await_debug_start(&self) -> io::Result<()> {
        self.send(&crate::frames::encode(
            crate::frames::FRAME_DEBUG_PREPARED,
            &[],
        ))?;
        let (state, changed) = &*self.responses;
        let mut state = state.lock().unwrap();
        loop {
            if state.debug_start {
                return Ok(());
            }
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "box channel closed before debugger readiness",
                ));
            }
            state = changed.wait(state).unwrap();
        }
    }

    fn close_writer(&self) {
        let _ = self
            .writer
            .lock()
            .unwrap()
            .shutdown(std::net::Shutdown::Write);
    }
}

fn signal_nested_children(children: &Arc<Mutex<HashMap<u64, NestedChild>>>, signal: i32) {
    for child in children.lock().unwrap().values() {
        unsafe { libc::kill(-child.pid, signal) };
    }
}

fn wait_nested_children(
    children: &Arc<Mutex<HashMap<u64, NestedChild>>>,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if children.lock().unwrap().is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn write_appliance_frame(writer: &Arc<Mutex<UnixStream>>, frame: &ApplianceFrame) {
    let mut writer = writer.lock().unwrap();
    let _ = crate::socket_wire::write_atom(&mut *writer, frame);
    let _ = writer.flush();
}

fn forward_guest_process(
    channel: &ApplianceBoxChannel,
    event: crate::generated_wire::GuestProcessEvent,
) -> io::Result<()> {
    use crate::wire::WireValue;
    let mut encoded = Vec::new();
    crate::generated_wire::BoxFrame::GuestProcess { event }
        .encode_atom(&mut encoded)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("encode guest process event: {error:?}"),
            )
        })?;
    channel.send(&encoded)
}

fn nested_output(writer: &Arc<Mutex<UnixStream>>, stream: u64, bytes: &[u8]) {
    for chunk in bytes.chunks(crate::generated_wire::LIMIT_STREAM_CHUNK_BYTES) {
        let Ok(data) = crate::wire::BoundedBytes::new(chunk.to_vec()) else {
            continue;
        };
        write_appliance_frame(writer, &ApplianceFrame::NestedOutput { stream, data });
    }
}

fn nested_host_args(request: &ApplianceRunRequest) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("run"),
        OsString::from("--net"),
        OsString::from(match request.net_mode {
            NetMode::Off => "off",
            NetMode::Host => "host",
            NetMode::Tap => "tap",
        }),
        OsString::from("--qemu"),
        OsString::from(architecture_name(request.architecture)),
    ];
    if request.capture_environment {
        arguments.push(OsString::from("-e"));
    }
    if request.no_parent {
        arguments.push(OsString::from("--no-parent"));
    }
    if request.readonly_parent {
        arguments.push(OsString::from("--readonly-parent"));
    }
    if request.brush {
        arguments.push(OsString::from("-b"));
    }
    if let Some(name) = &request.name {
        arguments.push(OsString::from(name.as_str()));
    }
    if let Some(cwd) = &request.cwd {
        arguments.push(OsString::from("-C"));
        arguments.push(OsString::from_vec(cwd.as_slice().to_vec()));
    }
    arguments.push(OsString::from("--"));
    arguments.extend(
        request
            .command
            .as_slice()
            .iter()
            .map(|word| OsString::from_vec(word.as_slice().to_vec())),
    );
    arguments
}

fn spawn_nested_appliance(
    stream: u64,
    request: ApplianceRunRequest,
    box_channel: Arc<ApplianceBoxChannel>,
    control: Arc<Mutex<UnixStream>>,
    children: Arc<Mutex<HashMap<u64, NestedChild>>>,
) {
    if children.lock().unwrap().contains_key(&stream) {
        nested_output(&control, stream, b"sarun: duplicate nested stream\n");
        write_appliance_frame(&control, &ApplianceFrame::NestedResult { stream, code: 1 });
        return;
    }
    let engine = box_channel.request_connection();
    let engine = match engine {
        Ok(engine) => engine,
        Err(error) => {
            nested_output(
                &control,
                stream,
                format!("sarun: nested engine connection: {error}\n").as_bytes(),
            );
            write_appliance_frame(&control, &ApplianceFrame::NestedResult { stream, code: 1 });
            return;
        }
    };
    let engine_fd = engine.as_raw_fd();
    let flags = unsafe { libc::fcntl(engine_fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(engine_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
    {
        nested_output(
            &control,
            stream,
            b"sarun: cannot inherit nested engine connection\n",
        );
        write_appliance_frame(&control, &ApplianceFrame::NestedResult { stream, code: 1 });
        return;
    }
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            nested_output(
                &control,
                stream,
                format!("sarun: nested executable: {error}\n").as_bytes(),
            );
            write_appliance_frame(&control, &ApplianceFrame::NestedResult { stream, code: 1 });
            return;
        }
    };
    let arguments = nested_host_args(&request);
    let mut child_command = Command::new(executable);
    child_command.args(arguments).env_clear();
    for (key, value) in request.environment.as_map() {
        child_command.env(
            OsStr::from_bytes(key.as_slice()),
            OsStr::from_bytes(value.as_slice()),
        );
    }
    child_command
        .env_remove("SARUN_APPLIANCE_BROKER")
        .env("SARUN_ENGINE_FD", engine_fd.to_string())
        .env("SARUN_ENGINE_PARENT", "1")
        .env("SARUN_APPLIANCE_ROOT", cache_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match child_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            nested_output(
                &control,
                stream,
                format!("sarun: launch nested appliance: {error}\n").as_bytes(),
            );
            write_appliance_frame(&control, &ApplianceFrame::NestedResult { stream, code: 1 });
            return;
        }
    };
    drop(engine);
    let pid = child.id() as i32;
    let input = child.stdin.take();
    let output_threads: Vec<_> = [
        child.stdout.take().map(OutputPipe::Stdout),
        child.stderr.take().map(OutputPipe::Stderr),
    ]
    .into_iter()
    .flatten()
    .map(|mut output| {
        let writer = control.clone();
        std::thread::spawn(move || {
            let mut bytes = [0u8; 65536];
            loop {
                let count = match &mut output {
                    OutputPipe::Stdout(pipe) => pipe.read(&mut bytes),
                    OutputPipe::Stderr(pipe) => pipe.read(&mut bytes),
                };
                match count {
                    Ok(0) | Err(_) => break,
                    Ok(count) => nested_output(&writer, stream, &bytes[..count]),
                }
            }
        })
    })
    .collect();
    children
        .lock()
        .unwrap()
        .insert(stream, NestedChild { pid, input });
    std::thread::spawn(move || {
        let code = match child.wait() {
            Ok(status) => status.code().unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                128 + status.signal().unwrap_or(1)
            }),
            Err(_) => 1,
        };
        for output in output_threads {
            let _ = output.join();
        }
        children.lock().unwrap().remove(&stream);
        write_appliance_frame(&control, &ApplianceFrame::NestedResult { stream, code });
    });
}

enum OutputPipe {
    Stdout(std::process::ChildStdout),
    Stderr(std::process::ChildStderr),
}

pub fn wire_command(
    command: &[String],
    cwd: Option<&str>,
    net_mode: crate::net::NetMode,
    brush: bool,
    debug_mode: DebugMode,
) -> Result<ApplianceCommand, String> {
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
    let mut environment = wire_environment()?;
    if brush {
        environment.insert(
            crate::wire::BoundedBytes::new(b"SARUN_EXE".to_vec())
                .expect("fixed environment name is bounded"),
            crate::wire::BoundedBytes::new(b"/init".to_vec())
                .expect("fixed environment value is bounded"),
        );
    }
    let environment = crate::wire::BoundedMap::new(environment)
        .map_err(|error| format!("environment size violates relation bound: {error:?}"))?;
    Ok(ApplianceCommand {
        command,
        cwd,
        environment,
        net_mode: match net_mode {
            crate::net::NetMode::Off => NetMode::Off,
            crate::net::NetMode::Host => NetMode::Host,
            crate::net::NetMode::Tap => NetMode::Tap,
        },
        debug_mode,
    })
}

fn wire_environment() -> Result<BTreeMap<ShortOsString, WireOsString>, String> {
    std::env::vars_os()
        .filter(|(key, _)| {
            !matches!(
                key.as_bytes(),
                b"SARUN_ENGINE_FD" | b"SARUN_ENGINE_PARENT" | b"SARUN_APPLIANCE_ROOT"
            )
        })
        .map(|(key, value)| {
            let key = crate::wire::BoundedBytes::new(key.as_bytes().to_vec())
                .map_err(|error| format!("environment name exceeds relation bound: {error:?}"))?;
            let value = crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                .map_err(|error| format!("environment value exceeds relation bound: {error:?}"))?;
            Ok((key, value))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()
}

pub fn wire_nested_request(
    architecture: QemuArchitecture,
    name: Option<String>,
    capture_environment: bool,
    no_parent: bool,
    readonly_parent: bool,
    cwd: Option<String>,
    net_mode: crate::net::NetMode,
    brush: bool,
    command: Vec<String>,
) -> Result<ApplianceRunRequest, String> {
    let command = command
        .into_iter()
        .map(|value| {
            crate::wire::BoundedBytes::new(value.into_bytes())
                .map_err(|error| format!("command argument exceeds relation bound: {error:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let command = crate::wire::BoundedVec::new(command)
        .map_err(|error| format!("command size violates relation bound: {error:?}"))?;
    let name = name
        .map(crate::wire::BoundedText::new)
        .transpose()
        .map_err(|error| format!("box name exceeds relation bound: {error:?}"))?;
    let cwd = cwd
        .map(|value| crate::wire::BoundedBytes::new(value.into_bytes()))
        .transpose()
        .map_err(|error| format!("cwd exceeds relation bound: {error:?}"))?;
    let environment = crate::wire::BoundedMap::new(wire_environment()?)
        .map_err(|error| format!("environment size violates relation bound: {error:?}"))?;
    Ok(ApplianceRunRequest {
        architecture,
        name,
        capture_environment,
        no_parent,
        readonly_parent,
        cwd,
        net_mode: match net_mode {
            crate::net::NetMode::Off => NetMode::Off,
            crate::net::NetMode::Host => NetMode::Host,
            crate::net::NetMode::Tap => NetMode::Tap,
        },
        brush,
        command,
        environment,
    })
}

static NESTED_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn record_nested_signal(signal: i32) {
    NESTED_SIGNAL.store(signal, std::sync::atomic::Ordering::SeqCst);
}

/// Guest-side endpoint for `run --qemu` inside an appliance. It asks PID 1 to
/// relay one semantic launch operation to the host outer runner; the caller
/// sees an ordinary full-duplex process channel, never an engine socket.
pub fn nested_run(request: ApplianceRunRequest, broker: &str) -> io::Result<i32> {
    use std::os::linux::net::SocketAddrExt;
    let address = std::os::unix::net::SocketAddr::from_abstract_name(broker.as_bytes())?;
    let mut stream = UnixStream::connect_addr(&address)?;
    crate::socket_wire::write_atom(
        &mut stream,
        &ApplianceFrame::NestedOpen { stream: 0, request },
    )?;
    stream.flush()?;

    let operation_writer = Arc::new(Mutex::new(stream.try_clone()?));
    let input_writer = operation_writer.clone();
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        let mut bytes = vec![0u8; crate::generated_wire::LIMIT_STREAM_CHUNK_BYTES.min(65536)];
        loop {
            match input.read(&mut bytes) {
                Ok(0) | Err(_) => {
                    let mut input_stream = input_writer.lock().unwrap();
                    let _ = crate::socket_wire::write_atom(
                        &mut *input_stream,
                        &ApplianceFrame::NestedInputEof { stream: 0 },
                    );
                    let _ = input_stream.flush();
                    break;
                }
                Ok(count) => {
                    let Ok(data) = crate::wire::BoundedBytes::new(bytes[..count].to_vec()) else {
                        break;
                    };
                    let mut input_stream = input_writer.lock().unwrap();
                    if crate::socket_wire::write_atom(
                        &mut *input_stream,
                        &ApplianceFrame::NestedInput { stream: 0, data },
                    )
                    .and_then(|()| input_stream.flush())
                    .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        unsafe {
            libc::signal(
                signal,
                record_nested_signal as *const () as libc::sighandler_t,
            )
        };
    }
    let signal_writer = operation_writer;
    std::thread::spawn(move || {
        loop {
            let signal = NESTED_SIGNAL.swap(0, std::sync::atomic::Ordering::SeqCst);
            if signal != 0 {
                let mut signal_stream = signal_writer.lock().unwrap();
                if crate::socket_wire::write_atom(
                    &mut *signal_stream,
                    &ApplianceFrame::NestedSignal { stream: 0, signal },
                )
                .and_then(|()| signal_stream.flush())
                .is_err()
                {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    loop {
        match crate::socket_wire::read_atom::<_, ApplianceFrame>(&mut stream)? {
            ApplianceFrame::NestedOutput { stream: 0, data } => {
                io::stdout().write_all(data.as_slice())?;
                io::stdout().flush()?;
            }
            ApplianceFrame::NestedResult { stream: 0, code } => return Ok(code),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "guest nested-run broker returned an invalid frame",
                ));
            }
        }
    }
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

fn guest_proc_stat(pid: u32) -> Option<(u32, u64)> {
    let raw = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let rest = text
        .get(text.rfind(')')? + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    Some((rest.get(1)?.parse().ok()?, rest.get(19)?.parse().ok()?))
}

fn guest_process_provenance(pid: u32) -> Option<crate::generated_wire::ProcessProvenance> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let tgid = status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:")?.trim().parse::<u32>().ok())?;
    let (ppid, _) = guest_proc_stat(tgid)?;
    let executable = std::fs::read_link(format!("/proc/{tgid}/exe")).ok()?;
    let cwd = std::fs::read_link(format!("/proc/{tgid}/cwd")).ok()?;
    let argv = std::fs::read(format!("/proc/{tgid}/cmdline"))
        .ok()?
        .split(|byte| *byte == 0)
        .filter(|word| !word.is_empty())
        .map(|word| crate::wire::BoundedBytes::new(word.to_vec()).ok())
        .collect::<Option<Vec<_>>>()?;
    if argv.is_empty() {
        return None;
    }
    Some(crate::generated_wire::ProcessProvenance {
        tgid,
        ppid: i32::try_from(ppid).ok()?,
        executable: crate::wire::BoundedBytes::new(executable.as_os_str().as_bytes().to_vec())
            .ok()?,
        cwd: crate::wire::BoundedBytes::new(cwd.as_os_str().as_bytes().to_vec()).ok()?,
        argv: crate::wire::BoundedVec::new(argv).ok()?,
        environment: None,
    })
}

fn guest_descends_from(pid: u32, root: u32, parents: &HashMap<u32, u32>) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        if current == root {
            return true;
        }
        let Some(parent) = parents.get(&current).copied() else {
            return false;
        };
        if parent <= 1 || parent == current {
            return false;
        }
        current = parent;
    }
    false
}

fn observe_guest_processes(
    root: u32,
    epoch: u64,
    writer: &Arc<Mutex<std::fs::File>>,
    seen: &mut HashMap<u32, (u64, Vec<u8>)>,
) {
    let mut parents = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some((ppid, _)) = guest_proc_stat(pid) {
            parents.insert(pid, ppid);
        }
    }
    let mut processes = parents
        .keys()
        .copied()
        .filter(|pid| guest_descends_from(*pid, root, &parents))
        .collect::<Vec<_>>();
    processes.sort_unstable();
    for tgid in processes {
        let Some(provenance) = guest_process_provenance(tgid) else {
            continue;
        };
        let Some((_, start_tick)) = guest_proc_stat(tgid) else {
            continue;
        };
        let start = epoch.saturating_add(start_tick);
        let executable = provenance.executable.as_slice().to_vec();
        let tasks = std::fs::read_dir(format!("/proc/{tgid}/task"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .collect::<Vec<_>>();
        for pid in tasks {
            if seen
                .get(&pid)
                .is_some_and(|value| value == &(start, executable.clone()))
            {
                continue;
            }
            seen.insert(pid, (start, executable.clone()));
            write_appliance_file_frame(
                writer,
                &ApplianceFrame::Process {
                    event: crate::generated_wire::GuestProcessEvent {
                        pid,
                        provenance: provenance.clone(),
                        start,
                    },
                },
            );
        }
    }
}

fn execute_guest(
    command: &ApplianceCommand,
    control: &Arc<Mutex<std::fs::File>>,
) -> io::Result<i32> {
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
    // Every appliance command can launch another flat QEMU box. The broker is
    // local to this guest and carries only typed run/I/O operations to PID 1;
    // PID 1 forwards them to the still-live host outer runner.
    process.env("SARUN_APPLIANCE_BROKER", NESTED_BROKER);
    process.env("SARUN_EXE", "/init");
    if let Some(cwd) = &command.cwd {
        process.current_dir(OsStr::from_bytes(cwd.as_slice()));
    }
    // Route ordinary command output through SarunFs itself. Virtio-fs carries
    // the calling guest TID on every sink write, so the engine records the
    // actual writer and returns the same bytes as live ECHO frames. PID 1's
    // control stream remains separate and cannot be confused with user output.
    let stdout = std::fs::OpenOptions::new()
        .write(true)
        .open("/.slopbox-stdout")
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("open guest stdout capture sink: {error}"),
            )
        })?;
    let stderr = std::fs::OpenOptions::new()
        .write(true)
        .open("/.slopbox-stderr")
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("open guest stderr capture sink: {error}"),
            )
        })?;
    process
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
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
    let process_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mut initial_processes = HashMap::new();
    observe_guest_processes(root as u32, process_epoch, control, &mut initial_processes);
    if initial_processes.is_empty() {
        eprintln!(
            "sarun init: process observer did not see root {root}: stat={:?} provenance={}",
            guest_proc_stat(root as u32),
            guest_process_provenance(root as u32).is_some(),
        );
    }
    let process_observer_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer_stop = process_observer_stop.clone();
    let observer_writer = control.clone();
    let observer = std::thread::spawn(move || {
        let mut seen = initial_processes;
        while !observer_stop.load(std::sync::atomic::Ordering::Relaxed) {
            observe_guest_processes(root as u32, process_epoch, &observer_writer, &mut seen);
            std::thread::sleep(Duration::from_millis(10));
        }
        observe_guest_processes(root as u32, process_epoch, &observer_writer, &mut seen);
    });
    {
        let mut writer = control.lock().unwrap();
        crate::socket_wire::write_atom(&mut *writer, &ApplianceFrame::Ready)?;
        writer.flush()?;
    }
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
    process_observer_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = observer.join();
    let status = root_status.unwrap();
    Ok(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    })
}

fn await_debug_start<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mode: DebugMode,
) -> io::Result<()> {
    if mode == DebugMode::Off {
        return Ok(());
    }
    crate::socket_wire::write_atom(writer, &ApplianceFrame::DebugPrepared)?;
    writer.flush()?;
    match crate::socket_wire::read_atom::<_, ApplianceFrame>(reader)? {
        ApplianceFrame::DebugStart => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "host returned an invalid debug barrier frame",
        )),
    }
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

fn write_guest_outcome<W: io::Write>(stream: &mut W, outcome: io::Result<i32>) -> io::Result<i32> {
    let code = match outcome {
        Ok(code) => code,
        Err(error) => {
            eprintln!("sarun init: {error}");
            127
        }
    };
    crate::socket_wire::write_atom(stream, &ApplianceFrame::Result { code })?;
    stream.flush()?;
    Ok(code)
}

fn translate_guest_stream(frame: ApplianceFrame, stream: u64) -> io::Result<ApplianceFrame> {
    match frame {
        ApplianceFrame::NestedOpen { stream: 0, request } => {
            Ok(ApplianceFrame::NestedOpen { stream, request })
        }
        ApplianceFrame::NestedInput { stream: 0, data } => {
            Ok(ApplianceFrame::NestedInput { stream, data })
        }
        ApplianceFrame::NestedInputEof { stream: 0 } => {
            Ok(ApplianceFrame::NestedInputEof { stream })
        }
        ApplianceFrame::NestedSignal { stream: 0, signal } => {
            Ok(ApplianceFrame::NestedSignal { stream, signal })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local appliance caller sent an invalid frame",
        )),
    }
}

fn write_appliance_file_frame(writer: &Arc<Mutex<std::fs::File>>, frame: &ApplianceFrame) {
    let mut writer = writer.lock().unwrap();
    if let Err(error) =
        crate::socket_wire::write_atom(&mut *writer, frame).and_then(|()| writer.flush())
    {
        eprintln!("sarun init: appliance frame write: {error:?}");
    }
}

fn start_guest_nested_broker(control: &std::fs::File) -> io::Result<Arc<Mutex<std::fs::File>>> {
    use std::os::linux::net::SocketAddrExt;
    let address = std::os::unix::net::SocketAddr::from_abstract_name(NESTED_BROKER.as_bytes())?;
    let listener = std::os::unix::net::UnixListener::bind_addr(&address)?;
    let writer = Arc::new(Mutex::new(control.try_clone()?));
    let clients = Arc::new(Mutex::new(HashMap::<u64, Arc<Mutex<UnixStream>>>::new()));
    let mut reader = control.try_clone()?;
    let reader_clients = clients.clone();
    std::thread::spawn(move || {
        loop {
            let frame = match crate::socket_wire::read_atom::<_, ApplianceFrame>(&mut reader) {
                Ok(frame) => frame,
                Err(_) => break,
            };
            let (stream, local, finished) = match frame {
                ApplianceFrame::NestedOutput { stream, data } => (
                    stream,
                    ApplianceFrame::NestedOutput { stream: 0, data },
                    false,
                ),
                ApplianceFrame::NestedResult { stream, code } => (
                    stream,
                    ApplianceFrame::NestedResult { stream: 0, code },
                    true,
                ),
                _ => continue,
            };
            let client = reader_clients.lock().unwrap().get(&stream).cloned();
            if let Some(client) = client {
                let mut client = client.lock().unwrap();
                let _ = crate::socket_wire::write_atom(&mut *client, &local);
                let _ = client.flush();
            }
            if finished {
                reader_clients.lock().unwrap().remove(&stream);
            }
        }
    });

    let accept_writer = writer.clone();
    std::thread::spawn(move || {
        let next = std::sync::atomic::AtomicU64::new(1);
        for client in listener.incoming().flatten() {
            let stream = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut client_reader = match client.try_clone() {
                Ok(reader) => reader,
                Err(_) => continue,
            };
            let client = Arc::new(Mutex::new(client));
            clients.lock().unwrap().insert(stream, client.clone());
            let writer = accept_writer.clone();
            let clients = clients.clone();
            std::thread::spawn(move || {
                loop {
                    let frame =
                        crate::socket_wire::read_atom::<_, ApplianceFrame>(&mut client_reader);
                    let frame = match frame.and_then(|frame| translate_guest_stream(frame, stream))
                    {
                        Ok(frame) => frame,
                        Err(_) => {
                            write_appliance_file_frame(
                                &writer,
                                &ApplianceFrame::NestedSignal {
                                    stream,
                                    signal: libc::SIGTERM,
                                },
                            );
                            clients.lock().unwrap().remove(&stream);
                            break;
                        }
                    };
                    write_appliance_file_frame(&writer, &frame);
                }
            });
        }
    });
    Ok(writer)
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
        let control = start_guest_nested_broker(&stream)?;
        let guest_outcome = crate::net::tap::configure_appliance_network(command.net_mode)
            .map_err(|error| io::Error::other(format!("configure network: {error}")))
            .and_then(|()| {
                let mut writer = control.lock().unwrap();
                await_debug_start(&mut stream, &mut *writer, command.debug_mode)
            })
            .and_then(|()| execute_guest(&command, &control));
        // The result is the host's safe-to-stop boundary: publish it only
        // after dirty guest pages have reached the shared filesystem. Exec
        // and network failures are results too: the host must never wait for
        // a control reply that PID 1 has already decided it cannot produce.
        unsafe { libc::sync() };
        let mut control = control.lock().unwrap();
        write_guest_outcome(&mut *control, guest_outcome)
    })();
    let code = match outcome {
        Ok(code) => code,
        Err(error) => {
            eprintln!("sarun init: {error}");
            127
        }
    };
    // ACPI-free x86 microvm has no power-off device. Its `reboot=t` command
    // line turns RB_AUTOBOOT into a triple-fault reset, and QEMU's
    // `-no-reboot` makes that reset exit. Aarch64 has a real power-off path.
    let shutdown = if cfg!(target_arch = "x86_64") {
        libc::RB_AUTOBOOT
    } else {
        libc::RB_POWER_OFF
    };
    unsafe { libc::reboot(shutdown) };
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_exec_failure_is_a_framed_result_not_a_missing_reply() {
        let mut encoded = Vec::new();
        let code = write_guest_outcome(
            &mut encoded,
            Err(io::Error::from_raw_os_error(libc::ENOEXEC)),
        )
        .unwrap();
        assert_eq!(code, 127);
        let mut cursor = io::Cursor::new(encoded);
        let result: ApplianceFrame = crate::socket_wire::read_atom(&mut cursor).unwrap();
        assert_eq!(result, ApplianceFrame::Result { code: 127 });
    }

    #[test]
    fn qemu_arguments_keep_architecture_specific_devices_at_the_edge() {
        let (cpus, memory_mib) = appliance_resources();
        let a = qemu_args(
            QemuArchitecture::Aarch64,
            OsStr::new("K"),
            11,
            12,
            Path::new("D"),
            false,
            NetMode::Off,
            None,
            None,
        );
        let a = a
            .iter()
            .map(|v| v.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(a.contains("-machine virt"));
        assert!(a.contains("vhost-user-fs-pci"));
        assert!(a.contains("console=ttyAMA0"));
        assert!(a.contains("socket,id=fs,fd=11"));
        assert!(a.contains("socket,id=control,fd=12"));
        assert!(a.contains(&format!("-smp {cpus}")));
        assert!(a.contains(&format!("-m {memory_mib}M")));
        assert!(a.contains("-accel tcg,thread=multi"));
        let x = qemu_args(
            QemuArchitecture::X8664,
            OsStr::new("K"),
            11,
            12,
            Path::new("D"),
            false,
            NetMode::Tap,
            Some(17),
            None,
        );
        let x = x
            .iter()
            .map(|v| v.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(x.contains("-machine microvm,acpi=off,pcie=off,rtc=off"));
        assert!(x.contains("vhost-user-fs-device"));
        assert!(x.contains("console=ttyS0"));
        assert!(x.contains("-L D"));
        assert!(x.contains("-netdev dgram,id=network,local.type=fd,local.str=17"));
        assert!(x.contains("virtio-net-device"));

        let host = qemu_args(
            QemuArchitecture::Aarch64,
            OsStr::new("K"),
            11,
            12,
            Path::new("D"),
            false,
            NetMode::Host,
            None,
            None,
        );
        let host = host
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(host.contains("-netdev user,id=network"));
        assert!(!host.contains("local.type=fd"));
    }

    #[test]
    fn qemu_debug_endpoint_is_an_inherited_descriptor() {
        let args = qemu_args(
            QemuArchitecture::X8664,
            OsStr::new("/proc/self/fd/19"),
            11,
            12,
            Path::new("D"),
            false,
            NetMode::Off,
            None,
            Some(23),
        )
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
        assert!(args.contains("-kernel /proc/self/fd/19"));
        assert!(args.contains("-chardev socket,id=viros-rsp,fd=23"));
        assert!(args.contains("-gdb chardev:viros-rsp"));
        assert!(!args.contains("-S"));
        assert!(!args.contains("unix:path="));
        assert_eq!(args.matches("-gdb").count(), 1);
    }

    #[test]
    fn opaque_image_profiles_are_fixed_and_begin_stopped() {
        use crate::generated_wire::DebugImageProfile;
        let aarch64 = qemu_image_args(
            QemuArchitecture::Aarch64,
            DebugImageProfile::VirtInitramfsAarch64V1,
            OsStr::new("/proc/self/fd/19"),
            OsStr::new("/proc/self/fd/20"),
            "/sbin/init",
            Path::new("D"),
            false,
            23,
        )
        .unwrap()
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
        assert!(aarch64.contains("-machine virt"));
        assert!(aarch64.contains("-initrd /proc/self/fd/20"));
        assert!(aarch64.contains("console=ttyAMA0 rdinit=/sbin/init"));
        assert!(aarch64.ends_with("-S"));
        assert!(!aarch64.contains("sarun-root"));
        assert!(!aarch64.contains("sarun-control"));

        let x86 = qemu_image_args(
            QemuArchitecture::X8664,
            DebugImageProfile::MicrovmInitramfsX8664V1,
            OsStr::new("K"),
            OsStr::new("I"),
            "/sbin/init",
            Path::new("D"),
            false,
            23,
        )
        .unwrap()
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
        assert!(x86.contains("microvm,acpi=off,pcie=off,rtc=off"));
        assert!(x86.contains("console=ttyS0 rdinit=/sbin/init"));
        assert!(x86.contains("-L D"));
        let arm = qemu_image_args(
            QemuArchitecture::Arm,
            DebugImageProfile::VirtInitramfsArmV1,
            OsStr::new("K"),
            OsStr::new("I"),
            "/init",
            Path::new("D"),
            false,
            23,
        )
        .unwrap()
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
        assert!(arm.contains("-machine virt"));
        assert!(arm.contains("-cpu cortex-a15"));
        assert!(arm.contains("-smp 1"));
        assert!(arm.contains("-m 512M"));
        assert!(arm.contains("console=ttyAMA0 rdinit=/init"));
        assert!(!arm.contains("memory-backend-memfd"));

        let mmips = qemu_image_args(
            QemuArchitecture::Mmips,
            DebugImageProfile::MaltaInitramfsMipselV1,
            OsStr::new("K"),
            OsStr::new("I"),
            "/init",
            Path::new("D"),
            false,
            23,
        )
        .unwrap()
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
        assert!(mmips.contains("-machine malta"));
        assert!(mmips.contains("-cpu 24Kc"));
        assert!(mmips.contains("-smp 1"));
        assert!(mmips.contains("-m 256M"));
        assert!(mmips.contains("console=ttyS0,115200 rdinit=/init"));
        assert!(!mmips.contains("memory-backend-memfd"));
        assert!(
            qemu_image_args(
                QemuArchitecture::Aarch64,
                DebugImageProfile::MicrovmInitramfsX8664V1,
                OsStr::new("K"),
                OsStr::new("I"),
                "/sbin/init",
                Path::new("D"),
                false,
                23,
            )
            .is_err()
        );
    }

    #[test]
    fn appliance_resources_support_parallel_builds_without_unbounded_guests() {
        let (cpus, memory_mib) = appliance_resources();
        assert!((1..=MAX_APPLIANCE_CPUS).contains(&cpus));
        assert!((512..=4096).contains(&memory_mib));
        assert!(memory_mib >= 256 + cpus * 128);
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
            net_mode: NetMode::Off,
            debug_mode: DebugMode::Off,
        };
        let mut bytes = Vec::new();
        crate::socket_wire::write_versioned(&mut bytes, &value).unwrap();
        let decoded: ApplianceCommand =
            crate::socket_wire::read_versioned(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn brush_appliance_command_carries_fixed_runtime_environment() {
        let command = wire_command(
            &["/init".into(), "brush-sh".into()],
            Some("/work"),
            crate::net::NetMode::Off,
            true,
            DebugMode::Off,
        )
        .unwrap();
        let environment = command.environment.as_map();
        let value = |name: &[u8]| {
            environment
                .iter()
                .find(|(key, _)| key.as_slice() == name)
                .map(|(_, value)| value.as_slice())
        };
        assert_eq!(value(b"SARUN_BRUSH_SH"), None);
        assert_eq!(value(b"SARUN_EXE"), Some(b"/init".as_slice()));
        assert_eq!(
            command.cwd.as_ref().map(|value| value.as_slice()),
            Some(b"/work".as_slice())
        );
    }

    #[test]
    fn attach_barrier_requires_the_explicit_host_start_frame() {
        let (mut guest, mut host) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || {
            let frame: ApplianceFrame = crate::socket_wire::read_atom(&mut host).unwrap();
            assert_eq!(frame, ApplianceFrame::DebugPrepared);
            crate::socket_wire::write_atom(&mut host, &ApplianceFrame::DebugStart).unwrap();
            host.flush().unwrap();
        });
        let mut writer = guest.try_clone().unwrap();
        await_debug_start(&mut guest, &mut writer, DebugMode::AttachBeforeCommand).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn runner_barrier_waits_for_engine_debug_start() {
        let (runner, mut engine) = UnixStream::pair().unwrap();
        let channel = ApplianceBoxChannel::new(runner).unwrap();
        let waiter = {
            let channel = channel.clone();
            std::thread::spawn(move || channel.await_debug_start())
        };
        let expected = crate::frames::encode(crate::frames::FRAME_DEBUG_PREPARED, &[]);
        let mut actual = vec![0; expected.len()];
        engine.read_exact(&mut actual).unwrap();
        assert_eq!(actual, expected);
        assert!(!waiter.is_finished());
        engine
            .write_all(&crate::frames::encode(
                crate::frames::FRAME_DEBUG_START,
                &[],
            ))
            .unwrap();
        assert!(waiter.join().unwrap().is_ok());
    }

    #[test]
    fn runner_barrier_fails_when_engine_disconnects() {
        let (runner, engine) = UnixStream::pair().unwrap();
        let channel = ApplianceBoxChannel::new(runner).unwrap();
        drop(engine);
        assert!(channel.await_debug_start().is_err());
    }

    #[test]
    fn packet_socket_lane_preserves_frame_boundaries() {
        let (engine, qemu) = packet_socket_pair().unwrap();
        let frame = b"ethernet frame";
        assert_eq!(
            unsafe { libc::write(qemu.as_raw_fd(), frame.as_ptr().cast(), frame.len()) },
            frame.len() as isize
        );
        let mut bytes = [0; 64];
        let read =
            unsafe { libc::read(engine.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        assert!(read >= 0);
        let read = read as usize;
        assert_eq!(&bytes[..read], b"ethernet frame");
    }
}
