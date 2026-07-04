// Per-box networking = a fresh network namespace with ONE TAP device wired to
// a userland smoltcp stack the ENGINE drives. The netns and the TAP are created
// by the RUNNER, not the engine: the runner is the process that execs bwrap, so
// it can simply `unshare(CLONE_NEWNET)` and build the TAP right here, hand the
// TAP fd to the engine over the register channel (SCM_RIGHTS), then exec bwrap
// WITHOUT --unshare-net so the box inherits the runner's netns. The engine only
// polls the fd. No forked anchor, no /proc/<pid>/ns/net path handoff.
//
// Every box uses the SAME fixed addresses + MAC: each TAP is alone in its own
// netns, so there is nothing to disambiguate at L2/L3, and the engine keys each
// box's stack/flows/pcap by box id, never by address.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{Result, bail};

use super::subnet::BoxSubnet;

/// The fixed subnet every box's TAP is addressed from: box = .0.2, gateway =
/// .0.1, the synth-DNS pool spread across the rest of the /16. Identical for
/// every box (netns isolation makes per-box ranges pointless).
pub const BOX_SUBNET_ID: u16 = 1;

/// The box-side TAP hardware address. Cosmetic to the engine (smoltcp answers
/// as the gateway MAC); a stable locally-administered value just so the
/// interface has one.
pub const BOX_MAC: [u8; 6] = [0x02, 0x73, 0x72, 0x6e, 0x00, 0x02];

/// The fixed per-box subnet (see `BOX_SUBNET_ID`).
pub fn box_subnet() -> BoxSubnet { BoxSubnet::new(BOX_SUBNET_ID) }

/// Whether tap networking can actually work here — the SAME two gates the real
/// setup must pass: (1) getting a configurable network namespace (a bare
/// `unshare(CLONE_NEWNET)`, else via our own user namespace for the rootless
/// case — see `unshare_netns`), and (2) opening `/dev/net/tun` (TUNSETIFF), the
/// device-permission gate that a userns can't override when the node is
/// root-only. Probed in a throwaway forked child so a failure just means
/// "announce host fallback" instead of spawning a box that dies.
///
/// The child runs ONLY async-signal-safe code (raw syscalls, no heap): the
/// parent (the UI) may be multithreaded, and malloc after fork can deadlock.
/// That's why this re-implements the `unshare_netns` sequence with bare
/// syscalls rather than calling it. Probed once per process.
pub fn tap_available() -> bool {
    static PROBE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PROBE.get_or_init(|| unsafe {
        match libc::fork() {
            0 => libc::_exit(if probe_tap_child() { 0 } else { 1 }),
            -1 => true, // probe impossible — don't claim unavailability
            pid => {
                let mut st = 0;
                while libc::waitpid(pid, &mut st, 0) == -1
                    && *libc::__errno_location() == libc::EINTR {}
                libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0
            }
        }
    })
}

/// The probe body, run in the forked child. Async-signal-safe: no allocation.
/// Mirrors `unshare_netns` + `open_tap`. Returns true iff both gates pass.
unsafe fn probe_tap_child() -> bool {
    if libc::unshare(libc::CLONE_NEWNET) != 0 {
        if *libc::__errno_location() != libc::EPERM { return false; }
        // Rootless: gain the cap via a user namespace, then retry the netns.
        if libc::unshare(libc::CLONE_NEWUSER) != 0 { return false; }
        let _ = write_raw(b"/proc/self/setgroups\0", b"deny"); // best-effort
        if !write_id_map(b"/proc/self/uid_map\0", libc::getuid()) { return false; }
        if !write_id_map(b"/proc/self/gid_map\0", libc::getgid()) { return false; }
        if libc::unshare(libc::CLONE_NEWNET) != 0 { return false; }
    }
    // Device-permission gate: can we actually open + configure a TAP?
    let fd = libc::open(b"/dev/net/tun\0".as_ptr().cast(),
                        libc::O_RDWR | libc::O_CLOEXEC);
    if fd < 0 { return false; }
    #[repr(C)]
    struct Ifreq { name: [u8; 16], flags: i16, _pad: [u8; 22] }
    let mut req: Ifreq = std::mem::zeroed();
    req.name[..4].copy_from_slice(b"tap0");
    req.flags = (0x0002 | 0x1000) as i16; // IFF_TAP | IFF_NO_PI
    const TUNSETIFF: libc::c_ulong = 0x400454ca;
    let ok = libc::ioctl(fd, TUNSETIFF as _, &mut req) == 0;
    libc::close(fd);
    ok
}

