// DHCP server, one lease per box (always `subnet.box_ip()`), gateway and DNS
// both set to `subnet.gateway_ip()`. Parses/encodes via dhcproto.
//
// State is trivial: there's exactly one client (the box), so we don't even
// track yiaddr separately — `request_for(xid, mac)` returns the same lease
// info each time. Lease time = 24h; the box will renew transparently.

use anyhow::{Context, Result};
use dhcproto::v4::{Decodable, Decoder, Encodable, Encoder,
                   Message, MessageType, Opcode, OptionCode, DhcpOption};

use super::subnet::BoxSubnet;

pub struct DhcpServer {
    pub subnet: BoxSubnet,
}

impl DhcpServer {
    pub fn handle(&self, raw: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut dec = Decoder::new(raw);
        let req = Message::decode(&mut dec).context("decode dhcp")?;
        if req.opcode() != Opcode::BootRequest { return Ok(None); }
        // A BOOTREQUEST without an explicit DHCP message-type option is legacy
        // BOOTP; treating it as Discover is the correct default, not a dropped
        // error (genuine decode failures already returned via `?` above).
        let mt = req.opts().msg_type().unwrap_or(MessageType::Discover);
        match mt {
            MessageType::Discover => Ok(Some(self.reply(&req, MessageType::Offer)?)),
            MessageType::Request  => Ok(Some(self.reply(&req, MessageType::Ack)?)),
            MessageType::Release | MessageType::Decline => Ok(None),
            _ => Ok(None),
        }
    }

    fn reply(&self, req: &Message, mt: MessageType) -> Result<Vec<u8>> {
        let mut m = Message::default();
        m.set_opcode(Opcode::BootReply);
        m.set_htype(req.htype());
        // hlen is derived from chaddr, no setter.
        m.set_xid(req.xid());
        m.set_flags(req.flags());
        m.set_chaddr(req.chaddr());
        m.set_yiaddr(std::net::Ipv4Addr::from(self.subnet.box_ip()));
        m.set_siaddr(std::net::Ipv4Addr::from(self.subnet.gateway_ip()));
        m.opts_mut().insert(DhcpOption::MessageType(mt));
        m.opts_mut().insert(DhcpOption::ServerIdentifier(
            std::net::Ipv4Addr::from(self.subnet.gateway_ip())));
        m.opts_mut().insert(DhcpOption::AddressLeaseTime(86_400));
        m.opts_mut().insert(DhcpOption::SubnetMask(
            std::net::Ipv4Addr::from(self.subnet.netmask())));
        m.opts_mut().insert(DhcpOption::Router(vec![
            std::net::Ipv4Addr::from(self.subnet.gateway_ip())]));
        m.opts_mut().insert(DhcpOption::DomainNameServer(vec![
            std::net::Ipv4Addr::from(self.subnet.gateway_ip())]));
        // End option is added by the encoder automatically.
        let mut out = Vec::with_capacity(548);
        let mut enc = Encoder::new(&mut out);
        m.encode(&mut enc).context("encode dhcp")?;
        // Silence unused-import warning under future versions.
        let _ = OptionCode::SubnetMask;
        Ok(out)
    }
}
