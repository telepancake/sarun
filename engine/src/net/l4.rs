// Non-HTTP TCP: open the upstream socket in the engine's (host) namespace,
// and splice bytes both ways. Each leg is independently shut down on the
// peer's FIN. Policy gates the dial; on deny the box-side stream gets RST.

use std::net::IpAddr;

pub struct L4Target {
    pub host: String,
    pub port: u16,
    pub resolved_ip: Option<IpAddr>,
}
