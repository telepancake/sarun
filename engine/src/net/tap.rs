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

/// The gateway MAC: what the engine's smoltcp answers as, AND what the runner
/// seeds into the box's ARP cache. `StackRuntime::start` must be given the
/// same value.
pub fn gateway_mac() -> [u8; 6] { derive_gw_mac(BOX_SUBNET_ID) }

/// RUNNER-side: move THIS process into a fresh network namespace and equip it
/// with loopback + a TAP at the fixed box address, returning the TAP fd to hand
/// to the engine (which polls it). The caller then execs bwrap WITHOUT
/// --unshare-net so the box inherits this netns. Requires CAP_NET_ADMIN /
/// CAP_SYS_ADMIN in the caller's user namespace — the runner already clears that
/// bar (its bwrap child `setns`'d a netns before this change).
pub fn create_netns_tap() -> Result<OwnedFd> {
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        bail!("unshare(CLONE_NEWNET): {}", std::io::Error::last_os_error());
    }
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
