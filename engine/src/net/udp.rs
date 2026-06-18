// UDP demux: only :53 (DNS) is terminated; everything else is dropped.
//
// This is the policy that makes QUIC fail: clients send a QUIC Initial UDP
// packet to :443, get no response, fall back to TCP/443, and the TLS MITM
// takes over from there. No QUIC ALPN handshake means HTTP/3 is never
// negotiated, even if the upstream offers it.

use super::dns::DnsServer;

pub struct UdpDemux<'a> {
    pub dns: &'a DnsServer,
}

impl<'a> UdpDemux<'a> {
    /// `port` is the destination port the box dialed. Returns Some(reply) only
    /// for DNS; everything else is silently dropped.
    pub fn handle(&self, port: u16, payload: &[u8]) -> Option<Vec<u8>> {
        if port == 53 { return self.dns.handle(payload); }
        None
    }
}
