// Class E /16 per box. 12 bits of box id, 16 bits within the box's /16.
//
//   subnet  : ((240 | (id >> 8)) , (id & 0xff) , 0 , 0) / 16
//   gateway : (..) (..) .0.1     — engine's TAP-side; DHCP/DNS/GW
//   box     : (..) (..) .0.2     — the box's lease (always; one host per netns)
//   synth   : (..) (..) .x.y     — DNS pool for x in 1..=255, y in 0..=255
//
// Class E is RFC-1112 "reserved for future use"; Linux happily accepts it on
// local interfaces. Traffic never leaves the netns — smoltcp accepts every
// SYN — so the host's view of 240/4 doesn't matter.

pub const CLASS_E_PREFIX: u8 = 0b1111_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxSubnet { pub id: u16 } // 12 bits valid

impl BoxSubnet {
    pub fn new(id: u16) -> Self {
        assert!(id < 4096, "box id must fit in 12 bits");
        Self { id }
    }

    fn octets_prefix(self) -> (u8, u8) {
        let hi = (self.id >> 8) as u8 & 0x0F;   // 4 bits
        let lo = (self.id & 0xff) as u8;        // 8 bits
        (CLASS_E_PREFIX | hi, lo)
    }

    pub fn gateway_ip(self) -> [u8; 4] {
        let (a, b) = self.octets_prefix();
        [a, b, 0, 1]
    }

    pub fn box_ip(self) -> [u8; 4] {
        let (a, b) = self.octets_prefix();
        [a, b, 0, 2]
    }

    pub fn netmask(self) -> [u8; 4] { [255, 255, 0, 0] }

    /// /30 — what the BOX's kernel sees on the TAP. With only .0.1 (gw)
    /// and .0.2 (box) in the subnet, anything else (including synth pool
    /// IPs) routes via the default route → gateway → engine. Without this
    /// the box would believe synth IPs are on-link and try to ARP them
    /// directly (and smoltcp won't proxy-ARP for the whole /16).
    pub fn box_prefix_len(self) -> u8 { 30 }

    /// Yield synth-pool addresses .1.0 .. .255.254 (skipping .1.255 etc. broadcasts
    /// would be over-engineered: the /16 has one bcast at .255.255 and we just
    /// avoid the entire .0.* row reserved for gateway/box).
    pub fn synth_ip(self, idx: u32) -> Option<[u8; 4]> {
        // 65280 usable slots: .1.0 to .255.255 minus one bcast → just exclude .255.255.
        if idx >= 65279 { return None; }
        let off = idx + 256; // skip .0.*
        let (a, b) = self.octets_prefix();
        Some([a, b, (off >> 8) as u8, (off & 0xff) as u8])
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_zero_at_class_e_floor() {
        let s = BoxSubnet::new(0);
        assert_eq!(s.gateway_ip(), [240, 0, 0, 1]);
        assert_eq!(s.box_ip(), [240, 0, 0, 2]);
    }

    #[test]
    fn box_max_at_class_e_ceiling() {
        let s = BoxSubnet::new(4095);
        assert_eq!(s.gateway_ip(), [255, 255, 0, 1]);
        assert_eq!(s.box_ip(), [255, 255, 0, 2]);
    }

    #[test]
    fn synth_pool_starts_after_gateway_row() {
        let s = BoxSubnet::new(0);
        assert_eq!(s.synth_ip(0).unwrap(), [240, 0, 1, 0]);
        assert_eq!(s.synth_ip(255).unwrap(), [240, 0, 1, 255]);
        assert_eq!(s.synth_ip(256).unwrap(), [240, 0, 2, 0]);
    }

    #[test]
    fn synth_pool_exhausts() {
        let s = BoxSubnet::new(1);
        assert!(s.synth_ip(65278).is_some());
        assert!(s.synth_ip(65279).is_none());
    }
}