/// Write the identity id-map `"<id> <id> 1"` to `path` with raw syscalls (no
/// heap). `path` is NUL-terminated. Returns whether the write succeeded.
unsafe fn write_id_map(path: &[u8], id: libc::uid_t) -> bool {
    // Compose "<id> <id> 1\n" into a stack buffer.
    let mut buf = [0u8; 32];
    let mut n = 0;
    let mut push_uint = |buf: &mut [u8], n: &mut usize, mut v: u32| {
        let mut digits = [0u8; 10];
        let mut d = 0;
        if v == 0 { digits[0] = b'0'; d = 1; }
        while v > 0 { digits[d] = b'0' + (v % 10) as u8; v /= 10; d += 1; }
        while d > 0 { d -= 1; buf[*n] = digits[d]; *n += 1; }
    };
    push_uint(&mut buf, &mut n, id as u32);
    buf[n] = b' '; n += 1;
    push_uint(&mut buf, &mut n, id as u32);
    for &b in b" 1\n" { buf[n] = b; n += 1; }
    write_raw(path, &buf[..n])
}

/// Open `path` (NUL-terminated) and write `data` in full. Raw syscalls only.
unsafe fn write_raw(path: &[u8], data: &[u8]) -> bool {
    let fd = libc::open(path.as_ptr().cast(), libc::O_WRONLY);
    if fd < 0 { return false; }
    let r = libc::write(fd, data.as_ptr().cast(), data.len());
    libc::close(fd);
    r == data.len() as isize
}

/// The gateway MAC: what the engine's smoltcp answers as, AND what the runner
/// seeds into the box's ARP cache. `StackRuntime::start` must be given the
/// same value.
pub fn gateway_mac() -> [u8; 6] { derive_gw_mac(BOX_SUBNET_ID) }

/// RUNNER-side: move THIS process into a fresh network namespace and equip it
/// with loopback + a TAP at the fixed box address, returning the TAP fd to hand
/// to the engine (which polls it). The caller then execs bwrap WITHOUT
/// --unshare-net so the box inherits this netns. Needs no root: `unshare_netns`
/// self-acquires CAP_NET_ADMIN via an unprivileged user namespace when the
/// process doesn't already have it (the rootless top-level case).
pub fn create_netns_tap() -> Result<OwnedFd> {
    unshare_netns()?;
    let subnet = box_subnet();
    let tap_name = "tap0";
    // The IP on the TAP is the BOX's address (.0.2); the gateway (.0.1) lives on
    // the engine side, served entirely by smoltcp. A default route via the
    // gateway makes the box's normal dial paths work without a dhcp client.
    let tap = open_tap(tap_name)?;
    bring_link_up("lo")?;
    set_mac(tap_name, BOX_MAC)?;
    set_link_up(tap_name)?;
    assign_ip(tap_name, subnet.box_ip(), subnet.box_prefix_len())?;
    add_default_route(subnet.gateway_ip())?;
    // Seed the gateway MAC so the box's first packet doesn't stall on ARP.
    // Best-effort by design (the box would ARP-resolve anyway), but a failure
    // can flag a permissions/kernel issue worth seeing — log, don't abort.
    if let Err(e) = add_neigh(subnet.gateway_ip(), gateway_mac(), tap_name) {
        eprintln!("sarun-engine: net: ARP seed for gateway failed: {e}");
    }
    Ok(tap)
}

