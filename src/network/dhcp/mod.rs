//! Dynamic Host Configuration Protocol (DHCP) defined by RFC 2131.

mod options;
mod server;
#[cfg(test)]
mod tests;

pub use options::*;
pub use server::*;

use std::fmt;
use std::net::Ipv4Addr;

pub const BOOTREQUEST: u8 = 1;
pub const BOOTREPLY: u8 = 2;
/// Length of the fixed message header, including the options cookie.
pub const HDR_LEN: usize = 7 * 4 + 16 + 64 + 128 + 4;
pub const OPTIONS_COOKIE: u32 = 0x63825363;

/// NUL-terminated string field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NullString(pub Vec<u8>);

impl NullString {
    /// Creates a NUL-terminated string of at most 63 characters.
    pub fn new(v: &str) -> NullString {
        let mut bytes: Vec<u8> = v.bytes().take(63).collect();
        bytes.push(0);
        NullString(bytes)
    }

    /// Returns the bytes before the first NUL.
    pub fn as_bytes(&self) -> &[u8] {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        &self.0[..end]
    }
}

impl fmt::Display for NullString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.as_bytes()))
    }
}

/// Client hardware address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HAddr(pub Vec<u8>);

impl fmt::Display for HAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.0.iter().map(|b| format!("{:02x}", b)).collect();
        write!(f, "{}", parts.join(":"))
    }
}

impl HAddr {
    /// Returns the first hlen bytes of the address.
    pub fn prefix(&self, hlen: u8) -> HAddr {
        HAddr(self.0[..(hlen as usize).min(self.0.len())].to_vec())
    }
}

/// DHCP message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dhcp {
    pub op: u8,
    pub htype: u8,
    pub hlen: u8,
    pub hops: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: HAddr,
    pub sname: NullString,
    pub file: NullString,
    pub cookie: u32,
    pub options: Vec<DhcpOption>,

    /// The TAG_DHCP_MSG_TYPE option value.
    pub msg_type: MsgType,
    /// The TAG_ADDRESS_REQUEST option value.
    pub address_request: Option<Ipv4Addr>,
    /// The TAG_HOSTNAME option value.
    pub hostname: String,
}

impl Default for Dhcp {
    fn default() -> Dhcp {
        Dhcp {
            op: 0,
            htype: 0,
            hlen: 0,
            hops: 0,
            xid: 0,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: HAddr::default(),
            sname: NullString::default(),
            file: NullString::default(),
            cookie: 0,
            options: Vec::new(),
            msg_type: MsgType(0),
            address_request: None,
            hostname: String::new(),
        }
    }
}

impl fmt::Display for Dhcp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "op={}, htype={}, hlen={}, hops={}, xid={:08x}, secs={}, flags={:04x}, \
             ciaddr={}, yiaddr={}, siaddr={}, giaddr={}, chaddr={}, sname={:?}, \
             file={:?}, cookie={:08x}, options=[",
            self.op,
            self.htype,
            self.hlen,
            self.hops,
            self.xid,
            self.secs,
            self.flags,
            self.ciaddr,
            self.yiaddr,
            self.siaddr,
            self.giaddr,
            self.chaddr.prefix(self.hlen),
            self.sname.to_string(),
            self.file.to_string(),
            self.cookie
        )?;
        for (i, opt) in self.options.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", opt)?;
        }
        write!(f, "]")
    }
}

impl Dhcp {
    /// Adds the option to the message.
    pub fn add_option(&mut self, opt: DhcpOption) {
        self.options.push(opt);
    }

