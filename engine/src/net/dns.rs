// Synthetic DNS — answers A queries with an IP from the box's synth pool, and
// stores the reverse mapping so the TCP catch-all can recover (host, port)
// from a connection's destination IP.
//
// Same domain → same IP for the lifetime of the box (no LRU eviction; the
// /16 pool is 65k slots, way more than any realistic box will burn).
// AAAA → empty NOERROR (forces IPv4 fallback), everything else SERVFAIL.

use std::sync::Arc;

use domain::base::{Message, MessageBuilder, Rtype};
use domain::rdata::A;
use parking_lot::Mutex;

use super::subnet::BoxSubnet;

pub struct DnsServer {
    pub subnet: BoxSubnet,
    state: Arc<Mutex<State>>,
}

struct State {
    by_domain: std::collections::HashMap<String, [u8; 4]>,
    by_ip: std::collections::HashMap<[u8; 4], String>,
    next: u32,
}

impl DnsServer {
    pub fn new(subnet: BoxSubnet) -> Self {
        Self { subnet, state: Arc::new(Mutex::new(State {
            by_domain: Default::default(), by_ip: Default::default(), next: 0,
        }))}
    }

    /// Allocate or look up the synthetic IP for `host` (lowercased).
    fn alloc_for(&self, host: &str) -> Option<[u8; 4]> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let mut g = self.state.lock();
        if let Some(ip) = g.by_domain.get(&host) { return Some(*ip); }
        let ip = self.subnet.synth_ip(g.next)?;
        g.next += 1;
        g.by_domain.insert(host.clone(), ip);
        g.by_ip.insert(ip, host);
        Some(ip)
    }

    /// Reverse lookup for the TCP catch-all.
    pub fn host_for_ip(&self, ip: [u8; 4]) -> Option<String> {
        self.state.lock().by_ip.get(&ip).cloned()
    }

    /// Handle a raw UDP DNS request and produce a response or None for drop.
    pub fn handle(&self, raw: &[u8]) -> Option<Vec<u8>> {
        // A malformed query / one with no question is dropped silently — that
        // is correct, RFC-conformant DNS server behavior, not a swallowed error.
        let msg = Message::from_slice(raw).ok()?;
        let q = msg.first_question()?;
        let qname = q.qname().to_string();
        let qtype = q.qtype();

        // Builder failures below are NOT expected and would silently drop the
        // reply → the box's resolve times out mysteriously. Log them so a
        // failed answer is visible.
        let mut builder = match MessageBuilder::new_vec()
            .start_answer(&msg, domain::base::iana::Rcode::NOERROR) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("sarun-engine: net: dns start_answer {qname}: {e}");
                return None;
            }
        };
        if qtype == Rtype::A {
            if let Some(ip) = self.alloc_for(&qname) {
                let addr = std::net::Ipv4Addr::from(ip);
                if let Err(e) = builder.push((q.qname(), 30u32, A::new(addr))) {
                    eprintln!("sarun-engine: net: dns push A {qname}: {e}");
                    return None;
                }
            }
        } else if qtype == Rtype::AAAA {
            // Empty NOERROR; the client will fall back to A.
        } else {
            // SERVFAIL for anything else.
            let b = match MessageBuilder::new_vec()
                .start_answer(&msg, domain::base::iana::Rcode::SERVFAIL) {
                Ok(b) => b, // finish() consumes self; no `mut` needed
                Err(e) => {
                    eprintln!("sarun-engine: net: dns start_answer(servfail) \
                               {qname}: {e}");
                    return None;
                }
            };
            return Some(b.finish());
        }
        Some(builder.finish())
    }
}
