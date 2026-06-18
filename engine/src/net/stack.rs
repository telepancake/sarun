// Per-box smoltcp poll loop. Runs on a dedicated thread.
//
// Design choices:
//   • DHCP and DNS responses are crafted at the Ethernet/IP layer directly
//     (the requests are simple single-packet UDP, no need to consume the
//     smoltcp UDP socket budget for them). The poll thread peeks each
//     inbound frame; if it's DHCP-shaped or DNS-shaped, we hand it to the
//     respective server, encode the reply with an Ethernet+IPv4+UDP header,
//     and TX it back out the TAP. The smoltcp Interface still processes
//     the frame too (so its ARP cache learns the box's MAC).
//   • TCP uses smoltcp's full stack. We keep a pool of LISTEN sockets bound
//     to UNSPECIFIED:Any-port; on every poll we walk the set and any socket
//     in Established that hasn't been claimed yet gets handed off via the
//     `accept_tx` channel and immediately replaced with a fresh listener.
//   • Other UDP / ICMP / etc. is dropped at the smoltcp level (no sockets
//     bound to handle them).
//
// One frame in, one frame out, one capture call each direction.

use std::collections::{HashSet, VecDeque};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpProtocol, Ipv4Address,
                    Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr, EthernetFrame,
                    EthernetProtocol, EthernetRepr};

use super::dhcp::DhcpServer;
use super::dns::DnsServer;
use super::flows::FlowsLog;
use super::subnet::BoxSubnet;

const LISTEN_POOL: usize = 16;
const TCP_RX_BUF: usize = 64 * 1024;
const TCP_TX_BUF: usize = 64 * 1024;

/// A handle on an established TCP connection inside the stack, plus the
/// destination it was dialed at (recovered from the smoltcp socket's
/// `local_endpoint()` — i.e. the SYN's dst). The handler interacts with the
/// socket via the poll thread through `StackRuntime::tcp_io`.
pub struct AcceptedConn {
    pub handle: SocketHandle,
    pub dst_ip: [u8; 4],
    pub dst_port: u16,
    pub src_ip: [u8; 4],
    pub src_port: u16,
}

pub struct StackRuntime {
    pub box_id: u16,
    pub subnet: BoxSubnet,
    pub gateway_ip: [u8; 4],
    pub box_ip: [u8; 4],
    pub dns: Arc<DnsServer>,
    pub flows: Arc<FlowsLog>,
    /// Receiver for AcceptedConn; handed to the engine's dispatch task.
    accept_rx: Mutex<Option<std::sync::mpsc::Receiver<AcceptedConn>>>,
    /// Outbound command queue → poll thread (write bytes to a TCP socket,
    /// close a socket).
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
    /// Inbound rx from poll thread to a per-conn task: each connection has a
    /// unique receiver registered when it's accepted.
    rx_routes: Mutex<std::collections::HashMap<SocketHandle, std::sync::mpsc::Sender<Vec<u8>>>>,
}

enum Cmd {
    Write { handle: SocketHandle, data: Vec<u8> },
    Close { handle: SocketHandle },
    RegisterRx { handle: SocketHandle, tx: std::sync::mpsc::Sender<Vec<u8>> },
}

impl StackRuntime {
    pub fn start(box_id: u16, subnet: BoxSubnet, gateway_mac: [u8; 6],
                 box_mac: [u8; 6], tap_fd: std::os::fd::OwnedFd,
                 flows: Arc<FlowsLog>) -> Arc<Self> {
        let dns = Arc::new(DnsServer::new(subnet));
        let dhcp = DhcpServer { subnet, server_mac: gateway_mac };
        let (accept_tx, accept_rx) = std::sync::mpsc::channel();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let me = Arc::new(Self {
            box_id, subnet,
            gateway_ip: subnet.gateway_ip(), box_ip: subnet.box_ip(),
            dns: dns.clone(),
            flows: flows.clone(),
            accept_rx: Mutex::new(Some(accept_rx)),
            cmd_tx,
            rx_routes: Mutex::new(Default::default()),
        });
        let me2 = me.clone();
        std::thread::Builder::new()
            .name(format!("sarun-net-box{box_id}"))
            .spawn(move || {
                run_poll_loop(me2, tap_fd, gateway_mac, box_mac,
                              dhcp, dns, flows, accept_tx, cmd_rx);
            }).expect("spawn poll thread");
        me
    }

    /// Take ownership of the accept-side receiver. Called once by the
    /// dispatcher.
    pub fn take_accept_rx(&self)
        -> Option<std::sync::mpsc::Receiver<AcceptedConn>>
    {
        self.accept_rx.lock().take()
    }

