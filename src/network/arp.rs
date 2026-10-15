//! Address resolution protocol (ARP).

use super::{be16, put_be16, Mac};
use std::fmt;
use std::net::Ipv4Addr;

/// ARP message for IPv4 over Ethernet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arp {
    pub htype: u16,
    pub ptype: u16,
    pub hlen: u8,
    pub plen: u8,
    pub oper: u16,
    pub sha: Mac,
    pub spa: Ipv4Addr,
    pub tha: Mac,
    pub tpa: Ipv4Addr,
}

impl fmt::Display for Arp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HTYPE={:04x}, PTYPE={:04x}, HLEN={}, PLEN={}, OPER={}, SHA={}, SPA={}, THA={}, TPA={}",
            self.htype,
            self.ptype,
            self.hlen,
            self.plen,
            self.oper,
            self.sha,
            self.spa,
            self.tha,
            self.tpa
        )
    }
}

fn ipv4(data: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(data[0], data[1], data[2], data[3])
}

impl Arp {
    /// Parses an ARP packet.
    pub fn parse(data: &[u8]) -> Result<Arp, String> {
        if data.len() != 28 {
            return Err(format!("invalid ARP packet: len={}", data.len()));
        }
        Ok(Arp {
            htype: be16(&data[0..]),
            ptype: be16(&data[2..]),
            hlen: data[4],
            plen: data[5],
            oper: be16(&data[6..]),
            sha: Mac::from_slice(&data[8..14]),
            spa: ipv4(&data[14..18]),
            tha: Mac::from_slice(&data[18..24]),
            tpa: ipv4(&data[24..28]),
        })
    }
}

/// Writes an IPv4 over Ethernet ARP packet into the first 28 bytes of
/// buf.
pub fn make_arp(buf: &mut [u8], oper: u16, sha: Mac, spa: Ipv4Addr, tha: Mac, tpa: Ipv4Addr) {
    put_be16(&mut buf[0..], 1);
    put_be16(&mut buf[2..], 0x800);
    buf[4] = 6;
    buf[5] = 4;
    put_be16(&mut buf[6..], oper);
    buf[8..14].copy_from_slice(&sha.0);
    buf[14..18].copy_from_slice(&spa.octets());
    buf[18..24].copy_from_slice(&tha.0);
    buf[24..28].copy_from_slice(&tpa.octets());
}
