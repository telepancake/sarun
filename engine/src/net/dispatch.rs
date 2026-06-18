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
use super::ca::Ca;
use super::dns::DnsServer;
use super::mitm::KeyLogFile;
use super::stack::{AcceptedConn, StackRuntime};

pub struct Dispatcher {
    pub stack: Arc<StackRuntime>,
    pub dns: Arc<DnsServer>,
    pub box_name: String,
    pub ca: Arc<Ca>,
    pub keylog: Arc<KeyLogFile>,
    pub upstream_tls: Arc<rustls::ClientConfig>,
}

impl Dispatcher {
    pub fn start(stack: Arc<StackRuntime>, dns: Arc<DnsServer>,
                 box_name: String, ca: Arc<Ca>, keylog: Arc<KeyLogFile>,
                 upstream_tls: Arc<rustls::ClientConfig>,
                 rt: tokio::runtime::Handle) {
        let Some(rx) = stack.take_accept_rx() else { return; };
        let me = Self { stack, dns, box_name, ca, keylog, upstream_tls };
        std::thread::Builder::new()
            .name("sarun-net-dispatch".into())
            .spawn(move || {
                while let Ok(acc) = rx.recv() {
                    let stack = me.stack.clone();
                    let dns = me.dns.clone();
                    let box_name = me.box_name.clone();
                    let ca = me.ca.clone();
                    let keylog = me.keylog.clone();
                    let up = me.upstream_tls.clone();
                    rt.spawn(handle_conn(stack, dns, box_name, ca, keylog, up, acc));
                }
            }).expect("spawn dispatcher");
    }
}

async fn handle_conn(stack: Arc<StackRuntime>, dns: Arc<DnsServer>,
                     box_name: String,
                     ca: Arc<Ca>, keylog: Arc<KeyLogFile>,
                     upstream_tls: Arc<rustls::ClientConfig>,
                     acc: AcceptedConn) {
    let host = dns.host_for_ip(acc.dst_ip)
        .unwrap_or_else(|| ipv4(acc.dst_ip));
    let port = acc.dst_port;

    // ── policy gate ─────────────────────────────────────────────────────
    // The same rules.rs file the UI edits is consulted here at SYN-accept
    // time. Net-applicable rules (any clause references a NET_KIND) are
    // first-match-wins via `net::policy::decide`; file-only rules slide
    // off (field resolver returns "" for unknown kinds). Default = Allow
    // on no match, so the proxy works out of the box.
    let subj = super::policy::NetSubject {
        host: host.clone(),
        port,
        scheme: if port == 443 { "https" } else if port == 80 { "http" }
                else { "tcp" }.to_string(),
        sni: String::new(),
        proto: "tcp".to_string(),
        box_name: box_name.clone(),
        ..Default::default()
    };
    let rules = crate::rules::Rules::load();
    if super::policy::decide(&rules.rules, &subj) == crate::rules::Action::Discard {
        eprintln!("sarun-net: DENY {host}:{port} (box={box_name})");
        // Drop the stream — the smoltcp socket's RST goes back to the box,
        // curl/whatever sees "connection refused".
        stack.close(acc.handle);
        return;
    }

    let stream_io = SmoltcpStream::new(stack, acc.handle);
    let r = if port == 443 {
        super::mitm::serve_https(stream_io, &host, ca, keylog, upstream_tls).await
    } else if port == 80 {
        super::mitm::serve_http(stream_io, &host, port).await
    } else {
        super::l4::forward(stream_io, &host, port).await
    };
    if let Err(e) = r {
        eprintln!("sarun-net: conn {host}:{port}: {e}");
    }
}

fn ipv4(o: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3])
}
