//! macOS host half of Sarun's packet transport.
//!
//! QEMU boxes use a connected datagram socket pair, so the engine's smoltcp
//! stack receives the same Ethernet frames without a host TAP or namespace.

use std::os::fd::OwnedFd;

use anyhow::{Result, bail};

use super::subnet::BoxSubnet;

pub const BOX_SUBNET_ID: u16 = 1;
pub const BOX_MAC: [u8; 6] = [0x02, 0x73, 0x72, 0x6e, 0x00, 0x02];

#[derive(Debug)]
pub struct EarlyUserNamespaceRequired;

impl std::fmt::Display for EarlyUserNamespaceRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("network namespaces are a Linux host feature")
    }
}

impl std::error::Error for EarlyUserNamespaceRequired {}

pub fn mark_early_tap_reexec(_command: &mut std::process::Command) {}

pub fn prepare_early_tap() -> Result<bool> {
    Ok(false)
}

pub fn box_subnet() -> BoxSubnet {
    BoxSubnet::new(BOX_SUBNET_ID)
}

/// QEMU's datagram packet transport is available without `/dev/net/tun`.
pub fn tap_available() -> bool {
    true
}

pub fn gateway_mac() -> [u8; 6] {
    [0x02, 0x73, 0x72, 0x6e, 0x00, 0x01]
}

pub fn create_netns_tap() -> Result<OwnedFd> {
    bail!("native namespace/TAP boxes are unavailable on macOS; use --qemu")
}

pub fn tap_is_prepared() -> bool {
    false
}

pub fn keep_prepared_tap(_tap: OwnedFd) {}

pub fn configure_appliance_network(_mode: crate::generated_wire::NetMode) -> Result<()> {
    bail!("appliance network configuration runs inside the Linux guest")
}

pub const fn qemu_host_dns() -> [u8; 4] {
    [10, 0, 2, 3]
}

pub fn unshare_netns() -> Result<()> {
    bail!("network namespaces are unavailable on macOS; use --qemu")
}
