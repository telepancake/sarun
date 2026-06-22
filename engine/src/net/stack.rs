// Per-box smoltcp poll loop. Runs on a dedicated thread.
//
// Two UDP sockets bound for our two protocol responsibilities:
//   • DHCP server on UNSPECIFIED:67  (client broadcasts to 255.255.255.255)
//   • DNS  server on gateway_ip:53
// Other UDP (incl. QUIC :443) → no socket bound → smoltcp silently drops.
// That IS the design: what the proxy can't handle doesn't work.
//
// TCP termination is the "listener pool" pattern: many sockets in LISTEN
// at (UNSPECIFIED, 0) so any (dst_ip, dst_port) gets a SYN-ACK from us;
// each socket that transitions to Established gets pulled out, paired
// with a fresh listener, and handed to the dispatcher via `accept_tx`.
//
// One frame in, one frame out, one pcapng record each direction. The TAP
// fd was opened O_NONBLOCK by the tap-anchor child so smoltcp's poll
// doesn't block on the read.

use std::collections::{HashSet, VecDeque};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint,
                    Ipv4Address};

use super::dhcp::DhcpServer;
use super::dns::DnsServer;
use super::flows::FlowsLog;
use super::subnet::BoxSubnet;

// smoltcp won't accept a 0-port listen, so we bind a per-port pool covering
// the ports the box might realistically dial. Each port gets `LISTENERS_PER_PORT`
// idle sockets; as soon as one is claimed, a fresh listener is bound to the
// same port so concurrent connections still go through.
const LISTENERS_PER_PORT: usize = 4;
const TCP_RX_BUF: usize = 32 * 1024;
const TCP_TX_BUF: usize = 32 * 1024;
const UDP_BUF: usize = 32 * 1024;

/// Ports the box's TCP traffic gets terminated on. Anything else falls
/// through smoltcp with no listener → RST to the box (which is a fine signal
/// that the destination port isn't reachable through the proxy).
const LISTEN_PORTS: &[u16] = &[
    22, 25, 53, 80, 110, 143, 443, 465, 587, 993, 995,
    1025, 1080, 1433, 1521, 1883, 2049, 2375, 2376, 3000, 3128,
    3306, 4000, 4040, 4321, 5000, 5044, 5060, 5061, 5222, 5432,
    5601, 5672, 5900, 5984, 6000, 6379, 6443, 7000, 7474,
    7687, 8000, 8001, 8008, 8080, 8081, 8082, 8086, 8088,
    8443, 8500, 8888, 9000, 9001, 9042, 9090, 9091, 9092,
    9200, 9300, 9418, 10000, 10250, 11211, 15672, 25565, 27017,
    50051, 50052,
];

/// A handle on an established TCP connection inside the stack, plus the
/// destination it was dialed at (recovered from the smoltcp socket's
/// `local_endpoint()` — i.e. the SYN's dst). The handler interacts with the
/// socket via the poll thread through the StackRuntime command channel.
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
    accept_rx: Mutex<Option<std::sync::mpsc::Receiver<AcceptedConn>>>,
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
}

enum Cmd {
    Write { handle: SocketHandle, data: Vec<u8> },
    Close { handle: SocketHandle },
    RegisterRx { handle: SocketHandle, tx: std::sync::mpsc::Sender<Vec<u8>> },
}

