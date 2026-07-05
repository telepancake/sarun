// `-n` box networking: per-box netns + one TAP + a smoltcp userland TCP/IP
// stack, with DHCP and DNS answered by the engine, every TCP terminated in
// userland and re-originated to the real upstream from the engine's (host)
// namespace, every L2 frame captured to pcapng, and HTTPS MITM'd via a CA
// minted once and planted into the box's overlay so every TLS client trusts
// it without per-tool configuration. UDP other than :53 is dropped at L2,
// which is the design choice for QUIC (it falls back to TCP/443).
//
// Public surface:
//   `Net` (this module) holds only the per-engine SHARED state: the MITM root
//   CA and the one banner-prompt queue. The per-box stack is NOT started here
//   — the RUNNER creates the netns + TAP and hands the engine the TAP fd
//   (SCM_RIGHTS on the register conn); `control::prepare_net` then stands up a
//   `stack::StackRuntime` + a `dispatch::Dispatcher` around that fd. There is
//   no `Net`-level per-box handle: each box's stack is owned by its own poll
//   thread and torn down when that thread's TAP fd closes.
//
// Per-box layout (Class E /16 per box, 12 bits of box id):
//   box subnet : (240 | (box_id >> 8)).(box_id & 0xff).0.0/16
//   gateway    : .0.1   (the engine's TAP-side address; DHCP + DNS + GW)
//   box        : .0.2   (handed out by DHCP)
//   synth pool : .1.0 .. .255.254  (DNS A answers for arbitrary domains)
//
// Modules:
//   subnet   — Class E /16 math + per-box id alloc
//   ca       — generate-once root CA persisted under XDG_DATA
//   tap      — fork+unshare(NEWNET), create TAP + assign addr/link-up, ship
//              tapfd + netnsfd back over socketpair
//   stack    — smoltcp Interface poll loop on the tapfd (TCP + UDP demux:
//              only :53 is terminated, other UDP dropped — the QUIC choice)
//   bridge   — smoltcp socket ⇄ tokio AsyncRead/Write adapter (SmoltcpStream)
//   dispatch — per-box conn router: policy-gate then HTTP/HTTPS MITM / L4
//   dhcp     — DHCPv4 server, one lease per box (always .0.2)
//   dns      — UDP :53 server: A → synth-pool IP, store reverse mapping
//   mitm     — rustls accept with rcgen-on-demand leaf certs by SNI; hyper
//              http1; tokio-rustls + hyper as the upstream client
//   l4       — non-TLS-non-HTTP TCP: copy bytes upstream both ways
//   flows    — pcapng (one IDB for the TAP) + SSLKEYLOGFILE sidecar
//   policy   — bridge to rules.rs (host/port/scheme/sni fields)
//   prompt   — banner-style approval queue for unknown hosts
//   webcap   — tee decoded HTTP request/response into the box's webcap store
//   filter   — proxy-side adblock + response rewrite (DESIGN-web.md W7)

pub mod ca;
pub mod subnet;

/// `-n` / `-N` / default CLI choice. Off (the default) gives the box an
/// EMPTY netns — getaddrinfo and any dial fail closed. Host shares the
/// engine's own netns (the pre-feature behavior). Tap wires the box up to
/// the per-box smoltcp stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetMode { Off, Tap, Host }

impl NetMode {
    pub fn as_str(self) -> &'static str {
        match self { Self::Off => "off", Self::Tap => "tap", Self::Host => "host" }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s { "off" => Some(Self::Off), "tap" => Some(Self::Tap),
                  "host" => Some(Self::Host), _ => None }
    }
}

// Runtime modules.
pub mod tap;
pub mod stack;
pub mod dhcp;
pub mod dns;
pub mod mitm;
pub mod l4;
pub mod flows;
pub mod policy;
pub mod prompt;
pub mod bridge;
pub mod dispatch;
pub mod webcap;
pub mod filter;

/// The per-box hooks the MITM proxy applies to each flow (DESIGN-web.md
/// W2/W7). Bundled so one optional handle threads through the dispatcher and
/// `proxy_request`: `None` (or all-None fields) is the pure pass-through every
/// non-opted box gets. Future proxy hooks slot in here without re-touching the
/// signatures.
pub struct ProxyHooks {
    /// Web capture sink — teed request/response rows (`--webcap`).
    pub capture: Option<std::sync::Arc<webcap::WebCapSink>>,
    /// Ad/tracker block + response rewrite (`--webfilter`).
    pub filter: Option<std::sync::Arc<filter::Filter>>,
}


use std::sync::Arc;

/// Global per-engine networking state — held by the engine main loop. The
/// per-box smoltcp stack is owned by its own poll thread (driven by the box's
/// TAP fd) and dispatcher task; the engine keeps no per-box handle.
pub struct Net {
    pub ca: Arc<ca::Ca>,
    /// One global banner-prompt queue. Boxes share it: the user sees one
    /// banner at a time regardless of which box's connection triggered
    /// it. UI consumers (the TUI's tick loop) peek the queue via the
    /// `prompts.peek` control verb and answer via `prompts.answer`.
    pub prompts: Arc<prompt::PromptQueue>,
}

impl Net {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            ca: Arc::new(ca::Ca::load_or_create()?),
            prompts: prompt::PromptQueue::new(),
        })
    }
}

