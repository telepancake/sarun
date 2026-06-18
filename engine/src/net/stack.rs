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

const LISTEN_POOL: usize = 32;
const TCP_RX_BUF: usize = 64 * 1024;
const TCP_TX_BUF: usize = 64 * 1024;
const UDP_BUF: usize = 32 * 1024;

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
        let _ = self.cmd_tx.send(Cmd::RegisterRx { handle, tx });
    }
    pub fn write(&self, handle: SocketHandle, data: Vec<u8>) {
        let _ = self.cmd_tx.send(Cmd::Write { handle, data });
    }
    pub fn close(&self, handle: SocketHandle) {
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
        let _ = addrs.push(IpCidr::new(
            IpAddress::Ipv4(Ipv4Address::from(rt.gateway_ip)), 16));
    });

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
        let _ = s.bind(IpListenEndpoint { addr: None, port: 67 });
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
        let _ = s.bind(IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(Ipv4Address::from(rt.gateway_ip))),
            port: 53,
        });
        sockets.add(s)
    };

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

        // 2. Drive smoltcp.
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
                    let _ = s.send_slice(&reply, out);
                }
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
                let _ = s.send_slice(&reply, out);
            }
        }

        // 5. Promote freshly-established TCP sockets to AcceptedConn.
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
                        handle, dst_ip, dst_port: l.port,
                        src_ip, src_port: r.port,
                    }));
                }
            }
        }
        for (h, acc) in to_claim {
            claimed.insert(h);
            let _ = accept_tx.send(acc);
            listen_pool.push_back(add_listener(&mut sockets));
        }
        // Cap the pool so a busy box doesn't unbounded-grow.
        while listen_pool.len() > LISTEN_POOL * 4 {
            if let Some(h) = listen_pool.pop_front() {
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
    // Listen on any port at any address — we own every IP in the box's /16,
    // and the DNS allocator hands them out for whichever hostnames the box
    // asks about.
    let _ = s.listen(IpListenEndpoint { addr: None, port: 0 });
    sockets.add(s)
}

fn ip_octets(addr: IpAddress) -> [u8; 4] {
    match addr {
        IpAddress::Ipv4(v) => v.octets(),
        _ => [0, 0, 0, 0],
    }
}