impl StackRuntime {
    pub fn start(box_id: u16, subnet: BoxSubnet, gateway_mac: [u8; 6],
                 box_mac: [u8; 6], tap_fd: OwnedFd,
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

    pub fn take_accept_rx(&self)
        -> Option<std::sync::mpsc::Receiver<AcceptedConn>>
    {
        self.accept_rx.lock().take()
    }

    pub fn register_rx(&self, handle: SocketHandle,
                       tx: std::sync::mpsc::Sender<Vec<u8>>) {
        // A send failure means the poll thread is gone (box stack torn down);
        // surface it — silently dropping leaves a connection that never
        // receives bytes and mysteriously stalls.
        if let Err(e) = self.cmd_tx.send(Cmd::RegisterRx { handle, tx }) {
            eprintln!("sarun-engine: net: register_rx on dead poll thread: {e}");
        }
    }
    pub fn write(&self, handle: SocketHandle, data: Vec<u8>) {
        // Same: a dead poll thread silently swallowing writes manifests later
        // as a hung/half-open connection. Log it instead.
        if let Err(e) = self.cmd_tx.send(Cmd::Write { handle, data }) {
            eprintln!("sarun-engine: net: write on dead poll thread: {e}");
        }
    }
    pub fn close(&self, handle: SocketHandle) {
        // Best-effort: if the poll thread is already gone the socket is gone
        // too, so a failed close is genuinely nothing to act on. Ignore.
        let _ = self.cmd_tx.send(Cmd::Close { handle });
    }
}

/// TAP-fd PhyDevice. One frame in / one frame out per call; pcapng-records
/// both directions transparently. The fd is non-blocking (TUNSETIFF set it),
/// so `recv` returns None when there's nothing to read.
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
        // pcap capture is observability-only and must never break the
        // datapath; this is also the per-frame hot path, so logging here
        // would flood. A record error is intentionally dropped.
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
        // pcap capture is observability-only / per-frame hot path — drop a
        // record error rather than break or flood the datapath (see receive()).
        let _ = self.flows.record(&buf);
        // A short TAP write desyncs the box's L2 stream; surface it. Cannot
        // recover here (smoltcp owns retransmit), but it must not be invisible.
        let n = unsafe { libc::write(self.fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            eprintln!("sarun-engine: net: TAP write failed: {}",
                      std::io::Error::last_os_error());
        } else if (n as usize) < buf.len() {
            eprintln!("sarun-engine: net: short TAP write {}/{}",
                      n, buf.len());
        }
        r
    }
}

