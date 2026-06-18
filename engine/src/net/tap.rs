// Fork a netns-anchor child: it `unshare(CLONE_NEWNET)`s into a fresh netns,
// brings up loopback, creates a TAP, assigns the gateway IP, sets link up,
// pins /proc/self/ns/net (so the engine can `setns` later), and ships the
// TAP fd + a dup of its netns fd back over a socketpair as SCM_RIGHTS. Then
// it waits on a control-socket EOF (its only purpose now is to keep the
// netns alive for bwrap to inherit by setns'ing into /proc/<pid>/ns/net).
//
// The engine (parent) sends a NetHandle back to runner.rs:
//   • bwrap is spawned with `setns()` on the anchor's netns_fd before exec;
//     it does NOT pass --unshare-net, so it inherits the netns the engine
//     pre-equipped.
//   • The TAP fd stays in the engine process; smoltcp polls it.
//
// Why an anchor child at all: bwrap has no `--netns-fd` flag. To put a
// process into a pre-existing netns we'd need to be IN that netns when we
// fork the bwrap child. The anchor model is the simplest way to keep the
// netns referenced (via /proc/<pid>/ns/net) for as long as the box runs.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::subnet::BoxSubnet;

/// What the engine gets back per box.
pub struct AnchorRig {
    pub anchor_pid: i32,
    pub netns_path: PathBuf,
    pub tap_fd: OwnedFd,
    pub tap_name: String,
    pub mac: [u8; 6],
}

/// Spawn a netns-anchor child for `subnet` and wait for its handoff.
pub fn spawn_anchor(subnet: BoxSubnet) -> Result<AnchorRig> {
    // socketpair for the handoff
    let mut sv = [0i32; 2];
    let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                                      0, sv.as_mut_ptr()) };
    if r != 0 { bail!("socketpair: {}", std::io::Error::last_os_error()); }
    let parent_sock = unsafe { OwnedFd::from_raw_fd(sv[0]) };
    let child_sock = unsafe { OwnedFd::from_raw_fd(sv[1]) };

    let mac: [u8; 6] = {
        use rand::RngCore;
        let mut buf = [0u8; 6];
        rand::thread_rng().fill_bytes(&mut buf);
        // Locally administered, unicast.
        buf[0] = (buf[0] & 0xFE) | 0x02;
        buf
    };

    let pid = unsafe { libc::fork() };
    if pid < 0 { bail!("fork: {}", std::io::Error::last_os_error()); }
    if pid == 0 {
        // Child path: never returns (exec or _exit).
        drop(parent_sock);
        anchor_child_main(child_sock, subnet, mac);
    }

    // Parent: receive (tap_fd, netns_fd) over the socket.
    drop(child_sock);
    let (tap_fd, netns_fd, tap_name) = recv_handoff(&parent_sock)
        .context("recv anchor handoff")?;
    // We re-open the netns by path (more durable than holding the dup) — the
    // path is /proc/<pid>/ns/net and stays valid while the anchor is alive.
    drop(netns_fd); // we only need the path; the anchor keeps the fd open
    let netns_path = PathBuf::from(format!("/proc/{}/ns/net", pid));
    Ok(AnchorRig { anchor_pid: pid, netns_path, tap_fd, tap_name, mac })
}

const TAP_HANDOFF_KIND: u8 = 1;

fn recv_handoff(sock: &OwnedFd) -> Result<(OwnedFd, OwnedFd, String)> {
    // Frame: 1 byte kind | 4 byte BE name-len | name-bytes | 2 fds via SCM_RIGHTS.
    let mut buf = [0u8; 5 + 64];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cmsg_buf = [0u8; unsafe { libc::CMSG_SPACE(8) } as usize];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_buf.len() as _;
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
    if n < 0 { bail!("recvmsg: {}", std::io::Error::last_os_error()); }
    if (n as usize) < 5 { bail!("short handoff"); }
    if buf[0] != TAP_HANDOFF_KIND { bail!("bad handoff kind"); }
    let nlen = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if 5 + nlen > n as usize { bail!("name overflow"); }
    let tap_name = std::str::from_utf8(&buf[5..5 + nlen])
        .context("utf8 tap name")?.to_string();
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() { bail!("no cmsg"); }
    let (lvl, ty, len) = unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type, (*cmsg).cmsg_len) };
    if lvl != libc::SOL_SOCKET || ty != libc::SCM_RIGHTS {
        bail!("unexpected cmsg ({lvl}/{ty})");
    }
    let data_len = len as usize - unsafe { libc::CMSG_LEN(0) } as usize;
    if data_len < 8 { bail!("short cmsg fds"); }
    let mut fds = [0i32; 2];
    unsafe {
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg).cast::<i32>(),
                                      fds.as_mut_ptr(), 2);
    }
    let tap = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let ns = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((tap, ns, tap_name))
}

