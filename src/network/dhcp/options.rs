//! DHCP message types and options.

use std::fmt;
use std::net::Ipv4Addr;

/// DHCP message type (option 53).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsgType(pub u8);

pub const DHCPDISCOVER: MsgType = MsgType(1);
pub const DHCPOFFER: MsgType = MsgType(2);
pub const DHCPREQUEST: MsgType = MsgType(3);
pub const DHCPDECLINE: MsgType = MsgType(4);
pub const DHCPACK: MsgType = MsgType(5);
pub const DHCPNAK: MsgType = MsgType(6);
pub const DHCPRELEASE: MsgType = MsgType(7);
pub const DHCPINFORM: MsgType = MsgType(8);

impl fmt::Display for MsgType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            DHCPDISCOVER => "DHCPDISCOVER",
            DHCPOFFER => "DHCPOFFER",
            DHCPREQUEST => "DHCPREQUEST",
            DHCPDECLINE => "DHCPDECLINE",
            DHCPACK => "DHCPACK",
            DHCPNAK => "DHCPNAK",
            DHCPRELEASE => "DHCPRELEASE",
            DHCPINFORM => "DHCPINFORM",
            _ => return write!(f, "{{MsgType {}}}", self.0),
        };
        write!(f, "{}", name)
    }
}

/// DHCP option tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptionTag(pub u8);

pub const TAG_PAD: OptionTag = OptionTag(0);
pub const TAG_SUBNET_MASK: OptionTag = OptionTag(1);
pub const TAG_TIME_OFFSET: OptionTag = OptionTag(2);
pub const TAG_ROUTER: OptionTag = OptionTag(3);
pub const TAG_DOMAIN_SERVER: OptionTag = OptionTag(6);
pub const TAG_HOSTNAME: OptionTag = OptionTag(12);
pub const TAG_DOMAIN_NAME: OptionTag = OptionTag(15);
pub const TAG_MTU_INTERFACE: OptionTag = OptionTag(26);
pub const TAG_BROADCAST_ADDRESS: OptionTag = OptionTag(28);
pub const TAG_STATIC_ROUTE: OptionTag = OptionTag(33);
pub const TAG_NTP_SERVERS: OptionTag = OptionTag(42);
pub const TAG_ADDRESS_REQUEST: OptionTag = OptionTag(50);
pub const TAG_ADDRESS_TIME: OptionTag = OptionTag(51);
pub const TAG_DHCP_MSG_TYPE: OptionTag = OptionTag(53);
pub const TAG_DHCP_SERVER_ID: OptionTag = OptionTag(54);
pub const TAG_PARAMETER_LIST: OptionTag = OptionTag(55);
pub const TAG_MESSAGE: OptionTag = OptionTag(56);
pub const TAG_DHCP_MAX_MSG_SIZE: OptionTag = OptionTag(57);
pub const TAG_CLIENT_ID: OptionTag = OptionTag(61);
pub const TAG_RAPID_COMMIT: OptionTag = OptionTag(80);
pub const TAG_DHCP_CAPTIVE_PORTAL: OptionTag = OptionTag(114);
pub const TAG_AUTO_CONFIG: OptionTag = OptionTag(116);
pub const TAG_DOMAIN_SEARCH: OptionTag = OptionTag(119);
pub const TAG_SIP_SERVERS_DHCP_OPTION: OptionTag = OptionTag(120);
pub const TAG_CLASSLESS_STATIC_ROUTE_OPTION: OptionTag = OptionTag(121);
pub const TAG_FORCERENEW_NONCE_CAPABLE: OptionTag = OptionTag(145);
pub const TAG_END: OptionTag = OptionTag(255);

