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
use super::prompt::{PromptQueue, Verdict};
use super::stack::{AcceptedConn, StackRuntime};
use super::webcap::WebCapSink;

pub struct Dispatcher {
    pub stack: Arc<StackRuntime>,
    pub dns: Arc<DnsServer>,
    pub box_name: String,
    pub ca: Arc<Ca>,
    pub keylog: Arc<KeyLogFile>,
    pub upstream_tls: Arc<rustls::ClientConfig>,
    pub prompts: Arc<PromptQueue>,
    /// Per-box web capture sink (DESIGN-web.md W2). `None` when this box
    /// didn't opt into capture — the proxy then runs its pure pass-through.
    pub webcap: Option<Arc<WebCapSink>>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn start(stack: Arc<StackRuntime>, dns: Arc<DnsServer>,
                 box_name: String, ca: Arc<Ca>, keylog: Arc<KeyLogFile>,
                 upstream_tls: Arc<rustls::ClientConfig>,
                 prompts: Arc<PromptQueue>,
                 webcap: Option<Arc<WebCapSink>>,
                 rt: tokio::runtime::Handle) {
        let Some(rx) = stack.take_accept_rx() else { return; };
        let me = Self { stack, dns, box_name, ca, keylog, upstream_tls, prompts, webcap };
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
                    let prompts = me.prompts.clone();
                    let webcap = me.webcap.clone();
                    rt.spawn(handle_conn(stack, dns, box_name, ca, keylog,
                                          up, prompts, webcap, acc));
                }
            }).expect("spawn dispatcher");
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_conn(stack: Arc<StackRuntime>, dns: Arc<DnsServer>,
                     box_name: String,
                     ca: Arc<Ca>, keylog: Arc<KeyLogFile>,
                     upstream_tls: Arc<rustls::ClientConfig>,
                     prompts: Arc<PromptQueue>,
                     webcap: Option<Arc<WebCapSink>>,
                     acc: AcceptedConn) {
    let host = dns.host_for_ip(acc.dst_ip)
        .unwrap_or_else(|| ipv4(acc.dst_ip));
    let port = acc.dst_port;
    let scheme = if port == 443 { "https" } else if port == 80 { "http" }
                 else { "tcp" }.to_string();

    // ── policy gate ─────────────────────────────────────────────────────
    let subj = super::policy::NetSubject {
        host: host.clone(),
        port,
        scheme: scheme.clone(),
        sni: String::new(),
        proto: "tcp".to_string(),
        box_name: box_name.clone(),
        ..Default::default()
    };
    let rules = crate::rules::Rules::load();
    let verdict = super::policy::decide(&rules.rules, &subj);
    let allow = match verdict {
        crate::rules::Action::Apply => true,
        crate::rules::Action::Discard => false,
        crate::rules::Action::Passthrough => true,
        crate::rules::Action::Ask => {
            // Banner-prompt the user (deny-if-no-TUI is enforced inside
            // PromptQueue::ask). On AllowSave/DenySave persist a new rule
            // line to filerules so the next conn skips the banner.
            let v = prompts.ask(box_name.clone(), host.clone(),
                                port, scheme.clone()).await;
            if v.is_persistent() {
                let act = if v == Verdict::AllowSave { "apply" } else { "discard" };
                if let Err(e) = append_rule_line(
                    &format!("{act} host:{host}\n"))
                {
                    eprintln!("sarun-net: persist {act} host:{host}: {e}");
                }
            }
            v.is_allow()
        }
    };
    if !allow {
        eprintln!("sarun-net: DENY {host}:{port} (box={box_name})");
        stack.close(acc.handle);
        return;
    }

    let stream_io = SmoltcpStream::new(stack, acc.handle);
    let r = if port == 443 {
        super::mitm::serve_https(stream_io, &host, ca, keylog, upstream_tls, webcap).await
    } else if port == 80 {
        super::mitm::serve_http(stream_io, &host, port, webcap).await
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

/// PREPEND one rule line to the same on-disk filerules file the rules
/// pane edits. Prepend (not append) matters semantically: rules are
/// first-match-wins, and a specific saved rule like
///   apply host:example.com
/// must win over a broader earlier rule like
///   ask host:*
/// otherwise the very rule we just saved gets shadowed and the user
/// gets asked again on the next dial. Writing through a tempfile +
/// rename keeps the swap atomic so a concurrent reader can't see a
/// half-written file.
fn append_rule_line(line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = crate::paths::config_home();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("filerules");
    // No prior rules file → start from empty content; this is the first saved
    // rule, not a swallowed read error.
    let prev = std::fs::read_to_string(&path).unwrap_or_default();
    let tmp = dir.join("filerules.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true).truncate(true).write(true).open(&tmp)?;
        f.write_all(line.as_bytes())?;
        f.write_all(prev.as_bytes())?;
        // Best-effort durability: the atomic rename below is what guarantees a
        // reader never sees a half-written file; an fsync failure here only
        // weakens crash-durability of a UI-saved rule, so surface but proceed.
        if let Err(e) = f.sync_all() {
            eprintln!("sarun-engine: net: fsync filerules.tmp: {e}");
        }
    }
    std::fs::rename(tmp, path)
}