/// The anchor child's main. Never returns: either `_exit` on failure or
/// `pause()` to keep the netns referenced.
fn anchor_child_main(sock: OwnedFd, subnet: BoxSubnet, mac: [u8; 6]) -> ! {
    let res = (|| -> Result<()> {
        // 1. Unshare into a new netns.
        let r = unsafe { libc::unshare(libc::CLONE_NEWNET) };
        if r != 0 { bail!("unshare(CLONE_NEWNET): {}", std::io::Error::last_os_error()); }

        // 2. Open /proc/self/ns/net to dup back to the parent.
        let netns = std::fs::File::open("/proc/self/ns/net").context("open ns/net")?;
        let netns_fd: OwnedFd = netns.into();

        // 3. Bring loopback up + create + configure TAP via netlink (or fall
        //    back to ioctl). For now use ioctl + /sbin/ip-style assistance.
        let tap_name = format!("tap{}", subnet.id);
        let tap = open_tap(&tap_name).context("open TAP")?;
        bring_link_up("lo")?;
        set_mac(&tap_name, mac)?;
        set_link_up(&tap_name)?;
        assign_ip(&tap_name, subnet.gateway_ip(), subnet.prefix_len())?;

        // 4. Hand off (tap_fd, netns_fd) + tap_name.
        send_handoff(&sock, &tap, &netns_fd, &tap_name)?;
        Ok(())
    })();
    if let Err(e) = res {
        let msg = format!("anchor child: {e}\n");
        unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()); }
        unsafe { libc::_exit(1); }
    }
    // 5. Park forever; the engine's drop of NetHandle SIGTERMs us, which
    //    releases the last /proc/<pid>/ns/net reference and tears the netns
    //    + TAP down. (We deliberately keep the TAP fd locally too so the
    //    interface doesn't disappear out from under bwrap.)
    loop { unsafe { libc::pause(); } }
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
    let r = unsafe { libc::ioctl(owned.as_raw_fd(), TUNSETIFF, &mut req) };
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
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCGIFFLAGS, &mut req) };
    if r < 0 { bail!("SIOCGIFFLAGS: {}", std::io::Error::last_os_error()); }
    Ok(req.flags)
}

fn set_link_up(name: &str) -> Result<()> {
    let s = ioctl_sock()?;
    let mut req: IfreqFlags = unsafe { std::mem::zeroed() };
    write_name(&mut req.name, name)?;
    const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
    const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
    let _ = unsafe { libc::ioctl(s.as_raw_fd(), SIOCGIFFLAGS, &mut req) };
    req.flags |= 0x1; // IFF_UP
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSIFFLAGS, &mut req) };
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
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSIFHWADDR, &req) };
    if r < 0 { bail!("SIOCSIFHWADDR: {}", std::io::Error::last_os_error()); }
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
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSIFADDR, &req) };
    if r < 0 { bail!("SIOCSIFADDR({name}): {}", std::io::Error::last_os_error()); }
    // Netmask.
    let mut req: IfreqAddr = unsafe { std::mem::zeroed() };
    write_name(&mut req.name, name)?;
    req.addr.sin_family = libc::AF_INET as u16;
    let mask: u32 = if prefix == 0 { 0 } else { !0u32 << (32 - prefix) };
    req.addr.sin_addr = libc::in_addr { s_addr: mask.to_be() };
    const SIOCSIFNETMASK: libc::c_ulong = 0x891c;
    let r = unsafe { libc::ioctl(s.as_raw_fd(), SIOCSIFNETMASK, &req) };
    if r < 0 { bail!("SIOCSIFNETMASK({name}): {}", std::io::Error::last_os_error()); }
    let _ = ifflags(name)?; // suppress unused warning if it ever drifts
    Ok(())
}

fn send_handoff(sock: &OwnedFd, tap: &OwnedFd, ns: &OwnedFd, name: &str)
                -> Result<()> {
    let nb = name.as_bytes();
    if nb.len() >= 64 { bail!("tap name too long"); }
    let mut buf = vec![TAP_HANDOFF_KIND];
    buf.extend_from_slice(&(nb.len() as u32).to_be_bytes());
    buf.extend_from_slice(nb);
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cmsg_buf = [0u8; unsafe { libc::CMSG_SPACE(8) } as usize];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_buf.len() as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(8) as _;
        let fds = [tap.as_raw_fd(), ns.as_raw_fd()];
        std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(),
                                      libc::CMSG_DATA(c), 8);
    }
    let r = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
    if r < 0 { bail!("sendmsg handoff: {}", std::io::Error::last_os_error()); }
    Ok(())
}