/// Move THIS process into a fresh, configurable network namespace.
///
/// A bare `unshare(CLONE_NEWNET)` needs CAP_NET_ADMIN — satisfied when we run
/// as root, OR when we're already inside a user namespace that granted it (a
/// NESTED box, running under its parent's bwrap userns). In the ordinary
/// ROOTLESS top-level case it is refused (EPERM): an unprivileged user in the
/// initial userns has no such capability.
///
/// The fix costs nothing and needs no root: the creator of a user namespace
/// holds EVERY capability INSIDE it, whatever the uid mapping. So on EPERM we
/// first `unshare(CLONE_NEWUSER)` — with an IDENTITY map (your uid/gid → the
/// same values), so the box keeps running as YOU, matching host/off boxes
/// (bwrap maps identity too). That grants CAP_NET_ADMIN in the new userns and
/// the netns unshare then succeeds. We open + `TUNSETIFF` the TAP in THIS
/// process (no exec in between), so those caps are still held — an identity
/// map would drop them across an execve, but we never exec here. bwrap, run
/// later, makes its own nested userns for the box and inherits this netns
/// (it is run WITHOUT --unshare-net).
/// Move THIS process into a fresh (empty) network namespace, acquiring the
/// capability via a user namespace when unprivileged. Used by `--net off`
/// sud boxes to get a namespace where every dial fails closed (the sud
/// wrapper has no bwrap `--unshare-net` to do it). Public so the runner can
/// call it directly.
pub fn unshare_netns() -> Result<()> {
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } == 0 {
        return Ok(());
    }
    let bare = std::io::Error::last_os_error();
    if bare.raw_os_error() != Some(libc::EPERM) {
        bail!("unshare(CLONE_NEWNET): {bare}");
    }
    // Rootless: acquire the capability via our own user namespace, then retry.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        bail!("unshare(CLONE_NEWUSER) (for rootless tap): {}",
              std::io::Error::last_os_error());
    }
    // setgroups must be denied before an unprivileged gid_map write.
    let _ = std::fs::write("/proc/self/setgroups", "deny");
    std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))
        .map_err(|e| anyhow::anyhow!("write uid_map (rootless tap): {e}"))?;
    std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))
        .map_err(|e| anyhow::anyhow!("write gid_map (rootless tap): {e}"))?;
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        bail!("unshare(CLONE_NEWNET) after userns: {}",
              std::io::Error::last_os_error());
    }
    Ok(())
}

fn open_tap(name: &str) -> Result<OwnedFd> {
    let fd = unsafe { libc::open(b"/dev/net/tun\0".as_ptr().cast(),
                                  libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if fd < 0 { bail!("open /dev/net/tun: {}", std::io::Error::last_os_error()); }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    // struct ifreq { char ifr_name[16]; short ifr_flags; ... }
    #[repr(C)]
    struct Ifreq { name: [u8; 16], flags: i16, _pad: [u8; 22] }
    let mut req: Ifreq = unsafe { std::mem::zeroed() };
    let nb = name.as_bytes();
    if nb.len() >= req.name.len() { bail!("tap name too long"); }
    req.name[..nb.len()].copy_from_slice(nb);
    // IFF_TAP | IFF_NO_PI (no extra packet info header on each read)
    req.flags = (0x0002 | 0x1000) as i16;
    const TUNSETIFF: libc::c_ulong = 0x400454ca;
    let r = unsafe { libc::ioctl(owned.as_raw_fd(), TUNSETIFF as _, &mut req) };
    if r < 0 { bail!("TUNSETIFF: {}", std::io::Error::last_os_error()); }
    Ok(owned)
}

fn ioctl_sock() -> Result<OwnedFd> {
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if s < 0 { bail!("socket AF_INET: {}", std::io::Error::last_os_error()); }
    Ok(unsafe { OwnedFd::from_raw_fd(s) })
}

#[repr(C)]
struct IfreqFlags { name: [u8; 16], flags: i16, _pad: [u8; 22] }
#[repr(C)]
struct IfreqAddr { name: [u8; 16], addr: libc::sockaddr_in, _pad: [u8; 8] }
#[repr(C)]
struct IfreqHw  { name: [u8; 16], hwaddr: libc::sockaddr, _pad: [u8; 8] }

fn write_name(buf: &mut [u8; 16], name: &str) -> Result<()> {
    let nb = name.as_bytes();
    if nb.len() >= buf.len() { bail!("ifname too long"); }
    buf[..nb.len()].copy_from_slice(nb);
    Ok(())
}

fn ifflags(name: &str) -> Result<i16> {
    let s = ioctl_sock()?;
    let mut req: IfreqFlags = unsafe { std::mem::zeroed() };
    write_name(&mut req.name, name)?;
    const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCGIFFLAGS as _, &mut req) };
    if r < 0 { bail!("SIOCGIFFLAGS: {}", std::io::Error::last_os_error()); }
    Ok(req.flags)
}

