//! Network protocol stack and interfaces.

mod arp;
mod checksum;
pub mod dhcp;
mod ethernet;
mod ip;
pub mod tun;

pub use arp::*;
pub use checksum::*;
pub use ethernet::*;
pub use ip::*;

use std::fmt;

/// Reads a big-endian (network byte-order) u16 from data.
pub fn be16(data: &[u8]) -> u16 {
    u16::from_be_bytes([data[0], data[1]])
}

/// Writes v as a big-endian (network byte-order) u16 into buf.
pub fn put_be16(buf: &mut [u8], v: u16) {
    buf[..2].copy_from_slice(&v.to_be_bytes());
}

/// Ethernet address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    /// The broadcast address ff:ff:ff:ff:ff:ff.
    pub const BROADCAST: Mac = Mac([0xff; 6]);

    /// Creates an address from the first 6 bytes of data.
    pub fn from_slice(data: &[u8]) -> Mac {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&data[..6]);
        Mac(mac)
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = &self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        )
    }
}