    /// Encodes the message into its wire format.
    pub fn encode(&self) -> Vec<u8> {
        let l = HDR_LEN + self.options.iter().map(|o| o.encoded_len()).sum::<usize>();
        let mut buf = vec![0u8; l.div_ceil(4) * 4];

        fn put(buf: &mut [u8], data: &[u8]) {
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
        }

        buf[0] = self.op;
        buf[1] = self.htype;
        buf[2] = self.hlen;
        buf[3] = self.hops;
        buf[4..8].copy_from_slice(&self.xid.to_be_bytes());
        buf[8..10].copy_from_slice(&self.secs.to_be_bytes());
        buf[10..12].copy_from_slice(&self.flags.to_be_bytes());
        buf[12..16].copy_from_slice(&self.ciaddr.octets());
        buf[16..20].copy_from_slice(&self.yiaddr.octets());
        buf[20..24].copy_from_slice(&self.siaddr.octets());
        buf[24..28].copy_from_slice(&self.giaddr.octets());
        put(&mut buf[28..44], &self.chaddr.0);
        put(&mut buf[44..108], &self.sname.0);
        put(&mut buf[108..236], &self.file.0);
        buf[236..240].copy_from_slice(&OPTIONS_COOKIE.to_be_bytes());

        // Encode options.
        let mut ofs = 240;
        for opt in &self.options {
            buf[ofs] = opt.tag.0;
            ofs += 1;
            if opt.tag != TAG_PAD && opt.tag != TAG_END {
                buf[ofs] = opt.data.len() as u8;
                ofs += 1;
                buf[ofs..ofs + opt.data.len()].copy_from_slice(&opt.data);
                ofs += opt.data.len();
            }
        }
        buf
    }

    /// Decodes a message from its wire format. Truncated options end
    /// the option parsing.
    pub fn decode(data: &[u8]) -> Result<Dhcp, String> {
        let mut d = Decoder { data };
        let mut dhcp = Dhcp {
            op: d.u8()?,
            htype: d.u8()?,
            hlen: d.u8()?,
            hops: d.u8()?,
            xid: d.u32()?,
            secs: d.u16()?,
            flags: d.u16()?,
            ciaddr: d.ip()?,
            yiaddr: d.ip()?,
            siaddr: d.ip()?,
            giaddr: d.ip()?,
            chaddr: HAddr(d.data(16)?.to_vec()),
            sname: NullString(d.data(64)?.to_vec()),
            file: NullString(d.data(128)?.to_vec()),
            cookie: d.u32()?,
            ..Default::default()
        };

        // Parse options.
        while !d.data.is_empty() {
            let Ok(opt) = d.option() else {
                break;
            };
            match opt.tag {
                TAG_DHCP_MSG_TYPE if opt.data.len() == 1 => dhcp.msg_type = MsgType(opt.data[0]),
                TAG_ADDRESS_REQUEST if opt.data.len() == 4 => {
                    dhcp.address_request = Some(Ipv4Addr::new(
                        opt.data[0],
                        opt.data[1],
                        opt.data[2],
                        opt.data[3],
                    ))
                }
                TAG_HOSTNAME => dhcp.hostname = String::from_utf8_lossy(&opt.data).into_owned(),
                _ => {}
            }
            dhcp.options.push(opt);
        }
        Ok(dhcp)
    }
}

/// Wire format decoder.
struct Decoder<'a> {
    data: &'a [u8],
}

impl<'a> Decoder<'a> {
    fn data(&mut self, size: usize) -> Result<&'a [u8], String> {
        if self.data.len() < size {
            return Err("unexpected EOF".to_string());
        }
        let (v, rest) = self.data.split_at(size);
        self.data = rest;
        Ok(v)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.data(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let d = self.data(2)?;
        Ok(u16::from_be_bytes([d[0], d[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let d = self.data(4)?;
        Ok(u32::from_be_bytes([d[0], d[1], d[2], d[3]]))
    }

    fn ip(&mut self) -> Result<Ipv4Addr, String> {
        let d = self.data(4)?;
        Ok(Ipv4Addr::new(d[0], d[1], d[2], d[3]))
    }

    fn option(&mut self) -> Result<DhcpOption, String> {
        let tag = OptionTag(self.u8()?);
        if tag == TAG_PAD || tag == TAG_END {
            return Ok(DhcpOption {
                tag,
                data: Vec::new(),
            });
        }
        let l = self.u8()? as usize;
        Ok(DhcpOption {
            tag,
            data: self.data(l)?.to_vec(),
        })
    }
}