fn set_link_up(name: &str) -> Result<()> {
    let s = ioctl_sock()?;
    let mut req: IfreqFlags = unsafe { std::mem::zeroed() };
    write_name(&mut req.name, name)?;
    const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
    const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
    // Read existing flags first so the SET below preserves them. If the GET
    // fails we'd otherwise SET from a zeroed struct, clobbering every other
    // flag on the interface — bail instead of silently doing that.
    let g = unsafe { libc::ioctl(s.as_raw_fd(), SIOCGIFFLAGS as _, &mut req) };
    if g < 0 { bail!("SIOCGIFFLAGS up({name}): {}", std::io::Error::last_os_error()); }
    req.flags |= 0x1; // IFF_UP
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSIFFLAGS as _, &mut req) };
    if r < 0 { bail!("SIOCSIFFLAGS up({name}): {}", std::io::Error::last_os_error()); }
    Ok(())
}

fn bring_link_up(name: &str) -> Result<()> { set_link_up(name) }

fn set_mac(name: &str, mac: [u8; 6]) -> Result<()> {
    let s = ioctl_sock()?;
    let mut req: IfreqHw = unsafe { std::mem::zeroed() };
    write_name(&mut req.name, name)?;
    req.hwaddr.sa_family = 1; // ARPHRD_ETHER
    for i in 0..6 { req.hwaddr.sa_data[i] = mac[i] as libc::c_char; }
    const SIOCSIFHWADDR: libc::c_ulong = 0x8924;
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSIFHWADDR as _, &req) };
    if r < 0 { bail!("SIOCSIFHWADDR: {}", std::io::Error::last_os_error()); }
    Ok(())
}

/// Match control.rs::derive_gw_mac. Kept here too so the anchor child can
/// seed the box's ARP cache without an IPC round-trip.
fn derive_gw_mac(box_id: u16) -> [u8; 6] {
    [0x02, 0x73, 0x72, 0x6e, (box_id >> 8) as u8, (box_id & 0xff) as u8]
}

