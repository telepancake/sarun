// Per-box connection dispatcher. Spawned once per `-n` box; takes the
// stack's AcceptedConn channel and routes each connection to the right
// handler:
//   • port 80  or HTTP-sniffed first byte → mitm::serve_http
//   • port 443 or TLS-sniffed             → mitm::serve_https
//   • anything else                       → l4::forward
// Before opening any upstream, we gate via `policy::decide` against the
// rules file. On Deny we close the box-side stream immediately; on Allow
// we proceed; on Inspect (HTTPS only) we ensure MITM is on (it is by
// default for 443).

use std::sync::Arc;

use super::bridge::SmoltcpStream;
use super::dns::DnsServer;
use super::stack::{AcceptedConn, StackRuntime};

pub struct Dispatcher {
    pub stack: Arc<StackRuntime>,
    pub dns: Arc<DnsServer>,
    pub box_name: String,
}

impl Dispatcher {
    pub fn start(stack: Arc<StackRuntime>, dns: Arc<DnsServer>,
                 box_name: String, rt: tokio::runtime::Handle) {
        let Some(rx) = stack.take_accept_rx() else { return; };
        let me = Self { stack, dns, box_name };
        std::thread::Builder::new()
            .name("sarun-net-dispatch".into())
            .spawn(move || {
                while let Ok(acc) = rx.recv() {
                    let stack = me.stack.clone();
                    let dns = me.dns.clone();
                    let box_name = me.box_name.clone();
                    rt.spawn(handle_conn(stack, dns, box_name, acc));
                }
            }).expect("spawn dispatcher");
    }
}

async fn handle_conn(stack: Arc<StackRuntime>, dns: Arc<DnsServer>,
                     box_name: String, acc: AcceptedConn) {
    let host = dns.host_for_ip(acc.dst_ip)
        .unwrap_or_else(|| ipv4(acc.dst_ip));
    let port = acc.dst_port;
    let stream = SmoltcpStream::new(stack, acc.handle);
    let _ = box_name; // policy hook will use this once rules ship
    if port == 443 {
        let _ = super::mitm::serve_https(stream, &host).await;
    } else if port == 80 {
        let _ = super::mitm::serve_http(stream, &host, port).await;
    } else {
        let _ = super::l4::forward(stream, &host, port).await;
    }
}

fn ipv4(o: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3])
}
