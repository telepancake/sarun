// TCP termination + per-connection dispatcher.
//
// smoltcp's "listen wildcard" pattern: we keep N idle smoltcp::TcpSocket
// listeners in the SocketSet bound to LocalAddr=Any, dst_port=Any. The
// smoltcp poll thread, after each poll, walks the set; any socket that
// transitioned to Established gets pulled off the listener pool, paired
// with a fresh listener, and handed to a tokio task via a channel. The
// task discovers the real (host, port) via dst_addr → DNS reverse, then
// chooses:
//   • port 443 (or TLS-sniffed)   → mitm::handle_tls
//   • port 80  (or HTTP-sniffed)  → mitm::handle_http
//   • anything else                → l4::forward
//
// Cooperation with smoltcp: handlers don't own a raw fd, they own a
// `TunSocket` adapter that proxies reads/writes through the smoltcp poll
// loop via channels (recv_queue / send_queue). The poll loop is the only
// thread touching smoltcp state.

use std::net::Ipv4Addr;

pub struct Conn {
    pub box_id: u16,
    pub src: (Ipv4Addr, u16),
    pub dst_ip: Ipv4Addr,
    pub dst_port: u16,
    /// Reverse-lookup-derived hostname (if DNS allocated it).
    pub host: Option<String>,
}