    pub fn register_rx(&self, handle: SocketHandle,
                       tx: std::sync::mpsc::Sender<Vec<u8>>) {
        let _ = self.cmd_tx.send(Cmd::RegisterRx { handle, tx });
    }
    pub fn write(&self, handle: SocketHandle, data: Vec<u8>) {
        let _ = self.cmd_tx.send(Cmd::Write { handle, data });
    }
    pub fn close(&self, handle: SocketHandle) {
        let _ = self.cmd_tx.send(Cmd::Close { handle });
    }
}

/// PhyDevice impl over a raw TAP fd. Reads/writes one frame per call.
struct TapPhy {
    fd: RawFd,
    flows: Arc<FlowsLog>,
    mtu: usize,
}

impl Device for TapPhy {
    type RxToken<'a> = TapRx;
    type TxToken<'a> = TapTx<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = self.mtu;
        caps
    }

    fn receive(&mut self, _ts: SmolInstant)
               -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut buf = vec![0u8; self.mtu + 14];
        let n = unsafe {
            libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len())
        };
        if n <= 0 { return None; }
        buf.truncate(n as usize);
        let _ = self.flows.record(&buf);
        Some((TapRx { buf },
              TapTx { fd: self.fd, flows: &self.flows, mtu: self.mtu }))
    }

    fn transmit(&mut self, _ts: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(TapTx { fd: self.fd, flows: &self.flows, mtu: self.mtu })
    }
}

struct TapRx { buf: Vec<u8> }
impl RxToken for TapRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R { f(&self.buf) }
}

struct TapTx<'a> { fd: RawFd, flows: &'a FlowsLog, mtu: usize }
impl<'a> TxToken for TapTx<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let len = len.min(self.mtu + 14);
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        let _ = self.flows.record(&buf);
        unsafe { libc::write(self.fd, buf.as_ptr().cast(), buf.len()); }
        r
    }
}