/// Option value types for formatting: 'd' decimal, 'i' IP address,
/// 'I' IP address list, 'P' parameter list, 's' string, 'x' hex,
/// '0' no value.
pub(super) fn option_type(tag: OptionTag) -> Option<(char, &'static str)> {
    Some(match tag {
        // RFC 2132 - DHCP Options and BOOTP Vendor Extensions
        TAG_PAD => ('0', "Pad"),
        TAG_SUBNET_MASK => ('i', "Subnet Mask"),
        TAG_TIME_OFFSET => ('d', "Time Offset"),
        TAG_ROUTER => ('I', "Router"),
        TAG_DOMAIN_SERVER => ('I', "Domain Server"),
        TAG_HOSTNAME => ('s', "Hostname"),
        TAG_DOMAIN_NAME => ('s', "Domain Name"),
        TAG_MTU_INTERFACE => ('d', "MTU Interface"),
        TAG_BROADCAST_ADDRESS => ('i', "Broadcast Address"),
        TAG_STATIC_ROUTE => ('I', "Static Route"),
        TAG_NTP_SERVERS => ('I', "NTP Servers"),
        TAG_ADDRESS_REQUEST => ('i', "Address Request"),
        TAG_ADDRESS_TIME => ('d', "Address Time"),
        TAG_DHCP_MSG_TYPE => ('x', "DHCP Msg Type"),
        TAG_DHCP_SERVER_ID => ('i', "DHCP Server Id"),
        TAG_PARAMETER_LIST => ('P', "Parameter List"),
        TAG_MESSAGE => ('s', "Message"),
        TAG_DHCP_MAX_MSG_SIZE => ('d', "DHCP Max Msg Size"),
        TAG_CLIENT_ID => ('x', "Client Id"),
        TAG_END => ('0', "End"),

        // RFC 2563 - DHCP Auto-Configuration Option
        TAG_AUTO_CONFIG => ('d', "Auto-Config"),

        // RFC 3361 - DHCPv4 Option for SIP Servers
        TAG_SIP_SERVERS_DHCP_OPTION => ('x', "SIP Servers DHCP Option"),

        // RFC 3397 - DHCP Domain Search Option
        TAG_DOMAIN_SEARCH => ('x', "Domain Search"),

        // RFC 3442 - Classless Static Route Option for DHCPv4
        TAG_CLASSLESS_STATIC_ROUTE_OPTION => ('x', "Classless Static Route Option"),

        // RFC 4039 - Rapid Commit Option for DHCPv4
        TAG_RAPID_COMMIT => ('x', "Rapid Commit"),

        // RFC 6704 - Forcerenew Nonce
        TAG_FORCERENEW_NONCE_CAPABLE => ('d', "FORCERENEW_NONCE_CAPABLE"),

        // RFC 8910 - Captive-Portal Identification in DHCP and Router
        // Advertisements (RAs)
        TAG_DHCP_CAPTIVE_PORTAL => ('x', "DHCP Captive-Portal"),

        _ => return None,
    })
}

impl fmt::Display for OptionTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match option_type(*self) {
            Some((_, name)) => write!(f, "{}", name),
            None => write!(f, "{}", self.0),
        }
    }
}

/// DHCP option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhcpOption {
    pub tag: OptionTag,
    pub data: Vec<u8>,
}

impl DhcpOption {
    /// Creates an option with the data.
    pub fn new(tag: OptionTag, data: &[u8]) -> DhcpOption {
        DhcpOption {
            tag,
            data: data.to_vec(),
        }
    }

    /// Returns the encoded length of the option.
    pub fn encoded_len(&self) -> usize {
        match self.tag {
            TAG_PAD | TAG_END => 1,
            _ => 1 + 1 + self.data.len(),
        }
    }
}

fn format_ip(data: &[u8]) -> String {
    match data {
        [a, b, c, d] => Ipv4Addr::new(*a, *b, *c, *d).to_string(),
        _ => data.iter().map(|b| format!("{:02x}", b)).collect(),
    }
}

impl fmt::Display for DhcpOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (t, name) = match option_type(self.tag) {
            Some((t, name)) => (t, name.to_string()),
            None => ('x', self.tag.0.to_string()),
        };
        if self.data.is_empty() {
            return write!(f, "{}", name);
        }
        match t {
            'd' => {
                let v = self.data.iter().fold(0u64, |v, &b| v << 8 | b as u64);
                write!(f, "{}={}", name, v)
            }
            'i' => write!(f, "{}={}", name, format_ip(&self.data)),
            'I' => {
                let addrs: Vec<String> = self.data.chunks(4).map(format_ip).collect();
                write!(f, "{}={}", name, addrs.join(","))
            }
            'P' => {
                let params: Vec<String> = self
                    .data
                    .iter()
                    .map(|&p| match option_type(OptionTag(p)) {
                        Some((_, n)) => n.to_string(),
                        None => p.to_string(),
                    })
                    .collect();
                write!(f, "{}={}", name, params.join(","))
            }
            's' => write!(f, "{}={}", name, String::from_utf8_lossy(&self.data)),
            _ => {
                let hex: String = self.data.iter().map(|b| format!("{:02x}", b)).collect();
                write!(f, "{}={}", name, hex)
            }
        }
    }
}