/// `ip route add default via <gw>` via SIOCADDRT.
fn add_default_route(gw: [u8; 4]) -> Result<()> {
    let s = ioctl_sock()?;
    #[repr(C)]
    struct Rtentry {
        rt_pad1: libc::c_ulong,
        rt_dst: libc::sockaddr,
        rt_gateway: libc::sockaddr,
        rt_genmask: libc::sockaddr,
        rt_flags: libc::c_ushort,
        rt_pad2: libc::c_short,
        rt_pad3: libc::c_ulong,
        rt_tos: libc::c_uchar,
        rt_class: libc::c_uchar,
        rt_pad4: [libc::c_short; 3],
        rt_metric: libc::c_short,
        rt_dev: *mut libc::c_char,
        rt_mtu: libc::c_ulong,
        rt_window: libc::c_ulong,
        rt_irtt: libc::c_ushort,
    }
    let mut r: Rtentry = unsafe { std::mem::zeroed() };
    let mk = |ip: [u8; 4]| -> libc::sockaddr {
        let s_in = libc::sockaddr_in {
            sin_family: libc::AF_INET as u16,
            sin_port: 0,
            sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(ip) },
            sin_zero: [0; 8],
        };
        unsafe { std::mem::transmute(s_in) }
    };
    r.rt_dst = mk([0, 0, 0, 0]);
    r.rt_genmask = mk([0, 0, 0, 0]);
    r.rt_gateway = mk(gw);
    // RTF_UP | RTF_GATEWAY
    r.rt_flags = 0x0001 | 0x0002;
    const SIOCADDRT: libc::c_ulong = 0x890B;
    let rc = unsafe { libc::ioctl(s.as_raw_fd(), SIOCADDRT as _, &r) };
    if rc < 0 { bail!("SIOCADDRT default→{:?}: {}", gw, std::io::Error::last_os_error()); }
    Ok(())
}

/// `ip neigh add <gw> lladdr <mac> dev <ifname> nud permanent` via SIOCSARP.
fn add_neigh(ip: [u8; 4], mac: [u8; 6], ifname: &str) -> Result<()> {
    let s = ioctl_sock()?;
    #[repr(C)]
    struct Arpreq {
        arp_pa: libc::sockaddr,
        arp_ha: libc::sockaddr,
        arp_flags: libc::c_int,
        arp_netmask: libc::sockaddr,
        arp_dev: [u8; 16],
    }
    let mut a: Arpreq = unsafe { std::mem::zeroed() };
    let s_in = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(ip) },
        sin_zero: [0; 8],
    };
    a.arp_pa = unsafe { std::mem::transmute(s_in) };
    a.arp_ha.sa_family = 1; // ARPHRD_ETHER
    for i in 0..6 { a.arp_ha.sa_data[i] = mac[i] as libc::c_char; }
    a.arp_flags = 0x02 | 0x04;  // ATF_COM | ATF_PERM
    write_name(&mut a.arp_dev, ifname)?;
    const SIOCSARP: libc::c_ulong = 0x8955;
    let rc = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSARP as _, &a) };
    if rc < 0 { bail!("SIOCSARP {:?}: {}", ip, std::io::Error::last_os_error()); }
    Ok(())
}

fn assign_ip(name: &str, ip: [u8; 4], prefix: u8) -> Result<()> {
    let s = ioctl_sock()?;
    // Address.
    let mut req: IfreqAddr = unsafe { std::mem::zeroed() };
    write_name(&mut req.name, name)?;
    req.addr.sin_family = libc::AF_INET as u16;
    req.addr.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(ip) };
    const SIOCSIFADDR: libc::c_ulong = 0x8916;
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSIFADDR as _, &req) };
    if r < 0 { bail!("SIOCSIFADDR({name}): {}", std::io::Error::last_os_error()); }
    // Netmask.
    let mut req: IfreqAddr = unsafe { std::mem::zeroed() };
    write_name(&mut req.name, name)?;
    req.addr.sin_family = libc::AF_INET as u16;
    let mask: u32 = if prefix == 0 { 0 } else { !0u32 << (32 - prefix) };
    req.addr.sin_addr = libc::in_addr { s_addr: mask.to_be() };
    const SIOCSIFNETMASK: libc::c_ulong = 0x891c;
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSIFNETMASK as _, &req) };
    if r < 0 { bail!("SIOCSIFNETMASK({name}): {}", std::io::Error::last_os_error()); }
    let _ = ifflags(name)?; // suppress unused warning if it ever drifts
    Ok(())
}
