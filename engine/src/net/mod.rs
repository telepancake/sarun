// `-n` box networking: per-box netns + one TAP + a smoltcp userland TCP/IP
// stack, with DHCP and DNS answered by the engine, every TCP terminated in
// userland and re-originated to the real upstream from the engine's (host)
// namespace, every L2 frame captured to pcapng, and HTTPS MITM'd via a CA
// minted once and planted into the box's overlay so every TLS client trusts
// it without per-tool configuration. UDP other than :53 is dropped at L2,
// which is the design choice for QUIC (it falls back to TCP/443).
//
// Public surface:
//   `Net::start_box(box_id, gateway_mac, allow_real_egress)` — returns a
//   `NetHandle` whose `.netns_fd` is what `runner::run` setns'es bwrap into
//   (so bwrap inherits an already-equipped netns). Dropping the handle tears
//   the TAP and netns down.
//
// Per-box layout (Class E /16 per box, 12 bits of box id):
//   box subnet : (240 | (box_id >> 8)).(box_id & 0xff).0.0/16
//   gateway    : .0.1   (the engine's TAP-side address; DHCP + DNS + GW)
//   box        : .0.2   (handed out by DHCP)
//   synth pool : .1.0 .. .255.254  (DNS A answers for arbitrary domains)
//
// Modules:
//   subnet  — Class E /16 math + per-box id alloc
//   ca      — generate-once root CA persisted under XDG_DATA
//   tap     — fork+unshare(NEWNET), create TAP + assign addr/link-up, ship
//             tapfd + netnsfd back over socketpair
//   stack   — smoltcp Interface poll loop on the tapfd
//   dhcp    — DHCPv4 server, one lease per box (always .0.2)
//   dns     — UDP :53 server: A → synth-pool IP, store reverse mapping
//   tcp     — smoltcp listen wildcard → spawn per-conn task with dst tuple
//   udp     — UDP demux (only :53 terminated; rest dropped)
//   mitm    — rustls accept with rcgen-on-demand leaf certs by SNI; hyper
//             http1+2; reqwest as upstream client
//   l4      — non-TLS-non-HTTP TCP: copy bytes upstream both ways
//   flows   — pcapng (one IDB for the TAP) + SSLKEYLOGFILE sidecar
//   policy  — bridge to rules.rs (host/port/scheme/sni fields)
//   prompt  — banner-style approval queue for unknown hosts

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
pub mod tcp;
pub mod udp;
pub mod mitm;
pub mod l4;
pub mod flows;
pub mod policy;
pub mod prompt;
pub mod bridge;
pub mod dispatch;

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

/// Per-box networking the engine holds while the box runs: the in-engine
/// smoltcp stack runtime (its poll thread owns the TAP fd) plus the flow/keylog
/// paths. The netns + TAP belong to the BOX — the runner created them and the
/// kernel frees them when the box exits — so the engine forks no process and
/// holds no netns reference here.
pub struct NetHandle {
    pub box_id: u16,
    pub gateway_ip: [u8; 4],    // .0.1
    pub box_ip: [u8; 4],        // .0.2
    pub stack: Arc<stack::StackRuntime>,
    pub flows_path: PathBuf,
    pub keylog_path: PathBuf,
}

/// Global registry — `Net` is held by the engine main loop and one
/// `NetHandle` per `-n` box is registered while the box runs.
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

/// Construct a NetHandle from the pieces the stack module just built (the TAP
/// fd was created by the runner and now lives in the stack's poll thread).
pub fn make_handle(
    box_id: u16, gateway_ip: [u8; 4], box_ip: [u8; 4],
    stack: Arc<stack::StackRuntime>,
    flows_path: PathBuf, keylog_path: PathBuf,
) -> NetHandle {
    NetHandle { box_id, gateway_ip, box_ip, stack, flows_path, keylog_path }
}

#[allow(dead_code)]
pub fn unused(_: OwnedFd) {}