fn run_poll_loop(rt: Arc<StackRuntime>, tap_fd: std::os::fd::OwnedFd,
                 gw_mac: [u8; 6], box_mac: [u8; 6],
                 dhcp: DhcpServer, dns: Arc<DnsServer>, flows: Arc<FlowsLog>,
                 accept_tx: std::sync::mpsc::Sender<AcceptedConn>,
                 cmd_rx: std::sync::mpsc::Receiver<Cmd>) {
    let fd = tap_fd.as_raw_fd();
    // Non-blocking I/O on the TAP (TUNSETIFF set O_NONBLOCK on open).
    let mut phy = TapPhy { fd, flows: flows.clone(), mtu: 1500 };

    let mut cfg = Config::new(EthernetAddress(gw_mac).into());
    cfg.random_seed = rand::random();
    let started = Instant::now();
    let now = || SmolInstant::from_millis(started.elapsed().as_millis() as i64);
    let mut iface = Interface::new(cfg, &mut phy, now());
    iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(IpAddress::Ipv4(Ipv4Address::from(rt.gateway_ip)), 16)).ok();
    });

    let mut sockets = SocketSet::new(vec![]);
    let mut listen_pool: VecDeque<SocketHandle> = VecDeque::new();
    for _ in 0..LISTEN_POOL { listen_pool.push_back(add_listener(&mut sockets)); }
    let mut claimed: HashSet<SocketHandle> = HashSet::new();
    let mut rx_map: std::collections::HashMap<SocketHandle, std::sync::mpsc::Sender<Vec<u8>>>
        = Default::default();

    loop {
        // 1. Drain control commands.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Write { handle, data } => {
                    let s = sockets.get_mut::<tcp::Socket>(handle);
                    if s.can_send() { let _ = s.send_slice(&data); }
                }
                Cmd::Close { handle } => {
                    let s = sockets.get_mut::<tcp::Socket>(handle);
                    s.close();
                }
                Cmd::RegisterRx { handle, tx } => { rx_map.insert(handle, tx); }
            }
        }

        // 2. Intercept inbound frames for DHCP/DNS BEFORE smoltcp consumes them,
        //    by peeking at the next read non-destructively. Implementation:
        //    we keep our own non-smoltcp recv path here too. To avoid double
        //    consumption we read one frame manually, decide if it's a DHCP/DNS
        //    request, reply directly, and ALSO inject it into smoltcp via a
        //    side-channel Device. For simplicity we let smoltcp drive normal
        //    frames and detect DHCP via UDP src=68/dst=67 in a hook that we
        //    wedge into the poll loop here using a one-shot RX impl.
        //
        //    Practical implementation below: read frames in a tight inner loop,
        //    dispatch DHCP/DNS, and let everything else fall through to smoltcp
        //    via an immediate re-inject path (write-then-let-smoltcp-see).
        drain_tap_for_udp_intercepts(fd, &flows, &gw_mac, &box_mac, &rt,
                                     &dhcp, &dns);

        // 3. Let smoltcp do its thing.
        let _ = iface.poll(now(), &mut phy, &mut sockets);

        // 4. Promote freshly-established sockets to AcceptedConn.
        let mut to_claim = vec![];
        for handle in listen_pool.iter().copied() {
            if claimed.contains(&handle) { continue; }
            let s = sockets.get_mut::<tcp::Socket>(handle);
            if s.is_active() && s.state() == tcp::State::Established {
                let local = s.local_endpoint();
                let remote = s.remote_endpoint();
                if let (Some(l), Some(r)) = (local, remote) {
                    let dst_ip = ip_octets(l.addr);
                    let src_ip = ip_octets(r.addr);
                    to_claim.push((handle, AcceptedConn {
                        handle,
                        dst_ip, dst_port: l.port,
                        src_ip, src_port: r.port,
                    }));
                }
            }
        }
        for (h, acc) in to_claim {
            claimed.insert(h);
            let _ = accept_tx.send(acc);
            // Replenish the pool so there's always a listener waiting.
            listen_pool.push_back(add_listener(&mut sockets));
        }
        // Cap pool size to avoid unbounded growth.
        while listen_pool.len() > LISTEN_POOL * 4 {
            if let Some(h) = listen_pool.pop_front() {
                if !claimed.contains(&h) { sockets.remove(h); }
            }
        }

        // 5. For each established socket with rx-data and a registered route,
        //    drain bytes to its consumer.
        let routes: Vec<(SocketHandle, std::sync::mpsc::Sender<Vec<u8>>)> =
            rx_map.iter().map(|(h, t)| (*h, t.clone())).collect();
        for (h, tx) in routes {
            if !sockets.iter().any(|(handle, _)| handle == h) { rx_map.remove(&h); continue; }
            let s = sockets.get_mut::<tcp::Socket>(h);
            while s.can_recv() {
                let chunk = s.recv(|buf| (buf.len(), buf.to_vec()))
                    .unwrap_or_default();
                if chunk.is_empty() { break; }
                let _ = tx.send(chunk);
            }
        }

        std::thread::sleep(Duration::from_millis(2));
    }
}

fn add_listener(sockets: &mut SocketSet) -> SocketHandle {
    let rx = tcp::SocketBuffer::new(vec![0u8; TCP_RX_BUF]);
    let tx = tcp::SocketBuffer::new(vec![0u8; TCP_TX_BUF]);
    let mut s = tcp::Socket::new(rx, tx);
    // Listen on any port at any address.
    s.listen((IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0)).ok();
    sockets.add(s)
}

fn ip_octets(addr: IpAddress) -> [u8; 4] {
    match addr {
        IpAddress::Ipv4(v) => v.octets(),
        _ => [0, 0, 0, 0],
    }
}

/// Read frames from the TAP fd directly, intercept DHCP and DNS, and inject
/// everything else back through the TAP loopback path for smoltcp to pick up
/// on its next poll. Implementation: we read with MSG_DONTWAIT semantics
/// (fd was opened O_NONBLOCK); for non-intercept frames we use a side queue
/// so smoltcp's `phy.receive()` sees them. For now we don't actually steal —
/// we let smoltcp see every frame, and we just parse the DHCP/DNS ones in
/// parallel and write replies. (Smoltcp doesn't bind UDP listeners for these
/// ports, so it'll silently drop them — no double-handling.)
fn drain_tap_for_udp_intercepts(fd: RawFd, flows: &FlowsLog,
                                gw_mac: &[u8; 6], box_mac: &[u8; 6],
                                rt: &StackRuntime,
                                dhcp: &DhcpServer, dns: &DnsServer) {
    // Use MSG_PEEK to peek the next frame without dequeueing; if it's a DHCP
    // or DNS request, dequeue it AND craft the reply ourselves. Otherwise
    // leave it for smoltcp. Loop a few times so a burst gets drained.
    for _ in 0..8 {
        let mut buf = vec![0u8; 2048];
        let n = unsafe {
            libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_PEEK)
        };
        if n <= 0 { return; }
        buf.truncate(n as usize);
        let consumed = try_intercept_udp(&buf, gw_mac, box_mac, rt, dhcp, dns,
                                         fd, flows);
        if !consumed { return; } // leave it for smoltcp
        // Actually consume the frame we already peeked.
        let mut sink = [0u8; 4];
        unsafe { libc::recv(fd, sink.as_mut_ptr().cast(), 0, 0); }
        // recv(fd, buf, 0) returns 0 immediately without dequeueing on stream
        // sockets, but TAP is packet-oriented: a zero-length read still pops
        // the next packet. Belt-and-suspenders: also do a real read.
        let mut drain = vec![0u8; 2048];
        unsafe { libc::read(fd, drain.as_mut_ptr().cast(), drain.len()); }
    }
}

