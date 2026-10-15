//! Ethernet frames.

use super::{put_be16, Mac};

pub const ETHERNET_IPV4: u16 = 0x0800;
pub const ETHERNET_ARP: u16 = 0x0806;
pub const ETHERNET_IPV6: u16 = 0x86dd;

/// Writes the Ethernet header into the first 14 bytes of buf.
pub fn make_ethernet(buf: &mut [u8], dst: Mac, src: Mac, frame_type: u16) {
    buf[0..6].copy_from_slice(&dst.0);
    buf[6..12].copy_from_slice(&src.0);
    put_be16(&mut buf[12..], frame_type);
}