fn run_poll_loop(rt: Arc<StackRuntime>, tap_fd: OwnedFd,
                 gw_mac: [u8; 6], _box_mac: [u8; 6],
                 dhcp: DhcpServer, dns: Arc<DnsServer>, flows: Arc<FlowsLog>,
                 accept_tx: std::sync::mpsc::Sender<AcceptedConn>,
                 cmd_rx: std::sync::mpsc::Receiver<Cmd>) {
    let fd = tap_fd.as_raw_fd();
    let mut phy = TapPhy { fd, flows, mtu: 1500 };

    let mut cfg = Config::new(EthernetAddress(gw_mac).into());
    cfg.random_seed = rand::random();
    let started = Instant::now();
    let now = || SmolInstant::from_millis(started.elapsed().as_millis() as i64);
    let mut iface = Interface::new(cfg, &mut phy, now());
    iface.update_ip_addrs(|addrs| {
        // One-time stack setup: if the gateway address won't bind, the whole
        // box network is dead — surface it instead of starting a silent
        // black-hole stack. (push fails only on a full fixed-capacity store.)
        if addrs.push(IpCidr::new(
            IpAddress::Ipv4(Ipv4Address::from(rt.gateway_ip)), 16)).is_err() {
            eprintln!("sarun-engine: net: failed to assign gateway IP to iface");
        }
    });
    // any_ip = "process packets to ANY routable unicast address, not just
    // the one configured on the interface". The synth-pool IPs are not
    // configured (only the gateway is), so without any_ip + a covering
    // route, smoltcp rejects SYNs to 240.X.1.0 etc. The route below points
    // every Class E address back through our own gateway IP — smoltcp's
    // any_ip path then sees "next-hop is myself" and terminates locally.
    iface.set_any_ip(true);
    // The default route is what makes the any_ip catch-all terminate locally;
    // without it every box SYN to a synth IP is dropped. Surface a failure.
    if let Err(e) = iface.routes_mut().add_default_ipv4_route(
        Ipv4Address::from(rt.gateway_ip)) {
        eprintln!("sarun-engine: net: add default route failed: {e}");
    }

    let mut sockets = SocketSet::new(vec![]);

    // DHCP server: listen on any-addr:67. DHCPDISCOVER comes as a broadcast,
    // so the dst address is 255.255.255.255 — smoltcp accepts when the
    // bind is to an unspecified address.
    let dhcp_h = {
        let rx = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; UDP_BUF]);
        let tx = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; UDP_BUF]);
        let mut s = udp::Socket::new(rx, tx);
        // If the DHCP server can't bind :67 the box never gets a lease — fail
        // loudly rather than start a stack that silently can't hand out IPs.
        if let Err(e) = s.bind(IpListenEndpoint { addr: None, port: 67 }) {
            eprintln!("sarun-engine: net: DHCP bind :67 failed: {e}");
        }
        sockets.add(s)
    };

    // DNS server: bound on the gateway IP. The box will dial it as its
    // only nameserver (via the resolv.conf the runner planted).
    let dns_h = {
        let rx = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; UDP_BUF]);
        let tx = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; UDP_BUF]);
        let mut s = udp::Socket::new(rx, tx);
        // If the synthetic DNS can't bind :53 the box can't resolve anything —
        // surface it rather than black-hole every lookup.
        if let Err(e) = s.bind(IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(Ipv4Address::from(rt.gateway_ip))),
            port: 53,
        }) {
            eprintln!("sarun-engine: net: DNS bind :53 failed: {e}");
        }
        sockets.add(s)
    };

    // Per-port pool: (handle, port).
    let mut listen_pool: VecDeque<(SocketHandle, u16)> = VecDeque::new();
    for &p in LISTEN_PORTS {
        for _ in 0..LISTENERS_PER_PORT {
            listen_pool.push_back((add_listener(&mut sockets, p), p));
        }
    }
    let mut claimed: HashSet<SocketHandle> = HashSet::new();
    let mut rx_map: std::collections::HashMap<SocketHandle, std::sync::mpsc::Sender<Vec<u8>>>
        = Default::default();

    loop {
        // 1. Drain control commands.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Write { handle, data } => {
                    let s = sockets.get_mut::<tcp::Socket>(handle);
                    // A send_slice error here means box-bound bytes were lost
                    // (closed/aborted socket); surface so a truncated response
                    // isn't a silent mystery.
                    if s.can_send() {
                        if let Err(e) = s.send_slice(&data) {
                            eprintln!("sarun-engine: net: tcp send_slice: {e}");
                        }
                    }
                }
                Cmd::Close { handle } => {
                    let s = sockets.get_mut::<tcp::Socket>(handle);
                    s.close();
                }
                Cmd::RegisterRx { handle, tx } => { rx_map.insert(handle, tx); }
            }
        }

        // 2. Drive smoltcp. poll() returns a bool "did anything change",
        //    not a Result — discarding it is correct (we poll on a fixed tick).
        let _ = iface.poll(now(), &mut phy, &mut sockets);

        // 3. DHCP: any waiting request → reply.
        {
            let s = sockets.get_mut::<udp::Socket>(dhcp_h);
            while let Ok((data, meta)) = s.recv() {
                if let Ok(Some(reply)) = dhcp.handle(data) {
                    // The reply goes broadcast (255.255.255.255:68). Source
                    // is the gateway. smoltcp emits with the bound socket's
                    // local addr if we set local_address.
                    let dst = IpEndpoint {
                        addr: IpAddress::Ipv4(Ipv4Address::new(255, 255, 255, 255)),
                        port: 68,
                    };
                    let mut out = udp::UdpMetadata::from(dst);
                    out.local_address = Some(IpAddress::Ipv4(
                        Ipv4Address::from(rt.gateway_ip)));
                    // A dropped DHCP reply leaves the box without a lease;
                    // surface it.
                    if let Err(e) = s.send_slice(&reply, out) {
                        eprintln!("sarun-engine: net: DHCP reply send: {e}");
                    }
                }
                // meta (the request's source endpoint) is unused: DHCP replies
                // always go to the broadcast address built above.
                let _ = meta;
            }
        }

        // 4. DNS: any waiting query → answer.
        {
            let s = sockets.get_mut::<udp::Socket>(dns_h);
            // Collect (reply, dst) outside the borrow so we can call send.
            let mut to_send: Vec<(Vec<u8>, IpEndpoint)> = vec![];
            while let Ok((data, meta)) = s.recv() {
                if let Some(reply) = dns.handle(data) {
                    to_send.push((reply, meta.endpoint));
                }
            }
            for (reply, dst) in to_send {
                let out: udp::UdpMetadata = dst.into();
                // A dropped DNS reply makes the box's resolve fail/time out;
                // surface it.
                if let Err(e) = s.send_slice(&reply, out) {
                    eprintln!("sarun-engine: net: DNS reply send: {e}");
                }
            }
        }

        // 5. Promote freshly-established TCP sockets to AcceptedConn.
        let mut to_claim: Vec<(SocketHandle, u16, AcceptedConn)> = vec![];
        for &(handle, port) in listen_pool.iter() {
            if claimed.contains(&handle) { continue; }
            let s = sockets.get_mut::<tcp::Socket>(handle);
            if s.is_active() && s.state() == tcp::State::Established {
                let local = s.local_endpoint();
                let remote = s.remote_endpoint();
                if let (Some(l), Some(r)) = (local, remote) {
                    let dst_ip = ip_octets(l.addr);
                    let src_ip = ip_octets(r.addr);
                    to_claim.push((handle, port, AcceptedConn {
                        handle, dst_ip, dst_port: l.port,
                        src_ip, src_port: r.port,
                    }));
                }
            }
        }
        for (h, port, acc) in to_claim {
            claimed.insert(h);
            // A send failure means the dispatcher receiver is gone, so this
            // accepted connection will never be serviced — surface it.
            if let Err(e) = accept_tx.send(acc) {
                eprintln!("sarun-engine: net: accept handoff dropped \
                           (dispatcher gone): {e}");
            }
            listen_pool.push_back((add_listener(&mut sockets, port), port));
        }
        // GC: drop oldest unclaimed-but-stale listeners if pool blows up.
        let cap = LISTEN_PORTS.len() * LISTENERS_PER_PORT * 4;
        while listen_pool.len() > cap {
            if let Some((h, _)) = listen_pool.pop_front() {
                if !claimed.contains(&h) { sockets.remove(h); }
            }
        }

        // 6. For each established socket with rx-data and a registered route,
        //    drain bytes to its consumer.
        let routes: Vec<(SocketHandle, std::sync::mpsc::Sender<Vec<u8>>)> =
            rx_map.iter().map(|(h, t)| (*h, t.clone())).collect();
        for (h, tx) in routes {
            if !sockets.iter().any(|(handle, _)| handle == h) {
                rx_map.remove(&h);
                continue;
            }
            let s = sockets.get_mut::<tcp::Socket>(h);
            while s.can_recv() {
                let chunk = match s.recv(|buf| (buf.len(), buf.to_vec())) {
                    Ok(c) => c,
                    Err(e) => {
                        // A recv error on an established socket is a real
                        // datapath fault, not just "no data"; surface and stop
                        // draining this socket this tick.
                        eprintln!("sarun-engine: net: tcp recv: {e}");
                        break;
                    }
                };
                if chunk.is_empty() { break; }
                // A send failure means the per-conn consumer (SmoltcpStream
                // relay) is gone; drop the route so we stop trying.
                if tx.send(chunk).is_err() {
                    rx_map.remove(&h);
                    break;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(2));
    }
}

fn add_listener(sockets: &mut SocketSet, port: u16) -> SocketHandle {
    let rx = tcp::SocketBuffer::new(vec![0u8; TCP_RX_BUF]);
    let tx = tcp::SocketBuffer::new(vec![0u8; TCP_TX_BUF]);
    let mut s = tcp::Socket::new(rx, tx);
    // Listen at any local address (we own the whole /16) on this specific
    // port. smoltcp won't accept a 0-port listen, hence the per-port pool.
    // A listen failure means this pool slot won't accept SYNs — surface it.
    if let Err(e) = s.listen(IpListenEndpoint { addr: None, port }) {
        eprintln!("sarun-engine: net: listen on port {port}: {e}");
    }
    sockets.add(s)
}

fn ip_octets(addr: IpAddress) -> [u8; 4] {
    match addr {
        IpAddress::Ipv4(v) => v.octets(),
        _ => [0, 0, 0, 0],
    }
}