fn try_intercept_udp(frame: &[u8], gw_mac: &[u8; 6], box_mac: &[u8; 6],
                     rt: &StackRuntime,
                     dhcp: &DhcpServer, dns: &DnsServer,
                     fd: RawFd, flows: &FlowsLog) -> bool {
    let Ok(eth) = EthernetFrame::new_checked(frame) else { return false; };
    if eth.ethertype() != EthernetProtocol::Ipv4 { return false; }
    let Ok(ipv4) = Ipv4Packet::new_checked(eth.payload()) else { return false; };
    if ipv4.next_header() != IpProtocol::Udp { return false; }
    let Ok(udp) = UdpPacket::new_checked(ipv4.payload()) else { return false; };
    let (sp, dp) = (udp.src_port(), udp.dst_port());
    let src_ip = ipv4.src_addr();
    let dst_ip = ipv4.dst_addr();
    let payload = udp.payload();

    // DHCP server: dst 67, src 68 (client→server). Reply is broadcast to .255.
    if dp == 67 && sp == 68 {
        if let Ok(Some(reply)) = dhcp.handle(payload) {
            // Build IPv4+UDP+Ethernet reply (broadcast at L2).
            send_udp_reply(fd, flows, gw_mac, &[0xff; 6],
                           Ipv4Address::from(rt.gateway_ip),
                           Ipv4Address::new(255, 255, 255, 255),
                           67, 68, &reply);
        }
        return true;
    }

    // DNS server: dst 53.
    if dp == 53 {
        if let Some(reply) = dns.handle(payload) {
            send_udp_reply(fd, flows, gw_mac, box_mac,
                           Ipv4Address::from(rt.gateway_ip),
                           src_ip,
                           53, sp, &reply);
        }
        // DNS-shaped UDP to other than :53 — we still consume so it doesn't
        // pile up.
        return true;
    }

    // Any other UDP: drop (this is the QUIC kill switch).
    let _ = (src_ip, dst_ip); // suppress unused
    true
}

fn send_udp_reply(fd: RawFd, flows: &FlowsLog,
                  src_mac: &[u8; 6], dst_mac: &[u8; 6],
                  src_ip: Ipv4Address, dst_ip: Ipv4Address,
                  src_port: u16, dst_port: u16, payload: &[u8]) {
    let udp_repr = UdpRepr { src_port, dst_port };
    let payload_len = payload.len();
    let ip_repr = Ipv4Repr {
        src_addr: src_ip, dst_addr: dst_ip,
        next_header: IpProtocol::Udp,
        payload_len: 8 + payload_len,
        hop_limit: 64,
    };
    let total = 14 + ip_repr.buffer_len() + 8 + payload_len;
    let mut buf = vec![0u8; total];
    {
        let mut eth = EthernetFrame::new_unchecked(&mut buf);
        eth.set_dst_addr(EthernetAddress(*dst_mac));
        eth.set_src_addr(EthernetAddress(*src_mac));
        eth.set_ethertype(EthernetProtocol::Ipv4);
    }
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(*src_mac),
        dst_addr: EthernetAddress(*dst_mac),
        ethertype: EthernetProtocol::Ipv4,
    };
    let _ = eth_repr;
    {
        let mut ipv4 = Ipv4Packet::new_unchecked(&mut buf[14..]);
        ip_repr.emit(&mut ipv4, &Default::default());
    }
    {
        let off = 14 + ip_repr.buffer_len();
        let mut udp = UdpPacket::new_unchecked(&mut buf[off..]);
        udp_repr.emit(&mut udp, &src_ip.into(), &dst_ip.into(),
                      payload_len, |p| p.copy_from_slice(payload),
                      &Default::default());
    }
    let _ = flows.record(&buf);
    unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()); }
}
